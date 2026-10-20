//! Terminal dashboard runtime for rendering GitLab project status.

use std::collections::HashMap;
use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as CEvent, MouseButton, MouseEvent,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use gitlab::GitLabClient;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Position, Rect};
use ratatui::Terminal;
use storage::LocalStateStore;
use tokio::sync::mpsc;
use tracing::{error, warn};

mod actions;
mod cache;
mod input;
mod onboarding;
mod picker;
mod refresh;
mod render;
mod state;

pub use cache::{dashboard_cache_path, DASHBOARD_CACHE_FILE};
pub use picker::run_project_picker;

use actions::{
    spawn_action, spawn_artifacts_download, spawn_branches_fetch, spawn_environments_fetch,
    spawn_merge_requests_fetch, spawn_mr_head_pipeline_fetch, spawn_pipeline_jobs_fetch,
    spawn_pipelines_fetch, spawn_running_scan, spawn_tag_create, spawn_tags_fetch,
    spawn_trace_fetch, spawn_variables_fetch, ActionReport, VariablesError,
};
use input::Command;
pub use onboarding::run_onboarding;
pub use refresh::RefreshSources;
use refresh::{spawn_refresh, spawn_refresh_one, spawn_refresh_one_auto, RefreshReport};
use render::render;
use state::{AppState, Focus, Overlay, TraceFetchRequest};

/// One configured GitLab instance, ready for the dashboard: an authenticated client and the
/// projects and groups it tracks. The app builds one per `[[instance]]` in the config.
pub struct InstanceRuntime {
    pub url: String,
    pub client: GitLabClient,
    pub sources: RefreshSources,
}

/// Per-instance runtime backing one tab: its url, client, sources, and offline cache store.
struct InstanceMeta {
    url: String,
    client: GitLabClient,
    sources: RefreshSources,
    store: LocalStateStore,
}

/// Why the run loop returned: the user quit, or asked to add an instance (which suspends the
/// dashboard to run the setup wizard, then resumes).
enum Outcome {
    Quit,
    AddInstance,
}

/// Holds every configured instance and the single active [`AppState`]. Only the active tab has live
/// state; switching persists the current tab and loads the target's cached state. Async results are
/// tagged with a session generation so returning to a tab cannot accept its old results.
struct PendingRefresh {
    generation: u64,
    clears_pending: bool,
    task: Option<tokio::task::AbortHandle>,
}

struct Dashboard {
    metas: Vec<InstanceMeta>,
    labels: Vec<String>,
    active: usize,
    instance_generation: usize,
    request_generation: u64,
    completed_full_generation: u64,
    // Pipeline id 0 denotes the latest snapshot; other ids denote a viewed pipeline.
    refreshes: HashMap<(u64, u64), PendingRefresh>,
    full_refresh: Option<(u64, tokio::task::AbortHandle)>,
    state: AppState,
    /// Set when refresh events mutate `state`; the 1s UI tick flushes one write instead of one
    /// per project per refresh cycle.
    state_dirty: bool,
    /// Global pipeline-completion notification preference, applied to each tab's state.
    notifications_enabled: bool,
    /// True while a fast spinner-redraw tick chain is alive, so we never start a second one.
    spinner_running: bool,
}

impl Dashboard {
    fn new(instances: Vec<InstanceRuntime>, notifications_enabled: bool) -> Result<Self> {
        debug_assert!(
            !instances.is_empty(),
            "app guarantees at least one instance"
        );
        let labels: Vec<String> = instances
            .iter()
            .map(|inst| cache::instance_label(&inst.url))
            .collect();
        let mut metas = Vec::with_capacity(instances.len());
        for inst in instances {
            let path = cache::instance_cache_path(&inst.url)
                .context("failed to resolve dashboard cache path")?;
            metas.push(InstanceMeta {
                url: inst.url,
                client: inst.client,
                sources: inst.sources,
                store: LocalStateStore::from_path(path),
            });
        }
        let active = 0;
        let state = load_state_for(&metas[active], &labels, active, notifications_enabled);
        Ok(Self {
            metas,
            labels,
            active,
            instance_generation: 0,
            request_generation: 0,
            completed_full_generation: 0,
            refreshes: HashMap::new(),
            full_refresh: None,
            state,
            state_dirty: false,
            notifications_enabled,
            spinner_running: false,
        })
    }

    fn next_request(&mut self) -> u64 {
        self.request_generation += 1;
        self.request_generation
    }

    fn start_project_request(&mut self, key: (u64, u64)) -> u64 {
        if key.1 != 0 {
            self.refreshes.retain(|&(project, pipeline), request| {
                let keep = project != key.0 || pipeline == 0 || pipeline == key.1;
                if !keep { if let Some(task) = &request.task { task.abort(); } }
                keep
            });
        }
        let generation = self.next_request();
        if let Some(old) = self.refreshes.insert(key, PendingRefresh { generation, clears_pending: false, task: None }) {
            if let Some(task) = old.task { task.abort(); }
        }
        generation
    }

    fn finish_project_request(&mut self, key: (u64, u64), generation: u64) -> bool {
        let Some(request) = self.refreshes.get_mut(&key) else { return false; };
        if request.generation != generation || request.task.take().is_none() { return false; }
        if key.1 == 0 && generation < self.completed_full_generation {
            if request.clears_pending {
                self.state.pending_actions.remove(&key.0);
                self.state.in_flight.retain(|(project_id, _)| *project_id != key.0);
            }
            return false;
        }
        true
    }

    fn stop_refreshes(&mut self) {
        if let Some((_, task)) = self.full_refresh.take() { task.abort(); }
        for (_, request) in self.refreshes.drain() {
            if let Some(task) = request.task { task.abort(); }
        }
    }

    fn start_full_refresh(&mut self, tx: &mpsc::Sender<AppEvent>) {
        if self.full_refresh.is_some() { return; }
        let generation = self.next_request();
        self.state.mark_refreshing();
        let task = spawn_refresh(tx.clone(), self.instance_generation, generation,
            self.active_client().clone(), self.state.refresh_sources().clone(), self.state.ref_overrides.clone());
        self.full_refresh = Some((generation, task.abort_handle()));
    }

    fn accept_full_refresh(&mut self, report: &mut RefreshReport) -> bool {
        if report.instance != self.instance_generation {
            return false;
        }
        if self.full_refresh.as_ref().map(|(generation, _)| *generation) != Some(report.generation) { return false; }
        self.full_refresh = None;
        self.completed_full_generation = report.generation;
        for snapshot in &mut report.snapshots {
            if self.refreshes.get(&(snapshot.configured.id, 0))
                .is_some_and(|request| request.generation > report.generation) {
                if let Some(current) = self.state.items.iter().find(|item| item.configured.id == snapshot.configured.id) {
                    *snapshot = current.clone();
                }
            }
        }
        true
    }

    fn fetch_viewed_pipeline(&mut self, tx: &mpsc::Sender<AppEvent>, project_id: u64,
        pipeline_project_id: u64, pipeline_id: u64) {
        let key = (project_id, pipeline_id);
        let generation = self.start_project_request(key);
        let task = spawn_pipeline_jobs_fetch(tx.clone(), self.instance_generation, generation,
            self.active_client().clone(), project_id, pipeline_project_id, pipeline_id);
        self.refreshes.insert(key, PendingRefresh { generation, clears_pending: false, task: Some(task.abort_handle()) });
    }

    fn active_client(&self) -> &GitLabClient {
        &self.metas[self.active].client
    }

    fn active_store(&self) -> &LocalStateStore {
        &self.metas[self.active].store
    }

    /// Switches the active tab by `delta` (wrapping). Returns whether the active tab changed; when
    /// it does, the current tab's state is persisted and the target tab's cached state is loaded.
    fn switch(&mut self, delta: i32) -> bool {
        let n = self.metas.len();
        if n <= 1 {
            return false;
        }
        let next = (self.active as i32 + delta).rem_euclid(n as i32) as usize;
        if next == self.active {
            return false;
        }
        persist_dashboard_state(&self.state, self.active_store());
        self.state_dirty = false;
        self.stop_refreshes();
        self.instance_generation += 1;
        self.active = next;
        self.state = load_state_for(
            &self.metas[next],
            &self.labels,
            next,
            self.notifications_enabled,
        );
        true
    }

    /// Adds (or re-configures, matched by url) an instance and makes it the active tab. Used by the
    /// in-dashboard "add instance" flow.
    fn add_instance(&mut self, rt: InstanceRuntime) -> Result<()> {
        persist_dashboard_state(&self.state, self.active_store());
        self.state_dirty = false;
        let target = if let Some(idx) = self.metas.iter().position(|meta| meta.url == rt.url) {
            // Re-configuring an existing instance: refresh its client and sources, keep its cache.
            self.metas[idx].client = rt.client;
            self.metas[idx].sources = rt.sources;
            idx
        } else {
            let path = cache::instance_cache_path(&rt.url)
                .context("failed to resolve dashboard cache path")?;
            self.labels.push(cache::instance_label(&rt.url));
            self.metas.push(InstanceMeta {
                url: rt.url,
                client: rt.client,
                sources: rt.sources,
                store: LocalStateStore::from_path(path),
            });
            self.metas.len() - 1
        };
        self.stop_refreshes();
        self.instance_generation += 1;
        self.active = target;
        self.state = load_state_for(
            &self.metas[target],
            &self.labels,
            target,
            self.notifications_enabled,
        );
        Ok(())
    }
}

