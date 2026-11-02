//! Colour palette. Truecolor when the terminal says so, a close 256-colour
//! match otherwise, and no colour at all when `NO_COLOR` is set.

use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy)]
pub struct Theme {
    pub accent: Color,
    pub on_accent: Color,
    pub dim: Color,
    pub faint: Color,
    pub good: Color,
    pub bad: Color,
    pub sel_bg: Color,
    pub sel_fg: Color,
    pub plain: bool,
}

impl Theme {
    pub fn detect() -> Self {
        if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            return Self::plain();
        }
        let truecolor =
            std::env::var("COLORTERM").map(|v| v.contains("truecolor") || v.contains("24bit")).unwrap_or(false);
        let pick =
            |rgb: (u8, u8, u8), idx: u8| if truecolor { Color::Rgb(rgb.0, rgb.1, rgb.2) } else { Color::Indexed(idx) };
        Self {
            accent: pick((0xE0, 0x9F, 0x3E), 179),
            on_accent: pick((0x1A, 0x17, 0x14), 234),
            dim: pick((0x8C, 0x85, 0x7B), 245),
            faint: pick((0x57, 0x52, 0x4C), 240),
            good: pick((0x9D, 0xBF, 0x7E), 107),
            bad: pick((0xE5, 0x67, 0x5A), 167),
            sel_bg: pick((0x33, 0x2D, 0x26), 236),
            sel_fg: pick((0xF2, 0xEC, 0xE2), 255),
            plain: false,
        }
    }

    fn plain() -> Self {
        Self {
            accent: Color::Reset,
            on_accent: Color::Reset,
            dim: Color::Reset,
            faint: Color::Reset,
            good: Color::Reset,
            bad: Color::Reset,
            sel_bg: Color::Reset,
            sel_fg: Color::Reset,
            plain: true,
        }
    }

    fn fg(&self, c: Color) -> Style {
        if self.plain {
            Style::new()
        } else {
            Style::new().fg(c)
        }
    }

    pub fn text(&self) -> Style {
        Style::new()
    }
    pub fn strong(&self) -> Style {
        Style::new().add_modifier(Modifier::BOLD)
    }
    pub fn accent(&self) -> Style {
        self.fg(self.accent).add_modifier(if self.plain { Modifier::BOLD } else { Modifier::empty() })
    }
    pub fn dim(&self) -> Style {
        if self.plain {
            Style::new().add_modifier(Modifier::DIM)
        } else {
            self.fg(self.dim)
        }
    }
    pub fn faint(&self) -> Style {
        if self.plain {
            Style::new().add_modifier(Modifier::DIM)
        } else {
            self.fg(self.faint)
        }
    }
    pub fn good(&self) -> Style {
        self.fg(self.good)
    }
    pub fn bad(&self) -> Style {
        self.fg(self.bad).add_modifier(if self.plain { Modifier::BOLD } else { Modifier::empty() })
    }
    /// The brand chip: dark text on the accent colour.
    pub fn badge(&self) -> Style {
        if self.plain {
            Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::new().fg(self.on_accent).bg(self.accent).add_modifier(Modifier::BOLD)
        }
    }
    pub fn danger_badge(&self) -> Style {
        if self.plain {
            Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::new().fg(self.on_accent).bg(self.bad).add_modifier(Modifier::BOLD)
        }
    }
    pub fn selected(&self) -> Style {
        if self.plain {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().bg(self.sel_bg).fg(self.sel_fg)
        }
    }
}
