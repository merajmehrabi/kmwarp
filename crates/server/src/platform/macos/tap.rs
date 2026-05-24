//! `CGEventTap`-backed implementation of [`kmwarp_core::InputSource`].
//!
//! ## Threading model
//!
//! Apple mandates that a `CGEventTap`'s callback fire from a `CFRunLoop`.
//! We don't drive the run loop from tokio (it would peg a worker), so on
//! [`MacInputSource::install`] we:
//!
//! 1. Spawn a dedicated `std::thread` (`kmwarp-cgtap`).
//! 2. Inside it, create the tap via raw `CGEventTapCreate` FFI (see
//!    "Raw FFI" below), attach its mach port to the thread-local
//!    `CFRunLoop`, enable it, and call `CFRunLoop::run_current` (blocks).
//! 3. Publish the run-loop handle back to the installer via a
//!    `std::sync::mpsc` channel.
//! 4. Spawn a tiny watcher thread (`kmwarp-cgtap-shutdown`) that blocks on
//!    a `std::sync::mpsc::Receiver`. When the matching `Sender` (stored in
//!    `MacInputSource::shutdown`) drops, the watcher wakes and calls
//!    `CFRunLoop::stop` — which is documented thread-safe — and the tap
//!    thread exits its `run_current` and the thread terminates.
//!
//! Both shutdown and tap-disable handling avoid touching tokio because the
//! source can be torn down during runtime shutdown when tokio tasks may no
//! longer be schedulable.
//!
//! ## Raw FFI (and why we dropped the `core-graphics` safe wrapper)
//!
//! M2 used `core_graphics::event::CGEventTap`, whose internal callback
//! interprets a closure return of `None` as "pass through the original
//! event unchanged" — there's no way to actually return `NULL` and have
//! the OS swallow the event. M6's `RemoteActive` state requires
//! swallowing (the user is controlling the Windows peer; the Mac must
//! not also see those mouse moves and keystrokes), so we install the
//! tap directly via `CGEventTapCreate` and own the `extern "C"`
//! callback ourselves. The same `CFMachPort` returned by `CGEventTapCreate`
//! is then wrapped via `TCFType::wrap_under_create_rule` for run-loop
//! attachment.
//!
//! ## Swallow semantics
//!
//! The callback always *translates* the event and sends the
//! `SourceEvent` to the mpsc channel — even when swallow is on — so the
//! edge state machine sees release events for held keys (the M7
//! stuck-key drain depends on this). The swallow flag *only* controls
//! the return value to the OS:
//!   - `swallow == false` → return the original `CGEventRef` (pass-through).
//!   - `swallow == true`  → return `NULL` for any "swallowable" event
//!     (mouse motion/button/wheel + key down/up + flags-changed); other
//!     event types pass through unchanged.

use std::cell::Cell;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, OnceLock};
use std::thread;

use async_trait::async_trait;
use core_foundation::base::TCFType;
use core_foundation::mach_port::{CFMachPort, CFMachPortRef};
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{CGEventFlags, EventField};
use core_graphics::geometry::CGPoint;
use kmwarp_core::hid::macos::macos_to_hid;
use kmwarp_core::platform::{InputSource, KeyState, ModMask, MouseButton, SourceEvent};
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

use super::tap_error::TapError;

// ---------------------------------------------------------------------------
// Raw FFI to CGEventTap entry points.
// ---------------------------------------------------------------------------

type CGEventRef = *mut c_void;
type CGEventTapProxy = *const c_void;
type CGEventMask = u64;

// `CGEventTapLocation` (u32):
const TAP_LOC_HID: u32 = 0;
// `CGEventTapPlacement` (u32):
const TAP_PLACE_HEAD_INSERT: u32 = 0;
// `CGEventTapOptions` (u32):
const TAP_OPT_DEFAULT: u32 = 0;