/// Loads an instance's [`AppState`] from its cache (or fresh if absent) and stamps the tab metadata
/// used to render the instance tab strip.
fn load_state_for(
    meta: &InstanceMeta,
    labels: &[String],
    active: usize,
    notifications_enabled: bool,
) -> AppState {
    let mut state = match meta
        .store
        .load_state::<crate::cache::CachedDashboardState>()
    {
        Ok(Some(cached)) if cache::cache_identity(&cached.instance_url) == cache::cache_identity(&meta.url) => {
            AppState::from_cached(meta.sources.clone(), cached)
        }
        Ok(Some(_)) => {
            warn!("ignoring dashboard cache belonging to another GitLab instance");
            AppState::new(meta.sources.clone())
        }
        Ok(None) => AppState::new(meta.sources.clone()),
        Err(err) => {
            warn!("failed to load cached dashboard state: {err}");
            AppState::new(meta.sources.clone())
        }
    };
    state.instance_url = cache::cache_identity(&meta.url).to_string();
    state.instance_labels = labels.to_vec();
    state.active_instance = active;
    state.notifications_enabled = notifications_enabled;
    state
}

#[derive(Debug, Default)]
pub(crate) struct TerminalModeGuard {
    active: bool,
}

impl TerminalModeGuard {
    pub(crate) fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let guard = Self { active: true };
        // Mouse capture lets us route the wheel to whichever panel the cursor is over (so the log
        // keeps scrolling even when another panel is focused); without it the terminal only turns
        // the wheel into arrow keys aimed at the focused panel.
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
        Ok(guard)
    }

    pub(crate) fn restore(&mut self) -> Result<()> {
        if self.active {
            disable_raw_mode()?;
            execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        }
    }
}

#[derive(Debug)]
enum AppEvent {
    Input(CEvent),
    RefreshCompleted(RefreshReport),
    ProjectRefreshed {
        instance: usize,
        generation: u64,
        snapshot: Box<state::ProjectSnapshot>,
    },
    /// A background auto-refresh tick re-fetched a project; unlike [`AppEvent::ProjectRefreshed`]
    /// it must not disturb the user (keeps an open pipeline-history view).
    ProjectAutoRefreshed {
        instance: usize,
        generation: u64,
        snapshot: Box<state::ProjectSnapshot>,
    },
    /// Periodic timer that drives auto-refresh of projects with an in-flight pipeline.
    AutoRefreshTick,
    /// Periodic timer that redraws so optimistic timers advance; issues no requests.
    UiTick,
    /// Fast timer (only while a spinner is visible) that redraws so the spinner animates smoothly.
    SpinnerTick,
    ActionCompleted(ActionReport),
    TraceLoaded {
        instance: usize,
        project_id: u64,
        job_id: u64,
        generation: u64,
        result: std::result::Result<String, String>,
    },
    VariablesLoaded {
        instance: usize,
        project_id: u64,
        result: std::result::Result<Vec<gitlab::ProjectVariable>, VariablesError>,
    },
    BranchesLoaded {
        instance: usize,
        project_id: u64,
        result: std::result::Result<Vec<gitlab::Branch>, String>,
    },
    PipelinesLoaded {
        instance: usize,
        project_id: u64,
        result: std::result::Result<Vec<gitlab::Pipeline>, String>,
    },
    EnvironmentsLoaded {
        instance: usize,
        project_id: u64,
        result: std::result::Result<Vec<gitlab::Environment>, String>,
    },
    PipelineJobsLoaded {
        instance: usize,
        generation: u64,
        project_id: u64,
        pipeline_id: u64,
        result: std::result::Result<(gitlab::Pipeline, Vec<gitlab::Job>, Vec<gitlab::Bridge>), String>,
    },
    TagsLoaded {
        instance: usize,
        project_id: u64,
        result: std::result::Result<Vec<gitlab::Tag>, String>,
    },
    TagCreated {
        instance: usize,
        project_id: u64,
        tag_name: String,
        result: std::result::Result<(), String>,
    },
    MergeRequestsLoaded {
        instance: usize,
        generation: u64,
        scope: state::MrScope,
        /// The dashboard project the list was fetched for; `None` for instance-wide scopes.
        project_id: Option<u64>,
        result: std::result::Result<Vec<gitlab::MergeRequest>, String>,
    },
    /// One MR's lazily fetched head pipeline (`None` when the MR has no pipeline).
    MrHeadPipelineLoaded {
        instance: usize,
        generation: u64,
        project_id: u64,
        iid: u64,
        result: std::result::Result<Option<gitlab::Pipeline>, String>,
    },
    /// A background artifacts download finished; carries the written path and byte count.
    ArtifactsDownloaded {
        instance: usize,
        project_id: u64,
        job_id: u64,
        result: std::result::Result<(std::path::PathBuf, u64), String>,
    },
    /// Result of the "jump to running pipeline" scan ('g'): the projects found actively building
    /// and the branch each is building.
    RunningScanCompleted {
        instance: usize,
        hits: Vec<RunningHit>,
    },
}

/// One dashboard project GitLab is actively building, and which branch. Produced by the running
/// scan, consumed to pin that branch and jump to it.
#[derive(Debug, Clone)]
pub(crate) struct RunningHit {
    pub(crate) project_id: u64,
    pub(crate) ref_: String,
    pub(crate) pipeline_id: u64,
}

/// How often follow mode re-fetches a running job's trace.
const FOLLOW_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// How often the dashboard re-fetches projects whose latest pipeline is still in flight, so the
/// pipeline panel (job statuses) and a followed job log stay live without a keystroke. The TIME
/// column ticks every second on its own (see [`UI_TICK_INTERVAL`]); this only governs how often the
/// authoritative server state (statuses, durations) is re-synced.
const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(3);

/// How often the UI redraws on its own so optimistic timers (the running-job TIME column) advance
/// smoothly between server refreshes. Local only — issues no requests.
const UI_TICK_INTERVAL: Duration = Duration::from_secs(1);

/// How often the UI redraws while a spinner is visible (any loading / in-flight action), so the
/// `| / - \` animation looks smooth instead of ticking once a second. Runs only while
/// [`AppState::is_spinning`] holds, then stops.
// ponytail: lazygit uses 50ms; 80ms (~12fps) is visually smooth and lighter for a full-screen redraw.
const SPINNER_TICK_INTERVAL: Duration = Duration::from_millis(80);

/// Runs the interactive terminal dashboard for the configured GitLab projects.
///
/// # Errors
///
/// Returns an error if the terminal cannot be switched into raw/alternate-screen mode, the UI
/// cannot be drawn, or terminal state restoration fails.
pub async fn run_dashboard(
    instances: Vec<InstanceRuntime>,
    notifications_enabled: bool,
) -> Result<()> {
    let mut dashboard = Dashboard::new(instances, notifications_enabled)?;
    loop {
        // One terminal session per run-loop pass; the add-instance wizard runs between passes with
        // its own terminal session, so we tear ours down around it.
        let outcome = {
            let mut mode_guard = TerminalModeGuard::enter()?;
            let backend = CrosstermBackend::new(io::stdout());
            let mut terminal = Terminal::new(backend)?;
            let result = run_loop(&mut terminal, &mut dashboard).await;
            restore_terminal(&mut terminal)?;
            mode_guard.restore()?;
            result?
        };
        match outcome {
            Outcome::Quit => return Ok(()),
            Outcome::AddInstance => {
                add_instances_via_wizard(&mut dashboard).await?;
                dashboard.state = load_state_for(&dashboard.metas[dashboard.active], &dashboard.labels,
                    dashboard.active, dashboard.notifications_enabled);
            }
        }
    }
}

