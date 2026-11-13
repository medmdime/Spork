//! Interactive rebase editor: reorder commits and pick / squash / fixup / drop each one
//! (docs/specs/M11 §3). Git does the rewrite; this only builds the plan.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use spork_git::{Commit, TodoAction};

use crate::{RepoView, View};

pub struct RebasePlan {
    /// Commits after this revision are rewritten.
    pub base: String,
    pub rows: Vec<(Commit, TodoAction)>,
}

const ACTIONS: [(TodoAction, &str); 4] = [
    (TodoAction::Pick, "Pick"),
    (TodoAction::Squash, "Squash"),
    (TodoAction::Fixup, "Fixup"),
    (TodoAction::Drop, "Drop"),
];

impl RepoView {
    /// Loads the commits after `id`'s parent and opens the plan editor.
    pub(crate) fn start_interactive_rebase(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let base = format!("{id}^");
        let range_base = base.clone();
        self.run_op(
            window,
            cx,
            move |root| spork_git::rebase_range(root, &range_base),
            move |this, commits, _, cx| {
                let rows = commits.into_iter().map(|c| (c, TodoAction::Pick)).collect();
                this.rebase_plan = Some(RebasePlan { base, rows });
                this.view = View::Rebase;
                cx.notify();
            },
        );
    }

    fn run_rebase_plan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(plan) = self.rebase_plan.take() else {
            return;
        };
        let todo: Vec<_> = plan.rows.iter().map(|(c, a)| (c.id.clone(), *a)).collect();
        let base = plan.base;
        self.view = View::History;
        self.run_outcome(window, cx, "Rebase finished".into(), move |root| {
            spork_git::interactive_rebase(root, &base, &todo)
        });
    }

    fn plan_row(&self, ix: usize, cx: &mut Context<Self>) -> Div {
        let Some(plan) = &self.rebase_plan else {
            return div();
        };
        let (commit, action) = &plan.rows[ix];
        let last = plan.rows.len() - 1;
        let theme = cx.theme();
        let dropped = *action == TodoAction::Drop;
        let move_row = |id: &'static str, icon: IconName, to: Option<usize>| {
            Button::new(SharedString::from(format!("{id}-{ix}")))
                .ghost()
                .xsmall()
                .icon(icon)
                .disabled(to.is_none())
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let (Some(plan), Some(to)) = (this.rebase_plan.as_mut(), to) {
                        plan.rows.swap(ix, to);
                        cx.notify();
                    }
                }))
        };
        h_flex()
            .h(px(36.))
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .child(move_row("up", IconName::ChevronUp, ix.checked_sub(1)))
            .child(move_row(
                "down",
                IconName::ChevronDown,
                (ix < last).then_some(ix + 1),
            ))
            .child(h_flex().gap_1().children(ACTIONS.iter().map(|&(a, label)| {
                Button::new(SharedString::from(format!("{label}-{ix}")))
                    .xsmall()
                    .label(label)
                    .when(a == *action, |b| b.primary())
                    .when(a != *action, |b| b.ghost())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(plan) = this.rebase_plan.as_mut() {
                            plan.rows[ix].1 = a;
                            cx.notify();
                        }
                    }))
            })))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .when(dropped, |d| {
                        d.line_through().text_color(theme.muted_foreground)
                    })
                    .child(commit.subject.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.muted_foreground)
                    .child(commit.id.chars().take(7).collect::<String>()),
            )
    }

    pub(crate) fn rebase_view(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(plan) = &self.rebase_plan else {
            return div().into_any_element();
        };
        let count = plan.rows.len();
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        v_flex()
            .size_full()
            .child(
                v_flex()
                    .px_4()
                    .py_3()
                    .gap_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Interactive rebase"))
                    .child(div().text_xs().text_color(muted).child(format!(
                        "{count} commits, oldest first. Git replays them top to bottom; squash and fixup melt a commit into the one above."
                    ))),
            )
            .child(
                v_flex()
                    .id("rebase-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children((0..count).map(|ix| self.plan_row(ix, cx))),
            )
            .child(
                h_flex()
                    .px_4()
                    .py_3()
                    .gap_2()
                    .justify_end()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(Button::new("rebase-cancel").ghost().label("Cancel").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.rebase_plan = None;
                            this.view = View::History;
                            cx.notify();
                        },
                    )))
                    .child(
                        Button::new("rebase-start")
                            .primary()
                            .label("Start rebase")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| this.run_rebase_plan(window, cx))),
                    ),
            )
            .into_any_element()
    }
}
