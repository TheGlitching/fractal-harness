use crate::runner::{
    run_node, verify_node, RunnerError, COMPLETE_VERB, ESCALATE, ESCALATE_RESOLVE, NOTE_GLOBAL,
    REOPEN, SPLIT,
};
use crate::store::{
    Contract, Node, Store, StoreError, COMPLETE, FAILED, PENDING, RUNNING, SPLIT as SPLIT_STATUS,
    SUSPENDED,
};
use crate::tui::{StatsSnapshot, TuiState};
use crate::verify::GateScope;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) static INTERRUPTED: AtomicBool = AtomicBool::new(false);

const MAX_DEPTH: i64 = 4;
/// Retries per node before it fails closed. Six gives a small model room to
/// recover from a missed decision (the trial needed exactly that many); an
/// override lets a cheap or expensive model be tuned without a rebuild.
const DEFAULT_MAX_ATTEMPTS: usize = 6;
const MAX_STEPS: usize = 500;
/// Builds and test suites are slow; a gate needs a far longer leash than an
/// agent turn.
const GATE_TIMEOUT_SECS: u64 = 900;

fn max_attempts() -> usize {
    std::env::var("FRACTAL_MAX_ATTEMPTS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_ATTEMPTS)
}

pub struct RunReport {
    pub steps: usize,
    pub completed: usize,
    pub split: usize,
    pub refused: usize,
    pub failed: usize,
    pub root_status: String,
    pub escalations: usize,
    pub verifications: usize,
    pub verify_failures: usize,
    pub node_depths: Vec<i64>,
    /// A failed node's gate-passing-but-critic-contested work was committed as a
    /// checkpoint rather than reverted, so it survives for a later retry.
    pub checkpoint_committed: bool,
}

impl Default for RunReport {
    fn default() -> Self {
        RunReport {
            steps: 0,
            completed: 0,
            split: 0,
            refused: 0,
            failed: 0,
            root_status: PENDING.into(),
            escalations: 0,
            verifications: 0,
            verify_failures: 0,
            node_depths: vec![],
            checkpoint_committed: false,
        }
    }
}

impl RunReport {
    pub fn ok(&self) -> bool {
        self.root_status != FAILED && [COMPLETE, SPLIT_STATUS].contains(&self.root_status.as_str())
    }

    pub fn write_trace(&self, path: &str) {
        let mut depths_count: HashMap<i64, usize> = HashMap::new();
        for d in &self.node_depths {
            *depths_count.entry(*d).or_default() += 1;
        }
        let max_depth = self.node_depths.iter().max().copied().unwrap_or(1);
        let verify_catch = if self.verifications > 0 {
            // A single verification can fail more than once across retries, so
            // the ratio can exceed 1; it is a catch rate and is bounded to [0,1].
            (self.verify_failures as f64 / self.verifications as f64).min(1.0)
        } else {
            0.0
        };
        let record = serde_json::json!({
            "steps": self.steps, "completed": self.completed, "split": self.split,
            "refused": self.refused, "failed": self.failed, "root_status": self.root_status,
            "escalations": self.escalations, "verifications": self.verifications,
            "verify_failures": self.verify_failures, "verify_catch_rate": verify_catch,
            "depth_distribution": depths_count, "max_depth": max_depth,
        });
        let _ = std::fs::write(
            path,
            serde_json::to_string_pretty(&record).unwrap_or_default(),
        );
    }

    fn merge(&mut self, other: &RunReport) {
        self.steps += other.steps;
        self.completed += other.completed;
        self.split += other.split;
        self.refused += other.refused;
        self.failed += other.failed;
        self.escalations += other.escalations;
        self.verifications += other.verifications;
        self.verify_failures += other.verify_failures;
        self.node_depths.extend_from_slice(&other.node_depths);
        self.checkpoint_committed |= other.checkpoint_committed;
    }
}

pub fn run(
    store: &Store,
    state: &Arc<Mutex<TuiState>>,
    model: &str,
    interactive: bool,
) -> std::result::Result<RunReport, StoreError> {
    store.reconcile()?;
    let nodes = store.walk()?;
    for n in &nodes {
        if n.status == RUNNING {
            store.set_status(n, PENDING)?;
        }
    }
    let mut report = RunReport::default();
    let limit = std::env::var("FRACTAL_MAX_STEPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(MAX_STEPS);
    let max_parallel: usize = std::env::var("FRACTAL_PARALLEL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);
    let model_owned = model.to_string();

    while report.steps < limit {
        if INTERRUPTED.load(Ordering::SeqCst) {
            let mut s = state.lock().unwrap();
            s.status_line = "interrupted".into();
            break;
        }

        // Process any steer commands from the queue
        if let Ok(steers) = store.drain_steer_queue() {
            for (_id, cmd, payload) in steers {
                match cmd.as_str() {
                    "constraint" => {
                        let parts: Vec<&str> = payload.splitn(2, ':').collect();
                        if parts.len() == 2 {
                            let origin = parts[0];
                            let constraint = parts[1];
                            let _ = store.add_constraint_and_propagate(origin, constraint);
                        }
                    }
                    "retry" => {
                        let _ = store.retry(&payload);
                    }
                    _ => {}
                }
            }
        }

        let nodes = store.walk()?;
        let stale = store.stale_ids().unwrap_or_default();
        let runnable = next_nodes(&nodes, &stale);
        if runnable.is_empty() {
            // Check if any failed or blocked nodes exist
            let has_failed = nodes.iter().any(|n| n.status == FAILED);
            let all_complete = nodes
                .iter()
                .all(|n| n.status == COMPLETE || n.status == SPLIT_STATUS);
            {
                let mut s = state.lock().unwrap();
                if has_failed {
                    s.status_line = "Blocked on failed subtasks. Press 'r' on a failed node to retry, or 'q' to exit.".into();
                } else if all_complete {
                    s.status_line = "All tasks completed successfully.".into();
                    break;
                } else {
                    s.status_line =
                        "No runnable tasks. Waiting for dependencies or steer commands...".into();
                }
            }

            if has_failed {
                if interactive {
                    // A real TTY can still deliver a retry keystroke, so wait
                    // briefly for user steering without killing the thread.
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    continue;
                }
                // No TTY means no keystroke can ever arrive: terminate
                // deterministically with the failure recorded and a non-zero
                // status, instead of spinning forever.
                break;
            } else {
                break;
            }
        }
        let batch = &runnable[..runnable.len().min(max_parallel)];
        for _n in batch {
            report.steps += 1;
        }
        {
            let sn = snapshot(&report, &nodes);
            let mut s = state.lock().unwrap();
            s.nodes = nodes;
            s.stats = sn;
        }

        // Nodes execute one at a time on the shared working tree.
        //
        // Concurrent nodes in one directory cannot be attributed: a sibling's
        // uncommitted files appear in this node's diff, `git add` would commit
        // them under the wrong node, and a failed attempt's files would leak
        // into the next node. Per-node git worktrees were considered and
        // rejected: the in-place model has parents and children share a single
        // tree and `dist/`, so isolating each node would require cross-worktree
        // merges the tree's dependency model does not describe. Serializing the
        // whole node - not just verify+commit - is the low-risk correct option:
        // no sibling can hold uncommitted work while this node runs, so its
        // diff, verification and commit are exactly its own. `FRACTAL_PARALLEL`
        // is retained as a batch hint; execution stays serialized.
        for node in batch {
            let state_output = state.clone();
            let nid = node.id.clone();
            let on_output: crate::runner::OutputFn = Arc::new(move |line: &str| {
                if let Ok(mut s) = state_output.lock() {
                    let clean = line.trim().to_string();
                    s.log_lines.push(clean.clone());
                    s.node_activities.insert(nid.clone(), clean.clone());
                    if !clean.is_empty() {
                        s.last_activity = clean;
                    }
                }
            });

            let result = run_one_node(store, node, &model_owned, on_output, state);
            match result {
                Ok(sub) => {
                    let failed = sub.failed > 0;
                    report.merge(&sub);
                    // Revert the failed node's own files so they cannot leak
                    // into a sibling's diff. A checkpointed node's work is
                    // deliberately kept in history instead.
                    if failed && !sub.checkpoint_committed {
                        let nodes = store.walk().unwrap_or_default();
                        if let Some(n) = nodes.iter().find(|n2| n2.id == node.id) {
                            revert_failed_node(store, n);
                        }
                    }
                }
                Err(e) => {
                    let nodes = store.walk().unwrap_or_default();
                    if let Some(n) = nodes.iter().find(|n2| n2.id == node.id) {
                        let _ = store.append_decision(n, &format!("error: {e}"));
                        let _ = store.set_status(n, FAILED);
                        revert_failed_node(store, n);
                        report.failed += 1;
                    }
                }
            }
            let ns = store.walk().unwrap_or_default();
            let sn = snapshot(&report, &ns);
            let mut s = state.lock().unwrap();
            s.nodes = ns;
            s.stats = sn;
        }
    }

    let nodes = store.walk()?;
    report.root_status = surface_root_status(&nodes);
    if let Some(project) = store.tree_dir.parent() {
        report.write_trace(&project.join("trace.json").to_string_lossy());
    }
    {
        let sn = snapshot(&report, &nodes);
        let mut s = state.lock().unwrap();
        s.nodes = nodes;
        s.stats = sn;
        s.done = true;
        if report.root_status == COMPLETE {
            s.status_line = "done — root complete".into();
        } else {
            s.status_line = format!(
                "stopped — root: {} (press r on a node to retry)",
                report.root_status
            );
        }
    }
    Ok(report)
}

/// The status a completed run reports for its root. A failed node anywhere in
/// the tree means the goal was not delivered in full, even though the root
/// itself may still be `split` and individually retryable. Surfacing the failure
/// here makes the run report and exit say `failed` instead of hiding it behind a
/// non-terminal root status.
fn surface_root_status(nodes: &[Node]) -> String {
    let root = nodes
        .first()
        .map(|n| n.status.clone())
        .unwrap_or_else(|| PENDING.into());
    if root == COMPLETE {
        return root;
    }
    if nodes.iter().any(|n| n.status == FAILED) {
        return FAILED.to_string();
    }
    root
}

fn snapshot(report: &RunReport, nodes: &[Node]) -> StatsSnapshot {
    let goal_of = |status: &str| -> Vec<String> {
        nodes
            .iter()
            .filter(|n| n.status == status)
            .map(|n| n.goal.lines().next().unwrap_or(&n.goal).to_string())
            .collect()
    };
    StatsSnapshot {
        steps: report.steps,
        completed: report.completed,
        split: report.split,
        failed: report.failed,
        refused: report.refused,
        refused_goals: goal_of("refused"),
        failed_goals: goal_of("failed"),
    }
}

fn next_nodes(nodes: &[Node], stale: &HashSet<String>) -> Vec<Node> {
    // Stale dependents first: a dependency was reopened and its deliverable
    // changed after this node was accepted, so it must be re-verified against
    // the new dependency before anything downstream trusts it.
    let mut stale_nodes: Vec<Node> = nodes
        .iter()
        .filter(|n| stale.contains(&n.id))
        .cloned()
        .collect();
    stale_nodes.sort_by_key(|b| std::cmp::Reverse(b.depth));
    if !stale_nodes.is_empty() {
        return stale_nodes;
    }

    let by_id: HashMap<&str, &Node> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut runnable: Vec<Node> = nodes
        .iter()
        .filter(|n| n.status == PENDING && deps_satisfied(n, &by_id, stale))
        .cloned()
        .collect();
    runnable.sort_by_key(|b| std::cmp::Reverse(b.depth));
    if !runnable.is_empty() {
        return runnable;
    }

    let agg: Vec<Node> = nodes
        .iter()
        .filter(|n| n.status == SPLIT_STATUS && aggregatable(n, nodes, stale))
        .cloned()
        .collect();
    agg
}

fn deps_satisfied(node: &Node, by_id: &HashMap<&str, &Node>, stale: &HashSet<String>) -> bool {
    node.depends_on.iter().all(|dep| {
        by_id
            .get(dep.as_str())
            .is_some_and(|n| n.status == COMPLETE && !stale.contains(&n.id))
    })
}

fn aggregatable(node: &Node, nodes: &[Node], stale: &HashSet<String>) -> bool {
    let children: Vec<&Node> = nodes
        .iter()
        .filter(|n| n.parent.as_deref() == Some(&node.id))
        .collect();
    // Only aggregate once every child is accepted and none has gone stale.
    !children.is_empty()
        && children
            .iter()
            .all(|c| c.status == COMPLETE && !stale.contains(&c.id))
}

/// Reject a split whose dependency graph cannot be scheduled: an edge naming a
/// sibling that does not exist, or a cycle. Rejecting here (with a message the
/// agent sees) is what stops `add_children` from having to silently drop an
/// unmatched edge, which would make the child runnable too early.
fn reject_split_topology(contracts: &[Contract], existing: &[String]) -> Option<String> {
    let mut canonical: HashMap<String, String> = HashMap::new();
    for id in existing {
        canonical.insert(id.clone(), id.clone());
        if let Some(suffix) = id.rsplit('-').next() {
            canonical.insert(suffix.to_string(), id.clone());
        }
    }
    let mut keys: Vec<String> = Vec::new();
    for (i, c) in contracts.iter().enumerate() {
        let key = if c.id.trim().is_empty() {
            format!("#{}", i + 1)
        } else {
            c.id.trim().to_string()
        };
        canonical.insert(format!("{}", i + 1), key.clone());
        canonical.insert(format!("{:02}", i + 1), key.clone());
        canonical.insert(key.clone(), key.clone());
        keys.push(key);
    }

    let mut edges: HashMap<String, Vec<String>> = HashMap::new();
    for (key, c) in keys.iter().zip(contracts.iter()) {
        let mut deps = Vec::new();
        for dep in &c.depends_on {
            let dep = dep.trim();
            if dep.is_empty() {
                continue;
            }
            match canonical.get(dep) {
                Some(resolved) => deps.push(resolved.clone()),
                None => {
                    return Some(format!("depends_on names an unknown sibling {dep:?}"));
                }
            }
        }
        edges.insert(key.clone(), deps);
    }

    fn visit(
        node: &str,
        edges: &HashMap<String, Vec<String>>,
        visiting: &mut HashSet<String>,
        done: &mut HashSet<String>,
    ) -> bool {
        if done.contains(node) {
            return false;
        }
        if !visiting.insert(node.to_string()) {
            return true;
        }
        if let Some(deps) = edges.get(node) {
            for dep in deps {
                if edges.contains_key(dep) && visit(dep, edges, visiting, done) {
                    return true;
                }
            }
        }
        visiting.remove(node);
        done.insert(node.to_string());
        false
    }
    let mut visiting = HashSet::new();
    let mut done = HashSet::new();
    if keys
        .iter()
        .any(|k| visit(k, &edges, &mut visiting, &mut done))
    {
        return Some(
            "the proposed split contains a dependency cycle; a circular dependency cannot be scheduled"
                .into(),
        );
    }
    None
}

/// Reject a split whose `verification` lists name commands this machine cannot
/// execute. Left unchecked, such a gate fails identically on every retry and the
/// node's correct work is reverted. The offending entry is fed back so the model
/// can supply a real command or move a visual check to `manual_verification`.
fn reject_unrunnable_gates(root: &std::path::Path, contracts: &[Contract]) -> Option<String> {
    let mut offenders: Vec<String> = Vec::new();
    for c in contracts {
        for bad in crate::verify::unrunnable_entries(root, &c.verification) {
            let label = if c.id.trim().is_empty() {
                c.goal.lines().next().unwrap_or("subtask").to_string()
            } else {
                c.id.trim().to_string()
            };
            offenders.push(format!("{label}: {bad:?}"));
        }
    }
    if offenders.is_empty() {
        None
    } else {
        Some(format!(
            "these verification entries are not executable commands: {}",
            offenders.join(", ")
        ))
    }
}

/// A terminally failed node must not leave its half-written work in the shared
/// tree. Revert its uncommitted files so the next node's diff, verification and
/// commit see only that next node's work. Ignored harness paths survive.
fn revert_failed_node(store: &Store, node: &Node) {
    if let Err(e) = crate::git::reset_uncommitted(&store.root) {
        store
            .append_log(
                node,
                &serde_json::json!({"event":"revert_failed","error":e}),
            )
            .ok();
    }
}

/// Which gates a node is accountable for. An integrating node and a node that
/// owns the whole project (a childless root, which has no parent) run the
/// detected whole-project suite; any other leaf runs only explicit contract
/// gates.
///
/// Without the root case a root that never splits is `Leaf`, its contract's
/// verification was detected on an empty directory at init and stays empty, so
/// no gate ever runs - the `dev`/`start` smoke gate was unreachable (trial 4
/// §7.3). Re-resolving at verification time detects the suite the finished
/// project actually has.
fn gate_scope(node: &Node, aggregating: bool) -> GateScope {
    if aggregating || node.parent.is_none() {
        GateScope::Integration
    } else {
        GateScope::Leaf
    }
}

fn run_one_node(
    store: &Store,
    node: &Node,
    model: &str,
    on_output: crate::runner::OutputFn,
    state: &Arc<Mutex<TuiState>>,
) -> std::result::Result<RunReport, String> {
    let mut report = RunReport::default();
    let children = store.children_of(node).map_err(|e| e.to_string())?;
    let has_children = !children.is_empty();
    let aggregating = node.status == SPLIT_STATUS || has_children;
    store.set_status(node, RUNNING).map_err(|e| e.to_string())?;
    {
        let mut s = state.lock().unwrap();
        s.nodes = store.walk().unwrap_or_default();
        s.node_id = node.id.clone();
        s.node_goal = node.goal.clone();
        s.node_started_at = std::time::Instant::now();
        s.status_line = format!("running {}", node.id);
    }

    // Budget exhaustion is terminal, never a silent continuation. An aggregating
    // node is exempt: its allowance was deliberately spent on its children, and
    // synthesising their verified results is the point of the rollup.
    if !aggregating && store.budget_enabled() {
        let remaining = store.budget_remaining(&node.id).unwrap_or(0);
        if remaining <= 0 {
            store
                .append_decision(
                    node,
                    "failed: token budget exhausted before this node could run",
                )
                .ok();
            store
                .append_log(
                    node,
                    &serde_json::json!({"event":"budget_exhausted","remaining":remaining}),
                )
                .ok();
            store.set_status(node, FAILED).ok();
            report.failed += 1;
            return Ok(report);
        }
    }

    let mut feedback: Option<String> = None;
    // Sticky: once an attempt got as far as the critic - which means every
    // objective gate passed - the work is substantially correct even if the
    // critic rejects it, and must not be wiped on terminal failure.
    let mut critic_contested = false;
    // Sticky: an attempt saw a verification entry that could not execute. Even if
    // the node later fails for another reason, its real diff is evidence worth
    // keeping rather than reverting.
    let mut manual_gate_seen = false;
    let max_attempts = max_attempts();

    for attempt in 0..max_attempts {
        if INTERRUPTED.load(Ordering::SeqCst) {
            return Ok(report);
        }
        if attempt > 0 {
            on_output(&format!(
                "  [{}] retry attempt {}/{}",
                node.id,
                attempt + 1,
                max_attempts
            ));
        }

        // The split this attempt may propose is validated against the allowance
        // the node had when it started, which is exactly what its context showed
        // it. Moving it with the call's own debit would make the displayed
        // budget unusable.
        let pre_remaining = if store.budget_enabled() {
            store.budget_remaining(&node.id).ok()
        } else {
            None
        };

        let result = match run_node(store, node, model, on_output.clone(), feedback.as_deref()) {
            Ok(r) => r,
            Err(RunnerError::Other(e)) => {
                if INTERRUPTED.load(Ordering::SeqCst) {
                    return Ok(report);
                }
                store
                    .append_log(node, &serde_json::json!({"event":"error","error":e}))
                    .ok();
                feedback = Some(format!("Runtime error on previous attempt: {e}. Please output a valid JSON decision (split or complete) as your very last line."));
                continue;
            }
            Err(RunnerError::NoDecision(e)) => {
                store
                    .append_log(node, &serde_json::json!({"event":"error","error":e}))
                    .ok();
                feedback = Some(format!("No valid JSON decision was found on the previous attempt ({e}). Remember: your VERY LAST line must be a single raw JSON decision object (e.g. {{\"verb\":\"complete\",\"deliverable\":\"...\",\"summary\":\"...\",\"artifacts\":[...]}} or {{\"verb\":\"split\",\"subtasks\":[...]}})."));
                continue;
            }
            Err(RunnerError::Timeout) => {
                store
                    .append_log(
                        node,
                        &serde_json::json!({"event":"error","error":"timeout"}),
                    )
                    .ok();
                feedback = Some("The previous attempt timed out. If the task is too large, output a `split` decision immediately.".into());
                continue;
            }
            Err(e) => {
                store
                    .append_log(
                        node,
                        &serde_json::json!({"event":"error","error":e.to_string()}),
                    )
                    .ok();
                feedback = Some(format!(
                    "Error on previous attempt: {e}. Output a valid JSON decision."
                ));
                continue;
            }
        };

        match result.verb.as_str() {
            NOTE_GLOBAL => {
                if let Ok(eid) = store.note_global(
                    &result.entry_type,
                    &result.entry_content,
                    &result.entry_supersedes,
                ) {
                    store
                        .append_log(
                            node,
                            &serde_json::json!({"event":"note_global","entry_id":eid}),
                        )
                        .ok();
                }
                continue;
            }
            ESCALATE => {
                report.escalations += 1;
                store
                    .append_log(
                        node,
                        &serde_json::json!({
                            "event": "escalate",
                            "assumption": result.assumption,
                            "evidence": result.evidence
                        }),
                    )
                    .ok();
                store
                    .append_decision(
                        node,
                        &format!(
                            "escalated: assumption='{}' evidence='{}'",
                            result.assumption, result.evidence
                        ),
                    )
                    .ok();

                // The challenged assumption is deliberately NOT injected as an
                // accepted constraint here. It is only settled once the owner
                // rules; treating it as law before then builds the whole subtree
                // on the very claim under dispute.
                let owner = match find_escalation_point(store, node, &result.assumption) {
                    Some(o) => o,
                    None => {
                        store
                            .append_decision(node, "failed: escalated with no owning ancestor")
                            .ok();
                        store.set_status(node, FAILED).ok();
                        report.failed += 1;
                        return Ok(report);
                    }
                };

                if let Err(e) = suspend_branch(store, node, &owner) {
                    store
                        .append_decision(node, &format!("failed: could not suspend branch: {e}"))
                        .ok();
                    store.set_status(node, FAILED).ok();
                    report.failed += 1;
                    return Ok(report);
                }

                store.set_status(&owner, RUNNING).ok();
                let escalation_feedback = format!(
                    "A descendant node `{child}` escalated a challenged assumption. You own it \
                     and must settle it before the branch can continue.\n\n\
                     Assumption: {assumption}\nEvidence: {evidence}\n\n\
                     Answer with EXACTLY one JSON decision as your very last line:\n\
                     {{\"verb\":\"escalate_resolve\",\"resolution\":\"amend\"|\"overrule\"|\"replan\"|\"depends_on\",\"amended_constraint\":\"...\",\"amended_interface\":\"...\",\"rationale\":\"...\",\"target\":\"...\",\"dependency\":\"...\"}}\n\
                     - amend: replace the false constraint (amended_constraint), optionally naming a sibling (target) whose interface (amended_interface) must be exposed\n\
                     - overrule: give a rationale (rationale) the child must address, then it continues\n\
                     - depends_on: require that a sibling (dependency) is completed first\n\
                     - replan: discard this branch's children and re-plan it",
                    child = node.id,
                    assumption = result.assumption,
                    evidence = result.evidence
                );
                let resolve = run_node(
                    store,
                    &owner,
                    model,
                    on_output.clone(),
                    Some(&escalation_feedback),
                );
                store.set_status(&owner, SPLIT_STATUS).ok();
                let resolve = match resolve {
                    Ok(r) => r,
                    Err(e) => {
                        store
                            .append_decision(
                                node,
                                &format!("failed: escalation resolution unusable: {e}"),
                            )
                            .ok();
                        store.set_status(node, FAILED).ok();
                        report.failed += 1;
                        return Ok(report);
                    }
                };

                if resolve.verb.as_str() != ESCALATE_RESOLVE {
                    store
                        .append_decision(
                            node,
                            &format!(
                                "failed: owner returned '{}' instead of an escalate_resolve",
                                resolve.verb
                            ),
                        )
                        .ok();
                    store.set_status(node, FAILED).ok();
                    report.failed += 1;
                    return Ok(report);
                }

                match resolve.resolution.trim() {
                    "amend" => {
                        if !resolve.amended_constraint.trim().is_empty() {
                            let _ = store.amend_inherited_constraint(
                                &owner,
                                &result.assumption,
                                resolve.amended_constraint.trim(),
                            );
                        }
                        if !resolve.amended_interface.trim().is_empty()
                            && !resolve.target.trim().is_empty()
                        {
                            if let Some(target) =
                                resolve_sibling(store, &owner, resolve.target.trim())
                            {
                                let _ =
                                    store.add_interface(&target, resolve.amended_interface.trim());
                            }
                        }
                        let _ = resume_ancestors(store, node, &owner);
                        store.set_status(node, RUNNING).ok();
                        feedback = Some(
                            "The owning ancestor amended the challenged constraint. Proceed \
                             under the amended terms and deliver your contract."
                                .into(),
                        );
                        continue;
                    }
                    "overrule" => {
                        let _ = resume_ancestors(store, node, &owner);
                        store.set_status(node, RUNNING).ok();
                        feedback = Some(format!(
                            "The owning ancestor overruled your escalation. Rationale: {}",
                            resolve.rationale.trim()
                        ));
                        continue;
                    }
                    "depends_on" => {
                        if let Some(dep) = resolve_sibling(store, &owner, resolve.dependency.trim())
                        {
                            let _ = store.add_depends_on(node, &dep.id);
                        }
                        let _ = resume_ancestors(store, node, &owner);
                        store.set_status(node, PENDING).ok();
                        return Ok(report);
                    }
                    "replan" => {
                        let parent = node
                            .parent
                            .as_ref()
                            .and_then(|pid| store.get(pid).ok())
                            .unwrap_or_else(|| owner.clone());
                        let parent_resolve = run_node(
                            store,
                            &parent,
                            model,
                            on_output.clone(),
                            Some(&escalation_feedback),
                        );
                        if matches!(&parent_resolve, Ok(pr)
                            if pr.verb == ESCALATE_RESOLVE
                                && pr.resolution.trim() == "replan")
                        {
                            let _ = replan_branch(store, &parent);
                            return Ok(report);
                        }
                        let _ = resume_ancestors(store, node, &owner);
                        store.set_status(node, RUNNING).ok();
                        feedback = None;
                        continue;
                    }
                    other => {
                        store
                            .append_decision(
                                node,
                                &format!(
                                    "failed: escalation resolved with unknown resolution '{other}'"
                                ),
                            )
                            .ok();
                        store.set_status(node, FAILED).ok();
                        report.failed += 1;
                        return Ok(report);
                    }
                }
            }
            ESCALATE_RESOLVE => {
                // Only an owner reopened to settle an escalation may return
                // this; a node that does so unprompted is failing closed rather
                // than being silently ignored.
                store
                    .append_log(
                        node,
                        &serde_json::json!({"event":"error","error":"escalate_resolve without an escalation"}),
                    )
                    .ok();
                store
                    .append_decision(
                        node,
                        "failed: returned escalate_resolve without an escalation",
                    )
                    .ok();
                store.set_status(node, FAILED).ok();
                report.failed += 1;
                return Ok(report);
            }
            SPLIT => {
                if aggregating {
                    report.refused += 1;
                    feedback = Some("SPLIT refused: this node has already split once and its children are finished. You MUST COMPLETE this contract directly.".into());
                    continue;
                }
                // The hard cap stays as a backstop: a budget must never be the
                // only bound, and it must never remove this one.
                if node.depth >= MAX_DEPTH {
                    report.refused += 1;
                    feedback = Some(format!(
                        "SPLIT refused: the tree is limited to {MAX_DEPTH} levels and this node is already at depth {}. You MUST COMPLETE this contract directly.",
                        node.depth
                    ));
                    continue;
                }
                if result.subtasks.is_empty() {
                    report.refused += 1;
                    feedback = Some("SPLIT refused: subtasks list was empty. Provide at least one concrete subtask.".into());
                    continue;
                }
                if let Some(reason) = reject_split_topology(
                    &result.subtasks,
                    &children.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
                ) {
                    report.refused += 1;
                    store
                        .append_log(
                            node,
                            &serde_json::json!({"event":"split_refused","reason":&reason}),
                        )
                        .ok();
                    store
                        .append_decision(node, &format!("split refused: {reason}"))
                        .ok();
                    feedback = Some(format!(
                        "SPLIT refused by the orchestrator: {reason}. No child nodes were created. \
                         Complete this contract yourself, or correct the subtasks and answer again."
                    ));
                    continue;
                }
                if let Some(reason) = reject_unrunnable_gates(&store.root, &result.subtasks) {
                    report.refused += 1;
                    store
                        .append_log(
                            node,
                            &serde_json::json!({"event":"split_refused","reason":&reason}),
                        )
                        .ok();
                    store
                        .append_decision(node, &format!("split refused: {reason}"))
                        .ok();
                    feedback = Some(format!(
                        "SPLIT refused by the orchestrator: {reason}. No child nodes were created. \
                         Every `verification` entry must be a command that runs on this machine \
                         (e.g. `npm test`, `cargo test`, `python3 -m pytest`). Put visual or \
                         behavioural checks like \"clean rendering\" in `manual_verification` \
                         instead, then answer again."
                    ));
                    continue;
                }
                if store.budget_enabled() {
                    // The split-fee plus the child allocations must fit inside
                    // the allowance the node had when it started. This is what
                    // makes depth economic rather than magic.
                    let remaining = pre_remaining.unwrap_or(0);
                    let proposed: i64 = result.subtasks.iter().map(|c| c.allocation.max(0)).sum();
                    let fee = store.split_fee();
                    if fee + proposed > remaining {
                        let reason = format!(
                            "your proposed allocation is over budget: the {fee}-token split-fee \
                             plus {proposed} in child allocations exceeds your remaining token \
                             allowance of {remaining}"
                        );
                        report.refused += 1;
                        store
                            .append_log(
                                node,
                                &serde_json::json!({"event":"split_refused","reason":&reason}),
                            )
                            .ok();
                        store
                            .append_decision(node, &format!("split refused: {reason}"))
                            .ok();
                        feedback = Some(format!(
                            "SPLIT refused by the orchestrator: {reason}. No child nodes were \
                             created. Complete this contract yourself now, or propose smaller \
                             allocations that fit."
                        ));
                        continue;
                    }
                }
                match store.add_children(node, &result.subtasks) {
                    Ok(children) => {
                        let ids: Vec<&str> = children.iter().map(|c| c.id.as_str()).collect();
                        store
                            .append_decision(node, &format!("split into {}", ids.join(", ")))
                            .ok();
                        report.split += 1;
                        {
                            let mut s = state.lock().unwrap();
                            s.nodes = store.walk().unwrap_or_default();
                        }
                        return Ok(report);
                    }
                    Err(e) => {
                        report.refused += 1;
                        store
                            .append_log(
                                node,
                                &serde_json::json!({"event":"split_refused","reason":e.to_string()}),
                            )
                            .ok();
                        feedback = Some(format!("SPLIT rejected: {e}"));
                        continue;
                    }
                }
            }
            COMPLETE_VERB => {
                if !has_children
                    && result.artifacts.iter().all(|(_, c)| c.trim().is_empty())
                    && result.deliverable.trim().is_empty()
                    && result.summary.trim().is_empty()
                {
                    report.refused += 1;
                    feedback = Some("COMPLETE refused: no summary, deliverable, or code modifications were provided.".into());
                    continue;
                }

                // Fail closed on an empty diff. A leaf is where work lands, so
                // a leaf that describes its work but changed nothing on disk has
                // not delivered its contract - however plausible its prose.
                if !has_children && !crate::git::has_uncommitted_changes(&store.root) {
                    report.verify_failures += 1;
                    report.refused += 1;
                    store
                        .append_log(
                            node,
                            &serde_json::json!({
                                "event": "verify_failed",
                                "reason": "no file changed on disk"
                            }),
                        )
                        .ok();
                    feedback = Some(
                        "Verification FAILED: this node changed no file on disk. A leaf must \
                         leave a real change behind - implement the contract in the project, \
                         not just describe it."
                            .into(),
                    );
                    continue;
                }

                // Gate 1: the project's own commands. Their exit codes are the
                // only evidence that cannot be talked around, so they run before
                // any critic is consulted.
                //
                // Scope matters: a leaf is held only to gates its contract names,
                // because whole-project commands cannot pass until its siblings
                // exist. Integrating parents carry the project-wide suite, which
                // is exactly where cross-module breakage is both detectable and
                // fixable - and where `reopen` can push it back down.
                let contract = node.contract();
                let scope = gate_scope(node, aggregating);
                let gates =
                    crate::verify::resolve_gates(&store.root, &contract.verification, scope);
                if !gates.is_empty() {
                    on_output(&format!(
                        "  [{}] running {} verification gate(s)",
                        node.id,
                        gates.len()
                    ));
                    // A gate that launches the app writes runtime state
                    // (`.portfolio.json` and friends). Snapshot the untracked set
                    // before the gates run, then exclude whatever they created:
                    // that is the app's state, not this node's work, and it must
                    // not enter the diff, the critic's evidence or history.
                    let pre_gate_untracked = crate::git::untracked_files(&store.root);
                    let outcomes = crate::verify::run_gates(&store.root, &gates, GATE_TIMEOUT_SECS);
                    let manual: Vec<String> = outcomes
                        .iter()
                        .filter(|o| o.manual)
                        .map(|o| o.command.clone())
                        .collect();
                    if !manual.is_empty() {
                        manual_gate_seen = true;
                        store
                            .append_log(
                                node,
                                &serde_json::json!({
                                    "event":"gate_manual",
                                    "commands": manual,
                                    "note":"not executable on this machine; deferred to the critic",
                                }),
                            )
                            .ok();
                        store
                            .append_decision(
                                node,
                                &format!(
                                    "manual verification (critic-owned, not executed): {}",
                                    manual.join(", ")
                                ),
                            )
                            .ok();
                    }
                    let runtime: Vec<String> = crate::git::untracked_files(&store.root)
                        .difference(&pre_gate_untracked)
                        .cloned()
                        .collect();
                    if !runtime.is_empty() {
                        if let Err(e) = crate::git::ignore_runtime_paths(&store.root, &runtime) {
                            store
                                .append_log(
                                    node,
                                    &serde_json::json!({"event":"runtime_ignore_failed","error":e}),
                                )
                                .ok();
                        } else {
                            store
                                .append_log(
                                    node,
                                    &serde_json::json!({
                                        "event":"runtime_state_ignored",
                                        "files": runtime,
                                    }),
                                )
                                .ok();
                        }
                    }
                    if let Some(failures) = crate::verify::format_failures(&outcomes) {
                        report.verify_failures += 1;
                        report.refused += 1;
                        store
                            .append_log(
                                node,
                                &serde_json::json!({"event":"gate_failed","detail":&failures}),
                            )
                            .ok();
                        feedback = Some(failures);
                        continue;
                    }
                    store
                        .append_decision(node, &format!("gates passed: {}", gates.join(" && ")))
                        .ok();
                }

                // Manual checks (visual/UX) carry no exit code, so the critic is
                // the only honest judge: they are appended to the criteria it
                // grades. They are never executed as gates.
                let mut criteria = contract.acceptance_criteria;
                criteria.extend(contract.manual_verification.iter().cloned());
                report.verifications += 1;
                match verify_node(
                    store,
                    node,
                    &result.deliverable,
                    &result.artifacts,
                    &criteria,
                    model,
                ) {
                    Ok((verdict, _crit_details)) if verdict == "PASS" => {
                        store
                            .complete(
                                node,
                                &result.summary,
                                &result.deliverable,
                                &result.artifacts,
                            )
                            .map_err(|e| e.to_string())?;
                        store.append_decision(node, "verified: verdict=PASS").ok();

                        // One commit per verified node, so the code's history and
                        // the tree's history are the same history and any single
                        // node's contribution stays attributable and revertible.
                        match crate::git::commit_node_work(&store.root, &node.id, &result.summary) {
                            Ok(Some(sha)) => {
                                let short: String = sha.chars().take(8).collect();
                                let files = crate::git::changed_files_since(
                                    &store.root,
                                    &format!("{sha}~1"),
                                );
                                store
                                    .append_decision(
                                        node,
                                        &format!("committed {} ({} file(s))", short, files.len()),
                                    )
                                    .ok();
                                on_output(&format!("  [{}] committed {}", node.id, short));
                            }
                            Ok(None) => {
                                // Legitimate for a pure decomposition step.
                            }
                            Err(e) => {
                                store
                                    .append_log(
                                        node,
                                        &serde_json::json!({"event":"commit_failed","error":e}),
                                    )
                                    .ok();
                            }
                        }

                        report.completed += 1;
                        report.node_depths.push(node.depth);
                        {
                            let mut s = state.lock().unwrap();
                            s.nodes = store.walk().unwrap_or_default();
                        }
                        return Ok(report);
                    }
                    Ok((_, crit_details)) => {
                        critic_contested = true;
                        report.verify_failures += 1;
                        report.refused += 1;
                        let reasons: Vec<String> = crit_details
                            .iter()
                            .filter_map(|c| {
                                let name = c.get("name").and_then(|n| n.as_str()).unwrap_or("");
                                let reason = c.get("reason").and_then(|r| r.as_str()).unwrap_or("");
                                if !c.get("pass").and_then(|p| p.as_bool()).unwrap_or(true) {
                                    Some(format!("- Criterion '{name}' FAILED: {reason}"))
                                } else {
                                    None
                                }
                            })
                            .collect();
                        feedback = Some(format!(
                            "Verification FAILED. Please fix the following:\n{}",
                            if reasons.is_empty() {
                                "Deliverable did not satisfy acceptance criteria.".into()
                            } else {
                                reasons.join("\n")
                            }
                        ));
                        continue;
                    }
                    Err(e) => {
                        store
                            .append_log(
                                node,
                                &serde_json::json!({"event":"verify_error","error":e.to_string()}),
                            )
                            .ok();
                        report.refused += 1;
                        feedback = Some(format!("Verifier error: {e}"));
                        continue;
                    }
                }
            }
            REOPEN => {
                if children.is_empty() {
                    report.refused += 1;
                    feedback = Some(
                        "REOPEN refused: this node has no children to send back. Fix the contract yourself or escalate."
                            .into(),
                    );
                    continue;
                }
                if result.reopen_reason.trim().is_empty() {
                    report.refused += 1;
                    feedback = Some(
                        "REOPEN refused: a reason is required so the reopened child knows what to fix."
                            .into(),
                    );
                    continue;
                }
                match store.reopen_children(node, &result.reopen_children, &result.reopen_reason) {
                    Ok(reopened) if !reopened.is_empty() => {
                        on_output(&format!(
                            "  [{}] reopened {} for rework",
                            node.id,
                            reopened.join(", ")
                        ));
                        store
                            .append_log(
                                node,
                                &serde_json::json!({
                                    "event": "reopen",
                                    "children": reopened,
                                    "reason": result.reopen_reason,
                                }),
                            )
                            .ok();
                        report.split += 1;
                        {
                            let mut s = state.lock().unwrap();
                            s.nodes = store.walk().unwrap_or_default();
                        }
                        return Ok(report);
                    }
                    Ok(_) => {
                        report.refused += 1;
                        let ids: Vec<&str> = children.iter().map(|c| c.id.as_str()).collect();
                        feedback = Some(format!(
                            "REOPEN matched no child. Use exact ids from this node's own children: {}",
                            ids.join(", ")
                        ));
                        continue;
                    }
                    Err(e) => {
                        report.refused += 1;
                        feedback = Some(format!("REOPEN failed: {e}"));
                        continue;
                    }
                }
            }
            _ => {
                report.refused += 1;
                feedback = Some(format!("Unknown or unhandled verb: {}", result.verb));
                continue;
            }
        }
    }

    // A retries-exhausted node normally has its uncommitted work reverted, so it
    // cannot contaminate a sibling's diff. The exception is work that is
    // substantially correct even though the node failed: a node the objective
    // gates accepted and only the critic rejected, or a node whose gates could
    // not execute at all. Commit that as an explicit unverified checkpoint and
    // let the scheduler skip the revert, so a later retry or reopen builds on it
    // instead of starting from zero - which is exactly what the trial lost.
    let checkpoint_reason = if critic_contested {
        Some("critic contested")
    } else if manual_gate_seen {
        Some("gate could not execute")
    } else {
        None
    };
    if let Some(reason) = checkpoint_reason {
        if !crate::git::has_uncommitted_changes(&store.root) {
            // Nothing to preserve; fall through to the terminal failure.
        } else {
            match crate::git::commit_node_work(
                &store.root,
                &node.id,
                &format!("unverified checkpoint ({reason})"),
            ) {
                Ok(Some(sha)) => {
                    let short: String = sha.chars().take(8).collect();
                    store
                        .append_decision(
                            node,
                            &format!("committed unverified checkpoint {short} ({reason})"),
                        )
                        .ok();
                    report.checkpoint_committed = true;
                }
                Ok(None) => {}
                Err(e) => {
                    store
                        .append_log(
                            node,
                            &serde_json::json!({"event":"checkpoint_failed","error":e}),
                        )
                        .ok();
                }
            }
        }
    }

    store
        .append_decision(node, "failed: no usable answer after repeated retries")
        .ok();
    store.set_status(node, FAILED).ok();
    report.failed += 1;
    Ok(report)
}

// ---------------------------------------------------------------------------
// Upward escalation (SPEC 4.3 / Phase 3)
// ---------------------------------------------------------------------------

/// The nearest ancestor that owns the challenged assumption: the first ancestor
/// (nearest first) whose contract lists it as an inherited constraint. When no
/// ancestor names it - a discovered dependency rather than a falsified law - the
/// escalation goes to the direct parent, which owns the sibling topology.
fn find_escalation_point(store: &Store, node: &Node, assumption: &str) -> Option<Node> {
    let nodes = store.walk().ok()?;
    let by_id: HashMap<&str, &Node> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut chain: Vec<Node> = Vec::new();
    let mut cursor = node.parent.as_deref();
    while let Some(pid) = cursor {
        match by_id.get(pid) {
            Some(parent) => {
                chain.push((*parent).clone());
                cursor = parent.parent.as_deref();
            }
            None => break,
        }
    }
    if let Some(owner) = chain.iter().find(|anc| {
        anc.contract()
            .constraints
            .iter()
            .any(|c| c.trim() == assumption.trim())
    }) {
        return Some(owner.clone());
    }
    chain.first().cloned()
}

/// Mark `node` and its ancestors down to (but not including) `owner` suspended,
/// so nothing in the challenged branch runs while the escalation is settled.
fn suspend_branch(store: &Store, node: &Node, owner: &Node) -> Result<(), String> {
    let mut current = Some(node.clone());
    while let Some(n) = current {
        if n.id == owner.id {
            break;
        }
        store.set_status(&n, SUSPENDED).map_err(|e| e.to_string())?;
        current = n.parent.as_deref().and_then(|pid| store.get(pid).ok());
    }
    Ok(())
}

/// Return the suspended intermediate ancestors to SPLIT so they can aggregate
/// once the escalated leaf has resumed.
fn resume_ancestors(store: &Store, node: &Node, owner: &Node) -> Result<(), String> {
    let mut current = node.parent.as_deref().and_then(|pid| store.get(pid).ok());
    while let Some(n) = current {
        if n.id == owner.id {
            break;
        }
        store
            .set_status(&n, SPLIT_STATUS)
            .map_err(|e| e.to_string())?;
        current = n.parent.as_deref().and_then(|pid| store.get(pid).ok());
    }
    Ok(())
}

/// Find a sibling of `owner` named by an escalation resolution. Resolutions
/// name a sibling by its node id, its trailing id segment, or its goal text.
fn resolve_sibling(store: &Store, owner: &Node, tag: &str) -> Option<Node> {
    if tag.is_empty() {
        return None;
    }
    store
        .children_of(owner)
        .ok()?
        .into_iter()
        .find(|c| c.id == tag || c.id.ends_with(tag) || c.goal.contains(tag))
}

/// Prune a branch's children for `replan`, compacting each child's trace into
/// the parent's log before deleting it, then return the parent to PENDING.
fn replan_branch(store: &Store, parent: &Node) -> Result<(), String> {
    for child in store.children_of(parent).map_err(|e| e.to_string())? {
        store
            .compact_child_trace(parent, &child)
            .map_err(|e| e.to_string())?;
        store.delete_node(&child).map_err(|e| e.to_string())?;
    }
    store.set_status(parent, PENDING).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, status: &str, parent: Option<&str>, depth: i64, deps: &[&str]) -> Node {
        Node {
            id: id.into(),
            path: std::path::PathBuf::from(id),
            parent: parent.map(|p| p.to_string()),
            depth,
            status: status.into(),
            goal: id.into(),
            summary: String::new(),
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
            dep_fp: "{}".into(),
        }
    }

    #[test]
    fn a_stale_dependent_is_selected_before_fresh_work() {
        let nodes = vec![
            node("root", SPLIT_STATUS, None, 1, &[]),
            node("root-00", COMPLETE, Some("root"), 2, &[]),
            node("root-01", COMPLETE, Some("root"), 2, &["root-00"]),
            node("root-02", PENDING, Some("root"), 2, &[]),
        ];
        let mut stale = HashSet::new();
        stale.insert("root-01".to_string());
        let picked = next_nodes(&nodes, &stale);
        assert_eq!(
            picked.first().map(|n| n.id.as_str()),
            Some("root-01"),
            "the stale dependent must be re-run first"
        );
    }

    #[test]
    fn a_pending_node_waits_for_a_stale_dependency() {
        let nodes = vec![
            node("root", SPLIT_STATUS, None, 1, &[]),
            node("root-01", COMPLETE, Some("root"), 2, &[]),
            node("root-02", PENDING, Some("root"), 2, &["root-01"]),
        ];
        let fresh = next_nodes(&nodes, &HashSet::new());
        assert!(fresh.iter().any(|n| n.id == "root-02"));

        let mut stale = HashSet::new();
        stale.insert("root-01".to_string());
        let picked = next_nodes(&nodes, &stale);
        assert!(
            picked.iter().all(|n| n.id != "root-02"),
            "a dependent must not run while its dependency is stale"
        );
    }

    #[test]
    fn an_aggregator_waits_for_a_stale_child() {
        let nodes = vec![
            node("root", SPLIT_STATUS, None, 1, &[]),
            node("root-01", COMPLETE, Some("root"), 2, &[]),
        ];
        assert!(aggregatable(&nodes[0], &nodes, &HashSet::new()));
        let mut stale = HashSet::new();
        stale.insert("root-01".to_string());
        assert!(!aggregatable(&nodes[0], &nodes, &stale));
    }

    #[test]
    fn split_topology_rejects_unknown_siblings_and_cycles() {
        let valid = vec![
            Contract {
                goal: "a".into(),
                id: "a".into(),
                ..Default::default()
            },
            Contract {
                goal: "b".into(),
                id: "b".into(),
                depends_on: vec!["a".into()],
                ..Default::default()
            },
        ];
        assert!(reject_split_topology(&valid, &[]).is_none());

        let unknown = vec![Contract {
            goal: "b".into(),
            id: "b".into(),
            depends_on: vec!["ghost".into()],
            ..Default::default()
        }];
        assert!(reject_split_topology(&unknown, &[])
            .unwrap()
            .contains("unknown sibling"));

        let cyclic = vec![
            Contract {
                goal: "a".into(),
                id: "a".into(),
                depends_on: vec!["b".into()],
                ..Default::default()
            },
            Contract {
                goal: "b".into(),
                id: "b".into(),
                depends_on: vec!["a".into()],
                ..Default::default()
            },
        ];
        assert!(reject_split_topology(&cyclic, &[])
            .unwrap()
            .contains("cycle"));
    }

    /// D3: a failed descendant must make the run report its root as failed, even
    /// though the root itself stays `split` so it can still be retried.
    #[test]
    fn a_failed_branch_surfaces_as_a_failed_root_report() {
        let nodes = vec![
            node("root", SPLIT_STATUS, None, 1, &[]),
            node("root-01", COMPLETE, Some("root"), 2, &[]),
            node("root-02", FAILED, Some("root"), 2, &[]),
        ];
        assert_eq!(surface_root_status(&nodes), FAILED);

        let healthy = vec![
            node("root", SPLIT_STATUS, None, 1, &[]),
            node("root-01", COMPLETE, Some("root"), 2, &[]),
        ];
        assert_eq!(surface_root_status(&healthy), SPLIT_STATUS);

        let done = vec![node("root", COMPLETE, None, 1, &[])];
        assert_eq!(surface_root_status(&done), COMPLETE);
    }

    /// Section-4b regression: a root that never splits must still run the
    /// whole-project gates. At init its contract was auto-detected on an empty
    /// directory and stays empty, and `GateScope::Leaf` would run nothing at all
    /// - so the `dev`/`start` smoke gate was unreachable (trial 4 §7.3).
    #[test]
    fn a_non_splitting_root_runs_whole_project_gates() {
        let dir = std::env::temp_dir().join(format!("fractal_rootgates_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"scripts":{"build":"vite build","test":"vitest run","start":"tsx src/index.tsx"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();

        let root = node("root", RUNNING, None, 1, &[]);
        let leaf = node("root-01", RUNNING, Some("root"), 2, &[]);
        assert_eq!(
            gate_scope(&root, false),
            GateScope::Integration,
            "a childless root owns the whole project and must run its gates"
        );
        assert_eq!(gate_scope(&leaf, false), GateScope::Leaf);
        assert_eq!(
            gate_scope(&leaf, true),
            GateScope::Integration,
            "an aggregating node keeps the integration scope"
        );

        let gates = crate::verify::resolve_gates(&dir, &[], gate_scope(&root, false));
        assert_eq!(
            gates,
            crate::verify::detect_gates(&dir),
            "the root must run the suite detected at verification time"
        );
        assert!(
            gates.iter().any(|g| g.contains("tsc --noEmit")),
            "the root must run the project's typecheck: {gates:?}"
        );
        assert!(
            gates.iter().any(|g| crate::verify::is_launch_command(g)),
            "the launch smoke gate must be reachable on a non-splitting root: {gates:?}"
        );
        assert!(
            crate::verify::resolve_gates(&dir, &[], gate_scope(&leaf, false)).is_empty(),
            "a leaf must not inherit whole-project gates"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A split's prose or missing-binary gate is rejected at split time with the
    /// offending entry named, so the model can supply a real command.
    #[test]
    fn split_with_unrunnable_gate_is_rejected() {
        let dir = std::env::temp_dir().join(format!("fractal_splitgate_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bad = vec![Contract {
            id: "a".into(),
            goal: "build the TUI".into(),
            verification: vec!["manual smoke test".into()],
            ..Default::default()
        }];
        let reason = reject_unrunnable_gates(&dir, &bad).expect("must reject");
        assert!(
            reason.contains("manual smoke test") && reason.contains('a'),
            "the offending entry and child must be named: {reason}"
        );

        let good = vec![Contract {
            id: "a".into(),
            goal: "build the TUI".into(),
            verification: vec!["true".into()],
            manual_verification: vec!["the layout is clean".into()],
            ..Default::default()
        }];
        assert!(reject_unrunnable_gates(&dir, &good).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A single verification may fail across several retries, so the trace's
    /// catch rate must be reported as a bounded rate, never above 1.
    #[test]
    fn verify_catch_rate_is_bounded_to_one() {
        let r = RunReport {
            verifications: 5,
            verify_failures: 6,
            ..Default::default()
        };
        let path = std::env::temp_dir().join(format!(
            "fractal_trace_bound_{}_{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        r.write_trace(path.to_str().unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let rate = v.get("verify_catch_rate").unwrap().as_f64().unwrap();
        assert!(rate <= 1.0, "catch rate exceeded 1: {rate}");
        let _ = std::fs::remove_file(&path);
    }
}
