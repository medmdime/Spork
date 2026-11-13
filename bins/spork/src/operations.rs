//! Merge / rebase / cherry-pick / revert flows, the paused-operation banner, commit menu and
//! stashes (docs/specs/M11, M06, M12).

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::sidebar::SidebarMenuItem;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use spork_git::{GitError, OpState, Outcome, Stash};

use crate::{RepoView, View};

impl RepoView {
    /// Runs an operation that may stop on conflicts. Conflicts switch to Local Changes with a hint.
    pub(crate) fn run_outcome(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        done: String,
        op: impl FnOnce(&std::path::Path) -> Result<Outcome, GitError> + Send + 'static,
    ) {
        self.run_op(
            window,
            cx,
            op,
            move |this, outcome, window, cx| match outcome {
                Outcome::Done => window.push_notification(Notification::success(done), cx),
                Outcome::Conflicts => {
                    this.view = View::Changes;
                    window.push_notification(
                        Notification::warning(
                            "Stopped on conflicts. Resolve the files marked U, then Continue.",
                        ),
                        cx,
                    );
                }
            },
        );
    }

    fn op_state(&self) -> OpState {
        match &self.repo {
            Some(Ok(repo)) => repo.op,
            _ => OpState::None,
        }
    }

    /// Banner shown while git has paused a merge / rebase / cherry-pick / revert (M11 §0).
    pub(crate) fn op_banner(&self, cx: &mut Context<Self>) -> Option<Div> {
        let state = self.op_state();
        let title = match state {
            OpState::None => return None,
            OpState::Merging => "Merge in progress".to_owned(),
            OpState::Rebasing { step, total } => {
                format!("Rebase in progress, commit {step} of {total}")
            }
            OpState::CherryPicking => "Cherry-pick in progress".to_owned(),
            OpState::Reverting => "Revert in progress".to_owned(),
        };
        let conflicts = match &self.repo {
            Some(Ok(repo)) => repo.status.iter().filter(|e| e.conflicted).count(),
            _ => 0,
        };
        let hint = match conflicts {
            0 => "No conflicts left: continue when ready.".to_owned(),
            1 => "1 conflicted file to resolve.".to_owned(),
            n => format!("{n} conflicted files to resolve."),
        };
        let theme = cx.theme();
        let can_skip = !matches!(state, OpState::Merging);
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
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(title),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(hint),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("op-abort")
                                .small()
                                .ghost()
                                .label("Abort")
                                .disabled(self.busy)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.confirm_abort(window, cx)
                                })),
                        )
                        .when(can_skip, |h| {
                            h.child(
                                Button::new("op-skip")
                                    .small()
                                    .ghost()
                                    .label("Skip commit")
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.run_outcome(
                                            window,
                                            cx,
                                            "Skipped".into(),
                                            spork_git::skip_op,
                                        )
                                    })),
                            )
                        })
                        .child(
                            Button::new("op-continue")
                                .small()
                                .primary()
                                .label("Continue")
                                .disabled(self.busy || conflicts > 0)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.run_outcome(
                                        window,
                                        cx,
                                        "Done".into(),
                                        spork_git::continue_op,
                                    )
                                })),
                        ),
                ),
        )
    }

    fn confirm_abort(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            dialog
                .title("Abort the operation?")
                .description("Your branch goes back to how it was before it started. Resolved conflicts are lost.")
                .confirm()
                .ok_text("Abort")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    view.update(cx, |this, cx| {
                        this.run_op(window, cx, spork_git::abort_op, |_, _, window, cx| {
                            window.push_notification(Notification::info("Aborted"), cx)
                        })
                    });
                    true
                })
        });
    }

    /// Takes one side of a conflicted file whole (M11 §4).
    pub(crate) fn resolve_conflict(
        &mut self,
        path: String,
        ours: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_op(
            window,
            cx,
            move |root| spork_git::resolve_with(root, &path, ours),
            |_, _, _, _| {},
        );
    }

    /// Right-click menu of a commit row in the history (M06 §2).
    pub(crate) fn commit_menu(
        view: Entity<RepoView>,
        id: String,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        move |menu, _, _| {
            let short: String = id.chars().take(7).collect();
            let item =
                |label: &str, f: fn(&mut RepoView, String, &mut Window, &mut Context<RepoView>)| {
                    let (view, id) = (view.clone(), id.clone());
                    PopupMenuItem::new(label.to_owned()).on_click(move |_, window, cx| {
                        view.update(cx, |this, cx| f(this, id.clone(), window, cx))
                    })
                };
            menu.item(item(
                "Cherry-pick onto current branch",
                |this, id, window, cx| {
                    let short: String = id.chars().take(7).collect();
                    this.run_outcome(window, cx, format!("Cherry-picked {short}"), move |root| {
                        spork_git::cherry_pick(root, &id)
                    })
                },
            ))
            .item(item("Revert this commit", |this, id, window, cx| {
                let short: String = id.chars().take(7).collect();
                this.run_outcome(window, cx, format!("Reverted {short}"), move |root| {
                    spork_git::revert(root, &id)
                })
            }))
            .item(PopupMenuItem::separator())
            .item(item("New branch here…", |this, id, window, cx| {
                this.new_branch_dialog(id, window, cx)
            }))
            .item(item("Create tag here…", |this, id, window, cx| {
                this.tag_dialog(id, window, cx)
            }))
            .item(PopupMenuItem::separator())
            .item(item(
                "Interactive rebase from here…",
                |this, id, window, cx| this.start_interactive_rebase(id, window, cx),
            ))
            .item(item(&format!("Copy SHA ({short})"), |_, id, window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(id));
                window.push_notification(Notification::info("Commit SHA copied"), cx);
            }))
        }
    }

    /// Sidebar entries for the saved stashes, with Apply / Pop / Drop menus (M12 §1).
    pub(crate) fn stash_items(
        &self,
        stashes: &[Stash],
        cx: &mut Context<Self>,
    ) -> Vec<SidebarMenuItem> {
        let view = cx.entity();
        stashes
            .iter()
            .map(|s| {
                let (index, view) = (s.index, view.clone());
                let label = s
                    .message
                    .split_once(": ")
                    .map_or(s.message.as_str(), |(_, m)| m)
                    .to_owned();
                SidebarMenuItem::new(label)
                    .icon(IconName::Archive)
                    .context_menu(move |menu, _, _| {
                        let item = |label: &str, pop: Option<bool>| {
                            let view = view.clone();
                            PopupMenuItem::new(label.to_owned()).on_click(move |_, window, cx| {
                                view.update(cx, |this, cx| match pop {
                                    Some(pop) => this.run_outcome(
                                        window,
                                        cx,
                                        if pop {
                                            "Stash applied and removed"
                                        } else {
                                            "Stash applied"
                                        }
                                        .into(),
                                        move |root| spork_git::stash_apply(root, index, pop),
                                    ),
                                    None => this.confirm_drop_stash(index, window, cx),
                                })
                            })
                        };
                        menu.item(item("Apply", Some(false)))
                            .item(item("Apply and remove (pop)", Some(true)))
                            .item(PopupMenuItem::separator())
                            .item(item("Delete…", None))
                    })
            })
            .collect()
    }

    fn confirm_drop_stash(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            dialog
                .title("Delete this stash?")
                .description("Its changes are discarded and can't be recovered from Spork.")
                .confirm()
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    view.update(cx, |this, cx| {
                        this.run_op(
                            window,
                            cx,
                            move |root| spork_git::stash_drop(root, index),
                            |_, _, _, _| {},
                        )
                    });
                    true
                })
        });
    }

    /// "Stash changes" dialog: optional message, untracked files included by default.
    pub(crate) fn stash_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.stash_message
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.stash_untracked = true;
        let (view, input) = (cx.entity(), self.stash_message.clone());
        window.open_alert_dialog(cx, move |dialog, _, cx| {
            let (view, input) = (view.clone(), input.clone());
            let untracked = view.read(cx).stash_untracked;
            let toggle = view.clone();
            dialog
                .title("Stash changes")
                .description(
                    v_flex().gap_3().child(Input::new(&input)).child(
                        Checkbox::new("stash-untracked")
                            .label("Include untracked files")
                            .checked(untracked)
                            .on_click(move |checked, _, cx| {
                                toggle.update(cx, |this, cx| {
                                    this.stash_untracked = *checked;
                                    cx.notify();
                                })
                            }),
                    ),
                )
                .confirm()
                .ok_text("Stash")
                .on_ok(move |_, window, cx| {
                    let message = input.read(cx).value().to_string();
                    view.update(cx, |this, cx| {
                        let untracked = this.stash_untracked;
                        this.run_op(
                            window,
                            cx,
                            move |root| spork_git::stash_save(root, &message, untracked),
                            |_, _, window, cx| {
                                window
                                    .push_notification(Notification::success("Changes stashed"), cx)
                            },
                        )
                    });
                    true
                })
        });
    }

    /// "Create tag" dialog at `target`: name, optional message (annotated when given).
    pub(crate) fn tag_dialog(
        &mut self,
        target: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tag_name
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.tag_message
            .update(cx, |s, cx| s.set_value("", window, cx));
        let (view, name, message) = (cx.entity(), self.tag_name.clone(), self.tag_message.clone());
        let short: String = target.chars().take(7).collect();
        let muted = cx.theme().muted_foreground;
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (view, name, message, target) =
                (view.clone(), name.clone(), message.clone(), target.clone());
            dialog
                .title("Create tag")
                .description(
                    v_flex()
                        .gap_2()
                        .child(div().text_sm().text_color(muted).child(format!(
                            "On commit {short}. A message makes it an annotated tag."
                        )))
                        .child(Input::new(&name))
                        .child(Input::new(&message)),
                )
                .confirm()
                .ok_text("Create tag")
                .on_ok(move |_, window, cx| {
                    let (tag, text, target) = (
                        name.read(cx).value().trim().to_owned(),
                        message.read(cx).value().to_string(),
                        target.clone(),
                    );
                    view.update(cx, |this, cx| {
                        this.run_op(
                            window,
                            cx,
                            move |root| {
                                spork_git::create_tag(root, &tag, &target, &text)
                                    .map(|()| format!("Created tag {tag}"))
                            },
                            |_, msg, window, cx| {
                                window.push_notification(Notification::success(msg), cx)
                            },
                        )
                    });
                    true
                })
        });
    }

    pub(crate) fn stash_button(&self, cx: &mut Context<Self>) -> Button {
        Button::new("stash")
            .ghost()
            .icon(IconName::Archive)
            .tooltip("Stash changes")
            .disabled(self.busy)
            .on_click(cx.listener(|this, _, window, cx| this.stash_dialog(window, cx)))
    }
}
