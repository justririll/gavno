//! Connector color properties. Initialization combines them with mode/planes;
//! runtime toggles commit separately because Smithay 0.7 has no property hook.
use super::edid::{self, Capabilities};
use smithay::{
    backend::drm::DrmDeviceFd,
    reexports::drm::control::{
        atomic::AtomicModeReq,
        connector,
        property::{self, Value, ValueType},
        AtomicCommitFlags, Device,
    },
};
use std::io;

pub struct ConnectorColor {
    fd: DrmDeviceFd,
    connector: connector::Handle,
    colorspace: property::Handle,
    metadata: property::Handle,
    bpc: property::Handle,
    sdr_colorspace: u64,
    sdr_bpc: u64,
    bt2020: u64,
    pub caps: Capabilities,
    applied: AppliedColor,
}

impl ConnectorColor {
    pub fn probe(fd: DrmDeviceFd, connector: connector::Handle) -> Result<Self, String> {
        let props = fd.get_properties(connector).map_err(|e| e.to_string())?;
        let mut color = None;
        let mut metadata = None;
        let mut bpc = None;
        let mut caps = Capabilities::default();
        // DRM_MODE_PROP_ATOMIC hides atomic-only properties from legacy clients;
        // its absence does not prevent a normal property from being set atomically.
        for (handle, value) in props {
            let info = fd.get_property(handle).map_err(|e| e.to_string())?;
            match info.name().to_bytes() {
                b"EDID" => {
                    if let Ok(bytes) = fd.get_property_blob(value) {
                        caps = edid::parse(&bytes);
                    }
                }
                b"Colorspace" if info.mutable() => {
                    if let ValueType::Enum(values) = info.value_type() {
                        let find = |name: &[u8]| {
                            values
                                .values()
                                .1
                                .iter()
                                .find(|v| v.name().to_bytes() == name)
                                .map(|v| v.value())
                        };
                        if let (Some(bt2020), Some(default)) =
                            (find(b"BT2020_RGB"), find(b"Default"))
                        {
                            color = Some((handle, bt2020, default));
                        }
                    }
                }
                b"HDR_OUTPUT_METADATA"
                    if info.mutable() && matches!(info.value_type(), ValueType::Blob) =>
                {
                    metadata = Some(handle)
                }
                b"max bpc" if info.mutable() => {
                    if let ValueType::UnsignedRange(min, max) = info.value_type() {
                        if min <= 10 && max >= 10 {
                            bpc = Some((handle, value));
                        }
                    }
                }
                _ => {}
            }
        }
        if !caps.supported() {
            return Err("EDID does not advertise PQ, BT2020_RGB and static metadata type 1".into());
        }
        let (colorspace, bt2020, sdr_colorspace) =
            color.ok_or("missing mutable Colorspace/BT2020_RGB")?;
        let metadata = metadata.ok_or("missing mutable HDR_OUTPUT_METADATA")?;
        let (bpc, sdr_bpc) = bpc.ok_or("max bpc cannot select 10 bits")?;
        Ok(Self {
            fd,
            connector,
            colorspace,
            metadata,
            bpc,
            sdr_colorspace,
            sdr_bpc,
            bt2020,
            caps,
            applied: AppliedColor::default(),
        })
    }

    /// Another DRM owner may have changed these properties while our VT was inactive.
    pub fn invalidate(&mut self) {
        self.applied = AppliedColor::default();
    }

    pub fn is_current(&self, hdr: bool) -> bool {
        self.applied.0 == Some(hdr)
    }

    /// Include color properties in the initial mode/plane transaction. Keep
    /// metadata alive through both the atomic test and the real commit.
    pub fn commit_initial_hdr(&mut self, mut req: AtomicModeReq, name: &str) -> io::Result<()> {
        let blob = self.fd.create_property_blob(&self.caps.metadata())?;
        req.add_property(self.connector, self.colorspace, Value::Unknown(self.bt2020));
        req.add_property(self.connector, self.bpc, Value::UnsignedRange(10));
        req.add_property(self.connector, self.metadata, blob);
        crate::perf::startup_output("udev.output.mode_test.begin", name);
        let result = self.fd.atomic_commit(AtomicCommitFlags::ALLOW_MODESET | AtomicCommitFlags::TEST_ONLY, req.clone());
        crate::perf::startup_output("udev.output.mode_test.end", name);
        let result = result.and_then(|()| {
            crate::perf::startup_output("udev.output.mode_commit.begin", name);
            let result = self.fd.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req);
            crate::perf::startup_output("udev.output.mode_commit.end", name);
            result
        });
        let _ = self.fd.destroy_property_blob(blob.into());
        if result.is_ok() { self.applied.0 = Some(true); }
        result
    }

    pub fn enable(&mut self) -> io::Result<()> {
        let blob = self.fd.create_property_blob(&self.caps.metadata())?;
        let mut req = AtomicModeReq::new();
        req.add_property(self.connector, self.colorspace, Value::Unknown(self.bt2020));
        req.add_property(self.connector, self.bpc, Value::UnsignedRange(10));
        req.add_property(self.connector, self.metadata, blob);
        let result = self.applied.reconcile(true, || {
            self.fd.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)
        });
        // The committed property holds a kernel reference to the blob.
        let _ = self.fd.destroy_property_blob(u64::from(blob));
        result
    }

    pub fn restore(&mut self) -> io::Result<()> {
        let mut req = AtomicModeReq::new();
        req.add_property(
            self.connector,
            self.colorspace,
            Value::Unknown(self.sdr_colorspace),
        );
        req.add_property(self.connector, self.bpc, Value::UnsignedRange(self.sdr_bpc));
        req.add_property(self.connector, self.metadata, Value::Blob(0));
        self.applied.reconcile(false, || {
            self.fd.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)
        })
    }
}

impl Drop for ConnectorColor {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            tracing::warn!(%error, "could not restore SDR connector properties");
        }
    }
}

/// Only commits made during this acquisition establish known connector state.
#[derive(Default)]
struct AppliedColor(Option<bool>);
impl AppliedColor {
    fn reconcile(&mut self, hdr: bool, commit: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
        if self.0 != Some(hdr) {
            commit()?;
            self.0 = Some(hdr);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdr_is_committed_on_acquisition_and_reactivation() {
        // Model a previous DRM owner leaving PQ selected at startup.
        let mut connector_hdr = true;
        let mut applied = AppliedColor::default();
        applied.reconcile(false, || { connector_hdr = false; Ok(()) }).unwrap();
        assert!(!connector_hdr);
        applied.reconcile(false, || panic!("known SDR needs no new commit")).unwrap();
        // Another owner selects PQ while our VT is inactive.
        connector_hdr = true;
        applied = AppliedColor::default();
        applied.reconcile(false, || { connector_hdr = false; Ok(()) }).unwrap();
        assert!(!connector_hdr);
        assert_eq!(applied.0, Some(false));
    }

    #[test]
    fn failed_reconciliation_remains_unknown_and_hdr_is_reestablished() {
        let mut applied = AppliedColor::default();
        assert!(applied.reconcile(false, || Err(io::Error::other("KMS rejected"))).is_err());
        assert_eq!(applied.0, None);
        applied.reconcile(false, || Ok(())).unwrap();
        applied.reconcile(true, || Ok(())).unwrap();
        applied = AppliedColor::default();
        let mut committed = false;
        applied.reconcile(true, || { committed = true; Ok(()) }).unwrap();
        assert!(committed);
        assert_eq!(applied.0, Some(true));
    }
}
