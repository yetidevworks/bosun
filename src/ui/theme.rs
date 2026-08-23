//! Theme system. A `Theme` is a set of semantic color slots
//! (backgrounds, text, accents, status) that every render site reads
//! from instead of hardcoding `Color::Rgb`. Themes are TOML files —
//! two ship built in (`opencode`, `tokyonight`), and users can drop
//! extra `.toml` files into `$XDG_CONFIG_HOME/bosun/themes/` which
//! take priority over the built-ins if the name matches.
//!
//! Wire format: TOML with hex strings like `"#7c5cff"`. A small
//! `hex_color` serde module parses them into `ratatui::style::Color`.

use std::path::Path;

use ratatui::style::Color;
use serde::Deserialize;

/// Semantic color slots. Every render site in bosun reads from one of
/// these slots — if you find yourself reaching for `Color::Rgb` in a
/// render function, add a slot here instead.
#[derive(Debug, Clone, Deserialize)]
pub struct Theme {
    pub name: String,

    /// Deepest background. Used for the session list panel, form
    /// fields, and the dim wash behind modals.
    #[serde(with = "hex_color")]
    pub bg: Color,
    /// Slightly lighter panel color for unselected session rows.
    #[serde(with = "hex_color")]
    pub panel: Color,
    /// Secondary panel color for the bottom status bar and modal
    /// bodies — one step up from `panel`.
    #[serde(with = "hex_color")]
    pub panel_alt: Color,
    /// Row / field highlight — applied to the selected session row
    /// and to focused form fields.
    #[serde(with = "hex_color")]
    pub selection_bg: Color,

    /// Primary foreground color.
    #[serde(with = "hex_color")]
    pub text: Color,
    /// Dim foreground for hints, muted labels, window counts.
    #[serde(with = "hex_color")]
    pub text_muted: Color,

    /// Primary accent (bosun purple in the opencode theme). Used for
    /// the "bosun" badge, modal left accent bar, and selection marker.
    #[serde(with = "hex_color")]
    pub accent: Color,
    /// Drop-shadow color behind modals.
    #[serde(with = "hex_color")]
    pub shadow: Color,
    /// Foreground color used when dimming the main UI behind a modal.
    #[serde(with = "hex_color")]
    pub dim_fg: Color,

    /// Session status: running / busy.
    #[serde(with = "hex_color")]
    pub status_running: Color,
    /// Session status: waiting for user input.
    #[serde(with = "hex_color")]
    pub status_waiting: Color,
    /// Session status: idle / no active work.
    #[serde(with = "hex_color")]
    pub status_idle: Color,
    /// Session status: error / destructive action accent.
    #[serde(with = "hex_color")]
    pub status_error: Color,
    /// Session status: finished a turn, not read yet. Optional so a
    /// user theme written before this state existed still parses —
    /// [`Theme::done_color`] falls back to the theme's accent.
    #[serde(default, with = "hex_color_opt")]
    pub status_done: Option<Color>,
}

impl Theme {
    /// Pick a legible foreground for text drawn on top of `bg`.
    /// Chooses near-black or near-white by the background's perceived
    /// luminance (ITU-R BT.601 weights), so accent-filled surfaces —
    /// the "bosun" status chip, the active tab — stay readable on
    /// both light and dark accents across every theme, built-in or
    /// user-supplied. Non-RGB backgrounds (which we can't measure)
    /// fall back to the theme's primary text color.
    ///
    /// The threshold leans slightly toward dark ink: a mid-tone
    /// accent reads better with dark text than white, and that's the
    /// case the default themes hit.
    pub fn on(&self, bg: Color) -> Color {
        match bg {
            Color::Rgb(r, g, b) => {
                let luminance = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
                if luminance > 140.0 {
                    Color::Rgb(0x10, 0x12, 0x18) // near-black ink
                } else {
                    Color::Rgb(0xf2, 0xf3, 0xf5) // near-white ink
                }
            }
            _ => self.text,
        }
    }

