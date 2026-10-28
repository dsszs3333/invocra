use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::cache::{
    CachedDashboardState, CachedProjectSnapshot, CachedProjectStatus, SnapshotOrigin,
};
use config::ProjectEntry;
use gitlab::{
    Branch, Bridge, CiStatus, Environment, Job, MergeRequest, Pipeline, Project, ProjectVariable,
    Tag,
};
use serde::Serialize;

use crate::actions::Action;
use crate::refresh::RefreshSources;

/// Whole seconds of a GitLab job duration (reported as float seconds).
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "`as` truncates the fraction and saturates negative, NaN and huge values, as wanted"
)]
pub(crate) const fn job_duration_secs(seconds: f64) -> u64 {
    seconds as u64
}

#[derive(Debug, Clone)]
pub struct ProjectSnapshot {
    pub configured: ProjectEntry,
    pub project: Option<Project>,
    pub pipeline: Option<Pipeline>,
    pub jobs: Vec<Job>,
    /// Trigger jobs of the latest pipeline, each carrying its downstream pipeline's status.
    pub bridges: Vec<Bridge>,
    pub status: ProjectStatus,
    pub origin: SnapshotOrigin,
}

#[derive(Debug, Clone)]
pub enum ProjectStatus {
    Loading,
    Ready,
    Failed(String),
    PartialError(String),
}

impl ProjectSnapshot {
    fn loading(configured: ProjectEntry) -> Self {
        Self {
            configured,
            project: None,
            pipeline: None,
            jobs: Vec::new(),
            bridges: Vec::new(),
            status: ProjectStatus::Loading,
            origin: SnapshotOrigin::Static,
        }
    }

    fn from_cached_snapshot(configured: ProjectEntry, cached: CachedProjectSnapshot) -> Self {
        let status = match cached.status {
            CachedProjectStatus::Ready => ProjectStatus::Ready,
            CachedProjectStatus::Failed(message) => ProjectStatus::Failed(message),
            CachedProjectStatus::PartialError(message) => ProjectStatus::PartialError(message),
        };

        Self {
            configured,
            project: cached.project,
            pipeline: cached.pipeline,
            jobs: cached.jobs,
            bridges: cached.bridges,
            status,
            origin: cached.origin,
        }
    }

    pub(crate) const fn is_loading(&self) -> bool {
        matches!(self.status, ProjectStatus::Loading)
    }

    pub(crate) const fn is_failed(&self) -> bool {
        matches!(self.status, ProjectStatus::Failed(_))
    }

    pub(crate) const fn has_error(&self) -> bool {
        matches!(
            self.status,
            ProjectStatus::Failed(_) | ProjectStatus::PartialError(_)
        )
    }

    pub(crate) fn error_message(&self) -> Option<&str> {
        match &self.status {
            ProjectStatus::Failed(message) | ProjectStatus::PartialError(message) => {
                Some(message.as_str())
            }
            ProjectStatus::Loading | ProjectStatus::Ready => None,
        }
    }
}

/// Which panel owns the cursor, lazygit-style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Focus {
    Projects,
    Branches,
    Detail,
    Log,
    MergeRequests,
}

/// Whether the focused panel is shown alongside the others or expanded to fill the screen
/// (lazygit's `+`/`_`). Transient view state, never persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScreenMode {
    Normal,
    Expanded,
}

/// Modal overlay drawn on top of the panels.
// Not `Eq`: `Pipelines` carries `gitlab::Pipeline`, which is only `PartialEq`.
#[derive(Debug, PartialEq)]
pub(crate) enum Overlay {
    None,
    Help,
    Confirm(Action),
    Variables(VariablesViewState),
    /// Full-value viewer opened from the variables overlay; supports mouse substring selection.
    VariableValue(ValueViewState),
    Pipelines(PipelinesViewState),
    Environments(EnvironmentsViewState),
    Tags(TagsViewState),
    /// lazygit-style context menu of actions for the focused job/pipeline.
    ActionMenu(ActionMenuState),
}

/// A context menu of actions (retry/cancel/play/run/open/copy) for the focused job or pipeline.
#[derive(Debug, PartialEq)]
pub(crate) struct ActionMenuState {
    pub(crate) title: String,
    pub(crate) items: Vec<ActionMenuItem>,
    pub(crate) selected: usize,
}

#[derive(Debug, PartialEq)]
pub(crate) struct ActionMenuItem {
    pub(crate) label: String,
    pub(crate) description: String,
    pub(crate) effect: MenuEffect,
    /// When `Some`, the item is greyed out and selecting it does nothing; the string says why.
    pub(crate) disabled_reason: Option<String>,
}

/// What selecting an enabled menu item does.
#[derive(Debug, PartialEq)]
pub(crate) enum MenuEffect {
    /// Route through the existing confirmation flow before mutating.
    Confirm(Action),
    /// Open a URL in the browser.
    Open(String),
    /// Copy a URL to the clipboard.
    Copy(String),
    /// Download a job's artifacts archive in the background (no confirmation — not mutating).
    Download {
        project_id: u64,
        job_id: u64,
        job_name: String,
        filename: String,
    },
}

/// A one-shot trace fetch the run loop should issue to (re)start the trace-follow loop for an
/// open log, produced when a snapshot refresh changes a followed job's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TraceFetchRequest {
    pub(crate) project_id: u64,
    pub(crate) job_id: u64,
    pub(crate) generation: u64,
}

/// State of the full-screen job log viewer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LogViewState {
    pub(crate) project_id: u64,
    pub(crate) job_id: u64,
    pub(crate) job_name: String,
    pub(crate) job_status: String,
    /// Display rows: the sanitized trace wrapped to `wrap_width`. Everything (cursor, selection,
    /// scroll, mouse mapping) indexes these, so one row stays one screen line after wrapping.
    pub(crate) lines: Vec<String>,
    /// Sanitized but unwrapped trace lines, kept so a resize can re-wrap and "copy log" stays whole.
    pub(crate) raw: Vec<String>,
    /// Column width the trace is currently wrapped to (the log pane's inner width).
    pub(crate) wrap_width: usize,
    pub(crate) scroll: usize,
    /// Re-fetch the trace periodically and pin the view to the bottom, like `less +F`.
    pub(crate) follow: bool,
    pub(crate) loading: bool,
    /// Tags trace fetches; reports with a stale generation are dropped.
    pub(crate) generation: u64,
    pub(crate) error: Option<String>,
    /// Transient feedback shown in the title bar, e.g. after copying the log.
    pub(crate) notice: Option<String>,
    /// Cursor line for nvim-style visual selection (index into `lines`).
    pub(crate) cursor: usize,
    /// Anchor line when visual mode is active; `None` outside visual mode. The selection spans
    /// `min(anchor, cursor)..=max(anchor, cursor)`, line-wise like nvim's `V`.
    pub(crate) select_anchor: Option<usize>,
    /// Last rendered inner height of the pane, so the input layer can keep the cursor on screen.
    pub(crate) viewport_height: usize,
    /// In-log search (`/`, `n`/`N`).
    pub(crate) search: LogSearchState,
}

/// Search state of the log viewer: query input, matches, and the current match position.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct LogSearchState {
    /// True while the user is typing the query.
    pub(crate) input: bool,
    pub(crate) query: String,
    /// Indices into `lines` that match `query`; recomputed on a new trace or re-wrap.
    pub(crate) matches: Vec<usize>,
    /// Position in `matches` the cursor was last jumped to.
    pub(crate) current: Option<usize>,
}

impl LogViewState {
    /// Applies a trace fetch result if it belongs to this viewer (same job and generation).
    /// Returns true when follow mode should schedule the next fetch.
    pub(crate) fn apply_trace(
        &mut self,
        project_id: u64,
        job_id: u64,
        generation: u64,
        result: Result<String, String>,
    ) -> bool {
        if self.generation != generation || self.project_id != project_id || self.job_id != job_id {
            return false;
        }

        self.loading = false;
        match result {
            Ok(trace) => {
                self.raw = sanitize_trace(&trace);
                self.lines = wrap_lines(&self.raw, self.wrap_width);
                self.recompute_search();
                self.error = None;
                if self.follow {
                    self.scroll = self.lines.len();
                    self.cursor = self.lines.len().saturating_sub(1);
                } else {
                    self.cursor = self.cursor.min(self.lines.len().saturating_sub(1));
                }
                self.follow
            }
            Err(err) => {
                self.error = Some(err);
                self.follow = false;
                false
            }
        }
    }

    /// Inclusive `(start, end)` line range of the visual selection, or `None` when visual mode is
    /// off or there is nothing to select. Both ends are clamped to the current line count.
    pub(crate) fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.select_anchor?;
        let last = self.lines.len().checked_sub(1)?;
        let anchor = anchor.min(last);
        let cursor = self.cursor.min(last);
        Some((anchor.min(cursor), anchor.max(cursor)))
    }

    /// Number of lines currently selected in visual mode (0 when inactive).
    pub(crate) fn selection_len(&self) -> usize {
        self.selection_range()
            .map_or(0, |(start, end)| end - start + 1)
    }

    /// The selected lines joined with newlines, ready for the clipboard.
    pub(crate) fn selected_text(&self) -> String {
        match self.selection_range() {
            Some((start, end)) => self.lines[start..=end].join("\n"),
            None => String::new(),
        }
    }

    /// Enters visual mode, anchoring the selection at the current cursor line.
    pub(crate) fn begin_visual(&mut self) {
        self.follow = false;
        self.cursor = self.cursor.min(self.lines.len().saturating_sub(1));
        self.select_anchor = Some(self.cursor);
    }

    /// Enters visual mode anchored at `line` (clamped), for a mouse press that starts a selection.
    pub(crate) fn begin_visual_at(&mut self, line: usize) {
        self.follow = false;
        self.cursor = line.min(self.lines.len().saturating_sub(1));
        self.select_anchor = Some(self.cursor);
    }

    /// Leaves visual mode, clearing the anchor.
    pub(crate) fn end_visual(&mut self) {
        self.select_anchor = None;
    }

    /// True when the view is parked at (or past) the bottom of the log, i.e. the user has not
    /// scrolled up into the backlog. Used to decide whether auto-refresh may re-enable follow.
    pub(crate) fn at_tail(&self) -> bool {
        self.scroll >= self.lines.len().saturating_sub(self.viewport_height)
    }

    /// Moves the visual-mode cursor to `line` (clamped) and scrolls the minimum needed to keep it
    /// on screen, like nvim.
    pub(crate) fn move_cursor_to(&mut self, line: usize) {
        let last = self.lines.len().saturating_sub(1);
        self.cursor = line.min(last);
        let height = self.viewport_height.max(1);
        // Stay in step with render's `scroll.min(max_offset)`, so a selection begun in follow mode
        // (scroll left at lines.len()) doesn't read as "cursor above the window" on every drag.
        self.scroll = self.scroll.min(self.lines.len().saturating_sub(height));
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + height {
            self.scroll = self.cursor + 1 - height;
        }
    }

    /// Re-wraps the trace when the pane width changes, clamping cursor/scroll to the new row count.
    pub(crate) fn set_wrap_width(&mut self, width: usize) {
        if width == self.wrap_width {
            return;
        }
        self.wrap_width = width;
        self.lines = wrap_lines(&self.raw, width);
        self.recompute_search();
        let last = self.lines.len().saturating_sub(1);
        self.cursor = self.cursor.min(last);
        self.scroll = self.scroll.min(self.lines.len());
    }

    /// Recomputes search matches against the current display rows. Case-insensitive substring
    /// match, same semantics as the project filter. Call whenever `lines` changes.
    pub(crate) fn recompute_search(&mut self) {
        self.search.current = None;
        if self.search.query.is_empty() {
            self.search.matches.clear();
            return;
        }
        let needle = self.search.query.to_lowercase();
        self.search.matches = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
    }

    /// Commits the typed query: leaves input mode and jumps to the first match at or after the
    /// cursor (wrapping to the top), like nvim's `/` + Enter.
    pub(crate) fn search_commit(&mut self) {
        self.search.input = false;
        self.jump_to_match(|matches, cursor| {
            matches.iter().position(|&line| line >= cursor).unwrap_or(0)
        });
    }

    /// Moves to the next match after the cursor, wrapping to the first.
    pub(crate) fn search_next(&mut self) {
        self.jump_to_match(|matches, cursor| {
            matches.iter().position(|&line| line > cursor).unwrap_or(0)
        });
    }

    /// Moves to the previous match before the cursor, wrapping to the last.
    pub(crate) fn search_prev(&mut self) {
        self.jump_to_match(|matches, cursor| {
            matches
                .iter()
                .rposition(|&line| line < cursor)
                .unwrap_or(matches.len() - 1)
        });
    }

    /// Leaves search mode, clearing the query and match highlights.
    pub(crate) fn clear_search(&mut self) {
        self.search = LogSearchState::default();
    }

    fn jump_to_match(&mut self, pick: impl Fn(&[usize], usize) -> usize) {
        if self.search.matches.is_empty() {
            return;
        }
        let pos = pick(&self.search.matches, self.cursor);
        self.search.current = Some(pos);
        self.follow = false;
        self.move_cursor_to(self.search.matches[pos]);
    }
}

