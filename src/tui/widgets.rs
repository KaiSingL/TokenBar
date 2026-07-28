use chrono::Utc;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph, Wrap};
use ratatui::Frame;

use crate::model::{Account, AccountStatus, ProviderKind};

pub fn usage_color(percent: f64) -> Color {
    if percent < 60.0 {
        Color::Green
    } else if percent < 85.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}

pub fn render_account_card(
    f: &mut Frame,
    area: Rect,
    account: &Account,
    status: &AccountStatus,
) {
    let provider_label = match account.provider {
        ProviderKind::OpenCodeGo => "opencodego",
    };

    let title = format!(" {} · {} ", account.name, provider_label);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(match status {
            AccountStatus::Error { .. } => Style::default().fg(Color::Red),
            AccountStatus::Stale { .. } => Style::default().fg(Color::Yellow),
            AccountStatus::NoSession => Style::default().fg(Color::DarkGray).dim(),
            _ => Style::default().fg(Color::DarkGray),
        });
    let inner = block.inner(area);
    f.render_widget(block, area);

    match status {
        AccountStatus::NoSession => {
            let msg = format!(
                " No session loaded\n tokenbar session set {} --cookie \"...\"",
                account.name
            );
            f.render_widget(
                Paragraph::new(msg)
                    .style(Style::default().fg(Color::DarkGray))
                    .wrap(Wrap { trim: false }),
                inner,
            );
        }
        AccountStatus::Loading => {
            f.render_widget(
                Paragraph::new("Loading...").style(Style::default().fg(Color::DarkGray)),
                inner,
            );
        }
        AccountStatus::Ready(snapshot) => {
            render_usage_card(f, inner, snapshot.rolling.usage_percent, snapshot.rolling.reset_in_sec, "Rolling (5h)");
            if let Some(ref weekly) = snapshot.weekly {
                let inner_rect = Rect {
                    x: inner.x,
                    y: inner.y + 1,
                    width: inner.width,
                    height: inner.height.saturating_sub(2),
                };
                render_usage_card(f, inner_rect, weekly.usage_percent, weekly.reset_in_sec, "Weekly");
            }
            if let Some(ref monthly) = snapshot.monthly {
                let inner_rect = Rect {
                    x: inner.x,
                    y: inner.y + 2,
                    width: inner.width,
                    height: inner.height.saturating_sub(3),
                };
                render_usage_card(f, inner_rect, monthly.usage_percent, monthly.reset_in_sec, "Monthly");
            }
        }
        AccountStatus::Stale { last, ref error, failed_at } => {
            render_usage_card(f, inner, last.rolling.usage_percent, last.rolling.reset_in_sec, "Rolling (5h)");
            let age = Utc::now().signed_duration_since(*failed_at);
            let status_line = Line::from(vec![
                Span::styled(" ⚠ Stale ", Style::default().fg(Color::Yellow)),
                Span::styled(format!("({age} ago)"), Style::default().dim()),
                Span::styled(format!(": {error}"), Style::default().fg(Color::Red).dim()),
            ]);
            f.render_widget(Paragraph::new(status_line), Rect {
                x: inner.x + 1,
                y: inner.y + 2,
                width: inner.width.saturating_sub(2),
                height: 1,
            });
        }
        AccountStatus::Error { ref message, failed_at } => {
            let age = Utc::now().signed_duration_since(*failed_at);
            let error_line = Line::from(vec![
                Span::styled(" ✗ Error ", Style::default().fg(Color::Red)),
                Span::styled(format!("({age} ago)"), Style::default().dim()),
                Span::styled(format!(": {message}"), Style::default().fg(Color::Red)),
            ]);
            f.render_widget(Paragraph::new(error_line), Rect {
                x: inner.x + 1,
                y: inner.y + 1,
                width: inner.width.saturating_sub(2),
                height: 1,
            });
        }
    }
}

#[allow(unused_variables)]
fn render_usage_card(
    f: &mut Frame,
    area: Rect,
    percent: f64,
    reset_in_sec: u64,
    label: &str,
) {
    let color = usage_color(percent);
    let gauge = Gauge::default()
        .block(Block::default().title(format!(" {label} ")))
        .gauge_style(Style::default().fg(color).bg(Color::DarkGray))
        .percent(percent as u16)
        .label(format!("{percent:.0}%"));
    f.render_widget(gauge, area);
}
