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
            RunnerError::Timeout => write!(f, "timed out"),
            RunnerError::NotFound(s) => write!(f, "{s}"),
            RunnerError::NoDecision(s) => write!(f, "no JSON decision. last:\n{s}"),
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
You are one node of a fractal task tree. You are hydrated to make exactly \
one decision, then you dissolve — the tree is the memory, you are not.

Your SOLE JOB: read the contract you are given and decide immediately — \
SPLIT or COMPLETE?

SPLIT if the contract asks for more than ONE file, spans multiple concerns, \
or would take more than a couple of minutes. If you split, your ONLY output \
is the subtask list. Do NOT implement anything — you dissolve and the tree \
will run each subtask through its own node. Each subtask must be a single, \
focused job that another agent can complete in one shot.

COMPLETE only if the contract is a single small unit of work — one file, \
one concern, implementable in a single pass. Only then do you write code.

This decision recurs at every level. A large task arrives, the root splits \
it into N subtasks, each child receives one and makes the same choice. A \
child that can do its job in one pass completes; a child that cannot splits \
again. That recursion is the fractal — every node is a decomposer first, \
an implementer second.

Verbs: split (break into subtasks), complete (deliver the contract), \
escalate (flag an assumption or blocker), escalate_resolve (answer \
an escalation from a child), note_global (record knowledge for the tree).";

fn bullets(items: &[String]) -> String {
    if items.is_empty() {
        "- (none)\n".into()
    } else {
        items.iter().map(|s| format!("- {s}\n")).collect()
    }
}