/// Wraps each sanitized line to `width` columns so long lines stay readable. Breaks on spaces when
/// possible and hard-breaks tokens longer than the width; every source line yields at least one row.
/// ponytail: counts chars, not terminal columns — wide/CJK glyphs can overrun by a cell. Swap in
/// unicode-width if logs ever carry them. width 0 (pane not yet measured) passes lines through.
fn wrap_lines(raw: &[String], width: usize) -> Vec<String> {
    if width == 0 {
        return raw.to_vec();
    }
    raw.iter().flat_map(|line| wrap_line(line, width)).collect()
}

fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if line.chars().count() <= width {
        return vec![line.to_string()];
    }
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut len = 0;
    // split_inclusive keeps each word's trailing space, preserving the original spacing.
    for word in line.split_inclusive(' ') {
        let wlen = word.chars().count();
        if len > 0 && len + wlen > width {
            rows.push(std::mem::take(&mut row));
            len = 0;
        }
        if wlen > width {
            // A token wider than the pane: hard-break it across rows.
            for ch in word.chars() {
                if len == width {
                    rows.push(std::mem::take(&mut row));
                    len = 0;
                }
                row.push(ch);
                len += 1;
            }
        } else {
            row.push_str(word);
            len += wlen;
        }
    }
    rows.push(row);
    rows
}

/// A historical pipeline the user opened from the history overlay, shown in the `[3]` panel
/// instead of the snapshot's latest. Transient — never persisted.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ViewedPipeline {
    /// Project the pipeline belongs to — differs from the dashboard project's id when inspecting
    /// a downstream pipeline of a multi-project bridge.
    pub(crate) project_id: u64,
    pub(crate) pipeline: Pipeline,
    pub(crate) jobs: Vec<Job>,
    pub(crate) bridges: Vec<Bridge>,
    pub(crate) loading: bool,
    pub(crate) error: Option<String>,
}

/// State of the pipeline-history picker overlay.
#[derive(Debug, PartialEq)]
pub(crate) struct PipelinesViewState {
    pub(crate) project_id: u64,
    pub(crate) project_name: String,
    /// The ref the listing is scoped to (tracked branch), shown in the title; `None` lists all refs.
    pub(crate) ref_: Option<String>,
    pub(crate) loading: bool,
    pub(crate) pipelines: Vec<Pipeline>,
    pub(crate) selected: usize,
    pub(crate) error: Option<String>,
}

/// State of the tags overlay: a list of the project's tags plus an optional inline create form.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TagsViewState {
    pub(crate) project_id: u64,
    pub(crate) project_name: String,
    /// The branch a new tag points at (the selected branch when the overlay opened); `None`
    /// disables creating until a branch is known.
    pub(crate) ref_: Option<String>,
    pub(crate) loading: bool,
    pub(crate) tags: Vec<Tag>,
    pub(crate) selected: usize,
    pub(crate) error: Option<String>,
    /// `Some` while the inline "new tag" form is open.
    pub(crate) creating: Option<CreateTagForm>,
    /// `Some` while a tag's details are shown on top of the list.
    pub(crate) viewing: Option<Tag>,
}

impl TagsViewState {
    /// Selects the row at `index` (a mouse click); out-of-range clicks are ignored.
    pub(crate) fn select_at(&mut self, index: usize) {
        if index < self.tags.len() {
            self.selected = index;
        }
    }

    /// True when neither the create form nor the detail view is open (plain list mode).
    pub(crate) const fn in_list_mode(&self) -> bool {
        self.creating.is_none() && self.viewing.is_none()
    }
}

/// Inline form for creating a tag. `field` is 0 for the name input, 1 for the message input.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CreateTagForm {
    pub(crate) name: String,
    pub(crate) message: String,
    pub(crate) field: usize,
    pub(crate) error: Option<String>,
}

impl CreateTagForm {
    /// Routes a typed char / backspace into the active field.
    pub(crate) fn push(&mut self, ch: char) {
        match self.field {
            0 => self.name.push(ch),
            _ => self.message.push(ch),
        }
    }

    pub(crate) fn backspace(&mut self) {
        match self.field {
            0 => self.name.pop(),
            _ => self.message.pop(),
        };
    }

    /// Toggles between the name (0) and message (1) inputs.
    pub(crate) fn toggle_field(&mut self) {
        self.field = usize::from(self.field == 0);
    }
}

/// Whose merge requests the MR panel lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum MrScope {
    /// Open MRs of the selected project.
    #[default]
    Project,
    /// Open MRs authored by the token's user, instance-wide.
    CreatedByMe,
    /// Open MRs assigned to the token's user, instance-wide.
    AssignedToMe,
}

impl MrScope {
    pub(crate) const fn next(self) -> Self {
        match self {
            Self::Project => Self::CreatedByMe,
            Self::CreatedByMe => Self::AssignedToMe,
            Self::AssignedToMe => Self::Project,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::CreatedByMe => "created by me",
            Self::AssignedToMe => "assigned to me",
        }
    }

    /// The GitLab `scope=` parameter for the instance-wide list, `None` for the project list.
    pub(crate) const fn api_scope(self) -> Option<&'static str> {
        match self {
            Self::Project => None,
            Self::CreatedByMe => Some("created_by_me"),
            Self::AssignedToMe => Some("assigned_to_me"),
        }
    }
}

/// A lazily fetched head pipeline of one MR row. The absence of a map entry means "not requested
/// yet"; `Loading` dedupes in-flight fetches, like `BranchesState`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HeadPipelineSlot {
    Loading,
    /// Fetched; an MR may legitimately have no pipeline.
    Loaded(Option<Pipeline>),
    Failed(String),
}

/// State of the `[5] MRs` panel in the right column.
#[derive(Debug, Default)]
pub(crate) struct MrPanelState {
    pub(crate) generation: u64,
    pub(crate) scope: MrScope,
    pub(crate) loading: bool,
    pub(crate) mrs: Vec<MergeRequest>,
    pub(crate) selected: usize,
    pub(crate) error: Option<String>,
    /// Which project the current `Project`-scoped list belongs to, so a project switch reloads it.
    pub(crate) loaded_for_project: Option<u64>,
    /// Lazily fetched head pipelines, keyed by (project_id, iid).
    pub(crate) head_pipelines: HashMap<(u64, u64), HeadPipelineSlot>,
}

impl MrPanelState {
    pub(crate) fn selected_mr(&self) -> Option<&MergeRequest> {
        self.mrs.get(self.selected)
    }

    /// The selected MR's head pipeline, when it has been fetched successfully.
    pub(crate) fn selected_head_pipeline(&self) -> Option<&Pipeline> {
        let mr = self.selected_mr()?;
        match self.head_pipelines.get(&(mr.project_id, mr.iid))? {
            HeadPipelineSlot::Loaded(pipeline) => pipeline.as_ref(),
            _ => None,
        }
    }

    /// Resets the list (e.g. scope/project/instance change), keeping the chosen scope.
    pub(crate) fn reset(&mut self) {
        self.generation += 1;
        self.loading = false;
        self.mrs.clear();
        self.selected = 0;
        self.error = None;
        self.loaded_for_project = None;
        self.head_pipelines.clear();
    }
}

/// State of the environments/deployments overlay. Transient, like the pipeline picker.
#[derive(Debug, PartialEq)]
pub(crate) struct EnvironmentsViewState {
    pub(crate) project_id: u64,
    pub(crate) project_name: String,
    pub(crate) loading: bool,
    pub(crate) environments: Vec<Environment>,
    pub(crate) selected: usize,
    pub(crate) error: Option<String>,
}

impl EnvironmentsViewState {
    /// URL the selected row should open: the environment's public URL when configured, else the
    /// page of the job that deployed it.
    pub(crate) fn selected_url(&self) -> Option<String> {
        let environment = self.environments.get(self.selected)?;
        environment.external_url.clone().or_else(|| {
            environment
                .last_deployment
                .as_ref()?
                .deployable
                .as_ref()?
                .web_url
                .clone()
        })
    }
}

impl PipelinesViewState {
    /// Selects the row at `index` (a mouse click); out-of-range clicks are ignored.
    pub(crate) fn select_at(&mut self, index: usize) {
        if index < self.pipelines.len() {
            self.selected = index;
        }
    }
}

/// Turns a raw GitLab job trace into clean display lines: strips ANSI escape sequences,
/// collapses carriage-return overwrites (progress bars, GitLab's `\r\x1b[0K` line clears),
/// and removes Runner stream headers and section fold markers while keeping timestamps.
fn sanitize_trace(trace: &str) -> Vec<String> {
    strip_ansi(trace)
        .lines()
        .filter_map(sanitize_line)
        .collect()
}

/// Display text for one ANSI-stripped physical line, or `None` to drop it. A `section_start`/
/// `section_end` marker prefix is removed; GitLab puts the command echo after a `\r`, so a marker
/// carrying content keeps it while a bare marker is dropped entirely.
fn sanitize_line(line: &str) -> Option<String> {
    let (timestamp, content) = split_runner_header(line).unwrap_or(("", line));
    let (had_marker, rest) = strip_section_prefix(content);
    let collapsed = collapse_carriage_returns(rest);
    if had_marker && collapsed.is_empty() {
        return None;
    }
    if timestamp.is_empty() {
        Some(collapsed)
    } else {
        Some(format!("{timestamp} {collapsed}"))
    }
}

/// Recognizes Runner's `<UTC timestamp> <hex stream><O|E><space|+>` framing only at line start.
fn split_runner_header(line: &str) -> Option<(&str, &str)> {
    let (timestamp, rest) = line.split_once(' ')?;
    let time = timestamp.strip_suffix('Z')?;
    let seconds = time.get(..19)?;
    let fraction = time.get(19..)?;
    let valid_seconds = seconds
        .bytes()
        .zip(b"0000-00-00T00:00:00")
        .all(|(actual, pattern)| {
            if *pattern == b'0' {
                actual.is_ascii_digit()
            } else {
                actual == *pattern
            }
        });
    if !valid_seconds
        || (!fraction.is_empty()
            && (!fraction.starts_with('.')
                || !(2..=10).contains(&fraction.len())
                || !fraction[1..].bytes().all(|byte| byte.is_ascii_digit())))
    {
        return None;
    }
    let header = rest.as_bytes().get(..4)?;
    if !header[0].is_ascii_hexdigit()
        || !header[1].is_ascii_hexdigit()
        || !matches!(header[2], b'O' | b'E')
        || !matches!(header[3], b' ' | b'+')
    {
        return None;
    }
    Some((timestamp, &rest[4..]))
}

/// Splits a leading `section_(start|end):<ts>:<name>` marker off a line. Returns whether a marker
/// was present and the remainder from its trailing `\r` onward (empty when the marker stood alone).
fn strip_section_prefix(line: &str) -> (bool, &str) {
    let after_prefix = line
        .strip_prefix("section_start:")
        .or_else(|| line.strip_prefix("section_end:"));
    match after_prefix {
        Some(rest) => match rest.find('\r') {
            Some(index) => (true, &rest[index..]),
            None => (true, ""),
        },
        None => (false, line),
    }
}

/// Removes ANSI escape sequences: CSI (`ESC [ … final`) and OSC (`ESC ] … BEL`/`ST`).
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                // CSI: parameter/intermediate bytes until a final byte in 0x40..=0x7E.
                while let Some(&param) = chars.peek() {
                    chars.next();
                    if ('\u{40}'..='\u{7e}').contains(&param) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                // OSC: anything until BEL or the ST terminator (ESC \).
                while let Some(&param) = chars.peek() {
                    if param == '\u{07}' {
                        chars.next();
                        break;
                    }
                    if param == '\u{1b}' {
                        chars.next();
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                    chars.next();
                }
            }
            // A lone ESC or other escape: just drop the ESC byte.
            _ => {}
        }
    }
    out
}

/// Keeps only what survives the last carriage return, mirroring how a terminal redraws a line
/// that was overwritten in place.
fn collapse_carriage_returns(line: &str) -> String {
    line.rsplit('\r').next().unwrap_or("").to_owned()
}

/// State of the CI/CD variables overlay. Values stay masked until explicitly revealed
/// and are never persisted.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct VariablesViewState {
    pub(crate) project_id: u64,
    pub(crate) project_name: String,
    pub(crate) variables: Vec<ProjectVariable>,
    pub(crate) selected: usize,
    /// Reveals the value of the selected row only; reset when the cursor moves.
    pub(crate) revealed: bool,
    pub(crate) loading: bool,
    pub(crate) error: Option<String>,
    /// Transient one-line feedback shown in the overlay footer (e.g. "Copied").
    pub(crate) notice: Option<String>,
    /// Case-insensitive substring applied to KEY; empty means "show all".
    pub(crate) filter: String,
    /// True while the user is typing into the filter input.
    pub(crate) searching: bool,
}

impl VariablesViewState {
    /// Variables whose KEY matches the current filter, in display order.
    pub(crate) fn matching(&self) -> Vec<&ProjectVariable> {
        let needle = self.filter.to_lowercase();
        self.variables
            .iter()
            .filter(|variable| needle.is_empty() || variable.key.to_lowercase().contains(&needle))
            .collect()
    }

    /// The variable under the cursor within the filtered view, if any.
    pub(crate) fn selected_variable(&self) -> Option<&ProjectVariable> {
        self.matching().into_iter().nth(self.selected)
    }