/// Suspends into the setup wizard to add one or more instances, persists them to the config, and
/// makes the last one the active tab. Cancelling adds nothing.
async fn add_instances_via_wizard(dashboard: &mut Dashboard) -> Result<()> {
    let Some(added) = onboarding::run_onboarding(None).await? else {
        return Ok(());
    };
    // Merge into the existing config (don't clobber other instances on a transient read error).
    let mut cfg = config::load_config()?;
    for rt in &added {
        // Keep any proxy already set for this url; the wizard doesn't collect one.
        let proxy = cfg.proxy_for(&rt.url);
        let allow_insecure_http = cfg.allow_insecure_http_for(&rt.url);
        config::upsert_instance(
            &mut cfg,
            config::InstanceConfig {
                url: rt.url.clone(),
                proxy,
                allow_insecure_http,
                projects: rt.sources.projects.clone(),
                groups: rt.sources.groups.clone(),
            },
        );
    }
    config::save_config(&cfg)?;
    for rt in added {
        dashboard.add_instance(rt)?;
    }
    Ok(())
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    dashboard: &mut Dashboard,
) -> Result<Outcome> {
    let (tx, mut rx) = mpsc::channel::<AppEvent>(128);
    let stop = Arc::new(AtomicBool::new(false));
    let reader = spawn_event_reader(tx.clone(), stop.clone(), AppEvent::Input);
    dashboard.start_full_refresh(&tx);
    spawn_auto_refresh_tick(tx.clone());
    spawn_ui_tick(tx.clone());

    let outcome = loop {
        let active = dashboard.instance_generation;
        let active_client = dashboard.active_client().clone();
        maybe_fetch_branches(&mut dashboard.state, &tx, active, &active_client);
        if let Err(err) = terminal.draw(|frame| render(frame, &dashboard.state)) {
            break Err(err.into());
        }

        // Drain pipeline-completion notifications produced by the last batch of snapshot merges.
        for message in dashboard.state.pending_notifications.drain(..) {
            emit_notification(&message);
        }

        // Start the fast spinner-redraw tick when something began spinning (single point that
        // catches every loading source). The chain keeps itself alive until nothing is spinning.
        if dashboard.state.is_spinning() && !dashboard.spinner_running {
            dashboard.spinner_running = true;
            spawn_spinner_tick(tx.clone());
        }

        match rx.recv().await {
            Some(AppEvent::Input(event)) => {
                // Mouse hit-testing needs the screen rect the last frame was drawn into.
                let area = terminal
                    .size()
                    .map(|size| Rect::new(0, 0, size.width, size.height))
                    .unwrap_or_default();
                if let Some(outcome) = handle_input(&event, dashboard, &tx, area) {
                    break Ok(outcome);
                }
            }
            Some(AppEvent::RefreshCompleted(mut report)) => {
                if !dashboard.accept_full_refresh(&mut report) { continue; }
                if report.is_complete_failure && dashboard.state.is_showing_cached_data() {
                    dashboard.state.apply_cached_refresh_failure();
                    continue;
                }

                let should_persist = report.has_usable_data();
                let RefreshReport {
                    snapshots,
                    errors,
                    is_complete_failure,
                    group_errors,
                    ..
                } = report;

                dashboard.state.apply_live_refresh(
                    snapshots,
                    errors,
                    is_complete_failure,
                    &group_errors,
                );

                if should_persist {
                    dashboard.state_dirty = true;
                }
            }
            Some(AppEvent::ProjectRefreshed { instance, generation, snapshot }) => {
                if instance != dashboard.instance_generation {
                    continue;
                }
                if !dashboard.finish_project_request((snapshot.configured.id, 0), generation) { continue; }
                let follow = dashboard.state.apply_project_refresh(*snapshot);
                resume_log_follow(follow, &tx, instance, dashboard.active_client());
                dashboard.state_dirty = true;
            }
            Some(AppEvent::ProjectAutoRefreshed { instance, generation, snapshot }) => {
                if instance != dashboard.instance_generation {
                    continue;
                }
                if !dashboard.finish_project_request((snapshot.configured.id, 0), generation) { continue; }
                let follow = dashboard.state.apply_project_auto_refresh(*snapshot);
                resume_log_follow(follow, &tx, instance, dashboard.active_client());
                dashboard.state_dirty = true;
            }
            Some(AppEvent::AutoRefreshTick) => {
                refresh_active_projects(dashboard, &tx);
                spawn_auto_refresh_tick(tx.clone());
            }
            Some(AppEvent::UiTick) => {
                // The loop redraws at the top of every iteration, which is all an optimistic timer
                // needs. The tick also flushes at most one debounced cache write per second instead
                // of one per project per refresh cycle. ponytail: keep the write synchronous — a
                // small JSON file is sub-ms on SSD; reach for spawn_blocking only if profiling shows
                // the tick stalling.
                if dashboard.state_dirty {
                    persist_dashboard_state(&dashboard.state, dashboard.active_store());
                    dashboard.state_dirty = false;
                }
                spawn_ui_tick(tx.clone());
            }
            Some(AppEvent::SpinnerTick) => {
                // Keep the chain alive only while a spinner is still visible; otherwise let it stop
                // (the redraw itself happens at the top of the loop on every iteration).
                if dashboard.state.is_spinning() {
                    spawn_spinner_tick(tx.clone());
                } else {
                    dashboard.spinner_running = false;
                }
            }
            Some(AppEvent::ActionCompleted(report)) => {
                if report.instance != dashboard.instance_generation {
                    continue;
                }
                handle_action_completed(report, dashboard, &tx);
            }
            Some(AppEvent::TraceLoaded {
                instance,
                project_id,
                job_id,
                generation,
                result,
            }) => {
                if instance != dashboard.instance_generation {
                    continue;
                }
                handle_trace_loaded(project_id, job_id, generation, result, dashboard, &tx);
            }
            Some(AppEvent::VariablesLoaded {
                instance,
                project_id,
                result,
            }) => {
                if instance != dashboard.instance_generation {
                    continue;
                }
                handle_variables_loaded(project_id, result, &mut dashboard.state);
            }
            Some(AppEvent::BranchesLoaded {
                instance,
                project_id,
                result,
            }) => {
                if instance != dashboard.instance_generation {
                    continue;
                }
                dashboard.state.apply_branches_loaded(project_id, result);
            }
            Some(AppEvent::PipelinesLoaded {
                instance,
                project_id,
                result,
            }) => {
                if instance != dashboard.instance_generation {
                    continue;
                }
                handle_pipelines_loaded(project_id, result, &mut dashboard.state);
            }
            Some(AppEvent::EnvironmentsLoaded {
                instance,
                project_id,
                result,
            }) => {
                if instance != dashboard.instance_generation {
                    continue;
                }
                handle_environments_loaded(project_id, result, &mut dashboard.state);
            }
            Some(AppEvent::PipelineJobsLoaded {
                instance,
                generation,
                project_id,
                pipeline_id,
                result,
            }) => {
                if instance != dashboard.instance_generation {
                    continue;
                }
                if !dashboard.finish_project_request((project_id, pipeline_id), generation) { continue; }
                let follow = handle_pipeline_jobs_loaded(project_id, pipeline_id, result, &mut dashboard.state);
                resume_log_follow(follow, &tx, instance, dashboard.active_client());
            }
            Some(AppEvent::TagsLoaded {
                instance,
                project_id,
                result,
            }) => {
                if instance != dashboard.instance_generation {
                    continue;
                }
                handle_tags_loaded(project_id, result, &mut dashboard.state);
            }
            Some(AppEvent::TagCreated {
                instance,
                project_id,
                tag_name,
                result,
            }) => {
                if instance != dashboard.instance_generation {
                    continue;
                }
                if handle_tag_created(project_id, tag_name, result, &mut dashboard.state) {
                    // Load the tag's pipeline (and its jobs) into the detail panel. GitLab often
                    // registers the tag-triggered pipeline a moment after the tag, so fetch now and
                    // again after a short delay to catch it.
                    spawn_project_refresh(dashboard, &tx, project_id, Duration::ZERO);
                    spawn_project_refresh(dashboard, &tx, project_id, TAG_PIPELINE_RETRY_DELAY);
                }
            }
            Some(AppEvent::MergeRequestsLoaded {
                instance,
                generation,
                scope,
                project_id,
                result,
            }) => {
                if instance != dashboard.instance_generation {
                    continue;
                }
                handle_merge_requests_loaded(scope, generation, project_id, result, &mut dashboard.state);
                // Kick off the head-pipeline fetch for the initially selected row.
                for command in input::request_selected_head_pipeline(&mut dashboard.state) {
                    if let Command::FetchMrHeadPipeline { project_id, iid } = command {
                        spawn_mr_head_pipeline_fetch(
                            tx.clone(),
                            dashboard.instance_generation,
                            dashboard.active_client().clone(),
                            project_id,
                            iid,
                            dashboard.state.mr_panel.generation,
                        );
                    }
                }
            }
            Some(AppEvent::MrHeadPipelineLoaded {
                instance,
                generation,
                project_id,
                iid,
                result,
            }) => {
                if instance != dashboard.instance_generation {
                    continue;
                }
                if generation != dashboard.state.mr_panel.generation { continue; }
                let slot = match result {
                    Ok(pipeline) => state::HeadPipelineSlot::Loaded(pipeline),
                    Err(err) => state::HeadPipelineSlot::Failed(err),
                };
                dashboard
                    .state
                    .mr_panel
                    .head_pipelines
                    .insert((project_id, iid), slot);
            }
            Some(AppEvent::ArtifactsDownloaded {
                instance,
                project_id,
                job_id,
                result,
            }) => {
                if instance != dashboard.instance_generation {
                    continue;
                }
                let state = &mut dashboard.state;
                state.in_flight.remove(&(project_id, job_id));
                state.status_message = match result {
                    Ok((path, written)) => format!(
                        "Artifacts saved to {} ({:.1} MB)",
                        path.display(),
                        written as f64 / 1_048_576.0
                    ),
                    Err(err) => format!("Artifacts download failed: {err}"),
                };
            }
            Some(AppEvent::RunningScanCompleted { instance, hits }) => {
                if instance != dashboard.instance_generation {
                    continue;
                }
                handle_running_scan_completed(hits, dashboard, &tx);
            }
            None => break Ok(Outcome::Quit),
        }
    };

    // Flush a debounced cache write that the 1s UI tick hadn't picked up yet, so quitting right
    // after a refresh doesn't drop the latest snapshot.
    if dashboard.state_dirty {
        persist_dashboard_state(&dashboard.state, dashboard.active_store());
    }

    dashboard.stop_refreshes();
    dashboard.instance_generation += 1;
    dashboard.spinner_running = false;
    // Closing the receiver also unblocks a reader waiting on a full input queue.
    drop(rx);
    stop.store(true, Ordering::SeqCst);
    let _ = reader.await;
    outcome
}

/// Schedules the next auto-refresh tick. Self-rescheduling (the run loop re-arms it on receipt),
/// so the timer runs for the whole session and only fans out work when a pipeline is in flight.
fn spawn_auto_refresh_tick(tx: mpsc::Sender<AppEvent>) {
    tokio::spawn(async move {
        tokio::time::sleep(AUTO_REFRESH_INTERVAL).await;
        let _ = tx.send(AppEvent::AutoRefreshTick).await;
    });
}

/// Schedules the next UI redraw tick. Self-rescheduling, like [`spawn_auto_refresh_tick`], but it
/// only wakes the loop to redraw (advancing optimistic timers) — it never touches the network.
fn spawn_ui_tick(tx: mpsc::Sender<AppEvent>) {
    tokio::spawn(async move {
        tokio::time::sleep(UI_TICK_INTERVAL).await;
        let _ = tx.send(AppEvent::UiTick).await;
    });
}

