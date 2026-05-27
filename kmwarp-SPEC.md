# kmwarp — Cross-Platform Keyboard/Mouse Sharing

> Name chosen for technical accuracy — the core operation is cursor warping (`CGWarpMouseCursorPosition` on macOS, equivalent on Windows). Verified clear on GitHub and crates.io at time of writing.

## What this is

A software KVM that lets a single keyboard and mouse attached to a Mac control a Windows PC over the local network, mimicking macOS Universal Control across platforms.

**v1 scope:** unidirectional, Mac → Windows, single-monitor edge, two peers, same LAN.

Bidirectional control, multi-monitor topology editing, more than two peers, and cross-LAN relay are explicitly out of scope for v1.

## Goals

- Cursor crosses a configured screen edge from Mac to Windows seamlessly, with sub-30 ms perceived latency on a quiet LAN.
- Keyboard input on the Mac is routed to whichever machine currently owns the cursor.
- Modifier keys remap sensibly across platforms (Cmd↔Ctrl by default, Option↔Alt, configurable).
- Clipboard text syncs both directions.
- Single signed binary per platform, encrypted transport, runs as a background service/daemon.

## Non-goals (v1)

- File drag-and-drop transfer
- Topology editing UI beyond a single edge crossing
- More than two peers
- Mac-as-client (i.e. Windows-as-server)
- Cross-LAN / relay server
- Mobile clients
- Per-app shortcut overrides

## Architecture

Two-process design over a single TCP connection with length-prefixed binary framing and optional TLS.

- **Server** (macOS): owns the physical input. Captures globally via `CGEventTap`, decides per-event whether to consume locally or forward, manages the edge state machine.
- **Client** (Windows): receives events over the wire, injects them via `SendInput`. Signals back when the remote cursor would leave its screen so the server can relinquish control.

Heartbeat every 500 ms; either side declares the connection dead after 2 s of silence.

## Tech stack

- **Language:** Rust (Cargo workspace, three crates).
- **Async runtime:** `tokio`.
- **macOS bindings:** `core-graphics`, `core-foundation`, `objc2` for AppKit bits.
- **Windows bindings:** `windows` (Microsoft official).
- **Transport encryption:** `rustls` with self-signed certs pinned at pairing time.
- **Config:** TOML via `serde` + `toml`.
- **Logging:** `tracing` + `tracing-subscriber`.
- **Packaging:** `cargo-bundle` for macOS `.app`, `cargo-wix` for Windows MSI.
- **Codesigning:** Apple Developer ID + notarization on macOS; Authenticode on Windows. Wire this up by M10, not later.

No Electron, no Node, no Python in the input path. A Tauri-based config UI is acceptable as a separate process (M11).

## Crate layout

```
kmwarp/
├── Cargo.toml                 # workspace
├── crates/
│   ├── core/                  # wire protocol, HID translation, shared types,
│   │                          # edge state machine (platform-agnostic)
│   ├── server/                # macOS binary
│   │   └── platform/macos/    # CGEventTap, cursor warp, NSPasteboard
│   └── client/                # Windows binary
│       └── platform/windows/  # SendInput, clipboard listener, service wrapper
```

Platform layers are behind traits in `core` so the state machine can be unit-tested with mock platforms.

## Wire protocol (v1)

Header: `[u8 msg_type][u16 length LE]`, then payload. All multi-byte ints little-endian.

| Type | Name           | Payload                                                  |
|------|----------------|----------------------------------------------------------|
| 0x01 | Hello          | proto_version: u16, peer_name: utf8                      |
| 0x02 | HelloAck       | accepted: u8, server_screen_px: (u16, u16)               |
| 0x10 | MouseMoveRel   | dx: i16, dy: i16                                         |
| 0x11 | MouseButton    | button: u8, state: u8 (0=up, 1=down)                     |
| 0x12 | MouseWheel     | dx: i16, dy: i16                                         |
| 0x20 | KeyEvent       | hid_usage: u16, state: u8, modifiers: u8 bitmask         |
| 0x30 | ClipboardText  | utf-8 bytes (chunked if > 4 KiB; flag bit in header)     |
| 0x40 | TakeControl    | (server→client) you now own the cursor; entry_y: u16     |
| 0x41 | ReleaseControl | (client→server) cursor leaving back to server; exit_y    |
| 0xFE | Heartbeat      | seq: u32                                                 |
| 0xFF | Bye            | reason_code: u8                                          |

**Critical design choice:** keycodes on the wire are **USB HID usage codes** (HID Usage Page 0x07). Each platform translates at the boundary. Modifier remap (Cmd→Ctrl, etc.) is a config-driven translation layer applied during platform-side encode/decode, never in the wire protocol.

