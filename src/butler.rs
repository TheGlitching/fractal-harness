//! The butler: the one steering agent the user talks to.
//!
//! The butler lives *outside* the task tree. It is not a node and never gets a
//! node id, a contract or a status; the root and every node stay exactly as they
//! are. It is the architect/maintainer/mate: it inspects the tree through tools,
//! decides how a correction should be satisfied, and applies it through the same
//! `Store` operations the TUI, dashboard and scheduler use - never a parallel
//! state path.
//!
//! The correction policy is the point: a new constraint is not a reason to
//! replay the whole tree. The butler first asks whether the tree already
//! satisfies it (then it does nothing and says why), then reopens only the nodes
//! that must change, then adds nodes for genuinely new work, then retries.

use crate::store::{Node, Store, StoreError};
use serde_json::{json, Value};
use std::path::PathBuf;

const BUTLER_DIRNAME: &str = "butler";
const PLAN_FILENAME: &str = "last-plan.json";
const LOG_FILENAME: &str = "log.jsonl";
const CONVERSATION_FILENAME: &str = "conversation.json";
const MAX_DIFF_CHARS: usize = 8_000;
const MAX_REPLY_CHARS: usize = 8_000;
/// How many turns the durable conversation keeps. It is a display log, not the
/// audit trail (`log.jsonl` is); older turns roll off so a long session cannot
/// grow without bound.
const MAX_TURNS: usize = 50;

/// Serialises `begin_turn`/`update_turn` within one process so two HTTP requests
/// cannot both read a conversation and write it back over each other. The
/// conversation file itself is the cross-process guard: a `working` turn is
/// visible to any other butler entrypoint.
static TURN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn now() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S+00:00")
        .to_string()
}

fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n[truncated by the butler at {max} chars]", &s[..cut])
}

fn field<'a>(request: &'a Value, key: &str) -> &'a str {
    request
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
}

fn required<'a>(request: &'a Value, key: &str) -> Result<&'a str, StoreError> {
    let value = field(request, key);
    if value.is_empty() {
        return Err(StoreError::Other(format!("tool needs a '{key}'")));
    }
    Ok(value)
}

fn strings(request: &Value, key: &str) -> Vec<String> {
    request
        .get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Where the butler's own durable state lives: its last plan and its session
/// log. Deliberately under `.fractal/` (harness state, excluded from the user's
/// repository), not `tree/` - the butler is not part of the task tree.
pub fn butler_dir(store: &Store) -> PathBuf {
    store.state_dir.join(BUTLER_DIRNAME)
}

fn plan_path(store: &Store) -> PathBuf {
    butler_dir(store).join(PLAN_FILENAME)
}

fn log_path(store: &Store) -> PathBuf {
    butler_dir(store).join(LOG_FILENAME)
}

fn append_log(store: &Store, record: &Value) {
    use std::io::Write;
    let path = log_path(store);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(format!("{record}\n").as_bytes()));
}

/// The plan the butler recorded for its current session, if any.
pub fn read_plan(store: &Store) -> Option<Value> {
    let text = std::fs::read_to_string(plan_path(store)).ok()?;
    serde_json::from_str(&text).ok()
}

fn conversation_path(store: &Store) -> PathBuf {
    butler_dir(store).join(CONVERSATION_FILENAME)
}

/// The durable conversation: one turn per user message, each carrying the
/// butler's reply, plan and the tree changes it caused. Read by the dashboard
/// chat panel, so it survives a page refresh and a server restart.
pub fn read_conversation(store: &Store) -> Vec<Value> {
    std::fs::read_to_string(conversation_path(store))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

fn write_conversation(store: &Store, turns: &[Value]) -> Result<(), StoreError> {
    let path = conversation_path(store);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&Value::Array(turns.to_vec()))?,
    )?;
    Ok(())
}

fn is_working(turn: &Value) -> bool {
    turn.get("status").and_then(Value::as_str) == Some("working")
}

