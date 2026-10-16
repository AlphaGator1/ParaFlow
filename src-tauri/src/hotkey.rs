//! Global push-to-talk key detection via a low-level keyboard hook.
//!
//! Tauri's `global-shortcut` plugin wraps `RegisterHotKey`, which only ever
//! reports a *press*. Push-to-talk needs the press and the release, so we
//! install a `WH_KEYBOARD_LL` hook on a dedicated thread instead and watch the
//! raw key transitions ourselves.

use crossbeam_channel::Sender;
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use std::collections::HashSet;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetDoubleClickTime, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage, HHOOK,
    KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP,
};

/// What the pipeline reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PttEvent {
    /// Every key in the chord is now held.
    Down,
    /// The chord was broken by releasing one of its keys.
    Up,
    /// Start or stop hands-free recording: a double press when idle, and any
    /// single press once latched.
    ToggleLatch,
}

/// Bounds on the gap that counts as a double press.
///
/// The window itself comes from `GetDoubleClickTime`, not a constant of ours.
/// Someone who lengthened it in Ease of Access has told Windows they need
/// longer, and a latch that will not arm for them is a latch they cannot use.
/// Clamped at both ends because the setting is allowed to be 100 ms - barely
/// hittable for a two-key chord - or several seconds, at which point two
/// deliberately separate dictations would latch by accident.
const DOUBLE_PRESS_MIN: Duration = Duration::from_millis(200);
const DOUBLE_PRESS_MAX: Duration = Duration::from_millis(600);

fn clamp_double_window(ms: u64) -> Duration {
    Duration::from_millis(ms).clamp(DOUBLE_PRESS_MIN, DOUBLE_PRESS_MAX)
}

fn double_press_window() -> Duration {
    clamp_double_window(unsafe { GetDoubleClickTime() } as u64)
}

/// Was this press close enough behind the last release to be a double press?
///
/// `None` means the chord has not been released since the last time this was
/// answered, which is how a press is disarmed after it has been used.
fn is_double_press(last_up: Option<Instant>, now: Instant, window: Duration) -> bool {
    last_up.is_some_and(|t| now.saturating_duration_since(t) <= window)
}

/// A push-to-talk binding: every virtual-key code here must be held at once.
#[derive(Debug, Clone)]
pub struct Chord {
    pub keys: Vec<u32>,
}

impl Default for Chord {
    /// Ctrl + Win, matching Wispr Flow's Windows binding.
    fn default() -> Self {
        // Both left/right variants of each side are accepted by `matches`.
        Chord { keys: vec![VK_CONTROL, VK_WIN] }
    }
}

// Canonical (side-agnostic) virtual-key codes we track.
pub const VK_SHIFT: u32 = 0x10;
pub const VK_CONTROL: u32 = 0x11;
pub const VK_MENU: u32 = 0x12; // Alt
pub const VK_WIN: u32 = 0x5B; // treated as either LWIN or RWIN

// Side-specific codes as the low-level hook actually reports them. Spelled out
// rather than written inline: the shift and control pairs are adjacent and easy
// to transpose, and getting them backwards silently makes every chord
// unmatchable rather than failing loudly.
const VK_LSHIFT: u32 = 0xA0;
const VK_RSHIFT: u32 = 0xA1;
const VK_LCONTROL: u32 = 0xA2;
const VK_RCONTROL: u32 = 0xA3;
const VK_LMENU: u32 = 0xA4;
const VK_RMENU: u32 = 0xA5;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;

/// Human-readable name for a canonical virtual-key code.
///
/// Chords are stored in settings.json by name rather than by number, so the
/// file stays editable by hand and readable when someone is working out why
/// their hotkey stopped working.
pub fn vk_name(vk: u32) -> String {
    match vk {
        VK_CONTROL => "Ctrl".into(),
        VK_SHIFT => "Shift".into(),
        VK_MENU => "Alt".into(),
        VK_WIN => "Win".into(),
        VK_SPACE => "Space".into(),
        VK_TAB => "Tab".into(),
        VK_CAPITAL => "CapsLock".into(),
        0x30..=0x39 => char::from(vk as u8).to_string(), // 0-9
        0x41..=0x5A => char::from(vk as u8).to_string(), // A-Z
        0x70..=0x87 => format!("F{}", vk - 0x6F),        // F1-F24
        other => format!("VK_{other:#04X}"),
    }
}

