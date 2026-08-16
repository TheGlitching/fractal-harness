use crate::runner::{RunnerError, COMPLETE_VERB, ESCALATE, NOTE_GLOBAL, SPLIT, run_node, verify_node};
use crate::store::{Node, Store, StoreError, COMPLETE, FAILED, PENDING, RUNNING, SPLIT as SPLIT_STATUS};
use crate::tui::{StatsSnapshot, TuiState};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) static INTERRUPTED: AtomicBool = AtomicBool::new(false);

const MAX_DEPTH: i64 = 3;
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
        RunReport { steps: 0, completed: 0, split: 0, refused: 0, failed: 0,
            root_status: PENDING.into(), escalations: 0, verifications: 0, verify_failures: 0, node_depths: vec![] }
    }
}

impl RunReport {
    pub fn ok(&self) -> bool {
        self.root_status != FAILED && [COMPLETE, SPLIT_STATUS].contains(&self.root_status.as_str())
    }

    pub fn write_trace(&self, path: &str) {
        let mut depths_count: HashMap<i64, usize> = HashMap::new();
        for d in &self.node_depths { *depths_count.entry(*d).or_default() += 1; }
        let max_depth = self.node_depths.iter().max().copied().unwrap_or(1);
        let verify_catch = if self.verifications > 0 { self.verify_failures as f64 / self.verifications as f64 } else { 0.0 };
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

pub fn run(store: &Store, state: &Arc<Mutex<TuiState>>, model: &str) -> std::result::Result<RunReport, StoreError> {
    store.reconcile()?;
    let nodes = store.walk()?;
    for n in &nodes {
        if n.status == RUNNING { store.set_status(n, PENDING)?; }
    }
    let mut report = RunReport::default();
    let limit = std::env::var("FRACTAL_MAX_STEPS").ok().and_then(|s| s.parse().ok()).unwrap_or(MAX_STEPS);
    let max_parallel: usize = std::env::var("FRACTAL_PARALLEL").ok().and_then(|s| s.parse().ok()).unwrap_or(4);
    let model_owned = model.to_string();

    while report.steps < limit {
        if INTERRUPTED.load(Ordering::SeqCst) {
            let mut s = state.lock().unwrap();
            s.status_line = "interrupted".into();
            break;
        }
        let nodes = store.walk()?;
        let runnable = next_nodes(&nodes);
        if runnable.is_empty() { break; }

        let batch = &runnable[..runnable.len().min(max_parallel)];
        for _n in batch { report.steps += 1; }
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
                        if !clean.is_empty() { s.last_activity = clean; }
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
                Ok(sub) => { report.merge(&sub); }
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
    report.root_status = nodes.first().map(|n| n.status.clone()).unwrap_or_else(|| PENDING.into());
    if let Some(project) = store.tree_dir.parent() {
        report.write_trace(&project.join("trace.json").to_string_lossy());
    }
    {
        let sn = snapshot(&report, &nodes);
        let mut s = state.lock().unwrap();
        s.nodes = nodes;
        s.stats = sn;
        s.done = true;
        s.status_line = format!("done — root: {}", report.root_status);
    }
    Ok(report)
}

fn snapshot(report: &RunReport, nodes: &[Node]) -> StatsSnapshot {
    let goal_of = |status: &str| -> Vec<String> {
        nodes.iter()
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
    let mut runnable: Vec<Node> = nodes.iter()
        .filter(|n| n.status == PENDING && deps_satisfied(n, &by_id))
        .cloned()
        .collect();
    runnable.sort_by(|a, b| b.depth.cmp(&a.depth));
    if !runnable.is_empty() { return runnable; }

    let agg: Vec<Node> = nodes.iter()
        .filter(|n| n.status == SPLIT_STATUS && aggregatable(n, &by_id, nodes))
        .cloned()
        .collect();
    agg
}

fn deps_satisfied(node: &Node, by_id: &HashMap<&str, &Node>) -> bool {
    node.depends_on.iter().all(|dep| by_id.get(dep.as_str()).map_or(false, |n| n.status == COMPLETE))
}

fn aggregatable(node: &Node, _by_id: &HashMap<&str, &Node>, nodes: &[Node]) -> bool {
    let children: Vec<&Node> = nodes.iter().filter(|n| n.parent.as_deref() == Some(&node.id)).collect();
    !children.is_empty() && children.iter().all(|c| [COMPLETE, FAILED].contains(&c.status.as_str()))
}

fn run_one_node(store: &Store, node: &Node, model: &str, on_output: crate::runner::OutputFn, state: &Arc<Mutex<TuiState>>) -> std::result::Result<RunReport, String> {
    let mut report = RunReport::default();
    let children = store.children_of(node).map_err(|e| e.to_string())?;
    let aggregating = node.status == SPLIT_STATUS;
    store.set_status(node, RUNNING).map_err(|e| e.to_string())?;
    {
        let mut s = state.lock().unwrap();
        s.nodes = store.walk().unwrap_or_default();
    }

    for _ in 0..MAX_ATTEMPTS {
        let result = match run_node(&store, node, model, on_output.clone()) {
            Ok(r) => r,
            Err(RunnerError::Other(e)) => {
                store.append_log(node, &serde_json::json!({"event":"error","error":e})).ok();
                continue;
            }
            Err(e) => {
                store.append_log(node, &serde_json::json!({"event":"error","error":e.to_string()})).ok();
                store.append_decision(node, &format!("failed: {e}")).ok();
                store.set_status(node, FAILED).ok();
                report.failed += 1;
                { let mut s = state.lock().unwrap(); s.nodes = store.walk().unwrap_or_default(); }
                return Ok(report);
            }
        };

        match result.verb.as_str() {
            NOTE_GLOBAL => {
                if let Ok(eid) = store.note_global(&result.entry_type, &result.entry_content, &result.entry_supersedes) {
                    store.append_log(node, &serde_json::json!({"event":"note_global","entry_id":eid})).ok();
                }
                continue;
            }
            ESCALATE => {
                report.escalations += 1;
                store.append_log(node, &serde_json::json!({"event":"escalate","assumption":result.assumption,"evidence":result.evidence})).ok();
                return Ok(report);
            }
            SPLIT => {
                if aggregating || node.depth >= MAX_DEPTH && !store.budget_enabled() {
                    report.refused += 1;
                    continue;
                }
                if result.subtasks.is_empty() { report.refused += 1; continue; }
                match store.add_children(node, &result.subtasks) {
                    Ok(children) => {
                        let ids: Vec<&str> = children.iter().map(|c| c.id.as_str()).collect();
                        store.append_decision(node, &format!("split into {}", ids.join(", "))).ok();
                        report.split += 1;
                        { let mut s = state.lock().unwrap(); s.nodes = store.walk().unwrap_or_default(); }
                        return Ok(report);
                    }
                    Err(e) => {
                        report.refused += 1;
                        store.append_log(node, &serde_json::json!({"event":"split_refused","reason":e.to_string()})).ok();
                        continue;
                    }
                }
            }
            COMPLETE_VERB => {
                if children.is_empty() && result.artifacts.iter().all(|(_,c)| c.trim().is_empty()) && result.deliverable.trim().is_empty() {
                    report.refused += 1; continue;
                }
                let criteria = node.contract().acceptance_criteria;
                report.verifications += 1;
                match verify_node(&store, node, &result.deliverable, &criteria, model) {
                    Ok((verdict, _)) if verdict == "PASS" => {
                        store.complete(node, &result.summary, &result.deliverable, &result.artifacts).map_err(|e| e.to_string())?;
                        store.append_decision(node, "verified: verdict=PASS").ok();
                        report.completed += 1;
                        report.node_depths.push(node.depth);
                        { let mut s = state.lock().unwrap(); s.nodes = store.walk().unwrap_or_default(); }
                        return Ok(report);
                    }
                    Ok((_, _)) => { report.verify_failures += 1; report.refused += 1; continue; }
                    Err(e) => {
                        store.append_log(node, &serde_json::json!({"event":"verify_error","error":e.to_string()})).ok();
                        report.refused += 1; continue;
                    }
                }
            }
            _ => { report.refused += 1; continue; }
        }
    }

    store.append_decision(node, "failed: no usable answer after repeated refusals").ok();
    store.set_status(node, FAILED).ok();
    report.failed += 1;
    Ok(report)
}
