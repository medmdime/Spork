//! Git Flow: the "Git Flow" submenu of the repository switcher plus the
//! Initialize / Start / Finish Feature dialogs and the paused-finish banner
//! (M13 §1–§3, feature only). All git calls run on a background task behind
//! `self.gitflow_cancel`, like the search load.

use gpui_kit::component::IndexPath;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::select::{SearchableVec, Select, SelectState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use spork_git::gitflow::{
    FinishOptions, FinishResult, FlowKind, GitflowConfig, StartCheck, finish_step_label,
    gitflow_abort_finish, gitflow_finish, gitflow_init, gitflow_resume_finish, gitflow_start,
    gitflow_start_checks, latest_release_branch,
};

/// True when `branch` carries the configured prefix for `kind` (M13 §3): the
/// Finish actions are offered only from a branch of their own kind.
pub fn is_flow_branch(branch: &str, kind: FlowKind, config: &GitflowConfig) -> bool {
    branch.starts_with(kind.prefix(config))
}

/// True when `branch` carries the configured feature prefix (M13 §3): Finish
/// Feature is offered only from a feature branch.
pub fn is_feature_branch(branch: &str, config: &GitflowConfig) -> bool {
    is_flow_branch(branch, FlowKind::Feature, config)
}

/// "Start Feature / Release / Hotfix" dialog (M13 §2): the branch
/// name-or-version, the base branch, and the non-blocking start-check notes.
pub struct GitflowStartDialog {
    kind: FlowKind,
    name: Entity<InputState>,
    base: Entity<SelectState<SearchableVec<String>>>,
    notes: Vec<String>,
}

impl GitflowStartDialog {
    fn new(
        kind: FlowKind,
        branches: Vec<String>,
        default_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| match kind {
            FlowKind::Feature => InputState::new(window, cx).placeholder("e.g. login-form"),
            FlowKind::Release => InputState::new(window, cx).placeholder("e.g. 1.2.0"),
            FlowKind::Hotfix => InputState::new(window, cx).placeholder("e.g. 1.2.1"),
        });
        let base = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(branches),
                Some(IndexPath::new(default_ix)),
                window,
                cx,
            )
        });
        Self {
            kind,
            name,
            base,
            notes: Vec::new(),
        }
    }

    pub fn name(&self, cx: &App) -> String {
        self.name.read(cx).value().trim().to_owned()
    }

    pub fn base(&self, cx: &App) -> String {
        self.base
            .read(cx)
            .selected_value()
            .cloned()
            .unwrap_or_default()
    }

    fn set_notes(&mut self, notes: Vec<String>) {
        self.notes = notes;
    }
}

impl Render for GitflowStartDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let hint = match self.kind {
            FlowKind::Feature => "Name of the feature branch — the prefix is added automatically.",
            FlowKind::Release => "Version the release branch is for.",
            FlowKind::Hotfix => "Version the hotfix branch is for.",
        };
        v_flex()
            .gap_2()
            .child(div().text_sm().text_color(muted).child(hint))
            .child(Input::new(&self.name))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().child("Base branch"))
                    .child(Select::new(&self.base)),
            )
            .children(
                self.notes
                    .iter()
                    .map(|note| div().text_xs().text_color(muted).child(note.clone())),
            )
    }
}

/// "Finish Feature / Release / Hotfix" dialog (M13 §3): the per-run finish
/// options. Feature exposes rebase / no-ff / delete (+remote); release and
/// hotfix expose the tag message plus delete / push / no back-merge / sign.
pub struct GitflowFinishDialog {
    kind: FlowKind,
    options: FinishOptions,
    /// Release/hotfix only: the tag message field, prefilled with
    /// `<versiontag><version>`.
    tag_message: Option<Entity<InputState>>,
    /// Hotfix only: a non-blocking note about the back-merge target (the newest
    /// release branch, when one exists).
    note: Option<String>,
}

impl GitflowFinishDialog {
    fn new(
        kind: FlowKind,
        tag_default: String,
        tag_placeholder: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let tag_message = match kind {
            FlowKind::Feature => None,
            FlowKind::Release | FlowKind::Hotfix => Some(cx.new(|cx| {
                let mut input = InputState::new(window, cx).placeholder(tag_placeholder.clone());
                if !tag_default.is_empty() {
                    input.set_value(tag_default.clone(), window, cx);
                }
                input
            })),
        };
        Self {
            kind,
            options: FinishOptions::default(),
            tag_message,
            note: None,
        }
    }

    /// The selected finish options, including the tag message (release/hotfix).
    pub fn options(&self, cx: &App) -> FinishOptions {
        let mut options = self.options.clone();
        if let Some(input) = &self.tag_message {
            options.tag_message = input.read(cx).value().to_string();
        }
        options
    }

