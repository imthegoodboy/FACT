//! Daemon discovery, identity files and authenticated client connections.
//!
//! The daemon publishes a random token in an ACL-protected file. No network
//! listener exists: every client connects to a local named pipe and must prove
//! knowledge of the token before the daemon accepts a request.

mod client;
mod identity;

pub use client::{connect, DaemonClient, EventStream};
pub use identity::{
    generate_token, pipe_name, read_identity, remove_identity_if_token, validate_identity,
    write_identity, DaemonIdentity, IDENTITY_FILE,
};

use fact_protocol::{ProtocolError, PROTOCOL_VERSION};
use thiserror::Error;

pub(crate) fn require_protocol(found: u16) -> Result<(), RuntimeError> {
    if found == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(RuntimeError::Protocol(format!(
            "protocol mismatch: daemon speaks {found}, client speaks {PROTOCOL_VERSION}"
        )))
    }
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("FACT daemon is not running: {0}")]
    NotRunning(String),
    #[error("daemon identity is invalid: {0}")]
    InvalidIdentity(String),
    #[error("daemon rejected authentication: {0}")]
    Unauthorized(String),
    #[error("daemon request failed: {0}")]
    Request(#[from] ProtocolError),
    #[error("daemon protocol error: {0}")]
    Protocol(String),
    #[error("IPC error: {0}")]
    Ipc(#[from] fact_ipc::IpcError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid daemon state file: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unexpected daemon response: {0}")]
    Unexpected(&'static str),
}