## Edge state machine

```
        ┌──────────────┐  cursor crosses linked edge   ┌─────────────────┐
        │ LocalActive  │ ──────────────────────────►   │ RemoteActive    │
        │ (Mac drives) │ ◄──────────────────────────   │ (Windows drives)│
        └──────────────┘   ReleaseControl received     └─────────────────┘
```

**Transition LocalActive → RemoteActive (server-side):**
1. Send `TakeControl` with the y-coordinate at which the cursor crossed.
2. Warp local cursor 5 px back from the edge.
3. `CGDisplayHideCursor`.
4. Start swallowing tap events (return `NULL` from the callback).
5. Forward subsequent mouse deltas and all keyboard events.

**Transition RemoteActive → LocalActive:**
1. Stop swallowing tap events.
2. `CGDisplayShowCursor`.
3. Warp the local cursor to the edge at the y reported in `ReleaseControl`.

**Stuck-key recovery:** both sides track currently-held keys in `core`. On disconnect (heartbeat timeout, TCP RST, explicit `Bye`), each side synthesizes key-up for everything currently held before tearing down. Same rule on state transitions: never enter `RemoteActive` with held keys on the local side, and vice versa.

## Build order / milestones

Each milestone is demo-able and has a concrete acceptance test. Land them in order; do not start the next without the previous passing.

### M1 — TCP heartbeat (1 evening)
Server and client open a TCP connection, exchange `Hello`/`HelloAck`, then heartbeats every 500 ms.
**Accept:** kill either process; the other logs the loss within 2 s.

### M2 — Mouse capture on macOS (1 evening)
`CGEventTap` on the server logs every mouse move at full rate. Accessibility + Input Monitoring permissions documented in README. Tap auto-reenables on `kCGEventTapDisabledByTimeout`.
**Accept:** moving the mouse logs deltas at ≥ 60 Hz; tap survives a deliberate 200 ms hang in a sibling thread.

### M3 — Mouse injection on Windows (1 evening)
Client accepts test packets from a harness and calls `SendInput` with `MOUSEEVENTF_MOVE`.
**Accept:** harness sending a parametric circle of deltas moves the Windows cursor in a smooth circle.

### M4 — End-to-end mouse (1 evening)
Server forwards every tap delta to the client. Client injects. No edge logic yet — the Mac cursor still moves too. This is throwaway behavior to validate the pipe.
**Accept:** Mac cursor and Windows cursor move 1:1; measure round-trip latency via a timestamped echo (target < 15 ms LAN).

### M5 — Keyboard end-to-end (1–2 evenings)
HID translation tables: macOS virtual keycodes ↔ HID, Win32 VK ↔ HID. Forward key events with modifier bitmask.
**Accept:** typing the alphabet plus numbers plus common punctuation on Mac produces the correct characters in Windows Notepad. Document any deferred keys (media keys, Fn-layer) in a known-issues list.

### M6 — Edge state machine (weekend)
Implement transitions, cursor hide/warp, event swallowing. Hardcoded layout: Windows is immediately right of the Mac at `x == mac_screen_width`.
**Accept:** moving the cursor off the right edge stops moving the Mac cursor and starts moving the Windows cursor. Moving back left returns control. No stuck keys after 50 round trips.

### M7 — Modifier remap + stuck-key safety (1 evening)
Cmd↔Ctrl, Option↔Alt defaults, configurable in `~/.config/kmwarp/config.toml`. Release-all-keys on every disconnect and every control transition.
**Accept:** Cmd+C on Mac produces Ctrl+C on Windows; SIGKILL the server mid-Shift-hold and Windows does not have a stuck Shift.

### M8 — Clipboard sync (1 evening)
macOS: poll `NSPasteboard.changeCount` at 4 Hz. Windows: register a clipboard listener via `AddClipboardFormatListener`. UTF-8 text only in v1.
**Accept:** copy text on either side, paste on the other, with < 500 ms propagation.

### M9 — TLS + pairing (1 evening)
First connect: server displays a 6-digit code; client enters it; both derive a shared secret and pin each other's self-signed cert. Subsequent connects verify the pin.
**Accept:** unpaired client is rejected with a clear log line; paired client reconnects without prompting; tampering with the pin file causes a verification failure, not a silent accept.

#### Pairing UX surfaces (v1.1)

v1.0 implemented the pairing handshake but routed the code through stdin on both sides: the server printed the 6-digit code with `writeln!(stderr, ...)`, the client read it with `tokio::io::stdin().read_line(...)`. v1.1 keeps the wire protocol identical and adds GUI surfaces so neither side needs a terminal:

