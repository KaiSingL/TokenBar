use chrono::Utc;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::model::{Account, AccountStatus, ProviderKind, UsageSnapshot, UsageWindow};

pub fn usage_color(percent: f64) -> Color {
    if percent < 60.0 {
        Color::Green
    } else if percent < 85.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}

pub fn format_reset(secs: u64) -> String {
    if secs == 0 {
        return "now".into();
    }
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    if days > 0 {
        if hours > 0 {
            format!("{days}d {hours}h")
        } else {
            format!("{days}d")
        }
    } else if hours > 0 {
        if mins > 0 {
            format!("{hours}h {mins}m")
        } else {
            format!("{hours}h")
        }
    } else if mins > 0 {
        format!("{mins}m")
    } else {
        format!("{secs}s")
    }
}

/// Outer height for an account card (includes border rows).
pub fn card_height(status: &AccountStatus) -> u16 {
    let inner = match status {
        AccountStatus::Ready(s) => meter_count(s),
        AccountStatus::Stale { last: s, .. } => meter_count(s) + 1, // + stale note
        AccountStatus::NoSession => 2,
        AccountStatus::Loading => 1,
        AccountStatus::Error { .. } => 2,
    };
    inner + 2 // borders
}

fn meter_count(snapshot: &UsageSnapshot) -> u16 {
    let mut n = 1u16; // rolling always present
    if snapshot.weekly.is_some() {
        n += 1;
    }
    if snapshot.monthly.is_some() {
        n += 1;
    }
    n
}

fn provider_label(provider: ProviderKind) -> &'static str {
    provider.display_label()
}

fn status_badge(status: &AccountStatus) -> (String, Style) {
    match status {
        AccountStatus::Ready(_) => ("synced".into(), Style::default().fg(Color::Green)),
        AccountStatus::Loading => ("loading".into(), Style::default().fg(Color::Cyan)),
        AccountStatus::Stale { .. } => ("stale".into(), Style::default().fg(Color::Yellow)),
        AccountStatus::Error { .. } => ("error".into(), Style::default().fg(Color::Red)),
        AccountStatus::NoSession => ("no session".into(), Style::default().fg(Color::DarkGray)),
    }
}

pub fn render_account_card(f: &mut Frame, area: Rect, account: &Account, status: &AccountStatus) {
    let (badge_text, badge_style) = status_badge(status);
    let title = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            account.name.as_str(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" · {} ", provider_label(account.provider)),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    let title_right = Line::from(vec![
        Span::styled(badge_text, badge_style),
        Span::raw(" "),
    ]);

    let border_style = match status {
        AccountStatus::Error { .. } => Style::default().fg(Color::Red),
        AccountStatus::Stale { .. } => Style::default().fg(Color::Yellow),
        AccountStatus::NoSession => Style::default().fg(Color::DarkGray),
        AccountStatus::Loading => Style::default().fg(Color::Cyan),
        AccountStatus::Ready(_) => Style::default().fg(Color::DarkGray),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title(title_right.right_aligned())
        .border_style(border_style);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    match status {
        AccountStatus::NoSession => {
            let (title, hint) = match account.provider {
                ProviderKind::Zai => (
                    "No API key".to_string(),
                    format!(
                        "tokenbar login {} --provider zai --api-key …",
                        account.name
                    ),
                ),
                ProviderKind::OpenCodeGo => (
                    "No session loaded".to_string(),
                    format!("tokenbar login {}", account.name),
                ),
                ProviderKind::Grok => (
                    "No Grok session".to_string(),
                    format!("tokenbar login {} --provider grok", account.name),
                ),
            };
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(title, Style::default().fg(Color::DarkGray))),
                    Line::from(Span::styled(
                        hint,
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::DIM),
                    )),
                ]),
                inner,
            );
        }
        AccountStatus::Loading => {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "Fetching usage…",
                    Style::default().fg(Color::DarkGray),
                ))),
                inner,
            );
        }
        AccountStatus::Ready(snapshot) => {
            render_meters(f, inner, snapshot, None);
        }
        AccountStatus::Stale {
            last,
            error,
            failed_at,
        } => {
            let age = Utc::now().signed_duration_since(*failed_at);
            let note = format!("stale · {} ago · {error}", compact_age(age.num_seconds()));
            render_meters(f, inner, last, Some(note));
        }
        AccountStatus::Error { message, failed_at } => {
            let age = Utc::now().signed_duration_since(*failed_at);
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(vec![
                        Span::styled("Error", Style::default().fg(Color::Red).bold()),
                        Span::styled(
                            format!(" · {} ago", compact_age(age.num_seconds())),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]),
                    Line::from(Span::styled(
                        message.as_str(),
                        Style::default().fg(Color::Red).add_modifier(Modifier::DIM),
                    )),
                ])
                .wrap(Wrap { trim: true }),
                inner,
            );
        }
    }
}

