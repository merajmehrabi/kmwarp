//! Win32 native input dialog for the SPAKE2 pairing code.
//!
//! v1.0 read the 6-digit code from stdin. v1.1 ships the client as a
//! tray app (`kmwarp-client.exe run` hosts a Shell_NotifyIconW surface
//! with no console attached), so the first-pairing flow needs a GUI
//! prompt instead. This module is that prompt: a small modal popup
//! with a numeric Edit control + OK / Cancel.
//!
//! ## Threading model
//!
//! Win32 dialog APIs run a blocking message pump on the calling
//! thread. To stay tokio-friendly, [`ask_pairing_code`] is `async`
//! and hands the actual dialog work off via
//! [`tokio::task::spawn_blocking`]; the result is delivered back
//! through a [`tokio::sync::oneshot`] channel. The blocking task
//! runs on tokio's dedicated blocking pool, so no runtime worker is
//! parked while the user is typing.
//!
//! A future polish pass may switch to a `WM_USER`-style hand-off
//! onto the tray's existing message pump (better UX — the popup
//! would inherit the tray's window context), but that requires
//! cross-module coordination that's out of scope for the standalone
//! skeleton landed alongside task #14.
//!
//! ## Window-class registration
//!
//! The dialog uses a process-wide custom window class named
//! `kmwarp_pairing_dialog`. Registration is guarded by a [`OnceLock`]
//! so repeated `ask_pairing_code` invocations cost nothing after the
//! first. The class is never unregistered — the cost is one global
//! `WNDCLASSW` for the lifetime of the process.
//!
//! ## Validation
//!
//! On OK click (or Enter, via [`IsDialogMessageW`]), the Edit
//! contents are trimmed and required to be **exactly six ASCII
//! digits**. Empty / non-numeric / wrong-length inputs return an
//! `Err`; the dialog closes on submit either way. (A nicer UX would
//! keep the dialog open with an inline error label — left for
//! follow-up once the wiring lands in task #13 / #16.)
//!
//! On Cancel click (or Esc, via [`IsDialogMessageW`]), the function
//! returns `Err("pairing cancelled by user")`. The same error is
//! returned if the user closes the window via the title-bar X.

#![cfg(target_os = "windows")]

use std::sync::OnceLock;

use anyhow::{anyhow, bail, Context, Result};
use tokio::sync::oneshot;
use tracing::{debug, info, warn};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::EM_SETLIMITTEXT;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, IsDialogMessageW, LoadCursorW,
    PostQuitMessage, RegisterClassW, SendMessageW, SetForegroundWindow, SetWindowLongPtrW,
    ShowWindow, TranslateMessage, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CREATESTRUCTW, CW_USEDEFAULT,
    ES_AUTOHSCROLL, ES_NUMBER, GWLP_USERDATA, HMENU, IDC_ARROW, IDCANCEL, IDOK, MSG, SW_SHOW,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY, WNDCLASSW,
    WS_BORDER, WS_CAPTION, WS_CHILD, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

/// Expected SPAKE2 code length per the pairing spec.
const EXPECTED_CODE_LEN: usize = 6;

/// Window-class name for the popup. Kept short + namespaced so it
/// doesn't collide with anything else in the process.
const DIALOG_CLASS_NAME: PCWSTR = w!("kmwarp_pairing_dialog");

/// Child-control IDs. Win32 dispatches `WM_COMMAND` with the ID in
/// the low word of `wParam`; the OK/Cancel IDs deliberately reuse
/// the standard `IDOK` / `IDCANCEL` constants so `IsDialogMessageW`
/// auto-translates Enter → OK and Esc → Cancel.
const ID_EDIT: isize = 1001;

/// Dialog dimensions (client area, px at 96 DPI). The tray's DPI
/// awareness covers the popup too — Win11 scales automatically.
const DIALOG_W: i32 = 340;
const DIALOG_H: i32 = 180;

/// Outcome of one dialog session, owned by the calling thread's
/// stack frame and addressed from the WNDPROC via `GWLP_USERDATA`.
struct DialogState {
    /// Filled by the WNDPROC on OK/Cancel/Close; read by
    /// [`show_dialog_blocking`] after the message loop exits.
    result: Option<Result<String>>,
    /// Handle to the Edit control. Captured during `WM_CREATE` so
    /// `WM_COMMAND` handlers can read its text without re-finding
    /// the child by ID.
    edit_hwnd: HWND,
}

