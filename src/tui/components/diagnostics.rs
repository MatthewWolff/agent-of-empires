//! "memory pressure" strip, docked under the session-list column.
//!
//! Two stacked rows: a memory-over-time area sparkline on top, then a readout
//! row (percent plus used/total, and the agent/process counts). The sparkline
//! plots memory headroom; its color and the percent share a green/amber/red
//! band derived by [`pressure_band`] from headroom, PSI, and the macOS pressure
//! level (worst wins). See [`crate::process::metrics`] for why headroom is the
//! headline signal.
//!
//! The readout row sheds gracefully as width shrinks: the used/total detail
//! drops before the counts, and below that only the percent remains.

use std::collections::VecDeque;

use ratatui::prelude::*;
use ratatui::widgets::*;
use unicode_width::UnicodeWidthStr;

use crate::process::metrics::{pressure_band, MemorySample, MetricsSnapshot, PressureBand};
use crate::tui::styles::Theme;

/// The full 0..=100% scale in per-mille, so the sparkline plots against a fixed
/// ceiling instead of auto-scaling to whatever the recent peak happened to be.
const PER_MILLE_MAX: u64 = 1000;

fn band_color(theme: &Theme, band: PressureBand) -> Color {
    match band {
        PressureBand::Ok => theme.running,
        PressureBand::Warn => theme.waiting,
        PressureBand::Critical => theme.error,
    }
}

/// Compact binary-unit byte string: `22.7G`, `9.9G`, `512M`, `1K`, `512B`.
/// GiB keeps one decimal but drops a whole `.0`; smaller units round to a whole
/// number. Integer math throughout so the rounding is exact.
fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1 << 10;
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;

    if bytes >= GIB {
        // Tenths of a GiB, rounded, so 32 GiB reads "32G" and 22.7 GiB "22.7G".
        let tenths = (bytes as u128 * 10 + GIB as u128 / 2) / GIB as u128;
        if tenths % 10 == 0 {
            format!("{}G", tenths / 10)
        } else {
            format!("{}.{}G", tenths / 10, tenths % 10)
        }
    } else if bytes >= MIB {
        format!("{}M", (bytes + MIB / 2) / MIB)
    } else if bytes >= KIB {
        format!("{}K", (bytes + KIB / 2) / KIB)
    } else {
        format!("{}B", bytes)
    }
}

/// Render the memory-pressure strip into `area`.
///
/// `history` is the caller-owned ring buffer of headroom samples, newest at the
/// back, each stored as per-mille of `used_fraction` (`0..=1000`). The sparkline
/// plots against a fixed `1000` ceiling so the vertical scale is a stable
/// `0..100%`. When the buffer holds more samples than the sparkline is wide, the
/// newest `width` are shown (ratatui renders from the front, so the tail is
/// sliced here to keep the recent window on screen).
pub fn render(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    history: &VecDeque<u64>,
    snapshot: &MetricsSnapshot,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    // Stack vertically: sparkline on top, the readout on the bottom row. The
    // strip sits under the narrow list column, so a side-by-side split would
    // leave too few columns for the counts.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let spark_area = chunks[0];
    let text_area = chunks[1];

    let mem = &snapshot.memory;
    let band = pressure_band(mem);
    let color = band_color(theme, band);

    // Show the newest `width` samples: ratatui's Sparkline renders the first N
    // elements, so a buffer longer than the strip would otherwise drop the most
    // recent readings.
    let width = spark_area.width as usize;
    let start = history.len().saturating_sub(width);
    let recent: Vec<u64> = history.iter().skip(start).copied().collect();
    let sparkline = Sparkline::default()
        .data(recent)
        .max(PER_MILLE_MAX)
        .direction(RenderDirection::LeftToRight)
        .style(Style::default().fg(color));
    frame.render_widget(sparkline, spark_area);

    frame.render_widget(
        Paragraph::new(readout_line(
            theme,
            color,
            mem,
            snapshot,
            text_area.width as usize,
        )),
        text_area,
    );
}

/// Build the readout line under the sparkline, dropping detail as `avail`
/// (display cells) shrinks. A leading space insets it from the border. Before
/// the first sample (`total_bytes == 0`) the memory figure is unknown, so only
/// the counts show rather than a misleading `0%`.
fn readout_line<'a>(
    theme: &Theme,
    color: Color,
    mem: &MemorySample,
    snapshot: &MetricsSnapshot,
    avail: usize,
) -> Line<'a> {
    let counts = format!(
        "{} agents · {} procs",
        snapshot.counts.agents, snapshot.counts.procs
    );

    if mem.total_bytes == 0 {
        return Line::from(vec![
            Span::raw(" "),
            Span::styled(counts, Style::default().fg(theme.dimmed)),
        ]);
    }

    let pct = format!("{}%", (mem.used_fraction() * 100.0).round() as u32);
    let detail = format!(
        "{}/{}",
        format_bytes(mem.used_bytes()),
        format_bytes(mem.total_bytes)
    );
    let gap = "  ";
    let sep = "  │  ";

    let pct_span = Span::styled(pct.clone(), Style::default().fg(color).bold());
    let full_width = 1 + pct.width() + gap.width() + detail.width() + sep.width() + counts.width();
    let counts_width = 1 + pct.width() + gap.width() + counts.width();

    let spans = if full_width <= avail {
        vec![
            Span::raw(" "),
            pct_span,
            Span::raw(gap),
            Span::styled(detail, Style::default().fg(theme.text)),
            Span::styled(sep, Style::default().fg(theme.dimmed)),
            Span::styled(counts, Style::default().fg(theme.dimmed)),
        ]
    } else if counts_width <= avail {
        vec![
            Span::raw(" "),
            pct_span,
            Span::raw(gap),
            Span::styled(counts, Style::default().fg(theme.dimmed)),
        ]
    } else {
        vec![Span::raw(" "), pct_span]
    };
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_table() {
        let gib = 1u64 << 30;
        let cases = [
            (0u64, "0B"),
            (512, "512B"),
            (1024, "1K"),
            (536_870_912, "512M"), // 512 MiB
            (32 * gib, "32G"),     // whole GiB drops the .0
            (22 * gib + 7 * gib / 10, "22.7G"),
            (9 * gib + 9 * gib / 10, "9.9G"),
        ];
        for (bytes, expected) in cases {
            assert_eq!(format_bytes(bytes), expected, "bytes {bytes}");
        }
    }
}
