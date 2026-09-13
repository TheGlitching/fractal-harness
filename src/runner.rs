use crate::store::{strip_ansi, Contract, Node, Store, StoreError};
use regex::Regex;
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

pub const SPLIT: &str = "split";
pub const COMPLETE_VERB: &str = "complete";
pub const ESCALATE: &str = "escalate";
pub const ESCALATE_RESOLVE: &str = "escalate_resolve";
pub const NOTE_GLOBAL: &str = "note_global";
/// An integrating parent rejecting specific children's work and sending it back
/// down the tree, instead of the parent itself failing after N attempts.
pub const REOPEN: &str = "reopen";

#[derive(Debug)]
pub enum RunnerError {
    Timeout,
    NotFound(String),
    /// Returned when an executor produces no parseable decision. The scheduler
    /// retries on this variant and, after the retry budget, fails the node. It
    /// is never converted into a fabricated `complete`.
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

/// Per-node context bounds. The captain's core mechanism is small context, so
/// every collection injected into a prompt is bounded and the contract's
/// inherited constraints are deduplicated before they are rendered. Without
/// these, prompt size grows with the number of steering/escalation events, with
/// the branching factor, and with the size of dependency output.
const MAX_CONSTRAINTS: usize = 20;
const MAX_CONSTRAINT_BYTES: usize = 300;
const MAX_CONSTRAINT_TOTAL_BYTES: usize = 4_000;
const MAX_DEP_ARTIFACTS: usize = 12;
const MAX_DEP_ARTIFACT_BYTES: usize = 600;
const MAX_DEP_TOTAL_BYTES: usize = 6_000;
const MAX_SIBLINGS: usize = 20;
const MAX_CHILD_SUMMARIES: usize = 20;
const MAX_GLOBAL_ENTRIES: usize = 5;
const MAX_GLOBAL_ENTRY_BYTES: usize = 500;

fn truncate_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}... [truncated]", &s[..cut])
}

/// Collapse whitespace and truncate one constraint to its canonical form.
fn normalize_constraint(c: &str) -> String {
    let norm = c.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&norm, MAX_CONSTRAINT_BYTES)
}

/// Deduplicate inherited constraints by normalized text - a propagated
/// constraint carries no separate origin, so identical text is the same rule -
/// then cap the count and the total bytes. This is what stops a contract's
/// "Inherited constraints" section growing with every steering event.
fn bounded_constraints(raw: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut bytes = 0usize;
    for c in raw {
        let text = normalize_constraint(c);
        if text.is_empty() || !seen.insert(text.to_ascii_lowercase()) {
            continue;
        }
        if out.len() >= MAX_CONSTRAINTS || bytes + text.len() > MAX_CONSTRAINT_TOTAL_BYTES {
            break;
        }
        bytes += text.len();
        out.push(text);
    }
    out
}

fn bounded_contract(contract: &Contract) -> Contract {
    let mut out = contract.clone();
    out.constraints = bounded_constraints(&contract.constraints);
    out
}

