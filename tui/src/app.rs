use std::thread;
use std::time::Duration;

use anyhow::Result;
use crossbeam_channel::Sender;
use crossbeam_channel::{unbounded, Receiver};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use octoproxy_lib::metric::BackendStatus;
use ratatui::backend::Backend;
use ratatui::layout::{Constraint, Corner, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Span, Spans};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use ratatui::Frame;

use crate::fetch::Fetcher;
use crate::{BackendMetric, MetricApiResp};

static POLL_DURATION: Duration = Duration::from_millis(1000);

pub struct App {
    backends: Vec<BackendMetric>,
    backends_state: ListState,

    fetcher: Fetcher,
    pub do_quit: bool,
    pub log_text: String,
}

impl App {
    pub fn new(fetcher: Fetcher) -> Result<Self> {
        // let backends = fetcher.get_all_backends()?;
        let mut backends_state = ListState::default();
        backends_state.select(Some(0));

        let backends = Vec::new();

        Ok(Self {
            backends,
            fetcher,
            backends_state,
            do_quit: false,
            log_text: "".to_owned(),
        })
    }

    pub fn any_work_pending(&self) -> bool {
        self.fetcher.is_pending()
    }

    // TODO check_hard_exit
    fn check_quit(&mut self, ev: &Event) -> bool {
        if let Event::Key(e) = ev {
            if e.code == KeyCode::Char('q') && e.modifiers.is_empty() {
                return true;
            }
        }
        false
    }

    pub fn handle_fetch(&mut self, fetch: MetricApiResp) {
        match fetch {
            MetricApiResp::AllBackends { items } => {
                self.backends = items;
            }

            MetricApiResp::SwitchBackendStatus
            | MetricApiResp::ResetBackend
            | MetricApiResp::SwitchBackendProtocol => {}

            MetricApiResp::Error { msg } => self.log_text = msg,
        }
    }

