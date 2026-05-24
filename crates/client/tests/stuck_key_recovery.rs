//! M7 stuck-key recovery — client-side integration test.
//!
//! Per spec §M7 + PLAN.md §Verification "Stuck-key reproduction (M7)":
//! if the connection drops while the user is holding keys on the Mac,
//! the Windows client must synthesize the missing key-ups locally before
//! the injector task exits — otherwise Windows is left sitting on a
//! stuck Shift (or any other held modifier).
//!
//! ## What this test proves
//!
//! 1. A fake `FrameSource` feeds the client 5 `KeyEvent { LSHIFT, Down }`
//!    frames, then returns `Err(ClientError::Disconnected)` to simulate
//!    a hard TCP loss mid-hold.
//! 2. The injector loop runs to completion against that source plus a
//!    `RecorderSink` that captures every `inject_key` call.
//! 3. The recorded calls must contain the 5 originating `Down`s plus at
//!    least one synthesized `Up` for LSHIFT before the function returns.
//!
//! Deterministic by construction — no real timers in the assertion path,
//! only a `tokio::time::timeout` as a backstop so the test doesn't hang
//! CI if the drain regresses.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use kmwarp_client::error::ClientError;
use kmwarp_client::net::{injector_loop_with_source, FrameSource};
use kmwarp_core::clipboard::EchoGuard;
use kmwarp_core::hid::usage;
use kmwarp_core::wire::{key_state_code, Message};
use kmwarp_core::{InputSink, KeyState, ModMask, MouseButton};
use tokio::sync::{mpsc, Notify};

/// Records every `inject_key` call so the test can assert on the sequence.
/// Cheap to clone (Arc) so one handle stays in the test for assertions
/// while another lives inside the injector via the sink.
#[derive(Default)]
struct RecorderSink {
    calls: Arc<Mutex<Vec<(u16, KeyState)>>>,
}

impl RecorderSink {
    fn new() -> Self {
        Self::default()
    }

    fn handle(&self) -> Arc<Mutex<Vec<(u16, KeyState)>>> {
        Arc::clone(&self.calls)
    }
}

impl InputSink for RecorderSink {
    fn inject_mouse_rel(&mut self, _dx: i32, _dy: i32) {}
    fn inject_mouse_button(&mut self, _btn: MouseButton, _state: KeyState) {}
    fn inject_mouse_wheel(&mut self, _dx: i16, _dy: i16) {}
    fn inject_key(&mut self, hid: u16, state: KeyState, _mods: ModMask) {
        self.calls
            .lock()
            .expect("recorder mutex poisoned")
            .push((hid, state));
    }
    fn warp_cursor_abs(&mut self, _x: i32, _y: i32) {}
    fn hide_cursor(&mut self) {}
    fn show_cursor(&mut self) {}
}

/// Scripted frame source: emits a fixed queue, then returns
/// `Err(Disconnected)` to simulate the TCP-loss path the injector
/// needs to drain on.
struct ScriptedSource {
    remaining: std::collections::VecDeque<Message>,
}

impl ScriptedSource {
    fn new(frames: Vec<Message>) -> Self {
        Self {
            remaining: frames.into(),
        }
    }
}

#[async_trait]
impl FrameSource for ScriptedSource {
    async fn next_frame(&mut self) -> Result<Message, ClientError> {
        match self.remaining.pop_front() {
            Some(msg) => Ok(msg),
            None => Err(ClientError::Disconnected),
        }
    }
}

