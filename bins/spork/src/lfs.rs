//! Git LFS (M14): the "Git LFS" submenu of the repository switcher — install
//! hooks, fetch/pull objects, track/untrack patterns — plus the LFS status view
//! with per-row lock/unlock and the persistent banner shown while `git-lfs`
//! itself is missing. All long-running work runs in smol background tasks
//! behind `RepoView::lfs_cancel`, the way the Git Flow menu does it.

use std::ops::Range;
use std::path::PathBuf;

use glob::Pattern;
use gpui_kit::assets::IconName;
use gpui_kit::component::IndexPath;
use gpui_kit::component::Theme;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::select::{SearchableVec, Select, SelectState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use spork_git::lfs::{
    LfsFile, LfsLock, lfs_fetch, lfs_install_hooks, lfs_lock, lfs_locks, lfs_ls_files,
    lfs_progress, lfs_pull, lfs_track, lfs_track_preview, lfs_unlock, lfs_untrack,
};
use spork_git::{Cancellable, Ref, RefKind};

use crate::View;

/// Whether the `git-lfs` extension is installed, cached for the process: it is a
/// global `git lfs version` check, not a per-repository fact, so it runs once (M14 §1).
pub fn lfs_installed_cached() -> bool {
    static INSTALLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *INSTALLED.get_or_init(spork_git::lfs::lfs_installed)
}

/// Humanizes a byte count for the status table's size column.
pub fn lfs_format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value <= 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// `*.<ext>` for a file path, or `None` when the name has no extension.
pub fn lfs_pattern_from_path(path: &str) -> Option<String> {
    let name = path.rsplit('/').next()?;
    let ext = name.rsplit_once('.')?.1;
    if ext.is_empty() || ext.chars().any(char::is_whitespace) {
        return None;
    }
    Some(format!("*.{ext}"))
}

/// Remote names for the fetch/pull dialogs, in the same order the refs list
/// carries them.
pub fn lfs_remote_names(refs: &[Ref]) -> Vec<String> {
    refs.iter()
        .filter(|r| matches!(r.kind, RefKind::Remote))
        .map(|r| r.name.rsplit('/').next().unwrap_or(&r.name).to_owned())
        .collect()
}

/// Whether `path` matches one of the LFS `.gitattributes` patterns (M14 badges).
/// A pattern without a slash matches the file name at any depth (`*.png` also
/// matches `assets/a/b.png`); a pattern with a slash is matched against the
/// full path (`images/**` matches `images/x.png`). A pattern that does not
/// compile is skipped, so a broken `.gitattributes` never panics.
pub fn is_lfs_path(path: &str, patterns: &[String]) -> bool {
    for pattern in patterns {
        let Some(glob) = Pattern::new(pattern).ok() else {
            continue;
        };
        let matched = if pattern.contains('/') {
            glob.matches(path)
        } else {
            path.rsplit('/')
                .next()
                .is_some_and(|name| glob.matches(name))
        };
        if matched {
            return true;
        }
    }
    false
}

/// The small "LFS" chip for the status list and the diff headers (M14 badges),
/// styled after the history label badges.
pub fn lfs_badge(theme: &Theme) -> Div {
    let tint = theme.blue;
    div()
        .flex_none()
        .px_1p5()
        .rounded(px(4.))
        .bg(tint.opacity(0.18))
        .text_color(tint)
        .text_xs()
        .child("LFS")
}

/// One row of the LFS status table: a file joined with its lock, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsRow {
    pub path: String,
    pub oid: String,
    pub size: u64,
    pub downloaded: bool,
    /// `Some("you")` for our own lock, `Some(owner)` for someone else's,
    /// `None` when the file is not locked (or the locking data is missing).
    pub locked_by: Option<String>,
    pub locked: bool,
    pub lock_mine: bool,
}

