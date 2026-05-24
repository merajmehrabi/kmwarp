//! Client-side encoder + injector pumps (M4, with M5 + M6 + M7 layered on).
//!
//! - [`encoder_loop`] is the mirror of the server's — it owns the
//!   [`FrameWriter`] and drains a bounded `mpsc::Receiver<Message>` of
//!   outbound frames (heartbeats, `EchoPong` responses, `ReleaseControl`).
//! - [`injector_loop`] owns the [`FrameReader`] and an [`InputSink`]; for
//!   every incoming frame it pulses the deadline `Notify`, routes mouse +
//!   key variants to the sink, tracks held keys (M7), and responds to
//!   `EchoPing` by enqueuing a matching `EchoPong` into the outbound
//!   channel.
//!
//! ## M7 stuck-key safety
//!
//! Per spec §M7 + PLAN.md: every `KeyEvent { Down }` inserts the HID into
//! a session-local [`HeldKeys`] tracker, every `KeyEvent { Up }` removes
//! it. On injector exit — clean `Bye`, socket failure, *or* a
//! `tokio::task::abort()` from the JoinSet during teardown — the
//! [`InjectorGuard`]'s `Drop` impl synthesizes a local `KeyEvent { Up }`
//! through the sink for every still-held HID. This prevents Windows from
//! sitting on a stuck Shift after the Mac side disappears.
//!
//! The drain is synchronous (SendInput is sync), so it runs reliably even
//! inside the cancellation drop-chain.
//!
//! Wire-byte → enum conversion goes through `core::wire::convert` so
//! server and client cannot drift on `MouseButton.button` / `.state`
//! dictionary values.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use kmwarp_core::stuck_keys::HeldKeys;
use kmwarp_core::wire::{apply_key_to_sink, apply_mouse_to_sink, key_state_code, Message};
use kmwarp_core::{InputSink, KeyState, ModMask};
use tokio::sync::{mpsc, Notify};
use tracing::{debug, info, trace, warn};

use crate::error::ClientError;
use crate::net::{FrameReader, FrameWriter};

/// Drain `rx` into `writer` until the channel closes or the socket dies.
pub async fn encoder_loop(
    mut rx: mpsc::Receiver<Message>,
    mut writer: FrameWriter,
) -> Result<(), ClientError> {
    debug!("encoder_loop entered");
    while let Some(msg) = rx.recv().await {
        trace!(?msg, "encoder_loop writing frame");
        writer.write_frame(&msg).await?;
    }
    debug!("encoder_loop: outbound channel closed; exiting");
    Ok(())
}

/// RAII guard around the injector's `(HeldKeys, sink)` pair. Drains every
/// still-held key into a local `KeyEvent { Up }` on drop, which is the
/// M7 stuck-key recovery invariant.
///
/// Owning the sink (rather than borrowing) means the same `Drop` runs on
/// every exit path — early return, panic unwind, *and* `JoinSet::abort`
/// (which drops the spawned future). Tokio guarantees Drop impls in the
/// future stack run on cancellation, so this is the cheapest reliable
/// hook.
pub(crate) struct InjectorGuard<S: InputSink> {
    held: HeldKeys,
    sink: S,
}

impl<S: InputSink> InjectorGuard<S> {
    pub(crate) fn new(sink: S) -> Self {
        Self {
            held: HeldKeys::new(),
            sink,
        }
    }

    /// Mutable access to the held set for the tracker update.
    pub(crate) fn held_mut(&mut self) -> &mut HeldKeys {
        &mut self.held
    }

    /// Mutable access to the underlying sink for dispatch calls.
    pub(crate) fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }
}

impl<S: InputSink> Drop for InjectorGuard<S> {
    fn drop(&mut self) {
        if self.held.is_empty() {
            return;
        }
        let count = self.held.len();
        warn!(count, "draining held keys locally on injector exit (M7)");
        for hid in self.held.drain() {
            self.sink.inject_key(hid, KeyState::Up, ModMask::default());
        }
    }
}

/// Update [`HeldKeys`] for any `KeyEvent` carried in `msg`. Down inserts,
/// Up removes; unknown state bytes are left for [`apply_key_to_sink`] to
/// warn-and-drop.
pub(crate) fn track_key_in_held(msg: &Message, held: &mut HeldKeys) {
    if let Message::KeyEvent {
        hid_usage, state, ..
    } = msg
    {
        if *state == key_state_code::DOWN {
            held.insert(*hid_usage);
        } else if *state == key_state_code::UP {
            held.remove(*hid_usage);
        }
    }
}

