//! macOS LaunchAgent install/uninstall.
//!
//! v1.0 installs a `~/Library/LaunchAgents/com.kmwarp.server.plist`
//! that launchd loads at user login. The plist's `KeepAlive`
//! dictionary covers two of the spec §M10 requirements:
//!
//!   - `SuccessfulExit = false` — launchd restarts the agent if it
//!     exits non-zero (crash recovery).
//!   - `NetworkState = true` — launchd restarts the agent when the
//!     network comes back after going away. Combined with the M1
//!     in-process exponential-backoff reconnect (which handles the
//!     mid-session Wi-Fi blip), this satisfies "reconnects within 5 s
//!     of Wi-Fi return" even across hard network down events that
//!     outlive a single backoff cycle.
//!
//! `RunAtLoad = true` means the agent starts at user login without
//! requiring an explicit `launchctl start`.
//!
//! Logs go to `/tmp/kmwarp-server.log` / `.err`. Real packaging
//! (`cargo-bundle`, signed/notarized .pkg) routes them under
//! `~/Library/Logs/kmwarp/` instead; v1.0 keeps it simple.

use std::path::PathBuf;
use std::process::Command;
use std::{env, fs};

use tracing::{debug, info, warn};

use super::ServiceError;

/// Reverse-DNS-style label launchd uses to identify the agent.
pub const AGENT_LABEL: &str = "com.kmwarp.server";

/// Path to the plist file inside the user's `LaunchAgents` folder.
pub fn launch_agent_path() -> Result<PathBuf, ServiceError> {
    let home = directories::BaseDirs::new()
        .ok_or(ServiceError::NoHomeDir)?
        .home_dir()
        .to_owned();
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("{AGENT_LABEL}.plist")))
}

/// Write the plist, ensure the directory exists, and `launchctl load
/// -w` it. Idempotent: re-installing replaces the plist and reloads.
pub fn install_launch_agent() -> Result<(), ServiceError> {
    let plist_path = launch_agent_path()?;
    let exe_path = env::current_exe().map_err(ServiceError::NoCurrentExe)?;
    // Canonicalize so symlinked target/ paths resolve to the real
    // binary; launchd doesn't follow symlinks at launch time.
    let exe_path = exe_path.canonicalize().unwrap_or(exe_path);
    let plist = generate_plist(&exe_path);

    if let Some(parent) = plist_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&plist_path, plist)?;
    info!(path = %plist_path.display(), exe = %exe_path.display(), "wrote LaunchAgent plist");

    // Unload first if it's already loaded — launchctl errors if you
    // `load` a label that's already registered, and we want
    // idempotency. Ignore the unload exit status; "not loaded" is the
    // normal first-install case.
    let _ = run_launchctl(&["unload", "-w", &plist_path.to_string_lossy()]);
    let status = run_launchctl(&["load", "-w", &plist_path.to_string_lossy()])?;
    if !status.success() {
        return Err(ServiceError::LaunchctlFailed(format!(
            "load -w exited {:?}",
            status.code()
        )));
    }
    info!(
        label = AGENT_LABEL,
        "launchd agent loaded; agent will start on next login"
    );
    Ok(())
}

/// `launchctl unload -w` and remove the plist. Idempotent: missing
/// plist / not-loaded label both succeed silently.
pub fn uninstall_launch_agent() -> Result<(), ServiceError> {
    let plist_path = launch_agent_path()?;

    // unload is best-effort: if the agent was already unloaded (or
    // never loaded), we still want to remove the plist file.
    match run_launchctl(&["unload", "-w", &plist_path.to_string_lossy()]) {
        Ok(status) if status.success() => {
            debug!(label = AGENT_LABEL, "launchctl unloaded agent");
        }
        Ok(status) => {
            debug!(
                label = AGENT_LABEL,
                code = ?status.code(),
                "launchctl unload returned non-zero (agent may not have been loaded)"
            );
        }
        Err(e) => warn!(error = %e, "launchctl unload failed (continuing to remove plist)"),
    }

    match fs::remove_file(&plist_path) {
        Ok(()) => info!(path = %plist_path.display(), "removed LaunchAgent plist"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = %plist_path.display(), "plist already absent");
        }
        Err(e) => return Err(ServiceError::Io(e)),
    }
    Ok(())
}

fn run_launchctl(args: &[&str]) -> Result<std::process::ExitStatus, ServiceError> {
    debug!(?args, "launchctl");
    Command::new("launchctl")
        .args(args)
        .status()
        .map_err(|e| ServiceError::LaunchctlFailed(format!("could not spawn launchctl: {e}")))
}

/// Render the plist for the given absolute binary path. Pulled out so
/// it's unit-testable without touching launchctl.
fn generate_plist(exe_path: &std::path::Path) -> String {
    // The plist uses the property-list XML 1.0 DTD. The exe path is
    // emitted verbatim — there's no XML-special characters we expect
    // (the user's home directory might contain `&` or `<` in
    // theory, but in practice POSIX rejects those at account-creation
    // time on macOS).
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>run</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
        <key>NetworkState</key>
        <true/>
    </dict>
    <key>StandardOutPath</key>
    <string>/tmp/kmwarp-server.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/kmwarp-server.err</string>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#,
        label = AGENT_LABEL,
        exe = exe_path.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn plist_contains_label_exe_and_keepalive_keys() {
        let exe = PathBuf::from("/Users/test/bin/kmwarp-server");
        let plist = generate_plist(&exe);

        // Core sanity: agent label, the binary path, RunAtLoad, and
        // both KeepAlive sub-keys all need to appear.
        assert!(plist.contains("com.kmwarp.server"));
        assert!(plist.contains("/Users/test/bin/kmwarp-server"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<true/>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("<key>SuccessfulExit</key>"));
        assert!(plist.contains("<key>NetworkState</key>"));
        // Log paths are stable for v1.0.
        assert!(plist.contains("/tmp/kmwarp-server.log"));
        assert!(plist.contains("/tmp/kmwarp-server.err"));
    }

    #[test]
    fn plist_starts_with_xml_declaration_and_doctype() {
        let plist = generate_plist(&PathBuf::from("/whatever"));
        assert!(plist.starts_with(r#"<?xml version="1.0" encoding="UTF-8"?>"#));
        assert!(plist.contains("DOCTYPE plist PUBLIC"));
    }

    #[test]
    fn plist_program_arguments_includes_run_subcommand() {
        // The plist must invoke `kmwarp-server run` (not just the
        // bare binary), so the CLI's default subcommand routing
        // doesn't accidentally re-trigger install on every launch.
        let plist = generate_plist(&PathBuf::from("/bin/kmwarp-server"));
        // The run subcommand must appear as its own <string> entry
        // inside ProgramArguments.
        let pa_idx = plist
            .find("<key>ProgramArguments</key>")
            .expect("plist has ProgramArguments");
        let after = &plist[pa_idx..];
        assert!(
            after.contains("<string>/bin/kmwarp-server</string>")
                && after.contains("<string>run</string>"),
            "plist ProgramArguments must list the exe path + the `run` subcommand"
        );
    }

    #[test]
    fn launch_agent_path_resolves_inside_library_launchagents() {
        // On any platform where BaseDirs::new() succeeds (which
        // includes CI runners), the path must end with
        // Library/LaunchAgents/com.kmwarp.server.plist.
        let Ok(p) = launch_agent_path() else {
            // Sandboxed CI without $HOME — skip the assertion.
            return;
        };
        let s = p.display().to_string();
        assert!(
            s.ends_with("Library/LaunchAgents/com.kmwarp.server.plist"),
            "unexpected launch agent path: {s}"
        );
    }
}
