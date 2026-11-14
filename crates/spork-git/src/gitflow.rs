//! Git-flow (docs/specs/M13 §1–3): init, starting feature / release / hotfix branches,
//! and finishing a feature.
//!
//! The configuration uses the git-flow CLI's keys (`gitflow.branch.develop`,
//! `gitflow.prefix.feature`, …) so a repo set up in Spork stays compatible with
//! `git flow` and vice versa (M13 §1).
//!
//! Finishing a feature runs its steps as separate git calls and records progress in
//! `<git dir>/spork/gitflow-finish.json` (M13 §3): when a step stops on conflicts the
//! state file says which step to re-run, so the UI can offer "Resume git-flow finish"
//! once the conflicts are resolved, or drop the state file to abort.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    Cancellable, GitError, OpState, Outcome, allow_conflicts, op_state, run, run_cancellable,
    run_with_env,
};

/// Git-flow configuration. Prefixes are stored exactly the way the git-flow CLI stores them
/// (with the trailing `/`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitflowConfig {
    /// Production branch (git-flow calls it `master`).
    pub master: String,
    /// Development branch.
    pub develop: String,
    /// `feature/` prefix.
    pub feature: String,
    /// `release/` prefix.
    pub release: String,
    /// `hotfix/` prefix.
    pub hotfix: String,
    /// `bugfix/` prefix.
    pub bugfix: String,
    /// `support/` prefix.
    pub support: String,
    /// Version tag prefix (empty for plain version tags).
    pub versiontag: String,
}

/// Parses the output of `git config --list --null` into a git-flow configuration (M13 §1).
/// Entries are `key\nvalue` separated by NUL bytes. Returns `None` unless both
/// `gitflow.branch.develop` and `gitflow.branch.master` are set; prefix keys fall back to
/// the git-flow CLI defaults when absent.
pub fn parse_gitflow_config(config_list: &str) -> Option<GitflowConfig> {
    let values: std::collections::BTreeMap<String, String> = config_list
        .split('\0')
        .filter_map(|entry| {
            entry
                .split_once('\n')
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
        })
        .collect();
    let master = values.get("gitflow.branch.master")?;
    let develop = values.get("gitflow.branch.develop")?;
    let get = |key: &str| values.get(key).cloned();
    Some(GitflowConfig {
        master: master.clone(),
        develop: develop.clone(),
        feature: get("gitflow.prefix.feature").unwrap_or_else(|| "feature/".to_owned()),
        release: get("gitflow.prefix.release").unwrap_or_else(|| "release/".to_owned()),
        hotfix: get("gitflow.prefix.hotfix").unwrap_or_else(|| "hotfix/".to_owned()),
        bugfix: get("gitflow.prefix.bugfix").unwrap_or_else(|| "bugfix/".to_owned()),
        support: get("gitflow.prefix.support").unwrap_or_else(|| "support/".to_owned()),
        versiontag: get("gitflow.prefix.versiontag").unwrap_or_default(),
    })
}

/// Reads the git-flow configuration of `repo` (M13 §1); `None` when git-flow is not
/// initialized there.
pub fn gitflow_config(repo: &Path) -> Option<GitflowConfig> {
    let out = run(repo, &["config", "--list", "--null"]).ok()?;
    parse_gitflow_config(&String::from_utf8_lossy(&out))
}

fn not_initialized() -> GitError {
    GitError::Other {
        code: None,
        stderr: "git-flow is not initialized in this repository".to_owned(),
    }
}

