use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use gitlab::{Bridge, CiStatus, Job, Pipeline};

use super::util::{
    format_duration, format_timestamp, panel_block, spinner_frame, status_color, status_icon,
};
use crate::state::{AppState, Focus, ProjectSnapshot};

/// Rows reserved for the pipeline summary above the jobs table.
const HEADER_HEIGHT: u16 = 6;

pub(super) fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let focused = state.focus == Focus::Detail;
    let is_history = state.is_viewing_history();
    let title = if is_history {
        "[3] Pipeline (history)"
    } else {
        "[3] Pipeline"
    };
    let block = panel_block(title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(item) = state.selected_item() else {
        frame.render_widget(Paragraph::new("No project selected"), inner);
        return;
    };

    let pipeline = state.active_pipeline();
    let jobs = state.active_jobs();

    // Per-item stopwatch lives in `state.timer_secs`; history jobs are terminal so they read the
    // server value automatically, no special-casing needed here.
    let project_id = item.configured.id;

    // The history view fetches its jobs lazily; reflect its loading/error in the header.
    let view = state.viewed_pipeline_for_selected();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(HEADER_HEIGHT), Constraint::Min(2)])
        .split(inner);

    frame.render_widget(
        Paragraph::new(header_lines(item, pipeline, is_history, state, project_id))
            .wrap(Wrap { trim: true }),
        chunks[0],
    );
    if let Some(error) = view.and_then(|view| view.error.as_deref()) {
        frame.render_widget(
            Paragraph::new(format!("Failed to load jobs: {error}"))
                .style(Style::default().fg(Color::Red))
                .wrap(Wrap { trim: true }),
            chunks[1],
        );
    } else if view.is_some_and(|view| view.loading) {
        frame.render_widget(
            Paragraph::new(format!("{} Loading jobs...", spinner_frame())),
            chunks[1],
        );
    } else {
        render_jobs(
            frame,
            chunks[1],
            jobs,
            state.active_bridges(),
            focused,
            state.selected_job,
            state,
            project_id,
        );
    }

    // The jobs table starts after the header block and its own header row; map those onto the
    // bordered panel so the scrollbar thumb tracks the selected job.
    let (_, viewport) = jobs_layout(area);
    super::util::render_vertical_scrollbar(
        frame,
        area,
        jobs.len() + state.active_bridges().len(),
        viewport,
        state.selected_job,
    );
}

/// On-screen rows the jobs table occupies (below the panel border, the summary header block, and
/// the table's own header row) and how many are visible. Shared with mouse hit-testing.
pub(crate) fn jobs_layout(area: Rect) -> (Rect, usize) {
    // Border eats 1 top row, the summary header block `HEADER_HEIGHT`, the table header 1 more.
    let skip = 1 + HEADER_HEIGHT + 1;
    let body = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(skip),
        width: area.width.saturating_sub(2),
        // Border also eats the bottom row, hence `skip + 1`.
        height: area.height.saturating_sub(skip + 1),
    };
    (body, usize::from(body.height))
}

