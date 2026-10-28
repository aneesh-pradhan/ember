//! ember's mascot: a tiny dev board on your desk, fan spinning, churning out code.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme;

const STEEL: Color = Color::Rgb(150, 158, 172);
const FIN: Color = Color::Rgb(92, 99, 112);
const FAN: Color = Color::Rgb(200, 206, 216);
const PCB: Color = Color::Rgb(46, 150, 92);
const PCB_DARK: Color = Color::Rgb(28, 98, 60);
const PORT: Color = Color::Rgb(170, 170, 170);
const KEYWORD: Color = Color::Rgb(176, 148, 255);

fn fg(c: Color) -> Style {
    Style::default().fg(c)
}

fn s(text: &str, style: Style) -> Span<'static> {
    Span::styled(text.to_string(), style)
}

/// The startup banner: the board on the left, code streaming out of its port, and session info.
pub fn banner(model: &str, cwd: &str) -> Vec<Line<'static>> {
    let fins = || s("┃┃┃┃┃", fg(FIN));
    let side = || s("█", fg(STEEL));
    // The board is two cells wider than the heatsink on the right, so heatsink rows pad to line up the port.
    let port = || s(" ═╡ ", fg(PORT));
    let padded_port = || s("   ═╡ ", fg(PORT));
    let code = |parts: &[(&str, Style)]| parts.iter().map(|(t, st)| s(t, *st)).collect::<Vec<_>>();
    let kw = fg(KEYWORD);
    let name = fg(theme::GLOW);
    let punct = theme::dim();
    let plain = Style::default();

    let mut lines = vec![
        Line::default(),
        Line::from(vec![s("  ▗", fg(STEEL)), s("▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄", fg(STEEL)), s("▖", fg(STEEL))]),
        Line::from(vec![s("  ", plain), side(), fins(), s("╭───╮", fg(FAN)), fins(), side()]),
        {
            let mut v = vec![s("  ", plain), side(), fins(), s("│ ", fg(FAN)), s("◉", theme::accent()), s(" │", fg(FAN)), fins(), side(), padded_port()];
            v.extend(code(&[("fn ", kw), ("fix", name), ("(bug) {", punct)]));
            Line::from(v)
        },
        {
            let mut v = vec![s("  ", plain), side(), fins(), s("╰───╯", fg(FAN)), fins(), side(), padded_port()];
            v.extend(code(&[("  prove", name), ("(bug)?;", punct)]));
            Line::from(v)
        },
        {
            let mut v = vec![s("▄▄", fg(PCB)), side(), s("▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄", fg(PCB)), side(), s("▄▄", fg(PCB)), port()];
            v.extend(code(&[("  patch", name), ("()", punct)]));
            Line::from(v)
        },
        {
            let mut v = vec![s("▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀", fg(PCB_DARK)), port()];
            v.extend(code(&[("}", punct)]));
            Line::from(v)
        },
        Line::from(vec![s("  ▪ ▪ ▬▬ ▬▬", fg(PORT)), s("     ●", theme::accent())]),
        Line::default(),
    ];
    lines.extend([
        Line::from(vec![
            s("  ember", theme::bold().add_modifier(Modifier::BOLD)),
            s(&format!(" v{}", env!("CARGO_PKG_VERSION")), theme::dim()),
            s("  ·  your pocket Nemotron rig", theme::dim()),
        ]),
        Line::from(vec![s("  ", plain), s(model, theme::tier(crate::config::Tier::Coder)), s(&format!("  ·  {cwd}"), theme::dim())]),
        Line::styled("  /help for commands  ·  /fix <bug> proves it, then fixes it", theme::dim()),
        Line::default(),
    ]);
    lines
}

const FAN_FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];
const STREAM: [&str; 6] = ["⠁", "⠃", "⠇", "⠧", "⠷", "⠿"];

/// A one-line board for the working indicator: the fan spins, the LED blinks, and bits stream out.
pub fn mini(frame: usize) -> Vec<Span<'static>> {
    let fan = FAN_FRAMES[frame % FAN_FRAMES.len()];
    let led = if (frame / 3) % 2 == 0 { theme::accent() } else { fg(PCB_DARK) };
    let bits: String = (0..4).map(|i| STREAM[(frame + 5 * 6 - i * 2) % STREAM.len()]).collect();
    vec![
        s("▐", fg(STEEL)),
        s("┃┃", fg(FIN)),
        s(fan, fg(FAN)),
        s("┃┃", fg(FIN)),
        s("▌", fg(STEEL)),
        s("●", led),
        s("═", fg(PORT)),
        s(&bits, fg(PCB)),
        s(" ", Style::default()),
    ]
}
