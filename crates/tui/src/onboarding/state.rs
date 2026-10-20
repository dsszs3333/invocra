use config::{GroupEntry, ProjectEntry};
use crossterm::event::{KeyCode, KeyEvent};
use gitlab::{AccessibleGroup, AccessibleProject, GitLabClient};

use crate::picker::{PickItem, PickList};

pub(super) const DEFAULT_GITLAB_URL: &str = "https://gitlab.com";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    Url,
    Token,
    Validating,
    Select,
    /// After committing an instance: offer to add another or finish.
    AddMore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tab {
    Groups,
    Projects,
}

/// Intent emitted by key handling for the wizard's async loop to carry out.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum OnboardingCommand {
    Validate {
        url: String,
        token: String,
    },
    /// Commit the instance just configured, then ask whether to add another.
    Commit,
    /// Reset the wizard to configure another instance.
    AddAnother,
    /// Finish the wizard, keeping every committed instance.
    Finish,
    Cancel,
}

pub(super) struct OnboardingState {
    pub(super) step: Step,
    pub(super) url: String,
    token: String,
    pub(super) error: Option<String>,
    pub(super) tab: Tab,
    pub(super) groups: PickList,
    pub(super) projects: PickList,
    pub(super) client: Option<GitLabClient>,
}

impl OnboardingState {
    pub(super) fn new(start_url: String) -> Self {
        Self {
            step: Step::Url,
            url: start_url,
            token: String::new(),
            error: None,
            tab: Tab::Groups,
            groups: PickList::default(),
            projects: PickList::default(),
            client: None,
        }
    }

    pub(super) fn token_input(&self) -> &str {
        &self.token
    }

    pub(super) fn gitlab_url(&self) -> &str {
        &self.url
    }

    /// Masked token for display (length preserved so the user sees their typing register).
    pub(super) fn masked_token(&self) -> String {
        "•".repeat(self.token.chars().count())
    }

    fn normalized_url(&self) -> String {
        let trimmed = self.url.trim();
        if trimmed.is_empty() {
            DEFAULT_GITLAB_URL.to_string()
        } else {
            trimmed.to_string()
        }
    }

    /// Resets the wizard to configure the next instance, after the previous one was committed.
    pub(super) fn reset_for_next(&mut self) {
        self.step = Step::Url;
        self.url = DEFAULT_GITLAB_URL.to_string();
        self.token.clear();
        self.error = None;
        self.tab = Tab::Groups;
        self.groups = PickList::default();
        self.projects = PickList::default();
        self.client = None;
    }

    pub(super) fn enter_selection(
        &mut self,
        groups: &[AccessibleGroup],
        projects: &[AccessibleProject],
    ) {
        self.groups = PickList::from_labels(
            groups
                .iter()
                .map(|group| PickItem {
                    id: group.id,
                    label: group.full_path.clone(),
                    selected: false,
                })
                .collect(),
        );
        self.projects = PickList::from_labels(
            projects
                .iter()
                .map(|project| PickItem {
                    id: project.id,
                    label: project.path_with_namespace.clone(),
                    selected: false,
                })
                .collect(),
        );
        self.tab = Tab::Groups;
        self.step = Step::Select;
        self.error = None;
    }

    /// Builds the config entries from the current selection.
    pub(super) fn collect_selection(&self) -> (Vec<ProjectEntry>, Vec<GroupEntry>) {
        let projects = self
            .projects
            .selected()
            .map(|item| ProjectEntry {
                name: item.label.clone(),
                id: item.id,
            })
            .collect();
        let groups = self
            .groups
            .selected()
            .map(|item| GroupEntry {
                id: None,
                path: Some(item.label.clone()),
                include_subgroups: true,
            })
            .collect();
        (projects, groups)
    }

    fn active_list(&mut self) -> &mut PickList {
        match self.tab {
            Tab::Groups => &mut self.groups,
            Tab::Projects => &mut self.projects,
        }
    }

    pub(super) fn apply_key(&mut self, key: &KeyEvent) -> Option<OnboardingCommand> {
        match self.step {
            Step::Url => self.handle_url_key(key.code),
            Step::Token => self.handle_token_key(key.code),
            Step::Validating => None,
            Step::Select => self.handle_select_key(key.code),
            Step::AddMore => self.handle_addmore_key(key.code),
        }
    }

    fn handle_addmore_key(&mut self, code: KeyCode) -> Option<OnboardingCommand> {
        match code {
            KeyCode::Char('a' | 'A') => Some(OnboardingCommand::AddAnother),
            KeyCode::Enter | KeyCode::Esc => Some(OnboardingCommand::Finish),
            _ => None,
        }
    }

