//! `NSPasteboard`-backed implementation of [`kmwarp_core::Clipboard`].
//!
//! ## Watcher: dedicated thread, 4 Hz poll
//!
//! macOS doesn't deliver a "clipboard changed" notification to
//! background apps — `NSPasteboardDidChangeNotification` only fires for
//! the foreground process, which we are not. So per spec §M8 + PLAN.md
//! §M8, we poll `NSPasteboard.changeCount` at 4 Hz from a dedicated
//! `std::thread` and emit a [`ClipboardEvent::TextChanged`] whenever
//! the count differs from the last seen value.
//!
//! A dedicated `std::thread` (not a tokio task) avoids two problems:
//!   1. NSPasteboard's `Retained<NSPasteboard>` is not `Send` —
//!      keeping it inside one thread sidesteps the question.
//!   2. We never need to grab the pasteboard from across thread
//!      boundaries because the thread fetches `generalPasteboard()`
//!      fresh each tick (it's a cached singleton inside AppKit; the
//!      call is O(1)).
//!
//! ## Reads / writes from outside the watcher
//!
//! [`pasteboard_write`] and [`pasteboard_read`] are free-standing
//! functions that any thread can call. Each call fetches
//! `generalPasteboard()` and operates on it inline. NSPasteboard's
//! `setString:forType:` / `stringForType:` are documented thread-safe.
//! The cost is one Objective-C dispatch per call — negligible.
//!
//! ## Trait impl
//!
//! [`NsPasteboardClipboard`] owns the watcher and the receive half of
//! the mpsc channel. The trait's `read_text` / `write_text` delegate
//! to the free functions; `next_change` pulls from the channel.

use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;

use async_trait::async_trait;
use kmwarp_core::platform::{Clipboard, ClipboardEvent};
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

/// Poll period for the changeCount watcher. Spec §M8 mandates 4 Hz.
const POLL_PERIOD: Duration = Duration::from_millis(250);

/// Outbound channel bound. Clipboard events are rare (4 Hz max, only
/// on a real change) so a small bound suffices.
const EVENT_CHANNEL_BOUND: usize = 64;

/// Errors raised while bringing up the clipboard watcher.
#[derive(Debug, Error)]
pub enum ClipboardError {
    /// Failed to spawn the dedicated poller thread.
    #[error("failed to initialize NSPasteboard watcher: {0}")]
    Init(String),
}

/// Public handle: receives [`ClipboardEvent`]s emitted by the
/// dedicated poller thread, and exposes `read_text` / `write_text` via
/// the trait.
pub struct NsPasteboardClipboard {
    rx: mpsc::Receiver<ClipboardEvent>,
    // Dropping this Sender wakes the watcher thread, which exits.
    // `Option` so `Drop` can take it before joining.
    shutdown: Option<std_mpsc::Sender<()>>,
    watcher: Option<thread::JoinHandle<()>>,
}

impl NsPasteboardClipboard {
    /// Spawn the watcher thread and return a handle. Blocks briefly on
    /// `thread::Builder::spawn`; everything else is async.
    pub fn install() -> Result<Self, ClipboardError> {
        let (tx, rx) = mpsc::channel::<ClipboardEvent>(EVENT_CHANNEL_BOUND);
        let (shutdown_tx, shutdown_rx) = std_mpsc::channel::<()>();

        let watcher = thread::Builder::new()
            .name("kmwarp-pasteboard".into())
            .spawn(move || watch_loop(tx, shutdown_rx))
            .map_err(|e| ClipboardError::Init(e.to_string()))?;

        debug!("NSPasteboard watcher started");
        Ok(Self {
            rx,
            shutdown: Some(shutdown_tx),
            watcher: Some(watcher),
        })
    }
}

impl Drop for NsPasteboardClipboard {
    fn drop(&mut self) {
        // Signal shutdown and join. The thread checks the shutdown
        // channel between ticks, so worst-case latency is one POLL_PERIOD.
        drop(self.shutdown.take());
        if let Some(h) = self.watcher.take() {
            let _ = h.join();
        }
    }
}

