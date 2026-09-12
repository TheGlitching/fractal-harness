//! Git as the record of what each node actually changed.
//!
//! Before this existed, a node's output was a bag of files copied into an
//! `artifacts/` directory and then blindly re-written over the project root.
//! That made it impossible to answer the only questions that matter when a
//! long-running tree goes wrong: what did this node change, does it still
//! build, and can I undo just this node's work?
//!
//! Every completed node now produces exactly one commit in the project repo, so
//! the tree's history and the code's history are the same history.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

/// The harness owns these paths inside the user's project. They are written to
/// `.git/info/exclude` (never the project's own `.gitignore`) and also passed as
/// explicit pathspec exclusions whenever the harness stages a commit, so a node
/// commit cannot pick them up even if the user's `.gitignore` re-includes them.
const HARNESS_EXCLUDES: &str = "\
# fractal-harness internals - never commit these into the user's history
tree/
.fractal/
global/
dist/
trace.json
digest.md
.fractal_decision_*
# dependency trees, build caches and runtime junk - a node's diff is its work,
# not the 5000 files `npm install` happened to leave behind
node_modules/
.pnpm-store/
__pycache__/
*.py[cod]
.venv/
venv/
.env
.env.local
.env.*.local
*.log
.DS_Store
coverage/
.pytest_cache/
.next/
.cache/
";

/// `git add` that stages everything a node authored but no harness path.
///
/// `git add -A` with an explicit pathspec makes git refuse to add paths that its
/// ignore rules match, so the harness stages broadly and then explicitly
/// unstages harness paths. That second step is what makes the exclusion hold
/// even when a user's `.gitignore` re-includes one of them.
fn stage_node_work(root: &Path) -> Result<(), String> {
    git(root, &["add", "-A"])?;
    if head_sha(root).is_some() {
        git(root, RESET_EXCLUDING_HARNESS)?;
    }
    Ok(())
}

/// Undo the staging of any harness path, leaving its working-tree contents
/// alone. Used after a broad `git add -A`.
const RESET_EXCLUDING_HARNESS: &[&str] = &[
    "reset",
    "-q",
    "--",
    "tree",
    ".fractal",
    "global",
    "dist",
    "trace.json",
    "digest.md",
    ".fractal_decision_*",
];

/// The same exclusions for `git status`, so a node's "did it change anything"
/// answer is about node-authored files only.
const STATUS_EXCLUDING_HARNESS: &[&str] = &[
    "status",
    "--porcelain",
    "--",
    ".",
    ":(exclude)tree",
    ":(exclude).fractal",
    ":(exclude)global",
    ":(exclude)dist",
    ":(exclude)trace.json",
    ":(exclude)digest.md",
    ":(exclude).fractal_decision_*",
];

/// The same exclusions for diff evidence and untracked-file previews, so the
/// critic sees only the node's own changes.
const DIFF_EXCLUDING_HARNESS: &[&str] = &[
    "diff",
    "HEAD",
    "--",
    ".",
    ":(exclude)tree",
    ":(exclude).fractal",
    ":(exclude)global",
    ":(exclude)dist",
    ":(exclude)trace.json",
    ":(exclude)digest.md",
    ":(exclude).fractal_decision_*",
];

const LS_OTHERS_EXCLUDING_HARNESS: &[&str] = &[
    "ls-files",
    "--others",
    "--exclude-standard",
    "--",
    ".",
    ":(exclude)tree",
    ":(exclude).fractal",
    ":(exclude)global",
    ":(exclude)dist",
    ":(exclude)trace.json",
    ":(exclude)digest.md",
    ":(exclude).fractal_decision_*",
];

/// One process-wide guard for git index mutation. The scheduler already runs a
/// single node at a time, but the index is global to the repo and the guard
/// keeps any future concurrent caller from racing `git add`/`git commit`.
static INDEX_LOCK: Mutex<()> = Mutex::new(());

