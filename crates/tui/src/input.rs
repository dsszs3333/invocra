use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use gitlab::{CiStatus, Pipeline};

use crate::actions::Action;
use crate::state::{
    ActionMenuItem, ActionMenuState, AppState, CreateTagForm, EnvironmentsViewState, Focus,
    HeadPipelineSlot, LogSearchState, LogViewState, MenuEffect, MrScope, Overlay,
    PipelinesViewState, ScreenMode, TagsViewState, ValueViewState, VariablesViewState,
    ViewedPipeline,
};

/// Lines jumped by PageUp/PageDown in the log viewer.
const LOG_PAGE_STEP: usize = 20;

/// Moves one row within the list bounds, returning whether the selection changed.
pub(crate) fn step(selected: &mut usize, len: usize, down: bool) -> bool {
    if down {
        if *selected < len.saturating_sub(1) {
            *selected += 1;
            return true;
        }
    } else if *selected > 0 {
        *selected -= 1;
        return true;
    }
    false
}

/// Side effects requested by key handling; executed by the run loop.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Quit,
    Refresh,
    /// Scan every dashboard project for a running pipeline, then pin+jump to the branch being built.
    ScanRunning,
    /// Switch the active GitLab instance tab by `delta` (wraps around).
    SwitchInstance {
        delta: i32,
    },
    /// Suspend the dashboard to run the setup wizard and add a new instance.
    AddInstance,
    /// Re-fetch a single project's pipeline, e.g. after its tracked branch changed.
    RefreshProject {
        project_id: u64,
    },
    Dispatch(Action),
    FetchTrace {
        project_id: u64,
        job_id: u64,
        generation: u64,
    },
    FetchVariables {
        project_id: u64,
    },
    /// Fetch the recent pipelines for the history overlay.
    FetchPipelines {
        project_id: u64,
        ref_: Option<String>,
    },
    /// Fetch a project's environments for the environments overlay.
    FetchEnvironments {
        project_id: u64,
    },
    /// Fetch the project's tags for the tags overlay.
    FetchTags {
        project_id: u64,
    },
    /// Create a tag on `ref_`; a non-empty `message` makes it annotated.
    CreateTag {
        project_id: u64,
        tag_name: String,
        ref_: String,
        message: String,
    },
    /// Fetch the MR panel's list. `project_id` is `Some` for the project scope, `None` for the
    /// instance-wide "my MRs" scopes.
    FetchMergeRequests {
        scope: MrScope,
        project_id: Option<u64>,
    },
    /// Lazily fetch the head pipeline of one MR row.
    FetchMrHeadPipeline {
        project_id: u64,
        iid: u64,
    },
    /// Fetch the jobs of a specific (historical or downstream) pipeline the user opened to
    /// inspect. `project_id` keys the dashboard entry; `pipeline_project_id` is the project the
    /// pipeline belongs to (differs for downstream pipelines of multi-project bridges).
    FetchPipelineJobs {
        project_id: u64,
        pipeline_project_id: u64,
        pipeline_id: u64,
    },
    /// Download a job's artifacts archive to the user's Downloads directory.
    DownloadArtifacts {
        project_id: u64,
        job_id: u64,
        job_name: String,
        filename: String,
    },
    CopyToClipboard {
        value: String,
    },
    /// Open a URL in the user's default browser.
    OpenUrl {
        url: String,
    },
}

/// Applies a key press to the state and returns commands for the run loop to execute.
///
/// Input layers take priority in order: search input, overlay, panel keys.
/// Runs the focused panel/overlay's primary action, exactly as pressing Enter would. Used by a
/// double-click to activate the item under the cursor.
pub(crate) fn activate(state: &mut AppState) -> Vec<Command> {
    apply_key(state, &KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
}

pub(crate) fn apply_key(state: &mut AppState, key: &KeyEvent) -> Vec<Command> {
    if key.kind != KeyEventKind::Press {
        return Vec::new();
    }

    if state.search.active {
        handle_search_input(state, key.code);
        return Vec::new();
    }

    if state.branch_search.active {
        handle_branch_search_input(state, key.code);
        return Vec::new();
    }

    if state.overlay != Overlay::None {
        return handle_overlay_input(state, key.code);
    }

    handle_panel_input(state, key.code)
}

/// Live `/` filter editing for the branches panel; mirrors the projects filter but clamps the
/// branch cursor instead of the project selection.
fn handle_branch_search_input(state: &mut AppState, code: KeyCode) {
    match code {
        KeyCode::Esc => {
            state.branch_search.active = false;
            state.branch_search.query.clear();
            state.selected_branch = 0;
        }
        KeyCode::Enter => state.branch_search.active = false,
        KeyCode::Backspace => {
            state.branch_search.query.pop();
            state.selected_branch = 0;
        }
        KeyCode::Char(ch) => {
            state.branch_search.query.push(ch);
            state.selected_branch = 0;
        }
        _ => {}
    }
}

fn handle_search_input(state: &mut AppState, code: KeyCode) {
    match code {
        KeyCode::Esc => {
            state.search.active = false;
            state.search.query.clear();
            state.ensure_selection();
        }
        KeyCode::Enter => {
            state.search.active = false;
        }
        KeyCode::Backspace => {
            state.search.query.pop();
            state.ensure_selection();
        }
        KeyCode::Char(ch) => {
            state.search.query.push(ch);
            state.ensure_selection();
        }
        _ => {}
    }
}

fn handle_overlay_input(state: &mut AppState, code: KeyCode) -> Vec<Command> {
    if overlay_wants_close(&state.overlay, code) {
        state.overlay = Overlay::None;
        return Vec::new();
    }

    // Enter on a variables row opens the full-value window; needs to move the overlay aside, so it's
    // handled here with the whole `&mut AppState` rather than inside `handle_variables_input`.
    if matches!(&state.overlay, Overlay::Variables(v) if !v.searching) && code == KeyCode::Enter {
        open_value_window(state);
        return Vec::new();
    }
    // `o` opens the project's CI/CD settings page (where these variables live); needs the whole
    // state to resolve the project URL, so it's handled here rather than in `handle_variables_input`.
    if matches!(&state.overlay, Overlay::Variables(v) if !v.searching) && code == KeyCode::Char('o')
    {
        return open_variables_in_browser(state);
    }
    if matches!(state.overlay, Overlay::VariableValue(_)) {
        return handle_value_window_input(state, code);
    }

    // Selecting a pipeline writes to `state.viewed_pipeline`, so this handler needs the whole
    // `&mut AppState`, not just the overlay — split it out instead of borrowing `state.overlay`.
    if matches!(state.overlay, Overlay::Pipelines(_)) {
        return handle_pipelines_input(state, code);
    }

    // Submitting the create form clears the overlay form state and emits a command, so it needs the
    // whole `&mut AppState`.
    if matches!(state.overlay, Overlay::Tags(_)) {
        return handle_tags_input(state, code);
    }

    // Selecting a menu item may open the confirm dialog or emit a command, so it needs the whole
    // `&mut AppState` rather than just the overlay.
    if matches!(state.overlay, Overlay::ActionMenu(_)) {
        return handle_action_menu_input(state, code);
    }

    let mut commands = Vec::new();
    let mut confirmed: Option<Action> = None;
    match &mut state.overlay {
        // VariableValue is handled earlier via `handle_value_window_input`.
        Overlay::Help
        | Overlay::None
        | Overlay::Pipelines(_)
        | Overlay::Tags(_)
        | Overlay::VariableValue(_)
        | Overlay::ActionMenu(_) => {}
        Overlay::Confirm(action) => {
            if matches!(code, KeyCode::Char('y') | KeyCode::Enter) {
                confirmed = Some(action.clone());
            }
        }
        Overlay::Variables(view) => commands = handle_variables_input(view, code),
        Overlay::Environments(view) => commands = handle_environments_input(view, code),
    }

    if let Some(action) = confirmed {
        state.overlay = Overlay::None;
        state.pending_actions.insert(action.project_id());
        state.mark_in_flight(action.project_id(), action.item_id());
        state.status_message = format!("{}...", action.describe());
        commands.push(Command::Dispatch(action));
    }
    commands
}

fn overlay_wants_close(overlay: &Overlay, code: KeyCode) -> bool {
    match overlay {
        Overlay::Help => matches!(code, KeyCode::Esc | KeyCode::Char('q' | '?')),
        Overlay::Confirm(_) => matches!(code, KeyCode::Esc | KeyCode::Char('n')),
        // While typing a filter, these keys are text/clear, not close.
        Overlay::Variables(view) => {
            !view.searching && matches!(code, KeyCode::Esc | KeyCode::Char('q' | 'v'))
        }
        // The value window restores the variables overlay on close, so it handles Esc/q itself.
        Overlay::VariableValue(_) | Overlay::None => false,
        Overlay::Pipelines(_) => matches!(code, KeyCode::Esc | KeyCode::Char('q' | 'P')),
        Overlay::Environments(_) => matches!(code, KeyCode::Esc | KeyCode::Char('q' | 'e')),
        // While the create form or detail view is open, Esc/q/typing belong to them, not the overlay.
        Overlay::Tags(view) => {
            view.in_list_mode() && matches!(code, KeyCode::Esc | KeyCode::Char('q' | 'T'))
        }
        Overlay::ActionMenu(_) => matches!(code, KeyCode::Esc | KeyCode::Char('q' | 'x')),
    }
}

/// Navigation in the environments overlay. `Enter`/`o` open the selected environment's URL
/// (external URL when set, else the deploy job's page); `q`/`Esc`/`e` close it.
fn handle_environments_input(view: &mut EnvironmentsViewState, code: KeyCode) -> Vec<Command> {
    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            step(&mut view.selected, view.environments.len(), true);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            step(&mut view.selected, view.environments.len(), false);
        }
        KeyCode::Enter | KeyCode::Char('o') => {
            if let Some(url) = view.selected_url() {
                return vec![Command::OpenUrl { url }];
            }
        }
        _ => {}
    }
    Vec::new()
}

/// Keys for the tags overlay. In list mode: `j`/`k` move, `n` opens the create form; `q`/`Esc`/`T`
/// close (via `overlay_wants_close`). In form mode: chars type into the active field, `Tab` switches
/// name/message, `Esc` cancels the form, `Enter` validates and emits `Command::CreateTag`.
fn handle_tags_input(state: &mut AppState, code: KeyCode) -> Vec<Command> {
    let Overlay::Tags(view) = &mut state.overlay else {
        return Vec::new();
    };

    // Detail view: any close key is handled here (Esc/q swallowed by `overlay_wants_close` only in
    // list mode), so just clear it on Esc/q.
    if view.viewing.is_some() {
        if matches!(code, KeyCode::Esc | KeyCode::Char('q')) {
            view.viewing = None;
        }
        return Vec::new();
    }

    let Some(form) = &mut view.creating else {
        // List mode.
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                step(&mut view.selected, view.tags.len(), true);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                step(&mut view.selected, view.tags.len(), false);
            }
            KeyCode::Char('n') => view.creating = Some(CreateTagForm::default()),
            KeyCode::Enter => {
                view.viewing = view.tags.get(view.selected).cloned();
            }
            _ => {}
        }
        return Vec::new();
    };

    // Form mode.
    match code {
        KeyCode::Esc => view.creating = None,
        KeyCode::Tab => form.toggle_field(),
        KeyCode::Backspace => form.backspace(),
        KeyCode::Char(ch) => form.push(ch),
        KeyCode::Enter => {
            let name = form.name.trim().to_string();
            if name.is_empty() {
                form.error = Some(String::from("Enter a tag name"));
                return Vec::new();
            }
            let Some(ref_) = view.ref_.clone() else {
                form.error = Some(String::from("No branch selected to tag"));
                return Vec::new();
            };
            return vec![Command::CreateTag {
                project_id: view.project_id,
                tag_name: name,
                ref_,
                message: form.message.trim().to_string(),
            }];
        }
        _ => {}
    }
    Vec::new()
}

