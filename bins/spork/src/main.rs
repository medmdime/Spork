//! Spork desktop app. P0: open a repository and show its branches, remotes and tags (docs/specs/M01, M03).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::sidebar::{Sidebar, SidebarMenu, SidebarMenuItem};
use gpui_kit::component::{ActiveTheme as _, Theme, ThemeRegistry, TitleBar, h_flex, v_flex};
use gpui_kit::*;
use spork_git::{GitError, Ref, RefKind};

struct RepoView {
    requested: PathBuf,
    repo: Result<Loaded, GitError>,
    _appearance: Subscription,
}

struct Loaded {
    root: PathBuf,
    head: Option<String>,
    refs: Vec<Ref>,
}

impl RepoView {
    fn new(requested: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // follow the OS light/dark setting live
        let _appearance = cx.observe_window_appearance(window, |_, window, cx| {
            Theme::sync_system_appearance(Some(window), cx)
        });
        let repo = load(&requested);
        Self {
            requested,
            repo,
            _appearance,
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.repo = load(&self.requested);
        cx.notify();
    }
}

// shortcut: reads on the UI thread; fine for P0 ref counts, moves to the background executor with the watcher (P0 wk 2).
fn load(path: &Path) -> Result<Loaded, GitError> {
    let root = spork_git::toplevel(path)?;
    Ok(Loaded {
        head: spork_git::head_branch(&root)?,
        refs: spork_git::refs(&root)?,
        root,
    })
}

fn branch_item(r: &Ref) -> SidebarMenuItem {
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
        .active(r.is_head);
    if badge.is_empty() {
        item
    } else {
        item.suffix(move |_, _| div().text_xs().child(badge.clone()))
    }
}

/// Groups `a/b/c` names into folders, as in M03 §1 (`feature/login` → `feature ▸ login`).
fn folder_tree(refs: &[&Ref], depth: usize) -> Vec<SidebarMenuItem> {
    let mut folders: BTreeMap<&str, Vec<&Ref>> = BTreeMap::new();
    let mut leaves = Vec::new();
    for r in refs {
        match r.name.split('/').nth(depth) {
            Some(seg) if r.name.split('/').count() > depth + 1 => {
                folders.entry(seg).or_default().push(r)
            }
            _ => leaves.push(branch_item(r)),
        }
    }
    let mut items: Vec<SidebarMenuItem> = folders
        .into_iter()
        .map(|(name, inner)| {
            SidebarMenuItem::new(name.to_owned())
                .icon(IconName::Folder)
                .click_to_toggle(true)
                .children(folder_tree(&inner, depth + 1))
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

        let repo = match &self.repo {
            Ok(repo) => repo,
            Err(e) => {
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
        let locals = of(RefKind::Local);
        let remotes = of(RefKind::Remote);
        let tags: Vec<_> = of(RefKind::Tag)
            .iter()
            .map(|t| SidebarMenuItem::new(t.name.clone()).icon(IconName::Tag))
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
                        Button::new("refresh")
                            .ghost()
                            .icon(IconName::RefreshCw)
                            .tooltip("Refresh")
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    ),
            )
            .child(SidebarMenu::new().children([
                section(
                    "Branches",
                    locals.len(),
                    IconName::GitBranch,
                    folder_tree(&locals, 0),
                ),
                section(
                    "Remotes",
                    remotes.len(),
                    IconName::Globe,
                    folder_tree(&remotes, 0),
                ),
                section("Tags", tags.len(), IconName::Tag, tags),
            ]));

        let theme = cx.theme();
        base.child(sidebar).child(
            v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .mr_2()
                .mb_2()
                .bg(theme.background)
                .rounded(theme.radius)
                .px_8()
                .items_center()
                .justify_center()
                .gap_1()
                .child(
                    div()
                        .max_w_full()
                        .truncate()
                        .text_sm()
                        .child(repo.root.display().to_string()),
                )
                .child(div().text_sm().child(format!(
                    "{} branches · commit graph arrives in P1",
                    locals.len()
                ))),
        )
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
                cx.new(|cx| RepoView::new(requested, window, cx))
            })
            .expect("failed to open window");
            cx.activate(true);
        });
}
