#[path = "terminal_app.rs"]
mod app;
#[path = "api_client.rs"]
mod client;
#[path = "input_events.rs"]
mod events;
#[path = "terminal_renderer.rs"]
mod ui;

use std::{
    io::{self, Stdout},
    sync::Arc,
};

use anyhow::Context;
use crossterm::{
    cursor::{Hide, Show},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::{
    sync::mpsc::unbounded_channel,
    time::{self, Duration},
};

use self::{
    app::{App, AppCommand},
    client::{dispatch_command, ApiClient, NetworkResult},
    events::{EventPump, InputEvent},
};

pub async fn run_tui(api_url: Option<String>) -> anyhow::Result<()> {
    let client = Arc::new(ApiClient::new(api_url)?);
    let _panic_guard = PanicRestoreGuard::install();
    let mut session = TerminalSession::enter().context("enter terminal UI mode")?;

    let (input_tx, mut input_rx) = unbounded_channel::<InputEvent>();
    let (network_tx, mut network_rx) = unbounded_channel::<NetworkResult>();
    let _pump = EventPump::start(input_tx);

    let (mut app, commands) = App::new(client.base_url().to_string());
    dispatch_commands(&client, &network_tx, commands);

    let mut tick = time::interval(Duration::from_millis(125));
    loop {
        session
            .terminal
            .draw(|frame| ui::draw(frame, &app))
            .context("render TUI frame")?;

        tokio::select! {
            Some(input) = input_rx.recv() => {
                match input {
                    InputEvent::Key(key) => {
                        let commands = app.handle_key(key);
                        if should_quit(&commands) {
                            break;
                        }
                        dispatch_commands(&client, &network_tx, commands);
                    }
                    InputEvent::Resize => {}
                }
            }
            Some(result) = network_rx.recv() => {
                let commands = app.handle_network_result(result);
                dispatch_commands(&client, &network_tx, commands);
            }
            _ = tick.tick() => {
                let commands = app.on_tick();
                dispatch_commands(&client, &network_tx, commands);
            }
        }
    }

    session
        .restore()
        .context("restore terminal after TUI exit")?;
    Ok(())
}

fn should_quit(commands: &[AppCommand]) -> bool {
    commands
        .iter()
        .any(|command| matches!(command, AppCommand::Quit))
}

fn dispatch_commands(
    client: &Arc<ApiClient>,
    sender: &tokio::sync::mpsc::UnboundedSender<NetworkResult>,
    commands: Vec<AppCommand>,
) {
    for command in commands {
        if let AppCommand::Network(network) = command {
            dispatch_command(Arc::clone(client), sender.clone(), network);
        }
    }
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    restored: bool,
}

impl TerminalSession {
    fn enter() -> anyhow::Result<Self> {
        enable_raw_mode().context("enable raw mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Hide).context("enter alternate screen")?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).context("create ratatui terminal")?;
        Ok(Self {
            terminal,
            restored: false,
        })
    }

    fn restore(&mut self) -> anyhow::Result<()> {
        if self.restored {
            return Ok(());
        }
        restore_terminal_now().context("restore raw mode and screen")?;
        self.terminal.show_cursor().context("show cursor")?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if !self.restored {
            let _ = restore_terminal_now();
            let _ = self.terminal.show_cursor();
        }
    }
}

struct PanicRestoreGuard;

impl PanicRestoreGuard {
    fn install() -> Self {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            let _ = restore_terminal_now();
            previous(panic_info);
        }));
        Self
    }
}

fn restore_terminal_now() -> io::Result<()> {
    disable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, LeaveAlternateScreen, Show)
}
