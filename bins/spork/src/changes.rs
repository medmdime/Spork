//! Working copy: unstaged / staged files, per-file diff, commit box (docs/specs/M07).

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::diff::{Diff, DiffFile, DiffFileStatus, DiffSide};
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, h_resizable,
    resizable_panel, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use spork_git::{GitError, PatchTarget, StatusEntry};
use spork_patch::{FilePatch, Side};

use crate::RepoView;
use crate::binary::{BinaryPane, BinaryTarget};

const ROW_HEIGHT: f32 = 26.;

/// The file chosen for the diff pane: its path and which side (staged or not) it was picked from.
#[derive(Clone, PartialEq, Eq)]
pub struct ChangeSelection {
    pub path: String,
    pub staged: bool,
}

/// Status letter shown in the file lists, with its colour.
fn status_letter(entry: &StatusEntry, staged: bool, cx: &App) -> (char, Hsla) {
    let theme = cx.theme();
    let letter = match (entry.conflicted, entry.untracked(), staged) {
        (true, _, _) => 'U',
        (_, true, _) => '?',
        (_, _, true) => entry.index,
        (_, _, false) => entry.worktree,
    };
    let color = match letter {
        'A' | '?' => theme.green,
        'D' | 'U' => theme.red,
        'R' | 'C' => theme.blue,
        _ => theme.yellow,
    };
    (letter, color)
}

impl RepoView {
    fn status_entries(&self, staged: bool) -> Vec<&StatusEntry> {
        let Some(Ok(repo)) = &self.repo else {
            return Vec::new();
        };
        repo.status
            .iter()
            .filter(|e| {
                if staged {
                    e.has_staged()
                } else {
                    e.has_unstaged()
                }
            })
            .collect()
    }