/// Joins the tracked files with their locks by path. `None` locks (no locking
/// server) leave the lock fields empty without failing the table.
pub fn lfs_join_rows(files: &[LfsFile], locks: Option<&[LfsLock]>) -> Vec<LfsRow> {
    files
        .iter()
        .map(|f| {
            let lock = locks.and_then(|ls| ls.iter().find(|l| l.path == f.path));
            let (locked, lock_mine, locked_by) = match lock {
                Some(l) if l.mine => (true, true, Some("you".to_owned())),
                Some(l) => (true, false, Some(l.owner.clone().unwrap_or_default())),
                None => (false, false, None),
            };
            LfsRow {
                path: f.path.clone(),
                oid: f.oid.clone(),
                size: f.size,
                downloaded: f.downloaded,
                locked_by,
                locked,
                lock_mine,
            }
        })
        .collect()
}

/// Per-row action enablement: `(lock_enabled, unlock_enabled, force_needed)`.
/// Locking is offered for uncommitted or unlocked files; unlocking is offered
/// for locked files, and needs force confirmation when the lock is not ours.
pub fn lfs_row_actions(locked: bool, lock_mine: bool) -> (bool, bool, bool) {
    let lock_enabled = !locked;
    let unlock_enabled = locked;
    let force_needed = locked && !lock_mine;
    (lock_enabled, unlock_enabled, force_needed)
}

/// Submenu enablement for the hook-install entry (M14 §2).
pub fn lfs_install_hooks_enabled(in_use: bool, hooks: bool) -> bool {
    in_use && !hooks
}

/// Submenu enablement for the fetch/pull entries.
pub fn lfs_remote_op_enabled(in_use: bool, remotes: &[String]) -> bool {
    in_use && !remotes.is_empty()
}

/// Submenu enablement for the track/untrack/status entries.
pub fn lfs_track_op_enabled(in_use: bool) -> bool {
    in_use
}

/// What the "Git LFS" submenu needs, snapshotted from `RepoView` so the pure
/// enablement helpers stay testable.
#[derive(Debug, Clone, Default)]
pub struct LfsMenuState {
    pub in_use: bool,
    pub hooks: bool,
    pub remotes: Vec<String>,
}

impl crate::RepoView {
    /// Whether the loaded repository tracks files with Git LFS (M14).
    pub(crate) fn lfs_in_use(&self) -> bool {
        matches!(&self.repo, Some(Ok(repo)) if repo.lfs_in_use)
    }

    /// The loaded repository's `filter=lfs` patterns, for the status and diff badges (M14).
    pub(crate) fn lfs_patterns(&self) -> &[String] {
        match &self.repo {
            Some(Ok(repo)) => &repo.lfs_patterns,
            _ => &[],
        }
    }

    /// Snapshot of the LFS facts for the "Git LFS" submenu (M14 §1–§2).
    pub fn lfs_menu_state(&self) -> LfsMenuState {
        let Some(Ok(repo)) = self.repo.as_ref() else {
            return LfsMenuState::default();
        };
        LfsMenuState {
            in_use: repo.lfs_in_use,
            hooks: repo.lfs_hooks,
            remotes: lfs_remote_names(&repo.refs),
        }
    }

