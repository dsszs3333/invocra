//! Reusable filterable multi-select list, shared by the onboarding wizard and the standalone
//! `invocra config discover` project picker.
//!
//! [`PickList`] owns the selection/cursor/filter logic (unit-tested here and via the onboarding
//! tests); [`run_project_picker`] wraps it in a small standalone terminal loop so the CLI doesn't
//! re-implement its own picker.

use std::io::{self, Stdout};

use anyhow::Result;
use crossterm::event::{self, Event as CEvent, KeyCode, KeyEventKind};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};

use crate::{restore_terminal, TerminalModeGuard};

/// A selectable row in a [`PickList`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PickItem {
    pub(crate) id: u64,
    /// Full path — shown to the user and written to the config.
    pub(crate) label: String,
    pub(crate) selected: bool,
}

/// A filterable multi-select list with its own cursor (position within the visible subset).
#[derive(Debug, Default)]
pub(crate) struct PickList {
    pub(crate) items: Vec<PickItem>,
    pub(crate) cursor: usize,
    pub(crate) filter: String,
    pub(crate) filtering: bool,
}

impl PickList {
    pub(crate) fn from_labels(items: Vec<PickItem>) -> Self {
        Self {
            items,
            cursor: 0,
            filter: String::new(),
            filtering: false,
        }
    }

    /// Builds a list from `(id, label)` pairs, pre-selecting any whose id is in `preselected_ids`.
    pub(crate) fn with_preselected(items: &[(u64, String)], preselected_ids: &[u64]) -> Self {
        Self::from_labels(
            items
                .iter()
                .map(|(id, label)| PickItem {
                    id: *id,
                    label: label.clone(),
                    selected: preselected_ids.contains(id),
                })
                .collect(),
        )
    }

    pub(crate) fn visible_indices(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| needle.is_empty() || item.label.to_lowercase().contains(&needle))
            .map(|(index, _)| index)
            .collect()
    }

    pub(crate) fn clamp_cursor(&mut self) {
        let visible = self.visible_indices().len();
        if visible == 0 {
            self.cursor = 0;
        } else if self.cursor >= visible {
            self.cursor = visible - 1;
        }
    }

    pub(crate) fn move_cursor(&mut self, delta: isize) {
        let visible = self.visible_indices().len();
        if visible == 0 {
            return;
        }
        self.cursor = self.cursor.saturating_add_signed(delta).min(visible - 1);
    }

    pub(crate) fn toggle_current(&mut self) {
        let visible = self.visible_indices();
        if let Some(&index) = visible.get(self.cursor) {
            self.items[index].selected = !self.items[index].selected;
        }
    }

    pub(crate) fn select_all_visible(&mut self) {
        for index in self.visible_indices() {
            self.items[index].selected = true;
        }
    }

    pub(crate) fn clear_selection(&mut self) {
        for item in &mut self.items {
            item.selected = false;
        }
    }

    pub(crate) fn selected_count(&self) -> usize {
        self.items.iter().filter(|item| item.selected).count()
    }

    pub(crate) fn selected(&self) -> impl Iterator<Item = &PickItem> {
        self.items.iter().filter(|item| item.selected)
    }

    /// Ids of the selected items, in list order.
    pub(crate) fn selected_ids(&self) -> Vec<u64> {
        self.selected().map(|item| item.id).collect()
    }

    /// Applies a keypress while the `/` filter input is active.
    pub(crate) fn apply_filter_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.filter.clear();
                self.filtering = false;
                self.clamp_cursor();
            }
            KeyCode::Enter => self.filtering = false,
            KeyCode::Backspace => {
                self.filter.pop();
                self.cursor = 0;
            }
            KeyCode::Char(ch) => {
                self.filter.push(ch);
                self.cursor = 0;
            }
            _ => {}
        }
    }
}

/// Outcome of one keypress in the standalone picker loop.
enum PickerKey {
    Continue,
    Done,
    Cancel,
}

fn apply_picker_key(list: &mut PickList, code: KeyCode) -> PickerKey {
    if list.filtering {
        list.apply_filter_key(code);
        return PickerKey::Continue;
    }

    match code {
        KeyCode::Esc | KeyCode::Char('q') => return PickerKey::Cancel,
        KeyCode::Enter => return PickerKey::Done,
        KeyCode::Char('j') | KeyCode::Down => list.move_cursor(1),
        KeyCode::Char('k') | KeyCode::Up => list.move_cursor(-1),
        KeyCode::Char(' ') => list.toggle_current(),
        KeyCode::Char('a') => list.select_all_visible(),
        KeyCode::Char('c') => list.clear_selection(),
        KeyCode::Char('/') => list.filtering = true,
        _ => {}
    }
    PickerKey::Continue
}