fn index_lock() -> std::sync::MutexGuard<'static, ()> {
    INDEX_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;

    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn is_repo(root: &Path) -> bool {
    git(root, &["rev-parse", "--git-dir"]).is_ok()
}

/// Make the project a git repo if it is not one already, and guarantee at least
/// one commit exists so later nodes always have a base to diff against.
pub fn ensure_repo(root: &Path) -> Result<(), String> {
    let _index = index_lock();
    if !is_repo(root) {
        git(root, &["init"])?;
    }

    // Exclude the harness's own working directories from the user's repository
    // by default. `.git/info/exclude` is local to the clone and is never
    // committed, so this works whether or not the project already has a
    // `.gitignore`, and does not modify the project's own files.
    ensure_excludes(root)?;

    // A fresh repo has no HEAD; several operations (diff, revert, rev-parse
    // HEAD) are undefined until the first commit lands.
    if head_sha(root).is_none() {
        stage_node_work(root)?;
        commit(root, "chore: fractal baseline")?;
    }
    Ok(())
}

/// Write the harness's ignore block into `.git/info/exclude`, idempotently.
///
/// A project created by an older harness already carries the marker comment and
/// would otherwise never receive later additions (e.g. `node_modules/`), so this
/// reconciles line by line rather than skipping wholesale when the marker exists.
fn ensure_excludes(root: &Path) -> Result<(), String> {
    let rel = git(root, &["rev-parse", "--git-path", "info/exclude"])?;
    let path = {
        let p = PathBuf::from(&rel);
        if p.is_absolute() {
            p
        } else {
            root.join(p)
        }
    };
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut content = existing.clone();
    let mut changed = false;
    if existing.contains("# fractal-harness internals") {
        for line in HARNESS_EXCLUDES.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if !existing.lines().any(|l| l.trim() == line) {
                if !content.is_empty() && !content.ends_with('\n') {
                    content.push('\n');
                }
                content.push_str(line);
                content.push('\n');
                changed = true;
            }
        }
    } else {
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(HARNESS_EXCLUDES);
        changed = true;
    }
    if !changed {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, content).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

