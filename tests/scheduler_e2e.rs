//! End-to-end scheduler tests driven by a fake `omp` executor.
//!
//! These drive the real `fractal` binary in a throwaway project with a shell
//! script standing in for `omp` on `PATH`. No model, no network, no paid
//! inference: the fake emits a fixed decision (or verdict for the critic) so the
//! test exercises the scheduler, verification, git attribution and terminal
//! states for real.
//!
//! The non-ignored test asserts only behaviour that is currently true, so CI
//! stays green. The `#[ignore]`d tests record the terminal states the follow-up
//! reliability task must restore (upward escalation, deterministic termination
//! after failure, and fail-closed no-decision handling). They are expected to
//! hang or fail until that task lands, which is the point; run them with
//! `cargo test -- --ignored` once it does.

#![cfg(unix)]

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const FAKE_OMP: &str = r#"#!/usr/bin/env bash
# Fake omp executor for end-to-end scheduler tests.
MODE="${FAKE_OMP_MODE:-happy}"
NODE="${FRACTAL_NODE_ID:-}"

# Critic invocations carry no FRACTAL_NODE_ID, so always pass.
if [ -z "$NODE" ]; then
  echo '{"verdict":"PASS","reason":"fake critic always passes","criteria":[]}'
  exit 0
fi

ROOT=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--cwd" ]; then ROOT="$a"; fi
  prev="$a"
done

case "$MODE" in
  happy)
    if [ "$NODE" = "root" ] && [ ! -d "$ROOT/tree/root/children/root-01" ]; then
      echo '{"verb":"split","subtasks":[{"id":"a","goal":"do the only task","acceptance_criteria":["the task is done"]}]}'
    else
      echo '{"verb":"complete","deliverable":"done","summary":"did the only task"}'
    fi
    ;;
  silent)
    echo "working..."
    ;;
  escalate)
    if [ "$NODE" = "root-01" ]; then
      echo '{"verb":"escalate","assumption":"the inherited constraint is false","evidence":"observed otherwise"}'
    else
      echo '{"verb":"split","subtasks":[{"id":"a","goal":"do the only task","acceptance_criteria":["done"]}]}'
    fi
    ;;
  fail)
    echo '{"verb":"not_a_real_verb"}'
    ;;
  *)
    echo '{"verb":"complete","deliverable":"done","summary":"unknown fake mode"}'
    ;;
esac
"#;

struct Project {
    dir: PathBuf,
    fake_bin: PathBuf,
    mode: &'static str,
}

impl Project {
    fn new(name: &str, mode: &'static str) -> Self {
        let base =
            std::env::temp_dir().join(format!("fractal_e2e_{}_{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let dir = base.join("project");
        let fake_bin = base.join("fake-bin");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&fake_bin).unwrap();
        let omp = fake_bin.join("omp");
        fs::write(&omp, FAKE_OMP).unwrap();
        fs::set_permissions(&omp, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            dir,
            fake_bin,
            mode,
        }
    }

    /// Run the binary in the project with the fake executor on PATH.
    fn run(&self, args: &[&str], timeout: Duration) -> (Option<i32>, String, String) {
        let path = format!(
            "{}:{}",
            self.fake_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut child: Child = Command::new(env!("CARGO_BIN_EXE_fractal"))
            .args(args)
            .current_dir(&self.dir)
            .env("PATH", path)
            .env("FAKE_OMP_MODE", self.mode)
            .env("FRACTAL_MODEL", "default")
            .env("FRACTAL_TIMEOUT", "10")
            .env("FRACTAL_MAX_STEPS", "30")
            .env("FRACTAL_PARALLEL", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait().unwrap() {
                Some(status) => {
                    let mut out = String::new();
                    let mut err = String::new();
                    if let Some(mut o) = child.stdout.take() {
                        let _ = o.read_to_string(&mut out);
                    }
                    if let Some(mut e) = child.stderr.take() {
                        let _ = e.read_to_string(&mut err);
                    }
                    return (Some(status.code().unwrap_or(-1)), out, err);
                }
                None => {
                    if Instant::now() > deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return (None, String::new(), String::new());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    fn status(&self) -> String {
        let (_, out, err) = self.run(&["status"], Duration::from_secs(20));
        format!("{out}\n{err}")
    }
}

/// The happy path: the root splits, its child completes and verifies, the root
/// aggregates and completes, and the run exits cleanly with both nodes terminal.
#[test]
fn happy_path_reaches_terminal_success() {
    let p = Project::new("happy", "happy");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(60));
    assert_eq!(
        code,
        Some(0),
        "expected the run to complete successfully; stderr:\n{err}"
    );

    let status = p.status();
    assert!(
        status.contains("[complete]"),
        "no completed node in:\n{status}"
    );
    assert!(
        !status.contains("[running]"),
        "a node was left running:\n{status}"
    );
    assert!(
        !status.contains("[failed]"),
        "a node failed on the happy path:\n{status}"
    );
}

/// Desired: an escalated node must not be wedged `running`. The owner is
/// reopened and the node ends resolved or terminal, and the run terminates.
///
/// Ignored until the follow-up reliability task restores the escalation
/// lifecycle (audit F1). Today the escalated child stays `running` and a headless
/// run never terminates.
#[test]
#[ignore = "follow-up reliability task: restore upward escalation (audit F1)"]
fn escalation_reaches_a_terminal_state() {
    let p = Project::new("escalate", "escalate");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(30));
    assert!(code.is_some(), "run did not terminate; stderr:\n{err}");
    let status = p.status();
    assert!(
        !status.contains("[running]"),
        "escalated node left running:\n{status}"
    );
}

/// Desired: a failed node ends the run deterministically instead of spinning
/// forever waiting for a keystroke that cannot come in CI.
///
/// Ignored until the follow-up reliability task makes headless runs terminate
/// (audit F3).
#[test]
#[ignore = "follow-up reliability task: deterministic termination (audit F3)"]
fn failure_reaches_a_terminal_state() {
    let p = Project::new("fail", "fail");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(30));
    assert!(code.is_some(), "run did not terminate; stderr:\n{err}");
    assert_eq!(code, Some(1), "failed tree should not report success");
    assert!(
        p.status().contains("[failed]"),
        "failure was not recorded:\n{}",
        p.status()
    );
}

/// Desired: an executor that produces no parseable decision is an error that is
/// retried, then fails the node. It must never be silently accepted as complete.
///
/// Ignored until the follow-up reliability task makes "no decision" fail closed
/// (audit F2).
#[test]
#[ignore = "follow-up reliability task: fail-closed no-decision handling (audit F2)"]
fn no_decision_is_not_a_fabricated_success() {
    let p = Project::new("silent", "silent");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(30));
    assert!(code.is_some(), "run did not terminate; stderr:\n{err}");
    assert_ne!(code, Some(0), "a silent executor must not yield success");
    let status = p.status();
    assert!(
        !status.contains("[complete]"),
        "a no-decision node was marked complete:\n{status}"
    );
}
