//! Commit search (docs/specs/M05 §5): the quick filter over the loaded history, and the deep
//! search form with a persisted recent-searches list.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::{Diff, DiffFile};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, h_flex, resizable_panel, v_flex, v_resizable,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use spork_git::{Commit, SearchQuery};

use crate::history::{ROW_HEIGHT, relative_age};
use crate::{MAX_PATCH_BYTES, RepoView, SearchState};

// ---------------------------------------------------------------------------
// Quick filter (M05 §5, first bullet)
// ---------------------------------------------------------------------------

/// Whether a quick-filter query matches a commit (M05 §5): a SHA prefix, or an author / subject
/// containing the query — case-insensitive. A blank query matches everything.
pub fn matches_filter(commit: &Commit, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    commit.id.starts_with(&q)
        || commit.author.to_lowercase().contains(&q)
        || commit.subject.to_lowercase().contains(&q)
}

/// Indices of the rows matching a quick-filter query, in list order.
pub fn filter_matches(commits: &[Commit], query: &str) -> Vec<usize> {
    (0..commits.len())
        .filter(|&ix| matches_filter(&commits[ix], query))
        .collect()
}

/// State of the open filter bar (M05 §5).
#[derive(Clone, Debug, Default)]
pub struct QuickFilter {
    /// The query that produced `matches`.
    pub query: String,
    /// Matching row indices, in list order.
    pub matches: Vec<usize>,
    /// Position of the current match within `matches`.
    pub current: usize,
}

impl QuickFilter {
    /// Rebuilds the filter state for a new query.
    pub fn for_query(commits: &[Commit], query: &str) -> Self {
        Self {
            query: query.to_owned(),
            matches: filter_matches(commits, query),
            current: 0,
        }
    }

    /// The current match index, if any.
    pub fn current_ix(&self) -> Option<usize> {
        self.matches.get(self.current).copied()
    }

    /// Moves to the next match (previous with `backward`), wrapping, and returns its index.
    pub fn step(&mut self, backward: bool) -> Option<usize> {
        if self.matches.is_empty() {
            return None;
        }
        self.current = if backward {
            if self.current == 0 {
                self.matches.len() - 1
            } else {
                self.current - 1
            }
        } else {
            (self.current + 1) % self.matches.len()
        };
        self.current_ix()
    }

    /// `Some(true)` for the current match, `Some(false)` for another match, `None` for no match.
    /// A blank query never highlights.
    pub fn highlight(&self, ix: usize) -> Option<bool> {
        if self.query.trim().is_empty() {
            return None;
        }
        if self.current_ix() == Some(ix) {
            return Some(true);
        }
        self.matches.binary_search(&ix).ok().map(|_| false)
    }
}

impl RepoView {
    /// Opens (or refocuses) the filter bar above the history list (Cmd-F).
    pub(crate) fn open_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.view != crate::View::History {
            return;
        }
        if self.filter.is_none() {
            self.filter_input
                .update(cx, |input, cx| input.set_value("", window, cx));
            self.filter = Some(QuickFilter::default());
        }
        self.filter_input
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// Closes the filter bar (Esc); the full list stays visible, unhighlighted.
    pub(crate) fn close_filter(&mut self, cx: &mut Context<Self>) {
        self.filter = None;
        cx.notify();
    }

