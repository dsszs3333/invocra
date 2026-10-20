use ratatui::layout::{Constraint, Direction, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use super::util::{
    centered_rect, format_duration, format_timestamp, panel_block, spinner_frame, status_color,
    status_icon,
};
use crate::actions::Action;
use crate::state::{
    ActionMenuState, AppState, CreateTagForm, EnvironmentsViewState, Focus, LogViewState,
    PipelinesViewState, ScreenMode, TagsViewState, ValueViewState, VariablesViewState,
};

pub(super) fn render_confirm(frame: &mut Frame, action: &Action) {
    let area = centered_rect(50, Constraint::Percentage(20), frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default().title("Confirm").borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = vec![
        Line::from(Span::styled(
            action.describe(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("y / Enter confirm    n / Esc cancel"),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

/// Renders the persistent right-hand log panel. Shows the open job's trace, or a hint when no
/// log has been opened yet.
pub(super) fn render_job_log(frame: &mut Frame, area: Rect, state: &AppState) {
    let view = state.log.as_ref();

    let title = match view {
        Some(view) => {
            let follow_marker = if view.follow { " [follow]" } else { "" };
            let loading_marker = if view.loading {
                format!(" {}", spinner_frame())
            } else {
                String::new()
            };
            let trailer = if let Some(notice) = &view.notice {
                format!(" — {notice}")
            } else if view.search.input {
                format!(
                    " — search: {}▏({} matches)",
                    view.search.query,
                    view.search.matches.len()
                )
            } else if !view.search.query.is_empty() {
                let position = view
                    .search
                    .current
                    .map_or_else(String::new, |pos| format!("{}/", pos + 1));
                if view.search.matches.is_empty() {
                    format!(" — search: {} (no matches)", view.search.query)
                } else {
                    format!(
                        " — search: {} ({position}{}) · n/N next/prev · Esc clear",
                        view.search.query,
                        view.search.matches.len()
                    )
                }
            } else if view.select_anchor.is_some() {
                format!(
                    " — VISUAL {} lines · y copy · Esc cancel",
                    view.selection_len()
                )
            } else {
                String::from(" — v select · y copy · / search")
            };
            format!(
                "[4] Log: {} ({}){follow_marker}{loading_marker}{trailer}",
                view.job_name, view.job_status
            )
        }
        None => String::from("[4] Log"),
    };

    // Clear the pane each frame so a previous (longer) trace can't leave stale cells behind.
    frame.render_widget(Clear, area);
    let block = panel_block(&title, state.focus == Focus::Log);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(view) = view else {
        super::invocra::render_placeholder(frame, inner);
        return;
    };

    if let Some(error) = &view.error {
        frame.render_widget(
            Paragraph::new(format!("Failed to load log: {error}"))
                .style(Style::default().fg(Color::Red))
                .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }

    let height = usize::from(inner.height);
    let max_offset = view.lines.len().saturating_sub(height);
    let offset = if view.follow {
        max_offset
    } else {
        view.scroll.min(max_offset)
    };
    let focused = state.focus == Focus::Log;
    let selection = view.selection_range();
    let selection_style = Style::default().bg(Color::Blue).fg(Color::White);
    let cursor_style = Style::default().add_modifier(Modifier::REVERSED);
    let match_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let current_match = view.search.current.map(|pos| view.search.matches[pos]);
    // ponytail: linear `contains` per visible row — matches are sorted, binary search if traces
    // with thousands of matches ever feel slow.
    let is_match = |i: usize| !view.search.query.is_empty() && view.search.matches.contains(&i);
    // Build only the rows actually on screen — a 2MB trace is tens of thousands of lines, and
    // styling/laying out all of them every frame is what made opening a log take seconds.
    let end = (offset + height).min(view.lines.len());
    let lines: Vec<Line> = view.lines[offset..end]
        .iter()
        .enumerate()
        .map(|(j, line)| {
            let i = offset + j;
            let span = Line::from(line.as_str());
            match selection {
                // Visual mode: highlight the selected range like nvim's `v`.
                Some((start, end)) if (start..=end).contains(&i) => span.style(selection_style),
                // Normal mode: mark the cursor line so it's clear where `v` will anchor.
                None if focused && i == view.cursor => span.style(cursor_style),
                // Search: the current match reads as "cursor + match", other matches just pop.
                None if current_match == Some(i) => {
                    span.style(match_style.add_modifier(Modifier::REVERSED))
                }
                None if is_match(i) => span.style(match_style),
                _ => span,
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);

    super::util::render_vertical_scrollbar(frame, area, view.lines.len(), height, offset);
}

pub(super) fn render_variables(frame: &mut Frame, view: &VariablesViewState) {
    let area = centered_rect(80, Constraint::Percentage(70), frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(format!("Variables: {}", view.project_name))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if view.loading {
        frame.render_widget(
            Paragraph::new(format!("{} Loading variables...", spinner_frame())),
            inner,
        );
        return;
    }
    if let Some(error) = &view.error {
        frame.render_widget(
            Paragraph::new(error.as_str())
                .style(Style::default().fg(Color::Yellow))
                .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }
    if view.variables.is_empty() {
        frame.render_widget(Paragraph::new("No variables defined"), inner);
        return;
    }

    // Reserve a top line for the filter input/indicator whenever a filter is active
    // or being typed; otherwise give all the space to the table and footer.
    let show_filter = view.searching || !view.filter.is_empty();
    let (filter_area, table_area, detail_area) = variables_chunks(inner, show_filter);

    if let Some(filter_area) = filter_area {
        render_variables_filter(frame, view, filter_area);
    }

    let matching = view.matching();
    if matching.is_empty() {
        frame.render_widget(
            Paragraph::new(format!("No variables match '{}'", view.filter))
                .style(Style::default().fg(Color::DarkGray)),
            table_area,
        );
        render_variable_detail(frame, view, detail_area);
        return;
    }

    let rows = matching.iter().enumerate().map(|(index, variable)| {
        let value = if view.revealed && index == view.selected {
            variable.value.clone()
        } else {
            String::from("••••••")
        };
        let mut flags = Vec::new();
        if variable.protected {
            flags.push("protected");
        }
        if variable.masked {
            flags.push("masked");
        }
        Row::new([
            Cell::from(variable.key.as_str()),
            Cell::from(
                variable
                    .environment_scope
                    .as_deref()
                    .unwrap_or("*")
                    .to_owned(),
            ),
            Cell::from(
                variable
                    .variable_type
                    .as_deref()
                    .unwrap_or("env_var")
                    .to_owned(),
            ),
            Cell::from(flags.join(",")),
            Cell::from(value),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Min(16),
            Constraint::Length(10),
            Constraint::Length(9),
            Constraint::Length(17),
            Constraint::Min(10),
        ],
    )
    .header(Row::new([
        "Key",
        "Scope",
        "Type",
        "Flags",
        "Value (s reveal)",
    ]))
    .row_highlight_style(Style::default().bg(Color::Blue).fg(Color::Black));

    let mut table_state = TableState::default();
    table_state.select(Some(view.selected));
    frame.render_stateful_widget(table, table_area, &mut table_state);

    // One row of the table area is the header, so the body shows `height - 1` rows.
    let viewport = usize::from(table_area.height).saturating_sub(1);
    super::util::render_vertical_scrollbar(frame, area, matching.len(), viewport, view.selected);

    render_variable_detail(frame, view, detail_area);
}

/// Splits the variables overlay interior into (optional filter line, table, detail footer).
/// Shared between render and mouse hit-testing so the layout can't drift.
fn variables_chunks(inner: Rect, show_filter: bool) -> (Option<Rect>, Rect, Rect) {
    let constraints: &[Constraint] = if show_filter {
        &[
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(5),
        ]
    } else {
        &[Constraint::Min(3), Constraint::Length(5)]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints.to_vec())
        .split(inner);
    if show_filter {
        (Some(chunks[0]), chunks[1], chunks[2])
    } else {
        (None, chunks[0], chunks[1])
    }
}

/// On-screen rows the variables table occupies (below its header) and how many are visible, or
/// `None` when the overlay isn't showing a clickable list. Mirrors `render_variables`'s geometry.
pub(crate) fn variables_body(term_area: Rect, view: &VariablesViewState) -> Option<(Rect, usize)> {
    if view.loading || view.error.is_some() || view.matching().is_empty() {
        return None;
    }
    let inner = centered_rect(80, Constraint::Percentage(70), term_area).inner(Margin::new(1, 1));
    let show_filter = view.searching || !view.filter.is_empty();
    let (_, table_area, _) = variables_chunks(inner, show_filter);
    let body = Rect {
        y: table_area.y.saturating_add(1),
        height: table_area.height.saturating_sub(1),
        ..table_area
    };
    Some((body, usize::from(body.height)))
}

/// On-screen rows the pipelines table occupies (below its header) and how many are visible, or
/// `None` when the overlay isn't showing a clickable list. Mirrors `render_pipelines`'s geometry.
pub(crate) fn pipelines_body(term_area: Rect, view: &PipelinesViewState) -> Option<(Rect, usize)> {
    if view.loading || view.error.is_some() || view.pipelines.is_empty() {
        return None;
    }
    let inner = centered_rect(70, Constraint::Percentage(60), term_area).inner(Margin::new(1, 1));
    let body = Rect {
        y: inner.y.saturating_add(1),
        height: inner.height.saturating_sub(1),
        ..inner
    };
    Some((body, usize::from(body.height)))
}

pub(super) fn render_tags(frame: &mut Frame, view: &TagsViewState) {
    let area = centered_rect(70, Constraint::Percentage(60), frame.area());
    frame.render_widget(Clear, area);

    let title = match &view.ref_ {
        Some(ref_) => format!("Tags: {} (new → {ref_})", view.project_name),
        None => format!("Tags: {}", view.project_name),
    };
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if view.loading {
        frame.render_widget(
            Paragraph::new(format!("{} Loading tags...", spinner_frame())),
            inner,
        );
    } else if let Some(error) = &view.error {
        frame.render_widget(
            Paragraph::new(error.as_str())
                .style(Style::default().fg(Color::Yellow))
                .wrap(Wrap { trim: true }),
            inner,
        );
    } else if view.tags.is_empty() {
        let (_, footer_area) = tags_chunks(inner);
        frame.render_widget(Paragraph::new("No tags yet"), inner);
        render_tags_footer(frame, footer_area);
    } else {
        let (table_area, footer_area) = tags_chunks(inner);
        let rows = view.tags.iter().map(|tag| {
            Row::new([
                Cell::from(tag.name.clone()),
                Cell::from(tag.message.clone().unwrap_or_default()),
                Cell::from(
                    tag.target
                        .as_deref()
                        .unwrap_or("-")
                        .chars()
                        .take(8)
                        .collect::<String>(),
                ),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Min(16),
                Constraint::Min(20),
                Constraint::Length(10),
            ],
        )
        .header(Row::new(["Tag", "Message", "Target"]))
        .row_highlight_style(Style::default().bg(Color::Blue).fg(Color::Black));

        let mut table_state = TableState::default();
        table_state.select(Some(view.selected));
        frame.render_stateful_widget(table, table_area, &mut table_state);

        let viewport = usize::from(table_area.height).saturating_sub(1);
        super::util::render_vertical_scrollbar(
            frame,
            area,
            view.tags.len(),
            viewport,
            view.selected,
        );
        render_tags_footer(frame, footer_area);
    }

    // Sub-views drawn on top of the list.
    if let Some(form) = &view.creating {
        render_create_tag_form(frame, form);
    } else if let Some(tag) = &view.viewing {
        render_tag_detail(frame, tag);
    }
}

/// Splits the tags overlay interior into (table, footer with key hints). Shared between the renderer
/// and mouse hit-testing so the clickable rows match what's drawn.
fn tags_chunks(inner: Rect) -> (Rect, Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(2)])
        .split(inner);
    (chunks[0], chunks[1])
}

/// Key hints below the tags table, mirroring the variables overlay footer.
fn render_tags_footer(frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::TOP)
        .title("j/k move · ⏎ view tag · n new tag · q back");
    frame.render_widget(block, area);
}

/// On-screen rows the tags table occupies (below its header) and how many are visible, or `None`
/// when there's nothing clickable (loading/error/empty, or a sub-view is open).
pub(crate) fn tags_body(term_area: Rect, view: &TagsViewState) -> Option<(Rect, usize)> {
    if view.loading || view.error.is_some() || view.tags.is_empty() || !view.in_list_mode() {
        return None;
    }
    let inner = centered_rect(70, Constraint::Percentage(60), term_area).inner(Margin::new(1, 1));
    let (table_area, _) = tags_chunks(inner);
    let body = Rect {
        y: table_area.y.saturating_add(1),
        height: table_area.height.saturating_sub(1),
        ..table_area
    };
    Some((body, usize::from(body.height)))
}

/// Read-only detail box for a tag: name, target commit, and the full (possibly annotated) message.
fn render_tag_detail(frame: &mut Frame, tag: &gitlab::Tag) {
    let area = centered_rect(60, Constraint::Percentage(50), frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(format!("Tag: {}", tag.name))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Target: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(tag.target.as_deref().unwrap_or("-").to_owned()),
        ]),
        Line::from(""),
    ];
    match &tag.message {
        Some(message) if !message.is_empty() => {
            lines.push(Line::from(Span::styled(
                "Message:",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            for line in message.lines() {
                lines.push(Line::from(line.to_owned()));
            }
        }
        _ => lines.push(Line::from(Span::styled(
            "Lightweight tag (no message)",
            Style::default().fg(Color::DarkGray),
        ))),
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "q / Esc back",
        Style::default().add_modifier(Modifier::DIM),
    )));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// Draws the inline "new tag" form centered over the tags list. The active field carries the block
/// cursor; the other is shown plain.
fn render_create_tag_form(frame: &mut Frame, form: &CreateTagForm) {
    let area = centered_rect(60, Constraint::Length(11), frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default().title("New tag").borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let cursor = |active: bool| if active { "▌" } else { "" };
    let mut lines = vec![
        Line::from("Name:"),
        Line::from(Span::styled(
            format!("> {}{}", form.name, cursor(form.field == 0)),
            Style::default().fg(Color::Yellow),
        )),
        Line::from(""),
        Line::from("Message (optional → annotated tag):"),
        Line::from(Span::styled(
            format!("> {}{}", form.message, cursor(form.field == 1)),
            Style::default().fg(Color::Yellow),
        )),
    ];
    if let Some(error) = &form.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.as_str(),
            Style::default().fg(Color::Red),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter create    Tab switch field    Esc cancel",
        Style::default().add_modifier(Modifier::DIM),
    )));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

/// The log pane's text area and the line offset currently shown at its top row, or `None` when
/// there's nothing selectable. Mirrors the scroll math in `render_job_log` so mouse selection maps
/// clicks to the same lines the user sees.
pub(crate) fn log_body(
    term_area: Rect,
    focus: Focus,
    mode: ScreenMode,
    view: &LogViewState,
) -> Option<(Rect, usize)> {
    if view.error.is_some() || view.lines.is_empty() {
        return None;
    }
    let inner = super::panel_areas(term_area, focus, mode)
        .log
        .inner(Margin::new(1, 1));
    let height = usize::from(inner.height);
    let max_offset = view.lines.len().saturating_sub(height);
    let offset = if view.follow {
        max_offset
    } else {
        view.scroll.min(max_offset)
    };
    Some((inner, offset))
}

/// One-line KEY filter input shown above the table.
fn render_variables_filter(frame: &mut Frame, view: &VariablesViewState, area: Rect) {
    let cursor = if view.searching { "▏" } else { "" };
    let line = Line::from(vec![
        Span::styled(
            "Filter KEY: ",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("{}{cursor}", view.filter)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// Footer below the variables table: full value of the selected row plus copy hint.
fn render_variable_detail(frame: &mut Frame, view: &VariablesViewState, area: Rect) {
    let block = Block::default()
        .borders(Borders::TOP)
        .title("/ filter · s reveal · y copy · ⏎ open value · o open in browser");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(variable) = view.selected_variable() else {
        return;
    };

    let value_line = if view.revealed {
        Line::from(vec![
            Span::styled(
                format!("{} = ", variable.key),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(variable.value.clone()),
        ])
    } else {
        Line::from(Span::styled(
            format!("{} = •••••• (press s to reveal)", variable.key),
            Style::default().fg(Color::DarkGray),
        ))
    };

    let mut lines = vec![value_line];
    if let Some(notice) = &view.notice {
        lines.push(Line::from(Span::styled(
            notice.clone(),
            Style::default().fg(Color::Green),
        )));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// Centered interior (inside the border) of the value window. Shared by the renderer and the mouse
/// hit-test so a drag maps to exactly the characters drawn.
fn value_inner(term_area: Rect) -> Rect {
    centered_rect(70, Constraint::Percentage(60), term_area).inner(Margin::new(1, 1))
}

/// Full-screen-ish viewer for one variable's whole value, wrapped at the window width with the
/// current mouse selection highlighted.
pub(super) fn render_value_window(frame: &mut Frame, view: &ValueViewState) {
    let area = centered_rect(70, Constraint::Percentage(60), frame.area());
    frame.render_widget(Clear, area);

    let title = match &view.notice {
        Some(notice) => format!("Value: {} — {notice}", view.key),
        None => format!("Value: {} — drag to select · y copy · q back", view.key),
    };
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = usize::from(inner.width).max(1);
    let height = usize::from(inner.height);
    let total_rows = view.value.len().div_ceil(width);
    // ponytail: clamp scroll display-side; the stored offset may overshoot until the next render.
    let scroll = view.scroll.min(total_rows.saturating_sub(height));
    let selection = view.selection_range();

    let lines: Vec<Line> = (scroll..scroll.saturating_add(height))
        .filter_map(|row| {
            let start = row.checked_mul(width)?;
            if start >= view.value.len() {
                return None;
            }
            let end = (start + width).min(view.value.len());
            Some(value_row_line(&view.value[start..end], start, selection))
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
    super::util::render_vertical_scrollbar(frame, area, total_rows, height, scroll);
}

/// Builds one wrapped row, highlighting the part covered by the inclusive `selection` range.
fn value_row_line(row: &[char], start: usize, selection: Option<(usize, usize)>) -> Line<'static> {
    let plain = || Line::from(row.iter().collect::<String>());
    let Some((sel_start, sel_end)) = selection else {
        return plain();
    };
    let end = start + row.len();
    if sel_end < start || sel_start >= end {
        return plain();
    }
    let lo = sel_start.max(start) - start;
    let hi = (sel_end + 1).min(end) - start;
    let mut spans = Vec::new();
    if lo > 0 {
        spans.push(Span::raw(row[..lo].iter().collect::<String>()));
    }
    spans.push(Span::styled(
        row[lo..hi].iter().collect::<String>(),
        Style::default().bg(Color::Blue).fg(Color::Black),
    ));
    if hi < row.len() {
        spans.push(Span::raw(row[hi..].iter().collect::<String>()));
    }
    Line::from(spans)
}

/// Character index under `point` in the value window, or `None` when the pointer is outside the text
/// area or the value is empty. Mirrors `render_value_window`'s wrap so clicks hit what the user sees.
pub(crate) fn value_char_at(
    term_area: Rect,
    view: &ValueViewState,
    point: Position,
) -> Option<usize> {
    let inner = value_inner(term_area);
    if !inner.contains(point) {
        return None;
    }
    let width = usize::from(inner.width);
    if width == 0 || view.value.is_empty() {
        return None;
    }
    let total_rows = view.value.len().div_ceil(width);
    let scroll = view
        .scroll
        .min(total_rows.saturating_sub(usize::from(inner.height)));
    let row = scroll + usize::from(point.y - inner.y);
    let row_start = row.checked_mul(width)?;
    if row_start >= view.value.len() {
        return Some(view.value.len() - 1);
    }
    let row_len = (view.value.len() - row_start).min(width);
    let col = usize::from(point.x - inner.x).min(row_len.saturating_sub(1));
    Some(row_start + col)
}

/// Renders the environments/deployments overlay: one row per environment with its last deployment.
pub(super) fn render_environments(frame: &mut Frame, view: &EnvironmentsViewState) {
    let area = centered_rect(80, Constraint::Percentage(60), frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(format!(
            "Environments: {} — enter/o open · q close",
            view.project_name
        ))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if view.loading {
        frame.render_widget(
            Paragraph::new(format!("{} Loading environments...", spinner_frame())),
            inner,
        );
        return;
    }
    if let Some(error) = &view.error {
        frame.render_widget(
            Paragraph::new(error.as_str())
                .style(Style::default().fg(Color::Yellow))
                .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }
    if view.environments.is_empty() {
        frame.render_widget(Paragraph::new("No environments"), inner);
        return;
    }

    let rows = view.environments.iter().map(|environment| {
        let deployment = environment.last_deployment.as_ref();
        let status_cell = match deployment.and_then(|deployment| deployment.status.as_ref()) {
            Some(status) => Cell::from(format!("{} {}", status_icon(status), status))
                .style(Style::default().fg(status_color(status))),
            None => Cell::from("—"),
        };
        let when = deployment
            .and_then(|deployment| deployment.created_at.as_deref())
            .map_or_else(|| "-".to_owned(), format_timestamp);
        Row::new([
            Cell::from(environment.name.as_str()),
            status_cell,
            Cell::from(
                deployment
                    .and_then(|deployment| deployment.ref_.as_deref())
                    .unwrap_or("-")
                    .to_owned(),
            ),
            Cell::from(
                deployment
                    .and_then(|deployment| deployment.deployable.as_ref())
                    .map_or("-", |job| job.name.as_str())
                    .to_owned(),
            ),
            Cell::from(when),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Min(14),
            Constraint::Length(14),
            Constraint::Min(10),
            Constraint::Min(12),
            Constraint::Length(22),
        ],
    )
    .header(Row::new([
        "Environment",
        "Status",
        "Ref",
        "Job",
        "Deployed (UTC)",
    ]))
    .row_highlight_style(Style::default().bg(Color::Blue).fg(Color::Black));

    let mut table_state = TableState::default();
    table_state.select(Some(view.selected));
    frame.render_stateful_widget(table, inner, &mut table_state);

    let viewport = usize::from(inner.height).saturating_sub(1);
    super::util::render_vertical_scrollbar(
        frame,
        area,
        view.environments.len(),
        viewport,
        view.selected,
    );
}

pub(super) fn render_pipelines(frame: &mut Frame, view: &PipelinesViewState) {
    let area = centered_rect(70, Constraint::Percentage(60), frame.area());
    frame.render_widget(Clear, area);

    let title = match &view.ref_ {
        Some(ref_) => format!("Pipelines: {} @ {ref_}", view.project_name),
        None => format!("Pipelines: {}", view.project_name),
    };
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if view.loading {
        frame.render_widget(
            Paragraph::new(format!("{} Loading pipelines...", spinner_frame())),
            inner,
        );
        return;
    }
    if let Some(error) = &view.error {
        frame.render_widget(
            Paragraph::new(error.as_str())
                .style(Style::default().fg(Color::Yellow))
                .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }
    if view.pipelines.is_empty() {
        frame.render_widget(Paragraph::new("No pipelines"), inner);
        return;
    }

    let rows = view.pipelines.iter().map(|pipeline| {
        let started = pipeline
            .created_at
            .as_deref()
            .map_or_else(|| "-".to_owned(), format_timestamp);
        let duration = pipeline
            .duration
            .map_or_else(|| "-".into(), format_duration);
        Row::new([
            Cell::from(format!("#{}", pipeline.iid.unwrap_or(pipeline.id))),
            Cell::from(format!(
                "{} {}",
                status_icon(&pipeline.status),
                pipeline.status
            ))
            .style(Style::default().fg(status_color(&pipeline.status))),
            Cell::from(pipeline.ref_.as_deref().unwrap_or("-").to_owned()),
            Cell::from(started),
            Cell::from(duration),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(14),
            Constraint::Min(12),
            Constraint::Length(22),
            Constraint::Length(8),
        ],
    )
    .header(Row::new([
        "Pipeline",
        "Status",
        "Ref",
        "Started (UTC)",
        "Time",
    ]))
    .row_highlight_style(Style::default().bg(Color::Blue).fg(Color::Black));

    let mut table_state = TableState::default();
    table_state.select(Some(view.selected));
    frame.render_stateful_widget(table, inner, &mut table_state);

    // One row of the inner area is the header, so the body shows `height - 1` rows.
    let viewport = usize::from(inner.height).saturating_sub(1);
    super::util::render_vertical_scrollbar(
        frame,
        area,
        view.pipelines.len(),
        viewport,
        view.selected,
    );
}

pub(super) fn render_action_menu(frame: &mut Frame, menu: &ActionMenuState) {
    let area = centered_rect(56, Constraint::Percentage(40), frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(menu.title.as_str())
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = menu.items.iter().map(|item| {
        // Disabled rows are greyed and append their reason to the description.
        let description = match &item.disabled_reason {
            Some(reason) => format!("{} ({reason})", item.description),
            None => item.description.clone(),
        };
        let style = if item.disabled_reason.is_some() {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        };
        Row::new([Cell::from(item.label.as_str()), Cell::from(description)]).style(style)
    });
    let table = Table::new(rows, [Constraint::Length(18), Constraint::Min(12)])
        .row_highlight_style(Style::default().bg(Color::Blue).fg(Color::Black));

    let mut table_state = TableState::default();
    table_state.select(Some(menu.selected));
    frame.render_stateful_widget(table, inner, &mut table_state);
}

pub(super) fn render_help(frame: &mut Frame) {
    let area = centered_rect(60, Constraint::Percentage(70), frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default().title("Help").borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines: Vec<Line> = HELP_ENTRIES
        .iter()
        .map(|(keys, action)| {
            Line::from(vec![
                Span::styled(
                    format!("  {keys:<14}"),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(*action),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

const HELP_ENTRIES: &[(&str, &str)] = &[
    ("tab", "cycle projects / branches / pipeline / log"),
    (
        "1 / 2 / 3 / 4",
        "focus projects / branches / pipeline / log",
    ),
    ("h / l", "focus left column / log panel"),
    ("+ / _", "expand focused panel to full screen / collapse"),
    ("j / k", "move down / up (scroll in log)"),
    (
        "enter",
        "pipeline panel / pin branch / open job log / downstream pipeline",
    ),
    ("u", "unpin branch (track repo default)"),
    ("/", "filter projects or branches"),
    ("esc", "back / clear filter"),
    ("r", "refresh all projects"),
    ("g", "jump to a project with a running pipeline"),
    ("a", "add a GitLab instance"),
    ("[ / ]", "switch GitLab instance tab"),
    ("N", "run new pipeline on the tracked branch"),
    ("x", "action menu for the focused job / pipeline"),
    ("R", "retry pipeline (projects) / job (pipeline panel)"),
    ("c", "cancel running pipeline"),
    ("p", "play manual job"),
    ("L", "open job log (f follow, g/G top/bottom)"),
    (
        "v (in log)",
        "visual select lines — y copy selection, esc cancel",
    ),
    ("y (in log)", "copy whole log to clipboard"),
    ("/ (in log)", "search in log — enter jump, esc clear"),
    ("n / N (in log)", "next / previous search match"),
    (
        "v",
        "show CI/CD variables (/ filter, s reveal, y copy, o open settings)",
    ),
    ("P", "browse pipeline history (enter view, esc latest)"),
    ("e", "environments / deployments (enter/o open URL)"),
    ("T", "view tags (enter view, n new tag on selected branch)"),
    (
        "5 (MRs)",
        "merge requests — m scope, enter pipeline, x approve/merge",
    ),
    ("o", "open in browser (job / pipeline / project)"),
    ("?", "toggle this help"),
    ("q", "quit"),
];
