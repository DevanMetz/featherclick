//! User settings and their on-disk representation.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Config file location: `<config_dir>/featherclick/config.json`.
pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("featherclick")
        .join("config.json")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

impl MouseButton {
    pub const ALL: [Self; 3] = [Self::Left, Self::Right, Self::Middle];

    pub fn label(self) -> &'static str {
        match self {
            Self::Left => "Left",
            Self::Right => "Right",
            Self::Middle => "Middle",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClickStyle {
    #[default]
    Single,
    Double,
    Hold,
}

impl ClickStyle {
    pub const ALL: [Self; 3] = [Self::Single, Self::Double, Self::Hold];

    pub fn label(self) -> &'static str {
        match self {
            Self::Single => "Single",
            Self::Double => "Double",
            Self::Hold => "Hold",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimingMode {
    /// Every interval is exactly `interval_ms`.
    #[default]
    Fixed,
    /// Uniformly random between `interval_min_ms` and `interval_max_ms`.
    Uniform,
    /// Normally distributed around `interval_mean_ms` (clamped to +-4 sigma).
    Normal,
}

impl TimingMode {
    pub const ALL: [Self; 3] = [Self::Fixed, Self::Uniform, Self::Normal];

    pub fn label(self) -> &'static str {
        match self {
            Self::Fixed => "Fixed",
            Self::Uniform => "Random",
            Self::Normal => "Human-like",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionMode {
    /// Follow the pointer: click wherever it already is, without moving it.
    #[default]
    Cursor,
    /// A fixed screen point, re-visited before every click.
    Fixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JitterMode {
    #[default]
    Off,
    Rectangle,
    Ellipse,
    Normal,
}

impl JitterMode {
    pub const ALL: [Self; 4] = [Self::Off, Self::Rectangle, Self::Ellipse, Self::Normal];

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Rectangle => "Box",
            Self::Ellipse => "Ellipse",
            Self::Normal => "Spread",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

/// Everything the clicker needs. Unknown fields are ignored and missing fields
/// fall back to [`Default`], so old and hand-edited config files keep loading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // Clicking
    pub button: MouseButton,
    pub style: ClickStyle,
    pub hold_ms: u32,

    // Timing
    pub timing: TimingMode,
    pub interval_ms: u32,
    pub interval_min_ms: u32,
    pub interval_max_ms: u32,
    pub interval_mean_ms: u32,
    pub interval_std_ms: u32,
    pub start_delay_ms: u32,

    // Position
    pub position: PositionMode,
    pub point: (i32, i32),
    pub jitter: JitterMode,
    pub jitter_x: u32,
    pub jitter_y: u32,

    // Limits
    pub max_clicks: u64,
    pub max_seconds: u32,
    pub stop_on_move_px: u32,

    // Hotkeys, stored in the `HotKey` string form ("F6", "Ctrl+Shift+K").
    pub hotkey_toggle: String,
    pub hotkey_capture: String,

    // Window
    pub theme: Theme,
    pub always_on_top: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            button: MouseButton::Left,
            style: ClickStyle::Single,
            hold_ms: 40,

            timing: TimingMode::Uniform,
            interval_ms: 100,
            interval_min_ms: 40,
            interval_max_ms: 120,
            interval_mean_ms: 100,
            interval_std_ms: 25,
            start_delay_ms: 0,

            position: PositionMode::Cursor,
            point: (0, 0),
            jitter: JitterMode::Off,
            jitter_x: 4,
            jitter_y: 4,

            max_clicks: 0,
            max_seconds: 0,
            stop_on_move_px: 0,

            hotkey_toggle: "F6".to_string(),
            hotkey_capture: "F7".to_string(),

            theme: Theme::Dark,
            always_on_top: false,
        }
    }
}

impl Settings {
    /// Clamps hand-edited or stale values into ranges the engine can trust.
    pub fn sanitize(&mut self) {
        self.hold_ms = self.hold_ms.clamp(1, 60_000);
        self.interval_ms = self.interval_ms.clamp(1, 600_000);
        self.interval_min_ms = self.interval_min_ms.clamp(1, 600_000);
        self.interval_max_ms = self.interval_max_ms.clamp(1, 600_000);
        if self.interval_min_ms > self.interval_max_ms {
            std::mem::swap(&mut self.interval_min_ms, &mut self.interval_max_ms);
        }
        self.interval_mean_ms = self.interval_mean_ms.clamp(1, 600_000);
        self.interval_std_ms = self.interval_std_ms.min(self.interval_mean_ms);
        self.start_delay_ms = self.start_delay_ms.min(86_400_000);
        self.jitter_x = self.jitter_x.min(10_000);
        self.jitter_y = self.jitter_y.min(10_000);
        self.max_seconds = self.max_seconds.min(86_400);
        self.stop_on_move_px = self.stop_on_move_px.min(10_000);
    }

    /// Expected clicks per second for the configured timing, as a range.
    pub fn rate_range(&self) -> (f64, f64) {
        let to_rate = |ms: f64| if ms <= 0.0 { 0.0 } else { 1000.0 / ms };
        match self.timing {
            TimingMode::Fixed => {
                let r = to_rate(self.interval_ms as f64);
                (r, r)
            }
            TimingMode::Uniform => (
                to_rate(self.interval_max_ms as f64),
                to_rate(self.interval_min_ms as f64),
            ),
            TimingMode::Normal => {
                let mean = self.interval_mean_ms as f64;
                let spread = 4.0 * self.interval_std_ms as f64;
                (to_rate(mean + spread), to_rate((mean - spread).max(1.0)))
            }
        }
    }
}

pub fn load() -> Settings {
    let mut settings = std::fs::read_to_string(config_path())
        .ok()
        .and_then(|raw| match serde_json::from_str::<Settings>(&raw) {
            Ok(settings) => Some(settings),
            Err(err) => {
                eprintln!("featherclick: ignoring malformed config: {err}");
                None
            }
        })
        .unwrap_or_default();
    settings.sanitize();
    settings
}

pub fn save(settings: &Settings) -> std::io::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(settings)?;
    // Write-then-rename so a crash mid-write cannot truncate the config.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_repairs_inverted_and_empty_ranges() {
        let mut s = Settings {
            interval_min_ms: 300,
            interval_max_ms: 50,
            interval_ms: 0,
            interval_std_ms: 900,
            interval_mean_ms: 40,
            ..Settings::default()
        };
        s.sanitize();
        assert_eq!((s.interval_min_ms, s.interval_max_ms), (50, 300));
        assert_eq!(s.interval_ms, 1, "zero interval would spin the click loop");
        assert!(
            s.interval_std_ms <= s.interval_mean_ms,
            "a sigma above the mean would make the sampler clamp constantly"
        );
    }

    #[test]
    fn config_round_trips_and_tolerates_unknown_and_missing_fields() {
        let original = Settings {
            point: (1234, -56),
            button: MouseButton::Right,
            hotkey_toggle: "Ctrl+Shift+K".to_string(),
            ..Settings::default()
        };

        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), original);

        let legacy = r#"{"button":"middle","point":[7,8],"future_field":1}"#;
        let parsed: Settings = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.button, MouseButton::Middle);
        assert_eq!(parsed.point, (7, 8));
        assert_eq!(parsed.interval_ms, Settings::default().interval_ms);
    }

    #[test]
    fn rate_range_matches_the_configured_timing() {
        let mut s = Settings {
            timing: TimingMode::Fixed,
            interval_ms: 100,
            ..Settings::default()
        };
        assert_eq!(s.rate_range(), (10.0, 10.0));

        s.timing = TimingMode::Uniform;
        s.interval_min_ms = 50;
        s.interval_max_ms = 200;
        assert_eq!(s.rate_range(), (5.0, 20.0));

        s.timing = TimingMode::Normal;
        s.interval_mean_ms = 100;
        s.interval_std_ms = 25;
        let (low, high) = s.rate_range();
        assert!(
            low < 10.0 && high > 10.0,
            "{low}..{high} should straddle 10/s"
        );
    }
}
