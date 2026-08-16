use crate::store::{Contract, Node, Store, StoreError};
use regex::Regex;
use serde_json::Value;
use std::fs;
// use std::io::Write removed
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

pub const SPLIT: &str = "split";
pub const COMPLETE_VERB: &str = "complete";
pub const ESCALATE: &str = "escalate";
pub const ESCALATE_RESOLVE: &str = "escalate_resolve";
pub const NOTE_GLOBAL: &str = "note_global";

static DECISION_RE: OnceLock<Regex> = OnceLock::new();

fn decision_re() -> &'static Regex {
    DECISION_RE.get_or_init(|| {
        Regex::new(r#"\{"verb":\s*"(split|complete|escalate|escalate_resolve|note_global)""#).unwrap()
    })
}

#[derive(Debug)]
pub enum RunnerError {
    Timeout,
    NotFound(String),
    NoDecision(String),
    Other(String),
}

impl std::fmt::Display for RunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunnerError::Timeout => write!(f, "executor timed out"),
            RunnerError::NotFound(s) => write!(f, "{s}"),
            RunnerError::NoDecision(s) => write!(f, "no valid decision JSON in output. Last 600 chars:\n{s}"),
            RunnerError::Other(s) => write!(f, "{s}"),
        }
    }
}

#[derive(Debug, Default)]
pub struct VerbResult {
    pub verb: String,
    pub subtasks: Vec<Contract>,
    pub deliverable: String,
    pub summary: String,
    pub artifacts: Vec<(String, String)>,
    pub assumption: String,
    pub evidence: String,
    pub resolution: String,
    pub entry_type: String,
    pub entry_content: String,
    pub entry_supersedes: String,
}

const OP_SYSTEM: &str = "\
You are one node of a fractal task harness. You have been hydrated from a \
node of a persistent task tree and you will dissolve when you answer; the \
tree is the memory, you are not.

You are given a contract: a goal, its acceptance criteria, the interfaces you \
must respect, and the constraints inherited from every ancestor. Those \
constraints are laws — you may not relax them.

You have five possible actions:
* split — the task is larger than one agent can carry. Propose subtasks.
* complete — the task fits within your competence. Produce the deliverable.
* escalate — an inherited constraint is false. Name the assumption + evidence.
* escalate_resolve — settle an escalation: amend | overrule | replan | depends_on.
* note_global — write a lesson, convention, or skill to the global store.

Split only when you must.";

fn bullets(items: &[String]) -> String {
    if items.is_empty() { "- (none)\n".into() } else { items.iter().map(|s| format!("- {s}\n")).collect() }
}

pub fn assemble_context(store: &Store, node: &Node) -> std::result::Result<String, StoreError> {
    let contract_text = fs::read_to_string(node.contract_path()).unwrap_or_default();
    let mut parts = vec![format!("## Your contract\n\n{contract_text}\n")];

    if store.budget_enabled() {
        if let Ok(rem) = store.budget_remaining(&node.id) {
            parts.push(format!("## Budget\n- remaining token allowance: {rem}\n"));
        }
    }

    let global = store.retrieve_global(&node.goal, 5).unwrap_or_default();
    if !global.is_empty() {
        let lines: Vec<String> = global.iter().map(|e| format!("- {}: {}", e.entry_type, e.content)).collect();
        parts.push(format!("## Global knowledge\n\n{}\n\nAdhere to conventions; apply lessons.\n", lines.join("\n")));
    }

    if let Ok(ancestors) = store.ancestors(node) {
        if !ancestors.is_empty() {
            let mut lines = Vec::new();
            for a in &ancestors {
                lines.push(format!("- {} pursues: {}", a.id, a.goal));
                for c in &a.contract().constraints {
                    lines.push(format!("  - constraint: {c}"));
                }
            }
            parts.push(format!("## Inherited from your ancestors\n\n{}\n", lines.join("\n")));
        }
    }

    parts.push(if is_opencode() {
        "## Now answer\n\nYou have the full tools of a coding agent. Execute your \
         contract fully — write files into the current directory. When done, output \
         EXACTLY one JSON decision as the very last thing, no wrapping fences:\n\n\
         SPLIT: {\"verb\":\"split\",\"subtasks\":[{\"id\":\"...\",\"goal\":\"...\",\"acceptance_criteria\":[\"...\"],\"interfaces\":[],\"constraints\":[],\"depends_on\":[]}]}\n\
         COMPLETE: {\"verb\":\"complete\",\"deliverable\":\"...\",\"summary\":\"...\",\"artifacts\":[{\"path\":\"...\",\"content\":\"...\"}]}\n\
         NOTE_GLOBAL: {\"verb\":\"note_global\",\"type\":\"convention|lesson|skill\",\"content\":\"...\"}\n".into()
    } else {
        "## Now answer\n\nUse the split tool or the complete tool. Answer with one tool call and nothing else.\n".into()
    });

    Ok(parts.join("\n"))
}

fn is_opencode() -> bool {
    std::env::var("FRACTAL_EXECUTOR").unwrap_or_else(|_| "opencode".into()) == "opencode"
}