    /// Color for the "finished, unread" status glyph. Themes that
    /// predate the state (any user theme, since built-ins all set it)
    /// fall back to the accent, which is always defined and always
    /// distinct from the running/waiting/idle trio.
    pub fn done_color(&self) -> Color {
        self.status_done.unwrap_or(self.accent)
    }

    /// Resolve a theme by name. Checks the user theme directory
    /// first, then the built-in set, then falls back to opencode.
    pub fn load(name: &str, user_dir: Option<&Path>) -> Self {
        if let Some(dir) = user_dir {
            let path = dir.join(format!("{name}.toml"));
            if let Ok(s) = std::fs::read_to_string(&path) {
                match toml::from_str::<Theme>(&s) {
                    Ok(t) => return t,
                    Err(e) => {
                        tracing::warn!("failed to parse user theme {:?}: {}", path, e);
                    }
                }
            }
        }
        Self::builtin(name).unwrap_or_else(Self::default_opencode)
    }

    /// Try to load one of the compiled-in themes by name.
    pub fn builtin(name: &str) -> Option<Self> {
        let src = match name {
            "opencode" => include_str!("../../themes/opencode.toml"),
            "tokyonight" => include_str!("../../themes/tokyonight.toml"),
            "dracula" => include_str!("../../themes/dracula.toml"),
            "catppuccin-mocha" => include_str!("../../themes/catppuccin-mocha.toml"),
            "one-dark-pro" => include_str!("../../themes/one-dark-pro.toml"),
            "ayu-mirage" => include_str!("../../themes/ayu-mirage.toml"),
            "nord" => include_str!("../../themes/nord.toml"),
            "gruvbox-dark" => include_str!("../../themes/gruvbox-dark.toml"),
            "rose-pine" => include_str!("../../themes/rose-pine.toml"),
            "github-dark" => include_str!("../../themes/github-dark.toml"),
            "github-light" => include_str!("../../themes/github-light.toml"),
            "one-light" => include_str!("../../themes/one-light.toml"),
            "solarized-light" => include_str!("../../themes/solarized-light.toml"),
            "ayu-light" => include_str!("../../themes/ayu-light.toml"),
            "quiet-light" => include_str!("../../themes/quiet-light.toml"),
            _ => return None,
        };
        match toml::from_str::<Theme>(src) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::error!("built-in theme {} failed to parse: {}", name, e);
                None
            }
        }
    }

    /// Names of every compiled-in theme, in the order we want them
    /// shown in the picker. `opencode` is the default/anchor so it
    /// goes first; the rest are alphabetized by common usage.
    pub fn builtin_names() -> &'static [&'static str] {
        &[
            "opencode",
            "tokyonight",
            "dracula",
            "catppuccin-mocha",
            "one-dark-pro",
            "ayu-mirage",
            "nord",
            "gruvbox-dark",
            "rose-pine",
            "github-dark",
            "github-light",
            "one-light",
            "solarized-light",
            "ayu-light",
            "quiet-light",
        ]
    }

    /// Collect every theme available at this moment: built-ins plus
    /// any `.toml` files in the user themes directory. User themes
    /// with the same name as a built-in override the built-in, so we
    /// dedupe by name with user entries winning.
    pub fn available(user_dir: Option<&Path>) -> Vec<String> {
        let mut names: Vec<String> = Self::builtin_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        if let Some(dir) = user_dir {
            if let Ok(read) = std::fs::read_dir(dir) {
                for entry in read.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                        continue;
                    }
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        let stem = stem.to_string();
                        if !names.contains(&stem) {
                            names.push(stem);
                        }
                    }
                }
            }
        }
        names
    }

    /// Hard fallback. The built-in opencode theme should always
    /// parse — if it doesn't, we'd rather panic at startup than run
    /// with a garbled UI.
    pub fn default_opencode() -> Self {
        Self::builtin("opencode").expect("built-in opencode theme must parse")
    }
}

