//! Empty log-pane splash; keep the wordmark intact and fall back on small terminals.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

const ART: &str = include_str!("invocra.txt");
const ART_WIDTH: u16 = 88;
const ART_HEIGHT: u16 = 24;
const TITLE: &str = "Invocra";
const HINT: &str = "Select a job in the pipeline panel and press Enter to view its log.";
const AMBER: Color = Color::Rgb(0xCF, 0x9B, 0x34);
const INK: Color = Color::Rgb(0x33, 0x31, 0x2A);
const MUTED: Color = Color::Rgb(0x9A, 0x97, 0x88);

pub(super) fn render_placeholder(frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if area.width >= ART_WIDTH && area.height >= ART_HEIGHT {
        let hint_rows = if area.height >= ART_HEIGHT + 2 { 2 } else { 0 };
        let y = area.y + (area.height - ART_HEIGHT - hint_rows) / 2;
        let art_area = Rect::new(area.x + (area.width - ART_WIDTH) / 2, y, ART_WIDTH, ART_HEIGHT);
        frame.render_widget(Paragraph::new(ART).style(Style::default().fg(INK)), art_area);
        if hint_rows > 0 {
            render_hint(frame, Rect::new(area.x, y + ART_HEIGHT + 1, area.width, 1));
        }
    } else if area.width >= TITLE.len() as u16 && area.height >= 3 {
        let y = area.y + (area.height - 3) / 2;
        frame.render_widget(
            Paragraph::new(TITLE)
                .alignment(Alignment::Center)
                .style(Style::default().fg(AMBER).add_modifier(Modifier::BOLD)),
            Rect::new(area.x, y, area.width, 1),
        );
        render_hint(frame, Rect::new(area.x, y + 2, area.width, area.bottom() - y - 2));
    } else {
        render_hint(frame, area);
    }
}

fn render_hint(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new(HINT)
            .alignment(Alignment::Center)
            .style(Style::default().fg(MUTED))
            .wrap(Wrap { trim: true }),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn supplied_art_is_rendered_without_clipping_or_line_realignment() {
        assert!(ART.is_ascii());
        assert_eq!(ART.lines().count(), usize::from(ART_HEIGHT));
        let mut terminal = Terminal::new(TestBackend::new(ART_WIDTH, ART_HEIGHT)).expect("terminal");
        terminal.draw(|frame| render_placeholder(frame, frame.area())).expect("draw");
        let buffer = terminal.backend().buffer();
        for (y, line) in ART.lines().enumerate() {
            assert_eq!(line.len(), usize::from(ART_WIDTH));
            let actual: String = (0..ART_WIDTH).map(|x| buffer[(x, y as u16)].symbol()).collect();
            assert_eq!(actual, line, "row {y}");
        }
    }

    #[test]
    fn art_is_centered_as_a_block_and_small_panes_use_invocra_title() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal.draw(|frame| render_placeholder(frame, frame.area())).expect("draw");
        let buffer = terminal.backend().buffer();
        for (y, line) in ART.lines().enumerate() {
            let actual: String = (0..ART_WIDTH).map(|x| buffer[(x + 6, y as u16 + 2)].symbol()).collect();
            assert_eq!(actual, line, "row {y}");
        }
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).expect("terminal");
        terminal.draw(|frame| render_placeholder(frame, frame.area())).expect("draw");
        let row: String = (0..60).map(|x| terminal.backend().buffer()[(x, 8)].symbol()).collect();
        assert_eq!(row.trim(), TITLE);
    }

    #[test]
    fn tiny_and_offset_areas_render_without_writing_outside_the_pane() {
        for (width, height) in [(0, 0), (1, 1), (10, 4), (40, 12), (87, 24), (88, 23), (88, 25)] {
            let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
            terminal.draw(|frame| render_placeholder(frame, Rect::new(3, 2, width, height))).expect("draw");
            let buffer = terminal.backend().buffer();
            for y in 0..30 {
                for x in 0..100 {
                    if !Rect::new(3, 2, width, height).contains((x, y).into()) {
                        assert_eq!(buffer[(x, y)].symbol(), " ");
                    }
                }
            }
        }
    }
}