    fn set_note(&mut self, note: Option<String>) {
        self.note = note;
    }
}

impl Render for GitflowFinishDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let mut body = v_flex().gap_2();

        // Release/hotfix: the tag message field above the checkboxes.
        if let Some(input) = &self.tag_message {
            body = body.child(Input::new(input));
        }

        let view = view.clone();
        match self.kind {
            FlowKind::Feature => {
                body = body
                    .child({
                        let view = view.clone();
                        Checkbox::new("gitflow-finish-rebase")
                            .label("Rebase before merging")
                            .checked(self.options.rebase_before_merge)
                            .on_click(move |checked, _, cx| {
                                view.update(cx, |d, cx| {
                                    d.options.rebase_before_merge = *checked;
                                    cx.notify();
                                });
                            })
                    })
                    .child({
                        let view = view.clone();
                        Checkbox::new("gitflow-finish-no-ff")
                            .label("No fast-forward")
                            .checked(self.options.no_fast_forward)
                            .on_click(move |checked, _, cx| {
                                view.update(cx, |d, cx| {
                                    d.options.no_fast_forward = *checked;
                                    cx.notify();
                                });
                            })
                    })
                    .child({
                        let view = view.clone();
                        Checkbox::new("gitflow-finish-delete-branch")
                            .label("Delete branch")
                            .checked(self.options.delete_branch)
                            .on_click(move |checked, _, cx| {
                                view.update(cx, |d, cx| {
                                    d.options.delete_branch = *checked;
                                    cx.notify();
                                });
                            })
                    })
                    .child({
                        let view = view;
                        Checkbox::new("gitflow-finish-delete-remote")
                            .label("Delete remote branch")
                            .checked(self.options.delete_remote)
                            .on_click(move |checked, _, cx| {
                                view.update(cx, |d, cx| {
                                    d.options.delete_remote = *checked;
                                    cx.notify();
                                });
                            })
                    });
            }
            FlowKind::Release | FlowKind::Hotfix => {
                body = body
                    .child({
                        let view = view.clone();
                        Checkbox::new("gitflow-finish-delete-branch")
                            .label("Delete branch")
                            .checked(self.options.delete_branch)
                            .on_click(move |checked, _, cx| {
                                view.update(cx, |d, cx| {
                                    d.options.delete_branch = *checked;
                                    cx.notify();
                                });
                            })
                    })
                    .child({
                        let view = view.clone();
                        Checkbox::new("gitflow-finish-push")
                            .label("Push after finish")
                            .checked(self.options.push_after)
                            .on_click(move |checked, _, cx| {
                                view.update(cx, |d, cx| {
                                    d.options.push_after = *checked;
                                    cx.notify();
                                });
                            })
                    })
                    .child({
                        let view = view.clone();
                        Checkbox::new("gitflow-finish-no-back-merge")
                            .label("No back-merge")
                            .checked(self.options.no_back_merge)
                            .on_click(move |checked, _, cx| {
                                view.update(cx, |d, cx| {
                                    d.options.no_back_merge = *checked;
                                    cx.notify();
                                });
                            })
                    })
                    .child({
                        let view = view;
                        Checkbox::new("gitflow-finish-sign-tag")
                            .label("Sign tag")
                            .checked(self.options.sign_tag)
                            .on_click(move |checked, _, cx| {
                                view.update(cx, |d, cx| {
                                    d.options.sign_tag = *checked;
                                    cx.notify();
                                });
                            })
                    });
            }
        }

        // Hotfix: a non-blocking note about the back-merge target.
        if let Some(note) = &self.note {
            let muted = cx.theme().muted_foreground;
            body = body.child(div().text_xs().text_color(muted).child(note.clone()));
        }

        body
    }
}

impl crate::RepoView {
    /// Snapshot of (git-flow config, current branch, default branch) for the
    /// "Git Flow" submenu (M13 §1–§4).
    pub fn gitflow_menu_state(&self) -> (Option<GitflowConfig>, Option<String>, String) {
        match &self.repo {
            Some(Ok(repo)) => (
                repo.gitflow.clone(),
                repo.head.clone(),
                repo.default_branch.clone(),
            ),
            _ => (None, None, "main".to_owned()),
        }
    }
}