    /// Installs the LFS pre-push / pre-commit hooks for this repository
    /// (M14 §2).
    pub fn lfs_install_hooks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let root = repo.root.clone();
        self.lfs_cancel = Cancellable::default();
        self._lfs_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { lfs_install_hooks(&root) })
                .await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => window.push_notification(
                        Notification::success("LFS hooks installed for this repository"),
                        cx,
                    ),
                    Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
                }
                this.reload(cx);
            })
            .ok();
        }));
    }

    /// Opens the fetch-objects dialog (M14 §1).
    pub fn lfs_fetch_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.lfs_remote_dialog("Fetch Git LFS objects", true, window, cx);
    }

    /// Opens the pull dialog (M14 §1).
    pub fn lfs_pull_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.lfs_remote_dialog("Pull Git LFS objects", false, window, cx);
    }

    fn lfs_remote_dialog(
        &mut self,
        title: &'static str,
        fetch: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let remotes = lfs_remote_names(&repo.refs);
        if remotes.is_empty() {
            return;
        }
        let dialog = cx.new(|cx| LfsRemoteDialog::new(remotes, window, cx));
        let view = cx.entity();
        window.open_alert_dialog(cx, move |dialog_el, _, _| {
            let (dialog, ok_dialog, view) = (dialog.clone(), dialog.clone(), view.clone());
            dialog_el
                .title(title)
                .description(dialog)
                .confirm()
                .ok_text(if fetch { "Fetch" } else { "Pull" })
                .on_ok(move |_, window, cx| {
                    let remote = ok_dialog.read(cx).remote(cx);
                    if remote.is_empty() {
                        return false;
                    }
                    view.update(cx, |this, cx| {
                        this.run_lfs_fetch_pull(remote, fetch, window, cx)
                    });
                    true
                })
        });
    }

    fn run_lfs_fetch_pull(
        &mut self,
        remote: String,
        fetch: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let root = repo.root.clone();
        let progress_path = lfs_progress_file();
        let progress_read = progress_path.clone();
        let remote_name = remote.clone();
        self.lfs_cancel = Cancellable::default();
        let cancel = self.lfs_cancel.clone();
        let verb = if fetch { "fetch" } else { "pull" };
        self._lfs_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    if fetch {
                        lfs_fetch(&root, &remote, &progress_path, &cancel)
                    } else {
                        lfs_pull(&root, &remote, &progress_path, &cancel)
                    }
                })
                .await;
            let progress_line = lfs_progress(&progress_read);
            let _ = std::fs::remove_file(&progress_read);
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => {
                        let msg = if progress_line.is_empty() {
                            format!("LFS {verb} from {remote_name} finished")
                        } else {
                            format!("LFS {verb} from {remote_name} — {progress_line}")
                        };
                        window.push_notification(Notification::success(msg), cx);
                    }
                    Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
                }
                this.reload(cx);
            })
            .ok();
        }));
    }

    /// Opens the track dialog (M14 §2): pattern input plus a preview of the
    /// tracked files the pattern would take over, recomputed on OK.
    pub fn lfs_track_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let prefill = self
            .change_selected
            .as_ref()
            .and_then(|s| lfs_pattern_from_path(&s.path));
        let root = repo.root.clone();
        let dialog = cx.new(|cx| LfsPatternDialog::new(prefill, window, cx));
        let view = cx.entity();
        window.open_alert_dialog(cx, move |dialog_el, _, _| {
            let root = root.clone();
            let (dialog, ok_dialog, view) = (dialog.clone(), dialog.clone(), view.clone());
            dialog_el
                .title("Track files with Git LFS")
                .description(dialog)
                .confirm()
                .ok_text("Track")
                .on_ok(move |_, window, cx| {
                    let pattern = ok_dialog.read(cx).pattern(cx);
                    if pattern.is_empty() {
                        return false;
                    }
                    if !ok_dialog.read(cx).has_preview() {
                        // First OK: show which tracked files would be taken
                        // over (recomputed here, not on every keystroke) and
                        // keep the dialog open.
                        let preview = lfs_track_preview(&root, &pattern);
                        ok_dialog.update(cx, |d, cx| d.set_preview(preview, cx));
                        return false;
                    }
                    view.update(cx, |this, cx| this.run_lfs_track(pattern, window, cx));
                    true
                })
        });
    }

    fn run_lfs_track(&mut self, pattern: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let root = repo.root.clone();
        let pattern_msg = pattern.clone();
        self.lfs_cancel = Cancellable::default();
        self._lfs_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { lfs_track(&root, &pattern) })
                .await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => window.push_notification(
                        Notification::success(format!("Now tracking {pattern_msg} with Git LFS")),
                        cx,
                    ),
                    Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
                }
                this.reload(cx);
            })
            .ok();
        }));
    }

    /// Opens the untrack dialog (M14 §2).
    pub fn lfs_untrack_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(_)) = &self.repo else {
            return;
        };
        let dialog = cx.new(|cx| LfsPatternDialog::new(None, window, cx));
        let view = cx.entity();
        window.open_alert_dialog(cx, move |dialog_el, _, _| {
            let (dialog, ok_dialog, view) = (dialog.clone(), dialog.clone(), view.clone());
            dialog_el
                .title("Stop tracking with Git LFS")
                .description(dialog)
                .confirm()
                .ok_text("Untrack")
                .on_ok(move |_, window, cx| {
                    let pattern = ok_dialog.read(cx).pattern(cx);
                    if pattern.is_empty() {
                        return false;
                    }
                    view.update(cx, |this, cx| this.run_lfs_untrack(pattern, window, cx));
                    true
                })
        });
    }

    fn run_lfs_untrack(&mut self, pattern: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let root = repo.root.clone();
        let pattern_msg = pattern.clone();
        self.lfs_cancel = Cancellable::default();
        self._lfs_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { lfs_untrack(&root, &pattern) })
                .await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => window.push_notification(
                        Notification::success(format!("Stopped tracking {pattern_msg}")),
                        cx,
                    ),
                    Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
                }
                this.reload(cx);
            })
            .ok();
        }));
    }

    /// Opens the LFS status view and loads the table in the background
    /// (M14 §3).
    pub fn lfs_open_status(&mut self, cx: &mut Context<Self>) {
        self.view = View::Lfs;
        self.lfs_status = Some(LfsStatus::loading());
        self.lfs_load_status(cx);
        cx.notify();
    }

    /// Closes the LFS view and returns to the history (M14 §3).
    pub(crate) fn close_lfs(&mut self, cx: &mut Context<Self>) {
        self.lfs_cancel.kill();
        self._lfs_task = None;
        self._lfs_status_task = None;
        self.lfs_status = None;
        self.view = View::History;
        cx.notify();
    }

    fn lfs_load_status(&mut self, cx: &mut Context<Self>) {
        let Some(Ok(repo)) = self.repo.as_ref() else {
            return;
        };
        let root = repo.root.clone();
        self._lfs_status_task = Some(cx.spawn(async move |this, cx| {
            let (files, locks) = cx
                .background_spawn(async move {
                    let files = lfs_ls_files(&root);
                    // A missing locking server is not an error for the table:
                    // drop the locks and keep the files.
                    let locks = lfs_locks(&root).ok();
                    (files, locks)
                })
                .await;
            this.update_in(cx, |this, _window, cx| {
                let Some(status) = this.lfs_status.as_mut() else {
                    return;
                };
                status.loading = false;
                status.error = match files {
                    Ok(fs) => {
                        status.files = fs;
                        None
                    }
                    Err(e) => Some(e.to_string()),
                };
                status.locks = locks;
                this.lfs_notify(cx);
            })
            .ok();
        }));
    }

    fn lfs_refresh_status(&mut self, cx: &mut Context<Self>) {
        self.lfs_status = Some(LfsStatus::loading());
        self.lfs_load_status(cx);
    }

    fn lfs_notify(&mut self, cx: &mut Context<Self>) {
        cx.notify();
    }

    /// Locks one tracked file (M14 §4).
    fn lfs_lock_file(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let root = repo.root.clone();
        let path_msg = path.clone();
        self.lfs_cancel = Cancellable::default();
        self._lfs_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { lfs_lock(&root, &path, false) })
                .await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(lock) => {
                        let by = if lock.mine {
                            "you".to_owned()
                        } else {
                            lock.owner.clone().unwrap_or_else(|| "someone".to_owned())
                        };
                        window.push_notification(
                            Notification::success(format!("Locked {path_msg} — held by {by}")),
                            cx,
                        );
                    }
                    Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
                }
                this.lfs_refresh_status(cx);
            })
            .ok();
        }));
    }

    /// Unlocks one tracked file, confirming first when the lock is not ours
    /// (M14 §4).
    fn lfs_unlock_file(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let (mine, owner) = {
            let Some(s) = self.lfs_status.as_ref() else {
                return;
            };
            let lock = s
                .locks
                .as_ref()
                .and_then(|ls| ls.iter().find(|l| l.path == path))
                .cloned();
            match lock {
                Some(l) => (l.mine, l.owner),
                None => (false, None),
            }
        };
        if mine {
            self.run_lfs_unlock(path, false, window, cx);
            return;
        }
        let owner_name = owner.unwrap_or_else(|| "someone else".to_owned());
        let confirm = path.clone();
        let view = cx.entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (confirm, view) = (confirm.clone(), view.clone());
            dialog
                .title("Force unlock?")
                .description(format!(
                    "This file is locked by {owner_name} — force unlock?"
                ))
                .confirm()
                .ok_text("Force unlock")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    view.update(cx, |this, cx| {
                        this.run_lfs_unlock(confirm.clone(), true, window, cx)
                    });
                    true
                })
        });
    }

    fn run_lfs_unlock(
        &mut self,
        path: String,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let root = repo.root.clone();
        let path_msg = path.clone();
        self.lfs_cancel = Cancellable::default();
        self._lfs_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { lfs_unlock(&root, &path, force) })
                .await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => window.push_notification(
                        Notification::success(format!("Unlocked {path_msg}")),
                        cx,
                    ),
                    Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
                }
                this.lfs_refresh_status(cx);
            })
            .ok();
        }));
    }

    pub(crate) fn lfs_status_view(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(s) = self.lfs_status.as_ref() else {
            return div().into_any_element();
        };
        v_flex()
            .size_full()
            .child(self.lfs_status_header(s, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(self.lfs_status_list(s, cx)),
            )
            .into_any_element()
    }

    fn lfs_status_header(&self, s: &LfsStatus, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (muted, border, red) = (theme.muted_foreground, theme.border, theme.red);
        h_flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(border)
            .child(
                Button::new("lfs-back")
                    .ghost()
                    .icon(IconName::ArrowLeft)
                    .tooltip("Back to the previous view")
                    .on_click(cx.listener(|this, _, _, cx| this.close_lfs(cx))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .truncate()
                    .child("Git LFS files".to_owned()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .truncate()
                    .child(format!("{} files", s.files.len())),
            )
            .children(
                s.error
                    .iter()
                    .map(|e| div().text_xs().text_color(red).truncate().child(e.clone())),
            )
    }

    fn lfs_status_list(&self, s: &LfsStatus, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let centered = |msg: String| {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(muted)
                .child(msg)
                .into_any_element()
        };
        if s.loading {
            return centered("Loading LFS files…".into());
        }
        if let Some(error) = &s.error {
            return centered(error.clone());
        }
        if s.files.is_empty() {
            return centered("No files are tracked with Git LFS.".into());
        }
        uniform_list(
            "lfs-status",
            s.files.len(),
            cx.processor(|this, range, _, cx| this.lfs_status_rows(range, cx)),
        )
        .flex_1()
        .min_h_0()
        .into_any_element()
    }

    fn lfs_status_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(s) = self.lfs_status.as_ref() else {
            return Vec::new();
        };
        let rows = lfs_join_rows(&s.files, s.locks.as_deref());
        let theme = cx.theme();
        let (muted, border, red) = (theme.muted_foreground, theme.border, theme.red);
        range
            .map(|i| {
                let Some(row) = rows.get(i).cloned() else {
                    return div().into_any_element();
                };
                let (lock_enabled, unlock_enabled, _) = lfs_row_actions(row.locked, row.lock_mine);
                let path = row.path.clone();
                let (lock_path, unlock_path) = (path.clone(), path.clone());
                let oid = row.oid.chars().take(10).collect::<String>();
                let size = lfs_format_size(row.size);
                let downloaded = if row.downloaded { "yes" } else { "missing" };
                let locked_by = row.locked_by.clone().unwrap_or_default();
                h_flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .h(px(32.))
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .truncate()
                            .child(path.clone()),
                    )
                    .child(div().w(px(90.)).text_xs().text_color(muted).child(oid))
                    .child(div().w(px(70.)).text_xs().text_color(muted).child(size))
                    .child(
                        div()
                            .w(px(80.))
                            .text_xs()
                            .text_color(if row.downloaded { muted } else { red })
                            .child(downloaded),
                    )
                    .child(
                        div()
                            .w(px(110.))
                            .text_xs()
                            .text_color(muted)
                            .truncate()
                            .child(locked_by),
                    )
                    .child(
                        Button::new(("lfs-lock", i))
                            .small()
                            .ghost()
                            .label("Lock")
                            .disabled(!lock_enabled)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.lfs_lock_file(lock_path.clone(), window, cx)
                            })),
                    )
                    .child(
                        Button::new(("lfs-unlock", i))
                            .small()
                            .ghost()
                            .label("Unlock")
                            .disabled(!unlock_enabled)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.lfs_unlock_file(unlock_path.clone(), window, cx)
                            })),
                    )
                    .into_any_element()
            })
            .collect()
    }

    /// The persistent banner for repositories that use LFS while `git-lfs`
    /// is not installed (M14 §5).
    pub fn lfs_banner(&self, cx: &mut Context<Self>) -> Option<Div> {
        let Some(Ok(repo)) = self.repo.as_ref() else {
            return None;
        };
        if !repo.lfs_in_use || repo.lfs_installed {
            return None;
        }
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
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("This repository uses Git LFS but git-lfs is not installed"),
                )
                .child(
                    Button::new("lfs-install")
                        .small()
                        .ghost()
                        .label("Install git-lfs")
                        .on_click(cx.listener(|_, _, _, cx| {
                            cx.open_url("https://github.com/git-lfs/git-lfs#install")
                        })),
                ),
        )
    }
}

