//! Split-tree layout engine.
//!
//! A tab owns a binary tree of panes. Slots tile the content area with a
//! one-cell gutter around every split. Scrollbars overlay the right edge on
//! hover; every cell belongs to the terminal grid. Very small
//! viewports switch to focused-pane mode instead of shrinking every pane below a
//! readable size.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

pub const MAX_TREE_DEPTH: usize = 64;
pub const MAX_PANES: usize = 256;
pub const MIN_RATIO: f32 = 0.1;
pub const MAX_RATIO: f32 = 0.9;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitOrientation {
    /// Children are side by side. The gutter is vertical.
    Horizontal,
    /// Children are stacked. The gutter is horizontal.
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Right,
    Down,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LayoutNode {
    Leaf {
        pane: u64,
    },
    Split {
        orientation: SplitOrientation,
        ratio: f32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TabLayout {
    pub id: u32,
    pub title: String,
    pub custom_title: bool,
    pub root: Option<LayoutNode>,
    pub focus: Option<u64>,
    pub zoomed: bool,
    /// Keep the whole split tree visible even when slots fall below the
    /// configured minimum sizes. Explicitly restoring a zoomed pane turns this
    /// on so shrinking the terminal can no longer trap the user in focused-pane
    /// mode.
    pub fit_all: bool,
    pub minimized: Vec<u64>,
}

impl Default for TabLayout {
    fn default() -> Self {
        Self {
            id: 0,
            title: "Shell".to_string(),
            custom_title: false,
            root: None,
            focus: None,
            zoomed: false,
            fit_all: false,
            minimized: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SessionLayout {
    pub tabs: Vec<TabLayout>,
    pub active_tab: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn contains(self, x: u16, y: u16) -> bool {
        x >= self.x
            && y >= self.y
            && x < self.x.saturating_add(self.width)
            && y < self.y.saturating_add(self.height)
    }
}

#[derive(Debug, Clone)]
pub struct PaneSlot {
    pub pane: u64,
    pub slot: Rect,
    pub content: Rect,
    pub focused: bool,
}

#[derive(Debug, Clone)]
pub struct SplitBoundary {
    pub path: Vec<u8>,
    pub orientation: SplitOrientation,
    pub region: Rect,
    pub ratio: f32,
}

#[derive(Debug, Clone)]
pub struct LayoutResult {
    pub panes: Vec<PaneSlot>,
    pub splits: Vec<SplitBoundary>,
    pub focused: Option<u64>,
    /// True when the viewport was too small for the full tiling.
    pub tiny_mode: bool,
    /// True when the active tab is zoomed.
    pub zoomed: bool,
    pub hidden_panes: Vec<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct LayoutOptions {
    pub cols: u16,
    pub rows: u16,
    pub min_content_cols: u16,
    pub min_content_rows: u16,
}

impl Default for LayoutOptions {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            min_content_cols: 24,
            min_content_rows: 5,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LayoutError {
    #[error("tab {0} does not exist")]
    MissingTab(u32),
    #[error("pane {0} was not found")]
    MissingPane(u64),
    #[error("pane {0} is not in tab {1}")]
    PaneNotInTab(u64, u32),
    #[error("cannot split a pane outside the active layout tree")]
    InvalidTree,
    #[error("layout is invalid: {0}")]
    Invalid(String),
}

impl SessionLayout {
    pub fn new(tab_id: u32, panes: &[u64]) -> Self {
        let mut tab = TabLayout {
            id: tab_id,
            ..Default::default()
        };
        tab.root = build_balanced(panes);
        tab.focus = panes.first().copied();
        Self {
            tabs: vec![tab],
            active_tab: tab_id,
        }
    }

    pub fn active(&self) -> Option<&TabLayout> {
        self.tabs.iter().find(|t| t.id == self.active_tab)
    }

    pub fn active_mut(&mut self) -> Option<&mut TabLayout> {
        self.tabs.iter_mut().find(|t| t.id == self.active_tab)
    }

    pub fn tab(&self, id: u32) -> Option<&TabLayout> {
        self.tabs.iter().find(|t| t.id == id)
    }

    pub fn tab_mut(&mut self, id: u32) -> Option<&mut TabLayout> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    pub fn tab_for_pane(&self, pane: u64) -> Option<u32> {
        self.tabs.iter().find_map(|tab| {
            let inside = tab
                .root
                .as_ref()
                .is_some_and(|root| leaves(root).any(|p| p == pane));
            if inside {
                Some(tab.id)
            } else {
                None
            }
        })
    }

    pub fn all_visible_panes(&self) -> Vec<u64> {
        let mut out = Vec::new();
        for tab in &self.tabs {
            if let Some(root) = &tab.root {
                out.extend(leaves(root));
            }
        }
        out
    }

    pub fn all_panes_including_minimized(&self) -> Vec<u64> {
        let mut out = self.all_visible_panes();
        for tab in &self.tabs {
            out.extend(tab.minimized.iter().copied());
        }
        out
    }

    pub fn add_tab(&mut self, tab_id: u32, panes: &[u64], title: impl Into<String>) -> u32 {
        let mut tab = TabLayout {
            id: tab_id,
            title: title.into(),
            ..Default::default()
        };
        tab.root = build_balanced(panes);
        tab.focus = panes.first().copied();
        self.tabs.push(tab);
        self.active_tab = tab_id;
        tab_id
    }

    pub fn remove_tab(&mut self, tab_id: u32) -> Result<Vec<u64>, LayoutError> {
        let Some(index) = self.tabs.iter().position(|t| t.id == tab_id) else {
            return Err(LayoutError::MissingTab(tab_id));
        };
        let removed = self.tabs.remove(index);
        let mut panes: Vec<u64> = removed
            .root
            .as_ref()
            .map(|r| leaves(r).collect())
            .unwrap_or_default();
        panes.extend(removed.minimized);
        if self.active_tab == tab_id {
            let next = self
                .tabs
                .get(index.min(self.tabs.len().saturating_sub(1)))
                .map(|t| t.id);
            self.active_tab = next.unwrap_or(0);
        }
        Ok(panes)
    }

    pub fn select_tab(&mut self, tab_id: u32) -> Result<(), LayoutError> {
        if self.tab(tab_id).is_none() {
            return Err(LayoutError::MissingTab(tab_id));
        }
        self.active_tab = tab_id;
        Ok(())
    }

    pub fn next_tab(&mut self, delta: isize) -> u32 {
        if self.tabs.is_empty() {
            return 0;
        }
        let index = self
            .tabs
            .iter()
            .position(|t| t.id == self.active_tab)
            .unwrap_or(0);
        let len = self.tabs.len() as isize;
        let next = (index as isize + delta).rem_euclid(len) as usize;
        self.active_tab = self.tabs[next].id;
        self.active_tab
    }

    /// Split the leaf containing `target` and place `new_pane` after (or below) it.
    pub fn split_pane(
        &mut self,
        target: u64,
        new_pane: u64,
        direction: SplitDirection,
    ) -> Result<u32, LayoutError> {
        let tab_id = self
            .tab_for_pane(target)
            .ok_or(LayoutError::MissingPane(target))?;
        let tab = self
            .tab_mut(tab_id)
            .ok_or(LayoutError::MissingTab(tab_id))?;
        let root = tab.root.take().ok_or(LayoutError::InvalidTree)?;
        tab.root = Some(split_leaf(root, target, new_pane, direction)?);
        tab.focus = Some(new_pane);
        tab.zoomed = false;
        Ok(tab_id)
    }

    /// Remove a pane from a tab's tiling and return the next focus candidate.
    /// Move a pane (visible or minimized) to another tab, splitting that tab's
    /// focused pane on the right. Empty source tabs are removed.
    pub fn move_pane_to_tab(&mut self, pane: u64, target_tab: u32) -> Result<(), LayoutError> {
        if self.tab(target_tab).is_none() {
            return Err(LayoutError::MissingTab(target_tab));
        }
        let source_tab = self
            .tab_for_pane(pane)
            .or_else(|| {
                self.tabs
                    .iter()
                    .find(|tab| tab.minimized.contains(&pane))
                    .map(|tab| tab.id)
            })
            .ok_or(LayoutError::MissingPane(pane))?;
        if source_tab == target_tab {
            return Ok(());
        }

        {
            let source = self
                .tab_mut(source_tab)
                .ok_or(LayoutError::MissingTab(source_tab))?;
            if source.minimized.contains(&pane) {
                source.minimized.retain(|p| *p != pane);
            } else {
                let root = source.root.take().ok_or(LayoutError::InvalidTree)?;
                match remove_leaf(root, pane)? {
                    RemoveResult::Replaced(node, next) => {
                        source.root = Some(node);
                        if source.focus == Some(pane) {
                            source.focus =
                                next.or_else(|| source.root.as_ref().and_then(first_leaf));
                        }
                    }
                    RemoveResult::Empty(next) => {
                        source.root = None;
                        source.focus = next;
                    }
                }
            }
            if source.root.is_none() {
                source.zoomed = false;
            }
        }
        let source_empty = self
            .tab(source_tab)
            .is_some_and(|tab| tab.root.is_none() && tab.minimized.is_empty());
        if source_empty {
            self.remove_tab(source_tab)?;
        }

        let target = self
            .tab_mut(target_tab)
            .ok_or(LayoutError::MissingTab(target_tab))?;
        match target.root.take() {
            Some(root) => {
                let focus = target.focus.or_else(|| first_leaf(&root)).unwrap_or(pane);
                target.root = Some(split_leaf(root, focus, pane, SplitDirection::Right)?);
            }
            None => target.root = Some(LayoutNode::Leaf { pane }),
        }
        target.focus = Some(pane);
        target.zoomed = false;
        self.active_tab = target_tab;
        Ok(())
    }

    /// Swap the positions of two visible panes in the same tab.
    pub fn swap_panes(&mut self, a: u64, b: u64) -> Result<(), LayoutError> {
        if a == b {
            return Ok(());
        }
        let tab_a = self.tab_for_pane(a).ok_or(LayoutError::MissingPane(a))?;
        let tab_b = self.tab_for_pane(b).ok_or(LayoutError::MissingPane(b))?;
        if tab_a != tab_b {
            return Err(LayoutError::Invalid(
                "panes may only be swapped inside one tab".into(),
            ));
        }
        let tab = self.tab_mut(tab_a).ok_or(LayoutError::MissingTab(tab_a))?;
        let root = tab.root.as_mut().ok_or(LayoutError::InvalidTree)?;
        swap_leaf_ids(root, a, b);
        if tab.focus == Some(a) {
            tab.focus = Some(b);
        } else if tab.focus == Some(b) {
            tab.focus = Some(a);
        }
        Ok(())
    }

    pub fn remove_pane(&mut self, pane: u64) -> Result<Option<u64>, LayoutError> {
        let tab_id = self
            .tab_for_pane(pane)
            .ok_or(LayoutError::MissingPane(pane))?;
        let tab = self
            .tab_mut(tab_id)
            .ok_or(LayoutError::MissingTab(tab_id))?;
        let root = tab.root.take();
        let result = match root {
            Some(root) => remove_leaf(root, pane)?,
            None => return Err(LayoutError::MissingPane(pane)),
        };
        match result {
            RemoveResult::Replaced(node, next) => {
                tab.root = Some(node);
                if tab.focus == Some(pane) {
                    tab.focus = next.or_else(|| tab.root.as_ref().and_then(first_leaf));
                }
            }
            RemoveResult::Empty(next) => {
                tab.root = None;
                tab.focus = next;
            }
        }
        if tab.root.is_none() {
            tab.zoomed = false;
        }
        Ok(tab.focus)
    }

    pub fn hide_pane(&mut self, pane: u64) -> Result<(), LayoutError> {
        let tab_id = self
            .tab_for_pane(pane)
            .ok_or(LayoutError::MissingPane(pane))?;
        let tab = self
            .tab_mut(tab_id)
            .ok_or(LayoutError::MissingTab(tab_id))?;
        let root = tab.root.take();
        let result = match root {
            Some(root) => remove_leaf(root, pane)?,
            None => return Err(LayoutError::MissingPane(pane)),
        };
        match result {
            RemoveResult::Replaced(node, next) => {
                tab.root = Some(node);
                if tab.focus == Some(pane) {
                    tab.focus = next.or_else(|| tab.root.as_ref().and_then(first_leaf));
                }
            }
            RemoveResult::Empty(next) => {
                tab.root = None;
                tab.focus = next;
            }
        }
        if !tab.minimized.contains(&pane) {
            tab.minimized.push(pane);
        }
        if tab.root.is_none() {
            tab.zoomed = false;
        }
        Ok(())
    }

    /// Restore a minimized pane, splitting the focused pane if one exists.
    pub fn restore_pane(&mut self, pane: u64) -> Result<u32, LayoutError> {
        let tab_id = self
            .tabs
            .iter()
            .find(|t| t.minimized.contains(&pane))
            .map(|t| t.id)
            .ok_or(LayoutError::MissingPane(pane))?;
        let tab = self
            .tab_mut(tab_id)
            .ok_or(LayoutError::MissingTab(tab_id))?;
        tab.minimized.retain(|p| *p != pane);
        match tab.root.take() {
            Some(root) => {
                let target = tab.focus.or_else(|| first_leaf(&root)).unwrap_or(pane);
                tab.root = Some(split_leaf(root, target, pane, SplitDirection::Right)?);
                tab.focus = Some(pane);
            }
            None => {
                tab.root = Some(LayoutNode::Leaf { pane });
                tab.focus = Some(pane);
            }
        }
        Ok(tab_id)
    }

    pub fn restore_all_minimized(&mut self) -> Vec<u64> {
        let panes: Vec<u64> = self
            .tabs
            .iter()
            .flat_map(|t| t.minimized.iter().copied())
            .collect();
        for pane in &panes {
            let _ = self.restore_pane(*pane);
        }
        panes
    }

    /// Rewrite every pane identity, e.g. when a saved layout is reopened in a
    /// daemon that already owns other panes with the same old IDs.
    pub fn remap_panes(&mut self, map: &BTreeMap<u64, u64>) -> Result<(), LayoutError> {
        for tab in &mut self.tabs {
            if let Some(root) = &mut tab.root {
                remap_node(root, map)?;
            }
            if let Some(focus) = tab.focus {
                tab.focus = Some(
                    *map.get(&focus)
                        .ok_or(LayoutError::Invalid("focus pane is not remapped".into()))?,
                );
            }
            let mut minimized = Vec::with_capacity(tab.minimized.len());
            for pane in &tab.minimized {
                minimized.push(*map.get(pane).ok_or(LayoutError::Invalid(
                    "minimized pane is not remapped".into(),
                ))?);
            }
            tab.minimized = minimized;
        }
        Ok(())
    }

    pub fn zoom(&mut self, zoomed: bool) -> Result<(), LayoutError> {
        let active = self.active_tab;
        let tab = self.active_mut().ok_or(LayoutError::MissingTab(active))?;
        if zoomed && tab.focus.is_none() {
            return Err(LayoutError::Invalid("no focused pane to zoom".into()));
        }
        tab.zoomed = zoomed;
        // Restoring from zoom is also the explicit escape hatch from
        // focused-pane mode. Keep every pane visible even if the window is now
        // too small for the configured minimum slot size.
        tab.fit_all = !zoomed;
        Ok(())
    }

    pub fn focus_pane(&mut self, pane: u64) -> Result<(), LayoutError> {
        let tab_id = self
            .tab_for_pane(pane)
            .ok_or(LayoutError::MissingPane(pane))?;
        self.tab_mut(tab_id)
            .ok_or(LayoutError::MissingTab(tab_id))?
            .focus = Some(pane);
        self.active_tab = tab_id;
        Ok(())
    }

    pub fn cycle_focus(&mut self, delta: isize) -> Option<u64> {
        let tab = self.active_mut()?;
        let root = tab.root.as_ref()?;
        let panes: Vec<u64> = leaves(root).collect();
        if panes.is_empty() {
            return None;
        }
        let current = tab
            .focus
            .and_then(|p| panes.iter().position(|x| *x == p))
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(panes.len() as isize) as usize;
        tab.focus = Some(panes[next]);
        tab.focus
    }

    pub fn set_split_ratio(&mut self, path: &[u8], ratio: f32) -> Result<(), LayoutError> {
        let active = self.active_tab;
        let tab = self.active_mut().ok_or(LayoutError::MissingTab(active))?;
        let root = tab.root.as_mut().ok_or(LayoutError::InvalidTree)?;
        set_ratio_at(root, path, ratio)
    }

    pub fn layout(&self, options: LayoutOptions) -> LayoutResult {
        let Some(tab) = self.active() else {
            return LayoutResult {
                panes: Vec::new(),
                splits: Vec::new(),
                focused: None,
                tiny_mode: false,
                zoomed: false,
                hidden_panes: Vec::new(),
            };
        };
        let visible: Vec<u64> = tab
            .root
            .as_ref()
            .map(|r| leaves(r).collect())
            .unwrap_or_default();
        let focused = tab
            .focus
            .filter(|f| visible.contains(f))
            .or_else(|| visible.first().copied());
        let full_rect = Rect::new(0, 0, options.cols, options.rows);
        let mut slots = Vec::new();
        let mut splits = Vec::new();
        let too_small_viewport = options.cols == 0 || options.rows < 3;
        if let Some(root) = &tab.root {
            if !too_small_viewport && !tab.zoomed {
                layout_node(root, full_rect, &mut Vec::new(), &mut slots, &mut splits);
            }
        }
        let too_small = slots.iter().any(|s| {
            s.content.width < options.min_content_cols
                || s.content.height < options.min_content_rows
                || s.content.height == 0
        });
        let use_focused_only = too_small_viewport || tab.zoomed || (too_small && !tab.fit_all);
        if use_focused_only {
            let hidden_panes: Vec<u64> = visible
                .iter()
                .copied()
                .filter(|p| Some(*p) != focused)
                .collect();
            let panes = if let Some(focus) = focused {
                let content = full_rect;
                vec![PaneSlot {
                    pane: focus,
                    slot: full_rect,
                    content,
                    focused: true,
                }]
            } else {
                Vec::new()
            };
            return LayoutResult {
                panes,
                splits: Vec::new(),
                focused,
                tiny_mode: too_small || too_small_viewport,
                zoomed: tab.zoomed,
                hidden_panes,
            };
        }
        for slot in &mut slots {
            slot.focused = Some(slot.pane) == focused;
        }
        LayoutResult {
            panes: slots,
            splits,
            focused,
            tiny_mode: false,
            zoomed: false,
            hidden_panes: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), LayoutError> {
        if self.tabs.is_empty() {
            return Err(LayoutError::Invalid("session has no tabs".into()));
        }
        let mut tab_ids = HashSet::new();
        let mut pane_ids = HashSet::new();
        let mut total = 0usize;
        for tab in &self.tabs {
            if !tab_ids.insert(tab.id) {
                return Err(LayoutError::Invalid(format!("duplicate tab id {}", tab.id)));
            }
            if tab.title.chars().count() > 128 {
                return Err(LayoutError::Invalid("tab title is too long".into()));
            }
            if let Some(root) = &tab.root {
                let mut depth = 0usize;
                collect_and_validate(root, &mut pane_ids, &mut total, 0, &mut depth)?;
                if let Some(focus) = tab.focus {
                    if !leaf_exists(root, focus) {
                        return Err(LayoutError::Invalid(format!(
                            "focus {focus} is not visible in tab {}",
                            tab.id
                        )));
                    }
                }
            }
            let mut minimized = HashSet::new();
            for pane in &tab.minimized {
                if !pane_ids.insert(*pane) {
                    return Err(LayoutError::Invalid(format!("duplicate pane id {pane}")));
                }
                if !minimized.insert(*pane) {
                    return Err(LayoutError::Invalid(format!(
                        "pane {pane} is minimized twice"
                    )));
                }
                total += 1;
            }
        }
        if total > MAX_PANES {
            return Err(LayoutError::Invalid(format!(
                "too many panes ({total} > {MAX_PANES})"
            )));
        }
        if !tab_ids.contains(&self.active_tab) {
            return Err(LayoutError::Invalid("active tab does not exist".into()));
        }
        Ok(())
    }

    /// Repair benign inconsistencies (unknown focus, ratios out of range) while
    /// keeping the persisted pane tree.
    pub fn sanitize(&mut self) {
        self.tabs
            .retain(|t| t.root.is_some() || !t.minimized.is_empty());
        if self.tabs.is_empty() {
            self.active_tab = 0;
            return;
        }
        if !self.tabs.iter().any(|t| t.id == self.active_tab) {
            self.active_tab = self.tabs[0].id;
        }
        for tab in &mut self.tabs {
            if let Some(root) = &mut tab.root {
                sanitize_node(root);
                if tab.focus.is_none() || !tab.focus.is_some_and(|f| leaf_exists(root, f)) {
                    tab.focus = first_leaf(root);
                }
            } else {
                tab.focus = None;
                tab.zoomed = false;
            }
            tab.minimized
                .retain(|p| !tab.root.as_ref().is_some_and(|root| leaf_exists(root, *p)));
        }
    }
}

fn sanitize_node(node: &mut LayoutNode) {
    if let LayoutNode::Split {
        ratio,
        first,
        second,
        ..
    } = node
    {
        if !ratio.is_finite() {
            *ratio = 0.5;
        } else {
            *ratio = ratio.clamp(MIN_RATIO, MAX_RATIO);
        }
        sanitize_node(first);
        sanitize_node(second);
    }
}

fn collect_and_validate(
    node: &LayoutNode,
    panes: &mut HashSet<u64>,
    total: &mut usize,
    depth: usize,
    max_depth: &mut usize,
) -> Result<(), LayoutError> {
    if depth > MAX_TREE_DEPTH {
        return Err(LayoutError::Invalid("layout tree is too deep".into()));
    }
    *max_depth = (*max_depth).max(depth);
    match node {
        LayoutNode::Leaf { pane } => {
            if !panes.insert(*pane) {
                return Err(LayoutError::Invalid(format!("duplicate pane id {pane}")));
            }
            *total += 1;
        }
        LayoutNode::Split {
            ratio,
            first,
            second,
            ..
        } => {
            if !ratio.is_finite() || *ratio < MIN_RATIO || *ratio > MAX_RATIO {
                return Err(LayoutError::Invalid("split ratio is out of range".into()));
            }
            collect_and_validate(first, panes, total, depth + 1, max_depth)?;
            collect_and_validate(second, panes, total, depth + 1, max_depth)?;
        }
    }
    Ok(())
}

fn set_ratio_at(node: &mut LayoutNode, path: &[u8], ratio: f32) -> Result<(), LayoutError> {
    if !ratio.is_finite() {
        return Err(LayoutError::Invalid("ratio is not finite".into()));
    }
    let Some((&step, rest)) = path.split_first() else {
        return match node {
            LayoutNode::Split { ratio: current, .. } => {
                *current = ratio.clamp(MIN_RATIO, MAX_RATIO);
                Ok(())
            }
            LayoutNode::Leaf { .. } => Err(LayoutError::Invalid("path points to a leaf".into())),
        };
    };
    match node {
        LayoutNode::Split { first, second, .. } => match step {
            0 => set_ratio_at(first, rest, ratio),
            1 => set_ratio_at(second, rest, ratio),
            _ => Err(LayoutError::Invalid("invalid split path".into())),
        },
        LayoutNode::Leaf { .. } => Err(LayoutError::Invalid("split path is too deep".into())),
    }
}

fn layout_node(
    node: &LayoutNode,
    rect: Rect,
    path: &mut Vec<u8>,
    slots: &mut Vec<PaneSlot>,
    splits: &mut Vec<SplitBoundary>,
) {
    match node {
        LayoutNode::Leaf { pane } => {
            slots.push(PaneSlot {
                pane: *pane,
                slot: rect,
                content: rect,
                focused: false,
            });
        }
        LayoutNode::Split {
            orientation,
            ratio,
            first,
            second,
        } => {
            if rect.width < 3 || rect.height < 3 {
                // Let the caller detect the invalid slot as tiny by emitting a
                // zero-sized leaf; this should only be reachable with pathological
                // persisted ratios.
                slots.push(PaneSlot {
                    pane: match **first {
                        LayoutNode::Leaf { pane } => pane,
                        _ => 0,
                    },
                    slot: rect,
                    content: Rect::new(rect.x, rect.y, 0, 0),
                    focused: false,
                });
                return;
            }
            splits.push(SplitBoundary {
                path: path.clone(),
                orientation: *orientation,
                region: rect,
                ratio: *ratio,
            });
            match orientation {
                SplitOrientation::Horizontal => {
                    let usable = rect.width.saturating_sub(1).max(2);
                    let left = ((usable as f32) * ratio).round() as u16;
                    let left = left.clamp(1, usable.saturating_sub(1));
                    let right_w = usable.saturating_sub(left);
                    let left_rect = Rect::new(rect.x, rect.y, left, rect.height);
                    let right_rect = Rect::new(rect.x + left + 1, rect.y, right_w, rect.height);
                    path.push(0);
                    layout_node(first, left_rect, path, slots, splits);
                    path.pop();
                    path.push(1);
                    layout_node(second, right_rect, path, slots, splits);
                    path.pop();
                }
                SplitOrientation::Vertical => {
                    let usable = rect.height.saturating_sub(1).max(2);
                    let top = ((usable as f32) * ratio).round() as u16;
                    let top = top.clamp(1, usable.saturating_sub(1));
                    let bottom_h = usable.saturating_sub(top);
                    let top_rect = Rect::new(rect.x, rect.y, rect.width, top);
                    let bottom_rect = Rect::new(rect.x, rect.y + top + 1, rect.width, bottom_h);
                    path.push(0);
                    layout_node(first, top_rect, path, slots, splits);
                    path.pop();
                    path.push(1);
                    layout_node(second, bottom_rect, path, slots, splits);
                    path.pop();
                }
            }
        }
    }
}

fn build_balanced(panes: &[u64]) -> Option<LayoutNode> {
    match panes {
        [] => None,
        [only] => Some(LayoutNode::Leaf { pane: *only }),
        _ => {
            let mid = panes.len() / 2;
            let orientation = if panes.len() >= 4 {
                SplitOrientation::Horizontal
            } else {
                SplitOrientation::Vertical
            };
            let first = build_balanced(&panes[..mid]);
            let second = build_balanced(&panes[mid..]);
            match (first, second) {
                (Some(first), Some(second)) => Some(LayoutNode::Split {
                    orientation,
                    ratio: 0.5,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(node), None) | (None, Some(node)) => Some(node),
                (None, None) => None,
            }
        }
    }
}

fn split_leaf(
    node: LayoutNode,
    target: u64,
    new_pane: u64,
    direction: SplitDirection,
) -> Result<LayoutNode, LayoutError> {
    match node {
        LayoutNode::Leaf { pane } if pane == target => Ok(LayoutNode::Split {
            orientation: match direction {
                SplitDirection::Right => SplitOrientation::Horizontal,
                SplitDirection::Down => SplitOrientation::Vertical,
            },
            ratio: 0.5,
            first: Box::new(LayoutNode::Leaf { pane }),
            second: Box::new(LayoutNode::Leaf { pane: new_pane }),
        }),
        LayoutNode::Leaf { .. } => Err(LayoutError::MissingPane(target)),
        LayoutNode::Split {
            orientation,
            ratio,
            first,
            second,
        } => {
            if leaf_exists(&first, target) {
                let new_first = split_leaf(*first, target, new_pane, direction)?;
                Ok(LayoutNode::Split {
                    orientation,
                    ratio,
                    first: Box::new(new_first),
                    second,
                })
            } else if leaf_exists(&second, target) {
                let new_second = split_leaf(*second, target, new_pane, direction)?;
                Ok(LayoutNode::Split {
                    orientation,
                    ratio,
                    first,
                    second: Box::new(new_second),
                })
            } else {
                Err(LayoutError::MissingPane(target))
            }
        }
    }
}

enum RemoveResult {
    Replaced(LayoutNode, Option<u64>),
    Empty(Option<u64>),
}

fn remove_leaf(node: LayoutNode, target: u64) -> Result<RemoveResult, LayoutError> {
    match node {
        LayoutNode::Leaf { pane } if pane == target => Ok(RemoveResult::Empty(None)),
        LayoutNode::Leaf { .. } => Err(LayoutError::MissingPane(target)),
        LayoutNode::Split {
            orientation,
            ratio,
            first,
            second,
        } => {
            if let LayoutNode::Leaf { pane } = *first {
                if pane == target {
                    let next = first_leaf(&second);
                    return Ok(RemoveResult::Replaced(*second, next));
                }
            }
            if let LayoutNode::Leaf { pane } = *second {
                if pane == target {
                    let next = first_leaf(&first);
                    return Ok(RemoveResult::Replaced(*first, next));
                }
            }
            let mut first = first;
            let mut second = second;
            if leaf_exists(&first, target) {
                match remove_leaf(*first, target)? {
                    RemoveResult::Replaced(node, next) => {
                        first = Box::new(node);
                        Ok(RemoveResult::Replaced(
                            LayoutNode::Split {
                                orientation,
                                ratio,
                                first,
                                second,
                            },
                            next,
                        ))
                    }
                    RemoveResult::Empty(next) => Ok(RemoveResult::Replaced(*second, next)),
                }
            } else if leaf_exists(&second, target) {
                match remove_leaf(*second, target)? {
                    RemoveResult::Replaced(node, next) => {
                        second = Box::new(node);
                        Ok(RemoveResult::Replaced(
                            LayoutNode::Split {
                                orientation,
                                ratio,
                                first,
                                second,
                            },
                            next,
                        ))
                    }
                    RemoveResult::Empty(next) => Ok(RemoveResult::Replaced(*first, next)),
                }
            } else {
                Err(LayoutError::MissingPane(target))
            }
        }
    }
}

fn remap_node(node: &mut LayoutNode, map: &BTreeMap<u64, u64>) -> Result<(), LayoutError> {
    match node {
        LayoutNode::Leaf { pane } => {
            *pane = *map
                .get(pane)
                .ok_or(LayoutError::Invalid("pane is not remapped".into()))?;
        }
        LayoutNode::Split { first, second, .. } => {
            remap_node(first, map)?;
            remap_node(second, map)?;
        }
    }
    Ok(())
}

fn swap_leaf_ids(node: &mut LayoutNode, a: u64, b: u64) {
    match node {
        LayoutNode::Leaf { pane } => {
            if *pane == a {
                *pane = b;
            } else if *pane == b {
                *pane = a;
            }
        }
        LayoutNode::Split { first, second, .. } => {
            swap_leaf_ids(first, a, b);
            swap_leaf_ids(second, a, b);
        }
    }
}

fn leaves(node: &LayoutNode) -> Leaves<'_> {
    Leaves { stack: vec![node] }
}

struct Leaves<'a> {
    stack: Vec<&'a LayoutNode>,
}

impl<'a> Iterator for Leaves<'a> {
    type Item = u64;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(node) = self.stack.pop() {
            match node {
                LayoutNode::Leaf { pane } => return Some(*pane),
                LayoutNode::Split { first, second, .. } => {
                    self.stack.push(second);
                    self.stack.push(first);
                }
            }
        }
        None
    }
}

fn first_leaf(node: &LayoutNode) -> Option<u64> {
    leaves(node).next()
}

fn leaf_exists(node: &LayoutNode, target: u64) -> bool {
    leaves(node).any(|p| p == target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(cols: u16, rows: u16) -> LayoutOptions {
        LayoutOptions {
            cols,
            rows,
            min_content_cols: 20,
            min_content_rows: 4,
        }
    }

    #[test]
    fn grid_eight_creates_eight_slots() {
        let layout = SessionLayout::new(1, &[1, 2, 3, 4, 5, 6, 7, 8]);
        let result = layout.layout(opts(200, 50));
        assert_eq!(result.panes.len(), 8);
        assert!(!result.tiny_mode);
        let mut seen = HashSet::new();
        let mut area = 0u32;
        for slot in &result.panes {
            assert!(seen.insert(slot.pane));
            assert!(slot.content.width >= 20);
            assert!(slot.content.height >= 4);
            assert!(slot.slot.x + slot.slot.width <= 200);
            assert!(slot.slot.y + slot.slot.height <= 50);
            area += slot.slot.width as u32 * slot.slot.height as u32;
        }
        assert_eq!(seen.len(), 8);
        // Gutters are not painted, so total slot area is strictly smaller.
        assert!(area < 200 * 50);
        assert!(!result.splits.is_empty());
    }

    #[test]
    fn grid_eight_fits_a_standard_terminal_without_titles() {
        let layout = SessionLayout::new(1, &[1, 2, 3, 4, 5, 6, 7, 8]);
        for (cols, rows) in [(120, 35), (100, 30), (80, 24)] {
            let result = layout.layout(LayoutOptions {
                cols,
                rows,
                min_content_cols: 16,
                min_content_rows: 5,
            });
            assert_eq!(result.panes.len(), 8, "{cols}x{rows} must stay a grid");
            assert!(!result.tiny_mode, "{cols}x{rows} must not collapse");
            for slot in &result.panes {
                assert_eq!(slot.content.height, slot.slot.height);
                assert!(slot.content.width >= 16);
            }
        }
    }

    #[test]
    fn terminal_content_uses_the_full_pane_width() {
        let layout = SessionLayout::new(1, &[1]);
        let result = layout.layout(opts(80, 24));
        assert_eq!(result.panes.len(), 1);
        let slot = &result.panes[0];
        assert_eq!(slot.slot.width, 80);
        assert_eq!(slot.slot.height, 24);
        assert_eq!(slot.content.width, 80);
        assert_eq!(slot.content.height, 24);
        assert_eq!(slot.content.x + slot.content.width, 80);
        assert_eq!(slot.slot.x + slot.slot.width, 80);
    }

    #[test]
    fn tiny_viewport_switches_to_focused_mode() {
        let layout = SessionLayout::new(1, &[1, 2, 3, 4, 5, 6, 7, 8]);
        let result = layout.layout(opts(60, 18));
        assert!(result.tiny_mode);
        assert_eq!(result.panes.len(), 1);
        assert_eq!(result.focused, Some(1));
        assert_eq!(result.hidden_panes.len(), 7);
        assert!(result.panes[0].content.width > 20);
    }

    #[test]
    fn zoom_shows_only_focused_pane() {
        let mut layout = SessionLayout::new(1, &[1, 2, 3, 4]);
        layout.focus_pane(3).unwrap();
        layout.zoom(true).unwrap();
        let result = layout.layout(opts(180, 50));
        assert!(result.zoomed);
        assert_eq!(result.panes.len(), 1);
        assert_eq!(result.panes[0].pane, 3);
    }

    #[test]
    fn restoring_zoom_keeps_every_pane_visible_in_a_small_window() {
        let mut layout = SessionLayout::new(1, &[1, 2, 3, 4, 5, 6, 7, 8]);
        layout.focus_pane(4).unwrap();
        layout.zoom(true).unwrap();
        assert_eq!(layout.layout(opts(60, 18)).panes.len(), 1);

        layout.zoom(false).unwrap();
        let result = layout.layout(opts(60, 18));
        assert!(!result.tiny_mode);
        assert!(!result.zoomed);
        assert_eq!(result.panes.len(), 8);
        for slot in &result.panes {
            assert!(slot.slot.width > 0 && slot.slot.height > 0);
            assert!(slot.content.width > 0 && slot.content.height > 0);
        }
        assert!(layout.tab(1).unwrap().fit_all);
    }

    #[test]
    fn fit_all_survives_serialization() {
        let mut layout = SessionLayout::new(1, &[1, 2, 3, 4, 5, 6, 7, 8]);
        layout.zoom(false).unwrap();
        let json = serde_json::to_string(&layout).unwrap();
        let restored: SessionLayout = serde_json::from_str(&json).unwrap();
        assert!(restored.tab(1).unwrap().fit_all);
        assert_eq!(restored.layout(opts(60, 18)).panes.len(), 8);
    }

    #[test]
    fn split_and_close_are_symmetric() {
        let mut layout = SessionLayout::new(1, &[1]);
        layout.split_pane(1, 2, SplitDirection::Right).unwrap();
        layout.split_pane(2, 3, SplitDirection::Down).unwrap();
        assert_eq!(layout.all_visible_panes(), vec![1, 2, 3]);
        layout.remove_pane(2).unwrap();
        assert_eq!(layout.all_visible_panes(), vec![1, 3]);
        layout.remove_pane(1).unwrap();
        assert_eq!(layout.all_visible_panes(), vec![3]);
    }

    #[test]
    fn minimize_keeps_identity_and_restores() {
        let mut layout = SessionLayout::new(1, &[1, 2]);
        layout.hide_pane(2).unwrap();
        assert_eq!(layout.all_visible_panes(), vec![1]);
        assert_eq!(layout.tab(1).unwrap().minimized, vec![2]);
        layout.restore_pane(2).unwrap();
        assert_eq!(layout.all_visible_panes().len(), 2);
        assert!(layout.tab(1).unwrap().minimized.is_empty());
    }

    #[test]
    fn focus_cycles() {
        let mut layout = SessionLayout::new(1, &[1, 2, 3]);
        assert_eq!(layout.cycle_focus(1), Some(2));
        assert_eq!(layout.cycle_focus(1), Some(3));
        assert_eq!(layout.cycle_focus(-1), Some(2));
    }

    #[test]
    fn ratio_updates_clamp() {
        let mut layout = SessionLayout::new(1, &[1, 2]);
        layout.set_split_ratio(&[], 0.99).unwrap();
        // Tall enough for a 90/10 split to keep both panes above the minimum.
        let result = layout.layout(opts(100, 80));
        let split = &result.splits[0];
        assert!(split.ratio <= MAX_RATIO);
        assert!(split.ratio >= MIN_RATIO);
    }

    #[test]
    fn persistence_validation_rejects_bad_ratios() {
        let mut layout = SessionLayout::new(1, &[1, 2]);
        if let Some(tab) = layout.active_mut() {
            if let Some(LayoutNode::Split { ratio, .. }) = &mut tab.root {
                *ratio = 99.0;
            }
        }
        assert!(layout.validate().is_err());
        layout.sanitize();
        layout.validate().unwrap();
    }

    #[test]
    fn move_and_swap_panes() {
        let mut layout = SessionLayout::new(1, &[1, 2]);
        layout.add_tab(2, &[3], "second");
        layout.move_pane_to_tab(1, 2).unwrap();
        assert_eq!(layout.tab_for_pane(1), Some(2));
        assert_eq!(layout.all_visible_panes(), vec![2, 3, 1]);
        layout.swap_panes(3, 1).unwrap();
        let visible = layout.all_visible_panes();
        assert_eq!(visible.len(), 3);
        assert!(visible.contains(&1) && visible.contains(&2) && visible.contains(&3));
        layout.validate().unwrap();
    }

    #[test]
    fn persistence_round_trip() {
        let layout = SessionLayout::new(7, &[10, 11, 12]);
        let json = serde_json::to_string(&layout).unwrap();
        let back: SessionLayout = serde_json::from_str(&json).unwrap();
        assert_eq!(layout, back);
        back.validate().unwrap();
    }

    #[test]
    fn remove_last_tab_yields_no_tabs() {
        let mut layout = SessionLayout::new(4, &[1]);
        let panes = layout.remove_tab(4).unwrap();
        assert_eq!(panes, vec![1]);
        assert!(layout.active().is_none());
    }

    #[test]
    fn moving_pane_between_tabs_preserves_process_id() {
        let mut layout = SessionLayout::new(1, &[1, 2]);
        layout.add_tab(2, &[3], "two");
        layout.remove_pane(2).unwrap();
        layout.split_pane(3, 2, SplitDirection::Down).unwrap();
        layout.validate().unwrap();
        assert_eq!(layout.tab_for_pane(2), Some(2));
    }
}