    /// Re-filters the already-loaded rows for the input's current value (no git call, M05 §5).
    pub(crate) fn refilter(&mut self, cx: &mut Context<Self>) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let Some(filter) = self.filter.as_mut() else {
            return;
        };
        let query = self.filter_input.read(cx).value().to_owned();
        *filter = QuickFilter::for_query(&repo.history.commits, &query);
        let ix = if query.trim().is_empty() {
            None
        } else {
            filter.current_ix()
        };
        cx.notify();
        if let Some(ix) = ix {
            self.select(ix, cx);
        }
    }

    /// Moves to the next / previous match (Enter / Shift-Enter).
    pub(crate) fn step_filter(&mut self, backward: bool, cx: &mut Context<Self>) {
        let Some(ix) = self.filter.as_mut().and_then(|f| f.step(backward)) else {
            return;
        };
        self.select(ix, cx);
    }

    /// The filter bar above the history list, with a "3/42" position hint (M05 §5).
    pub(crate) fn filter_bar(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let hint = self
            .filter
            .as_ref()
            .filter(|f| !f.query.trim().is_empty())
            .map(|f| format!("{}/{}", f.current + 1, f.matches.len()))
            .unwrap_or_default();
        h_flex()
            .w_full()
            .h(px(34.))
            .flex_none()
            .border_b_1()
            .border_color(border)
            .px_3()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.filter_input)),
            )
            .when(!hint.is_empty(), |bar| {
                bar.child(div().flex_none().text_xs().text_color(muted).child(hint))
            })
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" && !e.keystroke.modifiers.platform {
                    this.close_filter(cx);
                }
            }))
    }
}

// ---------------------------------------------------------------------------
// Deep search: recent searches (same file pattern as the recent repos list)
// ---------------------------------------------------------------------------

const MAX_RECENT_SEARCHES: usize = 10;
/// One persisted search per line, tab-separated fields.
const LINE_FIELDS: usize = 7;

fn searches_file() -> Option<PathBuf> {
    Some(crate::repos::config_dir()?.join("searches.txt"))
}

fn load_from(file: &Path) -> Vec<SearchQuery> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(query_from_line)
        .collect()
}

/// Moves `query` to the top of the list in `file`, keeping at most 10.
fn remember_in(file: &Path, query: &SearchQuery) -> std::io::Result<()> {
    let mut list = load_from(file);
    list.retain(|q| q != query);
    list.insert(0, query.clone());
    list.truncate(MAX_RECENT_SEARCHES);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = String::new();
    for q in &list {
        text.push_str(&query_to_line(q));
        text.push('\n');
    }
    std::fs::write(file, text)
}

/// The saved searches, newest first.
pub fn recent_searches() -> Vec<SearchQuery> {
    searches_file().map(|f| load_from(&f)).unwrap_or_default()
}

/// Moves `query` to the top of the saved list (at most 10 entries).
pub fn remember_search(query: &SearchQuery) {
    if let Some(file) = searches_file() {
        remember_in(&file, query).ok();
    }
}

/// One saved search as a line (message, author, committer, content, path, since, until).
pub fn query_to_line(q: &SearchQuery) -> String {
    [
        q.message.as_str(),
        q.author.as_str(),
        q.committer.as_str(),
        q.content.as_str(),
        q.path.as_str(),
        q.since.as_str(),
        q.until.as_str(),
    ]
    .join("\t")
}

/// Parses one saved-search line; `None` if it is malformed (wrong field count).
pub fn query_from_line(line: &str) -> Option<SearchQuery> {
    let f: Vec<&str> = line.split('\t').collect();
    if f.len() != LINE_FIELDS {
        return None;
    }
    Some(SearchQuery {
        message: f[0].to_owned(),
        author: f[1].to_owned(),
        committer: f[2].to_owned(),
        content: f[3].to_owned(),
        path: f[4].to_owned(),
        since: f[5].to_owned(),
        until: f[6].to_owned(),
    })
}

/// Whether a deep-search query has no field set.
pub fn query_is_empty(q: &SearchQuery) -> bool {
    [
        q.message.as_str(),
        q.author.as_str(),
        q.committer.as_str(),
        q.content.as_str(),
        q.path.as_str(),
        q.since.as_str(),
        q.until.as_str(),
    ]
    .iter()
    .all(|s| s.trim().is_empty())
}

/// A short label for a saved search in the "Recent" menu.
pub fn query_label(q: &SearchQuery) -> String {
    let parts: Vec<&str> = [
        q.message.as_str(),
        q.author.as_str(),
        q.committer.as_str(),
        q.content.as_str(),
        q.path.as_str(),
    ]
    .into_iter()
    .map(str::trim)
    .filter(|s| !s.is_empty())
    .collect();
    if parts.is_empty() {
        return "all commits".into();
    }
    parts.join(" · ").chars().take(60).collect()
}

