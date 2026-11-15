//! SDR composition in a BT.2020/PQ output container. Client color management
//! deliberately remains outside this module.
pub mod compose;
pub mod edid;
pub mod kms;
pub mod mode;

/// Runtime state reported by the window-management IPC's screen method.
#[derive(serde::Serialize)]
pub struct Status {
    pub enabled: bool,
    pub pending: Option<bool>,
    pub supported: bool,
    pub sdr_white_nits: f32,
}