/// Claim the next turn and persist it as `working` before any agent runs, so a
/// refresh (or another entrypoint) sees the request immediately. Refuses while a
/// previous turn is still working: the tree has one steering agent at a time.
/// `node` is the step the user had selected in the graph, if any; it is stored
/// with the turn and handed to the butler so "this step" is unambiguous.
pub fn begin_turn(store: &Store, message: &str, node: Option<&str>) -> Result<String, StoreError> {
    let _guard = TURN_LOCK.lock().unwrap();
    let mut turns = read_conversation(store);
    if turns.last().map(is_working).unwrap_or(false) {
        return Err(StoreError::Other(
            "a butler request is already running".into(),
        ));
    }
    // Unique even after old turns roll off the front of the history.
    let id = format!(
        "turn-{}-{}",
        turns.len() + 1,
        chrono::Utc::now().timestamp_millis()
    );
    let mut turn = json!({
        "id": id,
        "at": now(),
        "status": "working",
        "message": message,
    });
    if let Some(node) = node {
        turn["node"] = json!(node);
    }
    turns.push(turn);
    if turns.len() > MAX_TURNS {
        let drop = turns.len() - MAX_TURNS;
        turns.drain(0..drop);
    }
    write_conversation(store, &turns)?;
    Ok(id)
}

fn update_turn(store: &Store, turn_id: &str, patch: Value) -> Result<(), StoreError> {
    let _guard = TURN_LOCK.lock().unwrap();
    let mut turns = read_conversation(store);
    let Some(turn) = turns.iter_mut().find(|t| t["id"] == turn_id) else {
        return Ok(());
    };
    if let (Some(obj), Some(fields)) = (turn.as_object_mut(), patch.as_object()) {
        for (key, value) in fields {
            obj.insert(key.clone(), value.clone());
        }
    }
    write_conversation(store, &turns)
}

/// Mark any turn left `working` by a dead process as failed, so a restart does
/// not show a phantom run in progress or block the next request forever.
pub fn recover_interrupted(store: &Store) {
    let _guard = TURN_LOCK.lock().unwrap();
    let mut turns = read_conversation(store);
    let mut changed = false;
    for turn in turns.iter_mut() {
        if is_working(turn) {
            if let Some(obj) = turn.as_object_mut() {
                obj.insert("status".into(), json!("failed"));
                obj.insert(
                    "error".into(),
                    json!("the butler was interrupted before it finished this request"),
                );
            }
            changed = true;
        }
    }
    if changed {
        let _ = write_conversation(store, &turns);
    }
}

fn node_summary(node: &Node) -> Value {
    let deliverables: Vec<String> = node
        .find_artifacts()
        .iter()
        .filter_map(|p| p.strip_prefix(node.artifacts_dir()).ok())
        .map(|p| p.display().to_string())
        .collect();
    json!({
        "id": node.id,
        "parent": node.parent,
        "depth": node.depth,
        "status": node.status,
        "goal": node.goal,
        "summary": node.summary,
        "depends_on": node.depends_on,
        "deliverables": deliverables,
    })
}

fn node_detail(store: &Store, node: &Node) -> Value {
    let contract = node.contract();
    let decisions: Vec<String> = std::fs::read_to_string(node.decisions_path())
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("- "))
        .map(|l| l.to_string())
        .collect();
    let events: Vec<Value> = std::fs::read_to_string(node.log_path())
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    let tail: Vec<Value> = events.iter().rev().take(20).cloned().collect();
    let diff = crate::git::node_diff(&store.root, &node.id, false).unwrap_or_default();
    json!({
        "id": node.id,
        "parent": node.parent,
        "depth": node.depth,
        "status": node.status,
        "goal": node.goal,
        "summary": node.summary,
        "acceptance_criteria": contract.acceptance_criteria,
        "interfaces": contract.interfaces,
        "constraints": contract.constraints,
        "verification": contract.verification,
        "manual_verification": contract.manual_verification,
        "depends_on": node.depends_on,
        "decisions": decisions,
        "events": tail,
        "diff": cap_chars(&diff, MAX_DIFF_CHARS),
        "activity": store.read_activity(&node.id),
    })
}

fn runnable_ids(store: &Store) -> Result<Vec<String>, StoreError> {
    let nodes = store.walk()?;
    let stale = store.stale_ids().unwrap_or_default();
    Ok(crate::scheduler::next_nodes(&nodes, &stale)
        .into_iter()
        .map(|n| n.id)
        .collect())
}

