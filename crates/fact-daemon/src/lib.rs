//! FACT per-user daemon.
//!
//! One daemon owns every ConPTY process and terminal model for a user. Clients
//! connect over an authenticated local named pipe; there is no network listener.
//! All state mutation runs on the owner thread so PTY output, SQLite writes and
//! frame fan-out never race each other.

mod manager;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context};
use fact_config::{Config, Paths};
use fact_ipc::{
    client_connect, recv_client, secure_file_acl, send_server, PipeConnection, PipeListener,
};
use fact_protocol::{
    ClientMessage, Command, ProtocolError, ResponsePayload, ServerMessage, PROTOCOL_VERSION,
};
use fact_runtime::{
    generate_token, pipe_name, remove_identity_if_token, write_identity, DaemonIdentity,
};
use manager::SessionManager;

const TICK: Duration = Duration::from_millis(33);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const EVENT_QUEUE: usize = 8;
static NEXT_SUBSCRIBER_ID: AtomicU64 = AtomicU64::new(1);

/// Start a daemon using the configuration file for `paths`.
pub fn run(paths: Paths) -> anyhow::Result<()> {
    let config = Config::load(&paths).context("loading FACT configuration")?;
    run_with_config(paths, config)
}

/// Start a daemon with an explicit configuration. Primarily useful for tests
/// and for callers that already validated the config.
pub fn run_with_config(paths: Paths, config: Config) -> anyhow::Result<()> {
    config.validate().context("validating FACT configuration")?;
    paths.ensure().context("creating FACT data directories")?;
    secure_file_acl(&paths.base).context("securing FACT data directory")?;
    secure_file_acl(&paths.runtime_dir).context("securing FACT runtime directory")?;
    secure_file_acl(&paths.log_dir).context("securing FACT log directory")?;

    let token = generate_token().context("generating daemon token")?;
    let pipe = pipe_name(&paths);
    let listener = PipeListener::bind(&pipe).map_err(|error| {
        anyhow!(
            "could not create local pipe '{pipe}': {error}. Another FACT daemon may already be running."
        )
    })?;
    let identity = DaemonIdentity {
        protocol: PROTOCOL_VERSION,
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
        token: token.clone(),
        pipe: pipe.clone(),
        started_at_ms: fact_session::now_ms(),
    };
    write_identity(&paths, &identity).context("publishing daemon identity")?;

    log_line(
        &paths,
        &format!(
            "daemon {} started pid={} pipe={}",
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
            pipe
        ),
    );

    let (event_tx, event_rx) = mpsc::channel::<DaemonEvent>();
    let auth = Arc::new(Auth {
        token: token.clone(),
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
    });
    let stop = Arc::new(AtomicBool::new(false));
    let accept_auth = Arc::clone(&auth);
    let accept_tx = event_tx.clone();
    let accept_stop = Arc::clone(&stop);
    let accept_log = paths.daemon_log.clone();
    let accept_handle = thread::Builder::new()
        .name("fact-accept".into())
        .spawn(move || accept_loop(listener, accept_auth, accept_tx, accept_stop, accept_log))
        .context("spawning daemon accept thread")?;

    let mut manager = SessionManager::new(paths.clone(), config).context("opening sessions")?;
    let mut fatal_error: Option<anyhow::Error> = None;
    loop {
        let event = event_rx.recv_timeout(TICK);
        match event {
            Ok(DaemonEvent::Command {
                command,
                bound,
                reply,
            }) => {
                let result = manager.handle(command, bound.as_deref());
                let shutdown = manager.shutdown_requested();
                manager.tick();
                let _ = reply.send(result);
                if shutdown {
                    break;
                }
            }
            Ok(DaemonEvent::Subscribe {
                id,
                token: stream_token,
                tx,
            }) => {
                if let Err(error) = manager.subscribe(id, &stream_token, tx.clone()) {
                    let _ = tx.try_send(ServerMessage::Fatal(error));
                } else {
                    manager.tick();
                }
            }
            Ok(DaemonEvent::Unsubscribe { id }) => {
                manager.unsubscribe(id);
            }
            Err(RecvTimeoutError::Timeout) => manager.tick(),
            Err(RecvTimeoutError::Disconnected) => {
                fatal_error = Some(anyhow!("daemon command channel closed unexpectedly"));
                break;
            }
        }
    }

    manager.shutdown();
    drop(manager);
    stop.store(true, Ordering::Release);
    // Wake the blocking accept call so the listener is dropped before the
    // identity file is removed.
    for _ in 0..20 {
        if client_connect(&pipe).is_ok() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = accept_handle.join();
    remove_identity_if_token(&paths, &token);
    log_line(&paths, "daemon stopped");
    if let Some(error) = fatal_error {
        return Err(error);
    }
    Ok(())
}

struct Auth {
    token: String,
    daemon_version: String,
}

enum DaemonEvent {
    Command {
        command: Command,
        bound: Option<String>,
        reply: SyncSender<Result<ResponsePayload, ProtocolError>>,
    },
    Subscribe {
        id: u64,
        token: String,
        tx: SyncSender<ServerMessage>,
    },
    Unsubscribe {
        id: u64,
    },
}

fn accept_loop(
    mut listener: PipeListener,
    auth: Arc<Auth>,
    events: mpsc::Sender<DaemonEvent>,
    stop: Arc<AtomicBool>,
    log_path: std::path::PathBuf,
) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok(connection) => {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let auth = Arc::clone(&auth);
                let events = events.clone();
                let _ = thread::Builder::new()
                    .name("fact-connection".into())
                    .spawn(move || handle_connection(connection, auth, events));
            }
            Err(error) => {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                append_log(&log_path, &format!("accept failed: {error}; retrying"));
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn handle_connection(
    mut connection: PipeConnection,
    auth: Arc<Auth>,
    events: mpsc::Sender<DaemonEvent>,
) {
    let hello = match recv_client(&mut connection) {
        Ok(ClientMessage::Hello(hello)) => hello,
        Ok(_) => {
            let _ = send_server(
                &mut connection,
                &ServerMessage::Fatal(ProtocolError::new(
                    fact_protocol::ErrorCode::Unauthorized,
                    "the first message must be Hello",
                )),
            );
            return;
        }
        Err(_) => return,
    };
    if hello.protocol != PROTOCOL_VERSION {
        let _ = send_server(
            &mut connection,
            &ServerMessage::Fatal(ProtocolError::new(
                fact_protocol::ErrorCode::VersionMismatch,
                format!(
                    "protocol mismatch: client {} daemon {}",
                    hello.protocol, PROTOCOL_VERSION
                ),
            )),
        );
        return;
    }
    if !constant_time_eq(&hello.token, &auth.token) {
        let _ = send_server(
            &mut connection,
            &ServerMessage::Fatal(ProtocolError::new(
                fact_protocol::ErrorCode::Unauthorized,
                "invalid daemon token",
            )),
        );
        return;
    }
    if send_server(
        &mut connection,
        &ServerMessage::HelloOk {
            protocol: PROTOCOL_VERSION,
            daemon_version: auth.daemon_version.clone(),
            daemon_pid: std::process::id(),
        },
    )
    .is_err()
    {
        return;
    }

    let mut bound_session: Option<String> = None;
    loop {
        let message = match recv_client(&mut connection) {
            Ok(message) => message,
            Err(_) => return,
        };
        match message {
            ClientMessage::Request { id, command } => {
                let rename = matches!(command, Command::RenameSession { .. });
                let (reply_tx, reply_rx) = mpsc::sync_channel(1);
                if events
                    .send(DaemonEvent::Command {
                        command,
                        bound: bound_session.clone(),
                        reply: reply_tx,
                    })
                    .is_err()
                {
                    return;
                }
                let result = match reply_rx.recv_timeout(REQUEST_TIMEOUT) {
                    Ok(result) => result,
                    Err(_) => Err(ProtocolError::internal(
                        "daemon did not answer the request in time",
                    )),
                };
                if let Ok(ResponsePayload::Attached(info)) = &result {
                    bound_session = Some(info.session.name.clone());
                }
                if rename {
                    if let Ok(ResponsePayload::Session(summary)) = &result {
                        bound_session = Some(summary.name.clone());
                    }
                }
                let response = match result {
                    Ok(payload) => ServerMessage::RequestOk { id, payload },
                    Err(error) => ServerMessage::RequestErr { id, error },
                };
                if send_server(&mut connection, &response).is_err() {
                    return;
                }
            }
            ClientMessage::Subscribe { token } => {
                let id = NEXT_SUBSCRIBER_ID.fetch_add(1, Ordering::Relaxed);
                let (tx, rx) = mpsc::sync_channel::<ServerMessage>(EVENT_QUEUE);
                if events
                    .send(DaemonEvent::Subscribe { id, token, tx })
                    .is_err()
                {
                    return;
                }
                while let Ok(message) = rx.recv() {
                    if send_server(&mut connection, &message).is_err() {
                        break;
                    }
                }
                // The event stream is the lifetime of the subscriber. Tell the
                // manager even when the pipe closed on the last frame so
                // `fact stats` does not count dead clients forever.
                let _ = events.send(DaemonEvent::Unsubscribe { id });
                return;
            }
            ClientMessage::Hello(_) => {
                let _ = send_server(
                    &mut connection,
                    &ServerMessage::Fatal(ProtocolError::invalid(
                        "this connection is already authenticated",
                    )),
                );
                return;
            }
        }
    }
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn log_line(paths: &Paths, message: &str) {
    append_log(&paths.daemon_log, message);
}

fn append_log(path: &std::path::Path, message: &str) {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(
            file,
            "{} pid={} {message}",
            fact_session::now_ms(),
            std::process::id()
        );
    }
}