/// Writes the git-flow configuration and brings the branch layout up to date (M13 §1).
/// Idempotent: re-running it on an initialized repo just rewrites the same keys. In a repo
/// without commits the production branch is created first with an empty initial commit.
pub fn gitflow_init(repo: &Path, config: &GitflowConfig) -> Result<(), GitError> {
    // Unborn repo (no commits yet): give the production branch its first empty commit.
    let has_commits = run(repo, &["rev-parse", "HEAD"]).is_ok();
    if !has_commits {
        let on = run(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .ok()
            .map(|o| String::from_utf8_lossy(&o).trim_end().to_owned());
        if on.as_deref() != Some(config.master.as_str()) {
            run(repo, &["switch", "-c", &config.master])?;
        }
        run(
            repo,
            &[
                "commit",
                "--allow-empty",
                "-m",
                &format!("Initial commit on {}", config.master),
            ],
        )?;
    }
    let keys = [
        ("gitflow.branch.master", &config.master),
        ("gitflow.branch.develop", &config.develop),
        ("gitflow.prefix.feature", &config.feature),
        ("gitflow.prefix.release", &config.release),
        ("gitflow.prefix.hotfix", &config.hotfix),
        ("gitflow.prefix.bugfix", &config.bugfix),
        ("gitflow.prefix.support", &config.support),
        ("gitflow.prefix.versiontag", &config.versiontag),
    ];
    for (key, value) in keys {
        run(repo, &["config", key, value])?;
    }
    let develop_exists = run(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{}", config.develop),
        ],
    )
    .is_ok();
    if !develop_exists {
        run(repo, &["branch", &config.develop, &config.master])?;
    }
    Ok(())
}

/// The kind of branch a "Start" action creates (M13 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowKind {
    Feature,
    Release,
    Hotfix,
}

impl FlowKind {
    /// Its prefix in `config`.
    pub fn prefix<'a>(&self, config: &'a GitflowConfig) -> &'a str {
        match self {
            FlowKind::Feature => &config.feature,
            FlowKind::Release => &config.release,
            FlowKind::Hotfix => &config.hotfix,
        }
    }

    /// The base branch git-flow starts this kind of branch from.
    pub fn default_base<'a>(&self, config: &'a GitflowConfig) -> &'a str {
        match self {
            FlowKind::Hotfix => &config.master,
            FlowKind::Feature | FlowKind::Release => &config.develop,
        }
    }
}

/// Warn-only observations for a "Start" dialog (M13 §2). Never block the action.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartCheck {
    /// The base branch is behind its upstream (`upstream`, commits behind).
    pub base_behind_upstream: Option<(String, u32)>,
    /// The base branch's upstream no longer exists.
    pub gone_upstream: Option<String>,
    /// Other release/hotfix branches that already exist (release/hotfix kinds only).
    pub other_branches: Vec<String>,
}

/// Computes the start-check notes for `gitflow_start` (M13 §2).
pub fn gitflow_start_checks(
    repo: &Path,
    kind: FlowKind,
    name_or_version: &str,
    base: &str,
) -> Result<StartCheck, GitError> {
    let config = gitflow_config(repo).ok_or_else(not_initialized)?;
    let mut check = StartCheck::default();
    let upstream = {
        let out = run(
            repo,
            &[
                "for-each-ref",
                "--format=%(upstream:short)",
                &format!("refs/heads/{base}"),
            ],
        )?;
        String::from_utf8_lossy(&out).trim_end().to_owned()
    };
    if !upstream.is_empty() {
        let out = run(
            repo,
            &[
                "for-each-ref",
                "--format=%(upstream:track,nobracket)",
                &format!("refs/heads/{base}"),
            ],
        )?;
        let (_, behind, gone) = crate::parse_track(String::from_utf8_lossy(&out).trim_end());
        if gone {
            check.gone_upstream = Some(upstream);
        } else if behind > 0 {
            check.base_behind_upstream = Some((upstream, behind));
        }
    }
    if matches!(kind, FlowKind::Release | FlowKind::Hotfix) {
        let prefix = kind.prefix(&config);
        let new_name = prefix.to_owned() + name_or_version;
        let out = run(
            repo,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
        )?;
        for branch in String::from_utf8_lossy(&out).lines() {
            let branch = branch.trim_end();
            if branch.starts_with(prefix)
                && branch != new_name
                && !branch.trim_start_matches(prefix).is_empty()
            {
                check.other_branches.push(branch.to_owned());
            }
        }
    }
    Ok(check)
}