    /// Selects the row at `index` (a mouse click), re-hiding the value like cursor movement does.
    pub(crate) fn select_at(&mut self, index: usize) {
        if index < self.matching().len() {
            self.selected = index;
            self.revealed = false;
            self.notice = None;
        }
    }
}

/// Full-value viewer for a single CI variable, opened from the variables overlay. The whole value is
/// wrapped at the window width so a mouse drag maps to an exact character range that `y` copies.
#[derive(Debug, PartialEq)]
pub(crate) struct ValueViewState {
    pub(crate) key: String,
    /// Value as chars, so wrapping and selection index by character (not byte) offset.
    pub(crate) value: Vec<char>,
    /// Topmost wrapped row shown; clamped to the content at render time.
    pub(crate) scroll: usize,
    /// Character index where a drag selection began; `None` until the user clicks.
    pub(crate) anchor: Option<usize>,
    /// Character index under the pointer; with `anchor` set it bounds the selection.
    pub(crate) cursor: usize,
    pub(crate) notice: Option<String>,
    /// The variables overlay to restore when this window closes.
    pub(crate) parent: Box<VariablesViewState>,
}

impl ValueViewState {
    /// Inclusive character range `(start, end)` of the current selection, or `None` when nothing is
    /// selected yet. Both ends are clamped to the value length.
    pub(crate) fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        let last = self.value.len().checked_sub(1)?;
        let anchor = anchor.min(last);
        let cursor = self.cursor.min(last);
        Some((anchor.min(cursor), anchor.max(cursor)))
    }

    /// The selected substring, or the whole value when nothing is selected.
    pub(crate) fn selected_text(&self) -> String {
        match self.selection_range() {
            Some((start, end)) => self.value[start..=end].iter().collect(),
            None => self.value.iter().collect(),
        }
    }
}

/// State of the `/` project filter.
#[derive(Debug, Default)]
pub(crate) struct SearchState {
    /// True while the user is typing in the filter input.
    pub(crate) active: bool,
    pub(crate) query: String,
}

impl SearchState {
    pub(crate) fn matches(&self, name: &str) -> bool {
        self.query.is_empty() || name.to_lowercase().contains(&self.query.to_lowercase())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DataSource {
    Live,
    Cached,
}

/// Lazily-fetched branch lists and their per-project fetch state, for the branches panel.
///
/// The three maps move together and encode an invariant: a project is "idle" (untouched),
/// "loading", "loaded" (has a cache entry), or "failed" (has an error) — never an inconsistent
/// mix. Keeping them behind methods is what makes that invariant hold in one place instead of at
/// every call site. `loading` also dedupes in-flight requests; `errors` suppresses a retry storm
/// until the next manual refresh.
#[derive(Debug, Default)]
pub(crate) struct BranchesState {
    cache: HashMap<u64, Vec<Branch>>,
    loading: HashSet<u64>,
    errors: HashMap<u64, String>,
}

impl BranchesState {
    /// Marks a project's branch list as being fetched right now.
    pub(crate) fn mark_loading(&mut self, project_id: u64) {
        self.loading.insert(project_id);
    }

    /// The cached branch list for a project, if it has been loaded.
    fn cached(&self, project_id: u64) -> Option<&Vec<Branch>> {
        self.cache.get(&project_id)
    }

    /// True once a project's branches have been loaded into the cache.
    pub(crate) fn has_cache(&self, project_id: u64) -> bool {
        self.cache.contains_key(&project_id)
    }

    /// The last fetch error for a project, if the most recent attempt failed.
    pub(crate) fn error(&self, project_id: u64) -> Option<&String> {
        self.errors.get(&project_id)
    }

    /// True when nothing has been attempted yet: no cache, not loading, no error.
    fn is_idle(&self, project_id: u64) -> bool {
        !self.cache.contains_key(&project_id)
            && !self.loading.contains(&project_id)
            && !self.errors.contains_key(&project_id)
    }

    /// Records a successful fetch, clearing any prior loading flag and error.
    fn store(&mut self, project_id: u64, branches: Vec<Branch>) {
        self.loading.remove(&project_id);
        self.errors.remove(&project_id);
        self.cache.insert(project_id, branches);
    }

    /// Records a failed fetch, clearing the loading flag.
    fn fail(&mut self, project_id: u64, error: String) {
        self.loading.remove(&project_id);
        self.errors.insert(project_id, error);
    }

    /// Drops all recorded errors so the next refresh can retry failed projects.
    fn clear_errors(&mut self) {
        self.errors.clear();
    }
}

/// Whether the dashboard is currently showing a live or a cached (offline) snapshot, and — when
/// cached — the unix time the snapshot was saved. The two always change together, so they live as
/// one value rather than two fields that could drift out of sync.
#[derive(Debug)]
pub(crate) struct CachedView {
    source: DataSource,
    saved_at_unix_seconds: Option<u64>,
}

impl CachedView {
    /// A view backed by a fresh, live fetch.
    fn live() -> Self {
        Self {
            source: DataSource::Live,
            saved_at_unix_seconds: None,
        }
    }

    /// A view backed by a cached snapshot saved at the given unix time.
    fn cached(saved_at_unix_seconds: u64) -> Self {
        Self {
            source: DataSource::Cached,
            saved_at_unix_seconds: Some(saved_at_unix_seconds),
        }
    }

    /// True while the dashboard is displaying cached (offline) data.
    pub(crate) fn is_cached(&self) -> bool {
        self.source == DataSource::Cached
    }

    /// The unix time the displayed cached snapshot was saved, if any.
    pub(crate) fn saved_at(&self) -> Option<u64> {
        self.saved_at_unix_seconds
    }

    /// Switches back to live data after a successful refresh.
    fn set_live(&mut self) {
        self.source = DataSource::Live;
        self.saved_at_unix_seconds = None;
    }
}

#[derive(Debug)]
pub struct AppState {
    pub(crate) instance_url: String,
    pub(crate) items: Vec<ProjectSnapshot>,
    sources: RefreshSources,
    pub(crate) selected_project_id: Option<u64>,
    pub(crate) selected_job: usize,
    /// Cursor within the (filtered) branch list of the selected project.
    pub(crate) selected_branch: usize,
    pub(crate) focus: Focus,
    /// Normal (all panels visible) or Expanded (focused panel fills the screen). Toggled with `+`/`_`.
    pub(crate) screen_mode: ScreenMode,
    pub(crate) overlay: Overlay,
    /// The most recently opened job log, shown in the right pane until another is opened.
    /// Persists independently of focus, so the user can navigate away and back.
    pub(crate) log: Option<LogViewState>,
    pub(crate) search: SearchState,
    /// `/` filter for the branches panel; independent of the projects [`SearchState`].
    pub(crate) branch_search: SearchState,
    /// Branch the dashboard tracks instead of the repo default, per project id. Persisted.
    pub(crate) ref_overrides: HashMap<u64, String>,
    /// Lazily-fetched branch lists and their fetch state, for the branches panel.
    pub(crate) branches: BranchesState,
    pub(crate) is_refreshing: bool,
    pub(crate) status_message: String,
    pub(crate) cached_view: CachedView,
    /// Projects with a mutating action in flight, awaiting their single-project refresh.
    pub(crate) pending_actions: HashSet<u64>,
    /// Individual items (pipelines/jobs) with a mutating action in flight, keyed by
    /// `(project_id, item_id)`, so the spinner can mark the exact row. Distinct from
    /// `pending_actions` (per-project). Cleared on action completion and on snapshot replace.
    pub(crate) in_flight: HashSet<(u64, u64)>,
    /// Monotonic counter tagging log-viewer fetches; see [`LogViewState::generation`].
    pub(crate) log_generation: u64,
    /// In-memory only cache of CI/CD variables per project, for instant re-open.
    /// These are secrets, so this is never serialized to disk (see [`VariablesViewState`]).
    pub(crate) variables_cache: HashMap<u64, Vec<ProjectVariable>>,
    /// Pipeline the user is inspecting per project (history overlay). Transient, not persisted.
    pub(crate) viewed_pipeline: HashMap<u64, ViewedPipeline>,
    /// The `[5] MRs` panel; survives refreshes, resets on instance switch.
    pub(crate) mr_panel: MrPanelState,
    /// Stopwatch anchor per timed item, keyed by `(project_id, job_or_pipeline_id)`. Set once when
    /// an active item is first seen (`now − server_duration`); the TIME column then shows
    /// `now − anchor`, so it ticks every second and never jumps on a refresh (we never re-base).
    /// Only live data gets anchors — cached/offline items are absent, so they show the server value.
    // ponytail: anchors leak per session; add eviction only if a session sees thousands of jobs.
    pub(crate) timer_anchor: HashMap<(u64, u64), Instant>,
    /// Last left-click (time, column, row), for detecting a double-click to activate an item.
    pub(crate) last_click: Option<(Instant, u16, u16)>,
    /// Tab labels (one per configured GitLab instance) and the active tab index, for rendering the
    /// instance tab strip. Owned by the `Dashboard`, set on build/switch.
    pub(crate) instance_labels: Vec<String>,
    pub(crate) active_instance: usize,
    /// Whether to ring the bell + post a desktop notification when a watched pipeline finishes.
    /// Set from config in `load_state_for`; defaults to true so unit tests opt in by default.
    pub(crate) notifications_enabled: bool,
    /// Last pipeline status we emitted a completion notification for, per project. Seeded on the
    /// first snapshot WITHOUT notifying, so a cold start (or already-finished pipelines at launch)
    /// is silent; only a later transition into a terminal state fires.
    pub(crate) last_notified_status: HashMap<u64, CiStatus>,
    /// Pipeline-completion messages produced during snapshot merges, drained by the run loop which
    /// turns each into a terminal bell + OSC 9 desktop notification.
    pub(crate) pending_notifications: Vec<String>,
}

impl AppState {
    pub(crate) fn new(sources: RefreshSources) -> Self {
        let items: Vec<ProjectSnapshot> = sources
            .projects
            .iter()
            .cloned()
            .map(ProjectSnapshot::loading)
            .collect();

        let mut state = Self {
            instance_url: String::new(),
            items,
            sources,
            selected_project_id: None,
            selected_job: 0,
            selected_branch: 0,
            focus: Focus::Projects,
            screen_mode: ScreenMode::Normal,
            overlay: Overlay::None,
            log: None,
            search: SearchState::default(),
            branch_search: SearchState::default(),
            ref_overrides: HashMap::new(),
            branches: BranchesState::default(),
            is_refreshing: true,
            status_message: String::from("Loading projects..."),
            cached_view: CachedView::live(),
            pending_actions: HashSet::new(),
            in_flight: HashSet::new(),
            log_generation: 0,
            variables_cache: HashMap::new(),
            viewed_pipeline: HashMap::new(),
            mr_panel: MrPanelState::default(),
            timer_anchor: HashMap::new(),
            last_click: None,
            instance_labels: Vec::new(),
            active_instance: 0,
            notifications_enabled: true,
            last_notified_status: HashMap::new(),
            pending_notifications: Vec::new(),
        };
        state.ensure_selection();
        state
    }

    pub(crate) fn from_cached(sources: RefreshSources, cached_state: CachedDashboardState) -> Self {
        let mut cached_by_id = cached_state
            .snapshots
            .into_iter()
            .map(|snapshot| (snapshot.configured.id, snapshot))
            .collect::<HashMap<_, _>>();
        let mut items: Vec<ProjectSnapshot> = sources
            .projects
            .iter()
            .cloned()
            .map(|configured| match cached_by_id.remove(&configured.id) {
                Some(snapshot) => ProjectSnapshot::from_cached_snapshot(configured, snapshot),
                None => ProjectSnapshot::loading(configured),
            })
            .collect();
        // Group-resolved projects are unknown until the first refresh; restore the cached ones
        // so the dashboard is populated when starting offline.
        items.extend(
            cached_by_id
                .into_values()
                .filter(|snapshot| snapshot.origin == SnapshotOrigin::Group)
                .map(|snapshot| {
                    ProjectSnapshot::from_cached_snapshot(snapshot.configured.clone(), snapshot)
                }),
        );
        items.sort_by(|left, right| left.configured.name.cmp(&right.configured.name));

        let mut state = Self {
            instance_url: String::new(),
            items,
            sources,
            selected_project_id: None,
            selected_job: 0,
            selected_branch: 0,
            focus: Focus::Projects,
            screen_mode: ScreenMode::Normal,
            overlay: Overlay::None,
            log: None,
            search: SearchState::default(),
            branch_search: SearchState::default(),
            ref_overrides: cached_state.ref_overrides,
            branches: BranchesState::default(),
            is_refreshing: true,
            status_message: String::from("Showing cached data while refreshing..."),
            cached_view: CachedView::cached(cached_state.saved_at_unix_seconds),
            pending_actions: HashSet::new(),
            in_flight: HashSet::new(),
            log_generation: 0,
            variables_cache: HashMap::new(),
            viewed_pipeline: HashMap::new(),
            mr_panel: MrPanelState::default(),
            timer_anchor: HashMap::new(),
            last_click: None,
            instance_labels: Vec::new(),
            active_instance: 0,
            notifications_enabled: true,
            last_notified_status: HashMap::new(),
            pending_notifications: Vec::new(),
        };
        state.ensure_selection();
        state
    }

    /// Indices into `items` that pass the current search filter, in display order.
    pub(crate) fn visible_indices(&self) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| self.search.matches(&item.configured.name))
            .map(|(index, _)| index)
            .collect()
    }

