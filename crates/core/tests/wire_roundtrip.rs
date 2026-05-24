//! Wire codec round-trip + edge-case tests.
//!
//! Every `Message` variant is round-tripped through `encode_frame` →
//! `decode_frame` and compared. The mouse-motion variant additionally gets
//! property-tested over the full `(i16, i16)` space. Partial-buffer and
//! garbage-type behaviors of `decode_frame` are pinned down too.

use bytes::BytesMut;
use kmwarp_core::{
    wire::{decode_frame, encode_frame, msg_type, Message},
    WireError,
};
use proptest::prelude::*;

/// Every variant in one table; if a new variant is added to `Message` we
/// expect this table to grow with it.
fn all_variants() -> Vec<Message> {
    vec![
        Message::Hello {
            proto_version: 1,
            peer_name: "merajs-mbp".to_string(),
        },
        Message::Hello {
            proto_version: 1,
            peer_name: String::new(),
        },
        Message::HelloAck {
            accepted: true,
            server_screen_px: (2560, 1440),
        },
        Message::HelloAck {
            accepted: false,
            server_screen_px: (0, 0),
        },
        Message::MouseMoveRel { dx: 0, dy: 0 },
        Message::MouseMoveRel {
            dx: i16::MIN,
            dy: i16::MAX,
        },
        Message::MouseButton {
            button: 0,
            state: 0,
        },
        Message::MouseButton {
            button: 2,
            state: 1,
        },
        Message::MouseWheel { dx: -3, dy: 7 },
        Message::KeyEvent {
            hid_usage: 0x04,
            state: 1,
            modifiers: 0b0000_0010,
        },
        Message::KeyEvent {
            hid_usage: 0xFFFF,
            state: 0,
            modifiers: 0xFF,
        },
        Message::ClipboardText {
            chunk_flags: 0,
            bytes: b"hello".to_vec(),
        },
        Message::ClipboardText {
            chunk_flags: 0,
            bytes: Vec::new(),
        },
        Message::ClipboardText {
            chunk_flags: 0b0000_0001,
            bytes: vec![0xFF; 4096],
        },
        Message::TakeControl { entry_y: 0 },
        Message::TakeControl { entry_y: u16::MAX },
        Message::ReleaseControl { exit_y: 720 },
        Message::Heartbeat { seq: 0 },
        Message::Heartbeat { seq: u32::MAX },
        Message::Bye { reason_code: 0 },
        Message::Bye { reason_code: 0xFE },
        Message::EchoPing { ts_ns: 0 },
        Message::EchoPing { ts_ns: u64::MAX },
        Message::EchoPong {
            ts_ns: 1_234_567_890,
        },
        // M9 pairing
        Message::PairSpakeA { msg: vec![] },
        Message::PairSpakeA {
            msg: vec![0x11; 33], // Ed25519Group element is ~33 bytes
        },
        Message::PairSpakeB {
            msg: vec![0x22; 33],
        },
        Message::PairCertExchange {
            cert_der: vec![0x30; 256],
            hmac: [0xAB; 32],
        },
        Message::PairCertExchange {
            cert_der: Vec::new(),
            hmac: [0; 32],
        },
        Message::PairAccepted,
        Message::PairRejected { reason_code: 0 },
        Message::PairRejected { reason_code: 4 },
    ]
}

#[test]
fn roundtrip_every_variant() {
    for original in all_variants() {
        let mut buf = BytesMut::new();
        encode_frame(&original, &mut buf).expect("encode succeeds");
        let decoded = decode_frame(&mut buf)
            .expect("decode returns Ok")
            .expect("a complete frame was produced");
        assert_eq!(decoded, original, "round-trip mismatch for {:?}", original);
        assert!(
            buf.is_empty(),
            "decode_frame should consume exactly one frame"
        );
    }
}

#[test]
fn decode_returns_none_when_header_incomplete() {
    let mut buf = BytesMut::new();
    encode_frame(&Message::Heartbeat { seq: 42 }, &mut buf).expect("encode");
    // Truncate to less than a full header.
    buf.truncate(2);
    let before = buf.clone();
    let res = decode_frame(&mut buf).expect("no error, just incomplete");
    assert!(res.is_none(), "expected Ok(None) for partial header");
    assert_eq!(buf, before, "buffer must be left untouched on Ok(None)");
}

#[test]
fn decode_returns_none_when_payload_incomplete() {
    let mut buf = BytesMut::new();
    encode_frame(
        &Message::Hello {
            proto_version: 1,
            peer_name: "merajs-mbp".to_string(),
        },
        &mut buf,
    )
    .expect("encode");
    // Keep the header but drop one payload byte off the end.
    buf.truncate(buf.len() - 1);
    let before = buf.clone();
    let res = decode_frame(&mut buf).expect("no error, just incomplete");
    assert!(res.is_none(), "expected Ok(None) for partial payload");
    assert_eq!(buf, before, "buffer must be left untouched on Ok(None)");
}

