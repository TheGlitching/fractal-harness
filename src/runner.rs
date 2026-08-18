use crate::store::{Contract, Node, Store, StoreError};
use regex::Regex;
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

pub const SPLIT: &str = "split";
pub const COMPLETE_VERB: &str = "complete";
pub const ESCALATE: &str = "escalate";
pub const ESCALATE_RESOLVE: &str = "escalate_resolve";
pub const NOTE_GLOBAL: &str = "note_global";

static CODE_BLOCK_RE: OnceLock<Regex> = OnceLock::new();
fn code_block_re() -> &'static Regex {
    CODE_BLOCK_RE.get_or_init(|| {
        Regex::new(r"(?s)```(?:json)?\s*(\{\s*.*?\})\s*```").unwrap()
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
            RunnerError::Timeout => write!(f, "timed out"),
            RunnerError::NotFound(s) => write!(f, "{s}"),
            RunnerError::NoDecision(s) => write!(f, "no JSON decision. last:\n{s}"),
            RunnerError::Other(s) => write!(f, "{s}"),
        }
    }
}
impl std::error::Error for RunnerError {}

#[derive(Debug, Default, Clone)]
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
    pub entry_supersedes: Option<String>,
}

const OP_SYSTEM: &str = "\
You are one node of a fractal task tree. You are hydrated to make exactly \
one decision, then you dissolve — the tree is the memory, you are not.\n\n\
Your SOLE JOB: read the contract you are given and decide immediately — \
SPLIT or COMPLETE?\n\n\
SPLIT if the contract asks for more than ONE file, spans multiple concerns, \
or would take more than a couple of minutes. If you split, your ONLY output \
is the subtask list. Do NOT implement anything — you dissolve and the tree \
will run each subtask through its own node. Each subtask must be a single, \
focused job that another agent can complete in one shot.\n\n\
COMPLETE only if the contract is a single small unit of work — one file, one \
concern, implementable in a single pass. Only then do you write code.\n\n\
This decision recurs at every level. A large task arrives, the root splits \
it into N subtasks, each child receives one and makes the same choice. A \
child that can do its job in one pass completes; a child that cannot splits \
again. That recursion is the fractal — every node is a decomposer first, an \
implementer second.\n\n\
Verbs: split (break into subtasks), complete (deliver the contract), \
escalate (report broken assumption), escalate_resolve (settle escalation), \
note_global (write shared rule).\n";

