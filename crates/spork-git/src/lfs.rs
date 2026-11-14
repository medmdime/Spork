//! Git LFS support (docs/specs/M14): detection, status, tracking, locks, and fetch/pull.
//!
//! Thin wrappers over the `git lfs` subcommands plus pure parsers for their output. A
//! missing `git-lfs` extension or an LFS remote without a locking server surfaces as an
//! error — or an empty result where noted — and never as a panic.

use std::path::{Path, PathBuf};

use glob::Pattern;
use serde::Deserialize;

use crate::{Cancellable, GitError, run, run_with_env, stage};

// --- Detection ---------------------------------------------------------------

/// Parse a `.gitattributes` file into `(pattern, attributes)` pairs.
///
/// Comment (`#`) and blank lines are skipped; attributes split on whitespace, with
/// double-quoted segments (`"foo bar"`) kept together and their quotes dropped — a
/// deliberately minimal quoting model, enough for the `filter=lfs` detection below.
pub fn parse_gitattributes(text: &str) -> Vec<(String, Vec<String>)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let mut tokens = split_ws(line);
            if tokens.is_empty() {
                return None;
            }
            let pattern = tokens.remove(0);
            Some((pattern, tokens))
        })
        .collect()
}

/// Split on whitespace, keeping double-quoted segments together (the quotes are
/// dropped; an unbalanced quote simply treats the rest of the line as quoted).
fn split_ws(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in line.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The patterns in a `.gitattributes` file whose attributes include `filter=lfs`.
pub fn lfs_patterns(text: &str) -> Vec<String> {
    parse_gitattributes(text)
        .into_iter()
        .filter(|(_, attrs)| attrs.iter().any(|a| a == "filter=lfs"))
        .map(|(pattern, _)| pattern)
        .collect()
}

/// Whether the repository uses Git LFS: the working tree's `.gitattributes` or the one
/// at HEAD (best effort) declares an LFS filter.
pub fn lfs_in_use(repo: &Path) -> bool {
    if let Ok(text) = std::fs::read_to_string(repo.join(".gitattributes"))
        && !lfs_patterns(&text).is_empty()
    {
        return true;
    }
    if let Ok(out) = run(repo, &["show", "HEAD:.gitattributes"])
        && !lfs_patterns(&String::from_utf8_lossy(&out)).is_empty()
    {
        return true;
    }
    false
}

/// Whether the `git-lfs` extension is installed (`git lfs version` exits 0).
pub fn lfs_installed() -> bool {
    run(Path::new("."), &["lfs", "version"]).is_ok()
}

/// Parse the output of `git lfs version` into the bare version number
/// (`"git-lfs/3.8.1 (GitHub; darwin arm64; go 1.27.3)"` → `Some("3.8.1")`).
pub fn parse_lfs_version(out: &str) -> Option<String> {
    let line = out.lines().next()?.trim();
    let version = line
        .strip_prefix("git-lfs/")?
        .split_whitespace()
        .next()?
        .trim();
    (!version.is_empty()).then(|| version.to_owned())
}

/// Whether the LFS filters are installed locally: `git lfs install --local` writes
/// `filter.lfs.smudge` and `filter.lfs.clean` into the local config.
pub fn lfs_hooks_installed(repo: &Path) -> bool {
    run(repo, &["config", "--local", "--get", "filter.lfs.smudge"]).is_ok()
        && run(repo, &["config", "--local", "--get", "filter.lfs.clean"]).is_ok()
}

// --- Status ------------------------------------------------------------------

/// One Git LFS file as listed by `git lfs ls-files`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsFile {
    pub path: String,
    pub oid: String,
    pub size: u64,
    /// The object is stored in this repository's LFS object store.
    pub downloaded: bool,
}

#[derive(Deserialize)]
struct LsFileJson {
    name: String,
    size: i64,
    oid: String,
    #[serde(default)]
    downloaded: bool,
}

#[derive(Deserialize)]
struct LsFilesJson {
    files: Vec<LsFileJson>,
}

/// Parse the output of `git lfs ls-files --json` into file rows.
///
/// `--json` is git-lfs' documented stable output for scripts and the only form that
/// carries the exact object size (the human-readable `--long` form prints
/// `<oid> <marker> <path>` without a size). Malformed input yields an empty list,
/// never a panic.
pub fn parse_lfs_ls_files(out: &str) -> Vec<LfsFile> {
    let Ok(LsFilesJson { files }) = serde_json::from_str(out) else {
        return Vec::new();
    };
    files
        .into_iter()
        .map(|f| LfsFile {
            path: f.name,
            oid: f.oid,
            size: f.size.max(0) as u64,
            downloaded: f.downloaded,
        })
        .collect()
}

