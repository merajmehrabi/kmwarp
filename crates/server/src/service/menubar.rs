//! macOS menu bar status item for `kmwarp-server`.
//!
//! v1.0 shipped headless; v1.1 makes the daemon visible by hanging an
//! `NSStatusItem` off the system status bar that mirrors
//! [`crate::app::ServerStatus`] as a one-character glyph + label in the
//! top-right of the screen, with a small pop-down menu carrying a
//! human-readable status line and a Quit action.
//!
//! ## Pairing surface (v1.1)
//!
//! When [`ServerStatus::Pairing { code }`] is the live state, the menu
//! grows three transient items at the top:
//!
//! ```text
//!   Pairing code:                 (disabled label)
//!   123456                        (monospaced, 22 pt)
//!   Copy code                     (clicks → NSPasteboard write)
//!   ───────────
//!   Status: pairing — code 123456
//!   ───────────
//!   Quit kmwarp
//! ```
//!
//! The items are constructed once at startup and toggled with
//! `setHidden:` per transition, so the visible menu stays stable when
//! not pairing. On the first tick that sees `Pairing`, an `NSAlert`
//! is also raised so the operator notices even if the menu bar isn't
//! in their direct line of sight. The alert is best-effort — under
//! `.accessory` activation policy it won't steal focus, just appear.
//!
//! ## Threading model
//!
//! NSApplication / NSStatusItem MUST live on the process's main thread
//! (AppKit asserts on `MainThreadMarker`). The `run_server` tokio task
//! graph runs on a worker thread spawned by `main.rs`; this module
//! takes over the main thread for the rest of the process's lifetime.
//!
//! Cross-thread status delivery is via a single
//! `tokio::sync::watch::Receiver<ServerStatus>` — borrow-only, no
//! awaits — polled by a 4 Hz `NSTimer` running inside the NSApp run
//! loop. (Using a tokio runtime on the main thread would conflict with
//! NSApp ownership; using a thread + manual `mpsc` would still need a
//! main-thread tick to drain. The NSTimer wins on simplicity.)
//!
//! ## Quit action
//!
//! The menu's "Quit kmwarp" item targets the same handler class as the
//! tick timer. It invokes the user-supplied `on_quit` (typically:
//! signal the runtime, sleep briefly to let logs flush, then
//! `std::process::exit`) and then calls `NSApp.terminate(nil)` as a
//! belt-and-braces backstop in case the runtime tear-down doesn't
//! itself exit the process.
//!
//! ## Why `define_class!` (objc2 0.5+)
//!
//! Both the timer callback and the Quit menu item want to be Cocoa
//! targets, so we need a real Objective-C class with selectors AppKit
//! can dispatch into. `define_class!` is the idiomatic objc2 0.5+ way
//! to declare one with Rust-owned ivars.

#![cfg(target_os = "macos")]

use std::cell::RefCell;

