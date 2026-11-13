//! Spork desktop app. P1: repository sidebar (docs/specs/M03) and commit list with graph (M05).

mod askpass;
mod branches;
mod changes;
mod history;
mod operations;
mod rebase;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use changes::ChangeSelection;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::{Diff, DiffEvent, DiffFile, DiffState};
use gpui_kit::component::input::{InputState, TextareaState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::sidebar::{Sidebar, SidebarMenu, SidebarMenuItem};
use gpui_kit::component::{ActiveTheme as _, Theme, ThemeRegistry, TitleBar, h_flex, v_flex};
use gpui_kit::component::{Disableable as _, WindowExt as _};
use gpui_kit::component::{resizable_panel, v_resizable};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use history::{History, ROW_HEIGHT};
use spork_git::{CommitDetails, GitError, OpState, Ref, RefKind, Stash, StatusEntry};

// shortcut: fixed cap until the history loads incrementally (M05 §1 batches), then it becomes a setting.
const MAX_COMMITS: usize = 50_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Changes,
    History,
    /// Interactive rebase plan editor (rebase.rs).
    Rebase,
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
    tag_name: Entity<InputState>,
    tag_message: Entity<InputState>,
    _prompts: Option<Task<()>>,
    description: Entity<TextareaState>,
    amend: bool,
    /// A git write (stage, commit…) is running.
    busy: bool,
    _op: Option<Task<()>>,
    _appearance: Subscription,
}

struct Loaded {
    root: PathBuf,
    head: Option<String>,
    refs: Vec<Ref>,
    status: Vec<StatusEntry>,
    op: OpState,
    stashes: Vec<Stash>,
    history: History,
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
    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
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

const MAX_PATCH_BYTES: usize = 16 * 1024 * 1024;

fn load(path: &Path) -> Result<Loaded, GitError> {
    let root = spork_git::toplevel(path)?;
    let refs = spork_git::refs(&root)?;
    let status = spork_git::status(&root)?;
    let op = spork_git::op_state(&root)?;
    let stashes = spork_git::stashes(&root)?;
    let history = History::new(spork_git::log(&root, MAX_COMMITS)?, &refs);
    Ok(Loaded {
        head: spork_git::head_branch(&root)?,
        refs,
        status,
        op,
        stashes,
        history,
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
                return base.justify_center().child(
                    v_flex()
                        .gap_3()
                        .items_center()
                        .child(div().text_lg().child(e.to_string()))
                        .child(div().text_sm().child(self.requested.display().to_string()))
                        .child(
                            Button::new("retry")
                                .label("Try again")
                                .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                        ),
                );
            }
        };

        let of = |kind| {
            repo.refs
                .iter()
                .filter(move |r: &&Ref| r.kind == kind)
                .collect::<Vec<_>>()
        };
        let stash_items = self.stash_items(&repo.stashes, cx);
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
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(name))
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
        let panel = panel.children(self.op_banner(cx));

        if self.view == View::Rebase {
            let plan = self.rebase_view(cx);
            return base.child(sidebar).child(panel.child(plan));
        }
        if self.view == View::Changes {
            let changes = self.changes_view(cx);
            return base.child(sidebar).child(panel.child(changes));
        }
        if count == 0 {
            return base
                .child(sidebar)
                .child(panel.child(centered("No commits yet".into())));
        }
        let list = v_flex()
            .size_full()
            .child(history::header(lanes, cx))
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
        let mono = theme.mono_font_family.clone();
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

        let summary = v_flex()
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
            );

        let body = if details.patch.len() > MAX_PATCH_BYTES {
            centered("This commit is too large to show inline.".into())
        } else if details.patch.is_empty() {
            centered("No file changes".into())
        } else {
            v_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(Diff::new(&self.diff).size_full())
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

        range
            .map(|ix| {
                let commit = &h.commits[ix];
                let labels = h.labels.get(&commit.id).into_iter().flatten();
                let menu = RepoView::commit_menu(view.clone(), commit.id.clone());
                h_flex()
                    .id(ix)
                    .w_full()
                    .h(px(ROW_HEIGHT))
                    .px_3()
                    .gap_3()
                    .text_sm()
                    .when(self.selected == Some(ix), |row| row.bg(active))
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
