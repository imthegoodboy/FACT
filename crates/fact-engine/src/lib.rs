//! FACT's in-process terminal engine.
//!
//! One thread owns the ConPTY processes, terminal models and split layout. The
//! UI sends commands over a channel and receives rendered frames over another.
//! There is no background service, named pipe, database or saved session: when
//! the FACT process exits, its panes exit with it.

mod model;
mod session;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use fact_config::Config;
use fact_protocol::{Command, ProtocolError, ResponsePayload, ServerMessage};
use fact_pty::OutputWaker;

use session::{SessionManager, SESSION_NAME};

/// Safety-net tick while nothing happens. Output and exits wake the owner
/// directly, so an idle workspace costs a few wakeups per second.
const IDLE_TICK: Duration = Duration::from_millis(250);
/// Shortest gap between output-driven ticks (~120 frames per second). Bursts
/// that arrive faster are coalesced into the next frame.
const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(8);
/// Retry interval while the UI is too busy to take frames. Slower than the
/// frame cap so a stalled UI does not keep the engine rebuilding snapshots.
const BLOCKED_RETRY: Duration = Duration::from_millis(50);

enum Request {
    Command {
        command: Command,
        reply: SyncSender<Result<ResponsePayload, ProtocolError>>,
    },
    /// A pane produced output or exited.
    Wake,
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

    /// Wait up to `timeout` for the next message. `Ok(None)` means nothing
    /// arrived in time; an error means the engine has stopped.
    pub fn recv_timeout(&mut self, timeout: Duration) -> std::io::Result<Option<ServerMessage>> {
        if let Some(message) = self.pending.take() {
            return Ok(Some(message));
        }
        match self.rx.recv_timeout(timeout) {
            Ok(message) => Ok(Some(message)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(std::io::Error::new(
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
    let (request_tx, request_rx) = mpsc::channel();
    // One pending wake at a time: readers set the flag and post a request only
    // when the owner has already consumed the previous one.
    let wake_pending = Arc::new(AtomicBool::new(false));
    let waker = {
        let tx = request_tx.clone();
        let pending = Arc::clone(&wake_pending);
        OutputWaker::new(move || {
            if !pending.swap(true, Ordering::AcqRel) {
                let _ = tx.send(Request::Wake);
            }
        })
    };
    let mut manager = SessionManager::new(
        config,
        grid,
        profile.as_deref(),
        cwd.as_deref(),
        Some(waker),
    )
    .map_err(anyhow::Error::new)
    .context("could not start the first shell")?;
    let (event_tx, event_rx) = mpsc::sync_channel(8);
    manager.add_subscriber(event_tx);

    let thread = thread::Builder::new()
        .name("fact-engine".into())
        .spawn(move || engine_loop(manager, request_rx, wake_pending))
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

fn engine_loop(
    mut manager: SessionManager,
    requests: Receiver<Request>,
    wake_pending: Arc<AtomicBool>,
) {
    // Only frames caused by output count toward the frame cap. A keystroke's
    // echo must not wait behind the frame its own command just produced.
    let mut last_output_frame = Instant::now() - MIN_FRAME_INTERVAL;
    // Output was drained but its frame is held back to cap the frame rate,
    // or a pane still has queued output.
    let mut pending = false;
    loop {
        let mut timeout = if pending {
            MIN_FRAME_INTERVAL.saturating_sub(last_output_frame.elapsed())
        } else if manager.frames_blocked() {
            BLOCKED_RETRY
        } else {
            IDLE_TICK
        };
        if let Some(deadline) = manager.next_sync_deadline() {
            timeout = timeout.min(deadline.saturating_duration_since(Instant::now()));
        }
        match requests.recv_timeout(timeout) {
            Ok(Request::Command { command, reply }) => {
                let result = manager.handle(command, Some(SESSION_NAME));
                let shutdown = manager.shutdown_requested();
                // User actions get an immediate frame.
                pending = manager.tick();
                let _ = reply.send(result);
                if shutdown {
                    break;
                }
            }
            Ok(Request::Wake) => {
                // Clear before draining so output that lands meanwhile posts a
                // fresh wake instead of being missed.
                wake_pending.store(false, Ordering::Release);
                let backlog = manager.drain_output();
                if last_output_frame.elapsed() >= MIN_FRAME_INTERVAL {
                    manager.publish();
                    pending = backlog;
                    last_output_frame = Instant::now();
                } else {
                    pending = true;
                }
            }
            Ok(Request::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                pending = manager.tick();
                last_output_frame = Instant::now();
            }
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
