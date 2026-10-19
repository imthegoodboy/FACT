//! Real Windows ConPTY process engine used by every FACT pane.
//!
//! Each pane owns an independent pseudo console, shell process and job object.
//! There is no shared console: `fact --grid 8` starts eight real shells.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
