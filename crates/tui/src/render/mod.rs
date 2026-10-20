pub(crate) mod branches_panel;
pub(crate) mod detail_panel;
pub(crate) mod invocra;
mod help_bar;
mod mr_panel;
pub(crate) mod overlays;
pub(crate) mod projects_panel;
pub(crate) mod util;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::Frame;

use crate::state::{AppState, Focus, Overlay, ScreenMode};

/// Below this terminal width the 35/65 side-by-side layout gets too cramped to read, so the
/// dashboard drops to a single stacked column (lazygit-style).
const NARROW_WIDTH: u16 = 80;
/// Below this left-column height the three stacked panels can't each show useful rows, so they
/// switch to an accordion around the focused panel.
const SHORT_LEFT_HEIGHT: u16 = 24;
/// Height of a collapsed (unfocused) accordion panel: just its bordered title line.
const COLLAPSED: u16 = 1;

/// Stacks `count` panels vertically. When `focused` names one, it grows to fill the column and the
/// rest collapse to their title line (lazygit's accordion); otherwise the panels split evenly.
fn accordion(area: Rect, count: usize, focused: Option<usize>) -> Vec<Rect> {
    let constraints: Vec<Constraint> = match focused {
        Some(f) => (0..count)
            .map(|i| {
                if i == f {
                    Constraint::Min(COLLAPSED)
                } else {
                    Constraint::Length(COLLAPSED)
                }
            })
            .collect(),
        None => (0..count)
            .map(|_| Constraint::Ratio(1, count as u32))
            .collect(),
    };
    Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area)
        .to_vec()
}

/// Screen rectangles of the four panels, so mouse handling can hit-test the cursor against the
/// same layout the renderer drew. Recomputed from the terminal size (the layout is fixed, not
/// state-dependent), which keeps render and input in lockstep without sharing mutable state.
pub(crate) struct PanelAreas {
    pub(crate) projects: Rect,
    pub(crate) branches: Rect,
    pub(crate) detail: Rect,
    pub(crate) log: Rect,
    pub(crate) merge_requests: Rect,
    pub(crate) help: Rect,
}

/// Splits `area` into the dashboard's panels. Used by both the renderer and mouse hit-testing.
///
/// Adapts to the terminal size like lazygit: when too narrow, all panels stack into one column;
/// when the left column is too short, its panels accordion around the focused one. Both collapse
/// unfocused panels to their title line, which the user re-expands by focusing them (Tab / 1-4).
pub(crate) fn panel_areas(area: Rect, focus: Focus, mode: ScreenMode) -> PanelAreas {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(1)])
        .split(area);
    let (body, help) = (chunks[0], chunks[1]);

    // Expanded: the focused panel fills the body and the rest collapse to their title line,
    // regardless of terminal size. Reuses the accordion the narrow/short layouts already use.
    // Too narrow for side-by-side: same treatment — stack all five panels around the focus.
    if mode == ScreenMode::Expanded || body.width < NARROW_WIDTH {
        let order = [
            Focus::Projects,
            Focus::Branches,
            Focus::Detail,
            Focus::Log,
            Focus::MergeRequests,
        ];
        let rows = accordion(body, 5, order.iter().position(|f| *f == focus));
        return PanelAreas {
            projects: rows[0],
            branches: rows[1],
            detail: rows[2],
            log: rows[3],
            merge_requests: rows[4],
            help,
        };
    }

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(body);

    // Left column stacks three boxes: projects on top, then the branches and pipeline of the
    // selected project below. When too short to show all three, they accordion around the focus.
    let left = if body[0].height < SHORT_LEFT_HEIGHT {
        let order = [Focus::Projects, Focus::Branches, Focus::Detail];
        accordion(body[0], 3, order.iter().position(|f| *f == focus))
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(34),
                Constraint::Percentage(33),
                Constraint::Percentage(33),
            ])
            .split(body[0])
            .to_vec()
    };

    // Right column: the log on top, the MR panel below. Exactly one is expanded — the MR panel
    // only while focused, the log otherwise — so the log keeps almost the full column height in
    // everyday use and the MR panel collapses to its title line with the count.
    let right = accordion(body[1], 2, Some(usize::from(focus == Focus::MergeRequests)));

    PanelAreas {
        projects: left[0],
        branches: left[1],
        detail: left[2],
        log: right[0],
        merge_requests: right[1],
        help,
    }
}

