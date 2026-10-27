//! Global hotkey registration.
//!
//! The manager must live on the thread that owns the platform event loop
//! (required by the Windows and macOS backends), so it is created in
//! `App::new`, which eframe runs on the main thread. Events themselves are
//! delivered on a process-wide channel that any thread may read.

use std::str::FromStr;
use std::time::{Duration, Instant};

pub use global_hotkey::GlobalHotKeyEvent;
use global_hotkey::hotkey::{HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyManager, HotKeyState};

/// What a registered hotkey does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Toggle,
    Capture,
}

/// Parses a config string ("F6", "Ctrl+Shift+K"). Returns `None` for an empty
/// or malformed binding rather than failing the whole config.
pub fn parse(text: &str) -> Option<HotKey> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    HotKey::from_str(trimmed).ok()
}

/// True when the binding text is present but cannot be parsed, which is worth
/// telling the user about.
pub fn invalid(text: &str) -> bool {
    !text.trim().is_empty() && parse(text).is_none()
}

pub struct Hotkeys {
    manager: GlobalHotKeyManager,
    toggle: Option<HotKey>,
    capture: Option<HotKey>,
}

impl Hotkeys {
    pub fn new() -> Result<Self, String> {
        GlobalHotKeyManager::new()
            .map(|manager| Self {
                manager,
                toggle: None,
                capture: None,
            })
            .map_err(|err| format!("global hotkeys unavailable: {err}"))
    }

    /// Replaces every binding. Returns a message when one could not be
    /// registered, which in practice means another application owns it.
    pub fn apply(&mut self, toggle: Option<HotKey>, capture: Option<HotKey>) -> Option<String> {
        for hotkey in [self.toggle, self.capture].into_iter().flatten() {
            let _ = self.manager.unregister(hotkey);
        }
        self.toggle = None;
        self.capture = None;

        let mut problem = None;
        for (action, hotkey) in [(Action::Toggle, toggle), (Action::Capture, capture)] {
            let Some(hotkey) = hotkey else { continue };
            match self.manager.register(hotkey) {
                Ok(()) => match action {
                    Action::Toggle => self.toggle = Some(hotkey),
                    Action::Capture => self.capture = Some(hotkey),
                },
                Err(err) => {
                    problem.get_or_insert_with(|| format!("{hotkey} unavailable: {err}"));
                }
            }
        }
        problem
    }
}

