//! macOS platform layer.
//!
//! Modules are added milestone-by-milestone:
//! - [`tap_error`] (M2): the `TapError` enum used by the install path.
//! - `permissions` (M2): TCC pre-flight checks + Settings deep-links.
//! - `tap` (M2): `CGEventTap`-backed `InputSource`.
//! - `m2_demo` (M2): acceptance harness wired behind `KMWARP_M2_DEMO=1`.

pub mod tap_error;

pub use tap_error::TapError;
