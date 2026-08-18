use crate::runner::{
    run_node, verify_node, RunnerError, COMPLETE_VERB, ESCALATE, NOTE_GLOBAL, SPLIT,
};
use crate::store::{
    Node, Store, StoreError, COMPLETE, FAILED, PENDING, RUNNING, SPLIT as SPLIT_STATUS,
};
use crate::tui::{StatsSnapshot, TuiState};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) static INTERRUPTED: AtomicBool = AtomicBool::new(false);

const MAX_DEPTH: i64 = 4;
const MAX_ATTEMPTS: usize = 3;
const MAX_STEPS: usize = 500;

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
        }
    }
}

impl RunReport {
    pub fn ok(&self) -> bool {
        self.root_status != FAILED
            && [COMPLETE, SPLIT_STATUS].contains(&self.root_status.as_str())
    }

    pub fn write_trace(&self, path: &str) {
        let mut depths_count: HashMap<i64, usize> = HashMap::new();
        for d in &self.node_depths {
            *depths_count.entry(*d).or_default() += 1;
        }
        let max_depth = self.node_depths.iter().max().copied().unwrap_or(1);
        let verify_catch = if self.verifications > 0 {
            self.verify_failures as f64 / self.verifications as f64
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
        let _ = std::fs::write(path, serde_json::to_string_pretty(&record).unwrap_or_default());
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
    }
}

pub fn run(
    store: &Store,
    state: &Arc<Mutex<TuiState>>,
    model: &str,
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
        let runnable = next_nodes(&nodes);
        if runnable.is_empty() {
            // Check if any failed or blocked nodes exist
            let has_failed = nodes.iter().any(|n| n.status == FAILED);
            let all_complete = nodes.iter().all(|n| n.status == COMPLETE || n.status == SPLIT_STATUS);
            let mut s = state.lock().unwrap();
            if has_failed {
                s.status_line = "Blocked: some subtasks failed. Press 'r' on failed nodes to retry, or 'q' to exit.".into();
            } else if all_complete {
                s.status_line = "All tasks completed successfully.".into();
            } else {
                s.status_line = "No runnable tasks. Dependencies may be unsatisfied.".into();
            }
            break;
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

        use std::sync::mpsc;
        let (tx, rx) = mpsc::channel::<(String, std::result::Result<RunReport, String>)>();

        std::thread::scope(|scope| {
            for node in batch {
                let state_ttui = state.clone();
                let state_output = state.clone();
                let nid = node.id.clone();
                let n = node.clone();
                let m = model_owned.clone();
                let tx = tx.clone();
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

                scope.spawn(move || {
                    let r = run_one_node(store, &n, &m, on_output, &state_ttui);
                    let _ = tx.send((n.id.clone(), r));
                });
            }
            drop(tx);
        });

        for (node_id, result) in rx.iter() {
            match result {
                Ok(sub) => {
                    report.merge(&sub);
                }
                Err(e) => {
                    let nodes = store.walk().unwrap_or_default();
                    if let Some(n) = nodes.iter().find(|n2| n2.id == node_id) {
                        let _ = store.append_decision(&n, &format!("error: {e}"));
                        let _ = store.set_status(&n, FAILED);
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
    report.root_status = nodes
        .first()
        .map(|n| n.status.clone())
        .unwrap_or_else(|| PENDING.into());
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
            s.status_line = format!("stopped — root: {} (press r on a node to retry)", report.root_status);
        }
    }
    Ok(report)
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

fn next_nodes(nodes: &[Node]) -> Vec<Node> {
    let by_id: HashMap<&str, &Node> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut runnable: Vec<Node> = nodes
        .iter()
        .filter(|n| n.status == PENDING && deps_satisfied(n, &by_id))
        .cloned()
        .collect();
    runnable.sort_by(|a, b| b.depth.cmp(&a.depth));
    if !runnable.is_empty() {
        return runnable;
    }

    let agg: Vec<Node> = nodes
        .iter()
        .filter(|n| n.status == SPLIT_STATUS && aggregatable(n, &by_id, nodes))
        .cloned()
        .collect();
    agg
}

fn deps_satisfied(node: &Node, by_id: &HashMap<&str, &Node>) -> bool {
    node.depends_on
        .iter()
        .all(|dep| by_id.get(dep.as_str()).map_or(false, |n| n.status == COMPLETE))
}

fn aggregatable(node: &Node, _by_id: &HashMap<&str, &Node>, nodes: &[Node]) -> bool {
    let children: Vec<&Node> = nodes
        .iter()
        .filter(|n| n.parent.as_deref() == Some(&node.id))
        .collect();
    !children.is_empty() && children.iter().all(|c| [COMPLETE, FAILED].contains(&c.status.as_str()))
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
    let aggregating = node.status == SPLIT_STATUS;
    store.set_status(node, RUNNING).map_err(|e| e.to_string())?;
    {
        let mut s = state.lock().unwrap();
        s.nodes = store.walk().unwrap_or_default();
        s.node_id = node.id.clone();
        s.node_goal = node.goal.clone();
        s.node_started_at = std::time::Instant::now();
        s.status_line = format!("running {}", node.id);
    }

    let mut feedback: Option<String> = None;

    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            on_output(&format!(
                "  [{}] retry attempt {}/{}",
                node.id,
                attempt + 1,
                MAX_ATTEMPTS
            ));
        }

        let result = match run_node(store, node, model, on_output.clone(), feedback.as_deref()) {
            Ok(r) => r,
            Err(RunnerError::Other(e)) => {
                store
                    .append_log(node, &serde_json::json!({"event":"error","error":e}))
                    .ok();
                feedback = Some(format!("Runtime error on previous attempt: {e}"));
                continue;
            }
            Err(e) => {
                store
                    .append_log(node, &serde_json::json!({"event":"error","error":e.to_string()}))
                    .ok();
                store.append_decision(node, &format!("failed: {e}")).ok();
                store.set_status(node, FAILED).ok();
                report.failed += 1;
                {
                    let mut s = state.lock().unwrap();
                    s.nodes = store.walk().unwrap_or_default();
                }
                return Ok(report);
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
                        .append_log(node, &serde_json::json!({"event":"note_global","entry_id":eid}))
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

                if let Some(ref pid) = node.parent {
                    let nodes = store.walk().unwrap_or_default();
                    if let Some(parent) = nodes.iter().find(|n| n.id == *pid) {
                        let escalation_msg = format!(
                            "Child {} escalated assumption: '{}' with evidence: '{}'",
                            node.id, result.assumption, result.evidence
                        );
                        let _ = store.append_decision(parent, &escalation_msg);
                        let constraint = format!(
                            "Assumption invalid (from {}): {}",
                            node.id, result.assumption
                        );
                        let _ = store.add_constraint_and_propagate(&parent.id, &constraint);
                    }
                }

                store.append_decision(
                    node,
                    &format!(
                        "escalated: assumption='{}' evidence='{}'",
                        result.assumption, result.evidence
                    ),
                ).ok();

                return Ok(report);
            }
            SPLIT => {
                if aggregating || (node.depth >= MAX_DEPTH && !store.budget_enabled()) {
                    report.refused += 1;
                    feedback = Some("SPLIT refused: max depth reached or aggregating node. You MUST COMPLETE this contract directly.".into());
                    continue;
                }
                if result.subtasks.is_empty() {
                    report.refused += 1;
                    feedback = Some("SPLIT refused: subtasks list was empty. Provide at least one concrete subtask.".into());
                    continue;
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
                if children.is_empty()
                    && result.artifacts.iter().all(|(_, c)| c.trim().is_empty())
                    && result.deliverable.trim().is_empty()
                {
                    report.refused += 1;
                    feedback = Some("COMPLETE refused: no artifacts or deliverable content was provided. You must write the implementation into artifacts/".into());
                    continue;
                }
                let criteria = node.contract().acceptance_criteria;
                report.verifications += 1;
                match verify_node(store, node, &result.deliverable, &criteria, model) {
                    Ok((verdict, crit_details)) if verdict == "PASS" => {
                        store
                            .complete(node, &result.summary, &result.deliverable, &result.artifacts)
                            .map_err(|e| e.to_string())?;
                        store.append_decision(node, "verified: verdict=PASS").ok();
                        report.completed += 1;
                        report.node_depths.push(node.depth);
                        {
                            let mut s = state.lock().unwrap();
                            s.nodes = store.walk().unwrap_or_default();
                        }
                        return Ok(report);
                    }
                    Ok((_, crit_details)) => {
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
            _ => {
                report.refused += 1;
                feedback = Some(format!("Unknown or unhandled verb: {}", result.verb));
                continue;
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
