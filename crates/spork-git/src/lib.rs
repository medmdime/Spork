//! The one place Spork runs git (docs/00-overview.md §3.4) plus the ref reader behind the sidebar (docs/specs/M03).

pub mod gitflow;
pub mod lean;
pub mod lfs;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
// shortcut: blocking, no progress/cancel yet; network ops get a job runner with progress in M09.
pub fn run(repo: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
    run_with_input(repo, args, None)
}

/// Like [`run`], feeding `input` to git's stdin (commit messages, pathspec lists).
pub fn run_with_input(
    repo: &Path,
    args: &[&str],
    input: Option<&[u8]>,
) -> Result<Vec<u8>, GitError> {
    run_with_env(repo, args, input, &[], None)
}

/// A handle that can kill the git child of a [`run_cancellable`] call. Cloning is cheap;
/// `kill` is safe from the UI thread and a no-op when nothing is running. Dropping the
/// task that started the run is not enough on its own — the git process keeps running
/// until it is killed or it exits on its own (M05 §5). One handle covers one run; make
/// a fresh one for each run.
#[derive(Clone, Default)]
pub struct Cancellable {
    kill: Arc<AtomicBool>,
}

impl Cancellable {
    /// Requests that the running child be killed. The blocking run returns an error
    /// within milliseconds.
    pub fn kill(&self) {
        self.kill.store(true, Ordering::SeqCst);
    }
}

/// Like [`run`], but the spawned child can be killed through `cancel`. Blocks until the
/// child exits — on its own, or shortly after `cancel.kill()`.
pub fn run_cancellable(
    repo: &Path,
    args: &[&str],
    cancel: &Cancellable,
) -> Result<Vec<u8>, GitError> {
    run_with_env(repo, args, None, &[], Some(cancel))
}

/// Like [`run_with_input`], with extra environment variables for this one command.
/// When `cancel` is given, the child can be killed through the handle (M05 §5).
pub(crate) fn run_with_env(
    repo: &Path,
    args: &[&str],
    input: Option<&[u8]>,
    env: &[(&str, &str)],
    cancel: Option<&Cancellable>,
) -> Result<Vec<u8>, GitError> {
    let mut cmd = Command::new(git_binary());
    cmd.args(["-c", "core.quotepath=false", "-c", "color.ui=false"])
        .args(args)
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        // reads must not take the index lock behind the user's back
        .env("GIT_OPTIONAL_LOCKS", "0")
        // never wait for an editor nobody sees: Spork always passes messages explicitly
        .env("GIT_EDITOR", "true")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    askpass::configure(&mut cmd);
    cmd.envs(env.iter().copied());
    let mut child = cmd
        .spawn()
        .map_err(|e| GitError::GitMissing(e.to_string()))?;
    if let Some(input) = input {
        // write from a thread so a large stdin can't deadlock against a full stdout pipe
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let input = input.to_vec();
        std::thread::spawn(move || std::io::Write::write_all(&mut stdin, &input));
    }
    let out = if let Some(cancel) = cancel {
        // the child stays owned by the reaper thread; only it may call `kill` on it,
        // so cancellation is a flag, never a handover of the handle
        let kill = cancel.kill.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break collect_output(child, status),
                    Ok(None) => {
                        if kill.load(Ordering::SeqCst) {
                            child.kill().ok();
                        } else {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => break Err(e),
                }
            };
            let _ = tx.send(result);
        });
        rx.recv()
            .map_err(|e| GitError::Other {
                code: None,
                stderr: e.to_string(),
            })?
            .map_err(|e| GitError::GitMissing(e.to_string()))?
    } else {
        child
            .wait_with_output()
            .map_err(|e| GitError::GitMissing(e.to_string()))?
    };
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(classify(
            out.status.code(),
            &String::from_utf8_lossy(&out.stderr),
        ))
    }
}

/// Collects the output an already-reaped child left in its pipes.
fn collect_output(mut child: Child, status: ExitStatus) -> io::Result<Output> {
    let mut stdout = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        std::io::Read::read_to_end(&mut pipe, &mut stdout)?;
    }
    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.stderr.take() {
        std::io::Read::read_to_end(&mut pipe, &mut stderr)?;
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
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
pub(crate) fn parse_track(track: &str) -> (u32, u32, bool) {
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

/// Fields of a deep commit search (M05 §5); a field only takes effect when it is not blank.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchQuery {
    /// `--grep` (case-insensitive) on the subject.
    pub message: String,
    /// `--author` regex.
    pub author: String,
    /// `--committer` regex.
    pub committer: String,
    /// Pickaxe regex over each commit's diff (`-G`).
    pub content: String,
    /// Pathspec: only commits touching this path.
    pub path: String,
    /// `--since` date.
    pub since: String,
    /// `--until` date.
    pub until: String,
}

/// The `git log` arguments for a deep search (M05 §5). Pure, so it is testable without a repo.
pub fn build_search_args(q: &SearchQuery) -> Vec<String> {
    let has = |s: &str| !s.trim().is_empty();
    let mut args = vec![
        "log".to_string(),
        "-z".to_string(),
        "--all".to_string(),
        LOG_FORMAT.to_string(),
    ];
    if has(&q.message) {
        args.extend([
            "--grep".to_string(),
            q.message.trim().to_string(),
            "-i".to_string(),
        ]);
    }
    if has(&q.author) {
        args.extend(["--author".to_string(), q.author.trim().to_string()]);
    }
    if has(&q.committer) {
        args.extend(["--committer".to_string(), q.committer.trim().to_string()]);
    }
    if has(&q.content) {
        args.extend(["-G".to_string(), q.content.trim().to_string()]);
    }
    if has(&q.since) {
        args.extend(["--since".to_string(), q.since.trim().to_string()]);
    }
    if has(&q.until) {
        args.extend(["--until".to_string(), q.until.trim().to_string()]);
    }
    if has(&q.path) {
        args.extend(["--".to_string(), q.path.trim().to_string()]);
    }
    args
}

/// Deep commit search (M05 §5): `git log --all` with the query's filters, in commit-row shape.
/// The git child is registered in `cancel` so the caller can kill it (see [`run_cancellable`]).
pub fn search(repo: &Path, q: &SearchQuery, cancel: &Cancellable) -> Result<Vec<Commit>, GitError> {
    let args = build_search_args(q);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run_cancellable(repo, &args, cancel)?;
    Ok(parse_log(&String::from_utf8_lossy(&out)))
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

// ---------------------------------------------------------------------------
// File history (docs/specs/M08 §6)
// ---------------------------------------------------------------------------

/// One commit in a file's history (docs/specs/M08 §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCommit {
    pub oid: String,
    pub author: String,
    /// Author time, Unix seconds.
    pub date: i64,
    pub subject: String,
    /// The path the file has in this commit's tree; an older name when the file
    /// was renamed later on.
    pub path: String,
    /// Set when this commit itself renamed the file: the previous path.
    pub renamed_from: Option<String>,
}

/// The commits that touched one path, following renames, newest first
/// (docs/specs/M08 §6): `git log --follow -z --name-status` in commit-row shape.
/// The git child is registered in `cancel` so the caller can kill it
/// (see [`run_cancellable`]).
pub fn file_history(
    repo: &Path,
    path: &str,
    cancel: &Cancellable,
) -> Result<Vec<FileCommit>, GitError> {
    let out = run_cancellable(
        repo,
        &[
            "log",
            "--follow",
            "-z",
            "--name-status",
            LOG_FORMAT,
            "--",
            path,
        ],
        cancel,
    )?;
    parse_file_history(&String::from_utf8_lossy(&out))
}

/// Parses [`file_history`] output. Records are NUL-separated: a commit row
/// (`LOG_FORMAT` fields) followed by that commit's one name-status entry — a
/// status letter (`M`, `A`, `D`, `T`, or `R`/`C` with an optional similarity
/// score) and the touched path(s). Malformed output is an error, not a skip.
pub fn parse_file_history(out: &str) -> Result<Vec<FileCommit>, GitError> {
    fn malformed(msg: String) -> GitError {
        GitError::Other {
            code: None,
            stderr: format!("file history: malformed output ({msg})"),
        }
    }
    /// A record is a commit row only when all six `LOG_FORMAT` fields are present
    /// and the date parses; anything else is a name-status token.
    fn commit_row(record: &str) -> Option<FileCommit> {
        let mut f = record.splitn(6, '\x1f');
        let (oid, _parents, author, _email, date, subject) = (
            f.next()?,
            f.next()?,
            f.next()?,
            f.next()?,
            f.next()?,
            f.next()?,
        );
        if oid.is_empty() {
            return None;
        }
        Some(FileCommit {
            oid: oid.to_owned(),
            author: author.to_owned(),
            date: date.parse().ok()?,
            subject: subject.to_owned(),
            path: String::new(),
            renamed_from: None,
        })
    }
    let mut entries: Vec<FileCommit> = Vec::new();
    let mut current: Option<FileCommit> = None;
    let mut it = out.split('\0');
    while let Some(record) = it.next() {
        let record = record.trim_start_matches(['\n', '\r']);
        if record.is_empty() {
            continue;
        }
        if let Some(row) = commit_row(record) {
            if let Some(prev) = current.take() {
                if prev.path.is_empty() {
                    return Err(malformed(format!("commit {} has no file status", prev.oid)));
                }
                entries.push(prev);
            }
            current = Some(row);
            continue;
        }
        let entry = current
            .as_mut()
            .ok_or_else(|| malformed(format!("status without a commit row: {record:?}")))?;
        let (status, rest) = record.split_at(1);
        let rename = match status {
            "M" | "A" | "D" | "T" => false,
            "R" | "C" => {
                if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(malformed(format!(
                        "rename status without a score: {record:?}"
                    )));
                }
                true
            }
            _ => return Err(malformed(format!("unknown status: {record:?}"))),
        };
        let old = it
            .next()
            .ok_or_else(|| malformed("missing status path".into()))?
            .to_owned();
        let new = if rename {
            it.next()
                .ok_or_else(|| malformed("missing status path".into()))?
                .to_owned()
        } else {
            old.clone()
        };
        entry.path = new;
        if rename {
            entry.renamed_from = Some(old);
        }
    }
    if let Some(prev) = current {
        if prev.path.is_empty() {
            return Err(malformed(format!("commit {} has no file status", prev.oid)));
        }
        entries.push(prev);
    }
    Ok(entries)
}

/// The unified diff of one history entry's change to the file
/// (docs/specs/M08 §6). A renamed file is diffed between its old and new name so
/// the hunks survive; a root commit diffs against the empty tree.
pub fn file_history_diff(
    repo: &Path,
    oid: &str,
    path: &str,
    renamed_from: Option<&str>,
) -> Result<String, GitError> {
    let raw = match renamed_from {
        Some(old) if old != path => vec![old, path],
        _ => vec![path],
    };
    let paths: Vec<String> = raw.iter().map(|p| literal(p)).collect();
    let mut args: Vec<&str> = vec![
        "show",
        "--format=",
        "--no-color",
        "--no-ext-diff",
        "--find-renames",
        oid,
        "--",
    ];
    args.extend(paths.iter().map(String::as_str));
    Ok(String::from_utf8_lossy(&run(repo, &args)?).into_owned())
}

/// The content of a file at one commit (docs/specs/M08 §6, "open at revision").
pub fn show_file(repo: &Path, oid: &str, path: &str) -> Result<Vec<u8>, GitError> {
    run(repo, &["show", &format!("{oid}:{path}")])
}

/// Restores a working-tree file from one commit (docs/specs/M08 §6,
/// "checkout this version").
pub fn restore_file(repo: &Path, oid: &str, path: &str) -> Result<(), GitError> {
    run(repo, &["restore", "--source", oid, "--", &literal(path)])?;
    Ok(())
}

/// One line of a file at one revision, and the commit that last touched it
/// (docs/specs/M08 §5). Consecutive lines from the same commit carry the
/// same metadata; the UI groups them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameLine {
    /// The 1-based line number within the file at the blamed revision.
    pub line: usize,
    /// The commit that last touched the line (after `-M -C` move detection).
    pub final_oid: String,
    /// Author of `final_oid`; "" for boundary lines whose origin is unknown.
    pub author: String,
    /// Author time of `final_oid`, Unix seconds; 0 if the origin is unknown.
    pub author_date: i64,
    /// Subject of `final_oid`; "" if the origin is unknown.
    pub summary: String,
}