/// Creates `<prefix><name_or_version>` from `base` and switches to it (M13 §2).
/// Returns the new branch name.
pub fn gitflow_start(
    repo: &Path,
    kind: FlowKind,
    name_or_version: &str,
    base: &str,
) -> Result<String, GitError> {
    let config = gitflow_config(repo).ok_or_else(not_initialized)?;
    let name = kind.prefix(&config).to_owned() + name_or_version;
    if name_or_version.trim().is_empty()
        || name_or_version.contains('/')
        || name_or_version.contains(' ')
    {
        return Err(GitError::Other {
            code: None,
            stderr: format!("invalid name or version: {name_or_version}"),
        });
    }
    if base.trim().is_empty() {
        return Err(GitError::Other {
            code: None,
            stderr: "no base branch given".to_owned(),
        });
    }
    let base_exists = run(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{base}"),
        ],
    )
    .is_ok();
    if !base_exists {
        return Err(GitError::Other {
            code: None,
            stderr: format!("base branch {base} does not exist"),
        });
    }
    run(repo, &["switch", "-c", &name, base])?;
    Ok(name)
}

/// Options for `gitflow_finish` (M13 §3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinishOptions {
    /// Rebase the feature branch onto the development branch before merging.
    pub rebase_before_merge: bool,
    /// Merge with `--no-ff` (keep an explicit merge commit).
    pub no_fast_forward: bool,
    /// Delete the feature branch after merging (git-flow default).
    pub delete_branch: bool,
    /// Also delete the branch on its upstream remote.
    pub delete_remote: bool,
}

impl Default for FinishOptions {
    fn default() -> Self {
        Self {
            rebase_before_merge: false,
            no_fast_forward: false,
            delete_branch: true,
            delete_remote: false,
        }
    }
}

/// Saved progress of an interrupted finish (M13 §3), written to
/// `<git dir>/spork/gitflow-finish.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinishState {
    pub kind: String,
    pub name: String,
    /// 1-based index of the step to (re)run next.
    pub step: usize,
    pub options: FinishOptions,
}

/// How a finish run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishResult {
    /// All steps are done and the state file removed.
    Done,
    /// A step stopped on conflicts; `step` is the one to re-run after resolving them.
    Conflicts { step: usize },
}

/// Human-readable label for a saved finish step (for the banner).
pub fn finish_step_label(step: usize) -> &'static str {
    match step {
        1 => "rebase onto develop",
        2 => "switch to develop",
        3 => "merge into develop",
        _ => "cleaning up branches",
    }
}

/// The state file lives next to the rest of git's own state, under `spork/`.
fn state_file(repo: &Path) -> Result<PathBuf, GitError> {
    let out = run(repo, &["rev-parse", "--absolute-git-dir"])?;
    let git_dir = PathBuf::from(String::from_utf8_lossy(&out).trim_end());
    Ok(git_dir.join("spork").join("gitflow-finish.json"))
}

fn save_finish_state(repo: &Path, state: &FinishState) -> Result<(), GitError> {
    let file = state_file(repo)?;
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| update_error(&e, &file))?;
    }
    let json = serde_json::to_string(state).map_err(|e| update_error(&e, &file))?;
    std::fs::write(&file, json).map_err(|e| update_error(&e, &file))
}

fn update_error(e: &impl std::fmt::Display, file: &Path) -> GitError {
    GitError::Other {
        code: None,
        stderr: format!("could not update {}: {e}", file.display()),
    }
}

/// Reads the saved finish state, if a finish is in progress (M13 §3).
pub fn gitflow_finish_state(repo: &Path) -> Option<FinishState> {
    let file = state_file(repo).ok()?;
    let content = std::fs::read_to_string(file).ok()?;
    serde_json::from_str(&content).ok()
}

/// Drops the saved finish state without touching the repository (M13 §3 "Abort finish").
pub fn gitflow_abort_finish(repo: &Path) -> Result<(), GitError> {
    let file = state_file(repo)?;
    match std::fs::remove_file(&file) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(update_error(&e, &file)),
    }
}

