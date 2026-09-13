//! A local web dashboard over the same `Store` the TUI reads and writes.
//!
//! This is a second view of one state model, not a second state model. Every
//! mutation here routes through the identical `Store` methods the scheduler and
//! the TUI use, so a TUI/CLI action and a dashboard action produce the same
//! on-disk and indexed state and the TUI sees a dashboard action on its next
//! read.
//!
//! No framework, no build step, no CDN: a hand-rolled HTTP/1.1 server over
//! `std::net` serves one embedded HTML page and a small JSON API. It polls by
//! design; a long-lived SSE/websocket connection would buy nothing at this scale.
//!
//! Safety: bound to loopback by default. Requests whose peer is not loopback are
//! read-only unless the operator opts in with `--allow-remote-mutations`, and
//! every mutation is a POST carrying `application/json` (a GET can never mutate).

use crate::store::{Store, StoreError};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const INDEX_HTML: &str = include_str!("../assets/dashboard.html");
const MAX_BODY_BYTES: usize = 256 * 1024;
/// A full node patch can be very large. The dashboard shows a bounded prefix so
/// one generated file cannot stall the page; the marker says the harness cut it.
const MAX_DIFF_CHARS: usize = 40_000;
const MAX_EVENT_TAIL: usize = 300;

pub struct ServerConfig {
    pub host: Option<String>,
    pub port: u16,
    pub bind_all: bool,
    pub allow_remote_mutations: bool,
    /// Model handed to the butler when a chat request runs. Resolved once at
    /// startup so every request the dashboard serves uses the same one.
    pub model: String,
}

/// The address the dashboard binds to. Loopback unless the operator explicitly
/// asks otherwise, which is what keeps the default a local-only surface.
pub fn resolve_host(host: &Option<String>, bind_all: bool) -> String {
    if bind_all {
        "0.0.0.0".to_string()
    } else {
        host.clone()
            .filter(|h| !h.trim().is_empty())
            .unwrap_or_else(|| "127.0.0.1".to_string())
    }
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "::1" | "localhost")
}

/// Bind the listener without serving. Split out from `serve` so a caller (and
/// the tests) can observe the resolved address before the accept loop starts.
pub fn bind_listener(cfg: &ServerConfig) -> Result<TcpListener, String> {
    let host = resolve_host(&cfg.host, cfg.bind_all);
    TcpListener::bind((host.as_str(), cfg.port))
        .map_err(|e| format!("could not bind {host}:{}: {e}", cfg.port))
}

pub fn serve(project: &Path, cfg: ServerConfig) -> Result<(), String> {
    let host = resolve_host(&cfg.host, cfg.bind_all);
    let listener = bind_listener(&cfg)?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("could not read bound address: {e}"))?;
    let shown = if host == "0.0.0.0" || host == "::" {
        "<this-machine-ip>".to_string()
    } else {
        host.clone()
    };
    println!(
        "fractal dashboard listening on http://{shown}:{}",
        addr.port()
    );
    if !is_loopback_host(&host) {
        println!(
            "  note: bound to {host}; requests from other devices are read-only unless \
             --allow-remote-mutations is set"
        );
    }
    let store = Arc::new(Store::new(project));
    serve_listener(
        listener,
        store,
        cfg.allow_remote_mutations,
        project.to_path_buf(),
        cfg.model,
    );
    Ok(())
}

/// The line a run prints so the user knows where to watch progress.
pub fn progress_line(url: &str) -> String {
    format!("fractal started - see progress here: {url}")
}

/// Start the dashboard in a background thread for the lifetime of the process
/// and return the URL to announce. Best-effort by contract: the run must not
/// fail because the dashboard could not bind, so the caller keeps going and
/// reports how to start it manually. The default port is tried first; if it is
/// taken, any free port is used so a second run never blocks the first.
pub fn spawn(project: &Path, model: &str) -> Result<String, String> {
    let requested = ServerConfig {
        host: None,
        port: 8787,
        bind_all: false,
        allow_remote_mutations: false,
        model: model.to_string(),
    };
    let listener = bind_listener(&requested).or_else(|_| {
        bind_listener(&ServerConfig {
            host: None,
            port: 0,
            bind_all: false,
            allow_remote_mutations: false,
            model: model.to_string(),
        })
    })?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("could not read bound address: {e}"))?
        .port();
    let url = format!("http://127.0.0.1:{port}/");
    let store = Arc::new(Store::new(project));
    let project = project.to_path_buf();
    let model = model.to_string();
    std::thread::spawn(move || serve_listener(listener, store, false, project, model));
    Ok(url)
}