fn status_changes(before: &[Node], after: &[Node]) -> Vec<Value> {
    use std::collections::HashMap;
    let old: HashMap<&str, &str> = before
        .iter()
        .map(|n| (n.id.as_str(), n.status.as_str()))
        .collect();
    let mut changes = Vec::new();
    for n in after {
        match old.get(n.id.as_str()) {
            Some(prev) if *prev != n.status => {
                changes.push(json!({"id": n.id, "from": prev, "to": n.status}));
            }
            None => changes.push(json!({"id": n.id, "from": "absent", "to": n.status})),
            _ => {}
        }
    }
    changes
}

/// Record the butler's plan and rationale for this session. Persisted outside
/// the tree so `fractal ask` and the dashboard can surface what was decided and
/// why, and so the decision is auditable after the fact.
fn record_plan(store: &Store, request: &Value) -> Result<Value, StoreError> {
    let plan = json!({
        "at": now(),
        "action": field(request, "action"),
        "rationale": field(request, "rationale"),
        "nodes": strings(request, "nodes"),
    });
    let path = plan_path(store);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&plan)?)?;
    Ok(json!({"recorded": true, "plan": plan}))
}

/// Run the project's own verification gates against the live tree.
fn verify_project(store: &Store) -> Value {
    let gates = crate::verify::detect_gates(&store.root);
    if gates.is_empty() {
        return json!({"gates": [], "passed": true, "note": "no verification gates detected"});
    }
    let outcomes = crate::verify::run_gates(&store.root, &gates, 900);
    let list: Vec<Value> = outcomes
        .iter()
        .map(|o| {
            json!({
                "command": o.command,
                "passed": o.passed,
                "manual": o.manual,
            })
        })
        .collect();
    let passed = outcomes.iter().all(|o| o.passed || o.manual);
    json!({"gates": list, "passed": passed})
}

/// Resume the scheduler over the current tree. Only nodes that are runnable
/// (pending, or split with every child accepted, or stale) ever run; a plain
/// resume therefore never replays a completed tree.
pub fn resume(store: &Store, model: &str) -> Result<Value, String> {
    use crate::tui::{StatsSnapshot, TuiMode, TuiState};
    use std::sync::{Arc, Mutex};
    let state = Arc::new(Mutex::new(TuiState {
        nodes: vec![],
        stats: StatsSnapshot {
            steps: 0,
            completed: 0,
            split: 0,
            failed: 0,
            refused: 0,
            refused_goals: vec![],
            failed_goals: vec![],
        },
        log_lines: vec![],
        status_line: String::new(),
        node_id: "root".into(),
        node_goal: String::new(),
        node_started_at: std::time::Instant::now(),
        last_activity: String::new(),
        node_activities: std::collections::HashMap::new(),
        done: false,
        error: None,
        model: model.to_string(),
        selected_idx: 0,
        mode: TuiMode::Normal,
        prompt_message: None,
        inspect_scroll: 0,
    }));
    let report = crate::scheduler::run(store, &state, model, false).map_err(|e| e.to_string())?;
    Ok(json!({
        "root_status": report.root_status,
        "completed": report.completed,
        "failed": report.failed,
        "steps": report.steps,
    }))
}

