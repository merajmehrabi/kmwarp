# kmwarp — Security & Threat Model

## What kmwarp protects

kmwarp's wire path is the M9 onward stack:

```
keyboard/mouse events
       │
       ▼
[ application ] ── [ TLS 1.3 via tokio_rustls ] ── [ TCP ] ── network ── peer
                              ▲
                              │
                  self-signed cert generated at first launch,
                  pinned via SHA-256(cert_der) at pairing time
```

**Confidentiality + integrity in transit** is provided by TLS 1.3. The self-signed
certs are pinned out-of-band at first connect using **SPAKE2** with a 6-digit code
the user reads off the server's display and types on the client. After pairing,
each side stores the other's pin (hex-encoded SHA-256 of the cert DER) at
`~/.config/kmwarp/peer.pin`; subsequent connects verify the pin and refuse to
proceed on mismatch.

**SPAKE2** specifically is a password-authenticated key exchange — a network
attacker who intercepts the SPAKE2 messages learns *nothing* about the 6-digit
code (the protocol is information-theoretically secure against passive
observers, and offline brute-force is bounded by one online attempt per connection
attempt by the protocol's design). The HMAC-SHA256 step authenticates the cert
DER exchange itself under the SPAKE2-derived shared key K.

## Threat model

### In scope (kmwarp v1 actively defends)

- **Passive eavesdropping on the LAN.** All input and clipboard data is
  encrypted in transit by TLS. SPAKE2 prevents a network-only attacker from
  learning the pairing code from the wire.
- **Active MitM at first pair.** SPAKE2 + HMAC pin exchange ensures that a
  network attacker cannot insert their own cert between the two peers; the
  pin written to disk binds to the *real* peer's cert from then on.
- **Active MitM after first pair.** Pin verification on every connect catches
  any later attempt to substitute the cert (corporate proxy, rogue AP, etc.).
  Tampering with `peer.pin` to make it accept a different cert is detected by
  the next connection's HMAC-stage failure (would require the attacker to
  *also* know the pairing code).
- **Stuck modifiers on disconnect.** Held-key drain on every state transition
  and disconnect path (M7) — independent of cryptographic concerns, but
  matters for the input safety story.

### Explicitly out of scope (v1)

- **Local attacker with kernel access.** Anyone with root on either Mac or
  Windows can read all input through OS-level taps anyway. kmwarp's binaries
  and the unencrypted segment between the OS event taps and the TLS stream
  are trusted.
- **Local attacker with user-account access who can read** `peer.pin` **and**
  `~/.config/kmwarp/config.toml`. The pin file's secrecy doesn't matter for
  defense (knowing the pin only tells you the SHA-256 of the cert, which is
  public information once you've seen the cert); but a user-account attacker
  who can swap the binaries entirely or hook `SendInput` defeats anything
  this protocol does.
- **DoS by repeated connection attempts.** No rate-limiting in v1.
- **Side channels** beyond the constant-time HMAC verify already in use.
  Timing of keystrokes, mouse motion, or clipboard size *is* observable
  through encrypted-traffic analysis; kmwarp does not pad.
- **Compromised Apple Developer ID or Authenticode signing keys.** The
  signed/notarized binaries (M10) are trusted by the OS; if the signing
  identity is compromised the attacker can ship a malicious replacement.
  No code-signing rotation story in v1.
- **Cross-LAN / relay.** v1 is same-LAN only. Once relayed traffic enters
  scope (v2+), the threat model expands to include the relay operator.
- **More than two peers, mobile clients, file transfer, gamepad
  forwarding, audio.** None of these exist in v1; see PLAN.md
  §Out-of-scope.

### Known weaknesses we accept for v1

- **6-digit code is brute-forceable by an active MitM with one online attempt
  per pairing.** A user who reconnects 1,000,000 times with mismatched codes
  could be exhaustively probed. Pairing is meant to be a one-time event; if
  this is a concern, increase `PAIRING_CODE_DIGITS` in
  `core::pairing` and rebuild.
- **No revocation.** If a Mac is stolen, the only remedy is to delete the
  pin file on the Windows side (or vice versa).
- **No forward secrecy beyond TLS 1.3's own.** Each TLS session uses
  ephemeral keys, but the long-term cert is the same across sessions until
  re-pairing.

## How to re-pair

Delete `~/.config/kmwarp/peer.pin` on both sides and reconnect. The server
displays a fresh 6-digit code; the client prompts for it.

## How to report vulnerabilities

Email merajmehrabi@gmail.com (the project owner). Please include reproduction
steps if you have them; no bug-bounty program for v1.