#[tokio::test]
async fn stuck_key_recovery_drains_held_keys_on_disconnect() {
    // 1. Scripted source: 5 Shift-down frames, then EOF.
    let frames: Vec<Message> = (0..5)
        .map(|_| Message::KeyEvent {
            hid_usage: usage::LEFT_SHIFT,
            state: key_state_code::DOWN,
            modifiers: 0,
        })
        .collect();
    let source = ScriptedSource::new(frames);

    // 2. Recorder sink. Hold a handle for post-run assertions.
    let sink = RecorderSink::new();
    let recorded = sink.handle();

    // Plumbing the injector expects but the test doesn't observe.
    let (tx_out, _rx_out) = mpsc::channel::<Message>(16);
    let notify = Arc::new(Notify::new());
    let active = Arc::new(AtomicBool::new(false));

    // 3. Run with a backstop timeout. The injector should return Err
    //    (Disconnected) almost immediately after consuming the 5 frames,
    //    and the drain runs synchronously before that return.
    let echo_guard = Arc::new(Mutex::new(EchoGuard::new()));
    let run = injector_loop_with_source(source, sink, notify, tx_out, active, echo_guard);
    let result = tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .expect("injector hung past 3s backstop");

    // The disconnect is the expected exit path.
    assert!(
        matches!(result, Err(ClientError::Disconnected)),
        "expected ClientError::Disconnected, got {result:?}",
    );

    // 4. Assert on the recorded sequence.
    let calls = recorded.lock().expect("recorder mutex poisoned").clone();

    let downs: Vec<_> = calls
        .iter()
        .filter(|(hid, st)| *hid == usage::LEFT_SHIFT && *st == KeyState::Down)
        .collect();
    let ups: Vec<_> = calls
        .iter()
        .filter(|(hid, st)| *hid == usage::LEFT_SHIFT && *st == KeyState::Up)
        .collect();

    assert_eq!(
        downs.len(),
        5,
        "expected 5 LSHIFT downs, got {} (full transcript: {:?})",
        downs.len(),
        calls,
    );
    assert!(
        !ups.is_empty(),
        "M7 stuck-key invariant violated: no synthesized LSHIFT up after disconnect \
         (full transcript: {calls:?})",
    );

    // Ordering invariant: the drain must come AFTER the last down — the
    // injector cannot synthesize an Up before processing the inputs.
    let last_down_idx = calls
        .iter()
        .rposition(|(hid, st)| *hid == usage::LEFT_SHIFT && *st == KeyState::Down)
        .expect("at least one down was just asserted");
    let first_up_idx = calls
        .iter()
        .position(|(hid, st)| *hid == usage::LEFT_SHIFT && *st == KeyState::Up)
        .expect("at least one up was just asserted");
    assert!(
        first_up_idx > last_down_idx,
        "synthesized Up came BEFORE last Down — drain ran out of order \
         (last_down={last_down_idx}, first_up={first_up_idx}, calls={calls:?})",
    );
}

#[tokio::test]
async fn no_drain_when_held_set_is_balanced_before_disconnect() {
    // Regression guard: if every Down has a matching Up before the
    // disconnect, the drain must not synthesize phantom Ups.
    let source = ScriptedSource::new(vec![
        Message::KeyEvent {
            hid_usage: usage::LEFT_SHIFT,
            state: key_state_code::DOWN,
            modifiers: 0,
        },
        Message::KeyEvent {
            hid_usage: usage::LEFT_SHIFT,
            state: key_state_code::UP,
            modifiers: 0,
        },
    ]);

    let sink = RecorderSink::new();
    let recorded = sink.handle();

    let (tx_out, _rx_out) = mpsc::channel::<Message>(16);
    let notify = Arc::new(Notify::new());
    let active = Arc::new(AtomicBool::new(false));

    let echo_guard = Arc::new(Mutex::new(EchoGuard::new()));
    let run = injector_loop_with_source(source, sink, notify, tx_out, active, echo_guard);
    let _ = tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .expect("injector hung past 3s backstop");

    let calls = recorded.lock().expect("recorder mutex poisoned").clone();
    let ups: Vec<_> = calls
        .iter()
        .filter(|(hid, st)| *hid == usage::LEFT_SHIFT && *st == KeyState::Up)
        .collect();

    // Exactly one Up: the one the wire delivered. The drain saw an
    // empty held set and was a no-op.
    assert_eq!(
        ups.len(),
        1,
        "expected exactly 1 LSHIFT up (the wire-delivered one); \
         drain leaked extras: {calls:?}",
    );
}