/// One butler tool invocation. Every mutation routes through `Store` (or the
/// scheduler) so the TUI, dashboard, scheduler and butler share one state path.
pub fn tool(store: &Store, request: &Value, model: &str) -> Result<Value, StoreError> {
    match field(request, "tool") {
        "tree" => {
            let nodes = store.walk()?;
            Ok(json!({"nodes": nodes.iter().map(node_summary).collect::<Vec<_>>()}))
        }
        "node" => {
            let node = store.get(required(request, "id")?)?;
            Ok(node_detail(store, &node))
        }
        "digest" => Ok(json!({"digest": store.generate_digest()?})),
        "trace" => {
            let text = std::fs::read_to_string(store.root.join("trace.json")).unwrap_or_default();
            Ok(json!({"trace": serde_json::from_str::<Value>(&text).unwrap_or(Value::Null)}))
        }
        "next" => Ok(json!({"runnable": runnable_ids(store)?})),
        "verify" => Ok(verify_project(store)),
        "constraint" => {
            let node = required(request, "node")?;
            let text = required(request, "text")?;
            let affected = store.add_constraint_and_propagate(node, text)?;
            Ok(json!({"affected": affected, "constraint": text}))
        }
        "amend" => {
            let node = store.get(required(request, "node")?)?;
            let old = required(request, "old")?;
            let new = required(request, "new")?;
            let changed = store.amend_inherited_constraint(&node, old, new)?;
            Ok(json!({"changed": changed}))
        }
        "edit_contract" => {
            let node = store.get(required(request, "node")?)?;
            let mut contract = node.contract();
            if request.get("goal").is_some() {
                contract.goal = field(request, "goal").to_string();
            }
            if request.get("acceptance_criteria").is_some() {
                contract.acceptance_criteria = strings(request, "acceptance_criteria");
            }
            if request.get("verification").is_some() {
                contract.verification = strings(request, "verification");
            }
            if request.get("manual_verification").is_some() {
                contract.manual_verification = strings(request, "manual_verification");
            }
            if request.get("interfaces").is_some() {
                contract.interfaces = strings(request, "interfaces");
            }
            if contract.goal.is_empty() {
                return Err(StoreError::Other("contract goal must not be empty".into()));
            }
            store.write_contract(&node, &contract)?;
            store.append_decision(&node, "contract edited by the butler")?;
            Ok(json!({"edited": node.id}))
        }
        "reopen" => {
            let parent = store.get(required(request, "parent")?)?;
            let children = strings(request, "children");
            if children.is_empty() {
                return Err(StoreError::Other("reopen needs 'children'".into()));
            }
            let reason = required(request, "reason")?;
            let reopened = store.reopen_children(&parent, &children, reason)?;
            if reopened.is_empty() {
                return Err(StoreError::Other(format!(
                    "none of {} are direct children of {}",
                    children.join(", "),
                    parent.id
                )));
            }
            Ok(json!({"reopened": reopened}))
        }
        "split" => {
            let parent = store.get(required(request, "parent")?)?;
            let subtasks = request
                .get("subtasks")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| StoreError::Other("split needs a 'subtasks' array".into()))?;
            if subtasks.is_empty() {
                return Err(StoreError::Other("split needs at least one subtask".into()));
            }
            let contracts = crate::runner::contracts_from_subtasks(&subtasks);
            let existing: Vec<String> = store
                .children_of(&parent)?
                .iter()
                .map(|c| c.id.clone())
                .collect();
            if let Some(reason) = crate::scheduler::reject_split_topology(&contracts, &existing) {
                return Err(StoreError::Other(format!("split refused: {reason}")));
            }
            let children = store.add_children(&parent, &contracts)?;
            Ok(json!({
                "added": children.iter().map(|c| c.id.clone()).collect::<Vec<_>>()
            }))
        }
        "retry" => {
            let node = required(request, "node")?;
            let count = store.retry(node)?;
            Ok(json!({"reset": count}))
        }
        "resolve" => {
            let node = store.get(required(request, "node")?)?;
            match field(request, "resolution") {
                "amend" => {
                    let old = required(request, "old")?;
                    let new = required(request, "new")?;
                    let changed = store.amend_inherited_constraint(&node, old, new)?;
                    Ok(json!({"amended": changed}))
                }
                "depends_on" => {
                    let dep = required(request, "dependency")?;
                    let added = store.add_depends_on(&node, dep)?;
                    Ok(json!({"added": added}))
                }
                "interface" => {
                    let iface = required(request, "interface")?;
                    let added = store.add_interface(&node, iface)?;
                    Ok(json!({"added": added}))
                }
                other => Err(StoreError::Other(format!(
                    "unknown resolution {other:?}; use amend, depends_on or interface"
                ))),
            }
        }
        "plan" => record_plan(store, request),
        "resume" => resume(store, model).map_err(StoreError::Other),
        other => Err(StoreError::Other(format!(
            "unknown butler tool {other:?}; use tree, node, digest, trace, next, verify, \
             constraint, amend, edit_contract, reopen, split, retry, resolve, resume or plan"
        ))),
    }
}