/// A unique path for `GIT_LFS_PROGRESS`; git-lfs creates the file itself.
fn lfs_progress_file() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "spork-lfs-progress-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// State of the LFS status view: the tracked files, the optional locks (a
/// missing locking server leaves this `None`), and load state.
#[derive(Debug, Clone, Default)]
pub struct LfsStatus {
    pub files: Vec<LfsFile>,
    pub locks: Option<Vec<LfsLock>>,
    pub loading: bool,
    pub error: Option<String>,
}

impl LfsStatus {
    fn loading() -> Self {
        Self {
            loading: true,
            ..Default::default()
        }
    }
}

/// The fetch/pull dialog body: a remote select.
struct LfsRemoteDialog {
    remote: Entity<SelectState<SearchableVec<String>>>,
}

impl LfsRemoteDialog {
    fn new(remotes: Vec<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let remote = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(remotes),
                Some(IndexPath::new(0)),
                window,
                cx,
            )
        });
        Self { remote }
    }

    fn remote(&self, cx: &App) -> String {
        self.remote
            .read(cx)
            .selected_value()
            .cloned()
            .unwrap_or_default()
    }
}

impl Render for LfsRemoteDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child("Fetch the LFS objects stored on one remote."),
            )
            .child(Select::new(&self.remote))
    }
}

