//! Lean / GitHub-flow branching (M13 §4): a default branch plus short-lived feature
//! branches, no git-flow machinery. Start and finish are thin, conflict-tolerant
//! wrappers over plain git calls — no state file (a conflicted merge is surfaced through
//! the existing op-state conflict machinery so the app's conflict UI handles it).

use std::path::Path;

use crate::{GitError, Outcome, allow_conflicts, head_branch, run};

/// The repository's default branch (M13 §4). Resolution order: the `origin/HEAD`
/// remote-tracking ref (set by `git remote set-head origin --auto`), else `main`, else
/// `master`, else the branch we are currently on. Never panics — malformed or missing
/// output falls through to the next candidate.
pub fn default_branch(repo: &Path) -> String {
    if let Some(name) = origin_head_branch(repo) {
        return name;
    }
    for candidate in ["main", "master"] {
        if branch_exists(repo, candidate) {
            return candidate.to_owned();
        }
    }
    head_branch(repo)
        .ok()
        .flatten()
        .unwrap_or_else(|| "main".to_owned())
}

/// The branch `refs/remotes/origin/HEAD` points at, as a bare branch name. `None` when the
/// ref is missing or malformed (it is deliberately not visible through `parse_refs`).
fn origin_head_branch(repo: &Path) -> Option<String> {
    let out = run(repo, &["symbolic-ref", "refs/remotes/origin/HEAD"]).ok()?;
    let lost = String::from_utf8_lossy(&out);
    let refname = lost.trim_end();
    let name = refname.strip_prefix("refs/remotes/origin/")?;
    let name = name.trim_end();
    if name.is_empty() {
        None
    } else {
        Some(name.to_owned())
    }
}

/// Whether a local branch `name` exists.
fn branch_exists(repo: &Path, name: &str) -> bool {
    run(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ],
    )
    .is_ok()
}

/// A valid lean branch name: non-empty, no whitespace, no empty path segments (the "/"
/// separator is allowed for `feature/login`-style folders).
fn valid_branch_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(char::is_whitespace)
        && name.split('/').all(|seg| !seg.is_empty())
}

/// Starts a lean feature branch (M13 §4): `git switch -c <name> <base>`. `name` may use
/// "/" folders but must not contain whitespace or empty segments; `base` must exist.
/// Returns the new branch name.
pub fn lean_start(repo: &Path, name: &str, base: &str) -> Result<String, GitError> {
    if !valid_branch_name(name) {
        return Err(GitError::Other {
            code: None,
            stderr: format!("invalid branch name: {name:?}"),
        });
    }
    if !branch_exists(repo, base) {
        return Err(GitError::Other {
            code: None,
            stderr: format!("base branch does not exist: {base}"),
        });
    }
    run(repo, &["switch", "-c", name, base])?;
    Ok(name.to_owned())
}