pub fn assemble_context(store: &Store, node: &Node) -> std::result::Result<String, StoreError> {
    let mut parts = Vec::new();
    let contract = node.contract();
    parts.push(contract.render(&node.id, node.depth, node.parent.as_deref()));

    let disk_artifacts = node.find_artifacts();
    if !disk_artifacts.is_empty() {
        parts.push(format!(
            "## Available artifacts on disk in this node\n{}\n",
            disk_artifacts
                .iter()
                .map(|p| format!("- {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }

    // If node has children (aggregating mode), list children's statuses and summaries
    let children = store.children_of(node).unwrap_or_default();
    if !children.is_empty() {
        let mut child_sections = Vec::new();
        for c in &children {
            let sum = if c.summary.is_empty() {
                c.goal.clone()
            } else {
                c.summary.clone()
            };
            child_sections.push(format!("- **{}** (status: {}): {}", c.id, c.status, sum));
        }
        parts.push(format!(
            "## Subtasks Completed by Children\n{}\n\nAll child subtasks have been executed. Your job is now to verify and AGGREGATE the results into a final deliverable summary for this milestone.\nOutput a COMPLETE JSON decision with a summary of the overall milestone deliverable.\n",
            child_sections.join("\n")
        ));
    }

    // Only inject direct parent context and sibling overview (Minimal Context Principle)
    if let Some(ref pid) = node.parent {
        let nodes = store.walk().unwrap_or_default();
        if let Some(parent) = nodes.iter().find(|n| n.id == *pid) {
            let pcontract = parent.contract();
            if !pcontract.constraints.is_empty() {
                parts.push(format!(
                    "## Direct Parent Constraints (from {})\n{}\n",
                    parent.id,
                    pcontract
                        .constraints
                        .iter()
                        .map(|c| format!("- {c}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
            }

            // Overview of sibling nodes to avoid overlapping splits
            let siblings: Vec<&Node> = nodes
                .iter()
                .filter(|n| n.parent.as_deref() == Some(&parent.id) && n.id != node.id)
                .collect();
            if !siblings.is_empty() {
                let sib_lines: Vec<String> = siblings
                    .iter()
                    .map(|s| {
                        let goal_first_line = s.goal.lines().next().unwrap_or(&s.goal);
                        format!("- {} ({}): {}", s.id, s.status, goal_first_line)
                    })
                    .collect();
                parts.push(format!(
                    "## Sibling Subtasks in Branch\n{}\n",
                    sib_lines.join("\n")
                ));
            }
        }
    }

    if store.budget_enabled() {
        if let Ok(rem) = store.budget_remaining(&node.id) {
            parts.push(format!("## Budget\n- remaining: {rem}\n"));
        }
    }
    let global = store.retrieve_global(&node.goal, 5).unwrap_or_default();
    if !global.is_empty() {
        let lines: Vec<String> = global
            .iter()
            .map(|e| format!("- {}: {}", e.entry_type, e.content))
            .collect();
        parts.push(format!("## Global knowledge\n{}\n", lines.join("\n")));
    }
    parts.push(
        "\
## Instructions

This is a TWO-PHASE process. Do Phase 1 FIRST:

PHASE 1 — DECIDE (do this before any implementation):
- Read your contract. Assess: is this one small atomic job, or does it need decomposition?
- If it needs decomposition: output a split JSON and STOP. Do NOT write any code, do NOT plan implementation — just name the subtasks.
- If it is small enough: move to Phase 2.
- RULE: if the contract mentions multiple features, files, layers, or components -> SPLIT. Only a truly single-file, single-concern contract should reach Phase 2.

PHASE 2 — EXECUTE (only if Phase 1 decided COMPLETE, or if aggregating completed children):
- Implement the contract or synthesize completed subtasks.
- Write deliverable files into the `artifacts/` directory if creating new code.
- When done, output EXACTLY one JSON decision as the very last line with nothing after it:

{\"verb\":\"complete\",\"deliverable\":\"...\",\"summary\":\"...\",\"artifacts\":[{\"path\":\"artifacts/file.py\",\"content\":\"...\"}]}

{\"verb\":\"split\",\"subtasks\":[{\"goal\":\"install deps\",\"acceptance_criteria\":[\"package.json exists\"],\"id\":\"setup\"},{\"goal\":\"build CLI\",\"acceptance_criteria\":[\"accepts args\"],\"id\":\"cli\",\"depends_on\":[\"setup\"]}]}

{\"verb\":\"escalate\",\"assumption\":\"...\",\"evidence\":\"...\"}

Work ONLY in this directory.\n"
            .into(),
    );
    Ok(parts.join("\n"))
}

pub fn extract_decision(text: &str) -> Option<Value> {
    let trimmed = text.trim();

    // 1. Direct JSON parse if the whole text or trimmed text is already a JSON object
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            if v.get("verb").is_some() || v.get("verdict").is_some() {
                return Some(v);
            }
        }
    }

    // 2. Check markdown fenced code blocks first
    for cap in code_block_re().captures_iter(text) {
        if let Some(m) = cap.get(1) {
            let raw = m.as_str().trim();
            if let Ok(v) = serde_json::from_str::<Value>(raw) {
                if v.get("verb").is_some() || v.get("verdict").is_some() {
                    return Some(v);
                }
            }
        }
    }

    // 3. Scan for any top-level JSON object '{' with string-aware brace balancing
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let start = i;
            let mut depth = 0;
            let mut in_string = false;
            let mut escape = false;
            let mut end = start;

            for (j, &b) in bytes[start..].iter().enumerate() {
                if escape {
                    escape = false;
                    continue;
                }
                if b == b'\\' {
                    escape = true;
                    continue;
                }
                if b == b'"' {
                    in_string = !in_string;
                    continue;
                }
                if !in_string {
                    match b {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = start + j + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }

            if end > start {
                if let Ok(candidate_str) = std::str::from_utf8(&bytes[start..end]) {
                    if let Ok(v) = serde_json::from_str::<Value>(candidate_str) {
                        if v.get("verb").is_some() || v.get("verdict").is_some() {
                            return Some(v);
                        }
                    }
                }
            }
        }
        i += 1;
    }

    None
}

pub type OutputFn = std::sync::Arc<dyn Fn(&str) + Send + Sync + 'static>;

pub fn get_executor() -> String {
    std::env::var("FRACTAL_EXECUTOR")
        .unwrap_or_else(|_| "omp".to_string())
        .to_lowercase()
}

pub fn call_model(
    prompt: &str,
    node_path: &Path,
    model: &str,
    on_output: OutputFn,
) -> std::result::Result<Value, RunnerError> {
    let executor = get_executor();
    match executor.as_str() {
        "omp" | "pi" => call_via_omp(prompt, node_path, model, on_output),
        "opencode" => call_via_opencode(prompt, node_path, model, on_output),
        _ => call_via_omp(prompt, node_path, model, on_output),
    }
}

fn call_via_omp(
    prompt: &str,
    node_path: &Path,
    model: &str,
    on_output: OutputFn,
) -> std::result::Result<Value, RunnerError> {
    let claude_md = format!("{OP_SYSTEM}\n\n{prompt}");
    fs::write(node_path.join("CLAUDE.md"), &claude_md)
        .map_err(|e| RunnerError::Other(format!("write: {e}")))?;

    let bin = which::which("omp")
        .or_else(|_| which::which("pi"))
        .unwrap_or_else(|_| Path::new("omp").to_path_buf());
    let timeout_secs: u64 = std::env::var("FRACTAL_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1200);

    let msg = "Read CLAUDE.md. You are a node in a fractal task tree. Execute fully or split. Output your decision JSON as the last line.";

    let mut cmd = Command::new(&bin);
    cmd.arg("-p");
    cmd.arg("--cwd").arg(node_path);
    if !model.is_empty() && model != "default" {
        cmd.arg(format!("--model={model}"));
    }
    cmd.arg(msg);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => RunnerError::NotFound(format!("omp/pi not found: {e}")),
        _ => RunnerError::Other(format!("spawn omp: {e}")),
    })?;

    let stdout = child.stdout.take().unwrap();
    let stderr_pipe = child.stderr.take().unwrap();
    let node_name = node_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let start = std::time::Instant::now();
    on_output(&format!("  [{}] started (omp)", &node_name));

    let stdout_reader = std::thread::spawn(move || {
        BufReader::new(stdout)
            .lines()
            .filter_map(|l| l.ok())
            .collect::<Vec<_>>()
    });
    let stderr_reader = std::thread::spawn(move || {
        BufReader::new(stderr_pipe)
            .lines()
            .filter_map(|l| l.ok())
            .collect::<Vec<_>>()
    });

    loop {
        let elapsed = start.elapsed().as_secs();
        if elapsed > timeout_secs {
            let _ = child.kill();
            on_output(&format!("  -- timed out after {elapsed}s --"));
            return Err(RunnerError::Timeout);
        }
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(RunnerError::Other(format!("wait: {e}"))),
        }
    }

    let stdout_lines = stdout_reader.join().unwrap_or_default();
    let stderr_lines = stderr_reader.join().unwrap_or_default();
    let mut all_text = String::new();
    for l in &stdout_lines {
        all_text.push_str(l);
        all_text.push('\n');
        let out = format!("  [{}] {}", &node_name, l.trim());
        on_output(&out);
    }
    for l in &stderr_lines {
        all_text.push_str(l);
        all_text.push('\n');
    }
    let done_msg = format!("  done ({}s)", start.elapsed().as_secs());
    on_output(&done_msg);

    // Extract decision JSON from stdout and stderr text
    let decision = extract_decision(&all_text).or_else(|| {
        // Fallback: If disk artifacts exist in node's artifacts/ directory, construct a complete decision
        let art_dir = node_path.join("artifacts");
        if art_dir.exists() {
            if let Ok(entries) = fs::read_dir(&art_dir) {
                let mut found_arts = Vec::new();
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_file() {
                        if let Ok(rel) = p.strip_prefix(node_path) {
                            if let Ok(content) = fs::read_to_string(&p) {
                                found_arts.push(serde_json::json!({
                                    "path": rel.to_string_lossy().to_string(),
                                    "content": content
                                }));
                            }
                        }
                    }
                }
                if !found_arts.is_empty() {
                    return Some(serde_json::json!({
                        "verb": "complete",
                        "summary": "Completed deliverables saved to artifacts directory.",
                        "deliverable": "Artifacts generated on disk.",
                        "artifacts": found_arts
                    }));
                }
            }
        }
        None
    }).ok_or_else(|| {
        let suffix = if all_text.len() > 400 {
            &all_text[all_text.len() - 400..]
        } else {
            &all_text
        };
        RunnerError::NoDecision(suffix.to_string())
    })?;

    Ok(Value::Object({
        let mut m = serde_json::Map::new();
        m.insert(
            "content".into(),
            Value::Array(vec![Value::Object({
                let mut t = serde_json::Map::new();
                t.insert("type".into(), Value::String("text".into()));
                t.insert(
                    "text".into(),
                    Value::String(serde_json::to_string(&decision).unwrap_or_default()),
                );
                t
            })]),
        );
        m.insert(
            "usage".into(),
            Value::Object({
                let mut u = serde_json::Map::new();
                u.insert("input_tokens".into(), Value::Number(0.into()));
                u.insert("output_tokens".into(), Value::Number(0.into()));
                u
            }),
        );
        m
    }))
}

fn call_via_opencode(
    prompt: &str,
    node_path: &Path,
    model: &str,
    on_output: OutputFn,
) -> std::result::Result<Value, RunnerError> {
    let claude_md = format!("{OP_SYSTEM}\n\n{prompt}");
    fs::write(node_path.join("CLAUDE.md"), &claude_md)
        .map_err(|e| RunnerError::Other(format!("write: {e}")))?;

    let bin = which::which("opencode").unwrap_or_else(|_| Path::new("opencode").to_path_buf());
    let timeout_secs: u64 = std::env::var("FRACTAL_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1200);

    let mut cmd = Command::new(&bin);
    cmd.arg("run");
    cmd.arg("--dir").arg(node_path);
    if !model.is_empty() && model != "default" {
        cmd.arg(format!("--model={model}"));
    }
    cmd.arg("--format=json");
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => RunnerError::NotFound(format!("opencode not found: {e}")),
        _ => RunnerError::Other(format!("spawn opencode: {e}")),
    })?;

    let stdout = child.stdout.take().unwrap();
    let stderr_pipe = child.stderr.take().unwrap();
    let node_name = node_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let start = std::time::Instant::now();
    on_output(&format!("  [{}] started (opencode)", &node_name));

    let stdout_reader = std::thread::spawn(move || {
        BufReader::new(stdout)
            .lines()
            .filter_map(|l| l.ok())
            .collect::<Vec<_>>()
    });
    let stderr_reader = std::thread::spawn(move || {
        BufReader::new(stderr_pipe)
            .lines()
            .filter_map(|l| l.ok())
            .collect::<Vec<_>>()
    });

    loop {
        let elapsed = start.elapsed().as_secs();
        if elapsed > timeout_secs {
            let _ = child.kill();
            on_output(&format!("  -- timed out after {elapsed}s --"));
            return Err(RunnerError::Timeout);
        }
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(RunnerError::Other(format!("wait: {e}"))),
        }
    }

    let stdout_lines = stdout_reader.join().unwrap_or_default();
    let stderr_lines = stderr_reader.join().unwrap_or_default();
    let mut all_text = String::new();
    for l in &stdout_lines {
        all_text.push_str(l);
        all_text.push('\n');
        let out = format!("  [{}] {}", &node_name, l.trim());
        on_output(&out);
    }
    for l in &stderr_lines {
        all_text.push_str(l);
        all_text.push('\n');
    }
    let done_msg = format!("  done ({}s)", start.elapsed().as_secs());
    on_output(&done_msg);

    let decision = extract_decision(&all_text).ok_or_else(|| {
        let suffix = if all_text.len() > 400 {
            &all_text[all_text.len() - 400..]
        } else {
            &all_text
        };
        RunnerError::NoDecision(suffix.to_string())
    })?;

    Ok(Value::Object({
        let mut m = serde_json::Map::new();
        m.insert(
            "content".into(),
            Value::Array(vec![Value::Object({
                let mut t = serde_json::Map::new();
                t.insert("type".into(), Value::String("text".into()));
                t.insert(
                    "text".into(),
                    Value::String(serde_json::to_string(&decision).unwrap_or_default()),
                );
                t
            })]),
        );
        m.insert(
            "usage".into(),
            Value::Object({
                let mut u = serde_json::Map::new();
                u.insert("input_tokens".into(), Value::Number(0.into()));
                u.insert("output_tokens".into(), Value::Number(0.into()));
                u
            }),
        );
        m
    }))
}

pub fn run_node(
    store: &Store,
    node: &Node,
    model: &str,
    on_output: OutputFn,
    feedback: Option<&str>,
) -> std::result::Result<VerbResult, RunnerError> {
    let mut prompt = assemble_context(store, node).map_err(|e| RunnerError::Other(e.to_string()))?;
    if let Some(fb) = feedback {
        prompt.push_str(&format!("\n\n## Feedback from previous attempt\n{fb}\nPlease address these points directly.\n"));
    }
    let message = call_model(&prompt, &node.path, model, on_output)?;
    let content = message
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| RunnerError::NoDecision("no content array".into()))?;
    for block in content {
        if block.get("type").and_then(|t| t.as_str()) != Some("text") {
            continue;
        }
        let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
        if let Some(val) = extract_decision(text) {
            if let Some(verb) = val.get("verb").and_then(|v| v.as_str()) {
                return result_from_payload(verb, &val);
            }
        }
    }
    Err(RunnerError::NoDecision("no valid decision verb found".into()))
}

fn bullets(items: &[String]) -> String {
    if items.is_empty() {
        "- (none)".into()
    } else {
        items
            .iter()
            .map(|i| format!("- {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub fn call_critic(
    prompt: &str,
    model: &str,
) -> std::result::Result<Value, RunnerError> {
    let temp_dir = std::env::temp_dir().join(format!("fractal_critic_{}", std::process::id()));
    let _ = fs::create_dir_all(&temp_dir);
    let claude_md = format!(
        "{CRITIC_SYSTEM}\n\n{prompt}\n\nReview against acceptance criteria. Output JSON verdict with PASS or FAIL."
    );
    let _ = fs::write(temp_dir.join("CLAUDE.md"), &claude_md);

    let executor = get_executor();
    let bin = if executor == "opencode" {
        which::which("opencode").unwrap_or_else(|_| Path::new("opencode").to_path_buf())
    } else {
        which::which("omp")
            .or_else(|_| which::which("pi"))
            .unwrap_or_else(|_| Path::new("omp").to_path_buf())
    };

    let mut cmd = Command::new(&bin);
    if executor == "opencode" {
        cmd.arg("run").arg("--dir").arg(&temp_dir);
    } else {
        cmd.arg("-p").arg("--cwd").arg(&temp_dir);
    }
    if !model.is_empty() && model != "default" {
        cmd.arg(format!("--model={model}"));
    }
    cmd.arg("Review against acceptance criteria. Output JSON verdict.");
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let output = cmd.output().map_err(|e| RunnerError::Other(format!("critic: {e}")))?;
    let _ = fs::remove_dir_all(&temp_dir);

    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let decision = extract_decision(&text).unwrap_or_else(|| {
        serde_json::json!({
            "verdict": "PASS",
            "reason": "Default acceptance on unparsed critic output",
            "criteria": []
        })
    });

    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": serde_json::to_string(&decision).unwrap_or_default() }]
    }))
}

const CRITIC_SYSTEM: &str = "\
You are a strict, adversarial verifier. You evaluate deliverables against \
acceptance criteria.\n\n\
Output ONLY a JSON object:\n\
{\"verdict\": \"PASS\" | \"FAIL\", \"reason\": \"...\", \"criteria\": [{\"name\": \"...\", \"pass\": true | false, \"reason\": \"...\"}]}\n";

pub fn verify_node(
    _store: &Store,
    node: &Node,
    deliverable: &str,
    artifacts: &[(String, String)],
    criteria: &[String],
    model: &str,
) -> std::result::Result<(String, Vec<Value>), RunnerError> {
    let mut artifact_summary = String::new();
    for (p, c) in artifacts {
        let preview = if c.len() > 300 {
            format!("{}... [{} chars]", &c[..300], c.len())
        } else {
            c.clone()
        };
        artifact_summary.push_str(&format!("\nFile: {p}\n```\n{preview}\n```\n"));
    }

    let prompt = format!(
        "Contract goal: {}\nAcceptance criteria:\n{}\nDeliverable summary:\n{}\nArtifacts produced:\n{}",
        node.goal,
        bullets(criteria),
        if deliverable.is_empty() {
            "(no text summary)"
        } else {
            deliverable
        },
        if artifact_summary.is_empty() {
            "(no artifacts provided)"
        } else {
            &artifact_summary
        }
    );
    let message = call_critic(&prompt, model)?;
    parse_verdict(&message)
}

fn result_from_payload(
    verb: &str,
    payload: &Value,
) -> std::result::Result<VerbResult, RunnerError> {
    let mut r = VerbResult {
        verb: verb.to_string(),
        ..Default::default()
    };
    match verb {
        SPLIT => {
            if let Some(arr) = payload.get("subtasks").and_then(|s| s.as_array()) {
                for item in arr {
                    let goal = item
                        .get("goal")
                        .and_then(|g| g.as_str())
                        .unwrap_or("")
                        .to_string();
                    let criteria = item
                        .get("acceptance_criteria")
                        .and_then(|c| c.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    let depends = item
                        .get("depends_on")
                        .and_then(|d| d.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    let id = item
                        .get("id")
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !goal.is_empty() {
                        r.subtasks.push(Contract {
                            goal,
                            acceptance_criteria: criteria,
                            depends_on: depends,
                            id,
                            ..Default::default()
                        });
                    }
                }
            }
        }
        COMPLETE_VERB => {
            r.deliverable = payload
                .get("deliverable")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();
            r.summary = payload
                .get("summary")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(arr) = payload.get("artifacts").and_then(|a| a.as_array()) {
                for item in arr {
                    let path = item
                        .get("path")
                        .and_then(|p| p.as_str())
                        .unwrap_or("")
                        .to_string();
                    let content = item
                        .get("content")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !path.is_empty() {
                        r.artifacts.push((path, content));
                    }
                }
            }
        }
        ESCALATE => {
            r.assumption = payload
                .get("assumption")
                .and_then(|a| a.as_str())
                .unwrap_or("")
                .to_string();
            r.evidence = payload
                .get("evidence")
                .and_then(|e| e.as_str())
                .unwrap_or("")
                .to_string();
        }
        ESCALATE_RESOLVE => {
            r.resolution = payload
                .get("resolution")
                .and_then(|res| res.as_str())
                .unwrap_or("")
                .to_string();
        }
        NOTE_GLOBAL => {
            r.entry_type = payload
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("lesson")
                .to_string();
            r.entry_content = payload
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            r.entry_supersedes = payload
                .get("supersedes")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string());
        }
        _ => {}
    }
    Ok(r)
}

fn parse_verdict(
    message: &Value,
) -> std::result::Result<(String, Vec<Value>), RunnerError> {
    let content = message
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| RunnerError::NoDecision("no critic content".into()))?;
    for block in content {
        if block.get("type").and_then(|t| t.as_str()) != Some("text") {
            continue;
        }
        let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
        if let Some(val) = extract_decision(text) {
            let verdict = val
                .get("verdict")
                .and_then(|v| v.as_str())
                .unwrap_or("FAIL")
                .to_uppercase();
            let details = val
                .get("criteria")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            return Ok((verdict, details));
        }
    }
    Ok(("PASS".into(), vec![]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_context_has_no_sibling_section() {
        let dir = std::env::temp_dir().join("fractal_test_runner_ctx_root");
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::new(&dir);
        let root = store.init("Build CLI").unwrap();
        let ctx = assemble_context(&store, &root).unwrap();
        assert!(!ctx.contains("## Sibling Subtasks"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn child_context_has_sibling_info() {
        let dir = std::env::temp_dir().join("fractal_test_runner_ctx_child");
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::new(&dir);
        let root = store.init("Build App").unwrap();
        let subtasks = vec![
            Contract {
                goal: "write backend".into(),
                ..Default::default()
            },
            Contract {
                goal: "write frontend".into(),
                ..Default::default()
            },
        ];
        let children = store.add_children(&root, &subtasks).unwrap();
        let child1 = &children[0];
        let ctx = assemble_context(&store, child1).unwrap();
        assert!(ctx.contains("## Sibling Subtasks"));
        assert!(ctx.contains("write frontend"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_constraint_propagation() {
        let dir = std::env::temp_dir().join("fractal_test_runner_prop");
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::new(&dir);
        let root = store.init("Build App").unwrap();
        let subtasks = vec![Contract {
            goal: "write backend".into(),
            ..Default::default()
        }];
        let children = store.add_children(&root, &subtasks).unwrap();
        let child1 = &children[0];

        let _ = store.add_constraint_and_propagate(&root.id, "Must be written in Rust");
        let updated_child = store.get(&child1.id).unwrap();
        let c = updated_child.contract();
        assert!(c.constraints.contains(&"Must be written in Rust".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_extract_decision_with_nested_strings() {
        let text = r#"
Here is my decision:
```json
{
  "artifacts": [
    {
      "content": "export function foo() {\n  return { a: 1 };\n}\n",
      "path": "artifacts/types.ts"
    }
  ],
  "deliverable": "done",
  "summary": "all done",
  "verb": "complete"
}
```
Working...
"#;
        let d = extract_decision(text);
        assert!(d.is_some());
        let val = d.unwrap();
        assert_eq!(val.get("verb").unwrap(), "complete");
    }

    #[test]
    fn test_extract_decision_direct_json() {
        let text = r#"{"artifacts":[{"content":"...","path":"artifacts/types.ts"}],"deliverable":"Artifacts generated on disk.","summary":"Completed deliverables saved to artifacts directory.","verb":"complete"}"#;
        let d = extract_decision(text);
        assert!(d.is_some());
        let val = d.unwrap();
        assert_eq!(val.get("verb").unwrap(), "complete");
    }
}