/// Open a modal pairing-code dialog and return the entered code.
///
/// Returns:
/// - `Ok(code)` — exactly six ASCII digits, validated.
/// - `Err(_)` — user cancelled (Cancel button, Esc, or title-bar X),
///   the input failed validation, or the dialog couldn't be created.
///
/// Safe to call multiple times in one process; the window class is
/// registered exactly once.
pub async fn ask_pairing_code() -> Result<String> {
    let (tx, rx) = oneshot::channel::<Result<String>>();
    tokio::task::spawn_blocking(move || {
        let outcome = show_dialog_blocking();
        // Receiver hung up if `tx.send` errors — log at debug and
        // discard, the caller already gave up on us.
        if tx.send(outcome).is_err() {
            debug!("pairing dialog completed but caller dropped the oneshot");
        }
    });
    rx.await
        .context("pairing dialog blocking task panicked or was aborted")?
}

/// Synchronous worker that creates the popup and pumps messages
/// until the user clicks OK/Cancel or closes the window. Always
/// runs on a tokio blocking-pool thread.
fn show_dialog_blocking() -> Result<String> {
    register_class_once()?;

    // SAFETY: pure FFI; pairs with our own class registration.
    let hinstance = unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW failed")?;

    let mut state = DialogState {
        result: None,
        edit_hwnd: HWND::default(),
    };
    let state_ptr: *mut DialogState = &mut state;

    let style = WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_BORDER | WS_VISIBLE;
    // SAFETY: parent/menu null (top-level popup); hinstance is from
    // GetModuleHandleW above; lpparam is our stack-local state ptr
    // which outlives the message loop below.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            DIALOG_CLASS_NAME,
            w!("kmwarp pairing"),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            DIALOG_W,
            DIALOG_H,
            None,
            None,
            HINSTANCE(hinstance.0),
            Some(state_ptr.cast()),
        )
    }
    .context("CreateWindowExW for pairing dialog failed")?;

    // Best-effort foreground promotion. Win32 only allows this in
    // certain contexts (the calling process must own the active
    // window, etc.); failure is harmless — the popup is still
    // visible in the taskbar.
    unsafe {
        let _ = SetForegroundWindow(hwnd);
        let _ = ShowWindow(hwnd, SW_SHOW);
    }

    info!("pairing dialog shown");

    pump_messages(hwnd);

    state
        .result
        .unwrap_or_else(|| Err(anyhow!("pairing cancelled by user")))
}

