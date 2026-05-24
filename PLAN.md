# kmwarp — Implementation Plan (M0 → M10)

## Context

`/Users/meraj/Desktop/Dev/KMwarp/` is greenfield: only `kmwarp-SPEC.md` exists. The spec prescribes a Rust software-KVM that lets a Mac keyboard/mouse drive a Windows PC over LAN, mimicking macOS Universal Control. v1 is unidirectional (Mac → Windows), single edge, two peers, same LAN.

The spec already breaks v1 into milestones M1–M10 with acceptance criteria. The gaps this plan fills:

1. **M0 (scaffolding)** — the spec assumes a workspace exists; it doesn't.
2. **Cross-cutting decisions** locked once so each milestone is mechanical (trait surfaces, channel topology, codec shape, error model, config schema, logging contract).
3. **Concrete file/module layout** per crate so milestone work knows where things go.
4. **Pre-milestone calendar risks** — Apple Developer enrollment paperwork must start at M1, not M10.

Target hardware: physical Windows PC on same LAN (confirmed). M9 pairing uses **SPAKE2** (PAKE) — locked per user choice.

---

## Approach

Single Cargo workspace, 3 crates: `core` (platform-agnostic protocol + state machine + traits), `server` (macOS binary), `client` (Windows binary). Strict trait boundaries: `core` knows nothing about `CGEventTap` or `SendInput`; platform layers are thin adapters behind `InputSource` / `InputSink` / `Clipboard` traits. State machine and codec are pure, fully unit-testable with mock platforms.

Milestones land sequentially; each closes with a tagged release and the spec's acceptance test passing on real hardware.

---

## M0 — Scaffolding (do before M1)

Order matters; one commit per step.

1. `git init` in `/Users/meraj/Desktop/Dev/KMwarp/`; first commit is the existing spec.
2. **Workspace `Cargo.toml`** at repo root: `members = ["crates/core", "crates/server", "crates/client"]`, `resolver = "2"`, `[workspace.package]` with shared `version = "0.1.0"`, `edition = "2021"`, `[workspace.dependencies]` pinning everything used by 2+ crates: `tokio` (full), `bytes`, `serde`, `toml`, `thiserror`, `tracing`, `tracing-subscriber`, `rustls`, `tokio-rustls`, `rcgen`, `spake2`, `sha2`, `hex`, `directories`.
3. `rust-toolchain.toml` — `channel = "1.82.0"`, `components = ["rustfmt", "clippy"]`.
4. `rustfmt.toml` — `edition = "2021"`, `max_width = 100`, `imports_granularity = "Crate"`.
5. `clippy.toml` — `msrv = "1.82"`, disallow `.unwrap()` in non-test code.
6. `.gitignore` — `/target`, `.DS_Store`, NOT `Cargo.lock` (binary workspace).
7. **`crates/core/Cargo.toml`** — deps: `bytes`, `serde` (+derive), `thiserror`, `tracing`. Dev-deps: `tokio` (macros, rt, test-util), `proptest`. No platform deps. No `anyhow`.
8. **`crates/server/Cargo.toml`** — deps: `core` (path), `tokio`, `tokio-rustls`, `rustls`, `rcgen`, `spake2`, `sha2`, `hex`, `serde`, `toml`, `thiserror`, `anyhow`, `tracing`, `tracing-subscriber`, `directories`. `[target.'cfg(target_os = "macos")'.dependencies]`: `core-graphics`, `core-foundation`, `objc2`, `objc2-app-kit`, `objc2-foundation`. `[[bin]] name = "kmwarp-server"`.
9. **`crates/client/Cargo.toml`** — same core deps. `[target.'cfg(target_os = "windows")'.dependencies]`: `windows` with features `["Win32_UI_Input_KeyboardAndMouse", "Win32_UI_WindowsAndMessaging", "Win32_System_DataExchange", "Win32_System_Memory", "Win32_Foundation", "Win32_System_Threading"]`, plus `windows-service`. `[[bin]] name = "kmwarp-client"`.
10. Placeholder `crates/{server,client}/src/main.rs` that init `tracing_subscriber::fmt`, log "hello", exit. `crates/core/src/lib.rs` empty.
11. `.github/workflows/ci.yml` — matrix `{ os: [macos-latest, windows-latest] }`, steps: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. Conditional builds: macOS → `kmwarp-server`, Windows → `kmwarp-client`, both → `kmwarp-core`.
12. **Out-of-band**: start Apple Developer Program enrollment (~7-day wait). This unblocks M10 codesigning; spec is explicit it's not optional.