fn spawn_spinner_tick(tx: mpsc::Sender<AppEvent>) {
    tokio::spawn(async move {
        tokio::time::sleep(SPINNER_TICK_INTERVAL).await;
        let _ = tx.send(AppEvent::SpinnerTick).await;
    });
}

/// Re-fetches each project whose latest pipeline is still in flight, so its TIME column and job
/// statuses keep advancing. Silent (no "Refreshing…" flash) and per-project — idle projects cost
/// nothing. Projects already awaiting an action-driven refresh are skipped to avoid double fetches.
fn refresh_active_projects(dashboard: &mut Dashboard, tx: &mpsc::Sender<AppEvent>) {
    if dashboard.full_refresh.is_some() { return; }
    for project_id in dashboard.state.active_project_ids() {
        let key = (project_id, 0);
        if dashboard.refreshes.get(&key).is_some_and(|request| request.task.is_some()) { continue; }
        if let Some(item) = dashboard.state.items.iter().find(|item| item.configured.id == project_id) {
            let configured = item.configured.clone();
            let origin = item.origin;
            let ref_override = dashboard.state.ref_overrides.get(&project_id).cloned();
            let generation = dashboard.start_project_request(key);
            let task = spawn_refresh_one_auto(tx.clone(), dashboard.instance_generation, generation,
                dashboard.active_client().clone(), configured, origin, ref_override);
            dashboard.refreshes.insert(key, PendingRefresh { generation, clears_pending: false, task: Some(task.abort_handle()) });
        }
    }
    let views: Vec<_> = dashboard.state.viewed_pipeline.iter()
        .filter(|(_, view)| view.pipeline.status.is_active() && !view.loading
            && !dashboard.state.pending_actions.contains(&view.project_id))
        .map(|(&id, view)| (id, view.project_id, view.pipeline.id)).collect();
    for (id, project_id, pipeline_id) in views {
        if dashboard.refreshes.get(&(id, pipeline_id)).is_some_and(|request| request.task.is_some()) { continue; }
        dashboard.fetch_viewed_pipeline(tx, id, project_id, pipeline_id);
    }
}

/// After a refresh re-enabled follow on the open log, kick a one-shot trace fetch so the
/// trace-follow loop restarts (toggling `follow` alone doesn't re-arm it).
fn resume_log_follow(
    request: Option<TraceFetchRequest>,
    tx: &mpsc::Sender<AppEvent>,
    instance: usize,
    client: &GitLabClient,
) {
    if let Some(req) = request {
        spawn_trace_fetch(
            tx.clone(),
            instance,
            client.clone(),
            req.project_id,
            req.job_id,
            req.generation,
            None,
        );
    }
}

/// Lazily fetches the branch list of the selected project (once, cached), so the branches panel
/// fills in as the user moves between projects without a separate keystroke.
fn maybe_fetch_branches(
    state: &mut AppState,
    tx: &mpsc::Sender<AppEvent>,
    instance: usize,
    client: &GitLabClient,
) {
    if let Some(project_id) = state.branches_fetch_needed() {
        state.branches.mark_loading(project_id);
        spawn_branches_fetch(tx.clone(), instance, client.clone(), project_id);
    }
}

fn handle_action_completed(
    report: ActionReport,
    dashboard: &mut Dashboard,
    tx: &mpsc::Sender<AppEvent>,
) {
    let instance = dashboard.instance_generation;
    let client = dashboard.active_client().clone();
    let state = &mut dashboard.state;
    let project_id = report.action.project_id();

    // MR actions refresh the MR panel, not the project snapshot: the MR may belong to a project
    // that isn't even on the dashboard (instance-wide scopes).
    if report.action.is_mr_action() {
        state.pending_actions.remove(&project_id);
        state.in_flight.retain(|(pid, _)| *pid != project_id);
        match report.result {
            Ok(()) => {
                state.status_message = format!("{}: requested", report.action.describe());
                let scope = state.mr_panel.scope;
                let dashboard_project = state.mr_panel.loaded_for_project;
                state.mr_panel.reset();
                state.mr_panel.scope = scope;
                state.mr_panel.loading = true;
                state.mr_panel.loaded_for_project = dashboard_project;
                spawn_merge_requests_fetch(
                    tx.clone(),
                    instance,
                    client,
                    dashboard_project,
                    scope,
                    state.mr_panel.generation,
                );
            }
            Err(err) => {
                state.status_message = format!("{} failed: {err}", report.action.describe());
            }
        }
        return;
    }

    match report.result {
        Ok(()) => {
            state.status_message = format!("{}: requested", report.action.describe());
            let views: Vec<_> = state.viewed_pipeline.iter()
                .filter(|(_, view)| view.project_id == project_id)
                .map(|(&id, view)| (id, view.pipeline.id)).collect();
            for &(id, pipeline_id) in &views {
                if let Some(view) = dashboard.state.viewed_pipeline.get_mut(&id) { view.loading = true; }
                dashboard.fetch_viewed_pipeline(tx, id, project_id, pipeline_id);
            }
            if views.is_empty() {
                if dashboard.state.items.iter().any(|item| item.configured.id == project_id) {
                    spawn_project_refresh(dashboard, tx, project_id, Duration::ZERO);
                } else {
                    dashboard.state.pending_actions.remove(&project_id);
                    dashboard.state.in_flight.retain(|(pid, _)| *pid != project_id);
                }
            }
        }
        Err(err) => {
            state.pending_actions.remove(&project_id);
            // No refresh will follow a failed action, so clear the spinner here.
            state.in_flight.retain(|(pid, _)| *pid != project_id);
            state.status_message = format!("{} failed: {err}", report.action.describe());
        }
    }
}

fn handle_trace_loaded(
    project_id: u64,
    job_id: u64,
    generation: u64,
    result: std::result::Result<String, String>,
    dashboard: &mut Dashboard,
    tx: &mpsc::Sender<AppEvent>,
) {
    // Capture the active instance/client before borrowing the log mutably below.
    let instance = dashboard.instance_generation;
    let client = dashboard.active_client().clone();
    let Some(view) = &mut dashboard.state.log else {
        return;
    };
    if view.apply_trace(project_id, job_id, generation, result) {
        spawn_trace_fetch(
            tx.clone(),
            instance,
            client,
            project_id,
            job_id,
            generation,
            Some(FOLLOW_REFRESH_INTERVAL),
        );
    }
}

fn handle_variables_loaded(
    project_id: u64,
    result: std::result::Result<Vec<gitlab::ProjectVariable>, VariablesError>,
    state: &mut AppState,
) {
    // Normalize and refresh the in-memory cache on success, regardless of which
    // overlay (if any) is open now — a background fetch can outlive the modal.
    let outcome = match result {
        Ok(mut variables) => {
            variables.sort_by_key(|variable| variable.key.to_lowercase());
            state.variables_cache.insert(project_id, variables.clone());
            Ok(variables)
        }
        Err(err) => Err(err),
    };

    let Overlay::Variables(view) = &mut state.overlay else {
        return;
    };
    if view.project_id != project_id {
        return;
    }

    let was_loading = view.loading;
    view.loading = false;
    match outcome {
        Ok(variables) => {
            view.variables = variables;
            view.error = None;
            if was_loading {
                // Nothing was on screen yet: start at the top.
                view.selected = 0;
                view.revealed = false;
            } else {
                // Background refresh over already-shown data: keep the user's cursor
                // and filter, only clamping the selection if the list shrank.
                let len = view.matching().len();
                if view.selected >= len {
                    view.selected = len.saturating_sub(1);
                }
            }
        }
        // A transient background failure must not blank already-shown cached data;
        // surface errors only when there was nothing to show.
        Err(VariablesError::Forbidden) if was_loading => {
            view.error = Some(String::from(
                "Access denied: requires Maintainer role and a token with `api` scope",
            ));
        }
        Err(VariablesError::Other(err)) if was_loading => view.error = Some(err),
        Err(_) => {}
    }
}

/// Delivers a finished MR-list fetch into the panel, dropping stale responses (the user may have
/// switched scope or project while the fetch was in flight).
fn handle_merge_requests_loaded(
    scope: state::MrScope,
    generation: u64,
    project_id: Option<u64>,
    result: std::result::Result<Vec<gitlab::MergeRequest>, String>,
    state: &mut AppState,
) {
    let panel = &mut state.mr_panel;
    if !panel.loading || panel.scope != scope || panel.generation != generation {
        return;
    }
    if panel.scope == state::MrScope::Project && panel.loaded_for_project != project_id {
        return;
    }
    panel.loading = false;
    match result {
        Ok(mrs) => {
            panel.mrs = mrs;
            panel.selected = 0;
            panel.error = None;
        }
        Err(err) => panel.error = Some(err),
    }
}

/// Delivers a finished environments fetch into the overlay, if it's still open for that project.
fn handle_environments_loaded(
    project_id: u64,
    result: std::result::Result<Vec<gitlab::Environment>, String>,
    state: &mut AppState,
) {
    let Overlay::Environments(view) = &mut state.overlay else {
        return;
    };
    if view.project_id != project_id {
        return;
    }
    view.loading = false;
    match result {
        Ok(environments) => {
            view.environments = environments;
            view.selected = 0;
            view.error = None;
        }
        Err(err) => view.error = Some(err),
    }
}

fn handle_pipelines_loaded(
    project_id: u64,
    result: std::result::Result<Vec<gitlab::Pipeline>, String>,
    state: &mut AppState,
) {
    let Overlay::Pipelines(view) = &mut state.overlay else {
        return;
    };
    if view.project_id != project_id {
        return;
    }
    view.loading = false;
    match result {
        Ok(pipelines) => {
            view.pipelines = pipelines;
            view.selected = 0;
            view.error = None;
        }
        Err(err) => view.error = Some(err),
    }
}