    /// Runs a git write in the background, then reloads; failures become a notification.
    pub(crate) fn run_op<T: Send + 'static>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        op: impl FnOnce(&std::path::Path) -> Result<T, GitError> + Send + 'static,
        on_success: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let Some(Ok(repo)) = &self.repo else { return };
        let root = repo.root.clone();
        self.busy = true;
        cx.notify();
        self._op = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(async move { op(&root) }).await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(value) => on_success(this, value, window, cx),
                    Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
                }
                this.reload(cx);
            })
            .ok();
        }));
    }

    fn toggle_stage(
        &mut self,
        paths: Vec<String>,
        staged: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_op(
            window,
            cx,
            move |root| {
                let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
                if staged {
                    spork_git::unstage(root, &paths)
                } else {
                    spork_git::stage(root, &paths)
                }
            },
            |_, _, _, _| {},
        );
    }

    /// Asks before throwing away unstaged changes; untracked files go to the Trash (M07 §3).
    fn confirm_discard(&mut self, paths: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let entries: Vec<StatusEntry> = self
            .status_entries(false)
            .into_iter()
            .filter(|e| paths.contains(&e.path) && !e.conflicted)
            .cloned()
            .collect();
        if entries.is_empty() {
            return;
        }
        let untracked = entries.iter().filter(|e| e.untracked()).count();
        let what = match entries.as_slice() {
            [one] => one.path.clone(),
            many => format!("{} files", many.len()),
        };
        let note = match untracked {
            0 => "Unstaged changes are lost. Staged changes stay.".to_owned(),
            n => format!("Unstaged changes are lost; {n} untracked file(s) go to the Trash."),
        };
        let view = cx.entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (view, entries) = (view.clone(), entries.clone());
            dialog
                .title(format!("Discard changes to {what}?"))
                .description(note.clone())
                .confirm()
                .ok_text("Discard")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let entries = entries.clone();
                    view.update(cx, |this, cx| {
                        this.run_op(
                            window,
                            cx,
                            move |root| spork_git::discard(root, &entries),
                            |_, _, _, _| {},
                        )
                    });
                    true
                })
        });
    }

    /// Commit; with LFS in use, `git lfs locks --verify` runs first and a
    /// violation asks for confirmation before the commit (M14 edge cases).
    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let subject = self.subject.read(cx).value().trim().to_owned();
        let body = self.description.read(cx).value().trim().to_owned();
        if subject.is_empty() {
            window.push_notification(Notification::error("Write a commit subject first."), cx);
            return;
        }
        let message = if body.is_empty() {
            subject
        } else {
            format!("{subject}\n\n{body}")
        };
        let amend = self.amend;
        let verify = self.lfs_in_use();
        let (lfs_message, lfs_amend) = (message.clone(), amend);
        self.run_op(
            window,
            cx,
            move |root| {
                // one background task: verify the locks, then commit unless the user is asked
                if verify {
                    let warnings = spork_git::lfs::lfs_locks_verify(root);
                    if !warnings.is_empty() {
                        return Ok(Some(warnings));
                    }
                }
                spork_git::commit(root, &message, amend)?;
                Ok(None)
            },
            move |this, warnings, window, cx| match warnings {
                Some(warnings) => {
                    this.confirm_lfs_lock_commit(lfs_message, lfs_amend, warnings, window, cx)
                }
                None => this.clear_commit_box(window, cx),
            },
        );
    }

    /// Confirms a commit whose `git lfs locks --verify` found violations (M14
    /// edge cases): the commit only runs on "Proceed".
    fn confirm_lfs_lock_commit(
        &mut self,
        message: String,
        amend: bool,
        warnings: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (view, value) = (view.clone(), message.clone());
            dialog
                .title("Locked files changed elsewhere")
                .description(warnings.join("\n"))
                .confirm()
                .show_cancel(true)
                .ok_text("Proceed")
                .cancel_text("Cancel")
                .on_ok(move |_, window, cx| {
                    // the dialog callback is `Fn` (one per open), so clone the message
                    let value = value.clone();
                    view.update(cx, |this, cx| {
                        this.run_op(
                            window,
                            cx,
                            move |root| spork_git::commit(root, &value, amend),
                            |this, _, window, cx| this.clear_commit_box(window, cx),
                        )
                    });
                    true
                })
        });
    }

    /// Clears the commit box after a successful commit.
    fn clear_commit_box(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.subject.update(cx, |s, cx| s.set_value("", window, cx));
        self.description
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.amend = false;
    }

    fn set_amend(&mut self, amend: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.amend = amend;
        let empty = self.subject.read(cx).value().is_empty()
            && self.description.read(cx).value().is_empty();
        if let (true, true, Some(Ok(repo))) = (amend, empty, &self.repo) {
            // prefill with HEAD's message, like `git commit --amend` does
            if let Some(message) = spork_git::head_message(&repo.root) {
                let (subject, body) = message.split_once("\n\n").unwrap_or((&message, ""));
                let (subject, body) = (subject.to_owned(), body.to_owned());
                self.subject
                    .update(cx, |s, cx| s.set_value(subject, window, cx));
                self.description
                    .update(cx, |s, cx| s.set_value(body, window, cx));
            }
        }
        cx.notify();
    }

    /// Selects a changed file and loads its diff in the background.
    pub fn select_change(&mut self, selection: Option<ChangeSelection>, cx: &mut Context<Self>) {
        self.change_selected = selection.clone();
        let entry = selection.as_ref().and_then(|sel| {
            self.status_entries(sel.staged)
                .into_iter()
                .find(|e| e.path == sel.path)
                .cloned()
        });
        let (Some(sel), Some(entry), Some(Ok(repo))) = (selection, entry, &self.repo) else {
            self.change_selected = None;
            self.change_diff
                .update(cx, |s, cx| s.set_files(Vec::new(), cx));
            self.show_binary(BinaryPane::Changes, None, cx);
            cx.notify();
            return;
        };
        let root = repo.root.clone();
        // unstaged and untracked changes compare against the working-tree file itself
        let worktree = !sel.staged;
        self._change_diff_load = Some(cx.spawn(async move |this, cx| {
            let conflicted = entry.conflicted;
            let path = entry.path.clone();
            let binary_path = entry.path.clone();
            let text = cx
                .background_spawn(async move {
                    if conflicted {
                        // the working file with conflict markers, for the 3-way resolver (M11 §4)
                        std::fs::read_to_string(root.join(&entry.path)).map_err(|e| {
                            GitError::Other {
                                code: None,
                                stderr: e.to_string(),
                            }
                        })
                    } else {
                        spork_git::file_diff(&root, &entry, sel.staged)
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                let files = match (&text, conflicted) {
                    (Ok(t), true) => DiffFile::parse_conflicts(path, t)
                        .map(|f| vec![f])
                        .unwrap_or_default(),
                    (Ok(p), false) => DiffFile::parse(p).unwrap_or_default(),
                    _ => Vec::new(),
                };
                // binary (or LFS pointer) changes show the binary / image panel (M08 §3)
                let binary = match (&text, conflicted) {
                    (Ok(p), false) => BinaryTarget::from_patch(p, &binary_path, worktree),
                    _ => None,
                };
                this.show_binary(BinaryPane::Changes, binary, cx);
                this.change_patch = if conflicted { None } else { text.ok() };
                this.change_diff.update(cx, |s, cx| {
                    s.set_files(files, cx);
                    s.set_selected_lines(None, cx);
                });
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn file_rows(
        &mut self,
        staged: bool,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<Stateful<Div>> {
        let entries: Vec<StatusEntry> = self.status_entries(staged).into_iter().cloned().collect();
        let theme = cx.theme();
        let patterns = self.lfs_patterns();
        let (muted, active, hover) = (theme.muted_foreground, theme.list_active, theme.list_hover);
        range
            .filter_map(|ix| entries.get(ix).cloned().map(|e| (ix, e)))
            .map(|(ix, entry)| {
                let (letter, color) = status_letter(&entry, staged, cx);
                let is_lfs = crate::lfs::is_lfs_path(&entry.path, patterns);
                let (dir, name) = match entry.path.rsplit_once('/') {
                    Some((dir, name)) => (format!("{dir}/"), name.to_owned()),
                    None => (String::new(), entry.path.clone()),
                };
                let selection = ChangeSelection {
                    path: entry.path.clone(),
                    staged,
                };
                let is_selected = self.change_selected.as_ref() == Some(&selection);
                let path = entry.path.clone();
                h_flex()
                    .id(SharedString::from(format!(
                        "{}-{ix}",
                        if staged { "s" } else { "u" }
                    )))
                    .w_full()
                    .h(px(ROW_HEIGHT))
                    .px_3()
                    .gap_2()
                    .text_sm()
                    .when(is_selected, |r| r.bg(active))
                    .hover(move |r| r.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_change(Some(selection.clone()), cx)
                    }))
                    .child(
                        div()
                            .w(px(12.))
                            .flex_none()
                            .text_xs()
                            .text_color(color)
                            .child(letter.to_string()),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(div().flex_none().child(name))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(dir),
                            ),
                    )
                    .when(is_lfs, |row| row.child(crate::lfs::lfs_badge(cx.theme())))
                    .when(entry.conflicted, |row| {
                        let (mine, theirs) = (entry.path.clone(), entry.path.clone());
                        row.child(
                            Button::new(SharedString::from(format!("ours-{ix}")))
                                .ghost()
                                .xsmall()
                                .label("Keep current")
                                .tooltip("Resolve with the current branch's version (git checkout --ours)")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.resolve_conflict(mine.clone(), true, window, cx)
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("theirs-{ix}")))
                                .ghost()
                                .xsmall()
                                .label("Take incoming")
                                .tooltip("Resolve with the version being merged or applied (git checkout --theirs)")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.resolve_conflict(theirs.clone(), false, window, cx)
                                })),
                        )
                    })
                    .when(!staged && !entry.conflicted, |row| {
                        let path = entry.path.clone();
                        row.child(
                            Button::new(SharedString::from(format!("discard-{ix}")))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Undo2)
                                .tooltip("Discard changes…")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.confirm_discard(vec![path.clone()], window, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new(SharedString::from(format!("toggle-{}-{ix}", staged)))
                            .ghost()
                            .xsmall()
                            .icon(if staged {
                                IconName::Minus
                            } else {
                                IconName::Plus
                            })
                            .tooltip(if staged { "Unstage" } else { "Stage" })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.toggle_stage(vec![path.clone()], staged, window, cx)
                            })),
                    )
            })
            .collect()
    }

    fn file_section(&mut self, staged: bool, cx: &mut Context<Self>) -> Div {
        let paths: Vec<String> = self
            .status_entries(staged)
            .iter()
            .map(|e| e.path.clone())
            .collect();
        let count = paths.len();
        let theme = cx.theme();
        let header = h_flex()
            .h(px(32.))
            .flex_none()
            .px_3()
            .justify_between()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(format!(
                "{} ({count})",
                if staged { "Staged" } else { "Unstaged" }
            ))
            .when(count > 0, |h| {
                let all = paths.clone();
                h.child(
                    h_flex()
                        .gap_1()
                        .when(!staged, |h| {
                            h.child(
                                Button::new("discard-all")
                                    .ghost()
                                    .xsmall()
                                    .label("Discard all")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.confirm_discard(all.clone(), window, cx)
                                    })),
                            )
                        })
                        .child(
                            Button::new(if staged { "unstage-all" } else { "stage-all" })
                                .ghost()
                                .xsmall()
                                .label(if staged { "Unstage all" } else { "Stage all" })
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.toggle_stage(paths.clone(), staged, window, cx)
                                })),
                        ),
                )
            });
        let list = uniform_list(
            if staged {
                "staged-files"
            } else {
                "unstaged-files"
            },
            count,
            cx.processor(move |this, range, _, cx| this.file_rows(staged, range, cx)),
        )
        .flex_1()
        .min_h_0();
        v_flex().flex_1().min_h_0().child(header).child(list)
    }

    fn commit_box(&mut self, cx: &mut Context<Self>) -> Div {
        let staged = self.status_entries(true).len();
        let theme = cx.theme();
        let label = match (self.amend, staged) {
            (true, _) => "Amend last commit".to_owned(),
            (false, 1) => "Commit 1 file".to_owned(),
            (false, n) => format!("Commit {n} files"),
        };
        v_flex()
            .flex_none()
            .p_3()
            .gap_2()
            .border_t_1()
            .border_color(theme.border)
            .child(Input::new(&self.subject))
            .child(Textarea::new(&self.description).h(px(76.)))
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        Checkbox::new("amend")
                            .label("Amend")
                            .checked(self.amend)
                            .on_click(cx.listener(|this, checked: &bool, window, cx| {
                                this.set_amend(*checked, window, cx)
                            })),
                    )
                    .child(
                        Button::new("commit")
                            .primary()
                            .small()
                            .label(label)
                            .loading(self.busy)
                            .disabled(self.busy || (staged == 0 && !self.amend))
                            .on_click(cx.listener(|this, _, window, cx| this.commit(window, cx))),
                    ),
            )
    }

    /// Selected diff lines → display indices in the open file's patch, if a selection exists.
    fn selected_patch_lines(
        &self,
        cx: &App,
    ) -> Option<(FilePatch, std::ops::RangeInclusive<usize>)> {
        let range = self.change_diff.read(cx).selected_lines()?;
        let patch = FilePatch::parse(self.change_patch.as_deref()?)?;
        let side = |s: DiffSide| match s {
            DiffSide::Original => Side::Old,
            DiffSide::Modified => Side::New,
        };
        let lines = patch.range(
            (side(range.side()), range.start()),
            (side(range.end_side()), range.end()),
        )?;
        Some((patch, lines))
    }

    /// Stage / unstage / discard only the selected lines (M07 §4).
    fn apply_selection(
        &mut self,
        target: PatchTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((patch, lines)) = self.selected_patch_lines(cx) else {
            return;
        };
        let reverse = target != PatchTarget::Stage;
        let Some(partial) = patch.partial(|i, _| lines.contains(&i), reverse) else {
            window.push_notification(
                Notification::error("The selection contains no changed lines."),
                cx,
            );
            return;
        };
        self.run_op(
            window,
            cx,
            move |root| spork_git::apply_patch(root, &partial, target),
            |this, _, _, cx| {
                this.change_diff
                    .update(cx, |s, cx| s.set_selected_lines(None, cx))
            },
        );
    }

    /// Writes the file as resolved in the conflict view and marks it resolved (stages it).
    fn save_resolution(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.change_diff.read(cx).resolved_text(&path) else {
            return;
        };
        self.run_op(
            window,
            cx,
            move |root| {
                std::fs::write(root.join(&path), text).map_err(|e| GitError::Other {
                    code: None,
                    stderr: e.to_string(),
                })?;
                spork_git::stage(root, &[&path])
            },
            |_, _, window, cx| {
                window.push_notification(Notification::success("Conflict resolved"), cx)
            },
        );
    }

    /// Bar above a conflicted file: progress of the per-conflict choices and "Save resolution".
    fn conflict_bar(&self, path: &str, cx: &mut Context<Self>) -> Option<Div> {
        let state = self.change_diff.read(cx);
        let file = state.files().iter().find(|f| f.path().as_ref() == path)?;
        if file.status() != DiffFileStatus::Conflicted {
            return None;
        }
        let total = file.conflicts().len();
        let done = (0..total)
            .filter(|&i| state.conflict_resolution(path, i).is_some())
            .count();
        let resolved = state.resolved_text(path);
        let theme = cx.theme();
        let (path, ready) = (path.to_owned(), resolved.is_some());
        Some(
            h_flex()
                .flex_none()
                .h(px(36.))
                .px_3()
                .justify_between()
                .border_b_1()
                .border_color(theme.border)
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!("{done} of {total} resolved"))
                .child(
                    Button::new("save-resolution")
                        .small()
                        .primary()
                        .label("Save resolution")
                        .disabled(!ready || self.busy)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.save_resolution(path.clone(), window, cx)
                        })),
                ),
        )
    }

    /// Bar above the diff with actions for the selected lines.
    fn selection_bar(&self, staged: bool, cx: &mut Context<Self>) -> Option<Div> {
        let (patch, lines) = self.selected_patch_lines(cx)?;
        let changed = patch
            .lines()
            .enumerate()
            .filter(|(i, l)| lines.contains(i) && l.is_change())
            .count();
        let theme = cx.theme();
        let bar = h_flex()
            .flex_none()
            .h(px(36.))
            .px_3()
            .gap_2()
            .justify_between()
            .border_b_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.muted_foreground);
        if !patch.supports_partial() {
            return Some(bar.child("Deleted files can only be staged as a whole."));
        }
        let label = match changed {
            1 => "1 line selected".to_owned(),
            n => format!("{n} lines selected"),
        };
        let action = |id: &'static str, text: &'static str, target: PatchTarget| {
            Button::new(id)
                .small()
                .label(text)
                .disabled(changed == 0 || self.busy)
                .on_click(cx.listener(move |this, _, window, cx| match target {
                    PatchTarget::Discard => this.confirm_discard_lines(window, cx),
                    _ => this.apply_selection(target, window, cx),
                }))
        };
        let buttons = if staged {
            h_flex()
                .gap_2()
                .child(action("unstage-lines", "Unstage lines", PatchTarget::Unstage).primary())
        } else {
            h_flex()
                .gap_2()
                .child(action("discard-lines", "Discard lines…", PatchTarget::Discard).ghost())
                .child(action("stage-lines", "Stage lines", PatchTarget::Stage).primary())
        };
        Some(bar.child(label).child(buttons))
    }

    fn confirm_discard_lines(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            dialog
                .title("Discard the selected lines?")
                .description("These changes are removed from the file and can't be recovered.")
                .confirm()
                .ok_text("Discard")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    view.update(cx, |this, cx| {
                        this.apply_selection(PatchTarget::Discard, window, cx)
                    });
                    true
                })
        });
    }

    pub fn changes_view(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let left = v_flex()
            .size_full()
            .child(self.file_section(false, cx))
            .child(div().h(px(1.)).flex_none().bg(cx.theme().border))
            .child(self.file_section(true, cx))
            .child(self.commit_box(cx));
        let right = match &self.change_selected {
            Some(sel) => {
                let patterns = self.lfs_patterns().to_vec();
                v_flex()
                    .size_full()
                    .children(self.conflict_bar(&sel.path, cx))
                    .children(self.selection_bar(sel.staged, cx))
                    .map(|pane| {
                        if self.binary_active(BinaryPane::Changes, cx) {
                            return pane.child(self.change_binary.clone());
                        }
                        pane.child(
                            Diff::new(&self.change_diff)
                                .size_full()
                                .render_header_title_suffix(move |document, _, app| {
                                    if crate::lfs::is_lfs_path(document.path().as_ref(), &patterns)
                                    {
                                        crate::lfs::lfs_badge(app.theme())
                                    } else {
                                        div()
                                    }
                                }),
                        )
                    })
            }
            None => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(muted)
                .child(
                    if self.status_entries(false).is_empty() && self.status_entries(true).is_empty()
                    {
                        "Working tree clean"
                    } else {
                        "Select a file to see its changes"
                    },
                ),
        };
        h_resizable("changes-split")
            .child(resizable_panel().size(px(380.)).child(left))
            .child(resizable_panel().child(right))
            .into_any_element()
    }
}
