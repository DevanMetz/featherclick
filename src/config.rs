//! User settings and their on-disk representation.
//!
//! A file that does not parse is reported and left byte for byte. Callers
//! decide whether that is fatal. A write goes to a temporary file in the same
//! directory, is flushed, and only then replaces the destination, so a crash
//! leaves the previous file in place.

use std::path::{Path, PathBuf};

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

/// Settings read from disk, plus why they fell back to defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadOutcome {
    pub settings: Settings,
    /// Set when a file was present but could not be used. The file is unchanged.
    pub error: Option<String>,
}

/// Reads the config file. A missing file is defaults and not an error.
pub fn load_outcome() -> LoadOutcome {
    read_at(&config_path())
}

pub fn save(settings: &Settings) -> std::io::Result<()> {
    write_at(&config_path(), settings, false)
}

/// Like [`save`], but copies an existing file to `config.json.bak` first.
///
/// The copy happens before the replacement. If the copy fails, the original
/// file is left alone.
pub fn save_with_backup(settings: &Settings) -> std::io::Result<()> {
    write_at(&config_path(), settings, true)
}

fn read_at(path: &Path) -> LoadOutcome {
    match std::fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str::<Settings>(&raw) {
            Ok(mut settings) => {
                settings.sanitize();
                LoadOutcome {
                    settings,
                    error: None,
                }
            }
            Err(err) => LoadOutcome {
                settings: Settings::default(),
                error: Some(format!("cannot parse {}: {err}", path.display())),
            },
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => LoadOutcome {
            settings: Settings::default(),
            error: None,
        },
        Err(err) => LoadOutcome {
            settings: Settings::default(),
            error: Some(format!("cannot read {}: {err}", path.display())),
        },
    }
}

fn write_at(path: &Path, settings: &Settings, backup: bool) -> std::io::Result<()> {
    if backup && path.is_file() {
        std::fs::copy(path, backup_path(path))?;
    }
    // Persist the clamped values, so the next launch does not depend on
    // repairing the same hand edit again. The caller's copy is left as it is.
    let mut settings = settings.clone();
    settings.sanitize();
    let json = serde_json::to_string_pretty(&settings)?;
    write_atomic(path, json.as_bytes())
}

/// Sync the temp file before renaming it over `path`. A crash during the write
/// then keeps the previous file, and the new bytes are already durable once
/// the name points at them.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = temp_path(path);
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut file, bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(err) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

fn backup_path(path: &Path) -> PathBuf {
    sibling(path, ".bak")
}

fn temp_path(path: &Path) -> PathBuf {
    sibling(path, ".tmp")
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
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

    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("featherclick-config-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn a_missing_file_is_defaults_without_an_error() {
        let dir = Scratch::new();
        let outcome = read_at(&dir.path.join("config.json"));
        assert!(outcome.error.is_none());
        assert_eq!(outcome.settings, Settings::default());
    }

    #[test]
    fn loading_clamps_without_rewriting_the_file() {
        let dir = Scratch::new();
        let path = dir.path.join("config.json");
        let raw =
            r#"{"interval_ms":0,"interval_min_ms":300,"interval_max_ms":50,"future_field":1}"#;
        std::fs::write(&path, raw).unwrap();

        let outcome = read_at(&path);
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(outcome.settings.interval_ms, 1);
        assert_eq!(
            (
                outcome.settings.interval_min_ms,
                outcome.settings.interval_max_ms
            ),
            (50, 300)
        );
        assert_eq!(std::fs::read(&path).unwrap(), raw.as_bytes());
    }

    #[test]
    fn a_file_that_does_not_parse_is_left_byte_for_byte() {
        let dir = Scratch::new();
        let path = dir.path.join("config.json");
        let raw = b"{not json\n";
        std::fs::write(&path, raw).unwrap();

        let outcome = read_at(&path);
        let error = outcome.error.expect("parse failure");
        assert!(error.contains("cannot parse"), "{error}");
        assert!(error.contains("config.json"), "{error}");
        assert_eq!(outcome.settings, Settings::default());
        assert_eq!(std::fs::read(&path).unwrap(), raw);
    }

    #[test]
    fn save_replaces_the_file_and_clamps_what_it_writes() {
        let dir = Scratch::new();
        let path = dir.path.join("nested").join("config.json");
        let settings = Settings {
            interval_ms: 0,
            point: (12, -4),
            button: MouseButton::Middle,
            ..Settings::default()
        };

        write_at(&path, &settings, false).unwrap();

        assert!(!temp_path(&path).exists(), "temp file left behind");
        assert!(!backup_path(&path).exists());
        let outcome = read_at(&path);
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(outcome.settings.interval_ms, 1);
        assert_eq!(outcome.settings.point, (12, -4));
        assert_eq!(outcome.settings.button, MouseButton::Middle);
        // The caller's struct is not rewritten just because the file was.
        assert_eq!(settings.interval_ms, 0);
    }

    #[test]
    fn the_first_replacement_keeps_the_previous_bytes() {
        let dir = Scratch::new();
        let path = dir.path.join("config.json");
        let raw = b"{not json\n";
        std::fs::write(&path, raw).unwrap();

        write_at(&path, &Settings::default(), true).unwrap();

        assert_eq!(std::fs::read(backup_path(&path)).unwrap(), raw);
        assert!(read_at(&path).error.is_none());
        assert!(!temp_path(&path).exists());
    }

    #[test]
    fn a_failed_backup_does_not_replace_the_original() {
        let dir = Scratch::new();
        let path = dir.path.join("config.json");
        let raw = b"{not json\n";
        std::fs::write(&path, raw).unwrap();
        // `copy` refuses a directory destination, on every platform.
        std::fs::create_dir(backup_path(&path)).unwrap();

        let err = write_at(&path, &Settings::default(), true).unwrap_err();
        assert_eq!(std::fs::read(&path).unwrap(), raw, "{err}");
        assert!(!temp_path(&path).exists());
    }

    #[test]
    fn a_failed_write_keeps_both_the_original_and_its_backup() {
        let dir = Scratch::new();
        let path = dir.path.join("config.json");
        let raw = b"{not json\n";
        std::fs::write(&path, raw).unwrap();
        std::fs::create_dir(temp_path(&path)).unwrap();

        let err = write_at(&path, &Settings::default(), true).unwrap_err();
        assert_eq!(std::fs::read(&path).unwrap(), raw, "{err}");
        assert_eq!(std::fs::read(backup_path(&path)).unwrap(), raw);
    }
}
