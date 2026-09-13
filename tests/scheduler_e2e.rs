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
# Absolute shared path for cross-node coordination. A node's own $ROOT is its
# isolated worktree, so siblings cannot see each other's files; a concurrency
# test hands the shared project dir through FAKE_SHARED instead.
SHARED="${FAKE_SHARED:-$ROOT}"

# The butler is not a node and is identified by its own id. It steers only
# through `fractal butler-tool`, exactly as the real butler agent does.
if [ "$NODE" = "butler" ]; then
  bt() { "$FRACTAL_BIN" -p "$ROOT" butler-tool "$1" >/dev/null; }
  case "${BUTLER_MODE:-none}" in
    none)
      bt '{"tool":"plan","action":"none","rationale":"the tree already satisfies this request"}'
      ;;
    reopen)
      bt '{"tool":"reopen","parent":"root","children":["root-01"],"reason":"the wiring is wrong"}'
      bt '{"tool":"plan","action":"reopen","rationale":"only root-01 must change","nodes":["root-01"]}'
      ;;
    add)
      bt '{"tool":"split","parent":"root","subtasks":[{"id":"extra","goal":"add a new capability","acceptance_criteria":["it works"]}]}'
      bt '{"tool":"plan","action":"split","rationale":"new work no node owns","nodes":["root-02"]}'
      ;;
  esac
  echo "butler: handled ${BUTLER_MODE:-none}"
  exit 0
fi

complete() {
  mkdir -p "$ROOT/src"
  echo "delivered by ${NODE:-critic} $RANDOM" > "$ROOT/src/${NODE:-out}.txt"
  echo "{\"verb\":\"complete\",\"deliverable\":\"done\",\"summary\":\"did the task\"}"
}

# M1: do the work, then narrate the completion command as prose with the escaped
# quotes opencode's tool framing leaves behind, instead of emitting JSON.
narrated_complete() {
  mkdir -p "$ROOT/src"
  echo "delivered by ${NODE:-critic}" > "$ROOT/src/${NODE:-out}.txt"
  echo 'fractal done --summary \"narrated completion for this node\"</arg_value>'
}