/// Inverse of [`vk_name`]. Returns `None` for anything unrecognised, so a
/// hand-edited settings file with a typo falls back to the default rather
/// than producing an unpressable chord.
pub fn vk_from_name(name: &str) -> Option<u32> {
    let n = name.trim();
    let upper = n.to_ascii_uppercase();
    match upper.as_str() {
        "CTRL" | "CONTROL" => return Some(VK_CONTROL),
        "SHIFT" => return Some(VK_SHIFT),
        "ALT" | "MENU" => return Some(VK_MENU),
        "WIN" | "SUPER" | "META" => return Some(VK_WIN),
        "SPACE" => return Some(VK_SPACE),
        "TAB" => return Some(VK_TAB),
        "CAPSLOCK" => return Some(VK_CAPITAL),
        _ => {}
    }
    if upper.len() == 1 {
        let c = upper.as_bytes()[0];
        if c.is_ascii_alphanumeric() {
            return Some(c as u32);
        }
    }
    if let Some(rest) = upper.strip_prefix('F') {
        if let Ok(n) = rest.parse::<u32>() {
            if (1..=24).contains(&n) {
                return Some(0x6F + n);
            }
        }
    }
    if let Some(hex) = upper.strip_prefix("VK_0X") {
        return u32::from_str_radix(hex, 16).ok();
    }
    None
}

pub const VK_SPACE: u32 = 0x20;
pub const VK_TAB: u32 = 0x09;
pub const VK_CAPITAL: u32 = 0x14;

impl Chord {
    /// The chord as names, for display and for settings.json.
    pub fn names(&self) -> Vec<String> {
        self.keys.iter().map(|k| vk_name(*k)).collect()
    }

    /// Parse a chord from names. `None` if any name is unrecognised or the
    /// result would be empty.
    pub fn from_names<S: AsRef<str>>(names: &[S]) -> Option<Chord> {
        let keys: Option<Vec<u32>> =
            names.iter().map(|n| vk_from_name(n.as_ref()).map(canonical)).collect();
        let keys = keys?;
        if keys.is_empty() {
            return None;
        }
        Some(Chord { keys })
    }

    /// How it should read in the UI: "Ctrl + Win".
    pub fn label(&self) -> String {
        self.names().join(" + ")
    }
}

/// Collapse left/right key variants onto one canonical code so the chord
/// doesn't care which side of the keyboard was used.
fn canonical(vk: u32) -> u32 {
    match vk {
        VK_LCONTROL | VK_RCONTROL | VK_CONTROL => VK_CONTROL,
        VK_LSHIFT | VK_RSHIFT | VK_SHIFT => VK_SHIFT,
        VK_LMENU | VK_RMENU | VK_MENU => VK_MENU,
        VK_LWIN | VK_RWIN => VK_WIN,
        other => other,
    }
}

/// What the settings screen sees while the user presses a new chord.
#[derive(Debug, Clone)]
pub enum CaptureEvent {
    /// Keys held right now, so the field can update as the chord is built.
    Preview(Vec<u32>),
    /// Everything has been released. This is the chord to save.
    Done(Vec<u32>),
}

/// In-flight chord capture.
struct Capture {
    /// The largest set of keys seen held at once. Taking the maximum rather
    /// than the final state matters because releasing a chord is gradual -
    /// reading `held` at the moment the last key goes up would capture one key.
    best: Vec<u32>,
    tx: Sender<CaptureEvent>,
}

struct HookState {
    chord: Chord,
    held: HashSet<u32>,
    /// True while the full chord is engaged, so we only emit one Down per press.
    engaged: bool,
    tx: Sender<PttEvent>,
    /// When set, keystrokes are being read to define a new chord rather than
    /// matched against the current one.
    capture: Option<Capture>,

