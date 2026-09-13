//! Executable verification.
//!
//! Verification used to be a single LLM critic reading the agent's own prose
//! summary. That is unfalsifiable: a node could claim "implemented the settings
//! modal with live key validation", the critic would agree, and the file would
//! not exist. An entire extension shipped that way - typecheck broken in 30
//! places, one screen a hardcoded mock, one feature never written at all -
//! while every node reported PASS.
//!
//! So the critic is now the *second* gate. The first gate runs the project's own
//! commands and believes only their exit codes.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const OUTPUT_CAP: usize = 4000;
/// POSIX shell exit codes meaning the command itself could not be executed
/// (126 = found but not executable, 127 = command not found). A gate that exits
/// this way is malformed or unavailable, not failed work, so it is downgraded to
/// a manual check instead of dooming the node.
const SHELL_CANNOT_EXECUTE: [i32; 2] = [126, 127];

/// How long a "launches and displays" gate is allowed to run before it is
/// considered to have launched successfully. A terminal app or dev server never
/// exits on its own, so waiting for exit is wrong; a short liveness window is
/// the honest signal. Overridable for slow CI machines.
fn launch_smoke_secs() -> u64 {
    std::env::var("FRACTAL_LAUNCH_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5)
}

static LAUNCH_LOG_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct GateOutcome {
    pub command: String,
    pub passed: bool,
    pub output: String,
    /// The entry named no executable command (or the shell could not execute
    /// it). It is skipped rather than failed, and the critic owns the
    /// corresponding acceptance criterion.
    pub manual: bool,
}

/// Which gates a node is accountable for.
///
/// This distinction is load-bearing. Auto-detected gates are whole-project
/// commands (`tsc --noEmit`, `npm run build`), and a leaf cannot satisfy those:
/// when the first leaf runs, its siblings' modules do not exist yet, so the
/// project legitimately does not typecheck. Applying project-wide gates to
/// leaves would fail every early node through no fault of its own, exhaust its
/// retries and stall the tree before integration was ever reached.
///
/// So leaves are accountable only for gates their contract states explicitly,
/// and whole-project truth is enforced where it is actually actionable: on the
/// integrating parents, whose job is to assemble the pieces and prove they work
/// together - and which can reopen a specific child when they do not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateScope {
    /// A node implementing one contract directly.
    Leaf,
    /// A node aggregating completed children.
    Integration,
}

/// Gates a contract asked for explicitly, else the detected suite - but only for
/// integration nodes. Explicit gates always win and always apply, so a leaf can
/// still be held to a specific command when its contract names one.
pub fn resolve_gates(root: &Path, contract_gates: &[String], scope: GateScope) -> Vec<String> {
    if !contract_gates.is_empty() {
        return contract_gates.to_vec();
    }
    match scope {
        GateScope::Leaf => Vec::new(),
        GateScope::Integration => detect_gates(root),
    }
}
/// Infer build/test commands from the manifests actually present.
pub fn detect_gates(root: &Path) -> Vec<String> {
    let mut gates = Vec::new();

    if root.join("package.json").exists() {
        let manifest = std::fs::read_to_string(root.join("package.json")).unwrap_or_default();
        let scripts = manifest
            .split_once("\"scripts\"")
            .map(|(_, rest)| rest)
            .unwrap_or("");
        // A typecheck is the cheapest way to catch the cross-module drift that
        // per-module unit tests structurally cannot see.
        if root.join("tsconfig.json").exists() && binary_on_path("npx") {
            gates.push("npx tsc --noEmit".to_string());
        }
        if scripts.contains("\"build\"") && binary_on_path("npm") {
            gates.push("npm run build".to_string());
        }
        if scripts.contains("\"test\"") && binary_on_path("npm") {
            gates.push("npm test --silent".to_string());
        }
        // A TUI or dev server only proves itself by starting. `npm run dev` and
        // friends are launch commands (see `is_launch_command`), so an
        // integrating node gets a smoke-launch gate: a crash fails fast instead
        // of letting the project pass every check without ever running (trial 3's
        // React 19 / ink 4 TUI crashed and no gate started it). Ordered last so
        // build/test failures surface first.
        if binary_on_path("npm") {
            if let Some(script) = ["start", "dev", "serve", "preview"]
                .into_iter()
                .find(|s| scripts.contains(&format!("\"{s}\"")))
            {
                gates.push(format!("npm run {script}"));
            } else if binary_on_path("node") {
                // A TUI entry that is not wired to a script still has to be
                // started. Trial 5 run B launched its TUI as `node dist/app.js`
                // with no `start` script, so no smoke gate ran and a crashing TUI
                // would have passed every check. Fall back to the manifest's
                // declared entry point, then to a conventional built entry.
                if let Some(entry) = package_entrypoint(&manifest).or_else(|| dist_entrypoint(root))
                {
                    gates.push(format!("node {entry}"));
                }
            }
        }
    }

    if root.join("Cargo.toml").exists() && binary_on_path("cargo") {
        gates.push("cargo check --all-targets".to_string());
        gates.push("cargo test".to_string());
    }

    if root.join("pyproject.toml").exists() && root.join("tests").exists() {
        if let Some(python) = python_interpreter() {
            gates.push(format!("{python} -m pytest -q"));
        }
    }

    gates
}