/// Builds the "Git Flow" submenu of the repository switcher (M13 §1–§4).
pub fn gitflow_submenu(
    menu: PopupMenu,
    config: Option<GitflowConfig>,
    current: Option<String>,
    default_branch: String,
    view: Entity<crate::RepoView>,
) -> PopupMenu {
    let item =
        |label: &str,
         enabled: bool,
         f: fn(&mut crate::RepoView, &mut Window, &mut Context<crate::RepoView>)| {
            let view = view.clone();
            PopupMenuItem::new(label.to_owned())
                .on_click(move |_, window, cx| view.update(cx, |this, cx| f(this, window, cx)))
                .when(!enabled, |i| i.disabled(true))
        };
    match &config {
        None => menu.item(item(
            "Initialize…",
            true,
            crate::RepoView::gitflow_init_dialog,
        )),
        Some(cfg) => {
            let current = current.as_deref().unwrap_or_default();
            let on_feature = is_feature_branch(current, cfg);
            let on_release = is_flow_branch(current, FlowKind::Release, cfg);
            let on_hotfix = is_flow_branch(current, FlowKind::Hotfix, cfg);
            // Lean "Finish Branch…" needs a current branch that is not the
            // default branch (merging the default into itself is a no-op).
            let lean_finish = !current.is_empty() && current != default_branch;
            menu.item(item(
                "Start Feature…",
                true,
                crate::RepoView::gitflow_start_feature,
            ))
            .item(item(
                "Start Release…",
                true,
                crate::RepoView::gitflow_start_release,
            ))
            .item(item(
                "Start Hotfix…",
                true,
                crate::RepoView::gitflow_start_hotfix,
            ))
            .item(PopupMenuItem::separator())
            .item(item(
                "Finish Feature…",
                on_feature,
                crate::RepoView::gitflow_finish_dialog,
            ))
            .item(item(
                "Finish Release…",
                on_release,
                crate::RepoView::gitflow_finish_release,
            ))
            .item(item(
                "Finish Hotfix…",
                on_hotfix,
                crate::RepoView::gitflow_finish_hotfix,
            ))
            .item(PopupMenuItem::separator())
            .item(item(
                "Start Branch…",
                true,
                crate::RepoView::lean_start_branch,
            ))
            .item(item(
                "Finish Branch…",
                lean_finish,
                crate::RepoView::lean_finish_branch,
            ))
        }
    }
}

impl crate::RepoView {
    /// "Initialize…" — the Git Flow config fields with their defaults (M13 §1).
    pub fn gitflow_init_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let production = repo.head.clone().unwrap_or_else(|| "main".to_owned());
        self.gitflow_init_production
            .update(cx, |s, cx| s.set_value(production.clone(), window, cx));
        self.gitflow_init_development
            .update(cx, |s, cx| s.set_value("develop".to_owned(), window, cx));
        self.gitflow_init_feature
            .update(cx, |s, cx| s.set_value("feature/".to_owned(), window, cx));
        self.gitflow_init_release
            .update(cx, |s, cx| s.set_value("release/".to_owned(), window, cx));
        self.gitflow_init_hotfix
            .update(cx, |s, cx| s.set_value("hotfix/".to_owned(), window, cx));
        self.gitflow_init_bugfix
            .update(cx, |s, cx| s.set_value("bugfix/".to_owned(), window, cx));
        self.gitflow_init_support
            .update(cx, |s, cx| s.set_value("support/".to_owned(), window, cx));
        self.gitflow_init_versiontag
            .update(cx, |s, cx| s.set_value(String::new(), window, cx));

