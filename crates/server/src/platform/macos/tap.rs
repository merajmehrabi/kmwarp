//! `CGEventTap`-backed implementation of [`kmwarp_core::InputSource`].
//!
//! ## Threading model
//!
//! Apple mandates that a `CGEventTap`'s callback fire from a `CFRunLoop`.
//! We don't drive the run loop from tokio (it would peg a worker), so on
//! [`MacInputSource::install`] we:
//!
//! 1. Spawn a dedicated `std::thread` (`kmwarp-cgtap`).
//! 2. Inside it, create the tap, attach its mach port to the thread-local
//!    `CFRunLoop`, enable it, and call `CFRunLoop::run_current` (blocks).
//! 3. Publish the run-loop handle back to the installer via a
//!    `std::sync::mpsc` channel.
//! 4. Spawn a tiny watcher thread (`kmwarp-cgtap-shutdown`) that blocks on
//!    a `std::sync::mpsc::Receiver`. When the matching `Sender` (stored in
//!    `MacInputSource::_shutdown`) drops, the watcher wakes and calls
//!    `CFRunLoop::stop` — which is documented thread-safe — and the tap
//!    thread exits its `run_current` and the thread terminates.
//!
//! Both shutdown and tap-disable handling avoid touching tokio because the
//! source can be torn down during runtime shutdown when tokio tasks may no
//! longer be schedulable.
//!
//! ## Callback contract
//!
//! The callback is `Fn(&CGEvent) -> Option<CGEvent>`. M2 returns `None`
//! unconditionally, which the `core-graphics` wrapper interprets as
//! "pass the original event through" (no swallowing). M6 will revisit when
//! the edge state machine wants to consume events while remote-active.
//!
//! Tap-disabled handling (timeout / user-input) re-enables the tap from
//! inside the callback via `CGEventTapEnable` on the captured mach-port
//! pointer (published through an `OnceLock<usize>` shared with the outer
//! thread).

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, OnceLock};
use std::thread;

use async_trait::async_trait;
use core_foundation::base::TCFType;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, EventField,
};
use kmwarp_core::hid::macos::macos_to_hid;
use kmwarp_core::platform::{InputSource, KeyState, ModMask, MouseButton, SourceEvent};
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

use super::tap_error::TapError;

// The Rust binding's safe `CGEventTap::enable` requires `&self`, but the
// tap is owned by the run-loop thread and can't be borrowed from inside
// its own callback. So we re-enable via the C entry point directly,
// using a `usize` snapshot of the mach-port ref published into a shared
// `OnceLock` right after tap creation.
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
}

/// Public handle: a stream of `SourceEvent`s plus the shutdown plumbing for
/// the run-loop and watcher threads.
pub struct MacInputSource {
    rx: mpsc::UnboundedReceiver<SourceEvent>,
    // Dropping the Sender wakes the watcher thread, which stops the run loop.
    // `Option` so `Drop` can take it explicitly before joining the threads.
    shutdown: Option<std_mpsc::Sender<()>>,
    watcher: Option<thread::JoinHandle<()>>,
    tap_thread: Option<thread::JoinHandle<()>>,
}

