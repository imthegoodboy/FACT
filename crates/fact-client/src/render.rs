//! Rendering for the FACT terminal UI.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use fact_protocol::{
    cell_flags, friendly_title, semantic_color, CellDto, ColorDto, CursorShape, PaneView,
};
use ratatui::layout::Position;

use crate::{
    action_label, filtered_palette, fit_text as fit, help_lines, minimized_tray_rect, new_tab_rect,
    palette_items, pane_button_strip, pane_buttons, scrollbar_geometry, tab_ranges, utc_clock, App,
    ColdState, ContextState, HelpState, Overlay, PaletteState, PromptKind, PromptState,
    SearchState, SessionsState,
};

impl App {
    pub(crate) fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        if area.width == 0 || area.height == 0 {
            return;
        }
        {
            let buffer = frame.buffer_mut();
            buffer.set_style(area, Style::default().bg(self.theme.bg).fg(self.theme.fg));
        }

        let tabs_height = u16::from(area.height >= 1);
        let status_height = u16::from(area.height >= 2);
        let tabs = Rect::new(area.x, area.y, area.width, tabs_height);
        let status = Rect::new(
            area.x,
            area.bottom().saturating_sub(status_height),
            area.width,
            status_height,
        );
        let main = Rect::new(
            area.x,
            area.y + tabs_height,
            area.width,
            area.height
                .saturating_sub(tabs_height)
                .saturating_sub(status_height),
        );

        self.render_tabs(frame, tabs);
        if self.frame.is_some() {
            self.render_panes(frame, main);
        } else {
            self.render_splash(frame, main);
        }
        self.render_status(frame, status);

