use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use gitlab::MergeRequest;

use super::util::{panel_block, spinner_frame, status_color, status_icon};
use crate::state::{AppState, Focus, HeadPipelineSlot};

/// Renders the `[5] MRs` panel. Collapsed (title line only) it still shows the count, so the
/// panel is informative without stealing space from the log.
pub(crate) fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let panel = &state.mr_panel;
    let focused = state.focus == Focus::MergeRequests;

    let count = if panel.mrs.len() == 50 {
        String::from("50+")
    } else {
        panel.mrs.len().to_string()
    };
    let loading_marker = if panel.loading {
        format!(" {}", spinner_frame())
    } else {
        String::new()
    };
    let title = format!(
        "[5] MRs ({count}) — {}{loading_marker}",
        panel.scope.label()
    );
    let block = panel_block(&title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return; // collapsed to the title line
    }

    if panel.loading && panel.mrs.is_empty() {
        frame.render_widget(
            Paragraph::new(format!("{} Loading merge requests...", spinner_frame())),
            inner,
        );
        return;
    }
    if let Some(error) = &panel.error {
        frame.render_widget(
            Paragraph::new(error.as_str())
                .style(Style::default().fg(Color::Yellow))
                .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }
    if panel.mrs.is_empty() {
        frame.render_widget(Paragraph::new("No open merge requests"), inner);
        return;
    }

    let rows = panel.mrs.iter().map(|mr| {
        Row::new([
            Cell::from(format!("!{}", mr.iid)),
            Cell::from(mr.title.as_str()),
            Cell::from(
                mr.author
                    .as_ref()
                    .map_or("-", |a| a.username.as_str())
                    .to_owned(),
            ),
            Cell::from(format!("{}→{}", mr.source_branch, mr.target_branch)),
            ci_cell(state, mr),
            Cell::from(mr_status(mr)),
        ])
    });
    let highlight = if focused {
        Style::default().bg(Color::Blue).fg(Color::Black)
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Min(16),
            Constraint::Length(10),
            Constraint::Min(12),
            Constraint::Length(12),
            Constraint::Length(12),
        ],
    )
    .header(Row::new([
        "MR", "Title", "Author", "Branches", "CI", "Status",
    ]))
    .row_highlight_style(highlight);

    let mut table_state = TableState::default();
    table_state.select(Some(panel.selected));
    frame.render_stateful_widget(table, inner, &mut table_state);

    let viewport = usize::from(inner.height).saturating_sub(1);
    super::util::render_vertical_scrollbar(frame, area, panel.mrs.len(), viewport, panel.selected);
}

/// The CI column: the head pipeline's status once fetched, a spinner while loading, `-` when the
/// MR has no pipeline, and `?` before the row has been selected (fetch is selection-driven).
fn ci_cell(state: &AppState, mr: &MergeRequest) -> Cell<'static> {
    match state.mr_panel.head_pipelines.get(&(mr.project_id, mr.iid)) {
        Some(HeadPipelineSlot::Loading) => Cell::from(spinner_frame().to_string()),
        Some(HeadPipelineSlot::Loaded(Some(pipeline))) => Cell::from(format!(
            "{} {}",
            status_icon(&pipeline.status),
            pipeline.status
        ))
        .style(Style::default().fg(status_color(&pipeline.status))),
        Some(HeadPipelineSlot::Loaded(None)) => Cell::from("-"),
        Some(HeadPipelineSlot::Failed(_)) => {
            Cell::from("error").style(Style::default().fg(Color::Yellow))
        }
        None => Cell::from("?"),
    }
}

/// The Status column: draft/conflicts trump the server's detailed merge status.
fn mr_status(mr: &MergeRequest) -> String {
    if mr.draft {
        String::from("draft")
    } else if mr.has_conflicts {
        String::from("conflicts")
    } else {
        match mr.detailed_merge_status.as_deref() {
            Some("mergeable") | None => String::from("open"),
            Some(other) => other.replace('_', " "),
        }
    }
}
