use crate::runner::{RunnerError, COMPLETE_VERB, ESCALATE, NOTE_GLOBAL, SPLIT, run_node, verify_node};
use crate::store::{Node, Store, StoreError, COMPLETE, FAILED, PENDING, RUNNING, SPLIT as SPLIT_STATUS};
use std::collections::HashMap;
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
}

pub type StatusFn = Box<dyn Fn(&RunReport, &Vec<Node>)>;

pub fn run(store: &Store, on_status: Option<&StatusFn>) -> std::result::Result<RunReport, StoreError> {
    store.reconcile()?;
    let mut report = RunReport::default();
    let limit = std::env::var("FRACTAL_MAX_STEPS").ok().and_then(|s| s.parse().ok()).unwrap_or(MAX_STEPS);

    // Render immediately so the user sees the tree before any work starts.
    if let Some(ref f) = on_status { f(&report, &store.walk().unwrap_or_default()); }

    while report.steps < limit {
        if INTERRUPTED.load(Ordering::SeqCst) {
            break;
        }
        let nodes = store.walk()?;
        let next = next_node(&nodes);
        match next {
            Some(node) => {
                store.set_status(&node, RUNNING)?;
                if let Some(ref f) = on_status { f(&report, &store.walk().unwrap_or_default()); }
                report.steps += 1;
                execute(store, &node, &mut report)?;
                if let Some(ref f) = on_status { f(&report, &store.walk().unwrap_or_default()); }
            }
            None => break,
        }
    }

    let nodes = store.walk()?;
    report.root_status = nodes.first().map(|n| n.status.clone()).unwrap_or_else(|| PENDING.into());
    Ok(report)
}

fn next_node(nodes: &[Node]) -> Option<Node> {
    // Deepest pending node with all deps satisfied, then deepest split node with all children terminal
    let by_id: HashMap<&str, &Node> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut best: Option<(i64, &Node)> = None;
    for n in nodes {
        if n.status == PENDING && deps_satisfied(n, &by_id) {
            if best.map_or(true, |(d,_)| n.depth > d) { best = Some((n.depth, n)); }
        }
    }
    if best.is_some() { return best.map(|(_, n)| n.clone()); }

    // Aggregatable: split nodes whose children are all terminal
    let mut best_agg: Option<(i64, &Node)> = None;
    for n in nodes {
        if n.status == SPLIT_STATUS && aggregatable(n, &by_id, nodes) {
            if best_agg.map_or(true, |(d,_)| n.depth > d) { best_agg = Some((n.depth, n)); }
        }
    }
    if let Some((_, n)) = best_agg { return Some(n.clone()); }

    None
}

fn deps_satisfied(node: &Node, by_id: &HashMap<&str, &Node>) -> bool {
    node.depends_on.iter().all(|dep| by_id.get(dep.as_str()).map_or(false, |n| n.status == COMPLETE))
}

fn aggregatable(node: &Node, by_id: &HashMap<&str, &Node>, nodes: &[Node]) -> bool {
    let children: Vec<&Node> = nodes.iter().filter(|n| n.parent.as_deref() == Some(&node.id)).collect();
    !children.is_empty() && children.iter().all(|c| [COMPLETE, FAILED].contains(&c.status.as_str()))
}

fn execute(store: &Store, node: &Node, report: &mut RunReport) -> std::result::Result<(), StoreError> {
    let children = store.children_of(node)?;
    let aggregating = node.status == SPLIT_STATUS;
    store.set_status(node, RUNNING)?;

    for _ in 0..MAX_ATTEMPTS {
        let pre = if store.budget_enabled() { store.budget_remaining(&node.id).ok() } else { None };
        let result = match run_node(store, node) {
            Ok(r) => r,
            Err(RunnerError::Other(e)) => {
                store.append_log(node, &serde_json::json!({"event":"error","error":e}))?;
                continue;
            }
            Err(e) => {
                store.append_log(node, &serde_json::json!({"event":"error","error":e.to_string()}))?;
                store.append_decision(node, &format!("failed: {e}"))?;
                store.set_status(node, FAILED)?;
                report.failed += 1;
                return Ok(());
            }
        };

        let post = if store.budget_enabled() { store.budget_remaining(&node.id).ok() } else { None };
        let tokens = match (pre, post) { (Some(a), Some(b)) => a - b, _ => 0 };

        match result.verb.as_str() {
            NOTE_GLOBAL => {
                let eid = store.note_global(&result.entry_type, &result.entry_content, &result.entry_supersedes)?;
                store.append_log(node, &serde_json::json!({"event":"note_global","entry_id":eid}))?;
                continue;
            }
            ESCALATE => {
                report.escalations += 1;
                store.append_log(node, &serde_json::json!({"event":"escalate","assumption":result.assumption,"evidence":result.evidence}))?;
                return Ok(()); // Simplified: escalate -> mark failed for now
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
                        store.append_decision(node, &format!("split into {}", ids.join(", ")))?;
                        report.split += 1;
                        return Ok(());
                    }
                    Err(e) => { report.refused += 1; store.append_log(node, &serde_json::json!({"event":"split_refused","reason":e.to_string()}))?; continue; }
                }
            }
            COMPLETE_VERB => {
                if children.is_empty() && result.artifacts.iter().all(|(_,c)| c.trim().is_empty()) && result.deliverable.trim().is_empty() {
                    report.refused += 1; continue;
                }
                let criteria = node.contract().acceptance_criteria;
                report.verifications += 1;
                match verify_node(store, node, &result.deliverable, &criteria) {
                    Ok((verdict, _)) if verdict == "PASS" => {
                        store.complete(node, &result.summary, &result.deliverable, &result.artifacts)?;
                        store.append_decision(node, "verified: verdict=PASS")?;
                        report.completed += 1;
                        report.node_depths.push(node.depth);
                        return Ok(());
                    }
                    Ok((_, _)) => { report.verify_failures += 1; report.refused += 1; continue; }
                    Err(e) => { store.append_log(node, &serde_json::json!({"event":"verify_error","error":e.to_string()}))?; report.refused += 1; continue; }
                }
            }
            _ => { report.refused += 1; continue; }
        }
    }

    store.append_decision(node, "failed: no usable answer after repeated refusals")?;
    store.set_status(node, FAILED)?;
    report.failed += 1;
    Ok(())
}
