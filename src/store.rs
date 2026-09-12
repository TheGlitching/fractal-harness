use chrono::Utc;
use rusqlite::{params, Connection};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const ROOT_ID: &str = "root";

const TREE_DIRNAME: &str = "tree";
const GLOBAL_DIRNAME: &str = "global";
const STATE_DIRNAME: &str = ".fractal";
const UNIFIED_DIRNAME: &str = "dist";
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
    id TEXT PRIMARY KEY,
    parent TEXT,
    depth INTEGER NOT NULL,
    status TEXT NOT NULL,
    goal TEXT NOT NULL DEFAULT '',
    summary TEXT NOT NULL DEFAULT '',
    depends_on TEXT NOT NULL DEFAULT '[]',
    dep_fp TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS global_entries (
    id TEXT PRIMARY KEY,
    entry_type TEXT NOT NULL,
    content TEXT NOT NULL,
    superseded INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS budget (
    node_id TEXT PRIMARY KEY,
    allowance INTEGER NOT NULL,
    calls INTEGER NOT NULL DEFAULT 0,
    debits INTEGER NOT NULL DEFAULT 0,
    fee_paid INTEGER NOT NULL DEFAULT 0,
    children INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS steer_queue (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    command TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at TEXT NOT NULL
);
"#;

fn now() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
}

/// A small, dependency-free, stable 64-bit digest used for dependency
/// fingerprints. Not cryptographic; only needs to change when a deliverable does.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[derive(Debug, Clone, Default)]
pub struct Contract {
    pub goal: String,
    pub acceptance_criteria: Vec<String>,
    pub interfaces: Vec<String>,
    pub constraints: Vec<String>,
    #[allow(dead_code)]
    pub id: String,
    pub depends_on: Vec<String>,
    /// Shell commands that must exit 0 before this node may complete.
    /// Explicit gates apply at every scope, including leaves.
    pub verification: Vec<String>,
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
             ## id\n\n{model_id}\n\
             ## Goal\n\n{goal}\n\n\
             ## Acceptance criteria\n\n{ac}\
             ## Interfaces\n\n{iface}\
             ## Inherited constraints\n\n{cons}\
             ## depends_on\n\n{deps}\
             ## verification\n\n{verif}",
            nid = node_id,
            p = parent.unwrap_or("(none)"),
            depth = depth,
            model_id = self.id,
            goal = self.goal.trim(),
            ac = bullets(&self.acceptance_criteria),
            iface = bullets(&self.interfaces),
            cons = bullets(&self.constraints),
            deps = bullets(&self.depends_on),
            verif = bullets(&self.verification),
        )
    }

    pub fn parse(text: &str) -> Self {
        let mut sections: std::collections::HashMap<String, Vec<String>> = Default::default();
        let mut current: Option<String> = None;
        for line in text.lines() {
            if let Some(stripped) = line.strip_prefix("## ") {
                current = Some(stripped.trim().to_lowercase());
                sections
                    .entry(current.clone().unwrap_or_default())
                    .or_default();
            } else if let Some(ref cur) = current {
                sections
                    .entry(cur.clone())
                    .or_default()
                    .push(line.to_string());
            }
        }
        let unbullet = |key: &str| -> Vec<String> {
            sections
                .get(key)
                .map(|lines| {
                    lines
                        .iter()
                        .map(|l| l.trim())
                        .filter(|l| l.starts_with('-'))
                        .map(|l| l[1..].trim().to_string())
                        .filter(|l| !l.is_empty() && l != "(none stated)")
                        .collect()
                })
                .unwrap_or_default()
        };
        let body = |key: &str| -> String {
            sections
                .get(key)
                .map(|lines| lines.join("\n").trim().to_string())
                .unwrap_or_default()
        };
        let mut c = Contract {
            goal: body("goal"),
            acceptance_criteria: unbullet("acceptance criteria"),
            interfaces: unbullet("interfaces"),
            constraints: unbullet("inherited constraints"),
            id: body("id"),
            depends_on: unbullet("depends_on"),
            verification: unbullet("verification"),
            allocation: 0,
        };
        if c.goal.is_empty() {
            c.goal = body("goal");
        }
        c
    }
}

#[derive(Debug, Clone, Default)]
pub struct Node {
    pub id: String,
    pub path: PathBuf,
    pub parent: Option<String>,
    pub depth: i64,
    pub status: String,
    pub goal: String,
    pub summary: String,
    pub depends_on: Vec<String>,
    #[allow(dead_code)]
    pub dep_fp: String,
}

impl Node {
    pub fn contract_path(&self) -> PathBuf {
        self.path.join(CONTRACT_FILENAME)
    }
    pub fn decisions_path(&self) -> PathBuf {
        self.path.join(DECISIONS_FILENAME)
    }
    pub fn log_path(&self) -> PathBuf {
        self.path.join(LOG_DIRNAME).join(EVENTS_FILENAME)
    }
    pub fn log_dir(&self) -> PathBuf {
        self.path.join(LOG_DIRNAME)
    }
    pub fn artifacts_dir(&self) -> PathBuf {
        self.path.join(ARTIFACTS_DIRNAME)
    }
    pub fn children_dir(&self) -> PathBuf {
        self.path.join(CHILDREN_DIRNAME)
    }
    pub fn contract(&self) -> Contract {
        if let Ok(text) = fs::read_to_string(self.contract_path()) {
            Contract::parse(&text)
        } else {
            Contract {
                goal: self.goal.clone(),
                acceptance_criteria: vec![],
                interfaces: vec![],
                constraints: vec![],
                id: self.id.clone(),
                depends_on: self.depends_on.clone(),
                verification: vec![],
                allocation: 0,
            }
        }
    }
    pub fn find_artifacts(&self) -> Vec<PathBuf> {
        let mut results = Vec::new();
        let dir = self.artifacts_dir();
        if dir.exists() {
            if let Ok(entries) = fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_file() {
                        results.push(p);
                    }
                }
            }
        }
        results
    }
}

#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    NotInitialised,
    Json(serde_json::Error),
    Other(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sqlite(e) => write!(f, "sqlite: {e}"),
            StoreError::Io(e) => write!(f, "io: {e}"),
            StoreError::NotInitialised => write!(f, "project not initialised"),
            StoreError::Json(e) => write!(f, "json: {e}"),
            StoreError::Other(s) => write!(f, "{s}"),
        }
    }
}
impl std::error::Error for StoreError {}
impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e)
    }
}
impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}
impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        StoreError::Json(e)
    }
}

pub struct Store {
    pub root: PathBuf,
    pub tree_dir: PathBuf,
    pub global_dir: PathBuf,
    pub state_dir: PathBuf,
    db_path: PathBuf,
    conn: Mutex<Option<Connection>>,
}

impl Store {
    pub fn new(project_root: &Path) -> Self {
        let root = project_root.to_path_buf();
        let tree_dir = root.join(TREE_DIRNAME);
        let global_dir = root.join(GLOBAL_DIRNAME);
        let state_dir = root.join(STATE_DIRNAME);
        let db_path = state_dir.join(INDEX_FILENAME);
        Store {
            root,
            tree_dir,
            global_dir,
            state_dir,
            db_path,
            conn: Mutex::new(None),
        }
    }