fn serve_listener(
    listener: TcpListener,
    store: Arc<Store>,
    allow_remote_mutations: bool,
    project: PathBuf,
    model: String,
) {
    // A fresh server owns the conversation from here: any turn a dead process
    // left `working` is settled before the first request can be served.
    crate::butler::recover_interrupted(&store);
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let store = store.clone();
        let project = project.clone();
        let model = model.clone();
        std::thread::spawn(move || {
            let _ = handle_connection(stream, &store, &project, allow_remote_mutations, &model);
        });
    }
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    content_type: String,
    body: Vec<u8>,
}

struct Outcome {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

fn handle_connection(
    mut stream: TcpStream,
    store: &Store,
    project: &Path,
    allow_remote_mutations: bool,
    model: &str,
) -> std::io::Result<()> {
    let is_local = stream
        .peer_addr()
        .map(|a| a.ip().is_loopback())
        .unwrap_or(false);
    let Some(req) = read_request(&mut stream)? else {
        return Ok(());
    };
    let outcome = route(
        store,
        project,
        allow_remote_mutations,
        is_local,
        &req.method,
        &req.path,
        &req.query,
        &req.content_type,
        &req.body,
        model,
    );
    write_response(&mut stream, &outcome)
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "request ended before headers",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_BODY_BYTES + 64 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request too large",
            ));
        }
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_ascii_uppercase();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query) = split_target(&target);

    let mut content_length = 0usize;
    let mut content_type = String::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            match key.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_length = value.trim().parse().unwrap_or(0),
                "content-type" => content_type = value.trim().to_string(),
                _ => {}
            }
        }
    }
    if content_length > MAX_BODY_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "body too large",
        ));
    }

    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    Ok(Some(Request {
        method,
        path,
        query,
        content_type,
        body,
    }))
}

fn write_response(stream: &mut TcpStream, outcome: &Outcome) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        outcome.status,
        status_text(outcome.status),
        outcome.content_type,
        outcome.body.len(),
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(&outcome.body)?;
    stream.flush()
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        415 => "Unsupported Media Type",
        500 => "Internal Server Error",
        _ => "Error",
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn split_target(target: &str) -> (String, HashMap<String, String>) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut map = HashMap::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(percent_decode(k), percent_decode(v));
    }
    (path.to_string(), map)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

#[allow(clippy::too_many_arguments)]
fn route(
    store: &Store,
    project: &Path,
    allow_remote_mutations: bool,
    is_local: bool,
    method: &str,
    path: &str,
    query: &HashMap<String, String>,
    content_type: &str,
    body: &[u8],
    model: &str,
) -> Outcome {
    match (method, path) {
        ("GET", "/") => Outcome {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: INDEX_HTML.as_bytes().to_vec(),
        },
        ("GET", "/api/state") => match state_json(store, project) {
            Ok(value) => json_outcome(200, value),
            Err(e) => error_outcome(500, e.to_string()),
        },
        ("GET", "/api/node") => {
            let id = query.get("id").cloned().unwrap_or_default();
            if id.trim().is_empty() {
                return error_outcome(400, "missing node id".into());
            }
            match node_json(store, &id) {
                Ok(value) => json_outcome(200, value),
                Err(e) => error_outcome(404, e.to_string()),
            }
        }
        ("GET", "/api/butler") => json_outcome(
            200,
            json!({"turns": crate::butler::read_conversation(store)}),
        ),
        ("POST", "/api/butler") => {
            let project = project.to_path_buf();
            butler_post(
                store,
                model,
                is_local,
                allow_remote_mutations,
                content_type,
                body,
                move |turn_id, message, model| {
                    // A butler session can take minutes; run it off the request
                    // thread and let the page poll the durable conversation.
                    std::thread::spawn(move || {
                        let store = Store::new(&project);
                        let _ = crate::butler::run_turn(&store, &turn_id, &message, &model);
                    });
                },
            )
        }
        ("GET", _) | ("POST", _) => error_outcome(404, "not found".into()),
        _ => error_outcome(405, "method not allowed".into()),
    }
}

fn json_outcome(status: u16, value: Value) -> Outcome {
    Outcome {
        status,
        content_type: "application/json; charset=utf-8",
        body: value.to_string().into_bytes(),
    }
}

fn error_outcome(status: u16, message: String) -> Outcome {
    json_outcome(status, json!({"ok": false, "error": message}))
}

