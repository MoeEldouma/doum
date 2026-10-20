//! `doum top`: a live terminal view of the GPU queue, with keyboard controls.

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Sparkline, Table, TableState, Wrap};
use ratatui::Frame;
use serde_json::json;
use std::time::{Duration, Instant};

use crate::client::Client;
use crate::daemon::{fmt_dur, StatusView};
use crate::job::{Job, State};
use crate::metrics::Sample;

const PALM: Color = Color::Rgb(111, 207, 142);
const SAND: Color = Color::Rgb(231, 194, 125);
const RED: Color = Color::Rgb(255, 125, 107);
const MUTED: Color = Color::Rgb(111, 128, 122);

struct App {
    status: Option<StatusView>,
    history: Vec<Sample>,
    table: TableState,
    msg: String,
    confirm_cancel: Option<u64>,
}

impl App {
    /// Jobs in display order: running first, then the queue.
    fn rows(&self) -> Vec<&Job> {
        let Some(s) = &self.status else { return vec![] };
        let mut run: Vec<&Job> = s.jobs.iter().filter(|j| j.state.holds_shards()).collect();
        run.sort_by_key(|j| j.started_at);
        run.into_iter()
            .chain(s.queue.iter().filter_map(|id| s.jobs.iter().find(|j| j.id == *id)))
            .collect()
    }
    fn selected(&self) -> Option<&Job> {
        self.table.selected().and_then(|i| self.rows().get(i).copied())
    }
}

pub fn run(c: &Client) -> Result<()> {
    let mut term = ratatui::init();
    let mut app = App {
        status: None,
        history: vec![],
        table: TableState::default(),
        msg: String::new(),
        confirm_cancel: None,
    };
    let mut last_status = Instant::now() - Duration::from_secs(10);
    let mut last_hist = Instant::now() - Duration::from_secs(60);
    let res = loop {
        if last_status.elapsed() >= Duration::from_millis(1000) {
            match c.get::<StatusView>("/api/status") {
                Ok(s) => app.status = Some(s),
                Err(e) => app.msg = format!("{e}"),
            }
            last_status = Instant::now();
            let n = app.rows().len();
            if n == 0 {
                app.table.select(None);
            } else if app.table.selected().is_none_or(|i| i >= n) {
                app.table.select(Some(0));
            }
        }
        if last_hist.elapsed() >= Duration::from_secs(10) {
            if let Ok(h) = c.get::<Vec<Sample>>("/api/metrics?minutes=30") {
                app.history = h;
            }
            last_hist = Instant::now();
        }
        if let Err(e) = term.draw(|f| draw(f, &mut app)) {
            break Err(e.into());
        }
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                if let Some(id) = app.confirm_cancel.take() {
                    if matches!(k.code, KeyCode::Char('y')) {
                        app.msg = post(c, &format!("/api/jobs/{id}/cancel"), json!({}), &format!("job {id} cancelled"));
                    } else {
                        app.msg = "cancel aborted".into();
                    }
                    continue;
                }
                let n = app.rows().len();
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                    KeyCode::Down | KeyCode::Char('j') if n > 0 => app.table.select(Some(app.table.selected().map_or(0, |i| (i + 1) % n))),
                    KeyCode::Up | KeyCode::Char('k') if n > 0 => {
                        app.table.select(Some(app.table.selected().map_or(0, |i| (i + n - 1) % n)))
                    }
                    KeyCode::Char('p') => {
                        if let Some(j) = app.selected() {
                            let id = j.id;
                            app.msg = post(c, &format!("/api/jobs/{id}/pause"), json!({}), &format!("job {id} pausing"));
                        }
                    }
                    KeyCode::Char('t') => {
                        if let Some(j) = app.selected() {
                            let id = j.id;
                            app.msg = post(c, &format!("/api/jobs/{id}/top"), json!({}), &format!("job {id} moved up"));
                        }
                    }
                    KeyCode::Char('c') => {
                        if let Some((id, name)) = app.selected().map(|j| (j.id, j.name.clone())) {
                            app.msg = format!("cancel '{name}'? press y to confirm");
                            app.confirm_cancel = Some(id);
                        }
                    }
                    KeyCode::Char(ch @ ('1' | '2' | '3')) => {
                        if let Some(d) = app.status.as_ref().and_then(|s| s.decisions.first()).cloned() {
                            let i = (ch as u8 - b'1') as usize;
                            if let Some(choice) = d.options.get(i) {
                                app.msg = post(
                                    c,
                                    &format!("/api/decisions/{}", d.id),
                                    json!({ "choice": choice }),
                                    &format!("answered: {choice}"),
                                );
                            }
                        }
                    }
                    _ => {}
                }
                last_status = Instant::now() - Duration::from_secs(10);
            }
        }
    };
    ratatui::restore();
    res
}

fn post(c: &Client, path: &str, body: serde_json::Value, ok: &str) -> String {
    match c.post::<serde_json::Value>(path, &body) {
        Ok(_) => ok.to_string(),
        Err(e) => format!("{e}"),
    }
}

fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .split(area);

    let Some(s) = app.status.as_ref() else {
        f.render_widget(Paragraph::new(format!("connecting to the doum daemon… {}", app.msg)), area);
        return;
    };
    let now = s.now;

    // title line
    let mut title = vec![
        Span::styled(" Doum ", Style::default().fg(Color::Black).bg(PALM).add_modifier(Modifier::BOLD)),
        Span::raw(format!("  {:.0} GB memory free", s.mem_available_gb)),
    ];
    if let Some(l) = &s.latest {
        if let Some(c) = l.gpu_c {
            title.push(Span::styled(
                format!("  GPU {c:.0}°C"),
                Style::default().fg(if c >= 80.0 { RED } else { Color::Reset }),
            ));
        }
        if let Some(w) = l.gpu_w {
            title.push(Span::raw(format!(" {w:.0} W")));
        }
        if let Some(c) = l.soc_c {
            title.push(Span::styled(
                format!("  SoC {c:.0}°C"),
                Style::default().fg(if c >= 85.0 {
                    RED
                } else if c >= 75.0 {
                    SAND
                } else {
                    Color::Reset
                }),
            ));
        }
    }
    f.render_widget(Paragraph::new(Line::from(title)), rows[0]);

    // shard gauge
    let ratio = (s.used_shards as f64 / s.total_shards.max(1) as f64).clamp(0.0, 1.0);
    f.render_widget(
        Gauge::default()
            .block(Block::default().borders(Borders::ALL).title(" GPU shards "))
            .gauge_style(Style::default().fg(PALM).bg(Color::Rgb(21, 27, 25)))
            .ratio(ratio)
            .label(format!(
                "{}/{} used · {} running · {} waiting",
                s.used_shards,
                s.total_shards,
                s.jobs.iter().filter(|j| j.state.holds_shards()).count(),
                s.queue.len()
            )),
        rows[1],
    );

    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
        .split(rows[2]);
    draw_jobs(f, app, mid[0], now);
    draw_side(f, app, mid[1], now);

    let help = if app.msg.is_empty() {
        "↑↓ select · p pause · c cancel · t move up · 1/2/3 answer the question · q quit".to_string()
    } else {
        app.msg.clone()
    };
    f.render_widget(Paragraph::new(help).style(Style::default().fg(MUTED)), rows[3]);
}

fn draw_jobs(f: &mut Frame, app: &mut App, area: Rect, now: u64) {
    let rows: Vec<Row> = app
        .rows()
        .iter()
        .map(|j| {
            let (state, color) = match j.state {
                State::Running => ("running", PALM),
                State::Pausing => ("pausing", SAND),
                _ => ("waiting", MUTED),
            };
            let detail = if j.state.holds_shards() {
                let eta = j.eta_left(now).map(|e| format!("~{} left", fmt_dur(e))).unwrap_or_default();
                format!(
                    "{} {} {}",
                    j.started_at.map(|t| fmt_dur(now.saturating_sub(t))).unwrap_or_default(),
                    eta,
                    j.progress.message
                )
            } else {
                j.reason.clone()
            };
            Row::new(vec![
                Cell::from(format!("{}", j.id)),
                Cell::from(j.name.clone()),
                Cell::from(format!("{}", j.shards)),
                Cell::from(format!("{:?}", j.lane).to_lowercase()),
                Cell::from(state).style(Style::default().fg(color)),
                Cell::from(detail),
            ])
        })
        .collect();
    let t = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Length(24),
            Constraint::Length(6),
            Constraint::Length(11),
            Constraint::Length(8),
            Constraint::Min(10),
        ],
    )
    .header(Row::new(vec!["id", "job", "shards", "lane", "state", "detail"]).style(Style::default().fg(MUTED)))
    .row_highlight_style(Style::default().bg(Color::Rgb(30, 39, 36)).add_modifier(Modifier::BOLD))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" jobs: running, then waiting in start order "),
    );
    f.render_stateful_widget(t, area, &mut app.table);
}

fn draw_side(f: &mut Frame, app: &App, area: Rect, now: u64) {
    let Some(s) = &app.status else { return };
    let q_h = if s.decisions.is_empty() { 3 } else { 9 };
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(q_h),
            Constraint::Length(5),
            Constraint::Length(5),
            Constraint::Min(3),
        ])
        .split(area);

    let q = match s.decisions.first() {
        Some(d) => {
            let opts: Vec<String> = d
                .options
                .iter()
                .enumerate()
                .map(|(i, o)| {
                    format!(
                        "[{}] {}{}",
                        i + 1,
                        crate::notify::label(o),
                        if *o == d.recommended { "*" } else { "" }
                    )
                })
                .collect();
            format!(
                "{}\n{}  (default in {})",
                d.text,
                opts.join("  "),
                fmt_dur(d.deadline.saturating_sub(now))
            )
        }
        None => "none".into(),
    };
    f.render_widget(
        Paragraph::new(q).wrap(Wrap { trim: true }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" questions for you ")
                .border_style(Style::default().fg(if s.decisions.is_empty() { MUTED } else { SAND })),
        ),
        parts[0],
    );

    let used: Vec<u64> = app.history.iter().map(|h| h.used_shards as u64).collect();
    f.render_widget(
        Sparkline::default()
            .block(Block::default().borders(Borders::ALL).title(" shards used, 30 min "))
            .data(&used)
            .max(s.total_shards as u64)
            .style(Style::default().fg(PALM)),
        parts[1],
    );
    let temp: Vec<u64> = app.history.iter().map(|h| h.soc_c.unwrap_or(0.0).max(0.0) as u64).collect();
    f.render_widget(
        Sparkline::default()
            .block(Block::default().borders(Borders::ALL).title(" SoC °C, 30 min "))
            .data(&temp)
            .max(105)
            .style(Style::default().fg(SAND)),
        parts[2],
    );
    let outside: Vec<Line> = if s.unmanaged.is_empty() {
        vec![Line::from("nothing")]
    } else {
        s.unmanaged
            .iter()
            .map(|p| Line::from(format!("{} (pid {}) ~{:.1} GB", p.name, p.pid, p.mem_gb)))
            .collect()
    };
    f.render_widget(
        Paragraph::new(outside).block(Block::default().borders(Borders::ALL).title(" outside the queue ")),
        parts[3],
    );
}