    fn with_conn<F, R>(&self, f: F) -> Result<R, StoreError>
    where
        F: FnOnce(&Connection) -> Result<R, StoreError>,
    {
        let mut guard = self.conn.lock().unwrap();
        if guard.is_none() {
            let conn = Connection::open(&self.db_path)?;
            // A rollback journal plus synchronous=FULL means a committed status
            // has reached the disk before the next model call starts. That is
            // what makes a SIGKILL survivable: after a crash the index may lag
            // the filesystem, but a status it reports as durable really is.
            // WAL/NORMAL is faster but can lose the last commits on power loss.
            conn.execute_batch(
                "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
            )?;
            *guard = Some(conn);
        }
        f(guard.as_ref().unwrap())
    }

    pub fn require_initialised(&self) -> Result<(), StoreError> {
        if !self.db_path.exists() {
            return Err(StoreError::NotInitialised);
        }
        Ok(())
    }

    /// A per-subtree ledger exists when the root was initialised with a
    /// `FRACTAL_BUDGET` (or when one is supplied directly in tests). The env
    /// var is only consulted as a fallback so a resumed project whose ledger
    /// is already on disk stays economically bounded even when the variable is
    /// not exported in the new shell.
    pub fn budget_enabled(&self) -> bool {
        if std::env::var("FRACTAL_BUDGET").is_ok() {
            return true;
        }
        if !self.db_path.exists() {
            return false;
        }
        self.with_conn(|conn| {
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM budget WHERE node_id=?1",
                params![ROOT_ID],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        })
        .unwrap_or(false)
    }

    pub fn split_fee(&self) -> i64 {
        std::env::var("FRACTAL_SPLIT_FEE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(200)
    }

    pub fn init(&self, goal: &str) -> Result<Node, StoreError> {
        let budget = match std::env::var("FRACTAL_BUDGET") {
            Ok(raw) => Some(raw.parse().unwrap_or(100_000)),
            Err(_) => None,
        };
        self.init_with_budget(goal, budget)
    }

    /// `budget` of `Some(n)` creates the root ledger with allowance `n`; `None`
    /// (the default) leaves the tree under the hard depth cap. Kept separate
    /// from `init` so tests can create a bounded tree without mutating the
    /// process-global environment.
    pub fn init_with_budget(&self, goal: &str, budget: Option<i64>) -> Result<Node, StoreError> {
        fs::create_dir_all(&self.tree_dir)?;
        fs::create_dir_all(&self.global_dir)?;
        fs::create_dir_all(&self.state_dir)?;

        self.with_conn(|conn| {
            conn.execute_batch(SCHEMA)?;
            Ok(())
        })?;

        let root_node = Node {
            id: ROOT_ID.to_string(),
            path: self.tree_dir.join(ROOT_ID),
            parent: None,
            depth: 1,
            status: PENDING.to_string(),
            goal: goal.to_string(),
            summary: String::new(),
            depends_on: vec![],
            dep_fp: "{}".into(),
        };

        let contract = Contract {
            goal: goal.to_string(),
            acceptance_criteria: vec![
                "the goal is delivered in full".to_string(),
                "all the pieces are assembled into one working whole, not left as \
                 independent modules"
                    .to_string(),
                "the project's own build, typecheck and test commands pass".to_string(),
            ],
            interfaces: vec![],
            constraints: vec![],
            id: String::new(),
            depends_on: vec![],
            // The root always answers to the project's real commands.
            verification: crate::verify::detect_gates(&self.root),
            allocation: 0,
        };

        Self::materialise_node(&root_node, &contract)?;

        let stamp = now();
        self.with_conn(|conn| {
            Self::insert_node(conn, &root_node)?;
            if let Some(initial_budget) = budget {
                conn.execute(
                    "INSERT OR REPLACE INTO budget (node_id, allowance, calls, debits, fee_paid, children) VALUES (?1, ?2, 0, 0, 0, 0)",
                    params![ROOT_ID, initial_budget],
                )?;
            }
            conn.execute(
                "INSERT INTO nodes(id, parent, depth, status, goal, summary, depends_on, dep_fp, created_at, updated_at) \
                 VALUES(?1, NULL, 1, ?2, ?3, '', '[]', '{}', ?4, ?4) \
                 ON CONFLICT(id) DO UPDATE SET status=?2, goal=?3, updated_at=?4",
                params![ROOT_ID, PENDING, goal, stamp],
            )?;
            Ok(())
        })?;

        self.append_decision(&root_node, "node created")?;
        Ok(root_node)
    }

    fn materialise_node(node: &Node, contract: &Contract) -> Result<(), StoreError> {
        fs::create_dir_all(&node.path)?;
        fs::create_dir_all(node.log_path().parent().unwrap())?;
        fs::create_dir_all(node.artifacts_dir())?;
        fs::create_dir_all(node.children_dir())?;

        let contract_content = contract.render(&node.id, node.depth, node.parent.as_deref());
        fs::write(node.contract_path(), contract_content)?;

        if !node.decisions_path().exists() {
            let header = format!(
                "# Decisions: {}\n\nAppend-only semantic memory of this node.\n\n",
                node.id
            );
            fs::write(node.decisions_path(), header)?;
        }
        Ok(())
    }

    fn insert_node(conn: &Connection, node: &Node) -> Result<(), StoreError> {
        let stamp = now();
        let deps = serde_json::to_string(&node.depends_on)?;
        conn.execute(
            "INSERT OR IGNORE INTO nodes(id, parent, depth, status, goal, summary, depends_on, dep_fp, created_at, updated_at) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            params![
                node.id,
                node.parent,
                node.depth,
                node.status,
                node.goal,
                node.summary,
                deps,
                node.dep_fp,
                stamp,
            ],
        )?;
        Ok(())
    }

