//! Commit list with graph (docs/specs/M05): one row per commit, graph painted per row from `spork_graph::Row`.

use std::collections::HashMap;

use gpui_kit::component::{ActiveTheme as _, Theme, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use spork_git::{Commit, Ref, RefKind};
use spork_graph::{Pos, Row};

pub const ROW_HEIGHT: f32 = 28.;
const LANE_WIDTH: f32 = 14.;
const NODE_RADIUS: f32 = 4.;
/// Graph column stops growing after this many lanes (M05 §2 "compress when lanes > 40" comes later).
const MAX_VISIBLE_LANES: usize = 16;

/// Everything the list needs, computed off the UI thread.
pub struct History {
    pub commits: Vec<Commit>,
    pub rows: Vec<Row>,
    /// Commit id → ref badges pointing at it.
    pub labels: HashMap<String, Vec<Label>>,
    pub graph_lanes: usize,
}

pub struct Label {
    pub name: String,
    pub kind: RefKind,
    pub is_head: bool,
}

impl History {
    pub fn new(commits: Vec<Commit>, refs: &[Ref]) -> Self {
        let rows = spork_graph::layout(
            commits
                .iter()
                .map(|c| (c.id.as_str(), c.parents.iter().map(String::as_str))),
        );
        let mut labels: HashMap<String, Vec<Label>> = HashMap::new();
        for r in refs {
            labels.entry(r.commit.clone()).or_default().push(Label {
                name: r.name.clone(),
                kind: r.kind,
                is_head: r.is_head,
            });
        }
        // current branch first, then local, remote, tags
        for list in labels.values_mut() {
            list.sort_by_key(|l| (!l.is_head, l.kind as u8));
        }
        let graph_lanes = rows
            .iter()
            .map(|r| r.width)
            .max()
            .unwrap_or(1)
            .min(MAX_VISIBLE_LANES);
        Self {
            commits,
            rows,
            labels,
            graph_lanes,
        }
    }
}

fn lane_colors(theme: &Theme) -> [Hsla; 6] {
    [
        theme.blue,
        theme.green,
        theme.yellow,
        theme.magenta,
        theme.cyan,
        theme.red,
    ]
}

/// The graph cell of one row.
pub fn graph_cell(row: &Row, is_merge: bool, lanes: usize, cx: &App) -> impl IntoElement {
    let row = row.clone();
    let colors = lane_colors(cx.theme());
    let background = cx.theme().background;
    let color = move |i: usize| colors[i % colors.len()];
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let x = |lane: usize| bounds.origin.x + px(LANE_WIDTH * lane as f32 + LANE_WIDTH / 2.);
            let y = |pos: Pos| match pos {
                Pos::Top => bounds.origin.y,
                Pos::Center => bounds.origin.y + bounds.size.height / 2.,
                Pos::Bottom => bounds.origin.y + bounds.size.height,
            };
            for edge in &row.edges {
                let (a, b) = (
                    point(x(edge.from.0), y(edge.from.1)),
                    point(x(edge.to.0), y(edge.to.1)),
                );
                let mut path = PathBuilder::stroke(px(1.5));
                path.move_to(a);
                if a.x == b.x {
                    path.line_to(b);
                } else {
                    // S-curve with vertical tangents at both ends
                    let mid = (a.y + b.y) / 2.;
                    path.cubic_bezier_to(b, point(a.x, mid), point(b.x, mid));
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, color(edge.color));
                }
            }
            let r = px(NODE_RADIUS);
            let center = point(x(row.node), y(Pos::Center));
            let node = Bounds::new(point(center.x - r, center.y - r), size(r * 2., r * 2.));
            // merges are hollow rings, other commits solid dots
            let fill = if is_merge {
                background
            } else {
                color(row.color)
            };
            window.paint_quad(quad(
                node,
                r,
                fill,
                px(1.5),
                color(row.color),
                BorderStyle::default(),
            ));
        },
    )
    .w(px(LANE_WIDTH * lanes as f32 + 6.))
    .h_full()
    .flex_none()
}

pub fn label_badge(label: &Label, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let tint = match (label.is_head, label.kind) {
        (true, _) => theme.blue,
        (_, RefKind::Local) => theme.green,
        (_, RefKind::Remote) => theme.muted_foreground,
        (_, RefKind::Tag) => theme.yellow,
    };
    div()
        .flex_none()
        .px_1p5()
        .rounded(px(4.))
        .bg(tint.opacity(0.18))
        .text_color(tint)
        .text_xs()
        .when(label.is_head, |d| d.font_weight(FontWeight::SEMIBOLD))
        .child(label.name.clone())
}

/// "now", "5m", "3h", "4d", "6mo", "2y" — compact relative age.
pub fn relative_age(then: i64, now: i64) -> String {
    let s = (now - then).max(0);
    match s {
        0..60 => "now".into(),
        60..3_600 => format!("{}m", s / 60),
        3_600..86_400 => format!("{}h", s / 3_600),
        86_400..2_592_000 => format!("{}d", s / 86_400),
        2_592_000..31_536_000 => format!("{}mo", s / 2_592_000),
        _ => format!("{}y", s / 31_536_000),
    }
}

pub fn header(lanes: usize, cx: &App) -> impl IntoElement {
    let muted = cx.theme().muted_foreground;
    h_flex()
        .h(px(ROW_HEIGHT))
        .flex_none()
        .px_3()
        .gap_3()
        .text_xs()
        .text_color(muted)
        .child(
            div()
                .w(px(LANE_WIDTH * lanes as f32 + 6.))
                .flex_none()
                .child("Graph"),
        )
        .child(div().flex_1().min_w_0().child("Description"))
        .child(div().w(px(150.)).flex_none().child("Author"))
        .child(div().w(px(50.)).flex_none().child("Date"))
        .child(div().w(px(64.)).flex_none().child("Commit"))
}

#[cfg(test)]
mod tests {
    use super::relative_age;

    #[test]
    fn ages_are_compact() {
        let now = 1_000_000_000;
        assert_eq!(relative_age(now, now), "now");
        assert_eq!(relative_age(now - 120, now), "2m");
        assert_eq!(relative_age(now - 3 * 3_600, now), "3h");
        assert_eq!(relative_age(now - 4 * 86_400, now), "4d");
        assert_eq!(relative_age(now - 65 * 86_400, now), "2mo");
        assert_eq!(relative_age(now - 800 * 86_400, now), "2y");
        assert_eq!(relative_age(now + 50, now), "now"); // clock skew
    }
}