/// The entry point a `package.json` itself declares - `main`, or the first
/// `bin` - when it names a runnable JavaScript file. This is the smoke command
/// for a project that ships a TUI/dev entry but never wired a `start` script.
fn package_entrypoint(manifest: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(manifest).ok()?;
    let candidate = match json.get("main").and_then(|m| m.as_str()) {
        Some(m) if !m.trim().is_empty() => m.trim().to_string(),
        _ => match json.get("bin") {
            Some(serde_json::Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
            Some(serde_json::Value::Object(map)) => map
                .values()
                .filter_map(|v| v.as_str())
                .find(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_string())?,
            _ => return None,
        },
    };
    is_js_entrypoint(&candidate).then_some(candidate)
}

/// A conventional built entry point, for a project that declares neither a
/// launch script nor `main`/`bin` (trial 5 run B: `node dist/app.js`). Only
/// well-known names are accepted so an arbitrary library file is never launched.
fn dist_entrypoint(root: &Path) -> Option<String> {
    for name in [
        "index.js",
        "app.js",
        "main.js",
        "index.mjs",
        "app.mjs",
        "index.cjs",
    ] {
        let rel = format!("dist/{name}");
        if root.join(&rel).is_file() {
            return Some(rel);
        }
    }
    None
}

fn is_js_entrypoint(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".js") || lower.ends_with(".mjs") || lower.ends_with(".cjs")
}

/// The Python interpreter actually present on this machine. `python` is absent
/// on many modern systems while `python3` is not, and a gate naming an absent
/// interpreter fails identically forever.
fn python_interpreter() -> Option<&'static str> {
    if binary_on_path("python") {
        Some("python")
    } else if binary_on_path("python3") {
        Some("python3")
    } else {
        None
    }
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Is `name` an executable on PATH? Equivalent to `which name`, implemented by
/// scanning PATH so the harness needs no external `which`.
pub fn binary_on_path(name: &str) -> bool {
    if name.is_empty() || name.contains('/') {
        return false;
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| is_executable_file(&dir.join(name)))
}

