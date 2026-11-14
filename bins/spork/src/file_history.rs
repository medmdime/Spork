//! File history (docs/specs/M08 §6): the commits of one path, following renames,
//! the diff of the selected commit's change to the file, and the file revision
//! actions (open at revision, save as, checkout this version).

use std::ops::Range;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::diff::{Diff, DiffFile};
use gpui_kit::component::input::Input;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, WindowExt as _, h_flex, h_resizable, resizable_panel, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use spork_git::{FileCommit, GitError};

use crate::history::relative_age;
use crate::{RepoView, View};

/// One list row is two lines (subject, author · age · rename note).
const ROW_HEIGHT: f32 = 52.;

/// The open file history (M08 §6): the commits of one path, newest first.
pub struct FileHistory {
    /// The path the history was opened for (the file's current name).
    pub path: String,
    /// The view to return to on Back.
    pub from: View,
    pub entries: Vec<FileCommit>,
    pub selected: Option<usize>,
    /// The commit list is still loading.
    pub running: bool,
    /// The commit list load failed.
    pub error: Option<String>,
    /// The raw diff of the selected row's change to the file; `None` while it loads.
    pub diff: Option<Result<String, GitError>>,
    /// The "Open at revision" pane: the file's content, or the load error.
    pub content: Option<Result<String, String>>,
}

