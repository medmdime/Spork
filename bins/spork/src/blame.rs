//! Blame (docs/specs/M08 §5): which commit last touched each line of a file at
//! one revision — the file content in one row per line, a gutter cell rendered
//! once per run of lines blamed to the same commit, an in-file search, and
//! "blame previous revision" for the selected line.

use std::ops::Range;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, h_flex, v_flex};
use gpui_kit::prelude::{FluentBuilder as _, StatefulInteractiveElement as _};
use gpui_kit::*;
use spork_git::BlameLine;

use crate::history::relative_age;
use crate::{RepoView, View};

/// One list row is two lines (SHA · author · age, then the commit summary).
const LINE_HEIGHT: f32 = 40.;
/// The width of the gutter column.
const GUTTER_WIDTH: f32 = 260.;

/// An open blame (M08 §5): the lines of one path at one revision, and the
/// commit that last touched each of them.
pub struct Blame {
    /// The path to blame, as it exists at `rev`.
    pub path: String,
    /// The revision the file is blamed at.
    pub rev: String,
    /// A short label for `rev` in the header.
    pub rev_label: String,
    /// The view to return to on Back.
    pub from: View,
    /// One entry per line of the file, in file order.
    pub lines: Vec<BlameLine>,
    /// The file content at `rev`, for the right side of each row; `None`
    /// while it loads.
    pub content: Option<Result<String, String>>,
    /// The blame is still loading.
    pub running: bool,
    /// The blame load failed.
    pub error: Option<String>,
    /// The commits in `lines` that have no parent: for a line blamed to one
    /// of them, "blame previous revision" is unavailable.
    pub roots: Vec<String>,
    /// The selected file line (0-based): the target of "Blame previous revision".
    pub selected: Option<usize>,
}