/// A leading `VAR=value` assignment; the command is the first non-assignment.
fn is_assignment(token: &str) -> bool {
    match token.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && !name.chars().next().unwrap().is_ascii_digit()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

/// Commands `/bin/sh -c` runs itself, with no binary to resolve.
fn is_shell_builtin(token: &str) -> bool {
    matches!(
        token,
        "cd" | "test"
            | "["
            | ":"
            | "true"
            | "false"
            | "echo"
            | "export"
            | "set"
            | "unset"
            | "source"
            | "."
            | "exec"
            | "exit"
            | "command"
            | "which"
            | "printf"
            | "read"
            | "shift"
            | "trap"
            | "wait"
            | "type"
            | "hash"
            | "pwd"
            | "return"
            | "break"
            | "continue"
            | "eval"
            | "umask"
            | "ulimit"
            | "times"
            | "getopts"
            | "if"
            | "for"
            | "while"
            | "until"
            | "case"
            | "then"
            | "do"
            | "done"
            | "fi"
            | "esac"
    )
}

/// Can this `verification` entry actually be executed?
///
/// Prose (`manual smoke test`) and an interpreter this machine lacks (`python`
/// where only `python3` exists) name no command: running them fails identically
/// forever, exhausting a node's retries and reverting work that may be correct.
/// Such entries are downgraded to a manual check the critic judges, rather than
/// run as a gate.
pub fn entry_is_runnable(root: &Path, command: &str) -> bool {
    let command = command.trim();
    if command.is_empty() {
        return false;
    }
    let Some(token) = command.split_whitespace().find(|t| !is_assignment(t)) else {
        return false;
    };
    if token.contains('/') {
        // A path command runs relative to the project root (the gate's cwd).
        let path = if token.starts_with('/') {
            PathBuf::from(token)
        } else {
            root.join(token)
        };
        return is_executable_file(&path);
    }
    if is_shell_builtin(token) {
        return true;
    }
    binary_on_path(token)
}

/// The verification entries that name no executable command.
pub fn unrunnable_entries(root: &Path, entries: &[String]) -> Vec<String> {
    entries
        .iter()
        .filter(|e| !entry_is_runnable(root, e))
        .cloned()
        .collect()
}

fn truncate_tail(text: &str) -> String {
    if text.len() <= OUTPUT_CAP {
        return text.to_string();
    }
    // Keep the tail: compiler and test runners put the failure summary last.
    let mut start = text.len() - OUTPUT_CAP;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    format!("... [truncated]\n{}", &text[start..])
}

/// Does this gate start a long-running process rather than a terminating check?
///
/// A contract may legitimately say "the app launches" (a TUI or a dev server).
/// Such a command never exits, so waiting for its exit code wedges the node for
/// the entire gate timeout and then FAILs a working launch. Recognising these
/// commands lets them run under a short liveness window instead.
pub fn is_launch_command(command: &str) -> bool {
    let tokens: Vec<String> = command
        .split_whitespace()
        .map(|t| t.to_ascii_lowercase())
        .collect();
    let Some(first) = tokens.first() else {
        return false;
    };
    if matches!(
        first.as_str(),
        "npm" | "yarn" | "pnpm" | "bun" | "cargo" | "go" | "python" | "python3"
    ) {
        let rest = &tokens[1..];
        let rest = if rest.first().map(String::as_str) == Some("run") {
            &rest[1..]
        } else {
            rest
        };
        if let Some(script) = rest.first() {
            if matches!(
                script.as_str(),
                "start" | "serve" | "dev" | "preview" | "watch"
            ) {
                return true;
            }
        }
    }
    // A bare JS entry point (`node dist/app.js`) is how a project with no npm
    // start script launches its app, so it is a launch: it must be smoke-tested
    // under the liveness window rather than waited on until the gate timeout.
    if matches!(first.as_str(), "node" | "nodejs") && tokens.iter().any(|t| is_js_entrypoint(t)) {
        return true;
    }
    // A bare process told to serve/watch. Exclude test and build commands so
    // `npm test --watch` is never mistaken for a launch.
    if !tokens
        .iter()
        .any(|t| t == "test" || t == "build" || t.contains("test"))
        && tokens
            .iter()
            .any(|t| t == "--serve" || t == "--watch" || t == "--hot")
    {
        return true;
    }
    false
}

#[cfg(unix)]
fn kill_process_group(child: &mut std::process::Child) {
    let pid = child.id();
    // The child leads its own process group (see `process_group(0)`), so signal
    // the whole group: `npm start` must not leave the launched app running after
    // the gate ends.
    let _ = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "kill -TERM -{pid} 2>/dev/null; sleep 0.3; kill -KILL -{pid} 2>/dev/null"
        ))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
    let _ = child.wait();
    std::thread::sleep(Duration::from_millis(100));
}