    // --- hands-free latch -------------------------------------------------
    /// When the chord was last released. `None` disarms double-press detection.
    last_up: Option<Instant>,
    /// How close two presses must be to count as a double press.
    double_window: Duration,
    /// Whether double-tap latching is switched on in settings.
    latch_enabled: bool,
    /// Mirrors the pipeline's latch state. The hook needs it because a press
    /// while latched must always read as "stop", never as the first half of a
    /// new double press.
    latched: bool,
    /// Set when the press in progress was itself a latch action, so its release
    /// does not arm the detector. Without this, the habit of double-tapping to
    /// *stop* would stop the latch and immediately start it again.
    suppress_arm: bool,
}

static STATE: OnceCell<Mutex<HookState>> = OnceCell::new();

/// Order a set of keys for display: modifiers first, in a fixed order, then
/// the rest. Without this a HashSet would produce "Win + Ctrl" one run and
/// "Ctrl + Win" the next.
fn ordered(keys: &HashSet<u32>) -> Vec<u32> {
    const MODIFIER_ORDER: [u32; 4] = [VK_CONTROL, VK_MENU, VK_SHIFT, VK_WIN];
    let mut out: Vec<u32> = MODIFIER_ORDER.iter().copied().filter(|m| keys.contains(m)).collect();
    let mut rest: Vec<u32> =
        keys.iter().copied().filter(|k| !MODIFIER_ORDER.contains(k)).collect();
    rest.sort_unstable();
    out.extend(rest);
    out
}

/// Swap the push-to-talk chord without reinstalling the hook.
pub fn set_chord(chord: Chord) {
    let Some(cell) = STATE.get() else { return };
    let normalized = Chord { keys: chord.keys.iter().copied().map(canonical).collect() };
    let label = normalized.label();
    {
        let mut st = cell.lock();
        st.chord = normalized;
        // Any partially-held state refers to the old chord.
        st.held.clear();
        st.engaged = false;
        st.last_up = None;
        st.suppress_arm = false;
    }
    // Logged after releasing the lock: the hook thread waits on it for every
    // keystroke, and Windows drops hooks that take too long.
    tracing::info!(chord = %label, "push-to-talk chord changed");
}

/// Arm or disarm double-tap latching without reinstalling the hook.
pub fn set_latch_enabled(enabled: bool) {
    let Some(cell) = STATE.get() else { return };
    {
        let mut st = cell.lock();
        if st.latch_enabled == enabled {
            return;
        }
        st.latch_enabled = enabled;
        st.last_up = None;
    }
    tracing::info!(enabled, "hands-free latch setting changed");
}

/// Tell the hook whether the pipeline is currently recording hands-free.
///
/// Set to true the moment a latch starts and false the moment it ends -
/// before any slow work - because while it is set every press is turned into
/// a stop, and a press that arrived during transcription would be swallowed.
pub fn set_latched(latched: bool) {
    let Some(cell) = STATE.get() else { return };
    let mut st = cell.lock();
    st.latched = latched;
}

/// Start reading keystrokes as a chord definition.
///
/// While capturing, every key is swallowed - the user is pressing combinations
/// that would otherwise trigger things, and having the capture field also open
/// the Start menu would be its own bug.
pub fn begin_capture(tx: Sender<CaptureEvent>) {
    let Some(cell) = STATE.get() else { return };
    {
        let mut st = cell.lock();
        st.held.clear();
        st.engaged = false;
        st.capture = Some(Capture { best: Vec::new(), tx });
    }
    tracing::info!("hotkey capture started");
}

/// Abandon capture without changing anything.
pub fn cancel_capture() {
    let Some(cell) = STATE.get() else { return };
    let cancelled = {
        let mut st = cell.lock();
        let was = st.capture.take().is_some();
        if was {
            st.held.clear();
        }
        was
    };
    if cancelled {
        tracing::info!("hotkey capture cancelled");
    }
}

