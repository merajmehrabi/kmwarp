//! Frame encoder/decoder.
//!
//! The encoder appends a complete frame to a caller-owned `BytesMut`. The
//! decoder reads as many bytes as it needs and only `advance`s the buffer
//! when a full frame has been successfully parsed — partial-frame callers
//! get `Ok(None)` and the buffer is left untouched so a later call after
//! more bytes arrive can simply retry.

use crate::{
    error::WireError,
    wire::{msg_type, Message, FRAME_HEADER_LEN, MAX_PAYLOAD_LEN},
};
use bytes::{Buf, BufMut, BytesMut};

/// Append the on-wire representation of `msg` to `buf`.
///
/// Steady-state callers should `reserve` ahead of time on `buf` so this
/// function performs no allocation. Returns `PayloadTooLong` if a variable
/// length field overflows the `u16` length header.
pub fn encode_frame(msg: &Message, buf: &mut BytesMut) -> Result<(), WireError> {
    match msg {
        Message::Hello {
            proto_version,
            peer_name,
        } => {
            let _span = tracing::trace_span!("msg.Hello").entered();
            let name_bytes = peer_name.as_bytes();
            // proto_version (2) + name_len (2) + name_bytes
            let payload_len = check_payload_len(4usize.saturating_add(name_bytes.len()))?;
            let name_len =
                u16::try_from(name_bytes.len()).map_err(|_| too_long(name_bytes.len()))?;
            buf.reserve(FRAME_HEADER_LEN + payload_len as usize);
            buf.put_u8(msg_type::HELLO);
            buf.put_u16_le(payload_len);
            buf.put_u16_le(*proto_version);
            buf.put_u16_le(name_len);
            buf.put_slice(name_bytes);
            tracing::trace!(proto_version, name_len, "encoded Hello");
        }
        Message::HelloAck {
            accepted,
            server_screen_px,
        } => {
            let _span = tracing::trace_span!("msg.HelloAck").entered();
            // accepted (1) + width (2) + height (2)
            let payload_len: u16 = 5;
            buf.reserve(FRAME_HEADER_LEN + payload_len as usize);
            buf.put_u8(msg_type::HELLO_ACK);
            buf.put_u16_le(payload_len);
            buf.put_u8(u8::from(*accepted));
            buf.put_u16_le(server_screen_px.0);
            buf.put_u16_le(server_screen_px.1);
            tracing::trace!(accepted, "encoded HelloAck");
        }
        Message::MouseMoveRel { dx, dy } => {
            let _span = tracing::trace_span!("msg.MouseMoveRel").entered();
            let payload_len: u16 = 4;
            buf.reserve(FRAME_HEADER_LEN + payload_len as usize);
            buf.put_u8(msg_type::MOUSE_MOVE_REL);
            buf.put_u16_le(payload_len);
            buf.put_i16_le(*dx);
            buf.put_i16_le(*dy);
            tracing::trace!(dx, dy, "encoded MouseMoveRel");
        }
        Message::MouseButton { button, state } => {
            let _span = tracing::trace_span!("msg.MouseButton").entered();
            let payload_len: u16 = 2;
            buf.reserve(FRAME_HEADER_LEN + payload_len as usize);
            buf.put_u8(msg_type::MOUSE_BUTTON);
            buf.put_u16_le(payload_len);
            buf.put_u8(*button);
            buf.put_u8(*state);
            tracing::trace!(button, state, "encoded MouseButton");
        }
        Message::MouseWheel { dx, dy } => {
            let _span = tracing::trace_span!("msg.MouseWheel").entered();
            let payload_len: u16 = 4;
            buf.reserve(FRAME_HEADER_LEN + payload_len as usize);
            buf.put_u8(msg_type::MOUSE_WHEEL);
            buf.put_u16_le(payload_len);
            buf.put_i16_le(*dx);
            buf.put_i16_le(*dy);
            tracing::trace!(dx, dy, "encoded MouseWheel");
        }
        Message::KeyEvent {
            hid_usage,
            state,
            modifiers,
        } => {
            let _span = tracing::trace_span!("msg.KeyEvent").entered();
            // hid_usage (2) + state (1) + modifiers (1)
            let payload_len: u16 = 4;
            buf.reserve(FRAME_HEADER_LEN + payload_len as usize);
            buf.put_u8(msg_type::KEY_EVENT);
            buf.put_u16_le(payload_len);
            buf.put_u16_le(*hid_usage);
            buf.put_u8(*state);
            buf.put_u8(*modifiers);
            tracing::trace!(hid_usage, state, modifiers, "encoded KeyEvent");
        }
        Message::ClipboardText { chunk_flags, bytes } => {
            let _span = tracing::trace_span!("msg.ClipboardText").entered();
            // chunk_flags (1) + byte_len (2) + bytes
            let payload_len = check_payload_len(3usize.saturating_add(bytes.len()))?;
            let byte_len = u16::try_from(bytes.len()).map_err(|_| too_long(bytes.len()))?;
            buf.reserve(FRAME_HEADER_LEN + payload_len as usize);
            buf.put_u8(msg_type::CLIPBOARD_TEXT);
            buf.put_u16_le(payload_len);
            buf.put_u8(*chunk_flags);
            buf.put_u16_le(byte_len);
            buf.put_slice(bytes);
            tracing::trace!(chunk_flags, byte_len, "encoded ClipboardText");
        }
        Message::TakeControl { entry_y } => {
            let _span = tracing::trace_span!("msg.TakeControl").entered();
            let payload_len: u16 = 2;
            buf.reserve(FRAME_HEADER_LEN + payload_len as usize);
            buf.put_u8(msg_type::TAKE_CONTROL);
            buf.put_u16_le(payload_len);
            buf.put_u16_le(*entry_y);
            tracing::trace!(entry_y, "encoded TakeControl");
        }
        Message::ReleaseControl { exit_y } => {
            let _span = tracing::trace_span!("msg.ReleaseControl").entered();
            let payload_len: u16 = 2;
            buf.reserve(FRAME_HEADER_LEN + payload_len as usize);
            buf.put_u8(msg_type::RELEASE_CONTROL);
            buf.put_u16_le(payload_len);
            buf.put_u16_le(*exit_y);
            tracing::trace!(exit_y, "encoded ReleaseControl");
        }
        Message::Heartbeat { seq } => {
            let _span = tracing::trace_span!("msg.Heartbeat").entered();
            let payload_len: u16 = 4;
            buf.reserve(FRAME_HEADER_LEN + payload_len as usize);
            buf.put_u8(msg_type::HEARTBEAT);
            buf.put_u16_le(payload_len);
            buf.put_u32_le(*seq);
            tracing::trace!(seq, "encoded Heartbeat");
        }
        Message::Bye { reason_code } => {
            let _span = tracing::trace_span!("msg.Bye").entered();
            let payload_len: u16 = 1;
            buf.reserve(FRAME_HEADER_LEN + payload_len as usize);
            buf.put_u8(msg_type::BYE);
            buf.put_u16_le(payload_len);
            buf.put_u8(*reason_code);
            tracing::trace!(reason_code, "encoded Bye");
        }
        Message::EchoPing { ts_ns } => {
            let _span = tracing::trace_span!("msg.EchoPing").entered();
            let payload_len: u16 = 8;
            buf.reserve(FRAME_HEADER_LEN + payload_len as usize);
            buf.put_u8(msg_type::ECHO_PING);
            buf.put_u16_le(payload_len);
            buf.put_u64_le(*ts_ns);
            tracing::trace!(ts_ns, "encoded EchoPing");
        }
        Message::EchoPong { ts_ns } => {
            let _span = tracing::trace_span!("msg.EchoPong").entered();
            let payload_len: u16 = 8;
            buf.reserve(FRAME_HEADER_LEN + payload_len as usize);
            buf.put_u8(msg_type::ECHO_PONG);
            buf.put_u16_le(payload_len);
            buf.put_u64_le(*ts_ns);
            tracing::trace!(ts_ns, "encoded EchoPong");
        }
    }
    Ok(())
}