**Verify M0:** `cargo build --workspace` succeeds on macOS host (Windows-gated client deps must not break the macOS build).

---

## Cross-cutting design (lock before M1)

### Trait surface in `core::platform`

Split into three traits so neither binary implements unused methods:

```rust
trait InputSink {
    fn inject_mouse_rel(&mut self, dx: i32, dy: i32);
    fn inject_mouse_button(&mut self, btn: MouseButton, state: KeyState);
    fn inject_mouse_wheel(&mut self, dx: i16, dy: i16);
    fn inject_key(&mut self, hid: u16, state: KeyState, mods: ModMask);
    fn warp_cursor_abs(&mut self, x: i32, y: i32);
    fn hide_cursor(&mut self);
    fn show_cursor(&mut self);
}

trait InputSource {
    async fn next_event(&mut self) -> SourceEvent; // MouseRel, MouseButton, MouseWheel, Key, CursorAt
}

trait Clipboard {
    fn read_text(&self) -> Option<String>;
    fn write_text(&mut self, s: &str);
    async fn next_change(&mut self) -> ClipboardEvent;
}
```

Server impls `InputSource` + cursor-control parts of `InputSink` (warp/hide) + `Clipboard`. Client impls full `InputSink` + `Clipboard`.

### Async channel topology

- `CGEventTap` callback (sync, time-critical) → `tokio::sync::mpsc::unbounded` → drain task. **Unbounded** because callback must never block; drain task coalesces consecutive `MouseMoveRel` if backlog > 100.
- Drain task → `mpsc::channel(256)` → wire encoder. **Bounded**; encoder is the natural backpressure point, `try_send` and drop with `warn!` if socket stalls.
- Socket reader → `mpsc::channel(256)` → injector task. **Bounded**; injection paced by OS.

### Wire codec

`core::wire` exposes:
```rust
pub fn encode_frame(msg: &Message, buf: &mut BytesMut);          // alloc-free steady state
pub fn decode_frame(buf: &mut BytesMut) -> Result<Option<Message>, WireError>;  // Ok(None) = need more bytes
```
Use `bytes::BytesMut`. `Message` enum mirrors the spec table 1:1. Header is `[u8 msg_type][u16 length LE]` as specified.

### Error model

`thiserror` per crate, no `anyhow` in `core`. `anyhow::Result<()>` only inside `fn main()` of each binary. Concrete: `core::WireError`, `core::StateError`, `server::ServerError` (wraps `core::*` + `io::Error` + `rustls::Error`), `client::ClientError` mirror.

### HID translation

`core::hid::macos::MACOS_VK_TO_HID: &[(u16, u16)]` and `core::hid::windows::WIN32_VK_TO_HID: &[(u16, u16)]` — pure const slices, no `lazy_static`. Helpers `pub fn macos_to_hid(vk) -> Option<u16>` etc. Tables grow as M5 progresses. `ModMask` bitfield defined in `core::hid::mod.rs`.

### Edge state machine

`core::edge::StateMachine` owns `enum State { LocalActive, RemoteActive }`. Method `fn on_event(&mut self, e: SourceEvent, screen_w: u32) -> SmallVec<[Action; 4]>` returns `Action::{SendTakeControl{y}, SendReleaseControl{y}, WarpLocal{x,y}, HideCursor, ShowCursor, StartSwallow, StopSwallow, ForwardMouse{..}, ForwardKey{..}}`. Server applies actions; state machine itself is pure and fully testable with no platform deps.

### Config

`~/.config/kmwarp/config.toml` (mac) and `%APPDATA%\kmwarp\config.toml` (win), resolved via `directories` crate.

```toml
[peer]
bind = "0.0.0.0:51423"       # server
connect = "10.0.0.5:51423"   # client
name = "merajs-mbp"

[edge]
side = "right"               # right|left|top|bottom
remote_screen_px = [2560, 1440]

[modifiers]
cmd = "ctrl"
option = "alt"

[tls]
pin_file = "~/.config/kmwarp/peer.pin"
```

### Logging contract

`tracing` everywhere. `#[cfg(debug_assertions)]` adds per-message `info_span!("msg.{Variant}", ..)` in codec encode/decode; release builds strip via compile-time gate. `RUST_LOG=kmwarp=debug` env override via `tracing_subscriber::EnvFilter::from_default_env()`.