/// Blame one path at one revision (docs/specs/M08 §5):
/// `git blame --porcelain -M -C [-w]`. The git child process is registered in
/// `cancel` so the caller can kill it (see [`run_cancellable`]).
pub fn blame(
    repo: &Path,
    rev: &str,
    path: &str,
    ignore_whitespace: bool,
    cancel: &Cancellable,
) -> Result<Vec<BlameLine>, GitError> {
    let mut args: Vec<&str> = vec!["blame", "--porcelain", "-M", "-C"];
    if ignore_whitespace {
        args.push("-w");
    }
    // git blame takes the path as a plain path: no globbing and no pathspec
    // magic (`:(literal)` is rejected), so the raw path is used
    args.extend([rev, "--", path]);
    let out = run_cancellable(repo, &args, cancel)?;
    parse_blame_porcelain(&String::from_utf8_lossy(&out))
}

/// Parse the output of `git blame --porcelain` (docs/specs/M08 §5). A group
/// starts with `<sha> <orig-line> <final-line> [<count>]` (count is omitted
/// when it is 1), optionally followed by a header block (`author`,
/// `author-time`, `summary`, …), then `<count>` content lines (tab-prefixed
/// in modern git; `<sha> <content>`-prefixed in older git). A commit's
/// header block is emitted at most once per blame run; later groups of the
/// same commit (adjacent or not) omit it and inherit the recorded metadata.
/// `boundary` marks a group whose origin is at or before the first commit of
/// the file. Malformed output is an error, not a skip.
pub fn parse_blame_porcelain(out: &str) -> Result<Vec<BlameLine>, GitError> {
    fn malformed(msg: String) -> GitError {
        GitError::Other {
            code: None,
            stderr: format!("blame: malformed output ({msg})"),
        }
    }
    /// A group header: `<sha> <orig-line> <final-line> [<count>]` — the
    /// count is omitted when it is 1.
    fn group(line: &str) -> Option<(String, usize, usize)> {
        let mut f = line.splitn(4, ' ');
        let sha = f.next()?.to_owned();
        if !(7..=64).contains(&sha.len()) || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let orig: usize = f.next()?.parse().ok()?;
        let fin: usize = f.next()?.parse().ok()?;
        let count: usize = f.next().and_then(|s| s.parse().ok()).unwrap_or(1);
        (orig > 0 && fin > 0 && count > 0).then_some((sha, fin, count))
    }
    /// The content of a line, if this line is one: tab-prefixed (modern git)
    /// or `<sha> <content>`-prefixed (older git).
    fn content(line: &str) -> Option<&str> {
        if let Some(rest) = line.strip_prefix('\t') {
            return Some(rest);
        }
        let bytes = line.as_bytes();
        for n in [64, 40] {
            if bytes.len() >= n
                && bytes[..n].iter().all(|b| b.is_ascii_hexdigit())
                && (bytes.len() == n || bytes[n] == b' ')
            {
                return Some(if bytes.len() == n { "" } else { &line[n + 1..] });
            }
        }
        None
    }
    /// One group as it accumulates: its content lines, and the header block
    /// it carried (absent when the commit repeats the previous group).
    #[derive(Default)]
    struct Group {
        oid: String,
        fin: usize,
        count: usize,
        rows: Vec<usize>,
        author: Option<String>,
        date: Option<i64>,
        summary: Option<String>,
    }
    /// A group is complete once its content lines are in; its metadata is
    /// its own header block, or — when git omitted it because this commit's
    /// header was already emitted earlier in the output (the header is
    /// emitted at most once per commit) — the metadata recorded for that
    /// commit.
    fn finish(
        g: Group,
        seen: &mut std::collections::HashMap<String, (String, i64, String)>,
    ) -> Result<Vec<BlameLine>, GitError> {
        let zero = g.oid.bytes().all(|b| b == b'0');
        let meta = if zero {
            (String::new(), 0, String::new())
        } else if g.author.is_some() || g.date.is_some() || g.summary.is_some() {
            // git emits the whole block at once; a partial one is malformed
            let (author, date, summary) = (
                g.author
                    .ok_or_else(|| malformed(format!("partial header for {}", g.oid)))?,
                g.date
                    .ok_or_else(|| malformed(format!("partial header for {}", g.oid)))?,
                g.summary
                    .ok_or_else(|| malformed(format!("partial header for {}", g.oid)))?,
            );
            let meta = (author, date, summary);
            seen.insert(g.oid.clone(), meta.clone());
            meta
        } else {
            seen.get(&g.oid)
                .cloned()
                .ok_or_else(|| malformed(format!("header omitted for unseen commit {}", g.oid)))?
        };
        let (author, date, summary) = meta;
        Ok(g.rows
            .into_iter()
            .map(|line| BlameLine {
                line,
                final_oid: g.oid.clone(),
                author: author.clone(),
                author_date: date,
                summary: summary.clone(),
            })
            .collect())
    }

    let mut lines: Vec<BlameLine> = Vec::new();
    let mut current: Option<Group> = None;
    let mut seen: std::collections::HashMap<String, (String, i64, String)> =
        std::collections::HashMap::new();
    for raw in out.lines() {
        if raw.is_empty() {
            continue;
        }
        // checked before content lines: a group header (`<sha> <orig> <final>
        // [<count>]`) would otherwise look like an older-git content line
        if let Some((oid, fin, count)) = group(raw) {
            if current.is_some() {
                return Err(malformed(
                    "group header before the previous group is complete".into(),
                ));
            }
            current = Some(Group {
                oid,
                fin,
                count,
                ..Default::default()
            });
            continue;
        }
        if let Some(_text) = content(raw) {
            let Some(g) = current.as_mut() else {
                return Err(malformed("content line before any group".into()));
            };
            if g.rows.len() >= g.count {
                return Err(malformed(format!(
                    "more content lines than group {} declares",
                    g.oid
                )));
            }
            g.rows.push(g.fin + g.rows.len());
            // the group is finished once its content lines are in
            if let Some(g) = current.take() {
                if g.rows.len() == g.count {
                    lines.extend(finish(g, &mut seen)?);
                } else {
                    current = Some(g);
                }
            }
            continue;
        }
        // a header-block line for the in-progress group
        let g = current
            .as_mut()
            .ok_or_else(|| malformed(format!("header line outside a group: {raw:?}")))?;
        if !g.rows.is_empty() {
            return Err(malformed(format!(
                "header line after the group's content lines: {raw:?}"
            )));
        }
        if raw == "boundary" {
            continue;
        }
        let (key, value) = raw
            .split_once(' ')
            .ok_or_else(|| malformed(format!("unrecognized header line: {raw:?}")))?;
        match key {
            "author" => g.author = Some(value.to_owned()),
            "author-time" => {
                g.date = Some(
                    value
                        .parse()
                        .map_err(|_| malformed(format!("bad author-time: {value:?}")))?,
                );
            }
            "summary" => g.summary = Some(value.to_owned()),
            // carried by the format, not used by the view
            "author-mail" | "author-tz" | "committer" | "committer-mail" | "committer-time"
            | "committer-tz" | "filename" | "previous" => {}
            _ => return Err(malformed(format!("unknown header key: {raw:?}"))),
        }
    }
    if let Some(g) = current {
        if g.rows.len() != g.count {
            return Err(malformed(format!(
                "group {} has {} of {} content lines",
                g.oid,
                g.rows.len(),
                g.count
            )));
        }
        lines.extend(finish(g, &mut seen)?);
    }
    Ok(lines)
}

/// The commit ids in `oids` that have no parent (first commits, M08 §5):
/// `git rev-list --no-walk --parents` prints `<oid> <parents…>` per commit,
/// so an id with no parent is exactly one id per line.
pub fn root_commits(repo: &Path, oids: &[&str]) -> Result<Vec<String>, GitError> {
    if oids.is_empty() {
        return Ok(Vec::new());
    }
    let args: Vec<&str> = std::iter::once("rev-list")
        .chain(["--no-walk", "--parents"])
        .chain(oids.iter().copied())
        .collect();
    let out = run(repo, &args)?;
    Ok(parse_root_commits(&String::from_utf8_lossy(&out), oids))
}

/// Split `git rev-list --no-walk --parents` output into the ids without a
/// parent. Lines with no parent (exactly one id) are roots; lines with
/// parents are not.
pub fn parse_root_commits(out: &str, oids: &[&str]) -> Vec<String> {
    out.lines()
        .filter(|line| line.split_whitespace().count() == 1)
        .map(str::to_owned)
        .filter(|oid| oids.iter().any(|o| *o == oid))
        .collect()
}

/// One changed path from `git status --porcelain=v2` (docs/specs/M07 §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    pub path: String,
    /// Previous path for renames/copies.
    pub orig_path: Option<String>,
    /// Index (staged) state: `.` unchanged, `M`, `A`, `D`, `R`, `C`, `T`; `?` untracked.
    pub index: char,
    /// Work-tree (unstaged) state, same letters.
    pub worktree: char,
    pub conflicted: bool,
}

impl StatusEntry {
    pub fn untracked(&self) -> bool {
        self.index == '?'
    }
    pub fn has_staged(&self) -> bool {
        !self.conflicted && !self.untracked() && self.index != '.'
    }
    pub fn has_unstaged(&self) -> bool {
        self.conflicted || self.untracked() || self.worktree != '.'
    }
}

pub fn status(repo: &Path) -> Result<Vec<StatusEntry>, GitError> {
    let out = run(
        repo,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--find-renames",
        ],
    )?;
    Ok(parse_status(&String::from_utf8_lossy(&out)))
}

/// Parse `git status --porcelain=v2 -z` output.
pub fn parse_status(out: &str) -> Vec<StatusEntry> {
    let mut entries = Vec::new();
    let mut records = out.split('\0');
    while let Some(record) = records.next() {
        let entry = |xy: &str, path: &str, orig: Option<&str>, conflicted| {
            let mut c = xy.chars();
            StatusEntry {
                path: path.to_owned(),
                orig_path: orig.map(str::to_owned),
                index: c.next().unwrap_or('.'),
                worktree: c.next().unwrap_or('.'),
                conflicted,
            }
        };
        match record.split_once(' ') {
            // ordinary: 1 XY sub mH mI mW hH hI path
            Some(("1", rest)) => {
                if let [xy, .., path] = rest.splitn(8, ' ').collect::<Vec<_>>()[..] {
                    entries.push(entry(xy, path, None, false));
                }
            }
            // rename/copy: 2 XY sub mH mI mW hH hI score path, then origPath as its own record
            Some(("2", rest)) => {
                if let [xy, .., path] = rest.splitn(9, ' ').collect::<Vec<_>>()[..] {
                    entries.push(entry(xy, path, records.next(), false));
                }
            }
            // unmerged: u XY sub m1 m2 m3 mW h1 h2 h3 path
            Some(("u", rest)) => {
                if let [xy, .., path] = rest.splitn(10, ' ').collect::<Vec<_>>()[..] {
                    entries.push(entry(xy, path, None, true));
                }
            }
            Some(("?", path)) => entries.push(entry("??", path, None, false)),
            _ => {} // `!` ignored, `#` headers, trailing empty record
        }
    }
    entries
}

/// `git add` the paths (also records deletions).
pub fn stage(repo: &Path, paths: &[&str]) -> Result<(), GitError> {
    with_pathspecs(repo, &["add", "--all"], paths)
}