/// Standard dialog message pump. `IsDialogMessageW` handles Tab
/// navigation, Enter → default button, Esc → Cancel; everything
/// else falls through to `Translate`/`Dispatch`.
fn pump_messages(hwnd: HWND) {
    let mut msg = MSG::default();
    loop {
        // SAFETY: pure FFI; `msg` is valid for the call.
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        // Returns 0 on WM_QUIT, -1 on error, >0 otherwise.
        if r.0 <= 0 {
            break;
        }
        // SAFETY: `msg` was populated by the just-completed GetMessageW;
        // `hwnd` is the dialog we own.
        let dlg_handled = unsafe { IsDialogMessageW(hwnd, &msg) };
        if !dlg_handled.as_bool() {
            // SAFETY: standard message-pump pattern.
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

/// Process-wide cache of the class-registration outcome.
static REGISTER_RESULT: OnceLock<Result<(), String>> = OnceLock::new();

fn register_class_once() -> Result<()> {
    let outcome = REGISTER_RESULT.get_or_init(do_register_class);
    match outcome {
        Ok(()) => Ok(()),
        Err(msg) => Err(anyhow!("RegisterClassW for pairing dialog: {msg}")),
    }
}

fn do_register_class() -> Result<(), String> {
    // SAFETY: pure FFI calls; class data lives for program lifetime.
    let hinstance = unsafe { GetModuleHandleW(None) }.map_err(|e| e.to_string())?;
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }
        .map_err(|e| format!("LoadCursorW: {e}"))?;
    let wnd_class = WNDCLASSW {
        lpfnWndProc: Some(dialog_wnd_proc),
        hInstance: HINSTANCE(hinstance.0),
        hCursor: cursor,
        lpszClassName: DIALOG_CLASS_NAME,
        ..Default::default()
    };
    // SAFETY: `wnd_class` lives until RegisterClassW returns.
    let atom = unsafe { RegisterClassW(&wnd_class) };
    if atom == 0 {
        return Err(windows::core::Error::from_win32().to_string());
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────
// Window procedure
// ──────────────────────────────────────────────────────────────────

extern "system" fn dialog_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            // SAFETY: at WM_CREATE, lparam points to a valid CREATESTRUCTW
            // whose lpCreateParams field is the void* we passed to
            // CreateWindowExW (our DialogState pointer). Both are valid
            // for the duration of the call.
            let create_struct = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
            let state_ptr = create_struct.lpCreateParams as *mut DialogState;
            // SAFETY: hwnd is the freshly-created window; we own it.
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
            }
            build_children(hwnd, state_ptr);
            LRESULT(0)
        }
        WM_COMMAND => {
            let ctrl_id = (wparam.0 & 0xFFFF) as i32;
            handle_command(hwnd, ctrl_id);
            LRESULT(0)
        }
        WM_CLOSE => {
            // Title-bar X. State.result stays None → caller sees
            // "pairing cancelled by user". DestroyWindow triggers
            // WM_DESTROY → PostQuitMessage → pump exit.
            // SAFETY: hwnd is our window.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: bounded-effect FFI.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: standard default handler fallback.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Allocate the static label, edit, and OK/Cancel buttons.
/// Geometry is in client-area pixels at 96 DPI; the WM_CREATE caller
/// runs on the thread that registered the class.
fn build_children(parent: HWND, state_ptr: *mut DialogState) {
    // SAFETY: pure FFI.
    let Ok(hinstance) = (unsafe { GetModuleHandleW(None) }) else {
        warn!("pairing dialog: GetModuleHandleW failed in WM_CREATE");
        return;
    };
    let hinst = HINSTANCE(hinstance.0);

    // Static label
    // SAFETY: `STATIC` is a preregistered Win32 window class; parent
    // hwnd is the freshly-created dialog; hinst is the module handle.
    let _label = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("Enter the 6-digit code from your Mac:"),
            WS_CHILD | WS_VISIBLE,
            12,
            14,
            300,
            18,
            parent,
            None,
            hinst,
            None,
        )
    };

    // Numeric edit
    let edit_style =
        WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | bits(ES_NUMBER) | bits(ES_AUTOHSCROLL);
    // SAFETY: hmenu carries the control ID for WM_COMMAND dispatch;
    // we encode ID_EDIT as a fake HMENU per Win32 convention.
    match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            PCWSTR::null(),
            edit_style,
            12,
            38,
            316,
            26,
            parent,
            HMENU(ID_EDIT as *mut std::ffi::c_void),
            hinst,
            None,
        )
    } {
        Ok(edit) => {
            // Cap entry at six characters. EM_SETLIMITTEXT only
            // constrains keyboard entry, not WM_SETTEXT — fine for
            // our use (the user types the code in).
            // SAFETY: edit is valid; constants are well-defined.
            unsafe {
                SendMessageW(edit, EM_SETLIMITTEXT, WPARAM(EXPECTED_CODE_LEN), LPARAM(0));
                let _ = SetFocus(edit);
            }
            if !state_ptr.is_null() {
                // SAFETY: state_ptr is the live caller stack-frame
                // address; we hold the only mutable reference during
                // dispatch (single-threaded WNDPROC).
                unsafe { (*state_ptr).edit_hwnd = edit };
            }
        }
        Err(e) => warn!(error = %e, "pairing dialog: failed to create Edit control"),
    }

    let btn_style = WS_CHILD | WS_VISIBLE | WS_TABSTOP;
    let ok_style = btn_style | bits(BS_DEFPUSHBUTTON);
    let cancel_style = btn_style | bits(BS_PUSHBUTTON);

    // SAFETY: same notes as above; IDOK/IDCANCEL come from the
    // standard MESSAGEBOX_RESULT constants so IsDialogMessageW maps
    // Enter → OK and Esc → Cancel automatically.
    let _ = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            w!("OK"),
            ok_style,
            156,
            90,
            80,
            28,
            parent,
            HMENU(IDOK.0 as isize as *mut std::ffi::c_void),
            hinst,
            None,
        )
    };
    let _ = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            w!("Cancel"),
            cancel_style,
            246,
            90,
            80,
            28,
            parent,
            HMENU(IDCANCEL.0 as isize as *mut std::ffi::c_void),
            hinst,
            None,
        )
    };
}

