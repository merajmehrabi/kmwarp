# IDEAS — deferred work + v1.1 hopper

Items that came up during v1.0 implementation and were consciously
deferred. Each item names the milestone that triggered it and the
nearest v1.1 / vNext slot that should pick it up.

## M10 — Background service / daemon

### Menu bar status item (macOS)

**Status:** deferred to v1.1.

**Spec note:** §M10 calls for "menu bar item shows connected /
disconnected." v1.0 ships headless.

**Why deferred:** `NSStatusItem` requires running on the main thread
alongside an `NSApplication` event loop, which is hostile to our
existing tokio runtime topology. Two paths to add it:

  (a) **AppKit shim on a dedicated thread.** Call
      `NSApplicationLoad()`, start an `NSApplication` event loop on
      a dedicated thread, install `NSStatusItem` with a static icon
      + dynamic title. Communicate state via an `Arc<AtomicU8>`
      (0 = disconnected, 1 = connected, 2 = pairing). ~half a day
      of work.

  (b) **Punt to the M11 Tauri config UI.** The v1.1 milestone
      already plans a Tauri app for edge-config + modifier remap.
      That app is a natural host for the menu bar item; rolling it
      in there avoids the AppKit-thread dance in the headless
      daemon. Preferred path.

**Trigger to revisit:** when M11 (config UI) starts. The Tauri
process becomes the GUI surface; the headless daemon stays headless
and the menu bar lives in the GUI process.

### Logs under `~/Library/Logs/kmwarp/`

**Status:** deferred to v1.1.

v1.0 routes `StandardOutPath` / `StandardErrorPath` to
`/tmp/kmwarp-server.log` / `.err`. The macOS-conventional location
is `~/Library/Logs/kmwarp/`, with daily rotation via
`tracing-subscriber-rolling`. The signed/notarized .pkg installer
should land both at once.

### Signed pkg installer

**Status:** scripts shipped; .pkg generation not yet automated.

`scripts/build-mac.sh` does the universal-binary lipo + codesign +
notarize round trip, but stops at the bare signed binary. The
double-click installer (`.pkg` via `pkgbuild` / `productbuild`)
is a v1.0 nice-to-have that needs Apple Installer signing setup.
The README points users at `target/universal/release/kmwarp-server`
+ `kmwarp-server install` for now.

## M5 — Keyboard end-to-end

- Numpad keys (kVK_ANSI_Keypad0..9, KeypadDecimal, KeypadEnter,
  KeypadPlus, KeypadMinus, KeypadMultiply, KeypadDivide, KeypadClear,
  KeypadEquals) — not in `MACOS_VK_TO_HID`. Users with a numeric
  keypad will type digits via numpad → no events forwarded.
- `kVK_Function` (0x3F, physical Fn key) — omitted from both the HID
  table and the FlagsChanged held-tracker. No clean USB HID code for
  it.
- `kVK_ISO_Section` (0x0A) — only on non-US ISO layouts.
- F13–F20, brightness/volume media keys, eject — Mac-only system
  keys.
- IME / dead keys — explicitly out of v1 scope.

## M6 — Edge state machine

- Multi-monitor topology editing — v1.1 (M11 config UI).
- Configurable edge side (left/top/bottom) — v1.1.

## M8 — Clipboard sync

- Binary clipboards (images, files) — v1 is UTF-8 text only.
- Chunk re-assembly compression flag — `ChunkFlags` reserved bits
  2–7 are available; not used by v1.

## M9 — TLS + pairing

- Multi-peer pin storage — v1's `peer.pin` is single-peer.
- Cert rotation UX — v1 requires `KMWARP_REPAIR=1` to re-pair.