    pub fn add_children(
        &self,
        parent: &Node,
        contracts: &[Contract],
    ) -> Result<Vec<Node>, StoreError> {
        if contracts.is_empty() {
            return Ok(vec![]);
        }
        let existing_children = self.children_of(parent)?;
        let existing = existing_children.len();
        let mut children = Vec::new();
        let mut id_map: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        // Seed with children that already exist. A parent may split again later
        // to add a capability, and that new child legitimately depends on its
        // existing siblings. Without these entries such a dependency resolves to
        // nothing, gets stripped with only a warning, and the new child becomes
        // immediately runnable - running before the work it depends on.
        for existing_child in &existing_children {
            id_map.insert(existing_child.id.clone(), existing_child.id.clone());
            if let Some(suffix) = existing_child.id.rsplit('-').next() {
                id_map.insert(suffix.to_string(), existing_child.id.clone());
            }
        }

        for (i, c) in contracts.iter().enumerate() {
            let cid = format!("{}-{:02}", parent.id, existing + i + 1);
            if !c.id.is_empty() {
                id_map.insert(c.id.clone(), cid.clone());
            }
            id_map.insert(format!("{}", i + 1), cid.clone());
            id_map.insert(format!("{:02}", i + 1), cid.clone());
            id_map.insert(cid.clone(), cid.clone());
        }

        let inherited_constraints = parent.contract().constraints;

        for (i, c) in contracts.iter().enumerate() {
            let cid = format!("{}-{:02}", parent.id, existing + i + 1);
            // Resolve each proposed dependency against the known sibling ids.
            // A name that resolves to nothing is preserved verbatim rather than
            // dropped: silently stripping it would make the child immediately
            // runnable before the work it says it needs, which is worse than an
            // unresolvable edge the scheduler refuses to schedule.
            let resolved_deps: Vec<String> = c
                .depends_on
                .iter()
                .map(|dep| id_map.get(dep).cloned().unwrap_or_else(|| dep.clone()))
                .collect();

            let mut child_contract = c.clone();
            for ic in &inherited_constraints {
                if !child_contract.constraints.contains(ic) {
                    child_contract.constraints.push(ic.clone());
                }
            }

            let cnode = Node {
                id: cid.clone(),
                path: parent.children_dir().join(&cid),
                parent: Some(parent.id.clone()),
                depth: parent.depth + 1,
                status: PENDING.to_string(),
                goal: child_contract.goal.clone(),
                summary: String::new(),
                depends_on: resolved_deps,
                dep_fp: "{}".into(),
            };
            Self::materialise_node(&cnode, &child_contract)?;
            children.push(cnode);
        }

        let budget = self.budget_enabled();
        let explicit: Vec<i64> = contracts.iter().map(|c| c.allocation.max(0)).collect();
        let fee = self.split_fee();
        // A subtask that omits `allocation` - which the split template never asks
        // for - must not be created with a zero allowance and fail "token budget
        // exhausted" before it can run. Undefined allocations inherit an equal
        // share of the parent's remaining allowance, so the parent's budget still
        // bounds the whole subtree.
        let parent_remaining = if budget {
            self.budget_remaining(&parent.id).unwrap_or(0).max(0)
        } else {
            0
        };
        let explicit_total: i64 = explicit.iter().sum();
        let unallocated = explicit.iter().filter(|a| **a == 0).count() as i64;
        let share = if unallocated > 0 {
            ((parent_remaining - explicit_total).max(0)) / unallocated
        } else {
            0
        };
        let allocations: Vec<i64> = explicit
            .iter()
            .map(|a| if *a == 0 { share } else { *a })
            .collect();
        let stamp = now();
        self.with_conn(|conn| {
            for ch in &children {
                Self::insert_node(conn, ch)?;
            }
            if budget {
                // Charge the split to the parent and open a ledger row for each
                // child carrying the allocation the parent granted it. Without
                // this the allocation is cosmetic and a child could recurse on
                // unbounded credit.
                let granted: i64 = allocations.iter().sum();
                conn.execute(
                    "UPDATE budget SET fee_paid=fee_paid+?1, children=children+?2, debits=debits+?1 WHERE node_id=?3",
                    params![fee, granted, parent.id],
                )?;
                for (ch, allocation) in children.iter().zip(allocations.iter()) {
                    conn.execute(
                        "INSERT OR IGNORE INTO budget (node_id, allowance, calls, debits, fee_paid, children) VALUES (?1, ?2, 0, 0, 0, 0)",
                        params![ch.id, allocation],
                    )?;
                }
            }
            conn.execute(
                "UPDATE nodes SET status=?1,updated_at=?2 WHERE id=?3",
                params![SPLIT, &stamp, parent.id],
            )?;
            Ok(())
        })?;
        Ok(children)
    }