#[cfg(not(unix))]
fn kill_process_group(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Run a gate that is expected to keep running. Output goes to a log file rather
/// than a pipe: a launcher that forks children would otherwise keep the pipe's
/// write end open and block a read to EOF even after the direct child is killed.
fn run_launch_gate(root: &Path, command: &str, smoke_secs: u64) -> GateOutcome {
    let log_path = std::env::temp_dir().join(format!(
        "fractal_launch_{}_{}.log",
        std::process::id(),
        LAUNCH_LOG_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let log = match File::create(&log_path) {
        Ok(f) => f,
        Err(e) => {
            return GateOutcome {
                command: command.to_string(),
                passed: false,
                output: format!("could not create launch log: {e}"),
                manual: false,
            }
        }
    };
    let stderr = log.try_clone().ok();

    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log));
    if let Some(stderr) = stderr {
        cmd.stderr(Stdio::from(stderr));
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&log_path);
            return GateOutcome {
                command: command.to_string(),
                passed: false,
                output: format!("could not start gate: {e}"),
                manual: false,
            };
        }
    };

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut text = std::fs::read_to_string(&log_path).unwrap_or_default();
                let _ = std::fs::remove_file(&log_path);
                if text.trim().is_empty() {
                    text = format!("command exited with {status} before the launch window");
                }
                return GateOutcome {
                    command: command.to_string(),
                    passed: status.success(),
                    output: truncate_tail(text.trim()),
                    manual: false,
                };
            }
            Ok(None) => {
                if start.elapsed().as_secs() >= smoke_secs {
                    kill_process_group(&mut child);
                    let text = std::fs::read_to_string(&log_path).unwrap_or_default();
                    let _ = std::fs::remove_file(&log_path);
                    return GateOutcome {
                        command: command.to_string(),
                        passed: true,
                        output: truncate_tail(
                            format!(
                                "still running after {smoke_secs}s - treated as a successful launch\n{}",
                                text.trim()
                            )
                            .trim(),
                        ),
                        manual: false,
                    };
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&log_path);
                return GateOutcome {
                    command: command.to_string(),
                    passed: false,
                    output: format!("gate wait failed: {e}"),
                    manual: false,
                };
            }
        }
    }
}

pub fn run_gate(root: &Path, command: &str, timeout_secs: u64) -> GateOutcome {
    // An entry that names no executable command is a manual/visual check, not a
    // gate: running it would fail identically on every retry and revert work that
    // may be correct. Skip it and let the critic own the criterion.
    if !entry_is_runnable(root, command) {
        return GateOutcome {
            command: command.to_string(),
            passed: false,
            output: format!(
                "{command:?} is not an executable command on this machine; \
                 skipped as a manual check for the critic"
            ),
            manual: true,
        };
    }
    if is_launch_command(command) {
        return run_launch_gate(root, command, launch_smoke_secs());
    }
    let child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return GateOutcome {
                command: command.to_string(),
                passed: false,
                output: format!("could not start gate: {e}"),
                manual: false,
            }
        }
    };

    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = child.wait_with_output().ok();
                let mut text = String::new();
                if let Some(o) = out {
                    text.push_str(&String::from_utf8_lossy(&o.stdout));
                    text.push_str(&String::from_utf8_lossy(&o.stderr));
                }
                // The shell could not execute the command (missing/not
                // executable). That is a broken gate, not failed work.
                let manual = matches!(status.code(), Some(c) if SHELL_CANNOT_EXECUTE.contains(&c));
                return GateOutcome {
                    command: command.to_string(),
                    passed: !manual && status.success(),
                    output: truncate_tail(text.trim()),
                    manual,
                };
            }
            Ok(None) => {
                if start.elapsed().as_secs() > timeout_secs {
                    let _ = child.kill();
                    return GateOutcome {
                        command: command.to_string(),
                        passed: false,
                        output: format!("gate timed out after {timeout_secs}s"),
                        manual: false,
                    };
                }
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
            Err(e) => {
                return GateOutcome {
                    command: command.to_string(),
                    passed: false,
                    output: format!("gate wait failed: {e}"),
                    manual: false,
                }
            }
        }
    }
}