        let view = cx.entity();
        let (production, development, feature, release, hotfix, bugfix, support, versiontag) = (
            self.gitflow_init_production.clone(),
            self.gitflow_init_development.clone(),
            self.gitflow_init_feature.clone(),
            self.gitflow_init_release.clone(),
            self.gitflow_init_hotfix.clone(),
            self.gitflow_init_bugfix.clone(),
            self.gitflow_init_support.clone(),
            self.gitflow_init_versiontag.clone(),
        );
        let muted = cx.theme().muted_foreground;
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (
                view,
                production,
                development,
                feature,
                release,
                hotfix,
                bugfix,
                support,
                versiontag,
            ) = (
                view.clone(),
                production.clone(),
                development.clone(),
                feature.clone(),
                release.clone(),
                hotfix.clone(),
                bugfix.clone(),
                support.clone(),
                versiontag.clone(),
            );
            let row = |label: &str, input: &Entity<InputState>| {
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().w(px(120.)).child(label.to_owned()))
                    .child(Input::new(input))
            };
            dialog
                .title("Initialize Git Flow")
                .description(
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child("Stored in the repository's config under gitflow.*."),
                        )
                        .child(row("Production branch", &production))
                        .child(row("Development branch", &development))
                        .child(row("Feature prefix", &feature))
                        .child(row("Release prefix", &release))
                        .child(row("Hotfix prefix", &hotfix))
                        .child(row("Bugfix prefix", &bugfix))
                        .child(row("Support prefix", &support))
                        .child(row("Version tag prefix", &versiontag)),
                )
                .confirm()
                .ok_text("Initialize")
                .on_ok(move |_, window, cx| {
                    let production = production.read(cx).value().trim().to_owned();
                    let development = development.read(cx).value().trim().to_owned();
                    if production.is_empty() || development.is_empty() {
                        return false;
                    }
                    let config = GitflowConfig {
                        master: production,
                        develop: development,
                        feature: feature.read(cx).value().trim().to_owned(),
                        release: release.read(cx).value().trim().to_owned(),
                        hotfix: hotfix.read(cx).value().trim().to_owned(),
                        bugfix: bugfix.read(cx).value().trim().to_owned(),
                        support: support.read(cx).value().trim().to_owned(),
                        versiontag: versiontag.read(cx).value().trim().to_owned(),
                    };
                    view.update(cx, |this, cx| this.run_gitflow_init(config, window, cx));
                    true
                })
        });
    }

    /// "Start Feature…" (M13 §2).
    pub fn gitflow_start_feature(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.gitflow_start_dialog(FlowKind::Feature, window, cx);
    }

    /// "Start Release…" (M13 §2).
    pub fn gitflow_start_release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.gitflow_start_dialog(FlowKind::Release, window, cx);
    }

    /// "Start Hotfix…" (M13 §2).
    pub fn gitflow_start_hotfix(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.gitflow_start_dialog(FlowKind::Hotfix, window, cx);
    }

    fn gitflow_start_dialog(
        &mut self,
        kind: FlowKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let Some(cfg) = &repo.gitflow else {
            return;
        };
        let root = repo.root.clone();
        let branches: Vec<String> = repo
            .refs
            .iter()
            .filter(|r| r.kind == spork_git::RefKind::Local)
            .map(|r| r.name.clone())
            .collect();
        let default_base = kind.default_base(cfg).to_owned();
        let options = if branches.is_empty() {
            vec![default_base.clone()]
        } else {
            branches
        };
        let default_ix = options.iter().position(|b| b == &default_base).unwrap_or(0);
        let dialog = cx.new(|cx| GitflowStartDialog::new(kind, options, default_ix, window, cx));

        // Start-check notes (M13 §2): background, non-blocking.
        let (check_dialog, check_root, check_base) = (dialog.clone(), root.clone(), default_base);
        self._gitflow_task = Some(cx.spawn_in(window, async move |_, cx| {
            let check = cx
                .background_spawn(async move {
                    gitflow_start_checks(&check_root, kind, "", &check_base)
                })
                .await;
            let notes = match check {
                Ok(check) => start_check_notes(&check),
                Err(_) => Vec::new(),
            };
            check_dialog.update(cx, |d, cx| {
                d.set_notes(notes);
                cx.notify();
            });
        }));

        let view = cx.entity();
        let title = match kind {
            FlowKind::Feature => "Start Feature",
            FlowKind::Release => "Start Release",
            FlowKind::Hotfix => "Start Hotfix",
        };
        window.open_alert_dialog(cx, move |dialog_el, _, _| {
            let (dialog, ok_dialog, view) = (dialog.clone(), dialog.clone(), view.clone());
            dialog_el
                .title(title)
                .description(dialog)
                .confirm()
                .ok_text("Start")
                .on_ok(move |_, window, cx| {
                    let name = ok_dialog.read(cx).name(cx);
                    if name.is_empty() {
                        return false;
                    }
                    let base = ok_dialog.read(cx).base(cx);
                    view.update(cx, |this, cx| {
                        this.run_gitflow_start(kind, name, base, window, cx)
                    });
                    true
                })
        });
    }

    /// "Finish Feature…" (M13 §3): offered only from a feature branch.
    pub fn gitflow_finish_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.gitflow_finish_dialog_kind(FlowKind::Feature, window, cx);
    }

    /// "Finish Release…" (M13 §3): offered only from a release branch.
    pub fn gitflow_finish_release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.gitflow_finish_dialog_kind(FlowKind::Release, window, cx);
    }

    /// "Finish Hotfix…" (M13 §3): offered only from a hotfix branch.
    pub fn gitflow_finish_hotfix(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.gitflow_finish_dialog_kind(FlowKind::Hotfix, window, cx);
    }

    fn gitflow_finish_dialog_kind(
        &mut self,
        kind: FlowKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let Some(cfg) = &repo.gitflow else {
            return;
        };
        let on_kind_branch = repo
            .head
            .as_deref()
            .is_some_and(|branch| is_flow_branch(branch, kind, cfg));
        if !on_kind_branch {
            return;
        }
        // The tag version is the branch name after the kind's prefix
        // (release/1.2.0 -> 1.2.0); the tag message defaults to
        // `<versiontag><version>`.
        let version = repo
            .head
            .as_deref()
            .and_then(|branch| branch.strip_prefix(kind.prefix(cfg)))
            .unwrap_or_default()
            .to_owned();
        let (tag_default, tag_placeholder) = match kind {
            FlowKind::Feature => (String::new(), String::new()),
            FlowKind::Release => (
                cfg.versiontag.clone() + &version,
                "Release 1.2.0".to_owned(),
            ),
            FlowKind::Hotfix => (cfg.versiontag.clone() + &version, "Hotfix 1.2.1".to_owned()),
        };

        let dialog =
            cx.new(|cx| GitflowFinishDialog::new(kind, tag_default, tag_placeholder, window, cx));

        // Hotfix: the back-merge target (the newest release branch, when one
        // exists) is a local git read; run it off the UI thread and surface it
        // as a note (M13 §3, git-flow rule).
        if kind == FlowKind::Hotfix {
            let (note_dialog, note_root, note_prefix) =
                (dialog.clone(), repo.root.clone(), cfg.release.clone());
            self._gitflow_task = Some(cx.spawn_in(window, async move |_, cx| {
                let latest = cx
                    .background_spawn(
                        async move { latest_release_branch(&note_root, &note_prefix) },
                    )
                    .await;
                let note = latest.filter(|branch| !branch.is_empty()).map(|branch| {
                    format!("Back-merges into {branch} (newest release branch) instead of develop.")
                });
                note_dialog.update(cx, |d, cx| {
                    d.set_note(note);
                    cx.notify();
                });
            }));
        }

        let view = cx.entity();
        let (title, hint) = match kind {
            FlowKind::Feature => (
                "Finish Feature",
                "The feature branch is merged into development, then merged into the production branch. Pauses on conflicts.",
            ),
            FlowKind::Release => (
                "Finish Release",
                "The release branch is merged into the production branch, tagged, then back-merged into development. Pauses on conflicts.",
            ),
            FlowKind::Hotfix => (
                "Finish Hotfix",
                "The hotfix branch is merged into the production branch, tagged, then back-merged. Pauses on conflicts.",
            ),
        };
        let muted = cx.theme().muted_foreground;
        window.open_alert_dialog(cx, move |dialog_el, _, _| {
            let (dialog, view) = (dialog.clone(), view.clone());
            dialog_el
                .title(title)
                .description(
                    v_flex()
                        .gap_2()
                        .child(div().text_sm().text_color(muted).child(hint))
                        .child(dialog.clone()),
                )
                .confirm()
                .ok_text("Finish")
                .on_ok(move |_, window, cx| {
                    let options = dialog.read(cx).options(cx);
                    view.update(cx, |this, cx| {
                        this.run_gitflow_finish(kind, options, window, cx)
                    });
                    true
                })
        });
    }

    /// Banner while a git-flow finish is paused on conflicts (M13 §3); the
    /// conflicted merge/rebase itself stays in the normal conflict UI.
    pub fn gitflow_finish_banner(&self, cx: &mut Context<Self>) -> Option<Div> {
        let state = match &self.repo {
            Some(Ok(repo)) => repo.paused_finish.as_ref()?,
            _ => return None,
        };
        let theme = cx.theme();
        Some(
            h_flex()
                .flex_none()
                .px_4()
                .py_2()
                .gap_3()
                .justify_between()
                .bg(theme.yellow.opacity(0.12))
                .border_b_1()
                .border_color(theme.yellow.opacity(0.3))
                .child(
                    v_flex()
                        .child(
                            div().text_sm().font_weight(FontWeight::SEMIBOLD).child(format!(
                                "Git-flow finish paused at {}",
                                finish_step_label(&state.kind, state.step)
                            )),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!(
                                    "Branch: {} — resolve the conflicts in Local Changes, then resume.",
                                    state.name
                                )),
                        ),
                )
                .child(
                    h_flex().gap_2().child(
                        Button::new("gitflow-finish-abort")
                            .small()
                            .ghost()
                            .label("Abort finish")
                            .disabled(self.gitflow_busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.gitflow_abort_finish(window, cx)
                            })),
                    ),
                )
                .child(
                    Button::new("gitflow-finish-resume")
                        .small()
                        .primary()
                        .label("Resume git-flow finish")
                        .disabled(self.gitflow_busy)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.gitflow_resume_finish(window, cx)
                        })),
                ),
        )
    }

    /// Initialize runs `git config` writes — no remote calls, but keep it off
    /// the UI thread like every other git work (M13 §1).
    fn run_gitflow_init(
        &mut self,
        config: GitflowConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let root = repo.root.clone();
        self.gitflow_busy = true;
        cx.notify();
        self._gitflow_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { gitflow_init(&root, &config) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.gitflow_busy = false;
                match result {
                    Ok(()) => {
                        window.push_notification(Notification::success("Git Flow initialized"), cx)
                    }
                    Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
                }
                this.reload(cx);
            })
            .ok();
        }));
    }

    fn run_gitflow_start(
        &mut self,
        kind: FlowKind,
        name: String,
        base: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let root = repo.root.clone();
        self.gitflow_busy = true;
        cx.notify();
        // The library's gitflow_start takes no Cancellable (it is one local
        // `switch -c`); the handle is refreshed so a repo switch still kills
        // anything still in flight.
        self.gitflow_cancel = spork_git::Cancellable::default();
        self._gitflow_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { gitflow_start(&root, kind, &name, &base) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.gitflow_busy = false;
                match result {
                    Ok(branch) => window
                        .push_notification(Notification::success(format!("Started {branch}")), cx),
                    Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
                }
                this.reload(cx);
            })
            .ok();
        }));
    }

    fn run_gitflow_finish(
        &mut self,
        kind: FlowKind,
        options: FinishOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let root = repo.root.clone();
        self.gitflow_busy = true;
        cx.notify();
        self.gitflow_cancel = spork_git::Cancellable::default();
        let cancel = self.gitflow_cancel.clone();
        self._gitflow_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { gitflow_finish(kind, &root, &options, &cancel) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.gitflow_busy = false;
                match result {
                    Ok(FinishResult::Done) => {
                        window.push_notification(Notification::success(flow_finished(kind)), cx)
                    }
                    Ok(FinishResult::Conflicts { step }) => window.push_notification(
                        Notification::warning(format!(
                            "Paused at {label} — resolve the conflicts, then resume.",
                            label = finish_step_label(kind_spelling(kind), step)
                        )),
                        cx,
                    ),
                    Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
                }
                // The reload re-reads the finish state file, so the banner
                // appears (or refreshes its step) right here.
                this.reload(cx);
            })
            .ok();
        }));
    }

    /// "Resume git-flow finish" (M13 §3): continues the persisted state.
    pub fn gitflow_resume_finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let root = repo.root.clone();
        // The kind is read from the persisted state so the notifications name
        // the right flow (feature / release / hotfix).
        let kind_spelling = repo
            .paused_finish
            .as_ref()
            .map(|state| state.kind.clone())
            .unwrap_or_else(|| "feature".to_owned());
        self.gitflow_busy = true;
        cx.notify();
        self.gitflow_cancel = spork_git::Cancellable::default();
        let cancel = self.gitflow_cancel.clone();
        self._gitflow_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { gitflow_resume_finish(&root, &cancel) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.gitflow_busy = false;
                match result {
                    Ok(FinishResult::Done) => window.push_notification(
                        Notification::success(flow_finished_spelling(&kind_spelling)),
                        cx,
                    ),
                    Ok(FinishResult::Conflicts { step }) => window.push_notification(
                        Notification::warning(format!(
                            "Still paused at {label} — resolve the conflicts, then resume again.",
                            label = finish_step_label(&kind_spelling, step)
                        )),
                        cx,
                    ),
                    Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
                }
                this.reload(cx);
            })
            .ok();
        }));
    }

    /// "Abort finish" (M13 §3): drops the persisted state; the conflicted
    /// merge/rebase is left for the normal conflict UI.
    pub fn gitflow_abort_finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let root = repo.root.clone();
        self.gitflow_busy = true;
        cx.notify();
        self._gitflow_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { gitflow_abort_finish(&root) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.gitflow_busy = false;
                match result {
                    Ok(()) => {
                        window.push_notification(Notification::info("Git-flow finish aborted"), cx)
                    }
                    Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
                }
                this.reload(cx);
            })
            .ok();
        }));
    }

    /// "Start Branch…" (M13 §4).
    pub fn lean_start_branch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let branches: Vec<String> = repo
            .refs
            .iter()
            .filter(|r| r.kind == spork_git::RefKind::Local)
            .map(|r| r.name.clone())
            .collect();
        let default_base = repo.default_branch.clone();
        let options = if branches.is_empty() {
            vec![default_base.clone()]
        } else {
            branches
        };
        let default_ix = options.iter().position(|b| b == &default_base).unwrap_or(0);
        let dialog = cx.new(|cx| LeanStartDialog::new(options, default_ix, window, cx));
        let view = cx.entity();
        window.open_alert_dialog(cx, move |dialog_el, _, _| {
            let (dialog, ok_dialog, view) = (dialog.clone(), dialog.clone(), view.clone());
            dialog_el
                .title("Start Branch")
                .description(dialog)
                .confirm()
                .ok_text("Start")
                .on_ok(move |_, window, cx| {
                    let name = ok_dialog.read(cx).name(cx);
                    if name.is_empty() {
                        return false;
                    }
                    let base = ok_dialog.read(cx).base(cx);
                    view.update(cx, |this, cx| this.run_lean_start(name, base, window, cx));
                    true
                })
        });
    }

    /// "Finish Branch…" (M13 §4).
    pub fn lean_finish_branch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let branches: Vec<String> = repo
            .refs
            .iter()
            .filter(|r| r.kind == spork_git::RefKind::Local)
            .map(|r| r.name.clone())
            .collect();
        let default_target = repo.default_branch.clone();
        let options = if branches.is_empty() {
            vec![default_target.clone()]
        } else {
            branches
        };
        let default_ix = options
            .iter()
            .position(|b| b == &default_target)
            .unwrap_or(0);
        let dialog = cx.new(|cx| LeanFinishDialog::new(options, default_ix, window, cx));
        let view = cx.entity();
        window.open_alert_dialog(cx, move |dialog_el, _, _| {
            let (dialog, ok_dialog, view) = (dialog.clone(), dialog.clone(), view.clone());
            dialog_el
                .title("Finish Branch")
                .description(dialog)
                .confirm()
                .ok_text("Finish")
                .on_ok(move |_, window, cx| {
                    let target = ok_dialog.read(cx).target(cx);
                    let delete_branch = ok_dialog.read(cx).delete_branch();
                    view.update(cx, |this, cx| {
                        this.run_lean_finish(target, delete_branch, window, cx)
                    });
                    true
                })
        });
    }

    fn run_lean_start(
        &mut self,
        name: String,
        base: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let root = repo.root.clone();
        self.gitflow_busy = true;
        cx.notify();
        // The library's lean_start takes no Cancellable (one local `switch -c`);
        // the handle is refreshed so a repo switch still kills anything still
        // in flight.
        self.gitflow_cancel = spork_git::Cancellable::default();
        self._gitflow_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { spork_git::lean::lean_start(&root, &name, &base) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.gitflow_busy = false;
                match result {
                    Ok(branch) => window
                        .push_notification(Notification::success(format!("Started {branch}")), cx),
                    Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
                }
                this.reload(cx);
            })
            .ok();
        }));
    }

    fn run_lean_finish(
        &mut self,
        target: String,
        delete_branch: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let root = repo.root.clone();
        let merged_msg = format!("Merged into {target}");
        self.gitflow_busy = true;
        cx.notify();
        // The library's lean_finish takes no Cancellable; the handle is
        // refreshed so a repo switch still kills anything still in flight.
        self.gitflow_cancel = spork_git::Cancellable::default();
        self._gitflow_task = Some(cx.spawn_in(window, async move |this, cx| {
            // A conflicted merge is left in the repo's op-state and is not an
            // error; check it so the notification is accurate. The existing
            // op banner (M11) shows once the reload re-reads op_state.
            let result = cx
                .background_spawn(async move {
                    let res = spork_git::lean::lean_finish(&root, &target, delete_branch);
                    let conflicted = res.is_ok()
                        && matches!(
                            spork_git::op_state(&root).ok(),
                            Some(spork_git::OpState::Merging)
                        );
                    (res, conflicted)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.gitflow_busy = false;
                match result {
                    (Ok(()), true) => window.push_notification(
                        Notification::warning(
                            "Stopped on conflicts. Resolve the files marked U, then Continue.",
                        ),
                        cx,
                    ),
                    (Ok(()), false) => {
                        window.push_notification(Notification::success(merged_msg), cx)
                    }
                    (Err(e), _) => window.push_notification(Notification::error(e.to_string()), cx),
                }
                this.reload(cx);
            })
            .ok();
        }));
    }
}

