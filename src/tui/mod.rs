use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::execute;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;
use tokio::sync::{RwLock, mpsc};

use crate::app::{AppEvent, AppState};
use crate::model::AccountStatus;
use crate::tui::widgets::render_account_card;

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
    execute!(terminal.backend_mut(), crossterm::terminal::LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    res
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: Arc<RwLock<AppState>>,
    event_tx: mpsc::Sender<AppEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut last_tick = tokio::time::Instant::now();

    loop {
        {
            let mut s = state.write().await;
            s.tick_count += 1;
        }

        terminal.draw(|f| {
            let area = f.area();
            let app_state = state.blocking_read();
            render_layout(f, area, &app_state);
        })?;

        let timeout = TICK_INTERVAL
            .checked_sub(last_tick.elapsed())
            .unwrap_or(Duration::ZERO);

        if event::poll(timeout)? {
            let ev = event::read()?;
            match ev {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
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
                Event::Resize(_, _) => {
                    // handled on next draw
                }
                _ => {}
            }
        }

        if last_tick.elapsed() >= TICK_INTERVAL {
            last_tick = tokio::time::Instant::now();
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
        Some(t) => format!("last refresh · {}", t.format("%H:%M:%S")),
        None => "not yet refreshed".into(),
    };
    let interval = app_state.config.refresh_interval_secs;
    let header = Line::from(vec![
        Span::styled(" TokenBar ", Style::default().bold()),
        Span::styled(
            format!("{last_refresh} · every {interval}s"),
            Style::default().dim(),
        ),
    ]);
    f.render_widget(
        Paragraph::new(header).style(Style::default().fg(Color::Cyan)),
        area,
    );
}

fn render_body(f: &mut ratatui::Frame, area: Rect, app_state: &AppState) {
    let count = app_state.accounts.len();
    if count == 0 {
        f.render_widget(
            Paragraph::new("No accounts configured. Create auth.toml with [[accounts]] entries.")
                .style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }

    let constraints = std::iter::repeat(Constraint::Length(5))
        .take(count)
        .chain(std::iter::once(Constraint::Min(0)))
        .collect::<Vec<_>>();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .spacing(1)
        .split(area);

    for (i, account) in app_state.accounts.iter().enumerate() {
        if i >= chunks.len() {
            break;
        }
        let status = app_state.statuses.get(i).cloned().unwrap_or(AccountStatus::Error {
            message: "unknown state".into(),
            failed_at: Utc::now(),
        });
        render_account_card(f, chunks[i], account, &status);
    }
}

fn render_footer(f: &mut ratatui::Frame, area: Rect) {
    let footer = Line::from(vec![
        Span::styled(" [R]", Style::default().fg(Color::Green)).bold(),
        Span::raw(" refresh "),
        Span::styled("[Q]", Style::default().fg(Color::Red)).bold(),
        Span::raw(" quit"),
    ]);
    f.render_widget(Paragraph::new(footer).dim(), area);
}