fn handle_tags_loaded(
    project_id: u64,
    result: std::result::Result<Vec<gitlab::Tag>, String>,
    state: &mut AppState,
) {
    let Overlay::Tags(view) = &mut state.overlay else {
        return;
    };
    if view.project_id != project_id {
        return;
    }
    view.loading = false;
    match result {
        Ok(tags) => {
            view.tags = tags;
            view.selected = 0;
            view.error = None;
        }
        Err(err) => view.error = Some(err),
    }
}

/// Applies a tag-create result. On success it closes the overlay and tracks the new tag as the
/// project's ref, so the detail panel loads the tag's pipeline and its jobs like any other branch's
/// (the user can play/retry them there); returns `true` so the caller refreshes that project. On
/// failure the error stays in the still-open form.
fn handle_tag_created(
    project_id: u64,
    tag_name: String,
    result: std::result::Result<(), String>,
    state: &mut AppState,
) -> bool {
    let Overlay::Tags(view) = &mut state.overlay else {
        return false;
    };
    if view.project_id != project_id {
        return false;
    }
    match result {
        Ok(()) => {
            state.overlay = Overlay::None;
            state.ref_overrides.insert(project_id, tag_name.clone());
            // A new tracked ref means a different latest pipeline; drop any open history view.
            state.viewed_pipeline.remove(&project_id);
            state.status_message = format!("Created tag {tag_name}; now tracking it");
            true
        }
        Err(err) => {
            if let Some(form) = &mut view.creating {
                form.error = Some(err);
            } else {
                view.error = Some(err);
            }
            false
        }
    }
}

/// Spawns a refresh of one project (latest pipeline + jobs for its tracked ref), after `delay`.
/// No-op if the id is unknown. Shared by the manual `RefreshProject` command and the tag-create
/// refreshes (an immediate one plus a delayed retry for the lagging tag pipeline).
fn spawn_project_refresh(
    dashboard: &mut Dashboard,
    tx: &mpsc::Sender<AppEvent>,
    project_id: u64,
    delay: Duration,
) {
    if let Some((configured, origin)) = dashboard
        .state
        .items
        .iter()
        .find(|item| item.configured.id == project_id)
        .map(|item| (item.configured.clone(), item.origin))
    {
        let ref_override = dashboard.state.ref_overrides.get(&project_id).cloned();
        let key = (project_id, 0);
        let generation = dashboard.start_project_request(key);
        let task = spawn_refresh_one(
            tx.clone(),
            dashboard.instance_generation,
            generation,
            dashboard.active_client().clone(),
            (configured, origin),
            ref_override,
            delay,
        );
        dashboard.refreshes.insert(key, PendingRefresh { generation, clears_pending: true, task: Some(task.abort_handle()) });
    }
}

/// Applies the running-pipeline scan: pins each building project onto the branch it's building (so
/// its list row reflects the running pipeline after refresh), then jumps focus to the branch panel
/// of the most recently started one. Pinning is what the tag-create flow already does, so the list
/// status and branch tracking stay consistent; `u` unpins.
fn handle_running_scan_completed(
    hits: Vec<RunningHit>,
    dashboard: &mut Dashboard,
    tx: &mpsc::Sender<AppEvent>,
) {
    let refresh_ids = dashboard.state.apply_running_hits(
        hits.into_iter()
            .map(|hit| (hit.project_id, hit.ref_, hit.pipeline_id))
            .collect(),
    );
    // Re-fetch each building project against its now-pinned branch so its row shows the running
    // pipeline (and the target's detail/branch panels fill in).
    for project_id in refresh_ids {
        spawn_project_refresh(dashboard, tx, project_id, Duration::ZERO);
    }
}

/// Delay before the second post-tag-create refresh, giving GitLab time to register the
/// tag-triggered pipeline. ponytail: one fixed retry; widen to a poll loop only if it proves flaky.
const TAG_PIPELINE_RETRY_DELAY: Duration = Duration::from_secs(3);

fn handle_pipeline_jobs_loaded(
    project_id: u64,
    pipeline_id: u64,
    result: std::result::Result<(gitlab::Pipeline, Vec<gitlab::Job>, Vec<gitlab::Bridge>), String>,
    state: &mut AppState,
) -> Option<TraceFetchRequest> {
    let view = state.viewed_pipeline.get_mut(&project_id)?;
    // The user may have switched to a different pipeline before this fetch returned.
    if view.pipeline.id != pipeline_id {
        return None;
    }
    view.loading = false;
    match result {
        Ok((pipeline, jobs, bridges)) => {
            view.pipeline = pipeline;
            view.jobs = jobs;
            view.bridges = bridges;
            view.error = None;
        }
        Err(err) => view.error = Some(err),
    }
    let pipeline_project_id = view.project_id;
    state.pending_actions.remove(&pipeline_project_id);
    state.in_flight.retain(|(pid, _)| *pid != pipeline_project_id);
    // Clamp the job cursor to the freshly loaded list.
    state.ensure_selection();
    state.sync_open_log_with_snapshot(pipeline_project_id)
}

/// Lines the mouse wheel scrolls the log per tick.
const MOUSE_LOG_SCROLL_STEP: usize = 3;

/// Returns `Some(outcome)` when the run loop should stop (quit or suspend to add an instance),
/// `None` to keep running.
fn handle_input(
    event: &CEvent,
    dashboard: &mut Dashboard,
    tx: &mpsc::Sender<AppEvent>,
    term_area: Rect,
) -> Option<Outcome> {
    let commands = match event {
        CEvent::Key(key) => {
            // Record the pane's visible height so visual-mode cursor moves can scroll to keep the
            // cursor on screen (the input layer has no other view onto the rendered geometry).
            let focus = dashboard.state.focus;
            let mode = dashboard.state.screen_mode;
            if let Some(view) = &mut dashboard.state.log {
                view.viewport_height = render::log_inner_height(term_area, focus, mode);
            }
            input::apply_key(&mut dashboard.state, key)
        }
        CEvent::Mouse(mouse) => handle_mouse(mouse, &mut dashboard.state, term_area),
        _ => return None,
    };

    // Keep the trace wrapped to the pane width; runs after apply_key so a just-opened log is sized
    // too. Like viewport_height, this only refreshes on key/mouse, so a bare resize re-wraps on the
    // next interaction.
    let focus = dashboard.state.focus;
    let mode = dashboard.state.screen_mode;
    if let Some(view) = &mut dashboard.state.log {
        view.set_wrap_width(render::log_inner_width(term_area, focus, mode));
    }

    for command in commands {
        match command {
            Command::Quit => return Some(Outcome::Quit),
            Command::AddInstance => return Some(Outcome::AddInstance),
            Command::SwitchInstance { delta } => {
                if dashboard.switch(delta) { dashboard.start_full_refresh(tx); }
            }
            Command::Refresh => {
                dashboard.state.mr_panel.reset();
                for command in input::focus_mr_refresh(&mut dashboard.state) {
                    if let Command::FetchMergeRequests { scope, project_id } = command {
                        spawn_merge_requests_fetch(tx.clone(), dashboard.instance_generation,
                            dashboard.active_client().clone(), project_id, scope, dashboard.state.mr_panel.generation);
                    }
                }
                dashboard.start_full_refresh(tx);
            }
            Command::RefreshProject { project_id } => {
                spawn_project_refresh(dashboard, tx, project_id, Duration::ZERO);
            }
            Command::ScanRunning => {
                let project_ids: Vec<u64> = dashboard
                    .state
                    .items
                    .iter()
                    .map(|item| item.configured.id)
                    .collect();
                if project_ids.is_empty() {
                    dashboard.state.status_message = String::from("No projects to scan");
                } else {
                    dashboard.state.status_message = String::from("Scanning for running pipelines…");
                    spawn_running_scan(
                        tx.clone(),
                        dashboard.instance_generation,
                        dashboard.active_client().clone(),
                        project_ids,
                    );
                }
            }
            Command::Dispatch(action) => {
                spawn_action(
                    tx.clone(),
                    dashboard.instance_generation,
                    dashboard.active_client().clone(),
                    action,
                );
            }
            Command::FetchTrace {
                project_id,
                job_id,
                generation,
            } => {
                spawn_trace_fetch(
                    tx.clone(),
                    dashboard.instance_generation,
                    dashboard.active_client().clone(),
                    project_id,
                    job_id,
                    generation,
                    None,
                );
            }
            Command::FetchVariables { project_id } => {
                spawn_variables_fetch(
                    tx.clone(),
                    dashboard.instance_generation,
                    dashboard.active_client().clone(),
                    project_id,
                );
            }
            Command::FetchPipelines { project_id, ref_ } => {
                spawn_pipelines_fetch(
                    tx.clone(),
                    dashboard.instance_generation,
                    dashboard.active_client().clone(),
                    project_id,
                    ref_,
                );
            }
            Command::FetchEnvironments { project_id } => {
                spawn_environments_fetch(
                    tx.clone(),
                    dashboard.instance_generation,
                    dashboard.active_client().clone(),
                    project_id,
                );
            }
            Command::FetchTags { project_id } => {
                spawn_tags_fetch(
                    tx.clone(),
                    dashboard.instance_generation,
                    dashboard.active_client().clone(),
                    project_id,
                );
            }
            Command::CreateTag {
                project_id,
                tag_name,
                ref_,
                message,
            } => {
                spawn_tag_create(
                    tx.clone(),
                    dashboard.instance_generation,
                    dashboard.active_client().clone(),
                    project_id,
                    tag_name,
                    ref_,
                    message,
                );
            }
            Command::FetchMergeRequests { scope, project_id } => {
                spawn_merge_requests_fetch(
                    tx.clone(),
                    dashboard.instance_generation,
                    dashboard.active_client().clone(),
                    project_id,
                    scope,
                    dashboard.state.mr_panel.generation,
                );
            }
            Command::FetchMrHeadPipeline { project_id, iid } => {
                spawn_mr_head_pipeline_fetch(
                    tx.clone(),
                    dashboard.instance_generation,
                    dashboard.active_client().clone(),
                    project_id,
                    iid,
                    dashboard.state.mr_panel.generation,
                );
            }
            Command::FetchPipelineJobs {
                project_id,
                pipeline_project_id,
                pipeline_id,
            } => {
                dashboard.fetch_viewed_pipeline(tx, project_id, pipeline_project_id, pipeline_id);
            }
            Command::DownloadArtifacts {
                project_id,
                job_id,
                job_name,
                filename,
            } => {
                spawn_artifacts_download(
                    tx.clone(),
                    dashboard.instance_generation,
                    dashboard.active_client().clone(),
                    project_id,
                    job_id,
                    job_name,
                    filename,
                );
            }
            Command::CopyToClipboard { value } => {
                copy_to_clipboard(&value);
            }
            Command::OpenUrl { url } => {
                open_url(&url);
            }
        }
    }
    None
}