// ---------------------------------------------------------------------------
// Deep search view (M05 §5, second bullet)
// ---------------------------------------------------------------------------

impl RepoView {
    /// The deep-search form: Message / Author / Committer / Content (pickaxe) / Path / dates.
    pub(crate) fn search_form(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let running = self.search.as_ref().is_some_and(|s| s.running);
        let label = |t: &'static str| {
            div()
                .w(px(84.))
                .flex_none()
                .text_xs()
                .text_color(muted)
                .child(t)
        };
        let field =
            |entity: &Entity<InputState>| div().flex_1().min_w_0().child(Input::new(entity));
        let view = cx.entity();
        let recent = self.recent_searches.clone();
        v_flex()
            .flex_none()
            .border_b_1()
            .border_color(border)
            .px_3()
            .py_2()
            .gap_2()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(label("Message"))
                    .child(field(&self.search_message))
                    .child(label("Author"))
                    .child(field(&self.search_author)),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(label("Committer"))
                    .child(field(&self.search_committer))
                    .child(label("Content"))
                    .child(field(&self.search_content)),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(label("Path"))
                    .child(field(&self.search_path))
                    .child(label("Since"))
                    .child(field(&self.search_since))
                    .child(label("Until"))
                    .child(field(&self.search_until)),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        Button::new("run-search")
                            .label("Search")
                            .primary()
                            .on_click(cx.listener(move |this, _, _, cx| this.start_search(cx))),
                    )
                    .child(
                        Button::new("cancel-search")
                            .label("Cancel")
                            .disabled(!running)
                            .on_click(cx.listener(move |this, _, _, cx| this.cancel_search(cx))),
                    )
                    .child(
                        Button::new("recent-searches")
                            .ghost()
                            .label("Recent")
                            .icon(IconName::ChevronDown)
                            .disabled(recent.is_empty())
                            .dropdown_menu(move |menu, _, _| {
                                let mut menu = menu;
                                for q in &recent {
                                    let (view, q) = (view.clone(), q.clone());
                                    menu = menu.item(PopupMenuItem::new(query_label(&q)).on_click(
                                        move |_, window, cx| {
                                            view.update(cx, |this, cx| {
                                                this.apply_recent_search(q.clone(), window, cx)
                                            })
                                        },
                                    ));
                                }
                                menu
                            }),
                    )
                    .child(div().flex_1()),
            )
    }

    /// Fills the form with a saved search (from the "Recent" menu).
    pub(crate) fn apply_recent_search(
        &mut self,
        q: SearchQuery,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_message
            .update(cx, |s, cx| s.set_value(q.message.as_str(), window, cx));
        self.search_author
            .update(cx, |s, cx| s.set_value(q.author.as_str(), window, cx));
        self.search_committer
            .update(cx, |s, cx| s.set_value(q.committer.as_str(), window, cx));
        self.search_content
            .update(cx, |s, cx| s.set_value(q.content.as_str(), window, cx));
        self.search_path
            .update(cx, |s, cx| s.set_value(q.path.as_str(), window, cx));
        self.search_since
            .update(cx, |s, cx| s.set_value(q.since.as_str(), window, cx));
        self.search_until
            .update(cx, |s, cx| s.set_value(q.until.as_str(), window, cx));
        cx.notify();
    }

    /// Reads the form into a query, fields trimmed (empty = unset).
    fn search_query(&self, cx: &mut Context<Self>) -> SearchQuery {
        SearchQuery {
            message: self.search_message.read(cx).value().trim().to_owned(),
            author: self.search_author.read(cx).value().trim().to_owned(),
            committer: self.search_committer.read(cx).value().trim().to_owned(),
            content: self.search_content.read(cx).value().trim().to_owned(),
            path: self.search_path.read(cx).value().trim().to_owned(),
            since: self.search_since.read(cx).value().trim().to_owned(),
            until: self.search_until.read(cx).value().trim().to_owned(),
        }
    }

    /// Runs a deep search off the UI thread; the git child is killable through Cancel (M05 §5).
    pub(crate) fn start_search(&mut self, cx: &mut Context<Self>) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let query = self.search_query(cx);
        if query_is_empty(&query) {
            return;
        }
        remember_search(&query);
        self.recent_searches = recent_searches();
        // a new search supersedes a running one: kill the old git process
        self.search_cancel.kill();
        let root = repo.root.clone();
        let cancel = spork_git::Cancellable::default();
        self.search_cancel = cancel.clone();
        self.search = Some(SearchState {
            commits: Vec::new(),
            selected: None,
            details: None,
            running: true,
            cancelled: false,
            error: None,
        });
        self._search_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { spork_git::search(&root, &query, &cancel) })
                .await;
            this.update(cx, |this, cx| {
                let Some(state) = this.search.as_mut() else {
                    return;
                };
                state.running = false;
                match result {
                    Ok(commits) => state.commits = commits,
                    Err(e) => {
                        state.error = Some(if state.cancelled {
                            "Search cancelled.".to_owned()
                        } else {
                            e.to_string()
                        });
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Cancels the running deep search (M05 §5): kills the git child, not just the result.
    pub(crate) fn cancel_search(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.search.as_mut() else {
            return;
        };
        if !state.running {
            return;
        }
        state.cancelled = true;
        self.search_cancel.kill();
        cx.notify();
    }

    /// Selects a search result and loads its details through the existing details mechanism.
    pub(crate) fn select_search(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(Ok(repo)) = &self.repo else {
            return;
        };
        let Some(state) = self.search.as_mut() else {
            return;
        };
        let Some(commit) = state.commits.get(ix) else {
            return;
        };
        state.selected = Some(ix);
        state.details = None;
        let (root, id, parent) = (
            repo.root.clone(),
            commit.id.clone(),
            commit.parents.first().cloned(),
        );
        // replacing the task drops (cancels) a load for a previously clicked result
        self._search_details_load = Some(cx.spawn(async move |this, cx| {
            let details = cx
                .background_spawn(async move {
                    spork_git::commit_details(&root, &id, parent.as_deref())
                })
                .await;
            this.update(cx, |this, cx| {
                let Some(state) = this.search.as_mut() else {
                    return;
                };
                // shortcut: whole-patch parse; huge commits get a per-file lazy diff in M08
                let files = match &details {
                    Ok(d) if d.patch.len() <= MAX_PATCH_BYTES => {
                        DiffFile::parse(&d.patch).unwrap_or_default()
                    }
                    _ => Vec::new(),
                };
                this.search_diff
                    .update(cx, |state, cx| state.set_files(files, cx));
                state.details = Some(details);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The Search view: form on top, then the running / error / results state.
    pub(crate) fn search_view(&mut self, cx: &mut Context<Self>) -> Div {
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
        let base = v_flex().size_full().child(self.search_form(cx));
        let Some(state) = &self.search else {
            return base.child(centered(
                "Enter at least one field and press Search.".into(),
            ));
        };
        if state.running {
            return base.child(centered("Searching…".into()));
        }
        if let Some(error) = &state.error {
            return base.child(centered(error.clone()));
        }
        if state.commits.is_empty() {
            return base.child(centered("No commits match.".into()));
        }
        base.child(self.search_results(cx, state))
    }

    /// The results list, with the details pane once a row is selected.
    fn search_results(&self, cx: &mut Context<Self>, state: &SearchState) -> Div {
        let list = v_flex().size_full().child(search_header(cx)).child(
            uniform_list(
                "search",
                state.commits.len(),
                cx.processor(|this, range, _, cx| this.search_rows(range, cx)),
            )
            .flex_1()
            .min_h_0(),
        );
        let base = v_flex().size_full();
        if state.selected.is_some() {
            base.child(
                v_resizable("search-split")
                    .child(resizable_panel().child(list))
                    .child(
                        resizable_panel()
                            .size(px(380.))
                            .child(self.search_details_view(cx)),
                    ),
            )
        } else {
            base.child(list)
        }
    }

    /// Row processor for the results list: the commit-row style, without the graph column.
    pub(crate) fn search_rows(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(state) = &self.search else {
            return Vec::new();
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let theme = cx.theme();
        let (muted, hover, active) = (theme.muted_foreground, theme.list_hover, theme.list_active);
        let mono = theme.mono_font_family.clone();
        range
            .map(|ix| {
                let commit = &state.commits[ix];
                h_flex()
                    .id(ix)
                    .w_full()
                    .h(px(ROW_HEIGHT))
                    .px_3()
                    .gap_3()
                    .text_sm()
                    .when(state.selected == Some(ix), |row| row.bg(active))
                    .hover(move |row| row.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.select_search(ix, cx)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(commit.subject.clone()),
                    )
                    .child(
                        div()
                            .w(px(150.))
                            .flex_none()
                            .truncate()
                            .text_color(muted)
                            .child(commit.author.clone()),
                    )
                    .child(
                        div()
                            .w(px(50.))
                            .flex_none()
                            .text_color(muted)
                            .child(relative_age(commit.time, now)),
                    )
                    .child(
                        div()
                            .w(px(64.))
                            .flex_none()
                            .font_family(mono.clone())
                            .text_xs()
                            .text_color(muted)
                            .child(commit.id.chars().take(7).collect::<String>()),
                    )
                    .into_any_element()
            })
            .collect()
    }

    /// Details pane for the selected search result.
    pub(crate) fn search_details_view(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let pane = v_flex().size_full().border_t_1().border_color(border);
        let centered = |msg: String| {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(muted)
                .child(msg)
        };
        let Some(state) = &self.search else {
            return pane;
        };
        let Some(ix) = state.selected else {
            return pane;
        };
        let Some(commit) = state.commits.get(ix) else {
            return pane;
        };
        let details = match &state.details {
            None => return pane.child(centered("Loading commit…".into())),
            Some(Err(e)) => return pane.child(centered(e.to_string())),
            Some(Ok(d)) => d,
        };
        let body = if details.patch.len() > MAX_PATCH_BYTES {
            centered("This commit is too large to show inline.".into())
        } else if details.patch.is_empty() {
            centered("No file changes".into())
        } else {
            v_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(Diff::new(&self.search_diff).size_full())
        };
        pane.child(crate::commit_summary(commit, details, cx))
            .child(body)
    }
}

/// Header of the results list: the history columns minus the graph.
fn search_header(cx: &App) -> impl IntoElement {
    let muted = cx.theme().muted_foreground;
    h_flex()
        .h(px(ROW_HEIGHT))
        .flex_none()
        .px_3()
        .gap_3()
        .text_xs()
        .text_color(muted)
        .child(div().flex_1().min_w_0().child("Description"))
        .child(div().w(px(150.)).flex_none().child("Author"))
        .child(div().w(px(50.)).flex_none().child("Date"))
        .child(div().w(px(64.)).flex_none().child("Commit"))
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_RECENT_SEARCHES, QuickFilter, filter_matches, load_from, matches_filter,
        query_from_line, query_is_empty, query_label, query_to_line, remember_in,
    };
    use spork_git::{Commit, SearchQuery};

    fn commit(id: &str, author: &str, subject: &str) -> Commit {
        Commit {
            id: id.to_owned(),
            parents: Vec::new(),
            author: author.to_owned(),
            email: format!("{author}@example.com"),
            time: 0,
            subject: subject.to_owned(),
        }
    }

    #[test]
    fn filter_matches_sha_prefix_author_or_subject_case_insensitive() {
        let commits = vec![
            commit("deadbeefcafe", "Alice Smith", "Add parser"),
            commit("1234abcd", "Bob Jones", "fix: parser bug"),
        ];
        assert!(matches_filter(&commits[0], "DEADBEEF")); // SHA prefix, typed uppercase
        assert!(matches_filter(&commits[0], "ALICE")); // author, typed uppercase
        assert!(matches_filter(&commits[0], "add PARSER")); // subject, mixed case
        assert!(matches_filter(&commits[1], "bob jones"));
        assert!(matches_filter(&commits[1], "parser bug"));
        assert!(!matches_filter(&commits[0], "deadbeef00")); // wrong prefix, no other hit
        assert!(!matches_filter(&commits[1], "zzz"));
        assert!(matches_filter(&commits[1], "   ")); // blank matches everything
        assert_eq!(filter_matches(&commits, "PARSER"), vec![0, 1]); // list order kept
        assert_eq!(filter_matches(&commits, "alice"), vec![0]);
        assert_eq!(filter_matches(&commits, ""), vec![0, 1]);
        assert_eq!(filter_matches(&commits, "zzz"), Vec::<usize>::new());
    }

    #[test]
    fn filter_steps_wrap_around_and_highlights_report_positions() {
        let commits = [
            commit("a", "one", "x"),
            commit("b", "two", "x"),
            commit("c", "three", "x"),
            commit("d", "four", "x"),
        ];
        let mut f = QuickFilter::for_query(&commits, "x");
        assert_eq!(f.matches, vec![0, 1, 2, 3]);
        assert_eq!(f.current_ix(), Some(0));
        assert_eq!(f.step(false), Some(1));
        assert_eq!(f.step(false), Some(2));
        assert_eq!(f.step(false), Some(3));
        assert_eq!(f.step(false), Some(0)); // forward wraps
        assert_eq!(f.step(true), Some(3)); // backward wraps
        assert_eq!(f.highlight(3), Some(true)); // current
        assert_eq!(f.highlight(0), Some(false)); // another match (no longer current)
        assert_eq!(f.highlight(1), Some(false));
        assert_eq!(f.highlight(2), Some(false));
        let blank = QuickFilter::for_query(&commits, "  ");
        assert_eq!(blank.matches.len(), 4);
        assert_eq!(blank.highlight(0), None); // a blank query never highlights
        let mut none = QuickFilter::for_query(&commits, "nope");
        assert_eq!(none.matches, Vec::<usize>::new());
        assert_eq!(none.current_ix(), None);
        assert_eq!(none.step(false), None);
        assert_eq!(none.step(true), None);
    }

    #[test]
    fn query_lines_round_trip_and_reject_garbage() {
        let q = SearchQuery {
            message: "add parser".into(),
            author: "Alice".into(),
            committer: String::new(),
            content: "fn main".into(),
            path: "src/main.rs".into(),
            since: "2024-01-01".into(),
            until: String::new(),
        };
        let line = query_to_line(&q);
        assert_eq!(
            line,
            "add parser\tAlice\t\tfn main\tsrc/main.rs\t2024-01-01\t"
        );
        assert_eq!(query_from_line(&line), Some(q.clone()));
        assert_eq!(query_from_line("too few fields"), None);
        assert_eq!(query_from_line("a\tb\tc\td\te\tf"), None);
        assert!(query_is_empty(&SearchQuery::default()));
        assert!(!query_is_empty(&q));
        assert_eq!(
            query_label(&q),
            "add parser · Alice · fn main · src/main.rs"
        );
        assert_eq!(query_label(&SearchQuery::default()), "all commits");
    }

    #[test]
    fn recent_searches_dedupe_and_cap() {
        let tmp = std::env::temp_dir().join(format!("spork-searches-{}", std::process::id()));
        let file = tmp.join("nested/searches.txt");
        for i in 0..(MAX_RECENT_SEARCHES + 5) {
            let q = SearchQuery {
                message: format!("query {i}"),
                ..Default::default()
            };
            remember_in(&file, &q).unwrap();
        }
        let list = load_from(&file);
        assert_eq!(list.len(), MAX_RECENT_SEARCHES);
        assert_eq!(
            list[0].message,
            format!("query {}", MAX_RECENT_SEARCHES + 4)
        );
        // remembering an existing entry moves it to the top without duplicating
        let moved = list[2].clone();
        remember_in(&file, &moved).unwrap();
        let list = load_from(&file);
        assert_eq!(list.len(), MAX_RECENT_SEARCHES);
        assert_eq!(list[0], moved);
        assert_eq!(list.iter().filter(|q| **q == moved).count(), 1);
        std::fs::remove_dir_all(&tmp).ok();
    }
}
