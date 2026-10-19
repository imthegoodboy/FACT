use crate::RuntimeError;
use fact_config::Paths;
use fact_protocol::PROTOCOL_VERSION;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

pub const IDENTITY_FILE: &str = "daemon.json";
const MAX_IDENTITY_BYTES: u64 = 16 * 1024;
const TOKEN_BYTES: usize = 32;

/// Everything a client needs to find and authenticate a running daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonIdentity {
    pub protocol: u16,
    pub daemon_version: String,
    pub pid: u32,
    /// 64 lowercase hex characters (256 random bits).
    pub token: String,
    /// `\\.\pipe\fact-...`
    pub pipe: String,
    pub started_at_ms: i64,
}

pub fn generate_token() -> Result<String, RuntimeError> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|error| RuntimeError::Protocol(format!("secure RNG failed: {error}")))?;
    let mut out = String::with_capacity(TOKEN_BYTES * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

/// Derive a stable, per-data-directory pipe name. Including a hash of the data
/// directory keeps separate FACT homes (and separate users) isolated without
/// relying on a network namespace.
pub fn pipe_name(paths: &Paths) -> String {
    // Canonicalize the parent when the directory does not exist yet: the name
    // must stay identical before and after `paths.ensure()` creates the base.
    let base = if let Ok(canonical) = fs::canonicalize(&paths.base) {
        canonical
    } else if let (Some(parent), Some(name)) = (paths.base.parent(), paths.base.file_name()) {
        fs::canonicalize(parent)
            .map(|parent| parent.join(name))
            .unwrap_or_else(|_| paths.base.clone())
    } else {
        paths.base.clone()
    };
    let normalized = base.to_string_lossy().to_ascii_lowercase();
    let digest = Sha256::digest(normalized.as_bytes());
    let mut hex = String::with_capacity(32);
    for byte in &digest[..12] {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    format!(r"\\.\pipe\fact-{hex}")
}

pub fn validate_identity(identity: &DaemonIdentity) -> Result<(), RuntimeError> {
    if identity.pid == 0 {
        return Err(RuntimeError::InvalidIdentity("pid is zero".into()));
    }
    if identity.daemon_version.trim().is_empty() || identity.daemon_version.len() > 64 {
        return Err(RuntimeError::InvalidIdentity(
            "daemon version is invalid".into(),
        ));
    }
    if identity.token.len() != TOKEN_BYTES * 2
        || !identity.token.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(RuntimeError::InvalidIdentity(
            "daemon token is not 256 bits of hex".into(),
        ));
    }
    if !identity.pipe.starts_with(r"\\.\pipe\fact-")
        || identity.pipe.len() > 256
        || identity.pipe.contains('\0')
        || identity.pipe[r"\\.\pipe\".len()..].contains('\\')
    {
        return Err(RuntimeError::InvalidIdentity(
            "daemon pipe name is invalid".into(),
        ));
    }
    if identity.protocol != PROTOCOL_VERSION {
        return Err(RuntimeError::InvalidIdentity(format!(
            "daemon protocol {} does not match client protocol {PROTOCOL_VERSION}",
            identity.protocol
        )));
    }
    Ok(())
}

pub fn read_identity(paths: &Paths) -> Result<DaemonIdentity, RuntimeError> {
    let path = &paths.daemon_json;
    let metadata = fs::metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            RuntimeError::NotRunning(format!(
                "{} does not exist; start the daemon with `fact resume`",
                path.display()
            ))
        } else {
            RuntimeError::Io(error)
        }
    })?;
    if metadata.len() > MAX_IDENTITY_BYTES {
        return Err(RuntimeError::InvalidIdentity(
            "daemon identity file is too large".into(),
        ));
    }
    let text = fs::read_to_string(path)?;
    let identity: DaemonIdentity = serde_json::from_str(&text)?;
    validate_identity(&identity)?;
    Ok(identity)
}

/// Publish a new identity file. The runtime directory and file are restricted
/// to the current user and SYSTEM before any token content is written.
pub fn write_identity(paths: &Paths, identity: &DaemonIdentity) -> Result<(), RuntimeError> {
    paths.ensure()?;
    secure_path(&paths.runtime_dir)?;
    secure_path(&paths.base)?;
    let json = serde_json::to_vec_pretty(identity)?;
    let tmp = paths
        .runtime_dir
        .join(format!("{IDENTITY_FILE}.tmp.{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    secure_path(&tmp)?;
    file.write_all(&json)?;
    file.sync_all()?;
    drop(file);
    if paths.daemon_json.exists() {
        fs::remove_file(&paths.daemon_json)?;
    }
    fs::rename(&tmp, &paths.daemon_json)?;
    secure_path(&paths.daemon_json)?;
    Ok(())
}

pub fn remove_identity_if_token(paths: &Paths, token: &str) -> bool {
    let Ok(identity) = read_identity(paths) else {
        return false;
    };
    if !constant_time_eq(&identity.token, token) {
        return false;
    }
    let _ = fs::remove_file(&paths.daemon_json);
    true
}

pub(crate) fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn secure_path(path: &Path) -> Result<(), RuntimeError> {
    #[cfg(windows)]
    {
        fact_ipc::secure_file_acl(path)?;
    }
    #[cfg(not(windows))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_name_is_stable_before_and_after_the_base_directory_exists() {
        let base = std::env::temp_dir().join(format!(
            "fact-pipe-test-{}-{}",
            std::process::id(),
            generate_token().expect("token")
        ));
        let _ = fs::remove_dir_all(&base);
        let paths = Paths::from_parts(base.clone(), base.join("config.toml"));
        let before = pipe_name(&paths);
        paths.ensure().expect("create base");
        let after = pipe_name(&paths);
        assert_eq!(before, after);
        assert!(before.starts_with(r"\\.\pipe\fact-"));
        let _ = fs::remove_dir_all(&base);
    }
}