// Subset of `CGEventType` (the wrapper's `#[repr(u32)]` enum). We match
// on `u32` rather than transmuting so an unknown OS-delivered value can
// never become UB.
mod etypes {
    pub const LEFT_MOUSE_DOWN: u32 = 1;
    pub const LEFT_MOUSE_UP: u32 = 2;
    pub const RIGHT_MOUSE_DOWN: u32 = 3;
    pub const RIGHT_MOUSE_UP: u32 = 4;
    pub const MOUSE_MOVED: u32 = 5;
    pub const LEFT_MOUSE_DRAGGED: u32 = 6;
    pub const RIGHT_MOUSE_DRAGGED: u32 = 7;
    pub const KEY_DOWN: u32 = 10;
    pub const KEY_UP: u32 = 11;
    pub const FLAGS_CHANGED: u32 = 12;
    pub const SCROLL_WHEEL: u32 = 22;
    pub const OTHER_MOUSE_DOWN: u32 = 25;
    pub const OTHER_MOUSE_UP: u32 = 26;
    pub const OTHER_MOUSE_DRAGGED: u32 = 27;
    pub const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
    pub const TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;
}

/// Signature of the `extern "C"` callback CGEventTap invokes.
type TapCallback = extern "C" fn(
    proxy: CGEventTapProxy,
    etype: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: CGEventMask,
        callback: TapCallback,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);

    // Read accessors used inside the callback. We avoid wrapping the
    // borrowed CGEventRef in the `core_graphics::event::CGEvent` newtype
    // because that requires the `foreign_types::ForeignType` trait to be
    // in scope, which would mean another workspace dep. The C symbols
    // are stable since 10.4.
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventGetFlags(event: CGEventRef) -> u64;
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
}

/// Tiny safe wrappers around the raw event-field FFI. All take a
/// borrowed `CGEventRef`; none of them release the ref.
fn ev_int_field(event: CGEventRef, field: u32) -> i64 {
    // SAFETY: `event` is non-null at every callsite (callback checks
    // before invoking translation), and `field` is a documented
    // `CGEventField` constant from the `EventField` module.
    unsafe { CGEventGetIntegerValueField(event, field) }
}

fn ev_flags(event: CGEventRef) -> CGEventFlags {
    // SAFETY: same as above.
    let bits = unsafe { CGEventGetFlags(event) };
    CGEventFlags::from_bits_truncate(bits)
}

fn ev_location(event: CGEventRef) -> CGPoint {
    // SAFETY: same as above. `CGPoint` is a plain `#[repr(C)]` struct
    // of two `f64`s — ABI-stable across the C boundary.
    unsafe { CGEventGetLocation(event) }
}

// ---------------------------------------------------------------------------
// Public handle.
// ---------------------------------------------------------------------------

/// Owns the tap thread, watcher thread, and the `Arc<AtomicBool>` that
/// the edge state machine flips to toggle event swallowing.
pub struct MacInputSource {
    rx: mpsc::UnboundedReceiver<SourceEvent>,
    swallow: Arc<AtomicBool>,
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

        let swallow = Arc::new(AtomicBool::new(false));

        // Shared mach-port ref so the callback can re-enable on tap-disabled
        // events. The value is set exactly once, immediately after
        // CGEventTapCreate succeeds.
        let port_holder: Arc<OnceLock<usize>> = Arc::new(OnceLock::new());

        let tx_for_thread = event_tx;
        let swallow_for_thread = Arc::clone(&swallow);
        let port_for_thread = Arc::clone(&port_holder);