# Critic invocations carry no FRACTAL_NODE_ID. The full critic prompt is passed
# inline as the last argument, so echo the node's own acceptance criteria back
# verbatim (a verdict grading foreign criteria is now rejected, and a bare PASS
# with no criteria has always been a FAIL).
if [ -z "$NODE" ]; then
  if [ "$MODE" = "critic_fail" ]; then
    echo '{"verdict":"FAIL","reason":"fake critic rejects the work","criteria":[{"name":"the task is done","pass":false,"reason":"fake rejection"}]}'
    exit 0
  fi
  # Regression guard: this mode fails unless the harness handed the critic the
  # gate result and the manifest snapshot. The leaf's only criterion is backed by
  # `npm start`, and package.json was committed before the run, so it is absent
  # from the node's diff and can only reach the critic through the snapshot.
  if [ "$MODE" = "gate_evidence" ]; then
    if printf '%s' "${!#}" | grep -q 'the app starts with npm start'; then
      if ! printf '%s' "${!#}" | grep -q 'Automated gates' \
         || ! printf '%s' "${!#}" | grep -q 'npm start' \
         || ! printf '%s' "${!#}" | grep -q 'package.json'; then
        echo '{"verdict":"FAIL","reason":"gate evidence or manifest snapshot missing from the critic prompt","criteria":[{"name":"the app starts with npm start","pass":false,"reason":"no evidence supplied"}]}'
        exit 0
      fi
    fi
  fi
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
  satisfied_no_diff)
    # A child whose contract is already satisfied by siblings: it changes
    # nothing, but its own gate passes against the existing tree. It must be
    # able to complete as "satisfied by existing artefacts".
    if [ "$NODE" = "root" ] && [ ! -d "$ROOT/tree/root/children/root-01" ]; then
      echo '{"verb":"split","subtasks":[{"id":"a","goal":"tests already written by a sibling","acceptance_criteria":["the task is done"],"verification":["true"]}]}'
    elif [ "$NODE" = "root" ]; then
      echo '{"verb":"complete","deliverable":"children aggregated","summary":"aggregated"}'
    else
      echo '{"verb":"complete","deliverable":"the deliverable already exists","summary":"already satisfied"}'
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
  two_leaves_one_fails)
    if [ "$NODE" = "root" ]; then
      if [ ! -d "$ROOT/tree/root/children/root-01" ]; then
        echo '{"verb":"split","subtasks":[{"id":"a","goal":"fails always","acceptance_criteria":["the task is done"]},{"id":"b","goal":"independent success","acceptance_criteria":["the task is done"]}]}'
      else
        echo '{"verb":"complete","deliverable":"children aggregated","summary":"aggregated"}'
      fi
    elif [ "$NODE" = "root-01" ]; then
      mkdir -p "$ROOT/src"
      echo "half written" > "$ROOT/src/leftover_${NODE}.txt"
      echo '{"verb":"not_a_real_verb"}'
    else
      complete
    fi
    ;;
  narrated_done)
    if [ "$NODE" = "root" ] && [ ! -d "$ROOT/tree/root/children/root-01" ]; then
      echo '{"verb":"split","subtasks":[{"id":"a","goal":"narrate completion","acceptance_criteria":["the task is done"]}]}'
    else
      narrated_complete
    fi
    ;;
  narrated_split)
    if [ "$NODE" = "root" ] && [ ! -d "$ROOT/tree/root/children/root-01" ]; then
      echo 'fractal split --subtasks '"'"'[{"id":"a","goal":"narrated child","acceptance_criteria":["the task is done"]}]'"'"''
    elif [ "$NODE" = "root" ]; then
      echo '{"verb":"complete","deliverable":"children aggregated","summary":"aggregated"}'
    else
      complete
    fi
    ;;
  critic_fail)
    if [ "$NODE" = "root" ] && [ ! -d "$ROOT/tree/root/children/root-01" ]; then
      echo '{"verb":"split","subtasks":[{"id":"a","goal":"build a contested module","acceptance_criteria":["the task is done"]}]}'
    elif [ "$NODE" = "root" ]; then
      echo '{"verb":"complete","deliverable":"children aggregated","summary":"aggregated"}'
    else
      complete
    fi
    ;;
  gate_evidence)
    # The leaf's criterion is backed by a gate, and the manifest that criterion
    # depends on was committed before the run (so it is not in the leaf's diff).
    # It can only verify if the harness feeds the gate result and the snapshot to
    # the critic; the fake critic above fails otherwise.
    if [ "$NODE" = "root" ] && [ ! -d "$ROOT/tree/root/children/root-01" ]; then
      echo '{"verb":"split","subtasks":[{"id":"a","goal":"wire the start script","acceptance_criteria":["the app starts with npm start"],"verification":["npm start"]}]}'
    elif [ "$NODE" = "root" ]; then
      echo '{"verb":"complete","deliverable":"children aggregated","summary":"aggregated"}'
    else
      mkdir -p "$ROOT/src"
      echo "delivered" > "$ROOT/src/app.js"
      echo '{"verb":"complete","deliverable":"done","summary":"wired the start script"}'
    fi
    ;;
  runtime_state)
    if [ "$NODE" = "root" ] && [ ! -d "$ROOT/tree/root/children/root-01" ]; then
      echo '{"verb":"split","subtasks":[{"id":"a","goal":"install deps","acceptance_criteria":["the task is done"],"verification":["touch .portfolio.json"]}]}'
    elif [ "$NODE" = "root" ]; then
      echo '{"verb":"complete","deliverable":"children aggregated","summary":"aggregated"}'
    else
      mkdir -p "$ROOT/src" "$ROOT/node_modules/dep"
      echo '{"name":"fake-app"}' > "$ROOT/package.json"
      echo "module.exports = 1" > "$ROOT/node_modules/dep/index.js"
      echo "delivered" > "$ROOT/src/app.js"
      echo '{"verb":"complete","deliverable":"done","summary":"installed deps"}'
    fi
    ;;
  prose_gate)
    # H1: the first split states a prose "verification" entry. The orchestrator
    # must refuse it and ask for a real command; the corrected split moves the
    # visual check to manual_verification.
    if [ "$NODE" = "root" ] && [ ! -d "$ROOT/tree/root/children/root-01" ]; then
      if [ ! -f "$ROOT/.prose_refused" ]; then
        touch "$ROOT/.prose_refused"
        echo '{"verb":"split","subtasks":[{"id":"a","goal":"build a TUI","acceptance_criteria":["the task is done"],"verification":["manual smoke test"]}]}'
      else
        echo '{"verb":"split","subtasks":[{"id":"a","goal":"build a TUI","acceptance_criteria":["the task is done"],"verification":["true"],"manual_verification":["the layout is clean"]}]}'
      fi
    elif [ "$NODE" = "root" ]; then
      echo '{"verb":"complete","deliverable":"children aggregated","summary":"aggregated"}'
    else
      complete
    fi
    ;;
  runtime_unrunnable_gate)
    # H1: the entry's first token resolves (env) but the shell cannot execute it
    # (exit 127). It must be downgraded to a manual check, not fail the node.
    if [ "$NODE" = "root" ] && [ ! -d "$ROOT/tree/root/children/root-01" ]; then
      echo '{"verb":"split","subtasks":[{"id":"a","goal":"do the only task","acceptance_criteria":["the task is done"],"verification":["env definitely-missing-fractal-xyz"]}]}'
    elif [ "$NODE" = "root" ]; then
      echo '{"verb":"complete","deliverable":"children aggregated","summary":"aggregated"}'
    else
      complete
    fi
    ;;
  manual_gate_with_real_failure)
    # H1 recovery: one gate cannot execute, another genuinely fails. The node
    # must still fail, but its real diff must survive as a checkpoint, not be
    # reverted.
    if [ "$NODE" = "root" ] && [ ! -d "$ROOT/tree/root/children/root-01" ]; then
      echo '{"verb":"split","subtasks":[{"id":"a","goal":"do the only task","acceptance_criteria":["the task is done"],"verification":["env definitely-missing-fractal-xyz","false"]}]}'
    elif [ "$NODE" = "root" ]; then
      echo '{"verb":"complete","deliverable":"children aggregated","summary":"aggregated"}'
    else
      complete
    fi
    ;;
  hang)
    echo "$$" > "$ROOT/.fake_omp_pid"
    exec sleep 120
    ;;
  concurrent)
    # Two independent leaves of one split. Each announces its start in the shared
    # dir and waits for the sibling to start while it is still running, so a
    # marker can only appear if both really executed at the same time. Each also
    # records whether its own worktree contains the sibling's file: it never
    # should, because each runs in its own worktree.
    if [ "$NODE" = "root" ]; then
      if [ ! -d "$SHARED/tree/root/children/root-01" ]; then
        echo '{"verb":"split","subtasks":[{"id":"a","goal":"first leaf","acceptance_criteria":["the task is done"]},{"id":"b","goal":"second leaf","acceptance_criteria":["the task is done"]}]}'
      else
        echo '{"verb":"complete","deliverable":"children aggregated","summary":"aggregated"}'
      fi
    else
      sib=root-02; [ "$NODE" = "root-02" ] && sib=root-01
      touch "$SHARED/start_$NODE"
      i=0
      while [ $i -lt 30 ]; do
        if [ -f "$SHARED/start_$sib" ] && [ ! -f "$SHARED/done_$sib" ]; then break; fi
        sleep 0.1; i=$((i+1))
      done
      if [ -f "$SHARED/start_$sib" ] && [ ! -f "$SHARED/done_$sib" ]; then
        touch "$SHARED/overlapped"
      fi
      mkdir -p "$ROOT/src"
      echo "$NODE" > "$ROOT/src/$NODE.txt"
      if [ -f "$ROOT/src/$sib.txt" ]; then touch "$SHARED/saw_sibling"; fi
      sleep 0.5
      touch "$SHARED/done_$NODE"
      echo '{"verb":"complete","deliverable":"done","summary":"did the task"}'
    fi
    ;;
  dep_order)
    # `b` (root-02) depends on `a` (root-01); `c` (root-03) is independent. `a`
    # and `c` may run together, but `b` must only start once `a`'s commit is in
    # the shared tree, so its worktree already contains root-01.txt.
    if [ "$NODE" = "root" ]; then
      if [ ! -d "$SHARED/tree/root/children/root-01" ]; then
        echo '{"verb":"split","subtasks":[{"id":"a","goal":"producer","acceptance_criteria":["the task is done"]},{"id":"b","goal":"consumer","depends_on":["a"],"acceptance_criteria":["the task is done"]},{"id":"c","goal":"independent","acceptance_criteria":["the task is done"]}]}'
      else
        echo '{"verb":"complete","deliverable":"children aggregated","summary":"aggregated"}'
      fi
    elif [ "$NODE" = "root-02" ]; then
      mkdir -p "$ROOT/src"
      if [ ! -f "$ROOT/src/root-01.txt" ]; then
        touch "$SHARED/b_ran_before_a"
      fi
      echo "$NODE" > "$ROOT/src/$NODE.txt"
      echo '{"verb":"complete","deliverable":"done","summary":"consumed the producer"}'
    else
      mkdir -p "$ROOT/src"
      echo "$NODE" > "$ROOT/src/$NODE.txt"
      echo '{"verb":"complete","deliverable":"done","summary":"did the task"}'
    fi
    ;;
  conflict)
    # Two independent nodes write the SAME file. The first integrated commit
    # wins; the second cherry-pick must conflict and fail that node without
    # corrupting the shared tree.
    if [ "$NODE" = "root" ]; then
      if [ ! -d "$SHARED/tree/root/children/root-01" ]; then
        echo '{"verb":"split","subtasks":[{"id":"a","goal":"first writer","acceptance_criteria":["the task is done"]},{"id":"b","goal":"second writer","acceptance_criteria":["the task is done"]}]}'
      else
        echo '{"verb":"complete","deliverable":"children aggregated","summary":"aggregated"}'
      fi
    else
      mkdir -p "$ROOT/src"
      echo "written by $NODE" > "$ROOT/src/shared.txt"
      echo '{"verb":"complete","deliverable":"done","summary":"wrote shared"}'
    fi
    ;;
  escalate_parallel)
    # One child escalates while its independent sibling runs to completion in
    # another worktree; the owner resolves on the shared tree, then the
    # escalated child resumes. Exercises the escalation lifecycle with a
    # concurrent batch.
    if [ "$NODE" = "root" ]; then
      ESC="$SHARED/.esc_par"
      RES="$SHARED/.res_par"
      if [ -f "$ESC" ] && [ ! -f "$RES" ]; then
        touch "$RES"
        echo '{"verb":"escalate_resolve","resolution":"overrule","rationale":"the constraint still holds; proceed"}'
      elif [ ! -d "$SHARED/tree/root/children/root-01" ]; then
        echo '{"verb":"split","subtasks":[{"id":"a","goal":"escalating leaf","acceptance_criteria":["the task is done"]},{"id":"b","goal":"independent leaf","acceptance_criteria":["the task is done"]}]}'
      else
        echo '{"verb":"complete","deliverable":"children aggregated","summary":"aggregated"}'
      fi
    elif [ "$NODE" = "root-01" ]; then
      if [ -f "$SHARED/.res_par" ]; then
        mkdir -p "$ROOT/src"
        echo "$NODE" > "$ROOT/src/$NODE.txt"
        echo '{"verb":"complete","deliverable":"done","summary":"resumed after overrule"}'
      else
        touch "$SHARED/.esc_par"
        echo '{"verb":"escalate","assumption":"the inherited constraint is false","evidence":"observed otherwise"}'
      fi
    else
      mkdir -p "$ROOT/src"
      echo "$NODE" > "$ROOT/src/$NODE.txt"
      sleep 0.5
      echo '{"verb":"complete","deliverable":"done","summary":"independent success"}'
    fi
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
            .env("FRACTAL_NO_DASHBOARD", "1")
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

    /// Commit everything currently in the project, forcing an identity so the
    /// commit does not depend on the machine's git config.
    fn commit_all(&self, message: &str) {
        self.git(&["add", "."]);
        self.git(&[
            "-c",
            "user.name=fractal",
            "-c",
            "user.email=fractal@localhost",
            "commit",
            "--no-verify",
            "-q",
            "-m",
            message,
        ]);
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

/// Trial 5 §7.4: a child whose criteria are already satisfied by siblings could
/// not produce a diff, failed every retry and failed the whole root. A node that
/// changed nothing but whose own gate passes against the existing tree must be
/// able to complete as "satisfied by existing artefacts".
#[test]
fn a_no_diff_node_completes_when_its_own_gate_passes() {
    let p = Project::new("satisfiednodiff", "satisfied_no_diff");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(30));
    assert_eq!(
        code,
        Some(0),
        "a no-diff node whose own gate passes must complete; stderr:\n{err}"
    );
    let status = p.status();
    assert!(
        status.contains("[complete]"),
        "the tree did not complete:\n{status}"
    );
    let decisions = fs::read_to_string(p.dir.join("tree/root/children/root-01/decisions.md"))
        .unwrap_or_default();
    assert!(
        decisions.contains("satisfied by existing artefacts"),
        "the no-diff completion was not recorded as such:\n{decisions}"
    );
}