/// Whether the outer terminal can render 24-bit ("true") color.
/// Modern terminals (iTerm2, Ghostty, WezTerm, Warp, kitty, Alacritty,
/// …) advertise it through `COLORTERM=truecolor` / `24bit`. Apple
/// Terminal.app does not — it speaks only the 256-color xterm palette
/// and renders 24-bit SGR sequences as garbage, which is why bosun's
/// all-`Rgb` chrome looked broken there. `BOSUN_TRUECOLOR=1`/`0`
/// forces the decision for terminals that misreport or for testing.
///
/// Resolved once and cached: the environment can't change under a
/// running process, and `ui::draw` calls this every frame.
pub fn terminal_truecolor() -> bool {
    use std::sync::OnceLock;
    static TRUECOLOR: OnceLock<bool> = OnceLock::new();
    *TRUECOLOR.get_or_init(|| {
        if let Some(v) = std::env::var_os("BOSUN_TRUECOLOR") {
            return v == "1" || v == "true";
        }
        matches!(
            std::env::var("COLORTERM").as_deref(),
            Ok("truecolor") | Ok("24bit")
        )
    })
}

/// Replace every 24-bit `Rgb` color in `buf` with the nearest
/// xterm-256 indexed color. Run once per frame (after all rendering)
/// when the outer terminal can't do truecolor, so bosun's chrome
/// *and* any truecolor an embedded pane emitted both land on a palette
/// the terminal can actually display. Doing it as a single post-pass
/// over the finished buffer means the theme, the `on()` helper, and
/// every render site stay blissfully truecolor — only this chokepoint
/// knows about the fallback. Named colors / already-indexed cells pass
/// through untouched.
pub fn degrade_buffer_to_256(buf: &mut ratatui::buffer::Buffer) {
    let area = buf.area;
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            cell.fg = to_256(cell.fg);
            cell.bg = to_256(cell.bg);
            cell.underline_color = to_256(cell.underline_color);
        }
    }
}

/// Map a single color into the 256-color space. Only `Rgb` is
/// rewritten; `Reset`, named ANSI, and `Indexed` colors are already
/// representable and pass through.
fn to_256(c: Color) -> Color {
    match c {
        Color::Rgb(r, g, b) => Color::Indexed(rgb_to_xterm256(r, g, b)),
        other => other,
    }
}

/// Nearest xterm-256 index for a 24-bit color. Considers both the
/// 6×6×6 color cube (indices 16–231) and the 24-step grayscale ramp
/// (232–255) and picks whichever is closer in Euclidean RGB distance —
/// grays in particular look far better off the ramp than off the
/// coarse cube. The 16 system colors (0–15) are skipped on purpose:
/// their actual RGB is terminal/theme-defined and unreliable, so we
/// never quantize *to* them.
fn rgb_to_xterm256(r: u8, g: u8, b: u8) -> u8 {
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];

    // Nearest cube step for one channel → (step index, step value).
    let nearest_step = |c: u8| -> (usize, u8) {
        let mut best = 0usize;
        let mut best_d = u16::MAX;
        for (i, &s) in STEPS.iter().enumerate() {
            let d = (s as i16 - c as i16).unsigned_abs();
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        (best, STEPS[best])
    };
    let (ri, rv) = nearest_step(r);
    let (gi, gv) = nearest_step(g);
    let (bi, bv) = nearest_step(b);
    let cube_code = (16 + 36 * ri + 6 * gi + bi) as u8;

    // Nearest gray on the 232..=255 ramp (values 8, 18, … 238).
    let avg = ((r as u16 + g as u16 + b as u16) / 3) as i16;
    let gray_i = (((avg - 8) as f32 / 10.0).round() as i16).clamp(0, 23);
    let gray_v = (8 + gray_i * 10) as u8;
    let gray_code = 232 + gray_i as u8;

    let dist = |cr: u8, cg: u8, cb: u8| -> i32 {
        let dr = r as i32 - cr as i32;
        let dg = g as i32 - cg as i32;
        let db = b as i32 - cb as i32;
        dr * dr + dg * dg + db * db
    };
    if dist(rv, gv, bv) <= dist(gray_v, gray_v, gray_v) {
        cube_code
    } else {
        gray_code
    }
}