/// Runs a standalone multi-select picker over `items` (`(id, label)` pairs), pre-selecting
/// `preselected_ids`. Returns the chosen ids, or `None` if the user cancels.
///
/// # Errors
///
/// Returns an error if the terminal cannot be set up or drawn to.
pub fn run_project_picker(
    title: &str,
    items: &[(u64, String)],
    preselected_ids: &[u64],
) -> Result<Option<Vec<u64>>> {
    let mut mode_guard = TerminalModeGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let outcome = run_picker_loop(&mut terminal, title, items, preselected_ids);

    restore_terminal(&mut terminal)?;
    mode_guard.restore()?;
    outcome
}

fn run_picker_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    title: &str,
    items: &[(u64, String)],
    preselected_ids: &[u64],
) -> Result<Option<Vec<u64>>> {
    let mut list = PickList::with_preselected(items, preselected_ids);

    loop {
        terminal.draw(|frame| render(frame, title, &list))?;

        if let CEvent::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match apply_picker_key(&mut list, key.code) {
                PickerKey::Continue => {}
                PickerKey::Done => return Ok(Some(list.selected_ids())),
                PickerKey::Cancel => return Ok(None),
            }
        }
    }
}

fn render(frame: &mut Frame, title: &str, list: &PickList) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let header = format!("{title}    selected: {}", list.selected_count());
    frame.render_widget(
        Paragraph::new(header).style(Style::default().add_modifier(Modifier::BOLD)),
        chunks[0],
    );

    render_pick_list(frame, chunks[1], list);

    let footer = if list.filtering {
        Paragraph::new(format!("/{}▌  (Enter apply · Esc clear)", list.filter))
            .style(Style::default().fg(Color::Yellow))
    } else {
        Paragraph::new("space select  a all  c clear  / filter  enter save  q cancel")
            .style(Style::default().add_modifier(Modifier::DIM))
    };
    frame.render_widget(footer, chunks[2]);
}

/// Renders a [`PickList`] as a `[x]/[ ] label` checklist. Shared by this CLI picker and the
/// onboarding wizard.
pub(crate) fn render_pick_list(frame: &mut Frame, area: Rect, list: &PickList) {
    let visible = list.visible_indices();
    if visible.is_empty() {
        frame.render_widget(Paragraph::new("(nothing to show)"), area);
        return;
    }

    let rows: Vec<ListItem> = visible
        .iter()
        .map(|&index| {
            let item = &list.items[index];
            let marker = if item.selected { "[x]" } else { "[ ]" };
            ListItem::new(format!("{marker} {}", item.label))
        })
        .collect();

    let widget = List::new(rows).highlight_style(Style::default().bg(Color::Blue).fg(Color::Black));
    let mut list_state = ListState::default();
    list_state.select(Some(list.cursor.min(visible.len() - 1)));
    frame.render_stateful_widget(widget, area, &mut list_state);
}

#[cfg(test)]
mod tests {
    use super::PickList;

    fn items() -> Vec<(u64, String)> {
        vec![
            (10, String::from("group/api")),
            (20, String::from("group/frontend")),
        ]
    }

    #[test]
    fn with_preselected_marks_matching_ids() {
        let list = PickList::with_preselected(&items(), &[20]);
        assert_eq!(list.selected_ids(), vec![20]);
    }

    #[test]
    fn toggle_and_collect_ids() {
        let mut list = PickList::with_preselected(&items(), &[]);
        list.toggle_current();
        assert_eq!(list.selected_ids(), vec![10]);
        list.toggle_current();
        assert!(list.selected_ids().is_empty());
    }

    #[test]
    fn filter_then_select_all_only_touches_visible() {
        let mut list = PickList::with_preselected(&items(), &[]);
        list.filtering = true;
        for ch in "front".chars() {
            list.apply_filter_key(crossterm::event::KeyCode::Char(ch));
        }
        assert_eq!(list.visible_indices().len(), 1);
        list.select_all_visible();
        assert_eq!(list.selected_ids(), vec![20]);
    }
}