        if self.overlay.is_some() {
            self.render_overlay(frame, area);
        }
    }

    fn render_tabs(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let buffer = frame.buffer_mut();
        buffer.set_style(area, Style::default().bg(self.theme.tab_bg));
        let Some(frame_data) = &self.frame else {
            let text = format!(" {} ", self.info.session.name);
            buffer.set_stringn(
                area.x.saturating_add(1),
                area.y,
                text,
                usize::from(area.width.saturating_sub(1)),
                Style::default().fg(self.theme.muted).bg(self.theme.tab_bg),
            );
            return;
        };

        for range in tab_ranges(&frame_data.tabs, area.width) {
            let Some(tab) = frame_data.tabs.iter().find(|tab| tab.id == range.id) else {
                continue;
            };
            let bell = frame_data
                .panes
                .iter()
                .any(|pane| pane.tab == tab.id && self.bells.contains(&pane.id));
            let style = if tab.active {
                Style::default()
                    .bg(self.theme.title_focus_bg)
                    .fg(self.theme.title_focus_fg)
                    .add_modifier(Modifier::BOLD)
            } else if bell {
                Style::default()
                    .bg(self.theme.tab_bg)
                    .fg(self.theme.warning)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(self.theme.tab_bg).fg(self.theme.tab_fg)
            };
            buffer.set_stringn(
                area.x.saturating_add(range.rect.x),
                area.y,
                range.label,
                usize::from(range.rect.width),
                style,
            );
        }

        if let Some(rect) = new_tab_rect(area.width) {
            buffer.set_stringn(
                area.x.saturating_add(rect.x),
                area.y,
                " + ",
                usize::from(rect.width),
                Style::default()
                    .bg(self.theme.tab_bg)
                    .fg(self.theme.muted)
                    .add_modifier(Modifier::BOLD),
            );
        }
    }

    fn render_splash(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let lines = vec![
            Line::from(Span::styled(
                "FACT",
                Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                format!("session {}", self.info.session.name),
                Style::default().fg(self.theme.fg),
            )),
            Line::from(Span::styled(
                "Connecting to the daemon event stream…",
                Style::default().fg(self.theme.muted),
            )),
        ];
        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Center)
            .style(Style::default().bg(self.theme.bg).fg(self.theme.fg));
        frame.render_widget(paragraph, area);
    }

    fn render_panes(&self, frame: &mut Frame, main: Rect) {
        let Some(frame_data) = &self.frame else {
            return;
        };
        let zoomed = self.active_tab().is_some_and(|tab| tab.zoomed);
        for pane in &frame_data.panes {
            self.render_pane(frame, main, pane, zoomed);
        }
        self.render_splits(frame, main, frame_data);
    }

    fn render_pane(&self, frame: &mut Frame, main: Rect, pane: &PaneView, zoomed: bool) {
        let slot = Rect::new(
            main.x.saturating_add(pane.slot.x),
            main.y.saturating_add(pane.slot.y),
            pane.slot.width,
            pane.slot.height,
        )
        .intersection(main);
        if slot.width == 0 || slot.height == 0 {
            return;
        }

        let title_bg = if pane.focused {
            self.theme.title_focus_bg
        } else {
            self.theme.title_bg
        };
        let bell = self.bells.contains(&pane.id);
        let title_fg = if bell {
            self.theme.warning
        } else if pane.focused {
            self.theme.title_focus_fg
        } else {
            self.theme.title_fg
        };
        let title_style =
            Style::default()
                .bg(title_bg)
                .fg(title_fg)
                .add_modifier(if pane.focused {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                });

        let buttons = pane_buttons(slot.width);
        let buttons_x = buttons.first().map(|button| button.x).unwrap_or(slot.width);
        {
            let buffer = frame.buffer_mut();
            let title_row = Rect::new(slot.x, slot.y, slot.width, 1);
            buffer.set_style(title_row, Style::default().bg(title_bg).fg(title_fg));
            let mut marker = if pane.focused { "▌" } else { " " }.to_string();
            marker.push(' ');
            let (next, _) = buffer.set_stringn(
                slot.x,
                slot.y,
                marker,
                2,
                Style::default().bg(title_bg).fg(if bell {
                    self.theme.warning
                } else {
                    self.theme.accent
                }),
            );
            let title_width = buttons_x
                .saturating_sub(next.saturating_sub(slot.x))
                .saturating_sub(1);
            let title = fit(&friendly_title(&pane.title), usize::from(title_width));
            buffer.set_stringn(next, slot.y, title, usize::from(title_width), title_style);

            if !buttons.is_empty() {
                let strip = pane_button_strip(zoomed);
                buffer.set_stringn(
                    slot.x.saturating_add(buttons_x),
                    slot.y,
                    strip,
                    usize::from(slot.width.saturating_sub(buttons_x)),
                    Style::default().bg(title_bg).fg(if pane.focused {
                        self.theme.title_fg
                    } else {
                        self.theme.muted
                    }),
                );
            }
        }

        if slot.height < 2 {
            return;
        }
        let content = Rect::new(
            slot.x,
            slot.y + 1,
            slot.width.saturating_sub(1).min(pane.content_cols),
            slot.height.saturating_sub(1).min(pane.content_rows),
        )
        .intersection(main);
        if content.width == 0 || content.height == 0 {
            return;
        }

        {
            let buffer = frame.buffer_mut();
            buffer.set_style(
                content,
                Style::default()
                    .bg(self.theme.terminal_bg)
                    .fg(self.theme.terminal_fg),
            );
            for y in content.y..content.bottom() {
                for x in content.x..content.right() {
                    buffer[(x, y)].set_symbol(" ");
                }
            }
            let scroll_column = Rect::new(
                slot.x.saturating_add(slot.width).saturating_sub(1),
                content.y,
                1,
                content.height,
            );
            buffer.set_style(scroll_column, Style::default().bg(self.theme.terminal_bg));
            for y in scroll_column.y..scroll_column.bottom() {
                buffer[(scroll_column.x, y)].set_symbol(" ");
            }
        }

        if let Some(buffer) = self.buffers.get(&pane.id) {
            let selected = self
                .selection
                .as_ref()
                .filter(|selection| selection.pane == pane.id);
            let buf = frame.buffer_mut();
            for y in 0..pane.content_rows.min(content.height) {
                for x in 0..pane.content_cols.min(content.width) {
                    let Some(cell) = buffer.cell(x, y) else {
                        continue;
                    };
                    let sx = content.x.saturating_add(x);
                    let sy = content.y.saturating_add(y);
                    let mut style = cell_style(cell, &self.theme);
                    if selected.is_some_and(|selection| selection.contains(x, y)) {
                        style = style.bg(self.theme.selection).fg(self.theme.fg);
                    }
                    if cell.flags & cell_flags::WIDE_SPACER != 0 {
                        buf[(sx, sy)].set_style(style);
                        continue;
                    }
                    let symbol = if cell.flags & cell_flags::HIDDEN != 0 {
                        " ".to_string()
                    } else {
                        cell.symbol()
                    };
                    buf[(sx, sy)].set_symbol(&symbol).set_style(style);
                }
            }
        } else {
            let buffer = frame.buffer_mut();
            let message = "waiting for terminal output…";
            let x = content
                .x
                .saturating_add(content.width.saturating_sub(message.chars().count() as u16) / 2);
            let y = content.y + content.height / 2;
            buffer.set_stringn(
                x,
                y,
                message,
                usize::from(content.width),
                Style::default()
                    .fg(self.theme.muted)
                    .bg(self.theme.terminal_bg),
            );
        }

        self.render_cursor(frame, pane, content);
        self.render_scrollbar(frame, main, pane);

        if !pane.alive {
            let code = pane
                .exit_code
                .map(|code| format!("exit code {code}"))
                .unwrap_or_else(|| "process exited".into());
            let text = format!(
                " {code} · {} restarts ",
                self.keys
                    .binding("restart_pane")
                    .map(|binding| binding.to_string())
                    .unwrap_or_else(|| "prefix+Shift+R".into())
            );
            let width = (text.chars().count() as u16).min(content.width);
            let x = content.x + content.width.saturating_sub(width) / 2;
            let y = content.y + content.height / 2;
            let buffer = frame.buffer_mut();
            buffer.set_stringn(
                x,
                y,
                fit(&text, usize::from(width)),
                usize::from(width),
                Style::default()
                    .bg(self.theme.error)
                    .fg(self.theme.bg)
                    .add_modifier(Modifier::BOLD),
            );
        }
    }

    /// Draw the cursor into the cell grid instead of leaving it to the host
    /// terminal. A hardware cursor is repositioned every frame and can blink or
    /// flicker while a fast TUI (an AI agent, for example) repaints. An explicit,
    /// steady cursor follows the daemon snapshot exactly and is visually stable.
    fn render_cursor(&self, frame: &mut Frame, pane: &PaneView, content: Rect) {
        if !pane.focused || self.overlay.is_some() || pane.scroll_offset > 0 {
            return;
        }
        if !pane.cursor.visible {
            return;
        }
        let Some(modifier) = cursor_modifier(pane.cursor.shape) else {
            return;
        };
        if pane.cursor.x >= content.width || pane.cursor.y >= content.height {
            return;
        }
        let x = content.x.saturating_add(pane.cursor.x);
        let y = content.y.saturating_add(pane.cursor.y);
        let buffer = frame.buffer_mut();
        let style = buffer[(x, y)].style().add_modifier(modifier);
        buffer[(x, y)].set_style(style);
    }

    fn render_scrollbar(&self, frame: &mut Frame, main: Rect, pane: &PaneView) {
        let Some(geometry) = scrollbar_geometry(pane) else {
            return;
        };
        let track = Rect::new(
            main.x.saturating_add(geometry.track.x),
            main.y.saturating_add(geometry.track.y),
            geometry.track.width,
            geometry.track.height,
        )
        .intersection(main);
        if track.width == 0 || track.height == 0 {
            return;
        }
        let buffer = frame.buffer_mut();
        buffer.set_style(
            track,
            Style::default()
                .bg(self.theme.title_bg)
                .fg(self.theme.gutter),
        );
        for y in track.y..track.bottom() {
            buffer[(track.x, y)].set_symbol(" ");
        }
        let thumb_top = geometry.thumb_y.max(geometry.track.y).min(
            geometry
                .track
                .y
                .saturating_add(geometry.track.height)
                .saturating_sub(1),
        );
        let thumb_bottom = geometry
            .thumb_y
            .saturating_add(geometry.thumb_height)
            .min(geometry.track.y.saturating_add(geometry.track.height));
        let start = main.y.saturating_add(thumb_top).max(track.y);
        let end = main.y.saturating_add(thumb_bottom).min(track.bottom());
        for y in start..end {
            buffer[(track.x, y)].set_symbol("▐").set_style(
                Style::default()
                    .bg(self.theme.terminal_bg)
                    .fg(if pane.focused {
                        self.theme.accent
                    } else {
                        self.theme.muted
                    }),
            );
        }
    }

    fn render_splits(&self, frame: &mut Frame, main: Rect, frame_data: &fact_protocol::Frame) {
        let buffer = frame.buffer_mut();
        for split in &frame_data.splits {
            let region = split.region;
            let highlighted = self
                .drag_split
                .as_ref()
                .is_some_and(|drag| drag.path == split.path);
            let style = Style::default().fg(if highlighted {
                self.theme.accent
            } else {
                self.theme.gutter
            });
            match split.orientation {
                fact_protocol::SplitOrientation::Horizontal => {
                    let usable = region.width.saturating_sub(1).max(2);
                    let left = ((usable as f32) * split.ratio).round() as u16;
                    let left = left.clamp(1, usable.saturating_sub(1));
                    let x = main.x.saturating_add(region.x).saturating_add(left);
                    for y in region.y..region.y.saturating_add(region.height) {
                        let sy = main.y.saturating_add(y);
                        if x < main.right() && sy < main.bottom() {
                            buffer[(x, sy)].set_symbol("│").set_style(style);
                        }
                    }
                }
                fact_protocol::SplitOrientation::Vertical => {
                    let usable = region.height.saturating_sub(1).max(2);
                    let top = ((usable as f32) * split.ratio).round() as u16;
                    let top = top.clamp(1, usable.saturating_sub(1));
                    let y = main.y.saturating_add(region.y).saturating_add(top);
                    if y >= main.bottom() {
                        continue;
                    }
                    for x in region.x..region.x.saturating_add(region.width) {
                        let sx = main.x.saturating_add(x);
                        if sx < main.right() {
                            buffer[(sx, y)].set_symbol("─").set_style(style);
                        }
                    }
                }
            }
        }
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let buffer = frame.buffer_mut();
        buffer.set_style(
            area,
            Style::default()
                .bg(self.theme.status_bg)
                .fg(self.theme.status_fg),
        );

        let name = match &self.frame {
            Some(frame_data) => frame_data.session.name.clone(),
            None => self.info.session.name.clone(),
        };
        let name = format!(" {name} ");
        let (mut x, _) = buffer.set_stringn(
            area.x,
            area.y,
            &name,
            usize::from(area.width),
            Style::default()
                .bg(self.theme.status_bg)
                .fg(self.theme.fg)
                .add_modifier(Modifier::BOLD),
        );
        if self.prefix_armed && x.saturating_add(8) <= area.right() {
            let (next, _) = buffer.set_stringn(
                x,
                area.y,
                " PREFIX ",
                8,
                Style::default()
                    .bg(self.theme.warning)
                    .fg(self.theme.bg)
                    .add_modifier(Modifier::BOLD),
            );
            x = next;
        }

        let minimized = self
            .frame
            .as_ref()
            .map(|frame| frame.minimized.len())
            .unwrap_or(0);
        let mut right_edge = area.right();
        if let Some(tray) = minimized_tray_rect(area.width, minimized) {
            let tray_x = area.x.saturating_add(tray.x);
            buffer.set_stringn(
                tray_x,
                area.y,
                format!("▣ {minimized} "),
                usize::from(tray.width),
                Style::default()
                    .bg(self.theme.title_bg)
                    .fg(self.theme.warning)
                    .add_modifier(Modifier::BOLD),
            );
            right_edge = tray_x;
        }

        let (message, style) = if let Some((message, _)) = &self.status {
            (
                format!(" {message} "),
                Style::default()
                    .bg(self.theme.status_bg)
                    .fg(self.theme.warning),
            )
        } else if self.frame.is_none() {
            (
                " connecting ".to_string(),
                Style::default()
                    .bg(self.theme.status_bg)
                    .fg(self.theme.muted),
            )
        } else {
            return;
        };
        let available = right_edge.saturating_sub(x);
        let message = fit(&message, usize::from(available));
        let width = (message.chars().count() as u16).min(available);
        let message_x = right_edge.saturating_sub(width);
        if width > 0 && message_x >= x {
            buffer.set_stringn(message_x, area.y, message, usize::from(width), style);
        }
    }

    fn render_overlay(&self, frame: &mut Frame, area: Rect) {
        let Some(overlay) = &self.overlay else {
            return;
        };
        match overlay {
            Overlay::Cold(cold) => self.render_cold(frame, area, cold),
            Overlay::Help(help) => self.render_help(frame, area, help),
            Overlay::Palette(palette) => self.render_palette(frame, area, palette),
            Overlay::Search(search) => self.render_search(frame, area, search),
            Overlay::Prompt(prompt) => self.render_prompt(frame, area, prompt),
            Overlay::Confirm(confirm) => self.render_confirm(frame, area, confirm),
            Overlay::Context(context) => self.render_context(frame, area, context),
            Overlay::Sessions(sessions) => self.render_sessions(frame, area, sessions),
        }
    }

    fn render_cold(&self, frame: &mut Frame, area: Rect, cold: &ColdState) {
        let height = (cold.notes.len() as u16 + 5).min(area.height.saturating_sub(2));
        let inner = panel(
            frame,
            area,
            " Session recovered ",
            66,
            height.max(7),
            self.theme.warning,
            &self.theme,
        );
        if inner.height == 0 {
            return;
        }
        let mut lines = Vec::new();
        lines.push(Line::from(Span::styled(
            "The layout and visible output were restored.",
            Style::default()
                .fg(self.theme.warning)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        for note in &cold.notes {
            lines.push(Line::from(vec![
                Span::styled("• ", Style::default().fg(self.theme.warning)),
                Span::styled(note.clone(), Style::default().fg(self.theme.fg)),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Press Enter to continue.",
            Style::default().fg(self.theme.muted),
        )));
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .style(Style::default().bg(self.theme.bg).fg(self.theme.fg)),
            inner,
        );
    }

    fn render_help(&self, frame: &mut Frame, area: Rect, help: &HelpState) {
        let inner = panel(
            frame,
            area,
            " Keyboard help ",
            82,
            area.height.saturating_sub(4).max(9),
            self.theme.accent,
            &self.theme,
        );
        if inner.height == 0 {
            return;
        }
        if inner.height < 2 {
            return;
        }
        let rows = usize::from(inner.height.saturating_sub(1));
        let lines = help_lines(&self.keys);
        let max_scroll = lines.len().saturating_sub(rows);
        let scroll = help.scroll.min(max_scroll);
        let body_height = inner.height.saturating_sub(1);
        let body = Rect::new(inner.x, inner.y, inner.width, body_height);
        let column_width = usize::from(inner.width.saturating_sub(4) / 2).max(10);
        let mut rendered = Vec::new();
        for (label, key) in lines.iter().skip(scroll).take(rows) {
            rendered.push(Line::from(vec![
                Span::styled(
                    format!("{:<width$}", fit(label, column_width), width = column_width),
                    Style::default().fg(self.theme.fg),
                ),
                Span::styled("  ", Style::default()),
                Span::styled(key.clone(), Style::default().fg(self.theme.accent)),
            ]));
        }
        frame.render_widget(
            Paragraph::new(rendered).style(Style::default().bg(self.theme.bg).fg(self.theme.fg)),
            body,
        );
        let footer = if max_scroll > 0 {
            format!(
                "↑↓ scroll ({}/{}) · Esc or F1 closes",
                scroll + 1,
                max_scroll + 1
            )
        } else {
            "Esc or F1 closes".to_string()
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                footer,
                Style::default().fg(self.theme.muted),
            )))
            .style(Style::default().bg(self.theme.bg)),
            Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
        );
    }

    fn render_palette(&self, frame: &mut Frame, area: Rect, palette: &PaletteState) {
        let inner = panel(
            frame,
            area,
            " Commands ",
            64,
            area.height.saturating_sub(4).clamp(8, 20),
            self.theme.accent,
            &self.theme,
        );
        if inner.height < 3 || inner.width == 0 {
            return;
        }
        let items = palette_items(&self.keys);
        let filtered = filtered_palette(&items, &palette.query);
        let input_y = inner.y;
        {
            let buffer = frame.buffer_mut();
            buffer.set_style(
                Rect::new(inner.x, input_y, inner.width, 1),
                Style::default().bg(self.theme.title_bg),
            );
            buffer.set_stringn(
                inner.x,
                input_y,
                "> ",
                2,
                Style::default()
                    .fg(self.theme.accent)
                    .bg(self.theme.title_bg)
                    .add_modifier(Modifier::BOLD),
            );
            buffer.set_stringn(
                inner.x + 2,
                input_y,
                &palette.query,
                usize::from(inner.width.saturating_sub(2)),
                Style::default().fg(self.theme.fg).bg(self.theme.title_bg),
            );
        }
        let cursor_x = (inner.x + 2 + palette.query.chars().count() as u16)
            .min(inner.right().saturating_sub(1));
        frame.set_cursor_position(Position::new(cursor_x, input_y));

        let list_y = input_y.saturating_add(2);
        let visible = usize::from(inner.bottom().saturating_sub(list_y).saturating_sub(1));
        let selected = palette.selected.min(filtered.len().saturating_sub(1));
        let start = selected.saturating_sub(visible.saturating_sub(1));
        let mut lines = Vec::new();
        for (index, item) in filtered.iter().enumerate().skip(start).take(visible) {
            let is_selected = index == selected;
            let style = if is_selected {
                Style::default()
                    .bg(self.theme.selection)
                    .fg(self.theme.fg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(self.theme.bg).fg(self.theme.fg)
            };
            let shortcut_width = item.shortcut.chars().count().min(20);
            let label_width = usize::from(inner.width)
                .saturating_sub(shortcut_width)
                .saturating_sub(4);
            let line = format!(
                " {:<label_width$}  {:<shortcut_width$}",
                fit(item.label, label_width),
                fit(&item.shortcut, shortcut_width)
            );
            lines.push(Line::from(Span::styled(
                fit(&line, usize::from(inner.width)),
                style,
            )));
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "  No matching commands",
                Style::default().fg(self.theme.muted),
            )));
        }
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(self.theme.bg).fg(self.theme.fg)),
            Rect::new(
                inner.x,
                list_y,
                inner.width,
                inner.bottom().saturating_sub(list_y).saturating_sub(1),
            ),
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Enter run · Esc close",
                Style::default().fg(self.theme.muted),
            )))
            .style(Style::default().bg(self.theme.bg)),
            Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
        );
    }

    fn render_search(&self, frame: &mut Frame, area: Rect, search: &SearchState) {
        let inner = panel(
            frame,
            area,
            " Search pane ",
            74,
            area.height.saturating_sub(4).max(9),
            self.theme.accent,
            &self.theme,
        );
        if inner.height < 3 || inner.width == 0 {
            return;
        }
        let buffer = frame.buffer_mut();
        buffer.set_style(
            Rect::new(inner.x, inner.y, inner.width, 1),
            Style::default().bg(self.theme.title_bg),
        );
        buffer.set_stringn(
            inner.x,
            inner.y,
            "> ",
            2,
            Style::default()
                .fg(self.theme.accent)
                .bg(self.theme.title_bg)
                .add_modifier(Modifier::BOLD),
        );
        buffer.set_stringn(
            inner.x + 2,
            inner.y,
            &search.query,
            usize::from(inner.width.saturating_sub(2)),
            Style::default().fg(self.theme.fg).bg(self.theme.title_bg),
        );

        if search.result.is_none() {
            let cursor_x = (inner.x + 2 + search.query.chars().count() as u16)
                .min(inner.right().saturating_sub(1));
            frame.set_cursor_position(Position::new(cursor_x, inner.y));
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        "Enter searches this pane's scrollback.",
                        Style::default().fg(self.theme.muted),
                    )),
                    Line::from(Span::styled(
                        "Esc closes · Ctrl+U clears",
                        Style::default().fg(self.theme.muted),
                    )),
                ])
                .style(Style::default().bg(self.theme.bg).fg(self.theme.fg)),
                Rect::new(
                    inner.x,
                    inner.y + 2,
                    inner.width,
                    inner.height.saturating_sub(3),
                ),
            );
            return;
        }

        let result = search.result.as_ref().unwrap();
        let mut lines = Vec::new();
        if result.hits.is_empty() {
            lines.push(Line::from(Span::styled(
                "No matches.",
                Style::default().fg(self.theme.warning),
            )));
        } else {
            for (index, hit) in result.hits.iter().enumerate() {
                let style = if index == search.selected {
                    Style::default()
                        .bg(self.theme.selection)
                        .fg(self.theme.fg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(self.theme.fg)
                };
                lines.push(Line::from(Span::styled(
                    fit(
                        &format!(" {:>6}  {}", hit.line, hit.preview),
                        usize::from(inner.width),
                    ),
                    style,
                )));
            }
        }
        let visible = usize::from(inner.height.saturating_sub(4));
        let start = search
            .selected
            .saturating_sub(visible.saturating_sub(1))
            .min(lines.len().saturating_sub(1));
        let body = lines
            .into_iter()
            .skip(start)
            .take(visible)
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(body).style(Style::default().bg(self.theme.bg).fg(self.theme.fg)),
            Rect::new(
                inner.x,
                inner.y + 2,
                inner.width,
                inner.height.saturating_sub(3),
            ),
        );
        let footer = format!(
            "{} hit{} · Enter jump · Esc close{}",
            result.hits.len(),
            if result.hits.len() == 1 { "" } else { "s" },
            if result.truncated {
                " · truncated"
            } else {
                ""
            }
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                fit(&footer, usize::from(inner.width)),
                Style::default().fg(self.theme.muted),
            )))
            .style(Style::default().bg(self.theme.bg)),
            Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
        );
    }

    fn render_prompt(&self, frame: &mut Frame, area: Rect, prompt: &PromptState) {
        let inner = panel(
            frame,
            area,
            &format!(" {} ", prompt.title),
            60,
            6,
            self.theme.accent,
            &self.theme,
        );
        if inner.height < 2 || inner.width == 0 {
            return;
        }
        let buffer = frame.buffer_mut();
        buffer.set_style(
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
            Style::default().bg(self.theme.title_bg),
        );
        buffer.set_stringn(
            inner.x,
            inner.y + 1,
            "  ",
            2,
            Style::default().bg(self.theme.title_bg),
        );
        buffer.set_stringn(
            inner.x + 2,
            inner.y + 1,
            &prompt.value,
            usize::from(inner.width.saturating_sub(3)),
            Style::default().fg(self.theme.fg).bg(self.theme.title_bg),
        );
        let cursor_x = (inner.x + 2 + prompt.value.chars().count() as u16)
            .min(inner.right().saturating_sub(1));
        frame.set_cursor_position(Position::new(cursor_x, inner.y + 1));
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                match prompt.kind {
                    PromptKind::RenameTab { .. } => "Enter renames · Esc cancels",
                },
                Style::default().fg(self.theme.muted),
            )))
            .style(Style::default().bg(self.theme.bg)),
            Rect::new(inner.x, inner.y + 3, inner.width, 1),
        );
    }

    fn render_confirm(&self, frame: &mut Frame, area: Rect, confirm: &crate::ConfirmState) {
        let inner = panel(
            frame,
            area,
            " Confirm ",
            56,
            7,
            self.theme.error,
            &self.theme,
        );
        if inner.height < 2 {
            return;
        }
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    confirm.message.clone(),
                    Style::default().fg(self.theme.fg),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Enter / Y confirm · Esc / N cancel",
                    Style::default().fg(self.theme.muted),
                )),
            ])
            .wrap(Wrap { trim: false })
            .style(Style::default().bg(self.theme.bg).fg(self.theme.fg)),
            inner,
        );
    }

    fn render_context(&self, frame: &mut Frame, area: Rect, context: &ContextState) {
        let width = 34u16.min(area.width.saturating_sub(2));
        if width < 8 {
            return;
        }
        let height = (context.actions.len() as u16 + 2).min(area.height.saturating_sub(2));
        if height < 3 {
            return;
        }
        let x = context.column.min(area.right().saturating_sub(width));
        let y = context
            .row
            .min(area.bottom().saturating_sub(height))
            .max(area.y);
        let rect = Rect::new(x, y, width, height);
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                " Pane ",
                Style::default().fg(self.theme.accent),
            ))
            .border_style(Style::default().fg(self.theme.accent))
            .style(Style::default().bg(self.theme.bg).fg(self.theme.fg));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.height == 0 {
            return;
        }
        let visible = usize::from(inner.height);
        let start = context
            .selected
            .saturating_sub(visible.saturating_sub(1))
            .min(context.actions.len().saturating_sub(1));
        let mut lines = Vec::new();
        for (index, action) in context.actions.iter().enumerate().skip(start).take(visible) {
            let selected = index == context.selected;
            let label = action_label(action);
            let key = self.keys.describe(action);
            let style = if selected {
                Style::default()
                    .bg(self.theme.selection)
                    .fg(self.theme.fg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(self.theme.fg)
            };
            lines.push(Line::from(Span::styled(
                fit(&format!(" {:<16} {}", label, key), usize::from(inner.width)),
                style,
            )));
        }
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(self.theme.bg)),
            inner,
        );
    }

    fn render_sessions(&self, frame: &mut Frame, area: Rect, sessions: &SessionsState) {
        let inner = panel(
            frame,
            area,
            " Sessions ",
            72,
            area.height.saturating_sub(4).max(9),
            self.theme.accent,
            &self.theme,
        );
        if inner.height < 2 {
            return;
        }
        let visible = usize::from(inner.height.saturating_sub(2));
        let selected = sessions
            .selected
            .min(sessions.sessions.len().saturating_sub(1));
        let start = selected.saturating_sub(visible.saturating_sub(1));
        let mut lines = Vec::new();
        for (index, session) in sessions
            .sessions
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
        {
            let is_selected = index == selected;
            let current = session.name == self.info.session.name;
            let state = if session.live {
                "live"
            } else if session.archived {
                "archived"
            } else {
                "saved"
            };
            let marker = if current { "▸" } else { " " };
            let text = format!(
                " {marker} {:<20} {:<8} {:>3} panes  {}",
                session.name,
                state,
                session.pane_count,
                utc_clock(session.updated_at_ms)
            );
            let style = if is_selected {
                Style::default()
                    .bg(self.theme.selection)
                    .fg(self.theme.fg)
                    .add_modifier(Modifier::BOLD)
            } else if current {
                Style::default().fg(self.theme.accent)
            } else {
                Style::default().fg(self.theme.fg)
            };
            lines.push(Line::from(Span::styled(
                fit(&text, usize::from(inner.width)),
                style,
            )));
        }
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(self.theme.bg).fg(self.theme.fg)),
            Rect::new(
                inner.x,
                inner.y,
                inner.width,
                inner.height.saturating_sub(1),
            ),
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Enter attach · Esc close",
                Style::default().fg(self.theme.muted),
            )))
            .style(Style::default().bg(self.theme.bg)),
            Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
        );
    }
}