mod hex_color {
    use ratatui::style::Color;
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D>(de: D) -> Result<Color, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(de)?;
        parse_hex(&s).map_err(serde::de::Error::custom)
    }

    pub(super) fn parse_hex(s: &str) -> Result<Color, String> {
        let hex = s.trim().trim_start_matches('#');
        if hex.len() != 6 {
            return Err(format!("expected #RRGGBB hex color, got {s:?}"));
        }
        let r = u8::from_str_radix(&hex[0..2], 16).map_err(|e| e.to_string())?;
        let g = u8::from_str_radix(&hex[2..4], 16).map_err(|e| e.to_string())?;
        let b = u8::from_str_radix(&hex[4..6], 16).map_err(|e| e.to_string())?;
        Ok(Color::Rgb(r, g, b))
    }
}

/// Same as [`hex_color`] but for optional keys — a theme file that
/// omits the color parses to `None` instead of failing. Keeps user
/// themes written against an older bosun loading unchanged when a new
/// status color is added.
mod hex_color_opt {
    use ratatui::style::Color;
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D>(de: D) -> Result<Option<Color>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let Some(s) = Option::<String>::deserialize(de)? else {
            return Ok(None);
        };
        super::hex_color::parse_hex(&s)
            .map(Some)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_to_256_maps_cube_corners() {
        // Pure black / white are the cube corners 16 / 231.
        assert_eq!(rgb_to_xterm256(0, 0, 0), 16);
        assert_eq!(rgb_to_xterm256(255, 255, 255), 231);
        // Exact cube primaries land on their cube codes.
        assert_eq!(rgb_to_xterm256(255, 0, 0), 196); // 16 + 36*5
        assert_eq!(rgb_to_xterm256(0, 255, 0), 46); // 16 + 6*5
        assert_eq!(rgb_to_xterm256(0, 0, 255), 21); // 16 + 5
    }

    #[test]
    fn rgb_to_256_prefers_grayscale_ramp_for_grays() {
        // A neutral mid-gray (#808080) is closer to the 232+ ramp than
        // to any cube step, so it must resolve into the ramp range.
        let idx = rgb_to_xterm256(0x80, 0x80, 0x80);
        assert!((232..=255).contains(&idx), "expected ramp, got {idx}");
    }

    #[test]
    fn to_256_passes_through_non_rgb() {
        assert_eq!(to_256(Color::Reset), Color::Reset);
        assert_eq!(to_256(Color::Indexed(42)), Color::Indexed(42));
        assert_eq!(to_256(Color::Red), Color::Red);
        assert!(matches!(to_256(Color::Rgb(10, 20, 30)), Color::Indexed(_)));
    }

    #[test]
    fn on_picks_dark_ink_for_light_accent() {
        let t = Theme::builtin("tokyonight").expect("tokyonight must parse");
        // tokyonight's accent is a light blue → dark ink for contrast.
        assert_eq!(t.on(t.accent), Color::Rgb(0x10, 0x12, 0x18));
        // An explicitly light background → dark ink.
        assert_eq!(
            t.on(Color::Rgb(0xff, 0xff, 0xff)),
            Color::Rgb(0x10, 0x12, 0x18)
        );
    }

