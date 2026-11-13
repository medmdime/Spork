//! Branch operations from the sidebar's right-click menus (docs/specs/M04).

use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, v_flex};
use gpui_kit::*;
use spork_git::{Ref, RefKind};

use crate::RepoView;

#[derive(Clone, Copy)]
enum Action {
    Checkout,
    CheckoutRemote,
    NewBranchFrom,
    Delete,
}

/// Right-click menu for a branch, remote branch or tag in the sidebar.
pub fn ref_menu(
    r: &Ref,
    view: Entity<RepoView>,
) -> impl Fn(PopupMenu, &mut Window, &mut App) -> PopupMenu + 'static {
    let (name, kind, is_head) = (r.name.clone(), r.kind, r.is_head);
    move |menu, _, _| {
        let item = |label: &str, action: Action| {
            let (view, name) = (view.clone(), name.clone());
            PopupMenuItem::new(label.to_owned()).on_click(move |_, window, cx| {
                view.update(cx, |this, cx| {
                    this.ref_action(action, name.clone(), window, cx)
                })
            })
        };
        match kind {
            RefKind::Local => menu
                .item(item("Checkout", Action::Checkout).disabled(is_head))
                .item(item("New branch from here…", Action::NewBranchFrom))
                .item(PopupMenuItem::separator())
                .item(item("Delete…", Action::Delete).disabled(is_head)),
            RefKind::Remote => menu
                .item(item("Checkout", Action::CheckoutRemote))
                .item(item("New branch from here…", Action::NewBranchFrom)),
            RefKind::Tag => menu.item(item("New branch from here…", Action::NewBranchFrom)),
        }
    }
}

impl RepoView {
    fn ref_action(
        &mut self,
        action: Action,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.entity();
        match action {
            Action::Checkout => self.run_op(
                window,
                cx,
                move |root| {
                    spork_git::checkout(root, &name).map(|()| format!("Switched to {name}"))
                },
                notify_success,
            ),
            Action::CheckoutRemote => self.run_op(
                window,
                cx,
                move |root| {
                    spork_git::checkout_remote(root, &name)
                        .map(|local| format!("Switched to {local}"))
                },
                notify_success,
            ),
            Action::NewBranchFrom => {
                self.branch_name
                    .update(cx, |s, cx| s.set_value("", window, cx));
                let input = self.branch_name.clone();
                let muted = cx.theme().muted_foreground;
                window.open_alert_dialog(cx, move |dialog, _, _| {
                    let (view, input, start) = (view.clone(), input.clone(), name.clone());
                    dialog
                        .title("New branch")
                        .description(
                            v_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(muted)
                                        .child(format!("Starts at {start}, then switches to it.")),
                                )
                                .child(Input::new(&input)),
                        )
                        .confirm()
                        .ok_text("Create branch")
                        .on_ok(move |_, window, cx| {
                            let new = input.read(cx).value().trim().to_owned();
                            let start = start.clone();
                            view.update(cx, |this, cx| {
                                this.run_op(
                                    window,
                                    cx,
                                    move |root| {
                                        spork_git::create_branch(root, &new, Some(&start), true)
                                            .map(|()| format!("Created and switched to {new}"))
                                    },
                                    notify_success,
                                )
                            });
                            true
                        })
                });
            }
            Action::Delete => {
                window.open_alert_dialog(cx, move |dialog, _, _| {
                    let (view, branch) = (view.clone(), name.clone());
                    dialog
                        .title(format!("Delete branch {branch}?"))
                        .description("Git only deletes it if its commits are merged somewhere, so no work is lost.")
                        .confirm()
                        .ok_text("Delete")
                        .ok_variant(ButtonVariant::Danger)
                        .on_ok(move |_, window, cx| {
                            let branch = branch.clone();
                            view.update(cx, |this, cx| {
                                this.run_op(
                                    window,
                                    cx,
                                    move |root| spork_git::delete_branch(root, &branch).map(|()| format!("Deleted {branch}")),
                                    notify_success,
                                )
                            });
                            true
                        })
                });
            }
        }
    }
}

fn notify_success(
    _: &mut RepoView,
    message: String,
    window: &mut Window,
    cx: &mut Context<RepoView>,
) {
    window.push_notification(Notification::success(message), cx);
}
