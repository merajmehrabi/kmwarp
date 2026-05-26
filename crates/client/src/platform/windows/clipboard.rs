//! Windows clipboard observer + reader/writer for M8.
//!
//! ## Architecture
//!
//! Win32 has two ways to learn about clipboard changes:
//!
//! 1. **`AddClipboardFormatListener`** with a hidden message-only window
//!    that receives `WM_CLIPBOARDUPDATE`. Push-based, lowest latency.
//! 2. **Polling `GetClipboardSequenceNumber`** at some cadence.
//!
//! We use (1) — it's how every native Windows clipboard tool does it and
//! the latency on a paste-after-copy is a perceived UI metric the spec
//! caps at 500 ms.
//!
//! The listener lives on a dedicated OS thread (not a tokio task) because
//! `GetMessageW` blocks, and Windows requires the message pump to live on
//! the thread that owns the window. The thread is a process-wide
//! singleton — re-`install()` replaces the registered sender so a fresh
//! session sees only fresh events. Cross-session "leak" of one OS thread
//! is fine for v1 (no reconnect storms in practice).
//!
//! ## Reading / writing the clipboard
//!
//! Free functions [`read_clipboard_text`] / [`write_clipboard_text`] do
//! their own `OpenClipboard` / `CloseClipboard` pair. Both retry briefly
//! if the clipboard is held by another process (`OpenClipboard` returns
//! Err in that case) — Win32 docs explicitly call this out as a "race
//! with whatever else might be touching the clipboard" hazard.
//!
//! UTF-16 conversion goes through `String::from_utf16_lossy` on the read
//! path (Win32 hands us raw `*const u16`) and `str::encode_utf16` on the
//! write path. The wire protocol is UTF-8 only per spec M8; this layer
//! is the encode/decode boundary.
//!
//! ## Echo suppression
//!
//! Lives in `kmwarp_core::clipboard::EchoGuard` (used at the
//! `clipboard_out_task` layer). This module deliberately doesn't know
//! about it — the out task gates on `is_echo_of_remote(&text)` *before*
//! calling [`write_clipboard_text`] for outbound chunks, and updates
//! `remember_remote_write(&text)` *after* writing inbound payloads.

use std::sync::{Mutex, Once, OnceLock};

use kmwarp_core::ClipboardEvent;
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};
use windows::core::w;
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard,
    SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
    TranslateMessage, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLIPBOARDUPDATE,
    WNDCLASSW,
};

use crate::platform::windows::inject_error::ClipboardError;

/// `CF_UNICODETEXT` clipboard format id (Win32 constant). Hard-coded so
/// we don't depend on which feature flag exposes the
/// `CLIPBOARD_FORMAT` newtype across `windows` crate versions.
const CF_UNICODETEXT: u32 = 13;

/// How many times to retry `OpenClipboard` if another process owns it.
/// Win32 docs explicitly note OpenClipboard can return false on a race;
/// 3 attempts × 10 ms ≈ 30 ms worst-case wait, well under the spec's
/// 500 ms clipboard-propagation budget.
const OPEN_RETRIES: u32 = 3;
const OPEN_RETRY_DELAY_MS: u64 = 10;

/// Process-wide sender into the active listener. Replaced on every
/// [`WinClipboard::install`] so the previous session's `Receiver` (held
/// by the old `WinClipboard`) stops getting fed.
static CLIPBOARD_SENDER: OnceLock<Mutex<Option<mpsc::UnboundedSender<ClipboardEvent>>>> =
    OnceLock::new();

/// Guards the one-shot spawn of the listener OS thread.
static LISTENER_ONCE: Once = Once::new();

/// Async-facing handle returned by [`WinClipboard::install`]. Each
/// session owns one; dropping it is enough to stop receiving events
/// (the singleton listener thread keeps running, but its `try_send` on
/// our replaced sender slot just fails harmlessly).
pub struct WinClipboard {
    rx: mpsc::UnboundedReceiver<ClipboardEvent>,
}

impl WinClipboard {
    /// Install the clipboard listener for this session.
    ///
    /// First call also spawns the process-wide listener thread; further
    /// calls just rotate the active sender. Returns the receiver wrapped
    /// in `WinClipboard`.
    pub fn install() -> Result<Self, ClipboardError> {
        let (tx, rx) = mpsc::unbounded_channel();

        let cell = CLIPBOARD_SENDER.get_or_init(|| Mutex::new(None));
        {
            let mut guard = cell
                .lock()
                .map_err(|e| ClipboardError::Init(format!("sender mutex poisoned: {e}")))?;
            *guard = Some(tx);
        }

        // Spawn the listener exactly once for the process. Subsequent
        // installs just swap the sender slot above.
        LISTENER_ONCE.call_once(|| {
            std::thread::Builder::new()
                .name("kmwarp-clipboard-listener".into())
                .spawn(listener_thread)
                .expect("failed to spawn clipboard listener thread");
        });

        Ok(Self { rx })
    }

