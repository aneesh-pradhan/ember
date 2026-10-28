//! ember's palette: warm accents on whatever background the terminal has.

use std::sync::OnceLock;

use ratatui::buffer::Buffer;
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
    if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff --git") {
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

/// Whether the terminal shows 24-bit color. macOS Terminal.app is the common exception;
/// EMBER_COLORS=256 or EMBER_COLORS=truecolor overrides the guess.
fn truecolor() -> bool {
    static TRUECOLOR: OnceLock<bool> = OnceLock::new();
    *TRUECOLOR.get_or_init(|| match std::env::var("EMBER_COLORS").as_deref() {
        Ok("256") => false,
        Ok("truecolor" | "24bit") => true,
        _ => std::env::var("TERM_PROGRAM").as_deref() != Ok("Apple_Terminal"),
    })
}

/// Map RGB colors to the nearest xterm-256 color when the terminal can't show 24-bit color.
pub fn adapt(buf: &mut Buffer) {
    if truecolor() {
        return;
    }
    for cell in buf.content.iter_mut() {
        cell.fg = to_256(cell.fg);
        cell.bg = to_256(cell.bg);
    }
}

fn to_256(color: Color) -> Color {
    let Color::Rgb(r, g, b) = color else { return color };
    if r == g && g == b {
        // Grayscale ramp 232..=255 covers 8..=238 in steps of 10.
        return Color::Indexed(match r {
            0..=7 => 16,
            249..=255 => 231,
            v => 232 + ((v as u16 - 8) / 10).min(23) as u8,
        });
    }
    let level = |v: u8| ((v as u16 * 5 + 127) / 255) as u8;
    Color::Indexed(16 + 36 * level(r) + 6 * level(g) + level(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_rgb_to_256() {
        assert_eq!(to_256(Color::Rgb(255, 0, 0)), Color::Indexed(196));
        assert_eq!(to_256(Color::Rgb(128, 128, 128)), Color::Indexed(244));
        assert_eq!(to_256(Color::Reset), Color::Reset);
    }
}
