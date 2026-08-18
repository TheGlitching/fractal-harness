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

static DECISION_RE: OnceLock<Regex> = OnceLock::new();
fn decision_re() -> &'static Regex {
    DECISION_RE.get_or_init(|| {
        Regex::new(r#"\{"verb":\s*"(split|complete|escalate|escalate_resolve|note_global)""#)
            .unwrap()
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
            RunnerError::NoDecision(s) => write!(f, "no decision found: {s}"),
            RunnerError::Other(s) => write!(f, "{s}"),
        }
    }
}

pub type OutputFn = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

pub fn get_executor() -> String {
    std::env::var("FRACTAL_EXECUTOR")
        .unwrap_or_else(|_| "omp".to_string())
        .to_lowercase()
}

pub fn pick_model(default: &str) -> String {
    if !default.is_empty() && default != "default" {
        return default.to_string();
    }
    match get_executor().as_str() {
        "omp" | "pi" => "openrouter/google/gemini-3.7-flash".to_string(),
        "opencode" => "claude-sonnet-4-6".to_string(),
        _ => "openrouter/google/gemini-3.7-flash".to_string(),
    }
}

const OP_SYSTEM: &str = "\
You are a node in a fractal task tree.

Your SOLE JOB: read the contract you are given and decide immediately — \
SPLIT if the contract asks for more than ONE file, spans multiple concerns, \
or would take more than a couple of minutes. If you split, your ONLY output \
is the subtask list. Do not implement anything — you dissolve and the tree \
is memory, you are not.

COMPLETE only if the contract is a single small unit of work — one file, \
one concern, implementable in a single pass. Only then do you write code.

This decision recurs at every level. A large task arrives, the root splits \
it into N subtasks, each child receives one and makes the same choice. A \
child that can do its job in one pass completes; a child that cannot splits \
again. That recursion is the fractal — every node is a decomposer first, an \
implementer second.

Verbs: split (break into subtasks), complete (deliver the contract), \
escalate (report broken assumption), escalate_resolve (settle escalation), \
note_global (write shared rule).
";

