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
    gitflow_start_checks,
};

/// True when `branch` carries the configured feature prefix (M13 §3: Finish
/// Feature is offered only from a feature branch).
pub fn is_feature_branch(branch: &str, config: &GitflowConfig) -> bool {
    branch.starts_with(config.feature.as_str())
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

/// "Finish Feature" dialog (M13 §3): the per-run finish options.
#[derive(Default)]
pub struct GitflowFinishDialog {
    options: FinishOptions,
}

impl GitflowFinishDialog {
    pub fn options(&self) -> FinishOptions {
        self.options.clone()
    }
}

impl Render for GitflowFinishDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        v_flex()
            .gap_2()
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
                let view = view.clone();
                Checkbox::new("gitflow-finish-delete-remote")
                    .label("Delete remote branch")
                    .checked(self.options.delete_remote)
                    .on_click(move |checked, _, cx| {
                        view.update(cx, |d, cx| {
                            d.options.delete_remote = *checked;
                            cx.notify();
                        });
                    })
            })
    }
}

impl crate::RepoView {
    /// Snapshot of (git-flow config, current branch) for the "Git Flow" submenu (M13 §1).
    pub fn gitflow_menu_state(&self) -> (Option<GitflowConfig>, Option<String>) {
        match &self.repo {
            Some(Ok(repo)) => (repo.gitflow.clone(), repo.head.clone()),
            _ => (None, None),
        }
    }
}

/// Builds the "Git Flow" submenu of the repository switcher (M13 §1).
pub fn gitflow_submenu(
    menu: PopupMenu,
    config: Option<GitflowConfig>,
    current: Option<String>,
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
        Some(cfg) => menu
            .item(item(
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
                current
                    .as_deref()
                    .is_some_and(|branch| is_feature_branch(branch, cfg)),
                crate::RepoView::gitflow_finish_dialog,
            )),
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

    /// "Finish Feature…" (M13 §3).
    pub fn gitflow_finish_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let Some(cfg) = &repo.gitflow else {
            return;
        };
        let on_feature_branch = repo
            .head
            .as_deref()
            .is_some_and(|branch| is_feature_branch(branch, cfg));
        if !on_feature_branch {
            return;
        }
        let dialog = cx.new(|_| GitflowFinishDialog::default());
        let view = cx.entity();
        let muted = cx.theme().muted_foreground;
        window.open_alert_dialog(cx, move |dialog_el, _, _| {
            let (dialog, view) = (dialog.clone(), view.clone());
            dialog_el
                .title("Finish Feature")
                .description(
                    v_flex()
                        .gap_2()
                        .child(div().text_sm().text_color(muted).child(
                            "The feature branch is merged into development, then merged into the production branch. Pauses on conflicts.",
                        ))
                        .child(dialog.clone()),
                )
                .confirm()
                .ok_text("Finish")
                .on_ok(move |_, window, cx| {
                    let options = dialog.read(cx).options();
                    view.update(cx, |this, cx| this.run_gitflow_finish(options, window, cx));
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
                .background_spawn(async move {
                    gitflow_finish(FlowKind::Feature, &root, &options, &cancel)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.gitflow_busy = false;
                match result {
                    Ok(FinishResult::Done) => {
                        window.push_notification(Notification::success("Feature finished"), cx)
                    }
                    Ok(FinishResult::Conflicts { step }) => window.push_notification(
                        Notification::warning(format!(
                            "Paused at {label} — resolve the conflicts, then resume.",
                            label = finish_step_label("feature", step)
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
                    Ok(FinishResult::Done) => {
                        window.push_notification(Notification::success("Feature finished"), cx)
                    }
                    Ok(FinishResult::Conflicts { step }) => window.push_notification(
                        Notification::warning(format!(
                            "Still paused at {label} — resolve the conflicts, then resume again.",
                            label = finish_step_label("feature", step)
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
    use super::is_feature_branch;
    use spork_git::gitflow::GitflowConfig;

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
}