/// Number of log lines visible in the pane for a given terminal area. Mirrors the bordered-block
/// inset that `render_job_log` applies, so the input layer's visual-mode cursor math stays in step
/// with what the user actually sees on screen.
pub(crate) fn log_inner_height(term_area: Rect, focus: Focus, mode: ScreenMode) -> usize {
    usize::from(
        panel_areas(term_area, focus, mode)
            .log
            .height
            .saturating_sub(2),
    )
}

/// Inner width of the log pane (minus its borders), used to wrap long trace lines to the window.
pub(crate) fn log_inner_width(term_area: Rect, focus: Focus, mode: ScreenMode) -> usize {
    usize::from(
        panel_areas(term_area, focus, mode)
            .log
            .width
            .saturating_sub(2),
    )
}

pub(crate) fn render(frame: &mut Frame, state: &AppState) {
    let areas = panel_areas(frame.area(), state.focus, state.screen_mode);

    projects_panel::render(frame, areas.projects, state);
    branches_panel::render(frame, areas.branches, state);
    detail_panel::render(frame, areas.detail, state);
    // The wide right pane always shows the job log (a placeholder until one is opened).
    overlays::render_job_log(frame, areas.log, state);
    mr_panel::render(frame, areas.merge_requests, state);
    help_bar::render(frame, areas.help, state);

    match &state.overlay {
        Overlay::Help => overlays::render_help(frame),
        Overlay::Confirm(action) => overlays::render_confirm(frame, action),
        Overlay::Variables(view) => overlays::render_variables(frame, view),
        Overlay::VariableValue(view) => overlays::render_value_window(frame, view),
        Overlay::Pipelines(view) => overlays::render_pipelines(frame, view),
        Overlay::Environments(view) => overlays::render_environments(frame, view),
        Overlay::Tags(view) => overlays::render_tags(frame, view),
        Overlay::ActionMenu(menu) => overlays::render_action_menu(frame, menu),
        Overlay::None => {}
    }
}

#[cfg(test)]
mod tests {
    use config::ProjectEntry;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use ratatui::layout::Rect;

    use super::{panel_areas, render, COLLAPSED, NARROW_WIDTH};
    use crate::refresh::RefreshSources;
    use crate::state::{AppState, Focus, LogViewState, Overlay, PipelinesViewState, ScreenMode};

