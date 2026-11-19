//! Finger pointer helper: webcam hand tracking that drives the cursor.
//!
//! The library holds the parts the compositor shares with the helper process
//! (wire protocol) plus the pure gesture logic. The camera and the neural
//! networks live behind the `runtime` feature, used by the `gavno-cam` binary.

pub mod filter;
pub mod gesture;
pub use gavno_cam_proto as protocol;
pub mod vision;

#[cfg(feature = "runtime")]
pub mod camera;
#[cfg(feature = "runtime")]
pub mod models;
