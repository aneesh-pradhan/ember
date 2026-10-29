//! ember's looks: a palette, glyphs and a little personality, on whatever background the terminal has.
//!
//! `kawaii` (the default) is pastel bubblegum with sparkles, hearts and kaomoji, and a mascot whose face follows
//! what it's doing. `classic` is the original warm ember orange. EMBER_THEME or `/theme` picks one.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};

use crate::config::Tier;

/// ember's own orange: the mascot's antenna and LED in every theme.
pub const EMBER: Color = Color::Rgb(255, 122, 61);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    Kawaii,
    Classic,
}

impl Theme {
    pub fn parse(s: &str) -> Option<Theme> {
        match s.trim().to_ascii_lowercase().as_str() {
            "kawaii" | "cute" => Some(Theme::Kawaii),
            "classic" => Some(Theme::Classic),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Theme::Kawaii => "kawaii",
            Theme::Classic => "classic",
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn current() -> Theme {
    if CURRENT.load(Ordering::Relaxed) == 1 { Theme::Classic } else { Theme::Kawaii }
}

pub fn set(theme: Theme) {
    CURRENT.store(if theme == Theme::Classic { 1 } else { 0 }, Ordering::Relaxed);
}

/// Pick the theme at startup: EMBER_THEME, else the one `/theme` saved, else kawaii.
pub fn init() {
    let saved = theme_file().and_then(|p| std::fs::read_to_string(p).ok());
    let theme = std::env::var("EMBER_THEME")
        .ok()
        .as_deref()
        .and_then(Theme::parse)
        .or_else(|| saved.as_deref().and_then(Theme::parse))
        .unwrap_or(Theme::Kawaii);
    set(theme);
}

/// Remember a theme for later sessions (`~/.config/ember/theme`, next to the env file).
pub fn save(theme: Theme) -> std::io::Result<()> {
    let path = theme_file().ok_or_else(|| std::io::Error::other("no config directory"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, format!("{}\n", theme.label()))
}

fn theme_file() -> Option<std::path::PathBuf> {
    crate::config::user_env_file().map(|p| p.with_file_name("theme"))
}

// ------------------------------------------------------------------ palette

struct Palette {
    accent: Color,
    code: Color,
    ok: Color,
    err: Color,
    info: Color,
    dim: Color,
    fast: Color,
    coder: Color,
    reasoner: Color,
}

const CLASSIC: Palette = Palette {
    accent: EMBER,
    code: Color::Rgb(255, 184, 108),
    ok: Color::Rgb(94, 201, 135),
    err: Color::Rgb(240, 98, 98),
    info: Color::Rgb(110, 168, 254),
    dim: Color::Rgb(128, 128, 128),
    fast: Color::Rgb(86, 204, 214),
    coder: Color::Rgb(176, 148, 255),
    reasoner: Color::Rgb(255, 184, 108),
};

/// Pastel bubblegum.
const KAWAII: Palette = Palette {
    accent: Color::Rgb(255, 133, 192),   // bubblegum pink
    code: Color::Rgb(255, 214, 130),     // butter
    ok: Color::Rgb(122, 222, 178),       // mint
    err: Color::Rgb(255, 112, 140),      // coral
    info: Color::Rgb(140, 196, 255),     // baby blue
    dim: Color::Rgb(160, 148, 184),      // lilac grey
    fast: Color::Rgb(140, 196, 255),     // baby blue (Nano)
    coder: Color::Rgb(196, 162, 255),    // lavender (Super)
    reasoner: Color::Rgb(255, 170, 214), // candy pink (Ultra)
};

fn palette() -> &'static Palette {
    match current() {
        Theme::Kawaii => &KAWAII,
        Theme::Classic => &CLASSIC,
    }
}

pub fn accent() -> Style {
    Style::default().fg(palette().accent)
}
pub fn dim() -> Style {
    Style::default().fg(palette().dim)
}
pub fn ok() -> Style {
    Style::default().fg(palette().ok)
}
pub fn err() -> Style {
    Style::default().fg(palette().err)
}
pub fn info() -> Style {
    Style::default().fg(palette().info)
}
pub fn code() -> Style {
    Style::default().fg(palette().code)
}
pub fn inline_code() -> Style {
    code()
}
pub fn link() -> Style {
    info().add_modifier(Modifier::UNDERLINED)
}
pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

pub fn tier(t: Tier) -> Style {
    let p = palette();
    Style::default().fg(match t {
        Tier::Fast => p.fast,
        Tier::Coder => p.coder,
        Tier::Reasoner => p.reasoner,
    })
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

// ------------------------------------------------------------------- glyphs

/// The marks ember draws around text. Each includes its trailing space where it leads a line.
pub struct Glyphs {
    /// Before the user's messages and in the input box.
    pub prompt: &'static str,
    /// Before ember's replies.
    pub reply: &'static str,
    /// The highlighted choice in menus and permission prompts.
    pub select: &'static str,
    pub tool_ok: &'static str,
    pub tool_err: &'static str,
    pub done: &'static str,
    pub failed: &'static str,
    pub todo: &'static str,
    pub todo_active: &'static str,
    pub todo_done: &'static str,
    /// Before reasoning summaries ("reasoned for 3.2s").
    pub reasoning: &'static str,
    spinner: &'static [&'static str],
}

const CLASSIC_GLYPHS: Glyphs = Glyphs {
    prompt: "› ",
    reply: "● ",
    select: "› ",
    tool_ok: "●",
    tool_err: "●",
    done: "✓",
    failed: "✗",
    todo: "○",
    todo_active: "▸",
    todo_done: "✓",
    reasoning: "∴",
    spinner: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"],
};

const KAWAII_GLYPHS: Glyphs = Glyphs {
    prompt: "♡ ",
    reply: "✿ ",
    select: "✧ ",
    tool_ok: "✦",
    tool_err: "✗",
    done: "✿",
    failed: "✗",
    todo: "♡",
    todo_active: "✧",
    todo_done: "♥",
    reasoning: "☁",
    spinner: &["✶", "✷", "✸", "✹", "✺", "✹", "✸", "✷"],
};

pub fn glyphs() -> &'static Glyphs {
    match current() {
        Theme::Kawaii => &KAWAII_GLYPHS,
        Theme::Classic => &CLASSIC_GLYPHS,
    }
}

pub fn spinner(frame: usize) -> &'static str {
    let frames = glyphs().spinner;
    frames[frame % frames.len()]
}

// -------------------------------------------------------------- personality

/// How the mascot feels, shown as a kaomoji beside it in the kawaii theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mood {
    /// Waiting for the user.
    Idle,
    Working,
    Thinking,
    Busy,
    Sweaty,
    Happy,
    Sad,
}