    /// Position of the selected project within the visible list, if it is visible.
    pub(crate) fn selected_visible_position(&self) -> Option<usize> {
        let selected_id = self.selected_project_id?;
        self.visible_indices()
            .iter()
            .position(|&index| self.items[index].configured.id == selected_id)
    }

    pub(crate) fn selected_item(&self) -> Option<&ProjectSnapshot> {
        let selected_id = self.selected_project_id?;
        self.items
            .iter()
            .find(|item| item.configured.id == selected_id)
    }

    /// Marks a pipeline/job as having a mutating action in flight, so its row shows a spinner until
    /// the action completes (cleared in [`apply_project_refresh`](Self::apply_project_refresh) or on
    /// action failure).
    pub(crate) fn mark_in_flight(&mut self, project_id: u64, item_id: u64) {
        self.in_flight.insert((project_id, item_id));
    }

    /// Whether a specific pipeline/job currently has an action in flight.
    pub(crate) fn is_in_flight(&self, project_id: u64, item_id: u64) -> bool {
        self.in_flight.contains(&(project_id, item_id))
    }

    /// Whether any action is in flight for the given project (used for the project-row spinner,
    /// which also covers `RunPipeline` whose item id is 0).
    pub(crate) fn project_has_in_flight(&self, project_id: u64) -> bool {
        self.in_flight.iter().any(|(pid, _)| *pid == project_id)
    }

    /// Whether any spinner is currently shown on screen — a mutating action in flight or any kind of
    /// loading (refresh, project/branch/log/jobs fetch, variables/pipelines overlay). Drives the fast
    /// redraw tick (lazygit-style) so every "Loading…" label animates smoothly; false in full idle.
    pub(crate) fn is_spinning(&self) -> bool {
        if !self.in_flight.is_empty() || self.is_refreshing {
            return true;
        }
        if self.items.iter().any(ProjectSnapshot::is_loading) {
            return true;
        }
        if self.log.as_ref().is_some_and(|log| log.loading) {
            return true;
        }
        if self.viewed_pipeline.values().any(|view| view.loading) {
            return true;
        }
        // Keep ticking while a visible job is still running so its status spinner animates.
        if self.active_jobs().iter().any(|job| job.status.is_active()) {
            return true;
        }
        if let Some(project_id) = self.selected_project_id {
            if !self.branches.has_cache(project_id) && self.branches.error(project_id).is_none() {
                return true;
            }
        }
        matches!(&self.overlay, Overlay::Variables(view) if view.loading)
            || matches!(&self.overlay, Overlay::Pipelines(view) if view.loading)
            || matches!(&self.overlay, Overlay::Tags(view) if view.loading)
    }

    /// Keeps selection valid: if the selected project is filtered out or gone, falls back to the
    /// first visible project. Also clamps the job cursor.
    pub(crate) fn ensure_selection(&mut self) {
        let visible = self.visible_indices();
        let selection_visible = self.selected_project_id.is_some_and(|id| {
            visible
                .iter()
                .any(|&index| self.items[index].configured.id == id)
        });
        if !selection_visible {
            self.selected_project_id = visible
                .first()
                .map(|&index| self.items[index].configured.id);
            self.selected_job = 0;
            self.reset_branch_view();
        }
        // The job cursor spans regular jobs followed by bridge rows.
        let row_count = self.active_jobs().len() + self.active_bridges().len();
        if self.selected_job >= row_count {
            self.selected_job = row_count.saturating_sub(1);
        }
        let branch_count = self.visible_branches().len();
        if self.selected_branch >= branch_count {
            self.selected_branch = branch_count.saturating_sub(1);
        }
    }

    pub(crate) fn select_next_project(&mut self) {
        self.move_project_selection(1);
    }

    pub(crate) fn select_previous_project(&mut self) {
        self.move_project_selection(-1);
    }

