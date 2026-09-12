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

use std::path::Path;
use std::process::Command;

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
    if !is_repo(root) {
        git(root, &["init"])?;
    }

    // A fresh repo has no HEAD; several operations (diff, revert, rev-parse
    // HEAD) are undefined until the first commit lands.
    if head_sha(root).is_none() {
        let ignore = root.join(".gitignore");
        if !ignore.exists() {
            let _ = std::fs::write(
                &ignore,
                "node_modules/\ntarget/\ndist/\ntree/\n.fractal/\ntrace.json\ndigest.md\n.fractal_decision_*\n.DS_Store\n",
            );
        }
        git(root, &["add", "-A"])?;
        commit(root, "chore: fractal baseline")?;
    }
    Ok(())
}

pub fn head_sha(root: &Path) -> Option<String> {
    git(root, &["rev-parse", "HEAD"]).ok()
}

/// True when the working tree has no uncommitted change.
pub fn is_clean(root: &Path) -> bool {
    git(root, &["status", "--porcelain"])
        .map(|s| s.is_empty())
        .unwrap_or(false)
}

fn commit(root: &Path, message: &str) -> Result<String, String> {
    // Identity is forced per-invocation so a machine with no global git config
    // still produces commits, without mutating the user's config.
    git(
        root,
        &[
            "-c",
            "user.name=fractal",
            "-c",
            "user.email=fractal@localhost",
            "commit",
            "--no-verify",
            "-q",
            "-m",
            message,
        ],
    )?;
    head_sha(root).ok_or_else(|| "commit produced no HEAD".to_string())
}

/// Stage everything and commit on a node's behalf.
/// `Ok(None)` means the node changed nothing, which is a legitimate outcome for
/// a pure decomposition step and must not be reported as a failure.
pub fn commit_node_work(
    root: &Path,
    node_id: &str,
    summary: &str,
) -> Result<Option<String>, String> {
    git(root, &["add", "-A"])?;
    if is_clean(root) {
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
pub fn has_uncommitted_changes(root: &Path) -> bool {
    git(root, &["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

/// Human-readable evidence of the working tree's change vs HEAD, including
/// untracked files (new files an agent wrote are the common case for a leaf).
/// `git diff HEAD` alone shows nothing for a brand-new file, so previews of
/// untracked files are appended explicitly.
pub fn worktree_change_summary(root: &Path, max_bytes: usize) -> String {
    let mut out = String::new();

    let tracked = git(root, &["diff", "HEAD"]).unwrap_or_default();
    if !tracked.trim().is_empty() {
        out.push_str(&tracked);
        out.push('\n');
    }

    let untracked = git(root, &["ls-files", "--others", "--exclude-standard"]).unwrap_or_default();
    for file in untracked.lines().filter(|l| !l.is_empty()) {
        let content = std::fs::read_to_string(root.join(file)).unwrap_or_default();
        let preview: String = content.chars().take(2000).collect();
        out.push_str(&format!("\n--- new file: {file} ---\n{preview}\n"));
    }

    if out.len() <= max_bytes {
        return out;
    }
    let mut cut = max_bytes;
    while cut > 0 && !out.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n... [diff truncated, {} bytes total]",
        &out[..cut],
        out.len()
    )
}

/// Discard uncommitted noise so a retried attempt starts from the last known
/// good commit instead of inheriting the failed attempt's half-written files.
///
/// Currently exercised only by tests; re-enabled in the scheduler by the
/// follow-up isolation task (per-node isolation: judge each node against its own
/// diff and revert a failed attempt).
#[allow(dead_code)]
pub fn reset_uncommitted(root: &Path) -> Result<(), String> {
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
        assert!(is_clean(&dir), "working tree must be clean after commit");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reset_uncommitted_discards_failed_attempt() {
        let dir = temp_repo("reset");
        ensure_repo(&dir).unwrap();
        std::fs::write(dir.join("garbage.txt"), "half-written").unwrap();
        assert!(!is_clean(&dir));
        reset_uncommitted(&dir).unwrap();
        assert!(is_clean(&dir));
        assert!(!dir.join("garbage.txt").exists());
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
}