/// The butler's system prompt: its role, its correction policy, and the exact
/// tool syntax. Kept as one constant so it is reviewable in one place.
const ROLE_PROMPT: &str = r#"You are the Butler: the maintainer, architect and mate of a fractal task tree. You live OUTSIDE the tree. You are not a node: you have no contract, no status and no place in `tree/`. The root and every node are ordinary nodes, identical to one another; do not invent a special architect role for any of them. You are the one agent the user talks to.

Your job: when the user asks for a change or a correction, inspect the tree, decide the SMALLEST correct response, apply it, and record why. The tree is durable memory; a correction must not replay work that is still valid.

Correction policy - choose exactly one and say why:
- none: the tree already satisfies the request. Change nothing. Say which nodes already satisfy it.
- reopen: one or more existing nodes are wrong or incomplete. Reopen ONLY those nodes (and their subtrees) with a precise reason, so only they re-run.
- split: the request needs genuinely new work no node owns. Add child nodes to the right parent.
- amend: an inherited constraint is now false. Rewrite it where it is owned.
- edit_contract: a node's goal, criteria or gates must change.
- retry: reset a failed node/subtree to pending.
NEVER retry or reset the whole tree, and never reopen a node whose work is still correct. Prefer the fewest nodes.

Tools - run these from your shell (single-quoted JSON):
  "$FRACTAL_BIN" butler-tool '{"tool":"tree"}'
  "$FRACTAL_BIN" butler-tool '{"tool":"node","id":"root-01"}'
  "$FRACTAL_BIN" butler-tool '{"tool":"digest"}'
  "$FRACTAL_BIN" butler-tool '{"tool":"next"}'
  "$FRACTAL_BIN" butler-tool '{"tool":"verify"}'
  "$FRACTAL_BIN" butler-tool '{"tool":"constraint","node":"root","text":"..."}'
  "$FRACTAL_BIN" butler-tool '{"tool":"amend","node":"root","old":"...","new":"..."}'
  "$FRACTAL_BIN" butler-tool '{"tool":"edit_contract","node":"root-01","goal":"...","acceptance_criteria":["..."]}'
  "$FRACTAL_BIN" butler-tool '{"tool":"reopen","parent":"root","children":["root-01"],"reason":"..."}'
  "$FRACTAL_BIN" butler-tool '{"tool":"split","parent":"root","subtasks":[{"id":"a","goal":"...","acceptance_criteria":["..."]}]}'
  "$FRACTAL_BIN" butler-tool '{"tool":"retry","node":"root-01"}'
  "$FRACTAL_BIN" butler-tool '{"tool":"resolve","node":"root","resolution":"amend","old":"...","new":"..."}'
  "$FRACTAL_BIN" butler-tool '{"tool":"resume"}'

Finish by recording your decision (this is required, and it is how the user learns what you did and why):
  "$FRACTAL_BIN" butler-tool '{"tool":"plan","action":"none|reopen|split|amend|edit_contract|retry|resolve","rationale":"...","nodes":["..."]}'

Then print a short plain-language summary for the user."#;

fn build_prompt(store: &Store, message: &str, node: Option<&str>) -> Result<String, StoreError> {
    let nodes = store.walk()?;
    let mut tree = String::new();
    for n in &nodes {
        let indent = "  ".repeat((n.depth - 1) as usize);
        let goal = n.goal.lines().next().unwrap_or(&n.goal);
        let deps = if n.depends_on.is_empty() {
            String::new()
        } else {
            format!("  depends_on: {}", n.depends_on.join(", "))
        };
        tree.push_str(&format!("{indent}{} [{}] {}{deps}\n", n.id, n.status, goal));
    }
    let focus = match node {
        Some(id) => {
            let selected = store.get(id)?;
            let goal = selected.goal.lines().next().unwrap_or(&selected.goal);
            format!(
                "\n## Selected step\nThe user is looking at `{id}` [{}] {goal}\n\
                 When the request says \"this\", \"it\" or \"the step\", it means `{id}`.\n",
                selected.status
            )
        }
        None => String::new(),
    };
    Ok(format!(
        "{ROLE_PROMPT}\n\n## Current tree\n{tree}{focus}\n## User request\n{message}\n\n\
         Inspect what you need with the tools, then apply the smallest correct \
         change and finish with a `plan` tool call."
    ))
}

