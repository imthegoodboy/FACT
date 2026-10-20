use crate::{read_identity, require_protocol, RuntimeError};
use fact_config::Paths;
use fact_ipc::{client_connect, recv_server, send_client, PipeConnection};
use fact_protocol::{
    ClientMessage, Command, Hello, ResponsePayload, ServerMessage, PROTOCOL_VERSION,
};

/// An authenticated control connection to the daemon.
pub struct DaemonClient {
    conn: PipeConnection,
    token: String,
    pipe: String,
    daemon_pid: u32,
    next_request: u64,
}

impl std::fmt::Debug for DaemonClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonClient")
            .field("pipe", &self.pipe)
            .field("daemon_pid", &self.daemon_pid)
            .finish_non_exhaustive()
    }
}

/// A one-way stream of frames and lifecycle notices.
pub struct EventStream {
    conn: PipeConnection,
}

impl std::fmt::Debug for EventStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventStream").finish_non_exhaustive()
    }
}

/// Connect and authenticate to the daemon described by the current identity
/// file. Returns the identity too, which is useful for diagnostics.
pub fn connect(paths: &Paths) -> Result<(crate::DaemonIdentity, DaemonClient), RuntimeError> {
    let identity = read_identity(paths)?;
    require_protocol(identity.protocol)?;
    let conn = authenticate(&identity.pipe, &identity.token, Some(identity.pid))?;
    Ok((
        identity.clone(),
        DaemonClient {
            conn,
            token: identity.token,
            pipe: identity.pipe,
            daemon_pid: identity.pid,
            next_request: 0,
        },
    ))
}

impl DaemonClient {
    pub fn request(&mut self, command: Command) -> Result<ResponsePayload, RuntimeError> {
        self.next_request = self.next_request.wrapping_add(1);
        let id = self.next_request;
        send_client(&mut self.conn, &ClientMessage::Request { id, command })?;
        loop {
            match recv_server(&mut self.conn)? {
                ServerMessage::RequestOk {
                    id: response_id,
                    payload,
                } if response_id == id => return Ok(payload),
                ServerMessage::RequestErr {
                    id: response_id,
                    error,
                } if response_id == id => return Err(RuntimeError::Request(error)),
                ServerMessage::Fatal(error) => {
                    return Err(RuntimeError::Request(error));
                }
                // Stale responses should not happen on a serial control
                // connection, but ignore them rather than desynchronize.
                _ => continue,
            }
        }
    }

    /// Open the separate event pipe and subscribe with the one-time token from
    /// `AttachInfo`. The control connection stays usable for input and commands.
    pub fn subscribe(&self, stream_token: &str) -> Result<EventStream, RuntimeError> {
        let mut conn = authenticate(&self.pipe, &self.token, Some(self.daemon_pid))?;
        send_client(
            &mut conn,
            &ClientMessage::Subscribe {
                token: stream_token.to_string(),
            },
        )?;
        Ok(EventStream { conn })
    }

    pub fn pipe(&self) -> &str {
        &self.pipe
    }

    pub fn daemon_pid(&self) -> u32 {
        self.daemon_pid
    }
}

impl EventStream {
    /// Whether at least one byte is queued, without blocking. This lets a
    /// reader loop wake up and check a cancellation flag when a session is
    /// switched or the UI closes.
    pub fn available(&self) -> Result<bool, RuntimeError> {
        Ok(self.conn.bytes_available()? > 0)
    }

    /// Receive the next frame or lifecycle message.
    pub fn recv(&mut self) -> Result<ServerMessage, RuntimeError> {
        loop {
            match recv_server(&mut self.conn)? {
                ServerMessage::Frame(frame) => return Ok(ServerMessage::Frame(frame)),
                ServerMessage::SessionEnded { reason } => {
                    return Ok(ServerMessage::SessionEnded { reason });
                }
                ServerMessage::Fatal(error) => return Err(RuntimeError::Request(error)),
                ServerMessage::HelloOk { .. } => continue,
                other => {
                    return Err(RuntimeError::Protocol(format!(
                        "unexpected event message: {other:?}"
                    )))
                }
            }
        }
    }
}

fn authenticate(
    pipe: &str,
    token: &str,
    expected_pid: Option<u32>,
) -> Result<PipeConnection, RuntimeError> {
    let mut conn = client_connect(pipe)?;
    let hello = Hello {
        protocol: PROTOCOL_VERSION,
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        token: token.to_string(),
    };
    send_client(&mut conn, &ClientMessage::Hello(hello))?;
    match recv_server(&mut conn)? {
        ServerMessage::HelloOk {
            protocol,
            daemon_version: _,
            daemon_pid,
        } => {
            require_protocol(protocol)?;
            if let Some(expected) = expected_pid {
                if daemon_pid != expected {
                    return Err(RuntimeError::InvalidIdentity(format!(
                        "pipe is owned by pid {daemon_pid}, identity file says {expected}"
                    )));
                }
            }
            Ok(conn)
        }
        ServerMessage::Fatal(error) => Err(RuntimeError::Unauthorized(error.message)),
        other => Err(RuntimeError::Protocol(format!(
            "unexpected handshake response: {other:?}"
        ))),
    }
}