/// Untracked, non-ignored files in the working tree, relative to the repo root.
pub fn untracked_files(root: &Path) -> std::collections::HashSet<String> {
    git(root, &["ls-files", "--others", "--exclude-standard"])
        .map(|s| {
            s.lines()
                .filter(|l| !l.is_empty())
                .map(|l| l.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Record paths the harness should never commit - runtime state an app wrote
/// while a verification gate ran (e.g. `.portfolio.json`). `.git/info/exclude`
/// is local to the clone and never committed, so this keeps the user's own
/// `.gitignore` untouched while removing the file from every future diff.
///
/// ponytail: exact, root-anchored paths only. Globs a generated app might also
/// write are the app's `.gitignore`'s job; add a config knob if a real project
/// needs pattern-level exclusion here.
pub fn ignore_runtime_paths(root: &Path, paths: &[String]) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    let rel = git(root, &["rev-parse", "--git-path", "info/exclude"])?;
    let path = {
        let p = PathBuf::from(&rel);
        if p.is_absolute() {
            p
        } else {
            root.join(p)
        }
    };
    let mut content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut changed = false;
    for p in paths {
        let anchored = format!("/{}", p.trim_start_matches('/'));
        if !content.lines().any(|l| l.trim() == anchored) {
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&anchored);
            content.push('\n');
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, content).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

pub fn head_sha(root: &Path) -> Option<String> {
    git(root, &["rev-parse", "HEAD"]).ok()
}

fn commit(root: &Path, message: &str) -> Result<String, String> {
    // Identity is forced per-invocation so a machine with no global git config
    // still produces commits, without mutating the user's config.
    // `--allow-empty` is for the baseline commit of a project whose only files
    // are harness internals; node commits check for staged work first.
    git(
        root,
        &[
            "-c",
            "user.name=fractal",
            "-c",
            "user.email=fractal@localhost",
            "commit",
            "--no-verify",
            "--allow-empty",
            "-q",
            "-m",
            message,
        ],
    )?;
    head_sha(root).ok_or_else(|| "commit produced no HEAD".to_string())
}

fn has_staged_changes(root: &Path) -> bool {
    git(root, &["diff", "--cached", "--name-only"])
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

/// Stage and commit the node-authored changes in the working tree.
/// `Ok(None)` means the node changed nothing outside harness paths, which is a
/// legitimate outcome for a pure decomposition step and must not be reported as
/// a failure.
///
/// The harness stages with explicit exclusions, so only files the node authored
/// are committed; its own `tree/`, `.fractal/`, `global/`, `dist/` and decision
/// files can never enter the user's history.
pub fn commit_node_work(
    root: &Path,
    node_id: &str,
    summary: &str,
) -> Result<Option<String>, String> {
    let _index = index_lock();
    stage_node_work(root)?;
    if !has_staged_changes(root) {
        return Ok(None);
    }
    let headline = summary.lines().next().unwrap_or("work").trim();
    let headline = if headline.is_empty() {
        "work"
    } else {
        headline
    };
    let truncated: String = headline.chars().take(72).collect();
    let sha = commit(root, &format!("{node_id}: {truncated}"))?;
    Ok(Some(sha))
}

/// Files a node touched, relative to the repo root.
pub fn changed_files_since(root: &Path, base: &str) -> Vec<String> {
    git(root, &["diff", "--name-only", base, "HEAD"])
        .map(|s| {
            s.lines()
                .map(|l| l.to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// True when the working tree differs from HEAD, including files an agent has
/// created but not yet added. At verification time a node's work is uncommitted,
/// so this is the only honest answer to "did this node change anything".
/// Harness paths are excluded: they are never the node's work.
pub fn has_uncommitted_changes(root: &Path) -> bool {
    git(root, STATUS_EXCLUDING_HARNESS)
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

/// Auto-generated files whose contents are noise to a critic. A lockfile can be
/// hundreds of KB and sorts before hand-written source, so previewing it consumes
/// the whole evidence budget and hides the actual deliverable (trial 4 H2b).
fn is_generated_file(path: &str) -> bool {
    let name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path);
    name.ends_with("-lock.json") || name == "Cargo.lock"
}

fn file_header(file: &str, chars: usize) -> String {
    format!("\n--- new file: {file} ({chars} chars) ---\n")
}

/// Bytes a per-file cut marker can occupy, reserved before content is allocated
/// so the marker of one file cannot crowd out another file's window.
const FILE_MARKER_RESERVE: usize = 120;
/// Bytes reserved for the explicit harness-truncation marker appended when any
/// content had to be cut.
const GLOBAL_MARKER_RESERVE: usize = 300;

/// Human-readable evidence of the working tree's change vs HEAD, including
/// untracked files (new files an agent wrote are the common case for a leaf).
/// `git diff HEAD` alone shows nothing for a brand-new file, so previews of
/// untracked files are appended explicitly.
pub fn worktree_change_summary(root: &Path, max_bytes: usize) -> String {
    let mut out = String::new();

    let tracked = git(root, DIFF_EXCLUDING_HARNESS).unwrap_or_default();
    if !tracked.trim().is_empty() {
        out.push_str(&tracked);
        out.push('\n');
    }

    let untracked = git(root, LS_OTHERS_EXCLUDING_HARNESS).unwrap_or_default();
    let mut generated: Vec<String> = Vec::new();
    let mut files: Vec<(String, String)> = Vec::new();
    for file in untracked.lines().filter(|l| !l.is_empty()) {
        let content = std::fs::read_to_string(root.join(file)).unwrap_or_default();
        if is_generated_file(file) {
            generated.push(file.to_string());
        } else {
            files.push((file.to_string(), content));
        }
    }
    let generated_note = if generated.is_empty() {
        String::new()
    } else {
        format!(
            "\n--- auto-generated files (content omitted; {} in total): {} ---\n",
            generated.len(),
            generated.join(", ")
        )
    };

    // Preview each file whole. A silent per-file cut made a complete file look
    // like an incomplete deliverable to a strict critic, which then failed it on
    // every retry (H2). If everything fits, that is the entire summary.
    let previews: Vec<String> = files
        .iter()
        .map(|(file, content)| format!("{}{content}\n", file_header(file, content.chars().count())))
        .collect();
    let whole: usize =
        out.len() + generated_note.len() + previews.iter().map(|p| p.len()).sum::<usize>();
    if whole <= max_bytes {
        for p in &previews {
            out.push_str(p);
        }
        out.push_str(&generated_note);
        return out;
    }

    // The budget cannot show every file whole. Give every hand-authored file a
    // window so a large sibling cannot crowd it out entirely, then cut the
    // assembled summary with an explicit harness marker. Generated files already
    // contributed no content, so a lockfile can never starve the implementation.
    let fixed: usize = out.len()
        + generated_note.len()
        + files
            .iter()
            .map(|(f, c)| file_header(f, c.chars().count()).len() + 1)
            .sum::<usize>();
    let content_budget = max_bytes
        .saturating_sub(fixed)
        .saturating_sub(files.len() * FILE_MARKER_RESERVE + GLOBAL_MARKER_RESERVE);
    let share = if files.is_empty() {
        0
    } else {
        content_budget / files.len()
    };
    for (file, content) in &files {
        let total = content.chars().count();
        let window = share.min(total);
        let shown: String = content.chars().take(window).collect();
        out.push_str(&file_header(file, total));
        out.push_str(&shown);
        out.push('\n');
        if window < total {
            out.push_str(&format!(
                "... [{file}: preview cut by the harness at {window} of {total} chars]\n"
            ));
        }
    }
    out.push_str(&generated_note);

    // Say plainly that the harness cut the evidence so a critic does not read a
    // capped preview as the node shipping a truncated deliverable.
    format!(
        "{out}\n... [evidence truncated by the harness: showing {} of {whole} bytes. \
         The remainder was omitted for length, NOT by the node; judge the evidence \
         present and do not FAIL a deliverable merely because this preview ended.]",
        out.len(),
    )
}

/// Discard uncommitted noise so a retried attempt starts from the last known
/// good commit instead of inheriting the failed attempt's half-written files.
///
/// The scheduler calls this when a node fails terminally: its uncommitted work
/// is reverted so the next node's diff, verification and commit see only that
/// next node's work. Ignored harness paths survive (`git clean -fd` does not
/// remove ignored files), so the tree's own memory is untouched.
pub fn reset_uncommitted(root: &Path) -> Result<(), String> {
    let _index = index_lock();
    git(root, &["reset", "--hard", "HEAD"])?;
    git(root, &["clean", "-fd"])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("fractal_git_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ensure_repo_creates_repo_and_baseline_commit() {
        let dir = temp_repo("ensure");
        assert!(!is_repo(&dir));
        ensure_repo(&dir).unwrap();
        assert!(is_repo(&dir));
        assert!(head_sha(&dir).is_some(), "baseline commit must exist");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_repo_is_idempotent_on_existing_repo() {
        let dir = temp_repo("idem");
        ensure_repo(&dir).unwrap();
        let first = head_sha(&dir).unwrap();
        ensure_repo(&dir).unwrap();
        assert_eq!(
            first,
            head_sha(&dir).unwrap(),
            "must not add a second baseline"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_node_work_returns_none_when_nothing_changed() {
        let dir = temp_repo("noop");
        ensure_repo(&dir).unwrap();
        let sha = commit_node_work(&dir, "root-01", "did nothing").unwrap();
        assert!(
            sha.is_none(),
            "a no-change node must not fabricate a commit"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_node_work_records_changed_files() {
        let dir = temp_repo("changes");
        ensure_repo(&dir).unwrap();
        let base = head_sha(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        let sha = commit_node_work(&dir, "root-02", "add a.txt").unwrap();
        assert!(sha.is_some());
        let files = changed_files_since(&dir, &base);
        assert_eq!(files, vec!["a.txt".to_string()]);
        assert!(
            !has_uncommitted_changes(&dir),
            "working tree must be clean after commit"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reset_uncommitted_discards_failed_attempt() {
        let dir = temp_repo("reset");
        ensure_repo(&dir).unwrap();
        std::fs::write(dir.join("garbage.txt"), "half-written").unwrap();
        assert!(has_uncommitted_changes(&dir));
        reset_uncommitted(&dir).unwrap();
        assert!(!has_uncommitted_changes(&dir));
        assert!(!dir.join("garbage.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two nodes that run one after another must each commit only their own
    /// files. Before per-node isolation the second node's commit could sweep in
    /// the first node's uncommitted files, or `git add -A` could commit a
    /// sibling's work under the wrong node id.
    #[test]
    fn node_commits_are_isolated() {
        let dir = temp_repo("isolate");
        ensure_repo(&dir).unwrap();

        std::fs::write(dir.join("a.txt"), "A").unwrap();
        let sha_a = commit_node_work(&dir, "root-01", "work a")
            .unwrap()
            .expect("a.txt must produce a commit");

        std::fs::write(dir.join("b.txt"), "B").unwrap();
        let sha_b = commit_node_work(&dir, "root-02", "work b")
            .unwrap()
            .expect("b.txt must produce a commit");

        let files_a = git(&dir, &["show", "--format=", "--name-only", &sha_a]).unwrap();
        let files_b = git(&dir, &["show", "--format=", "--name-only", &sha_b]).unwrap();
        assert_eq!(files_a, "a.txt", "node A's commit saw node B's file");
        assert_eq!(files_b, "b.txt", "node B's commit saw node A's file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failed node's files are reverted, so its half-written work never
    /// appears in the next node's diff or commit.
    #[test]
    fn failed_node_files_are_reverted_before_the_next_node() {
        let dir = temp_repo("revertnext");
        ensure_repo(&dir).unwrap();

        std::fs::write(dir.join("half_written.txt"), "failed attempt").unwrap();
        assert!(has_uncommitted_changes(&dir));
        reset_uncommitted(&dir).unwrap();
        assert!(!has_uncommitted_changes(&dir));

        // The next node starts from the last good commit and sees only itself.
        std::fs::write(dir.join("good.txt"), "next node").unwrap();
        let sha = commit_node_work(&dir, "root-02", "work").unwrap().unwrap();
        let files = git(&dir, &["show", "--format=", "--name-only", &sha]).unwrap();
        assert_eq!(files, "good.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Running inside a user repo with its own `.gitignore` must never pollute
    /// that repo's history with harness internals, and must not rewrite the
    /// user's `.gitignore`.
    #[test]
    fn harness_paths_never_enter_an_existing_repo() {
        let dir = temp_repo("exclude");
        git(&dir, &["init", "-q"]).unwrap();
        std::fs::write(dir.join(".gitignore"), "node_modules/\n").unwrap();
        ensure_repo(&dir).unwrap();

        for p in [
            "tree/root/contract.md",
            ".fractal/index.db",
            ".fractal/index.db-wal",
            ".fractal/index.db-shm",
            "global/e/entry.md",
            "dist/out.js",
            "trace.json",
            "digest.md",
            ".fractal_decision_root",
        ] {
            let path = dir.join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "harness").unwrap();
        }
        std::fs::write(dir.join("src_app.rs"), "fn main() {}").unwrap();

        let status = git(&dir, &["status", "--porcelain"]).unwrap();
        assert!(
            status.contains("src_app.rs"),
            "a node-authored file must be visible: {status}"
        );
        for harness in [
            "tree/",
            ".fractal/",
            "global/",
            "dist/",
            "trace.json",
            "digest.md",
            ".fractal_decision_",
        ] {
            assert!(
                !status.contains(harness),
                "harness path {harness} leaked into git status: {status}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(dir.join(".gitignore")).unwrap(),
            "node_modules/\n",
            "the user's own .gitignore must not be rewritten"
        );

        let sha = commit_node_work(&dir, "root-01", "work").unwrap().unwrap();
        let files = git(&dir, &["show", "--format=", "--name-only", &sha]).unwrap();
        assert_eq!(
            files, "src_app.rs",
            "a node commit must contain only node-authored files"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn untracked_file_counts_as_a_change() {
        let dir = temp_repo("untracked");
        ensure_repo(&dir).unwrap();
        assert!(
            !has_uncommitted_changes(&dir),
            "clean tree must report none"
        );
        std::fs::write(dir.join("new_module.txt"), "fn main() {}").unwrap();
        assert!(
            has_uncommitted_changes(&dir),
            "a brand-new file must count as a change, not be invisible to `git diff HEAD`"
        );
        let summary = worktree_change_summary(&dir, 4000);
        assert!(
            summary.contains("new_module.txt"),
            "evidence must name the new file: {summary}"
        );
        assert!(summary.contains("fn main()"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// H2 regression: a new file longer than the old 2,000-char preview cap must
    /// be shown whole when it fits the overall budget. The trial-3 critic saw the
    /// first 2,000 chars of a complete 3,333-char README with no marker and
    /// failed it as truncated.
    #[test]
    fn new_file_longer_than_2000_chars_is_shown_whole() {
        let dir = temp_repo("longfile");
        ensure_repo(&dir).unwrap();
        let body = "x".repeat(3333);
        let content = format!("{body}\nEND-OF-FILE-MARKER");
        std::fs::write(dir.join("README.md"), &content).unwrap();

        let summary = worktree_change_summary(&dir, 12_000);
        assert!(
            summary.contains("README.md"),
            "the new file must be named: {summary}"
        );
        assert!(
            summary.contains("END-OF-FILE-MARKER"),
            "the end of a >2000-char file must be visible, not silently cut: {} chars shown",
            summary.len()
        );
        assert!(
            !summary.contains("truncated"),
            "a file that fits the budget must carry no truncation marker"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// H2: when the overall budget is genuinely exceeded, the cut is explicit and
    /// blames the harness, not the node, so a critic cannot read it as an
    /// incomplete deliverable.
    #[test]
    fn oversized_evidence_is_explicitly_marked_as_harness_truncation() {
        let dir = temp_repo("overcap");
        ensure_repo(&dir).unwrap();
        std::fs::write(dir.join("big.txt"), "y".repeat(5000)).unwrap();

        let summary = worktree_change_summary(&dir, 500);
        assert!(
            summary.len() <= 500 + 400,
            "the cap must bound the summary: {} bytes",
            summary.len()
        );
        assert!(
            summary.contains("evidence truncated by the harness"),
            "the cut must be explicit: {summary}"
        );
        assert!(
            summary.contains("NOT by the node"),
            "the marker must not accuse the node of truncating its deliverable: {summary}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// H2b regression: a 142 KB lockfile sorted before the source must not
    /// consume the whole evidence budget and hide the implementation. Trial 4's
    /// critic saw only README and the lockfile (and not one line of `src/`).
    #[test]
    fn generated_lockfile_cannot_starve_evidence() {
        let dir = temp_repo("lockstarvation");
        ensure_repo(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"portfolio","scripts":{"build":"tsc","test":"node --test"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/main.ts"),
            "export const PORTFOLIO_IMPLEMENTATION = 'live totals';\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("package-lock.json"),
            format!(
                "{{\"lockfileVersion\":3,\"packages\":{{\"x\":\"{}\"}}}}",
                "z".repeat(142_000)
            ),
        )
        .unwrap();

        let summary = worktree_change_summary(&dir, 12_000);
        assert!(
            summary.contains("src/main.ts"),
            "the implementation must be named despite the lockfile: {} bytes",
            summary.len()
        );
        assert!(
            summary.contains("PORTFOLIO_IMPLEMENTATION"),
            "the implementation must receive a content window, not just its name"
        );
        assert!(
            summary.contains("package-lock.json"),
            "the generated lockfile must still be named so the critic knows it changed"
        );
        assert!(
            !summary.contains(&"z".repeat(200)),
            "lockfile content must not consume the budget"
        );
        assert!(
            summary.contains("tsconfig.json") && summary.contains("package.json"),
            "every hand-authored file must receive a window: {summary}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D5 regression: a JS/TS leaf's `npm install` must not put the dependency
    /// tree into history, and standard build/cache junk must stay out too.
    #[test]
    fn dependency_trees_and_standard_junk_never_enter_history() {
        let dir = temp_repo("deps");
        ensure_repo(&dir).unwrap();

        for junk in [
            "node_modules/left-pad/index.js",
            ".pnpm-store/meta.json",
            "__pycache__/mod.pyc",
            "coverage/lcov.info",
            "npm-debug.log",
        ] {
            let path = dir.join(junk);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "junk").unwrap();
        }
        std::fs::write(dir.join("src_app.ts"), "export const x = 1;\n").unwrap();

        let sha = commit_node_work(&dir, "root-01", "work").unwrap().unwrap();
        let files = git(&dir, &["show", "--format=", "--name-only", &sha]).unwrap();
        assert_eq!(
            files, "src_app.ts",
            "dependency/build junk leaked into the node commit: {files}"
        );
        assert!(
            !has_uncommitted_changes(&dir),
            "ignored junk must not read as a node-authored change"
        );
        assert!(
            dir.join("node_modules/left-pad/index.js").exists(),
            "the ignored tree must stay on disk, only out of history"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An existing project created by an older harness already has the exclude
    /// marker, so a newly added ignore rule must still be merged in.
    #[test]
    fn excludes_add_later_rules_to_an_existing_project() {
        let dir = temp_repo("excludeupgrade");
        git(&dir, &["init", "-q"]).unwrap();
        let exclude = dir.join(".git/info/exclude");
        std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        std::fs::write(
            &exclude,
            "# fractal-harness internals - never commit these into the user's history\ntree/\n",
        )
        .unwrap();

        ensure_repo(&dir).unwrap();

        let content = std::fs::read_to_string(&exclude).unwrap();
        assert!(
            content.lines().any(|l| l.trim() == "node_modules/"),
            "node_modules was not merged into an existing exclude file:\n{content}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D5: runtime state an app writes while a gate runs is recorded in the
    /// harness exclude so it is never committed, while genuine work still is.
    #[test]
    fn runtime_state_written_by_a_gate_is_kept_out_of_history() {
        let dir = temp_repo("runtimestate");
        ensure_repo(&dir).unwrap();

        // The node's own work lands first...
        std::fs::write(dir.join("app.ts"), "export const x = 1;\n").unwrap();
        let before = untracked_files(&dir);
        // ...then a gate launches the app, which writes runtime state.
        std::fs::write(dir.join(".portfolio.json"), "{\"holdings\":[]}").unwrap();

        let runtime: Vec<String> = untracked_files(&dir).difference(&before).cloned().collect();
        assert_eq!(
            runtime,
            vec![".portfolio.json".to_string()],
            "only gate-created state is runtime, the node's source is not"
        );
        ignore_runtime_paths(&dir, &runtime).unwrap();

        let sha = commit_node_work(&dir, "root-01", "work").unwrap().unwrap();
        let files = git(&dir, &["show", "--format=", "--name-only", &sha]).unwrap();
        assert_eq!(files, "app.ts", "runtime state was committed: {files}");
        assert!(
            dir.join(".portfolio.json").exists(),
            "runtime state should remain on disk, just untracked"
        );
        let status = git(&dir, &["status", "--porcelain"]).unwrap();
        assert!(
            !status.contains(".portfolio.json"),
            "runtime state must not pollute status or the next node's diff: {status}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