/// Opens `url` in the system default browser without blocking or leaking child output into the TUI.
///
/// macOS + Linux only (the shipped targets: brew tap + linux binaries). Add a
/// `cmd /c start ""` branch if a Windows build is ever needed. Errors are swallowed — the worst
/// case is nothing opens, and there's no useful recovery from inside a full-screen TUI.
fn open_url(url: &str) {
    use std::process::{Command as OsCommand, Stdio};
    if !is_browser_safe_url(url) {
        return;
    }
    let prog = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = OsCommand::new(prog)
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Whether `url` is safe to hand to `open`/`xdg-open`. The url ultimately comes from a GitLab
/// `web_url` (server-controlled), so allow only http(s): this blocks `file://`/custom-scheme
/// handlers and argument injection (a leading `-` the opener would treat as a flag, not a URL).
fn is_browser_safe_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

/// How close in time two left-clicks must be to count as a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// Routes a mouse event to whatever the cursor is over: wheel events scroll the list under the
/// pointer (or the open overlay), left-clicks pick the item under the pointer (double-click
/// activates it). Returns any commands the activation produced, for the run loop to dispatch.
fn handle_mouse(mouse: &MouseEvent, state: &mut AppState, term_area: Rect) -> Vec<Command> {
    let point = Position::new(mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::ScrollDown => handle_scroll(true, state, term_area, point),
        MouseEventKind::ScrollUp => handle_scroll(false, state, term_area, point),
        // A press in the value window or log starts a text selection; anywhere else picks an item.
        MouseEventKind::Down(MouseButton::Left) => {
            return if value_select_start(state, term_area, point)
                || log_select_start(state, term_area, point)
            {
                Vec::new()
            } else {
                handle_click(state, term_area, point)
            };
        }
        // Dragging extends an in-progress value/log selection (no effect elsewhere).
        MouseEventKind::Drag(MouseButton::Left)
            if matches!(state.overlay, Overlay::VariableValue(_)) =>
        {
            value_select_extend(state, term_area, point);
        }
        MouseEventKind::Drag(MouseButton::Left) => log_select_extend(state, term_area, point),
        _ => {}
    }
    Vec::new()
}

/// Anchors a character selection in the value window at the pressed character. Returns whether the
/// value window is open (it owns every click while up, so the press is consumed either way).
fn value_select_start(state: &mut AppState, term_area: Rect, point: Position) -> bool {
    let Overlay::VariableValue(view) = &mut state.overlay else {
        return false;
    };
    if let Some(index) = render::overlays::value_char_at(term_area, view, point) {
        view.anchor = Some(index);
        view.cursor = index;
        view.notice = None;
    }
    true
}

/// Extends the value window's selection to the dragged character. Returns whether the value window
/// is open (so the caller skips log-selection handling).
fn value_select_extend(state: &mut AppState, term_area: Rect, point: Position) -> bool {
    let Overlay::VariableValue(view) = &mut state.overlay else {
        return false;
    };
    if view.anchor.is_some() {
        if let Some(index) = render::overlays::value_char_at(term_area, view, point) {
            view.cursor = index;
        }
    }
    true
}

/// Starts a line-wise selection in the log pane when the press lands on a log line, focusing the
/// pane so `y` can copy it. Returns whether the press was consumed by the log.
fn log_select_start(state: &mut AppState, term_area: Rect, point: Position) -> bool {
    let Some((body, offset)) = state.log.as_ref().and_then(|view| {
        render::overlays::log_body(term_area, state.focus, state.screen_mode, view)
    }) else {
        return false;
    };
    if !body.contains(point) {
        return false;
    }
    let line = offset + usize::from(point.y - body.y);
    let Some(view) = state.log.as_mut() else {
        return false;
    };
    if line >= view.lines.len() {
        return false;
    }
    view.begin_visual_at(line);
    state.focus = Focus::Log;
    true
}

/// Extends an in-progress log selection to the dragged line. The window holds still while the
/// pointer is inside the pane and only scrolls once it passes the top or bottom edge.
fn log_select_extend(state: &mut AppState, term_area: Rect, point: Position) {
    if state
        .log
        .as_ref()
        .is_none_or(|view| view.select_anchor.is_none())
    {
        return;
    }
    let Some((body, offset)) = state.log.as_ref().and_then(|view| {
        render::overlays::log_body(term_area, state.focus, state.screen_mode, view)
    }) else {
        return;
    };
    // Signed map: within the window (rel in 0..height) the line stays visible → no scroll; above or
    // below the pane the line falls outside → move_cursor_to scrolls just enough to follow it.
    let height = usize::from(body.height);
    let rel = i64::from(point.y) - i64::from(body.y);
    let target = (offset as i64 + rel).max(0) as usize;
    if let Some(view) = state.log.as_mut() {
        // Mouse events don't update viewport_height (only key events do); refresh it so the edge
        // detection matches the rendered pane.
        view.viewport_height = height;
        view.move_cursor_to(target.min(view.lines.len().saturating_sub(1)));
    }
}

/// Scrolls the list under the cursor: the open overlay if one is up (it owns the screen), otherwise
/// the panel under the pointer. This is what keeps the log scrollable from any focus.
fn handle_scroll(down: bool, state: &mut AppState, term_area: Rect, point: Position) {
    match &mut state.overlay {
        Overlay::Variables(view) => {
            if down {
                if view.selected + 1 < view.matching().len() {
                    view.selected += 1;
                    view.revealed = false;
                    view.notice = None;
                }
            } else if view.selected > 0 {
                view.selected -= 1;
                view.revealed = false;
                view.notice = None;
            }
            return;
        }
        Overlay::Pipelines(view) => {
            if down {
                if view.selected + 1 < view.pipelines.len() {
                    view.selected += 1;
                }
            } else if view.selected > 0 {
                view.selected -= 1;
            }
            return;
        }
        Overlay::Environments(view) => {
            if down {
                if view.selected + 1 < view.environments.len() {
                    view.selected += 1;
                }
            } else if view.selected > 0 {
                view.selected -= 1;
            }
            return;
        }
        Overlay::VariableValue(view) => {
            if down {
                view.scroll = view.scroll.saturating_add(1);
            } else {
                view.scroll = view.scroll.saturating_sub(1);
            }
            return;
        }
        // Help/Confirm/ActionMenu/Tags don't scroll; swallow the event so it doesn't leak to panels.
        Overlay::Help | Overlay::Confirm(_) | Overlay::ActionMenu(_) | Overlay::Tags(_) => return,
        Overlay::None => {}
    }

    let areas = render::panel_areas(term_area, state.focus, state.screen_mode);
    if areas.log.contains(point) {
        state.scroll_log(down, MOUSE_LOG_SCROLL_STEP);
    } else if areas.merge_requests.contains(point) {
        // Selection only; the head-pipeline fetch stays keyboard/click-driven.
        let panel = &mut state.mr_panel;
        if down {
            if panel.selected + 1 < panel.mrs.len() {
                panel.selected += 1;
            }
        } else if panel.selected > 0 {
            panel.selected -= 1;
        }
    } else if areas.projects.contains(point) {
        if down {
            state.select_next_project();
        } else {
            state.select_previous_project();
        }
    } else if areas.branches.contains(point) {
        if down {
            state.select_next_branch();
        } else {
            state.select_previous_branch();
        }
    } else if areas.detail.contains(point) {
        if down {
            state.select_next_job();
        } else {
            state.select_previous_job();
        }
    }
}

/// Picks the item under a left-click: the open overlay's list if one is up, otherwise the projects,
/// branches, or jobs row under the pointer (also focusing that panel). A second click on the same
/// spot within [`DOUBLE_CLICK`] activates the item, as if Enter were pressed. Clicks elsewhere do
/// nothing.
fn handle_click(state: &mut AppState, term_area: Rect, point: Position) -> Vec<Command> {
    let now = Instant::now();
    let double = matches!(
        state.last_click,
        Some((when, x, y)) if x == point.x && y == point.y && now.duration_since(when) < DOUBLE_CLICK
    );
    state.last_click = Some((now, point.x, point.y));

    // An open overlay owns the screen, so its list is the only click target.
    match &mut state.overlay {
        Overlay::Variables(view) => {
            let hit = render::overlays::variables_body(term_area, view)
                .and_then(|(body, _)| hit_row(body, view.selected, view.matching().len(), point));
            if let Some(index) = hit {
                view.select_at(index);
            }
            return activate_if(state, double && hit.is_some());
        }
        Overlay::Pipelines(view) => {
            let hit = render::overlays::pipelines_body(term_area, view)
                .and_then(|(body, _)| hit_row(body, view.selected, view.pipelines.len(), point));
            if let Some(index) = hit {
                view.select_at(index);
            }
            return activate_if(state, double && hit.is_some());
        }
        Overlay::Tags(view) => {
            let hit = render::overlays::tags_body(term_area, view)
                .and_then(|(body, _)| hit_row(body, view.selected, view.tags.len(), point));
            if let Some(index) = hit {
                view.select_at(index);
            }
            // A double-click opens the selected tag's detail view, as Enter would.
            return activate_if(state, double && hit.is_some());
        }
        // The value window consumes its clicks in `value_select_start` before reaching here.
        Overlay::VariableValue(_) => return Vec::new(),
        // Environments is scroll/keyboard-only; swallow clicks like the other modal overlays.
        Overlay::Environments(_) => return Vec::new(),
        // ActionMenu is keyboard-only for now.
        Overlay::Help | Overlay::Confirm(_) | Overlay::ActionMenu(_) => return Vec::new(),
        Overlay::None => {}
    }

    let areas = render::panel_areas(term_area, state.focus, state.screen_mode);
    let (projects_body, _) = render::projects_panel::body_layout(areas.projects);
    if let Some(index) = hit_row(
        projects_body,
        state.selected_visible_position().unwrap_or(0),
        state.visible_indices().len(),
        point,
    ) {
        state.select_project_at(index);
        state.focus = Focus::Projects;
        return activate_if(state, double);
    }
    if let Some((body, _)) = render::branches_panel::body_layout(areas.branches, state) {
        if let Some(index) = hit_row(
            body,
            state.selected_branch,
            state.visible_branches().len(),
            point,
        ) {
            state.select_branch_at(index);
            state.focus = Focus::Branches;
            return activate_if(state, double);
        }
    }
    let (jobs_body, _) = render::detail_panel::jobs_layout(areas.detail);
    if let Some(index) = hit_row(
        jobs_body,
        state.selected_job,
        state.active_jobs().len(),
        point,
    ) {
        state.select_job_at(index);
        state.focus = Focus::Detail;
        return activate_if(state, double);
    }
    if areas.merge_requests.contains(point) {
        // Body = inner area minus the border (1 each side) and the table header row.
        let body = Rect {
            x: areas.merge_requests.x + 1,
            y: areas.merge_requests.y + 2,
            width: areas.merge_requests.width.saturating_sub(2),
            height: areas.merge_requests.height.saturating_sub(3),
        };
        let hit = hit_row(
            body,
            state.mr_panel.selected,
            state.mr_panel.mrs.len(),
            point,
        );
        // A click anywhere on the (possibly collapsed) panel focuses it, which also expands it.
        let mut commands = input::focus_mr_panel(state);
        if let Some(index) = hit {
            state.mr_panel.selected = index;
            commands.extend(input::request_selected_head_pipeline(state));
        }
        if double && hit.is_some() {
            commands.extend(input::activate(state));
        }
        return commands;
    }
    Vec::new()
}

/// Runs the focused item's Enter action when `yes`, else nothing. Keeps the double-click branches
/// in [`handle_click`] to one line each.
fn activate_if(state: &mut AppState, yes: bool) -> Vec<Command> {
    if yes {
        input::activate(state)
    } else {
        Vec::new()
    }
}

/// Maps a click within a list `body` to the item index under it, accounting for ratatui's
/// auto-scroll. Render rebuilds a default `TableState` each frame and only selects, so for the
/// height-1 rows used everywhere the scroll offset is fully determined by the selection. Returns
/// `None` when the click misses the body or lands past the last item.
fn hit_row(body: Rect, selected: usize, len: usize, point: Position) -> Option<usize> {
    if !body.contains(point) {
        return None;
    }
    let viewport = usize::from(body.height);
    if viewport == 0 {
        return None;
    }
    // ponytail: mirrors ratatui's first-visible-row for a fresh TableState; if render ever persists
    // table state across frames, read the real offset instead.
    let offset = selected.saturating_sub(viewport - 1);
    let index = offset + usize::from(point.y - body.y);
    (index < len).then_some(index)
}

/// Rings the terminal bell and posts an OSC 9 desktop notification (iTerm2, kitty, wezterm, modern
/// VTE). Same raw-stdout approach as [`copy_to_clipboard`]; terminals that don't support OSC 9
/// simply ignore the sequence.
fn emit_notification(message: &str) {
    let mut out = io::stdout();
    // BEL rings the bell; ESC ] 9 ; <message> BEL is the OSC 9 desktop notification.
    let sequence = format!("\x07\x1b]9;{message}\x07");
    let _ = out.write_all(sequence.as_bytes());
    let _ = out.flush();
}

/// Copies `text` to the system clipboard using the OSC 52 terminal sequence.
///
/// This needs no native clipboard dependency and works over SSH, provided the
/// terminal supports OSC 52 (iTerm2, kitty, wezterm, tmux with `set -g set-clipboard on`).
fn copy_to_clipboard(text: &str) {
    let mut out = io::stdout();
    // ESC ] 52 ; c ; <base64> BEL
    let sequence = format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()));
    let _ = out.write_all(sequence.as_bytes());
    let _ = out.flush();
}

