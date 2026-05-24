//! M9 crypto integration tests — cert, pin, SPAKE2, HMAC, and the
//! end-to-end pairing flow shape.

use kmwarp_core::error::{PairingError, TlsError};
use kmwarp_core::pairing::{
    cert_hmac, cert_hmac_verify, generate_code, ClientPairing, ServerPairing, SHARED_KEY_LEN,
};
use kmwarp_core::tls::{generate_self_signed, load_from_disk, pin_hash_of, save_to_disk, PinStore};
use tempfile::tempdir;

#[test]
fn cert_generation_produces_loadable_pkcs8() {
    let dir = tempdir().expect("tempdir");
    let cert_path = dir.path().join("cert.der");
    let key_path = dir.path().join("key.der");

    let b = generate_self_signed().expect("generate");
    save_to_disk(&b, &cert_path, &key_path).expect("save");

    let loaded = load_from_disk(&cert_path, &key_path).expect("load");
    assert_eq!(loaded.cert_der, b.cert_der);
    assert_eq!(loaded.private_key_der, b.private_key_der);
}

#[test]
fn pin_hash_matches_sha256_fixture() {
    // SHA-256("abc") = ba7816bf...f20015ad
    let h = pin_hash_of(b"abc");
    assert_eq!(
        hex::encode(h),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn pin_store_roundtrip_through_disk() {
    let dir = tempdir().expect("tempdir");
    let store = PinStore::new(dir.path().join("peer.pin"));
    assert!(store.load().expect("ok").is_none());

    let bundle = generate_self_signed().expect("gen");
    let pin = pin_hash_of(&bundle.cert_der);
    store.store(&pin).expect("store");

    let loaded = store.load().expect("load").expect("present");
    assert_eq!(loaded, pin);
}

#[test]
fn pin_store_tampered_file_yields_pin_file_corrupt() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("peer.pin");
    std::fs::write(&path, "tampered!").expect("write");
    let store = PinStore::new(&path);
    assert!(matches!(store.load(), Err(TlsError::PinFileCorrupt(_))));
}

#[test]
fn spake2_full_roundtrip_with_real_cert_hmac() {
    // Generate the code on the server.
    let code = generate_code().expect("rng");

    // SPAKE2 element exchange.
    let (server, msg_a) = ServerPairing::start(&code).expect("server start");
    let (client, msg_b) = ClientPairing::start(&code).expect("client start");
    let k_server = server.finish(&msg_b).expect("server finish");
    let k_client = client.finish(&msg_a).expect("client finish");
    assert_eq!(k_server, k_client);
    assert_eq!(k_server.len(), SHARED_KEY_LEN);

    // Each side generates its self-signed cert.
    let server_cert = generate_self_signed().expect("server cert");
    let client_cert = generate_self_signed().expect("client cert");

    // Each side computes HMAC over its own cert DER under K.
    let server_hmac = cert_hmac(&k_server, &server_cert.cert_der);
    let client_hmac = cert_hmac(&k_client, &client_cert.cert_der);

    // Each side verifies the other's HMAC against its own (matching) K.
    let server_recomputes_client_hmac = cert_hmac(&k_server, &client_cert.cert_der);
    let client_recomputes_server_hmac = cert_hmac(&k_client, &server_cert.cert_der);
    assert!(cert_hmac_verify(
        &client_recomputes_server_hmac,
        &server_hmac
    ));
    assert!(cert_hmac_verify(
        &server_recomputes_client_hmac,
        &client_hmac
    ));

    // Each side pins the OTHER side's cert DER.
    let pin_server_keeps = pin_hash_of(&client_cert.cert_der);
    let pin_client_keeps = pin_hash_of(&server_cert.cert_der);
    assert_ne!(pin_server_keeps, pin_client_keeps);
}

#[test]
fn spake2_wrong_code_diverges_keys_so_hmac_fails() {
    let (server, msg_a) = ServerPairing::start("111111").expect("server");
    let (client, msg_b) = ClientPairing::start("222222").expect("client");
    let k_server = server.finish(&msg_b).expect("server");
    let k_client = client.finish(&msg_a).expect("client");
    assert_ne!(k_server, k_client, "wrong code → different keys");

    // The cert exchange uses the (now different) keys. HMAC verify
    // catches the mismatch.
    let cert = b"fake cert der".to_vec();
    let server_hmac = cert_hmac(&k_server, &cert);
    let client_computes = cert_hmac(&k_client, &cert);
    assert!(!cert_hmac_verify(&server_hmac, &client_computes));
}

#[test]
fn cert_hmac_verify_catches_flipped_bit() {
    let k = [0x42u8; 32];
    let cert = b"cert bytes";
    let mut tag = cert_hmac(&k, cert);
    let original = tag;
    for byte_idx in [0, 7, 15, 31] {
        tag = original;
        tag[byte_idx] ^= 0x01;
        assert!(
            !cert_hmac_verify(&original, &tag),
            "flipped bit at {byte_idx} should fail verify"
        );
    }
}

#[test]
fn generate_code_passes_validation() {
    for _ in 0..50 {
        let code = generate_code().expect("rng");
        // Server should accept the codes it generates.
        ServerPairing::start(&code).expect("server accepts");
        ClientPairing::start(&code).expect("client accepts");
    }
}

#[test]
fn server_pairing_rejects_non_6_digit_codes() {
    for bad in &["12345", "1234567", "12345a", "abcdef", "      "] {
        assert!(
            matches!(
                ServerPairing::start(bad),
                Err(PairingError::CodeMustBe6Digits)
            ),
            "should reject {bad:?}"
        );
    }
}
