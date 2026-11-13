//! The one place Spork runs git (docs/00-overview.md §3.4) plus the ref reader behind the sidebar (docs/specs/M03).

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitError {
    /// git couldn't be started at all (not installed, bad path).
    GitMissing(String),
    NotARepo,
    IndexLocked,
    AuthFailed,
    NonFastForward,
    Other {
        code: Option<i32>,
        stderr: String,
    },
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::GitMissing(e) => write!(f, "git could not be started: {e}"),
            GitError::NotARepo => f.write_str("This folder is not a git repository."),
            GitError::IndexLocked => {
                f.write_str("Another git process is running (index.lock exists).")
            }
            GitError::AuthFailed => f.write_str("Authentication failed."),
            GitError::NonFastForward => f.write_str("The remote has commits you don't have yet."),
            GitError::Other { stderr, .. } => f.write_str(stderr.trim()),
        }
    }
}

impl std::error::Error for GitError {}

/// Map a failed git run to a friendly error. Relies on `LC_ALL=C`, which `run` always sets.
pub fn classify(code: Option<i32>, stderr: &str) -> GitError {
    let s = stderr;
    if s.contains("not a git repository") {
        GitError::NotARepo
    } else if s.contains("index.lock") && s.contains("File exists") {
        GitError::IndexLocked
    } else if s.contains("Authentication failed")
        || s.contains("could not read Username")
        || s.contains("Permission denied (publickey")
    {
        GitError::AuthFailed
    } else if s.contains("[rejected]")
        && (s.contains("non-fast-forward") || s.contains("fetch first"))
    {
        GitError::NonFastForward
    } else {
        GitError::Other {
            code,
            stderr: stderr.to_owned(),
        }
    }
}

/// Path of the git binary: `SPORK_GIT` if set, else `git` from PATH.
pub fn git_binary() -> PathBuf {
    std::env::var_os("SPORK_GIT")
        .map(PathBuf::from)
        .unwrap_or_else(|| "git".into())
}

/// Run git in `repo` and return stdout. Never prompts on a terminal.
// shortcut: blocking and no stdin/progress/cancel yet; becomes async with jobs when network ops land (P2).
pub fn run(repo: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let mut cmd = Command::new(git_binary());
    cmd.args(["-c", "core.quotepath=false", "-c", "color.ui=false"])
        .args(args)
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        // reads must not take the index lock behind the user's back
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd
        .output()
        .map_err(|e| GitError::GitMissing(e.to_string()))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(classify(
            out.status.code(),
            &String::from_utf8_lossy(&out.stderr),
        ))
    }
}

/// Top-level directory of the repository containing `path`.
pub fn toplevel(path: &Path) -> Result<PathBuf, GitError> {
    let out = run(path, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(String::from_utf8_lossy(&out).trim_end()))
}