/// Navigation and selection in the pipeline-history overlay. `Enter` switches the detail panel to
/// the chosen pipeline (loading its jobs); `q`/`Esc`/`P` close it (handled by `overlay_wants_close`).
fn handle_pipelines_input(state: &mut AppState, code: KeyCode) -> Vec<Command> {
    let Overlay::Pipelines(view) = &mut state.overlay else {
        return Vec::new();
    };
    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            step(&mut view.selected, view.pipelines.len(), true);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            step(&mut view.selected, view.pipelines.len(), false);
        }
        KeyCode::Enter => {
            let Some(pipeline) = view.pipelines.get(view.selected).cloned() else {
                return Vec::new();
            };
            let project_id = view.project_id;
            let pipeline_id = pipeline.id;
            state.overlay = Overlay::None;
            state.focus = Focus::Detail;
            state.selected_job = 0;
            state.viewed_pipeline.insert(
                project_id,
                ViewedPipeline {
                    project_id,
                    bridges: Vec::new(),
                    pipeline,
                    jobs: Vec::new(),
                    loading: true,
                    error: None,
                },
            );
            return vec![Command::FetchPipelineJobs {
                project_id,
                pipeline_project_id: project_id,
                pipeline_id,
            }];
        }
        _ => {}
    }
    Vec::new()
}

/// Keys the focused log panel consumes: scroll/follow motions, visual-mode entry (`v`) and yank
/// (`y`), plus `Esc` while a visual selection is active. Everything else falls through to the
/// global panel keys so the user is never locked in. Because this only fires while the log is
/// focused, `v` means "visual select" here and still means "show variables" everywhere else.
fn log_consumes_key(state: &AppState, code: KeyCode) -> bool {
    if state.focus != Focus::Log {
        return false;
    }
    // While the user is typing a search query, the log owns every key.
    if state.log.as_ref().is_some_and(|view| view.search.input) {
        return true;
    }
    match code {
        KeyCode::Char('j' | 'k' | 'g' | 'G' | 'f' | 'v' | 'y' | '/' | 'n' | 'N')
        | KeyCode::Up
        | KeyCode::Down
        | KeyCode::PageUp
        | KeyCode::PageDown => true,
        KeyCode::Esc => state
            .log
            .as_ref()
            .is_some_and(|view| view.select_anchor.is_some() || !view.search.query.is_empty()),
        _ => false,
    }
}

/// Cursor/selection/follow keys for the log panel, applied to the open log if any.
///
/// Navigation is cursor-based: `j`/`k`/`g`/`G`/page keys move a visible cursor line (the renderer
/// highlights it while the log is focused) and scroll just enough to keep it on screen. The cursor
/// is where a visual selection starts, so the user can see exactly which line `v` will anchor.
fn handle_log_input(state: &mut AppState, code: KeyCode) -> Vec<Command> {
    let Some(view) = &mut state.log else {
        return Vec::new();
    };
    view.notice = None;
    // Search-input mode: edit the query incrementally, Enter commits, Esc cancels.
    if view.search.input {
        match code {
            KeyCode::Esc => view.clear_search(),
            KeyCode::Enter => view.search_commit(),
            KeyCode::Backspace => {
                view.search.query.pop();
                view.recompute_search();
            }
            KeyCode::Char(c) => {
                view.search.query.push(c);
                view.recompute_search();
            }
            _ => {}
        }
        return Vec::new();
    }
    match code {
        // Toggle nvim-style line-wise visual selection. `v` only reaches here while the log is
        // focused; elsewhere it still opens the CI/CD variables overlay.
        KeyCode::Char('v') => {
            if view.select_anchor.is_some() {
                view.end_visual();
            } else {
                view.begin_visual();
            }
        }
        // Esc leaves visual mode first; a second Esc clears the search highlight, like nvim.
        KeyCode::Esc if view.select_anchor.is_some() => view.end_visual(),
        KeyCode::Esc => view.clear_search(),
        // Start typing a search query; entering search stops tailing.
        KeyCode::Char('/') => {
            view.follow = false;
            view.search = LogSearchState {
                input: true,
                ..LogSearchState::default()
            };
        }
        KeyCode::Char('n') => view.search_next(),
        KeyCode::Char('N') => view.search_prev(),
        // Follow mode only applies outside a selection; tailing pins the cursor to the last line.
        KeyCode::Char('f') if view.select_anchor.is_none() => return log_toggle_follow(view),
        KeyCode::Char('y') => return log_yank(view),
        _ => log_move_cursor(view, code),
    }
    Vec::new()
}

/// Cursor motion in the log: `j`/`k`, page keys, and `g`/`G` for top/bottom. Moving up or paging up
/// cancels follow mode so the view stops tailing.
fn log_move_cursor(view: &mut LogViewState, code: KeyCode) {
    let last_line = view.lines.len().saturating_sub(1);
    match code {
        KeyCode::Char('j') | KeyCode::Down => view.move_cursor_to(view.cursor + 1),
        KeyCode::Char('k') | KeyCode::Up => {
            view.follow = false;
            view.move_cursor_to(view.cursor.saturating_sub(1));
        }
        KeyCode::PageDown => view.move_cursor_to(view.cursor + LOG_PAGE_STEP),
        KeyCode::PageUp => {
            view.follow = false;
            view.move_cursor_to(view.cursor.saturating_sub(LOG_PAGE_STEP));
        }
        KeyCode::Char('g') => {
            view.follow = false;
            view.move_cursor_to(0);
        }
        KeyCode::Char('G') => view.move_cursor_to(last_line),
        _ => {}
    }
}

/// Toggles log follow (tail) mode. Enabling it tails the last line and re-fetches the trace.
fn log_toggle_follow(view: &mut LogViewState) -> Vec<Command> {
    view.follow = !view.follow;
    if view.follow {
        view.loading = true;
        view.cursor = view.lines.len().saturating_sub(1);
        return vec![Command::FetchTrace {
            project_id: view.project_id,
            job_id: view.job_id,
            generation: view.generation,
        }];
    }
    Vec::new()
}

/// Copies the active visual selection (if any) or the whole log to the clipboard.
fn log_yank(view: &mut LogViewState) -> Vec<Command> {
    if view.lines.is_empty() {
        view.notice = Some(String::from("Nothing to copy"));
        return Vec::new();
    }
    if view.select_anchor.is_some() {
        let value = view.selected_text();
        let count = view.selection_len();
        view.end_visual();
        view.notice = Some(format!("Copied {count} lines"));
        return vec![Command::CopyToClipboard { value }];
    }
    // Copy the unwrapped trace so clipboard lines aren't chopped at the pane width.
    let value = view.raw.join("\n");
    view.notice = Some(format!("Copied {} lines", view.raw.len()));
    vec![Command::CopyToClipboard { value }]
}

fn handle_variables_input(view: &mut VariablesViewState, code: KeyCode) -> Vec<Command> {
    if view.searching {
        handle_variables_filter_input(view, code);
        return Vec::new();
    }

    match code {
        KeyCode::Char('/') => {
            view.searching = true;
            view.notice = None;
        }
        KeyCode::Char('j' | 'k') | KeyCode::Down | KeyCode::Up => {
            let len = view.matching().len();
            let down = matches!(code, KeyCode::Char('j') | KeyCode::Down);
            if step(&mut view.selected, len, down) {
                view.revealed = false;
                view.notice = None;
            }
        }
        KeyCode::Char('s') => view.revealed = !view.revealed,
        KeyCode::Char('y') => {
            if let Some((key, value)) = view
                .selected_variable()
                .map(|variable| (variable.key.clone(), variable.value.clone()))
            {
                view.notice = Some(format!("Copied value of {key}"));
                return vec![Command::CopyToClipboard { value }];
            }
        }
        _ => {}
    }
    Vec::new()
}

/// Opens the selected project's CI/CD settings page (where these variables live) in the browser.
/// Feedback goes to the overlay's own notice line, like the copy action.
fn open_variables_in_browser(state: &mut AppState) -> Vec<Command> {
    let url = state
        .selected_item()
        .and_then(|item| item.project.as_ref())
        .and_then(|project| project.web_url.as_deref())
        .map(|base| format!("{base}/-/settings/ci_cd#js-cicd-variables-settings"));
    let (notice, commands) = match url {
        Some(url) => (
            String::from("Opening in browser…"),
            vec![Command::OpenUrl { url }],
        ),
        None => (String::from("Nothing to open in browser"), Vec::new()),
    };
    if let Overlay::Variables(view) = &mut state.overlay {
        view.notice = Some(notice);
    }
    commands
}

/// Live KEY filter editing. Esc clears the filter and exits; Enter keeps it and
/// returns to navigation. Any edit resets the cursor to the first match.
fn handle_variables_filter_input(view: &mut VariablesViewState, code: KeyCode) {
    match code {
        KeyCode::Esc => {
            view.filter.clear();
            view.searching = false;
            view.selected = 0;
            view.revealed = false;
        }
        KeyCode::Enter => view.searching = false,
        KeyCode::Backspace => {
            view.filter.pop();
            view.selected = 0;
            view.revealed = false;
        }
        KeyCode::Char(ch) => {
            view.filter.push(ch);
            view.selected = 0;
            view.revealed = false;
        }
        _ => {}
    }
}

/// Replaces the variables overlay with the full-value window for the selected row, stashing the
/// variables state so the window can restore it on close. No-op if no row is selected.
fn open_value_window(state: &mut AppState) {
    let Overlay::Variables(view) = &state.overlay else {
        return;
    };
    let Some((key, value)) = view.selected_variable().map(|variable| {
        (
            variable.key.clone(),
            variable.value.chars().collect::<Vec<_>>(),
        )
    }) else {
        return;
    };
    let Overlay::Variables(parent) = std::mem::replace(&mut state.overlay, Overlay::None) else {
        return; // unreachable: matched above
    };
    state.overlay = Overlay::VariableValue(ValueViewState {
        key,
        value,
        scroll: 0,
        anchor: None,
        cursor: 0,
        notice: None,
        parent: Box::new(parent),
    });
}

/// Keys in the full-value window: `y` copies the selection (or the whole value), `j`/`k` scroll,
/// `Esc`/`q` return to the variables overlay.
fn handle_value_window_input(state: &mut AppState, code: KeyCode) -> Vec<Command> {
    let Overlay::VariableValue(view) = &mut state.overlay else {
        return Vec::new();
    };
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            if let Overlay::VariableValue(view) =
                std::mem::replace(&mut state.overlay, Overlay::None)
            {
                state.overlay = Overlay::Variables(*view.parent);
            }
        }
        KeyCode::Char('y') => {
            let value = view.selected_text();
            let copied = view
                .selection_range()
                .map_or(view.value.len(), |(s, e)| e - s + 1);
            view.notice = Some(format!("Copied {copied} chars"));
            return vec![Command::CopyToClipboard { value }];
        }
        KeyCode::Char('j') | KeyCode::Down => view.scroll = view.scroll.saturating_add(1),
        KeyCode::Char('k') | KeyCode::Up => view.scroll = view.scroll.saturating_sub(1),
        _ => {}
    }
    Vec::new()
}

fn handle_panel_input(state: &mut AppState, code: KeyCode) -> Vec<Command> {
    // While the log panel is focused, scroll/follow keys drive the trace; everything else
    // (panel switches, refresh, quit, …) still falls through so the user isn't locked in.
    if log_consumes_key(state, code) {
        return handle_log_input(state, code);
    }

    // Each handler owns a disjoint set of keys and returns `None` to fall through to the next.
    handle_global_keys(state, code)
        .or_else(|| handle_focus_keys(state, code))
        .or_else(|| handle_motion_keys(state, code))
        .or_else(|| handle_action_keys(state, code))
        .unwrap_or_default()
}

/// Keys that work in any panel: quit, refresh, help.
fn handle_global_keys(state: &mut AppState, code: KeyCode) -> Option<Vec<Command>> {
    match code {
        KeyCode::Char('q') => Some(vec![Command::Quit]),
        KeyCode::Char('r') => Some(vec![Command::Refresh]),
        // Jump to whichever project GitLab is actively building (e.g. right after a push).
        KeyCode::Char('g') => Some(vec![Command::ScanRunning]),
        // Switch between GitLab instance tabs. No-op when only one instance is configured (the run
        // loop ignores a switch that doesn't change the active tab).
        KeyCode::Char(']') => Some(vec![Command::SwitchInstance { delta: 1 }]),
        KeyCode::Char('[') => Some(vec![Command::SwitchInstance { delta: -1 }]),
        KeyCode::Char('a') => Some(vec![Command::AddInstance]),
        KeyCode::Char('?') => {
            state.overlay = Overlay::Help;
            Some(Vec::new())
        }
        _ => None,
    }
}

