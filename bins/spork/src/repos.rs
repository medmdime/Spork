//! Opening repositories: recent list, switcher, welcome screen, Open / Clone / New (docs/specs/M01, M02).

use std::path::{Path, PathBuf};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::*;

use crate::{RepoView, View};

const MAX_RECENT: usize = 15;

/// Spork's settings folder: `~/Library/Application Support/Spork`, `%APPDATA%\Spork`, or
/// `$XDG_CONFIG_HOME/spork` (default `~/.config/spork`).
pub fn config_dir() -> Option<PathBuf> {
    let env = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(target_os = "macos") {
        Some(env("HOME")?.join("Library/Application Support/Spork"))
    } else if cfg!(windows) {
        Some(env("APPDATA")?.join("Spork"))
    } else {
        Some(
            env("XDG_CONFIG_HOME")
                .or_else(|| Some(env("HOME")?.join(".config")))?
                .join("spork"),
        )
    }
}

fn recent_file() -> Option<PathBuf> {
    Some(config_dir()?.join("recent.txt"))
}

fn load_from(file: &Path) -> Vec<PathBuf> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Moves `path` to the top of the list in `file` (one path per line), keeping at most 15.
fn remember_in(file: &Path, path: &Path) -> std::io::Result<()> {
    let mut list = load_from(file);
    list.retain(|p| p != path);
    list.insert(0, path.to_path_buf());
    list.truncate(MAX_RECENT);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text: String = list.iter().map(|p| format!("{}\n", p.display())).collect();
    std::fs::write(file, text)
}

/// Recently opened repositories that still exist, newest first.
pub fn recent() -> Vec<PathBuf> {
    recent_file()
        .map(|f| load_from(&f))
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.join(".git").exists())
        .collect()
}

pub fn remember(path: &Path) {
    if let Some(file) = recent_file() {
        remember_in(&file, path).ok();
    }
}

fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

impl RepoView {
    /// Shows another repository in this window, starting fresh.
    pub(crate) fn switch_repo(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.requested = path;
        self.repo = None;
        self.selected = None;
        self.details = None;
        self.change_selected = None;
        self.rebase_plan = None;
        // a repo switch invalidates the search UI too (M05 §5)
        self.filter = None;
        self.search = None;
        self._search_task = None;
        self.search_cancel.kill();
        self.view = View::History;
        self._watcher = None;
        self.gitflow_cancel.kill();
        self.lfs_cancel.kill();
        self._lfs_task = None;
        self._lfs_status_task = None;
        self.lfs_status = None;
        self.reload(cx);
    }