/// Remove the paths from the index, keeping work-tree changes. Works before the first commit too.
pub fn unstage(repo: &Path, paths: &[&str]) -> Result<(), GitError> {
    if run(repo, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok() {
        with_pathspecs(repo, &["restore", "--staged"], paths)
    } else {
        with_pathspecs(repo, &["rm", "--cached", "-r", "--quiet"], paths)
    }
}

/// Pathspec that matches exactly `path`: a file named `*.rs` or `:x` is never a pattern.
/// (Per path, not `GIT_LITERAL_PATHSPECS`, which breaks `git stash --include-untracked`.)
fn literal(path: &str) -> String {
    format!(":(literal){path}")
}

/// Runs `git <args> --pathspec-from-file=-` with NUL-separated paths on stdin (no argv length limits).
fn with_pathspecs(repo: &Path, args: &[&str], paths: &[&str]) -> Result<(), GitError> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut input = Vec::new();
    for p in paths {
        input.extend_from_slice(literal(p).as_bytes());
        input.push(0);
    }
    let args = [args, &["--pathspec-from-file=-", "--pathspec-file-nul"]].concat();
    run_with_input(repo, &args, Some(&input)).map(drop)
}

/// Throws away unstaged changes (docs/specs/M07 §3): tracked files go back to their staged/committed
/// content; untracked files are moved to the system Trash, never deleted outright.
pub fn discard(repo: &Path, entries: &[StatusEntry]) -> Result<(), GitError> {
    let tracked: Vec<&str> = entries
        .iter()
        .filter(|e| !e.untracked() && !e.conflicted && e.worktree != '.')
        .map(|e| e.path.as_str())
        .collect();
    with_pathspecs(repo, &["restore", "--worktree"], &tracked)?;
    let untracked: Vec<PathBuf> = entries
        .iter()
        .filter(|e| e.untracked())
        .map(|e| repo.join(&e.path))
        .collect();
    if !untracked.is_empty() {
        trash::delete_all(&untracked).map_err(|e| GitError::Other {
            code: None,
            stderr: format!("Could not move untracked files to the Trash: {e}"),
        })?;
    }
    Ok(())
}

/// A multi-step operation git has paused, usually on conflicts (docs/specs/M11 §0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpState {
    None,
    Merging,
    Rebasing { step: usize, total: usize },
    CherryPicking,
    Reverting,
}

pub fn op_state(repo: &Path) -> Result<OpState, GitError> {
    let git_dir = PathBuf::from(
        String::from_utf8_lossy(&run(repo, &["rev-parse", "--absolute-git-dir"])?).trim_end(),
    );
    let num = |dir: &str, file: &str| {
        std::fs::read_to_string(git_dir.join(dir).join(file))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    };
    Ok(if git_dir.join("rebase-merge").is_dir() {
        OpState::Rebasing {
            step: num("rebase-merge", "msgnum"),
            total: num("rebase-merge", "end"),
        }
    } else if git_dir.join("rebase-apply").is_dir() {
        OpState::Rebasing {
            step: num("rebase-apply", "next"),
            total: num("rebase-apply", "last"),
        }
    } else if git_dir.join("MERGE_HEAD").is_file() {
        OpState::Merging
    } else if git_dir.join("CHERRY_PICK_HEAD").is_file() {
        OpState::CherryPicking
    } else if git_dir.join("REVERT_HEAD").is_file() {
        OpState::Reverting
    } else {
        OpState::None
    })
}

/// How an operation that can stop on conflicts ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    /// Paused with conflicts to resolve; then continue or abort.
    Conflicts,
}

/// Turns "git stopped because of conflicts" into `Outcome::Conflicts` instead of an error.
pub(crate) fn allow_conflicts(
    repo: &Path,
    result: Result<Vec<u8>, GitError>,
) -> Result<Outcome, GitError> {
    match result {
        Ok(_) => Ok(Outcome::Done),
        Err(e) => {
            let conflicted = status(repo).is_ok_and(|st| st.iter().any(|e| e.conflicted));
            if conflicted {
                Ok(Outcome::Conflicts)
            } else {
                Err(e)
            }
        }
    }
}

fn is_merge_commit(repo: &Path, commit: &str) -> bool {
    run(
        repo,
        &["rev-parse", "--verify", "--quiet", &format!("{commit}^2")],
    )
    .is_ok()
}

/// Merges `branch` into the current branch (M11 §1). Fast-forwards when possible.
pub fn merge(repo: &Path, branch: &str) -> Result<Outcome, GitError> {
    allow_conflicts(repo, run(repo, &["merge", "--no-edit", branch]))
}

/// Rebases the current branch onto `onto`, stashing and restoring local changes (M11 §2).
pub fn rebase(repo: &Path, onto: &str) -> Result<Outcome, GitError> {
    allow_conflicts(repo, run(repo, &["rebase", "--autostash", onto]))
}

/// Applies one commit on top of HEAD (M06). Merge commits are picked against their first parent.
pub fn cherry_pick(repo: &Path, commit: &str) -> Result<Outcome, GitError> {
    let mut args = vec!["cherry-pick"];
    if is_merge_commit(repo, commit) {
        args.extend(["-m", "1"]);
    }
    args.push(commit);
    allow_conflicts(repo, run(repo, &args))
}

/// Creates a commit undoing `commit` (M06). Merge commits are reverted against their first parent.
pub fn revert(repo: &Path, commit: &str) -> Result<Outcome, GitError> {
    let mut args = vec!["revert", "--no-edit"];
    if is_merge_commit(repo, commit) {
        args.extend(["-m", "1"]);
    }
    args.push(commit);
    allow_conflicts(repo, run(repo, &args))
}

fn op_command(state: OpState) -> Option<&'static str> {
    match state {
        OpState::None => None,
        OpState::Merging => Some("merge"),
        OpState::Rebasing { .. } => Some("rebase"),
        OpState::CherryPicking => Some("cherry-pick"),
        OpState::Reverting => Some("revert"),
    }
}

/// Continues the paused operation once conflicts are resolved and staged.
pub fn continue_op(repo: &Path) -> Result<Outcome, GitError> {
    let Some(cmd) = op_command(op_state(repo)?) else {
        return Ok(Outcome::Done);
    };
    allow_conflicts(repo, run(repo, &[cmd, "--continue"]))
}

/// Skips the current commit of a rebase / cherry-pick / revert.
pub fn skip_op(repo: &Path) -> Result<Outcome, GitError> {
    let Some(cmd) = op_command(op_state(repo)?) else {
        return Ok(Outcome::Done);
    };
    allow_conflicts(repo, run(repo, &[cmd, "--skip"]))
}

/// Abandons the paused operation and restores the state from before it started.
pub fn abort_op(repo: &Path) -> Result<(), GitError> {
    let Some(cmd) = op_command(op_state(repo)?) else {
        return Ok(());
    };
    run(repo, &[cmd, "--abort"]).map(drop)
}

/// Resolves a conflicted file by taking one side whole (M11 §4), then marks it resolved.
/// `ours` is the current branch's side (during a rebase: the branch being rebased onto).
pub fn resolve_with(repo: &Path, path: &str, ours: bool) -> Result<(), GitError> {
    let side = if ours { "--ours" } else { "--theirs" };
    let spec = &literal(path);
    match run(repo, &["checkout", side, "--", spec]) {
        Ok(_) => stage(repo, &[path]),
        // the chosen side deleted the file
        Err(_) => run(repo, &["rm", "--quiet", "--", spec]).map(drop),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stash {
    /// Position in `git stash list`, as in `stash@{index}`.
    pub index: usize,
    pub id: String,
    pub message: String,
    pub time: i64,
}

/// Saved stashes, newest first (docs/specs/M12 §1).
pub fn stashes(repo: &Path) -> Result<Vec<Stash>, GitError> {
    let out = run(repo, &["stash", "list", "--format=%H%x1f%gs%x1f%ct"])?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let mut f = line.split('\x1f');
            Some(Stash {
                index,
                id: f.next()?.to_owned(),
                message: f.next()?.to_owned(),
                time: f.next()?.parse().ok()?,
            })
        })
        .collect())
}

/// Stashes local changes (untracked files too when asked). Empty message = git's default.
pub fn stash_save(repo: &Path, message: &str, include_untracked: bool) -> Result<(), GitError> {
    let mut args = vec!["stash", "push"];
    if include_untracked {
        args.push("--include-untracked");
    }
    if !message.trim().is_empty() {
        args.extend(["--message", message.trim()]);
    }
    run(repo, &args).map(drop)
}

/// Applies a stash; `pop` also drops it, except when it conflicts (git keeps it then).
pub fn stash_apply(repo: &Path, index: usize, pop: bool) -> Result<Outcome, GitError> {
    let name = format!("stash@{{{index}}}");
    allow_conflicts(
        repo,
        run(repo, &["stash", if pop { "pop" } else { "apply" }, &name]),
    )
}

pub fn stash_drop(repo: &Path, index: usize) -> Result<(), GitError> {
    run(repo, &["stash", "drop", &format!("stash@{{{index}}}")]).map(drop)
}

/// Creates tag `name` at `target`; annotated when `message` isn't empty (docs/specs/M12 §2).
pub fn create_tag(repo: &Path, name: &str, target: &str, message: &str) -> Result<(), GitError> {
    if run(repo, &["check-ref-format", &format!("refs/tags/{name}")]).is_err() {
        return Err(GitError::Other {
            code: None,
            stderr: format!("`{name}` is not a valid tag name."),
        });
    }
    if message.trim().is_empty() {
        run(repo, &["tag", name, target]).map(drop)
    } else {
        run(
            repo,
            &[
                "tag",
                "--annotate",
                name,
                "--message",
                message.trim(),
                target,
            ],
        )
        .map(drop)
    }
}

pub fn delete_tag(repo: &Path, name: &str) -> Result<(), GitError> {
    run(repo, &["tag", "--delete", name]).map(drop)
}

/// Pushes one tag to `origin` (or the only remote). Returns the remote's name.
pub fn push_tag(repo: &Path, name: &str) -> Result<String, GitError> {
    let remotes = String::from_utf8_lossy(&run(repo, &["remote"])?).into_owned();
    let remotes: Vec<&str> = remotes.lines().collect();
    let remote = match remotes[..] {
        _ if remotes.contains(&"origin") => "origin",
        [only] => only,
        _ => {
            return Err(GitError::Other {
                code: None,
                stderr: "No `origin` remote to push the tag to.".into(),
            });
        }
    };
    run(repo, &["push", remote, &format!("refs/tags/{name}")])?;
    Ok(remote.to_owned())
}

/// What to do with one commit in an interactive rebase (docs/specs/M11 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoAction {
    Pick,
    /// Meld into the previous commit, keeping both messages.
    Squash,
    /// Meld into the previous commit, dropping this message.
    Fixup,
    Drop,
}

/// Commits after `base` up to HEAD, oldest first: the rows of the interactive rebase editor.
pub fn rebase_range(repo: &Path, base: &str) -> Result<Vec<Commit>, GitError> {
    let range = format!("{base}..HEAD");
    let out = run(
        repo,
        &[
            "log",
            "-z",
            "--reverse",
            "--topo-order",
            LOG_FORMAT,
            &range,
            "--",
        ],
    )?;
    let commits = parse_log(&String::from_utf8_lossy(&out));
    if commits.iter().any(|c| c.parents.len() > 1) {
        return Err(GitError::Other {
            code: None,
            stderr: "This range contains merge commits; interactive rebase would flatten them."
                .into(),
        });
    }
    Ok(commits)
}

/// Rewrites the commits after `base` following `plan` (in the new order). Git does the work:
/// the plan is written as git's todo list and copied in by `GIT_SEQUENCE_EDITOR`.
pub fn interactive_rebase(
    repo: &Path,
    base: &str,
    plan: &[(String, TodoAction)],
) -> Result<Outcome, GitError> {
    let fail = |msg: &str| GitError::Other {
        code: None,
        stderr: msg.to_owned(),
    };
    match plan.iter().find(|(_, a)| *a != TodoAction::Drop) {
        None => return Err(fail("Every commit would be dropped; use Reset instead.")),
        Some((_, TodoAction::Squash | TodoAction::Fixup)) => {
            return Err(fail(
                "The first kept commit can't be squashed or fixed up: there is nothing before it.",
            ));
        }
        _ => {}
    }
    let todo: String = plan
        .iter()
        .map(|(id, action)| {
            let verb = match action {
                TodoAction::Pick => "pick",
                TodoAction::Squash => "squash",
                TodoAction::Fixup => "fixup",
                TodoAction::Drop => "drop",
            };
            format!("{verb} {id}\n")
        })
        .collect();
    let file = std::env::temp_dir().join(format!("spork-rebase-todo-{}", askpass::random_token()));
    std::fs::write(&file, todo).map_err(|e| fail(&e.to_string()))?;
    // the editor command runs in git's shell with the todo path appended: `cp <ours> <git's>`
    let editor = format!("cp '{}'", file.display().to_string().replace('\'', "'\\''"));
    let result = run_with_env(
        repo,
        &["rebase", "--interactive", "--autostash", base],
        None,
        &[("GIT_SEQUENCE_EDITOR", &editor)],
        None,
    );
    std::fs::remove_file(&file).ok();
    allow_conflicts(repo, result)
}

