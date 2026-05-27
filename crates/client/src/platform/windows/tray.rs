//! Windows system-tray status icon for `kmwarp-client`.
//!
//! Mirrors the macOS menubar pattern from `crates/server/src/service/menubar.rs`:
//! a single status entry that reflects the live [`ClientStatus`] and exposes
//! a one-shot "Quit kmwarp" action.
//!
//! ## Threading model
//!
//! The `tray-icon` crate creates a hidden message-only window during
//! [`TrayIconBuilder::build`] and routes Shell_NotifyIconW callbacks and menu
//! clicks through that window's WNDPROC. Win32 requires a thread pumping
//! `GetMessage` / `PeekMessage` on the thread that owns the window. This
//! module's [`run_on_main_thread`] takes over the calling thread for the
//! process's lifetime and pumps messages at ~30 Hz so the tokio
//! [`watch::Receiver`] can be polled on the same loop without blocking on
//! `GetMessage`.
//!
//! ## ClientStatus source
//!
//! The [`ClientStatus`] enum is defined in [`crate::app`] (added by task #9
//! in the same v1.1 series). The tray needs only a `Clone + PartialEq`
//! enum it can borrow from a watch channel; rendering is done by
//! [`status_glyph_and_short`] / [`status_label`] inside this module so the
//! app layer doesn't have to grow a Display impl on its lifecycle type.
//!
//! ## Quit
//!
//! The "Quit kmwarp" menu item invokes the user-supplied `on_quit` closure
//! (typically: signal the tokio runtime to tear down) and then sleeps a
//! short grace period before calling `std::process::exit(0)`. The grace
//! matches the macOS menubar handler — gives the runtime a beat to flush
//! logs and close sockets before the process vanishes.

#![cfg(target_os = "windows")]

use std::thread::sleep;
use std::time::Duration;

use tokio::sync::watch;
use tracing::{debug, info, warn};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, TrayIcon, TrayIconBuilder,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
};

use crate::app::ClientStatus;

/// Cadence for the message-pump tick and watch-receiver poll. 33 ms
/// ≈ 30 Hz keeps tooltip updates "perceptually instant" (sub-frame) while
/// the idle CPU cost stays trivial — the loop body is just a PeekMessage,
/// a watch borrow, and a try_recv.
const TICK_INTERVAL_MS: u64 = 33;

/// Grace window between `on_quit` returning and `ExitProcess(0)`. Mirrors
/// the macOS menubar pattern so the runtime tear-down has time to flush
/// logs and close sockets cleanly.
const QUIT_GRACE_MS: u64 = 500;

/// Tray icon dimensions. 16x16 is the canonical Shell_NotifyIconW size on
/// Windows; the system upscales for HiDPI taskbars.
const ICON_W: u32 = 16;
const ICON_H: u32 = 16;

/// Flat icon colour (RGBA, sRGB). Soft blue — visually distinct from the
/// stock Windows system icons without being attention-grabbing. The status
/// glyph in the tooltip carries the live state; the icon stays constant.
const ICON_RGBA: [u8; 4] = [0x55, 0x9C, 0xFF, 0xFF];

/// Take over the calling thread, build the tray icon, and drive the Win32
/// message pump forever.
///
/// This call **never returns**. The pump exits only via the "Quit kmwarp"
/// menu item, which runs `on_quit`, sleeps [`QUIT_GRACE_MS`], and calls
/// `std::process::exit(0)`. The `!` return type makes that contract
/// explicit.
///
/// `rx` is the runtime-side broadcast of `ClientStatus`; the loop polls it
/// every [`TICK_INTERVAL_MS`] and re-renders the tooltip + "Status: …"
/// menu line on change. `on_quit` is invoked exactly once before the
/// process exits.
pub fn run_on_main_thread(
    rx: watch::Receiver<ClientStatus>,
    on_quit: Box<dyn FnOnce() + Send>,
) -> ! {
    let menu = Menu::new();
    let status_item = MenuItem::new("Status: starting…", false, None);
    let separator = PredefinedMenuItem::separator();
    let quit_item = MenuItem::new("Quit kmwarp", true, None);

    if let Err(e) = menu.append_items(&[&status_item, &separator, &quit_item]) {
        warn!(error = ?e, "tray: Menu::append_items failed; menu will be empty");
    }

    let icon = build_icon();

    let tray = match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("⚪ kmwarp (starting…)")
        .with_icon(icon)
        .build()
    {
        Ok(t) => t,
        Err(e) => {
            // No tray means no UI surface at all on Windows; bail loudly
            // rather than silently soldiering on with no way to quit.
            warn!(error = ?e, "tray: TrayIconBuilder::build failed; aborting");
            std::process::exit(1);
        }
    };

    let menu_rx = MenuEvent::receiver();
    let quit_id = quit_item.id().clone();
    let mut rx = rx;
    let mut last_rendered: Option<ClientStatus> = None;
    let mut on_quit = Some(on_quit);

    info!("tray icon online; entering Win32 message pump");

    loop {
        pump_messages();

        if rx.has_changed().unwrap_or(false) {
            let latest = rx.borrow_and_update().clone();
            if last_rendered.as_ref() != Some(&latest) {
                debug!(?latest, "tray: re-rendering status");
                render(&tray, &status_item, &latest);
                last_rendered = Some(latest);
            }
        }

        while let Ok(event) = menu_rx.try_recv() {
            if event.id == quit_id {
                info!("tray: Quit clicked; tearing down");
                if let Some(cb) = on_quit.take() {
                    cb();
                }
                sleep(Duration::from_millis(QUIT_GRACE_MS));
                std::process::exit(0);
            }
        }

        sleep(Duration::from_millis(TICK_INTERVAL_MS));
    }
}

