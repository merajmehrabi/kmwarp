//! Clipboard sync helpers: chunking, reassembly, echo suppression.
//!
//! The wire protocol carries clipboard payloads as `Message::ClipboardText
//! { chunk_flags: u8, bytes: Vec<u8> }`. The codec's `u16` length field
//! tops out at 64 KiB-3 per frame, and v1 clipboard contents can exceed
//! that (long shell commands, code snippets, multi-page text). Chunking
//! across multiple `ClipboardText` frames is the M8 solution:
//!
//! - Sender: [`Chunker::split`] produces a `Vec<Message>` with `FIRST` /
//!   `LAST` flag bits set on the boundary frames. Single-frame payloads
//!   set both.
//! - Receiver: [`Reassembler::ingest`] feeds frames in arrival order and
//!   yields `Some(String)` once the `LAST` flag arrives.
//!
//! Both sides also need [`EchoGuard`] to avoid an infinite ping-pong:
//! after we write the clipboard in response to a remote frame, we
//! remember the SHA-256 hash of what we wrote; the next observed local
//! change whose hash matches gets suppressed.

use bitflags::bitflags;

use crate::wire::Message;

/// Maximum payload bytes per `ClipboardText` frame before chunking
/// kicks in. Picked to match the spec note "chunked if > 4 KiB"; the
/// wire `u16` length field could carry more but we cap here so a single
/// runaway clipboard write can't monopolize the encoder.
pub const CHUNK_SIZE: usize = 4 * 1024;

bitflags! {
    /// Flag bits the sender sets in `Message::ClipboardText.chunk_flags`.
    ///
    /// - `FIRST` marks the first frame of a (possibly multi-frame)
    ///   payload. The receiver resets its assembly buffer on this flag.
    /// - `LAST` marks the final frame; the receiver returns the
    ///   assembled string. A single-frame payload sets both bits.
    /// - Reserved bits 2-7 must be zero on the wire.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ChunkFlags: u8 {
        const FIRST = 0b0000_0001;
        const LAST  = 0b0000_0010;
    }
}

/// Splits a UTF-8 payload into a sequence of `Message::ClipboardText`
/// frames. Stateless; no instance state — the type exists so callers
/// can grep for it and to leave room for tuning knobs (e.g. a future
/// compression flag) without a signature change.
pub struct Chunker;

impl Chunker {
    /// Split `text` into one or more wire messages.
    ///
    /// - `text.len() <= CHUNK_SIZE` → one message with `FIRST | LAST`.
    /// - Larger → N messages; first has `FIRST`, last has `LAST`,
    ///   middle frames have neither flag bit set.
    ///
    /// Empty `text` produces a single frame with `FIRST | LAST` and
    /// empty `bytes` — receivers see "clipboard cleared to empty
    /// string". The caller decides whether to send that or skip.
    pub fn split(text: &str) -> Vec<Message> {
        let bytes = text.as_bytes();
        if bytes.len() <= CHUNK_SIZE {
            return vec![Message::ClipboardText {
                chunk_flags: (ChunkFlags::FIRST | ChunkFlags::LAST).bits(),
                bytes: bytes.to_vec(),
            }];
        }
        let chunks: Vec<&[u8]> = bytes.chunks(CHUNK_SIZE).collect();
        let last_idx = chunks.len() - 1;
        let mut out = Vec::with_capacity(chunks.len());
        for (i, chunk) in chunks.into_iter().enumerate() {
            let mut flags = ChunkFlags::empty();
            if i == 0 {
                flags |= ChunkFlags::FIRST;
            }
            if i == last_idx {
                flags |= ChunkFlags::LAST;
            }
            out.push(Message::ClipboardText {
                chunk_flags: flags.bits(),
                bytes: chunk.to_vec(),
            });
        }
        out
    }
}