    /// Native folder picker, then `then(folder)`.
    fn pick_folder(
        &mut self,
        prompt: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, PathBuf, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(prompt.into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut picked))) = paths.await
                && let Some(folder) = picked.pop()
            {
                this.update_in(cx, |this, window, cx| then(this, folder, window, cx))
                    .ok();
            }
        })
        .detach();
    }

    pub(crate) fn open_repo_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_folder("Open", window, cx, |this, folder, _, cx| {
            this.switch_repo(folder, cx)
        });
    }

    pub(crate) fn new_repo_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_folder(
            "Create repository here",
            window,
            cx,
            |this, folder, window, cx| {
                let target = folder.clone();
                this.run_op(
                    window,
                    cx,
                    move |_| spork_git::init(&target),
                    move |this, _, window, cx| {
                        window.push_notification(Notification::success("Repository created"), cx);
                        this.switch_repo(folder, cx);
                    },
                );
            },
        );
    }

    /// Asks for the URL, then where to put it, then clones and opens the result.
    pub(crate) fn clone_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clone_url
            .update(cx, |s, cx| s.set_value("", window, cx));
        let (view, input) = (cx.entity(), self.clone_url.clone());
        let muted = cx.theme().muted_foreground;
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (view, input) = (view.clone(), input.clone());
            dialog
                .title("Clone repository")
                .description(
                    v_flex()
                        .gap_2()
                        .child(div().text_sm().text_color(muted).child("Repository URL (https, ssh, or a local path). Next you pick the folder to clone into."))
                        .child(Input::new(&input)),
                )
                .confirm()
                .ok_text("Choose folder…")
                .on_ok(move |_, window, cx| {
                    let url = input.read(cx).value().trim().to_owned();
                    if url.is_empty() {
                        return false;
                    }
                    view.update(cx, |this, cx| {
                        this.pick_folder("Clone here", window, cx, move |this, parent, window, cx| {
                            let name = spork_git::repo_name_from_url(&url);
                            this.run_op(
                                window,
                                cx,
                                move |_| spork_git::clone(&url, &parent, &name),
                                |this, dest, window, cx| {
                                    window.push_notification(Notification::success("Cloned"), cx);
                                    this.switch_repo(dest, cx);
                                },
                            );
                        })
                    });
                    true
                })
        });
    }

    /// Repository name in the sidebar header; opens the switcher.
    pub(crate) fn repo_switcher(&self, name: String, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let current = match &self.repo {
            Some(Ok(repo)) => Some(repo.root.clone()),
            _ => None,
        };
        let recent: Vec<PathBuf> = self
            .recent
            .iter()
            .filter(|p| Some(*p) != current.as_ref())
            .cloned()
            .collect();
        Button::new("repo-switcher")
            .ghost()
            .label(name)
            .icon(IconName::ChevronDown)
            .dropdown_menu(move |menu, window, cx| {
                let mut menu = menu;
                for path in &recent {
                    let (view, path) = (view.clone(), path.clone());
                    menu = menu.item(PopupMenuItem::new(display_name(&path)).on_click(
                        move |_, _, cx| {
                            view.update(cx, |this, cx| this.switch_repo(path.clone(), cx))
                        },
                    ));
                }
                if !recent.is_empty() {
                    menu = menu.item(PopupMenuItem::separator());
                }
                let action =
                    |label: &str, f: fn(&mut RepoView, &mut Window, &mut Context<RepoView>)| {
                        let view = view.clone();
                        PopupMenuItem::new(label.to_owned()).on_click(move |_, window, cx| {
                            view.update(cx, |this, cx| f(this, window, cx))
                        })
                    };
                let menu = menu
                    .item(action("Open repository…", RepoView::open_repo_dialog))
                    .item(action("Clone repository…", RepoView::clone_dialog))
                    .item(action("New repository…", RepoView::new_repo_dialog))
                    .item(PopupMenuItem::separator());
                let gitflow_view = view.clone();
                let menu = menu.submenu("Git Flow", window, cx, move |menu, _window, cx| {
                    let (config, current, default_branch) =
                        gitflow_view.read(cx).gitflow_menu_state();
                    crate::gitflow::gitflow_submenu(
                        menu,
                        config,
                        current,
                        default_branch,
                        gitflow_view.clone(),
                    )
                });
                let lfs_view = view.clone();
                menu.submenu("Git LFS", window, cx, move |menu, _window, cx| {
                    let state = lfs_view.read(cx).lfs_menu_state();
                    crate::lfs::lfs_submenu(menu, state, lfs_view.clone())
                })
            })
    }

    /// Shown when no repository is open (or the folder isn't one).
    pub(crate) fn welcome(&self, problem: Option<String>, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let hover = theme.list_hover;
        let button =
            |id: &'static str,
             label: &'static str,
             f: fn(&mut RepoView, &mut Window, &mut Context<RepoView>)| {
                Button::new(id)
                    .label(label)
                    .on_click(cx.listener(move |this, _, window, cx| f(this, window, cx)))
            };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_4()
            .child(
                div()
                    .text_2xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Spork"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(problem.unwrap_or_else(|| "Open a repository to get started.".into())),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(button("welcome-open", "Open…", RepoView::open_repo_dialog).primary())
                    .child(button("welcome-clone", "Clone…", RepoView::clone_dialog))
                    .child(button(
                        "welcome-new",
                        "New repository…",
                        RepoView::new_repo_dialog,
                    )),
            )
            .child(
                v_flex()
                    .w(px(420.))
                    .gap_1()
                    .children(self.recent.iter().enumerate().map(|(ix, path)| {
                        let target = path.clone();
                        h_flex()
                            .id(("recent", ix))
                            .px_3()
                            .py_1()
                            .gap_3()
                            .rounded(px(6.))
                            .hover(move |s| s.bg(hover))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.switch_repo(target.clone(), cx)
                            }))
                            .child(div().text_sm().child(display_name(path)))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(path.display().to_string()),
                            )
                    })),
            )
    }
}

#[cfg(test)]
mod tests {
    // explicit: `super::*` would pull in gpui's `test` attribute, which shadows #[test]
    use super::{MAX_RECENT, load_from, remember_in};
    use std::path::{Path, PathBuf};

    #[test]
    fn recent_list_moves_to_top_dedupes_and_caps() {
        let tmp = std::env::temp_dir().join(format!("spork-recent-{}", std::process::id()));
        let file = tmp.join("nested/recent.txt");
        for i in 0..20 {
            remember_in(&file, Path::new(&format!("/repos/{i}"))).unwrap();
        }
        remember_in(&file, Path::new("/repos/7")).unwrap();
        let list = load_from(&file);
        assert_eq!(list.len(), MAX_RECENT);
        assert_eq!(list[0], PathBuf::from("/repos/7"));
        assert_eq!(
            list.iter()
                .filter(|p| p.as_path() == Path::new("/repos/7"))
                .count(),
            1
        );
        assert_eq!(list[1], PathBuf::from("/repos/19"));
        std::fs::remove_dir_all(&tmp).ok();
    }
}
