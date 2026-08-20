//! A terminal chat UI for Ira (ratatui + crossterm), built on the SDK `Session`.
//!
//! Layout: a header bar, a scrolling conversation that streams tokens live, and
//! an input box. Like the rest of the CLI it touches only `ira::sdk` — the UI is
//! pure presentation over the `Event` stream.

use crossterm::event::{Event as CEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use futures_util::StreamExt;
use ira::sdk::query::{open_session, Event};
use ira::sdk::session::RepoOptions;
use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::io::stdout;
use std::path::PathBuf;
use tokio::sync::mpsc;

#[derive(PartialEq, Clone, Copy)]
enum Role {
    User,
    Assistant,
    Thinking,
    Tool,
    Error,
}

struct Block2 {
    role: Role,
    text: String,
}

/// Streaming updates flowing from the turn task into the UI loop.
enum Up {
    Delta(String),
    Thinking(String),
    Tool(String),
    Done,
    Error(String),
}

struct App {
    blocks: Vec<Block2>,
    input: String,
    streaming: bool,
    follow: bool, // auto-scroll to bottom
    scroll: u16,
    model_label: String,
}

impl App {
    fn append(&mut self, role: Role, text: &str) {
        match self.blocks.last_mut() {
            Some(b) if b.role == role && (role == Role::Assistant || role == Role::Thinking) => {
                b.text.push_str(text);
            }
            _ => self.blocks.push(Block2 { role, text: text.to_string() }),
        }
        self.follow = true;
    }
    fn push(&mut self, role: Role, text: String) {
        self.blocks.push(Block2 { role, text });
        self.follow = true;
    }
}

pub async fn run_tui(
    dir: PathBuf,
    model: Option<String>,
    repo: Option<RepoOptions>,
    permission_mode: Option<String>,
) -> i32 {
    let session = match open_session(dir, model.clone(), repo, permission_mode) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    if enable_raw_mode().is_err() {
        eprintln!("error: this needs an interactive terminal");
        return 1;
    }
    let mut out = stdout();
    let _ = crossterm::execute!(out, EnterAlternateScreen);
    let mut term = match Terminal::new(ratatui::backend::CrosstermBackend::new(out)) {
        Ok(t) => t,
        Err(e) => {
            let _ = disable_raw_mode();
            eprintln!("error: {e}");
            return 1;
        }
    };

    let mut app = App {
        blocks: Vec::new(),
        input: String::new(),
        streaming: false,
        follow: true,
        scroll: 0,
        model_label: model.unwrap_or_else(|| "agent default".into()),
    };
    let mut events = EventStream::new();
    let (tx, mut rx) = mpsc::channel::<Up>(512);
    let mut quit = false;

    while !quit {
        let _ = term.draw(|f| draw(f, &mut app));

        tokio::select! {
            maybe = events.next() => {
                let Some(Ok(CEvent::Key(k))) = maybe else { continue };
                if k.kind == KeyEventKind::Release { continue; }
                match (k.code, k.modifiers) {
                    (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => quit = true,
                    (KeyCode::Esc, _) => quit = true,
                    (KeyCode::Enter, _) => {
                        let msg = app.input.trim().to_string();
                        if !app.streaming && !msg.is_empty() {
                            app.input.clear();
                            app.push(Role::User, msg.clone());
                            app.streaming = true;
                            let mut turn = session.send(msg);
                            let txc = tx.clone();
                            tokio::spawn(async move {
                                while let Some(ev) = turn.next().await {
                                    let up = match ev {
                                        Event::Delta(t) => Up::Delta(t),
                                        Event::Thinking(t) => Up::Thinking(t),
                                        Event::ToolCall { name, args } => Up::Tool(format!("{name}({})", short(&args.to_string()))),
                                        Event::ToolResult { name, is_error, .. } => Up::Tool(format!("{} {name}", if is_error { "✗" } else { "→" })),
                                        Event::Done => Up::Done,
                                        Event::Error(m) => Up::Error(m),
                                    };
                                    if txc.send(up).await.is_err() { break; }
                                }
                                let _ = txc.send(Up::Done).await;
                            });
                        }
                    }
                    (KeyCode::Backspace, _) => { app.input.pop(); }
                    (KeyCode::Up, _) => { app.follow = false; app.scroll = app.scroll.saturating_sub(1); }
                    (KeyCode::Down, _) => { app.scroll = app.scroll.saturating_add(1); }
                    (KeyCode::Char(c), _) => app.input.push(c),
                    _ => {}
                }
            }
            Some(up) = rx.recv() => match up {
                Up::Delta(t) => app.append(Role::Assistant, &t),
                Up::Thinking(t) => app.append(Role::Thinking, &t),
                Up::Tool(s) => app.push(Role::Tool, s),
                Up::Done => app.streaming = false,
                Up::Error(m) => { app.push(Role::Error, m); app.streaming = false; }
            }
        }
    }

    let _ = disable_raw_mode();
    let _ = crossterm::execute!(term.backend_mut(), LeaveAlternateScreen);
    let _ = term.show_cursor();
    0
}

fn short(s: &str) -> String {
    if s.chars().count() <= 60 { s.to_string() } else { format!("{}…", s.chars().take(60).collect::<String>()) }
}

fn render_lines(blocks: &[Block2]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for b in blocks {
        let (prefix, style) = match b.role {
            Role::User => ("you  ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Role::Assistant => ("", Style::default()),
            Role::Thinking => ("💭 ", Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)),
            Role::Tool => ("⚙ ", Style::default().fg(Color::Blue)),
            Role::Error => ("error: ", Style::default().fg(Color::Red)),
        };
        for (i, seg) in b.text.split('\n').enumerate() {
            let content = if i == 0 { format!("{prefix}{seg}") } else { seg.to_string() };
            lines.push(Line::styled(content, style));
        }
        lines.push(Line::from(""));
    }
    lines
}

fn visual_rows(lines: &[Line], width: u16) -> u16 {
    let w = width.max(1) as usize;
    let mut n: usize = 0;
    for l in lines {
        let len: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
        n += if len == 0 { 1 } else { len.div_ceil(w) };
    }
    n.min(u16::MAX as usize) as u16
}

fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(3)]).split(area);

    // Header bar.
    let status = if app.streaming { "streaming…" } else { "ready" };
    let header = Paragraph::new(format!("  Ira · {} · {}", app.model_label, status))
        .style(Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD));
    f.render_widget(header, rows[0]);

    // Conversation (auto-scroll to bottom unless the user scrolled up).
    let lines = render_lines(&app.blocks);
    let inner_w = rows[1].width.saturating_sub(2);
    let inner_h = rows[1].height.saturating_sub(2);
    let total = visual_rows(&lines, inner_w);
    let max_scroll = total.saturating_sub(inner_h);
    let scroll = if app.follow { max_scroll } else { app.scroll.min(max_scroll) };
    app.scroll = scroll;
    let convo = Paragraph::new(lines)
        .block(Block::bordered().border_type(BorderType::Rounded).title(" conversation "))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(convo, rows[1]);

    // Input box.
    let title = if app.streaming { " thinking… (Esc quits) " } else { " message · Enter sends · Esc quits · ↑/↓ scroll " };
    let input = Paragraph::new(format!("› {}", app.input))
        .block(Block::bordered().border_type(BorderType::Rounded).title(title))
        .style(Style::default().fg(Color::White));
    f.render_widget(input, rows[2]);
    let cx = rows[2].x + 3 + app.input.chars().count() as u16;
    f.set_cursor_position(Position::new(cx.min(rows[2].x + rows[2].width.saturating_sub(2)), rows[2].y + 1));
}