/// Trial 5 §7.5: `fractal digest` reported `root [running]` while status said
/// complete. After a completed run it must report the root under Done.
#[test]
fn digest_reports_a_completed_root_as_done() {
    let p = Project::new("digestroot", "happy");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(30));
    assert_eq!(code, Some(0), "run did not complete; stderr:\n{err}");
    let (dcode, out, derr) = p.run(&["digest"], Duration::from_secs(20));
    assert_eq!(dcode, Some(0), "digest failed: {derr}");
    let digest = format!("{out}\n{derr}");
    let done = digest.split("## Blocked").next().unwrap_or(&digest);
    assert!(
        done.contains("**root**"),
        "the completed root must be listed under Done:\n{digest}"
    );
    assert!(
        !digest.contains("running"),
        "a completed run must not be reported as running:\n{digest}"
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

/// A criterion backed by a passing gate must verify even when the file it depends
/// on was committed by an earlier node and is absent from this node's diff. The
/// fake critic fails unless it receives both the gate result and the manifest
/// snapshot, so a completed run proves the evidence reached it.
#[test]
fn critic_verifies_a_gate_backed_criterion_against_a_committed_manifest() {
    let p = Project::new("gateevidence", "gate_evidence");
    p.install(
        "npm",
        "#!/usr/bin/env bash\nif [ \"$1\" = start ]; then sleep 30; fi\nexit 0\n",
    );
    // The manifest is committed before the run, so it is not part of the leaf's
    // diff - exactly the situation that made the criterion unverifiable.
    p.git(&["init"]);
    fs::write(
        p.dir.join("package.json"),
        r#"{"name":"toy","scripts":{"start":"node src/app.js"}}"#,
    )
    .unwrap();
    p.commit_all("chore: add the start script manifest");
    assert!(
        p.git(&["status", "--porcelain"]).is_empty(),
        "the seed files must be committed before fractal init runs"
    );

    let (code, _out, err) = p.run_env(
        &["init", "build a toy"],
        Duration::from_secs(60),
        &[("FRACTAL_LAUNCH_TIMEOUT", "1")],
    );
    assert_eq!(
        code,
        Some(0),
        "a gate-backed criterion with a committed manifest must verify; stderr:\n{err}"
    );
    let status = p.status();
    assert!(
        status.contains("[complete]"),
        "the tree did not complete:\n{status}"
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

/// D3: one node failing terminally must not abandon its independent sibling, and
/// the run must report the failure at the root. The failed node's own files are
/// reverted; the independent sibling's committed work survives.
#[test]
fn one_node_failure_does_not_abandon_independent_siblings() {
    let p = Project::new("siblingfail", "two_leaves_one_fails");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(60));
    assert!(code.is_some(), "run did not terminate; stderr:\n{err}");
    assert_ne!(code, Some(0), "a failed branch must not report success");
    let status = p.status();
    assert!(
        status.contains("[complete]"),
        "the independent sibling never ran:\n{status}"
    );
    assert!(
        status.contains("[failed]"),
        "the failure was not recorded:\n{status}"
    );
    assert!(
        !p.dir.join("src/leftover_root-01.txt").exists(),
        "the failed node's file was left in the shared tree"
    );
    assert!(
        p.dir.join("src/root-02.txt").exists(),
        "the independent sibling's committed work was lost to the failure"
    );
    let log = p.git(&["log", "--format=%s"]);
    assert!(
        log.contains("root-02"),
        "the independent sibling produced no commit:\n{log}"
    );
}

/// M1: work is real, but the model prints `fractal done --summary "..."` as
/// prose with escaped quotes rather than emitting a JSON decision. The command
/// must be recovered and still pass the normal diff/critic gates.
#[test]
fn narrated_done_is_recovered_and_verified() {
    let p = Project::new("narrateddone", "narrated_done");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(60));
    assert_eq!(
        code,
        Some(0),
        "a narrated completion must be recovered; stderr:\n{err}"
    );
    let status = p.status();
    assert!(
        status.contains("[complete]"),
        "narrated run did not complete:\n{status}"
    );
    assert!(
        p.dir.join("src/root-01.txt").exists(),
        "the narrated node's work is missing"
    );
}

/// M1: the same tolerance for a narrated `fractal split --subtasks '[...]'`.
#[test]
fn narrated_split_is_recovered() {
    let p = Project::new("narratedsplit", "narrated_split");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(60));
    assert_eq!(
        code,
        Some(0),
        "a narrated split must be recovered; stderr:\n{err}"
    );
    let status = p.status();
    assert!(
        status.contains("[complete]"),
        "narrated split run did not complete:\n{status}"
    );
    assert!(
        p.dir.join("tree/root/children/root-01").exists(),
        "the narrated split created no child"
    );
}