impl MacInputSource {
    /// Install the tap and start the run-loop + shutdown-watcher threads.
    ///
    /// Blocks the caller briefly while the run-loop thread reports back
    /// whether `CGEventTapCreate` succeeded.
    pub fn install() -> Result<Self, TapError> {
        let (event_tx, event_rx) = mpsc::unbounded_channel::<SourceEvent>();
        let (shutdown_tx, shutdown_rx) = std_mpsc::channel::<()>();
        let (rl_tx, rl_rx) = std_mpsc::sync_channel::<Result<CFRunLoop, TapError>>(1);

        // Shared mach-port ref so the callback can re-enable on tap-disabled
        // events. The value is set exactly once, immediately after
        // CGEventTapCreate succeeds.
        let port_holder: Arc<OnceLock<usize>> = Arc::new(OnceLock::new());
        let port_for_cb = Arc::clone(&port_holder);
        let port_for_publish = Arc::clone(&port_holder);

        let tap_thread = thread::Builder::new()
            .name("kmwarp-cgtap".into())
            .spawn(move || {
                // Per-modifier-keycode "currently held" bitmap, lives only
                // on the tap thread (the closure runs single-threaded inside
                // its CFRunLoop, so `Cell` is sufficient — no Mutex needed).
                // See `translate_flags_changed` for the index layout.
                let held_mods: Cell<u32> = Cell::new(0);
                let tap_result = CGEventTap::new(
                    CGEventTapLocation::HID,
                    CGEventTapPlacement::HeadInsertEventTap,
                    CGEventTapOptions::Default,
                    interest_mask(),
                    move |_proxy, etype, event| {
                        callback(etype, event, &event_tx, &port_for_cb, &held_mods)
                    },
                );

                let tap = match tap_result {
                    Ok(t) => t,
                    Err(()) => {
                        let _ = rl_tx.send(Err(TapError::TapCreateFailed));
                        return;
                    }
                };

                let loop_source = match tap.mach_port.create_runloop_source(0) {
                    Ok(s) => s,
                    Err(()) => {
                        let _ = rl_tx.send(Err(TapError::RunLoopFailed));
                        return;
                    }
                };
                let current = CFRunLoop::get_current();
                // SAFETY: `kCFRunLoopCommonModes` is a CoreFoundation extern
                // static; access is unsafe but the value is read-only and
                // valid for the lifetime of the process.
                let mode = unsafe { kCFRunLoopCommonModes };
                current.add_source(&loop_source, mode);

                // Publish the mach-port ref so the callback can re-enable
                // the tap on disabled-by-timeout / disabled-by-user-input.
                let port_ref = tap.mach_port.as_concrete_TypeRef() as usize;
                let _ = port_for_publish.set(port_ref);

                tap.enable();

                if rl_tx.send(Ok(current)).is_err() {
                    // Installer was dropped before we got here; exit cleanly.
                    return;
                }

                CFRunLoop::run_current();
                debug!("kmwarp-cgtap thread exiting CFRunLoop");
                drop(tap); // explicit: tap stays alive across run_current
            })
            .map_err(|_| TapError::RunLoopFailed)?;

        let run_loop = match rl_rx.recv() {
            Ok(Ok(rl)) => rl,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(TapError::RunLoopFailed),
        };
        info!("CGEventTap installed");

        let watcher = thread::Builder::new()
            .name("kmwarp-cgtap-shutdown".into())
            .spawn(move || {
                // Blocks until the matching Sender is dropped — i.e. the
                // MacInputSource is being torn down.
                let _ = shutdown_rx.recv();
                run_loop.stop();
                debug!("CGEventTap run loop stop requested");
            })
            .map_err(|_| TapError::RunLoopFailed)?;

        Ok(Self {
            rx: event_rx,
            shutdown: Some(shutdown_tx),
            watcher: Some(watcher),
            tap_thread: Some(tap_thread),
        })
    }
}

impl Drop for MacInputSource {
    fn drop(&mut self) {
        // Wake the watcher first so the run loop stops, then join both
        // threads. Both terminate within milliseconds — well below any
        // reasonable shutdown deadline.
        drop(self.shutdown.take());
        if let Some(h) = self.watcher.take() {
            let _ = h.join();
        }
        if let Some(h) = self.tap_thread.take() {
            let _ = h.join();
        }
    }
}

#[async_trait]
impl InputSource for MacInputSource {
    async fn next_event(&mut self) -> Option<SourceEvent> {
        self.rx.recv().await
    }
}