/// Run every gate. Stops at the first failure: later gates are usually
/// meaningless once the build is broken, and the first error is the actionable
/// one to feed back to the agent. A manual (unexecutable) entry is skipped, not
/// stopped on.
pub fn run_gates(root: &Path, gates: &[String], timeout_secs: u64) -> Vec<GateOutcome> {
    let mut outcomes = Vec::new();
    for gate in gates {
        let outcome = run_gate(root, gate, timeout_secs);
        let failed = !outcome.passed && !outcome.manual;
        outcomes.push(outcome);
        if failed {
            break;
        }
    }
    outcomes
}

/// The gates the harness actually executed, as first-class evidence for the
/// critic. The command, its outcome and the tail of its output are ground truth:
/// a criterion a passing gate covers is settled by that gate, not by whether the
/// file it depends on happens to be in this node's diff. Manual (unexecutable)
/// entries are shown as not run so the critic knows it owns them.
pub fn format_gate_evidence(outcomes: &[GateOutcome]) -> String {
    if outcomes.is_empty() {
        return String::new();
    }
    let mut text = String::from(
        "Automated gates (executed by the harness against the project on disk; treat \
         a PASS as ground truth - a criterion a passing gate covers is satisfied by \
         that gate):\n",
    );
    for o in outcomes {
        let status = if o.manual {
            "NOT RUN (downgraded to a manual check)"
        } else if o.passed {
            "PASS"
        } else {
            "FAIL"
        };
        text.push_str(&format!("\n$ {} -> {status}\n", o.command));
        if !o.output.is_empty() {
            text.push_str(&o.output);
            text.push('\n');
        }
    }
    text
}