/// Default folder name for a clone: `https://host/org/spork.git` → `spork`, `git@host:org/x` → `x`.
pub fn repo_name_from_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let last = trimmed.rsplit(['/', ':', '\\']).next().unwrap_or(trimmed);
    let name = last.strip_suffix(".git").unwrap_or(last);
    if name.is_empty() {
        "repository".into()
    } else {
        name.into()
    }
}

/// Clones `url` into `parent/name` with submodules (docs/specs/M02 §1). Refuses to touch an
/// existing non-empty folder. Returns the new repository's path.
pub fn clone(url: &str, parent: &Path, name: &str) -> Result<PathBuf, GitError> {
    let dest = parent.join(name);
    let busy = std::fs::read_dir(&dest).is_ok_and(|mut entries| entries.next().is_some());
    if busy {
        return Err(GitError::Other {
            code: None,
            stderr: format!("{} already exists and isn't empty.", dest.display()),
        });
    }
    run(
        parent,
        &["clone", "--recurse-submodules", "--", url.trim(), name],
    )?;
    Ok(dest)
}

/// Creates a new repository in `path` (made if missing). Uses `init.defaultBranch`, else `main`.
pub fn init(path: &Path) -> Result<(), GitError> {
    std::fs::create_dir_all(path).map_err(|e| GitError::Other {
        code: None,
        stderr: e.to_string(),
    })?;
    let has_default = run(path, &["config", "--get", "init.defaultBranch"]).is_ok();
    let args: &[&str] = if has_default {
        &["init", "--quiet"]
    } else {
        &["init", "--quiet", "--initial-branch=main"]
    };
    run(path, args).map(drop)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// Checked-out branch (short name); `None` when detached.
    pub branch: Option<String>,
    /// The repository's main working tree (can't be removed).
    pub is_main: bool,
}

/// All working trees of the repository, main one first (docs/specs/M12 §4).
pub fn worktrees(repo: &Path) -> Result<Vec<Worktree>, GitError> {
    let out = run(repo, &["worktree", "list", "--porcelain", "-z"])?;
    let mut list: Vec<Worktree> = Vec::new();
    for field in String::from_utf8_lossy(&out).split('\0') {
        if let Some(path) = field.strip_prefix("worktree ") {
            list.push(Worktree {
                path: PathBuf::from(path),
                branch: None,
                is_main: list.is_empty(),
            });
        } else if let (Some(branch), Some(w)) = (field.strip_prefix("branch "), list.last_mut()) {
            w.branch = Some(branch.trim_start_matches("refs/heads/").to_owned());
        }
    }
    Ok(list)
}

/// Checks out an existing local `branch` in a new working tree at `path`.
pub fn add_worktree(repo: &Path, path: &Path, branch: &str) -> Result<(), GitError> {
    run(repo, &["worktree", "add", &path.to_string_lossy(), branch]).map(drop)
}

/// Removes a working tree; git refuses if it has uncommitted changes.
pub fn remove_worktree(repo: &Path, path: &Path) -> Result<(), GitError> {
    run(repo, &["worktree", "remove", &path.to_string_lossy()]).map(drop)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submodule {
    pub path: String,
    pub commit: String,
    /// `' '` up to date, `'-'` not initialized, `'+'` different commit checked out, `'U'` conflict.
    pub state: char,
}

/// Submodules, recursively (M12 §3).
pub fn submodules(repo: &Path) -> Result<Vec<Submodule>, GitError> {
    let out = run(repo, &["submodule", "status", "--recursive"])?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|line| {
            let state = line.chars().next()?;
            let mut f = line[state.len_utf8()..].split(' ');
            let commit = f.next()?.to_owned();
            let path = f.next()?.to_owned();
            Some(Submodule {
                path,
                commit,
                state,
            })
        })
        .collect())
}

/// Initializes and checks out every submodule at the commit the repository records.
pub fn update_submodules(repo: &Path) -> Result<(), GitError> {
    run(repo, &["submodule", "update", "--init", "--recursive"]).map(drop)
}

/// Where a partial patch from `spork-patch` is applied (docs/specs/M07 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchTarget {
    /// Forward patch into the index.
    Stage,
    /// Reverse patch out of the index.
    Unstage,
    /// Reverse patch out of the work tree (throws the selected changes away).
    Discard,
}

/// Applies a patch to the index or work tree. Git rejects patches that don't fit, so a failure
/// changes nothing.
pub fn apply_patch(repo: &Path, patch: &str, target: PatchTarget) -> Result<(), GitError> {
    let mut args = vec!["apply", "--recount", "--whitespace=nowarn"];
    match target {
        PatchTarget::Stage => args.push("--cached"),
        PatchTarget::Unstage => args.extend(["--cached", "--reverse"]),
        PatchTarget::Discard => args.push("--reverse"),
    }
    args.push("-");
    run_with_input(repo, &args, Some(patch.as_bytes())).map(drop)
}

/// Commits the index with `message` (hooks run as usual). `amend` replaces HEAD.
pub fn commit(repo: &Path, message: &str, amend: bool) -> Result<(), GitError> {
    let mut args = vec!["commit", "--cleanup=strip", "-F", "-"];
    if amend {
        args.push("--amend");
    }
    run_with_input(repo, &args, Some(message.as_bytes())).map(drop)
}

/// Full message of HEAD (prefill for amend), or `None` before the first commit.
pub fn head_message(repo: &Path) -> Option<String> {
    let out = run(repo, &["log", "-1", "--format=%B", "HEAD", "--"]).ok()?;
    Some(String::from_utf8_lossy(&out).trim_end().to_owned())
}

/// Patch for one changed file: staged (HEAD→index) or unstaged (index→work tree; untracked shown as added).
pub fn file_diff(repo: &Path, entry: &StatusEntry, staged: bool) -> Result<String, GitError> {
    let base = ["diff", "--no-color", "--no-ext-diff", "--find-renames"];
    let out = if staged {
        let mut paths = vec![literal(&entry.path)];
        paths.extend(entry.orig_path.as_deref().map(literal));
        let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
        run(repo, &[&base[..], &["--cached", "--"], &paths].concat())?
    } else if entry.untracked() {
        run_allowing_diff(
            repo,
            &[&base[..], &["--no-index", "--", NULL_DEVICE, &entry.path]].concat(),
        )?
    } else {
        run(repo, &[&base[..], &["--", &literal(&entry.path)]].concat())?
    };
    Ok(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(windows)]
const NULL_DEVICE: &str = "NUL";
#[cfg(not(windows))]
const NULL_DEVICE: &str = "/dev/null";

/// For `diff --no-index`, exit code 1 means "files differ" and stdout holds the patch.
fn run_allowing_diff(repo: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let out = Command::new(git_binary())
        .args(["-c", "core.quotepath=false", "-c", "color.ui=false"])
        .args(args)
        .current_dir(repo)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| GitError::GitMissing(e.to_string()))?;
    match out.status.code() {
        Some(0 | 1) => Ok(out.stdout),
        code => Err(classify(code, &String::from_utf8_lossy(&out.stderr))),
    }
}

/// Fetches every remote, pruning remote branches deleted upstream (docs/specs/M09 §1).
pub fn fetch(repo: &Path) -> Result<(), GitError> {
    run(repo, &["fetch", "--all", "--prune"]).map(drop)
}

/// Pulls the current branch's upstream; merge vs rebase follows the user's git config. Never opens an editor.
pub fn pull(repo: &Path) -> Result<(), GitError> {
    run(repo, &["pull", "--no-edit"]).map(drop)
}

/// Pushes the current branch. The first push sets the upstream on `origin` (or the only remote).
/// Returns a short summary for the notification.
pub fn push(repo: &Path) -> Result<String, GitError> {
    let fail = |msg: &str| GitError::Other {
        code: None,
        stderr: msg.to_owned(),
    };
    let branch = head_branch(repo)?
        .ok_or_else(|| fail("HEAD is detached: check out a branch to push it."))?;
    if run(
        repo,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )
    .is_ok()
    {
        run(repo, &["push"])?;
        return Ok(format!("Pushed {branch}"));
    }
    let remotes = String::from_utf8_lossy(&run(repo, &["remote"])?).into_owned();
    let remotes: Vec<&str> = remotes.lines().collect();
    let remote = match remotes[..] {
        _ if remotes.contains(&"origin") => "origin",
        [only] => only,
        [] => return Err(fail("This repository has no remote to push to.")),
        _ => {
            return Err(fail(
                "Several remotes and none is `origin`: set an upstream for this branch first.",
            ));
        }
    };
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    run(repo, &["push", "--set-upstream", remote, &refspec])?;
    Ok(format!("Pushed {branch} to {remote}"))
}

/// Switches to a local branch (docs/specs/M04 §2). Git refuses if local changes would be overwritten.
pub fn checkout(repo: &Path, branch: &str) -> Result<(), GitError> {
    run(repo, &["switch", "--no-guess", branch]).map(drop)
}

/// Checks out a remote branch such as `origin/feature/x`: switches to the local branch of the same
/// name if it exists, otherwise creates it tracking the remote one (M04 §3). Returns the local name.
pub fn checkout_remote(repo: &Path, remote_branch: &str) -> Result<String, GitError> {
    let local = remote_branch
        .split_once('/')
        .map_or(remote_branch, |(_, b)| b)
        .to_owned();
    let exists = run(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{local}"),
        ],
    )
    .is_ok();
    if exists {
        checkout(repo, &local)?;
    } else {
        run(repo, &["switch", "--track", remote_branch])?;
    }
    Ok(local)
}

/// Creates `name` at `start` (default HEAD), optionally switching to it (M04 §1).
pub fn create_branch(
    repo: &Path,
    name: &str,
    start: Option<&str>,
    switch: bool,
) -> Result<(), GitError> {
    if run(repo, &["check-ref-format", "--branch", name]).is_err() {
        return Err(GitError::Other {
            code: None,
            stderr: format!("`{name}` is not a valid branch name."),
        });
    }
    let mut args = if switch {
        vec!["switch", "-c", name]
    } else {
        vec!["branch", name]
    };
    args.extend(start);
    run(repo, &args).map(drop)
}

/// Deletes a local branch; git refuses if it isn't merged anywhere (no data loss).
pub fn delete_branch(repo: &Path, name: &str) -> Result<(), GitError> {
    run(repo, &["branch", "--delete", name]).map(drop)
}