### TCP defaults

`TCP_NODELAY = true` set in M1 connection code. **Do not defer.**

---

## Crate file tree (target at v1.0)

```
crates/core/src/
├── lib.rs
├── wire/{mod.rs, codec.rs, tests.rs}    # Message enum + framing
├── hid/{mod.rs, macos.rs, windows.rs}   # VK ↔ HID tables, ModMask
├── edge/{mod.rs, tests.rs}              # StateMachine, Action
├── platform.rs                          # InputSink, InputSource, Clipboard traits
├── config.rs                            # Config struct + load_from_path
├── stuck_keys.rs                        # HeldKeys tracker
└── error.rs

crates/server/src/
├── main.rs                              # arg parse, runtime
├── app.rs                               # top-level loop + reconnect backoff
├── net/{mod.rs, connection.rs, pairing.rs}
├── platform/{mod.rs, macos/{mod.rs, tap.rs, inject.rs, clipboard.rs, permissions.rs}}
├── service/{mod.rs, launchagent.rs, menubar.rs}
└── config_paths.rs

crates/client/src/
├── main.rs
├── app.rs
├── net/{mod.rs, connection.rs}
├── platform/{mod.rs, windows/{mod.rs, inject.rs, clipboard.rs, dpi.rs}}
├── service/{mod.rs, windows_service.rs}
└── config_paths.rs
```

---

## Milestone expansion

For each: new files, key APIs, and risks beyond the spec's gotchas. Acceptance tests are exactly as the spec specifies — referenced as "Accept: per spec Mn."

### M1 — TCP heartbeat
- **New:** `core/wire/{mod.rs, codec.rs}` (Hello, HelloAck, Heartbeat, Bye only), `core/error.rs`, `server/net/connection.rs`, `client/net/connection.rs`, `server/app.rs`, `client/app.rs`.
- **APIs:** `Connection::{read_frame, write_frame}`, `Heartbeat::spawn(tx, 500ms)`, `DeadlineWatcher::new(2s)`, `pub async fn run_server(cfg)`, `pub async fn run_client(cfg)`.
- **Risks:** Tokio task lifetime — use `tokio::select!` + `JoinSet` so heartbeat aborts on connection drop. Set `TCP_NODELAY` here. **Out-of-band: kick off Apple Developer Program enrollment today.**
- Accept: per spec M1.

### M2 — Mouse capture (macOS)
- **New:** `server/platform/macos/{mod.rs, tap.rs, permissions.rs}`, `core/platform.rs` (`InputSource` trait), `core/stuck_keys.rs` (skeleton).
- **APIs:** `MacInputSource { rx: UnboundedReceiver<SourceEvent> }`, `fn install_tap(tx) -> Result<CFRunLoopSourceRef>`, `fn check_permissions() -> PermStatus`. Tap auto-reenables on `kCGEventTapDisabledByTimeout` from the first commit.
- **Risks:** `CFRunLoopRun` on dedicated `std::thread::spawn`, not on tokio. Permissions deep-link via `x-apple.systempreferences:` URL.
- Accept: per spec M2.

### M3 — Mouse injection (Windows)
- **New:** `client/platform/windows/{mod.rs, inject.rs, dpi.rs}`. Test harness binary or integration test that feeds synthetic deltas.
- **APIs:** `WinInputSink` impl `InputSink`; `fn send_mouse_rel(dx, dy)`, `fn send_mouse_abs_norm(x, y)` (0..65535), `fn screen_size_px() -> (u32, u32)`.
- **Risks:** DPI awareness manifest required (app manifest or `SetProcessDpiAwarenessContext`) — without it, coordinates skew on HiDPI. Clamp wire i16 deltas safely into `LONG`.
- **30-min spike:** verify a non-elevated `SendInput` reaches user desktop. De-risks M10's session-0 question.
- Accept: per spec M3.

### M4 — End-to-end mouse
- **New:** Extend `core/wire` with `MouseMoveRel`, `MouseButton`, `MouseWheel`. Wire server source → connection → client sink. Add debug-only `EchoPing/EchoPong` messages.
- **APIs:** `async fn encoder_loop(rx, sock_w)`, `async fn injector_loop(sock_r, sink)`. Latency harness uses `hdrhistogram`.
- **Risks:** HiDPI normalization decision — fix wire convention as **physical pixels of server screen**; both sides convert at platform boundary. Decided here, not deferred.
- Accept: per spec M4.