    /// Await the next clipboard change. Returns `None` if the sender
    /// half has been replaced (rare — only on `install()` called twice
    /// in fast succession with the old `WinClipboard` still alive) or
    /// the listener thread died.
    pub async fn next_change(&mut self) -> Option<ClipboardEvent> {
        self.rx.recv().await
    }
}

/// Listener thread entry point. Registers a hidden message-only window,
/// installs the clipboard listener, and pumps messages forever.
fn listener_thread() {
    debug!("clipboard listener thread starting");
    // SAFETY: `GetModuleHandleW(None)` returns the EXE's instance handle;
    // pure FFI with no aliasing concerns.
    let hinstance = match unsafe { GetModuleHandleW(None) } {
        Ok(h) => h,
        Err(e) => {
            warn!(error = %e, "GetModuleHandleW failed; clipboard listener disabled");
            return;
        }
    };

    let class_name = w!("kmwarp_clipboard_listener");
    let window_name = w!("kmwarp clipboard");

    let wnd_class = WNDCLASSW {
        lpfnWndProc: Some(clipboard_wnd_proc),
        hInstance: windows::Win32::Foundation::HINSTANCE(hinstance.0),
        lpszClassName: class_name,
        ..Default::default()
    };
    // SAFETY: `wnd_class` is valid for the duration of the call.
    let atom = unsafe { RegisterClassW(&wnd_class) };
    if atom == 0 {
        let err = windows::core::Error::from_win32();
        warn!(%err, "RegisterClassW failed; clipboard listener disabled");
        return;
    }

    // HWND_MESSAGE parent makes this a "message-only" window — invisible,
    // never appears in alt-tab, never receives input events, but can
    // receive broadcast / posted messages.
    // SAFETY: pure FFI; class name lives for static program lifetime.
    let hwnd = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            window_name,
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            None,
            windows::Win32::Foundation::HINSTANCE(hinstance.0),
            None,
        )
    } {
        Ok(h) => h,
        Err(e) => {
            warn!(error = %e, "CreateWindowExW failed; clipboard listener disabled");
            return;
        }
    };

    // SAFETY: `hwnd` is the window we just created; valid for the
    // lifetime of this thread.
    if let Err(e) = unsafe { AddClipboardFormatListener(hwnd) } {
        warn!(error = %e, "AddClipboardFormatListener failed; clipboard listener disabled");
        return;
    }

    debug!("clipboard listener installed; entering message loop");

    let mut msg = MSG::default();
    // SAFETY: `GetMessageW` only fails on bad pointers; ours are valid.
    while unsafe { GetMessageW(&mut msg, hwnd, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    debug!("clipboard listener thread exiting");
}

/// Window proc for the message-only listener window. Called by Windows
/// on the listener thread for every dispatched message.
///
/// On `WM_CLIPBOARDUPDATE` we read the current clipboard text and push
/// it through the current `CLIPBOARD_SENDER`. Everything else falls
/// through to `DefWindowProcW`.
extern "system" fn clipboard_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CLIPBOARDUPDATE {
        if let Some(text) = read_clipboard_text() {
            if let Some(cell) = CLIPBOARD_SENDER.get() {
                if let Ok(guard) = cell.lock() {
                    if let Some(tx) = guard.as_ref() {
                        // `send` on an unbounded channel only errors when
                        // the receiver is dropped — that just means the
                        // session ended; not worth logging at warn.
                        let _ = tx.send(ClipboardEvent::TextChanged(text));
                    }
                }
            }
        }
        return LRESULT(0);
    }
    // SAFETY: standard wndproc fallback.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Read the current `CF_UNICODETEXT` clipboard content as a Rust `String`.
///
/// Returns `None` if:
/// - the clipboard couldn't be opened after [`OPEN_RETRIES`] retries,
/// - it doesn't carry `CF_UNICODETEXT` (e.g. an image-only copy),
/// - or the lock / pointer dereference failed.
///
/// Errors are logged at `trace`/`warn` and swallowed — clipboard reads
/// are best-effort and shouldn't tear down the session.
pub fn read_clipboard_text() -> Option<String> {
    if !open_clipboard_with_retry() {
        return None;
    }

    // SAFETY: ownership-style RAII would be nicer; for now we manually
    // CloseClipboard on every exit path via the early-return helpers.
    let result = unsafe { read_clipboard_text_locked() };
    // SAFETY: paired with the preceding open.
    let _ = unsafe { CloseClipboard() };
    result
}

/// Caller must hold the clipboard open. Always close it after this returns.
unsafe fn read_clipboard_text_locked() -> Option<String> {
    let handle = match GetClipboardData(CF_UNICODETEXT) {
        Ok(h) => h,
        Err(e) => {
            trace!(error = %e, "GetClipboardData(CF_UNICODETEXT) failed (no text on clipboard?)");
            return None;
        }
    };
    if handle.is_invalid() {
        return None;
    }

    // GlobalLock returns a non-null pointer to the locked memory or null
    // on failure. CF_UNICODETEXT data is a null-terminated wide string.
    let hglobal = windows::Win32::Foundation::HGLOBAL(handle.0);
    let ptr = GlobalLock(hglobal) as *const u16;
    if ptr.is_null() {
        warn!("GlobalLock returned null for clipboard handle");
        return None;
    }

    // Walk to the terminating NUL to bound the slice. Allocated buffer
    // size from GlobalSize lets us cap the walk so a malformed clipboard
    // entry can't run off the end.
    let cap_bytes = GlobalSize(hglobal);
    let cap_u16 = cap_bytes / 2;
    let mut len = 0usize;
    while len < cap_u16 && *ptr.add(len) != 0 {
        len += 1;
    }
    let slice = std::slice::from_raw_parts(ptr, len);
    let text = String::from_utf16_lossy(slice);

    let _ = GlobalUnlock(hglobal);
    Some(text)
}

/// Write `text` to the clipboard as `CF_UNICODETEXT`. UTF-8 → UTF-16
/// encoding happens here.
///
/// On success the system takes ownership of the allocated `HGLOBAL`
/// (per `SetClipboardData` contract); do not `GlobalFree` after.
pub fn write_clipboard_text(text: &str) -> Result<(), ClipboardError> {
    if !open_clipboard_with_retry() {
        return Err(ClipboardError::Write(
            "OpenClipboard failed after retries".into(),
        ));
    }
    // SAFETY: clipboard is held open until the explicit CloseClipboard
    // below. The inner helper is responsible for SetClipboardData's
    // ownership-transfer semantics.
    let result = unsafe { write_clipboard_text_locked(text) };
    // SAFETY: paired with open.
    let _ = unsafe { CloseClipboard() };
    result
}

/// Caller must hold the clipboard open. Always close after.
unsafe fn write_clipboard_text_locked(text: &str) -> Result<(), ClipboardError> {
    // Always empty the clipboard before SetClipboardData — Win32 docs
    // say SetClipboardData fails if EmptyClipboard hasn't been called
    // since the window opened the clipboard.
    if let Err(e) = EmptyClipboard() {
        return Err(ClipboardError::Write(format!("EmptyClipboard: {e}")));
    }

    // UTF-16 + trailing NUL. Encode into a Vec<u16> then copy into a
    // GMEM_MOVEABLE-allocated buffer that the OS will own.
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0u16)).collect();
    let byte_size = wide.len() * std::mem::size_of::<u16>();

    let hglobal = GlobalAlloc(GMEM_MOVEABLE, byte_size)
        .map_err(|e| ClipboardError::Write(format!("GlobalAlloc({byte_size}): {e}")))?;
    let dst = GlobalLock(hglobal) as *mut u16;
    if dst.is_null() {
        return Err(ClipboardError::Write("GlobalLock returned null".into()));
    }
    std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
    let _ = GlobalUnlock(hglobal);

    // SetClipboardData wants a HANDLE; HGLOBAL is a thin newtype around
    // a pointer in windows-rs, so wrap by value.
    let handle = HANDLE(hglobal.0);
    if let Err(e) = SetClipboardData(CF_UNICODETEXT, handle) {
        // On error WE still own the memory and must free it. On success
        // the system takes ownership.
        let _ = windows::Win32::Foundation::GlobalFree(hglobal);
        return Err(ClipboardError::Write(format!("SetClipboardData: {e}")));
    }
    Ok(())
}

/// Try to `OpenClipboard(None)`. Returns `true` on success. Retries a
/// few times with a short sleep — another process may briefly hold the
/// global clipboard lock during a copy/paste.
fn open_clipboard_with_retry() -> bool {
    for attempt in 0..OPEN_RETRIES {
        // SAFETY: pure FFI; we pair every Ok with a CloseClipboard.
        if unsafe { OpenClipboard(None) }.is_ok() {
            return true;
        }
        if attempt + 1 < OPEN_RETRIES {
            std::thread::sleep(std::time::Duration::from_millis(OPEN_RETRY_DELAY_MS));
        }
    }
    let err = windows::core::Error::from_win32();
    warn!(%err, "OpenClipboard failed after retries");
    false
}
