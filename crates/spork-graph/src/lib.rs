//! Commit-graph lane layout (docs/specs/M05 §2). Pure: commits in display order in, drawing instructions out.
//!
//! Each lane remembers the commit it is waiting for. A commit lands in the first lane waiting for it
//! (or a free one), other lanes waiting for it merge into it, its first parent continues straight down,
//! and extra parents (merges) take free lanes. A lane keeps its colour for as long as it lives.

/// Vertical anchor inside a row: top edge, the node's centre line, or bottom edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pos {
    Top,
    Center,
    Bottom,
}

/// A line to draw inside one row, from `(lane, pos)` to `(lane, pos)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edge {
    pub from: (usize, Pos),
    pub to: (usize, Pos),
    pub color: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Lane of this commit's node.
    pub node: usize,
    /// Colour index of the node (an index into the UI palette, taken modulo its length).
    pub color: usize,
    pub edges: Vec<Edge>,
    /// Number of lanes this row needs to draw.
    pub width: usize,
}

#[derive(Clone)]
struct Lane<'a> {
    waits_for: &'a str,
    color: usize,
}

/// Lays out commits given newest-first in display order as `(id, parent ids)`.
pub fn layout<'a, P>(commits: impl IntoIterator<Item = (&'a str, P)>) -> Vec<Row>
where
    P: IntoIterator<Item = &'a str>,
{
    let mut lanes: Vec<Option<Lane<'a>>> = Vec::new();
    let mut next_color = 0;
    let mut new_color = || {
        next_color += 1;
        next_color - 1
    };
    let mut rows = Vec::new();

    for (id, parents) in commits {
        let parents: Vec<&str> = parents.into_iter().collect();
        let before = lanes.len();
        let waiting = |l: &Option<Lane>| l.as_ref().is_some_and(|l| l.waits_for == id);

        let (node, color) = match lanes.iter().position(waiting) {
            Some(i) => (i, lanes[i].as_ref().unwrap().color),
            None => {
                let i = lanes
                    .iter()
                    .position(Option::is_none)
                    .unwrap_or(lanes.len());
                if i == lanes.len() {
                    lanes.push(None);
                }
                (i, new_color())
            }
        };

        let mut edges = Vec::new();
        for (i, lane) in lanes.iter_mut().enumerate() {
            let Some(l) = lane else { continue };
            if l.waits_for == id {
                // this lane ends here (it may be the node's own lane, or a branch merging into it)
                edges.push(Edge {
                    from: (i, Pos::Top),
                    to: (node, Pos::Center),
                    color: l.color,
                });
                *lane = None;
            } else {
                edges.push(Edge {
                    from: (i, Pos::Top),
                    to: (i, Pos::Bottom),
                    color: l.color,
                });
            }
        }

        if let Some((first, rest)) = parents.split_first() {
            lanes[node] = Some(Lane {
                waits_for: first,
                color,
            });
            edges.push(Edge {
                from: (node, Pos::Center),
                to: (node, Pos::Bottom),
                color,
            });
            for p in rest {
                let target = lanes
                    .iter()
                    .position(|l| l.as_ref().is_some_and(|l| l.waits_for == *p));
                let (j, c) = match target {
                    Some(j) => (j, lanes[j].as_ref().unwrap().color),
                    None => {
                        let j = lanes
                            .iter()
                            .position(Option::is_none)
                            .unwrap_or(lanes.len());
                        let c = new_color();
                        if j == lanes.len() {
                            lanes.push(None);
                        }
                        lanes[j] = Some(Lane {
                            waits_for: p,
                            color: c,
                        });
                        (j, c)
                    }
                };
                edges.push(Edge {
                    from: (node, Pos::Center),
                    to: (j, Pos::Bottom),
                    color: c,
                });
            }
        }

        while matches!(lanes.last(), Some(None)) {
            lanes.pop();
        }
        rows.push(Row {
            node,
            color,
            edges,
            width: before.max(lanes.len()).max(node + 1),
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use Pos::*;

    fn run(commits: &[(&'static str, &[&'static str])]) -> Vec<Row> {
        layout(commits.iter().map(|(id, ps)| (*id, ps.iter().copied())))
    }

    fn e(from: (usize, Pos), to: (usize, Pos), color: usize) -> Edge {
        Edge { from, to, color }
    }

    #[test]
    fn linear_history_is_one_straight_lane() {
        let rows = run(&[("c", &["b"]), ("b", &["a"]), ("a", &[])]);
        assert!(
            rows.iter()
                .all(|r| r.node == 0 && r.width == 1 && r.color == 0)
        );
        assert_eq!(rows[0].edges, vec![e((0, Center), (0, Bottom), 0)]);
        assert_eq!(
            rows[1].edges,
            vec![e((0, Top), (0, Center), 0), e((0, Center), (0, Bottom), 0)]
        );
        assert_eq!(rows[2].edges, vec![e((0, Top), (0, Center), 0)]); // root: nothing below
    }

    #[test]
    fn merge_opens_a_lane_that_closes_at_the_fork_point() {
        // m merges feature f into main; both come from base b
        let rows = run(&[("m", &["b", "f"]), ("f", &["b"]), ("b", &[])]);
        assert_eq!((rows[0].node, rows[0].width), (0, 2));
        assert_eq!(
            rows[0].edges,
            vec![
                e((0, Center), (0, Bottom), 0),
                e((0, Center), (1, Bottom), 1)
            ]
        );
        assert_eq!((rows[1].node, rows[1].color), (1, 1)); // f sits in the merge's lane, keeps its colour
        assert_eq!(
            rows[1].edges,
            vec![
                e((0, Top), (0, Bottom), 0),
                e((1, Top), (1, Center), 1),
                e((1, Center), (1, Bottom), 1)
            ]
        );
        // both lanes wait for b: they converge into one node and the graph narrows back
        assert_eq!(rows[2].node, 0);
        assert_eq!(
            rows[2].edges,
            vec![e((0, Top), (0, Center), 0), e((1, Top), (0, Center), 1)]
        );
        assert_eq!(rows[2].width, 2);
    }

    #[test]
    fn unrelated_tips_get_their_own_lanes_and_colours() {
        let rows = run(&[("a", &["base"]), ("b", &["base"]), ("base", &[])]);
        assert_eq!((rows[0].node, rows[0].color), (0, 0));
        assert_eq!((rows[1].node, rows[1].color), (1, 1));
        assert_eq!(rows[1].edges[0], e((0, Top), (0, Bottom), 0)); // a's lane passes by
        assert_eq!(rows[2].node, 0);
    }

    #[test]
    fn octopus_merge_takes_one_lane_per_extra_parent() {
        let rows = run(&[
            ("o", &["p1", "p2", "p3"]),
            ("p3", &[]),
            ("p2", &[]),
            ("p1", &[]),
        ]);
        assert_eq!(rows[0].width, 3);
        assert_eq!(rows[0].edges.len(), 3);
        assert_eq!((rows[1].node, rows[2].node, rows[3].node), (2, 1, 0));
    }

    #[test]
    fn freed_lane_is_reused() {
        // x ends (root) in lane 1, so the next new tip y reuses lane 1 instead of growing to lane 2
        let rows = run(&[("a", &["b"]), ("x", &[]), ("y", &["b"]), ("b", &[])]);
        assert_eq!(rows[1].node, 1);
        assert_eq!(rows[2].node, 1);
        assert_eq!(rows[2].width, 2);
    }

    #[test]
    fn parent_outside_the_loaded_range_keeps_its_lane_open() {
        let rows = run(&[("a", &["not-loaded"])]);
        assert_eq!(rows[0].edges, vec![e((0, Center), (0, Bottom), 0)]);
    }

    #[test]
    fn large_linear_history_lays_out_quickly() {
        let ids: Vec<String> = (0..200_000).map(|i| i.to_string()).collect();
        let start = std::time::Instant::now();
        let rows = layout(
            ids.iter()
                .enumerate()
                .map(|(i, id)| (id.as_str(), ids.get(i + 1).map(String::as_str))),
        );
        assert_eq!(rows.len(), 200_000);
        assert!(
            start.elapsed().as_secs() < 2,
            "layout too slow: {:?}",
            start.elapsed()
        );
    }
}
