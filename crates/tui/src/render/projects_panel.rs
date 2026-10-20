use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Cell, Row, Table, TableState};
use ratatui::Frame;

use super::util::{panel_block, spinner_frame, status_color, status_icon};
use crate::state::{AppState, Focus};

pub(super) fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let visible = state.visible_indices();

    let rows = visible.iter().map(|&index| {
        let item = &state.items[index];
        let (mut status, color) = item.pipeline.as_ref().map_or_else(
            || {
                if item.has_error() {
                    ("!".into(), Color::Red)
                } else if item.is_loading() {
                    (format!("{} loading", spinner_frame()), Color::Reset)
                } else {
                    ("-".into(), Color::Reset)
                }
            },
            |pipeline| {
                (
                    format!("{} {}", status_icon(&pipeline.status), pipeline.status),
                    status_color(&pipeline.status),
                )
            },
        );
        // Animated spinner while any action is in flight for this project (covers "run pipeline",
        // which has no pre-existing item row of its own).
        if state.project_has_in_flight(item.configured.id) {
            status = format!("{} {status}", spinner_frame());
        }

        Row::new([
            Cell::from(item.configured.name.as_str()),
            Cell::from(status).style(Style::default().fg(color)),
        ])
    });

    let count = if state.search.query.is_empty() {
        format!("[1] Projects ({})", visible.len())
    } else {
        format!(
            "[1] Projects ({}/{} · /{})",
            visible.len(),
            state.items.len(),
            state.search.query
        )
    };
    // With more than one configured instance, show only the active label plus its position. A full
    // tab strip would overflow and silently clip the title (hiding the active marker) once instances
    // are many or hostnames long. `[`/`]` cycles, so "where am I, of how many" is enough.
    let title = if state.instance_labels.len() > 1 {
        format!(
            "{count} · ‹{}› [{}/{}]",
            state.instance_labels[state.active_instance],
            state.active_instance + 1,
            state.instance_labels.len()
        )
    } else {
        count
    };

    let table = Table::new(rows, [Constraint::Min(12), Constraint::Length(14)])
        .header(Row::new(["Project", "Status"]))
        .block(panel_block(&title, state.focus == Focus::Projects))
        .row_highlight_style(Style::default().bg(Color::Blue).fg(Color::Black));

    let mut table_state = TableState::default();
    table_state.select(state.selected_visible_position());
    frame.render_stateful_widget(table, area, &mut table_state);

    let (_, viewport) = body_layout(area);
    super::util::render_vertical_scrollbar(
        frame,
        area,
        visible.len(),
        viewport,
        state.selected_visible_position().unwrap_or(0),
    );
}

/// On-screen rows the project list occupies (inside the border, below the header) and how many are
/// visible. Single source of truth shared with mouse hit-testing in `lib.rs`.
pub(crate) fn body_layout(area: Rect) -> (Rect, usize) {
    // The bordered box eats two rows and the header one more, leaving `height - 3` for rows.
    let viewport = usize::from(area.height).saturating_sub(3);
    let body = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(2),
        width: area.width.saturating_sub(2),
        height: u16::try_from(viewport).unwrap_or(u16::MAX),
    };
    (body, viewport)
}
