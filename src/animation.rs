//! ember's small motions, all cosmetic: the banner's sparkles twinkle once at startup, the kawaii mascot blinks at an
//! idle prompt, the working line spins and names the step it's on, and a finished fix or an error gets a brief flash.
//! Everything is drawn into the frame from state, so nothing is printed between streamed output and typing never
//! waits on a frame.
//!
//! Settings come from `[animation]` in `~/.config/ember/config.toml`. Motion is also off when stdout isn't a terminal,
//! when `CI` or `NO_COLOR` is set, or when `EMBER_REDUCED_MOTION=1`.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::theme;

/// Glyphs the banner's twinkle looks for. Single-width, so the cells line up in every terminal.
const SPARKLES: &[&str] = &["✧", "⋆", "✦", "˚", "｡"];
/// How long the banner twinkles after startup, before speed is applied.
const TWINKLE: Duration = Duration::from_millis(1000);
/// How long the sparkle burst shows after a fix passes.
const BURST: Duration = Duration::from_millis(500);
/// How long the status line stays in the error color after an error.
const FLASH: Duration = Duration::from_millis(400);
/// How long the mascot's eyes stay shut in one blink.
const BLINK: Duration = Duration::from_millis(300);
/// Seconds between blinks, drawn at random from this range before speed is applied.
const BLINK_GAP: (f64, f64) = (2.5, 4.0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speed {
    Slow,
    Normal,
    Fast,
}

impl Speed {
    pub fn parse(s: &str) -> Option<Speed> {
        match s.trim().to_ascii_lowercase().as_str() {
            "slow" => Some(Speed::Slow),
            "normal" => Some(Speed::Normal),
            "fast" => Some(Speed::Fast),
            _ => None,
        }
    }

    /// Stretch a motion's timing: slow is more relaxed, fast is snappier.
    fn stretch(self, d: Duration) -> Duration {
        d.mul_f64(match self {
            Speed::Slow => 1.5,
            Speed::Normal => 1.0,
            Speed::Fast => 0.6,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    pub speed: Speed,
    pub mascot_blink: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { enabled: true, speed: Speed::Normal, mascot_blink: true }
    }
}

/// `[animation]` from a parsed config.toml, with defaults wherever a key is missing or unreadable.
fn settings_from(table: &toml::Table) -> Settings {
    let mut settings = Settings::default();
    let Some(section) = table.get("animation").and_then(toml::Value::as_table) else { return settings };
    if let Some(on) = section.get("enabled").and_then(toml::Value::as_bool) {
        settings.enabled = on;
    }
    if let Some(speed) = section.get("speed").and_then(toml::Value::as_str).and_then(Speed::parse) {
        settings.speed = speed;
    }
    if let Some(on) = section.get("mascot_blink").and_then(toml::Value::as_bool) {
        settings.mascot_blink = on;
    }
    settings
}

/// `config.toml` next to the env file: `~/.config/ember/config.toml`.
fn config_file() -> Option<PathBuf> {
    crate::config::user_env_file().map(|p| p.with_file_name("config.toml"))
}

fn read_table() -> toml::Table {
    config_file()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| toml::from_str::<toml::Table>(&s).ok())
        .unwrap_or_default()
}

/// Save `enabled` under `[animation]`, keeping the rest of config.toml. Refuses to rewrite a file it can't parse.
pub fn save_enabled(enabled: bool) -> std::io::Result<()> {
    let path = config_file().ok_or_else(|| std::io::Error::other("no config directory"))?;
    let mut table = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str::<toml::Table>(&text)
            .map_err(|e| std::io::Error::other(format!("{} isn't valid TOML: {e}", path.display())))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(e) => return Err(e),
    };
    let section = table.entry("animation").or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let Some(section) = section.as_table_mut() {
        section.insert("enabled".into(), toml::Value::Boolean(enabled));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, toml::to_string(&table).map_err(std::io::Error::other)?)
}

/// Whether this run should stay still whatever the config says.
fn muted_by_environment() -> bool {
    !std::io::stdout().is_terminal()
        || std::env::var_os("CI").is_some()
        || std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
        || std::env::var("EMBER_REDUCED_MOTION").as_deref() == Ok("1")
}

/// What's moving right now. The UI owns it and calls `tick` once per frame, so every timing is measured from there.
pub struct Motion {
    settings: Settings,
    muted: bool,
    /// The time of the latest frame.
    now: Instant,
    /// Startup, for the banner's twinkle.
    started: Instant,
    /// When the next blink starts, and when the current one ends.
    blink_at: Instant,
    blink_until: Option<Instant>,
    /// xorshift state for blink timing; no extra dependency needed.
    rng: u64,
    /// When the last /fix passed, and when the last error happened.
    burst: Option<Instant>,
    flash: Option<Instant>,
}

impl Motion {
    pub fn new(now: Instant) -> Motion {
        let seed = SystemTime::now().duration_since(UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64) | 1;
        Motion::with(settings_from(&read_table()), muted_by_environment(), now, seed)
    }

    fn with(settings: Settings, muted: bool, now: Instant, seed: u64) -> Motion {
        let mut motion = Motion {
            settings,
            muted,
            now,
            started: now,
            blink_at: now,
            blink_until: None,
            rng: seed,
            burst: None,
            flash: None,
        };
        motion.blink_at = motion.next_blink(now);
        motion
    }

    /// Whether motion is on for this run: the config allows it and the environment doesn't mute it.
    pub fn active(&self) -> bool {
        self.settings.enabled && !self.muted
    }

    pub fn muted(&self) -> bool {
        self.muted
    }

    pub fn set_enabled(&mut self, on: bool) {
        self.settings.enabled = on;
    }

    /// Advance blinking. Call once per frame.
    pub fn tick(&mut self, now: Instant) {
        self.now = now;
        match self.blink_until {
            Some(end) if now >= end => {
                self.blink_until = None;
                self.blink_at = self.next_blink(now);
            }
            Some(_) => {}
            None if now >= self.blink_at => self.blink_until = Some(now + BLINK),
            None => {}
        }
    }

    fn next_blink(&mut self, from: Instant) -> Instant {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        let unit = (self.rng % 1000) as f64 / 1000.0;
        let (low, high) = BLINK_GAP;
        from + self.settings.speed.stretch(Duration::from_secs_f64(low + (high - low) * unit))
    }

    /// Brighten the banner's sparkles for a moment after startup, then leave the screen alone.
    pub fn twinkle(&self, buf: &mut Buffer, area: Rect) {
        if !self.active() || self.now.saturating_duration_since(self.started) >= self.settings.speed.stretch(TWINKLE) {
            return;
        }
        let (Some(bright), Some(dim)) = (theme::accent().fg, theme::dim().fg) else { return };
        let step = (self.now.saturating_duration_since(self.started).as_millis() / 120) as u32;
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let Some(cell) = buf.cell_mut((x, y)) else { continue };
                if !SPARKLES.contains(&cell.symbol()) {
                    continue;
                }
                let lit = (u32::from(x) * 7 + u32::from(y) * 3 + step) % 3 != 0;
                cell.set_fg(if lit { bright } else { dim });
            }
        }
    }

    /// The mascot's eyes are shut this frame: only while blinking is on.
    pub fn eyes_closed(&self) -> bool {
        self.active() && self.settings.mascot_blink && self.blink_until.is_some()
    }

    /// The kawaii mascot's face for an idle prompt, with its eyes shut while it blinks. None in other themes, and
    /// none at all when motion is off.
    pub fn idle_face(&self) -> Option<String> {
        if !self.active() {
            return None;
        }
        let face = theme::face(theme::Mood::Working)?;
        Some(if self.eyes_closed() { face.replacen('•', "-", 2) } else { face.to_string() })
    }

    /// A fix passed: start the sparkle burst.
    pub fn celebrate(&mut self) {
        self.burst = Some(Instant::now());
    }

    /// The burst's sparkles for this frame, or None once it has faded.
    pub fn burst(&self) -> Option<String> {
        const SHINE: [&str; 4] = ["✦", "✧", "⋆", "✧"];
        if !self.active() {
            return None;
        }
        let age = self.now.saturating_duration_since(self.burst?);
        if age >= self.settings.speed.stretch(BURST) {
            return None;
        }
        let phase = (age.as_millis() / 100) as usize;
        Some((0..5).map(|i| SHINE[(i + phase) % SHINE.len()]).collect::<Vec<_>>().join(" "))
    }

    /// An error happened: flash the status line once.
    pub fn flash_error(&mut self) {
        self.flash = Some(Instant::now());
    }

    /// Whether the status line should show the error color this frame.
    pub fn error_flash(&self) -> bool {
        self.active()
            && self
                .flash
                .is_some_and(|at| self.now.saturating_duration_since(at) < self.settings.speed.stretch(FLASH))
    }

    /// The spinner beside the working line: a sparkle cycle in kawaii, the braille spinner otherwise. It stops on its
    /// first frame when motion is off.
    pub fn spinner(&self, frame: usize) -> &'static str {
        const KAWAII: [&str; 4] = ["✦", "·", "✧", "·"];
        let frame = if self.active() { frame } else { 0 };
        match theme::current() {
            theme::Theme::Kawaii => KAWAII[frame % KAWAII.len()],
            theme::Theme::Classic => theme::spinner(frame),
        }
    }

    /// What the working line says ember is doing, named from the step it's on. The dots cycle while it works.
    pub fn activity(&self, step: Option<&str>, thinking: bool, frame: usize) -> String {
        let label = match step {
            Some(s) if s.starts_with("Run `") || s.starts_with('$') => {
                if theme::is_test_run(s) {
                    "Running tests"
                } else {
                    "Running a command"
                }
            }
            Some(s) if s.starts_with("Read") || s.starts_with("List") || s.starts_with("Look at") => "Reading files",
            Some(s) if s.starts_with("Search") => "Searching the code",
            Some(s) if s.starts_with("Edit") || s.starts_with("Write") => "Editing files",
            Some(s) if s.starts_with("Web") => "Searching the web",
            Some(s) if s.starts_with("Fetch") => "Reading a web page",
            None if thinking => "Thinking",
            _ => "Working",
        };
        if !self.active() {
            return format!("{label}…");
        }
        format!("{label}{:<3}", ".".repeat((frame / 3) % 4))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn motion(speed: Speed, now: Instant) -> Motion {
        Motion::with(Settings { enabled: true, speed, mascot_blink: true }, false, now, 0x9e37_79b9_7f4a_7c15)
    }

    #[test]
    fn speed_parses_and_stretches() {
        assert_eq!(Speed::parse(" Fast "), Some(Speed::Fast));
        assert_eq!(Speed::parse("warp"), None);
        assert_eq!(Speed::Slow.stretch(Duration::from_secs(1)), Duration::from_millis(1500));
    }

    #[test]
    fn missing_or_bad_keys_fall_back_to_defaults() {
        let table: toml::Table = toml::from_str("[animation]\nspeed = \"warp\"\nmascot_blink = false\n").unwrap();
        let settings = settings_from(&table);
        assert!(settings.enabled);
        assert_eq!(settings.speed, Speed::Normal);
        assert!(!settings.mascot_blink);
        assert_eq!(settings_from(&toml::Table::new()), Settings::default());
    }

    #[test]
    fn blink_closes_then_reopens_within_the_gap() {
        let start = Instant::now();
        let mut m = motion(Speed::Normal, start);
        m.tick(start + Duration::from_millis(4100));
        assert!(m.eyes_closed(), "a blink starts before 4 seconds have passed");
        m.tick(start + Duration::from_millis(4500));
        assert!(!m.eyes_closed(), "the blink ends after 300 ms");
    }

    #[test]
    fn muted_motion_never_blinks_or_flashes() {
        let start = Instant::now();
        let mut m = Motion::with(Settings::default(), true, start, 7);
        m.tick(start + Duration::from_secs(60));
        m.flash_error();
        assert!(!m.eyes_closed());
        assert!(!m.error_flash());
        assert!(m.idle_face().is_none());
    }

    #[test]
    fn activity_names_the_step() {
        let m = motion(Speed::Normal, Instant::now());
        assert_eq!(m.activity(Some("Read src/a.rs"), false, 0).trim_end(), "Reading files");
        assert_eq!(m.activity(Some("Run `cargo test --quiet`"), false, 0).trim_end(), "Running tests");
        assert_eq!(m.activity(None, true, 0).trim_end(), "Thinking");
        assert_eq!(m.activity(None, false, 0).trim_end(), "Working");
        assert_eq!(m.activity(None, true, 3).trim_end(), "Thinking.");
    }
}