#[async_trait]
impl Clipboard for NsPasteboardClipboard {
    fn read_text(&self) -> Option<String> {
        pasteboard_read()
    }

    fn write_text(&mut self, s: &str) {
        pasteboard_write(s);
    }

    async fn next_change(&mut self) -> Option<ClipboardEvent> {
        self.rx.recv().await
    }
}

/// Read the current `NSPasteboardTypeString` value from
/// `generalPasteboard`. Returns `None` when the pasteboard either has
/// no string contents (image-only, etc.) or the read fails.
///
/// Safe to call from any thread.
pub fn pasteboard_read() -> Option<String> {
    // SAFETY: `generalPasteboard` is a documented Objective-C call that
    // returns a +0 autoreleased object; we wrap into `Retained`
    // via objc2's accessor. `stringForType:` is documented thread-safe.
    unsafe {
        let pb = NSPasteboard::generalPasteboard();
        let s = pb.stringForType(NSPasteboardTypeString)?;
        Some(s.to_string())
    }
}

/// Write `s` to `generalPasteboard` as `NSPasteboardTypeString`,
/// clearing prior contents first per Apple's contract.
///
/// Safe to call from any thread.
pub fn pasteboard_write(s: &str) {
    // SAFETY: see `pasteboard_read`. `setString:forType:` returns a
    // boolean success indicator; on failure we log and continue.
    unsafe {
        let pb = NSPasteboard::generalPasteboard();
        let _generation = pb.clearContents();
        let ns = NSString::from_str(s);
        let ok = pb.setString_forType(&ns, NSPasteboardTypeString);
        if !ok {
            warn!(
                len = s.len(),
                "NSPasteboard setString:forType: returned false"
            );
        }
    }
}

/// Watcher loop. Polls `changeCount` every [`POLL_PERIOD`] and emits a
/// `TextChanged` event when the count moves AND a string is present.
///
/// Exits when the matching `Sender` to `shutdown_rx` is dropped.
fn watch_loop(tx: mpsc::Sender<ClipboardEvent>, shutdown_rx: std_mpsc::Receiver<()>) {
    // Seed `last_cc` with the *current* value so we don't emit a
    // synthetic "changed" event on startup for whatever was already on
    // the pasteboard from before the server launched.
    let mut last_cc = unsafe { NSPasteboard::generalPasteboard().changeCount() };
    trace!(last_cc, "NSPasteboard watch_loop online");

    loop {
        // Wait up to POLL_PERIOD for a shutdown signal; the recv_timeout
        // doubles as our sleep, so we wake up immediately on shutdown
        // rather than sitting through the rest of a 250 ms sleep.
        match shutdown_rx.recv_timeout(POLL_PERIOD) {
            Ok(()) | Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                debug!("NSPasteboard watcher exiting (shutdown signaled)");
                return;
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {} // normal tick
        }

        // SAFETY: see `pasteboard_read`.
        let (cc, text) = unsafe {
            let pb = NSPasteboard::generalPasteboard();
            let cc = pb.changeCount();
            let text = pb
                .stringForType(NSPasteboardTypeString)
                .map(|s| s.to_string());
            (cc, text)
        };

        if cc == last_cc {
            continue;
        }
        last_cc = cc;
        let Some(s) = text else {
            // Pasteboard changed but contains no string (e.g. an image
            // copy). v1 is text-only; ignore.
            trace!(cc, "pasteboard changeCount moved but no string content");
            continue;
        };
        // Use blocking_send: we're on a std::thread, not inside a tokio
        // runtime. Drops the event silently if the consumer is gone.
        if tx.blocking_send(ClipboardEvent::TextChanged(s)).is_err() {
            debug!("clipboard event consumer dropped; watcher exiting");
            return;
        }
    }
}