    pub fn event(&mut self, ev: Event) -> Result<()> {
        log::trace!("event: {:?}", ev);

        if self.check_quit(&ev) {
            self.do_quit = true;
            return Ok(());
        }

        if let Event::Key(key) = ev {
            if key.kind == KeyEventKind::Press {
                match key.code {
                    KeyCode::Down | KeyCode::Char('k') => self.select_next_backend(),
                    KeyCode::Up | KeyCode::Char('j') => self.select_prev_backend(),
                    KeyCode::Char(' ') => self.switch_backend_status(),
                    KeyCode::Char('s') => self.switch_backend_protocol(),
                    KeyCode::Char('r') => self.reset_backend(),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn reset_backend(&mut self) {
        if let Some(selected) = self.backends_state.selected() {
            self.log_text = "".to_string();
            self.fetcher.reset_backend(selected);
        }
    }

    fn switch_backend_status(&mut self) {
        if let Some(selected) = self.backends_state.selected() {
            self.log_text = "".to_string();
            self.fetcher.switch_backend_status(selected);
        }
    }

    fn switch_backend_protocol(&mut self) {
        if let Some(selected) = self.backends_state.selected() {
            self.log_text = "".to_string();
            self.fetcher.switch_backend_protocol(selected);
        }
    }

    fn select_next_backend(&mut self) {
        let i = match self.backends_state.selected() {
            Some(i) => {
                if i >= self.backends.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.backends_state.select(Some(i));
    }

    fn select_prev_backend(&mut self) {
        let i = match self.backends_state.selected() {
            Some(i) => {
                if i == 0 {
                    self.backends.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.backends_state.select(Some(i));
    }

    fn draw_log<B: Backend>(&self, f: &mut Frame<B>, r: Rect) {
        let text = vec![Spans::from(self.log_text.clone())];

        let block = Block::default().borders(Borders::ALL).title("Error Log");
        let paragraph = Paragraph::new(text)
            .style(Style::default().fg(Color::Gray))
            .block(block)
            .wrap(Wrap { trim: true });
        f.render_widget(paragraph, r);
    }

    fn draw_top_bar<B: Backend>(&self, f: &mut Frame<B>, r: Rect) {
        let r = r.inner(&Margin {
            vertical: 0,
            horizontal: 1,
        });

        let count = self
            .backends
            .iter()
            .map(|b| {
                if b.metric.status == BackendStatus::Normal {
                    1
                } else {
                    0
                }
            })
            .reduce(|acc, x| acc + x)
            .unwrap_or(0);

        let tab_labels = [
            // Span::raw("listen port: 1229"),
            // Span::raw("load balance algo: round robin"),
            Span::raw(format!("available backends : {}", count)),
        ];
        let divider = "|";

        let tabs = tab_labels.into_iter().map(Spans::from).collect();

        f.render_widget(
            Tabs::new(tabs)
                .block(Block::default().border_style(Style::default().fg(Color::DarkGray)))
                .style(Style::default().fg(Color::DarkGray))
                .divider(divider),
            r,
        );
    }

    fn draw_backends<B: Backend>(&mut self, f: &mut Frame<B>, r: Rect) {
        let block = Block::default().borders(Borders::ALL).title("Backends");

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(2)].as_ref())
            .split(r);

        // header
        let mut header_chunk = chunks[0];
        header_chunk.x = header_chunk.x.saturating_add(1).min(header_chunk.right());
        header_chunk.width = header_chunk.width.saturating_sub(1);

        header_chunk.y = header_chunk.y.saturating_add(1).min(header_chunk.bottom());
        header_chunk.height = header_chunk.height.saturating_sub(1);

        let list_chunk = block.inner(chunks[1]);
        f.render_widget(block, r);

        let header_text = Spans::from(vec![
            Span::from(format!("{}{:<10}", "   ", "name")),
            Span::from(format!("{:<12}", "status")),
            Span::from(format!("{:<12}", "active_conn")),
            Span::from(format!("{:<12}", "conn_time")),
            Span::from(format!("{:<12}", "proto")),
        ]);

        let header = Paragraph::new(header_text).style(Style::default().fg(Color::Blue));
        f.render_widget(header, header_chunk);

        let is_pending = self.fetcher.is_pending();
        let pending_id = self.fetcher.get_pending_on_id();

        let backends: Vec<ListItem> = self
            .backends
            .iter()
            .enumerate()
            .map(|(id, backend)| {
                let current_status = if is_pending && (pending_id == id) {
                    Modifier::CROSSED_OUT
                } else {
                    Modifier::BOLD
                };

                (current_status, backend)
            })
            .map(|(current_status, backend)| {
                let status = match backend.metric.status {
                    BackendStatus::Normal => Style::default().fg(Color::Green),
                    BackendStatus::Closed => Style::default().fg(Color::Red),
                    BackendStatus::Unknown => Style::default().fg(Color::Yellow),
                    BackendStatus::ForceClosed => Style::default().fg(Color::Magenta),
                };

                let item = Spans::from(vec![
                    Span::styled(
                        format!("{:<10}", backend.backend_name),
                        status.add_modifier(current_status),
                    ),
                    Span::styled(
                        format!("{:<12}", backend.metric.status),
                        Style::default().add_modifier(current_status),
                    ),
                    Span::styled(
                        format!("{:<12}", backend.metric.active_connections),
                        Style::default().add_modifier(current_status),
                    ),
                    Span::styled(
                        format!(
                            "{0:<12}",
                            format!("{}ms", backend.metric.connection_time.as_millis())
                        ),
                        Style::default().add_modifier(current_status),
                    ),
                    Span::styled(
                        format!("{:<12}", backend.metric.protocol),
                        Style::default().add_modifier(current_status),
                    ),
                ]);

                ListItem::new(vec![
                    item,
                    Spans::from(" ".repeat(list_chunk.width as usize)),
                ])
                //
            })
            .collect();

        let backends = List::new(backends)
            .start_corner(Corner::TopLeft)
            .highlight_symbol(">> ");

        f.render_stateful_widget(backends, list_chunk, &mut self.backends_state);
    }

    pub fn draw<B: Backend>(&mut self, f: &mut Frame<B>) -> Result<()> {
        let fsize = f.size();

        let chunks_main = Layout::default()
            .direction(Direction::Vertical)
            .constraints(
                [
                    Constraint::Length(2),
                    Constraint::Min(2),
                    Constraint::Length(if self.log_text.is_empty() { 0 } else { 4 }),
                ]
                .as_ref(),
            )
            .split(fsize);

        self.draw_top_bar(f, chunks_main[0]);
        self.draw_backends(f, chunks_main[1]);
        self.draw_log(f, chunks_main[2]);
        Ok(())
    }
}

#[derive(Clone)]
pub struct Input {
    receiver: Receiver<Event>,
}

impl Input {
    pub fn new() -> Self {
        let (tx, rx) = unbounded();

        thread::spawn(move || {
            if let Err(e) = Self::input_loop(&tx) {
                log::error!("input thread error: {}", e);
            }
        });

        Self { receiver: rx }
    }

    fn input_loop(tx: &Sender<Event>) -> Result<()> {
        loop {
            if let Some(e) = Self::poll(POLL_DURATION)? {
                tx.send(e).unwrap();
            }
        }
    }

    pub fn receiver(&self) -> Receiver<Event> {
        self.receiver.clone()
    }

    fn poll(dur: Duration) -> anyhow::Result<Option<Event>> {
        if event::poll(dur)? {
            Ok(Some(event::read()?))
        } else {
            Ok(None)
        }
    }
}