/// Minimal standard-alphabet base64 encoder (RFC 4648), used for the OSC 52 payload.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18 & 0x3f) as usize] as char);
        out.push(ALPHABET[(triple >> 12 & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6 & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn spawn_event_reader<E: Send + 'static>(
    tx: mpsc::Sender<E>,
    stop: Arc<AtomicBool>,
    input: fn(CEvent) -> E,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || loop {
        // Checked each iteration (≤100ms granularity) so the reader can be stopped on demand.
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match event::poll(Duration::from_millis(100)) {
            Ok(true) => match event::read() {
                Ok(ev) => {
                    if tx.blocking_send(input(ev)).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    error!("event read error: {err}");
                    break;
                }
            },
            Ok(false) => {}
            Err(err) => {
                error!("event poll error: {err}");
                break;
            }
        }
    })
}

pub(crate) fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    terminal.show_cursor()?;
    Ok(())
}

fn persist_dashboard_state(state: &AppState, local_state_store: &LocalStateStore) {
    if let Err(err) = local_state_store.save_state(&state.to_cached_state_ref()) {
        warn!("failed to save cached dashboard state: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::{base64_encode, hit_row, is_browser_safe_url};
    use ratatui::layout::{Position, Rect};

    #[test]
    fn browser_safe_url_allows_http_s_and_rejects_injection() {
        assert!(is_browser_safe_url("https://gitlab.com/foo"));
        assert!(is_browser_safe_url("http://gitlab.local/foo"));
        // Server-controlled web_url turned into an opener flag or a non-web handler.
        assert!(!is_browser_safe_url("-a/Applications/Calculator.app"));
        assert!(!is_browser_safe_url("file:///etc/passwd"));
        assert!(!is_browser_safe_url("javascript:alert(1)"));
        assert!(!is_browser_safe_url(""));
    }

    #[test]
    fn hit_row_maps_clicks_through_the_scroll_offset() {
        // Body of 5 visible rows starting at screen y=2, list of 20 items, selection scrolled to 10.
        let body = Rect::new(0, 2, 10, 5);
        // selected=10, viewport=5 → offset=6, so the rows on screen are items 6..=10.
        assert_eq!(hit_row(body, 10, 20, Position::new(3, 2)), Some(6));
        assert_eq!(hit_row(body, 10, 20, Position::new(3, 6)), Some(10));
        // Unscrolled list shows items from the top.
        assert_eq!(hit_row(body, 0, 20, Position::new(3, 4)), Some(2));
        // Clicks above the body (header/border) and outside it miss.
        assert_eq!(hit_row(body, 0, 20, Position::new(3, 1)), None);
        assert_eq!(hit_row(body, 0, 20, Position::new(3, 7)), None);
        // A row that maps past the last item is rejected (short list, tall body).
        assert_eq!(
            hit_row(Rect::new(0, 0, 10, 5), 0, 3, Position::new(3, 4)),
            None
        );
    }

    #[test]
    fn base64_encode_matches_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
    #[test]
    fn viewed_pipeline_refresh_keeps_context_and_updates_the_actual_project() {
        use crate::state::{AppState, ViewedPipeline};
        let mut state = AppState::new(crate::RefreshSources {
            projects: vec![config::ProjectEntry { id: 1, name: "dashboard".into() }], groups: vec![],
        });
        let mut pipeline: gitlab::Pipeline = serde_json::from_str(r#"{"id":500,"status":"running"}"#).expect("pipeline");
        state.viewed_pipeline.insert(1, ViewedPipeline {
            project_id: 8, pipeline: pipeline.clone(), jobs: vec![], bridges: vec![], loading: true, error: None,
        });
        state.pending_actions.insert(8);
        state.mark_in_flight(8, 500);
        pipeline.status = gitlab::CiStatus::Canceled;
        let job = serde_json::from_str(r#"{"id":999,"name":"retried","status":"pending"}"#).expect("job");
        super::handle_pipeline_jobs_loaded(1, 500, Ok((pipeline, vec![job], vec![])), &mut state);
        let view = state.viewed_pipeline.get(&1).expect("history view stays open");
        assert_eq!(view.project_id, 8);
        assert_eq!(view.pipeline.status, gitlab::CiStatus::Canceled);
        assert_eq!(view.jobs[0].id, 999);
        assert!(!state.pending_actions.contains(&8));
        assert!(!state.is_in_flight(8, 500));
    }

    fn test_dashboard(cache: &std::path::Path) -> super::Dashboard {
        let sources = crate::RefreshSources {
            projects: vec![config::ProjectEntry { id: 1, name: "project".into() }], groups: vec![],
        };
        let metas = (0..2).map(|index| super::InstanceMeta {
            url: format!("https://git{index}.example"),
            client: gitlab::GitLabClient::new(format!("https://git{index}.example"), "test", None).expect("client"),
            sources: sources.clone(),
            store: storage::LocalStateStore::from_path(cache.join(format!("{index}.json"))),
        }).collect();
        super::Dashboard {
            metas, labels: vec!["first".into(), "second".into()], active: 0,
            instance_generation: 0, request_generation: 0, completed_full_generation: 0, refreshes: Default::default(), full_refresh: None,
            state: crate::state::AppState::new(sources), state_dirty: false,
            notifications_enabled: false, spinner_running: false,
        }
    }

    #[tokio::test]
    async fn refresh_requests_supersede_old_refs_and_cancel_old_views() {
        let cache = tempfile::tempdir().expect("cache");
        let mut dashboard = test_dashboard(cache.path());
        let (tx, _rx) = tokio::sync::mpsc::channel(128);
        super::spawn_project_refresh(&mut dashboard, &tx, 1, std::time::Duration::from_secs(60));
        let old = dashboard.refreshes[&(1, 0)].generation;
        let old_task = dashboard.refreshes[&(1, 0)].task.clone().expect("task");
        dashboard.state.ref_overrides.insert(1, "new-ref".into());
        super::spawn_project_refresh(&mut dashboard, &tx, 1, std::time::Duration::from_secs(60));
        let current = dashboard.refreshes[&(1, 0)].generation;
        assert!(!dashboard.finish_project_request((1, 0), old));
        assert!(dashboard.refreshes[&(1, 0)].task.is_some());
        dashboard.fetch_viewed_pipeline(&tx, 1, 1, 100);
        dashboard.fetch_viewed_pipeline(&tx, 1, 1, 101);
        assert!(!dashboard.refreshes.contains_key(&(1, 100)));
        assert_eq!(dashboard.refreshes.len(), 2);
        tokio::task::yield_now().await;
        assert!(old_task.is_finished());
        assert!(dashboard.finish_project_request((1, 0), current));
        dashboard.stop_refreshes();
    }

    #[tokio::test]
    async fn slow_auto_refresh_does_not_accumulate_requests() {
        let cache = tempfile::tempdir().expect("cache");
        let mut dashboard = test_dashboard(cache.path());
        dashboard.state.items[0].pipeline = Some(serde_json::from_str(r#"{"id":42,"status":"running"}"#).expect("pipeline"));
        let (tx, _rx) = tokio::sync::mpsc::channel(128);
        for _ in 0..100 { super::refresh_active_projects(&mut dashboard, &tx); }
        assert_eq!(dashboard.request_generation, 1);
        assert_eq!(dashboard.refreshes.len(), 1);
        assert!(dashboard.refreshes[&(1, 0)].task.is_some());
        dashboard.stop_refreshes();
    }

    #[tokio::test]
    async fn full_refresh_dedupes_and_cannot_revert_a_newer_project_request() {
        let cache = tempfile::tempdir().expect("cache");
        let mut dashboard = test_dashboard(cache.path());
        let (tx, _rx) = tokio::sync::mpsc::channel(128);
        dashboard.start_full_refresh(&tx);
        let generation = dashboard.request_generation;
        dashboard.start_full_refresh(&tx);
        assert_eq!(dashboard.request_generation, generation);
        let mut report = super::RefreshReport {
            instance: 0, generation, snapshots: dashboard.state.items.clone(),
            errors: 0, is_complete_failure: false, group_errors: vec![],
        };
        dashboard.start_project_request((1, 0));
        dashboard.state.items[0].configured.name = "newer".into();
        assert!(dashboard.accept_full_refresh(&mut report));
        assert_eq!(report.snapshots[0].configured.name, "newer");
        dashboard.stop_refreshes();
    }

    #[tokio::test]
    async fn returning_to_a_tab_rejects_the_previous_session() {
        let cache = tempfile::tempdir().expect("cache");
        let mut dashboard = test_dashboard(cache.path());
        let original = dashboard.instance_generation;
        assert!(dashboard.switch(1));
        assert!(dashboard.switch(-1));
        assert_eq!(dashboard.active, 0);
        assert_ne!(dashboard.instance_generation, original);
        let mut report = super::RefreshReport {
            instance: original, generation: 0, snapshots: vec![],
            errors: 0, is_complete_failure: false, group_errors: vec![],
        };
        assert!(!dashboard.accept_full_refresh(&mut report));
    }

    #[test]
    fn merge_request_results_require_scope_and_generation() {
        use crate::state::MrScope;
        let mut state = crate::state::AppState::new(crate::RefreshSources::default());
        state.mr_panel.scope = MrScope::CreatedByMe;
        state.mr_panel.loading = true;
        let old = state.mr_panel.generation;
        state.mr_panel.reset();
        state.mr_panel.scope = MrScope::AssignedToMe;
        state.mr_panel.loading = true;
        let current = state.mr_panel.generation;
        super::handle_merge_requests_loaded(MrScope::CreatedByMe, old, None, Err("old".into()), &mut state);
        assert!(state.mr_panel.loading);
        assert!(state.mr_panel.error.is_none());
        state.mr_panel.scope = MrScope::CreatedByMe;
        super::handle_merge_requests_loaded(MrScope::CreatedByMe, old, None, Err("old".into()), &mut state);
        assert!(state.mr_panel.loading);
        super::handle_merge_requests_loaded(MrScope::CreatedByMe, current, None, Err("current".into()), &mut state);
        assert!(!state.mr_panel.loading);
        assert_eq!(state.mr_panel.error.as_deref(), Some("current"));
    }

    #[test]
    fn loading_mr_list_is_replaced_when_the_selected_project_changes() {
        let mut state = crate::state::AppState::new(crate::RefreshSources {
            projects: vec![config::ProjectEntry { id: 1, name: "first".into() }, config::ProjectEntry { id: 2, name: "second".into() }],
            groups: vec![],
        });
        assert_eq!(crate::input::focus_mr_panel(&mut state).len(), 1);
        let generation = state.mr_panel.generation;
        state.select_next_project();
        assert_eq!(crate::input::focus_mr_panel(&mut state).len(), 1);
        assert!(state.mr_panel.generation > generation);
        assert_eq!(state.mr_panel.loaded_for_project, Some(2));
    }

    #[test]
    fn cache_from_another_server_does_not_restore_pins_or_snapshots() {
        let cache = tempfile::tempdir().expect("cache");
        let mut dashboard = test_dashboard(cache.path());
        dashboard.state.instance_url = dashboard.metas[0].url.clone();
        dashboard.state.ref_overrides.insert(1, "private-ref".into());
        dashboard.state.items[0].configured.name = "cached".into();
        super::persist_dashboard_state(&dashboard.state, &dashboard.metas[0].store);
        let matching = super::load_state_for(&dashboard.metas[0], &dashboard.labels, 0, false);
        assert_eq!(matching.ref_overrides.get(&1).map(String::as_str), Some("private-ref"));
        dashboard.metas[0].url = "https://another.example".into();
        let wrong = super::load_state_for(&dashboard.metas[0], &dashboard.labels, 0, false);
        assert!(wrong.ref_overrides.is_empty());
        assert_eq!(wrong.items[0].configured.name, "project");
        assert!(!wrong.is_showing_cached_data());
    }

    #[tokio::test]
    async fn stopped_input_reader_exits_and_releases_its_sender() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let reader = super::spawn_event_reader(tx, stop, super::AppEvent::Input);
        tokio::time::timeout(std::time::Duration::from_secs(1), reader).await.expect("reader stopped").expect("reader joined");
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn full_refresh_keeps_view_reads_and_rejects_older_snapshot_results() {
        let cache = tempfile::tempdir().expect("cache");
        let mut dashboard = test_dashboard(cache.path());
        let (tx, _rx) = tokio::sync::mpsc::channel(128);
        super::spawn_project_refresh(&mut dashboard, &tx, 1, std::time::Duration::from_secs(60));
        let old = dashboard.refreshes[&(1, 0)].generation;
        dashboard.state.pending_actions.insert(1);
        dashboard.state.mark_in_flight(1, 42);
        dashboard.fetch_viewed_pipeline(&tx, 1, 1, 42);
        let view_generation = dashboard.refreshes[&(1, 42)].generation;
        dashboard.start_full_refresh(&tx);
        assert!(dashboard.refreshes[&(1, 42)].task.is_some());
        let mut report = super::RefreshReport {
            instance: 0, generation: dashboard.request_generation, snapshots: vec![],
            errors: 0, is_complete_failure: false, group_errors: vec![],
        };
        assert!(dashboard.accept_full_refresh(&mut report));
        assert!(!dashboard.finish_project_request((1, 0), old));
        assert!(!dashboard.state.pending_actions.contains(&1));
        assert!(!dashboard.state.is_in_flight(1, 42));
        assert!(dashboard.finish_project_request((1, 42), view_generation));
        dashboard.stop_refreshes();
        let auto_generation = dashboard.start_project_request((1, 0));
        let pending = tokio::spawn(std::future::pending::<()>());
        dashboard.refreshes.get_mut(&(1, 0)).expect("request").task = Some(pending.abort_handle());
        dashboard.completed_full_generation = auto_generation + 1;
        dashboard.state.pending_actions.insert(1);
        assert!(!dashboard.finish_project_request((1, 0), auto_generation));
        assert!(dashboard.state.pending_actions.contains(&1));
        pending.abort();
    }

}
