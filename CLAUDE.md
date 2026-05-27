# kmwarp — agent guide

Rust software-KVM: macOS server captures keyboard/mouse and forwards to a Windows client over TLS+TCP on the LAN. Mimics Universal Control. v1.0 is unidirectional (Mac → Windows).

## Layout

```
crates/core    — pure, no_std-ish: wire codec, edge state machine, HID tables, traits
crates/server  — macOS binary (kmwarp-server) — InputSource + cursor sink
crates/client  — Windows binary (kmwarp-client) — InputSink + clipboard
```

Strict trait boundaries: `core::platform::{InputSource, InputSink, Clipboard}`. `core` must not depend on `core-graphics`, `windows`, or any platform crate.

## Build / test / lint

The repo pins `rust-toolchain.toml = 1.82.0`, but several transitive deps (clap_lex ≥1.1) require **Rust 1.85+**. Always build with stable:

```sh
RUSTUP_TOOLCHAIN=stable cargo build --workspace
RUSTUP_TOOLCHAIN=stable cargo test --workspace
RUSTUP_TOOLCHAIN=stable cargo clippy --workspace --all-targets -- -D warnings
RUSTUP_TOOLCHAIN=stable cargo fmt --check
```

Windows builds run on the user's test box at `ssh meraj@192.168.0.34 -p 2222`.

## Wire protocol

- Length-prefixed frames: `[u8 msg_type][u16 length LE][payload]`.
- Mouse motion is **relative deltas in physical pixels of the server screen**; both sides convert at the platform boundary. Do not change without bumping the protocol version.
- Edge transitions: `TakeControl { entry_y }` (server→client when crossing right), `ReleaseControl { exit_y }` (client→server when returning left).

## Edge state machine (`core/src/edge/mod.rs`)

- Pure. No tokio. No clock reads — every entry point takes `now_ms: u64`.
- States: `LocalActive` ↔ `RemoteActive`.
- **Thrash cooldown is asymmetric**: applied only to L→R (`on_local_event`). `ReleaseControl` from the peer must always land — `cursor_watch` on the client only fires once per leave, so dropping it would deadlock the SM in `RemoteActive` forever. Do not "fix" by adding a cooldown check to `on_wire_message`.
- Multi-monitor: `server::app` snapshots per-display geometry on startup and tracks `home_display_right` — the edge crossing happens at the rightmost extent of the display the cursor is currently on, NOT the main display's width and NOT the global max. Half-open bounds: cursor maxes at `display.right - 1`, so the crossing test is `x >= d_right - 1`.

## macOS server gotchas

- `CGEventTap` callback is sync and time-critical → `mpsc::unbounded` drain task. Tap runs on a dedicated `std::thread::spawn` with its own `CFRunLoop`, NOT on tokio.
- `hide_cursor()` must also call `CGAssociateMouseAndMouseCursorPosition(false)`. Hiding alone leaves the invisible cursor following the user's hand — Mac cursor drifts during RemoteActive and re-crosses the edge immediately. `show_cursor()` must re-associate FIRST, then unhide.
- After every `warp_mouse_cursor_position`, immediately re-associate — macOS suppresses cursor events for ~250ms after a warp otherwise.
- On `TakeControl`, client warps cursor 40px inset from the entry edge (`TAKE_ENTRY_INSET_PX` in `client/src/net/pump.rs`) to defeat the ReleaseControl-bounce deadlock.
- TCC permissions required: Accessibility + Input Monitoring. M2 demo (`KMWARP_M2_DEMO=1`) is the canonical first-run sanity check.

## Windows client gotchas

- `windows-rs` 0.58 dropped `Option<T>` wrapping of bare handle types. `HWND`, `HANDLE`, `HINSTANCE`, etc. are passed bare — do NOT wrap in `Some()`. `PCWSTR::null()` for optional string params. `RegisterClassW`/`WNDCLASSW` live behind the `Win32_Graphics_Gdi` feature. `GlobalFree` is in `Win32::Foundation`, not `Win32::System::Memory`.
- DPI manifest required for HiDPI; otherwise coordinates skew.
- Session 0 isolation: a service running as `LocalSystem` can't `SendInput` to the user desktop. Use the helper-spawn pattern — service `WTSQueryUserToken` + `CreateProcessAsUserW` into the active session.

## Style

- No `unwrap()` outside tests (enforced by `clippy.toml`).
- No `anyhow` in `core`; binaries use it only at `fn main()` boundary.
- `thiserror` for typed error enums per crate.
- Default: no comments. Add one only when the **why** is non-obvious (hidden constraint, subtle invariant, workaround for a specific bug). Never write what the code does.

## Where things live

- Spec: `kmwarp-SPEC.md` — source of truth, re-read at each milestone.
- Plan: `PLAN.md` — M0→M10 roadmap (v1.0 complete).
- Future work: `IDEAS.md`.

## Status

v0.1.0 shipped (tag `v0.1.0`). v1.1 in flight: menu bar status item (`crates/server/src/service/menubar.rs`).