/// CGEventTap callback body. Kept allocation-free; just translates and
/// forwards into the unbounded channel.
fn callback(
    etype: CGEventType,
    event: &CGEvent,
    tx: &mpsc::UnboundedSender<SourceEvent>,
    port_holder: &OnceLock<usize>,
    held_mods: &Cell<u32>,
) -> Option<CGEvent> {
    match etype {
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
            if let Some(&mp) = port_holder.get() {
                // SAFETY: `mp` originated from `CFMachPort::as_concrete_TypeRef`
                // on the tap's mach port, which is owned by the run-loop
                // thread for the lifetime of this callback.
                unsafe { CGEventTapEnable(mp as *mut c_void, true) };
                warn!(?etype, "CGEventTap auto-reenabled after OS-disable");
            } else {
                error!("tap-disabled event arrived before mach port was published");
            }
            return None;
        }
        _ => {}
    }
    if let Some(ev) = translate(etype, event, held_mods) {
        // Unbounded send; only fails if the receiver was dropped (consumer
        // gone). In that case the source is being torn down and the run
        // loop will stop shortly via the shutdown watcher.
        let _ = tx.send(ev);
    }
    // M6: every mouse-motion event also emits an absolute `CursorAt` so
    // the edge state machine can see the cursor cross the right edge.
    // CursorAt is server-internal — never on the wire — but the SM
    // distinguishes between "you moved by (dx,dy)" and "you're now at
    // (x,y)" because crossings are detected against absolute screen
    // coords, not deltas.
    if is_mouse_motion(etype) {
        let loc = event.location();
        // `CGPoint` is logical points; PLAN.md HiDPI normalization is a
        // M11 follow-up. SM treats the value as opaque screen units.
        let _ = tx.send(SourceEvent::CursorAt {
            x: loc.x as i32,
            y: loc.y as i32,
        });
    }
    // Pass-through: M2 never swallows. M6 will revisit.
    None
}

/// Mouse-motion event types — the only ones whose absolute location
/// matters for the edge state machine. Button up/down events also carry
/// a location but the spec triggers crossings off motion only.
fn is_mouse_motion(etype: CGEventType) -> bool {
    matches!(
        etype,
        CGEventType::MouseMoved
            | CGEventType::LeftMouseDragged
            | CGEventType::RightMouseDragged
            | CGEventType::OtherMouseDragged
    )
}

fn interest_mask() -> Vec<CGEventType> {
    vec![
        // Pointer motion
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDragged,
        // Buttons
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
        // Wheel
        CGEventType::ScrollWheel,
        // Keyboard — subscribed so M5 can wire them in without re-installing
        // the tap. M2's `translate` returns None for these.
        CGEventType::KeyDown,
        CGEventType::KeyUp,
        CGEventType::FlagsChanged,
    ]
}

fn translate(etype: CGEventType, event: &CGEvent, held_mods: &Cell<u32>) -> Option<SourceEvent> {
    use CGEventType::{
        FlagsChanged, KeyDown, KeyUp, LeftMouseDown, LeftMouseDragged, LeftMouseUp, MouseMoved,
        OtherMouseDown, OtherMouseDragged, OtherMouseUp, RightMouseDown, RightMouseDragged,
        RightMouseUp, ScrollWheel,
    };
    match etype {
        MouseMoved | LeftMouseDragged | RightMouseDragged | OtherMouseDragged => {
            let dx = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X);
            let dy = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y);
            Some(SourceEvent::MouseRel {
                dx: clamp_i64_to_i16(dx),
                dy: clamp_i64_to_i16(dy),
            })
        }
        LeftMouseDown => Some(SourceEvent::MouseButton {
            button: MouseButton::Left,
            state: KeyState::Down,
        }),
        LeftMouseUp => Some(SourceEvent::MouseButton {
            button: MouseButton::Left,
            state: KeyState::Up,
        }),
        RightMouseDown => Some(SourceEvent::MouseButton {
            button: MouseButton::Right,
            state: KeyState::Down,
        }),
        RightMouseUp => Some(SourceEvent::MouseButton {
            button: MouseButton::Right,
            state: KeyState::Up,
        }),
        OtherMouseDown => Some(SourceEvent::MouseButton {
            button: other_button(event),
            state: KeyState::Down,
        }),
        OtherMouseUp => Some(SourceEvent::MouseButton {
            button: other_button(event),
            state: KeyState::Up,
        }),
        ScrollWheel => {
            // Axis 1 = vertical, Axis 2 = horizontal per Apple's docs.
            let dy = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1);
            let dx = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2);
            Some(SourceEvent::MouseWheel {
                dx: clamp_i64_to_i16(dx),
                dy: clamp_i64_to_i16(dy),
            })
        }
        KeyDown => translate_key(event, KeyState::Down),
        KeyUp => translate_key(event, KeyState::Up),
        FlagsChanged => translate_flags_changed(event, held_mods),
        _ => None,
    }
}