### M5 — Keyboard end-to-end
- **New:** `core/hid/{macos.rs, windows.rs}` populated, `core/wire` `KeyEvent`. Server tap handles `kCGEventKeyDown/Up`, filters `kCGKeyboardEventAutorepeat`. Client `inject.rs` adds `send_key(hid, down)` via `KEYEVENTF_SCANCODE`.
- **APIs:** `core::hid::macos_to_hid(vk) -> Option<u16>`, `core::hid::windows::hid_to_scancode(h) -> Option<u16>`. `ModMask` bits finalized.
- **Risks:** Dead keys / IME — explicitly out of v1 scope; document in `IDEAS.md`. Key-repeat filtering on macOS side from day one.
- Accept: per spec M5.

### M6 — Edge state machine
- **New:** `core/edge/{mod.rs, tests.rs}`. `core/wire` adds `TakeControl`, `ReleaseControl`. Server `MacInputSink` for cursor warp + hide (server-side `InputSink` now needed).
- **APIs:** `StateMachine::on_event(...) -> SmallVec<[Action; 4]>`. Client polls cursor position to detect leaving its screen and emits `ReleaseControl`.
- **Risks:** Edge thrash — 5 px back-warp + 50 ms cooldown. Hardcoded layout (Windows immediately right of Mac); topology becomes config in v1.1.
- Accept: per spec M6.

### M7 — Modifier remap + stuck-key safety
- **New:** `core/config.rs` parses `[modifiers]`. `core/stuck_keys::HeldKeys` fully implemented; drain hooked into both sides' shutdown + every state transition.
- **APIs:** `ModRemap::apply(in: ModMask) -> ModMask`. `HeldKeys::{insert, remove, drain_release_actions() -> Vec<Action>}`.
- **Risks:** Drain must run synchronously on TCP RST detection before encoder task exits. Cover with a unit test that simulates abrupt source termination mid-hold.
- Accept: per spec M7.

### M8 — Clipboard sync
- **New:** `server/platform/macos/clipboard.rs` (4 Hz `NSPasteboard.changeCount` poll), `client/platform/windows/clipboard.rs` (`AddClipboardFormatListener` + hidden message-only window). `core/wire/ClipboardText` (chunked >4 KiB via flag bit).
- **APIs:** `trait Clipboard` impls; `pub async fn watch_clipboard() -> impl Stream<Item = String>`.
- **Risks:** Echo loops — track last-set SHA-256 and ignore changeCount bumps that match. Chunk reassembly buffer in `wire::codec`.
- Accept: per spec M8.

### M9 — TLS + pairing (SPAKE2)
- **New:** `server/net/pairing.rs`, both sides wrap `Connection` in `tokio_rustls::TlsStream`. `rcgen` for self-signed cert at first launch. Pin file at `~/.config/kmwarp/peer.pin` (SHA-256 of peer cert DER, hex-encoded).
- **APIs:** `fn generate_self_signed() -> (Certificate, PrivateKey)`, `fn pin_hash(cert: &Certificate) -> [u8; 32]`. Pairing flow:
  1. Server generates 6-digit code, displays it, derives SPAKE2 element A.
  2. Client prompts for code, derives SPAKE2 element B, exchanges.
  3. Both derive shared key K via SPAKE2.
  4. Exchange cert DER blobs inside a frame authenticated with `HMAC-SHA256(K, cert_der)`.
  5. Each side verifies HMAC, then writes the peer's pin hash to disk.
- **Risks:** SPAKE2 needs a deterministic point-derivation from the password — use the `spake2` crate's `Identity` API. Document threat model in repo `SECURITY.md` (one paragraph).
- Accept: per spec M9.

### M10 — Background service / daemon
- **New:** `server/service/{launchagent.rs, menubar.rs}`, `client/service/windows_service.rs`. `scripts/` for build, sign, notarize: `cargo-bundle` → Apple `codesign` → `xcrun notarytool submit`; Windows `cargo-wix` → `signtool sign`.
- **APIs:** `fn install_launchagent()` writes `~/Library/LaunchAgents/com.kmwarp.server.plist` and `launchctl load`s it. Windows entry uses `windows-service` crate.
- **Risks:**
  - **Codesigning** — Apple Developer enrollment must already be complete (started at M1). First notarization round-trip is ~1 hour each attempt; expect 3-5 attempts to get entitlements right.
  - **Windows session 0 isolation** — `SendInput` from a service running as `LocalSystem` cannot reach the user desktop. Use a "service + user-session helper" split: the service registers, then spawns a helper into the active session via `WTSQueryUserToken` + `CreateProcessAsUser`. This is the M3 spike's payoff.
