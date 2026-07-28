use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::execute;
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;
use tokio::sync::{RwLock, mpsc};
use tokio::time::interval;

use crate::app::{AppEvent, AppState};
use crate::model::AccountStatus;
use crate::tui::widgets::{card_height, render_account_card};

mod widgets;

const TICK_INTERVAL: Duration = Duration::from_secs(1);

pub async fn run_tui(
    state: Arc<RwLock<AppState>>,
    event_tx: mpsc::Sender<AppEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let res = run_loop(&mut terminal, state, event_tx).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    res
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: Arc<RwLock<AppState>>,
    event_tx: mpsc::Sender<AppEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut events = EventStream::new();
    let mut ticker = interval(TICK_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await;

    loop {
        {
            let mut s = state.write().await;
            s.tick_count += 1;
        }

        {
            let app_state = state.read().await;
            terminal.draw(|f| {
                let area = f.area();
                render_layout(f, area, &app_state);
            })?;
        }

        tokio::select! {
            _ = ticker.tick() => {}
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Char('Q') => {
                                let _ = event_tx.send(AppEvent::Quit).await;
                                break;
                            }
                            KeyCode::Char('r') | KeyCode::Char('R') => {
                                event_tx.send(AppEvent::Refresh).await?;
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Event::Resize(_, _))) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => break,
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

fn render_layout(f: &mut ratatui::Frame, area: Rect, app_state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(f, chunks[0], app_state);
    render_body(f, chunks[1], app_state);
    render_footer(f, chunks[2]);
}

fn render_header(f: &mut ratatui::Frame, area: Rect, app_state: &AppState) {
    let last_refresh = match app_state.last_refresh {
        Some(t) => format!("last {}", t.format("%H:%M:%S")),
        None => "not yet refreshed".into(),
    };
    let interval = app_state.config.refresh_interval_secs;

    let mut left = vec![
        Span::styled(
            " TokenBar ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("·  ", Style::default().fg(Color::DarkGray)),
        Span::styled(last_refresh, Style::default().fg(Color::Gray)),
        Span::styled(
            format!("  ·  every {interval}s"),
            Style::default().fg(Color::DarkGray),
        ),
    ];

    if app_state.is_refreshing {
        left.push(Span::raw("  "));
        left.push(Span::styled(
            "refreshing…",
            Style::default().fg(Color::Cyan),
        ));
    }

    let right = if app_state.is_refreshing {
        Span::styled("● sync", Style::default().fg(Color::Cyan))
    } else if app_state.last_refresh.is_some() {
        Span::styled("● live", Style::default().fg(Color::Green))
    } else {
        Span::styled("○ idle", Style::default().fg(Color::DarkGray))
    };

    let header_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(8)])
        .split(area);

    f.render_widget(Paragraph::new(Line::from(left)), header_chunks[0]);
    f.render_widget(
        Paragraph::new(Line::from(right)).right_aligned(),
        header_chunks[1],
    );
}

fn render_body(f: &mut ratatui::Frame, area: Rect, app_state: &AppState) {
    let count = app_state.accounts.len();
    if count == 0 {
        f.render_widget(
            Paragraph::new("No accounts configured. Add [[accounts]] entries to auth.toml.")
                .style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }

    let mut constraints: Vec<Constraint> = Vec::with_capacity(count * 2);
    for i in 0..count {
        let status = app_state
            .statuses
            .get(i)
            .cloned()
            .unwrap_or(AccountStatus::Error {
                message: "unknown state".into(),
                failed_at: Utc::now(),
            });
        constraints.push(Constraint::Length(card_height(&status)));
        if i + 1 < count {
            constraints.push(Constraint::Length(1)); // gap
        }
    }
    constraints.push(Constraint::Min(0));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let mut chunk_idx = 0usize;
    for (i, account) in app_state.accounts.iter().enumerate() {
        if chunk_idx >= chunks.len() {
            break;
        }
        let status = app_state
            .statuses
            .get(i)
            .cloned()
            .unwrap_or(AccountStatus::Error {
                message: "unknown state".into(),
                failed_at: Utc::now(),
            });
        render_account_card(f, chunks[chunk_idx], account, &status);
        chunk_idx += 1;
        if i + 1 < count {
            chunk_idx += 1; // skip gap
        }
    }
}

fn render_footer(f: &mut ratatui::Frame, area: Rect) {
    let footer = Line::from(vec![
        Span::styled(
            " [r]",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" refresh", Style::default().fg(Color::DarkGray)),
        Span::raw("   "),
        Span::styled(
            "[q]",
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" quit", Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(footer), area);
}
