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

use std::ffi::c_void;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, OnceLock};
use std::thread;

use async_trait::async_trait;
use core_foundation::base::TCFType;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    EventField,
};
use kmwarp_core::platform::{InputSource, KeyState, MouseButton, SourceEvent};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

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
                let tap_result = CGEventTap::new(
                    CGEventTapLocation::HID,
                    CGEventTapPlacement::HeadInsertEventTap,
                    CGEventTapOptions::Default,
                    interest_mask(),
                    move |_proxy, etype, event| callback(etype, event, &event_tx, &port_for_cb),
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
    if let Some(ev) = translate(etype, event) {
        // Unbounded send; only fails if the receiver was dropped (consumer
        // gone). In that case the source is being torn down and the run
        // loop will stop shortly via the shutdown watcher.
        let _ = tx.send(ev);
    }
    // Pass-through: M2 never swallows. M6 will revisit.
    None
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

fn translate(etype: CGEventType, event: &CGEvent) -> Option<SourceEvent> {
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
        // M5 wires these in; M2 ignores them so the channel only carries
        // mouse events for the acceptance test.
        KeyDown | KeyUp | FlagsChanged => None,
        _ => None,
    }
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
}