// ---------------------------------------------------------------------------
// Lean branching dialogs (M13 §4): one branch off the default branch, finished
// by a no-ff merge back into it.
// ---------------------------------------------------------------------------

/// "Start Branch…" (M13 §4): the branch name (folder names like
/// `feature/login` are allowed) and the branch to create it from (the default
/// branch by default).
pub struct LeanStartDialog {
    name: Entity<InputState>,
    base: Entity<SelectState<SearchableVec<String>>>,
}

impl LeanStartDialog {
    fn new(
        branches: Vec<String>,
        default_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("e.g. feature/login-form"));
        let base = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(branches),
                Some(IndexPath::new(default_ix)),
                window,
                cx,
            )
        });
        Self { name, base }
    }

    pub fn name(&self, cx: &App) -> String {
        self.name.read(cx).value().trim().to_owned()
    }

    pub fn base(&self, cx: &App) -> String {
        self.base
            .read(cx)
            .selected_value()
            .cloned()
            .unwrap_or_default()
    }
}

impl Render for LeanStartDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .gap_2()
            .child(div().text_sm().text_color(muted).child(
                "Name of the branch to create. Folder names like feature/login are allowed.",
            ))
            .child(Input::new(&self.name))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().child("Create at"))
                    .child(Select::new(&self.base)),
            )
    }
}