fn cursor_modifier(shape: CursorShape) -> Option<Modifier> {
    match shape {
        CursorShape::Block => Some(Modifier::REVERSED),
        CursorShape::Underline | CursorShape::Beam => Some(Modifier::UNDERLINED),
        CursorShape::Hidden => None,
    }
}

fn cell_style(cell: &CellDto, theme: &crate::theme::Theme) -> Style {
    let mut fg = color(cell.fg, theme);
    let mut bg = color(cell.bg, theme);
    let flags = cell.flags;
    if flags & cell_flags::INVERSE != 0 {
        std::mem::swap(&mut fg, &mut bg);
    }
    if flags & cell_flags::HIDDEN != 0 {
        fg = bg;
    }
    let mut style = Style::default().fg(fg).bg(bg);
    if flags & cell_flags::BOLD != 0 {
        style = style.add_modifier(Modifier::BOLD);
    }
    if flags & cell_flags::ITALIC != 0 {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if flags & (cell_flags::UNDERLINE | cell_flags::DOUBLE_UNDERLINE) != 0 {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if flags & cell_flags::DIM != 0 {
        style = style.add_modifier(Modifier::DIM);
    }
    if flags & cell_flags::STRIKEOUT != 0 {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    style
}

fn color(value: ColorDto, theme: &crate::theme::Theme) -> Color {
    match value {
        ColorDto::Named(index) => match index {
            semantic_color::FOREGROUND => theme.terminal_fg,
            semantic_color::BACKGROUND => theme.terminal_bg,
            semantic_color::CURSOR => theme.accent,
            semantic_color::BRIGHT_FOREGROUND => theme.fg,
            semantic_color::DIM_BACKGROUND => theme.title_bg,
            other => Color::Indexed(other),
        },
        ColorDto::Indexed(index) => Color::Indexed(index),
        ColorDto::Rgb { r, g, b } => Color::Rgb(r, g, b),
    }
}

fn panel(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    width_percent: u16,
    height: u16,
    border: Color,
    theme: &crate::theme::Theme,
) -> Rect {
    if area.width < 8 || area.height < 5 {
        return Rect::new(area.x, area.y, 0, 0);
    }
    let max_width = area.width.saturating_sub(4).max(1);
    let desired_width = (u32::from(area.width) * u32::from(width_percent) / 100) as u16;
    let width = desired_width.min(max_width).max(12.min(max_width));
    let max_height = area.height.saturating_sub(2).max(1);
    let height = height.clamp(3, max_height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let rect = Rect::new(x, y, width, height);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            title.to_string(),
            Style::default().fg(border).add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(border))
        .style(Style::default().bg(theme.bg).fg(theme.fg));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    inner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_adds_ellipsis() {
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("abc", 4), "abc");
        assert_eq!(fit("abc", 0), "");
    }

    #[test]
    fn cursor_shapes_map_to_steady_cell_modifiers() {
        assert_eq!(
            cursor_modifier(CursorShape::Block),
            Some(Modifier::REVERSED)
        );
        assert_eq!(
            cursor_modifier(CursorShape::Underline),
            Some(Modifier::UNDERLINED)
        );
        assert_eq!(
            cursor_modifier(CursorShape::Beam),
            Some(Modifier::UNDERLINED)
        );
        assert_eq!(cursor_modifier(CursorShape::Hidden), None);
    }

    #[test]
    fn semantic_colors_use_theme_defaults() {
        let theme = crate::theme::Theme::fact_dark();
        assert_eq!(
            color(ColorDto::Named(semantic_color::BACKGROUND), &theme),
            theme.terminal_bg
        );
        assert_eq!(
            color(ColorDto::Rgb { r: 1, g: 2, b: 3 }, &theme),
            Color::Rgb(1, 2, 3)
        );
    }
}