- **Server (macOS).** When `ServerStatus = Pairing { code }`, the menu bar dropdown shows the code in 22 pt monospace with a "Copy code" item that writes to `NSPasteboard`, and an `NSAlert` is raised once per pairing session announcing the code. The stdout box-around-the-code remains for headless installs.
- **Client (Windows).** The tray-mode build (`kmwarp-client.exe run`) presents a modal Win32 input dialog asking for the 6-digit code (`Shell_NotifyIconW` apps have no console). The dialog runs on `spawn_blocking` so no tokio worker is parked while the user is typing. `KMWARP_HEADLESS=1` (and the `RunAsHelper` service-spawned path) reverts to the v1.0 stdin prompt.

The pairing code itself flows through an injected `CodeProvider` (`Box<dyn FnOnce() -> BoxFuture<'static, Result<String>> + Send>`); the dispatch happens in `client::main::build_windows_dialog_factory` vs `stdin_code_provider`. Wire-level pairing is unchanged.

### M10 — Background service / daemon (several evenings)
macOS: LaunchAgent plist, menu-bar status item. Windows: install as a Windows service so `SendInput` can target UAC-elevated windows.
**Accept:** survives reboot on both sides; reconnects within 5 s of Wi-Fi return; menu bar item shows connected/disconnected.

### M11 — Config UI (optional, deferrable to v1.1)
Tauri app for screen-edge selection and modifier remap. Reads/writes the same TOML.
**Accept:** can change which edge links to Windows and apply without restarting the daemon.

## Gotchas to bake in from day one

- **Latency budget.** Keep the input path zero-allocation in steady state. Pre-size all buffers. Set `TCP_NODELAY` on the socket. Profile with `tracing` spans on every protocol message in debug builds; strip in release.
- **`CGEventTap` callbacks must be fast.** Do not do I/O in the callback. Push events into an unbounded `tokio::sync::mpsc` channel; a dedicated task drains it and writes to the socket.
- **HiDPI normalization.** macOS reports points (logical), Windows APIs report pixels (physical). Normalize at the protocol boundary; the wire format is physical pixels of the server screen. Each platform layer converts.
- **UAC.** Non-elevated `SendInput` cannot reach elevated windows. M10 (Windows service) is therefore mandatory for real use, not optional polish.
- **Key repeat.** The destination OS generates repeats from a sustained held state. Forward press + release only. Filter `kCGKeyboardEventAutorepeat` events from the tap.
- **Permissions UX.** On first launch the server must clearly direct the user to System Settings → Privacy & Security → Accessibility *and* Input Monitoring. Don't silently fail when missing; show a blocking dialog with a button that opens the right pane via `x-apple.systempreferences:`.
- **Coordinate spaces on Windows.** `SendInput` with `MOUSEEVENTF_ABSOLUTE` uses normalized 0..65535 across the virtual screen, not pixels. Use `MOUSEEVENTF_MOVE` (relative) for the main path; absolute only on `TakeControl` to position at the entry point.
- **Sleep/wake.** macOS event taps survive sleep; TCP sockets usually do not. Implement reconnect with exponential backoff (250 ms → 5 s, capped).
- **Codesigning is not optional.** Unsigned macOS event-tap apps work for the developer but fail for any other user. Build the signing pipeline in M10.

## Reference implementations (read, don't copy)

- **Input Leap** — github.com/input-leap/input-leap. Active C++ fork of Barrier. Best living reference for the edge state machine and platform glue.
- **Barrier** — older fork, similar shape, sometimes easier to read.
- **Synergy 1.x** source — historical, but the protocol design holds up.
- **`enigo`, `rdev`** Rust crates — useful as platform-layer references; not suitable as a runtime dependency for the input core.

## Repository conventions

- Conventional Commits; `main` is always shippable.
- `cargo fmt` + `cargo clippy -- -D warnings` in CI.
- Cross-compile not required; build natively on each target. CI runs both a macOS and a Windows job.
- Integration tests live in `crates/core/tests/` and exercise wire protocol round-trips against mock `Platform` implementations.
- Every milestone closes with a tagged release (`v0.1.0-m1`, etc.) and a short demo GIF in the README.

## Out-of-scope reminders (do not let scope creep eat M1–M10)

If during implementation you find yourself wanting to add: file transfer, drag-and-drop, mobile clients, a relay server, multi-monitor topology editing, per-app remaps, gamepad forwarding, or audio — write it in `IDEAS.md` and keep moving.