/// Install the hook and pump messages. Blocks forever - call on its own thread.
pub fn run(chord: Chord, latch_enabled: bool, tx: Sender<PttEvent>) -> anyhow::Result<()> {
    let normalized = Chord { keys: chord.keys.iter().copied().map(canonical).collect() };
    let double_window = double_press_window();
    tracing::info!(
        latch_enabled,
        double_press_ms = double_window.as_millis(),
        "hands-free latch configured"
    );

    STATE
        .set(Mutex::new(HookState {
            chord: normalized,
            held: HashSet::new(),
            engaged: false,
            tx,
            capture: None,
            last_up: None,
            double_window,
            latch_enabled,
            latched: false,
            suppress_arm: false,
        }))
        .map_err(|_| anyhow::anyhow!("keyboard hook already installed"))?;

    unsafe {
        // A NULL module handle is correct for a hook proc living in this process.
        let hook: HHOOK = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0)?;
        tracing::info!(?hook, "low-level keyboard hook installed");

        // The hook only fires while this thread pumps messages.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// Runs on the hook thread for every keystroke system-wide. Must stay fast:
/// Windows silently removes hooks that exceed the `LowLevelHooksTimeout`.
unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let Some(cell) = STATE.get() else {
        return CallNextHookEx(None, code, wparam, lparam);
    };

    let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);

    // Ignore anything we synthesised ourselves. `inject` releases the modifiers
    // and then sends Ctrl+V, and those events come straight back through this
    // hook - the Ctrl *down* in particular would re-add VK_CONTROL to `held`
    // and, with Win still physically held, immediately re-engage the chord and
    // start recording again mid-paste.
    if kb.flags.0 & LLKHF_INJECTED.0 != 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let vk = canonical(kb.vkCode);
    let msg = wparam.0 as u32;
    let is_down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
    let is_up = matches!(msg, WM_KEYUP | WM_SYSKEYUP);

    let mut st = cell.lock();

    // --- capture mode: read the chord, let nothing through ------------------
    if st.capture.is_some() {
        if is_down {
            st.held.insert(vk);
        } else if is_up {
            st.held.remove(&vk);
        }
        let current = ordered(&st.held);
        let empty = st.held.is_empty();

        let mut outgoing = None;
        if let Some(cap) = st.capture.as_mut() {
            if current.len() > cap.best.len() {
                cap.best = current.clone();
            }
            if empty {
                let chord = std::mem::take(&mut cap.best);
                let tx = cap.tx.clone();
                st.capture = None;
                if !chord.is_empty() {
                    outgoing = Some((tx, CaptureEvent::Done(chord)));
                }
            } else {
                outgoing = Some((cap.tx.clone(), CaptureEvent::Preview(current.clone())));
            }
        }
        drop(st);

        // Logged outside the lock, like the push-to-talk log below.
        tracing::debug!(vk, is_down, is_up, ?current, empty, "capture saw a key");
        if let Some((tx, event)) = outgoing {
            let _ = tx.send(event);
        }
        // Swallow it. The user is pressing arbitrary combinations; letting them
        // reach other apps would fire whatever they happen to be bound to.
        return LRESULT(1);
    }

    // --- normal mode --------------------------------------------------------
    let mut just_engaged = false;
    let mut sent: Option<PttEvent> = None;
    if is_down {
        st.held.insert(vk);
        if !st.engaged && st.chord.keys.iter().all(|k| st.held.contains(k)) {
            st.engaged = true;
            just_engaged = true;

            let event = if st.latched {
                // Latched: every press means stop, whatever its timing. Reading
                // it as the first half of a fresh double press instead is how
                // "stop" turns into "stop and immediately start again".
                st.suppress_arm = true;
                PttEvent::ToggleLatch
            } else if st.latch_enabled
                && is_double_press(st.last_up, Instant::now(), st.double_window)
            {
                st.suppress_arm = true;
                PttEvent::ToggleLatch
            } else {
                PttEvent::Down
            };
            // Consumed either way: a triple tap is a double tap plus a press,
            // not two latches.
            st.last_up = None;
            let _ = st.tx.send(event);
            sent = Some(event);
        }
    } else if is_up {
        st.held.remove(&vk);
        if st.engaged && !st.chord.keys.iter().all(|k| st.held.contains(k)) {
            st.engaged = false;
            st.last_up = if st.suppress_arm { None } else { Some(Instant::now()) };
            st.suppress_arm = false;
            let _ = st.tx.send(PttEvent::Up);
            sent = Some(PttEvent::Up);
        }
    }
    let uses_win = st.chord.keys.contains(&VK_WIN);
    drop(st);

    // Logged outside the lock: this thread must not hold it while formatting,
    // and Windows silently uninstalls hooks that take too long.
    if let Some(event) = sent {
        tracing::debug!(?event, "push-to-talk");
    }

    if just_engaged && uses_win {
        defeat_start_menu();
    }

    CallNextHookEx(None, code, wparam, lparam)
}