/// Stateful frame-by-frame assembler. One per peer connection.
///
/// Holds a `Vec<u8>` buffer that grows as chunks arrive and is reset
/// (a) when a `FIRST`-flagged frame arrives mid-stream (defensive
/// against a peer that crashed and is starting fresh), or (b) when
/// `ingest` returns `Ok(Some(_))` on the `LAST` frame.
#[derive(Default, Debug)]
pub struct Reassembler {
    buffer: Vec<u8>,
    in_progress: bool,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a payload is currently mid-assembly (we've seen `FIRST`
    /// but not `LAST`). Exposed for tracing / shutdown diagnostics.
    pub fn in_progress(&self) -> bool {
        self.in_progress
    }

    /// Feed an incoming wire `Message`. Returns:
    /// - `Ok(Some(String))` when this frame completed a payload.
    /// - `Ok(None)` when this frame was either a non-`ClipboardText`
    ///   variant (silently ignored) or a middle/first chunk in a
    ///   multi-frame payload.
    /// - `Err(ReassembleError::ChunkWithoutFirst)` if a frame without
    ///   `FIRST` arrived while no payload was in progress.
    /// - `Err(ReassembleError::NotUtf8(_))` if the completed bytes are
    ///   not valid UTF-8. The buffer is reset so the next `FIRST`
    ///   starts cleanly.
    pub fn ingest(&mut self, msg: &Message) -> Result<Option<String>, ReassembleError> {
        let (chunk_flags, bytes) = match msg {
            Message::ClipboardText { chunk_flags, bytes } => (*chunk_flags, bytes),
            _ => return Ok(None),
        };
        let flags = ChunkFlags::from_bits_truncate(chunk_flags);

        if flags.contains(ChunkFlags::FIRST) {
            if self.in_progress {
                tracing::warn!(
                    bytes_so_far = self.buffer.len(),
                    "new FIRST chunk while a payload was still in progress; dropping partial"
                );
            }
            self.buffer.clear();
            self.in_progress = true;
        }

        if !self.in_progress {
            return Err(ReassembleError::ChunkWithoutFirst);
        }

        self.buffer.extend_from_slice(bytes);

        if flags.contains(ChunkFlags::LAST) {
            self.in_progress = false;
            let assembled = std::mem::take(&mut self.buffer);
            match String::from_utf8(assembled) {
                Ok(s) => Ok(Some(s)),
                Err(e) => {
                    // The buffer is already empty after `take`; nothing
                    // to clean up. Wrap the error.
                    Err(ReassembleError::NotUtf8(e))
                }
            }
        } else {
            Ok(None)
        }
    }
}

/// Errors from [`Reassembler::ingest`].
#[derive(Debug, thiserror::Error)]
pub enum ReassembleError {
    /// A non-`FIRST` chunk arrived while no payload was in progress.
    /// The connection should treat this as a peer-side bug; v1 logs
    /// and drops the frame.
    #[error("non-FIRST clipboard chunk without an in-progress payload")]
    ChunkWithoutFirst,

    /// The completed payload bytes are not valid UTF-8. v1 clipboard
    /// sync is text-only; binary clipboards arrive in M11 or later.
    #[error("clipboard payload is not valid UTF-8: {0}")]
    NotUtf8(#[from] std::string::FromUtf8Error),
}

/// SHA-256-based echo suppression.
///
/// Both sides write the clipboard when they receive `ClipboardText`
/// from the peer. Without suppression, the clipboard watcher then
/// fires (because we just changed the clipboard), and we'd send that
/// text right back — infinite ping-pong.
///
/// Usage pattern:
/// ```ignore
/// // On receiving a complete clipboard text from the peer:
/// clipboard.write_text(&text);
/// echo_guard.remember_remote_write(&text);
///
/// // In the watcher loop, when the OS reports a local change:
/// let new = clipboard.read_text().unwrap_or_default();
/// if echo_guard.is_echo_of_remote(&new) {
///     // Skip — we just wrote this ourselves.
/// } else {
///     send_to_peer(new);
/// }
/// ```
#[derive(Default, Debug)]
pub struct EchoGuard {
    last_remote_hash: Option<[u8; 32]>,
}

impl EchoGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remember that we just wrote `text` to the local clipboard in
    /// response to a peer message. Replaces any previously-remembered
    /// hash — only the most recent remote write is tracked.
    pub fn remember_remote_write(&mut self, text: &str) {
        self.last_remote_hash = Some(Self::hash(text));
    }

    /// True iff `text` matches the most recent remembered remote
    /// write — i.e. the local change is an echo of something the peer
    /// asked us to set, and we should NOT send it back.
    pub fn is_echo_of_remote(&self, text: &str) -> bool {
        match self.last_remote_hash {
            Some(h) => h == Self::hash(text),
            None => false,
        }
    }

    /// Explicitly forget the last remote write. Useful at peer
    /// disconnect to avoid stale suppression of a legitimate post-
    /// disconnect local copy.
    pub fn clear(&mut self) {
        self.last_remote_hash = None;
    }

