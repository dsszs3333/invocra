use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use super::state::{OnboardingState, Step, Tab};

pub(super) fn render(frame: &mut Frame, state: &OnboardingState) {
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
    let area = crate::render::util::centered_rect(70, Constraint::Length(11), frame.area());
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