    fn move_project_selection(&mut self, delta: isize) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        let position = self.selected_visible_position().unwrap_or(0);
        let next = position.saturating_add_signed(delta).min(visible.len() - 1);
        let next_id = self.items[visible[next]].configured.id;
        if self.selected_project_id != Some(next_id) {
            self.selected_project_id = Some(next_id);
            self.selected_job = 0;
            self.reset_branch_view();
        }
    }

    /// Selects a project by id (if it's on the dashboard), clearing any active project filter so
    /// the row is actually visible. Used by the running-pipeline jump. Returns whether the id was
    /// found.
    pub(crate) fn select_project_by_id(&mut self, id: u64) -> bool {
        if !self.items.iter().any(|item| item.configured.id == id) {
            return false;
        }
        self.search.active = false;
        self.search.query.clear();
        if self.selected_project_id != Some(id) {
            self.selected_project_id = Some(id);
            self.selected_job = 0;
            self.reset_branch_view();
        }
        true
    }

    /// Applies a running-pipeline scan result. Each `(project_id, ref, pipeline_id)` is a project
    /// GitLab is actively building; pins every one onto its building branch (so its list row shows
    /// the running pipeline after a refresh), then selects the most recently started one and focuses
    /// its branch panel. Returns the project ids to re-fetch, or empty when nothing was running.
    pub(crate) fn apply_running_hits(&mut self, mut hits: Vec<(u64, String, u64)>) -> Vec<u64> {
        if hits.is_empty() {
            self.status_message = String::from("No running pipelines");
            return Vec::new();
        }
        // Newest pipeline first, so the jump lands on the branch the user most likely just pushed.
        hits.sort_by_key(|hit| std::cmp::Reverse(hit.2));
        for (project_id, ref_, _) in &hits {
            self.ref_overrides.insert(*project_id, ref_.clone());
        }
        let (target_id, target_ref, _) = &hits[0];
        let (target_id, target_ref) = (*target_id, target_ref.clone());
        self.select_project_by_id(target_id);
        self.focus = Focus::Branches;
        self.status_message = if hits.len() == 1 {
            format!("Running: {target_id} @ {target_ref}")
        } else {
            format!(
                "{} running · jumped to {target_id} @ {target_ref}",
                hits.len()
            )
        };
        hits.into_iter().map(|(id, _, _)| id).collect()
    }

    /// Resets the branches panel cursor and filter; called when the selected project changes so
    /// the panel doesn't keep a stale cursor into a different project's branch list.
    fn reset_branch_view(&mut self) {
        self.selected_branch = 0;
        self.branch_search.active = false;
        self.branch_search.query.clear();
    }

    pub(crate) fn select_next_job(&mut self) {
        let job_count = self.active_jobs().len() + self.active_bridges().len();
        if self.selected_job + 1 < job_count {
            self.selected_job += 1;
        }
    }

    pub(crate) fn select_previous_job(&mut self) {
        self.selected_job = self.selected_job.saturating_sub(1);
    }

    /// Scrolls the open job log by `step` lines, mirroring the j/k key handling: scrolling up
    /// also drops follow mode so the view stops snapping back to the tail. Works regardless of
    /// which panel currently has focus, so the mouse wheel can scroll the log from anywhere.
    pub(crate) fn scroll_log(&mut self, down: bool, step: usize) {
        if let Some(view) = &mut self.log {
            if down {
                view.scroll = (view.scroll + step).min(view.lines.len());
            } else {
                view.scroll = view.scroll.saturating_sub(step);
                view.follow = false;
            }
        }
    }

    pub(crate) fn selected_job_item(&self) -> Option<&Job> {
        self.active_jobs().get(self.selected_job)
    }

    /// The bridge row under the job cursor, if the cursor is past the regular jobs.
    pub(crate) fn selected_bridge_item(&self) -> Option<&Bridge> {
        self.active_bridges()
            .get(self.selected_job.checked_sub(self.active_jobs().len())?)
    }

    /// Project owning the pipeline displayed in the detail panel.
    pub(crate) fn active_pipeline_project_id(&self) -> Option<u64> {
        let item = self.selected_item()?;
        Some(
            self.viewed_pipeline
                .get(&item.configured.id)
                .map_or(item.configured.id, |view| view.project_id),
        )
    }

    /// Pipeline shown in the detail panel: the inspected historical one if any, else the latest.
    pub(crate) fn active_pipeline(&self) -> Option<&Pipeline> {
        let item = self.selected_item()?;
        self.viewed_pipeline
            .get(&item.configured.id)
            .map(|view| &view.pipeline)
            .or(item.pipeline.as_ref())
    }

    /// Jobs shown in the detail panel: the inspected pipeline's jobs if any, else the latest's.
    pub(crate) fn active_jobs(&self) -> &[Job] {
        match self.selected_item() {
            Some(item) => self
                .viewed_pipeline
                .get(&item.configured.id)
                .map_or(item.jobs.as_slice(), |view| view.jobs.as_slice()),
            None => &[],
        }
    }

    /// Bridge (trigger) jobs shown under the regular jobs in the detail panel.
    pub(crate) fn active_bridges(&self) -> &[Bridge] {
        match self.selected_item() {
            Some(item) => self
                .viewed_pipeline
                .get(&item.configured.id)
                .map_or(item.bridges.as_slice(), |view| view.bridges.as_slice()),
            None => &[],
        }
    }

    /// URL to open in a browser for the current context: the viewed job's page when a log is open,
    /// else the detail panel's pipeline, else the selected project. `None` when nothing applies.
    pub(crate) fn browse_url(&self) -> Option<String> {
        let project_url = || {
            self.selected_item()
                .and_then(|i| i.project.as_ref())
                .and_then(|p| p.web_url.clone())
        };
        if let Some(log) = self.log.as_ref() {
            if let Some(url) = self
                .active_jobs()
                .iter()
                .find(|j| j.id == log.job_id)
                .and_then(|j| j.web_url.clone())
            {
                return Some(url);
            }
            return self
                .active_pipeline()
                .and_then(|p| p.web_url.clone())
                .or_else(project_url);
        }
        match self.focus {
            // The MR panel opens the selected MR's page.
            Focus::MergeRequests => self
                .mr_panel
                .selected_mr()
                .and_then(|mr| mr.web_url.clone())
                .or_else(project_url),
            // A selected bridge row opens its downstream pipeline's page.
            Focus::Detail => self
                .selected_bridge_item()
                .and_then(|bridge| bridge.downstream_pipeline.as_ref())
                .and_then(|downstream| downstream.web_url.clone())
                .or_else(|| self.active_pipeline().and_then(|p| p.web_url.clone()))
                .or_else(project_url),
            _ => project_url(),
        }
    }

    /// The inspected-pipeline view for the selected project, if the detail panel is showing history.
    pub(crate) fn viewed_pipeline_for_selected(&self) -> Option<&ViewedPipeline> {
        let item = self.selected_item()?;
        self.viewed_pipeline.get(&item.configured.id)
    }

    /// True when the detail panel is showing a non-latest pipeline for the selected project.
    pub(crate) fn is_viewing_history(&self) -> bool {
        self.viewed_pipeline_for_selected().is_some()
    }

    /// Stops inspecting history for the selected project; returns true if anything changed.
    pub(crate) fn clear_viewed_pipeline(&mut self) -> bool {
        let Some(id) = self.selected_project_id else {
            return false;
        };
        let removed = self.viewed_pipeline.remove(&id).is_some();
        if removed {
            self.selected_job = 0;
        }
        removed
    }

    /// The branch the dashboard tracks for `project_id`: an explicit override, or the repo default.
    pub(crate) fn effective_ref(&self, project_id: u64) -> Option<String> {
        if let Some(branch) = self.ref_overrides.get(&project_id) {
            return Some(branch.clone());
        }
        self.items
            .iter()
            .find(|item| item.configured.id == project_id)
            .and_then(|item| item.project.as_ref())
            .and_then(|project| project.default_branch.clone())
    }

    /// Branches of the selected project that pass the branch filter, in display order.
    pub(crate) fn visible_branches(&self) -> Vec<&Branch> {
        let Some(project_id) = self.selected_project_id else {
            return Vec::new();
        };
        let Some(branches) = self.branches.cached(project_id) else {
            return Vec::new();
        };
        branches
            .iter()
            .filter(|branch| self.branch_search.matches(&branch.name))
            .collect()
    }

    /// The branch under the cursor within the filtered branch list, if any.
    pub(crate) fn selected_branch_item(&self) -> Option<&Branch> {
        self.visible_branches()
            .into_iter()
            .nth(self.selected_branch)
    }

    pub(crate) fn select_next_branch(&mut self) {
        let count = self.visible_branches().len();
        if self.selected_branch + 1 < count {
            self.selected_branch += 1;
        }
    }

    pub(crate) fn select_previous_branch(&mut self) {
        self.selected_branch = self.selected_branch.saturating_sub(1);
    }

    /// Selects the project at visible position `pos` (a mouse click), resetting the job and branch
    /// cursors just like keyboard movement does. Out-of-range positions are ignored.
    pub(crate) fn select_project_at(&mut self, pos: usize) {
        let visible = self.visible_indices();
        let Some(&index) = visible.get(pos) else {
            return;
        };
        let id = self.items[index].configured.id;
        if self.selected_project_id != Some(id) {
            self.selected_project_id = Some(id);
            self.selected_job = 0;
            self.reset_branch_view();
        }
    }

    pub(crate) fn select_branch_at(&mut self, index: usize) {
        if index < self.visible_branches().len() {
            self.selected_branch = index;
        }
    }

    pub(crate) fn select_job_at(&mut self, index: usize) {
        if index < self.active_jobs().len() {
            self.selected_job = index;
        }
    }

    /// Pins the selected project to the branch under the cursor; selecting the repo default clears
    /// the override (back to "follow default"). Returns the project id when a refresh is warranted.
    pub(crate) fn pin_selected_branch(&mut self) -> Option<u64> {
        let project_id = self.selected_project_id?;
        let branch = self.selected_branch_item()?;
        let name = branch.name.clone();
        let is_default = branch.default;
        let changed = if is_default {
            self.ref_overrides.remove(&project_id).is_some()
        } else {
            self.ref_overrides.insert(project_id, name.clone()) != Some(name.clone())
        };
        if changed {
            // A different tracked ref means a different latest pipeline; drop any history view.
            self.viewed_pipeline.remove(&project_id);
            self.status_message = if is_default {
                format!("Tracking default branch for project {project_id}")
            } else {
                format!("Tracking {name}")
            };
            Some(project_id)
        } else {
            None
        }
    }

    /// Drops the branch override for the selected project. Returns the project id if one was set.
    pub(crate) fn unpin_selected_project(&mut self) -> Option<u64> {
        let project_id = self.selected_project_id?;
        if self.ref_overrides.remove(&project_id).is_some() {
            self.viewed_pipeline.remove(&project_id);
            self.status_message = format!("Tracking default branch for project {project_id}");
            Some(project_id)
        } else {
            None
        }
    }

    /// The selected project whose branch list still needs fetching (not cached, loading or failed).
    pub(crate) fn branches_fetch_needed(&self) -> Option<u64> {
        let project_id = self.selected_project_id?;
        self.branches.is_idle(project_id).then_some(project_id)
    }

    pub(crate) fn refresh_sources(&self) -> &RefreshSources {
        &self.sources
    }

    /// Merges a single-project refresh (after a mutating action) into the dashboard. Returns a
    /// one-shot trace fetch to issue when the open log just (re)entered follow mode.
    pub(crate) fn apply_project_refresh(
        &mut self,
        snapshot: ProjectSnapshot,
    ) -> Option<TraceFetchRequest> {
        let project_id = snapshot.configured.id;
        self.pending_actions.remove(&project_id);
        // Drop any in-flight spinners for this project: the fresh snapshot reflects the action's
        // result, and a retry may have minted a new job id, so clear by project rather than item.
        self.in_flight.retain(|(pid, _)| *pid != project_id);
        // A fresh snapshot reflects the latest pipeline; stop showing a stale history view.
        self.viewed_pipeline.remove(&project_id);
        self.merge_snapshot(snapshot);
        self.sync_open_log_with_snapshot(project_id)
    }

    /// Like [`apply_project_refresh`](Self::apply_project_refresh) but for the background
    /// auto-refresh tick: preserves an open pipeline-history view and leaves `pending_actions`
    /// alone (the tick skips projects with a pending action), so it never disturbs the user.
    pub(crate) fn apply_project_auto_refresh(
        &mut self,
        snapshot: ProjectSnapshot,
    ) -> Option<TraceFetchRequest> {
        let project_id = snapshot.configured.id;
        self.merge_snapshot(snapshot);
        self.sync_open_log_with_snapshot(project_id)
    }

    /// Ids of projects the auto-refresh tick should poll: latest pipeline is in flight and no
    /// action-driven refresh is already pending for them.
    pub(crate) fn active_project_ids(&self) -> Vec<u64> {
        self.items
            .iter()
            .filter(|item| {
                item.pipeline
                    .as_ref()
                    .is_some_and(|pipeline| pipeline.status.is_active())
                    && !self.pending_actions.contains(&item.configured.id)
            })
            .map(|item| item.configured.id)
            .collect()
    }

    /// Elapsed seconds to display for a timed item. While `active` and anchored, returns the
    /// monotonic `now − anchor` (ticks 1/s, never jumps); otherwise the server value (terminal jobs,
    /// cached data with no anchor).
    pub(crate) fn timer_secs(
        &self,
        project_id: u64,
        id: u64,
        server_secs: u64,
        active: bool,
    ) -> u64 {
        if active {
            if let Some(anchor) = self.timer_anchor.get(&(project_id, id)) {
                return anchor.elapsed().as_secs();
            }
        }
        server_secs
    }

    /// Decides whether `snapshot`'s pipeline just transitioned into a terminal state, updating the
    /// per-project tracker. Returns the message to surface, or `None`. Seeds the tracker on the
    /// first sighting regardless of `notifications_enabled` (so toggling it on mid-session never
    /// fires for an already-finished pipeline), and never fires twice for the same transition.
    fn note_pipeline_transition(&mut self, snapshot: &ProjectSnapshot) -> Option<String> {
        let project_id = snapshot.configured.id;
        let pipeline = snapshot.pipeline.as_ref()?;
        let new_status = pipeline.status.clone();
        // Seed/update the tracker BEFORE the enabled check so a later toggle-on stays silent.
        let previous = self
            .last_notified_status
            .insert(project_id, new_status.clone());
        if !self.notifications_enabled {
            return None;
        }
        let terminal = matches!(
            new_status,
            CiStatus::Success | CiStatus::Failed | CiStatus::Canceled
        );
        match previous {
            Some(old) if old != new_status && terminal => Some(format!(
                "Pipeline #{} {} in {}",
                pipeline.iid.unwrap_or(pipeline.id),
                new_status,
                snapshot.configured.name
            )),
            // First sighting and unchanged/nonterminal statuses never notify.
            _ => None,
        }
    }

    /// Sets a stopwatch anchor for each active, already-started item in the snapshot (pipeline and
    /// jobs with a known `duration`) the first time it is seen. Never re-bases an existing anchor —
    /// that is what keeps the display from jumping on each refresh.
    fn sync_timer_anchors(&mut self, snapshot: &ProjectSnapshot) {
        let project_id = snapshot.configured.id;
        let now = Instant::now();
        let mut anchor = |id: u64, server_secs: u64| {
            self.timer_anchor
                .entry((project_id, id))
                .or_insert_with(|| {
                    now.checked_sub(Duration::from_secs(server_secs))
                        .unwrap_or(now)
                });
        };
        if let Some(pipeline) = &snapshot.pipeline {
            if pipeline.status.is_active() {
                if let Some(secs) = pipeline.duration {
                    anchor(pipeline.id, secs);
                }
            }
        }
        for job in &snapshot.jobs {
            if job.status.is_active() {
                if let Some(secs) = job.duration {
                    anchor(job.id, job_duration_secs(secs));
                }
            }
        }
    }

    /// Replaces (or inserts) the snapshot, preserving last known data on a failed fetch.
    /// Keeps `items` name-sorted and the selection valid; callers manage history and actions.
    fn merge_snapshot(&mut self, snapshot: ProjectSnapshot) {
        let project_id = snapshot.configured.id;
        self.sync_timer_anchors(&snapshot);
        if let Some(message) = self.note_pipeline_transition(&snapshot) {
            self.pending_notifications.push(message);
        }
        match self
            .items
            .iter_mut()
            .find(|item| item.configured.id == project_id)
        {
            Some(existing) if snapshot.is_failed() => existing.status = snapshot.status,
            Some(existing) => *existing = snapshot,
            None => {
                self.items.push(snapshot);
                self.items
                    .sort_by(|left, right| left.configured.name.cmp(&right.configured.name));
            }
        }
        self.ensure_selection();
    }

    /// Keeps an open job log in sync with a freshly merged snapshot: updates the displayed status
    /// and re-evaluates follow. Returns a trace fetch to issue when follow needs (re)starting —
    /// flipping `follow` alone does not re-arm the trace-follow loop. The user is never yanked out
    /// of scrollback: follow is only re-enabled while parked at the tail.
    pub(crate) fn sync_open_log_with_snapshot(
        &mut self,
        project_id: u64,
    ) -> Option<TraceFetchRequest> {
        let view = self.log.as_ref()?;
        if view.project_id != project_id {
            return None;
        }
        let job_id = view.job_id;
        // Read the live status before borrowing `self.log` mutably.
        let status = self
            .viewed_pipeline
            .values()
            .filter(|pipeline| pipeline.project_id == project_id)
            .flat_map(|pipeline| &pipeline.jobs)
            .chain(
                self.items
                    .iter()
                    .filter(|item| item.configured.id == project_id)
                    .flat_map(|item| &item.jobs),
            )
            .find(|job| job.id == job_id)?
            .status
            .clone();

        let view = self.log.as_mut()?;
        view.job_status = status.to_string();
        let request = TraceFetchRequest {
            project_id,
            job_id,
            generation: view.generation,
        };
        if status.is_active() {
            let was_following = view.follow;
            if view.at_tail() {
                view.follow = true;
            }
            // Restart the loop only on an off->on transition.
            (!was_following && view.follow).then_some(request)
        } else {
            // Terminal: capture the final log once, then stop following.
            let needs_final = view.follow;
            view.follow = false;
            needs_final.then_some(request)
        }
    }

    /// Merges a fetched branch list into the cache, sorting default-first then alphabetically.
    pub(crate) fn apply_branches_loaded(
        &mut self,
        project_id: u64,
        result: Result<Vec<Branch>, String>,
    ) {
        match result {
            Ok(mut branches) => {
                branches.sort_by(|left, right| {
                    right
                        .default
                        .cmp(&left.default)
                        .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                });
                self.branches.store(project_id, branches);
                if self.selected_project_id == Some(project_id) {
                    let count = self.visible_branches().len();
                    if self.selected_branch >= count {
                        self.selected_branch = count.saturating_sub(1);
                    }
                }
            }
            Err(err) => self.branches.fail(project_id, err),
        }
    }

    pub(crate) fn is_showing_cached_data(&self) -> bool {
        self.cached_view.is_cached()
    }

    pub(crate) fn mark_refreshing(&mut self) {
        // Let a manual refresh retry branch lists that previously failed to load.
        self.branches.clear_errors();
        self.is_refreshing = true;
        self.status_message = if self.is_showing_cached_data() {
            String::from("Refreshing cached data...")
        } else {
            String::from("Refreshing...")
        };
    }

    pub(crate) fn apply_live_refresh(
        &mut self,
        snapshots: Vec<ProjectSnapshot>,
        errors: usize,
        is_complete_failure: bool,
        group_errors: &[String],
    ) {
        for snapshot in &snapshots {
            self.sync_timer_anchors(snapshot);
        }
        self.items = snapshots;
        self.ensure_selection();
        self.is_refreshing = false;
        self.cached_view.set_live();
        let mut message = if is_complete_failure {
            format!("Refresh failed for all projects ({errors} error(s))")
        } else if errors == 0 {
            String::from("Loaded projects")
        } else {
            format!("Loaded with {errors} error(s)")
        };
        if !group_errors.is_empty() {
            message.push_str(&format!("; {} group(s) failed", group_errors.len()));
        }
        self.status_message = message;
    }

    pub(crate) fn apply_cached_refresh_failure(&mut self) {
        self.is_refreshing = false;
        self.status_message = String::from("Offline: showing cached data");
    }

    pub(crate) fn to_cached_state_ref(&self) -> CachedDashboardStateRef<'_> {
        CachedDashboardStateRef {
            instance_url: &self.instance_url,
            saved_at_unix_seconds: current_unix_timestamp(),
            snapshots: self
                .items
                .iter()
                .map(CachedProjectSnapshotRef::from_snapshot)
                .collect(),
            ref_overrides: &self.ref_overrides,
        }
    }
}

fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[derive(Debug, Serialize)]
pub(crate) struct CachedDashboardStateRef<'a> {
    instance_url: &'a str,
    saved_at_unix_seconds: u64,
    snapshots: Vec<CachedProjectSnapshotRef<'a>>,
    ref_overrides: &'a HashMap<u64, String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CachedProjectSnapshotRef<'a> {
    configured: &'a ProjectEntry,
    project: &'a Option<Project>,
    pipeline: &'a Option<Pipeline>,
    jobs: &'a [Job],
    bridges: &'a [Bridge],
    status: CachedProjectStatusRef<'a>,
    origin: SnapshotOrigin,
}

