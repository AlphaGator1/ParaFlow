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

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VIRTUAL_KEY,
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
    /// Chord pressed twice quickly - latch into hands-free until the next press.
    /// Reserved for the hands-free milestone; the hook does not emit it yet.
    #[allow(dead_code)]
    ToggleLatch,
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

struct HookState {
    chord: Chord,
    held: HashSet<u32>,
    /// True while the full chord is engaged, so we only emit one Down per press.
    engaged: bool,
    tx: Sender<PttEvent>,
}

static STATE: OnceCell<Mutex<HookState>> = OnceCell::new();

/// Install the hook and pump messages. Blocks forever - call on its own thread.
pub fn run(chord: Chord, tx: Sender<PttEvent>) -> anyhow::Result<()> {
    let normalized = Chord { keys: chord.keys.iter().copied().map(canonical).collect() };

    STATE
        .set(Mutex::new(HookState {
            chord: normalized,
            held: HashSet::new(),
            engaged: false,
            tx,
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

    let mut st = cell.lock();
    let mut just_engaged = false;

    match msg {
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            st.held.insert(vk);
            if !st.engaged && st.chord.keys.iter().all(|k| st.held.contains(k)) {
                st.engaged = true;
                just_engaged = true;
                let _ = st.tx.send(PttEvent::Down);
            }
        }
        WM_KEYUP | WM_SYSKEYUP => {
            st.held.remove(&vk);
            if st.engaged && !st.chord.keys.iter().all(|k| st.held.contains(k)) {
                st.engaged = false;
                let _ = st.tx.send(PttEvent::Up);
            }
        }
        _ => {}
    }
    let uses_win = st.chord.keys.contains(&VK_WIN);
    drop(st);

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
}