/// Feedback an agent can act on: the exact command and the tail of its output.
/// Manual (unexecutable) entries are not failures and are omitted.
pub fn format_failures(outcomes: &[GateOutcome]) -> Option<String> {
    let failures: Vec<&GateOutcome> = outcomes.iter().filter(|o| !o.passed && !o.manual).collect();
    if failures.is_empty() {
        return None;
    }
    let mut text = String::from("Automated verification FAILED. Fix these before completing:\n");
    for failure in failures {
        text.push_str(&format!(
            "\n$ {}\n{}\n",
            failure.command,
            if failure.output.is_empty() {
                "(no output)"
            } else {
                &failure.output
            }
        ));
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fractal_verify_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn failing_gate_is_reported_as_failure() {
        let dir = temp_dir("fail");
        let outcome = run_gate(&dir, "exit 3", 10);
        assert!(!outcome.passed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn passing_gate_captures_output() {
        let dir = temp_dir("pass");
        let outcome = run_gate(&dir, "echo hello-gate", 10);
        assert!(outcome.passed);
        assert!(outcome.output.contains("hello-gate"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_gates_stops_at_first_failure() {
        let dir = temp_dir("stop");
        let gates = vec![
            "true".to_string(),
            "exit 1".to_string(),
            "echo never".to_string(),
        ];
        let outcomes = run_gates(&dir, &gates, 10);
        assert_eq!(outcomes.len(), 2, "must not run gates after a failure");
        assert!(!outcomes[1].passed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detect_gates_includes_typecheck_for_ts_project() {
        let dir = temp_dir("detect");
        std::fs::write(
            dir.join("package.json"),
            r#"{"scripts":{"build":"vite build","test":"vitest run"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
        let gates = detect_gates(&dir);
        assert!(
            gates.iter().any(|g| g.contains("tsc --noEmit")),
            "typecheck gate missing: {gates:?}"
        );
        assert!(gates.iter().any(|g| g.contains("npm run build")));
        assert!(gates.iter().any(|g| g.contains("npm test")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A TUI/server deliverable must be smoke-launched, not judged only on
    /// build/test that never start it. Trial 3's React 19 / ink 4 TUI crashed on
    /// `npm run dev` and no gate ever ran it.
    #[test]
    fn detect_gates_smoke_launches_a_dev_server() {
        let dir = temp_dir("detectlaunch");
        std::fs::write(
            dir.join("package.json"),
            r#"{"scripts":{"build":"vite build","test":"vitest run","dev":"tsx src/index.tsx"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
        let gates = detect_gates(&dir);
        let launch = gates
            .iter()
            .find(|g| is_launch_command(g))
            .unwrap_or_else(|| panic!("no launch gate emitted: {gates:?}"));
        assert_eq!(launch, "npm run dev");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Trial 5 §7.3: a TUI entry that is not wired to a start script must still
    /// be smoke-launched. The manifest's own `main` names it.
    #[test]
    fn detect_gates_derives_smoke_launch_from_main() {
        let dir = temp_dir("detectmain");
        std::fs::write(
            dir.join("package.json"),
            r#"{"main":"dist/app.js","scripts":{"build":"tsc","test":"jest"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
        let gates = detect_gates(&dir);
        let launch = gates
            .iter()
            .find(|g| is_launch_command(g))
            .unwrap_or_else(|| panic!("no launch gate emitted for a main entry: {gates:?}"));
        assert_eq!(launch, "node dist/app.js");
        assert!(entry_is_runnable(&dir, launch), "derived gate must run");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The motivating case: no `start` script and no `main`, but a conventional
    /// built entry (`dist/app.js`) that run B launched by hand.
    #[test]
    fn detect_gates_derives_smoke_launch_from_a_built_entry() {
        let dir = temp_dir("detectdist");
        std::fs::write(
            dir.join("package.json"),
            r#"{"scripts":{"build":"tsc","test":"jest"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        std::fs::write(dir.join("dist/app.js"), "console.log('tui')\n").unwrap();
        let gates = detect_gates(&dir);
        assert!(
            gates.iter().any(|g| g == "node dist/app.js"),
            "a built entry with no start script must still be smoke-launched: {gates:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_contract_gates_apply_to_every_scope() {
        let dir = temp_dir("explicit");
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        let explicit = vec!["make verify".to_string()];
        assert_eq!(resolve_gates(&dir, &explicit, GateScope::Leaf), explicit);
        assert_eq!(
            resolve_gates(&dir, &explicit, GateScope::Integration),
            explicit
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression guard for a deadlock: whole-project gates on leaves would fail
    /// the first leaf (its siblings' modules do not exist yet), burn its retries
    /// and stall the whole tree before integration.
    #[test]
    fn leaves_do_not_inherit_whole_project_gates() {
        let dir = temp_dir("leafscope");
        std::fs::write(
            dir.join("package.json"),
            r#"{"scripts":{"build":"vite build","test":"vitest run"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();

        assert!(
            resolve_gates(&dir, &[], GateScope::Leaf).is_empty(),
            "a leaf must not be gated on the whole project building"
        );
        assert!(
            !resolve_gates(&dir, &[], GateScope::Integration).is_empty(),
            "an integrating parent must run the project's own build/test suite"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_failures_is_none_when_all_pass() {
        let outcomes = vec![GateOutcome {
            command: "true".into(),
            passed: true,
            output: String::new(),
            manual: false,
        }];
        assert!(format_failures(&outcomes).is_none());
    }

    #[test]
    fn format_failures_names_the_command() {
        let outcomes = vec![GateOutcome {
            command: "npx tsc --noEmit".into(),
            passed: false,
            output: "error TS2322".into(),
            manual: false,
        }];
        let text = format_failures(&outcomes).unwrap();
        assert!(text.contains("npx tsc --noEmit"));
        assert!(text.contains("error TS2322"));
    }

    /// H1 regression: a prose entry is not a command. It must be skipped as a
    /// manual check rather than run and failed forever.
    #[test]
    fn unrunnable_prose_entry_is_a_manual_check_not_a_failure() {
        let dir = temp_dir("prosegate");
        let outcome = run_gate(&dir, "manual smoke test", 10);
        assert!(outcome.manual, "prose must be downgraded to manual");
        assert!(
            format_failures(std::slice::from_ref(&outcome)).is_none(),
            "a manual check must never be reported as a gate failure"
        );
        // The pre-flight is what drives this, without spawning a shell.
        assert!(!entry_is_runnable(&dir, "manual smoke test"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// H1: a command whose binary is absent (the trial's `python`, or an
    /// unknown tool) is likewise skipped instead of dooming the node.
    #[test]
    fn unrunnable_missing_binary_entry_is_a_manual_check() {
        let dir = temp_dir("missingbin");
        let outcome = run_gate(&dir, "definitely-not-a-real-binary-xyz --check", 10);
        assert!(outcome.manual, "a missing binary must be manual");
        assert!(!entry_is_runnable(
            &dir,
            "definitely-not-a-real-binary-xyz --check"
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Even when the first token resolves, a shell that cannot execute the
    /// command (exit 127) is reported as a manual check, not failed work.
    #[test]
    fn shell_command_not_found_is_a_manual_check() {
        let dir = temp_dir("cf127");
        let outcome = run_gate(&dir, "env definitely-missing-inside-xyz", 10);
        assert!(
            outcome.manual,
            "shell exit 127 must downgrade to manual: {}",
            outcome.output
        );
        assert!(
            format_failures(std::slice::from_ref(&outcome)).is_none(),
            "a 127 gate must not fail the node"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A genuine failing gate still fails: the downgrade must not swallow real
    /// verification.
    #[test]
    fn genuine_failing_gate_is_still_a_failure() {
        let dir = temp_dir("realgate");
        let outcome = run_gate(&dir, "exit 3", 10);
        assert!(!outcome.passed && !outcome.manual);
        assert!(format_failures(std::slice::from_ref(&outcome)).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn entry_runnable_recognises_builtins_assignments_and_paths() {
        let dir = temp_dir("runnable");
        assert!(entry_is_runnable(&dir, "cd sub && cargo test"));
        assert!(entry_is_runnable(&dir, "FOO=bar echo hi"));
        assert!(entry_is_runnable(&dir, "true"));
        assert!(entry_is_runnable(&dir, "exit 0"));
        assert!(!entry_is_runnable(&dir, ""));
        assert!(!entry_is_runnable(&dir, "   "));
        // A path must exist and be executable relative to the project root.
        assert!(!entry_is_runnable(&dir, "./script.sh"));
        let script = dir.join("script.sh");
        std::fs::write(&script, "#!/bin/sh\ntrue\n").unwrap();
        assert!(
            !entry_is_runnable(&dir, "./script.sh"),
            "a non-executable file is not runnable"
        );
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(entry_is_runnable(&dir, "./script.sh"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unrunnable_entries_reports_each_offender() {
        let dir = temp_dir("offenders");
        let entries = vec![
            "true".to_string(),
            "manual smoke test".to_string(),
            "python -c \"print(1)\"".to_string(),
        ];
        // `python` may or may not exist on the test box; whichever it is, the
        // invariant is that anything reported is genuinely unrunnable.
        let bad = unrunnable_entries(&dir, &entries);
        assert!(bad.contains(&"manual smoke test".to_string()));
        assert!(bad.iter().all(|e| !entry_is_runnable(&dir, e)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every gate `detect_gates` emits must actually be runnable, so an
    /// inferred gate can never doom a valid project.
    #[test]
    fn detect_gates_only_emits_runnable_commands() {
        let dir = temp_dir("detectrunnable");
        std::fs::write(
            dir.join("package.json"),
            r#"{"scripts":{"build":"vite build","test":"vitest run"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("pyproject.toml"), "[project]").unwrap();
        for gate in detect_gates(&dir) {
            assert!(
                entry_is_runnable(&dir, &gate),
                "detect_gates emitted an unexecutable gate: {gate}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Python is probed, not assumed: when only `python3` exists the gate uses
    /// it rather than the absent `python`.
    #[test]
    fn python_gate_uses_an_available_interpreter() {
        let dir = temp_dir("pythongate");
        std::fs::write(dir.join("pyproject.toml"), "[project]").unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        let gates = detect_gates(&dir);
        let py = gates.iter().find(|g| g.contains("pytest"));
        match py {
            Some(gate) => {
                assert!(
                    entry_is_runnable(&dir, gate),
                    "python gate must name an installed interpreter: {gate}"
                );
            }
            None => {
                assert!(
                    python_interpreter().is_none(),
                    "a python gate must be emitted when an interpreter exists"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn launch_commands_are_recognised() {
        assert!(is_launch_command("npm start"));
        assert!(is_launch_command("npm run dev"));
        assert!(is_launch_command("yarn serve"));
        assert!(is_launch_command("pnpm run preview"));
        assert!(is_launch_command("node dist/index.js --serve"));
        assert!(is_launch_command("node dist/app.js"));
        assert!(is_launch_command("node dist/cli.cjs"));
        assert!(is_launch_command("cargo run -- --watch"));
        assert!(!is_launch_command("npm test"));
        assert!(!is_launch_command("npm run build"));
        assert!(!is_launch_command("npm test --watch"));
        assert!(!is_launch_command(""));
    }

    /// D4 regression: a gate that launches a long-running app must PASS once it
    /// is demonstrably alive, instead of blocking for the whole gate timeout and
    /// failing a working launch.
    #[test]
    fn a_live_launch_gate_passes_with_captured_output() {
        let dir = temp_dir("launch");
        let outcome = run_launch_gate(&dir, "echo launched-ok; exec sleep 30", 1);
        assert!(
            outcome.passed,
            "a live launch must be treated as success: {}",
            outcome.output
        );
        assert!(
            outcome.output.contains("still running"),
            "launch pass must say it was alive: {}",
            outcome.output
        );
        assert!(
            outcome.output.contains("launched-ok"),
            "launch pass must capture output: {}",
            outcome.output
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_launch_that_crashes_immediately_fails() {
        let dir = temp_dir("launchcrash");
        let outcome = run_launch_gate(&dir, "exit 7", 1);
        assert!(
            !outcome.passed,
            "a launch that exits non-zero is not a launch"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_early_successful_exit_is_still_a_pass() {
        let dir = temp_dir("launchexit");
        let outcome = run_launch_gate(&dir, "echo done", 1);
        assert!(outcome.passed, "a launch that exits 0 is a pass");
        assert!(outcome.output.contains("done"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The critic must be told which gates ran, that they passed, and what they
    /// printed - that is the evidence a criterion can rest on when the file it
    /// depends on is absent from the diff.
    #[test]
    fn gate_evidence_names_the_command_pass_and_output() {
        let outcomes = vec![GateOutcome {
            command: "npm start".to_string(),
            passed: true,
            output: "listening on :3000".to_string(),
            manual: false,
        }];
        let text = format_gate_evidence(&outcomes);
        assert!(text.contains("npm start"), "command missing: {text}");
        assert!(text.contains("PASS"), "outcome missing: {text}");
        assert!(
            text.contains("listening on :3000"),
            "output missing: {text}"
        );
    }

    /// A manual entry was never executed, so the critic must not read it as a
    /// pass or a failure.
    #[test]
    fn gate_evidence_marks_a_manual_entry_as_not_run() {
        let outcomes = vec![GateOutcome {
            command: "look at the layout".to_string(),
            passed: false,
            output: "skipped".to_string(),
            manual: true,
        }];
        let text = format_gate_evidence(&outcomes);
        assert!(text.contains("NOT RUN"), "manual entry mislabelled: {text}");
        assert!(
            !text.contains("-> PASS"),
            "manual entry read as pass: {text}"
        );
    }
}
