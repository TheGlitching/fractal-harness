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

# Critic invocations carry no FRACTAL_NODE_ID. The full critic prompt is passed
# inline as the last argument, so echo the node's own acceptance criteria back
# verbatim (a verdict grading foreign criteria is now rejected, and a bare PASS
# with no criteria has always been a FAIL).
if [ -z "$NODE" ]; then
  CRIT="$(printf '%s' "${!#}" | awk '
    /^Acceptance criteria:/ {grab=1; next}
    /^Deliverable summary:/ {grab=0}
    grab && /^  - / {sub(/^  - /, ""); n++; printf "%s{\"name\":\"%s\",\"pass\":true,\"reason\":\"fake\"}", (n>1?",":""), $0}
  ')"
  if [ -z "$CRIT" ]; then
    CRIT='{"name":"the task is done","pass":true,"reason":"fake"}'
  fi
  echo "{\"verdict\":\"PASS\",\"reason\":\"fake critic passes\",\"criteria\":[${CRIT}]}"
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
  launch_gate)
    if [ "$NODE" = "root" ] && [ ! -d "$ROOT/tree/root/children/root-01" ]; then
      echo '{"verb":"split","subtasks":[{"id":"a","goal":"launch the app","acceptance_criteria":["the task is done"],"verification":["npm start"]}]}'
    else
      complete
    fi
    ;;
  chain)
    # Always split, proposing the node's remaining allowance minus the fee, so
    # depth is bounded only by the budget (the hard cap is a backstop).
    CLAUDE="${!#}"
    CLAUDE="${CLAUDE#*@}"
    CLAUDE="${CLAUDE%%. You are*}"
    REM=0
    if [ -f "$CLAUDE" ]; then
      CONTENT="$(cat "$CLAUDE")"
      if [[ "$CONTENT" =~ remaining:\ (-?[0-9]+) ]]; then REM="${BASH_REMATCH[1]}"; fi
    fi
    FEE="${FRACTAL_SPLIT_FEE:-200}"
    ALLOC=$(( REM - FEE ))
    if [ "$ALLOC" -lt 0 ]; then ALLOC=0; fi
    echo "{\"verb\":\"split\",\"subtasks\":[{\"id\":\"c\",\"goal\":\"chain ${NODE}\",\"acceptance_criteria\":[\"the task is done\"],\"allocation\":${ALLOC}}]}"
    ;;
  overalloc)
    if [ "$NODE" = "root" ] && [ ! -f "$ROOT/.attempted_overalloc" ]; then
      touch "$ROOT/.attempted_overalloc"
      echo '{"verb":"split","subtasks":[{"id":"a","goal":"a","allocation":100000000},{"id":"b","goal":"b","allocation":100000000}]}'
    else
      complete
    fi
    ;;
  partial_fail)
    if [ "$NODE" = "root" ] && [ ! -d "$ROOT/tree/root/children/root-01" ]; then
      echo '{"verb":"split","subtasks":[{"id":"a","goal":"do the only task","acceptance_criteria":["the task is done"]}]}'
    else
      mkdir -p "$ROOT/src"
      echo "half written" > "$ROOT/src/leftover_${NODE}.txt"
      echo '{"verb":"not_a_real_verb"}'
    fi
    ;;
  two_leaves)
    if [ "$NODE" = "root" ]; then
      if [ ! -d "$ROOT/tree/root/children/root-01" ]; then
        echo '{"verb":"split","subtasks":[{"id":"a","goal":"first leaf","acceptance_criteria":["the task is done"]},{"id":"b","goal":"second leaf","acceptance_criteria":["the task is done"]}]}'
      else
        echo '{"verb":"complete","deliverable":"children aggregated","summary":"aggregated"}'
      fi
    else
      complete
    fi
    ;;
  hang)
    echo "$$" > "$ROOT/.fake_omp_pid"
    exec sleep 120
    ;;
  *)
    complete
    ;;
esac
"#;

/// A minimal stand-in for the `opencode` CLI. It speaks opencode's invocation
/// (`run --auto`, project root as working directory) and nothing of `omp`, so a
/// run that succeeds against it proves the opencode executor really executed.
const FAKE_OPENCODE: &str = r#"#!/usr/bin/env bash
ROOT="$(pwd)"
NODE="${FRACTAL_NODE_ID:-}"
# Prove opencode, not omp, served this run.
echo "opencode" > "$ROOT/.opencode_ran"

