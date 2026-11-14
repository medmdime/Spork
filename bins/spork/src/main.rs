//! Spork desktop app. P1: repository sidebar (docs/specs/M03) and commit list with graph (M05).

mod askpass;
mod blame;
mod branches;
mod changes;
mod file_history;
mod gitflow;
mod history;
mod operations;
mod rebase;
mod repos;
mod search;
mod worktrees;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use changes::ChangeSelection;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::{Diff, DiffEvent, DiffFile, DiffState};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::sidebar::{Sidebar, SidebarMenu, SidebarMenuItem};
use gpui_kit::component::{ActiveTheme as _, Icon, Theme, ThemeRegistry, TitleBar, h_flex, v_flex};
use gpui_kit::component::{Disableable as _, WindowExt as _};
use gpui_kit::component::{resizable_panel, v_resizable};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use history::{History, ROW_HEIGHT};
use spork_git::{
    Commit, CommitDetails, GitError, OpState, Ref, RefKind, SearchQuery, Stash, StatusEntry,
    Submodule, Worktree,
};

// shortcut: fixed cap until the history loads incrementally (M05 §1 batches), then it becomes a setting.
const MAX_COMMITS: usize = 50_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Changes,
    History,
    /// Deep commit search: form + results (docs/specs/M05 §5).
    Search,
    /// Interactive rebase plan editor (rebase.rs).
    Rebase,
    /// File history of one path (M08 §6).
    FileHistory,
    /// Blame of one path at one revision (M08 §5).
    Blame,
}

struct RepoView {
    requested: PathBuf,
    view: View,
    /// `None` until the first load finishes.
    repo: Option<Result<Loaded, GitError>>,
    selected: Option<usize>,
    /// Details of the selected commit; `None` while loading.
    details: Option<Result<CommitDetails, GitError>>,
    diff: Entity<DiffState>,
    _load: Option<Task<()>>,
    _details_load: Option<Task<()>>,
    /// Watches the repository so external changes (terminal, IDE) show up without Refresh.
    _watcher: Option<(notify::RecommendedWatcher, Task<()>)>,
    // working copy (changes.rs)
    change_selected: Option<ChangeSelection>,
    change_diff: Entity<DiffState>,
    /// Raw patch of the open file, for line staging (spork-patch).
    change_patch: Option<String>,
    _diff_events: Subscription,
    _change_diff_load: Option<Task<()>>,
    subject: Entity<InputState>,
    /// Name field of the "New branch" dialog (branches.rs).
    branch_name: Entity<InputState>,
    /// Answer field of git's password / passphrase prompts (askpass.rs).
    prompt_input: Entity<InputState>,
    /// "Stash changes" dialog state (operations.rs).
    stash_message: Entity<InputState>,
    stash_untracked: bool,
    rebase_plan: Option<rebase::RebasePlan>,
    /// Recently opened repositories (repos.rs).
    recent: Vec<PathBuf>,
    clone_url: Entity<InputState>,
    tag_name: Entity<InputState>,
    tag_message: Entity<InputState>,
    _prompts: Option<Task<()>>,
    description: Entity<TextareaState>,
    amend: bool,
    /// A git write (stage, commit…) is running.
    busy: bool,
    _op: Option<Task<()>>,
    _appearance: Subscription,
    // quick filter over the loaded history (M05 §5)
    filter: Option<search::QuickFilter>,
    filter_input: Entity<InputState>,
    _filter_events: Subscription,
    // deep search (M05 §5)
    search: Option<SearchState>,
    search_diff: Entity<DiffState>,
    _search_task: Option<Task<()>>,
    _search_details_load: Option<Task<()>>,
    search_cancel: spork_git::Cancellable,
    search_message: Entity<InputState>,
    search_author: Entity<InputState>,
    search_committer: Entity<InputState>,
    search_content: Entity<InputState>,
    search_path: Entity<InputState>,
    search_since: Entity<InputState>,
    search_until: Entity<InputState>,
    recent_searches: Vec<SearchQuery>,
    // file history (M08 §6)
    file_history: Option<file_history::FileHistory>,
    file_history_diff: Entity<DiffState>,
    file_history_cancel: spork_git::Cancellable,
    /// Destination field of the "Save as…" dialog (file_history.rs).
    file_history_save: Entity<InputState>,
    _file_history_task: Option<Task<()>>,
    _file_history_diff_load: Option<Task<()>>,
    _file_history_content_load: Option<Task<()>>,
    // blame (M08 §5)
    blame: Option<blame::Blame>,
    blame_cancel: spork_git::Cancellable,
    /// In-file search input of the blame view (blame.rs).
    blame_search: Entity<InputState>,
    _blame_task: Option<Task<()>>,
    /// Kept alive so the blame search input keeps notifying (blame.rs).
    _blame_search_events: Subscription,
    // git flow (M13 §1–§3, gitflow.rs)
    gitflow_init_production: Entity<InputState>,
    gitflow_init_development: Entity<InputState>,
    gitflow_init_feature: Entity<InputState>,
    gitflow_init_release: Entity<InputState>,
    gitflow_init_hotfix: Entity<InputState>,
    gitflow_init_bugfix: Entity<InputState>,
    gitflow_init_support: Entity<InputState>,
    gitflow_init_versiontag: Entity<InputState>,
    /// Kills an in-flight git-flow background git call on a repo switch.
    gitflow_cancel: spork_git::Cancellable,
    /// A git-flow init / start / finish / resume is running.
    gitflow_busy: bool,
    _gitflow_task: Option<Task<()>>,
}