/// The track/untrack dialog body: a pattern input plus, for tracking, the
/// preview of tracked files the pattern would take over.
struct LfsPatternDialog {
    pattern: Entity<InputState>,
    preview: Option<Vec<String>>,
}

impl LfsPatternDialog {
    fn new(prefill: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let pattern = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("e.g. *.png");
            if let Some(p) = prefill {
                input.set_value(p, window, cx);
            }
            input
        });
        Self {
            pattern,
            preview: None,
        }
    }

    fn pattern(&self, cx: &App) -> String {
        self.pattern.read(cx).value().trim().to_owned()
    }

    fn has_preview(&self) -> bool {
        self.preview.is_some()
    }

    fn set_preview(&mut self, files: Vec<String>, cx: &mut Context<Self>) {
        self.preview = Some(files);
        cx.notify();
    }
}

impl Render for LfsPatternDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let mut body = v_flex()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child("Glob pattern of files to apply Git LFS tracking to."),
            )
            .child(Input::new(&self.pattern));
        if let Some(files) = &self.preview {
            body = body.child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(format!("{} tracked file(s) match:", files.len())),
            );
            if files.is_empty() {
                body = body.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child("No tracked files match the pattern."),
                );
            } else {
                let list = v_flex()
                    .id("lfs-track-preview")
                    .gap_0p5()
                    .max_h(px(180.))
                    .overflow_y_scroll()
                    .children(
                        files
                            .iter()
                            .take(500)
                            .map(|f| div().text_xs().truncate().child(f.clone())),
                    );
                body = body.child(list);
            }
        }
        body
    }
}