impl<'a> CachedProjectSnapshotRef<'a> {
    fn from_snapshot(snapshot: &'a ProjectSnapshot) -> Self {
        Self {
            configured: &snapshot.configured,
            project: &snapshot.project,
            pipeline: &snapshot.pipeline,
            jobs: &snapshot.jobs,
            bridges: &snapshot.bridges,
            status: CachedProjectStatusRef::from_status(&snapshot.status),
            origin: snapshot.origin,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) enum CachedProjectStatusRef<'a> {
    Ready,
    Failed(&'a str),
    PartialError(&'a str),
}

impl<'a> CachedProjectStatusRef<'a> {
    fn from_status(status: &'a ProjectStatus) -> Self {
        match status {
            ProjectStatus::Loading => Self::PartialError("loading"),
            ProjectStatus::Ready => Self::Ready,
            ProjectStatus::Failed(message) => Self::Failed(message),
            ProjectStatus::PartialError(message) => Self::PartialError(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use config::ProjectEntry;

    use crate::cache::{
        CachedDashboardState, CachedProjectSnapshot, CachedProjectStatus, SnapshotOrigin,
    };

    use super::{sanitize_trace, AppState, ProjectSnapshot, ProjectStatus, RefreshSources};

    #[test]
    fn sanitize_trace_strips_ansi_section_markers_and_carriage_returns() {
        let raw = concat!(
            "section_start:1700000000:prepare\r\u{1b}[0K\u{1b}[32;1mRunning with gitlab-runner\u{1b}[0;m\n",
            "\u{1b}[0Kdownloading 50%\rdownloading 100%\n",
            "section_end:1700000000:prepare\r\u{1b}[0K\n",
            "plain line\n",
        );

        let lines = sanitize_trace(raw);

        assert_eq!(
            lines,
            vec![
                String::from("Running with gitlab-runner"),
                String::from("downloading 100%"),
                String::from("plain line"),
            ]
        );
    }

    #[test]
    fn sanitize_trace_strips_runner_headers_but_keeps_timestamps_and_content() {
        let raw = concat!(
            "2026-10-18T22:34:54.933928Z 00O \u{1b}[32mRunning with gitlab-runner\u{1b}[0m\n",
            "2026-10-18T22:34:55Z 01E ошибка: build failed\n",
            "2026-10-18T22:34:55.1Z aFO+  indented continuation\n",
            "2026-10-18T22:34:55.123456789Z 00O + literal plus, 00O stays in output\n",
        );

        assert_eq!(
            sanitize_trace(raw),
            vec![
                "2026-10-18T22:34:54.933928Z Running with gitlab-runner",
                "2026-10-18T22:34:55Z ошибка: build failed",
                "2026-10-18T22:34:55.1Z   indented continuation",
                "2026-10-18T22:34:55.123456789Z + literal plus, 00O stays in output",
            ]
        );
    }

    #[test]
    fn sanitize_trace_removes_timestamped_sections_and_collapses_progress() {
        let raw = concat!(
            "2026-10-18T22:34:54.934001Z 00O section_start:1792362894:prepare_executor\r\u{1b}[0K\n",
            "2026-10-18T22:34:54.934071Z 00O+\u{1b}[0KPreparing the shell executor\n",
            "2026-10-18T22:34:54.934208Z 00O section_end:1792362894:prepare_executor\r\u{1b}[0K\n",
            "2026-10-18T22:34:55Z 00O section_start:1792362895:script\r\u{1b}[0K$ cargo test\n",
            "2026-10-18T22:34:56Z 01O downloading 50%\rdownloading 100%\n",
        );

        assert_eq!(
            sanitize_trace(raw),
            vec![
                "2026-10-18T22:34:54.934071Z Preparing the shell executor",
                "2026-10-18T22:34:55Z $ cargo test",
                "2026-10-18T22:34:56Z downloading 100%",
            ]
        );
    }

    #[test]
    fn sanitize_trace_preserves_unframed_output_and_invalid_runner_headers() {
        let lines = [
            "00O ordinary output",
            "prefix 2026-10-18T22:34:55Z 00O literal header",
            "2026-10-18T22:34:55Z ordinary output",
            "2026-10-18T22:34:55Z 00O",
            "2026-10-18T22:34:55Z 0GO invalid stream number",
            "2026-10-18T22:34:55Z 00X unknown stream type",
            "2026-10-18T22:34:55Z 00O? unknown line type",
            "2026-10-18T22:34:55.Z 00O invalid fraction",
            "2026-10-18T22:34:55.1234567890Z 00O excessive precision",
            "2026-10-18T22:34:55.éZ 00O unicode fraction",
            "2026-10-18T22:34:55Z 🦀 keep UTF-8",
            "2026/10/18T22:34:55Z 00O invalid date format",
        ];

        assert_eq!(sanitize_trace(&lines.join("\n")), lines);
    }

    fn entry(name: &str, id: u64) -> ProjectEntry {
        ProjectEntry {
            name: name.into(),
            id,
        }
    }

    fn static_sources(projects: Vec<ProjectEntry>) -> RefreshSources {
        RefreshSources {
            projects,
            groups: Vec::new(),
        }
    }

    fn cached_snapshot(name: &str, id: u64, origin: SnapshotOrigin) -> CachedProjectSnapshot {
        CachedProjectSnapshot {
            bridges: Vec::new(),
            configured: entry(name, id),
            project: None,
            pipeline: None,
            jobs: Vec::new(),
            status: CachedProjectStatus::Ready,
            origin,
        }
    }

    #[test]
    fn project_snapshot_reports_loading_only_for_loading_status() {
        let snapshot = ProjectSnapshot {
            bridges: Vec::new(),
            configured: entry("group/api", 1),
            project: None,
            pipeline: None,
            jobs: Vec::new(),
            status: ProjectStatus::Loading,
            origin: SnapshotOrigin::Static,
        };

        assert!(snapshot.is_loading());
        assert!(!snapshot.has_error());
        assert_eq!(snapshot.error_message(), None);
    }

    #[test]
    fn project_snapshot_reports_error_message_for_failed_status() {
        let snapshot = ProjectSnapshot {
            bridges: Vec::new(),
            configured: entry("group/api", 1),
            project: None,
            pipeline: None,
            jobs: Vec::new(),
            status: ProjectStatus::Failed(String::from("boom")),
            origin: SnapshotOrigin::Static,
        };

        assert!(!snapshot.is_loading());
        assert!(snapshot.has_error());
        assert_eq!(snapshot.error_message(), Some("boom"));
    }

    #[test]
    fn app_state_mark_refreshing_updates_status_message() {
        let mut state = AppState::new(static_sources(vec![entry("group/api", 1)]));

        state.mark_refreshing();

        assert!(state.is_refreshing);
        assert_eq!(state.status_message, "Refreshing...");
    }

    #[test]
    fn app_state_from_cached_marks_data_as_cached() {
        let state = AppState::from_cached(
            static_sources(vec![entry("group/api", 1)]),
            CachedDashboardState {
                instance_url: "https://git.example".into(),
                saved_at_unix_seconds: 99,
                ref_overrides: HashMap::new(),
                snapshots: vec![cached_snapshot("group/api", 1, SnapshotOrigin::Static)],
            },
        );

        assert!(state.is_showing_cached_data());
        assert!(state.is_refreshing);
        assert!(state.cached_view.is_cached());
        assert_eq!(state.cached_view.saved_at(), Some(99));
    }

    #[test]
    fn app_state_from_cached_filters_removed_projects_and_adds_new_ones() {
        let state = AppState::from_cached(
            static_sources(vec![entry("group/api", 1), entry("group/web", 2)]),
            CachedDashboardState {
                instance_url: "https://git.example".into(),
                saved_at_unix_seconds: 99,
                ref_overrides: HashMap::new(),
                snapshots: vec![
                    cached_snapshot("old/group/api", 1, SnapshotOrigin::Static),
                    cached_snapshot("group/removed", 3, SnapshotOrigin::Static),
                ],
            },
        );

        assert_eq!(state.items.len(), 2);
        assert_eq!(state.items[0].configured.name, "group/api");
        assert!(matches!(state.items[0].status, ProjectStatus::Ready));
        assert_eq!(state.items[1].configured.name, "group/web");
        assert!(matches!(state.items[1].status, ProjectStatus::Loading));
    }

    #[test]
    fn app_state_from_cached_retains_group_resolved_snapshots() {
        let state = AppState::from_cached(
            static_sources(vec![entry("group/api", 1)]),
            CachedDashboardState {
                instance_url: "https://git.example".into(),
                saved_at_unix_seconds: 99,
                ref_overrides: HashMap::new(),
                snapshots: vec![
                    cached_snapshot("group/api", 1, SnapshotOrigin::Static),
                    cached_snapshot("group/from-group", 5, SnapshotOrigin::Group),
                ],
            },
        );

        assert_eq!(state.items.len(), 2);
        assert_eq!(state.items[0].configured.name, "group/api");
        assert_eq!(state.items[1].configured.name, "group/from-group");
        assert_eq!(state.items[1].origin, SnapshotOrigin::Group);
        assert!(matches!(state.items[1].status, ProjectStatus::Ready));
    }

    #[test]
    fn apply_live_refresh_replaces_cached_state_with_live_data() {
        let mut state = AppState::from_cached(
            static_sources(vec![entry("group/api", 1)]),
            CachedDashboardState {
                instance_url: "https://git.example".into(),
                saved_at_unix_seconds: 99,
                ref_overrides: HashMap::new(),
                snapshots: vec![cached_snapshot("group/api", 1, SnapshotOrigin::Static)],
            },
        );

        state.apply_live_refresh(
            vec![ProjectSnapshot {
                bridges: Vec::new(),
                configured: entry("group/api", 1),
                project: None,
                pipeline: None,
                jobs: Vec::new(),
                status: ProjectStatus::Ready,
                origin: SnapshotOrigin::Static,
            }],
            0,
            false,
            &[],
        );

        assert!(!state.is_showing_cached_data());
        assert!(!state.is_refreshing);
        assert_eq!(state.status_message, "Loaded projects");
        assert_eq!(state.cached_view.saved_at(), None);
    }

    #[test]
    fn apply_live_refresh_reports_group_errors_in_status_message() {
        let mut state = AppState::new(static_sources(vec![entry("group/api", 1)]));

        state.apply_live_refresh(
            vec![ProjectSnapshot {
                bridges: Vec::new(),
                configured: entry("group/api", 1),
                project: None,
                pipeline: None,
                jobs: Vec::new(),
                status: ProjectStatus::Ready,
                origin: SnapshotOrigin::Static,
            }],
            0,
            false,
            &[String::from("my-org: request failed")],
        );

        assert_eq!(state.status_message, "Loaded projects; 1 group(s) failed");
    }

    #[test]
    fn apply_project_refresh_replaces_snapshot_and_clears_pending_action() {
        let mut state = AppState::new(static_sources(vec![entry("group/api", 1)]));
        state.pending_actions.insert(1);

        state.apply_project_refresh(ProjectSnapshot {
            bridges: Vec::new(),
            configured: entry("group/api", 1),
            project: None,
            pipeline: None,
            jobs: Vec::new(),
            status: ProjectStatus::Ready,
            origin: SnapshotOrigin::Static,
        });

        assert!(state.pending_actions.is_empty());
        assert_eq!(state.items.len(), 1);
        assert!(matches!(state.items[0].status, ProjectStatus::Ready));
    }

    #[test]
    fn applying_a_project_refresh_clears_its_in_flight_items() {
        let mut state = AppState::new(static_sources(vec![entry("group/api", 1)]));
        state.mark_in_flight(1, 99);
        assert!(state.is_in_flight(1, 99));
        assert!(state.project_has_in_flight(1));

        state.apply_project_refresh(ProjectSnapshot {
            bridges: Vec::new(),
            configured: entry("group/api", 1),
            project: None,
            pipeline: None,
            jobs: Vec::new(),
            status: ProjectStatus::Ready,
            origin: SnapshotOrigin::Static,
        });

        // A retry can mint a new job id, so the snapshot replace evicts stale spinners by project.
        assert!(!state.is_in_flight(1, 99));
        assert!(!state.project_has_in_flight(1));
    }

    /// A ready snapshot for project `id` carrying a pipeline of the given status.
    fn snapshot_with_status(id: u64, status: &str) -> ProjectSnapshot {
        ProjectSnapshot {
            bridges: Vec::new(),
            configured: entry("group/api", id),
            project: None,
            pipeline: Some(pipeline(1, status)),
            jobs: Vec::new(),
            status: ProjectStatus::Ready,
            origin: SnapshotOrigin::Static,
        }
    }

    #[test]
    fn failed_project_refresh_preserves_last_known_data_until_recovery() {
        let mut state = AppState::new(static_sources(vec![entry("group/api", 1)]));
        let mut live = snapshot_with_status(1, "running");
        live.jobs.push(job(9, "build", "running"));
        state.apply_project_auto_refresh(live);

        let mut failed = ProjectSnapshot::loading(entry("group/api", 1));
        failed.status = ProjectStatus::Failed(String::from("offline"));
        state.apply_project_auto_refresh(failed.clone());

        assert_eq!(state.items[0].pipeline.as_ref().unwrap().id, 1);
        assert_eq!(state.items[0].jobs[0].id, 9);
        assert_eq!(state.items[0].error_message(), Some("offline"));
        assert_eq!(state.active_project_ids(), vec![1]);
        assert!(state.to_cached_state_ref().snapshots[0].pipeline.is_some());

        state.pending_actions.insert(1);
        state.apply_project_refresh(failed);
        assert!(state.pending_actions.is_empty());
        assert_eq!(state.items[0].jobs[0].id, 9);

        state.apply_project_auto_refresh(snapshot_with_status(1, "success"));
        assert_eq!(
            state.items[0].pipeline.as_ref().unwrap().status.as_str(),
            "success"
        );
        assert!(!state.items[0].has_error());
        assert!(state.items[0].jobs.is_empty());
    }

    #[test]
    fn first_snapshot_seeds_status_without_notifying() {
        let mut state = AppState::new(static_sources(vec![entry("group/api", 1)]));
        state.notifications_enabled = true;
        assert_eq!(
            state.note_pipeline_transition(&snapshot_with_status(1, "failed")),
            None
        );
    }

    #[test]
    fn transition_into_terminal_state_notifies_once() {
        let mut state = AppState::new(static_sources(vec![entry("group/api", 1)]));
        state.notifications_enabled = true;
        // Seed with a running pipeline: silent.
        assert_eq!(
            state.note_pipeline_transition(&snapshot_with_status(1, "running")),
            None
        );
        // running -> failed fires once.
        let message = state.note_pipeline_transition(&snapshot_with_status(1, "failed"));
        assert!(message.is_some_and(|m| m.contains("failed")));
        // Same terminal status again: no repeat.
        assert_eq!(
            state.note_pipeline_transition(&snapshot_with_status(1, "failed")),
            None
        );
    }

    #[test]
    fn disabled_notifications_still_seed_so_enabling_later_is_silent() {
        let mut state = AppState::new(static_sources(vec![entry("group/api", 1)]));
        state.notifications_enabled = false;
        // Seeds 'failed' while disabled.
        assert_eq!(
            state.note_pipeline_transition(&snapshot_with_status(1, "failed")),
            None
        );
        // Toggle on, same terminal status: must NOT fire.
        state.notifications_enabled = true;
        assert_eq!(
            state.note_pipeline_transition(&snapshot_with_status(1, "failed")),
            None
        );
    }

    fn pipeline(id: u64, status: &str) -> gitlab::Pipeline {
        gitlab::Pipeline {
            id,
            iid: Some(id),
            status: status.into(),
            ref_: Some("main".into()),
            web_url: None,
            duration: None,
            created_at: None,
            updated_at: None,
            sha: None,
        }
    }

    fn job(id: u64, name: &str, status: &str) -> gitlab::Job {
        gitlab::Job {
            artifacts_file: None,
            id,
            name: name.into(),
            status: status.into(),
            stage: None,
            duration: None,
            web_url: None,
        }
    }

    /// One ready project carrying a latest pipeline and jobs, selected.
    fn state_with_latest(latest_jobs: Vec<gitlab::Job>) -> AppState {
        let mut state = AppState::new(static_sources(vec![entry("group/api", 1)]));
        state.apply_project_refresh(ProjectSnapshot {
            bridges: Vec::new(),
            configured: entry("group/api", 1),
            project: None,
            pipeline: Some(pipeline(42, "success")),
            jobs: latest_jobs,
            status: ProjectStatus::Ready,
            origin: SnapshotOrigin::Static,
        });
        state
    }

    #[test]
    fn active_pipeline_and_jobs_prefer_the_viewed_history_pipeline() {
        let mut state = state_with_latest(vec![job(10, "build", "success")]);
        // No history view yet: the accessors return the snapshot's latest.
        assert_eq!(state.active_pipeline().map(|p| p.id), Some(42));
        assert_eq!(state.active_jobs().len(), 1);

        state.viewed_pipeline.insert(
            1,
            super::ViewedPipeline {
                project_id: 1,
                bridges: Vec::new(),
                pipeline: pipeline(7, "failed"),
                jobs: vec![
                    job(99, "old-build", "failed"),
                    job(100, "old-test", "failed"),
                ],
                loading: false,
                error: None,
            },
        );
        assert!(state.is_viewing_history());
        assert_eq!(state.active_pipeline().map(|p| p.id), Some(7));
        assert_eq!(state.active_jobs().len(), 2);
        assert_eq!(state.selected_job_item().map(|j| j.id), Some(99));
    }

    #[test]
    fn browse_url_targets_the_focused_context() {
        let mut state = AppState::new(static_sources(vec![entry("group/api", 1)]));
        state.apply_project_refresh(ProjectSnapshot {
            bridges: Vec::new(),
            configured: entry("group/api", 1),
            project: Some(gitlab::Project {
                id: 1,
                name: "api".into(),
                path_with_namespace: Some("group/api".into()),
                default_branch: Some("main".into()),
                web_url: Some("https://gl/group/api".into()),
            }),
            pipeline: Some(gitlab::Pipeline {
                web_url: Some("https://gl/group/api/-/pipelines/42".into()),
                ..pipeline(42, "success")
            }),
            jobs: vec![gitlab::Job {
                artifacts_file: None,
                web_url: Some("https://gl/group/api/-/jobs/10".into()),
                ..job(10, "build", "failed")
            }],
            status: ProjectStatus::Ready,
            origin: SnapshotOrigin::Static,
        });

        // Projects/Branches focus → the project page.
        state.focus = super::Focus::Projects;
        assert_eq!(state.browse_url().as_deref(), Some("https://gl/group/api"));

        // Detail focus → the pipeline page.
        state.focus = super::Focus::Detail;
        assert_eq!(
            state.browse_url().as_deref(),
            Some("https://gl/group/api/-/pipelines/42")
        );

        // An open log → the viewed job's page, whatever the focus.
        state.log = Some(log_with_lines(1, 10)); // job_id 10
        assert_eq!(
            state.browse_url().as_deref(),
            Some("https://gl/group/api/-/jobs/10")
        );

        // Nothing to open when the project carries no web_url and there's no pipeline/job.
        state.log = None;
        state.focus = super::Focus::Projects;
        state.apply_project_refresh(ProjectSnapshot {
            bridges: Vec::new(),
            configured: entry("group/api", 1),
            project: None,
            pipeline: None,
            jobs: Vec::new(),
            status: ProjectStatus::Ready,
            origin: SnapshotOrigin::Static,
        });
        assert_eq!(state.browse_url(), None);
    }

    #[test]
    fn apply_project_refresh_drops_the_history_view() {
        let mut state = state_with_latest(vec![job(10, "build", "success")]);
        state.viewed_pipeline.insert(
            1,
            super::ViewedPipeline {
                project_id: 1,
                bridges: Vec::new(),
                pipeline: pipeline(7, "failed"),
                jobs: vec![job(99, "old", "failed")],
                loading: false,
                error: None,
            },
        );

        state.apply_project_refresh(ProjectSnapshot {
            bridges: Vec::new(),
            configured: entry("group/api", 1),
            project: None,
            pipeline: Some(pipeline(43, "running")),
            jobs: vec![job(11, "build", "running")],
            status: ProjectStatus::Ready,
            origin: SnapshotOrigin::Static,
        });

        assert!(!state.is_viewing_history());
        assert_eq!(state.active_pipeline().map(|p| p.id), Some(43));
    }

    #[test]
    fn apply_project_auto_refresh_preserves_the_history_view() {
        let mut state = state_with_latest(vec![job(10, "build", "running")]);
        state.viewed_pipeline.insert(
            1,
            super::ViewedPipeline {
                project_id: 1,
                bridges: Vec::new(),
                pipeline: pipeline(7, "failed"),
                jobs: vec![job(99, "old", "failed")],
                loading: false,
                error: None,
            },
        );

        state.apply_project_auto_refresh(ProjectSnapshot {
            bridges: Vec::new(),
            configured: entry("group/api", 1),
            project: None,
            pipeline: Some(pipeline(43, "running")),
            jobs: vec![job(11, "build", "running")],
            status: ProjectStatus::Ready,
            origin: SnapshotOrigin::Static,
        });

        // The background tick must not yank the user out of the history view.
        assert!(state.is_viewing_history());
        assert_eq!(state.active_pipeline().map(|p| p.id), Some(7));
    }

    #[test]
    fn apply_project_auto_refresh_preserves_job_selection() {
        let mut state = state_with_latest(vec![
            job(10, "build", "running"),
            job(11, "test", "created"),
            job(12, "deploy", "created"),
        ]);
        state.selected_job = 2;

        state.apply_project_auto_refresh(ProjectSnapshot {
            bridges: Vec::new(),
            configured: entry("group/api", 1),
            project: None,
            pipeline: Some(pipeline(42, "running")),
            jobs: vec![
                job(10, "build", "success"),
                job(11, "test", "running"),
                job(12, "deploy", "created"),
            ],
            status: ProjectStatus::Ready,
            origin: SnapshotOrigin::Static,
        });

        assert_eq!(state.selected_job, 2);
    }

    /// Builds a project (id 1) with one job and an open log viewer over that job.
    fn state_with_open_log(job_status: &str, log: super::LogViewState) -> AppState {
        let mut state = state_with_latest(vec![job(10, "build", job_status)]);
        state.log = Some(log);
        state
    }

    #[test]
    fn auto_refresh_reenables_follow_when_job_becomes_active_at_tail() {
        let mut view = log_with_lines(5, 10); // few lines + tall viewport => parked at tail
        view.follow = false;
        let mut state = state_with_open_log("created", view);

        let request = state.apply_project_auto_refresh(ProjectSnapshot {
            bridges: Vec::new(),
            configured: entry("group/api", 1),
            project: None,
            pipeline: Some(pipeline(42, "running")),
            jobs: vec![job(10, "build", "running")],
            status: ProjectStatus::Ready,
            origin: SnapshotOrigin::Static,
        });

        let log = state.log.as_ref().unwrap();
        assert!(
            log.follow,
            "follow should re-enable for an active job at the tail"
        );
        assert_eq!(log.job_status, "running");
        // The run loop needs a one-shot fetch to restart the trace-follow loop.
        assert_eq!(request.map(|r| (r.project_id, r.job_id)), Some((1, 10)));
    }

    #[test]
    fn auto_refresh_does_not_reenable_follow_when_scrolled_into_backlog() {
        let mut view = log_with_lines(100, 10); // 100 lines, viewport 10
        view.follow = false;
        view.scroll = 0; // scrolled to the top, far from the tail (tail starts at 90)
        let mut state = state_with_open_log("created", view);

        let request = state.apply_project_auto_refresh(ProjectSnapshot {
            bridges: Vec::new(),
            configured: entry("group/api", 1),
            project: None,
            pipeline: Some(pipeline(42, "running")),
            jobs: vec![job(10, "build", "running")],
            status: ProjectStatus::Ready,
            origin: SnapshotOrigin::Static,
        });

        let log = state.log.as_ref().unwrap();
        assert!(
            !log.follow,
            "a reader scrolled into the backlog must not be forced to follow"
        );
        assert_eq!(log.job_status, "running");
        assert_eq!(request, None);
    }

    #[test]
    fn auto_refresh_stops_follow_when_job_reaches_terminal_status() {
        let mut view = log_with_lines(5, 10);
        view.follow = true;
        let mut state = state_with_open_log("running", view);

        let request = state.apply_project_auto_refresh(ProjectSnapshot {
            bridges: Vec::new(),
            configured: entry("group/api", 1),
            project: None,
            pipeline: Some(pipeline(42, "success")),
            jobs: vec![job(10, "build", "success")],
            status: ProjectStatus::Ready,
            origin: SnapshotOrigin::Static,
        });

        let log = state.log.as_ref().unwrap();
        assert!(!log.follow, "follow should stop once the job finishes");
        assert_eq!(log.job_status, "success");
        // One final fetch captures the completed log.
        assert_eq!(request.map(|r| (r.project_id, r.job_id)), Some((1, 10)));
    }

    #[test]
    fn active_project_ids_lists_in_flight_projects_skipping_pending_actions() {
        let mut state = AppState::new(static_sources(vec![
            entry("group/api", 1),
            entry("group/web", 2),
            entry("group/cleanup", 3),
        ]));
        let snapshot = |id: u64, name: &str, status: &str| ProjectSnapshot {
            bridges: Vec::new(),
            configured: entry(name, id),
            project: None,
            pipeline: Some(pipeline(id, status)),
            jobs: vec![job(id, "build", status)],
            status: ProjectStatus::Ready,
            origin: SnapshotOrigin::Static,
        };
        state.apply_project_auto_refresh(snapshot(1, "group/api", "running"));
        state.apply_project_auto_refresh(snapshot(2, "group/web", "success"));
        state.apply_project_auto_refresh(snapshot(3, "group/cleanup", "canceling"));

        // Only the project whose latest pipeline is in flight is polled.
        assert_eq!(state.active_project_ids(), vec![1, 3]);

        // A project already awaiting an action-driven refresh is skipped to avoid double fetches.
        state.pending_actions.insert(1);
        assert_eq!(state.active_project_ids(), vec![3]);
    }

    #[test]
    fn timer_anchors_running_jobs_for_a_monotonic_stopwatch() {
        // A running job with a known duration gets an anchor and reads ≈server value right away
        // (elapsed ≈ 0, base = now − duration → ≈ duration), and at least the server value.
        let mut state = state_with_latest(vec![gitlab::Job {
            artifacts_file: None,
            duration: Some(6.0),
            ..job(10, "build", "running")
        }]);
        assert!(state.timer_anchor.contains_key(&(1, 10)));
        assert!(state.timer_secs(1, 10, 6, true) >= 6);

        // A finished job ignores any anchor and shows the server value.
        assert_eq!(state.timer_secs(1, 10, 42, false), 42);

        // A pending job with no duration is not anchored.
        state.apply_project_auto_refresh(ProjectSnapshot {
            bridges: Vec::new(),
            configured: entry("group/api", 1),
            project: None,
            pipeline: Some(pipeline(42, "running")),
            jobs: vec![job(11, "test", "pending")],
            status: ProjectStatus::Ready,
            origin: SnapshotOrigin::Static,
        });
        assert!(!state.timer_anchor.contains_key(&(1, 11)));

        // Cached/offline data has no anchors, so it shows the server value (never ticks).
        let cached = AppState::from_cached(
            static_sources(vec![entry("group/api", 1)]),
            CachedDashboardState {
                instance_url: "https://git.example".into(),
                saved_at_unix_seconds: 99,
                ref_overrides: HashMap::new(),
                snapshots: vec![cached_snapshot("group/api", 1, SnapshotOrigin::Static)],
            },
        );
        assert!(cached.timer_anchor.is_empty());
        assert_eq!(cached.timer_secs(1, 10, 6, true), 6);
    }

    #[test]
    fn apply_cached_refresh_failure_keeps_cached_items_and_updates_status() {
        let mut state = AppState::from_cached(
            static_sources(vec![entry("group/api", 1)]),
            CachedDashboardState {
                instance_url: "https://git.example".into(),
                saved_at_unix_seconds: 99,
                ref_overrides: HashMap::new(),
                snapshots: vec![cached_snapshot("group/api", 1, SnapshotOrigin::Static)],
            },
        );

        state.apply_cached_refresh_failure();

        assert_eq!(state.items.len(), 1);
        assert!(!state.is_refreshing);
        assert_eq!(state.status_message, "Offline: showing cached data");
    }

    fn branch(name: &str, default: bool) -> gitlab::Branch {
        gitlab::Branch {
            name: name.into(),
            default,
        }
    }

    #[test]
    fn apply_branches_loaded_sorts_default_first_then_alphabetically() {
        let mut state = AppState::new(static_sources(vec![entry("group/api", 1)]));

        state.apply_branches_loaded(
            1,
            Ok(vec![
                branch("zeta", false),
                branch("alpha", false),
                branch("main", true),
            ]),
        );

        let names: Vec<_> = state.branches.cache[&1]
            .iter()
            .map(|b| b.name.as_str())
            .collect();
        assert_eq!(names, vec!["main", "alpha", "zeta"]);
        assert!(!state.branches.loading.contains(&1));
    }

    #[test]
    fn apply_branches_loaded_records_error_to_suppress_retry_storm() {
        let mut state = AppState::new(static_sources(vec![entry("group/api", 1)]));
        state.branches.mark_loading(1);

        state.apply_branches_loaded(1, Err(String::from("boom")));

        assert!(state.branches.errors.contains_key(&1));
        assert!(!state.branches.loading.contains(&1));
        // A project with a recorded error is not re-fetched until the next manual refresh.
        assert_eq!(state.branches_fetch_needed(), None);
    }

    #[test]
    fn pin_selected_branch_sets_override_and_default_clears_it() {
        let mut state = AppState::new(static_sources(vec![entry("group/api", 1)]));
        state
            .branches
            .cache
            .insert(1, vec![branch("main", true), branch("feature/x", false)]);

        // Select the non-default branch and pin it.
        state.selected_branch = 1;
        assert_eq!(state.pin_selected_branch(), Some(1));
        assert_eq!(
            state.ref_overrides.get(&1).map(String::as_str),
            Some("feature/x")
        );
        // Pinning the same branch again is a no-op (no refresh requested).
        assert_eq!(state.pin_selected_branch(), None);

        // Selecting the default branch clears the override.
        state.selected_branch = 0;
        assert_eq!(state.pin_selected_branch(), Some(1));
        assert!(!state.ref_overrides.contains_key(&1));
    }

    #[test]
    fn unpin_selected_project_removes_override_only_when_present() {
        let mut state = AppState::new(static_sources(vec![entry("group/api", 1)]));
        assert_eq!(state.unpin_selected_project(), None);

        state.ref_overrides.insert(1, String::from("feature/x"));
        assert_eq!(state.unpin_selected_project(), Some(1));
        assert!(!state.ref_overrides.contains_key(&1));
    }

    #[test]
    fn apply_running_hits_pins_all_and_jumps_to_newest() {
        let mut state = AppState::new(static_sources(vec![
            entry("group/api", 1),
            entry("group/web", 2),
        ]));
        // Seed both projects onto the dashboard so the jump target exists.
        for id in [1, 2] {
            state.apply_project_refresh(ProjectSnapshot {
                bridges: Vec::new(),
                configured: entry("group/x", id),
                project: None,
                pipeline: None,
                jobs: Vec::new(),
                status: ProjectStatus::Ready,
                origin: SnapshotOrigin::Static,
            });
        }

        // Project 2's pipeline (id 200) is newer than project 1's (id 100).
        let refresh_ids = state.apply_running_hits(vec![
            (1, String::from("feature/a"), 100),
            (2, String::from("feature/b"), 200),
        ]);

        // Every building branch is pinned...
        assert_eq!(
            state.ref_overrides.get(&1).map(String::as_str),
            Some("feature/a")
        );
        assert_eq!(
            state.ref_overrides.get(&2).map(String::as_str),
            Some("feature/b")
        );
        // ...and focus jumps to the newest one's branch panel.
        assert_eq!(state.selected_project_id, Some(2));
        assert_eq!(state.focus, super::Focus::Branches);
        // Both projects are returned for a re-fetch.
        assert_eq!(refresh_ids.len(), 2);
        assert!(refresh_ids.contains(&1) && refresh_ids.contains(&2));
    }

    #[test]
    fn apply_running_hits_reports_when_nothing_running() {
        let mut state = AppState::new(static_sources(vec![entry("group/api", 1)]));
        assert!(state.apply_running_hits(Vec::new()).is_empty());
        assert_eq!(state.status_message, "No running pipelines");
    }

    #[test]
    fn effective_ref_prefers_override_then_default_branch() {
        let mut state = AppState::new(static_sources(vec![entry("group/api", 1)]));
        // No project metadata loaded yet and no override: nothing to track.
        assert_eq!(state.effective_ref(1), None);

        state.apply_project_refresh(ProjectSnapshot {
            bridges: Vec::new(),
            configured: entry("group/api", 1),
            project: Some(gitlab::Project {
                id: 1,
                name: String::from("api"),
                path_with_namespace: Some(String::from("group/api")),
                default_branch: Some(String::from("main")),
                web_url: None,
            }),
            pipeline: None,
            jobs: Vec::new(),
            status: ProjectStatus::Ready,
            origin: SnapshotOrigin::Static,
        });
        assert_eq!(state.effective_ref(1).as_deref(), Some("main"));

        state.ref_overrides.insert(1, String::from("feature/x"));
        assert_eq!(state.effective_ref(1).as_deref(), Some("feature/x"));
    }

    #[test]
    fn wrap_line_breaks_on_spaces_and_hard_breaks_long_tokens() {
        // Short line: untouched.
        assert_eq!(super::wrap_line("hi", 10), vec!["hi"]);
        // Word boundary: wraps without splitting a word.
        assert_eq!(super::wrap_line("aaa bbb ccc", 7), vec!["aaa ", "bbb ccc"]);
        // A token longer than the width is hard-split so it stays readable.
        assert_eq!(
            super::wrap_line("abcdefghij", 4),
            vec!["abcd", "efgh", "ij"]
        );
        // Every wrapped row fits the width.
        for row in super::wrap_line("the quick brown fox jumps over", 8) {
            assert!(row.chars().count() <= 8, "row over width: {row:?}");
        }
    }

    #[test]
    fn scroll_log_moves_offset_and_scrolling_up_disables_follow() {
        let mut state = AppState::new(static_sources(vec![entry("group/api", 1)]));
        state.log = Some(super::LogViewState {
            project_id: 1,
            job_id: 10,
            job_name: String::from("build"),
            job_status: String::from("running"),
            lines: (0..100).map(|n| n.to_string()).collect(),
            raw: (0..100).map(|n| n.to_string()).collect(),
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
            search: super::LogSearchState::default(),
        });

        // Scrolling down advances the offset but keeps follow until the user scrolls up.
        state.scroll_log(true, 3);
        assert_eq!(state.log.as_ref().unwrap().scroll, 3);
        assert!(state.log.as_ref().unwrap().follow);

        // Scrolling up reduces the offset and leaves follow mode.
        state.scroll_log(false, 1);
        assert_eq!(state.log.as_ref().unwrap().scroll, 2);
        assert!(!state.log.as_ref().unwrap().follow);

        // The offset never runs past the end of the buffer or below zero.
        state.scroll_log(true, 1000);
        assert_eq!(state.log.as_ref().unwrap().scroll, 100);
        state.scroll_log(false, 1000);
        assert_eq!(state.log.as_ref().unwrap().scroll, 0);
    }

    #[test]
    fn job_cursor_spans_jobs_and_bridges_and_clamps_to_their_sum() {
        let mut state = state_with_latest(vec![job(10, "build", "success")]);
        state.items[0].bridges = vec![gitlab::Bridge {
            id: 20,
            name: String::from("trigger-child"),
            status: "success".into(),
            stage: None,
            downstream_pipeline: None,
        }];

        // Moving down from the job must reach the bridge row.
        state.select_next_job();
        state.ensure_selection();
        assert_eq!(state.selected_job, 1);
        assert!(state.selected_job_item().is_none());
        assert_eq!(
            state.selected_bridge_item().map(|bridge| bridge.id),
            Some(20)
        );

        // Past the combined list it clamps to the last row.
        state.selected_job = 5;
        state.ensure_selection();
        assert_eq!(state.selected_job, 1);
    }

    #[test]
    fn log_search_finds_matches_case_insensitively_and_survives_rewrap() {
        let mut view = log_with_lines(0, 10);
        view.raw = vec![
            String::from("ERROR: boom"),
            String::from("ok"),
            String::from("error again"),
        ];
        view.lines = view.raw.clone();
        view.search.query = String::from("error");
        view.recompute_search();
        assert_eq!(view.search.matches, vec![0, 2]);

        // Re-wrapping to a narrow pane shifts line indices; matches must follow.
        view.set_wrap_width(6);
        assert!(!view.search.matches.is_empty());
        assert!(view
            .search
            .matches
            .iter()
            .all(|&i| view.lines[i].to_lowercase().contains("error")));
    }

    #[test]
    fn log_search_next_and_prev_wrap_around_and_disable_follow() {
        let mut view = log_with_lines(100, 10);
        view.follow = true;
        view.search.query = String::from("5");
        view.recompute_search();
        let matches = view.search.matches.clone();

        view.search_commit();
        assert_eq!(view.cursor, matches[0]);
        assert!(!view.follow, "jumping to a match must stop tailing");

        // Stepping past the last match wraps to the first; `N` from the first wraps to the last.
        view.cursor = *matches.last().unwrap();
        view.search_next();
        assert_eq!(view.cursor, matches[0]);
        view.search_prev();
        assert_eq!(view.cursor, *matches.last().unwrap());
    }

    #[test]
    fn log_clear_search_resets_everything() {
        let mut view = log_with_lines(10, 5);
        view.search.input = true;
        view.search.query = String::from("1");
        view.recompute_search();
        assert!(!view.search.matches.is_empty());

        view.clear_search();
        assert_eq!(view.search, super::LogSearchState::default());
    }

    fn log_with_lines(count: usize, viewport_height: usize) -> super::LogViewState {
        super::LogViewState {
            project_id: 1,
            job_id: 10,
            job_name: String::from("build"),
            job_status: String::from("running"),
            lines: (0..count).map(|n| n.to_string()).collect(),
            raw: (0..count).map(|n| n.to_string()).collect(),
            wrap_width: 0,
            scroll: 0,
            follow: false,
            loading: false,
            generation: 1,
            error: None,
            notice: None,
            cursor: 0,
            select_anchor: None,
            viewport_height,
            search: super::LogSearchState::default(),
        }
    }

    #[test]
    fn visual_selection_spans_anchor_to_cursor_in_either_direction() {
        let mut view = log_with_lines(100, 10);
        view.move_cursor_to(5);
        view.begin_visual();
        // The anchor lands on the current cursor line.
        assert_eq!(view.select_anchor, Some(5));
        assert_eq!(view.cursor, 5);

        // Extending downward selects the inclusive range and yields its text.
        view.move_cursor_to(8);
        assert_eq!(view.selection_range(), Some((5, 8)));
        assert_eq!(view.selection_len(), 4);
        assert_eq!(view.selected_text(), "5\n6\n7\n8");

        // Moving the cursor above the anchor flips the range without losing the anchor.
        view.move_cursor_to(2);
        assert_eq!(view.selection_range(), Some((2, 5)));
        assert_eq!(view.selected_text(), "2\n3\n4\n5");

        view.end_visual();
        assert_eq!(view.select_anchor, None);
        assert_eq!(view.selection_range(), None);
        assert_eq!(view.selection_len(), 0);
    }

    #[test]
    fn moving_the_cursor_scrolls_just_enough_to_keep_it_visible() {
        let mut view = log_with_lines(100, 10);
        view.begin_visual();

        // Within the first page the view does not move.
        view.move_cursor_to(9);
        assert_eq!(view.scroll, 0);

        // Stepping past the bottom edge scrolls one line so the cursor stays on screen.
        view.move_cursor_to(10);
        assert_eq!(view.scroll, 1);
        assert_eq!(view.cursor, 10);

        // Jumping to the end pins the cursor to the last visible row.
        view.move_cursor_to(1000);
        assert_eq!(view.cursor, 99);
        assert_eq!(view.scroll, 90);

        // Scrolling back above the viewport pulls the offset up to the cursor.
        view.move_cursor_to(3);
        assert_eq!(view.scroll, 3);
    }
}