use objc2::declare_class;
use objc2::msg_send_id;
use objc2::mutability::MainThreadOnly;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::sel;
use objc2::{ClassType, DeclaredClass};
use objc2_app_kit::{
    NSAlert, NSApplication, NSApplicationActivationPolicy, NSFont, NSFontAttributeName,
    NSFontWeightRegular, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{
    MainThreadMarker, NSAttributedString, NSDictionary, NSObject, NSObjectProtocol, NSString,
    NSTimer,
};
use tokio::sync::watch;
use tracing::{debug, info};

use crate::app::ServerStatus;

/// Polling cadence for the watch-channel tick. 250 ms keeps the menu
/// bar feeling live (sub-frame for "perceptual immediate") without
/// burning CPU on a process that's otherwise idle.
const TICK_INTERVAL_SECONDS: f64 = 0.25;

/// Point size for the monospaced pairing-code display in the menu.
/// 22 pt is roughly twice the system menu item size — visibly
/// prominent without overflowing on the user's 1512 px-wide main
/// display.
const PAIRING_CODE_POINT_SIZE: f64 = 22.0;

/// Heap-allocated state held by [`MenubarController`]. Stored in an
/// objc2 ivars struct so the Objective-C runtime can hand it back via
/// `self.ivars()` from each selector callback.
///
/// All fields are `RefCell<Option<T>>` because:
///   * `watch::Receiver`'s `borrow_and_update` takes `&mut self`;
///   * `on_quit` is a one-shot `FnOnce` we drain via `Option::take`;
///   * the `NSStatusItem` retain is owned here so it lives for the
///     program's lifetime.
struct MenubarIvars {
    /// Live receiver of `ServerStatus` updates from the runtime.
    rx: RefCell<watch::Receiver<ServerStatus>>,
    /// One-shot quit handler. Drained on the first "Quit kmwarp" click.
    on_quit: RefCell<Option<Box<dyn FnOnce() + Send + 'static>>>,
    /// The status item retain — keeps the menu bar entry alive.
    status_item: RefCell<Option<Retained<NSStatusItem>>>,
    /// The disabled "Status: …" menu item; updated in place each tick.
    status_label_item: RefCell<Option<Retained<NSMenuItem>>>,
    /// "Pairing code:" label (always present in the menu, hidden when
    /// not pairing).
    pairing_label_item: RefCell<Option<Retained<NSMenuItem>>>,
    /// Monospaced display of the 6-digit code (hidden when not pairing).
    pairing_code_item: RefCell<Option<Retained<NSMenuItem>>>,
    /// "Copy code" action item that writes the current code to
    /// NSPasteboard via [`crate::platform::macos::clipboard::pasteboard_write`].
    pairing_copy_item: RefCell<Option<Retained<NSMenuItem>>>,
    /// Separator after the pairing block (hidden when not pairing so
    /// the menu doesn't have a free-floating divider when the pairing
    /// items themselves are gone).
    pairing_separator_item: RefCell<Option<Retained<NSMenuItem>>>,
    /// The currently-displayed pairing code, used by the `copyCode:`
    /// selector. `None` when not in Pairing state.
    current_pairing_code: RefCell<Option<String>>,
    /// Whether we've already raised the NSAlert for the current
    /// pairing session. Set true on first tick that sees Pairing,
    /// reset to false whenever status leaves Pairing. Prevents the
    /// alert from re-popping every tick.
    alert_shown_for_current_pairing: RefCell<bool>,
    /// Last status rendered to the UI. Used to skip redundant
    /// re-renders (the NSTimer fires unconditionally; the watch may not
    /// have advanced).
    last_rendered: RefCell<Option<ServerStatus>>,
}

declare_class!(
    /// Objective-C class hosting the menu bar tick callback and the
    /// Quit menu item action. One instance per process.
    struct MenubarController;

    unsafe impl ClassType for MenubarController {
        type Super = NSObject;
        // `MainThreadOnly` is correct because NSStatusItem / NSMenu /
        // NSApplication all assert main-thread access, and our ivars
        // wrap watch::Receiver which is `!Sync` anyway.
        type Mutability = MainThreadOnly;
        const NAME: &'static str = "KMWarpMenubarController";
    }

    impl DeclaredClass for MenubarController {
        type Ivars = MenubarIvars;
    }

    unsafe impl NSObjectProtocol for MenubarController {}

    unsafe impl MenubarController {
        /// `NSTimer` callback. Reads the latest `ServerStatus` from
        /// the watch channel and, if it differs from the last rendered
        /// value, repaints the status item button + the disabled
        /// "Status: …" menu line.
        #[method(tick:)]
        fn tick(&self, _timer: *mut AnyObject) {
            self.refresh_if_changed();
        }

        /// "Quit kmwarp" menu item action. Drains the one-shot
        /// `on_quit` callback (typically: signal the tokio runtime
        /// to tear down, then `std::process::exit`) and then asks
        /// NSApp to terminate as a backstop in case the callback
        /// returns without exiting.
        #[method(quitClicked:)]
        fn quit_clicked(&self, _sender: *mut AnyObject) {
            info!("menubar: Quit clicked; tearing down");
            if let Some(cb) = self.ivars().on_quit.borrow_mut().take() {
                cb();
            }
            // SAFETY: terminate: is a stock NSApplication method;
            // calling it on the main thread (we are) is supported.
            unsafe {
                let mtm = MainThreadMarker::new_unchecked();
                let app = NSApplication::sharedApplication(mtm);
                app.terminate(None);
            }
        }

        /// "Copy code" menu item action. Writes the live pairing code
        /// to the system pasteboard so the operator can paste it
        /// directly into the Windows tray dialog.
        ///
        /// No-ops if the current_pairing_code ivar is empty (the menu
        /// item should be hidden in that case, but we double-check to
        /// stay safe against race conditions).
        #[method(copyCode:)]
        fn copy_code(&self, _sender: *mut AnyObject) {
            let code = self.ivars().current_pairing_code.borrow().clone();
            if let Some(code) = code {
                info!(len = code.len(), "menubar: copying pairing code to pasteboard");
                // Reuses the existing NSPasteboard helper from the
                // clipboard sync module — same thread-safety
                // guarantees apply.
                crate::platform::macos::clipboard::pasteboard_write(&code);
            } else {
                debug!("menubar: copyCode fired with no code in flight; ignoring");
            }
        }
    }
);

impl MenubarController {
    /// Allocate + init a fresh controller bound to the supplied watch
    /// receiver and quit handler. Sets `ivars()` and zeros the
    /// optional fields; caller (`run_on_main_thread`) is responsible
    /// for plugging in the NSStatusItem / NSMenuItem after creation.
    fn new(
        mtm: MainThreadMarker,
        rx: watch::Receiver<ServerStatus>,
        on_quit: Box<dyn FnOnce() + Send + 'static>,
    ) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(MenubarIvars {
            rx: RefCell::new(rx),
            on_quit: RefCell::new(Some(on_quit)),
            status_item: RefCell::new(None),
            status_label_item: RefCell::new(None),
            pairing_label_item: RefCell::new(None),
            pairing_code_item: RefCell::new(None),
            pairing_copy_item: RefCell::new(None),
            pairing_separator_item: RefCell::new(None),
            current_pairing_code: RefCell::new(None),
            alert_shown_for_current_pairing: RefCell::new(false),
            last_rendered: RefCell::new(None),
        });
        // SAFETY: standard NSObject `init` — the macro-generated alloc
        // hands us an `Allocated<Self>` with no extra storage beyond
        // our ivars; `init` runs the superclass initializer.
        unsafe { msg_send_id![super(this), init] }
    }

    /// Render `status` into the menubar button, the "Status: …" line,
    /// and the pairing block. Idempotent; called from the timer tick.
    fn render(&self, status: &ServerStatus) {
        let (glyph, full_text) = format_status(status);
        let title = format!("{glyph} kmwarp");
        let label = format!("Status: {full_text}");

        let ivars = self.ivars();
        // Update the status bar button's title.
        if let Some(item) = ivars.status_item.borrow().as_ref() {
            // SAFETY: button() and setTitle: are documented NSStatusItem
            // accessors. We're on the main thread (timer fired from the
            // run loop) so the AppKit precondition holds.
            unsafe {
                if let Some(btn) = item.button(MainThreadMarker::new_unchecked()) {
                    btn.setTitle(&NSString::from_str(&title));
                }
            }
        }
        // Update the disabled status-line menu item.
        if let Some(item) = ivars.status_label_item.borrow().as_ref() {
            unsafe { item.setTitle(&NSString::from_str(&label)) };
        }

        // Pairing-block visibility + content. Only the Pairing variant
        // shows the dedicated code surface.
        match status {
            ServerStatus::Pairing { code } => {
                self.show_pairing_block(code);
            }
            _ => {
                self.hide_pairing_block();
            }
        }
    }

    /// Configure the pairing-block menu items for the current `code`
    /// and reveal them. Idempotent — safe to call every tick while
    /// in Pairing state.
    fn show_pairing_block(&self, code: &str) {
        let ivars = self.ivars();
        *ivars.current_pairing_code.borrow_mut() = Some(code.to_string());

        // Update the monospaced code display. The attributed title
        // survives across ticks but the code text might change if the
        // pairing flow rolls a fresh code, so always re-set.
        if let Some(item) = ivars.pairing_code_item.borrow().as_ref() {
            let attr = build_code_attributed_string(code);
            // SAFETY: setAttributedTitle: is a main-thread mutator on
            // an item we own; no aliasing.
            unsafe { item.setAttributedTitle(Some(&attr)) };
        }

        // Unhide everything in the pairing block.
        for item in [
            &ivars.pairing_label_item,
            &ivars.pairing_code_item,
            &ivars.pairing_copy_item,
            &ivars.pairing_separator_item,
        ] {
            if let Some(item) = item.borrow().as_ref() {
                // SAFETY: setHidden: is a main-thread mutator.
                unsafe { item.setHidden(false) };
            }
        }

        // First-time-into-Pairing nudge: pop an NSAlert so the
        // operator notices even if the menu bar isn't in their field
        // of view. Best-effort — accessory apps don't steal focus, so
        // the alert appears but the user keeps their existing focus.
        let mut shown = ivars.alert_shown_for_current_pairing.borrow_mut();
        if !*shown {
            *shown = true;
            drop(shown);
            raise_pairing_alert(code);
        }
    }

    /// Hide every pairing-block item and forget the current code.
    /// Safe to call repeatedly; no-op when already hidden.
    fn hide_pairing_block(&self) {
        let ivars = self.ivars();
        *ivars.current_pairing_code.borrow_mut() = None;
        *ivars.alert_shown_for_current_pairing.borrow_mut() = false;
        for item in [
            &ivars.pairing_label_item,
            &ivars.pairing_code_item,
            &ivars.pairing_copy_item,
            &ivars.pairing_separator_item,
        ] {
            if let Some(item) = item.borrow().as_ref() {
                unsafe { item.setHidden(true) };
            }
        }
    }

    /// Pull the latest value from the watch receiver; if it differs
    /// from the previously rendered value, repaint. Cheap when nothing
    /// has changed (one borrow + one PartialEq).
    fn refresh_if_changed(&self) {
        let ivars = self.ivars();
        // Hold the RefMut and watch::Ref each in their own binding so
        // their borrow scopes are explicit; returning the cloned value
        // through a tail expression makes the Ref temporary outlive
        // the RefMut.
        let mut rx = ivars.rx.borrow_mut();
        // borrow_and_update marks the value as seen; equivalent to
        // peeking, but cheaper to detect future changes.
        let latest = rx.borrow_and_update().clone();
        drop(rx);

        let mut last = ivars.last_rendered.borrow_mut();
        if last.as_ref() == Some(&latest) {
            return;
        }
        *last = Some(latest.clone());
        drop(last);
        debug!(?latest, "menubar: re-rendering status");
        self.render(&latest);
    }
}