struct Loaded {
    root: PathBuf,
    head: Option<String>,
    refs: Vec<Ref>,
    status: Vec<StatusEntry>,
    op: OpState,
    stashes: Vec<Stash>,
    worktrees: Vec<Worktree>,
    submodules: Vec<Submodule>,
    history: History,
    /// Git Flow config of the repository (M13 §1); `None` until initialized.
    gitflow: Option<spork_git::gitflow::GitflowConfig>,
    /// A finish interrupted by conflicts, possibly in a previous session (M13 §3).
    paused_finish: Option<spork_git::gitflow::FinishState>,
}

/// Deep-search state (M05 §5): results replace the history list while the Search view is active.
struct SearchState {
    commits: Vec<Commit>,
    selected: Option<usize>,
    /// Details of the selected result; `None` while loading.
    details: Option<Result<CommitDetails, GitError>>,
    running: bool,
    /// The run was cancelled by the user (rather than failing).
    cancelled: bool,
    error: Option<String>,
}

impl RepoView {
    fn new(
        requested: PathBuf,
        prompts: Option<smol::channel::Receiver<askpass::Prompt>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // follow the OS light/dark setting live
        let _appearance = cx.observe_window_appearance(window, |_, window, cx| {
            Theme::sync_system_appearance(Some(window), cx)
        });
        let change_diff = cx.new(|cx| DiffState::new([], cx));
        // the selection bar above the diff follows the line selection
        let _diff_events = cx.subscribe(&change_diff, |_, _, event: &DiffEvent, cx| {
            if matches!(
                event,
                DiffEvent::SelectionChanged(_)
                    | DiffEvent::SelectionEnded(_)
                    | DiffEvent::ConflictResolved(..)
            ) {
                cx.notify();
            }
        });
        let filter_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Filter by SHA, author, or subject…")
        });
        // quick filter: typing re-filters the loaded rows, Enter / Shift-Enter walk the matches (M05 §5)
        let _filter_events =
            cx.subscribe(
                &filter_input,
                |this, _, event: &InputEvent, cx| match event {
                    InputEvent::Change => this.refilter(cx),
                    InputEvent::PressEnter { shift, .. } => this.step_filter(*shift, cx),
                    InputEvent::Focus | InputEvent::Blur => {}
                },
            );
        // blame in-file search (M08 §5): typing re-highlights the matching lines
        let blame_search =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search in the file text…"));
        let _blame_search_events =
            cx.subscribe(&blame_search, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) && this.view == View::Blame {
                    cx.notify();
                }
            });
        let mut view = Self {
            requested,
            view: View::History,
            repo: None,
            selected: None,
            details: None,
            diff: cx.new(|cx| DiffState::new([], cx)),
            _load: None,
            _details_load: None,
            _watcher: None,
            change_selected: None,
            change_diff,
            _change_diff_load: None,
            change_patch: None,
            _diff_events,
            subject: cx.new(|cx| InputState::new(window, cx).placeholder("Commit subject")),
            branch_name: cx.new(|cx| InputState::new(window, cx).placeholder("feature/my-change")),
            prompt_input: cx.new(|cx| InputState::new(window, cx)),
            stash_message: cx
                .new(|cx| InputState::new(window, cx).placeholder("Message (optional)")),
            stash_untracked: true,
            rebase_plan: None,
            recent: repos::recent(),
            clone_url: cx.new(|cx| {
                InputState::new(window, cx).placeholder("https://github.com/org/repo.git")
            }),
            tag_name: cx.new(|cx| InputState::new(window, cx).placeholder("v1.2.0")),
            tag_message: cx.new(|cx| InputState::new(window, cx).placeholder("Message (optional)")),
            _prompts: prompts.map(|rx| {
                cx.spawn_in(window, async move |this, cx| {
                    while let Ok((prompt, reply)) = rx.recv().await {
                        this.update_in(cx, |this, window, cx| this.ask(prompt, reply, window, cx))
                            .ok();
                    }
                })
            }),
            description: cx
                .new(|cx| TextareaState::new(window, cx).placeholder("Description (optional)")),
            amend: false,
            busy: false,
            _op: None,
            _appearance,
            filter: None,
            filter_input,
            _filter_events,
            search: None,
            search_diff: cx.new(|cx| DiffState::new([], cx)),
            _search_task: None,
            _search_details_load: None,
            search_cancel: spork_git::Cancellable::default(),
            search_message: cx
                .new(|cx| InputState::new(window, cx).placeholder("Text in the commit message")),
            search_author: cx
                .new(|cx| InputState::new(window, cx).placeholder("Name or email (regex)")),
            search_committer: cx
                .new(|cx| InputState::new(window, cx).placeholder("Name or email (regex)")),
            search_content: cx
                .new(|cx| InputState::new(window, cx).placeholder("Pickaxe regex over the diff")),
            search_path: cx.new(|cx| InputState::new(window, cx).placeholder("e.g. src/main.rs")),
            search_since: cx.new(|cx| InputState::new(window, cx).placeholder("e.g. 2024-01-01")),
            search_until: cx.new(|cx| InputState::new(window, cx).placeholder("e.g. 2024-06-30")),
            recent_searches: search::recent_searches(),
            file_history: None,
            file_history_diff: cx.new(|cx| DiffState::new([], cx)),
            file_history_cancel: spork_git::Cancellable::default(),
            file_history_save: cx.new(|cx| InputState::new(window, cx)),
            _file_history_task: None,
            _file_history_diff_load: None,
            _file_history_content_load: None,
            blame: None,
            blame_cancel: spork_git::Cancellable::default(),
            blame_search,
            _blame_task: None,
            _blame_search_events,
            gitflow_init_production: cx.new(|cx| InputState::new(window, cx).placeholder("main")),
            gitflow_init_development: cx
                .new(|cx| InputState::new(window, cx).placeholder("develop")),
            gitflow_init_feature: cx.new(|cx| InputState::new(window, cx).placeholder("feature/")),
            gitflow_init_release: cx.new(|cx| InputState::new(window, cx).placeholder("release/")),
            gitflow_init_hotfix: cx.new(|cx| InputState::new(window, cx).placeholder("hotfix/")),
            gitflow_init_bugfix: cx.new(|cx| InputState::new(window, cx).placeholder("bugfix/")),
            gitflow_init_support: cx.new(|cx| InputState::new(window, cx).placeholder("support/")),
            gitflow_init_versiontag: cx.new(|cx| InputState::new(window, cx).placeholder("")),
            gitflow_cancel: spork_git::Cancellable::default(),
            gitflow_busy: false,
            _gitflow_task: None,
        };
        view.reload(cx);
        view
    }

    /// Reads the repository on a background thread; the current view stays until the new data is in.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let path = self.requested.clone();
        self._load = Some(cx.spawn(async move |this, cx| {
            let loaded = cx.background_spawn(async move { load(&path) }).await;
            this.update(cx, |this, cx| {
                // keep the same commit selected even if new commits shifted the rows
                let selected_id = this.selected_commit_id();
                if let Ok(repo) = &loaded {
                    repos::remember(&repo.root);
                    this.recent = repos::recent();
                }
                this.repo = Some(loaded);
                if let Some(Ok(repo)) = &this.repo {
                    let root = repo.root.clone();
                    let ix = selected_id
                        .and_then(|id| repo.history.commits.iter().position(|c| c.id == id));
                    if ix.is_none() {
                        this.selected = None;
                        this.details = None;
                    }
                    this.selected = ix;
                    if this._watcher.is_none() {
                        this.watch(root, cx);
                    }
                    // a quick filter stays open across reloads: re-map it onto the new rows
                    if this.filter.is_some() {
                        this.refilter(cx);
                    }
                }
                // refresh the open file diff (or clear it if the file is no longer changed)
                this.select_change(this.change_selected.clone(), cx);
                cx.notify();
            })
            .ok();
        }));
    }

    fn selected_commit_id(&self) -> Option<String> {
        let Some(Ok(repo)) = &self.repo else {
            return None;
        };
        Some(repo.history.commits.get(self.selected?)?.id.clone())
    }

    /// Reloads when files in the work tree or `.git` change, after 300 ms of quiet (one reload per burst).
    // shortcut: any change reloads everything; split refs / index / work-tree refreshes when status lands (P2).
    fn watch(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        use notify::{EventKind, RecursiveMode, Watcher as _};
        let (tx, rx) = smol::channel::unbounded::<()>();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            // reading files (including our own git reads) must not trigger reloads
            if event.is_ok_and(|e| !matches!(e.kind, EventKind::Access(_))) {
                tx.try_send(()).ok();
            }
        });
        let Ok(mut watcher) = watcher else { return };
        if watcher.watch(&root, RecursiveMode::Recursive).is_err() {
            return;
        }
        let task = cx.spawn(async move |this, cx| {
            while rx.recv().await.is_ok() {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(300))
                        .await;
                    if rx.is_empty() {
                        break;
                    }
                    while rx.try_recv().is_ok() {}
                }
                if this.update(cx, |this, cx| this.reload(cx)).is_err() {
                    break;
                }
            }
        });
        self._watcher = Some((watcher, task));
    }
}

