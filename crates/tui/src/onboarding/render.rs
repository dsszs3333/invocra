use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use super::state::{OnboardingState, Step, Tab};
use crate::render::invocra;

pub(super) fn render(frame: &mut Frame, state: &OnboardingState) {
    frame.render_widget(
        Block::default().style(Style::default().fg(invocra::IVORY).bg(invocra::BACKGROUND)),
        frame.area(),
    );
    match state.step {
        Step::Url => render_field(frame, state, "GitLab URL", &state.url, false),
        Step::Token => {
            render_field(
                frame,
                state,
                "Personal Access Token",
                &state.masked_token(),
                true,
            );
        }
        Step::Validating => render_message(frame, "Checking token and loading projects…"),
        Step::Select => render_select(frame, state),
        Step::AddMore => render_message(
            frame,
            "Instance added.\n\n[a] add another instance     [Enter] finish",
        ),
    }
}

fn render_field(frame: &mut Frame, state: &OnboardingState, label: &str, value: &str, last: bool) {
    let screen = frame.area();
    let form_height = 11;
    let art_height = if screen.width >= invocra::ART_WIDTH
        && screen.height > invocra::ART_HEIGHT + form_height {
        invocra::ART_HEIGHT
    } else {
        1
    };
    let content_height = art_height + form_height + 1;
    let form_area = if screen.height >= content_height {
        let top = screen.y + (screen.height - content_height) / 2;
        invocra::render_placeholder(frame, Rect::new(screen.x, top, screen.width, art_height));
        Rect::new(screen.x, top + art_height + 1, screen.width, form_height)
    } else {
        screen
    };
    let area = crate::render::util::centered_rect(70, Constraint::Length(form_height), form_area);
    let block = Block::default()
        .title("Invocra — first run")
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let step_no = if last { "Step 2/3" } else { "Step 1/3" };
    let hint = if last {
        "Token needs the `api` scope. Create one at GitLab → Settings → Access Tokens."
    } else {
        "Press Enter for the default (https://gitlab.com)."
    };

    let mut lines = vec![
        Line::from(Span::styled(
            step_no,
            Style::default().add_modifier(Modifier::DIM),
        )),
        Line::from(""),
        Line::from(format!("{label}:")),
        Line::from(Span::styled(
            format!("> {value}▌"),
            Style::default().fg(Color::Yellow),
        )),
        Line::from(""),
        Line::from(Span::styled(
            hint,
            Style::default().add_modifier(Modifier::DIM),
        )),
    ];
    if let Some(error) = &state.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.as_str(),
            Style::default().fg(Color::Red),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter continue    Esc back",
        Style::default().add_modifier(Modifier::DIM),
    )));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn render_message(frame: &mut Frame, message: &str) {
    let area = crate::render::util::centered_rect(60, Constraint::Length(7), frame.area());
    let block = Block::default()
        .title("Invocra — first run")
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(message).wrap(Wrap { trim: true }), inner);
}

fn render_select(frame: &mut Frame, state: &OnboardingState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    frame.render_widget(tabs_line(state), chunks[0]);

    let active = match state.tab {
        Tab::Groups => &state.groups,
        Tab::Projects => &state.projects,
    };
    crate::picker::render_pick_list(frame, chunks[1], active);

    let footer = if active.filtering {
        Paragraph::new(format!("/{}▌  (Enter apply · Esc clear)", active.filter))
            .style(Style::default().fg(Color::Yellow))
    } else if let Some(error) = &state.error {
        Paragraph::new(error.as_str()).style(Style::default().fg(Color::Red))
    } else {
        Paragraph::new("space select  a all  c clear  / filter  tab switch list  enter add instance  esc cancel")
            .style(Style::default().add_modifier(Modifier::DIM))
    };
    frame.render_widget(footer, chunks[2]);
}

fn tabs_line(state: &OnboardingState) -> Paragraph<'static> {
    let groups = format!(" Groups: {} ", state.groups.selected_count());
    let projects = format!(" Projects: {} ", state.projects.selected_count());
    let active = Style::default().bg(Color::Green).fg(Color::Black);
    let idle = Style::default().add_modifier(Modifier::DIM);
    let (gs, ps) = match state.tab {
        Tab::Groups => (active, idle),
        Tab::Projects => (idle, active),
    };
    Paragraph::new(Line::from(vec![
        Span::styled(groups, gs),
        Span::raw("  "),
        Span::styled(projects, ps),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn startup_keeps_the_form_below_the_art_and_uses_a_compact_header_when_it_cannot_fit() {
        let state = OnboardingState::new("https://gitlab.com".into());
        for (width, height, full_art) in [(120, 36, true), (85, 25, true), (84, 25, false), (85, 24, false)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            terminal.draw(|frame| render(frame, &state)).expect("draw startup");
            let buffer = terminal.backend().buffer();
            let rows: Vec<String> = (0..height)
                .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
                .collect();
            assert!(rows.iter().any(|row| row.contains("Step 1/3")));
            assert!(rows.iter().any(|row| row.contains("https://gitlab.com")));
            if full_art {
                let top = usize::from((height - 25) / 2);
                let left = usize::from((width - invocra::ART_WIDTH) / 2);
                for (y, line) in include_str!("../render/invocra.txt").lines().enumerate() {
                    let actual: String = rows[top + y].chars().skip(left).take(usize::from(invocra::ART_WIDTH)).collect();
                    assert_eq!(actual, line, "startup artwork row {y}");
                }
            } else {
                assert!(rows.iter().any(|row| row.trim() == "Invocra"));
                assert!(!rows.iter().flat_map(|row| row.chars()).any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch)));
            }
        }
    }
}