/// Try to parse a single frame from the front of `buf`.
///
/// - `Ok(Some(msg))`: a complete frame was decoded and consumed (buffer
///   advanced past it).
/// - `Ok(None)`: not enough bytes yet; buffer left untouched so the caller
///   can append more bytes from the socket and retry.
/// - `Err(_)`: a framing/protocol violation that the caller should treat as
///   fatal for the connection.
pub fn decode_frame(buf: &mut BytesMut) -> Result<Option<Message>, WireError> {
    if buf.len() < FRAME_HEADER_LEN {
        return Ok(None);
    }
    let msg_type_byte = buf[0];
    let payload_len = u16::from_le_bytes([buf[1], buf[2]]) as usize;
    let frame_len = FRAME_HEADER_LEN + payload_len;
    if buf.len() < frame_len {
        return Ok(None);
    }

    // Borrow a sub-slice and parse against it; release the borrow before
    // advancing the buffer so we can mutate it.
    let msg = {
        let payload = &buf[FRAME_HEADER_LEN..frame_len];
        decode_payload(msg_type_byte, payload)?
    };
    buf.advance(frame_len);
    Ok(Some(msg))
}

/// Decode the payload bytes that follow a known-good header.
fn decode_payload(msg_type_byte: u8, payload: &[u8]) -> Result<Message, WireError> {
    let mut r = PayloadReader::new(payload);
    match msg_type_byte {
        msg_type::HELLO => {
            let _span = tracing::trace_span!("msg.Hello").entered();
            let proto_version = r.read_u16_le()?;
            let name_len = r.read_u16_le()? as usize;
            let name_bytes = r.read_bytes(name_len)?;
            let peer_name = std::str::from_utf8(name_bytes)?.to_owned();
            tracing::trace!(proto_version, name_len, "decoded Hello");
            Ok(Message::Hello {
                proto_version,
                peer_name,
            })
        }
        msg_type::HELLO_ACK => {
            let _span = tracing::trace_span!("msg.HelloAck").entered();
            let accepted_byte = r.read_u8()?;
            let accepted = match accepted_byte {
                0 => false,
                1 => true,
                _ => {
                    return Err(WireError::InvalidPayload(
                        "HelloAck.accepted must be 0 or 1",
                    ))
                }
            };
            let w = r.read_u16_le()?;
            let h = r.read_u16_le()?;
            tracing::trace!(accepted, w, h, "decoded HelloAck");
            Ok(Message::HelloAck {
                accepted,
                server_screen_px: (w, h),
            })
        }
        msg_type::MOUSE_MOVE_REL => {
            let _span = tracing::trace_span!("msg.MouseMoveRel").entered();
            let dx = r.read_i16_le()?;
            let dy = r.read_i16_le()?;
            tracing::trace!(dx, dy, "decoded MouseMoveRel");
            Ok(Message::MouseMoveRel { dx, dy })
        }
        msg_type::MOUSE_BUTTON => {
            let _span = tracing::trace_span!("msg.MouseButton").entered();
            let button = r.read_u8()?;
            let state = r.read_u8()?;
            tracing::trace!(button, state, "decoded MouseButton");
            Ok(Message::MouseButton { button, state })
        }
        msg_type::MOUSE_WHEEL => {
            let _span = tracing::trace_span!("msg.MouseWheel").entered();
            let dx = r.read_i16_le()?;
            let dy = r.read_i16_le()?;
            tracing::trace!(dx, dy, "decoded MouseWheel");
            Ok(Message::MouseWheel { dx, dy })
        }
        msg_type::KEY_EVENT => {
            let _span = tracing::trace_span!("msg.KeyEvent").entered();
            let hid_usage = r.read_u16_le()?;
            let state = r.read_u8()?;
            let modifiers = r.read_u8()?;
            tracing::trace!(hid_usage, state, modifiers, "decoded KeyEvent");
            Ok(Message::KeyEvent {
                hid_usage,
                state,
                modifiers,
            })
        }
        msg_type::CLIPBOARD_TEXT => {
            let _span = tracing::trace_span!("msg.ClipboardText").entered();
            let chunk_flags = r.read_u8()?;
            let byte_len = r.read_u16_le()? as usize;
            let bytes = r.read_bytes(byte_len)?.to_vec();
            tracing::trace!(chunk_flags, byte_len, "decoded ClipboardText");
            Ok(Message::ClipboardText { chunk_flags, bytes })
        }
        msg_type::TAKE_CONTROL => {
            let _span = tracing::trace_span!("msg.TakeControl").entered();
            let entry_y = r.read_u16_le()?;
            tracing::trace!(entry_y, "decoded TakeControl");
            Ok(Message::TakeControl { entry_y })
        }
        msg_type::RELEASE_CONTROL => {
            let _span = tracing::trace_span!("msg.ReleaseControl").entered();
            let exit_y = r.read_u16_le()?;
            tracing::trace!(exit_y, "decoded ReleaseControl");
            Ok(Message::ReleaseControl { exit_y })
        }
        msg_type::HEARTBEAT => {
            let _span = tracing::trace_span!("msg.Heartbeat").entered();
            let seq = r.read_u32_le()?;
            tracing::trace!(seq, "decoded Heartbeat");
            Ok(Message::Heartbeat { seq })
        }
        msg_type::BYE => {
            let _span = tracing::trace_span!("msg.Bye").entered();
            let reason_code = r.read_u8()?;
            tracing::trace!(reason_code, "decoded Bye");
            Ok(Message::Bye { reason_code })
        }
        msg_type::ECHO_PING => {
            let _span = tracing::trace_span!("msg.EchoPing").entered();
            let ts_ns = r.read_u64_le()?;
            tracing::trace!(ts_ns, "decoded EchoPing");
            Ok(Message::EchoPing { ts_ns })
        }
        msg_type::ECHO_PONG => {
            let _span = tracing::trace_span!("msg.EchoPong").entered();
            let ts_ns = r.read_u64_le()?;
            tracing::trace!(ts_ns, "decoded EchoPong");
            Ok(Message::EchoPong { ts_ns })
        }
        unknown => Err(WireError::UnknownMsgType(unknown)),
    }
}