/// Continuously decode frames from `reader`, pulse `notify`, dispatch
/// mouse/key to `sink`, bounce `EchoPing` back as `EchoPong` via
/// `tx_out`, and toggle `active` on `TakeControl` / `ReleaseControl` so
/// the M6 cursor-leave watcher (Windows-only) knows when to poll.
///
/// Generic over the sink so the same loop drives a real
/// [`crate::platform::WinInputSink`] on Windows and the
/// [`crate::sink::NoOpSink`] on other targets.
///
/// On any exit (Ok or Err) the [`InjectorGuard`] drains held keys
/// through the sink — see module-level docs for the M7 invariant.
///
/// Returns `Err` on socket / wire failure (treat as session-fatal); on a
/// peer `Bye` it returns `Ok(())` for a clean session end.
pub async fn injector_loop<S: InputSink + Send>(
    mut reader: FrameReader,
    sink: S,
    notify: Arc<Notify>,
    tx_out: mpsc::Sender<Message>,
    active: Arc<AtomicBool>,
) -> Result<(), ClientError> {
    let mut guard = InjectorGuard::new(sink);
    loop {
        let msg = reader.read_frame().await?;
        notify.notify_one();
        dispatch_one(&msg, &mut guard, &tx_out, &active);
        // The dispatch helper returns `Some(())` for "session done"
        // (peer Bye). Use a tagged enum for clarity:
        if matches!(msg, Message::Bye { .. }) {
            return Ok(());
        }
    }
}

/// Apply one decoded frame's side effects. Pure (no awaits, no I/O),
/// so the integration test can drive it via a fake source without
/// pulling in tokio I/O plumbing.
///
/// Held-key tracking happens *before* dispatch so the M7 invariant
/// holds even if the dispatch helpers fail or warn-drop.
pub(crate) fn dispatch_one<S: InputSink>(
    msg: &Message,
    guard: &mut InjectorGuard<S>,
    tx_out: &mpsc::Sender<Message>,
    active: &Arc<AtomicBool>,
) {
    // Update held set before any side effect — even if the sink call
    // panics, the tracker reflects what the wire ordered.
    track_key_in_held(msg, guard.held_mut());

    // Mouse-first because mouse events dominate the steady-state
    // packet rate (cursor motion is continuous; keypresses are rare).
    if apply_mouse_to_sink(msg, guard.sink_mut()) {
        return;
    }
    if apply_key_to_sink(msg, guard.sink_mut()) {
        return;
    }
    match msg {
        Message::Heartbeat { seq } => {
            trace!(seq, "received Heartbeat");
        }
        Message::EchoPing { ts_ns } => {
            let response = Message::EchoPong { ts_ns: *ts_ns };
            if let Err(e) = tx_out.try_send(response) {
                warn!(error = ?e, "failed to enqueue EchoPong");
            }
        }
        Message::EchoPong { .. } => {
            // The client never sends EchoPing in M4, so we shouldn't
            // see pongs. Log at trace in case a future server-side
            // bug bounces them at us.
            trace!(?msg, "received unsolicited EchoPong; ignoring");
        }
        Message::Bye { reason_code } => {
            info!(reason_code, "peer sent Bye; ending session");
        }
        Message::Hello { .. } | Message::HelloAck { .. } => {
            warn!(?msg, "unexpected post-handshake control frame");
        }
        Message::TakeControl { entry_y } => {
            info!(entry_y, "received TakeControl; activating");
            guard.sink_mut().warp_cursor_abs(0, i32::from(*entry_y));
            active.store(true, Ordering::Relaxed);
        }
        Message::ReleaseControl { .. } => {
            trace!(
                ?msg,
                "received ReleaseControl from server (unusual); ignoring"
            );
        }
        Message::ClipboardText { .. } => {
            trace!(?msg, "received frame; M8 will handle");
        }
        // `apply_mouse_to_sink` / `apply_key_to_sink` covered these
        // above; the compiler can't tell, so fall through.
        Message::MouseMoveRel { .. } | Message::MouseButton { .. } | Message::MouseWheel { .. } => {
            unreachable!("handled by apply_mouse_to_sink")
        }
        Message::KeyEvent { .. } => unreachable!("handled by apply_key_to_sink"),
    }
}