        let tap_thread = thread::Builder::new()
            .name("kmwarp-cgtap".into())
            .spawn(move || {
                // Build the state struct on this thread, leak it into a
                // raw pointer for `CGEventTapCreate`'s `user_info`. We
                // reclaim and drop it just before the thread exits.
                let state = Box::new(TapState {
                    tx: tx_for_thread,
                    swallow: swallow_for_thread,
                    port_holder: Arc::clone(&port_for_thread),
                    held_mods: Cell::new(0),
                });
                let state_ptr = Box::into_raw(state) as *mut c_void;

                // SAFETY: extern call with valid types + a non-null
                // user_info we own. Returns a non-null CFMachPortRef on
                // success, null on failure (TCC denied etc.).
                let mach_port_ref = unsafe {
                    CGEventTapCreate(
                        TAP_LOC_HID,
                        TAP_PLACE_HEAD_INSERT,
                        TAP_OPT_DEFAULT,
                        make_event_mask(),
                        cb_thunk,
                        state_ptr,
                    )
                };
                if mach_port_ref.is_null() {
                    // SAFETY: we just leaked this in this thread; CG
                    // never stored it (tap create failed); reclaim.
                    unsafe {
                        let _ = Box::from_raw(state_ptr as *mut TapState);
                    }
                    let _ = rl_tx.send(Err(TapError::TapCreateFailed));
                    return;
                }

                // SAFETY: `CGEventTapCreate` returns the mach port with
                // create-rule retain (refcount +1, we own one). Wrap it
                // in CFMachPort which will CFRelease on Drop.
                let port = unsafe { CFMachPort::wrap_under_create_rule(mach_port_ref) };

                let loop_source = match port.create_runloop_source(0) {
                    Ok(s) => s,
                    Err(()) => {
                        // SAFETY: ditto; we own the leak.
                        unsafe {
                            let _ = Box::from_raw(state_ptr as *mut TapState);
                        }
                        let _ = rl_tx.send(Err(TapError::RunLoopFailed));
                        return;
                    }
                };
                let current = CFRunLoop::get_current();
                // SAFETY: `kCFRunLoopCommonModes` is a CoreFoundation
                // extern static; access is unsafe but the value is
                // read-only and valid for the lifetime of the process.
                let mode = unsafe { kCFRunLoopCommonModes };
                current.add_source(&loop_source, mode);

                // Publish the mach-port ref so the callback can re-enable
                // the tap on disabled-by-timeout / disabled-by-user-input.
                let port_ref = port.as_concrete_TypeRef() as usize;
                let _ = port_for_thread.set(port_ref);

                // SAFETY: pointer comes from a live CFMachPort owned by
                // this thread. `CGEventTapEnable` is the documented way
                // to enable the tap.
                unsafe { CGEventTapEnable(port_ref as *mut c_void, true) };

                if rl_tx.send(Ok(current)).is_err() {
                    // Installer was dropped before we got here.
                    unsafe {
                        CGEventTapEnable(port_ref as *mut c_void, false);
                        let _ = Box::from_raw(state_ptr as *mut TapState);
                    }
                    return;
                }

                CFRunLoop::run_current();
                debug!("kmwarp-cgtap thread exiting CFRunLoop");

                // Disable the tap before freeing the user_info, otherwise
                // a pending callback could dereference freed memory.
                unsafe {
                    CGEventTapEnable(port_ref as *mut c_void, false);
                }
                drop(port); // CFRelease

                // Reclaim the leaked TapState.
                // SAFETY: we created this Box in this thread above; no
                // callback can fire after the tap is disabled + port
                // released, so no aliasing.
                unsafe {
                    let _ = Box::from_raw(state_ptr as *mut TapState);
                }
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
            swallow,
            shutdown: Some(shutdown_tx),
            watcher: Some(watcher),
            tap_thread: Some(tap_thread),
        })
    }

    /// Get a handle to the swallow flag. The edge state machine flips
    /// this on `StartSwallow` / `StopSwallow` actions. Cheap clone (`Arc`).
    pub fn swallow_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.swallow)
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

// ---------------------------------------------------------------------------
// Callback state (leaked into CGEventTap's user_info).
// ---------------------------------------------------------------------------

struct TapState {
    tx: mpsc::UnboundedSender<SourceEvent>,
    swallow: Arc<AtomicBool>,
    port_holder: Arc<OnceLock<usize>>,
    /// Per-modifier-keycode held bitmap. See `translate_flags_changed`
    /// for the index layout. Single-threaded access from the tap thread.
    held_mods: Cell<u32>,
}

// SAFETY: `TapState` is only ever dereferenced from the single run-loop
// thread (`CGEventTapCreate` callbacks fire on the thread that
// registered the tap with the CFRunLoop). The `Cell<u32>` and
// `UnboundedSender` fields are touched only there; the `Arc`s are Sync.
// The Sync impl is needed because we share `&TapState` from the
// extern "C" thunk; the borrow is single-threaded so there's no actual
// concurrent access.
unsafe impl Sync for TapState {}