/// Build an NSAttributedString containing the pairing code in
/// monospaced [`PAIRING_CODE_POINT_SIZE`] pt with leading spaces for
/// visual breathing room inside the menu.
///
/// Erases the typed NSFont through `Retained::cast` because
/// `NSDictionary::from_id_slice` would otherwise infer the value type
/// as `NSObject`, and `NSAttributedString::initWithString_attributes`
/// wants `NSDictionary<_, AnyObject>`.
fn build_code_attributed_string(code: &str) -> Retained<NSAttributedString> {
    use objc2::runtime::AnyObject;
    // SAFETY: All three calls are main-thread API surface. We're on
    // the main thread inside the timer tick / startup path. The cast
    // from `Retained<NSFont>` to `Retained<AnyObject>` is safe because
    // every Objective-C class is a subclass of NSObject and `AnyObject`
    // is the universal-base typed-erased handle.
    unsafe {
        let font = NSFont::monospacedSystemFontOfSize_weight(
            PAIRING_CODE_POINT_SIZE,
            NSFontWeightRegular,
        );
        let font_any: Retained<AnyObject> = Retained::cast(font);
        let key: &NSString = NSFontAttributeName;
        let attrs: Retained<NSDictionary<NSString, AnyObject>> =
            NSDictionary::from_id_slice(&[key], &[font_any]);
        let text = NSString::from_str(&format!("  {code}"));
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &text,
            Some(&attrs),
        )
    }
}

