use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use super::util::{panel_block, spinner_frame};
use crate::state::{AppState, Focus};

pub(super) fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let focused = state.focus == Focus::Branches;

    let Some(project_id) = state.selected_project_id else {
        let block = panel_block("[2] Branches", focused);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(Paragraph::new("No project selected"), inner);
        return;
    };

    let name = state
        .selected_item()
        .map_or("", |item| item.configured.name.as_str());
    let title = format!("[2] Branches: {name}");
    let block = panel_block(&title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Loading / error / empty states, mirroring how the projects panel and variables overlay
    // surface their non-list conditions.
    if let Some(error) = state.branches.error(project_id) {
        frame.render_widget(
            Paragraph::new(format!("Failed to load branches: {error}"))
                .style(Style::default().fg(Color::Yellow)),
            inner,
        );
        return;
    }
    if !state.branches.has_cache(project_id) {
        frame.render_widget(
            Paragraph::new(format!("{} Loading branches...", spinner_frame())),
            inner,
        );
        return;
    }

    let show_filter = state.branch_search.active || !state.branch_search.query.is_empty();
    let (filter_area, table_area) = table_layout(inner, show_filter);

    if let Some(filter_area) = filter_area {
        render_filter(frame, filter_area, state);
    }

    let matching = state.visible_branches();
    if matching.is_empty() {
        let message = if show_filter {
            format!("No branches match '{}'", state.branch_search.query)
        } else {
            String::from("No branches")
        };
        frame.render_widget(
            Paragraph::new(message).style(Style::default().fg(Color::DarkGray)),
            table_area,
        );
        return;
    }

    let tracked = state.effective_ref(project_id);
    let rows = matching.iter().map(|branch| {
        let is_tracked = tracked.as_deref() == Some(branch.name.as_str());
        let marker = if is_tracked { "●" } else { " " };
        let label = if branch.default {
            "(default)"
        } else if state.ref_overrides.get(&project_id).map(String::as_str) == Some(&branch.name) {
            "(pinned)"
        } else {
            ""
        };
        let name_style = if is_tracked {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        Row::new([
            Cell::from(Span::styled(
                format!("{marker} {}", branch.name),
                name_style,
            )),
            Cell::from(Span::styled(label, Style::default().fg(Color::DarkGray))),
        ])
    });

    let highlight = if focused {
        Style::default().bg(Color::Blue).fg(Color::Black)
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    let table = Table::new(rows, [Constraint::Min(10), Constraint::Length(9)])
        .row_highlight_style(highlight);

    let mut table_state = TableState::default();
    table_state.select(Some(state.selected_branch));
    frame.render_stateful_widget(table, table_area, &mut table_state);

    // The branch table has no header, so the body fills the whole table area.
    super::util::render_vertical_scrollbar(
        frame,
        area,
        matching.len(),
        usize::from(table_area.height),
        state.selected_branch,
    );
}

/// Renders the branch filter and its input cursor.
fn render_filter(frame: &mut Frame, filter_area: Rect, state: &AppState) {
    let cursor = if state.branch_search.active {
        "▏"
    } else {
        ""
    };
    let line = Line::from(vec![
        Span::styled(
            "Filter branch: ",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("{}{cursor}", state.branch_search.query)),
    ]);
    frame.render_widget(Paragraph::new(line), filter_area);
}

/// Splits the panel interior into the optional filter line and the branch table area.
fn table_layout(inner: Rect, show_filter: bool) -> (Option<Rect>, Rect) {
    if show_filter {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(inner);
        (Some(chunks[0]), chunks[1])
    } else {
        (None, inner)
    }
}

/// On-screen rows the branch list occupies and how many are visible, or `None` when the panel is
/// showing something other than a branch list (no project / loading / error / empty). Shares
/// `table_layout` with render so the two can't drift.
pub(crate) fn body_layout(area: Rect, state: &AppState) -> Option<(Rect, usize)> {
    let project_id = state.selected_project_id?;
    if state.branches.error(project_id).is_some()
        || !state.branches.has_cache(project_id)
        || state.visible_branches().is_empty()
    {
        return None;
    }
    // Interior of the borders-all panel block.
    let inner = area.inner(Margin::new(1, 1));
    let show_filter = state.branch_search.active || !state.branch_search.query.is_empty();
    let (_, table_area) = table_layout(inner, show_filter);
    Some((table_area, usize::from(table_area.height)))
}
