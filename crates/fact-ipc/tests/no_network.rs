//! Guard the local-only IPC claim.
//!
//! FACT intentionally has no network listener. This source-level test is not a
//! substitute for reviewing dependencies, but it catches accidental use of the
//! standard library's TCP/UDP/HTTP APIs in any workspace crate.

use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN: &[&str] = &[
    "std::net::",
    "TcpListener",
    "TcpStream",
    "UdpSocket",
    "UdpBuilder",
    "TcpBuilder",
    "HttpListener",
    "WebSocket",
    "WSASocket",
    "WSAConnect",
    "socket2",
    "tokio::net",
    "async_std::net",
    "ureq::",
    "reqwest::",
];

fn rust_sources(directory: &Path, files: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", directory.display()));
    for entry in entries {
        let entry = entry.expect("directory entry");
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, files);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            files.push(path);
        }
    }
}

#[test]
fn workspace_sources_do_not_create_network_streams() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let crates = workspace.join("crates");
    let entries = fs::read_dir(&crates).expect("read crates directory");

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.expect("crate entry");
        let src = entry.path().join("src");
        if src.is_dir() {
            rust_sources(&src, &mut files);
        }
    }
    assert!(!files.is_empty(), "no workspace source files were found");

    let mut violations = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file)
            .unwrap_or_else(|error| panic!("could not read {}: {error}", file.display()));
        for needle in FORBIDDEN {
            if text.contains(needle) {
                violations.push(format!("{} contains {needle}", file.display()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "FACT must remain local-only; network APIs found:\n{}",
        violations.join("\n")
    );
}