/// Run one butler session: build the prompt, run the executor agent (which
/// inspects and steers through the tools), then surface the plan it recorded and
/// the tree changes it produced. Recorded to `.fractal/butler/log.jsonl` (the
/// audit trail) and to the durable conversation the dashboard chat panel reads.
pub fn run(store: &Store, message: &str, model: &str) -> Result<Value, String> {
    let turn_id = begin_turn(store, message, None).map_err(|e| e.to_string())?;
    run_turn(store, &turn_id, message, None, model)
}

/// Run the already-claimed turn `turn_id` and settle it as done or failed. Split
/// from `run` so the dashboard can claim a turn synchronously, return to the
/// browser, and run the agent in the background. `node` is the graph selection
/// that seeds the session's context.
pub fn run_turn(
    store: &Store,
    turn_id: &str,
    message: &str,
    node: Option<&str>,
    model: &str,
) -> Result<Value, String> {
    let outcome = run_session(store, message, node, model);
    match &outcome {
        Ok(record) => {
            let _ = update_turn(
                store,
                turn_id,
                json!({
                    "status": "done",
                    "reply": record.get("reply").cloned().unwrap_or(Value::Null),
                    "plan": record.get("plan").cloned().unwrap_or(Value::Null),
                    "tree_changes": record.get("tree_changes").cloned().unwrap_or(Value::Null),
                }),
            );
        }
        Err(error) => {
            let _ = update_turn(store, turn_id, json!({"status": "failed", "error": error}));
        }
    }
    outcome
}

fn run_session(
    store: &Store,
    message: &str,
    node: Option<&str>,
    model: &str,
) -> Result<Value, String> {
    store.reconcile().map_err(|e| e.to_string())?;
    // A new session must not inherit a previous session's plan.
    let _ = std::fs::remove_file(plan_path(store));

    let before = store.walk().map_err(|e| e.to_string())?;
    let prompt = build_prompt(store, message, node).map_err(|e| e.to_string())?;
    let on_output: crate::runner::OutputFn = std::sync::Arc::new(|line: &str| eprintln!("{line}"));

    let reply =
        crate::runner::run_butler_agent(&prompt, &butler_dir(store), &store.root, model, on_output)
            .map_err(|e| e.to_string())?;

    let plan = read_plan(store).unwrap_or_else(|| {
        json!({
            "action": "none",
            "rationale": "the butler recorded no plan for this request",
            "nodes": [],
        })
    });
    let after = store.walk().map_err(|e| e.to_string())?;
    let record = json!({
        "at": now(),
        "message": message,
        "reply": cap_chars(&reply, MAX_REPLY_CHARS),
        "plan": plan,
        "tree_changes": status_changes(&before, &after),
    });
    append_log(store, &record);
    Ok(record)
}

