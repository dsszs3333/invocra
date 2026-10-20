use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Scrollbar, ScrollbarOrientation, ScrollbarState};
use ratatui::Frame;

use gitlab::CiStatus;

use crate::state::AppState;

/// Centers a `percent_x`-wide, `vertical`-tall rect in `area`. A `Percentage` vertical centers
/// proportionally; any other constraint (e.g. `Length`) centers it with equal flexible margins.
pub(crate) fn centered_rect(percent_x: u16, vertical: Constraint, area: Rect) -> Rect {
    let outer = match vertical {
        Constraint::Percentage(p) => Constraint::Percentage((100 - p) / 2),
        _ => Constraint::Min(0),
    };
    let mid = Layout::vertical([outer, vertical, outer]).split(area)[1];
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(mid)[1]
}

/// Spinner frames, same set and order as lazygit (`| / - \`, all width 1). Advanced from wall-clock
/// so no per-item counter state is needed; the fast spinner tick (`SPINNER_TICK_INTERVAL`) drives a
/// redraw per frame while anything is loading. Used for in-flight rows and every "Loading…" label.
const SPINNER: [&str; 4] = ["|", "/", "-", "\\"];

/// Milliseconds per spinner frame; matched to the redraw cadence so the animation looks smooth.
const SPINNER_FRAME_MS: u128 = 80;

pub(super) fn spinner_frame() -> &'static str {
    use std::time::{SystemTime, UNIX_EPOCH};
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    SPINNER[(millis / SPINNER_FRAME_MS) as usize % SPINNER.len()]
}

pub(super) fn status_icon(status: &CiStatus) -> &'static str {
    match status {
        CiStatus::Success => "✓",
        CiStatus::Failed => "✗",
        CiStatus::Running | CiStatus::Pending => "●",
        CiStatus::Canceled => "■",
        CiStatus::Skipped => "»",
        CiStatus::Manual => "▶",
        _ => "•",
    }
}

pub(super) fn status_color(status: &CiStatus) -> Color {
    match status {
        CiStatus::Success => Color::Green,
        CiStatus::Failed => Color::Red,
        CiStatus::Running | CiStatus::Pending => Color::Yellow,
        CiStatus::Canceled => Color::DarkGray,
        CiStatus::Skipped => Color::Gray,
        CiStatus::Manual => Color::Cyan,
        _ => Color::Reset,
    }
}

pub(super) fn format_duration(seconds: u64) -> String {
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    format!("{minutes}m {seconds:02}s")
}

/// Formats an ISO 8601 timestamp like `2026-06-13T10:00:00.000Z` into a compact `2026-06-13 10:00`
/// for display. Returns the input unchanged if it doesn't look like an ISO datetime, so unexpected
/// formats degrade gracefully rather than being dropped.
pub(super) fn format_timestamp(iso: &str) -> String {
    match iso.split_once('T') {
        Some((date, time)) => {
            let hour_minute: String = time.chars().take(5).collect();
            format!("{date} {hour_minute}")
        }
        None => iso.to_owned(),
    }
}

pub(super) fn cached_suffix(state: &AppState) -> String {
    match state.cached_view.saved_at() {
        Some(saved_at) if state.is_showing_cached_data() => {
            format!(" (cached snapshot at {saved_at})")
        }
        _ => String::new(),
    }
}

/// Draws a vertical scrollbar on the right border of `area` when the content overflows the
/// viewport. `total` is the number of content rows, `viewport` the number of rows visible at
/// once, and `position` the index of the topmost visible row (or the cursor for selection lists).
/// No-op when everything fits, so panels without overflow stay clean.
pub(super) fn render_vertical_scrollbar(
    frame: &mut Frame,
    area: Rect,
    total: usize,
    viewport: usize,
    position: usize,
) {
    if viewport == 0 || total <= viewport {
        return;
    }
    // ratatui pins the thumb to the bottom only when `position == content_length - 1`, but our
    // `position` is a scroll offset whose maximum is `total - viewport` (last row resting at the
    // bottom of the viewport). Model the bar over that offset range so a fully-scrolled view
    // actually reaches the end; offsets past the max (e.g. a cursor in the last page) clamp there.
    let max_offset = total - viewport;
    let mut scrollbar_state = ScrollbarState::new(max_offset + 1)
        .viewport_content_length(viewport)
        .position(position.min(max_offset));
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None);
    // Inset vertically by one so the thumb rides the border between the box corners.
    frame.render_stateful_widget(
        scrollbar,
        area.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        &mut scrollbar_state,
    );
}

/// Panel frame whose border highlights when the panel owns the cursor, lazygit-style.
pub(super) fn panel_block(title: &str, focused: bool) -> Block<'_> {
    let block = Block::default().title(title).borders(Borders::ALL);
    if focused {
        block.border_style(Style::default().fg(Color::Green))
    } else {
        block
    }
}

#[cfg(test)]
mod tests {
    use gitlab::CiStatus;

    use super::{format_duration, format_timestamp, status_icon};

    #[test]
    fn formats_iso_timestamp_into_compact_date_and_time() {
        assert_eq!(
            format_timestamp("2026-06-13T10:05:42.000Z"),
            "2026-06-13 10:05"
        );
        assert_eq!(format_timestamp("2026-06-13T10:05:42Z"), "2026-06-13 10:05");
        // Non-ISO input is returned unchanged.
        assert_eq!(format_timestamp("not a date"), "not a date");
    }

    #[test]
    fn formats_durations_as_minutes_and_seconds() {
        assert_eq!(format_duration(0), "0m 00s");
        assert_eq!(format_duration(61), "1m 01s");
        assert_eq!(format_duration(3600), "60m 00s");
    }

    #[test]
    fn maps_manual_status_to_play_icon() {
        assert_eq!(status_icon(&CiStatus::Manual), "▶");
        assert_eq!(
            status_icon(&CiStatus::Other(String::from("unknown-status"))),
            "•"
        );
    }
}