/// Raise an NSAlert announcing the active pairing code. Best-effort
/// and blocking — the operator's click on OK returns control to the
/// timer loop. Under `.accessory` activation policy the alert appears
/// without stealing focus from whatever the user was doing.
fn raise_pairing_alert(code: &str) {
    // SAFETY: NSAlert::new + setters + runModal are main-thread APIs.
    // We're on the main thread (timer tick).
    unsafe {
        let mtm = MainThreadMarker::new_unchecked();
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("kmwarp pairing in progress"));
        alert.setInformativeText(&NSString::from_str(&format!(
            "Enter this code in the Windows client:\n\n  {code}\n\nThe code is also visible in the menu bar dropdown."
        )));
        let _response = alert.runModal();
    }
}

/// Take over the main thread, build the menu bar surface, and drive
/// the NSApp run loop forever.
///
/// This call **never returns** — `NSApp.run()` blocks until
/// `NSApplication.terminate:` is invoked, which calls `exit(0)` under
/// the hood. The `!` return type makes that contract explicit.
///
/// `rx` is the runtime-side broadcast of `ServerStatus`; `on_quit` is
/// invoked from the "Quit kmwarp" menu item before NSApp terminates
/// (typically: signal the tokio runtime, short grace sleep,
/// `std::process::exit`).
pub fn run_on_main_thread(
    rx: watch::Receiver<ServerStatus>,
    on_quit: Box<dyn FnOnce() + Send + 'static>,
) -> ! {
    let mtm = MainThreadMarker::new()
        .expect("service::menubar::run_on_main_thread must run on the main thread");

    // 1. Cocoa app bootstrap. `.accessory` = no dock icon, no menu
    //    bar app menu — exactly what a menu-bar-only daemon wants.
    //    Without this, the process gets a Dock icon AND steals
    //    keyboard focus on launch.
    let app = NSApplication::sharedApplication(mtm);
    // `setActivationPolicy` is generated by objc2 as a safe fn (it
    // only mutates AppKit-internal state and is documented thread-
    // checked); no unsafe block needed.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    // 2. Build the status item with variable length so the title
    //    autosizes to whatever string we put in it.
    //    `NSVariableStatusItemLength` is the documented "auto" sentinel.
    //
    //    SAFETY: `systemStatusBar` + `statusItemWithLength` are
    //    main-thread-only AppKit calls; the `mtm` we hold proves we
    //    are on the main thread.
    let status_item: Retained<NSStatusItem> = unsafe {
        let bar = NSStatusBar::systemStatusBar();
        bar.statusItemWithLength(NSVariableStatusItemLength)
    };

    // 3. Allocate the controller (carries ivars + Quit handler).
    let controller = MenubarController::new(mtm, rx, on_quit);

    // 4. Build the pop-down menu in display order:
    //
    //      [Pairing code:]           (disabled, hidden by default)
    //      [<code monospace>]        (hidden by default)
    //      [Copy code]               (hidden by default)
    //      ─────────────             (hidden by default)
    //      [Status: <text>]          (disabled, always visible)
    //      ─────────────             (always visible)
    //      [Quit kmwarp]             (Cmd+Q, always visible)
    let menu: Retained<NSMenu> = NSMenu::new(mtm);

    // SAFETY for the menu-item construction blocks below:
    // setTitle:/setEnabled:/setAction:/setTarget:/setKeyEquivalent:/
    // setHidden: are main-thread mutators on freshly-allocated
    // NSMenuItems that no other code has a reference to yet. No
    // aliasing.

    // 4a. Pairing block (hidden by default).
    let pairing_label_item: Retained<NSMenuItem> = unsafe {
        let item = NSMenuItem::new(mtm);
        item.setTitle(&NSString::from_str("Pairing code:"));
        item.setEnabled(false);
        item.setHidden(true);
        item
    };
    menu.addItem(&pairing_label_item);

    let pairing_code_item: Retained<NSMenuItem> = unsafe {
        let item = NSMenuItem::new(mtm);
        // Initial placeholder; replaced by `show_pairing_block` with
        // an NSAttributedString carrying the real code in monospace.
        item.setTitle(&NSString::from_str("      "));
        item.setEnabled(false);
        item.setHidden(true);
        item
    };
    menu.addItem(&pairing_code_item);

    let pairing_copy_item: Retained<NSMenuItem> = unsafe {
        let item = NSMenuItem::new(mtm);
        item.setTitle(&NSString::from_str("Copy code"));
        item.setAction(Some(sel!(copyCode:)));
        let target: &AnyObject = controller.as_ref();
        item.setTarget(Some(target));
        item.setHidden(true);
        item
    };
    menu.addItem(&pairing_copy_item);

    let pairing_separator_item = NSMenuItem::separatorItem(mtm);
    unsafe { pairing_separator_item.setHidden(true) };
    menu.addItem(&pairing_separator_item);

    // 4b. Always-visible block.
    let status_label_item: Retained<NSMenuItem> = unsafe {
        let item = NSMenuItem::new(mtm);
        item.setTitle(&NSString::from_str("Status: starting…"));
        // Disabled: it's a label, not a clickable target.
        item.setEnabled(false);
        item
    };
    menu.addItem(&status_label_item);

    let separator = NSMenuItem::separatorItem(mtm);
    menu.addItem(&separator);

    let quit_item: Retained<NSMenuItem> = unsafe {
        let item = NSMenuItem::new(mtm);
        item.setTitle(&NSString::from_str("Quit kmwarp"));
        item.setAction(Some(sel!(quitClicked:)));
        // Target is the controller; AppKit will dispatch
        // `-[KMWarpMenubarController quitClicked:]` on click.
        let target: &AnyObject = controller.as_ref();
        item.setTarget(Some(target));
        // Cmd+Q convenience binding. Costs nothing if the user
        // never opens the menu.
        item.setKeyEquivalent(&NSString::from_str("q"));
        item
    };
    menu.addItem(&quit_item);

    // Wire the menu onto the status item, and stash the retains in
    // the controller's ivars so the timer tick can address them.
    // SAFETY: `setMenu:` is a main-thread mutator on the status item.
    unsafe { status_item.setMenu(Some(&menu)) };
    *controller.ivars().status_item.borrow_mut() = Some(status_item);
    *controller.ivars().status_label_item.borrow_mut() = Some(status_label_item);
    *controller.ivars().pairing_label_item.borrow_mut() = Some(pairing_label_item);
    *controller.ivars().pairing_code_item.borrow_mut() = Some(pairing_code_item);
    *controller.ivars().pairing_copy_item.borrow_mut() = Some(pairing_copy_item);
    *controller.ivars().pairing_separator_item.borrow_mut() = Some(pairing_separator_item);

    // 5. Schedule the polling timer. Target = controller, selector =
    //    `tick:`, repeats forever at TICK_INTERVAL_SECONDS. We hold
    //    no Retained<NSTimer> ourselves — the run loop retains it
    //    until we invalidate, which we never do (process lifetime).
    unsafe {
        let _timer = NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            TICK_INTERVAL_SECONDS,
            controller.as_ref(),
            sel!(tick:),
            None,
            true,
        );
    }

    // 6. One initial render so the menu bar isn't blank between launch
    //    and the first tick fire (250 ms is small but visible).
    controller.refresh_if_changed();

    info!("menu bar status item online; entering NSApp run loop");

    // 7. Drive the AppKit run loop forever. `run` doesn't return until
    //    NSApp terminates, which calls into `exit(0)` itself — hence
    //    `unreachable!()` rather than a real fallthrough.
    //
    //    SAFETY: NSApplication::run is the standard NSApp run-loop
    //    entry; calling it on the main thread (we are) is required.
    unsafe {
        app.run();
    }

    // Keep `controller` alive across the run loop — without this the
    // optimizer could theoretically drop it before `run()` returns.
    // (`run()` never returns in practice, but the compiler can't know
    // that.)
    drop(controller);

    unreachable!("NSApplication::run terminated without exit");
}