/// Finishes the feature branch we are on (M13 §3): rebase (optional), switch to develop,
/// merge, delete the branch. Each step is a separate git call and progress is saved after
/// each one, so an interrupted run can be resumed with `gitflow_resume_finish`.
pub fn gitflow_finish(
    repo: &Path,
    options: &FinishOptions,
    cancel: &Cancellable,
) -> Result<FinishResult, GitError> {
    let config = gitflow_config(repo).ok_or_else(not_initialized)?;
    let out = run(repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let name = String::from_utf8_lossy(&out).trim_end().to_owned();
    if !name.starts_with(config.feature.as_str()) {
        return Err(GitError::Other {
            code: None,
            stderr: format!("{name} is not a {} branch", config.feature),
        });
    }
    save_finish_state(
        repo,
        &FinishState {
            kind: "feature".to_owned(),
            name: name.clone(),
            step: 1,
            options: options.clone(),
        },
    )?;
    finish_steps(repo, &name, 1, options, cancel)
}

/// Continues an interrupted finish from its saved step (M13 §3). The current branch is not
/// consulted — the saved state is.
pub fn gitflow_resume_finish(repo: &Path, cancel: &Cancellable) -> Result<FinishResult, GitError> {
    let state = gitflow_finish_state(repo).ok_or_else(|| GitError::Other {
        code: None,
        stderr: "no git-flow finish in progress".to_owned(),
    })?;
    if state.kind != "feature" {
        return Err(GitError::Other {
            code: None,
            stderr: format!("unknown git-flow finish kind: {}", state.kind),
        });
    }
    finish_steps(repo, &state.name, state.step, &state.options, cancel)
}

/// Runs the finish steps from `step` onward (1-based), saving progress after each one.
fn finish_steps(
    repo: &Path,
    name: &str,
    mut step: usize,
    options: &FinishOptions,
    cancel: &Cancellable,
) -> Result<FinishResult, GitError> {
    let develop = gitflow_config(repo).ok_or_else(not_initialized)?.develop;
    loop {
        let conflicted = match step {
            1 => {
                if options.rebase_before_merge {
                    match rebase_onto_develop(repo, name, &develop, cancel)? {
                        Outcome::Conflicts => true,
                        Outcome::Done => false,
                    }
                } else {
                    false
                }
            }
            2 => {
                run_cancellable(repo, &["switch", &develop], cancel)?;
                false
            }
            3 => match merge_feature(repo, name, options.no_fast_forward, cancel)? {
                Outcome::Conflicts => true,
                Outcome::Done => false,
            },
            4 => {
                delete_feature_branch(repo, name, options, cancel)?;
                false
            }
            _ => {
                return Err(GitError::Other {
                    code: None,
                    stderr: format!("unknown git-flow finish step {step}"),
                });
            }
        };
        if conflicted {
            // The state file already records `step` as the next step.
            return Ok(FinishResult::Conflicts { step });
        }
        step += 1;
        if step > 4 {
            std::fs::remove_file(state_file(repo)?).ok();
            return Ok(FinishResult::Done);
        }
        save_finish_state(
            repo,
            &FinishState {
                kind: "feature".to_owned(),
                name: name.to_owned(),
                step,
                options: options.clone(),
            },
        )?;
    }
}

/// Step 1: rebase the feature branch onto develop. Re-runs a paused rebase as-is.
fn rebase_onto_develop(
    repo: &Path,
    name: &str,
    develop: &str,
    cancel: &Cancellable,
) -> Result<Outcome, GitError> {
    let args = match op_state(repo)? {
        OpState::Rebasing { .. } => vec!["rebase", "--continue"],
        _ => vec!["rebase", develop, name],
    };
    allow_conflicts(
        repo,
        run_with_env(repo, &args, None, &[("GIT_EDITOR", "true")], Some(cancel)),
    )
}

/// Step 3: merge the feature branch into develop (we are on develop). When a merge is
/// already in progress (a previous attempt stopped on conflicts) it is continued instead
/// of started again.
fn merge_feature(
    repo: &Path,
    name: &str,
    no_fast_forward: bool,
    cancel: &Cancellable,
) -> Result<Outcome, GitError> {
    if matches!(op_state(repo)?, OpState::Merging) {
        return crate::continue_op(repo);
    }
    let mut args = vec!["merge", "--no-edit"];
    if no_fast_forward {
        args.push("--no-ff");
    }
    args.push(name);
    allow_conflicts(repo, run_cancellable(repo, &args, cancel))
}

/// Step 4: delete the feature branch locally, and on its upstream remote when asked.
/// The tracking remote is read before the local deletion — `git branch -d` removes the
/// branch's config section, which is where `branch.<name>.remote` lives. Both deletions
/// are skipped when the branch is already gone.
fn delete_feature_branch(
    repo: &Path,
    name: &str,
    options: &FinishOptions,
    cancel: &Cancellable,
) -> Result<(), GitError> {
    let local_exists = run(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ],
    )
    .is_ok();
    let remote = if options.delete_remote {
        tracking_remote(repo, name)?.filter(|remote| {
            run(
                repo,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/remotes/{remote}/{name}"),
                ],
            )
            .is_ok()
        })
    } else {
        None
    };
    if options.delete_branch && local_exists {
        run_cancellable(repo, &["branch", "-d", name], cancel)?;
    }
    if let Some(remote) = remote {
        run_cancellable(repo, &["push", &remote, "--delete", name], cancel)?;
    }
    Ok(())
}