/// Builds the "Git LFS" submenu of the repository switcher (M14 §1).
pub fn lfs_submenu(
    menu: PopupMenu,
    state: LfsMenuState,
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
    if !state.in_use {
        return menu.item(PopupMenuItem::new("Not an LFS repository").disabled(true));
    }
    let status_view = view.clone();
    let status = PopupMenuItem::new("Status".to_owned()).on_click(move |_, _window, cx| {
        status_view.update(cx, |this, cx| this.lfs_open_status(cx))
    });
    menu.item(item(
        "Install hooks",
        lfs_install_hooks_enabled(state.in_use, state.hooks),
        crate::RepoView::lfs_install_hooks,
    ))
    .item(item(
        "Fetch…",
        lfs_remote_op_enabled(state.in_use, &state.remotes),
        crate::RepoView::lfs_fetch_dialog,
    ))
    .item(item(
        "Pull…",
        lfs_remote_op_enabled(state.in_use, &state.remotes),
        crate::RepoView::lfs_pull_dialog,
    ))
    .item(PopupMenuItem::separator())
    .item(item(
        "Track…",
        lfs_track_op_enabled(state.in_use),
        crate::RepoView::lfs_track_dialog,
    ))
    .item(item(
        "Untrack…",
        lfs_track_op_enabled(state.in_use),
        crate::RepoView::lfs_untrack_dialog,
    ))
    .item(PopupMenuItem::separator())
    .item(status)
}