/// Panel focus switching, the per-panel `/` filter, and `Esc` (handled by [`escape_panel`]).
fn handle_focus_keys(state: &mut AppState, code: KeyCode) -> Option<Vec<Command>> {
    match code {
        KeyCode::Char('/') => match state.focus {
            Focus::Projects => state.search.active = true,
            Focus::Branches => state.branch_search.active = true,
            Focus::Detail | Focus::Log | Focus::MergeRequests => {}
        },
        KeyCode::Tab => {
            let next = match state.focus {
                Focus::Projects => Focus::Branches,
                Focus::Branches => Focus::Detail,
                Focus::Detail => Focus::Log,
                Focus::Log => Focus::MergeRequests,
                Focus::MergeRequests => Focus::Projects,
            };
            if next == Focus::MergeRequests {
                return Some(focus_mr_panel(state));
            }
            state.focus = next;
        }
        // h/l move between the left column (projects) and the right log pane.
        KeyCode::Char('1' | 'h') | KeyCode::Left => state.focus = Focus::Projects,
        KeyCode::Char('2') => state.focus = Focus::Branches,
        KeyCode::Char('3') => state.focus = Focus::Detail,
        KeyCode::Char('4' | 'l') | KeyCode::Right => state.focus = Focus::Log,
        KeyCode::Char('5') => return Some(focus_mr_panel(state)),
        // `+` toggles expanding the focused panel to fill the screen; `_` always collapses back.
        KeyCode::Char('+') => {
            state.screen_mode = match state.screen_mode {
                ScreenMode::Normal => ScreenMode::Expanded,
                ScreenMode::Expanded => ScreenMode::Normal,
            }
        }
        KeyCode::Char('_') => state.screen_mode = ScreenMode::Normal,
        KeyCode::Esc => escape_panel(state),
        _ => return None,
    }
    Some(Vec::new())
}

/// `Esc`: drop a pipeline-history view first, then leave the panel, then clear the project filter.
fn escape_panel(state: &mut AppState) {
    // In the pipeline panel, Esc first drops a history view (back to latest) before it moves
    // focus, so the user isn't forced out of the panel to leave history.
    if state.focus == Focus::Detail && state.clear_viewed_pipeline() {
        state.status_message = String::from("Showing latest pipeline");
    } else if state.focus != Focus::Projects {
        state.focus = Focus::Projects;
    } else if !state.search.query.is_empty() {
        state.search.query.clear();
        state.ensure_selection();
    }
}

/// Cursor movement (`j`/`k`) within the focused panel.
fn handle_motion_keys(state: &mut AppState, code: KeyCode) -> Option<Vec<Command>> {
    match code {
        KeyCode::Char('j') | KeyCode::Down => match state.focus {
            Focus::Projects => state.select_next_project(),
            Focus::Branches => state.select_next_branch(),
            Focus::Detail => state.select_next_job(),
            Focus::Log => {}
            Focus::MergeRequests => return Some(mr_move(state, true)),
        },
        KeyCode::Char('k') | KeyCode::Up => match state.focus {
            Focus::Projects => state.select_previous_project(),
            Focus::Branches => state.select_previous_branch(),
            Focus::Detail => state.select_previous_job(),
            Focus::Log => {}
            Focus::MergeRequests => return Some(mr_move(state, false)),
        },
        _ => return None,
    }
    Some(Vec::new())
}

/// Selection actions: `Enter` (focus-dependent), branch pin/unpin, pipeline/job mutations, and
/// opening the log/variables/pipeline-history overlays.
fn handle_action_keys(state: &mut AppState, code: KeyCode) -> Option<Vec<Command>> {
    match code {
        KeyCode::Enter => match state.focus {
            Focus::Projects => {
                state.focus = Focus::Detail;
                Some(Vec::new())
            }
            Focus::Branches => Some(pin_selected_branch(state)),
            // A bridge row drills into its downstream pipeline; a job row opens its log.
            Focus::Detail if state.selected_bridge_item().is_some() => {
                Some(open_downstream_pipeline(state))
            }
            Focus::Detail => Some(open_job_log(state)),
            Focus::Log => Some(Vec::new()),
            // An MR row drills into its head pipeline, reusing the history-view mechanism.
            Focus::MergeRequests => Some(open_mr_pipeline(state)),
        },
        KeyCode::Char('u') if state.focus == Focus::Branches => Some(unpin_selected_project(state)),
        KeyCode::Char('m') if state.focus == Focus::MergeRequests => Some(cycle_mr_scope(state)),
        KeyCode::Char('p') => {
            request_play_job(state);
            Some(Vec::new())
        }
        KeyCode::Char('R') => {
            request_retry(state);
            Some(Vec::new())
        }
        KeyCode::Char('c') => {
            request_cancel_pipeline(state);
            Some(Vec::new())
        }
        KeyCode::Char('N') => {
            request_run_pipeline(state);
            Some(Vec::new())
        }
        KeyCode::Char('L') => Some(open_job_log(state)),
        KeyCode::Char('v') => Some(open_variables(state)),
        KeyCode::Char('P') => Some(open_pipelines(state)),
        KeyCode::Char('e') => Some(open_environments(state)),
        KeyCode::Char('T') => Some(open_tags(state)),
        KeyCode::Char('o') => Some(open_in_browser(state)),
        KeyCode::Char('x') => {
            open_action_menu(state);
            Some(Vec::new())
        }
        _ => None,
    }
}

/// Builds the context menu of actions for the focused job/pipeline (lazygit's `x`). Each entry is
/// either enabled (routes through the existing confirm flow or opens/copies a URL) or disabled with
/// the same "why not" message the direct keybindings show. Built fresh on open, so it never carries
/// a stale action.
fn open_action_menu(state: &mut AppState) {
    let mut items: Vec<ActionMenuItem> = Vec::new();

    append_mr_actions(state, &mut items);
    append_job_actions(state, &mut items);
    append_pipeline_actions(state, &mut items);

    // Open / copy the URL for the current context (job page, else pipeline, else project).
    if let Some(url) = state.browse_url() {
        items.push(ActionMenuItem {
            label: String::from("Open in browser"),
            description: String::from("open this page in the default browser"),
            disabled_reason: None,
            effect: MenuEffect::Open(url.clone()),
        });
        items.push(ActionMenuItem {
            label: String::from("Copy URL"),
            description: String::from("copy this page's URL to the clipboard"),
            disabled_reason: None,
            effect: MenuEffect::Copy(url),
        });
    }

    if items.is_empty() {
        state.status_message = String::from("No actions available here");
        return;
    }
    // Start the cursor on the first enabled item so Enter does something useful.
    let selected = items
        .iter()
        .position(|item| item.disabled_reason.is_none())
        .unwrap_or(0);
    state.overlay = Overlay::ActionMenu(ActionMenuState {
        title: String::from("Actions"),
        items,
        selected,
    });
}

/// Appends actions for the focused merge request.
fn append_mr_actions(state: &AppState, items: &mut Vec<ActionMenuItem>) {
    if state.focus == Focus::MergeRequests {
        if let Some(mr) = state.mr_panel.selected_mr() {
            let in_flight = state.is_in_flight(mr.project_id, mr.iid);
            let busy_reason = || in_flight.then(|| String::from("action in progress"));
            items.push(ActionMenuItem {
                label: String::from("Approve MR"),
                description: format!("approve !{} — may be unavailable on Free/CE", mr.iid),
                disabled_reason: busy_reason(),
                effect: MenuEffect::Confirm(Action::ApproveMr {
                    project_id: mr.project_id,
                    iid: mr.iid,
                }),
            });
            let unmergeable = if mr.draft {
                Some(String::from("MR is a draft"))
            } else if mr.has_conflicts {
                Some(String::from("MR has conflicts"))
            } else if mr.state != "opened" {
                Some(format!("MR is {}", mr.state))
            } else if mr.sha.as_deref().is_none_or(str::is_empty) {
                Some(String::from("source commit SHA not loaded"))
            } else {
                None
            };
            items.push(ActionMenuItem {
                label: String::from("Merge MR"),
                description: format!("merge !{} into {}", mr.iid, mr.target_branch),
                disabled_reason: busy_reason().or(unmergeable),
                effect: MenuEffect::Confirm(Action::MergeMr {
                    project_id: mr.project_id,
                    iid: mr.iid,
                    sha: mr.sha.clone().unwrap_or_default(),
                }),
            });
        }
    }
}

/// Appends actions for the focused job.
fn append_job_actions(state: &AppState, items: &mut Vec<ActionMenuItem>) {
    if state.focus == Focus::Detail {
        if let Some(job) = selected_job_info(state) {
            let in_flight = state.is_in_flight(job.project_id, job.job_id);
            items.push(ActionMenuItem {
                label: String::from("Retry job"),
                description: String::from("re-run this failed/canceled job"),
                disabled_reason: if in_flight {
                    Some(String::from("action in progress"))
                } else if !job.status.is_retryable() {
                    Some(format!("job '{}' is not failed/canceled", job.name))
                } else {
                    None
                },
                effect: MenuEffect::Confirm(Action::RetryJob {
                    project_id: job.project_id,
                    job_id: job.job_id,
                    job_name: job.name.clone(),
                }),
            });
            items.push(ActionMenuItem {
                label: String::from("Play job"),
                description: String::from("start this manual job"),
                disabled_reason: if in_flight {
                    Some(String::from("action in progress"))
                } else if job.status != CiStatus::Manual {
                    Some(format!("job '{}' is not a manual job", job.name))
                } else {
                    None
                },
                effect: MenuEffect::Confirm(Action::PlayJob {
                    project_id: job.project_id,
                    job_id: job.job_id,
                    job_name: job.name.clone(),
                }),
            });
            items.push(ActionMenuItem {
                label: String::from("Download artifacts"),
                description: job.artifacts_file.as_ref().map_or_else(
                    || String::from("save the artifacts archive"),
                    |archive| format!("save {} ({})", archive.filename, format_size(archive.size)),
                ),
                disabled_reason: if in_flight {
                    Some(String::from("action in progress"))
                } else if job.artifacts_file.is_none() {
                    Some(format!("job '{}' has no artifacts", job.name))
                } else {
                    None
                },
                effect: MenuEffect::Download {
                    project_id: job.project_id,
                    job_id: job.job_id,
                    job_name: job.name,
                    filename: job
                        .artifacts_file
                        .map_or_else(|| String::from("artifacts.zip"), |archive| archive.filename),
                },
            });
        }
    }
}

/// Appends actions for the selected project's pipeline.
fn append_pipeline_actions(state: &AppState, items: &mut Vec<ActionMenuItem>) {
    if let Some(pipeline) = selected_pipeline_info(state) {
        if let (Some(pipeline_id), Some(status)) = (pipeline.pipeline_id, pipeline.status.as_ref())
        {
            let in_flight = state.is_in_flight(pipeline.project_id, pipeline_id);
            items.push(ActionMenuItem {
                label: String::from("Retry pipeline"),
                description: String::from("re-run failed/canceled jobs"),
                disabled_reason: if in_flight {
                    Some(String::from("action in progress"))
                } else if !status.is_retryable() {
                    Some(format!("pipeline is {status}, not failed/canceled"))
                } else {
                    None
                },
                effect: MenuEffect::Confirm(Action::RetryPipeline {
                    project_id: pipeline.project_id,
                    pipeline_id,
                }),
            });
            items.push(ActionMenuItem {
                label: String::from("Cancel pipeline"),
                description: String::from("stop the running pipeline"),
                disabled_reason: if in_flight {
                    Some(String::from("action in progress"))
                } else if !status.is_cancelable() {
                    Some(format!("pipeline is {status}, not running"))
                } else {
                    None
                },
                effect: MenuEffect::Confirm(Action::CancelPipeline {
                    project_id: pipeline.project_id,
                    pipeline_id,
                }),
            });
        }

        // Run a new pipeline on the tracked branch (no pre-existing item).
        items.push(match pipeline.ref_ {
            Some(ref_) => ActionMenuItem {
                label: String::from("Run new pipeline"),
                description: format!("trigger a pipeline on '{ref_}'"),
                disabled_reason: None,
                effect: MenuEffect::Confirm(Action::RunPipeline {
                    project_id: pipeline.project_id,
                    ref_,
                }),
            },
            None => ActionMenuItem {
                label: String::from("Run new pipeline"),
                description: String::from("trigger a pipeline on the tracked branch"),
                disabled_reason: Some(String::from("no branch to run on")),
                effect: MenuEffect::Confirm(Action::RunPipeline {
                    project_id: pipeline.project_id,
                    ref_: String::new(),
                }),
            },
        });
    }
}