/// D3: a node whose objective gates passed and only the critic rejected is
/// substantially correct. It still fails, but its work is checkpointed rather
/// than wiped, so a later retry or reopen can build on it.
#[test]
fn critic_contested_work_survives_as_a_checkpoint() {
    let p = Project::new("criticfail", "critic_fail");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(90));
    assert!(code.is_some(), "run did not terminate; stderr:\n{err}");
    assert_ne!(
        code,
        Some(0),
        "a critic-rejected node must not report success"
    );
    let status = p.status();
    assert!(
        status.contains("[failed]"),
        "the contested node was not failed:\n{status}"
    );
    assert!(
        p.dir.join("src/root-01.txt").exists(),
        "gate-passing-but-contested work was wiped instead of checkpointed"
    );
    let log = p.git(&["log", "--format=%s"]);
    assert!(
        log.contains("unverified checkpoint"),
        "no checkpoint commit was recorded:\n{log}"
    );
    // The rejection must be diagnosable from the run's own artifacts: verdict +
    // per-criterion reasons in the node's decisions and event log.
    let decisions = fs::read_to_string(p.dir.join("tree/root/children/root-01/decisions.md"))
        .expect("the failed node must keep a decisions.md");
    assert!(
        decisions.contains("critic rejected"),
        "the critic's rejection reason was not persisted to decisions.md:\n{decisions}"
    );
    let events = fs::read_to_string(p.dir.join("tree/root/children/root-01/log/events.jsonl"))
        .expect("the failed node must keep an event log");
    assert!(
        events.contains("critic_rejected") && events.contains("fake rejection"),
        "the critic's per-criterion rejection was not persisted to events.jsonl:\n{events}"
    );
}