/// Convert a `ServerStatus` into `(glyph, human_text)`. Glyphs match
/// the team spec:
///   ⚪ idle (no peer ever connected this run, or back to listening)
///   🟡 pairing
///   🟢 connected (handshake done, LocalActive)
///   🔵 active (RemoteActive — input is being forwarded)
fn format_status(status: &ServerStatus) -> (&'static str, String) {
    match status {
        ServerStatus::Idle => ("⚪", "idle".to_string()),
        ServerStatus::Listening { addr } => ("⚪", format!("listening on {addr}")),
        ServerStatus::Pairing { code } => ("🟡", format!("pairing — code {code}")),
        ServerStatus::Connected { peer } => ("🟢", format!("connected to {peer}")),
        ServerStatus::Active { peer } => ("🔵", format!("active — forwarding to {peer}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[test]
    fn format_status_idle() {
        let (g, t) = format_status(&ServerStatus::Idle);
        assert_eq!(g, "⚪");
        assert_eq!(t, "idle");
    }

    #[test]
    fn format_status_listening_includes_addr() {
        let addr: SocketAddr = "127.0.0.1:51423".parse().unwrap();
        let (g, t) = format_status(&ServerStatus::Listening { addr });
        assert_eq!(g, "⚪");
        assert!(t.contains("127.0.0.1:51423"));
    }

    #[test]
    fn format_status_pairing_shows_code() {
        let (g, t) = format_status(&ServerStatus::Pairing {
            code: "123456".into(),
        });
        assert_eq!(g, "🟡");
        assert!(t.contains("123456"));
    }

    #[test]
    fn format_status_connected_shows_peer() {
        let (g, t) = format_status(&ServerStatus::Connected {
            peer: "10.0.0.5:42".into(),
        });
        assert_eq!(g, "🟢");
        assert!(t.contains("10.0.0.5:42"));
    }

    #[test]
    fn format_status_active_shows_peer() {
        let (g, t) = format_status(&ServerStatus::Active {
            peer: "10.0.0.5:42".into(),
        });
        assert_eq!(g, "🔵");
        assert!(t.contains("10.0.0.5:42"));
    }
}