/// Renders a stored binding for display: the backends round-trip a canonical
/// form like `shift+control+KeyK`, which is accurate but not what anyone wants
/// to read in the UI.
pub fn pretty(binding: &str) -> String {
    let trimmed = binding.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    trimmed
        .split('+')
        .map(|part| match part.trim() {
            "shift" => "Shift",
            "control" => "Ctrl",
            "alt" => "Alt",
            "super" => "Super",
            "meta" => "Meta",
            other => other
                .strip_prefix("Key")
                .or_else(|| other.strip_prefix("Digit"))
                .unwrap_or(other),
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// `true` for the press transition only, so a held key fires once.
pub fn is_press(event: &GlobalHotKeyEvent) -> bool {
    event.state == HotKeyState::Pressed
}

/// Filters key events down to genuine presses.
///
/// X11 auto-repeat is the reason this exists: the server emits a *pair* of
/// events (KeyRelease immediately followed by KeyPress) on every repeat tick,
/// so a held hotkey would otherwise re-fire forever and a held toggle would
/// flap. Windows and macOS send repeated presses with no release in between.
/// Each binding is tracked on its own: a release of one key must not rearm
/// the other, or tapping pick-position while start/stop is held toggles twice.
pub struct Gate {
    /// Bindings whose press was accepted and whose release has not arrived.
    held: Vec<u32>,
    /// Latest release of each binding, so a repeat pair is still recognised
    /// when another key's events land between the two halves.
    released: Vec<(u32, Instant)>,
}

/// A release/press pair closer together than this is auto-repeat, not a tap.
/// X11 emits the pair back to back on each repeat tick; a human double-tap
/// leaves far more than this between release and press.
const REPEAT_PAIR_GAP: Duration = Duration::from_millis(25);

impl Gate {
    pub fn new() -> Self {
        Self {
            held: Vec::new(),
            released: Vec::new(),
        }
    }

    /// Feeds one event in; returns `true` when it should trigger its action.
    pub fn accept(&mut self, id: u32, pressed: bool, now: Instant) -> bool {
        self.released
            .retain(|(_, at)| now.saturating_duration_since(*at) < REPEAT_PAIR_GAP);
        if !pressed {
            self.held.retain(|held| *held != id);
            self.note_release(id, now);
            return false;
        }
        let is_repeat = self.released.iter().any(|(released_id, at)| {
            *released_id == id && now.saturating_duration_since(*at) < REPEAT_PAIR_GAP
        });
        if self.held.contains(&id) || is_repeat {
            self.hold(id);
            return false;
        }
        self.hold(id);
        true
    }

    fn hold(&mut self, id: u32) {
        if !self.held.contains(&id) {
            self.held.push(id);
        }
    }

    fn note_release(&mut self, id: u32, now: Instant) {
        if let Some(slot) = self.released.iter_mut().find(|(seen, _)| *seen == id) {
            slot.1 = now;
        } else {
            self.released.push((id, now));
        }
    }
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

/// True when `event` is the toggle binding of `settings`, resolved from the
/// config text so the notifier thread needs no shared manager state.
pub fn matches(event: &GlobalHotKeyEvent, text: &str) -> bool {
    parse(text).is_some_and(|hotkey| hotkey.id() == event.id)
}

pub use global_hotkey::hotkey::Code;

/// Builds a binding from a captured key combination.
pub fn binding(code: Code, ctrl: bool, shift: bool, alt: bool, sup: bool) -> HotKey {
    HotKey::new(modifiers(ctrl, shift, alt, sup), code)
}

/// Modifier set for a captured key combination.
pub fn modifiers(ctrl: bool, shift: bool, alt: bool, sup: bool) -> Option<Modifiers> {
    let mut mods = Modifiers::empty();
    mods.set(Modifiers::CONTROL, ctrl);
    mods.set(Modifiers::SHIFT, shift);
    mods.set(Modifiers::ALT, alt);
    mods.set(Modifiers::SUPER, sup);
    (!mods.is_empty()).then_some(mods)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_strings_round_trip_through_the_hotkey_type() {
        for text in ["F6", "F7", "Ctrl+Shift+K", "Alt+Digit1"] {
            let parsed = parse(text).unwrap_or_else(|| panic!("{text} should parse"));
            let printed = parsed.to_string();
            assert_eq!(
                parse(&printed).map(|hotkey| hotkey.id()),
                Some(parsed.id()),
                "{text} printed as {printed} and did not survive a round trip"
            );
        }
    }

    #[test]
    fn empty_and_malformed_bindings_are_optional_not_fatal() {
        assert!(parse("").is_none());
        assert!(parse("   ").is_none());
        assert!(parse("NotAKey").is_none());
        assert!(!invalid(""), "clearing a binding is allowed");
        assert!(!invalid("F6"));
        assert!(invalid("NotAKey"));
        assert!(!invalid("  "), "whitespace is treated as cleared");
    }

    #[test]
    fn stored_bindings_display_readably() {
        assert_eq!(pretty("F6"), "F6");
        assert_eq!(pretty("shift+control+KeyK"), "Shift+Ctrl+K");
        assert_eq!(pretty("alt+Digit1"), "Alt+1");
        assert_eq!(pretty("super+Numpad0"), "Super+Numpad0");
        assert_eq!(pretty(""), "");
        // whatever the UI prints must still parse back into a working binding
        let stored = "shift+control+KeyK";
        assert_eq!(
            parse(&pretty(stored)).map(|hotkey| hotkey.id()),
            parse(stored).map(|h| h.id())
        );
    }

    #[test]
    fn ids_identify_the_right_binding() {
        let toggle = parse("F8").unwrap();
        let capture = parse("F9").unwrap();
        assert_ne!(toggle.id(), capture.id());
        assert_eq!(parse("F8").unwrap().id(), toggle.id());
    }

    #[test]
    fn gate_passes_distinct_taps_and_ignores_releases() {
        let mut gate = Gate::new();
        let start = Instant::now();
        // A tap is press-then-release; releases alone must never trigger.
        let tap = |gate: &mut Gate, at: Duration| -> bool {
            let triggered = gate.accept(7, true, start + at);
            assert!(
                !gate.accept(7, false, start + at + Duration::from_millis(5)),
                "releases never trigger"
            );
            triggered
        };
        assert!(
            tap(&mut gate, Duration::ZERO),
            "the first press must trigger"
        );
        assert!(
            tap(&mut gate, Duration::from_millis(400)),
            "a second tap must trigger"
        );
        assert!(
            tap(&mut gate, Duration::from_millis(800)),
            "a third tap must trigger"
        );
    }

    #[test]
    fn gate_fires_once_while_a_key_is_held_on_x11() {
        let mut gate = Gate::new();
        let start = Instant::now();
        assert!(
            gate.accept(3, true, start),
            "the initial press must trigger"
        );

        // X11 auto-repeat: after the repeat delay, a Release/Press pair every 33 ms.
        let mut triggers = 0;
        for tick in 0..30 {
            let at = start + Duration::from_millis(500) + Duration::from_millis(33 * tick);
            gate.accept(3, false, at);
            if gate.accept(3, true, at + Duration::from_millis(1)) {
                triggers += 1;
            }
        }
        assert_eq!(triggers, 0, "holding the key must not retrigger");

        // ...and once really released, the next tap works again.
        assert!(!gate.accept(3, false, start + Duration::from_millis(1600)));
        assert!(gate.accept(3, true, start + Duration::from_millis(2000)));
    }

    #[test]
    fn gate_does_not_rearm_a_held_key_when_another_key_is_tapped() {
        let mut gate = Gate::new();
        let start = Instant::now();
        assert!(gate.accept(7, true, start), "start/stop must trigger");
        // Windows repeat: more presses, no release.
        assert!(!gate.accept(7, true, start + Duration::from_millis(500)));

        // Tap pick-position while start/stop is still held.
        assert!(gate.accept(8, true, start + Duration::from_millis(600)));
        assert!(!gate.accept(8, false, start + Duration::from_millis(650)));

        assert!(
            !gate.accept(7, true, start + Duration::from_millis(700)),
            "the held toggle must not fire again after the other key's release"
        );
        assert!(!gate.accept(7, true, start + Duration::from_millis(733)));

        assert!(!gate.accept(7, false, start + Duration::from_millis(900)));
        assert!(
            gate.accept(7, true, start + Duration::from_millis(1200)),
            "a later tap must trigger"
        );
    }

    #[test]
    fn gate_keeps_an_x11_repeat_pair_when_another_release_lands_between() {
        let mut gate = Gate::new();
        let start = Instant::now();
        assert!(gate.accept(7, true, start));
        assert!(gate.accept(8, true, start + Duration::from_millis(100)));

        // Release of 7, then a release of 8, then the repeat press of 7.
        // The pair is still a repeat of 7 even though 8 was the latest release.
        let at = start + Duration::from_millis(200);
        assert!(!gate.accept(7, false, at));
        assert!(!gate.accept(8, false, at + Duration::from_millis(1)));
        assert!(
            !gate.accept(7, true, at + Duration::from_millis(2)),
            "the repeat press of the held key must stay suppressed"
        );
    }

    #[test]
    fn gate_keeps_single_press_platforms_working() {
        // Windows and macOS send one press and a late synthetic release.
        let mut gate = Gate::new();
        let start = Instant::now();
        assert!(gate.accept(5, true, start));
        assert!(!gate.accept(5, false, start + Duration::from_millis(50)));
        assert!(gate.accept(5, true, start + Duration::from_millis(600)));
    }
}
