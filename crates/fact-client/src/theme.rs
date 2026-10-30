//! Color themes for the FACT terminal UI.

use ratatui::style::Color;

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub name: &'static str,
    pub bg: Color,
    pub fg: Color,
    pub muted: Color,
    pub accent: Color,
    pub tab_bg: Color,
    pub tab_fg: Color,
    pub title_bg: Color,
    pub title_fg: Color,
    pub title_focus_bg: Color,
    pub title_focus_fg: Color,
    pub status_bg: Color,
    pub status_fg: Color,
    pub gutter: Color,
    pub selection: Color,
    pub warning: Color,
    pub error: Color,
    pub terminal_bg: Color,
    pub terminal_fg: Color,
    /// Default backgrounds are left to the host terminal (see `background`).
    pub transparent: bool,
}

pub const THEME_NAMES: &[&str] = &["fact-dark", "fact-light", "dracula", "nord", "gruvbox"];

impl Theme {
    pub fn by_name(name: &str) -> Self {
        match name {
            "fact-light" => Self::fact_light(),
            "dracula" => Self::dracula(),
            "nord" => Self::nord(),
            "gruvbox" => Self::gruvbox(),
            _ => Self::fact_dark(),
        }
    }

    pub fn next(&self) -> Self {
        let index = THEME_NAMES
            .iter()
            .position(|name| *name == self.name)
            .unwrap_or(0);
        Self::by_name(THEME_NAMES[(index + 1) % THEME_NAMES.len()])
            .with_transparency(self.transparent)
    }

    /// Apply the user's own colors (`[colors]` in config.toml). A background
    /// covers panes, tab strip and status line alike, so they stay one
    /// surface in every theme.
    pub fn with_colors(mut self, colors: &fact_config::ColorOverrides) -> Self {
        let rgb = |[r, g, b]: [u8; 3]| Color::Rgb(r, g, b);
        if let Some(background) = colors.background.map(rgb) {
            self.bg = background;
            self.terminal_bg = background;
            self.tab_bg = background;
            self.status_bg = background;
        }
        if let Some(foreground) = colors.foreground.map(rgb) {
            self.fg = foreground;
            self.terminal_fg = foreground;
        }
        if let Some(accent) = colors.accent.map(rgb) {
            self.accent = accent;
        }
        if let Some(selection) = colors.selection.map(rgb) {
            self.selection = selection;
        }
        self
    }

    /// Leave terminal and screen backgrounds to the host so its background
    /// image or acrylic shows through. Panels, selection and status keep their
    /// opaque colors so they stay readable over any wallpaper.
    pub fn with_transparency(mut self, transparent: bool) -> Self {
        self.transparent = transparent;
        if transparent {
            self.terminal_bg = Color::Reset;
            self.tab_bg = Color::Reset;
        }
        self
    }

    /// Background for screen areas outside panels and overlays.
    pub fn screen_bg(&self) -> Color {
        if self.transparent {
            Color::Reset
        } else {
            self.bg
        }
    }

    pub fn fact_dark() -> Self {
        Self {
            name: "fact-dark",
            transparent: false,
            // The default workspace is intentionally monochrome: black chrome and
            // white type; only selection and the warning/error accents carry color.
            bg: Color::Rgb(0, 0, 0),
            fg: Color::Rgb(231, 233, 237),
            muted: Color::Rgb(132, 138, 148),
            accent: Color::Rgb(226, 229, 235),
            tab_bg: Color::Rgb(0, 0, 0),
            tab_fg: Color::Rgb(145, 151, 161),
            title_bg: Color::Rgb(8, 9, 11),
            title_fg: Color::Rgb(166, 172, 182),
            title_focus_bg: Color::Rgb(17, 18, 21),
            title_focus_fg: Color::Rgb(248, 249, 251),
            status_bg: Color::Rgb(0, 0, 0),
            status_fg: Color::Rgb(157, 163, 173),
            gutter: Color::Rgb(58, 62, 70),
            // A clear blue, readable over black and over wallpapers.
            selection: Color::Rgb(38, 79, 120),
            warning: Color::Rgb(242, 189, 99),
            error: Color::Rgb(244, 112, 112),
            terminal_bg: Color::Rgb(0, 0, 0),
            terminal_fg: Color::Rgb(231, 233, 237),
        }
    }

    pub fn fact_light() -> Self {
        Self {
            name: "fact-light",
            transparent: false,
            bg: Color::Rgb(246, 247, 249),
            fg: Color::Rgb(31, 35, 40),
            muted: Color::Rgb(102, 112, 133),
            accent: Color::Rgb(37, 99, 235),
            tab_bg: Color::Rgb(234, 238, 244),
            tab_fg: Color::Rgb(71, 84, 103),
            title_bg: Color::Rgb(232, 237, 244),
            title_fg: Color::Rgb(52, 64, 84),
            title_focus_bg: Color::Rgb(211, 227, 252),
            title_focus_fg: Color::Rgb(20, 52, 112),
            status_bg: Color::Rgb(234, 238, 244),
            status_fg: Color::Rgb(52, 64, 84),
            gutter: Color::Rgb(208, 214, 224),
            selection: Color::Rgb(199, 219, 255),
            warning: Color::Rgb(180, 83, 9),
            error: Color::Rgb(185, 28, 28),
            terminal_bg: Color::Rgb(255, 255, 255),
            terminal_fg: Color::Rgb(31, 35, 40),
        }
    }