    #[test]
    fn on_picks_light_ink_for_dark_accent() {
        let t = Theme::builtin("opencode").expect("opencode must parse");
        // opencode's accent is a mid-dark purple → light ink stays readable.
        assert_eq!(t.on(t.accent), Color::Rgb(0xf2, 0xf3, 0xf5));
        // An explicitly dark background → light ink.
        assert_eq!(
            t.on(Color::Rgb(0x00, 0x00, 0x00)),
            Color::Rgb(0xf2, 0xf3, 0xf5)
        );
    }

    #[test]
    fn builtin_opencode_parses() {
        let t = Theme::builtin("opencode").expect("opencode must parse");
        assert_eq!(t.name, "opencode");
        assert_eq!(t.accent, Color::Rgb(0x7c, 0x5c, 0xff));
        assert_eq!(t.bg, Color::Rgb(0x0b, 0x0d, 0x12));
    }

    #[test]
    fn builtin_tokyonight_parses() {
        let t = Theme::builtin("tokyonight").expect("tokyonight must parse");
        assert_eq!(t.name, "tokyonight");
        assert_eq!(t.accent, Color::Rgb(0x7a, 0xa2, 0xf7));
    }

    #[test]
    fn every_builtin_listed_also_parses() {
        for name in Theme::builtin_names() {
            let t =
                Theme::builtin(name).unwrap_or_else(|| panic!("built-in theme {name} must parse"));
            assert_eq!(
                t.name, *name,
                "theme file {name}.toml has name = {:?}, expected {:?}",
                t.name, name
            );
        }
    }

    #[test]
    fn available_themes_include_all_builtins_when_no_user_dir() {
        let names = Theme::available(None);
        for builtin in Theme::builtin_names() {
            assert!(names.iter().any(|n| n == builtin), "missing {builtin}");
        }
    }

    #[test]
    fn available_themes_add_user_dir_entries() {
        let dir = tempdir();
        std::fs::write(
            dir.join("my-custom.toml"),
            r##"name = "my-custom"
bg = "#000000"
panel = "#000000"
panel_alt = "#000000"
selection_bg = "#000000"
text = "#ffffff"
text_muted = "#ffffff"
accent = "#ff0000"
shadow = "#000000"
dim_fg = "#000000"
status_running = "#00ff00"
status_waiting = "#ffff00"
status_idle = "#888888"
status_error = "#ff0000"
"##,
        )
        .unwrap();
        let names = Theme::available(Some(&dir));
        assert!(names.contains(&"my-custom".to_string()));
        // User themes don't duplicate built-ins
        let opencode_count = names.iter().filter(|n| *n == "opencode").count();
        assert_eq!(opencode_count, 1);
    }

    #[test]
    fn unknown_builtin_is_none() {
        assert!(Theme::builtin("does-not-exist").is_none());
    }

    #[test]
    fn load_missing_theme_falls_back_to_opencode() {
        let t = Theme::load("nonexistent", None);
        assert_eq!(t.name, "opencode");
    }

    #[test]
    fn user_dir_override_takes_precedence() {
        let dir = tempdir();
        std::fs::write(
            dir.join("opencode.toml"),
            r##"name = "opencode-custom"
bg = "#000000"
panel = "#000000"
panel_alt = "#000000"
selection_bg = "#000000"
text = "#ffffff"
text_muted = "#ffffff"
accent = "#ff0000"
shadow = "#000000"
dim_fg = "#000000"
status_running = "#00ff00"
status_waiting = "#ffff00"
status_idle = "#888888"
status_error = "#ff0000"
"##,
        )
        .unwrap();
        let t = Theme::load("opencode", Some(&dir));
        assert_eq!(t.name, "opencode-custom");
        assert_eq!(t.accent, Color::Rgb(0xff, 0x00, 0x00));
    }

    /// A directory of this test's own. The counter is what guarantees
    /// two parallel tests can't land on the same path — a timestamp
    /// alone leaves that to clock resolution.
    fn tempdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let base = std::env::temp_dir().join(format!(
            "bosun-theme-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }
}