/// The mascot's face for a mood, or None in themes without kaomoji. Faces use only single-width characters:
/// the working indicator redraws ten times a second, and double-width characters (and halfwidth katakana marks
/// like ･ﾟ) left stray, shifted cells behind in tmux when the face changed.
pub fn face(mood: Mood) -> Option<&'static str> {
    if current() != Theme::Kawaii {
        return None;
    }
    Some(match mood {
        Mood::Idle => "(ᵔᴗᵔ)",
        Mood::Working => "(｡•ᴗ•｡)",
        Mood::Thinking => "(˘ω˘)",
        Mood::Busy => "ᕦ(•ᴗ•)ᕤ",
        Mood::Sweaty => "(•_•;)",
        Mood::Happy => "\\(^ᴗ^)/",
        Mood::Sad => "(╥_╥)",
    })
}

/// Whether a finished command looks like a test run, which makes the kawaii mascot happy when it passes.
pub fn is_test_run(summary: &str) -> bool {
    ["test", "pytest", "runtests", "spec"].iter().any(|w| summary.contains(w))
}

// ------------------------------------------------------------ color support

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

    #[test]
    fn themes_parse_and_kaomoji_only_show_in_kawaii() {
        assert_eq!(Theme::parse(" Kawaii "), Some(Theme::Kawaii));
        assert_eq!(Theme::parse("classic"), Some(Theme::Classic));
        assert_eq!(Theme::parse("neon"), None);
        set(Theme::Classic);
        assert_eq!(face(Mood::Happy), None);
        assert_eq!(spinner(0), "⠋");
        set(Theme::Kawaii);
        assert_eq!(face(Mood::Sad), Some("(╥_╥)"));
        assert_eq!(glyphs().todo_done, "♥");
    }

    #[test]
    fn faces_and_spinner_are_single_width() {
        use unicode_width::UnicodeWidthChar;
        set(Theme::Kawaii);
        let moods = [Mood::Working, Mood::Thinking, Mood::Busy, Mood::Sweaty, Mood::Happy, Mood::Sad];
        let text: String = moods.iter().filter_map(|m| face(*m)).chain(KAWAII_GLYPHS.spinner.iter().copied()).collect();
        for c in text.chars() {
            assert!(c.width().unwrap_or(0) <= 1, "{c:?} is double-width");
        }
    }
}
