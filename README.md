# kmwarp

Cross-platform keyboard / mouse sharing. A Mac drives a Windows PC over
the LAN, mimicking macOS Universal Control.

v1.0 scope: unidirectional (Mac → Windows), single-monitor edge, two
peers on the same LAN, encrypted transport (TLS + SPAKE2 pairing).

See `kmwarp-SPEC.md` for the full spec, `PLAN.md` for the milestone
breakdown, and `IDEAS.md` for v1.1+ follow-ups.

## Install (macOS server)

Download the latest macOS server tarball from the
[Releases page](https://github.com/merajmehrabi/kmwarp/releases/latest)
and unpack it somewhere on your `$PATH` (or anywhere — the
`install` subcommand below records the binary's absolute path):

```sh
tar xzf kmwarp-server-vX.Y.Z-aarch64-apple-darwin.tar.gz
mv kmwarp-server /usr/local/bin/   # or wherever you prefer
```

Then install it as a user LaunchAgent so it starts at login and
survives crashes / Wi-Fi blips:

```sh
kmwarp-server install
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
   pairing flow — the server menu bar item and stdout show a
   6-digit code, you type it into the client. The two sides then
   pin each other's TLS certificates and subsequent connects
   auto-authenticate.

Uninstall is the symmetric `kmwarp-server uninstall` — removes
the plist and `launchctl unload -w`s it. Idempotent.

The CLI also accepts `--help` and `--version`:

```sh
kmwarp-server --help
```

### Build from source (Advanced)

Developers who want to hack on the server can build locally
instead of using a release tarball. Requires Rust 1.82+ (the
toolchain is pinned via `rust-toolchain.toml`):

```sh
cargo build --release -p kmwarp-server
```

The binary lands at `target/release/kmwarp-server`. From there
the `install` / `uninstall` subcommands above work identically.

For builds suitable for distribution to other users (Gatekeeper-
green, no "unidentified developer" warnings), use the build
script `scripts/build-mac.sh` documented under
**Signed / notarized release builds** below.

## Install (Windows client)

Download the latest MSI installer from the
[Releases page](https://github.com/merajmehrabi/kmwarp/releases/latest)
(file: `kmwarp-client-X.Y.Z-x86_64.msi`) and double-click it.

> **SmartScreen warning.** Windows will show
> "Windows protected your PC" — click **More info → Run anyway**.
> We don't have an Authenticode cert yet, so this warning is
> expected. The binary is the same one built from this repo at
> the tagged commit; reproduce it yourself if you want by
> following **Build from source** below.

After install:

1. Launch **kmwarp client** from the Start Menu. A pairing
   dialog opens on first run.
2. Confirm the Mac server is running and showing its 6-digit
   pairing code (menu bar status item, or `tail -f
   /tmp/kmwarp-server.log`). Type that code into the dialog.
3. The MSI also registers `kmwarp-client` as an auto-start
   Windows service so it survives reboot. Verify with
   `Get-Service kmwarp-client` from PowerShell — should report
   `Running`. The pin file at `%APPDATA%\kmwarp\config\peer.pin`
   is shared with the service so subsequent reconnects
   auto-authenticate.
4. The client auto-discovers the server on the LAN via mDNS —
   no address configuration needed in the typical case.

To uninstall, use **Settings → Apps → kmwarp client → Uninstall**
(or `Get-Package kmwarp-client | Uninstall-Package` from an
elevated PowerShell).

### Build from source (Advanced)

If you'd rather build the client yourself (to audit the binary,
develop locally, or run an untagged commit), you'll need Rust
1.82+ on a Windows host:

1. Build the release binary:
   ```powershell
   cargo build --release -p kmwarp-client
   ```
2. Open PowerShell **as Administrator** (the SCM rejects
   unprivileged `create_service` calls).
3. Install + start:
   ```powershell
   .\target\release\kmwarp-client.exe install
   ```
4. Verify with `Get-Service kmwarp-client` — should report
   `Running`. Reboot to confirm AutoStart works.
5. Pair (one-time): launch interactively once
   (`.\target\release\kmwarp-client.exe`) to enter the 6-digit
   SPAKE2 code shown on the Mac.
6. Uninstall:
   ```powershell
   .\target\release\kmwarp-client.exe uninstall
   ```

### Troubleshooting: overriding auto-discovery

If mDNS discovery doesn't work (multicast blocked, multiple servers on
the LAN, or you want to pin a specific address), set `KMWARP_CONNECT`
on the service environment:

```powershell
$key = "HKLM:\SYSTEM\CurrentControlSet\Services\kmwarp-client"
New-ItemProperty -Path $key -Name "Environment" `
  -Value @("KMWARP_CONNECT=192.168.0.34:51423") `
  -PropertyType MultiString -Force
Restart-Service kmwarp-client
```

### Session-0 isolation (the gotcha)

A `LocalSystem` service runs in session 0 and cannot reach the
user-session desktop with `SendInput`. The service works around this
by re-spawning itself as `run-as-helper` into the active console
session via `WTSQueryUserToken` + `CreateProcessAsUserW`. This means:

- **No user logged in → no helper.** If nobody is signed in at the
  console, the helper spawn fails and the service exits cleanly.
  Log in and `Start-Service kmwarp-client`.
- **Active user changes mid-session.** v1 does not handle
  `WM_WTSSESSION_CHANGE`; if the logged-in user changes (logout +
  different user), restart the service.

### Signed Windows builds

Production deployment needs an Authenticode signature or Windows
Defender / corporate AV will flag the service binary. Run the
codesign pipeline:

```powershell
$env:KMWARP_PFX = "C:\path\to\codesign.pfx"
$env:KMWARP_PFX_PASSWORD = "..."
.\scripts\build-windows.ps1
```

The script builds release for `x86_64-pc-windows-msvc`, signs with
SHA256, and RFC-3161-timestamps via the configured service. The
optional cargo-wix MSI step is commented in the script (needs a
one-time `cargo wix init`).

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