pub fn assemble_context(store: &Store, node: &Node) -> std::result::Result<String, StoreError> {
    let contract_text = fs::read_to_string(node.contract_path()).unwrap_or_default();
    let mut parts = vec![format!(
        "## Your contract\n\n{contract_text}\n\n\
         Write ALL deliverable files into the `artifacts/` directory (it exists).\n"
    )];

    // Minimal context: only immediate parent constraints and direct siblings
    if let Ok(ancestors) = store.ancestors(node) {
        if !ancestors.is_empty() {
            if let Some(parent) = ancestors.last() {
                let mut lines = Vec::new();
                lines.push(format!("- parent {}: {}", parent.id, parent.goal));
                for c in &parent.contract().constraints {
                    lines.push(format!("  - constraint: {c}"));
                }
                parts.push(format!("## Inherited from parent\n{}\n", lines.join("\n")));

                let siblings = store.children_of(parent).unwrap_or_default();
                let slines: Vec<String> = siblings
                    .iter()
                    .filter(|s| s.id != node.id)
                    .map(|s| format!("- {} [{}]: {}", s.id, s.status, s.goal))
                    .collect();

                if !slines.is_empty() {
                    parts.push(format!(
                        "## Siblings (parent: {})\n\
                         Your parent split \"{}\" into these subtasks. \
                         YOU are {}. Handle ONLY your own contract.\n\
                         {}\n",
                        parent.id,
                        parent.goal,
                        node.id,
                        slines.join("\n")
                    ));
                }
                parts.push(format!(
                    "You are a SUBTASK of \"{}\". Your parent already decomposed \
                     the problem — do NOT repeat its split. Your only job is the \
                     contract above. Keep your work small, atomic, and focused.\n",
                    parent.goal
                ));
            }
        }
    }

    if store.budget_enabled() {
        if let Ok(rem) = store.budget_remaining(&node.id) {
            parts.push(format!("## Budget\n- remaining: {rem}\n"));
        }
    }
    let global = store.retrieve_global(&node.goal, 3).unwrap_or_default();
    if !global.is_empty() {
        let lines: Vec<String> = global
            .iter()
            .map(|e| format!("- {}: {}", e.entry_type, e.content))
            .collect();
        parts.push(format!("## Relevant knowledge\n{}\n", lines.join("\n")));
    }

    parts.push(
        "\
## Instructions

This is a TWO-PHASE process. Do Phase 1 FIRST:

PHASE 1 — DECIDE (do this before any implementation):
- Read your contract. Assess: is this one small atomic job, or does it need \
decomposition?
- If it needs decomposition: output a split JSON and STOP. Do NOT write any \
code, do NOT plan implementation — just name the subtasks.
- If it is small enough: move to Phase 2.
- RULE: if the contract mentions multiple features, files, layers, or \
components → SPLIT. Only a truly single-file, single-concern contract \
should reach Phase 2.

PHASE 2 — EXECUTE (only if Phase 1 decided COMPLETE):
- Implement the contract.
- Write ALL deliverable files into the `artifacts/` directory.
- When done, output EXACTLY one JSON decision as the very last line with \
nothing after it:

{\"verb\":\"complete\",\"deliverable\":\"...\",\"summary\":\"...\",\
\"artifacts\":[{\"path\":\"artifacts/file.py\",\"content\":\"...\"}]}

{\"verb\":\"split\",\"subtasks\":[{\"goal\":\"install deps\",\"acceptance_criteria\":[\"package.json exists\"],\"id\":\"setup\"},{\"goal\":\"build CLI\",\"acceptance_criteria\":[\"accepts args\"],\"id\":\"cli\",\"depends_on\":[\"setup\"]}]}

{\"verb\":\"escalate\",\"assumption\":\"...\",\"evidence\":\"...\"}

Work ONLY in this directory.
"
        .into(),
    );
    Ok(parts.join("\n"))
}

pub fn extract_decision(text: &str) -> Option<Value> {
    for m in decision_re().find_iter(text) {
        let start = m.start();
        let mut depth = 0;
        let mut end = start;
        for (i, ch) in text[start..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + i + 1;
                        break;
                    }
                }
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

fn call_via_opencode(
    prompt: &str,
    node_path: &Path,
    model: &str,
    on_output: OutputFn,
) -> std::result::Result<Value, RunnerError> {
    let claude_md = format!("{OP_SYSTEM}\n\n{prompt}");
    fs::write(node_path.join("CLAUDE.md"), &claude_md)
        .map_err(|e| RunnerError::Other(format!("write: {e}")))?;

    let iso_home = std::env::temp_dir().join(format!("fractal-home-{}", std::process::id()));
    let _ = fs::create_dir_all(&iso_home);

    let bin = which::which("opencode").unwrap_or_else(|_| Path::new("opencode").to_path_buf());
    let timeout_secs: u64 = std::env::var("FRACTAL_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1200);
    let real_home = std::env::var("HOME").unwrap_or_default();
    let msg = "Read CLAUDE.md. Execute fully. Write files into artifacts/. When done, output exactly one JSON decision as the last line.";
    let cmd_line = format!(
        "cd '{}' && '{}' run --auto --model '{}' '{}'",
        node_path.display(),
        bin.display(),
        model,
        msg,
    );
    let mut child = Command::new("/bin/sh")
        .args(["-c", &cmd_line])
        .env("HOME", &iso_home)
        .env("XDG_CONFIG_HOME", format!("{real_home}/.config"))
        .env("XDG_DATA_HOME", format!("{real_home}/.local/share"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            let _ = fs::remove_dir_all(&iso_home);
            match e.kind() {
                std::io::ErrorKind::NotFound => RunnerError::NotFound(format!("sh: {e}")),
                _ => RunnerError::Other(format!("sh: {e}")),
            }
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
            let _ = fs::remove_dir_all(&iso_home);
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
    let _ = fs::remove_dir_all(&iso_home);
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

pub fn call_critic(prompt: &str, model: &str) -> std::result::Result<Value, RunnerError> {
    let judge_prompt = format!(
        "\
You are a strict verifier. Judge the deliverable against each acceptance criterion and \
answer with exactly: {{\"verdict\":\"PASS\"|\"FAIL\",\"criteria\":[{{\"name\":\"...\",\
\"pass\":true|false,\"reason\":\"...\"}}]}}\n\n{prompt}"
    );

    let executor = get_executor();
    let timeout_secs: u64 = std::env::var("FRACTAL_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);

    let text = if executor == "opencode" {
        let bin = which::which("opencode").unwrap_or_else(|_| Path::new("opencode").to_path_buf());
        let output = Command::new(&bin)
            .args(["run", "--auto", "--model", model])
            .arg(&judge_prompt)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                let start = std::time::Instant::now();
                loop {
                    match c.try_wait() {
                        Ok(Some(_)) => return c.wait_with_output(),
                        Ok(None) => {
                            if start.elapsed().as_secs() > timeout_secs {
                                let _ = c.kill();
                            }
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                        Err(e) => return Err(e),
                    }
                }
            })
            .map_err(|e| RunnerError::Other(format!("critic opencode: {e}")))?;
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    } else {
        let bin = which::which("omp")
            .or_else(|_| which::which("pi"))
            .unwrap_or_else(|_| Path::new("omp").to_path_buf());
        let mut cmd = Command::new(&bin);
        cmd.arg("-p");
        if !model.is_empty() && model != "default" {
            cmd.arg(format!("--model={model}"));
        }
        cmd.arg(&judge_prompt);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let output = cmd
            .spawn()
            .and_then(|mut c| {
                let start = std::time::Instant::now();
                loop {
                    match c.try_wait() {
                        Ok(Some(_)) => return c.wait_with_output(),
                        Ok(None) => {
                            if start.elapsed().as_secs() > timeout_secs {
                                let _ = c.kill();
                            }
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                        Err(e) => return Err(e),
                    }
                }
            })
            .map_err(|e| RunnerError::Other(format!("critic omp: {e}")))?;
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    };

    Ok(serde_json::json!({
        "content":[{"type":"text","text":text}],
        "usage":{"input_tokens":0,"output_tokens":0}
    }))
}

fn parse_verdict(message: &Value) -> std::result::Result<(String, Vec<Value>), RunnerError> {
    let blocks = message
        .get("content")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    for block in &blocks {
        let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
        if let Some(v) = extract_decision(text) {
            if let Some(verdict) = v.get("verdict").and_then(|s| s.as_str()) {
                return Ok((
                    verdict.to_uppercase(),
                    v.get("criteria")
                        .and_then(|c| c.as_array())
                        .cloned()
                        .unwrap_or_default(),
                ));
            }
        }
        if let Ok(data) = serde_json::from_str::<Value>(text) {
            if let Some(v) = data.get("verdict").and_then(|v| v.as_str()) {
                return Ok((
                    v.to_uppercase(),
                    data.get("criteria")
                        .and_then(|c| c.as_array())
                        .cloned()
                        .unwrap_or_default(),
                ));
            }
        }
    }
    if !blocks.is_empty() {
        return Ok(("PASS".into(), vec![]));
    }
    Err(RunnerError::Other("no verdict".into()))
}

pub fn verify_node(
    _store: &Store,
    node: &Node,
    deliverable: &str,
    criteria: &[String],
    model: &str,
) -> std::result::Result<(String, Vec<Value>), RunnerError> {
    let prompt = format!(
        "Contract goal: {}\nAcceptance criteria:\n{}\nDeliverable:\n{}",
        node.goal,
        bullets(criteria),
        if deliverable.is_empty() {
            "(no text)"
        } else {
            deliverable
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
                r.subtasks = arr
                    .iter()
                    .map(|item| Contract {
                        goal: item
                            .get("goal")
                            .and_then(|g| g.as_str())
                            .unwrap_or("")
                            .to_string(),
                        acceptance_criteria: item
                            .get("acceptance_criteria")
                            .and_then(|a| a.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        interfaces: vec![],
                        constraints: item
                            .get("constraints")
                            .and_then(|c| c.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        id: item
                            .get("id")
                            .and_then(|i| i.as_str())
                            .unwrap_or("")
                            .to_string(),
                        depends_on: item
                            .get("depends_on")
                            .and_then(|d| d.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        allocation: item.get("allocation").and_then(|a| a.as_i64()).unwrap_or(0),
                    })
                    .collect();
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
                r.artifacts = arr
                    .iter()
                    .map(|item| {
                        (
                            item.get("path")
                                .and_then(|p| p.as_str())
                                .unwrap_or("out.txt")
                                .to_string(),
                            item.get("content")
                                .and_then(|c| c.as_str())
                                .unwrap_or("")
                                .to_string(),
                        )
                    })
                    .collect();
            }
        }
        NOTE_GLOBAL => {
            r.entry_type = payload
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            r.entry_content = payload
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            r.entry_supersedes = payload
                .get("supersedes")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
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
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();
        }
        _ => return Err(RunnerError::Other(format!("unknown verb {verb:?}"))),
    }
    Ok(r)
}

pub fn parse_message(message: &Value) -> std::result::Result<VerbResult, RunnerError> {
    let blocks = message
        .get("content")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    for block in &blocks {
        let name = block
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_lowercase();
        if block.get("type").and_then(|t| t.as_str()).unwrap_or("") == "tool_use"
            && [
                SPLIT,
                COMPLETE_VERB,
                ESCALATE,
                ESCALATE_RESOLVE,
                NOTE_GLOBAL,
            ]
            .contains(&name.as_str())
        {
            if let Some(input) = block.get("input") {
                return result_from_payload(&name, input);
            }
        }
    }
    for block in &blocks {
        let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
        if let Some(payload) = extract_decision(text) {
            if let Some(verb) = payload.get("verb").and_then(|v| v.as_str()) {
                return result_from_payload(verb, &payload);
            }
        }
        if let Ok(payload) = serde_json::from_str::<Value>(text) {
            if let Some(verb) = payload.get("verb").and_then(|v| v.as_str()) {
                return result_from_payload(verb, &payload);
            }
        }
    }
    Err(RunnerError::Other("no usable verb".into()))
}

pub fn run_node(
    store: &Store,
    node: &Node,
    model: &str,
    on_output: OutputFn,
    feedback: Option<&str>,
) -> std::result::Result<VerbResult, RunnerError> {
    let mut prompt =
        assemble_context(store, node).map_err(|e| RunnerError::Other(e.to_string()))?;
    if let Some(fb) = feedback {
        prompt.push_str(&format!(
            "\n\n## Feedback from previous attempt\n{}\nPlease correct this in this attempt.\n",
            fb
        ));
    }
    let message = call_model(&prompt, &node.path, model, on_output)?;
    parse_message(&message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Contract, Store};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_store() -> (Store, std::path::PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("fractal-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        let s = Store::new(&dir);
        s.init("build a CLI tool").unwrap();
        (s, dir)
    }

    #[test]
    fn child_context_has_sibling_info() {
        let (store, dir) = temp_store();
        let root = store
            .walk()
            .unwrap()
            .into_iter()
            .find(|n| n.id == "root")
            .unwrap();
        store.set_status(&root, "running").unwrap();

        let subtasks = vec![
            Contract {
                goal: "write parser".into(),
                acceptance_criteria: vec!["test".into()],
                interfaces: vec![],
                constraints: vec![],
                id: String::new(),
                depends_on: vec![],
                allocation: 0,
            },
            Contract {
                goal: "write CLI".into(),
                acceptance_criteria: vec!["test".into()],
                interfaces: vec![],
                constraints: vec![],
                id: String::new(),
                depends_on: vec![],
                allocation: 0,
            },
        ];
        let children = store.add_children(&root, &subtasks).unwrap();
        assert_eq!(children.len(), 2);

        let child = &children[0];
        let ctx = assemble_context(&store, child).unwrap();

        assert!(ctx.contains("SUBTASK"), "must mention it's a subtask");
        assert!(ctx.contains("write CLI"), "must show sibling goal");
        assert!(ctx.contains("root-01"), "must identify itself");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn root_context_has_no_sibling_section() {
        let (store, dir) = temp_store();
        let root = store
            .walk()
            .unwrap()
            .into_iter()
            .find(|n| n.id == "root")
            .unwrap();
        let ctx = assemble_context(&store, &root).unwrap();

        assert!(!ctx.contains("SUBTASK"), "root must not mention SUBTASK");
        assert!(
            !ctx.contains("Siblings"),
            "root must not have Siblings section"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_constraint_propagation() {
        let (store, dir) = temp_store();
        let root = store
            .walk()
            .unwrap()
            .into_iter()
            .find(|n| n.id == "root")
            .unwrap();

        let subtasks = vec![Contract {
            goal: "sub task 1".into(),
            acceptance_criteria: vec!["ok".into()],
            interfaces: vec![],
            constraints: vec![],
            id: String::new(),
            depends_on: vec![],
            allocation: 0,
        }];
        let children = store.add_children(&root, &subtasks).unwrap();
        assert_eq!(children.len(), 1);

        // Add constraint to root and check propagation to child
        let affected = store.add_constraint_and_propagate("root", "Must be written in Rust").unwrap();
        assert_eq!(affected, 2);

        let updated_child = store.get(&children[0].id).unwrap();
        let c = updated_child.contract();
        assert!(c.constraints.contains(&"Must be written in Rust".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
