//! In-memory session state owned by the engine.
//!
//! Nothing in this module is written to disk. A session lives exactly as long
//! as the FACT process and its panes.

use std::time::{SystemTime, UNIX_EPOCH};

use fact_layout::SessionLayout;
use fact_protocol::SessionSummary;

#[derive(Debug, Clone)]
pub struct PaneRecord {
    pub id: u64,
    pub pid: u32,
    pub exit_code: Option<i32>,
    pub profile: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionData {
    pub id: String,
    pub name: String,
    pub layout: SessionLayout,
    pub panes: Vec<PaneRecord>,
}

impl SessionData {
    pub fn new(name: impl Into<String>, layout: SessionLayout, panes: Vec<PaneRecord>) -> Self {
        let name = name.into();
        Self {
            id: format!("{}-{}", std::process::id(), now_ms()),
            name,
            layout,
            panes,
        }
    }

    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            pane_count: self.panes.len(),
            tab_count: self.layout.tabs.len(),
        }
    }

    pub fn pane(&self, id: u64) -> Option<&PaneRecord> {
        self.panes.iter().find(|pane| pane.id == id)
    }

    pub fn pane_mut(&mut self, id: u64) -> Option<&mut PaneRecord> {
        self.panes.iter_mut().find(|pane| pane.id == id)
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}