#[cfg(test)]
mod tests {
    use super::{
        LfsFile, LfsLock, Ref, RefKind, is_lfs_path, lfs_format_size, lfs_install_hooks_enabled,
        lfs_join_rows, lfs_pattern_from_path, lfs_remote_names, lfs_remote_op_enabled,
        lfs_row_actions, lfs_track_op_enabled,
    };

    fn file(path: &str, oid: &str, size: u64, downloaded: bool) -> LfsFile {
        LfsFile {
            path: path.to_owned(),
            oid: oid.to_owned(),
            size,
            downloaded,
        }
    }

    fn lock(path: &str, owner: Option<&str>, mine: bool) -> LfsLock {
        LfsLock {
            path: path.to_owned(),
            id: "id".to_owned(),
            locked_at: None,
            owner: owner.map(str::to_owned),
            mine,
        }
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(lfs_format_size(0), "0 B");
        assert_eq!(lfs_format_size(512), "512 B");
        assert_eq!(lfs_format_size(1024), "1.0 KB");
        assert_eq!(lfs_format_size(1536), "1.5 KB");
        assert_eq!(lfs_format_size(10_485_760), "10.0 MB");
        assert_eq!(lfs_format_size(3_221_225_472), "3.0 GB");
    }

    #[test]
    fn pattern_prefill_uses_the_last_path_segment() {
        assert_eq!(lfs_pattern_from_path("a/b/c.png"), Some("*.png".to_owned()));
        assert_eq!(
            lfs_pattern_from_path("dir/archive.tar.gz"),
            Some("*.gz".to_owned())
        );
        assert_eq!(lfs_pattern_from_path("readme"), None);
        assert_eq!(lfs_pattern_from_path("a/."), None);
        assert_eq!(lfs_pattern_from_path("a/b.c d"), None);
    }