impl RepoView {
    /// Opens the file history of `path` (M08 §6), loaded in the background.
    pub(crate) fn start_file_history(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let root = repo.root.clone();
        // a new history supersedes the running one: kill its git child and drop
        // the in-flight loads (dropped tasks never complete)
        self.file_history_cancel.kill();
        let cancel = spork_git::Cancellable::default();
        self.file_history_cancel = cancel.clone();
        self._file_history_diff_load = None;
        self._file_history_content_load = None;
        let from = self.view;
        let opened_path = path.clone();
        self.file_history = Some(FileHistory {
            path,
            from,
            entries: Vec::new(),
            selected: None,
            running: true,
            error: None,
            diff: None,
            content: None,
        });
        self.file_history_diff
            .update(cx, |state, cx| state.set_files(Vec::new(), cx));
        self.view = View::FileHistory;
        self._file_history_task = Some(cx.spawn(async move |this, cx| {
            let p = opened_path.clone();
            let entries = cx
                .background_spawn(async move { spork_git::file_history(&root, &p, &cancel) })
                .await;
            this.update(cx, |this, cx| {
                let Some(fh) = this.file_history.as_mut() else {
                    return;
                };
                // a stale completion for a superseded history: drop it
                if fh.path != opened_path {
                    return;
                }
                fh.running = false;
                match entries {
                    Ok(list) => {
                        fh.entries = list;
                        // auto-select the newest commit and load its diff
                        if !fh.entries.is_empty() {
                            fh.selected = Some(0);
                            this.load_file_history_diff(0, cx);
                        }
                    }
                    Err(e) => fh.error = Some(e.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Selects a history row and loads the diff of its change to the file (M08 §6).
    pub(crate) fn select_file_history(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(fh) = self.file_history.as_ref() else {
            return;
        };
        if fh.entries.get(ix).is_none() {
            return;
        }
        if let Some(fh) = self.file_history.as_mut() {
            fh.selected = Some(ix);
            fh.content = None; // back to the diff pane
        }
        self.load_file_history_diff(ix, cx);
        cx.notify();
    }

    /// Loads the diff of history row `ix` (M08 §6). A renamed file is diffed
    /// between its old and new name so the hunks survive; a root commit diffs
    /// against the empty tree.
    fn load_file_history_diff(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let Some(entry) = self.file_history.as_ref().and_then(|fh| fh.entries.get(ix)) else {
            return;
        };
        let root = repo.root.clone();
        let (oid, path, renamed_from) = (
            entry.oid.clone(),
            entry.path.clone(),
            entry.renamed_from.clone(),
        );
        // replacing the task cancels a load for a previously clicked row
        self._file_history_diff_load = Some(cx.spawn(async move |this, cx| {
            let patch = cx
                .background_spawn(async move {
                    spork_git::file_history_diff(&root, &oid, &path, renamed_from.as_deref())
                })
                .await;
            this.update(cx, |this, cx| {
                let Some(fh) = this.file_history.as_mut() else {
                    return;
                };
                if fh.selected != Some(ix) {
                    return; // the selection moved while the diff was loading
                }
                let files = match &patch {
                    Ok(p) if !p.is_empty() && p.len() <= crate::MAX_PATCH_BYTES => {
                        DiffFile::parse(p).unwrap_or_default()
                    }
                    _ => Vec::new(),
                };
                this.file_history_diff
                    .update(cx, |state, cx| state.set_files(files, cx));
                fh.diff = Some(patch);
                cx.notify();
            })
            .ok();
        }));
    }

    /// "Open at revision" (M08 §6): loads the file's content at the selected
    /// commit, under the name the file had in that commit.
    fn open_revision(&mut self, cx: &mut Context<Self>) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let Some(fh) = self.file_history.as_ref() else {
            return;
        };
        let Some(ix) = fh.selected else {
            return;
        };
        let Some(entry) = fh.entries.get(ix) else {
            return;
        };
        let root = repo.root.clone();
        let (oid, path) = (entry.oid.clone(), entry.path.clone());
        self._file_history_content_load = Some(cx.spawn(async move |this, cx| {
            let content = cx
                .background_spawn(async move { spork_git::show_file(&root, &oid, &path) })
                .await;
            this.update(cx, |this, cx| {
                let Some(fh) = this.file_history.as_mut() else {
                    return;
                };
                if fh.selected != Some(ix) {
                    return;
                }
                fh.content = Some(
                    content
                        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                        .map_err(|e| e.to_string()),
                );
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// "Save as…" (M08 §6): writes the selected revision's content to a
    /// user-chosen path (relative paths are taken against the repository).
    fn save_revision(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let Some(fh) = self.file_history.as_ref() else {
            return;
        };
        let Some(entry) = fh.selected.and_then(|ix| fh.entries.get(ix)) else {
            return;
        };
        let (oid, path) = (entry.oid.clone(), entry.path.clone());
        let default = repo.root.join(&fh.path).to_string_lossy().into_owned();
        let muted = cx.theme().muted_foreground;
        self.file_history_save
            .update(cx, |s, cx| s.set_value(&default, window, cx));
        let (view, input) = (cx.entity(), self.file_history_save.clone());
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (view, input, oid, path, muted) =
                (view.clone(), input.clone(), oid.clone(), path.clone(), muted);
            let short: String = oid.chars().take(7).collect();
            dialog
                .title("Save file as…")
                .description(
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child(format!(
                                    "Writes the version from commit {short} ({path}) to the path. A relative path is taken against the repository."
                                )),
                        )
                        .child(Input::new(&input)),
                )
                .confirm()
                .ok_text("Save")
                .on_ok(move |_, window, cx| {
                    let dest = input.read(cx).value().trim().to_owned();
                    if dest.is_empty() {
                        return false; // stay open until a path is given
                    }
                    let (oid, path) = (oid.clone(), path.clone());
                    let short: String = oid.chars().take(7).collect();
                    view.update(cx, |this, cx| {
                        this.run_op(
                            window,
                            cx,
                            move |root| {
                                let content = spork_git::show_file(root, &oid, &path)?;
                                let dest: std::path::PathBuf =
                                    if std::path::Path::new(&dest).is_absolute() {
                                        std::path::PathBuf::from(&dest)
                                    } else {
                                        root.join(&dest)
                                    };
                                if let Some(parent) = dest.parent() {
                                    std::fs::create_dir_all(parent).map_err(|e| GitError::Other {
                                        code: None,
                                        stderr: e.to_string(),
                                    })?;
                                }
                                std::fs::write(&dest, content).map_err(|e| GitError::Other {
                                    code: None,
                                    stderr: e.to_string(),
                                })?;
                                Ok(format!(
                                    "Saved revision {short} to {}",
                                    dest.display()
                                ))
                            },
                            |_, message, window, cx| {
                                window.push_notification(Notification::success(message), cx)
                            },
                        )
                    });
                    true
                })
        });
    }

    /// "Checkout this version" (M08 §6): confirms, then restores the working-tree
    /// file from the selected commit (unsaved edits to it are lost).
    fn checkout_revision(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(fh) = self.file_history.as_ref() else {
            return;
        };
        let Some(entry) = fh.selected.and_then(|ix| fh.entries.get(ix)) else {
            return;
        };
        let (oid, path) = (entry.oid.clone(), fh.path.clone());
        let view = cx.entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (view, oid, path) = (view.clone(), oid.clone(), path.clone());
            let short: String = oid.chars().take(7).collect();
            dialog
                .title(format!("Replace {path}?"))
                .description(
                    format!(
                        "The working-tree copy is overwritten with the version from commit {short}. Unsaved edits to it are lost."
                    ),
                )
                .confirm()
                .ok_text("Replace")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let (oid, path) = (oid.clone(), path.clone());
                    let short: String = oid.chars().take(7).collect();
                    let message = format!("Restored {path} from {short}");
                    view.update(cx, |this, cx| {
                        this.run_op(
                            window,
                            cx,
                            move |root| spork_git::restore_file(root, &oid, &path),
                            move |_, _, window, cx| {
                                window.push_notification(Notification::success(message.clone()), cx)
                            },
                        )
                    });
                    true
                })
        });
    }

    /// Closes the file history and returns to the view it was opened from (M08 §6).
    fn close_file_history(&mut self, cx: &mut Context<Self>) {
        let from = self
            .file_history
            .as_ref()
            .map(|fh| fh.from)
            .unwrap_or(View::History);
        self.file_history_cancel.kill();
        self._file_history_task = None;
        self._file_history_diff_load = None;
        self._file_history_content_load = None;
        self.file_history = None;
        self.view = from;
        cx.notify();
    }

    /// The File History view (M08 §6): the commit list on the left, the selected
    /// commit's change to the file (or its revision content) on the right.
    pub(crate) fn file_history_view(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(fh) = &self.file_history else {
            return div().into_any_element();
        };
        let list = v_flex()
            .size_full()
            .child(self.file_history_header(cx))
            .child(self.file_history_list(cx, fh));
        let right = v_flex()
            .size_full()
            .child(self.file_history_right(cx, fh))
            .child(self.file_history_actions(cx));
        h_resizable("file-history-split")
            .child(resizable_panel().size(px(320.)).child(list))
            .child(resizable_panel().child(right))
            .into_any_element()
    }

    /// Header of the history list: Back button and the path the history is for.
    fn file_history_header(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        h_flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                Button::new("file-history-back")
                    .ghost()
                    .icon(IconName::ArrowLeft)
                    .on_click(cx.listener(move |this, _, _, cx| this.close_file_history(cx))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .truncate()
                    .child(format!(
                        "History: {}",
                        self.file_history
                            .as_ref()
                            .map(|fh| fh.path.clone())
                            .unwrap_or_default()
                    )),
            )
    }

    /// The commit list, or the loading / error / empty state.
    fn file_history_list(&self, cx: &mut Context<Self>, fh: &FileHistory) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
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
        if fh.running {
            return centered("Loading file history…".into());
        }
        if let Some(error) = &fh.error {
            return centered(error.clone());
        }
        if fh.entries.is_empty() {
            return centered("No commits touch this path.".into());
        }
        uniform_list(
            "file-history",
            fh.entries.len(),
            cx.processor(|this, range, _, cx| this.file_history_rows(range, cx)),
        )
        .flex_1()
        .min_h_0()
        .into_any_element()
    }

    /// Rows for the history list (M08 §6): subject, then author · age, with a
    /// `was: old/path` note on the commit that renamed the file.
    fn file_history_rows(
        &mut self,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(fh) = self.file_history.as_ref() else {
            return Vec::new();
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let theme = cx.theme();
        let (muted, hover, active, renamed) = (
            theme.muted_foreground,
            theme.list_hover,
            theme.list_active,
            theme.yellow,
        );
        range
            .map(|ix| {
                let entry = &fh.entries[ix];
                let meta = format!("{} · {}", entry.author, relative_age(entry.date, now));
                h_flex()
                    .id(ix)
                    .w_full()
                    .h(px(ROW_HEIGHT))
                    .px_3()
                    .when(fh.selected == Some(ix), |row| row.bg(active))
                    .hover(move |row| row.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.select_file_history(ix, cx)))
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(div().text_sm().truncate().child(entry.subject.clone()))
                            .child(
                                h_flex()
                                    .gap_1()
                                    .items_center()
                                    .child(div().text_xs().text_color(muted).truncate().child(meta))
                                    .children(entry.renamed_from.as_ref().map(|old| {
                                        div()
                                            .text_xs()
                                            .text_color(renamed)
                                            .truncate()
                                            .child(format!("was: {old}"))
                                    })),
                            ),
                    )
                    .into_any_element()
            })
            .collect()
    }

    /// The right pane: the revision content when "Open at revision" is active,
    /// otherwise the diff of the selected row's change to the file.
    fn file_history_right(&self, cx: &mut Context<Self>, fh: &FileHistory) -> Div {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let centered = |msg: String| {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(muted)
                .child(msg)
        };
        let pane = v_flex().size_full().border_t_1().border_color(theme.border);
        let Some(_entry) = fh.selected.and_then(|ix| fh.entries.get(ix)) else {
            return pane.child(centered("Select a commit.".into()));
        };
        let body = if let Some(content) = &fh.content {
            match content {
                Err(error) => centered(error.clone()),
                Ok(text) => v_flex().flex_1().min_h_0().child(
                    div()
                        .id("file-history-content")
                        .flex_1()
                        .min_h_0()
                        .px_3()
                        .py_2()
                        .overflow_y_scroll()
                        .text_sm()
                        .child(text.clone()),
                ),
            }
        } else {
            match &fh.diff {
                None => centered("Loading diff…".into()),
                Some(Err(error)) => centered(error.to_string()),
                Some(Ok(patch)) if patch.is_empty() => centered("No changes.".into()),
                Some(Ok(patch)) if patch.len() > crate::MAX_PATCH_BYTES => {
                    centered("This file change is too large to show inline.".into())
                }
                Some(Ok(_)) => {
                    let patterns = self.lfs_patterns().to_vec();
                    v_flex().flex_1().min_h_0().w_full().child(
                        Diff::new(&self.file_history_diff)
                            .size_full()
                            .render_header_title_suffix(move |document, _, app| {
                                let is_lfs =
                                    crate::lfs::is_lfs_path(document.path().as_ref(), &patterns);
                                h_flex()
                                    .items_center()
                                    .gap_1()
                                    .child(document.path().clone())
                                    .when(is_lfs, |d| d.child(crate::lfs::lfs_badge(app.theme())))
                            }),
                    )
                }
            }
        };
        pane.child(body)
    }

    /// Action bar for the selected history entry (M08 §6).
    fn file_history_actions(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        h_flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(theme.border)
            .child(
                Button::new("file-history-open")
                    .ghost()
                    .icon(IconName::Eye)
                    .tooltip("Open this revision read-only")
                    .on_click(cx.listener(move |this, _, _, cx| this.open_revision(cx))),
            )
            .child(
                Button::new("file-history-save")
                    .ghost()
                    .icon(IconName::Save)
                    .tooltip("Save this revision to a path…")
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.save_revision(window, cx)),
                    ),
            )
            .child(
                Button::new("file-history-checkout")
                    .ghost()
                    .with_variant(ButtonVariant::Danger)
                    .icon(IconName::Undo2)
                    .tooltip("Replace the working-tree file with this revision")
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.checkout_revision(window, cx)),
                    ),
            )
            .child(
                Button::new("file-history-blame")
                    .ghost()
                    .icon(IconName::GitCommitHorizontal)
                    .tooltip("Blame this file at the selected revision")
                    .on_click(cx.listener(move |this, _, _, cx| this.blame_at_revision(cx))),
            )
    }
}