/// Validate that `len` fits the `u16` length field, returning it as `u16`.
fn check_payload_len(len: usize) -> Result<u16, WireError> {
    if len > MAX_PAYLOAD_LEN as usize {
        Err(too_long(len))
    } else {
        // Safe: we just bounds-checked against u16::MAX.
        Ok(len as u16)
    }
}

/// Build a `PayloadTooLong` with a saturated length, since the variant's
/// `len` field is itself a `u16`.
fn too_long(len: usize) -> WireError {
    WireError::PayloadTooLong {
        len: u16::try_from(len).unwrap_or(u16::MAX),
        max: MAX_PAYLOAD_LEN,
    }
}

/// Cursor over a borrowed payload slice; every read advances `pos` and
/// returns `ShortBuffer` if the slice would be overrun.
struct PayloadReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> PayloadReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn read_u8(&mut self) -> Result<u8, WireError> {
        let byte = *self.data.get(self.pos).ok_or(WireError::ShortBuffer)?;
        self.pos += 1;
        Ok(byte)
    }

    fn read_u16_le(&mut self) -> Result<u16, WireError> {
        let bytes = self.read_bytes(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_i16_le(&mut self) -> Result<i16, WireError> {
        let bytes = self.read_bytes(2)?;
        Ok(i16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_u32_le(&mut self) -> Result<u32, WireError> {
        let bytes = self.read_bytes(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_u64_le(&mut self) -> Result<u64, WireError> {
        let bytes = self.read_bytes(8)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn read_bytes(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        let end = self.pos.checked_add(n).ok_or(WireError::ShortBuffer)?;
        let slice = self.data.get(self.pos..end).ok_or(WireError::ShortBuffer)?;
        self.pos = end;
        Ok(slice)
    }
}