fn header_lines<'a>(
    item: &'a ProjectSnapshot,
    pipeline: Option<&'a Pipeline>,
    is_history: bool,
    state: &AppState,
    project_id: u64,
) -> Vec<Line<'a>> {
    let title = item.project.as_ref().map_or_else(
        || item.configured.name.clone(),
        |project| project.name.clone(),
    );
    let pipeline_line = pipeline.map_or_else(
        || Line::from("Pipeline: No pipeline"),
        |pipeline| {
            // Spin the icon while a retry/cancel for this pipeline is in flight.
            let icon = if state.is_in_flight(project_id, pipeline.id) {
                spinner_frame()
            } else {
                status_icon(&pipeline.status)
            };
            Line::from(vec![
                Span::raw(format!(
                    "Pipeline: #{} ",
                    pipeline.iid.unwrap_or(pipeline.id)
                )),
                Span::styled(
                    format!("{} {}", icon, pipeline.status),
                    Style::default().fg(status_color(&pipeline.status)),
                ),
            ])
        },
    );
    let branch = pipeline
        .and_then(|pipeline| pipeline.ref_.as_deref())
        .or_else(|| {
            item.project
                .as_ref()
                .and_then(|project| project.default_branch.as_deref())
        })
        .unwrap_or("-")
        .to_owned();
    let commit = pipeline
        .and_then(|pipeline| pipeline.sha.as_deref())
        .map_or_else(|| "-".into(), |sha| sha.chars().take(8).collect::<String>());
    let duration = pipeline
        .and_then(|pipeline| {
            pipeline.duration.map(|seconds| {
                state.timer_secs(
                    project_id,
                    pipeline.id,
                    seconds,
                    pipeline.status.is_active(),
                )
            })
        })
        .map_or_else(|| "-".into(), format_duration);
    let started = pipeline
        .and_then(|pipeline| pipeline.created_at.as_deref())
        .map_or_else(|| "-".into(), |ts| format!("{} UTC", format_timestamp(ts)));

    let mut lines = vec![
        Line::from(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        )),
        pipeline_line,
        Line::from(format!(
            "Branch:   {branch}   Commit: {commit}   Duration: {duration}"
        )),
        Line::from(format!("Started:  {started}")),
    ];
    if is_history {
        lines.push(Line::from(Span::styled(
            "viewing past pipeline — Esc for latest",
            Style::default().fg(Color::DarkGray),
        )));
    }
    if let Some(error) = item.error_message() {
        lines.push(Line::from(Span::styled(
            format!("Error: {error}"),
            Style::default().fg(Color::Red),
        )));
    }
    lines
}

#[allow(clippy::too_many_arguments)]
fn render_jobs(
    frame: &mut Frame,
    area: Rect,
    jobs: &[Job],
    bridges: &[Bridge],
    focused: bool,
    selected_job: usize,
    state: &AppState,
    project_id: u64,
) {
    if jobs.is_empty() && bridges.is_empty() {
        frame.render_widget(Paragraph::new("Jobs: none"), area);
        return;
    }

    let job_rows = jobs.iter().map(|job| {
        let duration = job.duration.map_or_else(
            || "-".into(),
            |seconds| {
                // Monotonic stopwatch for a running job; finished jobs read the server value.
                let secs = state.timer_secs(
                    project_id,
                    job.id,
                    crate::state::job_duration_secs(seconds),
                    job.status.is_active(),
                );
                format_duration(secs)
            },
        );
        // Spin the status icon while an action is in flight or the job itself is running.
        // Only `Running` spins — other active states (created/pending) keep their static icon.
        let icon = if state.is_in_flight(project_id, job.id) || job.status == CiStatus::Running {
            spinner_frame()
        } else {
            status_icon(&job.status)
        };
        Row::new([
            Cell::from(format!("{} {}", icon, job.status))
                .style(Style::default().fg(status_color(&job.status))),
            Cell::from(job.name.as_str()),
            Cell::from(job.stage.as_deref().unwrap_or_default().to_owned()),
            Cell::from(duration),
        ])
    });

    // Bridge (trigger) rows sit under the regular jobs. The icon/color reflect the downstream
    // pipeline's status when it exists (that's what the user cares about), else the bridge job's.
    let bridge_rows = bridges.iter().map(|bridge| {
        let status = bridge
            .downstream_pipeline
            .as_ref()
            .map_or(&bridge.status, |downstream| &downstream.status);
        let target = bridge.downstream_pipeline.as_ref().map_or_else(
            || String::from("⇒ downstream: not created"),
            |downstream| format!("⇒ downstream #{}", downstream.id),
        );
        Row::new([
            Cell::from(format!("{} {}", status_icon(status), status))
                .style(Style::default().fg(status_color(status))),
            Cell::from(format!("{} {target}", bridge.name)),
            Cell::from(bridge.stage.as_deref().unwrap_or_default().to_owned()),
            Cell::from("-"),
        ])
    });
    let rows = job_rows.chain(bridge_rows);

    let highlight = if focused {
        Style::default().bg(Color::Blue).fg(Color::Black)
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(13),
            Constraint::Min(12),
            Constraint::Length(12),
            Constraint::Length(8),
        ],
    )
    .header(Row::new(["Status", "Job", "Stage", "Time"]))
    .row_highlight_style(highlight);

    let mut table_state = TableState::default();
    table_state.select(Some(selected_job));
    frame.render_stateful_widget(table, area, &mut table_state);
}