/// Navigation and selection in the action menu. `j/k` move the cursor; `Enter` resolves the
/// selected effect (confirm dialog, open, or copy); disabled rows are skipped on Enter.
fn handle_action_menu_input(state: &mut AppState, code: KeyCode) -> Vec<Command> {
    let Overlay::ActionMenu(menu) = &mut state.overlay else {
        return Vec::new();
    };
    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            step(&mut menu.selected, menu.items.len(), true);
            Vec::new()
        }
        KeyCode::Char('k') | KeyCode::Up => {
            step(&mut menu.selected, menu.items.len(), false);
            Vec::new()
        }
        KeyCode::Enter => {
            let Some(item) = menu.items.get(menu.selected) else {
                return Vec::new();
            };
            if let Some(reason) = &item.disabled_reason {
                state.status_message = format!("{}: {reason}", item.label);
                return Vec::new();
            }
            // Take ownership of the effect, then close the menu.
            let Overlay::ActionMenu(menu) = std::mem::replace(&mut state.overlay, Overlay::None)
            else {
                return Vec::new();
            };
            let item = menu.items.into_iter().nth(menu.selected);
            match item.map(|item| item.effect) {
                Some(MenuEffect::Confirm(action)) => {
                    // Re-enter the existing confirm path; the single pending/in-flight insertion
                    // point stays in `handle_overlay_input`.
                    state.overlay = Overlay::Confirm(action);
                    Vec::new()
                }
                Some(MenuEffect::Open(url)) => {
                    state.status_message = String::from("Opening in browser…");
                    vec![Command::OpenUrl { url }]
                }
                Some(MenuEffect::Copy(value)) => {
                    state.status_message = String::from("Copied URL to clipboard");
                    vec![Command::CopyToClipboard { value }]
                }
                Some(MenuEffect::Download {
                    project_id,
                    job_id,
                    job_name,
                    filename,
                }) => {
                    // The in-flight spinner on the job row doubles as download progress.
                    state.mark_in_flight(project_id, job_id);
                    state.status_message = format!("Downloading artifacts of '{job_name}'…");
                    vec![Command::DownloadArtifacts {
                        project_id,
                        job_id,
                        job_name,
                        filename,
                    }]
                }
                None => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

/// Human-readable size for the artifacts archive, e.g. "3.4 MB".
#[expect(
    clippy::cast_precision_loss,
    reason = "Byte sizes are only displayed to one decimal place in KB or MB"
)]
fn format_size(bytes: u64) -> String {
    match bytes {
        0..=1023 => format!("{bytes} B"),
        1024..=1_048_575 => format!("{:.1} KB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

/// Opens the current context (viewed job, else pipeline, else project) in the default browser.
fn open_in_browser(state: &mut AppState) -> Vec<Command> {
    match state.browse_url() {
        Some(url) => {
            state.status_message = String::from("Opening in browser…");
            vec![Command::OpenUrl { url }]
        }
        None => {
            state.status_message = String::from("Nothing to open in browser");
            Vec::new()
        }
    }
}

/// Pins the selected project to the branch under the cursor (or clears the override when the repo
/// default is chosen), then refreshes that project so its pipeline reflects the new ref.
fn pin_selected_branch(state: &mut AppState) -> Vec<Command> {
    match state.pin_selected_branch() {
        Some(project_id) => vec![Command::RefreshProject { project_id }],
        None => Vec::new(),
    }
}

fn unpin_selected_project(state: &mut AppState) -> Vec<Command> {
    match state.unpin_selected_project() {
        Some(project_id) => vec![Command::RefreshProject { project_id }],
        None => Vec::new(),
    }
}

/// Owned facts about the selected job, so callers can mutate state afterwards.
struct SelectedJob {
    project_id: u64,
    job_id: u64,
    name: String,
    status: CiStatus,
    artifacts_file: Option<gitlab::ArtifactsFile>,
}

fn selected_job_info(state: &AppState) -> Option<SelectedJob> {
    let project_id = state.active_pipeline_project_id()?;
    let job = state.selected_job_item()?;
    Some(SelectedJob {
        project_id,
        job_id: job.id,
        name: job.name.clone(),
        status: job.status.clone(),
        artifacts_file: job.artifacts_file.clone(),
    })
}

/// Owned facts about the selected project's pipeline.
struct SelectedPipeline {
    project_id: u64,
    pipeline_id: Option<u64>,
    status: Option<CiStatus>,
    ref_: Option<String>,
}

fn selected_pipeline_info(state: &AppState) -> Option<SelectedPipeline> {
    let item = state.selected_item()?;
    let (project_id, pipeline) = if state.focus == Focus::Detail {
        (state.active_pipeline_project_id()?, state.active_pipeline())
    } else {
        (item.configured.id, item.pipeline.as_ref())
    };
    Some(SelectedPipeline {
        project_id,
        pipeline_id: pipeline.map(|pipeline| pipeline.id),
        status: pipeline.map(|pipeline| pipeline.status.clone()),
        ref_: if state.focus == Focus::Detail
            && state.viewed_pipeline.contains_key(&item.configured.id)
        {
            pipeline.and_then(|pipeline| pipeline.ref_.clone())
        } else {
            state.effective_ref(item.configured.id)
        },
    })
}

fn request_play_job(state: &mut AppState) {
    let Some(job) = selected_job_info(state) else {
        state.status_message = String::from("Play: select a job first");
        return;
    };
    if job.status != CiStatus::Manual {
        state.status_message = format!("Play: job '{}' is not a manual job", job.name);
        return;
    }
    state.overlay = Overlay::Confirm(Action::PlayJob {
        project_id: job.project_id,
        job_id: job.job_id,
        job_name: job.name,
    });
}

fn request_retry(state: &mut AppState) {
    // In the detail panel `R` retries the selected job; in the projects panel, the pipeline.
    if state.focus == Focus::Detail {
        let Some(job) = selected_job_info(state) else {
            state.status_message = String::from("Retry: select a job first");
            return;
        };
        if !job.status.is_retryable() {
            state.status_message = format!("Retry: job '{}' is not failed/canceled", job.name);
            return;
        }
        state.overlay = Overlay::Confirm(Action::RetryJob {
            project_id: job.project_id,
            job_id: job.job_id,
            job_name: job.name,
        });
        return;
    }

    let Some(pipeline) = selected_pipeline_info(state) else {
        return;
    };
    match (pipeline.pipeline_id, pipeline.status.as_ref()) {
        (Some(pipeline_id), Some(status)) if status.is_retryable() => {
            state.overlay = Overlay::Confirm(Action::RetryPipeline {
                project_id: pipeline.project_id,
                pipeline_id,
            });
        }
        (Some(_), Some(status)) => {
            state.status_message = format!("Retry: pipeline is {status}, not failed/canceled");
        }
        _ => state.status_message = String::from("Retry: no pipeline"),
    }
}

fn request_cancel_pipeline(state: &mut AppState) {
    let Some(pipeline) = selected_pipeline_info(state) else {
        return;
    };
    match (pipeline.pipeline_id, pipeline.status.as_ref()) {
        (Some(pipeline_id), Some(status)) if status.is_cancelable() => {
            state.overlay = Overlay::Confirm(Action::CancelPipeline {
                project_id: pipeline.project_id,
                pipeline_id,
            });
        }
        (Some(_), Some(status)) => {
            state.status_message = format!("Cancel: pipeline is {status}, not running");
        }
        _ => state.status_message = String::from("Cancel: no pipeline"),
    }
}

fn request_run_pipeline(state: &mut AppState) {
    let Some(pipeline) = selected_pipeline_info(state) else {
        state.status_message = String::from("Run: select a project first");
        return;
    };
    let Some(ref_) = pipeline.ref_ else {
        state.status_message = String::from("Run: no branch to run on");
        return;
    };
    state.overlay = Overlay::Confirm(Action::RunPipeline {
        project_id: pipeline.project_id,
        ref_,
    });
}

/// Inspects the downstream pipeline of the selected bridge row in the detail panel, reusing the
/// history-view mechanism. The downstream's own bridges load too, so drill-down nests naturally.
fn open_downstream_pipeline(state: &mut AppState) -> Vec<Command> {
    let Some(project_id) = state.selected_item().map(|item| item.configured.id) else {
        return Vec::new();
    };
    let Some(bridge) = state.selected_bridge_item() else {
        return Vec::new();
    };
    let Some(downstream) = bridge.downstream_pipeline.clone() else {
        state.status_message = String::from("Downstream pipeline not created yet");
        return Vec::new();
    };
    state.selected_job = 0;
    state.viewed_pipeline.insert(
        project_id,
        ViewedPipeline {
            project_id: downstream.project_id,
            pipeline: Pipeline {
                id: downstream.id,
                iid: None,
                status: downstream.status,
                ref_: None,
                web_url: downstream.web_url,
                duration: None,
                created_at: None,
                updated_at: None,
                sha: None,
            },
            jobs: Vec::new(),
            bridges: Vec::new(),
            loading: true,
            error: None,
        },
    );
    vec![Command::FetchPipelineJobs {
        project_id,
        pipeline_project_id: downstream.project_id,
        pipeline_id: downstream.id,
    }]
}

fn open_job_log(state: &mut AppState) -> Vec<Command> {
    let Some(job) = selected_job_info(state) else {
        state.status_message = String::from("Logs: select a job first");
        return Vec::new();
    };
    state.log_generation += 1;
    let generation = state.log_generation;
    // Follow any in-flight job (created/pending/preparing/running/…), not just running, so a log
    // opened before the job starts producing output still streams once it does.
    let follow = job.status.is_active();
    // Load into the persistent log pane and focus it; it stays visible after the user
    // navigates away, until another job log replaces it.
    state.log = Some(LogViewState {
        project_id: job.project_id,
        job_id: job.job_id,
        job_name: job.name,
        job_status: job.status.to_string(),
        lines: Vec::new(),
        raw: Vec::new(),
        wrap_width: 0,
        scroll: 0,
        follow,
        loading: true,
        generation,
        error: None,
        notice: None,
        cursor: 0,
        select_anchor: None,
        viewport_height: 0,
        search: LogSearchState::default(),
    });
    state.focus = Focus::Log;
    vec![Command::FetchTrace {
        project_id: job.project_id,
        job_id: job.job_id,
        generation,
    }]
}

fn open_variables(state: &mut AppState) -> Vec<Command> {
    let Some(item) = state.selected_item() else {
        return Vec::new();
    };
    let project_id = item.configured.id;
    let project_name = item.configured.name.clone();
    // Stale-while-revalidate: show the cached list instantly (no loading flash) and
    // always refresh in the background so values can't stay stale for long.
    let cached = state.variables_cache.get(&project_id).cloned();
    state.overlay = Overlay::Variables(VariablesViewState {
        project_id,
        project_name,
        loading: cached.is_none(),
        variables: cached.unwrap_or_default(),
        selected: 0,
        revealed: false,
        error: None,
        notice: None,
        filter: String::new(),
        searching: false,
    });
    vec![Command::FetchVariables { project_id }]
}

/// Opens the pipeline-history overlay for the selected project, scoped to its tracked ref.
/// Focuses the `[5] MRs` panel and kicks off a list fetch when it's empty, failed, or loaded for
/// a different project.
pub(crate) fn focus_mr_panel(state: &mut AppState) -> Vec<Command> {
    state.focus = Focus::MergeRequests;
    ensure_mr_list(state)
}

pub(crate) fn focus_mr_refresh(state: &mut AppState) -> Vec<Command> {
    if state.focus == Focus::MergeRequests {
        ensure_mr_list(state)
    } else {
        Vec::new()
    }
}

/// Requests the MR list when the panel has nothing usable for the current scope/project.
fn ensure_mr_list(state: &mut AppState) -> Vec<Command> {
    let project_id = state.selected_item().map(|item| item.configured.id);
    let panel = &mut state.mr_panel;
    let stale_project = panel.scope == MrScope::Project && panel.loaded_for_project != project_id;
    if panel.loading && !stale_project {
        return Vec::new();
    }
    if !panel.mrs.is_empty() && !stale_project && panel.error.is_none() {
        return Vec::new();
    }
    if panel.scope == MrScope::Project && project_id.is_none() {
        return Vec::new();
    }
    let scope = panel.scope;
    panel.reset();
    panel.scope = scope;
    panel.loading = true;
    panel.loaded_for_project = if scope == MrScope::Project {
        project_id
    } else {
        None
    };
    vec![Command::FetchMergeRequests { scope, project_id }]
}

/// Cycles the MR panel scope (project → created by me → assigned to me) and refetches.
fn cycle_mr_scope(state: &mut AppState) -> Vec<Command> {
    let next = state.mr_panel.scope.next();
    state.mr_panel.reset();
    state.mr_panel.scope = next;
    ensure_mr_list(state)
}

/// Moves the MR cursor and lazily requests the newly selected row's head pipeline.
fn mr_move(state: &mut AppState, down: bool) -> Vec<Command> {
    let panel = &mut state.mr_panel;
    step(&mut panel.selected, panel.mrs.len(), down);
    request_selected_head_pipeline(state)
}

/// Requests the selected MR's head pipeline unless it's already fetched or in flight.
pub(crate) fn request_selected_head_pipeline(state: &mut AppState) -> Vec<Command> {
    let Some(key) = state
        .mr_panel
        .selected_mr()
        .map(|mr| (mr.project_id, mr.iid))
    else {
        return Vec::new();
    };
    if state.mr_panel.head_pipelines.contains_key(&key) {
        return Vec::new();
    }
    state
        .mr_panel
        .head_pipelines
        .insert(key, HeadPipelineSlot::Loading);
    vec![Command::FetchMrHeadPipeline {
        project_id: key.0,
        iid: key.1,
    }]
}

/// Drills into the selected MR's head pipeline via the history-view mechanism; works for MRs in
/// other projects (instance-wide scopes) the same way multi-project bridges do.
fn open_mr_pipeline(state: &mut AppState) -> Vec<Command> {
    let Some(dashboard_project) = state.selected_item().map(|item| item.configured.id) else {
        return Vec::new();
    };
    let Some(mr_project) = state.mr_panel.selected_mr().map(|mr| mr.project_id) else {
        return Vec::new();
    };
    let Some(pipeline) = state.mr_panel.selected_head_pipeline().cloned() else {
        state.status_message = String::from("MR head pipeline not loaded (or the MR has none)");
        return Vec::new();
    };
    state.selected_job = 0;
    state.focus = Focus::Detail;
    state.viewed_pipeline.insert(
        dashboard_project,
        ViewedPipeline {
            project_id: mr_project,
            pipeline: pipeline.clone(),
            jobs: Vec::new(),
            bridges: Vec::new(),
            loading: true,
            error: None,
        },
    );
    vec![Command::FetchPipelineJobs {
        project_id: dashboard_project,
        pipeline_project_id: mr_project,
        pipeline_id: pipeline.id,
    }]
}

/// Opens the environments overlay for the selected project and starts the lazy fetch.
fn open_environments(state: &mut AppState) -> Vec<Command> {
    let Some(item) = state.selected_item() else {
        return Vec::new();
    };
    let project_id = item.configured.id;
    state.overlay = Overlay::Environments(EnvironmentsViewState {
        project_id,
        project_name: item.configured.name.clone(),
        loading: true,
        environments: Vec::new(),
        selected: 0,
        error: None,
    });
    vec![Command::FetchEnvironments { project_id }]
}

fn open_pipelines(state: &mut AppState) -> Vec<Command> {
    let Some(item) = state.selected_item() else {
        return Vec::new();
    };
    let project_id = item.configured.id;
    let project_name = item.configured.name.clone();
    let ref_ = state.effective_ref(project_id);
    state.overlay = Overlay::Pipelines(PipelinesViewState {
        project_id,
        project_name,
        ref_: ref_.clone(),
        loading: true,
        pipelines: Vec::new(),
        selected: 0,
        error: None,
    });
    vec![Command::FetchPipelines { project_id, ref_ }]
}

/// Opens the tags overlay for the selected project. A new tag points at the highlighted branch,
/// falling back to the tracked ref when no branch row is selected yet.
fn open_tags(state: &mut AppState) -> Vec<Command> {
    let Some(item) = state.selected_item() else {
        return Vec::new();
    };
    let project_id = item.configured.id;
    let project_name = item.configured.name.clone();
    let ref_ = state
        .selected_branch_item()
        .map(|branch| branch.name.clone())
        .or_else(|| state.effective_ref(project_id));
    state.overlay = Overlay::Tags(TagsViewState {
        project_id,
        project_name,
        ref_,
        loading: true,
        tags: Vec::new(),
        selected: 0,
        error: None,
        creating: None,
        viewing: None,
    });
    vec![Command::FetchTags { project_id }]
}

#[cfg(test)]
mod tests {
    use config::ProjectEntry;
    use crossterm::event::{KeyCode, KeyEvent};
    use gitlab::{Job, Pipeline};

    use super::{apply_key, Command};
    use crate::actions::Action;
    use crate::cache::SnapshotOrigin;
    use crate::refresh::RefreshSources;
    use crate::state::{AppState, Focus, Overlay, ProjectSnapshot, ProjectStatus};

    #[test]
    fn step_moves_only_within_list_bounds() {
        for (start, len, down, expected) in [
            (0, 0, true, 0),
            (0, 0, false, 0),
            (0, 1, true, 0),
            (0, 1, false, 0),
            (0, 2, true, 1),
            (1, 2, false, 0),
            (1, 2, true, 1),
        ] {
            let mut selected = start;
            assert_eq!(super::step(&mut selected, len, down), start != expected);
            assert_eq!(selected, expected);
        }
    }

    fn state_with_projects(names: &[(&str, u64)]) -> AppState {
        AppState::new(RefreshSources {
            projects: names
                .iter()
                .map(|(name, id)| ProjectEntry {
                    name: (*name).into(),
                    id: *id,
                })
                .collect(),
            groups: Vec::new(),
        })
    }

    fn pipeline(status: &str) -> Pipeline {
        Pipeline {
            id: 42,
            iid: Some(9),
            status: status.into(),
            ref_: Some("main".into()),
            web_url: None,
            duration: None,
            created_at: None,
            updated_at: None,
            sha: None,
        }
    }

    fn job(id: u64, name: &str, status: &str) -> Job {
        Job {
            artifacts_file: None,
            id,
            name: name.into(),
            status: status.into(),
            stage: None,
            duration: None,
            web_url: None,
        }
    }

    fn bridge(
        id: u64,
        name: &str,
        downstream: Option<gitlab::DownstreamPipeline>,
    ) -> gitlab::Bridge {
        gitlab::Bridge {
            id,
            name: name.into(),
            status: "success".into(),
            stage: None,
            downstream_pipeline: downstream,
        }
    }

    #[test]
    fn enter_on_bridge_row_drills_into_downstream_pipeline() {
        let mut state = state_with_pipeline("success", vec![job(10, "build", "success")]);
        state.items[0].bridges = vec![bridge(
            20,
            "trigger-child",
            Some(gitlab::DownstreamPipeline {
                id: 500,
                project_id: 8,
                status: "failed".into(),
                web_url: None,
            }),
        )];
        state.focus = Focus::Detail;
        state.selected_job = 1; // past the single job -> the bridge row

        let commands = press(&mut state, KeyCode::Enter);

        assert_eq!(
            commands,
            vec![Command::FetchPipelineJobs {
                project_id: 1,
                pipeline_project_id: 8,
                pipeline_id: 500,
            }]
        );
        let view = state.viewed_pipeline.get(&1).expect("viewed downstream");
        assert_eq!(view.project_id, 8);
        assert_eq!(view.pipeline.id, 500);
        assert!(view.loading);
    }

    #[test]
    fn enter_on_bridge_without_downstream_is_a_noop_with_hint() {
        let mut state = state_with_pipeline("success", vec![job(10, "build", "success")]);
        state.items[0].bridges = vec![bridge(20, "trigger-child", None)];
        state.focus = Focus::Detail;
        state.selected_job = 1;

        let commands = press(&mut state, KeyCode::Enter);

        assert!(commands.is_empty());
        assert!(state.viewed_pipeline.is_empty());
        assert!(state.status_message.contains("not created"));
    }

    /// One project with the given pipeline status and jobs, refreshed into the state.
    fn state_with_pipeline(pipeline_status: &str, jobs: Vec<Job>) -> AppState {
        let mut state = state_with_projects(&[("group/api", 1)]);
        state.apply_live_refresh(
            vec![ProjectSnapshot {
                bridges: Vec::new(),
                configured: ProjectEntry {
                    name: String::from("group/api"),
                    id: 1,
                },
                project: None,
                pipeline: Some(pipeline(pipeline_status)),
                jobs,
                status: ProjectStatus::Ready,
                origin: SnapshotOrigin::Static,
            }],
            0,
            false,
            &[],
        );
        state
    }

    fn press(state: &mut AppState, code: KeyCode) -> Vec<Command> {
        apply_key(state, &KeyEvent::from(code))
    }

    #[test]
    fn q_quits_and_r_refreshes() {
        let mut state = state_with_projects(&[("group/api", 1)]);
        assert_eq!(press(&mut state, KeyCode::Char('q')), vec![Command::Quit]);
        assert_eq!(
            press(&mut state, KeyCode::Char('r')),
            vec![Command::Refresh]
        );
    }

    #[test]
    fn tab_cycles_four_panels_and_hl_switch_columns() {
        let mut state = state_with_projects(&[("group/api", 1)]);
        assert_eq!(state.focus, Focus::Projects);

        press(&mut state, KeyCode::Tab);
        assert_eq!(state.focus, Focus::Branches);
        press(&mut state, KeyCode::Tab);
        assert_eq!(state.focus, Focus::Detail);
        press(&mut state, KeyCode::Tab);
        assert_eq!(state.focus, Focus::Log);
        press(&mut state, KeyCode::Tab);
        assert_eq!(state.focus, Focus::MergeRequests);
        press(&mut state, KeyCode::Tab);
        assert_eq!(state.focus, Focus::Projects);

        press(&mut state, KeyCode::Char('l'));
        assert_eq!(state.focus, Focus::Log);
        press(&mut state, KeyCode::Char('h'));
        assert_eq!(state.focus, Focus::Projects);
    }

    #[test]
    fn number_keys_switch_focus_to_matching_panel() {
        let mut state = state_with_projects(&[("group/api", 1)]);
        assert_eq!(state.focus, Focus::Projects);

        press(&mut state, KeyCode::Char('2'));
        assert_eq!(state.focus, Focus::Branches);
        press(&mut state, KeyCode::Char('3'));
        assert_eq!(state.focus, Focus::Detail);
        press(&mut state, KeyCode::Char('4'));
        assert_eq!(state.focus, Focus::Log);
        press(&mut state, KeyCode::Char('1'));
        assert_eq!(state.focus, Focus::Projects);
    }

    #[test]
    fn jk_move_project_cursor_and_keep_selection_by_id() {
        let mut state = state_with_projects(&[("a", 1), ("b", 2), ("c", 3)]);
        assert_eq!(state.selected_project_id, Some(1));

        press(&mut state, KeyCode::Char('j'));
        assert_eq!(state.selected_project_id, Some(2));
        press(&mut state, KeyCode::Char('j'));
        press(&mut state, KeyCode::Char('j'));
        assert_eq!(state.selected_project_id, Some(3), "clamps at end");
        press(&mut state, KeyCode::Char('k'));
        assert_eq!(state.selected_project_id, Some(2));
    }

    #[test]
    fn search_filters_projects_and_preserves_matching_selection() {
        let mut state = state_with_projects(&[("alpha", 1), ("beta", 2)]);
        press(&mut state, KeyCode::Char('j'));
        assert_eq!(state.selected_project_id, Some(2));

        press(&mut state, KeyCode::Char('/'));
        assert!(state.search.active);
        press(&mut state, KeyCode::Char('b'));
        press(&mut state, KeyCode::Char('e'));
        assert_eq!(state.visible_indices().len(), 1);
        assert_eq!(
            state.selected_project_id,
            Some(2),
            "selection still matches"
        );

        press(&mut state, KeyCode::Enter);
        assert!(!state.search.active);
        assert_eq!(state.search.query, "be");
    }

    #[test]
    fn search_moves_selection_to_first_visible_when_filtered_out() {
        let mut state = state_with_projects(&[("alpha", 1), ("beta", 2)]);
        press(&mut state, KeyCode::Char('/'));
        press(&mut state, KeyCode::Char('b'));
        assert_eq!(state.selected_project_id, Some(2));

        press(&mut state, KeyCode::Esc);
        assert!(!state.search.active);
        assert!(state.search.query.is_empty());
        assert_eq!(state.visible_indices().len(), 2);
    }

    #[test]
    fn question_mark_toggles_help_overlay() {
        let mut state = state_with_projects(&[("group/api", 1)]);
        press(&mut state, KeyCode::Char('?'));
        assert_eq!(state.overlay, Overlay::Help);

        let commands = press(&mut state, KeyCode::Char('q'));
        assert!(commands.is_empty(), "q closes overlay instead of quitting");
        assert_eq!(state.overlay, Overlay::None);
    }

    #[test]
    fn retry_in_projects_panel_confirms_pipeline_retry_only_when_retryable() {
        let mut state = state_with_pipeline("failed", vec![]);
        press(&mut state, KeyCode::Char('R'));
        assert_eq!(
            state.overlay,
            Overlay::Confirm(Action::RetryPipeline {
                project_id: 1,
                pipeline_id: 42,
            })
        );

        let mut state = state_with_pipeline("success", vec![]);
        press(&mut state, KeyCode::Char('R'));
        assert_eq!(state.overlay, Overlay::None);
        assert!(state.status_message.contains("not failed/canceled"));
    }

    #[test]
    fn retry_in_detail_panel_targets_selected_job() {
        let mut state = state_with_pipeline(
            "failed",
            vec![job(10, "build", "success"), job(11, "test", "failed")],
        );
        press(&mut state, KeyCode::Char('3'));
        press(&mut state, KeyCode::Char('j'));
        press(&mut state, KeyCode::Char('R'));

        assert_eq!(
            state.overlay,
            Overlay::Confirm(Action::RetryJob {
                project_id: 1,
                job_id: 11,
                job_name: String::from("test"),
            })
        );
    }

    #[test]
    fn play_requires_manual_job() {
        let mut state = state_with_pipeline("success", vec![job(10, "deploy", "manual")]);
        press(&mut state, KeyCode::Char('3'));
        press(&mut state, KeyCode::Char('p'));
        assert_eq!(
            state.overlay,
            Overlay::Confirm(Action::PlayJob {
                project_id: 1,
                job_id: 10,
                job_name: String::from("deploy"),
            })
        );

        let mut state = state_with_pipeline("success", vec![job(10, "build", "success")]);
        press(&mut state, KeyCode::Char('3'));
        press(&mut state, KeyCode::Char('p'));
        assert_eq!(state.overlay, Overlay::None);
        assert!(state.status_message.contains("not a manual job"));
    }

    #[test]
    fn cancel_requires_running_pipeline() {
        let mut state = state_with_pipeline("running", vec![]);
        press(&mut state, KeyCode::Char('c'));
        assert_eq!(
            state.overlay,
            Overlay::Confirm(Action::CancelPipeline {
                project_id: 1,
                pipeline_id: 42,
            })
        );

        let mut state = state_with_pipeline("failed", vec![]);
        press(&mut state, KeyCode::Char('c'));
        assert_eq!(state.overlay, Overlay::None);
        assert!(state.status_message.contains("not running"));
    }

    #[test]
    fn tags_form_validates_name_then_emits_create_command() {
        use crate::state::{Overlay, TagsViewState};

        let mut state = state_with_pipeline("success", vec![]);
        state.overlay = Overlay::Tags(TagsViewState {
            project_id: 1,
            project_name: String::from("group/api"),
            ref_: Some(String::from("main")),
            loading: false,
            tags: Vec::new(),
            selected: 0,
            error: None,
            creating: None,
            viewing: None,
        });

        // `n` opens the inline create form.
        press(&mut state, KeyCode::Char('n'));
        let Overlay::Tags(view) = &state.overlay else {
            panic!("expected tags overlay");
        };
        assert!(view.creating.is_some());

        // Enter with an empty name is rejected and keeps the form open.
        assert!(press(&mut state, KeyCode::Enter).is_empty());
        let Overlay::Tags(view) = &state.overlay else {
            panic!("expected tags overlay");
        };
        assert!(view.creating.as_ref().unwrap().error.is_some());

        // Type a name, then Enter emits the create command on the tracked ref.
        press(&mut state, KeyCode::Char('v'));
        press(&mut state, KeyCode::Char('1'));
        assert_eq!(
            press(&mut state, KeyCode::Enter),
            vec![Command::CreateTag {
                project_id: 1,
                tag_name: String::from("v1"),
                ref_: String::from("main"),
                message: String::new(),
            }]
        );
    }

    #[test]
    fn tags_enter_opens_and_closes_the_detail_view() {
        use crate::state::{Overlay, TagsViewState};
        use gitlab::Tag;

        let mut state = state_with_pipeline("success", vec![]);
        state.overlay = Overlay::Tags(TagsViewState {
            project_id: 1,
            project_name: String::from("group/api"),
            ref_: Some(String::from("main")),
            loading: false,
            tags: vec![Tag {
                name: String::from("v1.0.0"),
                message: Some(String::from("release")),
                target: Some(String::from("abc123")),
            }],
            selected: 0,
            error: None,
            creating: None,
            viewing: None,
        });

        // Enter on a tag opens its detail view; the overlay stays open.
        press(&mut state, KeyCode::Enter);
        let Overlay::Tags(view) = &state.overlay else {
            panic!("expected tags overlay");
        };
        assert_eq!(
            view.viewing.as_ref().map(|t| t.name.as_str()),
            Some("v1.0.0")
        );

        // q closes the detail view but keeps the overlay (not back to the dashboard).
        press(&mut state, KeyCode::Char('q'));
        let Overlay::Tags(view) = &state.overlay else {
            panic!("detail close should keep the tags overlay open");
        };
        assert!(view.viewing.is_none());
    }

    #[test]
    fn action_menu_enables_retry_pipeline_for_failed_and_disables_for_passing() {
        let mut state = state_with_pipeline("failed", vec![]);
        press(&mut state, KeyCode::Char('x'));
        let Overlay::ActionMenu(menu) = &state.overlay else {
            panic!("expected action menu, got {:?}", state.overlay);
        };
        let retry = menu
            .items
            .iter()
            .find(|i| i.label == "Retry pipeline")
            .expect("retry pipeline entry");
        assert!(retry.disabled_reason.is_none());

        let mut state = state_with_pipeline("success", vec![]);
        press(&mut state, KeyCode::Char('x'));
        let Overlay::ActionMenu(menu) = &state.overlay else {
            panic!("expected action menu");
        };
        let retry = menu
            .items
            .iter()
            .find(|i| i.label == "Retry pipeline")
            .expect("retry pipeline entry");
        assert!(retry.disabled_reason.is_some());
    }

    #[test]
    fn action_menu_download_artifacts_follows_artifacts_presence() {
        // A focused job with artifacts -> enabled entry naming the archive.
        let mut with = job(10, "build", "success");
        with.artifacts_file = Some(gitlab::ArtifactsFile {
            filename: String::from("artifacts.zip"),
            size: 2 * 1024 * 1024,
        });
        let mut state = state_with_pipeline("success", vec![with]);
        state.focus = Focus::Detail;
        press(&mut state, KeyCode::Char('x'));
        let Overlay::ActionMenu(menu) = &state.overlay else {
            panic!("expected action menu");
        };
        let download = menu
            .items
            .iter()
            .find(|i| i.label == "Download artifacts")
            .expect("download entry");
        assert!(download.disabled_reason.is_none());
        assert!(
            download.description.contains("2.0 MB"),
            "{}",
            download.description
        );

        // Without artifacts the entry is greyed out with a reason.
        let mut state = state_with_pipeline("success", vec![job(10, "build", "success")]);
        state.focus = Focus::Detail;
        press(&mut state, KeyCode::Char('x'));
        let Overlay::ActionMenu(menu) = &state.overlay else {
            panic!("expected action menu");
        };
        let download = menu
            .items
            .iter()
            .find(|i| i.label == "Download artifacts")
            .expect("download entry");
        assert!(download.disabled_reason.is_some());
    }

    #[test]
    fn e_opens_environments_overlay_and_requests_fetch() {
        let mut state = state_with_pipeline("success", vec![]);
        let commands = press(&mut state, KeyCode::Char('e'));

        assert_eq!(commands, vec![Command::FetchEnvironments { project_id: 1 }]);
        let Overlay::Environments(view) = &state.overlay else {
            panic!("expected environments overlay, got {:?}", state.overlay);
        };
        assert!(view.loading);
        assert_eq!(view.project_id, 1);

        // `e` again closes it.
        press(&mut state, KeyCode::Char('e'));
        assert_eq!(state.overlay, Overlay::None);
    }

    #[test]
    fn enter_in_environments_overlay_opens_external_url() {
        let mut state = state_with_pipeline("success", vec![]);
        press(&mut state, KeyCode::Char('e'));
        let Overlay::Environments(view) = &mut state.overlay else {
            panic!("expected environments overlay");
        };
        view.loading = false;
        view.environments = vec![gitlab::Environment {
            id: 1,
            name: String::from("production"),
            state: Some(String::from("available")),
            external_url: Some(String::from("https://prod.example")),
            last_deployment: None,
        }];

        let commands = press(&mut state, KeyCode::Enter);
        assert_eq!(
            commands,
            vec![Command::OpenUrl {
                url: String::from("https://prod.example")
            }]
        );
    }

    fn mr(iid: u64, title: &str) -> gitlab::MergeRequest {
        gitlab::MergeRequest {
            id: iid + 100,
            iid,
            project_id: 1,
            title: title.into(),
            author: None,
            source_branch: String::from("feat"),
            target_branch: String::from("main"),
            sha: Some(String::from("reviewed-sha")),
            state: String::from("opened"),
            draft: false,
            has_conflicts: false,
            detailed_merge_status: None,
            web_url: Some(format!("https://gitlab.example/mr/{iid}")),
            updated_at: None,
        }
    }

    #[test]
    fn key_5_focuses_mr_panel_and_requests_the_list() {
        let mut state = state_with_pipeline("success", vec![]);
        let commands = press(&mut state, KeyCode::Char('5'));

        assert_eq!(state.focus, Focus::MergeRequests);
        assert_eq!(
            commands,
            vec![Command::FetchMergeRequests {
                scope: crate::state::MrScope::Project,
                project_id: Some(1),
            }]
        );
        assert!(state.mr_panel.loading);

        // While loading, focusing again must not double-fetch.
        state.focus = Focus::Projects;
        assert!(press(&mut state, KeyCode::Char('5')).is_empty());
    }

    #[test]
    fn m_cycles_mr_scope_and_refetches() {
        let mut state = state_with_pipeline("success", vec![]);
        press(&mut state, KeyCode::Char('5'));
        state.mr_panel.loading = false;
        state.mr_panel.mrs = vec![mr(5, "Add feature")];

        let commands = press(&mut state, KeyCode::Char('m'));
        assert_eq!(state.mr_panel.scope, crate::state::MrScope::CreatedByMe);
        assert!(
            state.mr_panel.mrs.is_empty(),
            "scope switch resets the list"
        );
        assert_eq!(
            commands,
            vec![Command::FetchMergeRequests {
                scope: crate::state::MrScope::CreatedByMe,
                project_id: Some(1),
            }]
        );
    }

    #[test]
    fn moving_mr_cursor_lazily_requests_head_pipeline() {
        let mut state = state_with_pipeline("success", vec![]);
        press(&mut state, KeyCode::Char('5'));
        state.mr_panel.loading = false;
        state.mr_panel.mrs = vec![mr(5, "One"), mr(6, "Two")];

        let commands = press(&mut state, KeyCode::Char('j'));
        assert_eq!(state.mr_panel.selected, 1);
        assert_eq!(
            commands,
            vec![Command::FetchMrHeadPipeline {
                project_id: 1,
                iid: 6
            }]
        );
        // The slot is now in flight; moving back and forth must not re-request it.
        press(&mut state, KeyCode::Char('k'));
        assert!(press(&mut state, KeyCode::Char('j')).is_empty());
    }

    #[test]
    fn enter_on_mr_with_loaded_pipeline_drills_into_it() {
        let mut state = state_with_pipeline("success", vec![]);
        press(&mut state, KeyCode::Char('5'));
        state.mr_panel.loading = false;
        state.mr_panel.mrs = vec![mr(5, "One")];
        state.mr_panel.head_pipelines.insert(
            (1, 5),
            crate::state::HeadPipelineSlot::Loaded(Some(older_pipeline(900, 9, "failed"))),
        );

        let commands = press(&mut state, KeyCode::Enter);
        assert_eq!(
            commands,
            vec![Command::FetchPipelineJobs {
                project_id: 1,
                pipeline_project_id: 1,
                pipeline_id: 900,
            }]
        );
        assert_eq!(state.focus, Focus::Detail);
        assert_eq!(
            state.viewed_pipeline.get(&1).map(|v| v.pipeline.id),
            Some(900)
        );
    }

    #[test]
    fn merge_menu_item_is_disabled_for_draft_mrs() {
        let mut state = state_with_pipeline("success", vec![]);
        press(&mut state, KeyCode::Char('5'));
        state.mr_panel.loading = false;
        let mut draft = mr(5, "WIP");
        draft.draft = true;
        state.mr_panel.mrs = vec![draft];

        press(&mut state, KeyCode::Char('x'));
        let Overlay::ActionMenu(menu) = &state.overlay else {
            panic!("expected action menu, got {:?}", state.overlay);
        };
        let merge = menu
            .items
            .iter()
            .find(|item| item.label == "Merge MR")
            .expect("merge entry");
        assert_eq!(merge.disabled_reason.as_deref(), Some("MR is a draft"));
        let approve = menu
            .items
            .iter()
            .find(|item| item.label == "Approve MR")
            .expect("approve entry");
        assert!(approve.disabled_reason.is_none());
    }

    #[test]
    fn selecting_retry_in_menu_opens_the_existing_confirm_dialog() {
        let mut state = state_with_pipeline("failed", vec![]);
        press(&mut state, KeyCode::Char('x'));
        // Cursor starts on the first enabled item (Retry pipeline) for a failed pipeline.
        press(&mut state, KeyCode::Enter);
        assert_eq!(
            state.overlay,
            Overlay::Confirm(Action::RetryPipeline {
                project_id: 1,
                pipeline_id: 42,
            })
        );
    }

    #[test]
    fn action_menu_closes_on_x_and_does_nothing_for_disabled_rows() {
        // A passing pipeline: Retry/Cancel are disabled. Navigate to a disabled row, Enter is a no-op.
        let mut state = state_with_pipeline("success", vec![]);
        press(&mut state, KeyCode::Char('x'));
        assert!(matches!(state.overlay, Overlay::ActionMenu(_)));
        // `x` toggles the menu closed.
        press(&mut state, KeyCode::Char('x'));
        assert_eq!(state.overlay, Overlay::None);
    }

    #[test]
    fn confirm_dialog_dispatches_on_y_and_discards_on_n() {
        let mut state = state_with_pipeline("failed", vec![]);
        press(&mut state, KeyCode::Char('R'));

        let commands = press(&mut state, KeyCode::Char('y'));
        assert_eq!(
            commands,
            vec![Command::Dispatch(Action::RetryPipeline {
                project_id: 1,
                pipeline_id: 42,
            })]
        );
        assert_eq!(state.overlay, Overlay::None);
        assert!(state.pending_actions.contains(&1));

        let mut state = state_with_pipeline("failed", vec![]);
        press(&mut state, KeyCode::Char('R'));
        let commands = press(&mut state, KeyCode::Char('n'));
        assert!(commands.is_empty());
        assert_eq!(state.overlay, Overlay::None);
        assert!(state.pending_actions.is_empty());
    }

    #[test]
    fn enter_in_detail_panel_opens_job_log_and_focuses_log_pane() {
        let mut state = state_with_pipeline("running", vec![job(10, "build", "running")]);
        press(&mut state, KeyCode::Char('3'));

        let commands = press(&mut state, KeyCode::Enter);
        assert_eq!(
            commands,
            vec![Command::FetchTrace {
                project_id: 1,
                job_id: 10,
                generation: 1,
            }]
        );
        let Some(view) = &state.log else {
            panic!("expected a loaded job log");
        };
        assert!(view.follow, "running jobs start in follow mode");
        assert!(view.loading);
        assert_eq!(state.focus, Focus::Log, "opening a log focuses its pane");
    }

    #[test]
    fn opening_a_log_on_a_queued_job_also_starts_in_follow_mode() {
        // A job that has not started running yet is still in flight; following from the start means
        // its log streams as soon as it produces output, without reopening.
        for status in ["created", "pending", "preparing"] {
            let mut state = state_with_pipeline("running", vec![job(10, "build", status)]);
            press(&mut state, KeyCode::Char('3'));
            press(&mut state, KeyCode::Enter);
            let Some(view) = &state.log else {
                panic!("expected a loaded job log");
            };
            assert!(view.follow, "{status} jobs should start in follow mode");
        }
    }

    #[test]
    fn opening_a_log_on_a_finished_job_does_not_follow() {
        let mut state = state_with_pipeline("success", vec![job(10, "build", "success")]);
        press(&mut state, KeyCode::Char('3'));
        press(&mut state, KeyCode::Enter);
        let Some(view) = &state.log else {
            panic!("expected a loaded job log");
        };
        assert!(!view.follow, "a finished job's log is static");
    }

    #[test]
    fn log_panel_scroll_up_disables_follow_and_persists_after_leaving() {
        let mut state = state_with_pipeline("running", vec![job(10, "build", "running")]);
        press(&mut state, KeyCode::Char('3'));
        press(&mut state, KeyCode::Enter);

        // Focus is now on the log pane; k scrolls and leaves follow mode.
        press(&mut state, KeyCode::Char('k'));
        let Some(view) = &state.log else {
            panic!("expected a loaded job log");
        };
        assert!(!view.follow, "scrolling up leaves follow mode");

        // Leaving the pane keeps the log loaded so the user can come back to it.
        press(&mut state, KeyCode::Esc);
        assert_eq!(state.focus, Focus::Projects);
        assert!(
            state.log.is_some(),
            "log persists after focus leaves the pane"
        );
    }

    #[test]
    fn other_panels_stay_navigable_while_a_log_is_open() {
        let mut state = state_with_projects(&[("a", 1), ("b", 2)]);
        // Open a log, then move back to the projects panel and navigate it.
        press(&mut state, KeyCode::Char('L'));
        press(&mut state, KeyCode::Char('1'));
        assert_eq!(state.focus, Focus::Projects);
        press(&mut state, KeyCode::Char('j'));
        assert_eq!(
            state.selected_project_id,
            Some(2),
            "j still moves the project cursor"
        );
    }

    #[test]
    fn stale_trace_generations_are_ignored() {
        let mut state = state_with_pipeline("running", vec![job(10, "build", "running")]);
        press(&mut state, KeyCode::Char('L')); // generation 1
        press(&mut state, KeyCode::Char('L')); // reopen → generation 2

        let Some(view) = &mut state.log else {
            panic!("expected a loaded job log");
        };
        assert!(!view.apply_trace(1, 10, 1, Ok(String::from("stale"))));
        assert!(view.lines.is_empty(), "stale generation must be dropped");

        let should_refetch = view.apply_trace(1, 10, 2, Ok(String::from("line1\nline2")));
        assert!(should_refetch, "follow mode schedules the next fetch");
        assert_eq!(view.lines.len(), 2);
    }

    #[test]
    fn variables_overlay_opens_loading_and_reveals_on_s() {
        let mut state = state_with_pipeline("success", vec![]);
        let commands = press(&mut state, KeyCode::Char('v'));
        assert_eq!(commands, vec![Command::FetchVariables { project_id: 1 }]);

        let Overlay::Variables(view) = &mut state.overlay else {
            panic!("expected variables overlay");
        };
        assert!(view.loading);
        view.loading = false;
        view.variables = vec![gitlab::ProjectVariable {
            key: String::from("TOKEN"),
            value: String::from("secret"),
            variable_type: None,
            protected: false,
            masked: true,
            environment_scope: None,
        }];

        press(&mut state, KeyCode::Char('s'));
        let Overlay::Variables(view) = &state.overlay else {
            panic!("expected variables overlay");
        };
        assert!(view.revealed);

        for code in [
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Down,
            KeyCode::Up,
        ] {
            let Overlay::Variables(view) = &mut state.overlay else {
                panic!("expected variables overlay");
            };
            view.notice = Some(String::from("copied"));
            press(&mut state, code);
            let Overlay::Variables(view) = &state.overlay else {
                panic!("expected variables overlay");
            };
            assert!(view.revealed);
            assert_eq!(view.notice.as_deref(), Some("copied"));
        }

        press(&mut state, KeyCode::Char('v'));
        assert_eq!(state.overlay, Overlay::None);
    }

    #[test]
    fn variables_overlay_copies_selected_value_on_y() {
        let mut state = state_with_pipeline("success", vec![]);
        press(&mut state, KeyCode::Char('v'));
        let Overlay::Variables(view) = &mut state.overlay else {
            panic!("expected variables overlay");
        };
        view.loading = false;
        view.variables = vec![gitlab::ProjectVariable {
            key: String::from("TOKEN"),
            value: String::from("secret"),
            variable_type: None,
            protected: false,
            masked: true,
            environment_scope: None,
        }];

        let commands = press(&mut state, KeyCode::Char('y'));
        assert_eq!(
            commands,
            vec![Command::CopyToClipboard {
                value: String::from("secret"),
            }]
        );
        let Overlay::Variables(view) = &state.overlay else {
            panic!("expected variables overlay");
        };
        assert_eq!(view.notice.as_deref(), Some("Copied value of TOKEN"));
    }

    #[test]
    fn variables_overlay_opens_ci_cd_settings_on_o() {
        let mut state = state_with_pipeline("success", vec![]);
        // Give the project a web_url so the settings URL can be built.
        state.items[0].project = Some(gitlab::Project {
            id: 1,
            name: "api".into(),
            path_with_namespace: Some("group/api".into()),
            default_branch: Some("main".into()),
            web_url: Some("https://gl/group/api".into()),
        });
        press(&mut state, KeyCode::Char('v'));
        if let Overlay::Variables(view) = &mut state.overlay {
            view.loading = false;
        }

        let commands = press(&mut state, KeyCode::Char('o'));
        assert_eq!(
            commands,
            vec![Command::OpenUrl {
                url: String::from(
                    "https://gl/group/api/-/settings/ci_cd#js-cicd-variables-settings",
                ),
            }]
        );
        let Overlay::Variables(view) = &state.overlay else {
            panic!("overlay should stay open after opening the browser");
        };
        assert_eq!(view.notice.as_deref(), Some("Opening in browser…"));
    }

    #[test]
    fn value_window_copies_selected_substring_and_restores_on_esc() {
        let mut state = state_with_pipeline("success", vec![]);
        press(&mut state, KeyCode::Char('v'));
        let Overlay::Variables(view) = &mut state.overlay else {
            panic!("expected variables overlay");
        };
        view.loading = false;
        view.variables = vec![make_variable("TOKEN", "abcdef")];

        // Enter opens the full-value window.
        press(&mut state, KeyCode::Enter);
        let Overlay::VariableValue(view) = &mut state.overlay else {
            panic!("expected value window");
        };
        // Select "cde" (a mouse drag sets anchor/cursor the same way).
        view.anchor = Some(2);
        view.cursor = 4;
        assert_eq!(view.selected_text(), "cde");

        let commands = press(&mut state, KeyCode::Char('y'));
        assert_eq!(
            commands,
            vec![Command::CopyToClipboard {
                value: String::from("cde"),
            }]
        );
        let Overlay::VariableValue(view) = &state.overlay else {
            panic!("value window stays open after copy");
        };
        assert_eq!(view.notice.as_deref(), Some("Copied 3 chars"));

        // Esc returns to the variables overlay it came from.
        press(&mut state, KeyCode::Esc);
        let Overlay::Variables(view) = &state.overlay else {
            panic!("expected variables overlay restored");
        };
        assert_eq!(view.variables.len(), 1);
    }

    #[test]
    fn value_window_with_no_selection_copies_whole_value() {
        let mut state = state_with_pipeline("success", vec![]);
        press(&mut state, KeyCode::Char('v'));
        let Overlay::Variables(view) = &mut state.overlay else {
            panic!("expected variables overlay");
        };
        view.loading = false;
        view.variables = vec![make_variable("TOKEN", "secret")];

        press(&mut state, KeyCode::Enter);
        let commands = press(&mut state, KeyCode::Char('y'));
        assert_eq!(
            commands,
            vec![Command::CopyToClipboard {
                value: String::from("secret"),
            }]
        );
    }

    #[test]
    fn variables_overlay_filters_by_key_and_copies_match() {
        let mut state = state_with_pipeline("success", vec![]);
        press(&mut state, KeyCode::Char('v'));
        let Overlay::Variables(view) = &mut state.overlay else {
            panic!("expected variables overlay");
        };
        view.loading = false;
        view.variables = vec![
            make_variable("API_TOKEN", "tok"),
            make_variable("DB_PASSWORD", "pw"),
            make_variable("DB_USER", "admin"),
        ];

        // Enter filter mode and narrow to the two DB_* keys.
        press(&mut state, KeyCode::Char('/'));
        press(&mut state, KeyCode::Char('d'));
        press(&mut state, KeyCode::Char('b'));
        let Overlay::Variables(view) = &state.overlay else {
            panic!("expected variables overlay");
        };
        assert!(view.searching);
        let matched: Vec<&str> = view.matching().iter().map(|v| v.key.as_str()).collect();
        assert_eq!(matched, vec!["DB_PASSWORD", "DB_USER"]);

        // Confirm the filter, move to the second match, copy it.
        press(&mut state, KeyCode::Enter);
        press(&mut state, KeyCode::Char('j'));
        let commands = press(&mut state, KeyCode::Char('y'));
        assert_eq!(
            commands,
            vec![Command::CopyToClipboard {
                value: String::from("admin"),
            }]
        );

        // Esc clears the filter without closing the modal.
        press(&mut state, KeyCode::Char('/'));
        press(&mut state, KeyCode::Esc);
        let Overlay::Variables(view) = &state.overlay else {
            panic!("expected variables overlay still open");
        };
        assert!(view.filter.is_empty());
        assert!(!view.searching);
        assert_eq!(view.matching().len(), 3);
    }

    #[test]
    fn variables_overlay_uses_cache_on_reopen_and_refreshes_in_background() {
        let mut state = state_with_pipeline("success", vec![]);

        // First open: no cache, so it shows loading and requests a fetch.
        let commands = press(&mut state, KeyCode::Char('v'));
        assert_eq!(commands, vec![Command::FetchVariables { project_id: 1 }]);
        let Overlay::Variables(view) = &state.overlay else {
            panic!("expected variables overlay");
        };
        assert!(view.loading);

        // Fetch completes: data is shown (sorted) and cached.
        crate::handle_variables_loaded(
            1,
            Ok(vec![
                make_variable("B_KEY", "b"),
                make_variable("A_KEY", "a"),
            ]),
            &mut state,
        );
        let Overlay::Variables(view) = &state.overlay else {
            panic!("expected variables overlay");
        };
        assert!(!view.loading);
        let keys: Vec<&str> = view.variables.iter().map(|v| v.key.as_str()).collect();
        assert_eq!(keys, vec!["A_KEY", "B_KEY"]);

        // Reopen: cached data shows instantly (no loading flash), but a background
        // refresh is still requested.
        press(&mut state, KeyCode::Esc);
        assert_eq!(state.overlay, Overlay::None);
        let commands = press(&mut state, KeyCode::Char('v'));
        assert_eq!(commands, vec![Command::FetchVariables { project_id: 1 }]);
        let Overlay::Variables(view) = &mut state.overlay else {
            panic!("expected variables overlay");
        };
        assert!(!view.loading, "cached open must not flash loading");
        assert_eq!(view.variables.len(), 2);

        // Move the cursor, then let a background refresh arrive: the cursor is kept.
        view.selected = 1;
        crate::handle_variables_loaded(
            1,
            Ok(vec![
                make_variable("A_KEY", "a2"),
                make_variable("B_KEY", "b2"),
                make_variable("C_KEY", "c"),
            ]),
            &mut state,
        );
        let Overlay::Variables(view) = &state.overlay else {
            panic!("expected variables overlay");
        };
        assert_eq!(view.selected, 1, "background refresh keeps the cursor");
        assert_eq!(view.variables.len(), 3);
    }

    fn make_variable(key: &str, value: &str) -> gitlab::ProjectVariable {
        gitlab::ProjectVariable {
            key: String::from(key),
            value: String::from(value),
            variable_type: None,
            protected: false,
            masked: false,
            environment_scope: None,
        }
    }

    #[test]
    fn capital_p_opens_pipeline_history_overlay_loading() {
        let mut state = state_with_pipeline("success", vec![]);
        let commands = press(&mut state, KeyCode::Char('P'));
        assert_eq!(
            commands,
            vec![Command::FetchPipelines {
                project_id: 1,
                ref_: None,
            }]
        );
        let Overlay::Pipelines(view) = &state.overlay else {
            panic!("expected pipelines overlay");
        };
        assert!(view.loading);
        assert_eq!(view.project_id, 1);
    }

    #[test]
    fn selecting_a_pipeline_in_history_views_it_and_fetches_its_jobs() {
        let mut state = state_with_pipeline("success", vec![job(10, "build", "success")]);
        press(&mut state, KeyCode::Char('P'));

        // Simulate the listing arriving, then pick the second (older) pipeline.
        let Overlay::Pipelines(view) = &mut state.overlay else {
            panic!("expected pipelines overlay");
        };
        view.loading = false;
        view.pipelines = vec![pipeline("success"), older_pipeline(7, 5, "failed")];

        press(&mut state, KeyCode::Char('j'));
        let commands = press(&mut state, KeyCode::Enter);
        assert_eq!(
            commands,
            vec![Command::FetchPipelineJobs {
                project_id: 1,
                pipeline_project_id: 1,
                pipeline_id: 7,
            }]
        );
        assert_eq!(state.overlay, Overlay::None);
        assert_eq!(state.focus, Focus::Detail);

        let view = state.viewed_pipeline.get(&1).expect("viewed pipeline set");
        assert_eq!(view.pipeline.id, 7);
        assert!(view.loading);
        // The detail panel now reflects the inspected pipeline, not the latest snapshot.
        assert_eq!(state.active_pipeline().map(|p| p.id), Some(7));
    }

    #[test]
    fn esc_in_detail_panel_drops_history_view_before_leaving() {
        let mut state = state_with_pipeline("success", vec![job(10, "build", "success")]);
        state.focus = Focus::Detail;
        state.viewed_pipeline.insert(
            1,
            crate::state::ViewedPipeline {
                project_id: 1,
                bridges: Vec::new(),
                pipeline: older_pipeline(7, 5, "failed"),
                jobs: vec![job(99, "old", "failed")],
                loading: false,
                error: None,
            },
        );

        // First Esc clears the history view but keeps focus on the panel.
        press(&mut state, KeyCode::Esc);
        assert!(state.viewed_pipeline.is_empty());
        assert_eq!(state.focus, Focus::Detail);

        // Second Esc now leaves the panel as usual.
        press(&mut state, KeyCode::Esc);
        assert_eq!(state.focus, Focus::Projects);
    }

    fn older_pipeline(id: u64, iid: u64, status: &str) -> Pipeline {
        Pipeline {
            id,
            iid: Some(iid),
            status: status.into(),
            ref_: Some("main".into()),
            web_url: None,
            duration: None,
            created_at: None,
            updated_at: None,
            sha: None,
        }
    }

    #[test]
    fn esc_leaves_panel_focus_then_clears_filter() {
        let mut state = state_with_projects(&[("alpha", 1)]);
        press(&mut state, KeyCode::Char('/'));
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Enter);
        press(&mut state, KeyCode::Char('l'));

        press(&mut state, KeyCode::Esc);
        assert_eq!(state.focus, Focus::Projects);
        assert_eq!(state.search.query, "a", "first esc only leaves the panel");

        press(&mut state, KeyCode::Esc);
        assert!(
            state.search.query.is_empty(),
            "second esc clears the filter"
        );
    }
    #[test]
    fn viewed_pipeline_actions_and_logs_use_its_project_and_pipeline() {
        for project_id in [1, 8] {
            let mut state = state_with_pipeline("running", vec![]);
            let mut viewed = pipeline("running");
            viewed.id = 500;
            state.viewed_pipeline.insert(
                1,
                crate::state::ViewedPipeline {
                    project_id,
                    pipeline: viewed,
                    jobs: vec![job(999, "downstream", "failed")],
                    bridges: vec![],
                    loading: false,
                    error: None,
                },
            );
            state.focus = Focus::Detail;
            press(&mut state, KeyCode::Char('R'));
            assert_eq!(
                state.overlay,
                Overlay::Confirm(Action::RetryJob {
                    project_id,
                    job_id: 999,
                    job_name: "downstream".into(),
                })
            );
            state.overlay = Overlay::None;
            press(&mut state, KeyCode::Char('c'));
            assert_eq!(
                state.overlay,
                Overlay::Confirm(Action::CancelPipeline {
                    project_id,
                    pipeline_id: 500,
                })
            );
            state.overlay = Overlay::None;
            assert_eq!(
                press(&mut state, KeyCode::Char('L')),
                vec![Command::FetchTrace {
                    project_id,
                    job_id: 999,
                    generation: 1,
                }]
            );
            state.focus = Focus::Projects;
            press(&mut state, KeyCode::Char('c'));
            assert_eq!(
                state.overlay,
                Overlay::Confirm(Action::CancelPipeline {
                    project_id: 1,
                    pipeline_id: 42,
                })
            );
        }
    }

    #[test]
    fn merge_confirmation_freezes_the_reviewed_source_sha() {
        let mut state = state_with_pipeline("success", vec![]);
        state.focus = Focus::MergeRequests;
        state.mr_panel.mrs = vec![mr(5, "Review")];
        press(&mut state, KeyCode::Char('x'));
        let Overlay::ActionMenu(menu) = &mut state.overlay else {
            panic!("menu");
        };
        menu.selected = menu
            .items
            .iter()
            .position(|item| item.label == "Merge MR")
            .expect("merge item");
        press(&mut state, KeyCode::Enter);
        state.mr_panel.mrs[0].sha = Some("new-unreviewed-sha".into());
        assert_eq!(
            press(&mut state, KeyCode::Char('y')),
            vec![Command::Dispatch(Action::MergeMr {
                project_id: 1,
                iid: 5,
                sha: "reviewed-sha".into(),
            })]
        );
    }

    #[test]
    fn merge_is_disabled_without_the_source_sha() {
        let mut state = state_with_pipeline("success", vec![]);
        state.focus = Focus::MergeRequests;
        let mut request = mr(5, "Review");
        request.sha = None;
        state.mr_panel.mrs = vec![request];
        press(&mut state, KeyCode::Char('x'));
        let Overlay::ActionMenu(menu) = &state.overlay else {
            panic!("menu");
        };
        assert!(menu
            .items
            .iter()
            .find(|item| item.label == "Merge MR")
            .expect("merge item")
            .disabled_reason
            .as_deref()
            .expect("disabled reason")
            .contains("SHA"));
    }
}