#[test]
fn decode_rejects_unknown_msg_type() {
    let mut buf = BytesMut::new();
    // Frame: msg_type = 0x7A (unassigned), zero-length payload.
    buf.extend_from_slice(&[0x7A, 0x00, 0x00]);
    match decode_frame(&mut buf) {
        Err(WireError::UnknownMsgType(0x7A)) => {}
        other => panic!("expected UnknownMsgType(0x7A), got {:?}", other),
    }
}

#[test]
fn decode_rejects_invalid_utf8_in_hello() {
    let mut buf = BytesMut::new();
    // Hand-craft a Hello frame with an invalid utf-8 byte in peer_name.
    // payload = proto_version(2) + name_len(2) + 2 bytes of "name"
    let payload: [u8; 6] = [
        0x01, 0x00, // proto_version = 1
        0x02, 0x00, // name_len = 2
        0xFF, 0xFE, // not valid utf-8
    ];
    buf.extend_from_slice(&[msg_type::HELLO, payload.len() as u8, 0x00]);
    buf.extend_from_slice(&payload);
    match decode_frame(&mut buf) {
        Err(WireError::Utf8(_)) => {}
        other => panic!("expected Utf8 error, got {:?}", other),
    }
}

#[test]
fn decode_rejects_inner_length_overrun() {
    // Hello where name_len claims more bytes than the payload actually has.
    let mut buf = BytesMut::new();
    let payload: [u8; 4] = [
        0x01, 0x00, // proto_version = 1
        0xFF, 0x00, // name_len = 255, but there are 0 name bytes
    ];
    buf.extend_from_slice(&[msg_type::HELLO, payload.len() as u8, 0x00]);
    buf.extend_from_slice(&payload);
    match decode_frame(&mut buf) {
        Err(WireError::ShortBuffer) => {}
        other => panic!("expected ShortBuffer, got {:?}", other),
    }
}

#[test]
fn decode_rejects_helloack_with_invalid_accepted_byte() {
    let mut buf = BytesMut::new();
    // HelloAck: accepted=2 (invalid), w=0, h=0
    let payload: [u8; 5] = [0x02, 0x00, 0x00, 0x00, 0x00];
    buf.extend_from_slice(&[msg_type::HELLO_ACK, payload.len() as u8, 0x00]);
    buf.extend_from_slice(&payload);
    match decode_frame(&mut buf) {
        Err(WireError::InvalidPayload(_)) => {}
        other => panic!("expected InvalidPayload, got {:?}", other),
    }
}

#[test]
fn decode_consumes_only_one_frame_when_two_are_buffered() {
    let mut buf = BytesMut::new();
    encode_frame(&Message::Heartbeat { seq: 1 }, &mut buf).expect("encode");
    encode_frame(&Message::Heartbeat { seq: 2 }, &mut buf).expect("encode");
    let total = buf.len();

    let first = decode_frame(&mut buf).expect("ok").expect("frame");
    assert_eq!(first, Message::Heartbeat { seq: 1 });
    assert_eq!(
        buf.len(),
        total / 2,
        "should have consumed exactly one frame"
    );

    let second = decode_frame(&mut buf).expect("ok").expect("frame");
    assert_eq!(second, Message::Heartbeat { seq: 2 });
    assert!(buf.is_empty());
}

#[test]
fn empty_buffer_decodes_to_none() {
    let mut buf = BytesMut::new();
    assert!(decode_frame(&mut buf).expect("no err").is_none());
}

proptest! {
    #[test]
    fn mouse_move_rel_roundtrips_for_any_i16_pair(dx in any::<i16>(), dy in any::<i16>()) {
        let original = Message::MouseMoveRel { dx, dy };
        let mut buf = BytesMut::new();
        encode_frame(&original, &mut buf).expect("encode succeeds");
        let decoded = decode_frame(&mut buf)
            .expect("decode ok")
            .expect("frame");
        prop_assert_eq!(decoded, original);
        prop_assert!(buf.is_empty());
    }

    #[test]
    fn heartbeat_roundtrips_for_any_u32(seq in any::<u32>()) {
        let original = Message::Heartbeat { seq };
        let mut buf = BytesMut::new();
        encode_frame(&original, &mut buf).expect("encode succeeds");
        let decoded = decode_frame(&mut buf)
            .expect("decode ok")
            .expect("frame");
        prop_assert_eq!(decoded, original);
    }

    #[test]
    fn echo_ping_roundtrips_for_any_u64(ts_ns in any::<u64>()) {
        let original = Message::EchoPing { ts_ns };
        let mut buf = BytesMut::new();
        encode_frame(&original, &mut buf).expect("encode succeeds");
        let decoded = decode_frame(&mut buf)
            .expect("decode ok")
            .expect("frame");
        prop_assert_eq!(decoded, original);
        prop_assert!(buf.is_empty());
    }

    #[test]
    fn echo_pong_roundtrips_for_any_u64(ts_ns in any::<u64>()) {
        let original = Message::EchoPong { ts_ns };
        let mut buf = BytesMut::new();
        encode_frame(&original, &mut buf).expect("encode succeeds");
        let decoded = decode_frame(&mut buf)
            .expect("decode ok")
            .expect("frame");
        prop_assert_eq!(decoded, original);
        prop_assert!(buf.is_empty());
    }
}