/// Translate `kCGEventFlagsChanged` into a `SourceEvent::Key { state: … }`.
///
/// macOS doesn't tell us up vs. down directly on a flags-changed event;
/// it delivers the keycode of the modifier that just transitioned and the
/// *new* aggregate `CGEventFlags`. The teammate spec suggested XOR'ing the
/// old vs. new flags integer — but that's ambiguous when the user already
/// holds (say) Left Shift and then presses Right Shift, since the
/// aggregate `CGEventFlagShift` bit doesn't flip. So we keep a per-keycode
/// held-bitmap instead: every FlagsChanged for a tracked modifier vk
/// flips its bit, and "was previously held" → it's an Up.
///
/// Returns `None` (with a `trace!`) when:
///   - the vk isn't a modifier we track (e.g. raw `Fn` 0x3F — present in
///     the macOS keyboard but not in v1's HID table);
///   - the modifier's HID code isn't in `MACOS_VK_TO_HID` (paranoia: the
///     two should always agree for the keycodes in [`mod_bit`]).
///
/// The emitted `mods` field reflects the *aggregate* flags *after* the
/// transition, which is what the wire-format byte semantically means.
fn translate_flags_changed(event: &CGEvent, held: &Cell<u32>) -> Option<SourceEvent> {
    let cg_vk = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
    let bit_idx = match mod_bit(cg_vk) {
        Some(b) => b,
        None => {
            trace!(
                ?cg_vk,
                "FlagsChanged: vk is not a tracked modifier; dropping"
            );
            return None;
        }
    };
    let bit = 1u32 << bit_idx;
    let prev = held.get();
    let was_held = (prev & bit) != 0;
    held.set(prev ^ bit);
    let state = if was_held {
        KeyState::Up
    } else {
        KeyState::Down
    };

    let hid = match macos_to_hid(cg_vk) {
        Some(h) => h,
        None => {
            trace!(
                ?cg_vk,
                ?state,
                "FlagsChanged: vk has no HID mapping; dropping"
            );
            return None;
        }
    };
    let mods = mods_from_flags(event.get_flags());
    Some(SourceEvent::Key {
        hid_usage: hid,
        state,
        mods,
    })
}

/// Bit index in the held-modifier bitmap for each modifier macOS VK we
/// track. The ordering is purely a private detail of the tap state; the
/// only contract is that each tracked vk maps to a unique bit.
///
/// Includes both-side variants for Shift/Ctrl/Option/Command plus Caps
/// Lock. The `Fn` key (0x3F) is intentionally omitted — it isn't in the
/// v1 HID table and would only confuse downstream consumers.
fn mod_bit(vk: u16) -> Option<u8> {
    match vk {
        0x38 => Some(0), // kVK_Shift   (left)
        0x3C => Some(1), // kVK_RightShift
        0x3B => Some(2), // kVK_Control (left)
        0x3E => Some(3), // kVK_RightControl
        0x3A => Some(4), // kVK_Option  (left, alt)
        0x3D => Some(5), // kVK_RightOption
        0x37 => Some(6), // kVK_Command (left)
        0x36 => Some(7), // kVK_RightCommand
        0x39 => Some(8), // kVK_CapsLock
        _ => None,
    }
}