/// The LFS files of the current checkout with their size and local-download state.
///
/// Runs `git lfs ls-files --json` and then fills `downloaded` by checking the object
/// file in the repository's LFS store (`<git dir>/lfs/objects/<ab>/<cd>/<oid>`, git
/// dir via `rev-parse --absolute-git-dir`, as in gitflow's state file).
pub fn lfs_ls_files(repo: &Path) -> Result<Vec<LfsFile>, GitError> {
    let out = run(repo, &["lfs", "ls-files", "--json"])?;
    let objects = lfs_object_dir(repo)?;
    Ok(parse_lfs_ls_files(&String::from_utf8_lossy(&out))
        .into_iter()
        .map(|f| LfsFile {
            downloaded: lfs_object_exists(&objects, &f.oid).unwrap_or(f.downloaded),
            ..f
        })
        .collect())
}

/// The `<git dir>/lfs/objects` directory.
fn lfs_object_dir(repo: &Path) -> Result<PathBuf, GitError> {
    let out = run(repo, &["rev-parse", "--absolute-git-dir"])?;
    Ok(PathBuf::from(String::from_utf8_lossy(&out).trim_end())
        .join("lfs")
        .join("objects"))
}

/// Whether the object for an LFS oid exists locally; `None` for oids too short to
/// address, in which case the caller falls back to git-lfs' own flag. git-lfs stores
/// objects as `<oid[0:2]>/<oid[2:4]>/<full oid>`.
fn lfs_object_exists(objects: &Path, oid: &str) -> Option<bool> {
    if oid.len() < 4 {
        return None;
    }
    Some(objects.join(&oid[..2]).join(&oid[2..4]).join(oid).is_file())
}

// --- Locks -------------------------------------------------------------------

/// One Git LFS lock as listed by `git lfs locks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsLock {
    pub path: String,
    pub id: String,
    pub locked_at: Option<String>,
    /// The owner's display name.
    pub owner: Option<String>,
    /// The lock belongs to this repository's `user.email`.
    pub mine: bool,
}

#[derive(Deserialize)]
struct LockOwnerJson {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

#[derive(Deserialize)]
struct LockJson {
    id: String,
    path: String,
    #[serde(default)]
    locked_at: Option<String>,
    #[serde(default)]
    owner: Option<LockOwnerJson>,
}

/// Parse the output of `git lfs locks --json`: an array of
/// `{id, path, locked_at, owner: {name, email}, …}` objects (unknown fields such as
/// `custom_fields` are ignored). `mine` is left `false` — only [`lfs_locks`] knows the
/// repository's user and sets it. Malformed JSON is an error, never a panic.
pub fn parse_lfs_locks_json(json: &str) -> Result<Vec<LfsLock>, GitError> {
    let locks = parse_lock_entries(json)?;
    Ok(locks
        .into_iter()
        .map(|l| LfsLock {
            path: l.path,
            id: l.id,
            locked_at: l.locked_at,
            owner: l.owner.and_then(|o| o.name).filter(|n| !n.is_empty()),
            mine: false,
        })
        .collect())
}

/// The shared parse for `locks`-shaped JSON; kept internal so [`lfs_locks`] can still
/// see the owner's email when computing `mine`.
fn parse_lock_entries(json: &str) -> Result<Vec<LockJson>, GitError> {
    serde_json::from_str(json).map_err(|_| GitError::Other {
        code: None,
        stderr: "git lfs locks: malformed JSON output".into(),
    })
}

/// All locks on the remote; `mine` is set for locks whose owner matches the
/// repository's `user.email`. A remote without lock support yields an empty list,
/// not an error.
pub fn lfs_locks(repo: &Path) -> Result<Vec<LfsLock>, GitError> {
    let out = run(repo, &["lfs", "locks", "--json"])?;
    let locks = parse_lock_entries(&String::from_utf8_lossy(&out))?;
    let email = repo_user_email(repo);
    Ok(locks
        .into_iter()
        .map(|l| {
            let mine = match (&email, l.owner.as_ref().and_then(|o| o.email.as_deref())) {
                (Some(me), Some(theirs)) => *me == theirs,
                _ => false,
            };
            LfsLock {
                path: l.path,
                id: l.id,
                locked_at: l.locked_at,
                owner: l.owner.and_then(|o| o.name).filter(|n| !n.is_empty()),
                mine,
            }
        })
        .collect())
}

/// The repository's `user.email` (any config level), if set.
fn repo_user_email(repo: &Path) -> Option<String> {
    let out = run(repo, &["config", "--get", "user.email"]).ok()?;
    let email = String::from_utf8_lossy(&out).trim_end().to_owned();
    (!email.is_empty()).then_some(email)
}

/// Lock `path` on the remote (`git lfs lock [--force] <path>`).
///
/// The command runs with `--json` — the stable lock record, same shape as
/// [`lfs_locks`] — so the returned [`LfsLock`] carries the id and owner, not just the
/// path. A failure (no locking server, someone else's lock without `force`, …) is a
/// [`GitError`] carrying git-lfs' stderr.
pub fn lfs_lock(repo: &Path, path: &str, force: bool) -> Result<LfsLock, GitError> {
    let mut args = vec!["lfs", "lock"];
    if force {
        args.push("--force");
    }
    args.extend(["--json", path]);
    let out = run(repo, &args)?;
    parse_lfs_locks_json(&String::from_utf8_lossy(&out))?
        .into_iter()
        .next()
        .ok_or_else(|| GitError::Other {
            code: None,
            stderr: "git lfs lock returned no lock record".into(),
        })
}

/// Unlock `path` on the remote (`git lfs unlock [--force] <path>`).
pub fn lfs_unlock(repo: &Path, path: &str, force: bool) -> Result<(), GitError> {
    let mut args = vec!["lfs", "unlock"];
    if force {
        args.push("--force");
    }
    args.push(path);
    run(repo, &args).map(drop)
}

/// The warning lines in a `git lfs locks --verify` output: the violations worth
/// surfacing (lines starting with `error` or `locking`, case-insensitively), not the
/// ordinary `path<TAB>owner<TAB>ID:…` rows.
pub fn lfs_locks_verify_warnings(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.starts_with("error") || lower.starts_with("locking")
        })
        .map(str::to_owned)
        .collect()
}