/// "Finish Branch…" (M13 §4): merge the current branch into a target and
/// optionally delete it.
pub struct LeanFinishDialog {
    target: Entity<SelectState<SearchableVec<String>>>,
    delete_branch: bool,
}

impl LeanFinishDialog {
    fn new(
        branches: Vec<String>,
        default_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let target = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(branches),
                Some(IndexPath::new(default_ix)),
                window,
                cx,
            )
        });
        Self {
            target,
            delete_branch: false,
        }
    }

    pub fn target(&self, cx: &App) -> String {
        self.target
            .read(cx)
            .selected_value()
            .cloned()
            .unwrap_or_default()
    }

    pub fn delete_branch(&self) -> bool {
        self.delete_branch
    }
}

impl Render for LeanFinishDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        v_flex()
            .gap_2()
            .child(div().text_sm().child("Merge the current branch into"))
            .child(Select::new(&self.target))
            .child({
                let view = view;
                Checkbox::new("lean-finish-delete-branch")
                    .label("Delete branch")
                    .checked(self.delete_branch)
                    .on_click(move |checked, _, cx| {
                        view.update(cx, |d, cx| {
                            d.delete_branch = *checked;
                            cx.notify();
                        });
                    })
            })
    }
}

/// The library's kind tag for `kind` (`"feature"` / `"release"` / `"hotfix"`),
/// as used by `finish_step_label`.
fn kind_spelling(kind: FlowKind) -> &'static str {
    match kind {
        FlowKind::Feature => "feature",
        FlowKind::Release => "release",
        FlowKind::Hotfix => "hotfix",
    }
}