/// D5: dependency trees and app runtime state an agent's gate produced must
/// never enter the generated project's history, while real work still does.
#[test]
fn dependency_trees_and_runtime_state_stay_out_of_history() {
    let p = Project::new("runtimestate", "runtime_state");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(60));
    assert_eq!(code, Some(0), "runtime-state run failed; stderr:\n{err}");
    let tracked = p.git(&["ls-files"]);
    assert!(
        !tracked.contains("node_modules/"),
        "node_modules entered history:\n{tracked}"
    );
    assert!(
        !tracked.contains(".portfolio.json"),
        "runtime state entered history:\n{tracked}"
    );
    assert!(
        tracked.contains("src/app.js"),
        "the node's real work was not committed:\n{tracked}"
    );
    assert!(
        p.dir.join(".portfolio.json").exists(),
        "runtime state was deleted instead of left untracked"
    );
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

/// H1: a split whose `verification` entry is prose is refused with feedback, and
/// the recovered split moves the visual check to `manual_verification` so the
/// node can complete. Before this fix, `sh -c "manual smoke test"` failed on
/// every retry and the node's correct work was reverted.
#[test]
fn prose_verification_entry_is_refused_then_moved_to_manual() {
    let p = Project::new("prosegate", "prose_gate");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(60));
    assert_eq!(
        code,
        Some(0),
        "the run must recover from the refused split; stderr:\n{err}"
    );

    let decisions = fs::read_to_string(p.dir.join("tree/root/decisions.md")).unwrap_or_default();
    assert!(
        decisions.to_lowercase().contains("split refused"),
        "the prose verification entry was not refused with feedback:\n{decisions}"
    );

    let contract = fs::read_to_string(p.dir.join("tree/root/children/root-01/contract.md"))
        .unwrap_or_default();
    assert!(
        contract.contains("the layout is clean"),
        "the manual check was lost from the corrected contract:\n{contract}"
    );
    let verif = contract
        .split("## verification")
        .nth(1)
        .and_then(|s| s.split("## manual verification").next())
        .unwrap_or("");
    assert!(
        !verif.contains("the layout is clean"),
        "a manual check leaked into the executable gates:\n{contract}"
    );

    let status = p.status();
    assert!(
        status.contains("[complete]") && !status.contains("[failed]"),
        "the recovered run did not complete cleanly:\n{status}"
    );
}