    fn handle_url_key(&mut self, code: KeyCode) -> Option<OnboardingCommand> {
        match code {
            KeyCode::Esc => return Some(OnboardingCommand::Cancel),
            KeyCode::Enter => {
                let url = self.normalized_url();
                // The wizard only accepts https. A plaintext-http instance is an advanced case:
                // it must be added by hand in projects.toml with `allow_insecure_http = true`.
                if let Err(err) = config::check_url_scheme(&url, false) {
                    self.error = Some(err.to_string());
                    return None;
                }
                self.url = url;
                self.step = Step::Token;
                self.error = None;
            }
            KeyCode::Backspace => {
                self.url.pop();
            }
            KeyCode::Char(ch) => self.url.push(ch),
            _ => {}
        }
        None
    }

    fn handle_token_key(&mut self, code: KeyCode) -> Option<OnboardingCommand> {
        match code {
            KeyCode::Esc => {
                self.step = Step::Url;
                self.error = None;
            }
            KeyCode::Enter => {
                if self.token.trim().is_empty() {
                    self.error = Some(String::from("Enter a Personal Access Token"));
                    return None;
                }
                self.url = self.normalized_url();
                return Some(OnboardingCommand::Validate {
                    url: self.url.clone(),
                    token: self.token.trim().to_string(),
                });
            }
            KeyCode::Backspace => {
                self.token.pop();
            }
            KeyCode::Char(ch) => self.token.push(ch),
            _ => {}
        }
        None
    }

    fn handle_select_key(&mut self, code: KeyCode) -> Option<OnboardingCommand> {
        if self.active_list().filtering {
            self.handle_filter_key(code);
            return None;
        }

        match code {
            KeyCode::Esc => return Some(OnboardingCommand::Cancel),
            KeyCode::Enter => {
                if self.groups.selected_count() + self.projects.selected_count() == 0 {
                    self.error = Some(String::from("Select at least one group or project"));
                    return None;
                }
                return Some(OnboardingCommand::Commit);
            }
            KeyCode::Tab | KeyCode::Left | KeyCode::Right | KeyCode::Char('h' | 'l') => {
                self.tab = match self.tab {
                    Tab::Groups => Tab::Projects,
                    Tab::Projects => Tab::Groups,
                };
            }
            KeyCode::Char('j') | KeyCode::Down => self.active_list().move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.active_list().move_cursor(-1),
            KeyCode::Char(' ') => self.active_list().toggle_current(),
            KeyCode::Char('a') => self.active_list().select_all_visible(),
            KeyCode::Char('c') => self.active_list().clear_selection(),
            KeyCode::Char('/') => {
                self.active_list().filtering = true;
            }
            _ => {}
        }
        None
    }

    fn handle_filter_key(&mut self, code: KeyCode) {
        self.active_list().apply_filter_key(code);
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent};
    use gitlab::{AccessibleGroup, AccessibleProject};

    use super::{OnboardingCommand, OnboardingState, Step, Tab};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    fn press(state: &mut OnboardingState, code: KeyCode) -> Option<OnboardingCommand> {
        state.apply_key(&key(code))
    }

    fn type_str(state: &mut OnboardingState, text: &str) {
        for ch in text.chars() {
            press(state, KeyCode::Char(ch));
        }
    }

    fn sample_groups() -> Vec<AccessibleGroup> {
        vec![
            AccessibleGroup {
                id: 1,
                name: "Backend".into(),
                full_path: "org/backend".into(),
            },
            AccessibleGroup {
                id: 2,
                name: "Frontend".into(),
                full_path: "org/frontend".into(),
            },
        ]
    }

    fn sample_projects() -> Vec<AccessibleProject> {
        vec![AccessibleProject {
            id: 10,
            name: "tool".into(),
            path_with_namespace: "other/tool".into(),
        }]
    }

    #[test]
    fn starts_on_url_step_with_default() {
        let state = OnboardingState::new("https://gitlab.com".into());
        assert_eq!(state.step, Step::Url);
        assert_eq!(state.gitlab_url(), "https://gitlab.com");
    }

    #[test]
    fn empty_url_falls_back_to_default_on_advance() {
        let mut state = OnboardingState::new(String::new());
        press(&mut state, KeyCode::Enter);
        assert_eq!(state.step, Step::Token);
        assert_eq!(state.gitlab_url(), "https://gitlab.com");
    }

    #[test]
    fn token_step_requires_non_empty_then_emits_validate() {
        let mut state = OnboardingState::new("https://gl.example".into());
        press(&mut state, KeyCode::Enter); // -> Token

        let cmd = press(&mut state, KeyCode::Enter); // empty
        assert_eq!(cmd, None);
        assert!(state.error.is_some());

        type_str(&mut state, "glpat-xyz");
        let cmd = press(&mut state, KeyCode::Enter);
        assert_eq!(
            cmd,
            Some(OnboardingCommand::Validate {
                url: "https://gl.example".into(),
                token: "glpat-xyz".into(),
            })
        );
    }