impl RepoView {
    /// Opens the blame of `path` at `rev` (M08 §5), loaded in the background.
    pub(crate) fn start_blame(
        &mut self,
        path: String,
        rev: String,
        rev_label: String,
        from: View,
        cx: &mut Context<Self>,
    ) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let root = repo.root.clone();
        // a new blame supersedes the running one: kill its git child and drop
        // the in-flight load (dropped tasks never complete)
        self.blame_cancel.kill();
        let cancel = spork_git::Cancellable::default();
        self.blame_cancel = cancel.clone();
        self._blame_task = None;
        self.blame = Some(Blame {
            path: path.clone(),
            rev: rev.clone(),
            rev_label,
            from,
            lines: Vec::new(),
            content: None,
            running: true,
            error: None,
            roots: Vec::new(),
            selected: None,
        });
        self.view = View::Blame;
        self._blame_task = Some(cx.spawn(async move |this, cx| {
            // keep copies for the stale-completion check below
            let (check_path, check_rev) = (path.clone(), rev.clone());
            let (lines, content, roots, error) = cx
                .background_spawn(async move {
                    match spork_git::blame(&root, &rev, &path, false, &cancel) {
                        Ok(lines) => {
                            let mut oids: Vec<&str> = Vec::new();
                            for l in &lines {
                                let o = l.final_oid.as_str();
                                // the boundary group has no commit of its own
                                if o.bytes().any(|c| c != b'0') && !oids.contains(&o) {
                                    oids.push(o);
                                }
                            }
                            let roots = if oids.is_empty() {
                                Vec::new()
                            } else {
                                spork_git::root_commits(&root, &oids).unwrap_or_default()
                            };
                            let content = spork_git::show_file(&root, &rev, &path)
                                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                                .map_err(|e| e.to_string());
                            (lines, Some(content), roots, None)
                        }
                        Err(e) => (Vec::new(), None, Vec::new(), Some(e.to_string())),
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                let Some(b) = this.blame.as_mut() else {
                    return;
                };
                // a stale completion for a superseded blame: drop it
                if (b.path.as_str(), b.rev.as_str()) != (check_path.as_str(), check_rev.as_str()) {
                    return;
                }
                b.running = false;
                b.lines = lines;
                b.content = content;
                b.roots = roots;
                b.error = error;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// "Blame at this revision" (M08 §5): from the file history view, blames
    /// the file at the selected entry, under the name the file had in that
    /// commit.
    pub(crate) fn blame_at_revision(&mut self, cx: &mut Context<Self>) {
        let Some(fh) = self.file_history.as_ref() else {
            return;
        };
        let Some(entry) = fh.selected.and_then(|ix| fh.entries.get(ix)) else {
            return;
        };
        let short: String = entry.oid.chars().take(7).collect();
        self.start_blame(
            entry.path.clone(),
            entry.oid.clone(),
            short,
            View::FileHistory,
            cx,
        );
    }

    /// Selects a blame row (one file line); it becomes the target of "Blame
    /// previous revision" (M08 §5).
    pub(crate) fn blame_select(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(b) = self.blame.as_mut() else {
            return;
        };
        if b.lines.get(ix).is_none() {
            return;
        }
        b.selected = Some(ix);
        cx.notify();
    }

    /// "Blame previous revision" (M08 §5): re-runs the blame at the parent of
    /// the commit that last touched the selected line. The line's commit being
    /// a root commit has no parent, so the action stays disabled for it.
    pub(crate) fn blame_previous(&mut self, cx: &mut Context<Self>) {
        let (Some(b), Some(Ok(_repo))) = (self.blame.as_ref(), &self.repo) else {
            return;
        };
        let Some(line) = b.selected.and_then(|ix| b.lines.get(ix)) else {
            return;
        };
        let oid = &line.final_oid;
        let is_root = oid.bytes().all(|c| c == b'0') || b.roots.iter().any(|r| r == oid);
        if is_root {
            return; // the button is disabled for roots, with a hint
        }
        let path = b.path.clone();
        let (rev, label) = (format!("{oid}^"), format!("{}^", &oid[..7]));
        self.start_blame(path, rev, label, View::Blame, cx);
    }

    /// Closes the blame and returns to the view it was opened from (M08 §5).
    fn close_blame(&mut self, cx: &mut Context<Self>) {
        let from = self.blame.as_ref().map(|b| b.from).unwrap_or(View::History);
        self.blame_cancel.kill();
        self._blame_task = None;
        self.blame = None;
        self.view = from;
        cx.notify();
    }

    /// Clicking a gutter cell jumps to its commit in the commit list (M08 §5):
    /// the blame closes and the history row is selected, if the commit is
    /// among the loaded history rows.
    pub(crate) fn blame_jump(&mut self, oid: String, cx: &mut Context<Self>) {
        let ix = match &self.repo {
            Some(Ok(repo)) => repo.history.commits.iter().position(|c| c.id == oid),
            _ => None,
        };
        self.close_blame(cx);
        self.view = View::History;
        if let Some(ix) = ix {
            self.select(ix, cx);
        }
        cx.notify();
    }

    /// The Blame view (M08 §5): one row per line of the file, a gutter cell
    /// per run of lines blamed to the same commit, the in-file search, and the
    /// "Blame previous revision" action bar.
    pub(crate) fn blame_view(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let b = match &self.blame {
            Some(b) => b,
            None => return div().into_any_element(),
        };
        v_flex()
            .size_full()
            .child(self.blame_header(b, cx))
            .child(self.blame_search_bar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(self.blame_list(b, cx)),
            )
            .child(self.blame_actions(b, cx))
            .into_any_element()
    }

    /// Header of the blame: Back button, `path @ rev`, and the load error.
    fn blame_header(&self, b: &Blame, cx: &mut Context<Self>) -> Div {
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
                Button::new("blame-back")
                    .ghost()
                    .icon(IconName::ArrowLeft)
                    .tooltip("Back to the previous view")
                    .on_click(cx.listener(move |this, _, _, cx| this.close_blame(cx))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .truncate()
                    .child(format!("Blame: {} @ {}", b.path, b.rev_label)),
            )
            .child(div().text_xs().text_color(muted).truncate().child(format!(
                        "{} lines · {} commits",
                        b.lines.len(),
                        b.lines
                            .iter()
                            .map(|l| l.final_oid.as_str())
                            .collect::<std::collections::BTreeSet<_>>()
                            .len()
                    )))
            .children(
                b.error
                    .iter()
                    .map(|e| div().text_xs().text_color(red).truncate().child(e.clone())),
            )
    }

    /// The in-file search bar (M08 §5): a plain case-insensitive substring
    /// over the file text; matching lines are highlighted in the list.
    fn blame_search_bar(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        h_flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(Input::new(&self.blame_search).w(px(240.)))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Search in the file text — matching lines are highlighted."),
            )
    }

    /// The blame list, or the loading / error / empty state.
    fn blame_list(&self, b: &Blame, cx: &mut Context<Self>) -> AnyElement {
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
        if b.running {
            return centered("Loading blame…".into());
        }
        if let Some(error) = &b.error {
            return centered(error.clone());
        }
        if b.lines.is_empty() {
            return centered("This file is empty at this revision.".into());
        }
        uniform_list(
            "blame",
            b.lines.len(),
            cx.processor(|this, range, _, cx| this.blame_rows(range, cx)),
        )
        .flex_1()
        .min_h_0()
        .into_any_element()
    }

    /// Rows for the blame list (M08 §5): one per line of the file. The gutter
    /// cell renders once per run of lines blamed to the same commit — on the
    /// run's first row — and the run's other rows carry its continuation
    /// background. Clicking the gutter jumps to the commit; clicking the line
    /// selects it for "Blame previous revision".
    fn blame_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(b) = self.blame.as_ref() else {
            return Vec::new();
        };
        if b.running || b.lines.is_empty() {
            return Vec::new();
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let theme = cx.theme();
        let (muted, hover, active, match_bg) = (
            theme.muted_foreground,
            theme.list_hover,
            theme.list_active,
            theme.yellow_light,
        );
        let mono = theme.mono_font_family.clone();
        // case-insensitive substring search over the file text (M08 §5)
        let query = self.blame_search.read(cx).value().to_lowercase();
        let content_lines: Vec<&str> = b
            .content
            .as_ref()
            .and_then(|c| c.as_ref().ok())
            .map_or_else(Vec::new, |text| text.split('\n').collect());
        range
            .map(|ix| {
                let line = &b.lines[ix];
                let group_start = ix == 0 || b.lines[ix - 1].final_oid != line.final_oid;
                let text = content_lines.get(ix).copied().unwrap_or("");
                let is_match = !query.is_empty() && text.to_lowercase().contains(&query);
                let is_selected = b.selected == Some(ix);
                let gutter = self.blame_gutter(line, now, group_start, muted, mono.clone(), cx);
                h_flex()
                    .w_full()
                    .h(px(LINE_HEIGHT))
                    .child(gutter)
                    .child(
                        div()
                            .id(("blame-line", ix))
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .px_2()
                            .when(is_match, |d| d.bg(match_bg))
                            .when(is_selected, |d| d.bg(active))
                            .hover(move |d| d.bg(hover))
                            .on_click(cx.listener(move |this, _, _, cx| this.blame_select(ix, cx)))
                            .child(
                                div()
                                    .font_family(mono.clone())
                                    .text_sm()
                                    .truncate()
                                    .child(text.to_string()),
                            ),
                    )
                    .into_any_element()
            })
            .collect()
    }

    /// The gutter cell of one row (M08 §5): on the first row of a commit's
    /// run it shows the shortened SHA, author, age and commit summary, and
    /// clicking it jumps to the commit; the run's other rows show the
    /// continuation background only, so the cell reads as one cell spanning
    /// its run.
    fn blame_gutter(
        &self,
        line: &BlameLine,
        now: i64,
        group_start: bool,
        muted: Hsla,
        mono: SharedString,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = cx.theme();
        let border = theme.border;
        if !group_start {
            return div()
                .id(("blame-gutter", line.line))
                .w(px(GUTTER_WIDTH))
                .flex_none()
                .h_full()
                .bg(theme.list_hover)
                .border_r_1()
                .border_color(border);
        }
        // a single muted style: the app has no commit-age colouring helper
        let boundary = line.final_oid.bytes().all(|c| c == b'0');
        let short: String = line.final_oid.chars().take(7).collect();
        let author = if line.author.is_empty() {
            "(unknown)".to_owned()
        } else {
            line.author.clone()
        };
        let age = relative_age(line.author_date, now);
        let summary = if line.summary.is_empty() {
            if boundary {
                "(first commit of the file)".to_owned()
            } else {
                "(unknown commit)".to_owned()
            }
        } else {
            line.summary.clone()
        };
        let oid = line.final_oid.clone();
        div()
            .id(("blame-gutter", line.line))
            .w(px(GUTTER_WIDTH))
            .on_click(cx.listener(move |this, _, _, cx| this.blame_jump(oid.clone(), cx)))
            .child(
                v_flex()
                    .h_full()
                    .justify_center()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .child(div().font_family(mono).text_xs().child(short))
                            .child(div().text_xs().text_color(muted).truncate().child(author))
                            .child(div().text_xs().text_color(muted).child(age)),
                    )
                    .child(div().text_xs().text_color(muted).truncate().child(summary)),
            )
    }

    /// The blame action bar: "Blame previous revision" for the selected line,
    /// disabled with a hint when there is no selected line or its commit is a
    /// root commit (M08 §5).
    fn blame_actions(&self, b: &Blame, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let (disabled, hint) = match b.selected.and_then(|ix| b.lines.get(ix)) {
            None => (
                true,
                "Select a line to blame its previous revision.".to_owned(),
            ),
            Some(line)
                if line.final_oid.bytes().all(|c| c == b'0')
                    || b.roots.iter().any(|r| r == &line.final_oid) =>
            {
                (
                    true,
                    "This line's commit is the first commit of the file — there is no earlier revision to blame.".to_owned(),
                )
            }
            Some(_) => (false, String::new()),
        };
        h_flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(theme.border)
            .child(
                Button::new("blame-prev")
                    .ghost()
                    .icon(IconName::ClockArrowLeft)
                    .tooltip("Blame the parent of the commit that touched the selected line")
                    .disabled(disabled)
                    .on_click(cx.listener(move |this, _, _, cx| this.blame_previous(cx))),
            )
            .child(div().text_xs().text_color(muted).child(hint))
    }
}