    pub fn dracula() -> Self {
        Self {
            name: "dracula",
            transparent: false,
            bg: Color::Rgb(40, 42, 54),
            fg: Color::Rgb(248, 248, 242),
            muted: Color::Rgb(139, 145, 173),
            accent: Color::Rgb(189, 147, 249),
            tab_bg: Color::Rgb(33, 34, 44),
            tab_fg: Color::Rgb(191, 191, 211),
            title_bg: Color::Rgb(49, 51, 66),
            title_fg: Color::Rgb(220, 220, 238),
            title_focus_bg: Color::Rgb(68, 58, 97),
            title_focus_fg: Color::Rgb(248, 248, 242),
            status_bg: Color::Rgb(33, 34, 44),
            status_fg: Color::Rgb(196, 198, 220),
            gutter: Color::Rgb(68, 71, 90),
            selection: Color::Rgb(68, 71, 120),
            warning: Color::Rgb(241, 250, 140),
            error: Color::Rgb(255, 85, 85),
            terminal_bg: Color::Rgb(33, 34, 44),
            terminal_fg: Color::Rgb(248, 248, 242),
        }
    }

    pub fn nord() -> Self {
        Self {
            name: "nord",
            transparent: false,
            bg: Color::Rgb(46, 52, 64),
            fg: Color::Rgb(216, 222, 233),
            muted: Color::Rgb(143, 154, 173),
            accent: Color::Rgb(136, 192, 208),
            tab_bg: Color::Rgb(40, 45, 56),
            tab_fg: Color::Rgb(184, 193, 207),
            title_bg: Color::Rgb(53, 60, 74),
            title_fg: Color::Rgb(209, 216, 229),
            title_focus_bg: Color::Rgb(65, 84, 104),
            title_focus_fg: Color::Rgb(236, 239, 244),
            status_bg: Color::Rgb(40, 45, 56),
            status_fg: Color::Rgb(190, 199, 213),
            gutter: Color::Rgb(76, 86, 106),
            selection: Color::Rgb(67, 96, 121),
            warning: Color::Rgb(235, 203, 139),
            error: Color::Rgb(191, 97, 106),
            terminal_bg: Color::Rgb(36, 41, 51),
            terminal_fg: Color::Rgb(216, 222, 233),
        }
    }

    pub fn gruvbox() -> Self {
        Self {
            name: "gruvbox",
            transparent: false,
            bg: Color::Rgb(40, 40, 40),
            fg: Color::Rgb(235, 219, 178),
            muted: Color::Rgb(168, 153, 132),
            accent: Color::Rgb(250, 189, 47),
            tab_bg: Color::Rgb(50, 48, 47),
            tab_fg: Color::Rgb(213, 196, 161),
            title_bg: Color::Rgb(60, 56, 54),
            title_fg: Color::Rgb(235, 219, 178),
            title_focus_bg: Color::Rgb(80, 73, 60),
            title_focus_fg: Color::Rgb(251, 241, 199),
            status_bg: Color::Rgb(50, 48, 47),
            status_fg: Color::Rgb(213, 196, 161),
            gutter: Color::Rgb(80, 73, 69),
            selection: Color::Rgb(102, 92, 62),
            warning: Color::Rgb(250, 189, 47),
            error: Color::Rgb(251, 73, 52),
            terminal_bg: Color::Rgb(29, 32, 33),
            terminal_fg: Color::Rgb(235, 219, 178),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_colors_replace_the_theme_surface() {
        let colors = fact_config::ColorOverrides {
            background: Some([16, 24, 32]),
            foreground: Some([230, 225, 207]),
            accent: Some([255, 180, 84]),
            selection: None,
        };
        let theme = Theme::fact_dark().with_colors(&colors);
        let background = Color::Rgb(16, 24, 32);
        assert_eq!(theme.bg, background);
        assert_eq!(theme.terminal_bg, background);
        assert_eq!(theme.tab_bg, background, "the tab strip follows");
        assert_eq!(theme.status_bg, background);
        assert_eq!(theme.terminal_fg, Color::Rgb(230, 225, 207));
        assert_eq!(theme.accent, Color::Rgb(255, 180, 84));
        assert_eq!(theme.selection, Theme::fact_dark().selection);
        // Themes whose strip and status line have their own shade follow too.
        let nord = Theme::nord().with_colors(&colors);
        assert_eq!(nord.tab_bg, background);
        assert_eq!(nord.status_bg, background);
        // Transparency still clears the painted background afterwards.
        let clear = theme.with_transparency(true);
        assert_eq!(clear.terminal_bg, Color::Reset);
        assert_eq!(clear.bg, background, "panels keep an opaque color");
    }

    #[test]
    fn every_theme_cycles() {
        for name in THEME_NAMES {
            let theme = Theme::by_name(name);
            assert_eq!(theme.name, *name);
            assert_ne!(theme.next().name, theme.name);
        }
    }

    #[test]
    fn transparency_survives_theme_cycling() {
        let theme = Theme::fact_dark().with_transparency(true);
        assert_eq!(theme.terminal_bg, Color::Reset);
        assert_eq!(theme.screen_bg(), Color::Reset);
        assert_ne!(theme.bg, Color::Reset, "panels stay opaque");
        let next = theme.next();
        assert!(next.transparent);
        assert_eq!(next.terminal_bg, Color::Reset);
        assert_ne!(Theme::nord().screen_bg(), Color::Reset);
    }

    #[test]
    fn unknown_theme_falls_back() {
        assert_eq!(Theme::by_name("nope").name, "fact-dark");
    }
}
