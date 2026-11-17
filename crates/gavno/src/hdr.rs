//! SDR-compatible composition in a BT.2020/PQ output container. Explicitly
//! tagged client content is converted by `surface` before the output pass.
pub mod compose;
pub mod surface;
pub mod edid;
pub mod kms;
pub mod mode;
pub mod startup;

/// Runtime state reported by the window-management IPC's screen method.
#[derive(serde::Serialize)]
pub struct Status {
    pub enabled: bool,
    pub pending: Option<bool>,
    pub supported: bool,
    pub sdr_white_nits: f32,
}