/// Build the bitmask passed to `CGEventTapCreate`. Bit `n` set means
/// the OS should deliver event type `n` to the tap. Matches the
/// `CGEventMaskBit` C macro.
fn make_event_mask() -> CGEventMask {
    let types = [
        // Pointer motion
        etypes::MOUSE_MOVED,
        etypes::LEFT_MOUSE_DRAGGED,
        etypes::RIGHT_MOUSE_DRAGGED,
        etypes::OTHER_MOUSE_DRAGGED,
        // Buttons
        etypes::LEFT_MOUSE_DOWN,
        etypes::LEFT_MOUSE_UP,
        etypes::RIGHT_MOUSE_DOWN,
        etypes::RIGHT_MOUSE_UP,
        etypes::OTHER_MOUSE_DOWN,
        etypes::OTHER_MOUSE_UP,
        // Wheel
        etypes::SCROLL_WHEEL,
        // Keyboard
        etypes::KEY_DOWN,
        etypes::KEY_UP,
        etypes::FLAGS_CHANGED,
    ];
    let mut mask: CGEventMask = 0;
    for t in types {
        mask |= 1u64 << u64::from(t);
    }
    mask
}

/// The `extern "C"` callback CGEventTap invokes. Translates the event,
/// emits CursorAt for motion, and either passes the event through or
/// returns NULL to swallow depending on the swallow flag + event type.
extern "C" fn cb_thunk(
    _proxy: CGEventTapProxy,
    etype: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    if user_info.is_null() {
        // Should never happen — we always pass a valid pointer — but
        // defend so a bug doesn't crash the system run loop.
        return event;
    }
    // SAFETY: user_info is the `Box<TapState>` we leaked in
    // `MacInputSource::install`. The Box lives until the tap thread
    // exits, which only happens after `CGEventTapEnable(_, false)` has
    // been called on this tap — so no callback can fire after the Box
    // is reclaimed.
    let state: &TapState = unsafe { &*(user_info as *const TapState) };

    // Tap-disabled comes before everything else: the OS asks us to
    // re-enable the tap (it auto-disables on long callback latency or
    // explicit user input). Always pass through.
    if etype == etypes::TAP_DISABLED_BY_TIMEOUT || etype == etypes::TAP_DISABLED_BY_USER_INPUT {
        if let Some(&mp) = state.port_holder.get() {
            // SAFETY: `mp` originated from a live `CFMachPort` owned by
            // the tap thread; it's still alive (we're inside its
            // callback).
            unsafe { CGEventTapEnable(mp as *mut c_void, true) };
            warn!(etype, "CGEventTap auto-reenabled after OS-disable");
        } else {
            error!("tap-disabled arrived before mach port was published");
        }
        return event;
    }

    if event.is_null() {
        return event;
    }

    if let Some(src_ev) = translate(etype, event, &state.held_mods) {
        let _ = state.tx.send(src_ev);
    }
    // M6: emit absolute `CursorAt` for motion events so the state
    // machine can detect right-edge crossings. Always emit (even while
    // swallowing) — without it the SM can't tell when control should
    // return to local.
    if is_mouse_motion(etype) {
        let loc = ev_location(event);
        let _ = state.tx.send(SourceEvent::CursorAt {
            x: loc.x as i32,
            y: loc.y as i32,
        });
    }

    // M6 swallow: in `RemoteActive` the local OS must not see the
    // user's input. Translation+send already happened above so the SM's
    // stuck-key tracker stays consistent across the transition.
    if state.swallow.load(Ordering::Relaxed) && is_swallowable(etype) {
        return ptr::null_mut();
    }
    event
}

/// Mouse-motion event types — the only ones whose absolute location
/// matters for the edge state machine. Button up/down events also carry
/// a location but the spec triggers crossings off motion only.
fn is_mouse_motion(etype: u32) -> bool {
    matches!(
        etype,
        etypes::MOUSE_MOVED
            | etypes::LEFT_MOUSE_DRAGGED
            | etypes::RIGHT_MOUSE_DRAGGED
            | etypes::OTHER_MOUSE_DRAGGED
    )
}