impl RepoView {
    /// Selects a commit and loads its details + patch in the background (M06).
    pub(crate) fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(Ok(repo)) = &self.repo else { return };
        let Some(commit) = repo.history.commits.get(ix) else {
            return;
        };
        self.selected = Some(ix);
        self.details = None;
        let (root, id) = (repo.root.clone(), commit.id.clone());
        let parent = commit.parents.first().cloned();
        // replacing the task drops (cancels) a load for a previously clicked commit
        self._details_load = Some(cx.spawn(async move |this, cx| {
            let details = cx
                .background_spawn(async move {
                    spork_git::commit_details(&root, &id, parent.as_deref())
                })
                .await;
            this.update(cx, |this, cx| {
                // shortcut: whole-patch parse; huge commits get a per-file lazy diff in M08
                let files = match &details {
                    Ok(d) if d.patch.len() <= MAX_PATCH_BYTES => {
                        DiffFile::parse(&d.patch).unwrap_or_default()
                    }
                    _ => Vec::new(),
                };
                this.diff.update(cx, |state, cx| state.set_files(files, cx));
                this.details = Some(details);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

#[derive(Clone, Copy)]
enum RemoteOp {
    Fetch,
    Pull,
    Push,
}

impl RepoView {
    /// Fetch / Pull / Push in the background; the outcome is a notification (docs/specs/M09).
    fn remote_op(&mut self, op: RemoteOp, window: &mut Window, cx: &mut Context<Self>) {
        self.run_op(
            window,
            cx,
            move |root| match op {
                RemoteOp::Fetch => {
                    spork_git::fetch(root).map(|()| "Fetched all remotes".to_owned())
                }
                RemoteOp::Pull => spork_git::pull(root).map(|()| "Pulled".to_owned()),
                RemoteOp::Push => spork_git::push(root),
            },
            |_, message, window, cx| window.push_notification(Notification::success(message), cx),
        );
    }

    fn remote_button(&self, op: RemoteOp, cx: &mut Context<Self>) -> Button {
        let (id, icon, tip) = match op {
            RemoteOp::Fetch => ("fetch", IconName::CloudDownload, "Fetch all remotes"),
            RemoteOp::Pull => ("pull", IconName::ArrowDown, "Pull"),
            RemoteOp::Push => ("push", IconName::ArrowUp, "Push"),
        };
        Button::new(id)
            .ghost()
            .icon(icon)
            .tooltip(tip)
            .disabled(self.busy)
            .on_click(cx.listener(move |this, _, window, cx| this.remote_op(op, window, cx)))
    }
}

pub(crate) const MAX_PATCH_BYTES: usize = 16 * 1024 * 1024;

fn load(path: &Path) -> Result<Loaded, GitError> {
    let root = spork_git::toplevel(path)?;
    let refs = spork_git::refs(&root)?;
    let status = spork_git::status(&root)?;
    let op = spork_git::op_state(&root)?;
    let stashes = spork_git::stashes(&root)?;
    let worktrees = spork_git::worktrees(&root)?;
    let submodules = spork_git::submodules(&root)?;
    let history = History::new(spork_git::log(&root, MAX_COMMITS)?, &refs);
    Ok(Loaded {
        head: spork_git::head_branch(&root)?,
        refs,
        status,
        op,
        stashes,
        worktrees,
        submodules,
        history,
        // best effort: a missing / broken config or state file just hides the menu entry / banner
        gitflow: spork_git::gitflow::gitflow_config(&root),
        paused_finish: spork_git::gitflow::gitflow_finish_state(&root),
        root,
    })
}

fn branch_item(r: &Ref, view: &Entity<RepoView>) -> SidebarMenuItem {
    let label = r
        .name
        .rsplit_once('/')
        .map_or(r.name.as_str(), |(_, leaf)| leaf)
        .to_owned();
    let badge = match (r.ahead, r.behind, r.upstream_gone) {
        (_, _, true) => "gone".to_owned(),
        (0, 0, _) => String::new(),
        (a, 0, _) => format!("↑{a}"),
        (0, b, _) => format!("↓{b}"),
        (a, b, _) => format!("↑{a} ↓{b}"),
    };
    let item = SidebarMenuItem::new(label)
        .icon(IconName::GitBranch)
        .active(r.is_head)
        .context_menu(branches::ref_menu(r, view.clone()));
    if badge.is_empty() {
        item
    } else {
        item.suffix(move |_, _| div().text_xs().child(badge.clone()))
    }
}

/// Groups `a/b/c` names into folders, as in M03 §1 (`feature/login` → `feature ▸ login`).
fn folder_tree(refs: &[&Ref], depth: usize, view: &Entity<RepoView>) -> Vec<SidebarMenuItem> {
    let mut folders: BTreeMap<&str, Vec<&Ref>> = BTreeMap::new();
    let mut leaves = Vec::new();
    for r in refs {
        match r.name.split('/').nth(depth) {
            Some(seg) if r.name.split('/').count() > depth + 1 => {
                folders.entry(seg).or_default().push(r)
            }
            _ => leaves.push(branch_item(r, view)),
        }
    }
    let mut items: Vec<SidebarMenuItem> = folders
        .into_iter()
        .map(|(name, inner)| {
            SidebarMenuItem::new(name.to_owned())
                .icon(IconName::Folder)
                .click_to_toggle(true)
                .children(folder_tree(&inner, depth + 1, view))
        })
        .collect();
    items.extend(leaves);
    items
}

fn section(
    title: &str,
    count: usize,
    icon: IconName,
    children: Vec<SidebarMenuItem>,
) -> SidebarMenuItem {
    SidebarMenuItem::new(format!("{title} ({count})"))
        .icon(icon)
        .default_open(true)
        .click_to_toggle(true)
        .children(children)
}

/// The commit summary block shared by the history details pane and the search results pane.
pub(crate) fn commit_summary(
    commit: &Commit,
    details: &CommitDetails,
    cx: &mut Context<RepoView>,
) -> Div {
    let muted = cx.theme().muted_foreground;
    let mono = cx.theme().mono_font_family.clone();
    let (_, body) = details
        .message
        .split_once("\n\n")
        .unwrap_or((&details.message, ""));
    let mut who = format!(
        "{} <{}> · {}",
        commit.author, details.author_email, details.author_date
    );
    if details.committer != commit.author {
        who.push_str(&format!(
            "   committed by {} · {}",
            details.committer, details.committer_date
        ));
    }
    let short = |id: &String| id.chars().take(7).collect::<String>();
    let parents = commit
        .parents
        .iter()
        .map(short)
        .collect::<Vec<_>>()
        .join(", ");
    v_flex()
        .flex_none()
        .max_h(px(160.))
        .overflow_hidden()
        .px_4()
        .py_3()
        .gap_1()
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child(commit.subject.clone()),
        )
        .when(!body.trim().is_empty(), |d| {
            d.child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(body.trim().to_owned()),
            )
        })
        .child(div().text_xs().text_color(muted).child(who))
        .child(
            h_flex()
                .gap_3()
                .text_xs()
                .text_color(muted)
                .font_family(mono)
                .child(commit.id.clone())
                .when(!parents.is_empty(), |d| {
                    d.child(format!("parents {parents}"))
                }),
        )
}

impl Render for RepoView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        // window frame = panel gray; the main area is a rounded "island" on top of it
        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .text_color(theme.foreground)
            .child(TitleBar::new().child(div().text_sm().child("Spork")))
            .child(self.body(cx).flex_1().min_h_0())
            // Cmd-F opens the quick filter bar in the history view (M05 §5)
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| {
                let m = &e.keystroke.modifiers;
                if m.platform && !m.shift && !m.control && !m.alt && e.keystroke.key == "f" {
                    this.open_filter(window, cx);
                }
            }))
    }
}