/// Verify the local locks against the remote (`git lfs locks --verify`) and return the
/// warning lines. A failure (e.g. no locking server) yields an empty list, not an
/// error.
pub fn lfs_locks_verify(repo: &Path) -> Vec<String> {
    match run(repo, &["lfs", "locks", "--verify"]) {
        Ok(out) => lfs_locks_verify_warnings(&String::from_utf8_lossy(&out)),
        Err(_) => Vec::new(),
    }
}

// --- Track / untrack -----------------------------------------------------------

/// The tracked files (`git ls-files`) that [`lfs_track`] with `pattern` would take
/// over, matched in memory with [`glob::Pattern`]. A pattern that does not compile, or
/// a repository that cannot be listed, yields an empty list.
pub fn lfs_track_preview(repo: &Path, pattern: &str) -> Vec<String> {
    let Some(glob) = Pattern::new(pattern).ok() else {
        return Vec::new();
    };
    let Ok(out) = run(repo, &["ls-files"]) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out)
        .lines()
        .map(str::to_owned)
        .filter(|path| !path.is_empty() && glob.matches(path))
        .collect()
}

/// Track `pattern` with Git LFS (`git lfs track "<pattern>"`) and stage the updated
/// `.gitattributes` (M14).
pub fn lfs_track(repo: &Path, pattern: &str) -> Result<(), GitError> {
    validate_lfs_pattern(pattern)?;
    run(repo, &["lfs", "track", pattern])?;
    stage(repo, &[".gitattributes"])
}

/// Stop tracking `pattern` (`git lfs untrack "<pattern>"`) and stage the updated
/// `.gitattributes` (M14).
pub fn lfs_untrack(repo: &Path, pattern: &str) -> Result<(), GitError> {
    validate_lfs_pattern(pattern)?;
    run(repo, &["lfs", "untrack", pattern])?;
    stage(repo, &[".gitattributes"])
}

fn validate_lfs_pattern(pattern: &str) -> Result<(), GitError> {
    let trimmed = pattern.trim();
    if trimmed.is_empty() || trimmed != pattern {
        return Err(GitError::Other {
            code: None,
            stderr: format!("invalid LFS pattern: {pattern:?}"),
        });
    }
    Ok(())
}

// --- Fetch / pull, hooks, progress ----------------------------------------------