/// H1 regression: a gate the shell cannot execute (exit 127) must not doom a
/// node whose work is real. The correct diff survives and the node completes;
/// before the fix it failed six times and was reverted.
#[test]
fn unexecutable_gate_does_not_revert_correct_work() {
    let p = Project::new("rungate", "runtime_unrunnable_gate");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(60));
    assert_eq!(
        code,
        Some(0),
        "an unexecutable gate must not fail the whole run; stderr:\n{err}"
    );
    assert!(
        p.dir.join("src/root-01.txt").exists(),
        "the node's correct work was reverted because a gate could not execute"
    );
    let status = p.status();
    assert!(
        status.contains("[complete]") && !status.contains("[failed]"),
        "the node was failed over an unexecutable gate:\n{status}"
    );
    let decisions = fs::read_to_string(p.dir.join("tree/root/children/root-01/decisions.md"))
        .unwrap_or_default();
    assert!(
        decisions.contains("manual verification"),
        "the skipped gate was not recorded as a manual check:\n{decisions}"
    );
}

/// H1: when a node has a gate that cannot execute and also a gate that genuinely
/// fails, it still fails - but its real diff is kept as an unverified checkpoint
/// rather than reverted.
#[test]
fn work_survives_when_gates_cannot_execute() {
    let p = Project::new("manualfail", "manual_gate_with_real_failure");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(60));
    assert!(code.is_some(), "run did not terminate; stderr:\n{err}");
    assert_ne!(code, Some(0), "a genuinely failing gate must still fail");
    assert!(
        p.dir.join("src/root-01.txt").exists(),
        "work was reverted even though a gate could not execute"
    );
    let status = p.status();
    assert!(
        status.contains("[failed]"),
        "the node should have failed on the real gate:\n{status}"
    );
    let log = p.git(&["log", "--format=%s"]);
    assert!(
        log.contains("unverified checkpoint"),
        "the work was not checkpointed:\n{log}"
    );
}

