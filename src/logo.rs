//! ember's mascot: a chibi dev board with a glowing antenna, drawn as terminal pixel art.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme;

const STEEL: Color = Color::Rgb(150, 158, 172);
const FIN: Color = Color::Rgb(92, 99, 112);
const FAN: Color = Color::Rgb(200, 206, 216);
const PCB_DARK: Color = Color::Rgb(28, 98, 60);
const PORT: Color = Color::Rgb(170, 170, 170);
const BOARD_GREEN: Color = Color::Rgb(118, 185, 0);

/// The mascot, two pixels per terminal row. Each letter is a color from `pixel`; `.` is transparent.
const SPRITE: [&str; 20] = [
    "....................OO..",
    "...................OYOO.",
    "...................OOOO.",
    "....................OO..",
    "....f.f.f.f.f.f.f.f..S..",
    "....f.f.f.f.f.f.f.f..S..",
    "...DDDDDDDDDDDDDDDDDDS..",
    "..DKkkkkkkkkkkkkkkkkKD..",
    ".DKkKKKKKKKKKKKKKKKKKKD.",
    ".DKkKKEEKKKKKKKKEEKKKKD.",
    ".DKKKEWWEKKKKKKEWWEKKKD.",
    ".DKKKEWEEKKKKKKEWEEKKKD.",
    ".DKKKEEEEKMKKMKEEEEKKKD.",
    ".DKPPKEEKKKMMKKKEEKPPKD.",
    ".DKKKKKKKKKKKKKKKKKKKKD.",
    "..DKKKKKKKKKKKKKKKKKKD..",
    "...DDDDDDDDDDDDDDDDDD...",
    ".GGSSGSGGGGGGGGGGGGOGGG.",
    ".gggggggggggggggggggggg.",
    "...SS..............SS...",
];

fn pixel(c: char) -> Option<Color> {
    Some(match c {
        'D' => Color::Rgb(84, 46, 130),    // outline
        'K' => Color::Rgb(140, 90, 206),   // body
        'k' => Color::Rgb(176, 128, 232),  // body highlight
        'F' => Color::Rgb(36, 38, 44),     // heatsink fin
        'f' => Color::Rgb(118, 124, 136),  // fin edge
        'E' => Color::Rgb(16, 16, 20),     // eye
        'W' => Color::Rgb(250, 250, 250),  // eye sparkle
        'P' => Color::Rgb(255, 138, 168),  // blush
        'M' => Color::Rgb(16, 16, 20),     // smile
        'G' => BOARD_GREEN,                // circuit board
        'g' => Color::Rgb(74, 122, 0),     // board edge
        'S' => STEEL,                      // ports and feet
        'O' => theme::EMBER,               // antenna tip and status LED
        'Y' => Color::Rgb(255, 210, 160),  // antenna glint
        _ => return None,
    })
}

/// Render half-block pixel art: each cell shows a top pixel (foreground of `▀`) and a bottom pixel.
fn sprite_rows() -> Vec<Vec<Span<'static>>> {
    SPRITE
        .chunks(2)
        .map(|pair| {
            let (top, bottom) = (pair[0], pair[1]);
            top.chars()
                .zip(bottom.chars())
                .map(|(t, b)| match (pixel(t), pixel(b)) {
                    (None, None) => Span::raw(" "),
                    (Some(t), None) => Span::styled("▀", Style::default().fg(t)),
                    (None, Some(b)) => Span::styled("▄", Style::default().fg(b)),
                    (Some(t), Some(b)) if t == b => Span::styled("█", Style::default().fg(t)),
                    (Some(t), Some(b)) => Span::styled("▀", Style::default().fg(t).bg(b)),
                })
                .collect()
        })
        .collect()
}

/// The startup banner: the mascot with session info beside it (and sparkles, in the kawaii theme).
pub fn banner(model: &str, cwd: &str) -> Vec<Line<'static>> {
    let version = format!(" v{}", env!("CARGO_PKG_VERSION"));
    let model = Span::styled(model.to_string(), theme::tier(crate::config::Tier::Coder));
    let cwd = Span::styled(cwd.to_string(), theme::dim());
    let info: [Vec<Span<'static>>; 10] = match theme::current() {
        theme::Theme::Kawaii => {
            let pink = theme::accent();
            let lilac = theme::tier(crate::config::Tier::Coder);
            [
                vec![],
                vec![Span::styled("˚ ✧ ⋆ ｡", lilac)],
                vec![],
                vec![
                    Span::styled("✧･ﾟ: ", pink),
                    Span::styled("ember", pink.add_modifier(Modifier::BOLD)),
                    Span::styled(version, theme::dim()),
                    Span::styled(" :･ﾟ✧", pink),
                ],
                vec![Span::styled("your pocket Nemotron rig ", theme::dim()), Span::styled("♡ (｡•ᴗ•｡)", pink)],
                vec![model],
                vec![cwd],
                vec![Span::styled("/help for commands ♡ /fix <bug> proves it, then fixes it", theme::dim())],
                vec![],
                vec![Span::styled("｡ ⋆ ✦ ˚", lilac)],
            ]
        }
        theme::Theme::Classic => [
            vec![],
            vec![],
            vec![],
            vec![Span::styled("ember", theme::bold().add_modifier(Modifier::BOLD)), Span::styled(version, theme::dim())],
            vec![Span::styled("your pocket Nemotron rig", theme::dim())],
            vec![model],
            vec![cwd],
            vec![Span::styled("/help for commands · /fix <bug> proves it, then fixes it", theme::dim())],
            vec![],
            vec![],
        ],
    };
    let mut lines = vec![Line::default()];
    for (row, text) in sprite_rows().into_iter().zip(info) {
        let mut spans = vec![Span::raw("  ")];
        spans.extend(row);
        if !text.is_empty() {
            spans.push(Span::raw("   "));
            spans.extend(text);
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::default());
    lines
}

const FAN_FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];
const STREAM: [&str; 6] = ["⠁", "⠃", "⠇", "⠧", "⠷", "⠿"];

fn fg(c: Color) -> Style {
    Style::default().fg(c)
}

/// A one-line board for the working indicator: the fan spins, the LED blinks, and bits stream out. In the kawaii
/// theme its face follows `mood`.
pub fn mini(frame: usize, mood: theme::Mood) -> Vec<Span<'static>> {
    let fan = FAN_FRAMES[frame % FAN_FRAMES.len()];
    let led = if (frame / 3) % 2 == 0 { theme::accent() } else { fg(PCB_DARK) };
    let bits: String = (0..4).map(|i| STREAM[(frame + 5 * 6 - i * 2) % STREAM.len()]).collect();
    vec![
        Span::styled("▐", fg(STEEL)),
        Span::styled("┃┃", fg(FIN)),
        Span::styled(fan, fg(FAN)),
        Span::styled("┃┃", fg(FIN)),
        Span::styled("▌", fg(STEEL)),
        Span::styled("●", led),
        Span::styled("═", fg(PORT)),
        Span::styled(bits, fg(BOARD_GREEN)),
        Span::raw(" "),
    ]
    .into_iter()
    .chain(theme::face(mood).map(|face| Span::styled(format!("{face} "), theme::accent())))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sprite_is_rectangular_and_fully_mapped() {
        assert!(SPRITE.iter().all(|row| row.chars().count() == 24));
        assert!(SPRITE.iter().flat_map(|r| r.chars()).all(|c| c == '.' || pixel(c).is_some()));
        assert_eq!(sprite_rows().len(), 10);
    }
}
