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

use std::path::Path;
use std::process::{Command, Stdio};

const OUTPUT_CAP: usize = 4000;

#[derive(Debug, Clone)]
pub struct GateOutcome {
    pub command: String,
    pub passed: bool,
    pub output: String,
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
        if root.join("tsconfig.json").exists() {
            gates.push("npx tsc --noEmit".to_string());
        }
        if scripts.contains("\"build\"") {
            gates.push("npm run build".to_string());
        }
        if scripts.contains("\"test\"") {
            gates.push("npm test --silent".to_string());
        }
    }

    if root.join("Cargo.toml").exists() {
        gates.push("cargo check --all-targets".to_string());
        gates.push("cargo test".to_string());
    }

    if root.join("pyproject.toml").exists() && root.join("tests").exists() {
        gates.push("python -m pytest -q".to_string());
    }

    gates
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

pub fn run_gate(root: &Path, command: &str, timeout_secs: u64) -> GateOutcome {
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
                return GateOutcome {
                    command: command.to_string(),
                    passed: status.success(),
                    output: truncate_tail(text.trim()),
                };
            }
            Ok(None) => {
                if start.elapsed().as_secs() > timeout_secs {
                    let _ = child.kill();
                    return GateOutcome {
                        command: command.to_string(),
                        passed: false,
                        output: format!("gate timed out after {timeout_secs}s"),
                    };
                }
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
            Err(e) => {
                return GateOutcome {
                    command: command.to_string(),
                    passed: false,
                    output: format!("gate wait failed: {e}"),
                }
            }
        }
    }
}

/// Run every gate. Stops at the first failure: later gates are usually
/// meaningless once the build is broken, and the first error is the actionable
/// one to feed back to the agent.
pub fn run_gates(root: &Path, gates: &[String], timeout_secs: u64) -> Vec<GateOutcome> {
    let mut outcomes = Vec::new();
    for gate in gates {
        let outcome = run_gate(root, gate, timeout_secs);
        let failed = !outcome.passed;
        outcomes.push(outcome);
        if failed {
            break;
        }
    }
    outcomes
}

/// Feedback an agent can act on: the exact command and the tail of its output.
pub fn format_failures(outcomes: &[GateOutcome]) -> Option<String> {
    let failures: Vec<&GateOutcome> = outcomes.iter().filter(|o| !o.passed).collect();
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
        let dir = std::env::temp_dir().join(format!("fractal_verify_{}_{}", name, std::process::id()));
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
        let gates = vec!["true".to_string(), "exit 1".to_string(), "echo never".to_string()];
        let outcomes = run_gates(&dir, &gates, 10);
        assert_eq!(outcomes.len(), 2, "must not run gates after a failure");
        assert!(!outcomes[1].passed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detect_gates_includes_typecheck_for_ts_project() {
        let dir = temp_dir("detect");
        std::fs::write(dir.join("package.json"), r#"{"scripts":{"build":"vite build","test":"vitest run"}}"#).unwrap();
        std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
        let gates = detect_gates(&dir);
        assert!(gates.iter().any(|g| g.contains("tsc --noEmit")), "typecheck gate missing: {gates:?}");
        assert!(gates.iter().any(|g| g.contains("npm run build")));
        assert!(gates.iter().any(|g| g.contains("npm test")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_contract_gates_apply_to_every_scope() {
        let dir = temp_dir("explicit");
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        let explicit = vec!["make verify".to_string()];
        assert_eq!(resolve_gates(&dir, &explicit, GateScope::Leaf), explicit);
        assert_eq!(resolve_gates(&dir, &explicit, GateScope::Integration), explicit);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression guard for a deadlock: whole-project gates on leaves would fail
    /// the first leaf (its siblings' modules do not exist yet), burn its retries
    /// and stall the whole tree before integration.
    #[test]
    fn leaves_do_not_inherit_whole_project_gates() {
        let dir = temp_dir("leafscope");
        std::fs::write(dir.join("package.json"), r#"{"scripts":{"build":"vite build","test":"vitest run"}}"#).unwrap();
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
        let outcomes = vec![GateOutcome { command: "true".into(), passed: true, output: String::new() }];
        assert!(format_failures(&outcomes).is_none());
    }

    #[test]
    fn format_failures_names_the_command() {
        let outcomes = vec![GateOutcome {
            command: "npx tsc --noEmit".into(),
            passed: false,
            output: "error TS2322".into(),
        }];
        let text = format_failures(&outcomes).unwrap();
        assert!(text.contains("npx tsc --noEmit"));
        assert!(text.contains("error TS2322"));
    }
}
