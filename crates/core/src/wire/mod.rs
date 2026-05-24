//! Wire protocol: length-prefixed binary frames mirroring `kmwarp-SPEC.md`.
//!
//! Header layout, identical for every message:
//!
//! ```text
//! [u8 msg_type][u16 length LE][payload ... `length` bytes]
//! ```
//!
//! All multi-byte integers on the wire are little-endian. The `Message` enum
//! carries every variant the protocol will ship in v1; M1 only wires the
//! handshake/heartbeat subset through the codec, but downstream milestones
//! (M4 mouse, M5 keyboard, M6 edge, M8 clipboard) just fill in encode/decode
//! arms rather than touching the enum shape.

pub mod codec;
pub mod convert;

pub use codec::{decode_frame, encode_frame};
pub use convert::{
    apply_mouse_to_sink, byte_to_key_state, byte_to_mouse_button, key_state_to_byte,
    mouse_button_to_byte, source_event_to_message,
};

/// Wire protocol version negotiated in `Hello` / `HelloAck`.
pub const PROTO_VERSION: u16 = 1;

/// Maximum payload size representable by the `u16` length field.
pub const MAX_PAYLOAD_LEN: u16 = u16::MAX;

/// Frame header: one byte of message type plus a `u16` LE length.
pub const FRAME_HEADER_LEN: usize = 3;

/// `msg_type` byte assignments straight from the spec table.
///
/// `ECHO_PING` / `ECHO_PONG` are out-of-spec, allocated from the unused
/// `0x70` range for the M4 latency-probe harness. Carried in the wire
/// enum unconditionally so a `latency-probe`-enabled server can talk to a
/// stock client (and vice versa) without protocol version skew.
pub mod msg_type {
    pub const HELLO: u8 = 0x01;
    pub const HELLO_ACK: u8 = 0x02;
    pub const MOUSE_MOVE_REL: u8 = 0x10;
    pub const MOUSE_BUTTON: u8 = 0x11;
    pub const MOUSE_WHEEL: u8 = 0x12;
    pub const KEY_EVENT: u8 = 0x20;
    pub const CLIPBOARD_TEXT: u8 = 0x30;
    pub const TAKE_CONTROL: u8 = 0x40;
    pub const RELEASE_CONTROL: u8 = 0x41;
    pub const ECHO_PING: u8 = 0x70;
    pub const ECHO_PONG: u8 = 0x71;
    pub const HEARTBEAT: u8 = 0xFE;
    pub const BYE: u8 = 0xFF;
}

/// Wire-protocol convention for the `MouseButton.button` byte.
///
/// The spec table only specifies the field is a `u8`; we pin the mapping
/// here so the server and client encode/decode against the same dictionary.
pub mod mouse_button_code {
    pub const LEFT: u8 = 0;
    pub const RIGHT: u8 = 1;
    pub const MIDDLE: u8 = 2;
    pub const X1: u8 = 3;
    pub const X2: u8 = 4;
}

/// Wire-protocol convention for any `state` byte (mouse button / key).
///
/// Matches the spec table for `MouseButton` (`0 = up, 1 = down`).
pub mod key_state_code {
    pub const UP: u8 = 0;
    pub const DOWN: u8 = 1;
}

/// Every protocol message the v1 wire format can carry.
///
/// Variant payload layouts match `kmwarp-SPEC.md` exactly. Field types are
/// chosen so encode/decode is a direct LE serialization of each field in
/// declaration order (with the one exception of `Hello.peer_name` and
/// `ClipboardText.bytes`, which are length-prefixed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Opening handshake: client identifies itself.
    Hello {
        proto_version: u16,
        peer_name: String,
    },

    /// Server's response to `Hello`. `accepted == false` means the server is
    /// declining the connection; both peers then disconnect cleanly.
    HelloAck {
        accepted: bool,
        server_screen_px: (u16, u16),
    },

    /// Relative mouse motion in physical pixels of the server screen.
    MouseMoveRel { dx: i16, dy: i16 },

    /// Mouse button transition. `state == 0` is up, `state == 1` is down.
    MouseButton { button: u8, state: u8 },

    /// Scroll wheel ticks. Sign follows platform native direction.
    MouseWheel { dx: i16, dy: i16 },

    /// Key transition keyed by USB HID usage code (page 0x07).
    KeyEvent {
        hid_usage: u16,
        state: u8,
        modifiers: u8,
    },

    /// UTF-8 clipboard text. `chunk_flags` is reserved for M8's chunking
    /// support (bit 0 = "more chunks follow"); single-shot payloads send 0.
    ClipboardText { chunk_flags: u8, bytes: Vec<u8> },

    /// Server tells client "you now own the cursor".
    TakeControl { entry_y: u16 },

    /// Client tells server "cursor is leaving back to you".
    ReleaseControl { exit_y: u16 },

    /// Liveness probe; sent every 500 ms by both sides.
    Heartbeat { seq: u32 },

    /// Graceful shutdown signal.
    Bye { reason_code: u8 },

    /// Debug-only latency probe (M4 §latency-probe). `ts_ns` is opaque to
    /// the receiver — it's the prober's monotonic clock reading; the
    /// receiver simply echoes it back in an `EchoPong`.
    EchoPing { ts_ns: u64 },

    /// Reply to `EchoPing`. `ts_ns` is verbatim from the matching ping; the
    /// prober subtracts it from its current clock reading to compute RTT.
    EchoPong { ts_ns: u64 },
}