/// Current branch name, or `None` when HEAD is detached. Works on an unborn branch.
pub fn head_branch(repo: &Path) -> Result<Option<String>, GitError> {
    match run(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"]) {
        Ok(out) => Ok(Some(String::from_utf8_lossy(&out).trim_end().to_owned())),
        Err(GitError::Other { code: Some(1), .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Local,
    Remote,
    Tag,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    pub kind: RefKind,
    /// Full name, e.g. `refs/remotes/origin/main`.
    pub full: String,
    /// Display name, e.g. `origin/main`.
    pub name: String,
    pub target: String,
    /// Commit the ref points at (annotated tags peeled to their commit).
    pub commit: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Upstream is configured but its remote branch no longer exists.
    pub upstream_gone: bool,
    pub is_head: bool,
}

const REF_FORMAT: &str = "--format=%(refname)%00%(objectname)%00%(upstream:short)%00%(upstream:track,nobracket)%00%(HEAD)%00%(*objectname)";

pub fn refs(repo: &Path) -> Result<Vec<Ref>, GitError> {
    let out = run(
        repo,
        &[
            "for-each-ref",
            REF_FORMAT,
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ],
    )?;
    Ok(parse_refs(&String::from_utf8_lossy(&out)))
}

/// Parse `for-each-ref` output produced with `REF_FORMAT`.
pub fn parse_refs(out: &str) -> Vec<Ref> {
    out.lines()
        .filter_map(|line| {
            let mut f = line.split('\0');
            let full = f.next()?;
            let target = f.next()?;
            let upstream = f.next().unwrap_or("");
            let track = f.next().unwrap_or("");
            let head = f.next().unwrap_or("");
            let peeled = f.next().unwrap_or("");
            let (kind, name) = if let Some(n) = full.strip_prefix("refs/heads/") {
                (RefKind::Local, n)
            } else if let Some(n) = full.strip_prefix("refs/remotes/") {
                (RefKind::Remote, n)
            } else {
                (RefKind::Tag, full.strip_prefix("refs/tags/")?)
            };
            // `origin/HEAD` is a pointer, not a branch
            if kind == RefKind::Remote && name.ends_with("/HEAD") {
                return None;
            }
            let (ahead, behind, upstream_gone) = parse_track(track);
            Some(Ref {
                kind,
                full: full.to_owned(),
                name: name.to_owned(),
                target: target.to_owned(),
                commit: if peeled.is_empty() { target } else { peeled }.to_owned(),
                upstream: (!upstream.is_empty()).then(|| upstream.to_owned()),
                ahead,
                behind,
                upstream_gone,
                is_head: head == "*",
            })
        })
        .collect()
}

/// `"ahead 2, behind 1"` / `"ahead 2"` / `"gone"` / `""` → (ahead, behind, gone).
fn parse_track(track: &str) -> (u32, u32, bool) {
    if track == "gone" {
        return (0, 0, true);
    }
    let (mut ahead, mut behind) = (0, 0);
    for part in track.split(", ") {
        if let Some(n) = part.strip_prefix("ahead ") {
            ahead = n.parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            behind = n.parse().unwrap_or(0);
        }
    }
    (ahead, behind, false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub id: String,
    pub parents: Vec<String>,
    pub author: String,
    pub email: String,
    /// Author time, Unix seconds.
    pub time: i64,
    pub subject: String,
}

const LOG_FORMAT: &str = "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%at%x1f%s";

/// Commits reachable from branches, remote branches, tags and HEAD, newest first (date order), at most `max`.
// shortcut: CLI log; moves to an incremental gix revwalk when repos beyond ~1M commits matter (M05 §1).
pub fn log(repo: &Path, max: usize) -> Result<Vec<Commit>, GitError> {
    let max = format!("--max-count={max}");
    let args = [
        "log",
        "-z",
        "--date-order",
        "--ignore-missing",
        &max,
        LOG_FORMAT,
    ];
    // HEAD may not exist yet (new repo); --ignore-missing skips it instead of failing
    let out = run(
        repo,
        &[
            &args[..],
            &["--branches", "--remotes", "--tags", "HEAD", "--"],
        ]
        .concat(),
    )?;
    Ok(parse_log(&String::from_utf8_lossy(&out)))
}

/// Parse `git log -z` output produced with `LOG_FORMAT`.
pub fn parse_log(out: &str) -> Vec<Commit> {
    out.split('\0')
        .filter_map(|record| {
            let mut f = record.split('\x1f');
            Some(Commit {
                id: f.next().filter(|id| !id.is_empty())?.to_owned(),
                parents: f.next()?.split_whitespace().map(str::to_owned).collect(),
                author: f.next()?.to_owned(),
                email: f.next()?.to_owned(),
                time: f.next()?.parse().ok()?,
                subject: f.next()?.to_owned(),
            })
        })
        .collect()
}

/// Git's well-known empty tree: the "parent" of a root commit.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitDetails {
    /// Full message: subject, blank line, body.
    pub message: String,
    pub author_email: String,
    pub author_date: String,
    pub committer: String,
    pub committer_email: String,
    pub committer_date: String,
    /// Unified patch against the first parent (or the empty tree for a root commit).
    pub patch: String,
}

const DETAILS_FORMAT: &str = "--format=%ae%x1f%ad%x1f%cn%x1f%ce%x1f%cd%x1f%B";

/// Details panel data for one commit (docs/specs/M06). Merges are shown against their first parent.
pub fn commit_details(
    repo: &Path,
    id: &str,
    first_parent: Option<&str>,
) -> Result<CommitDetails, GitError> {
    let meta = run(
        repo,
        &[
            "show",
            "-s",
            "--date=format:%Y-%m-%d %H:%M",
            DETAILS_FORMAT,
            id,
            "--",
        ],
    )?;
    let meta = String::from_utf8_lossy(&meta);
    let mut f = meta.splitn(6, '\x1f');
    let mut next = || f.next().unwrap_or("").to_owned();
    let (author_email, author_date, committer, committer_email, committer_date) =
        (next(), next(), next(), next(), next());
    let message = next().trim_end().to_owned();
    let base = first_parent.unwrap_or(EMPTY_TREE);
    let patch = run(
        repo,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--find-renames",
            base,
            id,
            "--",
        ],
    )?;
    Ok(CommitDetails {
        message,
        author_email,
        author_date,
        committer,
        committer_email,
        committer_date,
        patch: String::from_utf8_lossy(&patch).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args([
                "-c",
                "user.name=Spork Test",
                "-c",
                "user.email=test@spork.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?} failed");
    }

    #[test]
    fn parses_track_and_skips_remote_head() {
        let out = "refs/heads/main\0aaa\0origin/main\0ahead 2, behind 1\0*\n\
                   refs/heads/old\0bbb\0origin/old\0gone\0 \n\
                   refs/remotes/origin/HEAD\0aaa\0\0\0 \n\
                   refs/remotes/origin/main\0ccc\0\0\0 \n\
                   refs/tags/v1.0\0ddd\0\0\0 \n";
        let r = parse_refs(out);
        assert_eq!(r.len(), 4);
        assert_eq!(
            (r[0].name.as_str(), r[0].ahead, r[0].behind, r[0].is_head),
            ("main", 2, 1, true)
        );
        assert_eq!(r[0].upstream.as_deref(), Some("origin/main"));
        assert!(r[1].upstream_gone && !r[1].is_head);
        assert_eq!(
            (r[2].kind, r[2].name.as_str()),
            (RefKind::Remote, "origin/main")
        );
        assert_eq!(
            (r[3].kind, r[3].name.as_str(), r[3].upstream.as_deref()),
            (RefKind::Tag, "v1.0", None)
        );
    }

    #[test]
    fn classifies_common_failures() {
        let c = |s| classify(Some(128), s);
        assert_eq!(
            c("fatal: not a git repository (or any of the parent directories): .git"),
            GitError::NotARepo
        );
        assert_eq!(
            c("fatal: Unable to create '/r/.git/index.lock': File exists."),
            GitError::IndexLocked
        );
        assert_eq!(
            c("fatal: Authentication failed for 'https://x/'"),
            GitError::AuthFailed
        );
        assert_eq!(
            c(" ! [rejected]        main -> main (non-fast-forward)"),
            GitError::NonFastForward
        );
        assert!(matches!(c("boom"), GitError::Other { .. }));
    }

    #[test]
    fn reads_a_real_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git(d, &["init", "-q", "-b", "main"]);
        assert_eq!(head_branch(d).unwrap().as_deref(), Some("main")); // unborn branch
        assert_eq!(log(d, 100).unwrap(), vec![]); // no commits yet is not an error
        git(d, &["commit", "-q", "--allow-empty", "-m", "one"]);
        git(d, &["branch", "feature/x"]);
        git(d, &["tag", "v1"]);
        git(d, &["tag", "-a", "v1-annotated", "-m", "release"]);
        // fake an upstream one commit behind, without any network (the remote URL is never contacted)
        git(
            d,
            &["remote", "add", "origin", "https://example.invalid/r.git"],
        );
        git(d, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(
            d,
            &["branch", "-q", "--set-upstream-to=origin/main", "main"],
        );
        git(d, &["commit", "-q", "--allow-empty", "-m", "two"]);

        let r = refs(d).unwrap();
        let main = r.iter().find(|r| r.full == "refs/heads/main").unwrap();
        assert!(main.is_head);
        assert_eq!((main.ahead, main.behind), (1, 0));
        assert!(
            r.iter()
                .any(|r| r.kind == RefKind::Local && r.name == "feature/x")
        );
        assert!(
            r.iter()
                .any(|r| r.kind == RefKind::Remote && r.name == "origin/main")
        );
        assert!(r.iter().any(|r| r.kind == RefKind::Tag && r.name == "v1"));

        let annotated = r.iter().find(|r| r.name == "v1-annotated").unwrap();
        assert_ne!(annotated.target, annotated.commit); // tag object vs the commit it points at
        assert_eq!(
            annotated.commit,
            r.iter().find(|r| r.name == "v1").unwrap().commit
        );

        git(d, &["switch", "-q", "feature/x"]);
        git(d, &["commit", "-q", "--allow-empty", "-m", "feature work"]);
        git(d, &["switch", "-q", "main"]);
        git(
            d,
            &["merge", "-q", "--no-ff", "-m", "merge feature", "feature/x"],
        );
        let commits = log(d, 100).unwrap();
        assert_eq!(commits[0].subject, "merge feature");
        assert_eq!(commits[0].parents.len(), 2);
        assert_eq!(commits[0].author, "Spork Test");
        assert_eq!(log(d, 2).unwrap().len(), 2);

        // details: root commit diffs against the empty tree, merges against their first parent
        let root = commits.last().unwrap();
        assert!(root.parents.is_empty());
        let details = commit_details(d, &root.id, None).unwrap();
        assert_eq!(details.message, "one");
        assert_eq!(details.committer, "Spork Test");
        assert_eq!(details.author_date.len(), "2026-01-01 00:00".len());

        std::fs::write(d.join("hello.txt"), "hi\n").unwrap();
        git(d, &["add", "hello.txt"]);
        git(d, &["commit", "-q", "-m", "add hello", "-m", "with a body"]);
        let c = &log(d, 1).unwrap()[0];
        let details = commit_details(d, &c.id, c.parents.first().map(String::as_str)).unwrap();
        assert_eq!(details.message, "add hello\n\nwith a body");
        assert!(details.patch.contains("+++ b/hello.txt"));
        assert!(details.patch.contains("+hi"));

        git(d, &["switch", "-q", "--detach", "HEAD"]);
        assert_eq!(head_branch(d).unwrap(), None);
        std::fs::create_dir(d.join("sub")).unwrap();
        let top = toplevel(&d.join("sub")).unwrap();
        assert_eq!(top.canonicalize().unwrap(), d.canonicalize().unwrap());
    }

    #[test]
    fn plain_folder_is_not_a_repo() {
        let tmp = tempfile::tempdir().unwrap();
        // stop git from discovering a repo in a parent of the temp dir
        let err = Command::new(git_binary())
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(tmp.path())
            .env("GIT_CEILING_DIRECTORIES", tmp.path().parent().unwrap())
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert_eq!(
            classify(err.status.code(), &String::from_utf8_lossy(&err.stderr)),
            GitError::NotARepo
        );
    }
}