/// Two independent ready nodes must execute at the same time, each in its own
/// worktree: a node can never see a sibling's uncommitted file, and each commit
/// contains only its own work.
#[test]
fn independent_ready_nodes_run_concurrently_in_isolated_worktrees() {
    let p = Project::new("concurrent", "concurrent");
    let shared = p.dir.to_string_lossy().to_string();
    let (code, _out, err) = p.run_env(
        &["init", "build a toy"],
        Duration::from_secs(90),
        &[("FRACTAL_PARALLEL", "4"), ("FAKE_SHARED", &shared)],
    );
    assert_eq!(code, Some(0), "concurrent run failed; stderr:\n{err}");
    assert!(
        p.dir.join("overlapped").exists(),
        "the two independent ready nodes never ran at the same time"
    );
    assert!(
        !p.dir.join("saw_sibling").exists(),
        "a node saw its sibling's uncommitted file in its worktree"
    );
    assert!(
        p.dir.join("src/root-01.txt").exists() && p.dir.join("src/root-02.txt").exists(),
        "both nodes' work was not integrated into the shared tree"
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
    assert_eq!(
        seen, 2,
        "expected a commit for each concurrent leaf; log:\n{log}"
    );
}

/// A dependent must still wait: it only runs once its dependency's commit is in
/// the shared tree, and the producer's commit precedes the consumer's.
#[test]
fn a_dependent_waits_and_commits_integrate_in_dependency_order() {
    let p = Project::new("deporder", "dep_order");
    let shared = p.dir.to_string_lossy().to_string();
    let (code, _out, err) = p.run_env(
        &["init", "build a toy"],
        Duration::from_secs(90),
        &[("FRACTAL_PARALLEL", "4"), ("FAKE_SHARED", &shared)],
    );
    assert_eq!(code, Some(0), "dep-order run failed; stderr:\n{err}");
    assert!(
        !p.dir.join("b_ran_before_a").exists(),
        "the dependent ran before its dependency was integrated"
    );
    for f in ["root-01.txt", "root-02.txt", "root-03.txt"] {
        assert!(
            p.dir.join("src").join(f).exists(),
            "missing {f} after integration"
        );
    }
    // `git log` is newest-first, so the consumer's commit must appear before the
    // producer's only if it was created later.
    let log = p.git(&["log", "--format=%s"]);
    let producer = log
        .lines()
        .position(|l| l.starts_with("root-01:"))
        .expect("no commit for the producer");
    let consumer = log
        .lines()
        .position(|l| l.starts_with("root-02:"))
        .expect("no commit for the consumer");
    assert!(
        consumer < producer,
        "the consumer's commit is not after the producer's:\n{log}"
    );
}

/// When two independent nodes change the same file, the second cherry-pick must
/// conflict: that node fails with the conflict recorded, and the shared tree
/// keeps exactly the first node's version rather than being corrupted.
#[test]
fn an_integration_conflict_fails_that_node_without_corrupting_the_tree() {
    let p = Project::new("conflict", "conflict");
    let shared = p.dir.to_string_lossy().to_string();
    let (code, _out, err) = p.run_env(
        &["init", "build a toy"],
        Duration::from_secs(90),
        &[("FRACTAL_PARALLEL", "4"), ("FAKE_SHARED", &shared)],
    );
    assert!(code.is_some(), "run did not terminate; stderr:\n{err}");
    assert_ne!(
        code,
        Some(0),
        "a conflicting integration must not report success"
    );
    let status = p.status();
    assert!(
        status.contains("[failed]"),
        "the conflicting node was not failed:\n{status}"
    );
    let content = fs::read_to_string(p.dir.join("src/shared.txt")).unwrap_or_default();
    assert!(
        content.starts_with("written by root-0"),
        "the shared tree was corrupted or left empty: {content:?}"
    );

    let mut conflicted = false;
    for child in ["root-01", "root-02"] {
        let decisions = fs::read_to_string(
            p.dir
                .join(format!("tree/root/children/{child}/decisions.md")),
        )
        .unwrap_or_default();
        if decisions.contains("integration conflict") {
            conflicted = true;
        }
    }
    assert!(
        conflicted,
        "no node recorded the integration conflict in its decisions"
    );
}

/// The escalation lifecycle must hold when a ready batch runs concurrently: one
/// child escalates, its independent sibling completes in another worktree, the
/// owner resolves the escalation, and the escalated child resumes and completes.
#[test]
fn escalation_resolves_while_an_independent_sibling_runs_concurrently() {
    let p = Project::new("escalateparallel", "escalate_parallel");
    let shared = p.dir.to_string_lossy().to_string();
    let (code, _out, err) = p.run_env(
        &["init", "build a toy"],
        Duration::from_secs(90),
        &[("FRACTAL_PARALLEL", "4"), ("FAKE_SHARED", &shared)],
    );
    assert_eq!(
        code,
        Some(0),
        "parallel escalation run failed; stderr:\n{err}"
    );
    let status = p.status();
    assert!(
        !status.contains("[failed]"),
        "escalation failed instead of resolving:\n{status}"
    );
    assert!(
        status.contains("[complete]"),
        "the tree did not complete after the parallel escalation:\n{status}"
    );
    for f in ["root-01.txt", "root-02.txt"] {
        assert!(
            p.dir.join("src").join(f).exists(),
            "missing {f}: a node's work was lost during escalation"
        );
    }
    let decisions = fs::read_to_string(p.dir.join("tree/root/children/root-01/decisions.md"))
        .unwrap_or_default();
    assert!(
        decisions.contains("escalated"),
        "the escalation was not recorded on the escalating child:\n{decisions}"
    );
}

/// The butler is the one agent the user talks to. When it judges a steer already
/// satisfied, it must change nothing at all and still record why.
#[test]
fn butler_judges_an_already_satisfied_steer_as_no_rework() {
    let p = Project::new("butlernone", "happy");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(60));
    assert_eq!(code, Some(0), "seed run failed; stderr:\n{err}");
    let before = p.status();

    let (acode, _aout, aerr) = p.run_env(
        &["ask", "make the result a little better"],
        Duration::from_secs(30),
        &[("BUTLER_MODE", "none")],
    );
    assert_eq!(acode, Some(0), "ask failed; stderr:\n{aerr}");
    assert_eq!(
        before,
        p.status(),
        "a steer the butler judged already satisfied must not touch any node"
    );

    let log = fs::read_to_string(p.dir.join(".fractal/butler/log.jsonl")).unwrap_or_default();
    assert!(
        log.contains("\"action\":\"none\""),
        "the plan was not recorded:\n{log}"
    );
    assert!(
        log.contains("already satisfies"),
        "the rationale was not recorded:\n{log}"
    );
}