- Accept: per spec M10.

---

## Verification

### Unit tests (in `crates/core/tests/`)

- `wire_roundtrip.rs` — table-driven encode→decode for every `Message`; proptest on `MouseMoveRel` with arbitrary `(i16, i16)`.
- `edge_transitions.rs` — drive `StateMachine` with scripted `SourceEvent` sequences against a mock recorder `InputSink`; assert action sequences and the no-stuck-keys-after-50-round-trips invariant.
- `stuck_keys.rs` — held set + drain yields correct release `Action` list.
- `hid_tables.rs` — assert bijection (no duplicate HID codes), ASCII coverage spot-check.

### Per-milestone manual verification

Every milestone close runs the spec's acceptance test on Mac + physical Windows box. Result captured in a one-paragraph note attached to the tagged release (`v0.1.0-m1` … `v0.1.0-m10`).

### Latency harness (M4 onward)

Debug-only `EchoPing { ts_ns: u64 }` / `EchoPong { ts_ns: u64 }` messages (gated behind `--features latency-probe`). Server timestamps with `Instant::now()`, client echoes immediately, server computes round-trip. Print p50/p95/p99 via `hdrhistogram` every 1000 samples. Run 10 s of continuous mouse motion; target p95 < 15 ms LAN.

### Stuck-key reproduction (M7)

`crates/client/tests/stuck_key_recovery.rs` with mock `InputSink` recording all injections:
1. Mock `InputSource` injects 5 `KeyEvent{Shift, down}`.
2. Wrapper SIGKILLs the server process.
3. Client must observe disconnect within 2 s and inject `KeyEvent{Shift, up}` before exiting.
4. Assert recorded sink contains the trailing release.

Full-hardware version (real Notepad) only at tag-time.

---

## Critical files

- `/Users/meraj/Desktop/Dev/KMwarp/kmwarp-SPEC.md` — source of truth; re-read at start of each milestone.
- `/Users/meraj/Desktop/Dev/KMwarp/Cargo.toml` — workspace root, created in M0.
- `/Users/meraj/Desktop/Dev/KMwarp/crates/core/src/wire/codec.rs` — extended in M1, M4, M5, M6, M8. Central protocol artifact.
- `/Users/meraj/Desktop/Dev/KMwarp/crates/core/src/edge/mod.rs` — M6's correctness story; pure logic, heavily tested.
- `/Users/meraj/Desktop/Dev/KMwarp/crates/server/src/platform/macos/tap.rs` — M2; hardest single platform file (run-loop + permissions + auto-reenable).
- `/Users/meraj/Desktop/Dev/KMwarp/crates/client/src/platform/windows/inject.rs` — M3 + M5; second hardest (DPI + scancode mapping).
- `/Users/meraj/Desktop/Dev/KMwarp/crates/server/src/net/pairing.rs` — M9; SPAKE2 + cert pinning.

---

## Sequencing notes

- **Parallelization:** M2 (mac source) and M3 (windows sink) can run in parallel after M1 — both depend only on M1 and produce independent platform layers. Everything else is strictly serial.
- **Time estimates to revise upward from the spec:**
  - M2: 2+ evenings (run-loop threading, permissions, auto-reenable, 200 ms hang test).
  - M3: ≥ 1 evening with DPI manifest and clamping quirks.
  - M9: 2–3 evenings minimum (SPAKE2 wiring + pairing UX + pin storage).
  - M10: 1–2 weeks (first-time Apple notarization + Windows session 0 split).
- **Calendar-blocking risks:**
  - Apple Developer Program enrollment — start at M1, takes up to 7 days.
  - Notarization round-trips — each ~1 hour, plan 3–5 attempts.

---

## Out-of-scope reminders (stays out of v1)

File transfer, drag-and-drop, mobile clients, relay server, multi-monitor topology, per-app remaps, gamepad forwarding, audio. Anything that would be tempting to add mid-milestone goes into `IDEAS.md` and the milestone keeps moving.