/// Stop the Start menu from opening when a Win-key chord is released.
///
/// Windows opens Start on Win key-up only if no other key was pressed *during*
/// the Win hold. Pressing Ctrl first and Win second does not count, so a plain
/// Ctrl+Win push-to-talk would pop the Start menu every single time.
///
/// The obvious fix - swallowing the Win key-up - is worse than the problem: it
/// desyncs the OS, leaving every other app believing Win is still held down.
/// Injecting a no-op key instead marks the Win press as "used" and lets the
/// real key-up through untouched.
unsafe fn defeat_start_menu() {
    // 0xFC (VK_NONAME) is reserved and does nothing anywhere.
    const VK_NONAME: u16 = 0xFC;

    let ev = |up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(VK_NONAME),
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs = [ev(false), ev(true)];
    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_collapse_to_the_same_canonical_key() {
        assert_eq!(canonical(VK_LCONTROL), VK_CONTROL);
        assert_eq!(canonical(VK_RCONTROL), VK_CONTROL);
        assert_eq!(canonical(VK_LSHIFT), VK_SHIFT);
        assert_eq!(canonical(VK_RSHIFT), VK_SHIFT);
        assert_eq!(canonical(VK_LMENU), VK_MENU);
        assert_eq!(canonical(VK_LWIN), VK_WIN);
        assert_eq!(canonical(VK_RWIN), VK_WIN);
    }

    #[test]
    fn modifiers_do_not_alias_each_other() {
        // The regression this guards: LCONTROL (0xA2) once mapped to VK_SHIFT,
        // which made Ctrl+Win impossible to press.
        assert_ne!(canonical(VK_LCONTROL), canonical(VK_LSHIFT));
        assert_ne!(canonical(VK_LCONTROL), canonical(VK_LMENU));
        assert_ne!(canonical(VK_LSHIFT), canonical(VK_LMENU));
    }

    #[test]
    fn ordinary_keys_pass_through() {
        assert_eq!(canonical(0x41), 0x41); // 'A'
    }

    #[test]
    fn default_chord_is_ctrl_plus_win() {
        let c = Chord::default();
        assert!(c.keys.contains(&VK_CONTROL));
        assert!(c.keys.contains(&VK_WIN));
        assert_eq!(c.keys.len(), 2);
    }

    // --- hands-free latch ---------------------------------------------------

    #[test]
    fn a_quick_second_press_is_a_double_press() {
        let window = Duration::from_millis(400);
        let up = Instant::now();
        let soon = up + Duration::from_millis(150);
        assert!(is_double_press(Some(up), soon, window));
    }

    #[test]
    fn a_slow_second_press_is_not() {
        let window = Duration::from_millis(400);
        let up = Instant::now();
        let later = up + Duration::from_millis(401);
        assert!(!is_double_press(Some(up), later, window));
    }

    #[test]
    fn a_press_with_no_prior_release_is_never_a_double_press() {
        // This is how the detector is disarmed: clearing `last_up` after using
        // it stops a third tap latching again, and stops the release of a
        // latch-stopping press from arming the next one.
        assert!(!is_double_press(None, Instant::now(), Duration::from_millis(400)));
    }

    #[test]
    fn the_double_press_window_is_clamped_at_both_ends() {
        // Windows allows 100ms, which is not hittable with a two-key chord...
        assert_eq!(clamp_double_window(100), DOUBLE_PRESS_MIN);
        // ...and several seconds, which would latch two separate dictations.
        assert_eq!(clamp_double_window(5_000), DOUBLE_PRESS_MAX);
        // The Windows default sits inside the range and is left alone.
        assert_eq!(clamp_double_window(500), Duration::from_millis(500));
    }
}
