//! FACT's in-process terminal engine.
//!
//! One thread owns the ConPTY processes, terminal models and split layout. The
//! UI sends commands over a channel and receives rendered frames over another.
//! There is no background service, named pipe, database or saved session: when
//! the FACT process exits, its panes exit with it.

mod model;
mod session;

use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use fact_config::Config;
use fact_protocol::{Command, ProtocolError, ResponsePayload, ServerMessage};

use session::{SessionManager, SESSION_NAME};

const TICK: Duration = Duration::from_millis(33);

enum Request {
    Command {
        command: Command,
        reply: SyncSender<Result<ResponsePayload, ProtocolError>>,
    },
    Shutdown,
}

/// Owns the engine thread and stops it when the UI exits.
pub struct Engine {
    requests: Sender<Request>,
    thread: Option<JoinHandle<()>>,
}

/// Synchronous command handle used by the UI.
#[derive(Clone)]
pub struct EngineClient {
    requests: Sender<Request>,
}

impl EngineClient {
    pub fn request(&self, command: Command) -> Result<ResponsePayload> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.requests
            .send(Request::Command { command, reply: tx })
            .context("FACT engine is no longer running")?;
        rx.recv()
            .context("FACT engine stopped before answering")?
            .map_err(anyhow::Error::new)
    }
}

/// Ordered frame stream consumed by the UI reader thread.
pub struct EventStream {
    rx: Receiver<ServerMessage>,
    pending: Option<ServerMessage>,
}

impl EventStream {
    pub fn available(&mut self) -> std::io::Result<bool> {
        if self.pending.is_some() {
            return Ok(true);
        }
        match self.rx.try_recv() {
            Ok(message) => {
                self.pending = Some(message);
                Ok(true)
            }
            Err(TryRecvError::Empty) => Ok(false),
            Err(TryRecvError::Disconnected) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "FACT engine closed",
            )),
        }
    }

    pub fn recv(&mut self) -> Result<ServerMessage> {
        if let Some(message) = self.pending.take() {
            return Ok(message);
        }
        self.rx
            .recv()
            .map_err(|_| anyhow::anyhow!("FACT engine closed"))
    }
}

/// Everything a UI needs to drive a fresh workspace.
pub struct StartedEngine {
    pub engine: Engine,
    pub client: EngineClient,
    pub events: EventStream,
}

/// Spawn the local engine with a fresh workspace. Nothing is restored.
pub fn start(
    config: Config,
    grid: u8,
    profile: Option<String>,
    cwd: Option<String>,
) -> Result<StartedEngine> {
    let mut manager = SessionManager::new(config, grid, profile.as_deref(), cwd.as_deref())
        .map_err(anyhow::Error::new)
        .context("could not start the first shell")?;
    let (event_tx, event_rx) = mpsc::sync_channel(8);
    manager.add_subscriber(event_tx);

    let (request_tx, request_rx) = mpsc::channel();
    let thread = thread::Builder::new()
        .name("fact-engine".into())
        .spawn(move || engine_loop(manager, request_rx))
        .context("could not start the FACT engine thread")?;

    Ok(StartedEngine {
        engine: Engine {
            requests: request_tx.clone(),
            thread: Some(thread),
        },
        client: EngineClient {
            requests: request_tx,
        },
        events: EventStream {
            rx: event_rx,
            pending: None,
        },
    })
}

fn engine_loop(mut manager: SessionManager, requests: Receiver<Request>) {
    loop {
        match requests.recv_timeout(TICK) {
            Ok(Request::Command { command, reply }) => {
                let result = manager.handle(command, Some(SESSION_NAME));
                let shutdown = manager.shutdown_requested();
                manager.tick();
                let _ = reply.send(result);
                if shutdown {
                    break;
                }
            }
            Ok(Request::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => manager.tick(),
        }
    }
    manager.shutdown();
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
