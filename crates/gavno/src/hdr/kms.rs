//! Extra connector properties are committed separately: Smithay 0.7's atomic
//! surface only sets connector CRTC_ID and has no arbitrary-property hook.
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
    enabled: bool,
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
            enabled: false,
        })
    }

    pub fn enable(&mut self) -> io::Result<()> {
        let blob = self.fd.create_property_blob(&self.caps.metadata())?;
        let mut req = AtomicModeReq::new();
        req.add_property(self.connector, self.colorspace, Value::Unknown(self.bt2020));
        req.add_property(self.connector, self.bpc, Value::UnsignedRange(10));
        req.add_property(self.connector, self.metadata, blob);
        let result = self.fd.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req);
        // The committed property holds a kernel reference to the blob.
        let _ = self.fd.destroy_property_blob(u64::from(blob));
        if result.is_ok() {
            self.enabled = true;
        }
        result
    }

    pub fn restore(&mut self) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let mut req = AtomicModeReq::new();
        req.add_property(
            self.connector,
            self.colorspace,
            Value::Unknown(self.sdr_colorspace),
        );
        req.add_property(self.connector, self.bpc, Value::UnsignedRange(self.sdr_bpc));
        req.add_property(self.connector, self.metadata, Value::Blob(0));
        self.fd
            .atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)?;
        self.enabled = false;
        Ok(())
    }
}

impl Drop for ConnectorColor {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            tracing::warn!(%error, "could not restore SDR connector properties");
        }
    }
}
