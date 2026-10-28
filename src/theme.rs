//! ember's palette: warm accents on whatever background the terminal has.

use ratatui::style::{Color, Modifier, Style};

use crate::config::Tier;

pub const EMBER: Color = Color::Rgb(255, 122, 61);
pub const GLOW: Color = Color::Rgb(255, 184, 108);
pub const GREEN: Color = Color::Rgb(94, 201, 135);
pub const RED: Color = Color::Rgb(240, 98, 98);
pub const BLUE: Color = Color::Rgb(110, 168, 254);
pub const GREY: Color = Color::Rgb(128, 128, 128);

pub fn accent() -> Style {
    Style::default().fg(EMBER)
}
pub fn dim() -> Style {
    Style::default().fg(GREY)
}
pub fn ok() -> Style {
    Style::default().fg(GREEN)
}
pub fn err() -> Style {
    Style::default().fg(RED)
}
pub fn info() -> Style {
    Style::default().fg(BLUE)
}
pub fn code() -> Style {
    Style::default().fg(GLOW)
}
pub fn inline_code() -> Style {
    Style::default().fg(GLOW)
}
pub fn link() -> Style {
    Style::default().fg(BLUE).add_modifier(Modifier::UNDERLINED)
}
pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

pub fn tier(t: Tier) -> Style {
    match t {
        Tier::Fast => Style::default().fg(Color::Rgb(86, 204, 214)),
        Tier::Coder => Style::default().fg(Color::Rgb(176, 148, 255)),
        Tier::Reasoner => Style::default().fg(GLOW),
    }
}

pub fn diff_line(line: &str) -> Style {
    if line.starts_with("+++") || line.starts_with("---") {
        bold()
    } else if line.starts_with('+') {
        ok()
    } else if line.starts_with('-') {
        err()
    } else if line.starts_with("@@") {
        info()
    } else {
        dim()
    }
}

pub const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