pub fn assemble_context(store: &Store, node: &Node) -> std::result::Result<String, StoreError> {
    let mut parts = Vec::new();
    let contract = bounded_contract(&node.contract());
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

    // Expose artifacts from direct dependencies (depends_on) and the unified
    // workspace. Bounded by count and total bytes: a dependency with a large
    // build output must not be able to inflate every consumer's prompt.
    let mut dep_artifacts = Vec::new();
    let all_nodes = store.walk().unwrap_or_default();
    let mut dep_bytes = 0usize;
    'deps: for dep_id in &node.depends_on {
        if let Some(dep_node) = all_nodes.iter().find(|n| n.id == *dep_id) {
            for art in dep_node.find_artifacts() {
                if dep_artifacts.len() >= MAX_DEP_ARTIFACTS || dep_bytes >= MAX_DEP_TOTAL_BYTES {
                    break 'deps;
                }
                if let Ok(rel) = art.strip_prefix(dep_node.artifacts_dir()) {
                    let preview = fs::read_to_string(&art).unwrap_or_default();
                    let head = truncate_chars(&preview, MAX_DEP_ARTIFACT_BYTES);
                    dep_bytes += head.len();
                    dep_artifacts.push(format!(
                        "### Dependency artifact: {} (from {})\n```\n{}\n```\n",
                        rel.display(),
                        dep_node.id,
                        head
                    ));
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
            // The rendered contract already carries every inherited constraint,
            // including the direct parent's. Only a parent constraint added after
            // this child's contract was written is genuinely new; injecting the
            // rest again duplicated the whole block into every child prompt.
            let known: std::collections::HashSet<String> = contract
                .constraints
                .iter()
                .map(|c| normalize_constraint(c).to_ascii_lowercase())
                .collect();
            let extra: Vec<String> = bounded_constraints(&parent.contract().constraints)
                .into_iter()
                .filter(|c| !known.contains(&c.to_ascii_lowercase()))
                .collect();
            if !extra.is_empty() {
                parts.push(format!(
                    "## Direct Parent Constraints (from {})\n{}\n",
                    parent.id,
                    extra
                        .iter()
                        .map(|c| format!("- {c}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
            }

            // Overview of sibling nodes to avoid overlapping splits. Truncated:
            // a wide branch must not linearise every sibling into every prompt.
            let siblings: Vec<&Node> = nodes
                .iter()
                .filter(|n| n.parent.as_deref() == Some(&parent.id) && n.id != node.id)
                .take(MAX_SIBLINGS)
                .collect();
            if !siblings.is_empty() {
                let sib_lines: Vec<String> = siblings
                    .iter()
                    .map(|s| {
                        let goal_first_line = s.goal.lines().next().unwrap_or(&s.goal);
                        format!(
                            "- {} ({}): {}",
                            s.id,
                            s.status,
                            truncate_chars(goal_first_line, 160)
                        )
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
        for c in children.iter().take(MAX_CHILD_SUMMARIES) {
            let sum = c.summary.lines().next().unwrap_or(&c.summary);
            let artifacts = c.find_artifacts();
            let art_str = if artifacts.is_empty() {
                String::new()
            } else {
                format!(
                    " [artifacts: {}]",
                    artifacts
                        .iter()
                        .take(10)
                        .map(|p| p.file_name().unwrap_or_default().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            child_summaries.push(format!(
                "- {} ({}): {}{}",
                c.id,
                c.status,
                truncate_chars(sum, 300),
                art_str
            ));
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
    let global = store
        .retrieve_global(&node.goal, MAX_GLOBAL_ENTRIES)
        .unwrap_or_default();
    if !global.is_empty() {
        let lines: Vec<String> = global
            .iter()
            .map(|e| {
                format!(
                    "- {}: {}",
                    e.entry_type,
                    truncate_chars(&e.content, MAX_GLOBAL_ENTRY_BYTES)
                )
            })
            .collect();
        parts.push(format!("## Global knowledge\n{}\n", lines.join("\n")));
    }

    parts.push(
        "\
## Instructions & Lifecycle Commands

You work directly in the real project repository, with your own tools (write,
edit, read, bash). There is no scratch area and no staging copy: the files you
write ARE the project. Your work is committed for you once verified, so do not
commit yourself.

### If this contract needs decomposition
Run this and STOP - do not write any code:
  fractal split --subtasks '[{\"id\":\"a\",\"goal\":\"...\",\"acceptance_criteria\":[\"...\"],\"verification\":[\"npm test\"],\"manual_verification\":[\"the UI renders cleanly\"]}]'

`verification` entries MUST be commands that run on this machine (`npm test`,
`cargo test`, `python3 -m pytest`). Non-command checks - visual, UX, or whether
something looks right - go in `manual_verification`, which the critic judges and
which are never run as shell commands.

Order the subtasks with `depends_on` so whoever consumes a module runs AFTER the
module it consumes exists. That ordering is what stops two children inventing two
different names for the same thing.

### If you are IMPLEMENTING this contract
You are editing a live codebase, not producing something to be merged later.

1. Read what already exists before writing anything. Use the real types, names,
   taxonomies and helpers this project already defines. Importing an existing
   type is always correct; redefining your own version of it is always wrong.
2. Write code that is connected as you write it: import it where it is used and
   register it where the app expects it. A module nothing imports is unfinished
   work, not finished work.
3. Never write a mock, stub, placeholder, hardcoded sample data or an empty
   component to satisfy a check. If you cannot implement the contract, escalate -
   do not fake it.
4. Never import from tree/ - that is the harness's own memory, not project code.
5. Prove it runs: execute the project's own commands with bash (typecheck, build,
   tests) and fix what they report.
6. Then:
  fractal done --summary \"what you implemented, where it is wired in, and which commands proved it\"

### If you have children: VERIFY ONLY - NEVER FIX
Your only job is to check that your children's work actually functions, and to
send back whatever does not. Do not write or edit project files in this role.
Repairing a child's code yourself would force you to hold every child's context
at once - the very thing this tree exists to prevent - and it hides the defect
from the child that owns it.

1. Run the real verification: `fractal verify`.
2. Look at what was actually produced, never at what was claimed:
     fractal node-diff --stat <child-id>
     fractal integrate-check        (JS/TS projects only)
3. Then do exactly one of:

   a. It works -> report it:
        fractal done --summary \"what your children delivered and what verification proved it\"

   b. Anything is broken, missing, stubbed, mocked, unreferenced, or disagrees
      with a sibling's types or names -> return it to the child that owns that
      code. Your reason text is the only thing that child will see, so state the
      failure and the requirement precisely:
        fractal reopen --children <id> --reason \"<what is broken, and what it must satisfy>\"

   c. A capability nobody was ever asked to build is genuinely absent -> add a
      child for it:
        fractal split --subtasks '[{\"id\":\"...\",\"goal\":\"...\",\"depends_on\":[\"...\"],\"acceptance_criteria\":[\"...\"]}]'

### If an inherited assumption is false
  fractal escalate --assumption \"...\" --evidence \"...\"
"
        .into(),
    );
    Ok(parts.join("\n"))
}

/// Scan `text` for the first balanced JSON object carrying one of `keys`.
///
/// Node decisions are keyed on `verb`; critic verdicts are keyed on `verdict`.
/// Keeping them separate is load-bearing: a critic that answers with a
/// node-style `{"verb":...}` must never be mistaken for a verdict, and a node
/// that answers only with a verdict must be treated as having said nothing.
fn extract_object_with_keys(text: &str, keys: &[&str]) -> Option<Value> {
    let matches_keys = |v: &Value| keys.iter().any(|k| v.get(*k).is_some());
    let trimmed = text.trim();

    // 1. Direct JSON parse if the whole text or trimmed text is already a JSON object
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            if matches_keys(&v) {
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
                    if matches_keys(&v) {
                        return Some(v);
                    }
                }
            }
        }
    }

    // 3. Scan for JSON blocks starting with one of the keys
    let alternation = keys.join("|");
    let re_start = Regex::new(&format!(r#"\{{\s*[\n\s]*"(?:{alternation})""#)).ok();
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
                    if matches_keys(&v) {
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
                if matches_keys(&v) {
                    return Some(v);
                }
            }
        }
    }

    None
}

/// A node's decision: must carry a `verb`, or the `decision` alias a small model
/// commonly emits for the same value. Still fail-closed: an alias that does not
/// resolve to a usable verb is not a decision.
pub fn extract_decision(text: &str) -> Option<Value> {
    extract_object_with_keys(text, &["verb", "decision"])
        .map(normalize_verb_alias)
        .filter(|v| v.get("verb").and_then(|x| x.as_str()).is_some())
}

/// Accept `decision` as an alias for `verb`. The value must be a non-empty
/// string; it is copied into `verb` so every downstream reader sees one key.
/// Still fail-closed: a `decision` that is not a usable string yields no verb.
fn normalize_verb_alias(mut v: Value) -> Value {
    let has_verb = v
        .get("verb")
        .and_then(|x| x.as_str())
        .is_some_and(|s| !s.is_empty());
    if !has_verb {
        if let Some(alias) = v
            .get("decision")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
        {
            v["verb"] = Value::String(alias.to_string());
        }
    }
    v
}

/// A critic's verdict: must carry a `verdict`. A `{"verb":...}` object is not a
/// verdict, so it does not match here and cannot be scored PASS.
pub fn extract_verdict(text: &str) -> Option<Value> {
    extract_object_with_keys(text, &["verdict"])
}

/// M1 mitigation: recover a decision a small model *narrated* instead of
/// executing.
///
/// The recurring failure mode is a leaf doing correct work, then printing a
/// literal `fractal done --summary "..."` line as prose rather than running it.
/// The transcript is not a decision channel, so this only fires when no JSON
/// decision exists, and only on the tail of the output - the prompt itself
/// contains example commands, and a real narrated command is the model's last
/// word, not an instruction. The recovered decision then passes through the
/// identical diff, gate and critic checks: this widens the channel a decision
/// can arrive on, it does not weaken what counts as delivery.
pub fn transcript_command_decision(text: &str, project_root: &Path) -> Option<Value> {
    // Last matching line wins: a model that narrates its command narrates it at
    // the end, and an earlier mention (if any) is superseded by it.
    for line in text.lines().rev().take(20) {
        if let Some(v) = command_line_to_decision(line, project_root) {
            return Some(v);
        }
    }
    None
}

fn command_line_to_decision(line: &str, project_root: &Path) -> Option<Value> {
    let mut s = line.trim().trim_start_matches(['`', '*', '>', '-', ' ']);
    if let Some(rest) = s.strip_prefix('$') {
        s = rest.trim_start();
    }
    // Tolerate opencode's tool framing wrapping the command.
    if s.starts_with('<') {
        if let Some(gt) = s.find('>') {
            s = s[gt + 1..].trim_start();
        }
    }
    if let Some(rest) = s.strip_prefix("fractal done") {
        return done_command_decision(rest);
    }
    if let Some(rest) = s.strip_prefix("fractal split") {
        return split_command_decision(rest, project_root);
    }
    None
}

/// `fractal done --summary "..."` (or a bare positional summary) -> `complete`.
/// A bare `fractal done` is still an explicit completion, so it recovers with a
/// placeholder summary instead of being discarded and burning a retry; the
/// empty-diff and critic checks still decide whether anything was delivered.
fn done_command_decision(rest: &str) -> Option<Value> {
    let summary = flag_value(rest, &["--summary", "-s"])
        .unwrap_or_else(|| command_value(rest))
        .trim()
        .to_string();
    let summary = if summary.is_empty() {
        "completed".to_string()
    } else {
        summary
    };
    Some(serde_json::json!({
        "verb": COMPLETE_VERB,
        "summary": summary,
        "deliverable": summary,
    }))
}

/// `fractal split --subtasks '[...]'` (inline JSON, `@file` or a path) -> `split`.
fn split_command_decision(rest: &str, project_root: &Path) -> Option<Value> {
    let raw = flag_value(rest, &["--subtasks", "-s"]).or_else(|| {
        let candidate = command_value(rest);
        if candidate.trim().is_empty() {
            None
        } else {
            Some(candidate)
        }
    })?;
    let raw = raw.trim();
    let json = if raw.starts_with('[') || raw.starts_with('{') {
        raw.to_string()
    } else {
        let path = raw.trim_start_matches('@').trim();
        fs::read_to_string(project_root.join(path)).ok()?
    };
    let value: Value = serde_json::from_str(&json).ok()?;
    let subtasks = if value.is_array() {
        value
    } else {
        value.get("subtasks")?.clone()
    };
    Some(serde_json::json!({ "verb": SPLIT, "subtasks": subtasks }))
}

/// Locate one `--flag` and return the shell-like value that follows it.
fn flag_value(s: &str, flags: &[&str]) -> Option<String> {
    for flag in flags {
        let mut search_start = 0;
        while let Some(pos) = s[search_start..].find(flag) {
            let abs = search_start + pos;
            let before_ok = abs == 0
                || s.as_bytes()
                    .get(abs - 1)
                    .is_some_and(|b| b.is_ascii_whitespace());
            let after = abs + flag.len();
            let after_ok = s[after..].chars().next().is_some_and(char::is_whitespace);
            if before_ok && after_ok {
                return Some(command_value(&s[after..]));
            }
            search_start = after;
        }
    }
    None
}

/// Extract a shell value: `\"...\"`, `"..."`, `'...'`, or the bare remainder.
/// The trial's real narration ended in `fractal done --summary \"...\"</arg_value>`,
/// so escaped quotes and trailing tool framing must both be tolerated. The
/// delimiter is chosen by the value's opening character, not by the first quote
/// anywhere - a single-quoted subtasks JSON is full of double quotes.
fn command_value(raw: &str) -> String {
    let raw = raw.trim();
    let raw = raw
        .trim_end_matches("</arg_value>")
        .trim_end_matches("</argument>")
        .trim_end();
    let delim = if raw.starts_with("\\\"") {
        Some("\\\"")
    } else if raw.starts_with('"') {
        Some("\"")
    } else if raw.starts_with('\'') {
        Some("'")
    } else {
        None
    };
    if let Some(delim) = delim {
        let after = delim.len();
        if let Some(last_rel) = raw[after..].rfind(delim) {
            let inner = &raw[after..after + last_rel];
            return inner.replace("\\\"", "\"").replace("\\\\", "\\");
        }
    }
    raw.to_string()
}

#[derive(Default, Debug)]
pub struct VerbResult {
    pub verb: String,
    pub subtasks: Vec<Contract>,
    pub deliverable: String,
    pub summary: String,
    pub artifacts: Vec<(String, String)>,
    pub assumption: String,
    /// Child ids an integrating parent is sending back for rework.
    pub reopen_children: Vec<String>,
    /// Why they are being sent back; becomes their retry feedback.
    pub reopen_reason: String,
    pub evidence: String,
    /// Settlement an owner returns for an escalation: `amend`, `overrule`,
    /// `replan`, or `depends_on`.
    pub resolution: String,
    /// `amend`: the rewritten inherited constraint.
    pub amended_constraint: String,
    /// `amend`: an interface to expose on a sibling.
    pub amended_interface: String,
    /// `overrule`: the rationale the escalating child must address.
    pub rationale: String,
    /// `amend`/`depends_on`: the sibling the resolution names.
    pub target: String,
    /// `depends_on`: the sibling the escalating node must depend on.
    pub dependency: String,
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

/// Run one node's agent. `work_root` is the directory the agent edits and the
/// orchestrator diffs, verifies and commits: the shared project tree when nodes
/// run one at a time, or the node's own worktree when a ready batch runs
/// concurrently. The node's memory (`contract.md`, `decisions.md`, `log/`) stays
/// in the shared store and is referenced by absolute path, so the audit trail is
/// never split across worktrees.
pub fn run_node(
    store: &Store,
    node: &Node,
    model: &str,
    on_output: OutputFn,
    feedback: Option<&str>,
    work_root: &Path,
) -> std::result::Result<VerbResult, RunnerError> {
    let mut prompt =
        assemble_context(store, node).map_err(|e| RunnerError::Other(e.to_string()))?;
    if let Some(fb) = feedback {
        prompt.push_str(&format!("\n\n## Feedback from previous attempt\n{fb}\n"));
    }

    let executor = get_executor();
    let project_root = work_root;
    match executor.as_str() {
        "omp" | "pi" => call_via_omp(
            &prompt,
            node.path.as_path(),
            project_root,
            model,
            store,
            node,
            on_output,
        ),
        "opencode" => call_via_opencode(
            &prompt,
            node.path.as_path(),
            project_root,
            model,
            store,
            node,
            on_output,
        ),
        other => Err(RunnerError::NotFound(format!(
            "unknown executor {other:?}; supported executors are omp, pi and opencode"
        ))),
    }
}

/// Estimate a model call's token cost. `FRACTAL_CALL_TOKENS` overrides the
/// heuristic when the provider's own accounting is unavailable or a project
/// wants to calibrate spend; otherwise it is a rough four-characters-per-token
/// estimate of the prompt plus the output.
fn estimate_call_tokens(text_len: usize) -> i64 {
    if let Ok(v) = std::env::var("FRACTAL_CALL_TOKENS") {
        if let Ok(n) = v.parse::<i64>() {
            if n > 0 {
                return n;
            }
        }
    }
    ((text_len / 4).max(1)) as i64
}

fn charge_call(store: &Store, node: &Node, prompt_len: usize, output_len: usize) {
    let tokens = estimate_call_tokens(prompt_len + output_len);
    let _ = store.debit_call(node, tokens);
}

/// How a streamed executor process ended. The collected output is returned in
/// every case so the caller can still read a decision (and charge for the call)
/// even when the process had to be killed.
enum StreamEnd {
    Exited,
    TimedOut,
    Interrupted,
}

/// Spawn already done; stream the child's stdout/stderr to `on_output` while
/// polling its exit, the timeout, and the interrupt flag. On timeout or
/// interrupt the child is killed and reaped before returning, so no executor is
/// ever left running after the harness stops.
fn collect_stream(
    mut child: std::process::Child,
    node_name: &str,
    on_output: &OutputFn,
    timeout_secs: u64,
) -> std::result::Result<(String, String, StreamEnd), RunnerError> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| RunnerError::Other("executor stdout unavailable".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| RunnerError::Other("executor stderr unavailable".into()))?;
    let (std_tx, std_rx) = std::sync::mpsc::channel::<(bool, String, String)>();
    let std_tx_err = std_tx.clone();

    let node_name_out = node_name.to_string();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            let line = strip_ansi(&line);
            if line.trim().is_empty() {
                continue;
            }
            let log_line = format!(" [{}] {}", node_name_out, line);
            let _ = std_tx.send((false, log_line, line));
        }
    });
    let node_name_err = node_name.to_string();
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            let line = strip_ansi(&line);
            if line.trim().is_empty() {
                continue;
            }
            let log_line = format!(" [{}] ERR: {}", node_name_err, line);
            let _ = std_tx_err.send((true, log_line, line));
        }
    });

    let mut stdout_lines = Vec::new();
    let mut stderr_lines = Vec::new();
    let drain = |stdout_lines: &mut Vec<String>, stderr_lines: &mut Vec<String>| {
        while let Ok((is_err, log_line, raw_line)) = std_rx.try_recv() {
            on_output(&log_line);
            if is_err {
                stderr_lines.push(raw_line);
            } else {
                stdout_lines.push(raw_line);
            }
        }
    };

    let start = std::time::Instant::now();
    let end = loop {
        drain(&mut stdout_lines, &mut stderr_lines);
        if crate::scheduler::INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            drain(&mut stdout_lines, &mut stderr_lines);
            break StreamEnd::Interrupted;
        }
        if start.elapsed().as_secs() > timeout_secs {
            let _ = child.kill();
            let _ = child.wait();
            drain(&mut stdout_lines, &mut stderr_lines);
            break StreamEnd::TimedOut;
        }
        match child.try_wait() {
            Ok(Some(_)) => break StreamEnd::Exited,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(e) => return Err(RunnerError::Other(format!("wait: {e}"))),
        }
    };
    drain(&mut stdout_lines, &mut stderr_lines);
    Ok((stdout_lines.join("\n"), stderr_lines.join("\n"), end))
}

/// Read a decision from the executor's decision file or its output, charge the
/// call to the node's ledger, and fail closed when no decision was produced.
fn decide(
    store: &Store,
    node: &Node,
    prompt_len: usize,
    all_text: &str,
    decision_file: &Path,
) -> std::result::Result<VerbResult, RunnerError> {
    let val = if decision_file.exists() {
        let content = fs::read_to_string(decision_file).unwrap_or_default();
        let _ = fs::remove_file(decision_file);
        serde_json::from_str::<Value>(&content).ok()
    } else {
        None
    }
    .map(normalize_verb_alias)
    .filter(|v| v.get("verb").and_then(|v| v.as_str()).is_some())
    .or_else(|| extract_decision(all_text))
    .or_else(|| {
        // M1: a small model may narrate its completion command as prose. Only
        // consulted when no JSON decision exists, and logged so the recovery is
        // visible rather than silent.
        let project_root = decision_file.parent().unwrap_or_else(|| Path::new("."));
        let recovered = transcript_command_decision(all_text, project_root);
        if let Some(v) = &recovered {
            store
                .append_log(
                    node,
                    &serde_json::json!({
                        "event": "decision_from_transcript",
                        "verb": v.get("verb").and_then(|x| x.as_str()).unwrap_or(""),
                    }),
                )
                .ok();
        }
        recovered
    });

    // Charge before deciding: the model was called whether or not it answered.
    charge_call(store, node, prompt_len, all_text.len());

    // Fail closed. An executor that died, rambled, or answered with something
    // that is not a decision has not delivered anything; fabricating a
    // `complete` here is how a silent agent was scored as success.
    let val = val.ok_or_else(|| {
        let tail: String = {
            let chars: Vec<char> = all_text.chars().collect();
            chars[chars.len().saturating_sub(600)..].iter().collect()
        };
        RunnerError::NoDecision(format!(
            "executor produced no parseable decision; last output was:\n{tail}"
        ))
    })?;

    let verb = val
        .get("verb")
        .and_then(|v| v.as_str())
        .ok_or_else(|| RunnerError::NoDecision("decision had no verb".into()))?;
    result_from_payload(verb, &val)
}

pub fn call_via_omp(
    prompt: &str,
    node_path: &Path,
    project_root: &Path,
    model: &str,
    store: &Store,
    node: &Node,
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

    let child = cmd
        .spawn()
        .map_err(|e| RunnerError::Other(format!("spawn: {e}")))?;
    let timeout_secs = std::env::var("FRACTAL_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    let (all_text, _stderr, end) = collect_stream(child, &node_name, &on_output, timeout_secs)?;
    let decision_file = project_root.join(format!(".fractal_decision_{}", node_name));
    let result = decide(store, node, claude_md.len(), &all_text, &decision_file);
    match end {
        StreamEnd::Exited => result,
        StreamEnd::TimedOut => Err(RunnerError::Timeout),
        StreamEnd::Interrupted => Err(RunnerError::Other("interrupted by user".into())),
    }
}

/// Run the real `opencode` CLI headlessly in the project, with its own model
/// selection and permission flags. Nothing about `omp` is reused here: the
/// command, flags and config are opencode's.
pub fn call_via_opencode(
    prompt: &str,
    node_path: &Path,
    project_root: &Path,
    model: &str,
    store: &Store,
    node: &Node,
    on_output: OutputFn,
) -> std::result::Result<VerbResult, RunnerError> {
    let claude_md = format!("{OP_SYSTEM}\n\n{prompt}");
    let claude_path = node_path.join("CLAUDE.md");
    fs::write(&claude_path, &claude_md).map_err(|e| RunnerError::Other(format!("write: {e}")))?;

    let bin = which::which("opencode")
        .map_err(|_| RunnerError::NotFound("opencode binary not found".into()))?;
    let node_name = node_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut cmd = Command::new(&bin);
    cmd.arg("run");
    cmd.arg("--auto");
    cmd.current_dir(project_root);
    cmd.env("FRACTAL_NODE_ID", &node_name);
    if !model.is_empty() && model != "default" {
        cmd.arg("--model").arg(model);
    }
    // opencode reads CLAUDE.md from its working directory; the project root is
    // the working directory, so the node's own file is referenced explicitly.
    if std::env::var_os("OPENCODE_CONFIG_CONTENT").is_none() {
        cmd.env("OPENCODE_CONFIG_CONTENT", r#"{"permission":{"*":"allow"}}"#);
    }
    let node_instructions = format!("Read @{}. You are a node in a fractal task tree. Use your tools directly in the project. Signal completion with `fractal done` or split with `fractal split`.", claude_path.display());
    cmd.arg(node_instructions);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let child = cmd
        .spawn()
        .map_err(|e| RunnerError::Other(format!("spawn: {e}")))?;
    let timeout_secs = std::env::var("FRACTAL_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    let (all_text, _stderr, end) = collect_stream(child, &node_name, &on_output, timeout_secs)?;
    let decision_file = project_root.join(format!(".fractal_decision_{}", node_name));
    let result = decide(store, node, claude_md.len(), &all_text, &decision_file);
    match end {
        StreamEnd::Exited => result,
        StreamEnd::TimedOut => Err(RunnerError::Timeout),
        StreamEnd::Interrupted => Err(RunnerError::Other("interrupted by user".into())),
    }
}

/// Run an executor session that is not a tree node: the butler. Same executor
/// path (omp/pi/opencode), streaming and timeout as a node, but the agent's
/// output is returned as text rather than parsed as a node decision and it is
/// not charged to any node ledger. `FRACTAL_BIN` points the agent at the running
/// binary so it can call the butler tools from its shell.
pub fn run_butler_agent(
    system_prompt: &str,
    agent_dir: &Path,
    project_root: &Path,
    model: &str,
    on_output: OutputFn,
) -> std::result::Result<String, RunnerError> {
    fs::create_dir_all(agent_dir).map_err(|e| RunnerError::Other(format!("write: {e}")))?;
    let claude_path = agent_dir.join("CLAUDE.md");
    fs::write(&claude_path, system_prompt)
        .map_err(|e| RunnerError::Other(format!("write: {e}")))?;

    let executor = get_executor();
    let mut cmd = match executor.as_str() {
        "opencode" => {
            let bin = which::which("opencode")
                .map_err(|_| RunnerError::NotFound("opencode binary not found".into()))?;
            let mut c = Command::new(bin);
            c.arg("run").arg("--auto").current_dir(project_root);
            if !model.is_empty() && model != "default" {
                c.arg("--model").arg(model);
            }
            if std::env::var_os("OPENCODE_CONFIG_CONTENT").is_none() {
                c.env("OPENCODE_CONFIG_CONTENT", r#"{"permission":{"*":"allow"}}"#);
            }
            c
        }
        "omp" | "pi" => {
            let bin = which::which("omp")
                .or_else(|_| which::which("pi"))
                .map_err(|_| RunnerError::NotFound("omp/pi binary not found".into()))?;
            let mut c = Command::new(bin);
            c.arg("-p")
                .arg("--cwd")
                .arg(project_root)
                .arg("--auto-approve")
                .arg("--approval-mode=yolo");
            if !model.is_empty() && model != "default" {
                c.arg(format!("--model={model}"));
            }
            c
        }
        other => {
            return Err(RunnerError::NotFound(format!(
                "unknown executor {other:?}; supported executors are omp, pi and opencode"
            )))
        }
    };

    // The butler steers through `fractal` subcommands; point it at the running
    // binary so the tools work even when `fractal` is not on PATH (tests), and
    // carry the resolved model so a nested `resume` tool uses the same one.
    cmd.env("FRACTAL_BIN", std::env::current_exe().unwrap_or_default())
        .env("FRACTAL_NODE_ID", "butler")
        .env("FRACTAL_MODEL", model)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.arg(system_prompt);

    let child = cmd
        .spawn()
        .map_err(|e| RunnerError::Other(format!("spawn: {e}")))?;
    let timeout_secs = std::env::var("FRACTAL_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    let (all_text, _stderr, end) = collect_stream(child, "butler", &on_output, timeout_secs)?;
    match end {
        StreamEnd::Exited => Ok(all_text),
        StreamEnd::TimedOut => Err(RunnerError::Timeout),
        StreamEnd::Interrupted => Err(RunnerError::Other("interrupted by user".into())),
    }
}

pub fn call_critic(
    store: &Store,
    node: &Node,
    prompt: &str,
    model: &str,
) -> std::result::Result<Value, RunnerError> {
    let temp_dir = std::env::temp_dir().join(format!("fractal_critic_{}", std::process::id()));
    let _ = fs::create_dir_all(&temp_dir);
    let claude_md = format!("{CRITIC_SYSTEM}\n\nReview against acceptance criteria. Output JSON verdict with PASS or FAIL.\n\n{prompt}");
    let _ = fs::write(temp_dir.join("CLAUDE.md"), &claude_md);

    let executor = get_executor();
    let text = if executor == "opencode" {
        let bin = which::which("opencode")
            .map_err(|_| RunnerError::NotFound("opencode binary not found".into()))?;
        let mut cmd = Command::new(&bin);
        cmd.arg("run").arg("--auto");
        cmd.current_dir(&temp_dir);
        // opencode resolves its project - and therefore which CLAUDE.md it loads
        // - from the inherited `PWD`, not the process cwd. Leaving `PWD` at the
        // project root made the critic read no criteria at all and invent them
        // from the repository. Pass the whole prompt inline and point `PWD` at
        // the same temp dir so delivery cannot depend on that resolution.
        cmd.env("PWD", &temp_dir);
        if !model.is_empty() && model != "default" {
            cmd.arg("--model").arg(model);
        }
        if std::env::var_os("OPENCODE_CONFIG_CONTENT").is_none() {
            cmd.env("OPENCODE_CONFIG_CONTENT", r#"{"permission":{"*":"allow"}}"#);
        }
        cmd.arg(&claude_md);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let output = cmd
            .output()
            .map_err(|e| RunnerError::Other(format!("critic: {e}")))?;
        format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    } else {
        let bin = which::which("omp")
            .or_else(|_| which::which("pi"))
            .map_err(|_| RunnerError::NotFound("omp/pi binary not found".into()))?;
        let mut cmd = Command::new(&bin);
        cmd.arg("-p");
        cmd.arg("--cwd").arg(&temp_dir);
        if !model.is_empty() && model != "default" {
            cmd.arg(format!("--model={model}"));
        }
        cmd.arg(&claude_md);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let output = cmd
            .output()
            .map_err(|e| RunnerError::Other(format!("critic: {e}")))?;
        format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    };
    let _ = fs::remove_dir_all(&temp_dir);
    // Verification is a model call too; charge it to the node's ledger.
    charge_call(store, node, claude_md.len(), text.len());
    // Fail closed, and require a real verdict. An unreadable response, a missing
    // verdict, or a node-shaped `{"verb":...}` object is not evidence of success.
    let decision = extract_verdict(&text).unwrap_or_else(|| {
        serde_json::json!({
            "verdict": "FAIL",
            "reason": "Critic produced no readable verdict; treating as not verified",
            "criteria": []
        })
    });

    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": serde_json::to_string(&decision).unwrap_or_default() }]
    }))
}

const CRITIC_SYSTEM: &str = "\
You are a strict, adversarial verifier judging whether a contract was actually \
fulfilled.

The 'Actual code change' section is the git diff of this work. It is ground truth; \
the deliverable summary is only a claim. Where they disagree, believe the diff.

The 'Automated gates' section is the harness's own execution of the project's \
commands against the files on disk. Each entry names the command, its outcome and \
the captured output. A PASS there is ground truth: a criterion that a passing \
gate describes is satisfied by that gate, and must not be marked not-verified \
merely because the file it depends on is absent from this node's diff. The \
'Project files' section is a bounded snapshot of the tree as it stands, including \
manifests and files committed by earlier nodes; it is evidence too.

The harness caps how much evidence it can show for length. When it does, the cut is \
marked explicitly and the omitted text was removed by the harness, not by the node: \
never treat a preview that ends as the node shipping truncated work. \
But a cut is not evidence either. If the visible evidence does not let you judge a \
criterion, do not PASS that criterion on the strength of the deliverable summary - \
mark it not-passed and say plainly what you could not see. Fail closed on unseen \
evidence; the missing bytes are the harness's limit, not the node's failure.

Judge only the evidence supplied in this prompt. Do not search the filesystem, do not \
run the project's commands, and do not look for the project elsewhere on disk: you are \
given a sandbox with no project access by design, and files found outside it are not \
the evidence this verdict is accountable to. If the supplied evidence cannot settle a \
criterion, say so and fail closed rather than going to find more.

FAIL if the diff is empty for a node that was supposed to implement something.
FAIL if the diff adds a stub, placeholder, mock, hardcoded sample data, or a \
component that renders nothing, whatever the summary claims.
FAIL if the diff adds a module nothing imports, when the contract required \
working behaviour.
For a node whose children already produced and verified the work, judge those \
verified child results against the acceptance criteria instead.
Judge ONLY the acceptance criteria listed above, which belong to this node. Do \
not invent criteria and do not judge another node's work: the criteria of a \
parent or a sibling are not yours to grade. In the JSON, copy each acceptance \
criterion's text verbatim into its `name`.

Output ONLY a JSON object:
{\"verdict\": \"PASS\" | \"FAIL\", \"reason\": \"...\", \"criteria\": [{\"name\": \"...\", \"pass\": true | false, \"reason\": \"...\"}]}
";

/// Total bytes of automated evidence handed to the critic. The diff, the gate
/// results and the project-file snapshot share it, so no one section can crowd
/// the others out of the prompt.
const CRITIC_EVIDENCE_BUDGET: usize = 12_000;
/// Share of the budget reserved for the project-file snapshot.
const CRITIC_SNAPSHOT_BUDGET: usize = 4_000;

#[allow(clippy::too_many_arguments)]
pub fn verify_node(
    store: &Store,
    node: &Node,
    deliverable: &str,
    artifacts: &[(String, String)],
    criteria: &[String],
    gates: &[crate::verify::GateOutcome],
    model: &str,
    work_root: &Path,
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
            let names: Vec<_> = art_list
                .iter()
                .filter_map(|p| p.file_name())
                .map(|f| f.to_string_lossy())
                .collect();
            children_summary.push_str(&format!(
                "- Subtask {} ({}): {} [Artifacts: {}]\n",
                c.id,
                c.status,
                c.summary,
                names.join(", ")
            ));
        }
    }

    // The strongest available evidence is what changed on disk. At verification
    // time the node's work is still uncommitted, so the working tree - including
    // brand-new untracked files - is precisely this node's contribution, not its
    // description of itself. Judging prose alone is how a node that never
    // created a file was passed as complete.
    //
    // Gates and project files are evidence a diff alone cannot carry: a passing
    // gate settles a criterion even when the file that criterion is about was
    // committed by an earlier node and is absent from this diff.
    let gate_commands: Vec<String> = gates.iter().map(|g| g.command.clone()).collect();
    let gate_evidence = crate::verify::format_gate_evidence(gates);
    let project_snapshot = crate::git::project_evidence_snapshot(
        work_root,
        criteria,
        &gate_commands,
        CRITIC_SNAPSHOT_BUDGET,
    );
    let diff_budget = CRITIC_EVIDENCE_BUDGET
        .saturating_sub(project_snapshot.len())
        .saturating_sub(gate_evidence.len())
        .max(2_000);
    let changed_on_disk = crate::git::has_uncommitted_changes(work_root);
    let code_evidence = if !changed_on_disk {
        if !children.is_empty() {
            "(no direct change; this node aggregates the verified children above)".to_string()
        } else {
            "(NO FILE CHANGED - this node modified nothing on disk)".to_string()
        }
    } else {
        format!(
            "Working-tree change (git):\n{}",
            crate::git::worktree_change_summary(work_root, diff_budget)
        )
    };

    let prompt = format!(
        "Contract goal: {}\nAcceptance criteria:\n{}\nDeliverable summary:\n{}\n{}\nActual code change:\n{}{}{}{}",
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
        code_evidence,
        if gate_evidence.is_empty() {
            String::new()
        } else {
            format!("\n\nAutomated gates (run by the harness):\n{gate_evidence}")
        },
        if project_snapshot.is_empty() {
            String::new()
        } else {
            format!(
                "\n\nProject files (current state, including files committed before this node):\n{project_snapshot}"
            )
        },
        if artifact_summary.is_empty() {
            String::new()
        } else {
            format!("\n\nDeclared artifacts:\n{artifact_summary}")
        }
    );
    let message = call_critic(store, node, &prompt, model)?;
    let (raw_verdict, raw_reason, raw_details) = parse_verdict_full(&message)?;
    // Reject a verdict that grades criteria belonging to another node. It is
    // retried with feedback rather than trusted, so a critic that never saw the
    // node's criteria cannot fail a correct leaf for its siblings' absent work.
    let (verdict, reason, details) = if verdict_matches_criteria(criteria, &raw_details) {
        (raw_verdict, raw_reason, raw_details)
    } else {
        let scope_reason = "the verdict did not address this node's own acceptance criteria; \
                            grade only the criteria listed for this node"
            .to_string();
        (
            "FAIL".to_string(),
            scope_reason.clone(),
            vec![
                serde_json::json!({"name": "verdict scope", "pass": false, "reason": scope_reason}),
            ],
        )
    };
    // Persist a rejection into the node's own log and decisions. Without this a
    // run that dies on critic judgement can only be explained by querying the
    // session database, which is exactly how trial 3's H2 stayed invisible.
    if verdict != "PASS" {
        store
            .append_log(
                node,
                &serde_json::json!({
                    "event": "critic_rejected",
                    "verdict": verdict,
                    "reason": reason,
                    "criteria": details,
                }),
            )
            .ok();
        store
            .append_decision(
                node,
                &format!(
                    "critic rejected (verdict={verdict}): {}",
                    reason.replace('\n', " ")
                ),
            )
            .ok();
    }
    Ok((verdict, details))
}

/// Parse a `split` decision's subtask array into contracts. Shared by node
/// decisions and the butler's `split` tool so both build identical contracts
/// from the same JSON shape.
pub fn contracts_from_subtasks(items: &[Value]) -> Vec<Contract> {
    let strings = |item: &Value, key: &str| -> Vec<String> {
        item.get(key)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    };
    items
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
            id: item
                .get("id")
                .and_then(|g| g.as_str())
                .unwrap_or("")
                .to_string(),
            interfaces: strings(item, "interfaces"),
            constraints: strings(item, "constraints"),
            depends_on: item
                .get("depends_on")
                .and_then(|d| d.as_array())
                .map(|d| {
                    d.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default(),
            verification: strings(item, "verification"),
            manual_verification: strings(item, "manual_verification"),
            allocation: item
                .get("allocation")
                .and_then(|a| a.as_i64())
                .unwrap_or(0)
                .max(0),
        })
        .collect()
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
                r.subtasks = contracts_from_subtasks(arr);
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
            r.amended_constraint = payload
                .get("amended_constraint")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            r.amended_interface = payload
                .get("amended_interface")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            r.rationale = payload
                .get("rationale")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            r.target = payload
                .get("target")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            r.dependency = payload
                .get("dependency")
                .and_then(|v| v.as_str())
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
        REOPEN => {
            r.reopen_children = payload
                .get("children")
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            r.reopen_reason = payload
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();
        }
        _ => {}
    }
    Ok(r)
}

/// Lowercase alphanumeric fingerprint of a criterion, so punctuation and case
/// differences do not hide a verbatim copy.
fn normalize_criterion(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Words that carry no requirement and would inflate a token-overlap score.
const CRITERION_STOPWORDS: [&str; 24] = [
    "the", "a", "an", "and", "or", "of", "to", "is", "are", "be", "for", "in", "on", "with",
    "that", "this", "it", "as", "at", "by", "from", "must", "should", "all",
];

/// Significant normalized tokens: short function words dropped so a paraphrase
/// is judged on its content words, not on shared filler.
fn criterion_tokens(text: &str) -> Vec<&str> {
    text.split_whitespace()
        .filter(|t| t.len() > 2 && !CRITERION_STOPWORDS.contains(t))
        .collect()
}

/// Fraction of the smaller criterion's significant tokens that appear in the
/// other. 1.0 when one is a subset of the other, so a critic that renames a
/// criterion while keeping its meaning still matches.
fn token_overlap(a: &str, b: &str) -> f64 {
    let ta = criterion_tokens(a);
    let tb = criterion_tokens(b);
    let (small, large) = if ta.len() <= tb.len() {
        (&ta, &tb)
    } else {
        (&tb, &ta)
    };
    if small.is_empty() || large.is_empty() {
        return 0.0;
    }
    let shared = small.iter().filter(|t| large.contains(t)).count();
    shared as f64 / small.len() as f64
}

/// True when two criteria refer to the same requirement: an exact match after
/// normalization, one containing the other (the critic may append its own
/// parenthetical detail to a copied criterion), or a paraphrase sharing most of
/// one criterion's significant words.
fn criterion_matches(returned: &str, criterion: &str) -> bool {
    if returned.is_empty() || criterion.is_empty() {
        return false;
    }
    returned == criterion
        || returned.contains(criterion)
        || criterion.contains(returned)
        || token_overlap(returned, criterion) >= 0.6
}

/// A verdict is only evidence about *this* node when it grades this node's own
/// acceptance criteria. A critic that never received them (the D2 defect) or
/// that reached into a parent's or sibling's contract invents foreign criteria;
/// those must be rejected and retried rather than scored.
fn verdict_matches_criteria(criteria: &[String], details: &[Value]) -> bool {
    if criteria.is_empty() {
        return true;
    }
    let names: Vec<String> = details
        .iter()
        .filter_map(|c| c.get("name").and_then(|n| n.as_str()))
        .map(normalize_criterion)
        .filter(|n| !n.is_empty())
        .collect();
    if names.is_empty() {
        return false;
    }
    let own: Vec<String> = criteria.iter().map(|c| normalize_criterion(c)).collect();
    // No criterion belongs to someone else.
    let foreign_free = names
        .iter()
        .all(|n| own.iter().any(|c| criterion_matches(n, c)));
    // Every one of this node's criteria is actually addressed.
    let all_addressed = own
        .iter()
        .all(|c| names.iter().any(|n| criterion_matches(n, c)));
    foreign_free && all_addressed
}

#[cfg(test)]
fn parse_verdict(msg: &Value) -> std::result::Result<(String, Vec<Value>), RunnerError> {
    parse_verdict_full(msg).map(|(verdict, _reason, details)| (verdict, details))
}

/// As `parse_verdict`, but also surfaces the critic's top-level reason. The
/// rejection reason is persisted to the node's own artifacts, so callers need it.
fn parse_verdict_full(
    msg: &Value,
) -> std::result::Result<(String, String, Vec<Value>), RunnerError> {
    let content = msg
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("");
    let d = extract_verdict(content)
        .ok_or_else(|| RunnerError::Other(format!("critic returned no verdict: {content}")))?;
    let mut verdict = d
        .get("verdict")
        .and_then(|v| v.as_str())
        .map(|v| v.trim().to_ascii_uppercase())
        .unwrap_or_else(|| "FAIL".to_string());
    let reason = d
        .get("reason")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();
    let details: Vec<Value> = d
        .get("criteria")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    // An explicit PASS must carry per-criterion results. A bare PASS with
    // nothing checked is not evidence that anything was verified.
    if verdict == "PASS" && details.is_empty() {
        verdict = "FAIL".to_string();
    }
    Ok((verdict, reason, details))
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

    /// M1: a small model that emits `decision` instead of `verb` made the same
    /// completion declaration. It must recover, with the value copied so every
    /// downstream reader still sees `verb`.
    #[test]
    fn decision_is_recovered_as_an_alias_for_verb() {
        let text = r#"All done:
{"decision":"complete","summary":"all children verified","deliverable":"the app"}"#;
        let val = extract_decision(text).expect("decision alias must be recovered");
        assert_eq!(val.get("verb").unwrap(), "complete");
        assert_eq!(val.get("summary").unwrap(), "all children verified");
    }

    /// Fail closed: a `decision` key whose value is not a usable string is not a
    /// decision, however decision-shaped the object looks.
    #[test]
    fn a_non_string_decision_alias_is_not_recovered() {
        assert!(extract_decision(r#"{"decision":{"verb":"complete"}}"#).is_none());
        assert!(extract_decision(r#"{"decision":""}"#).is_none());
    }

    /// A node decision is not a verdict. A critic answering with `{"verb":...}`
    /// must not be scored PASS.
    #[test]
    fn verdict_extraction_rejects_node_decisions() {
        assert!(extract_verdict(r#"{"verb":"complete","summary":"done"}"#).is_none());
        assert!(extract_verdict("no json here at all").is_none());
        assert!(extract_verdict(r#"{"verdict":"PASS","criteria":[]}"#).is_some());
    }

    fn critic_message(text: &str) -> Value {
        serde_json::json!({
            "content": [{ "type": "text", "text": text }]
        })
    }

    #[test]
    fn missing_verdict_fails_closed() {
        let msg = critic_message("I think it is probably fine.");
        let not_pass = parse_verdict(&msg)
            .map(|(verdict, _)| verdict != "PASS")
            .unwrap_or(true);
        assert!(not_pass, "a missing verdict must not default to PASS");
    }

    #[test]
    fn verb_shaped_critic_object_is_not_a_pass() {
        let msg = critic_message(r#"{"verb":"complete","summary":"done"}"#);
        let not_pass = parse_verdict(&msg)
            .map(|(verdict, _)| verdict != "PASS")
            .unwrap_or(true);
        assert!(not_pass, "a node-shaped object must not be scored PASS");
    }

    #[test]
    fn pass_requires_per_criterion_results() {
        let bare = critic_message(r#"{"verdict":"PASS","reason":"looks good","criteria":[]}"#);
        let (verdict, _) = parse_verdict(&bare).unwrap();
        assert_ne!(
            verdict, "PASS",
            "a bare PASS with no criteria is not evidence"
        );

        let real = critic_message(
            r#"{"verdict":"PASS","reason":"ok","criteria":[{"name":"builds","pass":true,"reason":"exit 0"}]}"#,
        );
        let (verdict, details) = parse_verdict(&real).unwrap();
        assert_eq!(verdict, "PASS");
        assert_eq!(details.len(), 1);
    }

    /// The D2 regression: a critic that never received the node's criteria
    /// returned the root's, and failed a correct scaffolding leaf for its
    /// siblings' absent work. Those foreign criteria must be rejected.
    #[test]
    fn verdict_scoped_to_another_nodes_criteria_is_rejected() {
        let own = vec![
            "package.json with name, scripts (build, typecheck, test, start)".to_string(),
            "tsconfig.json with strict mode".to_string(),
            "project compiles with npm run build and npm run typecheck".to_string(),
        ];
        let root_criteria: Vec<Value> = serde_json::from_str(
            r#"[
                {"name":"the goal is delivered in full","pass":false,"reason":"siblings missing"},
                {"name":"all the pieces are assembled into one working whole, not left as independent modules","pass":false,"reason":"siblings missing"},
                {"name":"the project's own build, typecheck and test commands pass","pass":false,"reason":"trivial"}
            ]"#,
        )
        .unwrap();
        assert!(
            !verdict_matches_criteria(&own, &root_criteria),
            "a verdict grading another node's criteria must be rejected"
        );
    }

    #[test]
    fn verdict_that_addresses_every_own_criterion_is_accepted() {
        let own = vec![
            "Holdings saved to JSON file on disk".to_string(),
            "Holdings loaded on app start".to_string(),
            "Add/remove holdings persisted".to_string(),
            "State survives app restart".to_string(),
        ];
        let returned: Vec<Value> = serde_json::from_str(
            r#"[
                {"name":"Holdings saved to JSON file on disk","pass":true,"reason":"ok"},
                {"name":"Holdings loaded on app start","pass":false,"reason":"main() empty"},
                {"name":"Add/remove holdings persisted","pass":true,"reason":"ok"},
                {"name":"State survives app restart","pass":true,"reason":"ok"}
            ]"#,
        )
        .unwrap();
        assert!(verdict_matches_criteria(&own, &returned));
    }

    #[test]
    fn verdict_missing_a_criterion_is_not_accepted() {
        let own = vec!["alpha works".to_string(), "beta works".to_string()];
        let returned: Vec<Value> =
            serde_json::from_str(r#"[{"name":"alpha works","pass":true,"reason":"ok"}]"#).unwrap();
        assert!(!verdict_matches_criteria(&own, &returned));
    }

    /// A paraphrasing critic that keeps a criterion's meaning must not be forced
    /// to FAIL only because it did not echo the name verbatim.
    #[test]
    fn paraphrased_criterion_is_accepted_by_token_overlap() {
        let own = vec!["README explains test and build commands".to_string()];
        let returned: Vec<Value> = serde_json::from_str(
            r#"[{"name":"Test and build commands are documented","pass":true,"reason":"ok"}]"#,
        )
        .unwrap();
        assert!(
            verdict_matches_criteria(&own, &returned),
            "a meaning-preserving rename must not be treated as a foreign criterion"
        );
    }

    /// Token overlap must not become a rubber stamp: an unrelated criterion is
    /// still rejected.
    #[test]
    fn unrelated_criterion_is_still_rejected() {
        let own = vec!["README explains test and build commands".to_string()];
        let returned: Vec<Value> = serde_json::from_str(
            r#"[{"name":"the app renders a portfolio table","pass":true,"reason":"ok"}]"#,
        )
        .unwrap();
        assert!(!verdict_matches_criteria(&own, &returned));
    }

    /// The top-level reason is surfaced for persistence, and a bare PASS is still
    /// downgraded.
    #[test]
    fn parse_verdict_full_surfaces_the_reason() {
        let msg = critic_message(
            r#"{"verdict":"FAIL","reason":"README is truncated","criteria":[{"name":"x","pass":false,"reason":"cut"}]}"#,
        );
        let (verdict, reason, details) = parse_verdict_full(&msg).unwrap();
        assert_eq!(verdict, "FAIL");
        assert_eq!(reason, "README is truncated");
        assert_eq!(details.len(), 1);
    }

    /// Per-node context must not grow with the project's age (steering events),
    /// its breadth (siblings) or the size of dependency output. Every injected
    /// collection is capped, so a large tree produces a bounded prompt.
    #[test]
    fn context_stays_bounded_as_the_tree_grows() {
        let dir = std::env::temp_dir().join(format!("fractal_ctx_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = crate::store::Store::new(&dir);
        let root = store.init("build a bounded thing").unwrap();

        // A long history of steering events, each propagated to descendants.
        for i in 0..80 {
            store
                .add_constraint_and_propagate(&root.id, &format!("rule {i}: {}", "x".repeat(200)))
                .unwrap();
        }

        // A wide branch: the first child is a dependency with a large artifact
        // set, the second consumes it, and 60 more are siblings.
        let mut contracts = vec![crate::store::Contract {
            goal: "produce dependency output".into(),
            id: "dep".into(),
            ..Default::default()
        }];
        contracts.push(crate::store::Contract {
            goal: "consume the dependency".into(),
            id: "consumer".into(),
            depends_on: vec!["dep".into()],
            ..Default::default()
        });
        for i in 0..60 {
            contracts.push(crate::store::Contract {
                goal: format!("sibling {i}"),
                id: format!("s{i}"),
                ..Default::default()
            });
        }
        let children = store.add_children(&root, &contracts).unwrap();
        let dep = children[0].clone();
        let consumer = children[1].clone();
        assert_eq!(
            consumer.depends_on,
            vec![dep.id.clone()],
            "the dependency alias must resolve to the real sibling id"
        );
        for i in 0..50 {
            std::fs::write(
                dep.artifacts_dir().join(format!("artifact_{i}.txt")),
                "y".repeat(2000),
            )
            .unwrap();
        }

        let ctx = assemble_context(&store, &consumer).unwrap();
        assert!(
            ctx.len() < 20_000,
            "context grew unbounded with constraints, siblings and dependency artifacts: {} bytes",
            ctx.len()
        );
        assert!(
            ctx.matches("Dependency artifact").count() <= MAX_DEP_ARTIFACTS,
            "dependency artifact previews were not capped"
        );
        assert!(
            ctx.matches("\n- ").count() <= 200,
            "injected list items were not capped"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// M1: the model narrates `fractal done --summary "..."` as text, with
    /// escaped quotes and opencode's tool framing, instead of executing it.
    #[test]
    fn narrated_done_command_recovers_a_complete_decision() {
        let text = "I implemented the module and verified it.\n\
                    fractal done --summary \"added portfolioCalc with 25 passing tests\"</arg_value>";
        let d = transcript_command_decision(text, Path::new(".")).unwrap();
        assert_eq!(d.get("verb").unwrap(), "complete");
        assert_eq!(
            d.get("summary").unwrap(),
            "added portfolioCalc with 25 passing tests"
        );
    }

    #[test]
    fn narrated_done_with_plain_quotes_is_recovered() {
        let text = "done\n$ fractal done --summary \"typed the price stream and wired it in\"";
        let d = transcript_command_decision(text, Path::new(".")).unwrap();
        assert_eq!(
            d.get("summary").unwrap(),
            "typed the price stream and wired it in"
        );
    }

    /// M1: a narrated split carries the same subtask JSON a real `fractal split`
    /// call would, and must be recovered as a split - not a completion.
    #[test]
    fn narrated_split_command_recovers_a_split_decision() {
        let text = "I need to decompose this.\n\
                    fractal split --subtasks '[{\"id\":\"a\",\"goal\":\"first\"},{\"id\":\"b\",\"goal\":\"second\"}]'";
        let d = transcript_command_decision(text, Path::new(".")).unwrap();
        assert_eq!(d.get("verb").unwrap(), "split");
        assert_eq!(d.get("subtasks").unwrap().as_array().unwrap().len(), 2);
    }

    /// The prompt's own instruction line mentions both commands with no
    /// arguments; it must never be mistaken for a decision, and genuinely empty
    /// output stays fail-closed.
    #[test]
    fn a_command_mention_without_arguments_is_not_a_decision() {
        let instruction = "Signal completion with `fractal done` or split with `fractal split`.";
        assert!(transcript_command_decision(instruction, Path::new(".")).is_none());
        assert!(transcript_command_decision("working...", Path::new(".")).is_none());
        assert!(transcript_command_decision("", Path::new(".")).is_none());
    }

    /// M1: a bare narrated `fractal done` (no `--summary`) is an explicit
    /// completion and must recover, so an aggregating parent is not failed six
    /// times for saying the right thing in the wrong shape.
    #[test]
    fn a_bare_narrated_done_is_recovered() {
        let d = transcript_command_decision("All children passed.\nfractal done", Path::new("."))
            .expect("a bare `fractal done` must recover");
        assert_eq!(d.get("verb").unwrap(), "complete");
        assert!(
            !d.get("summary").unwrap().as_str().unwrap().is_empty(),
            "a recovered completion needs a non-empty summary"
        );
    }

    /// Only the tail is scanned, so a command quoted early in a long transcript
    /// (e.g. from the prompt) cannot be replayed as this node's decision.
    #[test]
    fn only_the_tail_of_a_long_transcript_is_scanned() {
        let mut text = String::from("fractal done --summary \"an early example\"\n");
        for i in 0..30 {
            text.push_str(&format!("line {i}\n"));
        }
        assert!(transcript_command_decision(&text, Path::new(".")).is_none());
    }

    #[test]
    fn narrated_split_can_reference_a_subtasks_file() {
        let dir = std::env::temp_dir().join(format!("fractal_narrated_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("subtasks.json"),
            r#"[{"id":"only","goal":"do it"}]"#,
        )
        .unwrap();
        let text = "fractal split --subtasks @subtasks.json";
        let d = transcript_command_decision(text, &dir).unwrap();
        assert_eq!(d.get("verb").unwrap(), "split");
        assert_eq!(d.get("subtasks").unwrap().as_array().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A split's `manual_verification` list is parsed separately from the
    /// executable `verification` gates.
    #[test]
    fn split_payload_separates_manual_from_executable_verification() {
        let payload = serde_json::json!({
            "verb": "split",
            "subtasks": [{
                "id": "a",
                "goal": "build the TUI",
                "acceptance_criteria": ["it renders"],
                "verification": ["npm test"],
                "manual_verification": ["the layout is clean at 80x24", "keys respond"]
            }]
        });
        let result = result_from_payload("split", &payload).unwrap();
        let c = &result.subtasks[0];
        assert_eq!(c.verification, vec!["npm test".to_string()]);
        assert_eq!(
            c.manual_verification,
            vec![
                "the layout is clean at 80x24".to_string(),
                "keys respond".to_string()
            ]
        );
    }
}