/// One chat request to the butler. The turn is claimed and persisted before
/// `start` is called, so the browser gets an immediate working state while the
/// agent runs behind it. Split out from `route` so tests can drive the whole
/// request path without launching an executor.
fn butler_post<F>(
    store: &Store,
    model: &str,
    is_local: bool,
    allow_remote_mutations: bool,
    content_type: &str,
    body: &[u8],
    start: F,
) -> Outcome
where
    F: FnOnce(String, String, String),
{
    if !is_local && !allow_remote_mutations {
        return error_outcome(
            403,
            "remote mutations are disabled; restart with --allow-remote-mutations to enable them"
                .into(),
        );
    }
    if !content_type
        .to_ascii_lowercase()
        .contains("application/json")
    {
        return error_outcome(415, "Content-Type must be application/json".into());
    }
    let value: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return error_outcome(400, format!("invalid JSON body: {e}")),
    };
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if message.is_empty() {
        return error_outcome(400, "the butler needs a 'message'".into());
    }
    match crate::butler::begin_turn(store, &message) {
        Ok(turn_id) => {
            start(turn_id.clone(), message, model.to_string());
            json_outcome(202, json!({"ok": true, "turn": turn_id}))
        }
        Err(e) => error_outcome(409, e.to_string()),
    }
}