    fn ref_of(name: &str, kind: RefKind) -> Ref {
        Ref {
            kind,
            full: format!("refs/remotes/{name}"),
            name: name.to_owned(),
            target: "t".to_owned(),
            commit: "c".to_owned(),
            upstream: None,
            ahead: 0,
            behind: 0,
            upstream_gone: false,
            is_head: false,
        }
    }

    #[test]
    fn lfs_path_matches_basename_and_slash_patterns() {
        // a pattern without a slash matches the file name at any depth
        assert!(is_lfs_path("b.png", &["*.png".to_owned()]));
        assert!(is_lfs_path("assets/a/b.png", &["*.png".to_owned()]));
        // a pattern with a slash matches the full path
        assert!(is_lfs_path("images/x.png", &["images/**".to_owned()]));
        assert!(is_lfs_path("images/sub/y.png", &["images/**".to_owned()]));
        // no match, empty patterns, and a broken pattern never match or panic
        assert!(!is_lfs_path("a.txt", &["*.png".to_owned()]));
        assert!(!is_lfs_path("other/x.png", &["images/**".to_owned()]));
        assert!(!is_lfs_path("a.png", &[]));
        assert!(!is_lfs_path("a.png", &["[".to_owned()]));
    }

    #[test]
    fn remote_names_take_the_suffix() {
        let refs = vec![
            ref_of("origin/main", RefKind::Remote),
            ref_of("origin/dev", RefKind::Remote),
            ref_of("main", RefKind::Local),
        ];
        assert_eq!(lfs_remote_names(&refs), vec!["main", "dev"]);
    }

    #[test]
    fn joins_files_with_locks_by_path() {
        let files = vec![
            file("a.bin", "aaaaaaaaaa", 10, true),
            file("b.bin", "bbbbbbbbbb", 20, false),
            file("c.bin", "cccccccccc", 30, true),
        ];
        let locks = vec![lock("b.bin", Some("Ada"), false), lock("a.bin", None, true)];
        let rows = lfs_join_rows(&files, Some(&locks));
        assert_eq!(rows[0].path, "a.bin");
        assert!(rows[0].locked && rows[0].lock_mine);
        assert_eq!(rows[0].locked_by.as_deref(), Some("you"));
        assert_eq!(rows[1].path, "b.bin");
        assert!(rows[1].locked && !rows[1].lock_mine);
        assert_eq!(rows[1].locked_by.as_deref(), Some("Ada"));
        assert!(!rows[1].downloaded);
        assert!(!rows[2].locked);
        assert_eq!(rows[2].locked_by, None);
    }

    #[test]
    fn joins_without_lock_data() {
        let files = vec![file("a.bin", "aaaaaaaaaa", 10, true)];
        let rows = lfs_join_rows(&files, None);
        assert!(!rows[0].locked);
        assert_eq!(rows[0].locked_by, None);
    }

    #[test]
    fn row_action_enablement() {
        // Unlocked: lock offered, unlock not.
        let (l, u, f) = lfs_row_actions(false, false);
        assert!(l && !u && !f);
        // Ours: unlock offered, no force.
        let (l, u, f) = lfs_row_actions(true, true);
        assert!(!l && u && !f);
        // Someone else's: unlock offered, force needed.
        let (l, u, f) = lfs_row_actions(true, false);
        assert!(!l && u && f);
    }

    #[test]
    fn submenu_enablement() {
        assert!(lfs_install_hooks_enabled(true, false));
        assert!(!lfs_install_hooks_enabled(true, true));
        assert!(!lfs_install_hooks_enabled(false, false));

        assert!(lfs_remote_op_enabled(true, &["origin".to_owned()]));
        assert!(!lfs_remote_op_enabled(true, &[]));
        assert!(!lfs_remote_op_enabled(false, &["origin".to_owned()]));

        assert!(lfs_track_op_enabled(true));
        assert!(!lfs_track_op_enabled(false));
    }
}