    #[test]
    fn token_is_masked_for_display() {
        let mut state = OnboardingState::new("https://u".into());
        press(&mut state, KeyCode::Enter);
        type_str(&mut state, "abc");
        assert_eq!(state.masked_token(), "•••");
    }

    #[test]
    fn url_step_rejects_non_https() {
        let mut state = OnboardingState::new("http://insecure.example".into());
        let cmd = press(&mut state, KeyCode::Enter);
        assert_eq!(cmd, None);
        assert_eq!(state.step, Step::Url);
        assert!(state.error.is_some());
    }

    #[test]
    fn esc_on_url_cancels_esc_on_token_goes_back() {
        let mut state = OnboardingState::new("https://u".into());
        press(&mut state, KeyCode::Enter); // Token
        assert_eq!(press(&mut state, KeyCode::Esc), None);
        assert_eq!(state.step, Step::Url);
        assert_eq!(
            press(&mut state, KeyCode::Esc),
            Some(OnboardingCommand::Cancel)
        );
    }

    #[test]
    fn selection_toggles_and_collects_entries() {
        let mut state = OnboardingState::new("u".into());
        state.enter_selection(&sample_groups(), &sample_projects());
        assert_eq!(state.step, Step::Select);
        assert_eq!(state.tab, Tab::Groups);

        // Select first group.
        press(&mut state, KeyCode::Char(' '));
        // Switch to projects, select the project.
        press(&mut state, KeyCode::Tab);
        assert_eq!(state.tab, Tab::Projects);
        press(&mut state, KeyCode::Char(' '));

        let (projects, groups) = state.collect_selection();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].path.as_deref(), Some("org/backend"));
        assert!(groups[0].include_subgroups);
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "other/tool");
        assert_eq!(projects[0].id, 10);
    }

    #[test]
    fn enter_on_select_requires_a_choice() {
        let mut state = OnboardingState::new("u".into());
        state.enter_selection(&sample_groups(), &sample_projects());

        let cmd = press(&mut state, KeyCode::Enter);
        assert_eq!(cmd, None);
        assert!(state.error.is_some());

        press(&mut state, KeyCode::Char(' '));
        assert_eq!(
            press(&mut state, KeyCode::Enter),
            Some(OnboardingCommand::Commit)
        );
    }

    #[test]
    fn commit_then_add_another_resets_for_next_instance() {
        let mut state = OnboardingState::new("https://gl.example".into());
        state.enter_selection(&sample_groups(), &sample_projects());
        press(&mut state, KeyCode::Char(' ')); // select a group
        assert_eq!(
            press(&mut state, KeyCode::Enter),
            Some(OnboardingCommand::Commit)
        );

        // The async loop captures the instance and moves to the add-more prompt.
        state.step = Step::AddMore;
        assert_eq!(
            press(&mut state, KeyCode::Char('a')),
            Some(OnboardingCommand::AddAnother)
        );

        state.reset_for_next();
        assert_eq!(state.step, Step::Url);
        assert_eq!(state.gitlab_url(), "https://gitlab.com");
        assert_eq!(state.groups.selected_count(), 0);

        // From the add-more prompt, Enter finishes.
        state.step = Step::AddMore;
        assert_eq!(
            press(&mut state, KeyCode::Enter),
            Some(OnboardingCommand::Finish)
        );
    }

    #[test]
    fn filter_narrows_group_list() {
        let mut state = OnboardingState::new("u".into());
        state.enter_selection(&sample_groups(), &sample_projects());

        press(&mut state, KeyCode::Char('/'));
        assert!(state.groups.filtering);
        type_str(&mut state, "front");
        assert_eq!(state.groups.visible_indices().len(), 1);

        // Toggle the single visible (filtered) item, then exit filter.
        press(&mut state, KeyCode::Enter); // leaves filter mode
        assert!(!state.groups.filtering);
        press(&mut state, KeyCode::Char(' '));

        let (_, groups) = state.collect_selection();
        assert_eq!(groups[0].path.as_deref(), Some("org/frontend"));
    }

    #[test]
    fn select_all_and_clear() {
        let mut state = OnboardingState::new("u".into());
        state.enter_selection(&sample_groups(), &sample_projects());

        press(&mut state, KeyCode::Char('a'));
        assert_eq!(state.groups.selected_count(), 2);
        press(&mut state, KeyCode::Char('c'));
        assert_eq!(state.groups.selected_count(), 0);
    }
}