/// Fetch LFS objects for `remote` (`git lfs fetch <remote>`), with git-lfs progress
/// written to `progress_file` (read it back with [`lfs_progress`]). The git child can
/// be killed through `cancel`.
pub fn lfs_fetch(
    repo: &Path,
    remote: &str,
    progress_file: &Path,
    cancel: &Cancellable,
) -> Result<(), GitError> {
    run_lfs_with_progress(repo, "fetch", remote, progress_file, cancel)
}

/// Pull LFS objects for `remote` (`git lfs pull <remote>`); same progress and
/// cancellation contract as [`lfs_fetch`].
pub fn lfs_pull(
    repo: &Path,
    remote: &str,
    progress_file: &Path,
    cancel: &Cancellable,
) -> Result<(), GitError> {
    run_lfs_with_progress(repo, "pull", remote, progress_file, cancel)
}

fn run_lfs_with_progress(
    repo: &Path,
    command: &str,
    remote: &str,
    progress_file: &Path,
    cancel: &Cancellable,
) -> Result<(), GitError> {
    let progress = progress_file.to_string_lossy();
    run_with_env(
        repo,
        &["lfs", command, remote],
        None,
        &[("GIT_LFS_PROGRESS", &progress)],
        Some(cancel),
    )
    .map(drop)
}

