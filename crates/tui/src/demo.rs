//! Demo startup and side effects stay separate from the real dashboard command paths.

use anyhow::Result;
use gitlab::{GitLabClient, DEMO_URL};

use crate::input::Command;
use crate::state::{AppState, Overlay};
use crate::{InstanceRuntime, RefreshSources};

impl InstanceRuntime {
    /// Builds the demo dashboard entirely from embedded data, without config or credentials.
    ///
    /// # Errors
    /// Returns an error if the embedded fixtures cannot be decoded.
    pub async fn demo() -> Result<Self> {
        let client = GitLabClient::demo()?;
        let projects = client
            .list_projects()
            .await?
            .into_iter()
            .map(|project| config::ProjectEntry {
                id: project.id,
                name: project.path_with_namespace,
            })
            .collect();
        Ok(Self {
            url: DEMO_URL.into(),
            client,
            sources: RefreshSources {
                projects,
                groups: Vec::new(),
            },
        })
    }
}

// shortcut: actions leave fixtures unchanged; add in-memory transitions if demos need animation.
pub(crate) fn simulate_command(command: &Command, state: &mut AppState) -> bool {
    let action = match command {
        Command::Dispatch(action) => {
            state.pending_actions.remove(&action.project_id());
            state
                .in_flight
                .remove(&(action.project_id(), action.item_id()));
            action.describe()
        }
        Command::CreateTag { tag_name, .. } => {
            state.overlay = Overlay::None;
            format!("create tag {tag_name}")
        }
        Command::DownloadArtifacts {
            project_id, job_id, ..
        } => {
            state.in_flight.remove(&(*project_id, *job_id));
            "download artifacts".into()
        }
        Command::OpenUrl { .. } => "open in browser".into(),
        Command::AddInstance => "add instance (disabled)".into(),
        _ => return false,
    };
    state.status_message = format!("Demo mode: {action} is simulated");
    true
}

#[cfg(test)]
mod tests {
    use super::InstanceRuntime;
    use crate::actions::Action;
    use crate::input::Command;
    use crate::{handle_command, handle_refresh_event, persist_dashboard_state, Dashboard};
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn demo_builds_and_renders_a_dashboard_without_a_cache_or_credentials() {
        let runtime = InstanceRuntime::demo().await.expect("demo runtime");
        assert!(runtime.client.is_demo());
        let mut dashboard = Dashboard::new(vec![runtime], false).expect("dashboard");
        assert!(dashboard.active_store().is_none());
        let (tx, mut rx) = mpsc::channel(8);
        dashboard.start_full_refresh(&tx);
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("offline refresh")
            .expect("event");
        assert!(handle_refresh_event(event, &mut dashboard, &tx).is_none());
        assert_eq!(dashboard.state.items.len(), 5);
        assert!(dashboard
            .state
            .items
            .iter()
            .all(|item| !item.has_error() && !item.jobs.is_empty()));
        persist_dashboard_state(&dashboard.state, dashboard.active_store());
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40))
            .expect("test terminal");
        terminal
            .draw(|frame| crate::render::render(frame, &dashboard.state))
            .expect("render");
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("DEMO"));
        assert!(screen.contains("acme/payments-api"));
    }

    #[tokio::test]
    async fn demo_actions_are_simulated_without_spawning_side_effects() {
        let mut dashboard =
            Dashboard::new(vec![InstanceRuntime::demo().await.expect("demo")], false)
                .expect("dashboard");
        let (tx, mut rx) = mpsc::channel(8);
        let action = Action::RetryJob {
            project_id: 1,
            job_id: 1052,
            job_name: "integration:postgres".into(),
        };
        dashboard.state.pending_actions.insert(1);
        dashboard.state.mark_in_flight(1, 1052);
        assert!(handle_command(Command::Dispatch(action), &mut dashboard, &tx).is_none());
        assert!(dashboard.state.pending_actions.is_empty());
        assert!(dashboard.state.in_flight.is_empty());
        assert_eq!(
            dashboard.state.status_message,
            "Demo mode: Retry job 'integration:postgres' is simulated"
        );
        for command in [
            Command::OpenUrl {
                url: "https://demo.gitlab.example".into(),
            },
            Command::AddInstance,
            Command::CreateTag {
                project_id: 1,
                tag_name: "v2.0".into(),
                ref_: "main".into(),
                message: String::new(),
            },
            Command::DownloadArtifacts {
                project_id: 1,
                job_id: 1050,
                job_name: "build".into(),
                filename: "artifacts.zip".into(),
            },
        ] {
            assert!(handle_command(command, &mut dashboard, &tx).is_none());
            assert!(dashboard.state.status_message.starts_with("Demo mode:"));
        }
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }
}
