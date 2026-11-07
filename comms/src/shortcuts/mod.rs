//! Local microphone mute and global shortcut state. No server protocol changes.
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(windows)]
mod windows;

/// Even generations transmit; odd generations are muted. Changing generation also
/// invalidates microphone samples/commands buffered before a mute transition.
#[derive(Clone, Default)]
pub struct MicMute(Arc<AtomicU64>);

impl MicMute {
    pub fn generation(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    pub fn is_muted(&self) -> bool {
        self.generation() % 2 == 1
    }

    pub fn permits(&self, generation: u64) -> bool {
        generation % 2 == 0 && self.generation() == generation
    }

    fn set(&self, muted: bool) {
        let old = self.generation();
        if (old % 2 == 1) != muted {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    #[default]
    ToggleMute,
    HoldMute,
}

/// Windows virtual-key codes, including VK_XBUTTON1/2 for Mouse 4/5.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Binding {
    pub action: Action,
    pub key: Option<u16>,
}

pub struct Shortcuts {
    pub bindings: Vec<Binding>,
    pub mute: MicMute,
    pub error: Option<String>,
    down: HashSet<u16>,
    toggle_muted: bool,
    recording: Option<usize>,
    // A press used to record a binding must never execute that binding.
    ignored: HashSet<u16>,
}

pub type SharedShortcuts = Arc<Mutex<Shortcuts>>;

impl Shortcuts {
    pub fn new(bindings: Vec<Binding>, mute: MicMute) -> Self {
        Self {
            bindings,
            mute,
            error: None,
            down: HashSet::new(),
            toggle_muted: false,
            recording: None,
            ignored: HashSet::new(),
        }
    }

    pub fn start(shared: SharedShortcuts) {
        #[cfg(windows)]
        windows::start(shared);
        #[cfg(not(windows))]
        {
            shared.lock().unwrap().error = Some("Global shortcuts require Windows.".into());
        }
    }

    pub fn recording(&self) -> Option<usize> {
        self.recording
    }

    pub fn capturing_input(&self) -> bool {
        self.recording.is_some() || !self.ignored.is_empty()
    }

    pub fn record(&mut self, row: usize) {
        self.recording = Some(row);
        self.refresh();
    }

    pub fn cancel_recording(&mut self) {
        self.recording = None;
        self.refresh();
    }

    pub fn remove(&mut self, row: usize) {
        self.cancel_recording();
        self.bindings.remove(row);
        self.refresh();
    }

    pub fn hold_muted(&self) -> bool {
        self.bindings.iter().any(|b| {
            b.action == Action::HoldMute
                && b.key
                    .is_some_and(|key| self.down.contains(&key) && !self.ignored.contains(&key))
        })
    }

    pub fn refresh(&mut self) {
        self.mute.set(self.toggle_muted || self.hold_muted());
    }

    pub fn toggle(&mut self) {
        self.toggle_muted = !self.toggle_muted;
        self.refresh();
    }

    pub fn input(&mut self, key: u16, pressed: bool) {
        if !pressed {
            self.down.remove(&key);
            self.ignored.remove(&key);
            self.refresh();
            return;
        }
        // Ignore OS key repeat, including repeated Caps Lock key-down events.
        if !self.down.insert(key) || self.ignored.contains(&key) {
            return;
        }
        if let Some(row) = self.recording.take() {
            if key != 0x1b {
                self.bindings[row].key = Some(key);
            }
            self.ignored.insert(key);
        } else if self
            .bindings
            .iter()
            .any(|b| b.action == Action::ToggleMute && b.key == Some(key))
        {
            // Duplicate bindings of the same key still toggle only once per press.
            self.toggle_muted = !self.toggle_muted;
        }
        self.refresh();
    }

    /// On desktop/session changes Windows may omit key-up events. Clear held keys,
    /// and stay muted until the user explicitly toggles again (privacy first).
    #[cfg(windows)]
    fn lost_input(&mut self) {
        self.toggle_muted = self.mute.is_muted();
        self.down.clear();
        self.ignored.clear();
        self.recording = None;
        self.refresh();
    }
}

pub fn key_name(key: Option<u16>) -> String {
    let Some(key) = key else {
        return "Click to record".into();
    };
    match key {
        0x01 => "Mouse 1".into(),
        0x02 => "Mouse 2".into(),
        0x04 => "Mouse 3".into(),
        0x05 => "Mouse 4".into(),
        0x06 => "Mouse 5".into(),
        0x08 => "Backspace".into(),
        0x09 => "Tab".into(),
        0x0d => "Enter".into(),
        0x13 => "Pause".into(),
        0x14 => "CapsLock".into(),
        0x20 => "Space".into(),
        0x21 => "Page Up".into(),
        0x22 => "Page Down".into(),
        0x23 => "End".into(),
        0x24 => "Home".into(),
        0x25 => "Left".into(),
        0x26 => "Up".into(),
        0x27 => "Right".into(),
        0x28 => "Down".into(),
        0x2d => "Insert".into(),
        0x2e => "Delete".into(),
        0x30..=0x39 | 0x41..=0x5a => char::from_u32(key as u32).unwrap().to_string(),
        0x60..=0x69 => format!("Numpad {}", key - 0x60),
        0x70..=0x87 => format!("F{}", key - 0x6f),
        0xa0 => "Left Shift".into(),
        0xa1 => "Right Shift".into(),
        0xa2 => "Left Ctrl".into(),
        0xa3 => "Right Ctrl".into(),
        0xa4 => "Left Alt".into(),
        0xa5 => "Right Alt".into(),
        _ => format!("Key {key:#04X}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(bindings: &[(Action, u16)]) -> Shortcuts {
        Shortcuts::new(
            bindings
                .iter()
                .map(|&(action, key)| Binding {
                    action,
                    key: Some(key),
                })
                .collect(),
            MicMute::default(),
        )
    }

    #[test]
    fn overlapping_holds_only_unmute_after_last_release() {
        let mut s = state(&[(Action::HoldMute, 6), (Action::HoldMute, 0x14)]);
        s.input(6, true);
        s.input(0x14, true);
        s.input(6, false);
        assert!(s.mute.is_muted());
        s.input(0x14, false);
        assert!(!s.mute.is_muted());
    }

    #[test]
    fn releasing_hold_preserves_toggle_and_repeat_does_not_toggle() {
        let mut s = state(&[
            (Action::HoldMute, 6),
            (Action::ToggleMute, 0x14),
            (Action::ToggleMute, 0x14),
        ]);
        s.input(0x14, true);
        s.input(0x14, true);
        s.input(6, true);
        s.input(6, false);
        assert!(s.mute.is_muted());
        s.input(0x14, false);
        s.input(0x14, true);
        assert!(!s.mute.is_muted());
    }

    #[test]
    fn toggling_off_during_hold_waits_for_release() {
        let mut s = state(&[(Action::HoldMute, 6), (Action::ToggleMute, 0x14)]);
        s.toggle();
        s.input(6, true);
        s.input(0x14, true);
        assert!(s.mute.is_muted());
        s.input(6, false);
        assert!(!s.mute.is_muted());
    }

    #[test]
    fn configuring_does_not_release_an_existing_hold() {
        let mut s = state(&[(Action::HoldMute, 6), (Action::ToggleMute, 0x14)]);
        s.input(6, true);
        s.record(1);
        assert!(s.mute.is_muted());
        s.input(0x41, true);
        assert!(s.mute.is_muted());
        s.input(6, false);
        assert!(!s.mute.is_muted());
    }

    #[test]
    fn recording_never_triggers_binding_and_escape_preserves_old_key() {
        let mut s = state(&[(Action::ToggleMute, 6)]);
        s.record(0);
        s.input(0x14, true);
        assert_eq!(s.bindings[0].key, Some(0x14));
        assert!(!s.mute.is_muted());
        assert!(s.capturing_input());
        s.input(0x14, false);
        assert!(!s.capturing_input());
        s.input(0x14, true);
        assert!(s.mute.is_muted());
        s.record(0);
        s.input(0x1b, true);
        assert_eq!(s.bindings[0].key, Some(0x14));
    }

    #[test]
    fn deleting_active_hold_releases_it_and_old_audio_is_invalidated() {
        let mut s = state(&[(Action::HoldMute, 6)]);
        let before = s.mute.generation();
        s.input(6, true);
        assert!(!s.mute.permits(before));
        s.remove(0);
        assert!(!s.mute.is_muted());
        assert!(!s.mute.permits(before));
        assert!(s.mute.permits(s.mute.generation()));
    }
}