/// Translate a `kCGEventKeyDown` / `kCGEventKeyUp` event into a
/// `SourceEvent::Key`. Returns `None` (and emits a `trace!`) when:
///   - the event is an autorepeat (`kCGKeyboardEventAutorepeat == 1`) — per
///     the spec's "Key repeat" gotcha, the destination OS regenerates repeats
///     from a sustained held state, so we forward press + release only;
///   - the macOS virtual keycode has no entry in `MACOS_VK_TO_HID` (any key
///     outside the v1 alphanumeric / punctuation / nav / mod set).
fn translate_key(event: &CGEvent, state: KeyState) -> Option<SourceEvent> {
    // Autorepeat is only meaningful for KeyDown, but the field is also zero
    // on KeyUp so we can read it unconditionally and the check is a no-op
    // for releases. Keeps both arms symmetric.
    if state == KeyState::Down
        && event.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT) != 0
    {
        return None;
    }
    let cg_vk = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
    let hid = match macos_to_hid(cg_vk) {
        Some(h) => h,
        None => {
            trace!(?cg_vk, ?state, "unmapped macOS VK; dropping key event");
            return None;
        }
    };
    let mods = mods_from_flags(event.get_flags());
    Some(SourceEvent::Key {
        hid_usage: hid,
        state,
        mods,
    })
}

/// Project the aggregate `CGEventFlags` value onto our cross-platform
/// `ModMask`. Only the four chord modifiers (Shift/Control/Alt/Command)
/// are projected; `AlphaShift` (Caps Lock latch), `Help`, `SecondaryFn`,
/// and the numeric-pad bit are intentionally dropped — they aren't part
/// of the wire-format modifier byte.
fn mods_from_flags(flags: CGEventFlags) -> ModMask {
    let mut m = ModMask::default();
    if flags.contains(CGEventFlags::CGEventFlagShift) {
        m.insert(ModMask::SHIFT);
    }
    if flags.contains(CGEventFlags::CGEventFlagControl) {
        m.insert(ModMask::CTRL);
    }
    if flags.contains(CGEventFlags::CGEventFlagAlternate) {
        m.insert(ModMask::ALT);
    }
    if flags.contains(CGEventFlags::CGEventFlagCommand) {
        m.insert(ModMask::META);
    }
    m
}

fn other_button(event: &CGEvent) -> MouseButton {
    let btn = event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
    match btn {
        0 => MouseButton::Left,
        1 => MouseButton::Right,
        2 => MouseButton::Middle,
        3 => MouseButton::X1,
        _ => MouseButton::X2,
    }
}

