//! End-to-end scheduler tests driven by a fake `omp` executor.
//!
//! These drive the real `fractal` binary in a throwaway project with a shell
//! script standing in for `omp` on `PATH`. No model, no network, no paid
//! inference: the fake emits a fixed decision (or verdict for the critic) so the
//! test exercises the scheduler, verification, git attribution and terminal
//! states for real.
//!
//! The fake writes a real file whenever it "completes", because completion is
//! now justified by the actual diff: a node that only describes its work and
//! changes nothing on disk fails verification by design.

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

ROOT=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--cwd" ]; then ROOT="$a"; fi
  prev="$a"
done

complete() {
  mkdir -p "$ROOT/src"
  echo "delivered by ${NODE:-critic}" > "$ROOT/src/${NODE:-out}.txt"
  echo "{\"verb\":\"complete\",\"deliverable\":\"done\",\"summary\":\"did the task\"}"
}

# Critic invocations carry no FRACTAL_NODE_ID, so always pass with a real
# per-criterion result (a bare PASS with no criteria is now a FAIL).
if [ -z "$NODE" ]; then
  echo '{"verdict":"PASS","reason":"fake critic passes","criteria":[{"name":"the task is done","pass":true,"reason":"fake"}]}'
  exit 0
fi

case "$MODE" in
  happy)
    if [ "$NODE" = "root" ] && [ ! -d "$ROOT/tree/root/children/root-01" ]; then
      echo '{"verb":"split","subtasks":[{"id":"a","goal":"do the only task","acceptance_criteria":["the task is done"]}]}'
    else
      complete
    fi
    ;;
  silent)
    echo "working..."
    ;;
  escalate)
    ESC="$ROOT/.fractal_decision_esc"
    RES="$ROOT/.fractal_decision_res"
    if [ "$NODE" = "root" ]; then
      if [ -f "$ESC" ] && [ ! -f "$RES" ]; then
        touch "$RES"
        echo '{"verb":"escalate_resolve","resolution":"overrule","rationale":"the constraint still holds; proceed"}'
      elif [ -d "$ROOT/tree/root/children/root-01" ]; then
        complete
      else
        echo '{"verb":"split","subtasks":[{"id":"a","goal":"do the only task","acceptance_criteria":["the task is done"]}]}'
      fi
    elif [ "$NODE" = "root-01" ]; then
      if [ -f "$RES" ]; then
        complete
      else
        touch "$ESC"
        echo '{"verb":"escalate","assumption":"the inherited constraint is false","evidence":"observed otherwise"}'
      fi
    else
      complete
    fi
    ;;
  fail)
    echo '{"verb":"not_a_real_verb"}'
    ;;
  resolve_unprompted)
    echo '{"verb":"escalate_resolve","resolution":"amend","amended_constraint":"x"}'
    ;;
  empty_diff)
    if [ "$NODE" = "root" ] && [ ! -d "$ROOT/tree/root/children/root-01" ]; then
      echo '{"verb":"split","subtasks":[{"id":"a","goal":"do the only task","acceptance_criteria":["the task is done"]}]}'
    else
      # Claims completion while changing nothing on disk.
      echo '{"verb":"complete","deliverable":"done","summary":"trust me"}'
    fi
    ;;
  gate_fail)
    if [ "$NODE" = "root" ] && [ ! -d "$ROOT/tree/root/children/root-01" ]; then
      echo '{"verb":"split","subtasks":[{"id":"a","goal":"do the only task","acceptance_criteria":["the task is done"],"verification":["false"]}]}'
    else
      complete
    fi
    ;;
  *)
    complete
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

/// An escalated node must not be wedged `running`. The owner is reopened, the
/// node resumes under the owner's ruling, and the run terminates successfully.
#[test]
fn escalation_reaches_a_terminal_state() {
    let p = Project::new("escalate", "escalate");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(30));
    assert!(code.is_some(), "run did not terminate; stderr:\n{err}");
    assert_eq!(
        code,
        Some(0),
        "a settled escalation should let the tree complete; stderr:\n{err}"
    );
    let status = p.status();
    assert!(
        !status.contains("[running]"),
        "escalated node left running:\n{status}"
    );
    assert!(
        !status.contains("[failed]"),
        "escalation failed instead of resolving:\n{status}"
    );
    assert!(
        status.contains("[complete]"),
        "tree did not reach completion after escalation:\n{status}"
    );

    // The challenged assumption must not have been written into the child's
    // contract as an accepted constraint before the owner ruled on it.
    let contract_path = p.dir.join("tree/root/children/root-01/contract.md");
    let contract = fs::read_to_string(&contract_path).unwrap_or_default();
    assert!(
        !contract.contains("the inherited constraint is false"),
        "a challenged assumption was injected as a constraint before the owner ruled:\n{contract}"
    );
}

/// A leaf that claims completion while changing nothing on disk fails
/// verification: completion must be justified by a real diff, not prose.
#[test]
fn empty_diff_leaf_does_not_complete() {
    let p = Project::new("emptydiff", "empty_diff");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(30));
    assert!(code.is_some(), "run did not terminate; stderr:\n{err}");
    assert_ne!(code, Some(0), "an empty-diff leaf must not yield success");
    let status = p.status();
    assert!(
        !status.contains("[complete]"),
        "a leaf that changed nothing was marked complete:\n{status}"
    );
    assert!(
        status.contains("[failed]"),
        "empty-diff completion should fail the node:\n{status}"
    );
}

/// A leaf runs the executable gates its contract declares, and a failing gate
/// blocks completion.
#[test]
fn declared_leaf_gate_is_enforced() {
    let p = Project::new("gatefail", "gate_fail");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(30));
    assert!(code.is_some(), "run did not terminate; stderr:\n{err}");
    assert_ne!(code, Some(0), "a failing leaf gate must not yield success");
    let status = p.status();
    assert!(
        !status.contains("[complete]"),
        "a node whose declared gate failed was marked complete:\n{status}"
    );
}

/// A failed node ends the run deterministically instead of spinning forever
/// waiting for a keystroke that cannot come in CI.
#[test]
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

/// An executor that produces no parseable decision is an error that is retried,
/// then fails the node. It must never be silently accepted as complete.
#[test]
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

/// A node that returns `escalate_resolve` without any escalation to settle is
/// failing closed: the run terminates with a failed node instead of hanging.
#[test]
fn unprompted_escalate_resolve_fails_closed() {
    let p = Project::new("unprompted", "resolve_unprompted");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(30));
    assert!(code.is_some(), "run did not terminate; stderr:\n{err}");
    assert_ne!(code, Some(0));
    let status = p.status();
    assert!(
        !status.contains("[running]"),
        "node left running:\n{status}"
    );
    assert!(
        status.contains("[failed]"),
        "unprompted escalate_resolve should fail the node:\n{status}"
    );
}

/// A headless run accepts an explicit model without ever opening the picker.
#[test]
fn explicit_model_flag_runs_headless() {
    let p = Project::new("model", "happy");
    let (code, _out, err) = p.run(
        &["--model", "explicit-test-model", "init", "build a toy"],
        Duration::from_secs(60),
    );
    assert_eq!(code, Some(0), "explicit model run failed; stderr:\n{err}");
}