/// Drain every pending Win32 message on the calling thread without blocking.
/// `GetMessageW` would park the thread forever and starve the watch poll;
/// `PeekMessageW(PM_REMOVE)` is the non-blocking equivalent that lets us
/// interleave the watch tick.
fn pump_messages() {
    let mut msg = MSG::default();
    // SAFETY: pure FFI. `msg` lives for the call. `None` for the hwnd
    // means "any window on this thread" — exactly what we want, since
    // tray-icon owns the message-only window and we never need to
    // address it directly.
    while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
        // SAFETY: standard message-pump pattern; `msg` is the value just
        // populated by PeekMessage.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Update the tooltip prefix glyph + the disabled "Status: …" menu line.
/// Cheap; called only on transitions detected by [`watch::Receiver::has_changed`].
fn render(tray: &TrayIcon, status_item: &MenuItem, status: &ClientStatus) {
    let (glyph, short) = status_glyph_and_short(status);
    let tooltip = format!("{glyph} kmwarp ({short})");
    status_item.set_text(format!("Status: {}", status_label(status)));
    if let Err(e) = tray.set_tooltip(Some(&tooltip)) {
        warn!(error = ?e, "tray: set_tooltip failed");
    }
}

/// Long-form, human-friendly status string for the disabled "Status: …"
/// menu line. The tooltip uses the shorter [`status_glyph_and_short`]
/// suffix; this version is for the dropdown where there's room to
/// surface the SPAKE2 code or peer name.
fn status_label(status: &ClientStatus) -> String {
    match status {
        ClientStatus::Idle => "idle".to_string(),
        ClientStatus::Discovering => "browsing mDNS".to_string(),
        ClientStatus::Pairing { code } => format!("pairing — code {code}"),
        ClientStatus::Connecting { addr } => format!("connecting to {addr}"),
        ClientStatus::Connected { peer } => format!("connected to {peer}"),
        ClientStatus::Driven { peer } => format!("driven by {peer}"),
    }
}

/// (glyph, short tooltip suffix) pair per status. Glyphs mirror the
/// macOS menubar:
///   ⚪ idle/listening, 🔍 mDNS browsing, 🟡 pairing/connecting,
///   🟢 connected, 🔵 driven (RemoteActive on the server side).
fn status_glyph_and_short(status: &ClientStatus) -> (&'static str, String) {
    match status {
        ClientStatus::Idle => ("⚪", "Idle".to_string()),
        ClientStatus::Discovering => ("🔍", "Discovering".to_string()),
        ClientStatus::Pairing { .. } => ("🟡", "Pairing".to_string()),
        ClientStatus::Connecting { .. } => ("🟡", "Connecting".to_string()),
        ClientStatus::Connected { peer } => ("🟢", format!("Connected to {peer}")),
        ClientStatus::Driven { peer } => ("🔵", format!("Driven by {peer}")),
    }
}

/// Build the tray icon from a flat-colour RGBA buffer. Spec for v1.1
/// allows a solid-square fallback; a real keyboard-glyph ICO can land in
/// task #12 once we're verifying on hardware.
fn build_icon() -> Icon {
    let rgba = solid_color_rgba(ICON_W, ICON_H, ICON_RGBA);
    Icon::from_rgba(rgba, ICON_W, ICON_H)
        .expect("Icon::from_rgba on a known-good 16x16 buffer must succeed")
}

fn solid_color_rgba(w: u32, h: u32, color: [u8; 4]) -> Vec<u8> {
    let mut buf = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..(w * h) {
        buf.extend_from_slice(&color);
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[test]
    fn label_idle() {
        assert_eq!(status_label(&ClientStatus::Idle), "idle");
    }

    #[test]
    fn label_pairing_shows_code() {
        let s = ClientStatus::Pairing {
            code: "123456".into(),
        };
        assert!(status_label(&s).contains("123456"));
    }

    #[test]
    fn label_connecting_shows_addr() {
        let addr: SocketAddr = "10.0.0.5:51423".parse().expect("parsed");
        let s = ClientStatus::Connecting { addr };
        assert!(status_label(&s).contains("10.0.0.5:51423"));
    }

    #[test]
    fn glyph_for_connected_includes_peer() {
        let s = ClientStatus::Connected {
            peer: "mac.local".into(),
        };
        let (g, short) = status_glyph_and_short(&s);
        assert_eq!(g, "🟢");
        assert!(short.contains("mac.local"));
    }

    #[test]
    fn glyph_for_driven_uses_blue() {
        let s = ClientStatus::Driven {
            peer: "mac.local".into(),
        };
        let (g, _) = status_glyph_and_short(&s);
        assert_eq!(g, "🔵");
    }

    #[test]
    fn glyph_for_discovering_uses_magnifier() {
        let (g, _) = status_glyph_and_short(&ClientStatus::Discovering);
        assert_eq!(g, "🔍");
    }

    #[test]
    fn solid_color_rgba_length_is_4wh() {
        let buf = solid_color_rgba(16, 16, [1, 2, 3, 4]);
        assert_eq!(buf.len(), 16 * 16 * 4);
        assert_eq!(&buf[0..4], &[1, 2, 3, 4]);
        assert_eq!(&buf[buf.len() - 4..], &[1, 2, 3, 4]);
    }
}