/// Input event types that the swallow flag suppresses. We swallow
/// modifier flags-changed too — the user's modifiers should not leak
/// to local apps while controlling the remote machine.
fn is_swallowable(etype: u32) -> bool {
    matches!(
        etype,
        etypes::MOUSE_MOVED
            | etypes::LEFT_MOUSE_DRAGGED
            | etypes::RIGHT_MOUSE_DRAGGED
            | etypes::OTHER_MOUSE_DRAGGED
            | etypes::LEFT_MOUSE_DOWN
            | etypes::LEFT_MOUSE_UP
            | etypes::RIGHT_MOUSE_DOWN
            | etypes::RIGHT_MOUSE_UP
            | etypes::OTHER_MOUSE_DOWN
            | etypes::OTHER_MOUSE_UP
            | etypes::SCROLL_WHEEL
            | etypes::KEY_DOWN
            | etypes::KEY_UP
            | etypes::FLAGS_CHANGED
    )
}

fn translate(etype: u32, event: CGEventRef, held_mods: &Cell<u32>) -> Option<SourceEvent> {
    match etype {
        etypes::MOUSE_MOVED
        | etypes::LEFT_MOUSE_DRAGGED
        | etypes::RIGHT_MOUSE_DRAGGED
        | etypes::OTHER_MOUSE_DRAGGED => {
            let dx = ev_int_field(event, EventField::MOUSE_EVENT_DELTA_X);
            let dy = ev_int_field(event, EventField::MOUSE_EVENT_DELTA_Y);
            Some(SourceEvent::MouseRel {
                dx: clamp_i64_to_i16(dx),
                dy: clamp_i64_to_i16(dy),
            })
        }
        etypes::LEFT_MOUSE_DOWN => Some(SourceEvent::MouseButton {
            button: MouseButton::Left,
            state: KeyState::Down,
        }),
        etypes::LEFT_MOUSE_UP => Some(SourceEvent::MouseButton {
            button: MouseButton::Left,
            state: KeyState::Up,
        }),
        etypes::RIGHT_MOUSE_DOWN => Some(SourceEvent::MouseButton {
            button: MouseButton::Right,
            state: KeyState::Down,
        }),
        etypes::RIGHT_MOUSE_UP => Some(SourceEvent::MouseButton {
            button: MouseButton::Right,
            state: KeyState::Up,
        }),
        etypes::OTHER_MOUSE_DOWN => Some(SourceEvent::MouseButton {
            button: other_button(event),
            state: KeyState::Down,
        }),
        etypes::OTHER_MOUSE_UP => Some(SourceEvent::MouseButton {
            button: other_button(event),
            state: KeyState::Up,
        }),
        etypes::SCROLL_WHEEL => {
            let dy = ev_int_field(event, EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1);
            let dx = ev_int_field(event, EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2);
            Some(SourceEvent::MouseWheel {
                dx: clamp_i64_to_i16(dx),
                dy: clamp_i64_to_i16(dy),
            })
        }
        etypes::KEY_DOWN => translate_key(event, KeyState::Down),
        etypes::KEY_UP => translate_key(event, KeyState::Up),
        etypes::FLAGS_CHANGED => translate_flags_changed(event, held_mods),
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
fn translate_key(event: CGEventRef, state: KeyState) -> Option<SourceEvent> {
    // Autorepeat is only meaningful for KeyDown, but the field is also zero
    // on KeyUp so we can read it unconditionally and the check is a no-op
    // for releases. Keeps both arms symmetric.
    if state == KeyState::Down && ev_int_field(event, EventField::KEYBOARD_EVENT_AUTOREPEAT) != 0 {
        return None;
    }
    let cg_vk = ev_int_field(event, EventField::KEYBOARD_EVENT_KEYCODE) as u16;
    let hid = match macos_to_hid(cg_vk) {
        Some(h) => h,
        None => {
            trace!(?cg_vk, ?state, "unmapped macOS VK; dropping key event");
            return None;
        }
    };
    let mods = mods_from_flags(ev_flags(event));
    Some(SourceEvent::Key {
        hid_usage: hid,
        state,
        mods,
    })
}

/// Translate `kCGEventFlagsChanged` into a `SourceEvent::Key { state: … }`.
///
/// macOS doesn't tell us up vs. down directly on a flags-changed event;
/// it delivers the keycode of the modifier that just transitioned and the
/// *new* aggregate `CGEventFlags`. The aggregate XOR is ambiguous when
/// one side of a paired modifier is already held (press RShift while
/// LShift is down — aggregate Shift bit doesn't flip), so we keep a
/// per-keycode held-bitmap instead.
fn translate_flags_changed(event: CGEventRef, held: &Cell<u32>) -> Option<SourceEvent> {
    let cg_vk = ev_int_field(event, EventField::KEYBOARD_EVENT_KEYCODE) as u16;
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
    let mods = mods_from_flags(ev_flags(event));
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
/// v1 HID table and forwarding it would only confuse downstream consumers.
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

fn other_button(event: CGEventRef) -> MouseButton {
    let btn = ev_int_field(event, EventField::MOUSE_EVENT_BUTTON_NUMBER);
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
        let vks = [0x38, 0x3C, 0x3B, 0x3E, 0x3A, 0x3D, 0x37, 0x36, 0x39];
        let mut seen = std::collections::HashSet::new();
        for vk in vks {
            let bit = mod_bit(vk).expect("tracked modifier vk should map");
            assert!(bit < 9, "bit index {bit} out of range");
            assert!(seen.insert(bit), "duplicate bit index for vk 0x{vk:X}");
        }
        assert_eq!(mod_bit(0x00), None);
        assert_eq!(mod_bit(0x3F), None);
    }

    #[test]
    fn flags_changed_held_tracker_flips_state_per_keycode() {
        let held: Cell<u32> = Cell::new(0);

        let bit_l = 1u32 << mod_bit(0x38).unwrap();
        let prev = held.get();
        let was_held = (prev & bit_l) != 0;
        held.set(prev ^ bit_l);
        assert!(!was_held);
        assert_eq!(held.get(), bit_l);

        let bit_r = 1u32 << mod_bit(0x3C).unwrap();
        let prev = held.get();
        let was_held = (prev & bit_r) != 0;
        held.set(prev ^ bit_r);
        assert!(!was_held);
        assert_eq!(held.get(), bit_l | bit_r);

        let prev = held.get();
        let was_held = (prev & bit_r) != 0;
        held.set(prev ^ bit_r);
        assert!(was_held);
        assert_eq!(held.get(), bit_l);

        let prev = held.get();
        let was_held = (prev & bit_l) != 0;
        held.set(prev ^ bit_l);
        assert!(was_held);
        assert_eq!(held.get(), 0);
    }

    #[test]
    fn mods_from_flags_ignores_non_chord_bits() {
        let noise = CGEventFlags::CGEventFlagAlphaShift
            | CGEventFlags::CGEventFlagHelp
            | CGEventFlags::CGEventFlagSecondaryFn
            | CGEventFlags::CGEventFlagNumericPad;
        assert_eq!(mods_from_flags(noise), ModMask::default());
    }

    #[test]
    fn is_swallowable_covers_all_input_event_types() {
        // Spot-check: every event type the interest mask subscribes to
        // is also "swallowable" — otherwise RemoteActive would leak
        // events to local apps.
        let mask = make_event_mask();
        for etype in 0u32..=27u32 {
            if (mask & (1u64 << u64::from(etype))) != 0 {
                assert!(
                    is_swallowable(etype),
                    "etype {etype} is subscribed but not swallowable"
                );
            }
        }
    }

    #[test]
    fn is_mouse_motion_only_motion_events() {
        assert!(is_mouse_motion(etypes::MOUSE_MOVED));
        assert!(is_mouse_motion(etypes::LEFT_MOUSE_DRAGGED));
        assert!(is_mouse_motion(etypes::RIGHT_MOUSE_DRAGGED));
        assert!(is_mouse_motion(etypes::OTHER_MOUSE_DRAGGED));

        assert!(!is_mouse_motion(etypes::LEFT_MOUSE_DOWN));
        assert!(!is_mouse_motion(etypes::SCROLL_WHEEL));
        assert!(!is_mouse_motion(etypes::KEY_DOWN));
        assert!(!is_mouse_motion(etypes::FLAGS_CHANGED));
    }
}