/// Cast a Win32 `i32` style constant into the typed `WINDOW_STYLE`
/// newtype. The underlying bit pattern is identical; this helper
/// just avoids repeating the cast at every call site.
fn bits(style_const: i32) -> WINDOW_STYLE {
    WINDOW_STYLE(style_const as u32)
}

/// `WM_COMMAND` dispatch. OK validates and stores result; Cancel
/// stores a sentinel error. Both then destroy the window which
/// posts WM_QUIT and unblocks the message pump.
fn handle_command(hwnd: HWND, ctrl_id: i32) {
    // SAFETY: GWLP_USERDATA was set in WM_CREATE; the pointer
    // points to the caller's stack frame which outlives the loop.
    let state_ptr =
        unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut DialogState;
    if state_ptr.is_null() {
        return;
    }

    if ctrl_id == IDOK.0 {
        // SAFETY: state_ptr is the only live reference (WNDPROC is
        // single-threaded; we read edit_hwnd, then write result).
        let edit = unsafe { (*state_ptr).edit_hwnd };
        let typed = read_edit_text(edit);
        let result = validate_code(&typed);
        // SAFETY: same.
        unsafe { (*state_ptr).result = Some(result) };
        unsafe { let _ = DestroyWindow(hwnd); }
    } else if ctrl_id == IDCANCEL.0 {
        // SAFETY: same.
        unsafe {
            (*state_ptr).result = Some(Err(anyhow!("pairing cancelled by user")));
        }
        // SAFETY: hwnd is our dialog.
        unsafe { let _ = DestroyWindow(hwnd); }
    }
    // Any other ctrl_id (notifications from the Edit etc.) ignored.
}

/// Pull text from an Edit control into a Rust `String`. Returns an
/// empty string on any error — validation downstream rejects empty.
fn read_edit_text(edit: HWND) -> String {
    if edit.is_invalid() {
        return String::new();
    }
    // SAFETY: pure FFI on a valid HWND.
    let len = unsafe { GetWindowTextLengthW(edit) };
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; (len as usize) + 1];
    // SAFETY: buffer is sized to `len + 1` for the trailing NUL.
    let copied = unsafe { GetWindowTextW(edit, &mut buf) };
    let copied = copied.max(0) as usize;
    String::from_utf16_lossy(&buf[..copied])
}

/// Enforce the SPAKE2 contract: exactly six ASCII digits, no
/// whitespace, no separators. `ES_NUMBER` already blocks non-digit
/// typing on the Edit control, but defence in depth — paste / IME
/// can still smuggle weird input through.
fn validate_code(text: &str) -> Result<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        bail!("pairing code is empty");
    }
    if trimmed.len() != EXPECTED_CODE_LEN {
        bail!(
            "pairing code must be exactly {EXPECTED_CODE_LEN} digits (got {} characters)",
            trimmed.chars().count()
        );
    }
    if !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        bail!("pairing code must be all ASCII digits");
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_six_digits() {
        assert_eq!(validate_code("123456").unwrap(), "123456");
    }

    #[test]
    fn validate_trims_whitespace() {
        assert_eq!(validate_code("  123456 \n").unwrap(), "123456");
    }

    #[test]
    fn validate_rejects_empty() {
        assert!(validate_code("").is_err());
        assert!(validate_code("   ").is_err());
    }

    #[test]
    fn validate_rejects_short() {
        assert!(validate_code("12345").is_err());
    }

    #[test]
    fn validate_rejects_long() {
        assert!(validate_code("1234567").is_err());
    }

    #[test]
    fn validate_rejects_non_digit() {
        assert!(validate_code("12345A").is_err());
        assert!(validate_code("12-456").is_err());
    }

    #[test]
    fn validate_rejects_unicode_digits() {
        // Devanagari "6" is U+096C — looks digit-ish but not ASCII.
        assert!(validate_code("12345६").is_err());
    }

    #[test]
    fn expected_len_is_six() {
        assert_eq!(EXPECTED_CODE_LEN, 6);
    }
}