/// Convenience wrapper: apply a raw tool request and print the JSON result.
/// Used by the CLI so the tools are usable by a human as well as the agent.
pub fn describe_action(plan: &Value) -> String {
    let action = plan.get("action").and_then(Value::as_str).unwrap_or("none");
    let rationale = plan.get("rationale").and_then(Value::as_str).unwrap_or("");
    match action {
        "none" => format!("no change — {rationale}"),
        other => format!("{other} — {rationale}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Contract, COMPLETE, PENDING, SPLIT};

    fn temp_store(name: &str) -> (Store, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("fractal_butler_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (Store::new(&dir), dir)
    }

    fn tree(name: &str, child_count: usize) -> (Store, PathBuf, Node, Vec<Node>) {
        let (store, dir) = temp_store(name);
        let root = store.init("build a thing").unwrap();
        let contracts: Vec<Contract> = (0..child_count)
            .map(|i| Contract {
                goal: format!("child {i}"),
                id: format!("c{i}"),
                ..Default::default()
            })
            .collect();
        let children = store.add_children(&root, &contracts).unwrap();
        (store, dir, root, children)
    }

    #[test]
    fn tree_lists_goals_statuses_dependencies_and_deliverables() {
        let (store, dir, root, children) = tree("list", 2);
        store
            .complete(
                &children[0],
                "child done",
                "delivered",
                &[("out.txt".into(), "hello".into())],
                &dir,
            )
            .unwrap();
        let value = tool(&store, &json!({"tool":"tree"}), "").unwrap();
        let nodes = value.get("nodes").unwrap().as_array().unwrap();
        let root_json = nodes.iter().find(|n| n["id"] == "root").unwrap();
        assert_eq!(root_json["status"], SPLIT);
        assert_eq!(root_json["goal"], "build a thing");
        let first = nodes.iter().find(|n| n["id"] == children[0].id).unwrap();
        assert_eq!(first["status"], COMPLETE);
        assert_eq!(first["deliverables"][0], "out.txt");
        let _ = root;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_none_plan_causes_no_rework_and_is_recorded() {
        let (store, dir, root, children) = tree("none", 2);
        for c in &children {
            store.set_status(c, COMPLETE).unwrap();
        }
        let before: Vec<(String, String)> = store
            .walk()
            .unwrap()
            .into_iter()
            .map(|n| (n.id, n.status))
            .collect();

        let outcome = tool(
            &store,
            &json!({
                "tool": "plan",
                "action": "none",
                "rationale": "the tree already satisfies this; nodes root-01 and root-02 cover it",
                "nodes": []
            }),
            "",
        )
        .unwrap();
        assert_eq!(outcome["recorded"], true);

        let after: Vec<(String, String)> = store
            .walk()
            .unwrap()
            .into_iter()
            .map(|n| (n.id, n.status))
            .collect();
        assert_eq!(before, after, "a 'none' decision must not touch any node");
        assert_eq!(
            read_plan(&store).unwrap()["action"],
            "none",
            "the plan must be recorded so the decision is auditable"
        );
        let _ = root;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reopen_touches_only_the_named_children() {
        let (store, dir, root, children) = tree("reopen", 3);
        for c in &children {
            store.set_status(c, COMPLETE).unwrap();
        }
        let outcome = tool(
            &store,
            &json!({
                "tool": "reopen",
                "parent": "root",
                "children": [children[1].id.clone()],
                "reason": "the wiring is broken"
            }),
            "",
        )
        .unwrap();
        assert_eq!(outcome["reopened"], json!([children[1].id.clone()]));

        let after = store.walk().unwrap();
        let status = |id: &str| after.iter().find(|n| n.id == id).unwrap().status.clone();
        assert_eq!(
            status(&children[0].id),
            COMPLETE,
            "sibling 0 must be untouched"
        );
        assert_eq!(
            status(&children[2].id),
            COMPLETE,
            "sibling 2 must be untouched"
        );
        assert_eq!(
            status(&children[1].id),
            PENDING,
            "only the named child re-runs"
        );
        let _ = root;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reopen_refuses_a_node_outside_the_parent() {
        let (store, dir, root, children) = tree("reopen_scope", 1);
        let grandchild = store
            .add_children(
                &children[0],
                &[Contract {
                    goal: "deep".into(),
                    id: "deep".into(),
                    ..Default::default()
                }],
            )
            .unwrap()
            .remove(0);
        let err = tool(
            &store,
            &json!({
                "tool": "reopen",
                "parent": "root",
                "children": [grandchild.id.clone()],
                "reason": "nope"
            }),
            "",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("direct children"), "unexpected error: {err}");
        let _ = root;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_adds_new_children_for_genuinely_new_work() {
        let (store, dir, root, children) = tree("split", 1);
        assert_eq!(children.len(), 1);
        let outcome = tool(
            &store,
            &json!({
                "tool": "split",
                "parent": "root",
                "subtasks": [
                    {"id": "new", "goal": "add the missing capability",
                     "acceptance_criteria": ["it works"],
                     "depends_on": [children[0].id.clone()]}
                ]
            }),
            "",
        )
        .unwrap();
        let added = outcome["added"].as_array().unwrap();
        assert_eq!(added.len(), 1);
        let after = store.walk().unwrap();
        let new_id = added[0].as_str().unwrap();
        let new_node = after.iter().find(|n| n.id == new_id).unwrap();
        assert_eq!(new_node.status, PENDING);
        assert_eq!(new_node.goal, "add the missing capability");
        assert_eq!(new_node.depends_on, vec![children[0].id.clone()]);
        let _ = root;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_rejects_a_dependency_on_an_unknown_sibling() {
        let (store, dir, root, _children) = tree("split_bad", 1);
        let err = tool(
            &store,
            &json!({
                "tool": "split",
                "parent": "root",
                "subtasks": [{"id": "new", "goal": "x", "depends_on": ["ghost"]}]
            }),
            "",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown sibling"), "unexpected error: {err}");
        let _ = root;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn edit_contract_updates_the_goal_and_keeps_the_root_ordinary() {
        let (store, dir, _root, children) = tree("edit", 1);
        let before_root_status = store
            .walk()
            .unwrap()
            .iter()
            .find(|n| n.id == "root")
            .unwrap()
            .status
            .clone();
        tool(
            &store,
            &json!({
                "tool": "edit_contract",
                "node": children[0].id.clone(),
                "goal": "the corrected goal",
                "acceptance_criteria": ["new criterion"]
            }),
            "",
        )
        .unwrap();
        let after = store.walk().unwrap();
        let edited = after.iter().find(|n| n.id == children[0].id).unwrap();
        assert_eq!(edited.goal, "the corrected goal");
        assert_eq!(edited.contract().acceptance_criteria, vec!["new criterion"]);
        assert_eq!(
            after.iter().find(|n| n.id == "root").unwrap().status,
            before_root_status,
            "editing a child must not change the root's own status"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_reports_the_project_gates() {
        let (store, dir) = temp_store("verify");
        store.init("build a thing").unwrap();
        let result = tool(&store, &json!({"tool":"verify"}), "").unwrap();
        assert!(
            result.get("passed").is_some(),
            "verify must report a pass/fail result: {result}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn describe_action_reads_the_plan() {
        let plan = json!({"action":"reopen","rationale":"only root-01 changed"});
        assert_eq!(describe_action(&plan), "reopen — only root-01 changed");
        let none = json!({"action":"none","rationale":"already satisfied"});
        assert_eq!(describe_action(&none), "no change — already satisfied");
    }

    #[test]
    fn begin_turn_persists_the_message_and_refuses_a_second_run() {
        let (store, dir) = temp_store("begin_turn");
        let id = begin_turn(&store, "make the tracker use real data", Some("root-01")).unwrap();
        let turns = read_conversation(&store);
        assert_eq!(turns.len(), 1, "the request must be recorded immediately");
        assert_eq!(turns[0]["id"], id);
        assert_eq!(turns[0]["status"], "working");
        assert_eq!(turns[0]["message"], "make the tracker use real data");
        assert_eq!(
            turns[0]["node"], "root-01",
            "the graph selection must ride with the turn"
        );

        let second = begin_turn(&store, "another request", None);
        assert!(
            second.is_err(),
            "a working turn must block a second concurrent request"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_settled_turn_carries_the_plan_and_the_reply() {
        let (store, dir) = temp_store("settle_turn");
        let id = begin_turn(&store, "fix the wiring", None).unwrap();
        update_turn(
            &store,
            &id,
            json!({
                "status": "done",
                "reply": "reopened root-01 only",
                "plan": {"action": "reopen", "rationale": "root-01 is wrong", "nodes": ["root-01"]},
                "tree_changes": [{"id": "root-01", "from": "complete", "to": "pending"}],
            }),
        )
        .unwrap();
        let turns = read_conversation(&store);
        assert_eq!(turns[0]["status"], "done");
        assert_eq!(turns[0]["plan"]["action"], "reopen");
        assert_eq!(turns[0]["tree_changes"][0]["to"], "pending");
        // A settled turn no longer blocks the next request.
        assert!(begin_turn(&store, "a follow-up", None).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recover_interrupted_fails_a_leftover_working_turn() {
        let (store, dir) = temp_store("recover");
        begin_turn(&store, "a request whose process died", None).unwrap();
        recover_interrupted(&store);
        let turns = read_conversation(&store);
        assert_eq!(turns[0]["status"], "failed");
        assert!(
            begin_turn(&store, "the next request", None).is_ok(),
            "recovery must unblock the next request"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