/// A correction reopens only the nodes that must change (and their subtrees);
/// the rest of the completed tree is left as it is. Resuming then re-runs only
/// those nodes and re-aggregates.
#[test]
fn butler_reopens_only_the_named_child_then_resumes() {
    let p = Project::new("butlerreopen", "happy");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(60));
    assert_eq!(code, Some(0), "seed run failed; stderr:\n{err}");

    let (acode, _aout, aerr) = p.run_env(
        &["ask", "root-01 is wired wrong, fix it"],
        Duration::from_secs(30),
        &[("BUTLER_MODE", "reopen")],
    );
    assert_eq!(acode, Some(0), "ask failed; stderr:\n{aerr}");
    let status = p.status();
    assert!(
        status.contains("root-01  [pending]"),
        "the named child was not reopened:\n{status}"
    );
    assert!(
        p.dir.join("src/root-01.txt").exists(),
        "reopening must not delete the child's committed work"
    );

    let (rcode, _rout, rerr) = p.run(&["run"], Duration::from_secs(60));
    assert_eq!(rcode, Some(0), "resume failed; stderr:\n{rerr}");
    let done = p.status();
    assert!(
        done.contains("[complete]") && !done.contains("[failed]"),
        "the tree did not re-complete after the reopen:\n{done}"
    );
}

/// A steer for genuinely new work adds exactly the node that owns it, leaving
/// every existing completed node untouched.
#[test]
fn butler_adds_a_node_for_genuinely_new_work() {
    let p = Project::new("butleradd", "happy");
    let (code, _out, err) = p.run(&["init", "build a toy"], Duration::from_secs(60));
    assert_eq!(code, Some(0), "seed run failed; stderr:\n{err}");

    let (acode, _aout, aerr) = p.run_env(
        &["ask", "also handle the edge case nobody built"],
        Duration::from_secs(30),
        &[("BUTLER_MODE", "add")],
    );
    assert_eq!(acode, Some(0), "ask failed; stderr:\n{aerr}");
    let status = p.status();
    assert!(
        status.contains("root-02  [pending]"),
        "the new node was not added:\n{status}"
    );
    assert!(
        status.contains("root-01  [complete]"),
        "existing completed work was replayed:\n{status}"
    );
    let contract = fs::read_to_string(p.dir.join("tree/root/children/root-02/contract.md"))
        .unwrap_or_default();
    assert!(
        contract.contains("add a new capability"),
        "the new node's contract is missing:\n{contract}"
    );
}