    fn hash(text: &str) -> [u8; 32] {
        use sha2::Digest;
        sha2::Sha256::digest(text.as_bytes()).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unwrap_chunk(msg: &Message) -> (ChunkFlags, &[u8]) {
        match msg {
            Message::ClipboardText { chunk_flags, bytes } => (
                ChunkFlags::from_bits_truncate(*chunk_flags),
                bytes.as_slice(),
            ),
            other => panic!("expected ClipboardText, got {other:?}"),
        }
    }

    #[test]
    fn chunker_single_message_for_small_text() {
        let msgs = Chunker::split("hello");
        assert_eq!(msgs.len(), 1);
        let (flags, bytes) = unwrap_chunk(&msgs[0]);
        assert!(flags.contains(ChunkFlags::FIRST));
        assert!(flags.contains(ChunkFlags::LAST));
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn chunker_empty_text_emits_one_frame() {
        let msgs = Chunker::split("");
        assert_eq!(msgs.len(), 1);
        let (flags, bytes) = unwrap_chunk(&msgs[0]);
        assert!(flags.contains(ChunkFlags::FIRST | ChunkFlags::LAST));
        assert!(bytes.is_empty());
    }

    #[test]
    fn chunker_boundary_4kib_exact_stays_one_message() {
        // Exactly CHUNK_SIZE bytes — boundary is inclusive on the single
        // -message side.
        let text: String = "a".repeat(CHUNK_SIZE);
        let msgs = Chunker::split(&text);
        assert_eq!(msgs.len(), 1);
        let (flags, bytes) = unwrap_chunk(&msgs[0]);
        assert!(flags.contains(ChunkFlags::FIRST | ChunkFlags::LAST));
        assert_eq!(bytes.len(), CHUNK_SIZE);
    }

    #[test]
    fn chunker_just_over_boundary_emits_two_messages() {
        let text: String = "a".repeat(CHUNK_SIZE + 1);
        let msgs = Chunker::split(&text);
        assert_eq!(msgs.len(), 2);
        let (f0, b0) = unwrap_chunk(&msgs[0]);
        let (f1, b1) = unwrap_chunk(&msgs[1]);
        assert!(f0.contains(ChunkFlags::FIRST));
        assert!(!f0.contains(ChunkFlags::LAST));
        assert!(!f1.contains(ChunkFlags::FIRST));
        assert!(f1.contains(ChunkFlags::LAST));
        assert_eq!(b0.len(), CHUNK_SIZE);
        assert_eq!(b1.len(), 1);
    }

    #[test]
    fn chunker_multi_chunks_for_large_text() {
        // 10 KiB → 3 chunks of 4096 + 4096 + 2048.
        let text: String = "z".repeat(10 * 1024);
        let msgs = Chunker::split(&text);
        assert_eq!(msgs.len(), 3);

        let (f0, b0) = unwrap_chunk(&msgs[0]);
        let (f1, b1) = unwrap_chunk(&msgs[1]);
        let (f2, b2) = unwrap_chunk(&msgs[2]);

        assert!(f0.contains(ChunkFlags::FIRST));
        assert!(!f0.contains(ChunkFlags::LAST));
        assert_eq!(b0.len(), 4096);

        // Middle chunk: neither flag bit set.
        assert_eq!(f1, ChunkFlags::empty());
        assert_eq!(b1.len(), 4096);

        assert!(!f2.contains(ChunkFlags::FIRST));
        assert!(f2.contains(ChunkFlags::LAST));
        assert_eq!(b2.len(), 2048);
    }

    #[test]
    fn reassembler_single_frame_returns_immediately() {
        let mut r = Reassembler::new();
        let msgs = Chunker::split("hello world");
        assert_eq!(msgs.len(), 1);
        let result = r.ingest(&msgs[0]).expect("ok");
        assert_eq!(result, Some("hello world".to_string()));
        assert!(!r.in_progress());
    }

    #[test]
    fn reassembler_three_chunks_returns_on_last() {
        let mut r = Reassembler::new();
        let text: String = "0123456789".repeat(1024); // 10 KiB
        let msgs = Chunker::split(&text);
        assert_eq!(msgs.len(), 3);

        assert_eq!(r.ingest(&msgs[0]).expect("ok"), None);
        assert!(r.in_progress());
        assert_eq!(r.ingest(&msgs[1]).expect("ok"), None);
        assert!(r.in_progress());

        let assembled = r.ingest(&msgs[2]).expect("ok").expect("complete");
        assert_eq!(assembled, text);
        assert!(!r.in_progress());
    }

    #[test]
    fn reassembler_chunk_without_first_errors() {
        let mut r = Reassembler::new();
        // Hand-craft a middle chunk (neither FIRST nor LAST).
        let msg = Message::ClipboardText {
            chunk_flags: 0,
            bytes: b"orphan".to_vec(),
        };
        match r.ingest(&msg) {
            Err(ReassembleError::ChunkWithoutFirst) => {}
            other => panic!("expected ChunkWithoutFirst, got {other:?}"),
        }
    }

    #[test]
    fn reassembler_lone_last_chunk_also_errors() {
        let mut r = Reassembler::new();
        let msg = Message::ClipboardText {
            chunk_flags: ChunkFlags::LAST.bits(),
            bytes: b"orphan".to_vec(),
        };
        assert!(matches!(
            r.ingest(&msg),
            Err(ReassembleError::ChunkWithoutFirst)
        ));
    }

    #[test]
    fn reassembler_resets_on_new_first_mid_stream() {
        let mut r = Reassembler::new();
        // Start a multi-chunk payload but never finish it.
        let text: String = "x".repeat(10 * 1024);
        let abandoned = Chunker::split(&text);
        assert_eq!(r.ingest(&abandoned[0]).expect("ok"), None);
        assert_eq!(r.ingest(&abandoned[1]).expect("ok"), None);
        assert!(r.in_progress());

        // Peer restarts: sends a fresh FIRST. The partial is discarded.
        let fresh = Chunker::split("fresh start");
        let result = r.ingest(&fresh[0]).expect("ok");
        assert_eq!(result, Some("fresh start".to_string()));
        assert!(!r.in_progress());
    }

    #[test]
    fn reassembler_invalid_utf8_errors_at_last() {
        let mut r = Reassembler::new();
        // Hand-craft a single-chunk frame with invalid UTF-8 bytes.
        let msg = Message::ClipboardText {
            chunk_flags: (ChunkFlags::FIRST | ChunkFlags::LAST).bits(),
            bytes: vec![0xC3, 0x28], // invalid UTF-8 start byte
        };
        match r.ingest(&msg) {
            Err(ReassembleError::NotUtf8(_)) => {}
            other => panic!("expected NotUtf8, got {other:?}"),
        }
        // After the failure the reassembler must be ready for a new
        // FIRST.
        assert!(!r.in_progress());
        let ok = Chunker::split("recovery");
        assert_eq!(r.ingest(&ok[0]).expect("ok"), Some("recovery".to_string()));
    }

    #[test]
    fn reassembler_ignores_non_clipboard_messages() {
        let mut r = Reassembler::new();
        let result = r.ingest(&Message::Heartbeat { seq: 7 }).expect("ok");
        assert_eq!(result, None);
        assert!(!r.in_progress());
    }

    /// The combined round-trip test the team-lead asked for: split a
    /// payload of various sizes, ingest each chunk through a fresh
    /// reassembler, recover the original.
    #[test]
    fn roundtrip_split_then_ingest_recovers_text() {
        let cases = [
            "".to_string(),
            "a".to_string(),
            "a".repeat(CHUNK_SIZE),
            "b".repeat(CHUNK_SIZE + 1),
            "c".repeat(CHUNK_SIZE * 2),
            // Test with multi-byte UTF-8 — the chunker splits on bytes,
            // not chars, so a multi-byte rune could straddle a chunk
            // boundary. UTF-8 validation only runs on the assembled
            // buffer, so this should still recover cleanly.
            "🦀".repeat(CHUNK_SIZE), // 4 bytes per crab; > 4 KiB
            "héllo wörld".repeat(100),
        ];

        for original in &cases {
            let msgs = Chunker::split(original);
            let mut r = Reassembler::new();
            let mut completed = None;
            for m in &msgs {
                if let Some(s) = r.ingest(m).expect("ingest ok") {
                    completed = Some(s);
                }
            }
            assert_eq!(completed.as_deref(), Some(original.as_str()));
        }
    }

    // ─── EchoGuard ────────────────────────────────────────────────

    #[test]
    fn echo_guard_matches_immediately_after_remember() {
        let mut g = EchoGuard::new();
        g.remember_remote_write("paste me");
        assert!(g.is_echo_of_remote("paste me"));
    }

    #[test]
    fn echo_guard_doesnt_match_different_text() {
        let mut g = EchoGuard::new();
        g.remember_remote_write("paste me");
        assert!(!g.is_echo_of_remote("different"));
        assert!(!g.is_echo_of_remote("paste me!")); // one char different
    }

    #[test]
    fn echo_guard_starts_empty() {
        let g = EchoGuard::new();
        assert!(!g.is_echo_of_remote(""));
        assert!(!g.is_echo_of_remote("anything"));
    }

    #[test]
    fn echo_guard_replaces_previous_remember() {
        let mut g = EchoGuard::new();
        g.remember_remote_write("first");
        g.remember_remote_write("second");
        // Only the most recent is remembered.
        assert!(!g.is_echo_of_remote("first"));
        assert!(g.is_echo_of_remote("second"));
    }

    #[test]
    fn echo_guard_clear_drops_the_match() {
        let mut g = EchoGuard::new();
        g.remember_remote_write("paste me");
        assert!(g.is_echo_of_remote("paste me"));
        g.clear();
        assert!(!g.is_echo_of_remote("paste me"));
    }

    #[test]
    fn echo_guard_distinguishes_large_payloads() {
        let mut g = EchoGuard::new();
        let big_a: String = "a".repeat(50_000);
        let big_b: String = "b".repeat(50_000);
        g.remember_remote_write(&big_a);
        assert!(g.is_echo_of_remote(&big_a));
        assert!(!g.is_echo_of_remote(&big_b));
    }
}