/// The whole tree plus the run summary. Reading never reconciles: `reconcile`
/// repairs a fresh process's view from disk and would rewrite a node that is
/// legitimately `running` in the live scheduler to `pending`, so a read-only
/// observer must not call it.
pub fn state_json(store: &Store, project: &Path) -> Result<Value, StoreError> {
    store.require_initialised()?;
    let nodes = store.walk()?;
    let stale = store.stale_ids().unwrap_or_default();
    let tampered: std::collections::HashSet<String> = store.tampered_nodes().into_iter().collect();

    let mut counts: Map<String, Value> = Map::new();
    for status in [
        crate::store::COMPLETE,
        crate::store::FAILED,
        crate::store::PENDING,
        crate::store::RUNNING,
        crate::store::SPLIT,
        crate::store::SUSPENDED,
    ] {
        let n = nodes.iter().filter(|node| node.status == status).count();
        counts.insert(status.to_string(), json!(n));
    }
    counts.insert("total".to_string(), json!(nodes.len()));

    let mut list = Vec::with_capacity(nodes.len());
    for node in &nodes {
        let contract = node.contract();
        let children: Vec<String> = nodes
            .iter()
            .filter(|c| c.parent.as_deref() == Some(node.id.as_str()))
            .map(|c| c.id.clone())
            .collect();
        list.push(json!({
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
            "children": children,
            "stale": stale.contains(&node.id),
            "tampered": tampered.contains(&node.id),
            "activity": store.read_activity(&node.id),
        }));
    }

    let root_status = nodes
        .first()
        .map(|n| n.status.clone())
        .unwrap_or_else(|| crate::store::PENDING.to_string());
    let root_goal = nodes.first().map(|n| n.goal.clone()).unwrap_or_default();
    let overall_status = crate::scheduler::surface_root_status(&nodes);
    let digest = store.generate_digest().unwrap_or_default();
    let trace = std::fs::read_to_string(project.join("trace.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok());

    Ok(json!({
        "project": project.display().to_string(),
        "root_status": root_status,
        "root_goal": root_goal,
        "overall_status": overall_status,
        "counts": counts,
        "digest": digest,
        "trace": trace,
        "nodes": list,
    }))
}

/// One node in full: its contract, its memory, its committed diff, the assembled
/// context the executor would receive, and its artifacts.
pub fn node_json(store: &Store, node_id: &str) -> Result<Value, StoreError> {
    let node = store.get(node_id)?;
    let contract = node.contract();

    let decisions_raw = std::fs::read_to_string(node.decisions_path()).unwrap_or_default();
    let decisions: Vec<String> = decisions_raw
        .lines()
        .filter(|l| l.starts_with("- "))
        .map(|l| l.to_string())
        .collect();

    let log_raw = std::fs::read_to_string(node.log_path()).unwrap_or_default();
    let events: Vec<Value> = log_raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    let tail: Vec<Value> = events
        .iter()
        .rev()
        .take(MAX_EVENT_TAIL)
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let errors: Vec<Value> = tail
        .iter()
        .filter(|e| {
            e.get("event").and_then(Value::as_str) == Some("error") || e.get("error").is_some()
        })
        .cloned()
        .collect();
    let gate_outcomes: Vec<Value> = tail
        .iter()
        .filter(|e| {
            e.get("event")
                .and_then(Value::as_str)
                .map(|name| {
                    name.starts_with("gate")
                        || name.starts_with("critic")
                        || name.starts_with("verify")
                })
                .unwrap_or(false)
        })
        .cloned()
        .collect();

    let artifacts: Vec<Value> = node
        .find_artifacts()
        .iter()
        .map(|p| {
            let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            json!({
                "name": p.strip_prefix(node.artifacts_dir()).unwrap_or(p).display().to_string(),
                "size": size,
            })
        })
        .collect();

    let diff = crate::git::node_diff(&store.root, &node.id, false).unwrap_or_default();
    let diff = cap_chars(diff, MAX_DIFF_CHARS);

    let context = crate::runner::assemble_context(store, &node).unwrap_or_default();

    let children: Vec<String> = store
        .children_of(&node)
        .unwrap_or_default()
        .iter()
        .map(|c| c.id.clone())
        .collect();

    Ok(json!({
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
        "children": children,
        "decisions": decisions,
        "events": tail,
        "errors": errors,
        "gate_outcomes": gate_outcomes,
        "artifacts": artifacts,
        "diff": diff,
        "activity": store.read_activity(&node.id),
        "context_bytes": context.len(),
        "context": context,
    }))
}

fn cap_chars(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n\n[truncated by the dashboard at {max} of {} chars]",
        &s[..cut],
        s.chars().count()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Contract, Store};

    fn project(name: &str) -> (Store, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("fractal_dash_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (Store::new(&dir), dir)
    }

    fn tree(name: &str) -> (Store, PathBuf) {
        let (store, dir) = project(name);
        let root = store.init("build a thing").unwrap();
        let kids = store
            .add_children(
                &root,
                &[Contract {
                    goal: "child".into(),
                    id: "a".into(),
                    ..Default::default()
                }],
            )
            .unwrap();
        store
            .add_children(
                &kids[0],
                &[Contract {
                    goal: "grandchild".into(),
                    id: "b".into(),
                    ..Default::default()
                }],
            )
            .unwrap();
        (store, dir)
    }

    #[test]
    fn default_host_is_loopback_and_bind_all_is_opt_in() {
        assert_eq!(resolve_host(&None, false), "127.0.0.1");
        assert_eq!(resolve_host(&None, true), "0.0.0.0");
        assert_eq!(
            resolve_host(&Some("10.0.0.5".into()), false),
            "10.0.0.5",
            "an explicit host must win over the default"
        );
    }

    #[test]
    fn listener_binds_loopback_by_default() {
        let cfg = ServerConfig {
            host: None,
            port: 0,
            bind_all: false,
            allow_remote_mutations: false,
            model: "default".into(),
        };
        let listener = bind_listener(&cfg).expect("loopback bind must succeed");
        let addr = listener.local_addr().unwrap();
        assert!(addr.ip().is_loopback(), "default bind must be loopback");
    }

    #[test]
    fn progress_line_names_the_url() {
        let line = progress_line("http://127.0.0.1:8787/");
        assert!(
            line.contains("fractal started") && line.contains("http://127.0.0.1:8787/"),
            "the startup line must hand the user the real URL: {line}"
        );
    }

    #[test]
    fn spawn_falls_back_off_a_busy_default_port() {
        // Hold 8787 if we can; whatever the outcome, a spawned dashboard must
        // still come up on a free port rather than failing the run.
        let blocker = TcpListener::bind(("127.0.0.1", 8787)).ok();
        let (_store, dir) = tree("spawn_fallback");
        let url = spawn(&dir, "default").expect("spawn must always find a port");
        assert!(url.starts_with("http://127.0.0.1:"), "got {url}");
        assert!(url.ends_with('/'), "got {url}");
        if blocker.is_some() {
            assert!(
                !url.contains(":8787/"),
                "a busy default port must fall back to a free one: {url}"
            );
        }
        drop(blocker);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_json_reports_the_whole_tree() {
        let (store, dir) = tree("state");
        let value = state_json(&store, &dir).unwrap();
        let nodes = value.get("nodes").and_then(Value::as_array).unwrap();
        let ids: Vec<&str> = nodes
            .iter()
            .map(|n| n.get("id").and_then(Value::as_str).unwrap())
            .collect();
        assert!(ids.contains(&"root") && ids.contains(&"root-01") && ids.contains(&"root-01-01"));
        let root = nodes
            .iter()
            .find(|n| n.get("id").and_then(Value::as_str) == Some("root"));
        assert_eq!(
            value.get("root_status").and_then(Value::as_str),
            Some("split")
        );
        assert!(
            root.unwrap()
                .get("children")
                .unwrap()
                .as_array()
                .unwrap()
                .len()
                == 1,
            "the tree must expose each node's children"
        );
        assert!(value.get("digest").and_then(Value::as_str).is_some());
        assert_eq!(
            value.get("root_goal").and_then(Value::as_str),
            Some("build a thing"),
            "the run summary must name the project by its goal"
        );
        assert_eq!(
            value.get("overall_status").and_then(Value::as_str),
            Some("split"),
            "a healthy split tree reports its root status"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_child_surfaces_at_the_run_level() {
        let (store, dir) = tree("overall");
        let child = store.get("root-01").unwrap();
        store.set_status(&child, crate::store::FAILED).unwrap();
        let value = state_json(&store, &dir).unwrap();
        assert_eq!(
            value.get("overall_status").and_then(Value::as_str),
            Some("failed"),
            "a failed node anywhere must not hide behind a split root"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn node_activity_is_exposed_for_the_tree_and_the_detail_view() {
        let (store, dir) = tree("activity");
        store.write_activity("root-01", "running npm test").unwrap();
        let state = state_json(&store, &dir).unwrap();
        let node = state
            .get("nodes")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .find(|n| n.get("id").and_then(Value::as_str) == Some("root-01"))
            .unwrap();
        assert_eq!(
            node.get("activity").and_then(Value::as_str),
            Some("running npm test"),
            "the tree row must carry the latest activity"
        );
        let detail = node_json(&store, "root-01").unwrap();
        assert_eq!(
            detail.get("activity").and_then(Value::as_str),
            Some("running npm test")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn get_butler_returns_the_durable_conversation() {
        let (store, dir) = tree("butler_get");
        let outcome = route(
            &store,
            &dir,
            false,
            true,
            "GET",
            "/api/butler",
            &HashMap::new(),
            "",
            &[],
            "default",
        );
        assert_eq!(outcome.status, 200);
        let value: Value = serde_json::from_slice(&outcome.body).unwrap();
        assert_eq!(value["turns"].as_array().unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn butler_post_claims_a_turn_and_hands_it_to_the_runner() {
        let (store, dir) = tree("butler_post");
        let started = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = started.clone();
        let body =
            json!({"message": "the tracker should use real data, not simulated"}).to_string();
        let outcome = butler_post(
            &store,
            "default",
            true,
            false,
            "application/json",
            body.as_bytes(),
            move |turn_id, message, model| {
                sink.lock().unwrap().push((turn_id, message, model));
            },
        );
        assert_eq!(
            outcome.status, 202,
            "a claimed turn must be accepted at once"
        );
        let value: Value = serde_json::from_slice(&outcome.body).unwrap();
        assert_eq!(value["ok"], true);

        let calls = started.lock().unwrap();
        assert_eq!(calls.len(), 1, "the runner must be handed exactly one turn");
        assert_eq!(
            calls[0].1,
            "the tracker should use real data, not simulated"
        );
        assert_eq!(calls[0].2, "default");

        let turns = crate::butler::read_conversation(&store);
        assert_eq!(turns.len(), 1);
        assert_eq!(
            turns[0]["status"], "working",
            "the page must see it working"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn butler_post_refuses_while_a_request_is_already_running() {
        let (store, dir) = tree("butler_busy");
        crate::butler::begin_turn(&store, "the first request").unwrap();
        let body = json!({"message": "a second request"}).to_string();
        let outcome = butler_post(
            &store,
            "default",
            true,
            false,
            "application/json",
            body.as_bytes(),
            |_, _, _| panic!("a second request must not start a run"),
        );
        assert_eq!(outcome.status, 409, "a parallel run must be refused");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn butler_post_is_read_only_for_remote_callers_and_rejects_bad_input() {
        let (store, dir) = tree("butler_guard");
        let body = json!({"message": "hello"}).to_string();

        let remote = butler_post(
            &store,
            "default",
            false,
            false,
            "application/json",
            body.as_bytes(),
            |_, _, _| {},
        );
        assert_eq!(remote.status, 403, "a remote mutation must be refused");

        let bad_type = butler_post(
            &store,
            "default",
            true,
            false,
            "text/plain",
            body.as_bytes(),
            |_, _, _| {},
        );
        assert_eq!(bad_type.status, 415);

        let empty = butler_post(
            &store,
            "default",
            true,
            false,
            "application/json",
            b"{}",
            |_, _, _| {},
        );
        assert_eq!(empty.status, 400, "an empty message is not a request");

        assert!(
            crate::butler::read_conversation(&store).is_empty(),
            "a refused request must not leave a turn behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
