//! Local authenticated IPC transport. Windows named pipes, no network listeners.

use fact_protocol::{ClientMessage, ServerMessage, MAX_WIRE_FRAME};
use std::io::{Read, Write};
use thiserror::Error;

/// Control requests can carry session imports, so they share the wire limit.
pub const MAX_CONTROL_FRAME: usize = MAX_WIRE_FRAME;

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("IPC frame is too large ({size} bytes)")]
    FrameTooLarge { size: usize },
    #[error("IPC frame is empty")]
    EmptyFrame,
    #[error("IPC I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("IPC protocol error: {0}")]
    Protocol(String),
}

pub fn write_frame<W: Write>(writer: &mut W, bytes: &[u8]) -> Result<(), IpcError> {
    if bytes.is_empty() {
        return Err(IpcError::EmptyFrame);
    }
    if bytes.len() > MAX_WIRE_FRAME {
        return Err(IpcError::FrameTooLarge { size: bytes.len() });
    }
    let len = (bytes.len() as u32).to_le_bytes();
    writer.write_all(&len)?;
    writer.write_all(bytes)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame<R: Read>(reader: &mut R, max: usize) -> Result<Vec<u8>, IpcError> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 {
        return Err(IpcError::EmptyFrame);
    }
    if len > max || len > MAX_WIRE_FRAME {
        return Err(IpcError::FrameTooLarge { size: len });
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn encode<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, IpcError> {
    bincode::serialize(value).map_err(|e| IpcError::Protocol(e.to_string()))
}

pub fn decode<'a, T: serde::Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, IpcError> {
    bincode::deserialize(bytes).map_err(|e| IpcError::Protocol(e.to_string()))
}

pub fn send_client<W: Write>(writer: &mut W, msg: &ClientMessage) -> Result<(), IpcError> {
    let bytes = encode(msg)?;
    if bytes.len() > MAX_CONTROL_FRAME {
        return Err(IpcError::FrameTooLarge { size: bytes.len() });
    }
    write_frame(writer, &bytes)
}

pub fn recv_client<R: Read>(reader: &mut R) -> Result<ClientMessage, IpcError> {
    let bytes = read_frame(reader, MAX_CONTROL_FRAME)?;
    decode(&bytes)
}

pub fn send_server<W: Write>(writer: &mut W, msg: &ServerMessage) -> Result<(), IpcError> {
    let bytes = encode(msg)?;
    write_frame(writer, &bytes)
}

pub fn recv_server<R: Read>(reader: &mut R) -> Result<ServerMessage, IpcError> {
    let bytes = read_frame(reader, MAX_WIRE_FRAME)?;
    decode(&bytes)
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{
    client_connect, current_user_sid_string, file_dacl_sddl, secure_file_acl, server_accept,
    PipeConnection, PipeListener, PipeSecurity,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello").unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut cursor, 1024).unwrap(), b"hello");
    }

    #[test]
    fn rejects_zero_and_oversize_frames() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_le_bytes());
        let mut cursor = std::io::Cursor::new(buf);
        assert!(matches!(
            read_frame(&mut cursor, 1024),
            Err(IpcError::EmptyFrame)
        ));

        let mut buf = Vec::new();
        buf.extend_from_slice(&(2048u32).to_le_bytes());
        let mut cursor = std::io::Cursor::new(buf);
        assert!(matches!(
            read_frame(&mut cursor, 1024),
            Err(IpcError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn rejects_malformed_bincode() {
        assert!(decode::<ClientMessage>(&[0xFF, 0xFF, 0xFF]).is_err());
    }
}
