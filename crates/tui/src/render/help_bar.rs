use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::util::{cached_suffix, spinner_frame};
use crate::state::{AppState, Focus};

pub(super) fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    // A spinner sits in front of the status message while a global refresh/load is running, so the
    // "Loading projects…" / "Refreshing…" line animates like every other loading indicator.
    let status = if state.is_refreshing {
        format!("{} {}", spinner_frame(), state.status_message)
    } else {
        state.status_message.clone()
    };

    if state.focus == Focus::Log {
        let bar = Paragraph::new(format!(
            "{status}  |  j/k move  g/G top/bottom  f follow  v select  y copy  tab/esc leave  q quit"
        ));
        frame.render_widget(bar, area);
        return;
    }

    if state.search.active {
        let input = Paragraph::new(format!(
            "/{}▌  (Enter apply · Esc cancel)",
            state.search.query
        ))
        .style(Style::default().fg(Color::Yellow));
        frame.render_widget(input, area);
        return;
    }

    if state.branch_search.active {
        let input = Paragraph::new(format!(
            "branch /{}▌  (Enter apply · Esc cancel)",
            state.branch_search.query
        ))
        .style(Style::default().fg(Color::Yellow));
        frame.render_widget(input, area);
        return;
    }

    // Show the instance switch hint only when more than one instance is configured.
    let inst_hint = if state.instance_labels.len() > 1 {
        "  [/] instance"
    } else {
        ""
    };
    let hints = match state.focus {
        Focus::Projects => format!("j/k move  tab panel  / filter  g running  N run  R retry  c cancel  v vars  r refresh  a add{inst_hint}  ? help  q quit"),
        Focus::Branches => "j/k move  enter pin  u unpin  / filter  tab panel  r refresh  ? help  q quit".to_string(),
        Focus::Detail => "j/k jobs  enter logs  p play  R retry  c cancel  v vars  ? help  q quit".to_string(),
        Focus::MergeRequests => "j/k move  m scope  enter pipeline  o browser  x actions  ? help  q quit".to_string(),
        // Unreachable: the log panel renders its own hint bar above and returns early.
        Focus::Log => String::new(),
    };
    let bar = Paragraph::new(format!("{status}{}  |  {hints}", cached_suffix(state)));
    frame.render_widget(bar, area);
}
