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

pub use codec::{decode_frame, encode_frame};

/// Wire protocol version negotiated in `Hello` / `HelloAck`.
pub const PROTO_VERSION: u16 = 1;

/// Maximum payload size representable by the `u16` length field.
pub const MAX_PAYLOAD_LEN: u16 = u16::MAX;

/// Frame header: one byte of message type plus a `u16` LE length.
pub const FRAME_HEADER_LEN: usize = 3;

/// `msg_type` byte assignments straight from the spec table.
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
    pub const HEARTBEAT: u8 = 0xFE;
    pub const BYE: u8 = 0xFF;
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
}
