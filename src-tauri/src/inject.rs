//! Getting text into whatever app currently has focus.
//!
//! Two approaches exist and only one of them actually works everywhere.
//! Synthesising the text as individual keystrokes loses characters in apps
//! that throttle input, mangles IME state, and is slow for long passages.
//! Clipboard + Ctrl+V is what every shipping dictation tool converges on: it
//! is atomic, instant regardless of length, and works in Electron apps,
//! terminals, browsers, and native controls alike.
//!
//! The cost is that we borrow the user's clipboard, so we put it back.

use anyhow::{anyhow, Context, Result};
use std::time::Duration;

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
};

/// Time given to the target app to service the paste before we restore the
/// clipboard. Too short and slow apps (Electron, remote desktops) paste stale
/// content; too long and the user notices their clipboard flicker.
const PASTE_SETTLE: Duration = Duration::from_millis(120);

/// Paste `text` into the focused window, leaving the clipboard as we found it.
pub fn paste(text: &str) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }

    // The chord's other keys may still be down when we get here: the hook
    // reports Up as soon as any one key of the chord is released, and a
    // hands-free stop arrives with the whole chord held. Sending Ctrl+V on top
    // of a held Win would fire Win+Ctrl+V instead of pasting.
    release_modifiers()?;

    let saved = get_clipboard_text().unwrap_or(None);
    set_clipboard_text(text).context("failed to place transcript on clipboard")?;

    send_ctrl_v().context("failed to synthesise Ctrl+V")?;
    std::thread::sleep(PASTE_SETTLE);

    if let Some(prev) = saved {
        // Only put the old contents back if ours is still what is there.
        //
        // The settle above is a window in which the user can perfectly well
        // press Ctrl+C, and restoring blindly would throw away whatever they
        // just deliberately copied. Leaving a transcript on the clipboard is a
        // mild annoyance; silently eating a copy is data loss, so anything
        // other than "still ours" means hands off - including a read that
        // fails, since `with_clipboard` already retries and a failure past
        // that is not a state to guess about.
        match get_clipboard_text() {
            Ok(Some(current)) if current == text => {
                let _ = set_clipboard_text(&prev);
            }
            Ok(_) => tracing::debug!("clipboard changed during paste; leaving it as the user left it"),
            Err(e) => tracing::debug!(error = %e, "could not read the clipboard back; not restoring"),
        }
    }
    Ok(())
}

/// Synthesise key-up for any modifier that is *actually* still held.
///
/// Releasing all of them unconditionally looks harmless and is not. A key-up
/// with no matching key-down is still an event, and Windows reads a lone Alt
/// release as "Alt was tapped" - which activates the menu bar. In Notepad that
/// lights up the KeyTip hints (F, E, V...) and eats the Ctrl+V that follows, so
/// dictation silently pasted nothing. Apps that ignore a stray Alt, like Word
/// and VS Code, worked fine, which made it look app-specific rather than ours.
///
/// Querying the real key state first means we only ever send a release for
/// something genuinely down, so the common case sends nothing at all.
fn release_modifiers() -> Result<()> {
    const MODIFIERS: [VIRTUAL_KEY; 5] = [VK_CONTROL, VK_SHIFT, VK_MENU, VK_LWIN, VK_RWIN];

    let held: Vec<INPUT> = MODIFIERS
        .iter()
        .filter(|k| {
            // High bit set means the key is currently down.
            unsafe { GetAsyncKeyState(k.0 as i32) as u16 & 0x8000 != 0 }
        })
        .map(|k| key_input(*k, true))
        .collect();

    if held.is_empty() {
        return Ok(());
    }
    tracing::debug!(count = held.len(), "releasing modifiers still held before paste");
    send(&held)
}

fn send_ctrl_v() -> Result<()> {
    const VK_V: VIRTUAL_KEY = VIRTUAL_KEY(0x56);
    let inputs = [
        key_input(VK_CONTROL, false),
        key_input(VK_V, false),
        key_input(VK_V, true),
        key_input(VK_CONTROL, true),
    ];
    send(&inputs)
}

fn key_input(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> Result<()> {
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        return Err(anyhow!("SendInput sent {sent}/{} events", inputs.len()));
    }
    Ok(())
}

/// The clipboard is a single system-wide resource that any app can hold, so
/// opening it is retried rather than treated as fatal on first failure.
fn with_clipboard<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    const ATTEMPTS: usize = 10;
    for attempt in 0..ATTEMPTS {
        // A null owner ties the clipboard to the current task, which is what
        // we want - we are only borrowing it for the length of one paste.
        let opened = unsafe { OpenClipboard(HWND::default()) };
        if opened.is_ok() {
            let out = f();
            unsafe {
                let _ = CloseClipboard();
            }
            return out;
        }
        std::thread::sleep(Duration::from_millis(10 * (attempt as u64 + 1)));
    }
    Err(anyhow!("could not open clipboard - another app is holding it"))
}

fn get_clipboard_text() -> Result<Option<String>> {
    with_clipboard(|| unsafe {
        let handle = match GetClipboardData(CF_UNICODETEXT.0 as u32) {
            Ok(h) if !h.is_invalid() => h,
            // Non-text clipboard content (an image, files) - nothing to save.
            _ => return Ok(None),
        };
        let ptr = GlobalLock(windows::Win32::Foundation::HGLOBAL(handle.0)) as *const u16;
        if ptr.is_null() {
            return Ok(None);
        }
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
        let _ = GlobalUnlock(windows::Win32::Foundation::HGLOBAL(handle.0));
        Ok(Some(text))
    })
}

fn set_clipboard_text(text: &str) -> Result<()> {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    let bytes = wide.len() * std::mem::size_of::<u16>();

    with_clipboard(|| unsafe {
        EmptyClipboard().context("EmptyClipboard failed")?;

        // Must be GMEM_MOVEABLE: SetClipboardData takes ownership of the block
        // and the system frees it later, so this is never freed here.
        let hglobal = GlobalAlloc(GMEM_MOVEABLE, bytes).context("GlobalAlloc failed")?;
        let dst = GlobalLock(hglobal) as *mut u16;
        if dst.is_null() {
            return Err(anyhow!("GlobalLock failed"));
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
        let _ = GlobalUnlock(hglobal);

        SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(hglobal.0))
            .context("SetClipboardData failed")?;
        Ok(())
    })
}

/// Title of the focused window. Useful for logs; too unstable to drive
/// behaviour, since titles are whatever the app decides to put there.
pub fn foreground_window_title() -> Option<String> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let mut buf = [0u16; 512];
        let len = GetWindowTextW(hwnd, &mut buf);
        if len <= 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// Executable name of the focused window's process, lowercased and without the
/// `.exe` suffix - e.g. `"code"`, `"windowsterminal"`, `"outlook"`.
///
/// This drives per-app tone. The window *title* would be the easy thing to
/// match on and the wrong one: titles change with the open document, get
/// localised, and collide across apps. The process name is stable.
pub fn foreground_process() -> Option<String> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }

        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }

        // LIMITED_INFORMATION is the least privilege that can still read an
        // image name, and unlike PROCESS_QUERY_INFORMATION it succeeds against
        // processes at a higher integrity level.
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;

        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        ok.ok()?;

        let full = String::from_utf16_lossy(&buf[..len as usize]);
        let file = full.rsplit(['\\', '/']).next()?;
        Some(file.trim_end_matches(".exe").trim_end_matches(".EXE").to_lowercase())
    }
}