    pub fn complete(
        &self,
        node: &Node,
        summary: &str,
        deliverable: &str,
        artifacts: &[(String, String)],
    ) -> Result<(), StoreError> {
        self.write_artifacts(node, artifacts, deliverable)?;
        self.sync_unified_workspace(node, artifacts)?;
        let stamp = now();
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE nodes SET status=?1,summary=?2,updated_at=?3 WHERE id=?4",
                params![COMPLETE, summary.trim(), &stamp, node.id],
            )?;
            Ok(())
        })?;
        // Snapshot the dependencies' fingerprints at acceptance. If one of them
        // is later reopened and changes, this dependent is detected as stale and
        // re-run rather than trusted forever.
        self.record_dependencies(node)?;
        Ok(())
    }

    pub fn set_status(&self, node: &Node, status: &str) -> Result<(), StoreError> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE nodes SET status=?1,updated_at=?2 WHERE id=?3",
                params![status, &now(), node.id],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    pub fn write_artifacts(
        &self,
        node: &Node,
        artifacts: &[(String, String)],
        deliverable: &str,
    ) -> Result<(), StoreError> {
        fs::create_dir_all(node.artifacts_dir())?;
        let project_root = self.tree_dir.parent().unwrap_or(&self.tree_dir);
        let mut written = false;
        for (p, c) in artifacts {
            if c.trim().is_empty() {
                continue;
            }
            let rel = Self::safe_path(p);

            // 1. Write to node's artifact archive for memory & trace
            let target_node = node.artifacts_dir().join(&rel);
            if let Some(parent) = target_node.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&target_node, c)?;

            // 2. Write directly to the repository root for real unified in-place project code
            let target_root = project_root.join(&rel);
            if let Some(parent) = target_root.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&target_root, c)?;

            written = true;
        }
        if !written && !deliverable.trim().is_empty() {
            fs::write(node.artifacts_dir().join("deliverable.md"), deliverable)?;
        }
        Ok(())
    }

    fn safe_path(p: &str) -> PathBuf {
        let p = p.strip_prefix("artifacts/").unwrap_or(p);
        let mut out = PathBuf::new();
        for comp in Path::new(p).components() {
            if let std::path::Component::Normal(c) = comp {
                out.push(c);
            }
        }
        // If a leaf gave a bare file (e.g. "CategoryFilterBar.tsx" or "Component.test.tsx")
        // route it automatically into src/ or tests/ if not already qualified
        if out.components().count() == 1 {
            let name = out.to_string_lossy().to_string();
            if name.ends_with(".test.ts")
                || name.ends_with(".test.tsx")
                || name.ends_with(".spec.ts")
                || name.ends_with(".spec.tsx")
            {
                return PathBuf::from("tests").join(&name);
            } else if name.ends_with(".ts")
                || name.ends_with(".tsx")
                || name.ends_with(".css")
                || name.ends_with(".html")
            {
                // Keep root configs at root
                if ![
                    "vite.config.ts",
                    "tailwind.config.ts",
                    "postcss.config.js",
                    "tsconfig.json",
                    "package.json",
                ]
                .contains(&name.as_str())
                {
                    return PathBuf::from("src").join(&name);
                }
            }
        }
        out
    }
    pub fn unified_dir(&self) -> PathBuf {
        self.tree_dir
            .parent()
            .unwrap_or(&self.tree_dir)
            .join(UNIFIED_DIRNAME)
    }

    pub fn sync_unified_workspace(
        &self,
        node: &Node,
        artifacts: &[(String, String)],
    ) -> Result<(), StoreError> {
        let dist = self.unified_dir();
        fs::create_dir_all(&dist)?;

        for (p, c) in artifacts {
            if c.trim().is_empty() {
                continue;
            }
            let rel = Self::safe_path(p);
            let target = dist.join(&rel);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&target, c)?;
        }

        // Also copy any existing disk artifacts from this node
        for art in node.find_artifacts() {
            if let Ok(rel) = art.strip_prefix(node.artifacts_dir()) {
                let target = dist.join(rel);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                if art.is_file() {
                    let _ = fs::copy(&art, &target);
                }
            }
        }
        Ok(())
    }

    pub fn export_workspace(&self, target_dir: &Path) -> Result<usize, StoreError> {
        fs::create_dir_all(target_dir)?;
        let nodes = self.walk()?;
        let mut count = 0;
        for n in &nodes {
            if n.status == COMPLETE {
                for art in n.find_artifacts() {
                    if let Ok(rel) = art.strip_prefix(n.artifacts_dir()) {
                        if rel.to_string_lossy() == "deliverable.md" {
                            continue;
                        }
                        let dest = target_dir.join(rel);
                        if let Some(parent) = dest.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        if art.is_file() && fs::copy(&art, &dest).is_ok() {
                            count += 1;
                        }
                    }
                }
            }
        }
        Ok(count)
    }

    pub fn append_decision(&self, node: &Node, text: &str) -> Result<(), StoreError> {
        let line = format!("- {} {}\n", now(), text.trim());
        let path = node.decisions_path();
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        file.write_all(line.as_bytes())?;
        Ok(())
    }

    pub fn append_log(&self, node: &Node, record: &serde_json::Value) -> Result<(), StoreError> {
        let mut obj = record.clone();
        if let Some(m) = obj.as_object_mut() {
            m.insert("node".into(), serde_json::Value::String(node.id.clone()));
            m.insert("at".into(), serde_json::Value::String(now()));
        }
        let line = serde_json::to_string(&obj)?;
        let path = node.log_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        file.write_all(format!("{line}\n").as_bytes())?;
        Ok(())
    }

    pub fn add_constraint_and_propagate(
        &self,
        origin_node_id: &str,
        constraint: &str,
    ) -> Result<usize, StoreError> {
        let nodes = self.walk()?;
        let target = nodes
            .iter()
            .find(|n| n.id == origin_node_id)
            .ok_or_else(|| StoreError::Other(format!("node {origin_node_id:?} not found")))?;

        let mut affected = 0;

        // Add to target's contract
        let mut c = target.contract();
        if !c.constraints.iter().any(|x| x.trim() == constraint.trim()) {
            c.constraints.push(constraint.trim().to_string());
            fs::write(
                target.contract_path(),
                c.render(&target.id, target.depth, target.parent.as_deref()),
            )?;
            self.append_decision(target, &format!("added constraint: {}", constraint.trim()))?;
            affected += 1;
        }

        // Propagate to all pending / running descendants
        let prefix = format!("{}/", target.path.to_string_lossy());
        for n in &nodes {
            if n.id != target.id && n.path.to_string_lossy().starts_with(&prefix) {
                let mut nc = n.contract();
                if !nc.constraints.iter().any(|x| x.trim() == constraint.trim()) {
                    nc.constraints.push(constraint.trim().to_string());
                    fs::write(
                        n.contract_path(),
                        nc.render(&n.id, n.depth, n.parent.as_deref()),
                    )?;
                    self.append_decision(
                        n,
                        &format!(
                            "inherited constraint from {origin_node_id}: {}",
                            constraint.trim()
                        ),
                    )?;
                    affected += 1;
                }
            }
        }

        Ok(affected)
    }

    // -- escalation support -------------------------------------------------
    // Ported from the Python scheduler's suspend -> reopen-owner -> resolve
    // cycle. These are the store-side primitives the scheduler's escalation
    // handling calls; none of them injects a challenged assumption as an
    // accepted constraint before the owner has ruled.

    /// Replace an inherited constraint `old` with `new` in `owner` and every
    /// descendant. This is the `amend` resolution (SPEC 4.3): the branch resumes
    /// under amended terms rather than the falsified one.
    pub fn amend_inherited_constraint(
        &self,
        owner: &Node,
        old: &str,
        new: &str,
    ) -> Result<usize, StoreError> {
        if old.trim().is_empty() || new.trim().is_empty() {
            return Ok(0);
        }
        let nodes = self.walk()?;
        let prefix = format!("{}/", owner.path.to_string_lossy());
        let mut changed = 0;
        for n in nodes
            .iter()
            .filter(|n| n.id == owner.id || n.path.to_string_lossy().starts_with(&prefix))
        {
            let mut c = n.contract();
            let mut modified = false;
            for constraint in c.constraints.iter_mut() {
                if constraint.trim() == old.trim() {
                    *constraint = new.trim().to_string();
                    modified = true;
                }
            }
            if modified {
                fs::write(
                    n.contract_path(),
                    c.render(&n.id, n.depth, n.parent.as_deref()),
                )?;
                self.append_decision(n, &format!("amended constraint '{old}' -> '{new}'"))?;
                changed += 1;
            }
        }
        Ok(changed)
    }

    /// Expose a named interface on a node's contract. This satisfies a
    /// discovered dependency discovered during escalation (`amend` targeting a
    /// sibling), so the consumer can proceed against a stated interface.
    pub fn add_interface(&self, node: &Node, interface: &str) -> Result<bool, StoreError> {
        let iface = interface.trim();
        if iface.is_empty() {
            return Ok(false);
        }
        let mut c = node.contract();
        if c.interfaces.iter().any(|i| i.trim() == iface) {
            return Ok(false);
        }
        c.interfaces.push(iface.to_string());
        fs::write(
            node.contract_path(),
            c.render(&node.id, node.depth, node.parent.as_deref()),
        )?;
        self.append_decision(node, &format!("added interface: {iface}"))?;
        Ok(true)
    }

    /// Add a dependency edge `node -> dep_id`. This is the `depends_on`
    /// resolution: the escalating node cannot proceed until the sibling it
    /// discovered it needs has been accepted.
    pub fn add_depends_on(&self, node: &Node, dep_id: &str) -> Result<bool, StoreError> {
        let dep_id = dep_id.trim();
        if dep_id.is_empty() || node.depends_on.iter().any(|d| d == dep_id) {
            return Ok(false);
        }
        let mut deps = node.depends_on.clone();
        deps.push(dep_id.to_string());
        let encoded = serde_json::to_string(&deps)?;
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE nodes SET depends_on=?1,updated_at=?2 WHERE id=?3",
                params![encoded, &now(), node.id],
            )?;
            Ok(())
        })?;
        self.append_decision(node, &format!("added dependency: {dep_id}"))?;
        Ok(true)
    }

    /// Remove a node's directory and index rows. Used by `replan` pruning.
    pub fn delete_node(&self, node: &Node) -> Result<(), StoreError> {
        let _ = fs::remove_dir_all(&node.path);
        self.with_conn(|conn| {
            conn.execute("DELETE FROM nodes WHERE id=?1", params![node.id])?;
            conn.execute("DELETE FROM budget WHERE node_id=?1", params![node.id])?;
            Ok(())
        })?;
        Ok(())
    }

    /// Preserve a pruned child's episodic trace in the ancestor's `log/`, so
    /// work discarded as output is not lost as information (SPEC 4.3 replan).
    pub fn compact_child_trace(&self, parent: &Node, child: &Node) -> Result<(), StoreError> {
        let log_dir = parent.log_dir();
        fs::create_dir_all(&log_dir)?;
        let mut lines = vec![
            format!("# Compacted trace of pruned child {}", child.id),
            format!("- goal: {}", child.goal),
            format!(
                "- summary: {}",
                if child.summary.trim().is_empty() {
                    "(none)"
                } else {
                    child.summary.trim()
                }
            ),
        ];
        if let Ok(decisions) = fs::read_to_string(child.decisions_path()) {
            lines.push("- decisions:".into());
            for line in decisions.lines() {
                lines.push(format!("    {line}"));
            }
        }
        if let Ok(events) = fs::read_to_string(child.log_path()) {
            lines.push("- events:".into());
            for line in events.lines() {
                lines.push(format!("    {line}"));
            }
        }
        fs::write(
            log_dir.join(format!("compacted-{}.md", child.id)),
            format!("{}\n", lines.join("\n")),
        )?;
        Ok(())
    }

    pub fn enqueue_steer(&self, command: &str, payload: &str) -> Result<(), StoreError> {
        let stamp = now();
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO steer_queue(command, payload, created_at) VALUES(?1, ?2, ?3)",
                params![command, payload, &stamp],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    pub fn drain_steer_queue(&self) -> Result<Vec<(i64, String, String)>, StoreError> {
        self.with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT id, command, payload FROM steer_queue ORDER BY id ASC")?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            let items: Vec<(i64, String, String)> = rows.collect::<Result<Vec<_>, _>>()?;
            if !items.is_empty() {
                conn.execute("DELETE FROM steer_queue", [])?;
            }
            Ok(items)
        })
    }

    fn walk_disk(&self) -> Vec<(PathBuf, String, Option<String>, i64)> {
        let mut results = Vec::new();
        let root = self.tree_dir.join(ROOT_ID);
        if root.exists() {
            Self::walk_disk_rec(&root, ROOT_ID, None, 1, &mut results);
        }
        results
    }

    fn walk_disk_rec(
        dir: &Path,
        node_id: &str,
        parent: Option<&str>,
        depth: i64,
        out: &mut Vec<(PathBuf, String, Option<String>, i64)>,
    ) {
        out.push((
            dir.to_path_buf(),
            node_id.to_string(),
            parent.map(|s| s.to_string()),
            depth,
        ));
        let cdir = dir.join(CHILDREN_DIRNAME);
        for child_dir in Self::child_dirs(&cdir) {
            let cid = child_dir.file_name().unwrap().to_string_lossy().to_string();
            Self::walk_disk_rec(&child_dir, &cid, Some(node_id), depth + 1, out);
        }
    }

    fn child_dirs(dir: &Path) -> Vec<PathBuf> {
        let mut list = Vec::new();
        if let Ok(entries) = fs::read_dir(dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    list.push(p);
                }
            }
        }
        list.sort();
        list
    }

    fn goal_on_disk(node_dir: &Path) -> String {
        let cp = node_dir.join(CONTRACT_FILENAME);
        if let Ok(text) = fs::read_to_string(&cp) {
            Contract::parse(&text).goal
        } else {
            String::new()
        }
    }

    pub fn reconcile(&self) -> Result<(), StoreError> {
        self.require_initialised()?;
        let disk = self.walk_disk();
        self.with_conn(|conn| {
            // Snapshot the existing rows first: we adopt, repair and delete
            // against it, then drop whatever the filesystem no longer holds.
            let mut rows: std::collections::HashMap<String, (Option<String>, i64, String)> =
                std::collections::HashMap::new();
            {
                let mut stmt = conn.prepare("SELECT id,parent,depth,status FROM nodes")?;
                let mut iter = stmt.query([])?;
                while let Some(r) = iter.next()? {
                    rows.insert(
                        r.get::<_, String>(0)?,
                        (
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, String>(3)?,
                        ),
                    );
                }
            }

            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for (path, node_id, parent, depth) in &disk {
                seen.insert(node_id.clone());
                let has_children = !Self::child_dirs(&path.join(CHILDREN_DIRNAME)).is_empty();
                match rows.get(node_id) {
                    None => {
                        let status = if has_children { SPLIT } else { PENDING };
                        let n = Node {
                            id: node_id.clone(),
                            path: path.clone(),
                            parent: parent.clone(),
                            depth: *depth,
                            status: status.to_string(),
                            goal: Self::goal_on_disk(path),
                            summary: String::new(),
                            depends_on: vec![],
                            dep_fp: "{}".into(),
                        };
                        Self::insert_node(conn, &n)?;
                    }
                    Some((row_parent, row_depth, status)) => {
                        // A crash can leave a node `running` (or a freshly
                        // created one `pending`) that actually split before it
                        // died. The filesystem is authoritative for both the
                        // status and the topology.
                        let repaired = if status == RUNNING || status == PENDING {
                            if has_children {
                                SPLIT
                            } else {
                                PENDING
                            }
                        } else {
                            status.as_str()
                        };
                        if repaired != status || row_parent != parent || *row_depth != *depth {
                            conn.execute(
                                "UPDATE nodes SET status=?1,parent=?2,depth=?3,updated_at=?4 WHERE id=?5",
                                params![repaired, parent, depth, now(), node_id],
                            )?;
                        }
                    }
                }
            }

            for node_id in rows.keys().filter(|id| !seen.contains(*id)) {
                conn.execute("DELETE FROM nodes WHERE id=?1", params![node_id])?;
                conn.execute("DELETE FROM budget WHERE node_id=?1", params![node_id])?;
            }
            Ok(())
        })
    }

    pub fn walk(&self) -> Result<Vec<Node>, StoreError> {
        self.require_initialised()?;
        let disk = self.walk_disk();
        let mut nodes = Vec::new();
        for (path, node_id, parent, depth) in &disk {
            let (status, goal, summary, deps_s, dep_fp): (String, String, String, String, String) =
                self.with_conn(|conn| {
                    let r = conn.query_row(
                        "SELECT status,goal,summary,depends_on,dep_fp FROM nodes WHERE id=?1",
                        params![node_id],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, String>(2)?,
                                r.get::<_, String>(3)?,
                                r.get::<_, String>(4)?,
                            ))
                        },
                    );
                    match r {
                        Ok(v) => Ok(v),
                        Err(_) => Ok((
                            String::new(),
                            String::new(),
                            String::new(),
                            String::new(),
                            String::new(),
                        )),
                    }
                })?;
            let deps: Vec<String> = serde_json::from_str(&deps_s).unwrap_or_default();
            nodes.push(Node {
                id: node_id.clone(),
                path: path.clone(),
                parent: parent.clone(),
                depth: *depth,
                status: if status.is_empty() {
                    PENDING.to_string()
                } else {
                    status
                },
                goal: if goal.is_empty() {
                    Self::goal_on_disk(path)
                } else {
                    goal
                },
                summary,
                depends_on: deps,
                dep_fp,
            });
        }
        Ok(nodes)
    }

    pub fn get(&self, node_id: &str) -> std::result::Result<Node, StoreError> {
        self.walk()?
            .into_iter()
            .find(|n| n.id == node_id)
            .ok_or_else(|| StoreError::Other(format!("node {node_id:?} not found")))
    }

    pub fn children_of(&self, node: &Node) -> Result<Vec<Node>, StoreError> {
        let all = self.walk()?;
        Ok(all
            .into_iter()
            .filter(|n| n.parent.as_deref() == Some(&node.id))
            .collect())
    }

    pub fn generate_digest(&self) -> Result<String, StoreError> {
        let nodes = self.walk()?;
        let mut out = String::from("# Digest\n\n## Done\n");
        for d in nodes.iter().filter(|n| n.status == COMPLETE) {
            out.push_str(&format!(
                "- **{}**: {}\n",
                d.id,
                d.goal.lines().next().unwrap_or(&d.goal)
            ));
        }
        out.push_str("\n## Blocked\n");
        for b in nodes
            .iter()
            .filter(|n| n.status == SUSPENDED || n.status == FAILED)
        {
            out.push_str(&format!(
                "- **{}** ({}): {}\n",
                b.id,
                b.status,
                b.goal.lines().next().unwrap_or(&b.goal)
            ));
        }
        out.push_str("\n## Next\n");
        for p in nodes.iter().filter(|n| n.status == PENDING) {
            out.push_str(&format!(
                "- **{}**: {}\n",
                p.id,
                p.goal.lines().next().unwrap_or(&p.goal)
            ));
        }
        Ok(out)
    }

    pub fn retry(&self, node_id: &str) -> Result<usize, StoreError> {
        let nodes = self.walk()?;
        let target = nodes
            .iter()
            .find(|n| n.id == node_id)
            .ok_or_else(|| StoreError::Other(format!("node {node_id:?} not found")))?;
        let prefix = format!("{}/", target.path.to_string_lossy());
        let descendants: Vec<&Node> = nodes
            .iter()
            .filter(|n| n.id == node_id || n.path.to_string_lossy().starts_with(&prefix))
            .collect();
        let mut count = 0;
        for d in &descendants {
            if d.status == PENDING {
                continue;
            }
            self.set_status(d, PENDING)?;
            count += 1;
        }
        Ok(count)
    }
    /// Send specific children back to PENDING with a reason recorded on each.
    ///
    /// This is the escape hatch the tree previously lacked: when an integrating
    /// parent discovered that a child's work did not actually function, its only
    /// options were to keep retrying itself or to fail permanently. Neither
    /// re-engages the agent that owns the broken code. `reason` is appended to
    /// each child's decision log so the relaunched agent sees why it is back.
    pub fn reopen_children(
        &self,
        parent: &Node,
        child_ids: &[String],
        reason: &str,
    ) -> Result<Vec<String>, StoreError> {
        let nodes = self.walk()?;
        let mut reopened = Vec::new();

        for child_id in child_ids {
            let child = match nodes.iter().find(|n| &n.id == child_id) {
                Some(c) => c,
                None => continue,
            };
            // A parent may only reopen its own subtree.
            if child.parent.as_deref() != Some(parent.id.as_str()) {
                continue;
            }

            // Reset the child and everything beneath it, so a child that had
            // itself decomposed is re-derived rather than left half-stale.
            let prefix = format!("{}/", child.path.to_string_lossy());
            for node in nodes
                .iter()
                .filter(|n| n.id == child.id || n.path.to_string_lossy().starts_with(&prefix))
            {
                self.set_status(node, PENDING)?;
            }

            self.append_decision(
                child,
                &format!("reopened by {}: {}", parent.id, reason.trim()),
            )?;
            reopened.push(child.id.clone());
        }

        if !reopened.is_empty() {
            self.set_status(parent, SPLIT)?;
            self.append_decision(
                parent,
                &format!(
                    "reopened children {}: {}",
                    reopened.join(", "),
                    reason.trim()
                ),
            )?;
        }

        Ok(reopened)
    }
    /// The token allowance still spendable by `node_id`, with one split-fee set
    /// aside for the split it may still propose. A node with no ledger row has
    /// no budget: returning zero (rather than erroring) fails it closed.
    pub fn budget_remaining(&self, node_id: &str) -> Result<i64, StoreError> {
        use rusqlite::OptionalExtension;
        self.with_conn(|conn| {
            let row = conn
                .query_row(
                    "SELECT allowance,calls,fee_paid,children FROM budget WHERE node_id=?1",
                    params![node_id],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, i64>(3)?,
                        ))
                    },
                )
                .optional()?;
            match row {
                Some((a, c, f, ch)) => Ok(a - c - f - ch - self.split_fee()),
                None => Ok(0),
            }
        })
    }

    /// Charge a model call's actual usage to the node's ledger. Safe when no
    /// ledger exists: the update simply matches no rows.
    pub fn debit_call(&self, node: &Node, tokens: i64) -> Result<(), StoreError> {
        if tokens <= 0 {
            return Ok(());
        }
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE budget SET calls=calls+?1,debits=debits+?1 WHERE node_id=?2",
                params![tokens, node.id],
            )?;
            Ok(())
        })
    }

    /// A stable, non-cryptographic digest of everything a node's deliverable
    /// comprises: its summary and the contents of every artifact it left. Used
    /// only to notice that a dependency changed after it was accepted, so FNV-1a
    /// is enough; it never needs to resist an adversary.
    pub fn fingerprint(&self, node: &Node) -> String {
        let mut acc = String::new();
        acc.push_str(node.summary.trim());
        let dir = node.artifacts_dir();
        let mut files: Vec<PathBuf> = Vec::new();
        if dir.is_dir() {
            Self::collect_files(&dir, &mut files);
            files.sort();
        }
        for f in files {
            if let Ok(rel) = f.strip_prefix(&dir) {
                acc.push('\u{0}');
                acc.push_str(&rel.to_string_lossy());
                if let Ok(content) = fs::read_to_string(&f) {
                    acc.push('\u{0}');
                    acc.push_str(&content);
                }
            }
        }
        format!("{:016x}", fnv1a(acc.as_bytes()))
    }

    fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    Self::collect_files(&p, out);
                } else if p.is_file() {
                    out.push(p);
                }
            }
        }
    }

    /// Snapshot the fingerprints of a node's dependencies at acceptance.
    pub fn record_dependencies(&self, node: &Node) -> Result<(), StoreError> {
        if node.depends_on.is_empty() {
            return Ok(());
        }
        let mut current = serde_json::Map::new();
        for dep in &node.depends_on {
            let fp = match self.get(dep) {
                Ok(dep_node) => self.fingerprint(&dep_node),
                Err(_) => String::new(),
            };
            current.insert(dep.clone(), serde_json::Value::String(fp));
        }
        let encoded = serde_json::to_string(&current)?;
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE nodes SET dep_fp=?1, updated_at=?2 WHERE id=?3",
                params![encoded, &now(), node.id],
            )?;
            Ok(())
        })
    }

    /// Complete nodes whose recorded dependency fingerprints no longer match
    /// disk: their dependency was reopened and its deliverable changed after
    /// they were accepted, so their own verification is no longer trustworthy.
    pub fn stale_ids(&self) -> Result<std::collections::HashSet<String>, StoreError> {
        let nodes = self.walk()?;
        let by_id: std::collections::HashMap<&str, &Node> =
            nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let mut stale = std::collections::HashSet::new();
        for node in &nodes {
            if node.status != COMPLETE || node.depends_on.is_empty() {
                continue;
            }
            let recorded: std::collections::HashMap<String, String> =
                serde_json::from_str(&node.dep_fp).unwrap_or_default();
            for dep in &node.depends_on {
                if let Some(dep_node) = by_id.get(dep.as_str()) {
                    if recorded.get(dep).map(String::as_str)
                        != Some(self.fingerprint(dep_node).as_str())
                    {
                        stale.insert(node.id.clone());
                        break;
                    }
                }
            }
        }
        Ok(stale)
    }

    fn next_global_id(&self) -> Result<String, StoreError> {
        self.with_conn(|conn| {
            let c: i64 = conn.query_row("SELECT COUNT(*) FROM global_entries", [], |r| r.get(0))?;
            Ok(format!("global-{:03}", c + 1))
        })
    }

    pub fn note_global(
        &self,
        entry_type: &str,
        content: &str,
        supersedes: &Option<String>,
    ) -> Result<String, StoreError> {
        let eid = self.next_global_id()?;
        let stamp = now();
        if let Some(sup) = supersedes {
            if !sup.is_empty() {
                self.with_conn(|conn| {
                    conn.execute(
                        "UPDATE global_entries SET superseded=1 WHERE id=?1",
                        params![sup],
                    )?;
                    Ok(())
                })?;
            }
        }
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO global_entries(id, entry_type, content, superseded, created_at) VALUES(?1, ?2, ?3, 0, ?4)",
                params![eid, entry_type, content, stamp],
            )?;
            Ok(())
        })?;
        let entry_dir = self.global_dir.join(&eid);
        fs::create_dir_all(&entry_dir)?;
        let header = format!("# Global Entry: {eid}\n\ntype: {entry_type}\ncreated: {stamp}\n\n");
        fs::write(entry_dir.join("entry.md"), format!("{header}{content}\n"))?;
        Ok(eid)
    }

    pub fn retrieve_global(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<GlobalEntry>, StoreError> {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|s| s.to_lowercase())
            .filter(|s| s.len() > 2)
            .collect();
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT entry_type, content FROM global_entries WHERE superseded=0 ORDER BY created_at DESC, id DESC",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(GlobalEntry {
                    entry_type: row.get(0)?,
                    content: row.get(1)?,
                })
            })?;
            let mut scored: Vec<(usize, GlobalEntry)> = Vec::new();
            for r in rows {
                let entry = r?;
                let lower = entry.content.to_lowercase();
                let score = terms.iter().filter(|t| lower.contains(t.as_str())).count();
                if score > 0 || terms.is_empty() {
                    scored.push((score, entry));
                }
            }
            scored.sort_by_key(|b| std::cmp::Reverse(b.0));
            Ok(scored.into_iter().take(limit).map(|s| s.1).collect())
        })
    }
}

