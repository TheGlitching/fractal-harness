use chrono::Utc;
use regex::Regex;
use rusqlite::{Connection, params};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const ROOT_ID: &str = "root";

const TREE_DIRNAME: &str = "tree";
const GLOBAL_DIRNAME: &str = "global";
const STATE_DIRNAME: &str = ".fractal";
const INDEX_FILENAME: &str = "index.db";
const CONTRACT_FILENAME: &str = "contract.md";
const DECISIONS_FILENAME: &str = "decisions.md";
const LOG_DIRNAME: &str = "log";
const ARTIFACTS_DIRNAME: &str = "artifacts";
const CHILDREN_DIRNAME: &str = "children";
const EVENTS_FILENAME: &str = "events.jsonl";

pub const PENDING: &str = "pending";
pub const RUNNING: &str = "running";
pub const SPLIT: &str = "split";
pub const SUSPENDED: &str = "suspended";
pub const COMPLETE: &str = "complete";
pub const FAILED: &str = "failed";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS nodes (
    id          TEXT PRIMARY KEY,
    parent      TEXT,
    depth       INTEGER NOT NULL,
    status      TEXT NOT NULL,
    goal        TEXT NOT NULL DEFAULT '',
    summary     TEXT NOT NULL DEFAULT '',
    depends_on  TEXT NOT NULL DEFAULT '[]',
    dep_fp      TEXT NOT NULL DEFAULT '{}',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS nodes_parent ON nodes(parent);
CREATE INDEX IF NOT EXISTS nodes_status ON nodes(status);
CREATE TABLE IF NOT EXISTS budget (
    node_id     TEXT PRIMARY KEY,
    parent      TEXT,
    allowance   INTEGER NOT NULL DEFAULT 0,
    calls       INTEGER NOT NULL DEFAULT 0,
    fee_paid    INTEGER NOT NULL DEFAULT 0,
    children    INTEGER NOT NULL DEFAULT 0,
    debits      INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS steer_queue (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    command     TEXT NOT NULL,
    payload     TEXT NOT NULL,
    created_at  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS global_entries (
    id          TEXT PRIMARY KEY,
    type        TEXT NOT NULL,
    content     TEXT NOT NULL,
    superseded  INTEGER NOT NULL DEFAULT 0,
    supersedes  TEXT,
    created_at  TEXT NOT NULL
);
"#;

fn now() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
}

#[derive(Debug)]
pub struct Contract {
    pub goal: String,
    pub acceptance_criteria: Vec<String>,
    pub interfaces: Vec<String>,
    pub constraints: Vec<String>,
    #[allow(dead_code)]
    pub id: String,
    pub depends_on: Vec<String>,
    #[allow(dead_code)]
    pub allocation: i64,
}

impl Contract {
    pub fn render(&self, node_id: &str, depth: i64, parent: Option<&str>) -> String {
        let bullets = |items: &[String]| -> String {
            if items.is_empty() {
                "- (none stated)\n".to_string()
            } else {
                items.iter().map(|s| format!("- {}\n", s.trim())).collect()
            }
        };
        format!(
            "# Contract: {nid}\n\n- node: {nid}\n- parent: {p}\n- depth: {depth}\n\n\
             ## Goal\n\n{goal}\n\n\
             ## Acceptance criteria\n\n{ac}\
             ## Interfaces\n\n{iface}\
             ## Inherited constraints\n\n{cons}\
             ## depends_on\n\n{deps}",
            nid = node_id,
            p = parent.unwrap_or("(none)"),
            goal = self.goal.trim(),
            ac = bullets(&self.acceptance_criteria),
            iface = bullets(&self.interfaces),
            cons = bullets(&self.constraints),
            deps = bullets(&self.depends_on),
        )
    }

    pub fn parse(text: &str) -> Self {
        let mut sections: std::collections::HashMap<String, Vec<String>> = Default::default();
        let mut current: Option<String> = None;
        for line in text.lines() {
            if line.starts_with("## ") {
                current = Some(line[3..].trim().to_lowercase());
                sections.entry(current.clone().unwrap_or_default()).or_default();
            } else if let Some(ref cur) = current {
                sections.entry(cur.clone()).or_default().push(line.to_string());
            }
        }
        let body = |name: &str| -> String {
            sections.get(name).map(|v| v.join("\n")).unwrap_or_default().trim().to_string()
        };
        let items = |name: &str| -> Vec<String> {
            sections.get(name).map(|v| v.iter().filter_map(|l| {
                let s = l.trim();
                if s.starts_with("- ") { Some(s[2..].trim().to_string()) } else { None }
            }).collect()).unwrap_or_default()
        };
        Contract {
            goal: body("goal"),
            acceptance_criteria: items("acceptance criteria"),
            interfaces: items("interfaces"),
            constraints: items("inherited constraints"),
            depends_on: items("depends_on"),
            id: String::new(),
            allocation: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub id: String,
    pub path: PathBuf,
    pub parent: Option<String>,
    pub depth: i64,
    pub status: String,
    pub goal: String,
    pub summary: String,
    pub depends_on: Vec<String>,
    pub dep_fp: String,
}

impl Node {
    pub fn contract_path(&self) -> PathBuf { self.path.join(CONTRACT_FILENAME) }
    pub fn decisions_path(&self) -> PathBuf { self.path.join(DECISIONS_FILENAME) }
    pub fn log_dir(&self) -> PathBuf { self.path.join(LOG_DIRNAME) }
    pub fn artifacts_dir(&self) -> PathBuf { self.path.join(ARTIFACTS_DIRNAME) }
    pub fn children_dir(&self) -> PathBuf { self.path.join(CHILDREN_DIRNAME) }
    pub fn contract(&self) -> Contract {
        fs::read_to_string(self.contract_path()).map(|t| Contract::parse(&t)).unwrap_or_else(|_| Contract {
            goal: String::new(), acceptance_criteria: vec![], interfaces: vec![],
            constraints: vec![], id: String::new(), depends_on: vec![], allocation: 0,
        })
    }
}

#[derive(Debug, Clone)]
pub struct GlobalEntry {
    pub entry_type: String,
    pub content: String,
    #[allow(dead_code)]
    pub id: String,
    #[allow(dead_code)]
    pub superseded: bool,
    #[allow(dead_code)]
    pub supersedes: String,
    #[allow(dead_code)]
    pub created_at: String,
}

#[derive(Debug)]
pub enum StoreError {
    Uninitialised,
    Sql(rusqlite::Error),
    Io(std::io::Error),
    Other(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Uninitialised => write!(f, "project not initialised; run `fractal init <goal>` first"),
            StoreError::Sql(e) => write!(f, "sqlite: {e}"),
            StoreError::Io(e) => write!(f, "io: {e}"),
            StoreError::Other(s) => write!(f, "{s}"),
        }
    }
}
impl std::error::Error for StoreError {}
impl From<rusqlite::Error> for StoreError { fn from(e: rusqlite::Error) -> Self { StoreError::Sql(e) } }
impl From<std::io::Error> for StoreError { fn from(e: std::io::Error) -> Self { StoreError::Io(e) } }

pub struct Store {
    pub tree_dir: PathBuf,
    pub global_dir: PathBuf,
    pub state_dir: PathBuf,
    pub index_path: PathBuf,
    connection: Mutex<Option<Connection>>,
}

impl Store {
    pub fn new(project: &Path) -> Self {
        let project = project.canonicalize().unwrap_or_else(|_| project.to_path_buf());
        Store {
            tree_dir: project.join(TREE_DIRNAME),
            global_dir: project.join(GLOBAL_DIRNAME),
            state_dir: project.join(STATE_DIRNAME),
            index_path: project.join(STATE_DIRNAME).join(INDEX_FILENAME),
            connection: Mutex::new(None),
        }
    }

    fn with_conn<F, T>(&self, f: F) -> Result<T, StoreError>
    where F: FnOnce(&Connection) -> Result<T, StoreError> {
        let mut guard = self.connection.lock().unwrap();
        if guard.is_none() {
            fs::create_dir_all(&self.state_dir)?;
            let conn = Connection::open(&self.index_path)?;
            conn.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
            conn.execute_batch(SCHEMA)?;
            *guard = Some(conn);
        }
        f(guard.as_ref().unwrap())
    }

    pub fn close(&self) {
        if let Some(conn) = self.connection.lock().unwrap().take() { let _ = conn.close(); }
    }

    pub fn initialised(&self) -> bool {
        self.tree_dir.join(ROOT_ID).join(CONTRACT_FILENAME).is_file()
    }

    pub fn require_initialised(&self) -> Result<(), StoreError> {
        if !self.initialised() { Err(StoreError::Uninitialised) } else { Ok(()) }
    }

    pub fn init(&self, goal: &str) -> Result<Node, StoreError> {
        if self.initialised() { return Err(StoreError::Other("project already initialised".into())); }
        let contract = Contract {
            goal: goal.trim().to_string(),
            acceptance_criteria: vec!["the goal is delivered in full".into(), "every leaf leaves an artifact behind".into()],
            interfaces: vec![], constraints: vec![], id: String::new(), depends_on: vec![], allocation: 0,
        };
        let node = Node { id: ROOT_ID.to_string(), path: self.tree_dir.join(ROOT_ID), parent: None, depth: 1,
            status: PENDING.to_string(), goal: contract.goal.clone(), summary: String::new(), depends_on: vec![], dep_fp: "{}".into() };
        Self::materialise_node(&node, &contract)?;
        self.with_conn(|conn| {
            Self::insert_node(conn, &node)?;
            if let Ok(b) = std::env::var("FRACTAL_BUDGET") {
                if let Ok(a) = b.parse::<i64>() {
                    conn.execute("INSERT OR IGNORE INTO budget(node_id,parent,allowance) VALUES(?1,NULL,?2)", params![ROOT_ID, a])?;
                }
            }
            Ok(())
        })?;
        Ok(node)
    }

    fn materialise_node(node: &Node, contract: &Contract) -> Result<(), StoreError> {
        fs::create_dir_all(&node.path)?;
        for sd in &[LOG_DIRNAME, ARTIFACTS_DIRNAME, CHILDREN_DIRNAME] { fs::create_dir_all(node.path.join(sd))?; }
        fs::write(node.contract_path(), contract.render(&node.id, node.depth, node.parent.as_deref()))?;
        if !node.decisions_path().exists() {
            fs::write(node.decisions_path(), format!("# Decisions: {}\n\nAppend-only semantic memory of this node.\n\n- {} node created\n", node.id, now()))?;
        }
        Ok(())
    }

    fn insert_node(conn: &Connection, node: &Node) -> Result<(), StoreError> {
        let stamp = now();
        conn.execute(
            "INSERT OR IGNORE INTO nodes (id,parent,depth,status,goal,summary,depends_on,dep_fp,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![node.id, node.parent, node.depth, node.status, node.goal, node.summary, serde_json::to_string(&node.depends_on).unwrap_or_default(), node.dep_fp, &stamp, &stamp],
        )?;
        Ok(())
    }

    pub fn add_children(&self, parent: &Node, contracts: &[Contract]) -> Result<Vec<Node>, StoreError> {
        fs::create_dir_all(parent.children_dir())?;
        let existing = Self::child_dirs(&parent.path).len();
        let mut children = Vec::new();
        for (i, c) in contracts.iter().enumerate() {
            let cid = format!("{}-{:02}", parent.id, existing + i + 1);
            let cnode = Node { id: cid.clone(), path: parent.children_dir().join(&cid), parent: Some(parent.id.clone()),
                depth: parent.depth + 1, status: PENDING.to_string(), goal: c.goal.clone(), summary: String::new(),
                depends_on: c.depends_on.clone(), dep_fp: "{}".into() };
            Self::materialise_node(&cnode, c)?;
            children.push(cnode);
        }
        let stamp = now();
        self.with_conn(|conn| {
            for ch in &children { Self::insert_node(conn, ch)?; }
            conn.execute("UPDATE nodes SET status=?1,updated_at=?2 WHERE id=?3", params![SPLIT, &stamp, parent.id])?;
            Ok(())
        })?;
        Ok(children)
    }

    pub fn complete(&self, node: &Node, summary: &str, deliverable: &str, artifacts: &[(String, String)]) -> Result<(), StoreError> {
        self.write_artifacts(node, artifacts, deliverable)?;
        let smr = if summary.is_empty() { "(no summary given)" } else { summary };
        self.append_decision(node, &format!("completed: {smr}"))?;
        let stamp = now();
        self.with_conn(|conn| {
            conn.execute("UPDATE nodes SET status=?1,summary=?2,updated_at=?3 WHERE id=?4", params![COMPLETE, summary.trim(), &stamp, node.id])?;
            Ok(())
        })
    }

    pub fn set_status(&self, node: &Node, status: &str) -> Result<(), StoreError> {
        self.with_conn(|conn| {
            conn.execute("UPDATE nodes SET status=?1,updated_at=?2 WHERE id=?3", params![status, &now(), node.id])?;
            Ok(())
        })
    }

    pub fn write_artifacts(&self, node: &Node, artifacts: &[(String, String)], deliverable: &str) -> Result<(), StoreError> {
        fs::create_dir_all(node.artifacts_dir())?;
        let mut written = false;
        for (p, c) in artifacts {
            if c.trim().is_empty() { continue; }
            let rel = Self::safe_path(p);
            let target = node.artifacts_dir().join(&rel);
            if let Some(parent) = target.parent() { fs::create_dir_all(parent)?; }
            fs::write(&target, c)?;
            written = true;
        }
        if !written && !deliverable.trim().is_empty() {
            fs::write(node.artifacts_dir().join("deliverable.md"), deliverable)?;
        }
        Ok(())
    }

    fn safe_path(raw: &str) -> PathBuf {
        let cleaned = raw.trim().replace('\\', "/");
        let parts: Vec<&str> = cleaned.split('/').filter(|p| !p.is_empty() && *p != "." && *p != ".." && !p.contains(':')).collect();
        if parts.is_empty() { return PathBuf::from("artifact.txt"); }
        parts.iter().collect()
    }

    pub fn append_decision(&self, node: &Node, entry: &str) -> Result<(), StoreError> {
        let mut f = fs::OpenOptions::new().create(true).append(true).open(node.decisions_path())?;
        writeln!(f, "- {} {}", now(), entry.trim())?;
        Ok(())
    }

    pub fn append_log(&self, node: &Node, record: &serde_json::Value) -> Result<(), StoreError> {
        fs::create_dir_all(node.log_dir())?;
        let mut f = fs::OpenOptions::new().create(true).append(true).open(node.log_dir().join(EVENTS_FILENAME))?;
        let mut rec = record.clone();
        if let serde_json::Value::Object(ref mut m) = rec {
            m.entry("at").or_insert_with(|| serde_json::Value::String(now()));
            m.entry("node").or_insert_with(|| serde_json::Value::String(node.id.clone()));
        }
        writeln!(f, "{}", serde_json::to_string(&rec).unwrap_or_default())?;
        Ok(())
    }

    fn child_dirs(path: &Path) -> Vec<PathBuf> {
        let cdir = path.join(CHILDREN_DIRNAME);
        if !cdir.is_dir() { return vec![]; }
        let mut dirs: Vec<PathBuf> = fs::read_dir(&cdir).into_iter().flatten().filter_map(|e| e.ok()).filter(|e| e.path().is_dir()).map(|e| e.path()).collect();
        dirs.sort();
        dirs
    }

    fn walk_disk(&self) -> Vec<(PathBuf, String, Option<String>, i64)> {
        let root = self.tree_dir.join(ROOT_ID);
        if !root.is_dir() { return vec![]; }
        let mut result = Vec::new();
        Self::visit_disk(&root, None, 1, &mut result);
        result
    }

    fn visit_disk(path: &Path, parent: Option<String>, depth: i64, out: &mut Vec<(PathBuf, String, Option<String>, i64)>) {
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        out.push((path.to_path_buf(), name.clone(), parent, depth));
        for child in Self::child_dirs(path) {
            Self::visit_disk(&child, Some(name.clone()), depth + 1, out);
        }
    }

    fn goal_on_disk(path: &Path) -> String {
        let cf = path.join(CONTRACT_FILENAME);
        if !cf.is_file() { return String::new(); }
        fs::read_to_string(&cf).ok().map(|t| Contract::parse(&t).goal).unwrap_or_default()
    }

    pub fn reconcile(&self) -> Result<(), StoreError> {
        self.require_initialised()?;
        let disk = self.walk_disk();
        self.with_conn(|conn| {
            for (path, node_id, parent, depth) in &disk {
                let exists: bool = conn.query_row("SELECT COUNT(*) FROM nodes WHERE id=?1", params![node_id], |r| r.get::<_, i64>(0))? > 0;
                if !exists {
                    let has_children = !Self::child_dirs(path).is_empty();
                    let status = if has_children { SPLIT } else { PENDING };
                    let n = Node { id: node_id.clone(), path: path.clone(), parent: parent.clone(), depth: *depth,
                        status: status.to_string(), goal: Self::goal_on_disk(path), summary: String::new(),
                        depends_on: vec![], dep_fp: "{}".into() };
                    Self::insert_node(conn, &n)?;
                }
            }
            Ok(())
        })
    }

    pub fn walk(&self) -> Result<Vec<Node>, StoreError> {
        self.require_initialised()?;
        let disk = self.walk_disk();
        let mut nodes = Vec::new();
        for (path, node_id, parent, depth) in &disk {
            let (status, goal, summary, deps_s, dep_fp): (String, String, String, String, String) = self.with_conn(|conn| {
                let r = conn.query_row("SELECT status,goal,summary,depends_on,dep_fp FROM nodes WHERE id=?1", params![node_id],
                    |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?)));
                match r { Ok(v) => Ok(v), Err(_) => Ok((String::new(),String::new(),String::new(),String::new(),String::new())) }
            })?;
            let deps: Vec<String> = serde_json::from_str(&deps_s).unwrap_or_default();
            nodes.push(Node { id: node_id.clone(), path: path.clone(), parent: parent.clone(), depth: *depth,
                status: if status.is_empty() { PENDING.to_string() } else { status },
                goal: if goal.is_empty() { Self::goal_on_disk(path) } else { goal },
                summary, depends_on: deps, dep_fp });
        }
        Ok(nodes)
    }

    #[allow(dead_code)]
    pub fn get(&self, node_id: &str) -> std::result::Result<Node, StoreError> {
        self.walk()?.into_iter().find(|n| n.id == node_id)
            .ok_or_else(|| StoreError::Other(format!("node {node_id:?} not found")))
    }

    pub fn children_of(&self, node: &Node) -> Result<Vec<Node>, StoreError> {
        Ok(self.walk()?.into_iter().filter(|n| n.parent.as_deref() == Some(&node.id)).collect())
    }

    pub fn ancestors(&self, node: &Node) -> Result<Vec<Node>, StoreError> {
        let by_id: std::collections::HashMap<_, _> = self.walk()?.into_iter().map(|n| (n.id.clone(), n)).collect();
        let mut chain = Vec::new();
        let mut cur = by_id.get(&node.id);
        while let Some(n) = cur {
            if let Some(ref pid) = n.parent {
                if let Some(p) = by_id.get(pid) { chain.push(p.clone()); cur = Some(p); } else { break; }
            } else { break; }
        }
        chain.reverse();
        Ok(chain)
    }

    pub fn budget_enabled(&self) -> bool {
        self.with_conn(|c| Ok(c.query_row("SELECT COUNT(*) FROM budget WHERE node_id=?1", params![ROOT_ID], |r| r.get::<_,i64>(0))?))
            .map(|n| n > 0).unwrap_or(false)
    }

    pub fn split_fee(&self) -> i64 {
        std::env::var("FRACTAL_SPLIT_FEE").ok().and_then(|s| s.parse().ok()).unwrap_or(200)
    }

    pub fn budget_remaining(&self, node_id: &str) -> Result<i64, StoreError> {
        self.with_conn(|conn| {
            let (a, c, f, ch): (i64, i64, i64, i64) = conn.query_row(
                "SELECT allowance,calls,fee_paid,children FROM budget WHERE node_id=?1", params![node_id],
                |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
            Ok(a - c - f - ch - self.split_fee())
        })
    }

    #[allow(dead_code)]
    pub fn debit_call(&self, node: &Node, tokens: i64) -> Result<(), StoreError> {
        if tokens <= 0 || !self.budget_enabled() { return Ok(()); }
        self.with_conn(|conn| {
            conn.execute("UPDATE budget SET calls=calls+?1,debits=debits+?1 WHERE node_id=?2", params![tokens, node.id])?;
            Ok(())
        })
    }

    fn next_global_id(&self) -> Result<String, StoreError> {
        self.with_conn(|conn| {
            let c: i64 = conn.query_row("SELECT COUNT(*) FROM global_entries", [], |r| r.get(0))?;
            Ok(format!("global-{:03}", c + 1))
        })
    }

    pub fn note_global(&self, entry_type: &str, content: &str, supersedes: &str) -> Result<String, StoreError> {
        let eid = self.next_global_id()?;
        let stamp = now();
        if !supersedes.is_empty() {
            self.with_conn(|conn| { conn.execute("UPDATE global_entries SET superseded=1 WHERE id=?1", params![supersedes])?; Ok(()) })?;
        }
        self.with_conn(|conn| {
            conn.execute("INSERT INTO global_entries(id,type,content,supersedes,created_at) VALUES(?1,?2,?3,?4,?5)",
                params![eid, entry_type, content, supersedes, &stamp])?;
            Ok(())
        })?;
        fs::create_dir_all(&self.global_dir)?;
        fs::write(self.global_dir.join(format!("{eid}.md")), format!("# {entry_type}: {eid}\n\n{content}\n"))?;
        Ok(eid)
    }

    pub fn retrieve_global(&self, query: &str, k: usize) -> Result<Vec<GlobalEntry>, StoreError> {
        let entries: Vec<GlobalEntry> = self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT * FROM global_entries WHERE superseded=0")?;
            let rows = stmt.query_map([], |row| Ok(GlobalEntry {
                id: row.get(0)?, entry_type: row.get(1)?, content: row.get(2)?,
                superseded: row.get::<_,i64>(3)? != 0, supersedes: row.get::<_,String>(4).unwrap_or_default(),
                created_at: row.get(5)?,
            }))?;
            Ok(rows.collect::<std::result::Result<Vec<_>,_>>()?)
        })?;
        if entries.is_empty() { return Ok(vec![]); }
        let ql = query.to_lowercase();
        let keywords: Vec<&str> = ql.split_whitespace().filter(|w| w.len() > 1).collect();
        let mut scored: Vec<(i64, GlobalEntry)> = entries.into_iter().map(|e| {
            let cl = e.content.to_lowercase();
            let s = keywords.iter().filter(|kw| cl.contains(*kw)).count() as i64;
            (s, e)
        }).collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        let top: Vec<GlobalEntry> = scored.into_iter().take(k).map(|(_, e)| e).collect();
        Ok(top)
    }

    #[allow(dead_code)]
    pub fn has_running(&self) -> Result<bool, StoreError> {
        self.with_conn(|c| Ok(c.query_row("SELECT COUNT(*) FROM nodes WHERE status=?1", params![RUNNING], |r| r.get::<_,i64>(0))?)).map(|n| n > 0)
    }

    pub fn generate_digest(&self) -> Result<String, StoreError> {
        self.reconcile()?;
        let nodes = self.walk()?;
        let tag_re = Regex::new(r"\[N:([A-Za-z0-9_]+)\]").unwrap();
        let mut done = Vec::new();
        let mut blocked = Vec::new();
        let mut pending = Vec::new();
        for n in &nodes {
            let tag = tag_re.find(&n.goal).map(|m| format!(" [N:{}]", m.as_str()));
            let label = format!("{} [{}]{} {}", n.id, n.status, tag.unwrap_or_default(), n.goal);
            match n.status.as_str() {
                COMPLETE => done.push(label),
                FAILED | SUSPENDED => blocked.push(label),
                _ => pending.push(label),
            }
        }
        let mut out = String::from("# Digest\n\n## Done\n");
        if done.is_empty() { out.push_str("- (no completed tasks)\n"); } else { for d in &done { out.push_str(&format!("- {d}\n")); } }
        out.push_str("\n## Blocked\n");
        if blocked.is_empty() { out.push_str("- (no blocked tasks)\n"); } else { for b in &blocked { out.push_str(&format!("- {b}\n")); } }
        out.push_str("\n## Next\n");
        if pending.is_empty() { out.push_str("- All tasks completed. No pending work.\n"); } else { for n in &pending { out.push_str(&format!("- {n}\n")); } }
        Ok(out)
    }
}

impl Drop for Store {
    fn drop(&mut self) { self.close(); }
}
