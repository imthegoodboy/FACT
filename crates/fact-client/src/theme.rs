//! Color themes for the FACT terminal UI.

use ratatui::style::Color;

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub name: &'static str,
    pub bg: Color,
    pub fg: Color,
    pub muted: Color,
    pub accent: Color,
    pub on_accent: Color,
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
    }

    pub fn fact_dark() -> Self {
        Self {
            name: "fact-dark",
            bg: Color::Rgb(15, 17, 21),
            fg: Color::Rgb(214, 218, 226),
            muted: Color::Rgb(121, 130, 145),
            accent: Color::Rgb(79, 140, 255),
            on_accent: Color::Rgb(8, 18, 37),
            tab_bg: Color::Rgb(23, 27, 34),
            tab_fg: Color::Rgb(164, 173, 188),
            title_bg: Color::Rgb(27, 33, 42),
            title_fg: Color::Rgb(188, 197, 210),
            title_focus_bg: Color::Rgb(35, 57, 96),
            title_focus_fg: Color::Rgb(233, 240, 255),
            status_bg: Color::Rgb(24, 30, 40),
            status_fg: Color::Rgb(174, 184, 199),
            gutter: Color::Rgb(44, 52, 66),
            selection: Color::Rgb(45, 78, 128),
            warning: Color::Rgb(240, 178, 74),
            error: Color::Rgb(239, 107, 107),
            terminal_bg: Color::Rgb(10, 12, 16),
            terminal_fg: Color::Rgb(216, 220, 228),
        }
    }

    pub fn fact_light() -> Self {
        Self {
            name: "fact-light",
            bg: Color::Rgb(246, 247, 249),
            fg: Color::Rgb(31, 35, 40),
            muted: Color::Rgb(102, 112, 133),
            accent: Color::Rgb(37, 99, 235),
            on_accent: Color::Rgb(255, 255, 255),
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
            bg: Color::Rgb(40, 42, 54),
            fg: Color::Rgb(248, 248, 242),
            muted: Color::Rgb(139, 145, 173),
            accent: Color::Rgb(189, 147, 249),
            on_accent: Color::Rgb(30, 25, 48),
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
            bg: Color::Rgb(46, 52, 64),
            fg: Color::Rgb(216, 222, 233),
            muted: Color::Rgb(143, 154, 173),
            accent: Color::Rgb(136, 192, 208),
            on_accent: Color::Rgb(23, 32, 42),
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
            bg: Color::Rgb(40, 40, 40),
            fg: Color::Rgb(235, 219, 178),
            muted: Color::Rgb(168, 153, 132),
            accent: Color::Rgb(250, 189, 47),
            on_accent: Color::Rgb(40, 34, 20),
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
    fn every_theme_cycles() {
        for name in THEME_NAMES {
            let theme = Theme::by_name(name);
            assert_eq!(theme.name, *name);
            assert_ne!(theme.next().name, theme.name);
        }
    }

    #[test]
    fn unknown_theme_falls_back() {
        assert_eq!(Theme::by_name("nope").name, "fact-dark");
    }
}