    fn draw(state: &AppState) {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, state)).expect("draw");
    }

    #[test]
    fn narrow_terminal_stacks_panels_and_accordions_around_focus() {
        let area = Rect::new(0, 0, NARROW_WIDTH - 1, 30);
        let a = panel_areas(area, Focus::Branches, ScreenMode::Normal);
        // Single column: every panel spans the full width.
        for r in [a.projects, a.branches, a.detail, a.log] {
            assert_eq!(r.width, area.width);
        }
        // Focused panel expands; the rest collapse to their title line.
        assert_eq!(a.projects.height, COLLAPSED);
        assert_eq!(a.detail.height, COLLAPSED);
        assert_eq!(a.log.height, COLLAPSED);
        assert!(a.branches.height > COLLAPSED);
    }

    #[test]
    fn short_left_column_accordions_but_keeps_side_by_side_log() {
        let area = Rect::new(0, 0, 120, 12);
        let a = panel_areas(area, Focus::Detail, ScreenMode::Normal);
        // Wide enough to stay two-column: the log keeps its own pane beside the left stack.
        assert!(a.log.width < area.width);
        assert_eq!(a.projects.height, COLLAPSED);
        assert_eq!(a.branches.height, COLLAPSED);
        assert!(a.detail.height > COLLAPSED);
    }

    #[test]
    fn expanded_mode_gives_focused_panel_the_body_and_collapses_the_rest() {
        // Wide and tall: would normally be side-by-side, but Expanded forces the accordion.
        let area = Rect::new(0, 0, 200, 60);
        let a = panel_areas(area, Focus::Log, ScreenMode::Expanded);
        assert!(a.log.height > COLLAPSED);
        assert_eq!(a.projects.height, COLLAPSED);
        assert_eq!(a.branches.height, COLLAPSED);
        assert_eq!(a.detail.height, COLLAPSED);
        // The log spans the full width when expanded (no side column).
        assert_eq!(a.log.width, area.width);
    }

    #[test]
    fn renders_without_panicking_at_cramped_sizes() {
        let state = AppState::new(RefreshSources::default());
        for (w, h) in [(40, 20), (120, 10), (30, 8)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal.draw(|frame| render(frame, &state)).expect("draw");
        }
    }

    #[test]
    fn renders_empty_dashboard_without_panicking() {
        let state = AppState::new(RefreshSources::default());
        draw(&state);
    }

    #[test]
    fn renders_projects_help_overlay_and_search_without_panicking() {
        let mut state = AppState::new(RefreshSources {
            projects: vec![ProjectEntry {
                name: String::from("group/api"),
                id: 1,
            }],
            groups: Vec::new(),
        });
        draw(&state);

        state.overlay = Overlay::Help;
        draw(&state);

        state.overlay = Overlay::None;
        state.search.active = true;
        state.search.query = String::from("nomatch");
        state.ensure_selection();
        draw(&state);
    }

    #[test]
    fn renders_pipeline_history_overlay_without_panicking() {
        let mut state = AppState::new(RefreshSources {
            projects: vec![ProjectEntry {
                name: String::from("group/api"),
                id: 1,
            }],
            groups: Vec::new(),
        });

        // Loading state.
        state.overlay = Overlay::Pipelines(PipelinesViewState {
            project_id: 1,
            project_name: String::from("group/api"),
            ref_: Some(String::from("main")),
            loading: true,
            pipelines: Vec::new(),
            selected: 0,
            error: None,
        });
        draw(&state);

        // Populated state.
        state.overlay = Overlay::Pipelines(PipelinesViewState {
            project_id: 1,
            project_name: String::from("group/api"),
            ref_: Some(String::from("main")),
            loading: false,
            pipelines: vec![
                gitlab::Pipeline {
                    id: 42,
                    iid: Some(9),
                    status: "success".into(),
                    ref_: Some(String::from("main")),
                    web_url: None,
                    duration: Some(75),
                    created_at: Some(String::from("2026-06-13T09:58:00Z")),
                    updated_at: Some(String::from("2026-06-13T10:00:00Z")),
                    sha: None,
                },
                gitlab::Pipeline {
                    id: 41,
                    iid: Some(8),
                    status: "failed".into(),
                    ref_: Some(String::from("main")),
                    web_url: None,
                    duration: None,
                    created_at: None,
                    updated_at: None,
                    sha: None,
                },
            ],
            selected: 1,
            error: None,
        });
        draw(&state);
    }

    #[test]
    fn renders_split_screen_job_log_without_panicking() {
        let mut state = AppState::new(RefreshSources {
            projects: vec![ProjectEntry {
                name: String::from("group/api"),
                id: 1,
            }],
            groups: Vec::new(),
        });
        state.focus = Focus::Log;
        state.log = Some(LogViewState {
            project_id: 1,
            job_id: 10,
            job_name: String::from("build"),
            job_status: String::from("running"),
            lines: vec![String::from("line one"), String::from("line two")],
            raw: vec![String::from("line one"), String::from("line two")],
            wrap_width: 0,
            scroll: 0,
            follow: true,
            loading: false,
            generation: 1,
            error: None,
            notice: None,
            cursor: 0,
            select_anchor: None,
            viewport_height: 0,
            search: crate::state::LogSearchState::default(),
        });
        draw(&state);
    }

    #[test]
    fn branches_panel_surfaces_branch_fetch_error() {
        let mut state = AppState::new(RefreshSources {
            projects: vec![ProjectEntry {
                name: String::from("group/api"),
                id: 1,
            }],
            groups: Vec::new(),
        });
        state.focus = Focus::Branches;
        state.apply_branches_loaded(1, Err(String::from("boom")));

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, &state)).expect("draw");

        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            rendered.contains("Failed to load branches"),
            "branch fetch error should be visible in the branches panel"
        );
    }
}