fn compact_age(secs: i64) -> String {
    let secs = secs.max(0) as u64;
    format_reset(secs)
}

fn render_meters(f: &mut Frame, area: Rect, snapshot: &UsageSnapshot, footer_note: Option<String>) {
    let mut labels: Vec<String> = Vec::new();
    let mut windows: Vec<&UsageWindow> = Vec::new();

    labels.push(
        snapshot
            .rolling
            .label
            .clone()
            .unwrap_or_else(|| "Rolling".into()),
    );
    windows.push(&snapshot.rolling);

    if let Some(ref w) = snapshot.weekly {
        labels.push(w.label.clone().unwrap_or_else(|| "Weekly".into()));
        windows.push(w);
    }
    if let Some(ref m) = snapshot.monthly {
        labels.push(m.label.clone().unwrap_or_else(|| "Monthly".into()));
        windows.push(m);
    }

    let meter_rows = windows.len() as u16;
    let note_rows = if footer_note.is_some() { 1u16 } else { 0 };
    let total = meter_rows + note_rows;
    if total == 0 || area.height == 0 {
        return;
    }

    let constraints: Vec<Constraint> = (0..total).map(|_| Constraint::Length(1)).collect();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    for (i, window) in windows.iter().enumerate() {
        if i >= rows.len() {
            break;
        }
        render_meter_row(f, rows[i], &labels[i], window);
    }

    if let (Some(note), Some(row)) = (footer_note, rows.get(meter_rows as usize)) {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                note,
                Style::default().fg(Color::Yellow).add_modifier(Modifier::DIM),
            ))),
            *row,
        );
    }
}

fn render_meter_row(f: &mut Frame, area: Rect, label: &str, window: &UsageWindow) {
    if area.width < 20 {
        return;
    }

    let percent = window.usage_percent.clamp(0.0, 100.0);
    let color = usage_color(percent);
    let pct_text = format!("{percent:3.0}%");
    let reset_text = format!("resets {}", format_reset(window.reset_in_sec));

    // Fixed columns: label(9) + gap(1) + bar(flex) + gap(1) + pct(4) + gap(2) + reset(~14)
    let label_w = 9u16;
    let pct_w = 4u16;
    let reset_w = reset_text.len().min(16) as u16;
    let gaps = 4u16; // spaces between columns
    let fixed = label_w + pct_w + reset_w + gaps;
    let bar_w = area.width.saturating_sub(fixed).max(4);

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(label_w),
            Constraint::Length(1),
            Constraint::Length(bar_w),
            Constraint::Length(1),
            Constraint::Length(pct_w),
            Constraint::Length(2),
            Constraint::Min(reset_w),
        ])
        .split(area);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{label:<8}"),
            Style::default().fg(Color::Gray),
        ))),
        chunks[0],
    );

    let bar = build_bar(bar_w as usize, percent, color);
    f.render_widget(Paragraph::new(Line::from(bar)), chunks[2]);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            pct_text,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ))),
        chunks[4],
    );

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            reset_text,
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[6],
    );
}

fn build_bar(width: usize, percent: f64, color: Color) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let filled = ((percent / 100.0) * width as f64).round() as usize;
    let filled = filled.min(width);
    let empty = width - filled;
    let mut spans = Vec::with_capacity(2);
    if filled > 0 {
        spans.push(Span::styled(
            "█".repeat(filled),
            Style::default().fg(color),
        ));
    }
    if empty > 0 {
        spans.push(Span::styled(
            "░".repeat(empty),
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_usage_color_thresholds() {
        assert_eq!(usage_color(0.0), Color::Green);
        assert_eq!(usage_color(59.9), Color::Green);
        assert_eq!(usage_color(60.0), Color::Yellow);
        assert_eq!(usage_color(84.9), Color::Yellow);
        assert_eq!(usage_color(85.0), Color::Red);
        assert_eq!(usage_color(100.0), Color::Red);
    }

    #[test]
    fn test_format_reset() {
        assert_eq!(format_reset(0), "now");
        assert_eq!(format_reset(45), "45s");
        assert_eq!(format_reset(60), "1m");
        assert_eq!(format_reset(3_600), "1h");
        assert_eq!(format_reset(3_660), "1h 1m");
        assert_eq!(format_reset(86_400), "1d");
        assert_eq!(format_reset(90_000), "1d 1h");
    }

    #[test]
    fn test_build_bar_full_empty() {
        let full = build_bar(10, 100.0, Color::Red);
        assert_eq!(full.len(), 1);
        assert_eq!(full[0].content.as_ref(), "██████████");

        let empty = build_bar(10, 0.0, Color::Green);
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].content.as_ref(), "░░░░░░░░░░");
    }
}
