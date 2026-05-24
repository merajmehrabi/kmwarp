# kmwarp

Cross-platform keyboard / mouse sharing. A Mac drives a Windows PC over
the LAN, mimicking macOS Universal Control.

v1.0 scope: unidirectional (Mac → Windows), single-monitor edge, two
peers on the same LAN, encrypted transport (TLS + SPAKE2 pairing).

See `kmwarp-SPEC.md` for the full spec, `PLAN.md` for the milestone
breakdown, and `IDEAS.md` for v1.1+ follow-ups.

## Build

Requires Rust 1.82+ on macOS (server) and Windows (client). The
workspace is portable; only the platform-specific crates are
cfg-gated.

```sh
cargo build --release --workspace
```

The macOS server binary lands at `target/release/kmwarp-server`,
the Windows client at `target/release/kmwarp-client.exe`.

## Install (macOS server)

Once you have a binary, install it as a user LaunchAgent so it
starts at login and survives crashes / Wi-Fi blips:

```sh
target/release/kmwarp-server install
```

This:

- Writes `~/Library/LaunchAgents/com.kmwarp.server.plist`.
- `launchctl load -w`s it (loaded immediately + at every future
  login).
- Configures launchd to restart the agent on any non-zero exit
  (`KeepAlive.SuccessfulExit = false`) and when the network comes
  back (`KeepAlive.NetworkState = true`).

After install:

1. Grant **Accessibility** and **Input Monitoring** permissions
   to `kmwarp-server` in System Settings → Privacy & Security.
   (The plist's `ProgramArguments` records the absolute path of
   the binary you installed; that's the entry you need to
   approve.)
2. Watch the agent's stdout/stderr:
   ```sh
   tail -f /tmp/kmwarp-server.log /tmp/kmwarp-server.err
   ```
3. First connect from the Windows client triggers the SPAKE2
   pairing flow — the server logs a 6-digit code, you type it
   into the client. The two sides then pin each other's TLS
   certificates and subsequent connects auto-authenticate.

Uninstall is the symmetric `target/release/kmwarp-server uninstall`
— removes the plist and `launchctl unload -w`s it. Idempotent.

The CLI also accepts `--help` and `--version`:

```sh
target/release/kmwarp-server --help
```

## Signed / notarized release builds

For builds suitable for distribution to other users (Gatekeeper-
green, no "unidentified developer" warnings), use the build script:

```sh
export DEVELOPER_ID_APPLICATION="Developer ID Application: Your Name (TEAMID)"
scripts/build-mac.sh
```

The script:

- builds for both `aarch64-apple-darwin` and `x86_64-apple-darwin`,
- `lipo`s them into a universal binary,
- `codesign`s with hardened runtime + entitlements
  (`scripts/entitlements.plist`),
- submits to Apple's notarization service via `xcrun notarytool`,
- staples the notarization ticket.

Prerequisites and one-time setup steps (Developer ID cert, notarytool
keychain profile) are documented in `scripts/build-mac.sh`'s header.

Output: `target/universal/release/kmwarp-server` (signed + notarized).

## Config (`~/.config/kmwarp/config.toml`)

Optional. Missing file → built-in defaults (`Cmd→Ctrl`, `Option→Alt`).

```toml
[modifiers]
cmd = "ctrl"      # Cmd → Ctrl (default); also accepts "alt", "meta", "win", "shift", "identity"
option = "alt"    # Option → Alt (default identity)

[peer]
bind    = "0.0.0.0:51423"
connect = "10.0.0.5:51423"
name    = "merajs-mbp"
```

Logs the loaded path at startup; if you don't see your custom
config taking effect, check the path in the `loaded [modifiers]
from config path=...` log line.

## Develop

Standard workspace commands:

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

CI runs the same matrix on `macos-latest` + `windows-latest`.

## Status (v1.0)

| Milestone | Status |
|-----------|--------|
| M1 TCP heartbeat              | done |
| M2 macOS mouse capture        | done |
| M3 Windows mouse injection    | done |
| M4 End-to-end mouse           | done |
| M5 Keyboard end-to-end        | done |
| M6 Edge state machine         | done |
| M7 Modifier remap + stuck-key | done |
| M8 Clipboard sync             | done |
| M9 TLS + SPAKE2 pairing       | done |
| M10 Background service        | done (menu bar deferred — see IDEAS.md) |
| M11 Tauri config UI           | v1.1 |