fn extract_decision(text: &str) -> Option<Value> {
    for m in decision_re().find_iter(text) {
        let start = m.start();
        let mut depth = 0;
        let mut end = start;
        for (i, ch) in text[start..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => { depth -= 1; if depth == 0 { end = start + i + 1; break; } }
                _ => {}
            }
        }
        if end > start {
            if let Ok(v) = serde_json::from_str(&text[start..end]) {
                return Some(v);
            }
        }
    }
    None
}

pub fn call_model(prompt: &str, node_path: &Path) -> std::result::Result<Value, RunnerError> {
    if is_opencode() {
        call_via_opencode(prompt, node_path)
    } else {
        Err(RunnerError::Other("anthropic executor not implemented in Rust harness; set FRACTAL_EXECUTOR=opencode".into()))
    }
}

fn call_via_opencode(prompt: &str, node_path: &Path) -> std::result::Result<Value, RunnerError> {
    let claude_md = format!("{OP_SYSTEM}\n\n{prompt}");
    fs::write(node_path.join("CLAUDE.md"), &claude_md).map_err(|e| RunnerError::Other(format!("write CLAUDE.md: {e}")))?;

    let bin = which::which("opencode").unwrap_or_else(|_| std::path::PathBuf::from("opencode"));
    let timeout_secs: u64 = std::env::var("FRACTAL_TIMEOUT").ok().and_then(|s| s.parse().ok()).unwrap_or(600);
    let mut child = Command::new(&bin)
        .args(["run", "--auto"])
        .arg("Read CLAUDE.md. Work ONLY in this directory. Do NOT read or run files from parent directories. Build everything from scratch. When finished, output EXACTLY this JSON as the very last thing with nothing after it — no fences, no summary, no commentary: {\"verb\":\"complete\",\"deliverable\":\"built\",\"summary\":\"done\",\"artifacts\":[{\"path\":\"main.py\",\"content\":\"...\"}]}  If the task is too big, split instead.")
        .current_dir(node_path)
        .env("HOME", node_path)
        .env("OPENCODE_CONFIG_CONTENT", r#"{"permission":{"*":"allow"}}"#)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => RunnerError::NotFound(format!("opencode binary not found: {e}")),
            _ => RunnerError::Other(format!("opencode: {e}")),
        })?;

    let start = std::time::Instant::now();
    let out = loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                break child.wait_with_output()
                    .map_err(|e| RunnerError::Other(format!("opencode output: {e}")))?;
            }
            Ok(None) => {
                if start.elapsed().as_secs() > timeout_secs {
                    let _ = child.kill();
                    return Err(RunnerError::Timeout);
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(RunnerError::Other(format!("opencode: {e}"))),
        }
    };

    let text = format!("{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr));

    let decision = extract_decision(&text).ok_or_else(|| {
        let suffix = if text.len() > 600 { &text[text.len() - 600..] } else { &text };
        RunnerError::NoDecision(suffix.to_string())
    })?;

    Ok(Value::Object({
        let mut m = serde_json::Map::new();
        m.insert("content".into(), Value::Array(vec![Value::Object({
            let mut t = serde_json::Map::new();
            t.insert("type".into(), Value::String("text".into()));
            t.insert("text".into(), Value::String(serde_json::to_string(&decision).unwrap_or_default()));
            t
        })]));
        m.insert("usage".into(), Value::Object({
            let mut u = serde_json::Map::new();
            u.insert("input_tokens".into(), Value::Number(0.into()));
            u.insert("output_tokens".into(), Value::Number(0.into()));
            u
        }));
        m
    }))
}