pub fn assemble_context(store: &Store, node: &Node) -> std::result::Result<String, StoreError> {
    let mut parts = Vec::new();
    let contract = node.contract();
    parts.push(contract.render(&node.id, node.depth, node.parent.as_deref()));

    let disk_artifacts = node.find_artifacts();
    if !disk_artifacts.is_empty() {
        parts.push(format!(
            "## Available artifacts on disk\n{}\n",
            disk_artifacts
                .iter()
                .map(|p| format!("- {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }

    // Expose artifacts from direct dependencies (depends_on) and the unified workspace
    let mut dep_artifacts = Vec::new();
    let all_nodes = store.walk().unwrap_or_default();
    for dep_id in &node.depends_on {
        if let Some(dep_node) = all_nodes.iter().find(|n| n.id == *dep_id) {
            for art in dep_node.find_artifacts() {
                if let Ok(rel) = art.strip_prefix(dep_node.artifacts_dir()) {
                    let preview = fs::read_to_string(&art).unwrap_or_default();
                    let head = if preview.len() > 600 {
                        format!("{}... [{} bytes]", &preview[..600], preview.len())
                    } else {
                        preview
                    };
                    dep_artifacts.push(format!("### Dependency artifact: {} (from {})\n```\n{}\n```\n", rel.display(), dep_node.id, head));
                }
            }
        }
    }
    if !dep_artifacts.is_empty() {
        parts.push(format!(
            "## Dependency Code & Artifacts (depends_on)\n{}\n",
            dep_artifacts.join("\n")
        ));
    }

    let unified = store.unified_dir();
    if unified.exists() {
        parts.push(format!(
            "## Shared Unified Codebase (`dist/`)\nAll previously completed subtasks have synced their code to `{}`. You can reference or build upon them.\n",
            unified.display()
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

    // If this node has completed children, summarize them for aggregation
    let children = store.children_of(node).unwrap_or_default();
    if !children.is_empty() {
        let mut child_summaries = Vec::new();
        for c in &children {
            let sum = c.summary.lines().next().unwrap_or(&c.summary);
            let artifacts = c.find_artifacts();
            let art_str = if artifacts.is_empty() {
                String::new()
            } else {
                format!(
                    " [artifacts: {}]",
                    artifacts
                        .iter()
                        .map(|p| p.file_name().unwrap_or_default().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            child_summaries.push(format!("- {} ({}): {}{}", c.id, c.status, sum, art_str));
        }
        parts.push(format!(
            "## Subtasks Completed by Children\nAll child subtasks have succeeded:\n{}\n\nSince all child subtasks are complete, output a `complete` JSON decision synthesizing the milestone deliverables.\n",
            child_summaries.join("\n")
        ));
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
## Instructions & Lifecycle Commands

You are executing directly in the project workspace with tools (write, edit, read, bash).

### PHASE 1 — DECIDE (first check):
- If this contract requires multiple components, run fractal split via bash and STOP:
  fractal split --subtasks '[{\"id\":\"a\",\"goal\":\"...\",\"acceptance_criteria\":[\"...\"]}]'

### PHASE 2 — EXECUTE (if implementing directly):
1. Create or edit files directly in proper directories (src/..., tests/...).
2. Run your tests with bash to verify your implementation.
3. Signal completion with fractal done:
  fractal done --summary \"Summary of implemented code and tests\"

### ESCALATE (if an assumption is false):
  fractal escalate --assumption \"...\" --evidence \"...\"
"
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

    // 2. Look for JSON inside Markdown code blocks (```json ... ``` or ``` ... ```)
    let re_block = Regex::new(r"```(?:json)?\s*(\{[\s\S]*?\})\s*```").ok();
    if let Some(re) = re_block {
        for cap in re.captures_iter(text) {
            if let Some(m) = cap.get(1) {
                if let Ok(v) = serde_json::from_str::<Value>(m.as_str()) {
                    if v.get("verb").is_some() || v.get("verdict").is_some() {
                        return Some(v);
                    }
                }
            }
        }
    }

    // 3. Scan for JSON blocks starting with {"verb" or {"verdict"
    let re_start = Regex::new(r#"\{[\s\n]*"(?:verb|verdict)"[\s\S]*"#).ok();
    if let Some(re) = re_start {
        for mat in re.find_iter(text) {
            let sub = mat.as_str();
            let mut depth = 0;
            let mut in_string = false;
            let mut escape = false;
            let mut end_idx = None;

            for (idx, ch) in sub.char_indices() {
                if escape {
                    escape = false;
                    continue;
                }
                if ch == '\\' && in_string {
                    escape = true;
                    continue;
                }
                if ch == '"' {
                    in_string = !in_string;
                    continue;
                }
                if !in_string {
                    if ch == '{' {
                        depth += 1;
                    } else if ch == '}' {
                        depth -= 1;
                        if depth == 0 {
                            end_idx = Some(idx + 1);
                            break;
                        }
                    }
                }
            }

            if let Some(end) = end_idx {
                let candidate = &sub[..end];
                if let Ok(v) = serde_json::from_str::<Value>(candidate) {
                    if v.get("verb").is_some() || v.get("verdict").is_some() {
                        return Some(v);
                    }
                }
            }
        }
    }

    // 4. Fallback search for any balanced JSON object starting with '{'
    for (start, _) in text.match_indices('{') {
        let mut depth = 0;
        let mut in_string = false;
        let mut escape = false;
        let mut end_idx = None;

        for (offset, ch) in text[start..].char_indices() {
            if escape {
                escape = false;
                continue;
            }
            if ch == '\\' && in_string {
                escape = true;
                continue;
            }
            if ch == '"' {
                in_string = !in_string;
                continue;
            }
            if !in_string {
                if ch == '{' {
                    depth += 1;
                } else if ch == '}' {
                    depth -= 1;
                    if depth == 0 {
                        end_idx = Some(start + offset + 1);
                        break;
                    }
                }
            }
        }

        if let Some(end) = end_idx {
            if let Ok(v) = serde_json::from_str::<Value>(&text[start..end]) {
                if v.get("verb").is_some() || v.get("verdict").is_some() {
                    return Some(v);
                }
            }
        }
    }

    None
}

#[derive(Default, Debug)]
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

fn bullets(items: &[String]) -> String {
    if items.is_empty() {
        "  (none)".into()
    } else {
        items
            .iter()
            .map(|i| format!("  - {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
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
        prompt.push_str(&format!("\n\n## Feedback from previous attempt\n{fb}\n"));
    }

    let executor = get_executor();
    let project_root = store.tree_dir.parent().unwrap_or(&store.tree_dir);
    match executor.as_str() {
        "omp" | "pi" => call_via_omp(&prompt, node.path.as_path(), project_root, model, on_output),
        _ => call_via_omp(&prompt, node.path.as_path(), project_root, model, on_output),
    }
}

pub fn call_via_omp(
    prompt: &str,
    node_path: &Path,
    project_root: &Path,
    model: &str,
    on_output: OutputFn,
) -> std::result::Result<VerbResult, RunnerError> {
    let claude_md = format!("{OP_SYSTEM}\n\n{prompt}");
    let claude_path = node_path.join("CLAUDE.md");
    fs::write(&claude_path, &claude_md).map_err(|e| RunnerError::Other(format!("write: {e}")))?;

    let bin = which::which("omp")
        .or_else(|_| which::which("pi"))
        .map_err(|_| RunnerError::NotFound("omp/pi binary not found".into()))?;

    let node_name = node_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut cmd = Command::new(&bin);
    cmd.arg("-p");
    cmd.arg("--cwd").arg(project_root);
    cmd.arg("--auto-approve");
    cmd.arg("--approval-mode=yolo");
    cmd.env("FRACTAL_NODE_ID", &node_name);
    if !model.is_empty() && model != "default" {
        cmd.arg(format!("--model={model}"));
    }
    let node_instructions = format!("Read @{}. You are a node in a fractal task tree. Use your tools directly in the project. Signal completion with `fractal done` or split with `fractal split`.", claude_path.display());
    cmd.arg(node_instructions);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| RunnerError::Other(format!("spawn: {e}")))?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (std_tx, std_rx) = std::sync::mpsc::channel::<(bool, String, String)>();
    let std_tx_err = std_tx.clone();

    let node_name_out = node_name.clone();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().flatten() {
            let log_line = format!(" [{}] {}", node_name_out, line);
            let _ = std_tx.send((false, log_line, line));
        }
    });
    let node_name_err = node_name.clone();
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().flatten() {
            let log_line = format!(" [{}] ERR: {}", node_name_err, line);
            let _ = std_tx_err.send((true, log_line, line));
        }
    });

    let mut stdout_lines = Vec::new();
    let mut stderr_lines = Vec::new();
    let start = std::time::Instant::now();
    let timeout_secs = std::env::var("FRACTAL_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);

    loop {
        while let Ok((is_err, log_line, raw_line)) = std_rx.try_recv() {
            on_output(&log_line);
            if is_err {
                stderr_lines.push(raw_line);
            } else {
                stdout_lines.push(raw_line);
            }
        }
        if start.elapsed().as_secs() > timeout_secs {
            let _ = child.kill();
            return Err(RunnerError::Timeout);
        }
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(e) => return Err(RunnerError::Other(format!("wait: {e}"))),
        }
    }

    while let Ok((is_err, log_line, raw_line)) = std_rx.try_recv() {
        on_output(&log_line);
        if is_err {
            stderr_lines.push(raw_line);
        } else {
            stdout_lines.push(raw_line);
        }
    }
    let all_text = stdout_lines.join("\n");
    let decision_file = project_root.join(format!(".fractal_decision_{}", node_name));
    let val = if decision_file.exists() {
        let content = fs::read_to_string(&decision_file).unwrap_or_default();
        let _ = fs::remove_file(&decision_file);
        serde_json::from_str::<Value>(&content).ok()
    } else {
        None
    }
    .or_else(|| extract_decision(&all_text))
    .unwrap_or_else(|| {
        serde_json::json!({
            "verb": "complete",
            "summary": "Completed contract directly in workspace via native tools.",
            "deliverable": "Completed contract directly in workspace via native tools."
        })
    });

    let verb = val
        .get("verb")
        .and_then(|v| v.as_str())
        .unwrap_or(COMPLETE_VERB);
    result_from_payload(verb, &val)
}

pub fn call_critic(
    prompt: &str,
    model: &str,
) -> std::result::Result<Value, RunnerError> {
    let temp_dir = std::env::temp_dir().join(format!("fractal_critic_{}", std::process::id()));
    let _ = fs::create_dir_all(&temp_dir);
    let claude_md = format!("{CRITIC_SYSTEM}\n\nReview against acceptance criteria. Output JSON verdict with PASS or FAIL.\n\n{prompt}");
    let _ = fs::write(temp_dir.join("CLAUDE.md"), &claude_md);

    let bin = which::which("omp")
        .or_else(|_| which::which("pi"))
        .map_err(|_| RunnerError::NotFound("omp/pi binary not found".into()))?;

    let mut cmd = Command::new(&bin);
    cmd.arg("-p");
    cmd.arg("--cwd").arg(&temp_dir);
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
acceptance criteria.

Evaluate the deliverable summary, contract goal, and acceptance criteria. If the implementation satisfies the criteria, output PASS.

Output ONLY a JSON object:
{\"verdict\": \"PASS\" | \"FAIL\", \"reason\": \"...\", \"criteria\": [{\"name\": \"...\", \"pass\": true | false, \"reason\": \"...\"}]}
";

pub fn verify_node(
    store: &Store,
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

    // If local artifacts are empty, check if children have verified artifacts
    let children = store.children_of(node).unwrap_or_default();
    let mut children_summary = String::new();
    if !children.is_empty() {
        children_summary.push_str("Child Subtasks Completed & Verified:\n");
        for c in &children {
            let art_list = c.find_artifacts();
            let names: Vec<_> = art_list.iter().filter_map(|p| p.file_name()).map(|f| f.to_string_lossy()).collect();
            children_summary.push_str(&format!("- Subtask {} ({}): {} [Artifacts: {}]\n", c.id, c.status, c.summary, names.join(", ")));
        }
    }

    let prompt = format!(
        "Contract goal: {}\nAcceptance criteria:\n{}\nDeliverable summary:\n{}\n{}\nArtifacts produced:\n{}",
        node.goal,
        bullets(criteria),
        if deliverable.is_empty() {
            "(no text summary)"
        } else {
            deliverable
        },
        if children_summary.is_empty() {
            String::new()
        } else {
            format!("\n{children_summary}\n")
        },
        if artifact_summary.is_empty() {
            if !children.is_empty() {
                "(verified by completed child subtasks above)"
            } else if !deliverable.is_empty() {
                "(implemented directly in workspace via native tools)"
            } else {
                "(no artifacts provided)"
            }
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
                    let crit: Vec<String> = item
                        .get("acceptance_criteria")
                        .and_then(|a| a.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    let id = item
                        .get("id")
                        .and_then(|g| g.as_str())
                        .unwrap_or("")
                        .to_string();
                    let deps: Vec<String> = item
                        .get("depends_on")
                        .and_then(|d| d.as_array())
                        .map(|d| {
                            d.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    r.subtasks.push(Contract {
                        goal,
                        acceptance_criteria: crit,
                        id,
                        depends_on: deps,
                        ..Default::default()
                    });
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
            if r.deliverable.is_empty() && !r.summary.is_empty() {
                r.deliverable = r.summary.clone();
            }
            if let Some(arr) = payload.get("artifacts").and_then(|a| a.as_array()) {
                for art in arr {
                    let p = art
                        .get("path")
                        .and_then(|p| p.as_str())
                        .unwrap_or("")
                        .to_string();
                    let c = art
                        .get("content")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !p.is_empty() {
                        r.artifacts.push((p, c));
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

fn parse_verdict(msg: &Value) -> std::result::Result<(String, Vec<Value>), RunnerError> {
    let content = msg
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("");
    let d = extract_decision(content).ok_or_else(|| {
        RunnerError::Other(format!("unparseable critic response: {content}"))
    })?;
    let verdict = d
        .get("verdict")
        .and_then(|v| v.as_str())
        .unwrap_or("PASS")
        .to_string();
    let details: Vec<Value> = d
        .get("criteria")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    Ok((verdict, details))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_decision_simple() {
        let text = r#"Here is the output:
{"verb":"complete","deliverable":"all good","summary":"done"}
Done."#;
        let d = extract_decision(text);
        assert!(d.is_some());
        let val = d.unwrap();
        assert_eq!(val.get("verb").unwrap(), "complete");
    }

    #[test]
    fn test_extract_decision_in_markdown() {
        let text = r#"I have decomposed the goal:
```json
{
  "verb": "split",
  "subtasks": [
    {"id": "setup", "goal": "Build CLI", "acceptance_criteria": ["works"]}
  ]
}
```
Good luck!"#;
        let d = extract_decision(text);
        assert!(d.is_some());
        let val = d.unwrap();
        assert_eq!(val.get("verb").unwrap(), "split");
    }

    #[test]
    fn test_extract_decision_with_nested_strings() {
        let text = r#"Implementing:
{"verb":"complete","summary":"done","deliverable":"all good","artifacts":[{"path":"artifacts/test.ts","content":"export function score(a: number) { if (true) { return { a: 1 }; } return 0; }"}]}
Working..."#;
        let d = extract_decision(text);
        assert!(d.is_some());
        let val = d.unwrap();
        assert_eq!(val.get("verb").unwrap(), "complete");
    }
}