/// Finishes a lean feature branch (M13 §4): switch to `target`, merge the branch we came
/// from with `--no-ff`, then delete it when `delete_branch` is set. Runs on the branch
/// being finished — it is resolved before we switch away. A conflicted merge is **not**
/// an error: it is left in place and surfaced through the op-state conflict machinery, so
/// the caller's conflict UI drives continue/abort (no resume state machine).
pub fn lean_finish(repo: &Path, target: &str, delete_branch: bool) -> Result<(), GitError> {
    if !branch_exists(repo, target) {
        return Err(GitError::Other {
            code: None,
            stderr: format!("target branch does not exist: {target}"),
        });
    }
    let current = head_branch(repo)?.ok_or_else(|| GitError::Other {
        code: None,
        stderr: "cannot finish a lean branch from a detached HEAD".to_owned(),
    })?;
    run(repo, &["switch", target])?;
    match allow_conflicts(
        repo,
        run(repo, &["merge", "--no-ff", "--no-edit", &current]),
    )? {
        Outcome::Done => {}
        Outcome::Conflicts => return Ok(()),
    }
    if delete_branch {
        run(repo, &["branch", "-d", &current])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::tests::git;

    fn init(d: &Path, default: &str) {
        git(d, &["init", "-b", default]);
        git(d, &["config", "user.name", "t"]);
        git(d, &["config", "user.email", "t@t.t"]);
        git(d, &["commit", "--allow-empty", "-m", "first"]);
    }

    fn out(d: &Path, args: &[&str]) -> String {
        String::from_utf8_lossy(&run(d, args).unwrap())
            .trim_end()
            .to_owned()
    }

    #[test]
    fn lean_start_creates_a_branch_from_base() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        init(d, "main");
        let name = lean_start(d, "feature/login", "main").unwrap();
        assert_eq!(name, "feature/login");
        assert_eq!(
            out(d, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "feature/login"
        );
    }

    #[test]
    fn lean_start_accepts_folder_names() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        init(d, "main");
        lean_start(d, "feat/a/b/c", "main").unwrap();
        assert_eq!(out(d, &["rev-parse", "--abbrev-ref", "HEAD"]), "feat/a/b/c");
    }

    #[test]
    fn lean_start_rejects_empty_and_whitespace_names() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        init(d, "main");
        assert!(lean_start(d, "", "main").is_err());
        assert!(lean_start(d, "   ", "main").is_err());
        assert!(lean_start(d, "a//b", "main").is_err());
        assert!(lean_start(d, "a b", "main").is_err());
        assert!(lean_start(d, "/a", "main").is_err());
        assert!(lean_start(d, "a/", "main").is_err());
        // the repo was left untouched
        assert_eq!(out(d, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    }

    #[test]
    fn lean_start_requires_an_existing_base() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        init(d, "main");
        assert!(lean_start(d, "feature/x", "nope").is_err());
    }

    #[test]
    fn lean_finish_merges_no_ff_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        init(d, "main");
        lean_start(d, "feature/login", "main").unwrap();
        // a real commit on the branch so the merge has something to bring in
        std::fs::write(d.join("login.txt"), "login").unwrap();
        git(d, &["add", "login.txt"]);
        git(d, &["commit", "-m", "add login"]);

        lean_finish(d, "main", true).unwrap();
        // back on main
        assert_eq!(out(d, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
        // the merge is a no-ff merge commit with two parents
        let parents = out(d, &["rev-list", "--parents", "-n", "1", "HEAD"]);
        let parent_count = parents.split_whitespace().count() - 1;
        assert_eq!(parent_count, 2);
        // the branch is gone
        assert!(!branch_exists(d, "feature/login"));
    }

    #[test]
    fn lean_finish_keeps_the_branch_when_not_deleting() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        init(d, "main");
        lean_start(d, "feature/login", "main").unwrap();
        std::fs::write(d.join("login.txt"), "login").unwrap();
        git(d, &["add", "login.txt"]);
        git(d, &["commit", "-m", "add login"]);

        lean_finish(d, "main", false).unwrap();
        assert_eq!(out(d, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
        assert!(branch_exists(d, "feature/login"));
    }

    #[test]
    fn lean_finish_conflict_surfaces_as_merging_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        init(d, "main");
        // a base commit that both sides will diverge from
        std::fs::write(d.join("f.txt"), "base\n").unwrap();
        git(d, &["add", "f.txt"]);
        git(d, &["commit", "-m", "base f"]);
        // branch, then advance main and the branch differently on the same file
        lean_start(d, "feature/x", "main").unwrap();
        git(d, &["switch", "main"]);
        std::fs::write(d.join("f.txt"), "from main\n").unwrap();
        git(d, &["add", "f.txt"]);
        git(d, &["commit", "-m", "main f"]);
        git(d, &["switch", "feature/x"]);
        std::fs::write(d.join("f.txt"), "from branch\n").unwrap();
        git(d, &["add", "f.txt"]);
        git(d, &["commit", "-m", "branch f"]);

        // the merge conflicts but is surfaced, not an error
        lean_finish(d, "main", true).unwrap();
        assert!(matches!(
            crate::op_state(d).unwrap(),
            crate::OpState::Merging
        ));
    }

    #[test]
    fn default_branch_reads_origin_head() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        init(d, "main");
        // fake an origin/HEAD remote-tracking ref pointing at develop (symbolic ref,
        // which does not require the target to exist)
        git(
            d,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/develop",
            ],
        );
        assert_eq!(default_branch(d), "develop");
    }

    #[test]
    fn default_branch_falls_back_to_main_then_master() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        init(d, "main"); // on main
        assert_eq!(default_branch(d), "main");

        // no main, but master exists
        let dir2 = tempfile::tempdir().unwrap();
        let d2 = dir2.path();
        init(d2, "master");
        assert_eq!(default_branch(d2), "master");
    }
}
