//! Ritual composition shared by the startup wizard and the empty log pane.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

const ART: &str = include_str!("invocra.txt");
const WORDMARK: &str = include_str!("invocra-wordmark.txt");
pub(crate) const ART_WIDTH: u16 = 85;
pub(crate) const ART_HEIGHT: u16 = 13;
pub(crate) const WORDMARK_WIDTH: u16 = 28;
pub(crate) const WORDMARK_HEIGHT: u16 = 4;
const TITLE: &str = "Invocra";
const HINT: &str = "Select a job in the pipeline panel and press Enter to view its log.";
const MUTED: Color = Color::Rgb(0x9A, 0x97, 0x88);

pub(crate) fn render_placeholder(frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(Block::default().style(Style::reset()), area);
    let (art, width, height) = if area.width >= ART_WIDTH && area.height >= ART_HEIGHT {
        (ART, ART_WIDTH, ART_HEIGHT)
    } else if area.width >= WORDMARK_WIDTH && area.height >= WORDMARK_HEIGHT {
        (WORDMARK, WORDMARK_WIDTH, WORDMARK_HEIGHT)
    } else {
        if usize::from(area.width) >= TITLE.len() {
            let y = area.y + (area.height - 1) / 2;
            frame.render_widget(
                Paragraph::new(TITLE)
                    .alignment(Alignment::Center)
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                Rect::new(area.x, y, area.width, 1),
            );
        }
        return;
    };
    let hint_rows = if width == ART_WIDTH && area.height >= ART_HEIGHT + 2 {
        2
    } else {
        0
    };
    let y = area.y + (area.height - height - hint_rows) / 2;
    let art_area = Rect::new(area.x + (area.width - width) / 2, y, width, height);
    // Paragraphs do not wrap unless Wrap is set; leading spaces position the whole drawing.
    frame.render_widget(Paragraph::new(art), art_area);
    if hint_rows > 0 {
        render_hint(frame, Rect::new(area.x, y + height + 1, area.width, 1));
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
        assert!(ART
            .chars()
            .all(|ch| ch.is_ascii() || ('\u{2800}'..='\u{28ff}').contains(&ch)));
        assert_eq!(ART.lines().count(), usize::from(ART_HEIGHT));
        let mut terminal =
            Terminal::new(TestBackend::new(ART_WIDTH, ART_HEIGHT)).expect("terminal");
        terminal
            .draw(|frame| render_placeholder(frame, frame.area()))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        for (y, line) in (0_u16..).zip(ART.lines()) {
            assert_eq!(
                ratatui::text::Line::from(line).width(),
                usize::from(ART_WIDTH)
            );
            let actual: String = (0..ART_WIDTH).map(|x| buffer[(x, y)].symbol()).collect();
            assert_eq!(actual, line, "row {y}");
            for x in 0..ART_WIDTH {
                assert_eq!(buffer[(x, y)].fg, Color::Reset);
                assert_eq!(buffer[(x, y)].bg, Color::Reset);
            }
        }
    }

    #[test]
    fn art_is_centered_as_a_block() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal
            .draw(|frame| render_placeholder(frame, frame.area()))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        for (y, line) in (0_u16..).zip(ART.lines()) {
            let actual: String = (0..ART_WIDTH)
                .map(|x| buffer[(x + 7, y + 7)].symbol())
                .collect();
            assert_eq!(actual, line, "row {y}");
        }
    }

    #[test]
    fn smaller_panes_show_the_complete_centered_gothic_wordmark_without_the_scene() {
        let original: Vec<String> = ART
            .lines()
            .skip(5)
            .take(4)
            .map(|line| {
                line.chars()
                    .skip(46)
                    .take(usize::from(WORDMARK_WIDTH))
                    .collect()
            })
            .collect();
        assert_eq!(WORDMARK.lines().collect::<Vec<_>>(), original);
        for (width, height) in [(84, 13), (85, 12), (28, 4), (40, 12), (60, 20)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            terminal
                .draw(|frame| render_placeholder(frame, frame.area()))
                .expect("draw");
            let left = (width - WORDMARK_WIDTH) / 2;
            let top = (height - WORDMARK_HEIGHT) / 2;
            for y in 0..height {
                let expected = if (top..top + WORDMARK_HEIGHT).contains(&y) {
                    WORDMARK
                        .lines()
                        .nth(usize::from(y - top))
                        .expect("wordmark row")
                } else {
                    ""
                };
                for x in 0..width {
                    let cell = &terminal.backend().buffer()[(x, y)];
                    let ch = x
                        .checked_sub(left)
                        .and_then(|dx| expected.chars().nth(usize::from(dx)))
                        .unwrap_or(' ');
                    assert_eq!(
                        cell.symbol(),
                        ch.to_string(),
                        "{width}×{height} cell {x},{y}"
                    );
                    assert_eq!((cell.fg, cell.bg), (Color::Reset, Color::Reset));
                }
            }
        }
    }

    #[test]
    fn panes_too_small_for_the_gothic_wordmark_show_a_plain_title() {
        for (width, height) in [(27, 4), (28, 3), (7, 1), (40, 3)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            terminal
                .draw(|frame| render_placeholder(frame, frame.area()))
                .expect("draw");
            let buffer = terminal.backend().buffer();
            let row: String = (0..width)
                .map(|x| buffer[(x, (height - 1) / 2)].symbol())
                .collect();
            assert_eq!(row.trim(), TITLE);
            assert!(!buffer.content().iter().any(|cell| cell
                .symbol()
                .chars()
                .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch))));
        }
    }

    #[test]
    fn tiny_and_offset_areas_render_without_writing_outside_the_pane() {
        for (width, height) in [
            (0, 0),
            (1, 1),
            (7, 1),
            (10, 4),
            (40, 12),
            (84, 13),
            (85, 12),
            (85, 13),
            (85, 15),
        ] {
            let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
            terminal
                .draw(|frame| render_placeholder(frame, Rect::new(3, 2, width, height)))
                .expect("draw");
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