impl RepoView {
    fn body(&mut self, cx: &mut Context<Self>) -> Div {
        let base = h_flex().w_full();
        let centered = |msg: String| {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(div().text_sm().child(msg))
        };

        let repo = match &self.repo {
            None => return base.child(centered("Loading repository…".into())),
            Some(Ok(repo)) => repo,
            Some(Err(e)) => {
                let problem = match e {
                    GitError::NotARepo => None,
                    other => Some(format!("{}: {other}", self.requested.display())),
                };
                return base.child(self.welcome(problem, cx));
            }
        };

        let of = |kind| {
            repo.refs
                .iter()
                .filter(move |r: &&Ref| r.kind == kind)
                .collect::<Vec<_>>()
        };
        let stash_items = self.stash_items(&repo.stashes, cx);
        let worktree_items = self.worktree_items(&repo.worktrees, cx);
        let submodule_items = self.submodule_items(&repo.submodules, cx);
        let view = cx.entity();
        let locals = of(RefKind::Local);
        let remotes = of(RefKind::Remote);
        let tags: Vec<_> = of(RefKind::Tag)
            .iter()
            .map(|t| {
                SidebarMenuItem::new(t.name.clone())
                    .icon(IconName::Tag)
                    .context_menu(branches::ref_menu(t, view.clone()))
            })
            .collect();
        let name = repo.root.file_name().map_or_else(
            || repo.root.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let head = repo
            .head
            .clone()
            .unwrap_or_else(|| "HEAD (detached)".into());

        let sidebar = Sidebar::new("refs")
            .collapsible(false)
            .w(px(280.))
            .header(
                h_flex()
                    .w_full()
                    .justify_between()
                    .child(
                        v_flex()
                            .child(self.repo_switcher(name, cx))
                            .child(div().text_xs().child(head)),
                    )
                    .child(
                        h_flex()
                            .child(self.stash_button(cx))
                            .child(self.remote_button(RemoteOp::Fetch, cx))
                            .child(self.remote_button(RemoteOp::Pull, cx))
                            .child(self.remote_button(RemoteOp::Push, cx)),
                    ),
            )
            .child(
                SidebarMenu::new().children([
                    SidebarMenuItem::new(format!("Local Changes ({})", repo.status.len()))
                        .icon(IconName::FilePen)
                        .active(self.view == View::Changes)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.view = View::Changes;
                            cx.notify();
                        })),
                    SidebarMenuItem::new("All Commits")
                        .icon(IconName::GitCommitHorizontal)
                        .active(self.view == View::History)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.view = View::History;
                            cx.notify();
                        })),
                    SidebarMenuItem::new("Search")
                        .icon(IconName::Search)
                        .active(self.view == View::Search)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.view = View::Search;
                            cx.notify();
                        })),
                    section(
                        "Branches",
                        locals.len(),
                        IconName::GitBranch,
                        folder_tree(&locals, 0, &view),
                    ),
                    section(
                        "Remotes",
                        remotes.len(),
                        IconName::Globe,
                        folder_tree(&remotes, 0, &view),
                    ),
                    section("Tags", tags.len(), IconName::Tag, tags),
                    section("Stashes", stash_items.len(), IconName::Archive, stash_items),
                    section(
                        "Worktrees",
                        worktree_items.len(),
                        IconName::FolderGit2,
                        worktree_items,
                    ),
                    section(
                        "Submodules",
                        submodule_items.len(),
                        IconName::Package,
                        submodule_items,
                    ),
                ]),
            );

        let count = repo.history.commits.len();
        let lanes = repo.history.graph_lanes;
        let theme = cx.theme();
        let panel = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .mr_2()
            .mb_2()
            .bg(theme.background)
            .rounded(theme.radius)
            .overflow_hidden();
        let panel = panel
            .children(self.op_banner(cx))
            .children(self.gitflow_finish_banner(cx));

        if self.view == View::Rebase {
            let plan = self.rebase_view(cx);
            return base.child(sidebar).child(panel.child(plan));
        }
        if self.view == View::Changes {
            let changes = self.changes_view(cx);
            return base.child(sidebar).child(panel.child(changes));
        }
        if self.view == View::Search {
            let search = self.search_view(cx);
            return base.child(sidebar).child(panel.child(search));
        }
        if self.view == View::FileHistory {
            let history = self.file_history_view(cx);
            return base.child(sidebar).child(panel.child(history));
        }
        if self.view == View::Blame {
            let blame = self.blame_view(cx);
            return base.child(sidebar).child(panel.child(blame));
        }
        if count == 0 {
            return base
                .child(sidebar)
                .child(panel.child(centered("No commits yet".into())));
        }
        let list = v_flex()
            .size_full()
            .child(history::header(lanes, cx))
            // the quick filter bar sits above the list (M05 §5)
            .when(self.filter.is_some(), |list| {
                list.child(self.filter_bar(cx))
            })
            .child(
                uniform_list(
                    "history",
                    count,
                    cx.processor(|this, range, _, cx| this.history_rows(range, cx)),
                )
                .flex_1()
                .min_h_0(),
            );
        let panel = if self.selected.is_some() {
            panel.child(
                v_resizable("history-split")
                    .child(resizable_panel().child(list))
                    .child(
                        resizable_panel()
                            .size(px(380.))
                            .child(self.details_view(cx)),
                    ),
            )
        } else {
            panel.child(list)
        };
        base.child(sidebar).child(panel)
    }

    fn details_view(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let pane = v_flex().size_full().border_t_1().border_color(border);
        let centered = |msg: String| {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(muted)
                .child(msg)
        };
        let (Some(ix), Some(Ok(repo))) = (self.selected, &self.repo) else {
            return pane;
        };
        let commit = &repo.history.commits[ix];
        let details = match &self.details {
            None => return pane.child(centered("Loading commit…".into())),
            Some(Err(e)) => return pane.child(centered(e.to_string())),
            Some(Ok(d)) => d,
        };

        let summary = commit_summary(commit, details, cx);

        let body = if details.patch.len() > MAX_PATCH_BYTES {
            centered("This commit is too large to show inline.".into())
        } else if details.patch.is_empty() {
            centered("No file changes".into())
        } else {
            // per-file "File history" and "Blame" entries in the diff header (M08 §6, M08 §5)
            let view = cx.entity();
            // the diff header shows the path as it exists at this commit, so
            // both entries blame / follow the file at exactly this commit
            let (oid, short) = (
                commit.id.clone(),
                commit.id.chars().take(7).collect::<String>(),
            );
            v_flex().flex_1().min_h_0().w_full().child(
                Diff::new(&self.diff).size_full().render_header_suffix(
                    move |document, window, _| {
                        let view = view.clone();
                        let path = document.path().to_string();
                        // per-call copies: the Blame entry's listener moves them
                        // (the `oid` / `short` captures can only be shared, and
                        // the first listener closure already captures `path`)
                        let (oid, short) = (oid.clone(), short.clone());
                        let blame_path = path.clone();
                        let click = window
                            .listener_for::<RepoView, ClickEvent>(&view, move |this, _, _, cx| {
                                this.start_file_history(path.clone(), cx)
                            });
                        let blame_click = window.listener_for::<RepoView, ClickEvent>(
                            &view,
                            move |this, _, _, cx| {
                                this.start_blame(
                                    blame_path.clone(),
                                    oid.clone(),
                                    short.clone(),
                                    View::History,
                                    cx,
                                )
                            },
                        );
                        div().text_sm().child(
                            h_flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    div()
                                        .id("file-history-details-entry")
                                        .on_click(click)
                                        .cursor_pointer()
                                        .text_color(muted)
                                        .child(
                                            h_flex()
                                                .items_center()
                                                .gap_0p5()
                                                .child(
                                                    Icon::new(IconName::Calendar)
                                                        .size(px(14.))
                                                        .text_color(muted),
                                                )
                                                .child("File history"),
                                        ),
                                )
                                .child(div().text_color(muted).child("·"))
                                .child(
                                    div()
                                        .id("blame-details-entry")
                                        .on_click(blame_click)
                                        .cursor_pointer()
                                        .text_color(muted)
                                        .child(
                                            h_flex()
                                                .items_center()
                                                .gap_0p5()
                                                .child(
                                                    Icon::new(IconName::GitCommitHorizontal)
                                                        .size(px(14.))
                                                        .text_color(muted),
                                                )
                                                .child("Blame"),
                                        ),
                                ),
                        )
                    },
                ),
            )
        };
        pane.child(summary).child(body)
    }

    fn history_rows(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(Ok(repo)) = &self.repo else {
            return Vec::new();
        };
        let h = &repo.history;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let view = cx.entity();
        let theme = cx.theme();
        let (muted, hover, active) = (theme.muted_foreground, theme.list_hover, theme.list_active);
        let mono = theme.mono_font_family.clone();
        let filter = self.filter.as_ref().filter(|f| !f.query.trim().is_empty());

        range
            .map(|ix| {
                let commit = &h.commits[ix];
                let labels = h.labels.get(&commit.id).into_iter().flatten();
                let menu = RepoView::commit_menu(view.clone(), commit.id.clone());
                // a filtered-out row is dimmed; the current match takes the active color (M05 §5)
                let highlight = filter.and_then(|f| f.highlight(ix));
                h_flex()
                    .id(ix)
                    .w_full()
                    .h(px(ROW_HEIGHT))
                    .px_3()
                    .gap_3()
                    .text_sm()
                    .when(self.selected == Some(ix), |row| row.bg(active))
                    .when(highlight == Some(false), |row| row.bg(hover))
                    .when(highlight == Some(true), |row| row.bg(active))
                    .opacity(if filter.is_some() && highlight.is_none() {
                        0.35
                    } else {
                        1.0
                    })
                    .hover(move |row| row.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.select(ix, cx)))
                    .child(history::graph_cell(
                        &h.rows[ix],
                        commit.parents.len() > 1,
                        h.graph_lanes,
                        cx,
                    ))
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1p5()
                            .children(labels.map(|l| history::label_badge(l, cx)))
                            .child(div().min_w_0().truncate().child(commit.subject.clone())),
                    )
                    .child(
                        div()
                            .w(px(150.))
                            .flex_none()
                            .truncate()
                            .text_color(muted)
                            .child(commit.author.clone()),
                    )
                    .child(
                        div()
                            .w(px(50.))
                            .flex_none()
                            .text_color(muted)
                            .child(history::relative_age(commit.time, now)),
                    )
                    .child(
                        div()
                            .w(px(64.))
                            .flex_none()
                            .font_family(mono.clone())
                            .text_xs()
                            .text_color(muted)
                            .child(commit.id.chars().take(7).collect::<String>()),
                    )
                    .context_menu(menu)
                    .into_any_element()
            })
            .collect()
    }
}