/// Password / passphrase prompts from git and ssh, answered by Spork's UI (docs/specs/M10).
///
/// Git runs the Spork binary itself as `GIT_ASKPASS` / `SSH_ASKPASS` (with `SPORK_ASKPASS=1`).
/// That helper sends the prompt to the running app over a localhost connection guarded by a
/// random per-launch token, and prints the answer for git. Standard library only, same on
/// every OS.
pub mod askpass {
    use std::io::{self, BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::OnceLock;
    use std::time::Duration;

    /// Marks a process started by git as the askpass helper.
    pub const HELPER_ENV: &str = "SPORK_ASKPASS";
    const ADDR_ENV: &str = "SPORK_IPC";
    const TOKEN_ENV: &str = "SPORK_IPC_TOKEN";

    #[derive(Debug, Clone)]
    pub struct Config {
        /// Program git runs for prompts (the Spork executable).
        pub program: PathBuf,
        pub addr: String,
        pub token: String,
    }

    static CONFIG: OnceLock<Config> = OnceLock::new();

    /// Routes prompts of every git command started after this call to the app.
    pub fn install(config: Config) {
        CONFIG.set(config).ok();
    }

    pub(crate) fn configure(cmd: &mut Command) {
        let Some(c) = CONFIG.get() else { return };
        cmd.env("GIT_ASKPASS", &c.program)
            .env("SSH_ASKPASS", &c.program)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env(HELPER_ENV, "1")
            .env(ADDR_ENV, &c.addr)
            .env(TOKEN_ENV, &c.token);
        // older OpenSSH only uses SSH_ASKPASS when DISPLAY is set
        #[cfg(target_os = "linux")]
        if std::env::var_os("DISPLAY").is_none() {
            cmd.env("DISPLAY", ":0");
        }
    }

    /// 128 random bits as hex, from the OS-seeded hasher keys (no extra dependency).
    pub fn random_token() -> String {
        use std::hash::{BuildHasher, Hasher};
        (0..2u64)
            .map(|i| {
                let mut h = std::collections::hash_map::RandomState::new().build_hasher();
                h.write_u64(i);
                format!("{:016x}", h.finish())
            })
            .collect()
    }

    fn escape(s: &str) -> String {
        s.replace('\\', "\\\\")
            .replace('\n', "\\n")
            .replace('\t', "\\t")
    }

    fn unescape(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            match (c, c == '\\') {
                (_, true) => match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(other) => out.push(other),
                    None => {}
                },
                (c, false) => out.push(c),
            }
        }
        out
    }

    /// App side: answers prompts arriving on `listener`; `ask` returns the answer, `None` cancels.
    /// Connections without the right token are dropped.
    pub fn serve(listener: TcpListener, token: &str, ask: impl Fn(String) -> Option<String>) {
        for stream in listener.incoming().flatten() {
            handle(stream, token, &ask).ok();
        }
    }

    fn handle(
        stream: TcpStream,
        token: &str,
        ask: &impl Fn(String) -> Option<String>,
    ) -> io::Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line)?;
        let Some((sent, prompt)) = line.trim_end_matches('\n').split_once('\t') else {
            return Ok(());
        };
        if sent != token {
            return Ok(());
        }
        let reply = match ask(unescape(prompt)) {
            Some(answer) => format!("OK\t{}\n", escape(&answer)),
            None => "CANCEL\n".to_owned(),
        };
        (&stream).write_all(reply.as_bytes())
    }

    /// Helper side (run by git): asks the app. `None` when cancelled or the app is unreachable.
    pub fn request(addr: &str, token: &str, prompt: &str) -> Option<String> {
        let mut stream = TcpStream::connect(addr).ok()?;
        stream
            .write_all(format!("{token}\t{}\n", escape(prompt)).as_bytes())
            .ok()?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).ok()?;
        line.trim_end_matches('\n')
            .strip_prefix("OK\t")
            .map(unescape)
    }

    /// Entry point when git runs Spork as askpass: prints the answer, exit code 1 on cancel.
    pub fn helper_main() -> i32 {
        let prompt = std::env::args().nth(1).unwrap_or_default();
        let (Ok(addr), Ok(token)) = (std::env::var(ADDR_ENV), std::env::var(TOKEN_ENV)) else {
            return 1;
        };
        match request(&addr, &token, &prompt) {
            Some(answer) => {
                println!("{answer}");
                0
            }
            None => 1,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn prompt_round_trip_cancel_and_wrong_token() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap().to_string();
            let token = random_token();
            assert_eq!(token.len(), 32);
            let server_token = token.clone();
            std::thread::spawn(move || {
                serve(listener, &server_token, |prompt| match prompt.as_str() {
                    "Password for 'https://u@host':" => Some("s3cr\tet\\x".into()),
                    _ => None,
                })
            });
            assert_eq!(
                request(&addr, &token, "Password for 'https://u@host':").as_deref(),
                Some("s3cr\tet\\x")
            );
            assert_eq!(request(&addr, &token, "Username for 'https://host':"), None); // cancelled
            assert_eq!(
                request(&addr, "wrong-token", "Password for 'https://u@host':"),
                None
            );
            assert_ne!(random_token(), token);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn git(dir: &Path, args: &[&str]) {
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

    /// Same as `git`, but with a distinct author/committer name and date so
    /// tests can attribute lines to specific identities.
    fn git_as(dir: &Path, name: &str, date: &str, args: &[&str]) {
        let email = format!("{name}@spork.invalid");
        let ok = Command::new("git")
            .args(["-c", "commit.gpgsign=false"])
            .env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", &email)
            .env("GIT_COMMITTER_NAME", name)
            .env("GIT_COMMITTER_EMAIL", &email)
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?} failed");
    }

    /// The oid of `HEAD` in `dir`.
    fn head(dir: &Path) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git rev-parse HEAD failed");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
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
    fn working_copy_status_stage_unstage_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let write = |p: &str, c: &str| std::fs::write(d.join(p), c).unwrap();
        git(d, &["init", "-q", "-b", "main"]);
        git(d, &["config", "user.name", "Spork Test"]);
        git(d, &["config", "user.email", "test@spork.invalid"]);
        git(d, &["config", "commit.gpgsign", "false"]);

        // before the first commit: stage + unstage must work on an unborn HEAD
        write("a.txt", "one\n");
        stage(d, &["a.txt"]).unwrap();
        assert!(status(d).unwrap()[0].has_staged());
        unstage(d, &["a.txt"]).unwrap();
        assert!(status(d).unwrap()[0].untracked());
        stage(d, &["a.txt"]).unwrap();
        assert_eq!(head_message(d), None);
        commit(d, "first\n\nbody line", false).unwrap();
        assert_eq!(head_message(d).as_deref(), Some("first\n\nbody line"));

        write("a.txt", "one\ntwo\n"); // modified, unstaged
        write("star*.txt", "x\n"); // untracked, name with a glob char
        write("gone.txt", "bye\n");
        stage(d, &["gone.txt"]).unwrap();
        commit(d, "add gone", false).unwrap();
        std::fs::remove_file(d.join("gone.txt")).unwrap(); // deleted
        git(d, &["mv", "a.txt", "b.txt"]); // staged rename (with an unstaged edit inside)

        let st = status(d).unwrap();
        let find = |p: &str| {
            st.iter()
                .find(|e| e.path == p)
                .unwrap_or_else(|| panic!("{p} in {st:?}"))
        };
        assert!(find("star*.txt").untracked());
        assert_eq!(
            (find("gone.txt").index, find("gone.txt").worktree),
            ('.', 'D')
        );
        let renamed = find("b.txt");
        assert_eq!(
            (renamed.index, renamed.orig_path.as_deref()),
            ('R', Some("a.txt"))
        );

        // literal pathspec: staging `star*.txt` must not touch other files
        stage(d, &["star*.txt", "gone.txt"]).unwrap();
        let st = status(d).unwrap();
        assert!(st.iter().any(|e| e.path == "star*.txt" && e.index == 'A'));
        assert!(st.iter().any(|e| e.path == "gone.txt" && e.index == 'D'));
        unstage(d, &["star*.txt"]).unwrap();
        let st = status(d).unwrap();
        assert!(st.iter().any(|e| e.path == "star*.txt" && e.untracked()));

        // per-file diffs: untracked shows as added, staged rename shows as rename
        let star = st.iter().find(|e| e.path == "star*.txt").unwrap();
        assert!(file_diff(d, star, false).unwrap().contains("+x"));
        let renamed = st.iter().find(|e| e.path == "b.txt").unwrap();
        assert!(
            file_diff(d, renamed, true)
                .unwrap()
                .contains("rename to b.txt")
        );
        assert!(file_diff(d, renamed, false).unwrap().contains("+two"));

        // discard restores a tracked file to its index content (untracked → Trash isn't run in tests)
        write("gone.txt", "edited\n");
        git(d, &["add", "gone.txt"]);
        write("gone.txt", "edited again\n");
        let entry = status(d)
            .unwrap()
            .into_iter()
            .find(|e| e.path == "gone.txt")
            .unwrap();
        discard(d, &[entry]).unwrap();
        assert_eq!(
            std::fs::read_to_string(d.join("gone.txt")).unwrap(),
            "edited\n"
        );
        unstage(d, &["gone.txt"]).unwrap();
        let entry = status(d)
            .unwrap()
            .into_iter()
            .find(|e| e.path == "gone.txt")
            .unwrap();
        discard(d, &[entry]).unwrap();
        // unstaged → the index holds HEAD's version again, so discard restores that
        assert_eq!(
            std::fs::read_to_string(d.join("gone.txt")).unwrap(),
            "bye\n"
        );

        // amend rewrites HEAD's message without adding a commit
        commit(d, "add gone (amended)", true).unwrap();
        assert_eq!(head_message(d).as_deref(), Some("add gone (amended)"));
        assert_eq!(log(d, 10).unwrap().len(), 2);
    }

    #[test]
    fn fetch_pull_push_against_a_bare_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, a, b) = (
            tmp.path().join("server.git"),
            tmp.path().join("a"),
            tmp.path().join("b"),
        );
        git(
            tmp.path(),
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                server.to_str().unwrap(),
            ],
        );
        git(
            tmp.path(),
            &["init", "-q", "-b", "main", a.to_str().unwrap()],
        );
        git(&a, &["commit", "-q", "--allow-empty", "-m", "one"]);

        // no remote yet: a clear error instead of a git usage message
        assert!(push(&a).unwrap_err().to_string().contains("no remote"));
        git(&a, &["remote", "add", "origin", server.to_str().unwrap()]);
        // first push sets the upstream
        assert_eq!(push(&a).unwrap(), "Pushed main to origin");
        assert_eq!(
            refs(&a)
                .unwrap()
                .iter()
                .find(|r| r.name == "main")
                .unwrap()
                .upstream
                .as_deref(),
            Some("origin/main")
        );

        // someone else pushes a commit
        git(
            tmp.path(),
            &["clone", "-q", server.to_str().unwrap(), b.to_str().unwrap()],
        );
        git(&b, &["commit", "-q", "--allow-empty", "-m", "two"]);
        assert_eq!(push(&b).unwrap(), "Pushed main");

        // fetch shows we're behind, pull fast-forwards
        fetch(&a).unwrap();
        assert_eq!(
            refs(&a)
                .unwrap()
                .iter()
                .find(|r| r.name == "main")
                .unwrap()
                .behind,
            1
        );
        pull(&a).unwrap();
        assert_eq!(head_message(&a).as_deref(), Some("two"));

        // diverged: push is rejected with the friendly error
        git(&b, &["commit", "-q", "--allow-empty", "-m", "three"]);
        push(&b).unwrap();
        git(&a, &["commit", "-q", "--allow-empty", "-m", "local"]);
        assert_eq!(push(&a).unwrap_err(), GitError::NonFastForward);
    }

    #[test]
    fn branch_checkout_create_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git(d, &["init", "-q", "-b", "main"]);
        git(d, &["commit", "-q", "--allow-empty", "-m", "one"]);

        create_branch(d, "feature/x", None, false).unwrap();
        assert_eq!(head_branch(d).unwrap().as_deref(), Some("main"));
        create_branch(d, "feature/y", Some("main"), true).unwrap();
        assert_eq!(head_branch(d).unwrap().as_deref(), Some("feature/y"));
        assert!(
            create_branch(d, "bad..name", None, false)
                .unwrap_err()
                .to_string()
                .contains("not a valid")
        );

        checkout(d, "feature/x").unwrap();
        assert_eq!(head_branch(d).unwrap().as_deref(), Some("feature/x"));

        // unmerged branch: git refuses to delete it
        checkout(d, "feature/y").unwrap();
        git(d, &["commit", "-q", "--allow-empty", "-m", "only on y"]);
        checkout(d, "main").unwrap();
        assert!(delete_branch(d, "feature/y").is_err());
        delete_branch(d, "feature/x").unwrap();
        assert!(!refs(d).unwrap().iter().any(|r| r.name == "feature/x"));

        // remote branch: creates a tracking branch, then reuses it
        git(
            d,
            &["remote", "add", "origin", "https://example.invalid/r.git"],
        );
        git(d, &["update-ref", "refs/remotes/origin/feature/z", "HEAD"]);
        assert_eq!(checkout_remote(d, "origin/feature/z").unwrap(), "feature/z");
        let z = refs(d)
            .unwrap()
            .into_iter()
            .find(|r| r.name == "feature/z")
            .unwrap();
        assert_eq!(
            (z.is_head, z.upstream.as_deref()),
            (true, Some("origin/feature/z"))
        );
        checkout(d, "main").unwrap();
        assert_eq!(checkout_remote(d, "origin/feature/z").unwrap(), "feature/z");
    }

    /// Randomised check of line-level stage / unstage / discard against real git: every
    /// result must equal the file computed independently from a full-context diff.
    #[test]
    fn partial_patches_apply_exactly() {
        use spork_patch::{FilePatch, Kind};

        let mut seed: u64 = 0x5eed;
        let mut rand = move |n: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        // selection is a function of the line text, so it is identical in -U3 and full diffs
        let pick = |salt: u64, raw: &str| {
            let h = raw
                .bytes()
                .fold(salt, |h, b| h.wrapping_mul(31).wrapping_add(b as u64));
            h % 3 != 0
        };
        // expected file content after keeping `keep(kind, picked)` lines of a full-context diff
        let expected = |full: &str, salt: u64, keep: &dyn Fn(Kind, bool) -> bool| -> String {
            let patch = FilePatch::parse(full).unwrap();
            let lines: Vec<_> = patch.lines().collect();
            let mut out = String::new();
            for (i, l) in lines.iter().enumerate() {
                if l.kind == Kind::NoNewline || !keep(l.kind, pick(salt, &l.raw)) {
                    continue;
                }
                let text = &l.raw[1..];
                // a following "\ No newline" marker means this file line has no line ending
                let no_eol = lines.get(i + 1).is_some_and(|n| n.kind == Kind::NoNewline);
                out.push_str(if no_eol {
                    text.trim_end_matches('\n')
                } else {
                    text
                });
            }
            out
        };
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git(d, &["init", "-q", "-b", "main"]);
        let read_index =
            |d: &Path| String::from_utf8(run(d, &["show", ":f.txt"]).unwrap()).unwrap();
        let unstaged =
            |d: &Path| String::from_utf8(run(d, &["diff", "--", "f.txt"]).unwrap()).unwrap();
        let staged = |d: &Path| {
            String::from_utf8(run(d, &["diff", "--cached", "--", "f.txt"]).unwrap()).unwrap()
        };
        let full = |d: &Path, cached: bool| {
            let args: &[&str] = if cached {
                &["diff", "--cached", "-U100000", "--", "f.txt"]
            } else {
                &["diff", "-U100000", "--", "f.txt"]
            };
            String::from_utf8(run(d, args).unwrap()).unwrap()
        };

        let mut applied = 0;
        for round in 0..150u64 {
            // random base and random edit of it
            let lines = 5 + rand(40) as usize;
            let mut base: String = (0..lines).map(|i| format!("line {i} r{round}\n")).collect();
            let mut new = String::new();
            for (i, l) in base.lines().enumerate() {
                match rand(10) {
                    0 => {}
                    1 => new.push_str(&format!("line {i} changed r{round}\n")),
                    2 => new.push_str(&format!("{l}\ninserted {i} r{round}\n")),
                    _ => new.push_str(&format!("{l}\n")),
                }
            }
            if rand(4) == 0 {
                new.push_str(&format!("tail r{round}"));
            } // no trailing newline
            if rand(6) == 0 {
                base.pop();
            } // base without trailing newline
            if new == base {
                continue;
            }
            std::fs::write(d.join("f.txt"), &base).unwrap();
            git(d, &["add", "f.txt"]);
            git(d, &["commit", "-q", "--allow-empty", "-m", "base"]);
            std::fs::write(d.join("f.txt"), &new).unwrap();
            let edge_case = !base.ends_with('\n') || !new.ends_with('\n');
            let salt = round;

            // 1. stage a selection: index = base + picked changes
            let patch = FilePatch::parse(&unstaged(d)).unwrap();
            let want = expected(&full(d, false), salt, &|k, p| match k {
                Kind::Removed => !p,
                Kind::Added => p,
                _ => true,
            });
            if let Some(partial) = patch.partial(|_, l| pick(salt, &l.raw), false) {
                let before = read_index(d);
                match apply_patch(d, &partial, PatchTarget::Stage) {
                    Ok(()) => {
                        assert_eq!(read_index(d), want, "stage, round {round}\n{partial}");
                        applied += 1;
                    }
                    Err(e) => {
                        assert!(edge_case, "stage rejected, round {round}: {e}\n{partial}");
                        assert_eq!(read_index(d), before, "rejected patch changed the index");
                    }
                }
            }

            // 2. stage everything, then unstage a selection: index = new minus picked changes
            stage(d, &["f.txt"]).unwrap();
            let patch = FilePatch::parse(&staged(d)).unwrap();
            let want = expected(&full(d, true), salt, &|k, p| match k {
                Kind::Removed => p,
                Kind::Added => !p,
                _ => true,
            });
            if let Some(partial) = patch.partial(|_, l| pick(salt, &l.raw), true) {
                let before = read_index(d);
                match apply_patch(d, &partial, PatchTarget::Unstage) {
                    Ok(()) => assert_eq!(read_index(d), want, "unstage, round {round}\n{partial}"),
                    Err(e) => {
                        assert!(edge_case, "unstage rejected, round {round}: {e}\n{partial}");
                        assert_eq!(read_index(d), before);
                    }
                }
            }

            // 3. reset the index to base, then discard a selection from the work tree
            unstage(d, &["f.txt"]).unwrap();
            let patch = FilePatch::parse(&unstaged(d)).unwrap();
            let want = expected(&full(d, false), salt, &|k, p| match k {
                Kind::Removed => p,
                Kind::Added => !p,
                _ => true,
            });
            if let Some(partial) = patch.partial(|_, l| pick(salt, &l.raw), true) {
                match apply_patch(d, &partial, PatchTarget::Discard) {
                    Ok(()) => assert_eq!(
                        std::fs::read_to_string(d.join("f.txt")).unwrap(),
                        want,
                        "discard, round {round}"
                    ),
                    Err(e) => {
                        assert!(edge_case, "discard rejected, round {round}: {e}\n{partial}");
                        assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), new);
                    }
                }
            }
        }
        assert!(applied > 100, "too few rounds exercised: {applied}");
    }

    #[test]
    fn merge_rebase_pick_revert_with_conflicts() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let write = |c: &str| std::fs::write(d.join("f.txt"), c).unwrap();
        let read = || std::fs::read_to_string(d.join("f.txt")).unwrap();
        git(d, &["init", "-q", "-b", "main"]);
        write("base\n");
        git(d, &["add", "f.txt"]);
        git(d, &["commit", "-q", "-m", "base"]);
        git(d, &["switch", "-q", "-c", "feature"]);
        write("feature\n");
        git(d, &["commit", "-q", "-am", "feature edit"]);
        git(d, &["switch", "-q", "main"]);
        write("main\n");
        git(d, &["commit", "-q", "-am", "main edit"]);
        assert_eq!(op_state(d).unwrap(), OpState::None);

        // merge → conflict → abort restores main
        assert_eq!(merge(d, "feature").unwrap(), Outcome::Conflicts);
        assert_eq!(op_state(d).unwrap(), OpState::Merging);
        abort_op(d).unwrap();
        assert_eq!(
            (op_state(d).unwrap(), read()),
            (OpState::None, "main\n".into())
        );

        // merge → conflict → take theirs → continue creates the merge commit
        merge(d, "feature").unwrap();
        resolve_with(d, "f.txt", false).unwrap();
        assert!(!status(d).unwrap().iter().any(|e| e.conflicted));
        assert_eq!(continue_op(d).unwrap(), Outcome::Done);
        assert_eq!(
            (op_state(d).unwrap(), read()),
            (OpState::None, "feature\n".into())
        );
        assert_eq!(log(d, 1).unwrap()[0].parents.len(), 2);

        // rebase with conflicts reports progress; resolve with ours and continue
        git(d, &["switch", "-q", "-c", "topic", "HEAD~1"]); // main before the merge
        write("topic\n");
        git(d, &["commit", "-q", "-am", "topic edit"]);
        assert_eq!(rebase(d, "feature").unwrap(), Outcome::Conflicts);
        assert_eq!(
            op_state(d).unwrap(),
            OpState::Rebasing { step: 1, total: 2 }
        );
        resolve_with(d, "f.txt", true).unwrap(); // during a rebase "ours" = feature
        assert_eq!(continue_op(d).unwrap(), Outcome::Conflicts); // next commit conflicts too
        assert_eq!(skip_op(d).unwrap(), Outcome::Done);
        assert_eq!(op_state(d).unwrap(), OpState::None);

        // cherry-pick cleanly, then revert it
        git(d, &["switch", "-q", "main"]);
        std::fs::write(d.join("g.txt"), "g\n").unwrap();
        git(d, &["add", "g.txt"]);
        git(d, &["commit", "-q", "-m", "add g"]);
        let g = log(d, 1).unwrap()[0].id.clone();
        git(d, &["switch", "-q", "topic"]);
        assert_eq!(cherry_pick(d, &g).unwrap(), Outcome::Done);
        assert!(d.join("g.txt").exists());
        assert_eq!(revert(d, "HEAD").unwrap(), Outcome::Done);
        assert!(!d.join("g.txt").exists());
        // a merge commit is picked against its first parent instead of failing
        git(d, &["switch", "-q", "main"]);
        let merge_commit = log(d, 10)
            .unwrap()
            .into_iter()
            .find(|c| c.parents.len() == 2)
            .unwrap()
            .id;
        git(d, &["switch", "-q", "-c", "picker", "HEAD~3"]);
        assert!(cherry_pick(d, &merge_commit).is_ok());
    }

    #[test]
    fn stash_save_apply_pop_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git(d, &["init", "-q", "-b", "main"]);
        std::fs::write(d.join("f.txt"), "one\n").unwrap();
        git(d, &["add", "f.txt"]);
        git(d, &["commit", "-q", "-m", "base"]);
        assert!(stashes(d).unwrap().is_empty());

        std::fs::write(d.join("f.txt"), "two\n").unwrap();
        std::fs::write(d.join("new.txt"), "n\n").unwrap();
        stash_save(d, "wip: two", true).unwrap();
        let st = status(d).unwrap();
        assert!(st.is_empty(), "{st:?}"); // untracked file went into the stash too
        std::fs::write(d.join("f.txt"), "three\n").unwrap();
        stash_save(d, "", false).unwrap();
        let list = stashes(d).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].message, "On main: wip: two");
        assert!(list[0].message.starts_with("WIP on main"));

        // apply keeps the stash, pop removes it
        assert_eq!(stash_apply(d, 1, false).unwrap(), Outcome::Done);
        assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "two\n");
        assert_eq!(stashes(d).unwrap().len(), 2);
        // uncommitted changes to the same file block applying the other stash; nothing is lost
        assert!(stash_apply(d, 0, true).is_err());
        assert_eq!(stashes(d).unwrap().len(), 2);
        git(d, &["checkout", "--", "f.txt"]);
        std::fs::remove_file(d.join("new.txt")).unwrap();
        assert_eq!(stash_apply(d, 0, true).unwrap(), Outcome::Done);
        assert_eq!(stashes(d).unwrap().len(), 1);
        stash_drop(d, 0).unwrap();
        assert!(stashes(d).unwrap().is_empty());
    }

    #[test]
    fn tags_create_delete_push() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, d) = (tmp.path().join("server.git"), tmp.path().join("repo"));
        git(
            tmp.path(),
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                server.to_str().unwrap(),
            ],
        );
        git(
            tmp.path(),
            &["init", "-q", "-b", "main", d.to_str().unwrap()],
        );
        git(&d, &["commit", "-q", "--allow-empty", "-m", "one"]);
        git(&d, &["commit", "-q", "--allow-empty", "-m", "two"]);

        create_tag(&d, "v1.0", "HEAD~1", "").unwrap();
        create_tag(&d, "v2.0", "HEAD", "Second release").unwrap();
        assert!(
            create_tag(&d, "bad tag", "HEAD", "")
                .unwrap_err()
                .to_string()
                .contains("not a valid")
        );
        let tags: Vec<Ref> = refs(&d)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == RefKind::Tag)
            .collect();
        let v1 = tags.iter().find(|t| t.name == "v1.0").unwrap();
        let v2 = tags.iter().find(|t| t.name == "v2.0").unwrap();
        assert_eq!(v1.target, v1.commit); // lightweight
        assert_ne!(v2.target, v2.commit); // annotated: a tag object
        assert_eq!(v2.commit, log(&d, 1).unwrap()[0].id);

        assert!(push_tag(&d, "v2.0").is_err()); // no remote yet
        git(&d, &["remote", "add", "origin", server.to_str().unwrap()]);
        assert_eq!(push_tag(&d, "v2.0").unwrap(), "origin");
        assert!(
            !String::from_utf8(run(&server, &["tag"]).unwrap())
                .unwrap()
                .contains("v1.0")
        );
        assert!(
            String::from_utf8(run(&server, &["tag"]).unwrap())
                .unwrap()
                .contains("v2.0")
        );

        delete_tag(&d, "v1.0").unwrap();
        assert!(!refs(&d).unwrap().iter().any(|r| r.name == "v1.0"));
    }

    #[test]
    fn interactive_rebase_reorders_squashes_drops() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git(d, &["init", "-q", "-b", "main"]);
        for (i, name) in ["base", "a", "b", "c", "d"].iter().enumerate() {
            std::fs::write(d.join(format!("{name}.txt")), format!("{i}\n")).unwrap();
            git(d, &["add", "."]);
            git(d, &["commit", "-q", "-m", name]);
        }
        let subjects = |d: &Path| {
            log(d, 20)
                .unwrap()
                .into_iter()
                .map(|c| c.subject)
                .collect::<Vec<_>>()
        };
        let range = rebase_range(d, "HEAD~4").unwrap();
        assert_eq!(
            range.iter().map(|c| c.subject.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c", "d"]
        );
        let id = |s: &str| range.iter().find(|c| c.subject == s).unwrap().id.clone();

        // reorder d first, squash b into it... keep a, fixup c into a, drop nothing
        let plan = vec![
            (id("d"), TodoAction::Pick),
            (id("b"), TodoAction::Squash),
            (id("a"), TodoAction::Pick),
            (id("c"), TodoAction::Fixup),
        ];
        assert_eq!(
            interactive_rebase(d, "HEAD~4", &plan).unwrap(),
            Outcome::Done
        );
        assert_eq!(subjects(d), ["a", "d", "base"]);
        let squashed = &log(d, 20).unwrap()[1];
        assert!(
            commit_details(
                d,
                &squashed.id,
                squashed.parents.first().map(String::as_str)
            )
            .unwrap()
            .message
            .contains("b")
        );
        for f in ["a", "b", "c", "d"] {
            assert!(d.join(format!("{f}.txt")).exists(), "{f}.txt kept");
        }

        // drop removes the commit and its file
        let range = rebase_range(d, "HEAD~2").unwrap();
        let plan = vec![
            (range[0].id.clone(), TodoAction::Drop),
            (range[1].id.clone(), TodoAction::Pick),
        ];
        interactive_rebase(d, "HEAD~2", &plan).unwrap();
        assert_eq!(subjects(d), ["a", "base"]);

        // guard rails
        let only = vec![(log(d, 1).unwrap()[0].id.clone(), TodoAction::Squash)];
        assert!(interactive_rebase(d, "HEAD~1", &only).is_err());
        let all_dropped = vec![(log(d, 1).unwrap()[0].id.clone(), TodoAction::Drop)];
        assert!(interactive_rebase(d, "HEAD~1", &all_dropped).is_err());
        assert_eq!(op_state(d).unwrap(), OpState::None);
    }

    #[test]
    fn clone_init_and_names() {
        assert_eq!(
            repo_name_from_url("https://github.com/medmdime/Spork.git"),
            "Spork"
        );
        assert_eq!(repo_name_from_url("git@github.com:org/tool"), "tool");
        assert_eq!(repo_name_from_url("/srv/git/project.git/"), "project");
        assert_eq!(repo_name_from_url(""), "repository");

        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        init(&origin).unwrap();
        git(&origin, &["commit", "-q", "--allow-empty", "-m", "hello"]);
        assert!(head_branch(&origin).unwrap().is_some());

        let dest = clone(origin.to_str().unwrap(), tmp.path(), "copy").unwrap();
        assert_eq!(head_message(&dest).as_deref(), Some("hello"));
        // never clone over an existing non-empty folder
        let err = clone(origin.to_str().unwrap(), tmp.path(), "copy").unwrap_err();
        assert!(err.to_string().contains("isn't empty"));
    }

    #[test]
    fn worktrees_and_submodules() {
        let tmp = tempfile::tempdir().unwrap();
        let (lib, d) = (tmp.path().join("lib"), tmp.path().join("app"));
        init(&lib).unwrap();
        git(&lib, &["commit", "-q", "--allow-empty", "-m", "lib"]);
        init(&d).unwrap();
        git(&d, &["commit", "-q", "--allow-empty", "-m", "app"]);
        git(&d, &["branch", "feature"]);

        let wt = tmp.path().join("app-feature");
        add_worktree(&d, &wt, "feature").unwrap();
        let list = worktrees(&d).unwrap();
        assert_eq!(list.len(), 2);
        assert!(list[0].is_main && !list[1].is_main);
        assert_eq!(list[1].branch.as_deref(), Some("feature"));
        assert_eq!(head_branch(&wt).unwrap().as_deref(), Some("feature"));
        std::fs::write(wt.join("dirty.txt"), "x").unwrap();
        assert!(remove_worktree(&d, &wt).is_err()); // uncommitted work is protected
        std::fs::remove_file(wt.join("dirty.txt")).unwrap();
        remove_worktree(&d, &wt).unwrap();
        assert_eq!(worktrees(&d).unwrap().len(), 1);

        assert!(submodules(&d).unwrap().is_empty());
        git(
            &d,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                lib.to_str().unwrap(),
                "vendor/lib",
            ],
        );
        git(&d, &["commit", "-q", "-m", "add lib"]);
        let subs = submodules(&d).unwrap();
        assert_eq!(
            (subs.len(), subs[0].path.as_str(), subs[0].state),
            (1, "vendor/lib", ' ')
        );
    }

    #[test]
    fn parses_porcelain_v2_records() {
        let out = "1 .M N... 100644 100644 100644 aaa bbb src/main.rs\0\
                   2 R. N... 100644 100644 100644 aaa bbb R100 new name.rs\0old name.rs\0\
                   u UU N... 100644 100644 100644 100644 a b c conflict.rs\0\
                   ? notes/todo.md\0";
        let st = parse_status(out);
        assert_eq!(st.len(), 4);
        assert_eq!(
            (st[0].path.as_str(), st[0].index, st[0].worktree),
            ("src/main.rs", '.', 'M')
        );
        assert!(st[0].has_unstaged() && !st[0].has_staged());
        assert_eq!(
            (st[1].path.as_str(), st[1].orig_path.as_deref()),
            ("new name.rs", Some("old name.rs"))
        );
        assert!(st[2].conflicted && st[2].has_unstaged() && !st[2].has_staged());
        assert!(st[3].untracked());
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

    #[test]
    fn search_args_for_an_empty_query() {
        assert_eq!(
            build_search_args(&SearchQuery::default()),
            vec!["log", "-z", "--all", LOG_FORMAT]
        );
    }

    #[test]
    fn search_args_skip_blank_fields_and_trim_the_rest() {
        let q = SearchQuery {
            message: "  fix login  ".into(),
            author: "Jane".into(),
            committer: "   ".into(),
            content: "fn main".into(),
            path: "src/lib.rs".into(),
            since: "2024-01-01".into(),
            until: String::new(),
        };
        assert_eq!(
            build_search_args(&q),
            vec![
                "log",
                "-z",
                "--all",
                LOG_FORMAT,
                "--grep",
                "fix login",
                "-i",
                "--author",
                "Jane",
                "-G",
                "fn main",
                "--since",
                "2024-01-01",
                "--",
                "src/lib.rs",
            ]
        );
    }

    #[test]
    fn deep_search_filters_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git(d, &["init", "-q", "-b", "main"]);
        // dated commit: sets author/committer identity and both dates; increasing dates
        // along the chain keep the date-limited history walk well behaved
        let dated = |msg: &str, user: &str, email: &str, date: &str, extra: &[&str]| {
            let name = format!("user.name={user}");
            let mail = format!("user.email={email}");
            let mut args = vec![
                "-c",
                name.as_str(),
                "-c",
                mail.as_str(),
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                msg,
            ];
            args.extend_from_slice(extra);
            let ok = Command::new("git")
                .args(&args)
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", date)
                .current_dir(d)
                .status()
                .unwrap();
            assert!(ok.success());
        };
        dated(
            "add parser",
            "Spork Test",
            "test@spork.invalid",
            "2019-05-01T12:00:00",
            &["--allow-empty"],
        );
        std::fs::write(d.join("hello.txt"), "first line\n").unwrap();
        git(d, &["add", "hello.txt"]);
        dated(
            "fix parser bug",
            "Spork Test",
            "test@spork.invalid",
            "2022-05-01T12:00:00",
            &[],
        );
        std::fs::write(d.join("hello.txt"), "second line\n").unwrap();
        git(d, &["add", "hello.txt"]);
        // third commit: different author and committer, newest in the chain
        dated(
            "update docs",
            "CI Bot",
            "ci@spork.invalid",
            "2024-05-01T12:00:00",
            &["--author", "Other Dev <other@spork.invalid>"],
        );
        // an older branch: its tip is the 2019 commit, so the --all walk reaches it
        git(d, &["branch", "old", "HEAD~2"]);
        // unreachable commit (not on any ref): --all must not find it
        git(d, &["commit", "-q", "--allow-empty", "-m", "lost commit"]);
        git(d, &["reset", "-q", "--hard", "HEAD~1"]);

        let all = search(d, &SearchQuery::default(), &Cancellable::default()).unwrap();
        assert_eq!(all.len(), 3);

        let by_message = search(
            d,
            &SearchQuery {
                message: "PARSER".into(),
                ..Default::default()
            },
            &Cancellable::default(),
        )
        .unwrap();
        assert_eq!(by_message.len(), 2); // case-insensitive: "add parser" and "fix parser bug"

        let by_author = search(
            d,
            &SearchQuery {
                author: "Other Dev".into(),
                ..Default::default()
            },
            &Cancellable::default(),
        )
        .unwrap();
        assert_eq!(by_author.len(), 1);
        assert_eq!(by_author[0].author, "Other Dev");

        let by_committer = search(
            d,
            &SearchQuery {
                committer: "Spork Test".into(),
                ..Default::default()
            },
            &Cancellable::default(),
        )
        .unwrap();
        assert_eq!(by_committer.len(), 2);

        let by_content = search(
            d,
            &SearchQuery {
                content: "second line".into(),
                ..Default::default()
            },
            &Cancellable::default(),
        )
        .unwrap();
        assert_eq!(by_content.len(), 1);
        assert_eq!(by_content[0].subject, "update docs");

        let by_path = search(
            d,
            &SearchQuery {
                path: "hello.txt".into(),
                ..Default::default()
            },
            &Cancellable::default(),
        )
        .unwrap();
        assert_eq!(by_path.len(), 2);

        // date limits prune the history walk: --until reaches the 2019 commit only via
        // the old branch tip, --since walks the main chain and stops at it
        let old = search(
            d,
            &SearchQuery {
                until: "2021-01-01".into(),
                ..Default::default()
            },
            &Cancellable::default(),
        )
        .unwrap();
        assert_eq!(old.len(), 1);
        assert_eq!(old[0].subject, "add parser");
        let recent = search(
            d,
            &SearchQuery {
                since: "2021-01-01".into(),
                ..Default::default()
            },
            &Cancellable::default(),
        )
        .unwrap();
        assert_eq!(recent.len(), 2);

        let lost = search(
            d,
            &SearchQuery {
                message: "lost".into(),
                ..Default::default()
            },
            &Cancellable::default(),
        )
        .unwrap();
        assert!(lost.is_empty());
    }

    #[test]
    fn cancelling_a_run_kills_the_git_child() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().to_path_buf();
        git(&d, &["init", "-q", "-b", "main"]);
        let cancel = Cancellable::default();
        // `git daemon` never exits on its own: the run ends only when the process is killed
        let inner = cancel.clone();
        let run = std::thread::spawn(move || {
            // port 0: an ephemeral port, so no bind conflicts with other daemons
            run_cancellable(
                &d,
                &["daemon", "--reuseaddr", "--listen=127.0.0.1", "--port=0"],
                &inner,
            )
        });
        std::thread::sleep(std::time::Duration::from_millis(500)); // let it spawn
        cancel.kill();
        assert!(run.join().unwrap().is_err());
    }

    #[test]
    fn file_history_parses_commit_rows_and_renames() {
        // newest first; the second row renamed f.txt to g.txt
        let rows = [
            ("c4", "c3", "edit g", "\nM\0g.txt\0"),
            ("c3", "c2", "rename f to g", "\nR100\0f.txt\0g.txt\0"),
            ("c2", "c1", "edit f", "\nM\0f.txt\0"),
            ("c1", "", "add f", "\nA\0f.txt\0"),
        ];
        let mut out = String::new();
        for (oid, parents, subject, status) in &rows {
            out.push_str(&format!(
                "{oid}\x1f{parents}\x1fAlice\x1fa@example.com\x1f1700000000\x1f{subject}\0"
            ));
            out.push_str(status);
        }
        let entries = parse_file_history(&out).unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(
            (entries[0].oid.as_str(), entries[0].subject.as_str()),
            ("c4", "edit g")
        );
        assert_eq!(
            (entries[0].author.as_str(), entries[0].date),
            ("Alice", 1_700_000_000)
        );
        assert_eq!(entries[0].path, "g.txt");
        assert_eq!(entries[0].renamed_from, None);
        assert_eq!(entries[1].path, "g.txt");
        assert_eq!(entries[1].renamed_from.as_deref(), Some("f.txt"));
        assert_eq!(entries[2].path, "f.txt");
        assert_eq!(entries[2].renamed_from, None);
        assert_eq!(
            (entries[3].subject.as_str(), entries[3].path.as_str()),
            ("add f", "f.txt")
        );
        assert!(parse_file_history("").unwrap().is_empty());
    }

    #[test]
    fn file_history_rejects_malformed_output() {
        let row = "c1\x1f\x1fA\x1fa@x\x1f1\x1fadd\0";
        assert!(parse_file_history(&format!("{row}\nA\0f.txt\0")).is_ok());
        assert!(parse_file_history("\nM\0f.txt\0").is_err()); // status before any commit
        assert!(parse_file_history(&format!("{row}\nX\0f.txt\0")).is_err()); // unknown status
        assert!(parse_file_history(&format!("{row}\nR\0f.txt\0g.txt\0")).is_err()); // rename without a score
        assert!(parse_file_history(row).is_err()); // commit without a file status
        assert!(parse_file_history(&format!("{row}\nA\0")).is_err()); // missing path
    }

    #[test]
    fn file_history_follows_a_rename_in_a_real_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git(d, &["init", "-q", "-b", "main"]);
        std::fs::write(d.join("f.txt"), "one\n").unwrap();
        git(d, &["add", "f.txt"]);
        git(d, &["commit", "-q", "-m", "add f"]);
        std::fs::write(d.join("f.txt"), "one\ntwo\n").unwrap();
        git(d, &["add", "f.txt"]);
        git(d, &["commit", "-q", "-m", "edit f"]);
        git(d, &["mv", "f.txt", "g.txt"]);
        git(d, &["commit", "-q", "-m", "rename f to g"]);
        std::fs::write(d.join("g.txt"), "one\ntwo\nthree\n").unwrap();
        git(d, &["add", "g.txt"]);
        git(d, &["commit", "-q", "-m", "edit g"]);

        let history = file_history(d, "g.txt", &Cancellable::default()).unwrap();
        assert_eq!(
            history
                .iter()
                .map(|e| e.subject.as_str())
                .collect::<Vec<_>>(),
            ["edit g", "rename f to g", "edit f", "add f"]
        );
        assert_eq!(history[1].path, "g.txt");
        assert_eq!(history[1].renamed_from.as_deref(), Some("f.txt"));
        assert_eq!(history[2].path, "f.txt");
        assert_eq!(history[2].renamed_from, None);
        assert_eq!(history[3].path, "f.txt");
        assert!(
            file_history(d, "nope.txt", &Cancellable::default())
                .unwrap()
                .is_empty()
        );

        // the diff of each entry is its change to the file, rename included
        let rename_diff = file_history_diff(
            d,
            &history[1].oid,
            &history[1].path,
            history[1].renamed_from.as_deref(),
        )
        .unwrap();
        assert!(rename_diff.contains("rename from f.txt"), "{rename_diff}");
        assert!(rename_diff.contains("rename to g.txt"), "{rename_diff}");
        let edit_diff = file_history_diff(
            d,
            &history[2].oid,
            &history[2].path,
            history[2].renamed_from.as_deref(),
        )
        .unwrap();
        assert!(edit_diff.contains("+++ b/f.txt"), "{edit_diff}");
        let first_diff = file_history_diff(
            d,
            &history[3].oid,
            &history[3].path,
            history[3].renamed_from.as_deref(),
        )
        .unwrap();
        assert!(first_diff.contains("+one"), "{first_diff}"); // root commit: against the empty tree

        // the content of the file at each revision (under the name it had then)
        assert_eq!(
            show_file(d, &history[0].oid, &history[0].path).unwrap(),
            b"one\ntwo\nthree\n"
        );
        assert_eq!(
            show_file(d, &history[2].oid, &history[2].path).unwrap(),
            b"one\ntwo\n"
        );
        assert_eq!(
            show_file(d, &history[3].oid, &history[3].path).unwrap(),
            b"one\n"
        );

        // restoring the working-tree file from an older revision
        std::fs::write(d.join("g.txt"), "changed\n").unwrap();
        restore_file(d, &history[1].oid, "g.txt").unwrap();
        assert_eq!(std::fs::read(d.join("g.txt")).unwrap(), b"one\ntwo\n");
    }

    #[test]
    fn blame_parses_porcelain_groups() {
        let a = "6b2c8a1f00000000000000000000000000000000";
        let b = "7d9e2b3a00000000000000000000000000000000";
        let out = format!(
            "{a} 1 1 2\n\
             author Alice\n\
             author-time 1735689600\n\
             summary first commit\n\
             \tone\n\
             \ttwo\n\
             {b} 2 3\n\
             author Bob\n\
             author-time 1748736000\n\
             summary second commit\n\
             \tthree\n\
             {a} 4 4\n\
             \tfour\n\
             0000000000000000000000000000000000000000 1 5\n\
             boundary\n\
             \tfive\n"
        );
        let lines = parse_blame_porcelain(&out).unwrap();
        assert_eq!(
            lines
                .iter()
                .map(|l| (l.line, l.final_oid.as_str(), l.author.as_str()))
                .collect::<Vec<_>>(),
            [
                (1, a, "Alice"),
                (2, a, "Alice"),
                (3, b, "Bob"),
                // commit A repeats non-adjacently: its header was already
                // emitted, so this group inherits A's recorded metadata
                (4, a, "Alice"),
                (5, "0000000000000000000000000000000000000000", ""),
            ]
        );
        assert_eq!(lines[0].author_date, 1735689600);
        assert_eq!(lines[3].summary, "first commit"); // inherited from A
        assert_eq!(lines[3].author_date, 1735689600);
        assert_eq!(lines[2].summary, "second commit");
        assert_eq!(lines[4].author_date, 0);
        assert_eq!(lines[4].summary, "");
    }

    #[test]
    fn blame_rejects_malformed_output() {
        let a = "6b2c8a1f00000000000000000000000000000000";
        // content before any group
        assert!(parse_blame_porcelain("\tone\n").is_err());
        // a group that declares more lines than it has
        assert!(parse_blame_porcelain(&format!("{a} 1 1 2\n\tone\n")).is_err());
        // a group header while the previous group is still open
        assert!(parse_blame_porcelain(&format!("{a} 1 1 2\n\tone\n{a} 2 2\n\ttwo\n")).is_err());
        // a header key outside a group
        assert!(parse_blame_porcelain("author Alice\n").is_err());
        // an unknown header key
        assert!(parse_blame_porcelain(&format!("{a} 1 1 1\nbogus x\n\tone\n")).is_err());
        // a header omitted for a commit whose header was never emitted
        let b = "7d9e2b3a00000000000000000000000000000000";
        assert!(parse_blame_porcelain(&format!("{b} 1 1 1\n\tone\n")).is_err());
        // a partial header block (git emits all or nothing)
        assert!(parse_blame_porcelain(&format!("{a} 1 1 1\nauthor Alice\n\tone\n")).is_err());
    }

    #[test]
    fn blames_a_multi_author_file_in_a_real_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git(d, &["init", "-q", "-b", "main"]);
        std::fs::write(d.join("f.txt"), "a\nb\nc\n").unwrap();
        git_as(d, "Alice", "2025-01-01T00:00:00Z", &["add", "f.txt"]);
        git_as(
            d,
            "Alice",
            "2025-01-01T00:00:00Z",
            &["commit", "-q", "-m", "add f"],
        );
        let alice = head(d);
        std::fs::write(d.join("f.txt"), "a\nb2\nc\n").unwrap();
        git_as(d, "Bob", "2025-06-01T00:00:00Z", &["add", "f.txt"]);
        git_as(
            d,
            "Bob",
            "2025-06-01T00:00:00Z",
            &["commit", "-q", "-m", "tweak line 2"],
        );
        let bob = head(d);
        std::fs::write(d.join("f.txt"), "a2\nb2\nc\nd\n").unwrap();
        git_as(d, "Carol", "2026-11-01T00:00:00Z", &["add", "f.txt"]);
        git_as(
            d,
            "Carol",
            "2026-11-01T00:00:00Z",
            &["commit", "-q", "-m", "tweak line 1, add line 4"],
        );
        let carol = head(d);

        let lines = blame(d, &carol, "f.txt", false, &Cancellable::default()).unwrap();
        assert_eq!(
            lines
                .iter()
                .map(|l| (l.line, l.final_oid.as_str(), l.author.as_str()))
                .collect::<Vec<_>>(),
            [
                (1, carol.as_str(), "Carol"),
                (2, bob.as_str(), "Bob"),
                (3, alice.as_str(), "Alice"),
                (4, carol.as_str(), "Carol"),
            ]
        );

        // the same blame with whitespace ignored
        let lax = blame(d, &carol, "f.txt", true, &Cancellable::default()).unwrap();
        assert_eq!(lax.len(), 4);

        // paths with glob characters are passed through as-is: blame takes a
        // plain path (no globbing, no pathspec magic)
        std::fs::write(d.join("g*.txt"), "glob\n").unwrap();
        git_as(d, "Bob", "2025-06-01T00:00:00Z", &["add", "g*.txt"]);
        git_as(
            d,
            "Bob",
            "2025-06-01T00:00:00Z",
            &["commit", "-q", "-m", "add glob"],
        );
        let glob = blame(d, &head(d), "g*.txt", false, &Cancellable::default()).unwrap();
        assert_eq!(glob.len(), 1);
        assert_eq!(glob[0].author, "Bob");

        // only the first commit has no parent
        assert_eq!(
            root_commits(d, &[&alice, &bob, &carol]).unwrap(),
            vec![alice]
        );
        assert!(root_commits(d, &[]).unwrap().is_empty());

        // a missing path is an error, not a parse result
        assert!(blame(d, &carol, "nope.txt", false, &Cancellable::default()).is_err());
    }
}
