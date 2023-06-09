use std::{
    io::{self, Write},
    time::{Duration, Instant},
};

use clap::Parser;
use crossbeam_channel::{bounded, tick, unbounded, Receiver, Select};
use crossterm::{
    event::Event,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};

use anyhow::{bail, Result};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    Terminal,
};
use scopeguard::defer;
use serde::{Deserialize, Serialize};
use spinner::Spinner;

use crate::{
    app::{App, Input},
    fetch::Fetcher,
};

mod app;
mod fetch;
mod spinner;

static SPINNER_INTERVAL: Duration = Duration::from_millis(80);

#[derive(Serialize, Deserialize)]
pub(crate) enum MetricApiNotify {
    SwitchBackendStatus,
    SwitchBackendProtocol,
    ResetBackend,
    AllBackends,
    Error(String),
}

/// TUI commandline args
#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
pub struct Cmd {
    #[arg(short = 'p', default_value_t = 8404)]
    port: u64,
}

impl Cmd {
    pub fn run(self) -> Result<()> {
        //
        env_logger::init();

        setup_terminal()?;
        defer! {
            shutdown_terminal();
        };

        let mut terminal = start_terminal(io::stdout())?;
        let input = Input::new();

        run_app(&input, &mut terminal, self.port)
    }
}

fn run_app(
    input: &Input,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    port: u64,
) -> Result<(), anyhow::Error> {
    let (close_tx, close_rx) = bounded::<()>(1);
    let address = format!("ws://localhost:{}/", port);

    let (tx_fetcher, rx_fetcher) = unbounded();
    let fetcher = Fetcher::new(address, tx_fetcher, close_tx);

    let rx_input = input.receiver();
    let rx_spinner = tick(SPINNER_INTERVAL);

    let mut app = App::new(fetcher)?;
    let mut spinner = Spinner::default();

    loop {
        let event = select_event(&rx_input, &rx_spinner, &rx_fetcher, &close_rx)?;

        if matches!(event, QueueEvent::SpinnerUpdate) {
            spinner.update();
            spinner.draw(terminal)?;
            continue;
        }

        match event {
            QueueEvent::InputEvent(e) => {
                app.event(e)?;
            }
            QueueEvent::Fetch(MetricApiNotify::Error(msg)) => app.set_error(msg),
            QueueEvent::Fetch(_) => {}
            QueueEvent::AppClose => {
                break;
            }
            QueueEvent::SpinnerUpdate => unreachable!(),
        }

        draw(terminal, &mut app)?;

        spinner.set_state(app.any_work_pending());
        spinner.draw(terminal)?;

        if app.do_quit {
            break;
        }
    }
    Ok(())
}

fn draw<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> io::Result<()> {
    terminal.draw(|f| {
        if let Err(e) = app.draw(f) {
            log::error!("failed to draw: {:?}", e);
        }
    })?;

    Ok(())
}

fn select_event(
    rx_input: &Receiver<Event>,
    rx_spinner: &Receiver<Instant>,
    rx_fetcher: &Receiver<MetricApiNotify>,
    close_rx: &Receiver<()>,
) -> Result<QueueEvent> {
    let mut sel = Select::new();

    sel.recv(rx_input);
    sel.recv(rx_spinner);
    sel.recv(rx_fetcher);
    sel.recv(close_rx);

    let oper = sel.select();
    let index = oper.index();

    let ev = match index {
        0 => oper.recv(rx_input).map(QueueEvent::InputEvent),
        1 => oper.recv(rx_spinner).map(|_| QueueEvent::SpinnerUpdate),
        2 => oper.recv(rx_fetcher).map(QueueEvent::Fetch),
        3 => oper.recv(close_rx).map(|_| QueueEvent::AppClose),
        _ => bail!("unknown select source"),
    }?;

    Ok(ev)
}

pub(crate) enum QueueEvent {
    AppClose,
    SpinnerUpdate,
    Fetch(MetricApiNotify),
    InputEvent(Event),
}

fn setup_terminal() -> Result<()> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    Ok(())
}

fn shutdown_terminal() {
    let leave_screen = io::stdout().execute(LeaveAlternateScreen).map(|_f| ());

    if let Err(e) = leave_screen {
        eprintln!("leave_screen failed:\n{e}");
    }

    let leave_raw_mode = disable_raw_mode();

    if let Err(e) = leave_raw_mode {
        eprintln!("leave_raw_mode failed:\n{e}");
    }
}

fn start_terminal<W: Write>(buf: W) -> io::Result<Terminal<CrosstermBackend<W>>> {
    let backend = CrosstermBackend::new(buf);
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;
    terminal.clear()?;

    Ok(terminal)
}