/// Installs Spork's themes (JetBrains New UI palette) as the app's light and dark modes.
fn install_theme(cx: &mut App) {
    let registry = ThemeRegistry::global_mut(cx);
    registry
        .load_themes_from_str(include_str!("../assets/themes/spork.json"))
        .expect("bundled theme is valid JSON");
    let themes = registry.themes();
    let light = themes["Spork Light"].clone();
    let dark = themes["Spork Dark"].clone();
    Theme::update(cx, |theme| {
        theme.light_theme = light;
        theme.dark_theme = dark;
    });
}

fn main() {
    // git runs this same binary as GIT_ASKPASS / SSH_ASKPASS: answer and exit, no UI
    if std::env::var_os(spork_git::askpass::HELPER_ENV).is_some() {
        std::process::exit(spork_git::askpass::helper_main());
    }
    let requested = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().expect("current directory"));

    // shortcut: embeds the full Lucide icon set; switch to a trimmed asset source when binary size matters (P7).
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            install_theme(cx);
            // app-drawn title bar, so it matches the theme (and draws window buttons on Linux/Windows)
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1100.), px(720.)), cx)),
                ..TitleBar::window_options()
            };
            // open_window wraps the view in gpui-kit's Root (themes, dialogs, notifications)
            gpui_kit::open_window(options, cx, move |window, cx| {
                Theme::sync_system_appearance(Some(window), cx);
                let prompts = askpass::start();
                cx.new(|cx| RepoView::new(requested, prompts, window, cx))
            })
            .expect("failed to open window");
            cx.activate(true);
        });
}