pub fn call_critic(prompt: &str) -> std::result::Result<Value, RunnerError> {
    if !is_opencode() {
        return Err(RunnerError::Other("critic requires opencode executor".into()));
    }
    let judge_prompt = format!("\
You are a verifier. Judge the deliverable against each acceptance criterion \
and answer with exactly one JSON object: \
{{\"verdict\":\"PASS\" or \"FAIL\",\"criteria\":[{{\"name\":\"...\",\"pass\":true|false,\"reason\":\"...\"}}]}} \
No prose outside the JSON.\n\n{prompt}");

    let bin = which::which("opencode").unwrap_or_else(|_| std::path::PathBuf::from("opencode"));
    let output = Command::new(&bin)
        .args(["run", "--auto"])
        .arg(&judge_prompt)
        .env("OPENCODE_CONFIG_CONTENT", r#"{"permission":{"*":"allow"}}"#)
        .output()
        .map_err(|e| RunnerError::Other(format!("critic opencode: {e}")))?;

    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    Ok(Value::Object({
        let mut m = serde_json::Map::new();
        m.insert("content".into(), Value::Array(vec![Value::Object({
            let mut t = serde_json::Map::new();
            t.insert("type".into(), Value::String("text".into()));
            t.insert("text".into(), Value::String(text));
            t
        })]));
        m.insert("usage".into(), Value::Object({
            let mut u = serde_json::Map::new();
            u.insert("input_tokens".into(), Value::Number(0.into()));
            u.insert("output_tokens".into(), Value::Number(0.into()));
            u
        }));
        m
    }))
}

fn parse_verdict(message: &Value) -> std::result::Result<(String, Vec<Value>), RunnerError> {
    let blocks = message.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default();
    for block in &blocks {
        let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
        if let Ok(data) = serde_json::from_str::<Value>(text) {
            if let Some(v) = data.get("verdict").and_then(|v| v.as_str()) {
                let criteria = data.get("criteria").and_then(|c| c.as_array()).cloned().unwrap_or_default();
                return Ok((v.to_uppercase(), criteria));
            }
        }
    }
    if !blocks.is_empty() { return Ok(("PASS".into(), vec![])); }
    Err(RunnerError::Other("verifier returned no usable verdict".into()))
}

pub fn verify_node(store: &Store, node: &Node, deliverable: &str, criteria: &[String]) -> std::result::Result<(String, Vec<Value>), RunnerError> {
    let prompt = format!(
        "Contract goal: {}\nAcceptance criteria:\n{}\nDeliverable:\n{}\n",
        node.goal, bullets(criteria), if deliverable.is_empty() { "(no textual deliverable)" } else { deliverable }
    );
    let _ = store.append_log(node, &serde_json::json!({"event":"verify_request","criteria":criteria}));
    let message = call_critic(&prompt)?;
    let (verdict, results) = parse_verdict(&message)?;
    let _ = store.append_log(node, &serde_json::json!({"event":"verify_result","verdict":verdict}));
    Ok((verdict, results))
}

fn result_from_payload(verb: &str, payload: &Value) -> std::result::Result<VerbResult, RunnerError> {
    let mut r = VerbResult { verb: verb.to_string(), ..Default::default() };
    match verb {
        SPLIT => {
            if let Some(arr) = payload.get("subtasks").and_then(|s| s.as_array()) {
                r.subtasks = arr.iter().map(|item| {
                    let ac: Vec<String> = item.get("acceptance_criteria").and_then(|a| a.as_array())
                        .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
                    Contract {
                        goal: item.get("goal").and_then(|g| g.as_str()).unwrap_or("").to_string(),
                        acceptance_criteria: ac, interfaces: vec![], constraints: vec![],
                        id: item.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string(),
                        depends_on: item.get("depends_on").and_then(|d| d.as_array())
                            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()).unwrap_or_default(),
                        allocation: item.get("allocation").and_then(|a| a.as_i64()).unwrap_or(0),
                    }
                }).collect();
            }
        }
        COMPLETE_VERB => {
            r.deliverable = payload.get("deliverable").and_then(|d| d.as_str()).unwrap_or("").to_string();
            r.summary = payload.get("summary").and_then(|s| s.as_str()).unwrap_or("").to_string();
            if let Some(arr) = payload.get("artifacts").and_then(|a| a.as_array()) {
                r.artifacts = arr.iter().map(|item| {
                    (item.get("path").and_then(|p| p.as_str()).unwrap_or("out.txt").to_string(),
                     item.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string())
                }).collect();
            }
        }
        ESCALATE => {
            r.assumption = payload.get("assumption").and_then(|a| a.as_str()).unwrap_or("").to_string();
            r.evidence = payload.get("evidence").and_then(|e| e.as_str()).unwrap_or("").to_string();
        }
        ESCALATE_RESOLVE => {
            r.resolution = payload.get("resolution").and_then(|r| r.as_str()).unwrap_or("").to_lowercase();
        }
        NOTE_GLOBAL => {
            r.entry_type = payload.get("type").and_then(|t| t.as_str()).unwrap_or("").to_string();
            r.entry_content = payload.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
            r.entry_supersedes = payload.get("supersedes").and_then(|s| s.as_str()).unwrap_or("").to_string();
        }
        _ => return Err(RunnerError::Other(format!("unknown verb {verb:?}"))),
    }
    Ok(r)
}

pub fn parse_message(message: &Value) -> std::result::Result<VerbResult, RunnerError> {
    let blocks = message.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default();

    for block in &blocks {
        let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("").to_lowercase();
        if btype == "tool_use" && [SPLIT, COMPLETE_VERB, ESCALATE, ESCALATE_RESOLVE, NOTE_GLOBAL].contains(&name.as_str()) {
            if let Some(input) = block.get("input") {
                return result_from_payload(&name, input);
            }
        }
    }

    for block in &blocks {
        let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if btype == "text" || btype.is_empty() {
            let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
            if let Ok(payload) = serde_json::from_str::<Value>(text) {
                if let Some(verb) = payload.get("verb").and_then(|v| v.as_str()) {
                    return result_from_payload(verb, &payload);
                }
            }
        }
    }

    Err(RunnerError::Other("no usable verb found in response".into()))
}

pub fn run_node(store: &Store, node: &Node) -> std::result::Result<VerbResult, RunnerError> {
    let prompt = assemble_context(store, node).map_err(|e| RunnerError::Other(e.to_string()))?;
    let message = call_model(&prompt, &node.path)?;
    let result = parse_message(&message)?;
    Ok(result)
}