/// The last non-empty line git-lfs wrote to a `GIT_LFS_PROGRESS` file (the progress
/// line); `""` when the file is missing or empty.
pub fn lfs_progress(progress_file: &Path) -> String {
    std::fs::read_to_string(progress_file)
        .ok()
        .and_then(|text| {
            text.lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

/// Install the LFS hooks for this repository only (`git lfs install --local`).
pub fn lfs_install_hooks(repo: &Path) -> Result<(), GitError> {
    run(repo, &["lfs", "install", "--local"]).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::git;

    /// Real-`git lfs` tests are skipped where the extension is not installed (the
    /// pure-parse tests below run everywhere).
    fn require_lfs() -> bool {
        lfs_installed()
    }

    // --- pure parsers ---------------------------------------------------------

    #[test]
    fn parses_gitattributes_with_comments_and_quoted_values() {
        let text = "# a comment\n\n*.png filter=lfs diff=lfs merge=lfs -text\n*.md -text\n\"foo bar\" whitespace=pre\n";
        let expected: Vec<(String, Vec<String>)> = vec![
            (
                "*.png",
                vec!["filter=lfs", "diff=lfs", "merge=lfs", "-text"],
            ),
            ("*.md", vec!["-text"]),
            ("foo bar", vec!["whitespace=pre"]),
        ]
        .into_iter()
        .map(|(p, a)| (p.to_owned(), a.into_iter().map(String::from).collect()))
        .collect();
        assert_eq!(parse_gitattributes(text), expected);
    }

    #[test]
    fn lfs_patterns_finds_filter_lfs_only() {
        let text = "# none here\n*.txt -text\n*.bin filter=lfs diff=lfs merge=lfs -text\n*.png filter=lfs\n";
        assert_eq!(
            lfs_patterns(text),
            vec!["*.bin".to_owned(), "*.png".to_owned()]
        );
    }

    #[test]
    fn parses_lfs_version() {
        assert_eq!(
            parse_lfs_version("git-lfs/3.8.1 (GitHub; darwin arm64; go 1.27.3)\n").as_deref(),
            Some("3.8.1")
        );
        assert_eq!(parse_lfs_version(""), None);
        assert_eq!(parse_lfs_version("git: 'lfs' is not a git command\n"), None);
    }

    #[test]
    fn parses_ls_files_json_with_space_in_path() {
        let json = r#"{
 "files": [
  {
   "name": "data.bin",
   "size": 12,
   "checkout": true,
   "downloaded": true,
   "oid_type": "sha256",
   "oid": "97398960288b86af38fa6ab5d4d7627646e25160798c00998f7250efeba4d30c",
   "version": "https://git-lfs.github.com/spec/v1"
  },
  {
   "name": "my model.obj",
   "size": 1048576,
   "checkout": false,
   "downloaded": false,
   "oid_type": "sha256",
   "oid": "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899",
   "version": "https://git-lfs.github.com/spec/v1"
  }
 ]
}"#;
        let files = parse_lfs_ls_files(json);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "data.bin");
        assert_eq!(
            files[0].oid,
            "97398960288b86af38fa6ab5d4d7627646e25160798c00998f7250efeba4d30c"
        );
        assert_eq!(files[0].size, 12);
        assert!(files[0].downloaded);
        assert_eq!(files[1].path, "my model.obj");
        assert_eq!(files[1].size, 1_048_576);
        assert!(!files[1].downloaded);
    }

    #[test]
    fn parse_ls_files_malformed_is_empty() {
        assert!(parse_lfs_ls_files("not json").is_empty());
        assert!(parse_lfs_ls_files("{}").is_empty());
        assert!(parse_lfs_ls_files("").is_empty());
    }

    #[test]
    fn parses_locks_json_with_and_without_owner() {
        let json = r#"[
  {
    "id": "1",
    "path": "data.bin",
    "locked_at": "2025-11-01T10:00:00Z",
    "owner": { "name": "Spork Test", "email": "test@spork.invalid" },
    "custom_fields": {}
  },
  { "id": "2", "path": "other.bin" }
]"#;
        let locks = parse_lfs_locks_json(json).unwrap();
        assert_eq!(locks.len(), 2);
        assert_eq!(
            (locks[0].path.as_str(), locks[0].id.as_str()),
            ("data.bin", "1")
        );
        assert_eq!(locks[0].locked_at.as_deref(), Some("2025-11-01T10:00:00Z"));
        assert_eq!(locks[0].owner.as_deref(), Some("Spork Test"));
        assert!(!locks[0].mine);
        assert_eq!(
            (locks[1].path.as_str(), locks[1].id.as_str()),
            ("other.bin", "2")
        );
        assert_eq!(locks[1].locked_at, None);
        assert_eq!(locks[1].owner, None);
        assert!(!locks[1].mine);
    }

    #[test]
    fn parse_locks_json_malformed_is_error() {
        assert!(parse_lfs_locks_json("not json").is_err());
        assert!(parse_lfs_locks_json("42").is_err());
        assert!(parse_lfs_locks_json("[{}]").is_err());
    }

    #[test]
    fn picks_out_lock_verify_warning_lines() {
        let out = "data.bin\tSpork Test\tID:1\nLocking other.bin failed: lock not found\nError while retrieving locks: boom\n";
        assert_eq!(
            lfs_locks_verify_warnings(out),
            vec![
                "Locking other.bin failed: lock not found",
                "Error while retrieving locks: boom",
            ]
        );
        assert!(lfs_locks_verify_warnings("data.bin\tSpork Test\tID:1\n").is_empty());
    }

    #[test]
    fn lfs_progress_keeps_last_non_empty_line() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("progress");
        assert_eq!(lfs_progress(&file), ""); // missing file
        std::fs::write(&file, "line one\n\nline two\r\n").unwrap();
        assert_eq!(lfs_progress(&file), "line two");
    }

    // --- real-git round trips ---------------------------------------------------

    #[test]
    fn lfs_is_installed_in_the_test_environment() {
        assert!(require_lfs());
    }

    #[test]
    fn lfs_in_use_detects_working_tree_and_head_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "-q", "-b", "main"]);
        assert!(!lfs_in_use(d));

        std::fs::write(d.join(".gitattributes"), "*.png filter=lfs diff=lfs\n").unwrap();
        assert!(lfs_in_use(d));

        // still in use once the working-tree file is gone but HEAD has it
        git(d, &["add", ".gitattributes"]);
        git(d, &["commit", "-q", "-m", "attrs"]);
        std::fs::remove_file(d.join(".gitattributes")).unwrap();
        assert!(lfs_in_use(d));
    }

    #[test]
    fn track_commit_ls_files_and_untrack_round_trip() {
        if !require_lfs() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "-q", "-b", "main"]);
        lfs_install_hooks(d).unwrap();
        lfs_track(d, "*.bin").unwrap();

        // .gitattributes carries the filter and is staged (M14)
        let attrs = std::fs::read_to_string(d.join(".gitattributes")).unwrap();
        assert!(attrs.contains("*.bin filter=lfs"), "unexpected: {attrs}");
        let status = crate::status(d).unwrap();
        assert!(
            status
                .iter()
                .any(|e| e.path == ".gitattributes" && e.has_staged()),
            ".gitattributes should be staged"
        );

        // commit a real binary file
        let content: Vec<u8> = (0..=255u8).collect();
        std::fs::write(d.join("data.bin"), &content).unwrap();
        git(d, &["add", "data.bin"]);
        git(d, &["commit", "-q", "-m", "lfs bin"]);

        let files = lfs_ls_files(d).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "data.bin");
        assert_eq!(files[0].size, content.len() as u64);
        assert_eq!(files[0].oid.len(), 64);
        assert!(files[0].downloaded);

        // untrack drops the pattern from .gitattributes
        lfs_untrack(d, "*.bin").unwrap();
        let attrs = std::fs::read_to_string(d.join(".gitattributes")).unwrap();
        assert!(!attrs.contains("filter=lfs"));

        // pattern validation
        assert!(lfs_track(d, "").is_err());
        assert!(lfs_track(d, " *.bin ").is_err());
        assert!(lfs_untrack(d, "").is_err());
    }

    #[test]
    fn track_preview_matches_tracked_files_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "-q", "-b", "main"]);
        for f in ["a.png", "images/b.png", "images/sub/c.png", "d.txt"] {
            let p = d.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "x").unwrap();
        }
        git(d, &["add", "-A"]);

        assert_eq!(
            lfs_track_preview(d, "*.png"),
            vec![
                "a.png".to_owned(),
                "images/b.png".to_owned(),
                "images/sub/c.png".to_owned(),
            ]
        );
        assert_eq!(
            lfs_track_preview(d, "images/**"),
            vec!["images/b.png".to_owned(), "images/sub/c.png".to_owned()]
        );
        assert!(lfs_track_preview(d, "[").is_empty()); // bad pattern: no panic
    }

    #[test]
    fn lfs_round_trip_across_bare_origin() {
        if !require_lfs() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // a bare "origin" and a working repo pushing to it
        git(d, &["init", "--bare", "-q", "-b", "main", "origin.git"]);
        git(d, &["init", "-q", "-b", "main", "work"]);
        let work = d.join("work");
        git(
            &work,
            &[
                "remote",
                "add",
                "origin",
                d.join("origin.git").to_str().unwrap(),
            ],
        );

        // hooks: off before, on after install
        assert!(!lfs_hooks_installed(&work));
        lfs_install_hooks(&work).unwrap();
        assert!(lfs_hooks_installed(&work));

        lfs_track(&work, "*.bin").unwrap();
        let content: Vec<u8> = (0..=255u8).collect();
        std::fs::write(work.join("data.bin"), &content).unwrap();
        git(&work, &["add", ".gitattributes", "data.bin"]);
        git(&work, &["commit", "-q", "-m", "lfs bin"]);
        git(&work, &["push", "-q", "origin", "main"]);

        // a fresh clone that skipped the smudge: pointer in the work tree, no object
        let fresh = d.join("fresh");
        run_with_env(
            d,
            &[
                "clone",
                "-q",
                d.join("origin.git").to_str().unwrap(),
                "fresh",
            ],
            None,
            &[("GIT_LFS_SKIP_SMUDGE", "1")],
            None,
        )
        .unwrap();
        let pointer = std::fs::read_to_string(fresh.join("data.bin")).unwrap();
        assert!(pointer.starts_with("version https://git-lfs.github.com/spec/v1"));
        let files = lfs_ls_files(&fresh).unwrap();
        assert_eq!(files.len(), 1);
        assert!(!files[0].downloaded);

        // fetch with a progress file, then checkout materializes the object
        let progress = d.join("progress.txt");
        lfs_fetch(&fresh, "origin", &progress, &Cancellable::default()).unwrap();
        assert!(progress.is_file());
        assert_eq!(lfs_progress(&progress), ""); // tiny file: no progress line

        lfs_install_hooks(&fresh).unwrap();
        git(&fresh, &["lfs", "checkout"]);
        assert_eq!(std::fs::read(fresh.join("data.bin")).unwrap(), content);
        let files = lfs_ls_files(&fresh).unwrap();
        assert!(files[0].downloaded);
    }

    #[test]
    fn locks_without_a_locking_server_are_errors_or_empty() {
        if !require_lfs() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "--bare", "-q", "-b", "main", "origin.git"]);
        git(d, &["init", "-q", "-b", "main", "work"]);
        let work = d.join("work");
        git(
            &work,
            &[
                "remote",
                "add",
                "origin",
                d.join("origin.git").to_str().unwrap(),
            ],
        );
        git(&work, &["commit", "-q", "--allow-empty", "-m", "one"]);
        git(&work, &["push", "-q", "origin", "main"]);

        // no locking server: lock/unlock are errors, listing and verify are empty
        assert!(lfs_lock(&work, "data.bin", false).is_err());
        assert!(lfs_unlock(&work, "data.bin", false).is_err());
        assert!(lfs_locks(&work).unwrap().is_empty());
        assert!(lfs_locks_verify(&work).is_empty());
    }
}