/// Saturating cast `i64 → i16`. CG mouse deltas occasionally spike well
/// outside `i16` (e.g. a fast trackpad swipe across a HiDPI display); we
/// clamp rather than wrap.
fn clamp_i64_to_i16(v: i64) -> i16 {
    v.clamp(i16::MIN as i64, i16::MAX as i64) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_oversized_deltas() {
        assert_eq!(clamp_i64_to_i16(0), 0);
        assert_eq!(clamp_i64_to_i16(40_000), i16::MAX);
        assert_eq!(clamp_i64_to_i16(-40_000), i16::MIN);
        assert_eq!(clamp_i64_to_i16(123), 123);
        assert_eq!(clamp_i64_to_i16(-123), -123);
    }

    #[test]
    fn mods_from_flags_maps_chord_modifiers() {
        assert_eq!(mods_from_flags(CGEventFlags::empty()), ModMask::default());

        assert_eq!(
            mods_from_flags(CGEventFlags::CGEventFlagShift),
            ModMask::SHIFT
        );
        assert_eq!(
            mods_from_flags(CGEventFlags::CGEventFlagControl),
            ModMask::CTRL
        );
        assert_eq!(
            mods_from_flags(CGEventFlags::CGEventFlagAlternate),
            ModMask::ALT
        );
        assert_eq!(
            mods_from_flags(CGEventFlags::CGEventFlagCommand),
            ModMask::META
        );

        let all = CGEventFlags::CGEventFlagShift
            | CGEventFlags::CGEventFlagControl
            | CGEventFlags::CGEventFlagAlternate
            | CGEventFlags::CGEventFlagCommand;
        let expected = {
            let mut m = ModMask::default();
            m.insert(ModMask::SHIFT);
            m.insert(ModMask::CTRL);
            m.insert(ModMask::ALT);
            m.insert(ModMask::META);
            m
        };
        assert_eq!(mods_from_flags(all), expected);
    }

    #[test]
    fn mod_bit_assigns_unique_indices_for_tracked_modifiers() {
        // Every documented mod VK gets a unique bit index 0..=8; an
        // off-list VK returns None.
        let vks = [0x38, 0x3C, 0x3B, 0x3E, 0x3A, 0x3D, 0x37, 0x36, 0x39];
        let mut seen = std::collections::HashSet::new();
        for vk in vks {
            let bit = mod_bit(vk).expect("tracked modifier vk should map");
            assert!(bit < 9, "bit index {bit} out of range");
            assert!(seen.insert(bit), "duplicate bit index for vk 0x{vk:X}");
        }
        // A letter (kVK_ANSI_A = 0x00) and the Fn key (0x3F) are not
        // tracked modifiers.
        assert_eq!(mod_bit(0x00), None);
        assert_eq!(mod_bit(0x3F), None);
    }

    #[test]
    fn flags_changed_held_tracker_flips_state_per_keycode() {
        // Walk the bitmap directly — we can't fabricate a CGEvent in a
        // unit test, so we exercise the state transition logic by
        // re-implementing the same flip-and-test sequence the
        // `translate_flags_changed` code path uses.
        let held: Cell<u32> = Cell::new(0);

        // Press Left Shift (vk 0x38, bit 0): was_held false → Down.
        let bit_l = 1u32 << mod_bit(0x38).unwrap();
        let prev = held.get();
        let was_held = (prev & bit_l) != 0;
        held.set(prev ^ bit_l);
        assert!(!was_held);
        assert_eq!(held.get(), bit_l);

        // Press Right Shift (vk 0x3C, bit 1) while Left still held:
        // was_held false → Down. Aggregate Shift bit would have been
        // unchanged, but per-key state is correct.
        let bit_r = 1u32 << mod_bit(0x3C).unwrap();
        let prev = held.get();
        let was_held = (prev & bit_r) != 0;
        held.set(prev ^ bit_r);
        assert!(!was_held);
        assert_eq!(held.get(), bit_l | bit_r);

        // Release Right Shift: was_held true → Up.
        let prev = held.get();
        let was_held = (prev & bit_r) != 0;
        held.set(prev ^ bit_r);
        assert!(was_held);
        assert_eq!(held.get(), bit_l);

        // Release Left Shift: was_held true → Up.
        let prev = held.get();
        let was_held = (prev & bit_l) != 0;
        held.set(prev ^ bit_l);
        assert!(was_held);
        assert_eq!(held.get(), 0);
    }

    #[test]
    fn mods_from_flags_ignores_non_chord_bits() {
        // AlphaShift (Caps Lock latch), Help, SecondaryFn (the Fn key),
        // and the numeric-pad bit should not bleed into ModMask — they
        // are not wire-format modifiers.
        let noise = CGEventFlags::CGEventFlagAlphaShift
            | CGEventFlags::CGEventFlagHelp
            | CGEventFlags::CGEventFlagSecondaryFn
            | CGEventFlags::CGEventFlagNumericPad;
        assert_eq!(mods_from_flags(noise), ModMask::default());
    }
}
