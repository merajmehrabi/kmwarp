//! macOS platform layer.
//!
//! Modules are added milestone-by-milestone:
//! - [`tap_error`] (M2): the `TapError` enum used by the install path.
//! - [`permissions`] (M2): TCC pre-flight checks + Settings deep-links.
//! - [`tap`] (M2/M5/M6): `CGEventTap`-backed `InputSource`; M5 added
//!   keyboard translation, M6 added CursorAt emission + swallow flag.
//! - [`inject`] (M6): server-side `InputSink` — cursor warp + hide/show.
//! - [`clipboard`] (M8): `NSPasteboard`-backed `Clipboard` impl,
//!   4 Hz changeCount poll on a dedicated thread.
//! - [`m2_demo`] (M2): mouse-capture acceptance harness behind
//!   `KMWARP_M2_DEMO=1`.
//! - [`m5_demo`] (M5): keyboard-capture acceptance harness behind
//!   `KMWARP_M5_DEMO=1`.

pub mod clipboard;
pub mod inject;
pub mod m2_demo;
pub mod m5_demo;
pub mod permissions;
pub mod tap;
pub mod tap_error;

pub use permissions::{
    check_permissions, open_accessibility_pane, open_input_monitoring_pane, PermStatus,
};
pub use tap::MacInputSource;
pub use tap_error::TapError;