/// The "… finished" success notification for `kind`.
fn flow_finished(kind: FlowKind) -> &'static str {
    match kind {
        FlowKind::Feature => "Feature finished",
        FlowKind::Release => "Release finished",
        FlowKind::Hotfix => "Hotfix finished",
    }
}

/// The "… finished" success notification for a persisted-state kind tag.
fn flow_finished_spelling(spelling: &str) -> &'static str {
    match spelling {
        "release" => "Release finished",
        "hotfix" => "Hotfix finished",
        _ => "Feature finished",
    }
}

/// Warning notes from [`StartCheck`] (M13 §2): warn, never block.
fn start_check_notes(check: &StartCheck) -> Vec<String> {
    let mut notes = Vec::new();
    if let Some((upstream, behind)) = &check.base_behind_upstream {
        notes.push(format!(
            "Base branch is {behind} commit(s) behind {upstream} — consider pulling first."
        ));
    }
    if let Some(upstream) = &check.gone_upstream {
        notes.push(format!(
            "Base branch's upstream {upstream} no longer exists."
        ));
    }
    for branch in &check.other_branches {
        notes.push(format!("Another branch already exists: {branch}"));
    }
    notes
}

#[cfg(test)]
mod tests {
    // explicit: super::* would pull in gpui's test attribute, shadowing #[test]
    use super::{is_feature_branch, is_flow_branch};
    use spork_git::gitflow::{FlowKind, GitflowConfig};