#[derive(Debug, Clone)]
pub struct GlobalEntry {
    pub entry_type: String,
    pub content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> (Store, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("fractal_store_{}_{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        (Store::new(&dir), dir)
    }

    /// A parent that splits again to add a capability produces a child which
    /// depends on its EXISTING siblings. If those ids do not resolve, the
    /// dependency is stripped and the new child runs before the work it needs.
    #[test]
    fn added_child_can_depend_on_existing_siblings() {
        let (store, dir) = temp_store("crossbatch");
        let root = store.init("build a thing").unwrap();

        let first = store
            .add_children(
                &root,
                &[Contract {
                    goal: "produce a module".into(),
                    id: "producer".into(),
                    ..Default::default()
                }],
            )
            .unwrap();
        assert_eq!(first.len(), 1);
        let producer_id = first[0].id.clone();

        let second = store
            .add_children(
                &root,
                &[Contract {
                    goal: "consume that module".into(),
                    id: "consumer".into(),
                    depends_on: vec![producer_id.clone()],
                    ..Default::default()
                }],
            )
            .unwrap();

        assert_eq!(second.len(), 1);
        assert_eq!(
            second[0].depends_on,
            vec![producer_id],
            "dependency on an existing sibling must survive, or ordering is lost"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reopen_children_resets_only_own_children() {
        let (store, dir) = temp_store("reopen");
        let root = store.init("goal").unwrap();
        let kids = store
            .add_children(
                &root,
                &[
                    Contract {
                        goal: "a".into(),
                        id: "a".into(),
                        ..Default::default()
                    },
                    Contract {
                        goal: "b".into(),
                        id: "b".into(),
                        ..Default::default()
                    },
                ],
            )
            .unwrap();

        for kid in &kids {
            store.set_status(kid, COMPLETE).unwrap();
        }

        let reopened = store
            .reopen_children(&root, &[kids[0].id.clone()], "does not build")
            .unwrap();
        assert_eq!(reopened, vec![kids[0].id.clone()]);

        let after = store.walk().unwrap();
        let a = after.iter().find(|n| n.id == kids[0].id).unwrap();
        let b = after.iter().find(|n| n.id == kids[1].id).unwrap();
        assert_eq!(a.status, PENDING, "reopened child must be runnable again");
        assert_eq!(b.status, COMPLETE, "untouched sibling must keep its status");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reopen_refuses_a_node_that_is_not_our_child() {
        let (store, dir) = temp_store("notmine");
        let root = store.init("goal").unwrap();
        let kids = store
            .add_children(
                &root,
                &[Contract {
                    goal: "a".into(),
                    id: "a".into(),
                    ..Default::default()
                }],
            )
            .unwrap();
        let grandkids = store
            .add_children(
                &kids[0],
                &[Contract {
                    goal: "deep".into(),
                    id: "deep".into(),
                    ..Default::default()
                }],
            )
            .unwrap();
        store.set_status(&grandkids[0], COMPLETE).unwrap();

        // root is the grandparent, not the parent, so this must be refused.
        let reopened = store
            .reopen_children(&root, &[grandkids[0].id.clone()], "nope")
            .unwrap();
        assert!(
            reopened.is_empty(),
            "a parent may only reopen its own direct children"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn contract_roundtrips_verification_gates() {
        let contract = Contract {
            goal: "do it".into(),
            verification: vec!["npm test".into(), "npx tsc --noEmit".into()],
            ..Default::default()
        };
        let parsed = Contract::parse(&contract.render("root-01", 2, Some("root")));
        assert_eq!(parsed.verification, contract.verification);
    }

    fn tree_with_child(name: &str) -> (Store, Node, Node, PathBuf) {
        let (store, dir) = temp_store(name);
        let root = store.init("goal").unwrap();
        // The owner carries the constraint a descendant will later challenge.
        store
            .add_constraint_and_propagate(&root.id, "the wall bears the load")
            .unwrap();
        let child = store
            .add_children(
                &root,
                &[Contract {
                    goal: "child".into(),
                    id: "a".into(),
                    ..Default::default()
                }],
            )
            .unwrap()
            .remove(0);
        (store, root, child, dir)
    }

    #[test]
    fn amend_replaces_constraint_across_the_subtree() {
        let (store, root, child, dir) = tree_with_child("amend");
        // The child inherited the root's constraint; amendment must rewrite it
        // in the owner's contract and every descendant, not merely append.
        let changed = store
            .amend_inherited_constraint(&root, "the wall bears the load", "the wall needs a beam")
            .unwrap();
        assert!(changed >= 1, "at least the owner must change");
        let reloaded = store.walk().unwrap();
        let owner = reloaded.iter().find(|n| n.id == root.id).unwrap();
        assert!(
            owner
                .contract()
                .constraints
                .iter()
                .any(|c| c.contains("needs a beam")),
            "owner constraint must be amended"
        );
        assert!(
            !owner
                .contract()
                .constraints
                .iter()
                .any(|c| c.contains("bears the load")),
            "the falsified constraint must not survive"
        );
        let _ = child;
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_interface_and_depends_on_persist() {
        let (store, _root, child, dir) = tree_with_child("iface");
        assert!(store
            .add_interface(&child, "Authenticator.verify(token)")
            .unwrap());
        assert!(
            !store
                .add_interface(&child, "Authenticator.verify(token)")
                .unwrap(),
            "adding the same interface twice is a no-op"
        );
        let reloaded = store.walk().unwrap();
        let c = reloaded.iter().find(|n| n.id == child.id).unwrap();
        assert!(c.contract().interfaces.iter().any(|i| i.contains("verify")));

        let dep = "root-99".to_string();
        assert!(store.add_depends_on(&child, &dep).unwrap());
        let reloaded = store.walk().unwrap();
        let c = reloaded.iter().find(|n| n.id == child.id).unwrap();
        assert!(c.depends_on.contains(&dep));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn replan_compacts_children_and_returns_parent_to_pending() {
        let (store, root, child, dir) = tree_with_child("replan");
        store
            .append_decision(&child, "valuable episodic detail")
            .unwrap();
        store.compact_child_trace(&root, &child).unwrap();
        store.delete_node(&child).unwrap();
        store.set_status(&root, PENDING).unwrap();

        assert!(
            !child.path.exists(),
            "pruned child directory must be gone from the tree"
        );
        let reloaded = store.walk().unwrap();
        assert!(
            !reloaded.iter().any(|n| n.id == child.id),
            "pruned child must be gone from the index"
        );
        assert_eq!(
            reloaded.iter().find(|n| n.id == root.id).unwrap().status,
            PENDING
        );
        let compacted = fs::read_to_string(root.log_dir().join("compacted-root-01.md")).unwrap();
        assert!(
            compacted.contains("valuable episodic detail"),
            "compaction must preserve the child's trace: {compacted}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn budget_ledger_honours_allocations_and_debits() {
        let (store, dir) = temp_store("budget");
        let root = store.init_with_budget("build a thing", Some(1000)).unwrap();
        let fee = store.split_fee();
        assert!(store.budget_enabled());
        assert_eq!(store.budget_remaining("root").unwrap(), 1000 - fee);

        store.debit_call(&root, 50).unwrap();
        assert_eq!(store.budget_remaining("root").unwrap(), 950 - fee);

        let children = store
            .add_children(
                &root,
                &[Contract {
                    goal: "producer".into(),
                    id: "a".into(),
                    allocation: 300,
                    ..Default::default()
                }],
            )
            .unwrap();
        // The parent pays the fee and the grant; the child receives the grant.
        // `remaining` also keeps one further split-fee in reserve.
        assert_eq!(
            store.budget_remaining("root").unwrap(),
            1000 - 50 - 2 * fee - 300
        );
        assert_eq!(store.budget_remaining(&children[0].id).unwrap(), 300 - fee);

        // debits is the node's own call usage plus its split fee.
        let total: i64 = store
            .with_conn(|conn| {
                Ok(
                    conn.query_row("SELECT debits FROM budget WHERE node_id='root'", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .unwrap();
        assert_eq!(total, 50 + fee);

        // Allocation is not cosmetic: a grandchild inherits it.
        let grandchildren = store
            .add_children(
                &children[0],
                &[Contract {
                    goal: "leaf".into(),
                    id: "b".into(),
                    allocation: 100,
                    ..Default::default()
                }],
            )
            .unwrap();
        assert_eq!(
            store.budget_remaining(&grandchildren[0].id).unwrap(),
            100 - fee
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// D1 regression: the model omitted `allocation` (the split template never
    /// asks for one), so every child was created with `allowance = 0` and its
    /// remaining computed as `0 - split_fee = -200`, failing the whole run
    /// before any child could run. Omitted allocations must be funded from the
    /// parent's remaining allowance.
    #[test]
    fn omitted_allocations_are_funded_from_the_parent() {
        let (store, dir) = temp_store("budget_inherit");
        let root = store
            .init_with_budget("build a real thing", Some(300_000))
            .unwrap();
        // The root has already spent tokens on its split call, as in run 1.
        store.debit_call(&root, 2987).unwrap();

        let children = store
            .add_children(
                &root,
                &[
                    Contract {
                        goal: "a".into(),
                        id: "a".into(),
                        allocation: 0,
                        ..Default::default()
                    },
                    Contract {
                        goal: "b".into(),
                        id: "b".into(),
                        allocation: 0,
                        ..Default::default()
                    },
                ],
            )
            .unwrap();

        for child in &children {
            let remaining = store.budget_remaining(&child.id).unwrap();
            assert!(
                remaining > 0,
                "child {} with an omitted allocation is not runnable: remaining = {remaining}",
                child.id
            );
        }
        // Both inherited children share what the parent had left; the parent
        // does not mint budget for them.
        let first = store.budget_remaining(&children[0].id).unwrap();
        let second = store.budget_remaining(&children[1].id).unwrap();
        assert_eq!(first, second, "inherited shares should be equal");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Mixed splits: explicit allocations are honoured, only the omitted ones
    /// inherit the leftover.
    #[test]
    fn explicit_allocations_win_over_inherited_shares() {
        let (store, dir) = temp_store("budget_mixed");
        let root = store
            .init_with_budget("build a real thing", Some(1000))
            .unwrap();
        let fee = store.split_fee();
        let children = store
            .add_children(
                &root,
                &[
                    Contract {
                        goal: "explicit".into(),
                        id: "a".into(),
                        allocation: 300,
                        ..Default::default()
                    },
                    Contract {
                        goal: "inherit".into(),
                        id: "b".into(),
                        allocation: 0,
                        ..Default::default()
                    },
                ],
            )
            .unwrap();
        assert_eq!(store.budget_remaining(&children[0].id).unwrap(), 300 - fee);
        assert!(
            store.budget_remaining(&children[1].id).unwrap() > 0,
            "the omitted allocation should have inherited a spendable share"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconcile_repairs_topology_and_drops_vanished_nodes() {
        let (store, dir) = temp_store("reconcile");
        let root = store.init("build a thing").unwrap();
        let children = store
            .add_children(
                &root,
                &[
                    Contract {
                        goal: "stays".into(),
                        id: "a".into(),
                        ..Default::default()
                    },
                    Contract {
                        goal: "vanishes".into(),
                        id: "b".into(),
                        ..Default::default()
                    },
                ],
            )
            .unwrap();
        let stays = children[0].clone();
        let vanishes = children[1].clone();

        // Corrupt the surviving child's topology in the index.
        store
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE nodes SET depth=99, parent='ghost' WHERE id=?1",
                    params![stays.id],
                )?;
                Ok(())
            })
            .unwrap();
        // A directory the index never saw, adopted on reconcile.
        let stray_dir = root.children_dir().join("root-77");
        fs::create_dir_all(&stray_dir).unwrap();
        fs::write(
            stray_dir.join("contract.md"),
            "# Contract: root-77\n\n- node: root-77\n\n## Goal\n\nstray work\n",
        )
        .unwrap();
        // A directory that is gone while its row remains.
        fs::remove_dir_all(&vanishes.path).unwrap();

        store.reconcile().unwrap();
        let nodes = store.walk().unwrap();
        let repaired = nodes.iter().find(|n| n.id == stays.id).unwrap();
        assert_eq!(repaired.depth, 2, "depth must be repaired from disk");
        assert_eq!(repaired.parent.as_deref(), Some("root"));
        assert_eq!(repaired.status, PENDING, "a childless node is pending");
        assert_eq!(
            nodes.iter().find(|n| n.id == "root").unwrap().status,
            SPLIT,
            "a node with children on disk is split"
        );
        assert!(
            !nodes.iter().any(|n| n.id == vanishes.id),
            "a row whose directory is gone must be deleted"
        );
        assert!(
            nodes.iter().any(|n| n.id == "root-77"),
            "a directory the index never saw must be adopted"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn changed_dependency_marks_dependent_stale() {
        let (store, dir) = temp_store("stale");
        let root = store.init("build a thing").unwrap();
        let kids = store
            .add_children(
                &root,
                &[
                    Contract {
                        goal: "producer".into(),
                        id: "a".into(),
                        ..Default::default()
                    },
                    Contract {
                        goal: "consumer".into(),
                        id: "b".into(),
                        depends_on: vec!["a".into()],
                        ..Default::default()
                    },
                ],
            )
            .unwrap();
        let producer = kids.iter().find(|n| n.goal == "producer").unwrap().clone();
        let consumer = kids.iter().find(|n| n.goal == "consumer").unwrap().clone();
        assert_eq!(
            consumer.depends_on,
            vec![producer.id.clone()],
            "the model-proposed sibling id must resolve to the real node id"
        );

        store
            .complete(
                &producer,
                "v1",
                "deliverable",
                &[("a.txt".into(), "v1".into())],
            )
            .unwrap();
        store
            .complete(
                &consumer,
                "done",
                "deliverable",
                &[("b.txt".into(), "done".into())],
            )
            .unwrap();
        assert!(
            store.stale_ids().unwrap().is_empty(),
            "a freshly accepted dependent is not stale"
        );

        // Reopen the producer and change what it delivers.
        store.set_status(&producer, PENDING).unwrap();
        store
            .complete(
                &producer,
                "v2",
                "deliverable",
                &[("a.txt".into(), "v2".into())],
            )
            .unwrap();
        let stale = store.stale_ids().unwrap();
        assert!(
            stale.contains(&consumer.id),
            "a dependent must be re-run when its dependency changes: {stale:?}"
        );
        assert!(
            !stale.contains(&producer.id),
            "the changed producer is not itself stale"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