/// The remote that tracks `name` (from `branch.<name>.remote`), if any.
fn tracking_remote(repo: &Path, name: &str) -> Result<Option<String>, GitError> {
    let Ok(out) = run(repo, &["config", "--get", &format!("branch.{name}.remote")]) else {
        return Ok(None);
    };
    let remote = String::from_utf8_lossy(&out).trim_end().to_owned();
    if remote.is_empty() {
        Ok(None)
    } else {
        Ok(Some(remote))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::git;

    fn default_config() -> GitflowConfig {
        GitflowConfig {
            master: "master".to_owned(),
            develop: "develop".to_owned(),
            feature: "feature/".to_owned(),
            release: "release/".to_owned(),
            hotfix: "hotfix/".to_owned(),
            bugfix: "bugfix/".to_owned(),
            support: "support/".to_owned(),
            versiontag: "v".to_owned(),
        }
    }

    fn file(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    /// Runs git and returns the trimmed stdout (panics on failure).
    fn out(d: &Path, args: &[&str]) -> String {
        String::from_utf8_lossy(&run(d, args).unwrap())
            .trim_end()
            .to_owned()
    }

    #[test]
    fn init_writes_config_and_creates_develop() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "-b", "master"]);
        file(d, "app.txt", "one\n");
        git(d, &["add", "app.txt"]);
        git(d, &["commit", "-m", "first"]);

        gitflow_init(d, &default_config()).unwrap();

        assert_eq!(gitflow_config(d).unwrap(), default_config());
        let branches = out(d, &["branch", "--list"]);
        assert!(branches.contains("develop"), "develop must exist");
        assert!(branches.contains("master"));
        // develop starts at the same commit as master
        assert_eq!(
            out(d, &["rev-parse", "develop", "master"]).lines().count(),
            2,
            "develop and master must point at the same commit"
        );
    }

    #[test]
    fn init_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "-b", "master"]);
        git(d, &["commit", "--allow-empty", "-m", "first"]);

        gitflow_init(d, &default_config()).unwrap();
        let before = out(d, &["rev-parse", "develop"]);
        git(d, &["commit", "--allow-empty", "-m", "more on develop"]);
        git(d, &["switch", "master"]);
        git(d, &["commit", "--allow-empty", "-m", "more on master"]);

        gitflow_init(d, &default_config()).unwrap();

        let after = out(d, &["rev-parse", "develop"]);
        assert_eq!(before, after, "re-init must not move develop");
        assert_eq!(gitflow_config(d).unwrap(), default_config());
    }

    #[test]
    fn init_unborn_repo_creates_production_commit() {
        for (init_branch, master) in [("main", "master"), ("main", "main")] {
            let dir = tempfile::tempdir().unwrap();
            let d = dir.path();
            git(d, &["init", "-b", init_branch]);
            assert!(
                run(d, &["rev-parse", "HEAD"]).is_err(),
                "repo must start unborn"
            );

            let mut cfg = default_config();
            cfg.master = master.to_owned();
            gitflow_init(d, &cfg).unwrap();

            assert_eq!(
                out(d, &["rev-parse", "--abbrev-ref", "HEAD"]),
                master,
                "HEAD must be on the production branch"
            );
            assert_eq!(
                out(d, &["rev-list", "--count", "HEAD"]),
                "1",
                "exactly one (empty initial) commit"
            );
            assert_eq!(gitflow_config(d).unwrap(), cfg);
        }
    }

    #[test]
    fn parse_gitflow_config_requires_both_branches() {
        let all =
            "gitflow.branch.master\nproduction\0gitflow.branch.develop\ndev\0user.name\nFoo\0";
        let cfg = parse_gitflow_config(all).unwrap();
        assert_eq!(cfg.master, "production");
        assert_eq!(cfg.develop, "dev");
        // prefixes fall back to the git-flow CLI defaults
        assert_eq!(cfg.feature, "feature/");
        assert_eq!(cfg.release, "release/");
        assert_eq!(cfg.hotfix, "hotfix/");
        assert_eq!(cfg.bugfix, "bugfix/");
        assert_eq!(cfg.support, "support/");
        assert_eq!(cfg.versiontag, "");

        // explicit prefixes win
        let cfg = parse_gitflow_config(
            "gitflow.branch.master\nm\0gitflow.branch.develop\nd\0gitflow.prefix.feature\nfeat/\0",
        )
        .unwrap();
        assert_eq!(cfg.feature, "feat/");
        assert_eq!(cfg.release, "release/");

        // garbage values never panic, they just don't parse
        assert!(parse_gitflow_config("").is_none());
        assert!(parse_gitflow_config("gitflow.branch.master\nmaster\0").is_none());
        assert!(parse_gitflow_config("gitflow.branch.develop\ndevelop\0").is_none());
        assert!(parse_gitflow_config("totally\nnot\0config").is_none());
    }

    #[test]
    fn config_round_trip_in_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "-b", "master"]);
        git(d, &["commit", "--allow-empty", "-m", "first"]);
        assert!(
            gitflow_config(d).is_none(),
            "uninitialized repo has no config"
        );

        gitflow_init(d, &default_config()).unwrap();
        assert_eq!(gitflow_config(d).unwrap(), default_config());
    }

    #[test]
    fn start_feature_creates_prefixed_branch() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "-b", "master"]);
        git(d, &["commit", "--allow-empty", "-m", "first"]);
        gitflow_init(d, &default_config()).unwrap();
        git(d, &["switch", "develop"]);
        git(d, &["commit", "--allow-empty", "-m", "dev work"]);

        let name = gitflow_start(d, FlowKind::Feature, "login-page", "develop").unwrap();
        assert_eq!(name, "feature/login-page");
        assert_eq!(
            out(d, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "feature/login-page"
        );

        // a missing base is an error, not a panic
        assert!(matches!(
            gitflow_start(d, FlowKind::Feature, "x", "nope").unwrap_err(),
            GitError::Other { .. }
        ));
        // empty or slashed names are rejected before git sees them
        assert!(gitflow_start(d, FlowKind::Feature, "", "develop").is_err());
        assert!(gitflow_start(d, FlowKind::Feature, "a/b", "develop").is_err());
        // an existing branch is not clobbered
        assert!(gitflow_start(d, FlowKind::Feature, "login-page", "develop").is_err());
    }

    #[test]
    fn start_release_and_hotfix_use_their_base() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "-b", "master"]);
        git(d, &["commit", "--allow-empty", "-m", "first"]);
        gitflow_init(d, &default_config()).unwrap();

        let name = gitflow_start(d, FlowKind::Release, "1.2.0", "develop").unwrap();
        assert_eq!(name, "release/1.2.0");

        git(d, &["switch", "master"]);
        let name = gitflow_start(d, FlowKind::Hotfix, "1.1.1", "master").unwrap();
        assert_eq!(name, "hotfix/1.1.1");
    }

    #[test]
    fn start_checks_report_upstream_and_other_branches() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        // a bare "origin" that develop is tracked on
        let origin = d.join("origin.git");
        git(d, &["init", "--bare", "-b", "master", "origin.git"]);
        git(d, &["init", "-b", "master"]);
        git(d, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(d, &["commit", "--allow-empty", "-m", "first"]);
        git(d, &["push", "-u", "origin", "master"]);
        gitflow_init(d, &default_config()).unwrap();
        git(d, &["push", "-u", "origin", "develop"]);
        git(d, &["switch", "develop"]);
        git(d, &["commit", "--allow-empty", "-m", "dev one"]);
        git(d, &["push"]);
        // move origin/develop ahead of local develop: develop is now behind its upstream
        git(d, &["switch", "-c", "remote-ahead", "develop"]);
        git(d, &["commit", "--allow-empty", "-m", "remote work"]);
        git(d, &["push", "origin", "remote-ahead:develop"]);
        git(d, &["switch", "develop"]);
        git(d, &["branch", "-D", "remote-ahead"]);
        // a release branch already exists
        gitflow_start(d, FlowKind::Release, "0.9.0", "develop").unwrap();

        let check = gitflow_start_checks(d, FlowKind::Release, "1.0.0", "develop").unwrap();
        assert_eq!(
            check.base_behind_upstream.as_ref(),
            Some(&("origin/develop".to_owned(), 1))
        );
        assert!(check.gone_upstream.is_none());
        assert_eq!(check.other_branches, vec!["release/0.9.0".to_owned()]);

        // feature starts don't collect "other branches"
        let check = gitflow_start_checks(d, FlowKind::Feature, "one", "develop").unwrap();
        assert!(check.other_branches.is_empty());
    }

    #[test]
    fn finish_feature_happy_path() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "-b", "master"]);
        git(d, &["commit", "--allow-empty", "-m", "first"]);
        gitflow_init(d, &default_config()).unwrap();
        git(d, &["switch", "develop"]);
        git(d, &["commit", "--allow-empty", "-m", "dev work"]);
        gitflow_start(d, FlowKind::Feature, "one", "develop").unwrap();
        file(d, "one.txt", "feature one\n");
        git(d, &["add", "one.txt"]);
        git(d, &["commit", "-m", "add one"]);
        // develop moves on too, so the merge is a real merge
        git(d, &["switch", "develop"]);
        git(d, &["commit", "--allow-empty", "-m", "more dev work"]);
        // finish runs from the feature branch itself
        git(d, &["switch", "feature/one"]);

        let cancel = Cancellable::default();
        let result = gitflow_finish(
            d,
            &FinishOptions {
                no_fast_forward: true,
                ..Default::default()
            },
            &cancel,
        )
        .unwrap();
        assert_eq!(result, FinishResult::Done);

        assert_eq!(
            out(d, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "develop",
            "finish must leave us on develop"
        );
        // the merge commit has two parents (no-ff)
        assert_eq!(
            out(d, &["rev-list", "--parents", "-n", "1", "HEAD"])
                .split_whitespace()
                .count(),
            3,
            "merge commit + two parents"
        );
        // the feature branch is gone and the state file is removed
        assert!(!out(d, &["branch", "--list"]).contains("feature/one"));
        assert!(!state_file(d).unwrap().exists());
    }

    #[test]
    fn finish_feature_delete_remote_against_bare_origin() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let origin = d.join("origin.git");
        git(d, &["init", "--bare", "-b", "master", "origin.git"]);
        git(d, &["init", "-b", "master"]);
        git(d, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(d, &["commit", "--allow-empty", "-m", "first"]);
        git(d, &["push", "-u", "origin", "master"]);
        gitflow_init(d, &default_config()).unwrap();
        git(d, &["push", "-u", "origin", "develop"]);
        git(d, &["switch", "develop"]);
        git(d, &["commit", "--allow-empty", "-m", "dev work"]);
        gitflow_start(d, FlowKind::Feature, "two", "develop").unwrap();
        file(d, "two.txt", "feature two\n");
        git(d, &["add", "two.txt"]);
        git(d, &["commit", "-m", "add two"]);
        // the feature branch is tracked on origin
        git(d, &["push", "-u", "origin", "feature/two"]);

        let cancel = Cancellable::default();
        let result = gitflow_finish(
            d,
            &FinishOptions {
                delete_remote: true,
                ..Default::default()
            },
            &cancel,
        )
        .unwrap();
        assert_eq!(result, FinishResult::Done);

        // the branch is gone from the bare origin itself (local remote-tracking refs are
        // pruned on the next fetch, not by the push)
        let origin_branches = out(&origin, &["branch", "--list"]);
        assert!(
            !origin_branches.contains("feature/two"),
            "the remote feature branch must be deleted"
        );
        assert!(origin_branches.contains("develop"));
    }

    fn diverging_feature(d: &Path) {
        // develop and the feature branch edit the same file differently
        git(d, &["init", "-b", "master"]);
        git(d, &["commit", "--allow-empty", "-m", "first"]);
        gitflow_init(d, &default_config()).unwrap();
        git(d, &["switch", "develop"]);
        file(d, "shared.txt", "base\n");
        git(d, &["add", "shared.txt"]);
        git(d, &["commit", "-m", "base file"]);
        gitflow_start(d, FlowKind::Feature, "conflict", "develop").unwrap();
        file(d, "shared.txt", "base\nfeature line\n");
        git(d, &["add", "shared.txt"]);
        git(d, &["commit", "-m", "feature edit"]);
        git(d, &["switch", "develop"]);
        file(d, "shared.txt", "base\ndevelop line\n");
        git(d, &["add", "shared.txt"]);
        git(d, &["commit", "-m", "develop edit"]);
        git(d, &["switch", "feature/conflict"]);
    }

    #[test]
    fn finish_feature_rebase_conflict_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        diverging_feature(d);

        let cancel = Cancellable::default();
        let result = gitflow_finish(
            d,
            &FinishOptions {
                rebase_before_merge: true,
                ..Default::default()
            },
            &cancel,
        )
        .unwrap();
        assert_eq!(result, FinishResult::Conflicts { step: 1 });

        // the state file says "re-run step 1"
        let state = gitflow_finish_state(d).unwrap();
        assert_eq!(state.kind, "feature");
        assert_eq!(state.name, "feature/conflict");
        assert_eq!(state.step, 1);
        assert!(state.options.rebase_before_merge);
        assert!(d.join(".git/spork/gitflow-finish.json").is_file());

        // resolve the conflict and let the resume re-run the rebase, then the rest
        git(d, &["checkout", "--theirs", "shared.txt"]);
        git(d, &["add", "shared.txt"]);
        let result = gitflow_resume_finish(d, &cancel).unwrap();
        assert_eq!(result, FinishResult::Done);

        assert!(
            !state_file(d).unwrap().exists(),
            "the state file must be removed"
        );
        assert_eq!(out(d, &["rev-parse", "--abbrev-ref", "HEAD"]), "develop");
        assert!(!out(d, &["branch", "--list"]).contains("feature/conflict"));
    }

    #[test]
    fn finish_feature_merge_conflict_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        diverging_feature(d);

        // no rebase: the conflict happens on the merge (step 3)
        let cancel = Cancellable::default();
        let result = gitflow_finish(d, &FinishOptions::default(), &cancel).unwrap();
        assert_eq!(result, FinishResult::Conflicts { step: 3 });

        let state = gitflow_finish_state(d).unwrap();
        assert_eq!(state.step, 3);
        assert!(!state.options.rebase_before_merge);

        // resolve and resume: merge continues, branch gets deleted
        git(d, &["checkout", "--theirs", "shared.txt"]);
        git(d, &["add", "shared.txt"]);
        let result = gitflow_resume_finish(d, &cancel).unwrap();
        assert_eq!(result, FinishResult::Done);

        assert!(!state_file(d).unwrap().exists());
        assert_eq!(out(d, &["rev-parse", "--abbrev-ref", "HEAD"]), "develop");
        assert_eq!(
            out(d, &["rev-list", "--parents", "-n", "1", "HEAD"])
                .split_whitespace()
                .count(),
            3,
            "the merge commit must have two parents"
        );
    }

    #[test]
    fn resume_without_state_fails() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "-b", "master"]);
        git(d, &["commit", "--allow-empty", "-m", "first"]);
        let cancel = Cancellable::default();
        assert!(gitflow_resume_finish(d, &cancel).is_err());
    }

    #[test]
    fn abort_removes_state_without_touching_the_repo() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        diverging_feature(d);

        let cancel = Cancellable::default();
        let result = gitflow_finish(d, &FinishOptions::default(), &cancel).unwrap();
        assert_eq!(result, FinishResult::Conflicts { step: 3 });
        // abort: drop the state file, the conflicted merge is left as-is
        gitflow_abort_finish(d).unwrap();
        assert!(gitflow_finish_state(d).is_none());
        assert!(matches!(op_state(d).unwrap(), OpState::Merging));
    }

    #[test]
    fn finish_requires_a_feature_branch() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "-b", "master"]);
        git(d, &["commit", "--allow-empty", "-m", "first"]);
        gitflow_init(d, &default_config()).unwrap();
        git(d, &["switch", "develop"]);
        let cancel = Cancellable::default();
        assert!(gitflow_finish(d, &FinishOptions::default(), &cancel).is_err());
        // nothing was started: no state file
        assert!(gitflow_finish_state(d).is_none());
    }
}