    fn config() -> GitflowConfig {
        GitflowConfig {
            master: "main".to_owned(),
            develop: "develop".to_owned(),
            feature: "feature/".to_owned(),
            release: "release/".to_owned(),
            hotfix: "hotfix/".to_owned(),
            bugfix: "bugfix/".to_owned(),
            support: "support/".to_owned(),
            versiontag: String::new(),
        }
    }

    #[test]
    fn feature_branch_matches_prefix() {
        assert!(is_feature_branch("feature/login", &config()));
        assert!(is_feature_branch("feature/login-form/v2", &config()));
    }

    #[test]
    fn other_branches_do_not_match() {
        let cfg = config();
        assert!(!is_feature_branch("featurex/login", &cfg));
        assert!(!is_feature_branch("develop", &cfg));
        assert!(!is_feature_branch("main", &cfg));
        assert!(!is_feature_branch("release/1.0.0", &cfg));
    }

    #[test]
    fn release_branch_matches_prefix() {
        let cfg = config();
        assert!(is_flow_branch("release/1.0.0", FlowKind::Release, &cfg));
        assert!(is_flow_branch(
            "release/1.2.0-rc.1",
            FlowKind::Release,
            &cfg
        ));
        // A release branch is not a feature or hotfix branch.
        assert!(!is_flow_branch("release/1.0.0", FlowKind::Feature, &cfg));
        assert!(!is_flow_branch("release/1.0.0", FlowKind::Hotfix, &cfg));
    }

    #[test]
    fn hotfix_branch_matches_prefix() {
        let cfg = config();
        assert!(is_flow_branch("hotfix/1.0.1", FlowKind::Hotfix, &cfg));
        assert!(is_flow_branch("hotfix/login-crash", FlowKind::Hotfix, &cfg));
        // A hotfix branch is not a feature or release branch.
        assert!(!is_flow_branch("hotfix/1.0.1", FlowKind::Feature, &cfg));
        assert!(!is_flow_branch("hotfix/1.0.1", FlowKind::Release, &cfg));
    }
}
