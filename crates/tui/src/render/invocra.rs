//! Ritual composition shared by the startup wizard and the empty log pane.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

const ART: &str = include_str!("invocra.txt");
pub(crate) const ART_WIDTH: u16 = 85;
pub(crate) const ART_HEIGHT: u16 = 13;
const TITLE: &str = "Invocra";
const HINT: &str = "Select a job in the pipeline panel and press Enter to view its log.";
pub(crate) const IVORY: Color = Color::Rgb(0xE8, 0xE4, 0xDC);
pub(crate) const BACKGROUND: Color = Color::Rgb(0x0B, 0x0D, 0x10);
const MUTED: Color = Color::Rgb(0x9A, 0x97, 0x88);

pub(crate) fn render_placeholder(frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(Block::default().style(Style::default().bg(BACKGROUND)), area);
    if area.width >= ART_WIDTH && area.height >= ART_HEIGHT {
        let hint_rows = if area.height >= ART_HEIGHT + 2 { 2 } else { 0 };
        let y = area.y + (area.height - ART_HEIGHT - hint_rows) / 2;
        let art_area = Rect::new(area.x + (area.width - ART_WIDTH) / 2, y, ART_WIDTH, ART_HEIGHT);
        // Paragraphs do not wrap unless Wrap is set; leading spaces position the whole drawing.
        frame.render_widget(Paragraph::new(ART).style(Style::default().fg(IVORY)), art_area);
        if hint_rows > 0 {
            render_hint(frame, Rect::new(area.x, y + ART_HEIGHT + 1, area.width, 1));
        }
    } else if area.width >= TITLE.len() as u16 {
        let y = area.y + (area.height - 1) / 2;
        frame.render_widget(
            Paragraph::new(TITLE)
                .alignment(Alignment::Center)
                .style(Style::default().fg(IVORY).add_modifier(Modifier::BOLD)),
            Rect::new(area.x, y, area.width, 1),
        );
    }
}

fn render_hint(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new(HINT)
            .alignment(Alignment::Center)
            .style(Style::default().fg(MUTED)),
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
        assert!(ART.chars().all(|ch| ch.is_ascii() || ('\u{2800}'..='\u{28ff}').contains(&ch)));
        assert_eq!(ART.lines().count(), usize::from(ART_HEIGHT));
        let mut terminal = Terminal::new(TestBackend::new(ART_WIDTH, ART_HEIGHT)).expect("terminal");
        terminal.draw(|frame| render_placeholder(frame, frame.area())).expect("draw");
        let buffer = terminal.backend().buffer();
        for (y, line) in ART.lines().enumerate() {
            assert_eq!(ratatui::text::Line::from(line).width(), usize::from(ART_WIDTH));
            let actual: String = (0..ART_WIDTH).map(|x| buffer[(x, y as u16)].symbol()).collect();
            assert_eq!(actual, line, "row {y}");
            for x in 0..ART_WIDTH {
                assert_eq!(buffer[(x, y as u16)].fg, IVORY);
                assert_eq!(buffer[(x, y as u16)].bg, BACKGROUND);
            }
        }
    }

    #[test]
    fn art_is_centered_as_a_block_and_small_panes_use_invocra_title() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal.draw(|frame| render_placeholder(frame, frame.area())).expect("draw");
        let buffer = terminal.backend().buffer();
        for (y, line) in ART.lines().enumerate() {
            let actual: String = (0..ART_WIDTH).map(|x| buffer[(x + 7, y as u16 + 7)].symbol()).collect();
            assert_eq!(actual, line, "row {y}");
        }
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).expect("terminal");
        terminal.draw(|frame| render_placeholder(frame, frame.area())).expect("draw");
        let row: String = (0..60).map(|x| terminal.backend().buffer()[(x, 9)].symbol()).collect();
        assert_eq!(row.trim(), TITLE);
    }

    #[test]
    fn tiny_and_offset_areas_render_without_writing_outside_the_pane() {
        for (width, height) in [(0, 0), (1, 1), (7, 1), (10, 4), (40, 12), (84, 13), (85, 12), (85, 13), (85, 15)] {
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
