//! Worktrees and submodules in the sidebar (docs/specs/M12 §3–4).

use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::sidebar::SidebarMenuItem;
use gpui_kit::*;
use spork_git::{Submodule, Worktree};

use crate::RepoView;

impl RepoView {
    pub(crate) fn worktree_items(
        &self,
        worktrees: &[Worktree],
        cx: &mut Context<Self>,
    ) -> Vec<SidebarMenuItem> {
        let view = cx.entity();
        worktrees
            .iter()
            .filter(|w| !w.is_main)
            .map(|w| {
                let folder = w.path.file_name().map_or_else(
                    || w.path.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                let label = w.branch.clone().unwrap_or(folder);
                let (path, view) = (w.path.clone(), view.clone());
                SidebarMenuItem::new(label)
                    .icon(IconName::FolderGit2)
                    .context_menu(move |menu, _, _| {
                        let (open, remove, open_view, remove_view) =
                            (path.clone(), path.clone(), view.clone(), view.clone());
                        menu.item(PopupMenuItem::new("Open in this window").on_click(
                            move |_, _, cx| {
                                open_view.update(cx, |this, cx| this.switch_repo(open.clone(), cx))
                            },
                        ))
                        .item(PopupMenuItem::separator())
                        .item(
                            PopupMenuItem::new("Remove worktree…").on_click(
                                move |_, window, cx| {
                                    remove_view.update(cx, |this, cx| {
                                        this.confirm_remove_worktree(remove.clone(), window, cx)
                                    })
                                },
                            ),
                        )
                    })
            })
            .collect()
    }

    fn confirm_remove_worktree(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (view, path) = (view.clone(), path.clone());
            dialog
                .title("Remove this worktree?")
                .description(format!("Deletes the folder {}. Git refuses if it has uncommitted changes; the branch is kept.", path.display()))
                .confirm()
                .ok_text("Remove")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let path = path.clone();
                    view.update(cx, |this, cx| {
                        this.run_op(window, cx, move |root| spork_git::remove_worktree(root, &path), |_, _, window, cx| {
                            window.push_notification(Notification::success("Worktree removed"), cx)
                        })
                    });
                    true
                })
        });
    }

    /// Branch menu: check `branch` out in a new worktree next to a folder the user picks.
    pub(crate) fn new_worktree_dialog(
        &mut self,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Ok(repo)) = &self.repo else { return };
        let repo_name = repo
            .root
            .file_name()
            .map_or_else(|| "repo".into(), |n| n.to_string_lossy().into_owned());
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Create worktree in".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut picked))) = paths.await else {
                return;
            };
            let Some(parent) = picked.pop() else { return };
            let leaf = branch.rsplit('/').next().unwrap_or(&branch).to_owned();
            let path = parent.join(format!("{repo_name}-{leaf}"));
            this.update_in(cx, |this, window, cx| {
                this.run_op(
                    window,
                    cx,
                    move |root| spork_git::add_worktree(root, &path, &branch).map(|()| path),
                    |_, path, window, cx| {
                        window.push_notification(
                            Notification::success(format!(
                                "Worktree created at {}",
                                path.display()
                            )),
                            cx,
                        )
                    },
                )
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn submodule_items(
        &self,
        submodules: &[Submodule],
        cx: &mut Context<Self>,
    ) -> Vec<SidebarMenuItem> {
        let view = cx.entity();
        let root = match &self.repo {
            Some(Ok(repo)) => repo.root.clone(),
            _ => return Vec::new(),
        };
        submodules
            .iter()
            .map(|s| {
                let note = match s.state {
                    '-' => " (not initialized)",
                    '+' => " (different commit)",
                    'U' => " (conflict)",
                    _ => "",
                };
                let (path, view) = (root.join(&s.path), view.clone());
                SidebarMenuItem::new(format!("{}{note}", s.path))
                    .icon(IconName::Package)
                    .context_menu(move |menu, _, _| {
                        let (path, open_view, update_view) =
                            (path.clone(), view.clone(), view.clone());
                        menu.item(PopupMenuItem::new("Open in this window").on_click(
                            move |_, _, cx| {
                                open_view.update(cx, |this, cx| this.switch_repo(path.clone(), cx))
                            },
                        ))
                        .item(
                            PopupMenuItem::new("Update all submodules").on_click(
                                move |_, window, cx| {
                                    update_view.update(cx, |this, cx| {
                                        this.run_op(
                                            window,
                                            cx,
                                            spork_git::update_submodules,
                                            |_, _, window, cx| {
                                                window.push_notification(
                                                    Notification::success("Submodules updated"),
                                                    cx,
                                                )
                                            },
                                        )
                                    })
                                },
                            ),
                        )
                    })
            })
            .collect()
    }
}