if [ -z "$NODE" ]; then
  CRIT="$(printf '%s' "${!#}" | awk '
    /^Acceptance criteria:/ {grab=1; next}
    /^Deliverable summary:/ {grab=0}
    grab && /^  - / {sub(/^  - /, ""); n++; printf "%s{\"name\":\"%s\",\"pass\":true,\"reason\":\"fake\"}", (n>1?",":""), $0}
  ')"
  if [ -z "$CRIT" ]; then
    CRIT='{"name":"the task is done","pass":true,"reason":"fake"}'
  fi
  echo "{\"verdict\":\"PASS\",\"reason\":\"fake opencode critic passes\",\"criteria\":[${CRIT}]}"
  exit 0
fi

if [ "$NODE" = "root" ] && [ ! -d "$ROOT/tree/root/children/root-01" ]; then
  echo '{"verb":"split","subtasks":[{"id":"a","goal":"do the only task","acceptance_criteria":["the task is done"]}]}'
else
  mkdir -p "$ROOT/src"
  echo "delivered by opencode" > "$ROOT/src/${NODE}.txt"
  echo '{"verb":"complete","deliverable":"done","summary":"did the task with opencode"}'
fi
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
        let p = Self {
            dir,
            fake_bin,
            mode,
        };
        p.install("omp", FAKE_OMP);
        p
    }

    /// Install an executable into the fake bin directory.
    fn install(&self, name: &str, script: &str) {
        let path = self.fake_bin.join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn spawn(&self, args: &[&str], extra: &[(&str, &str)]) -> Child {
        let path = format!(
            "{}:{}",
            self.fake_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_fractal"));
        cmd.args(args)
            .current_dir(&self.dir)
            .env("PATH", path)
            .env("FAKE_OMP_MODE", self.mode)
            .env("FRACTAL_MODEL", "default")
            .env("FRACTAL_TIMEOUT", "10")
            .env("FRACTAL_MAX_STEPS", "30")
            .env("FRACTAL_PARALLEL", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd.spawn().unwrap()
    }

    fn wait(mut child: Child, timeout: Duration) -> (Option<i32>, String, String) {
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

    /// Run the binary in the project with the fake executor on PATH.
    fn run(&self, args: &[&str], timeout: Duration) -> (Option<i32>, String, String) {
        self.run_env(args, timeout, &[])
    }

    fn run_env(
        &self,
        args: &[&str],
        timeout: Duration,
        extra: &[(&str, &str)],
    ) -> (Option<i32>, String, String) {
        Self::wait(self.spawn(args, extra), timeout)
    }

    fn status(&self) -> String {
        let (_, out, err) = self.run(&["status"], Duration::from_secs(20));
        format!("{out}\n{err}")
    }

    /// Run a git command in the project repo and return its stdout.
    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Sum of every budget row's own-debits column.
    fn ledger_debits(&self) -> i64 {
        let db = self.dir.join(".fractal/index.db");
        let conn =
            rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let mut stmt = conn.prepare("SELECT debits FROM budget").unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0)).unwrap();
        rows.map(|r| r.unwrap()).sum()
    }

    fn ledger_calls_plus_fees(&self) -> i64 {
        let db = self.dir.join(".fractal/index.db");
        let conn =
            rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        conn.query_row(
            "SELECT COALESCE(SUM(calls + fee_paid), 0) FROM budget",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn ledger_calls(&self) -> i64 {
        let db = self.dir.join(".fractal/index.db");
        let conn =
            rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        conn.query_row("SELECT COALESCE(SUM(calls), 0) FROM budget", [], |r| {
            r.get(0)
        })
        .unwrap()
    }

    /// Depth of the on-disk tree: root alone is 1.
    fn max_depth(&self) -> usize {
        fn walk(dir: &std::path::Path) -> usize {
            let mut best = 0;
            if let Ok(entries) = fs::read_dir(dir.join("children")) {
                for e in entries.flatten() {
                    if e.path().is_dir() {
                        best = best.max(walk(&e.path()));
                    }
                }
            }
            1 + best
        }
        walk(&self.dir.join("tree/root"))
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

/// A gate that launches a long-running app must not wedge the run: the launch is
/// treated as a PASS once it is demonstrably alive, so a terminal app whose
/// criterion is "the app launches" can complete.
#[test]
fn non_terminating_launch_gate_does_not_wedge_the_run() {
    let p = Project::new("launchgate", "launch_gate");
    // `npm start` is recognised as a launch and stays alive, exactly like a TUI.
    p.install(
        "npm",
        "#!/usr/bin/env bash\nif [ \"$1\" = start ]; then sleep 30; fi\nexit 0\n",
    );
    let (code, _out, err) = p.run_env(
        &["init", "build a toy"],
        Duration::from_secs(60),
        &[("FRACTAL_LAUNCH_TIMEOUT", "1")],
    );
    assert_eq!(
        code,
        Some(0),
        "a launching gate must let the tree complete; stderr:\n{err}"
    );
    let status = p.status();
    assert!(
        status.contains("[complete]"),
        "launch-gated run did not complete:\n{status}"
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

/// A budget that cannot fund another split makes the tree fail rather than
/// continue silently, and the ledger records every real call debit plus the
/// split fees exactly.
#[test]
fn budget_exhaustion_fails_rather_than_silently_continuing() {
    let p = Project::new("budget", "chain");
    let (code, _out, err) = p.run_env(
        &["init", "build a chain"],
        Duration::from_secs(60),
        &[
            ("FRACTAL_BUDGET", "300"),
            ("FRACTAL_SPLIT_FEE", "100"),
            ("FRACTAL_CALL_TOKENS", "50"),
        ],
    );
    assert!(code.is_some(), "run did not terminate; stderr:\n{err}");
    assert_ne!(code, Some(0), "an exhausted tree must not report success");
    let status = p.status();
    assert!(
        !status.contains("[complete]"),
        "a node completed despite exhaustion:\n{status}"
    );
    assert!(
        status.contains("[failed]"),
        "exhaustion must fail a node, never continue silently:\n{status}"
    );
    assert!(
        p.ledger_debits() > 0,
        "the ledger recorded no call debits at all"
    );
    assert!(
        p.ledger_calls() > 0,
        "no model call was actually debited to the ledger"
    );
    assert_eq!(
        p.ledger_debits(),
        p.ledger_calls_plus_fees(),
        "ledger debits must equal recorded call usage plus split fees"
    );
}

/// Depth is bounded by economics, not only by the hard cap: a larger budget
/// recurses deeper on the same always-split task.
#[test]
fn budget_bounds_recursion_economically() {
    let small = Project::new("budgetsmall", "chain");
    let (small_code, _out, small_err) = small.run_env(
        &["init", "build a chain"],
        Duration::from_secs(60),
        &[
            ("FRACTAL_BUDGET", "300"),
            ("FRACTAL_SPLIT_FEE", "100"),
            ("FRACTAL_CALL_TOKENS", "50"),
        ],
    );
    assert!(
        small_code.is_some(),
        "small-budget run did not terminate; stderr:\n{small_err}"
    );

    let big = Project::new("budgetbig", "chain");
    let (big_code, _out, big_err) = big.run_env(
        &["init", "build a chain"],
        Duration::from_secs(60),
        &[
            ("FRACTAL_BUDGET", "1200"),
            ("FRACTAL_SPLIT_FEE", "100"),
            ("FRACTAL_CALL_TOKENS", "50"),
        ],
    );
    assert!(
        big_code.is_some(),
        "large-budget run did not terminate; stderr:\n{big_err}"
    );

    assert!(
        small.max_depth() >= 1,
        "even a tiny budget must afford the root"
    );
    assert!(
        big.max_depth() > small.max_depth(),
        "a 4x budget did not recurse deeper: small={} big={}",
        small.max_depth(),
        big.max_depth()
    );
}

/// A split whose child allocations exceed the remaining allowance is refused
/// before any child is created, and the refusal reaches the agent's context so
/// it can recover.
#[test]
fn over_allocated_split_is_rejected_with_feedback() {
    let p = Project::new("overalloc", "overalloc");
    let (code, _out, err) = p.run_env(
        &["init", "build a toy"],
        Duration::from_secs(60),
        &[
            ("FRACTAL_BUDGET", "1000000"),
            ("FRACTAL_SPLIT_FEE", "100"),
            ("FRACTAL_CALL_TOKENS", "50"),
        ],
    );
    assert_eq!(
        code,
        Some(0),
        "the root should recover and complete after the rejection; stderr:\n{err}"
    );
    let children = p.dir.join("tree/root/children");
    let names: Vec<_> = fs::read_dir(&children)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .collect();
    assert!(
        names.is_empty(),
        "the over-allocating split created children: {names:?}"
    );
    let decisions = fs::read_to_string(p.dir.join("tree/root/decisions.md")).unwrap();
    let lowered = decisions.to_lowercase();
    assert!(
        lowered.contains("budget") || lowered.contains("allocat") || lowered.contains("exceed"),
        "the rejection was not recorded where the agent can see it:\n{decisions}"
    );
}

/// Selecting `--executor opencode` really runs opencode. The `omp` on PATH is a
/// failing stub, so a successful run can only have come from opencode.
#[test]
fn opencode_executor_runs_opencode() {
    let p = Project::new("opencode", "happy");
    p.install(
        "omp",
        "#!/usr/bin/env bash\necho 'omp must not be used by the opencode executor' >&2\nexit 42\n",
    );
    p.install("opencode", FAKE_OPENCODE);
    let (code, _out, err) = p.run_env(
        &["init", "build a toy"],
        Duration::from_secs(60),
        &[("FRACTAL_EXECUTOR", "opencode")],
    );
    assert_eq!(code, Some(0), "opencode run failed; stderr:\n{err}");
    assert!(
        p.dir.join(".opencode_ran").exists(),
        "the opencode binary was never invoked"
    );
    let status = p.status();
    assert!(
        status.contains("[complete]"),
        "the opencode run did not complete:\n{status}"
    );
}

/// A node that fails leaves no files behind: its half-written work is reverted
/// before the run ends, so it cannot contaminate any later node's diff or
/// commit.
#[test]
fn failed_node_files_are_reverted() {
    let p = Project::new("partialfail", "partial_fail");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(30));
    assert!(code.is_some(), "run did not terminate; stderr:\n{err}");
    assert_ne!(code, Some(0), "a failed node must not report success");
    assert!(
        !p.dir.join("src/leftover_root-01.txt").exists(),
        "a failed node's half-written file was left in the shared tree"
    );
    let status = p.git(&["status", "--porcelain"]);
    assert!(
        !status.contains("leftover"),
        "the failed node's file leaked into git status: {status}"
    );
}

/// Two nodes that both run and commit must each commit only their own file.
/// Before per-node isolation a shared `git add -A` could sweep a sibling's
/// uncommitted work into the wrong node's commit.
#[test]
fn nodes_commit_only_their_own_work() {
    let p = Project::new("twoleaves", "two_leaves");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(60));
    assert_eq!(code, Some(0), "two-leaf run failed; stderr:\n{err}");
    let status = p.status();
    assert!(
        status.contains("[complete]"),
        "run did not complete:\n{status}"
    );

    let log = p.git(&["log", "--format=%H %s"]);
    let mut seen = 0;
    for line in log.lines() {
        let (sha, subject) = line.split_once(' ').unwrap_or((line, ""));
        let node = subject.split(':').next().unwrap_or("").trim();
        if node == "root-01" || node == "root-02" {
            let files = p.git(&["show", "--format=", "--name-only", sha]);
            let files: Vec<&str> = files.lines().filter(|l| !l.trim().is_empty()).collect();
            let expected = format!("src/{node}.txt");
            assert_eq!(
                files,
                vec![expected.as_str()],
                "commit {sha} ({subject}) contains another node's file: {files:?}"
            );
            seen += 1;
        }
    }
    assert_eq!(seen, 2, "expected a commit for each leaf; log:\n{log}");
}

/// Interrupting the harness must kill and reap the running executor child
/// rather than leaving it running after the harness exits.
#[test]
fn interrupt_reaps_the_running_executor() {
    let p = Project::new("interrupt", "hang");
    let child = p.spawn(&["init", "build a toy"], &[]);
    let pid_file = p.dir.join(".fake_omp_pid");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !pid_file.exists() {
        assert!(Instant::now() < deadline, "the executor never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    let fake_pid = fs::read_to_string(&pid_file).unwrap().trim().to_string();
    let fractal_pid = child.id().to_string();

    // SIGINT the harness, exactly as Ctrl-C would.
    let signalled = Command::new("kill")
        .args(["-INT", &fractal_pid])
        .status()
        .unwrap();
    assert!(signalled.success(), "could not signal the harness");
    let (code, _out, _err) = Project::wait(child, Duration::from_secs(30));
    assert!(code.is_some(), "the harness did not terminate after SIGINT");

    let still_alive = Command::new("kill")
        .args(["-0", &fake_pid])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    if still_alive {
        let _ = Command::new("kill").args(["-9", &fake_pid]).status();
    }
    assert!(
        !still_alive,
        "executor child {fake_pid} was left running after the interrupt"
    );
}
