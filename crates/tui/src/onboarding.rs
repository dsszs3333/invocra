//! First-run setup wizard: GitLab URL → token → group/project selection.
//!
//! Runs its own terminal event loop (separate from the dashboard) and, on success, persists the
//! token to the keyring and the chosen sources to `projects.toml`, returning a ready client.

use std::io::{self, Stdout};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use anyhow::Result;
use crossterm::event::{Event as CEvent, KeyEventKind};
use gitlab::{AccessibleGroup, AccessibleProject, GitLabClient};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use storage::TokenStore;
use tokio::sync::mpsc;

use crate::{restore_terminal, InstanceRuntime, RefreshSources, TerminalModeGuard};

mod render;
mod state;

use state::{OnboardingCommand, OnboardingState, Step, DEFAULT_GITLAB_URL};

#[derive(Debug)]
enum OnbEvent {
    Input(CEvent),
    Validated(Box<Result<ValidatedData, String>>),
}

#[derive(Debug)]
struct ValidatedData {
    client: GitLabClient,
    groups: Vec<AccessibleGroup>,
    projects: Vec<AccessibleProject>,
}

/// Runs the setup wizard, looping so the user can configure several instances in one pass. Returns
/// `Ok(None)` if cancelled before any instance was committed, otherwise the committed instances.
/// The caller is responsible for persisting them to the config (tokens are already saved to the
/// keyring during validation).
///
/// # Errors
///
/// Returns an error if the terminal cannot be set up or a token cannot be saved to the keyring.
pub async fn run_onboarding(default_url: Option<String>) -> Result<Option<Vec<InstanceRuntime>>> {
    let mut mode_guard = TerminalModeGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, default_url).await;

    restore_terminal(&mut terminal)?;
    mode_guard.restore()?;
    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    default_url: Option<String>,
) -> Result<Option<Vec<InstanceRuntime>>> {
    let (tx, mut rx) = mpsc::channel::<OnbEvent>(64);
    let stop = Arc::new(AtomicBool::new(false));
    let reader = crate::spawn_event_reader(tx.clone(), stop.clone(), OnbEvent::Input);

    let start_url = default_url
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_GITLAB_URL.to_string());
    let mut state = OnboardingState::new(start_url);
    let mut completed: Vec<InstanceRuntime> = Vec::new();

    let result = loop {
        if let Err(err) = terminal.draw(|frame| render::render(frame, &state)) {
            break Err(err.into());
        }

        match rx.recv().await {
            Some(OnbEvent::Input(CEvent::Key(key))) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match state.apply_key(&key) {
                    Some(OnboardingCommand::Cancel | OnboardingCommand::Finish) => {
                        break Ok(done(completed));
                    }
                    Some(OnboardingCommand::Validate { url, token }) => {
                        state.step = Step::Validating;
                        state.error = None;
                        spawn_validate(tx.clone(), url, token);
                    }
                    Some(OnboardingCommand::Commit) => {
                        completed.push(commit(&mut state));
                        state.step = Step::AddMore;
                    }
                    Some(OnboardingCommand::AddAnother) => state.reset_for_next(),
                    None => {}
                }
            }
            Some(OnbEvent::Input(_)) => {}
            Some(OnbEvent::Validated(result)) => match *result {
                Ok(data) => {
                    // Token is valid — persist it now so a later cancel still leaves auth set up.
                    // Key it by the instance url so multiple instances can each hold their own token.
                    if let Err(err) =
                        TokenStore::for_instance(state.gitlab_url()).save_token(state.token_input())
                    {
                        break Err(err.into());
                    }
                    state.enter_selection(&data.groups, &data.projects);
                    state.client = Some(data.client);
                }
                Err(message) => {
                    state.error = Some(message);
                    state.step = Step::Token;
                }
            },
            None => break Ok(done(completed)),
        }
    };
    drop(rx);
    stop.store(true, Ordering::SeqCst);
    let _ = reader.await;
    result
}

/// `Some` of the committed instances, or `None` when nothing was committed.
fn done(completed: Vec<InstanceRuntime>) -> Option<Vec<InstanceRuntime>> {
    if completed.is_empty() {
        None
    } else {
        Some(completed)
    }
}

/// Captures the instance the user just finished configuring into a runtime descriptor.
fn commit(state: &mut OnboardingState) -> InstanceRuntime {
    let (projects, groups) = state.collect_selection();
    let client = state
        .client
        .take()
        .expect("client is set before reaching the selection step");
    InstanceRuntime {
        url: state.gitlab_url().to_string(),
        client,
        sources: RefreshSources { projects, groups },
    }
}

fn spawn_validate(tx: mpsc::Sender<OnbEvent>, url: String, token: String) {
    tokio::spawn(async move {
        let result = validate(url, token).await;
        let _ = tx.send(OnbEvent::Validated(Box::new(result))).await;
    });
}

async fn validate(url: String, token: String) -> Result<ValidatedData, String> {
    // ponytail: onboarding connects directly — adding a proxy-only instance would need a proxy
    // field in the wizard; set `proxy` in the config TOML for an already-configured instance.
    let client = GitLabClient::new(&url, &token, None).map_err(|err| err.to_string())?;
    client.get_current_user().await.map_err(|err| {
        if err.is_unauthorized() {
            String::from("Invalid token (401). Check the value and that it has `api` scope.")
        } else {
            format!("Could not reach {url}: {err}")
        }
    })?;
    let groups = client.list_groups().await.map_err(|err| err.to_string())?;
    let projects = client
        .list_projects()
        .await
        .map_err(|err| err.to_string())?;
    Ok(ValidatedData {
        client,
        groups,
        projects,
    })
}
