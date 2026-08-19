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
            if line.starts_with("## ") {
                current = Some(line[3..].trim().to_lowercase());
                sections.entry(current.clone().unwrap_or_default()).or_default();
            } else if let Some(ref cur) = current {
                sections.entry(cur.clone()).or_default().push(line.to_string());
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
            conn.execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
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

    pub fn budget_enabled(&self) -> bool {
        std::env::var("FRACTAL_BUDGET").is_ok()
    }

    pub fn split_fee(&self) -> i64 {
        std::env::var("FRACTAL_SPLIT_FEE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(200)
    }

    pub fn init(&self, goal: &str) -> Result<Node, StoreError> {
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
            if self.budget_enabled() {
                let initial_budget: i64 = std::env::var("FRACTAL_BUDGET")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(100_000);
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
            let resolved_deps: Vec<String> = c
                .depends_on
                .iter()
                .filter_map(|dep| {
                    let resolved = id_map.get(dep).cloned();
                    if resolved.is_none() {
                        eprintln!(
                            "  fractal: warning — depends_on '{}' for {} does not match any known id, stripping",
                            dep, cid
                        );
                    }
                    resolved
                })
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
        let stamp = now();
        self.with_conn(|conn| {
            for ch in &children {
                Self::insert_node(conn, ch)?;
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
            if name.ends_with(".test.ts") || name.ends_with(".test.tsx") || name.ends_with(".spec.ts") || name.ends_with(".spec.tsx") {
                return PathBuf::from("tests").join(&name);
            } else if name.ends_with(".ts") || name.ends_with(".tsx") || name.ends_with(".css") || name.ends_with(".html") {
                // Keep root configs at root
                if !["vite.config.ts", "tailwind.config.ts", "postcss.config.js", "tsconfig.json", "package.json"].contains(&name.as_str()) {
                    return PathBuf::from("src").join(&name);
                }
            }
        }
        out
    }
    pub fn unified_dir(&self) -> PathBuf {
        self.tree_dir.parent().unwrap_or(&self.tree_dir).join(UNIFIED_DIRNAME)
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
                        if art.is_file() {
                            if fs::copy(&art, &dest).is_ok() {
                                count += 1;
                            }
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
        let mut file = fs::OpenOptions::new().create(true).append(true).open(path)?;
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
        let mut file = fs::OpenOptions::new().create(true).append(true).open(path)?;
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
                    self.append_decision(n, &format!("inherited constraint from {origin_node_id}: {}", constraint.trim()))?;
                    affected += 1;
                }
            }
        }

        Ok(affected)
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
            let mut stmt = conn.prepare("SELECT id, command, payload FROM steer_queue ORDER BY id ASC")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
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
        out.push((dir.to_path_buf(), node_id.to_string(), parent.map(|s| s.to_string()), depth));
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
            for (path, node_id, parent, depth) in &disk {
                let exists: bool = conn.query_row(
                    "SELECT COUNT(*) FROM nodes WHERE id=?1",
                    params![node_id],
                    |r| r.get::<_, i64>(0),
                )? > 0;
                if !exists {
                    let has_children = !Self::child_dirs(path).is_empty();
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
            }
            Ok(())
        })
    }

    pub fn walk(&self) -> Result<Vec<Node>, StoreError> {
        self.require_initialised()?;
        let disk = self.walk_disk();
        let mut nodes = Vec::new();
        for (path, node_id, parent, depth) in &disk {
            let (status, goal, summary, deps_s, dep_fp): (
                String,
                String,
                String,
                String,
                String,
            ) = self.with_conn(|conn| {
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

    #[allow(dead_code)]
    pub fn get(&self, node_id: &str) -> std::result::Result<Node, StoreError> {
        self.walk()?
            .into_iter()
            .find(|n| n.id == node_id)
            .ok_or_else(|| StoreError::Other(format!("node {node_id:?} not found")))
    }

    pub fn children_of(&self, node: &Node) -> Result<Vec<Node>, StoreError> {
        let all = self.walk()?;
        Ok(all.into_iter().filter(|n| n.parent.as_deref() == Some(&node.id)).collect())
    }

    pub fn generate_digest(&self) -> Result<String, StoreError> {
        let nodes = self.walk()?;
        let mut out = String::from("# Digest\n\n## Done\n");
        for d in nodes.iter().filter(|n| n.status == COMPLETE) {
            out.push_str(&format!("- **{}**: {}\n", d.id, d.goal.lines().next().unwrap_or(&d.goal)));
        }
        out.push_str("\n## Blocked\n");
        for b in nodes.iter().filter(|n| n.status == SUSPENDED || n.status == FAILED) {
            out.push_str(&format!("- **{}** ({}): {}\n", b.id, b.status, b.goal.lines().next().unwrap_or(&b.goal)));
        }
        out.push_str("\n## Next\n");
        for p in nodes.iter().filter(|n| n.status == PENDING) {
            out.push_str(&format!("- **{}**: {}\n", p.id, p.goal.lines().next().unwrap_or(&p.goal)));
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

            self.append_decision(child, &format!("reopened by {}: {}", parent.id, reason.trim()))?;
            reopened.push(child.id.clone());
        }

        if !reopened.is_empty() {
            self.set_status(parent, SPLIT)?;
            self.append_decision(
                parent,
                &format!("reopened children {}: {}", reopened.join(", "), reason.trim()),
            )?;
        }

        Ok(reopened)
    }
    pub fn budget_remaining(&self, node_id: &str) -> Result<i64, StoreError> {
        self.with_conn(|conn| {
            let (a, c, f, ch): (i64, i64, i64, i64) = conn.query_row(
                "SELECT allowance,calls,fee_paid,children FROM budget WHERE node_id=?1",
                params![node_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            Ok(a - c - f - ch - self.split_fee())
        })
    }

    #[allow(dead_code)]
    pub fn debit_call(&self, node: &Node, tokens: i64) -> Result<(), StoreError> {
        if tokens <= 0 || !self.budget_enabled() {
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

    pub fn retrieve_global(&self, query: &str, limit: usize) -> Result<Vec<GlobalEntry>, StoreError> {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|s| s.to_lowercase())
            .filter(|s| s.len() > 2)
            .collect();
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, entry_type, content FROM global_entries WHERE superseded=0 ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(GlobalEntry {
                    id: row.get(0)?,
                    entry_type: row.get(1)?,
                    content: row.get(2)?,
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
            scored.sort_by(|a, b| b.0.cmp(&a.0));
            Ok(scored.into_iter().take(limit).map(|s| s.1).collect())
        })
    }
}

#[derive(Debug, Clone)]
pub struct GlobalEntry {
    pub id: String,
    pub entry_type: String,
    pub content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!("fractal_store_{}_{}", name, std::process::id()));
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
                &[Contract { goal: "produce a module".into(), id: "producer".into(), ..Default::default() }],
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
                    Contract { goal: "a".into(), id: "a".into(), ..Default::default() },
                    Contract { goal: "b".into(), id: "b".into(), ..Default::default() },
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
            .add_children(&root, &[Contract { goal: "a".into(), id: "a".into(), ..Default::default() }])
            .unwrap();
        let grandkids = store
            .add_children(&kids[0], &[Contract { goal: "deep".into(), id: "deep".into(), ..Default::default() }])
            .unwrap();
        store.set_status(&grandkids[0], COMPLETE).unwrap();

        // root is the grandparent, not the parent, so this must be refused.
        let reopened = store
            .reopen_children(&root, &[grandkids[0].id.clone()], "nope")
            .unwrap();
        assert!(reopened.is_empty(), "a parent may only reopen its own direct children");

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
}
