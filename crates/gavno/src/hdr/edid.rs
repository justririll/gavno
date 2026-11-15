//! Bounded CTA-861 parser and the Linux HDR_OUTPUT_METADATA ABI.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Capabilities {
    pub pq: bool,
    pub bt2020_rgb: bool,
    pub static_type1: bool,
    pub max_nits: Option<f64>,
    pub max_fall: Option<f64>,
    pub min_nits: Option<f64>,
}

impl Capabilities {
    pub fn supported(self) -> bool {
        self.pq && self.bt2020_rgb && self.static_type1
    }

    /// Missing CTA luminance fields mean unspecified, not zero. Use SDR white
    /// as a conservative metadata fallback; never invent a panel peak.
    pub fn metadata(self) -> [u8; 32] {
        let mut bytes = [0; 32];
        // u32 metadata_type = 0, HDMI type 1; union starts at offset 4.
        bytes[4] = 2; // SMPTE ST 2084
        bytes[5] = 0; // Static_Metadata_Descriptor_ID = 0 (type 1)
        let max = self.max_nits.unwrap_or(203.0);
        let words = [
            35400,
            14600,
            8500,
            39850,
            6550,
            2300, // BT.2020 R, G, B (1/50000)
            15635,
            16450, // D65
            max.round().clamp(1.0, 65535.0) as u16,
            (self.min_nits.unwrap_or(0.0) * 10000.0)
                .round()
                .clamp(0.0, 65535.0) as u16,
            max.round().clamp(1.0, 65535.0) as u16,
            self.max_fall.unwrap_or(max).round().clamp(1.0, 65535.0) as u16,
        ];
        for (i, value) in words.into_iter().enumerate() {
            bytes[6 + i * 2..8 + i * 2].copy_from_slice(&value.to_ne_bytes());
        }
        bytes
    }
}

pub fn parse(edid: &[u8]) -> Capabilities {
    let mut caps = Capabilities::default();
    if edid.len() < 128
        || edid[..8] != [0, 255, 255, 255, 255, 255, 255, 0]
        || edid[..128].iter().fold(0u8, |a, b| a.wrapping_add(*b)) != 0
    {
        return caps;
    }
    for block in edid[128..].chunks_exact(128).take(edid[126] as usize) {
        if block[0] != 0x02
            || block[1] < 3
            || block.iter().fold(0u8, |a, b| a.wrapping_add(*b)) != 0
        {
            continue;
        }
        let end = usize::from(block[2]);
        if !(4..=127).contains(&end) {
            continue;
        }
        parse_cta_blocks(&block[4..end], &mut caps);
    }
    caps
}

fn parse_cta_blocks(mut blocks: &[u8], caps: &mut Capabilities) {
    while let Some((&header, rest)) = blocks.split_first() {
        let len = usize::from(header & 31);
        if rest.len() < len {
            break;
        }
        let data = &rest[..len];
        blocks = &rest[len..];
        if header >> 5 != 7 {
            continue;
        }
        match data {
            [0x05, color, ..] => caps.bt2020_rgb |= color & 0x80 != 0,
            [0x06, eotf, descriptors, luminance @ ..] => {
                caps.pq |= eotf & 0x04 != 0;
                caps.static_type1 |= descriptors & 1 != 0;
                let decode =
                    |code: u8| (code != 0).then(|| 50.0 * 2.0_f64.powf(f64::from(code) / 32.0));
                if let Some(max) = luminance.first().and_then(|v| decode(*v)) {
                    caps.max_nits = Some(max);
                    caps.max_fall = luminance.get(1).and_then(|v| decode(*v));
                    caps.min_nits = luminance
                        .get(2)
                        .map(|v| max * (f64::from(*v) / 255.0).powi(2) / 100.0);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn edid(blocks: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 256];
        b[..8].copy_from_slice(&[0, 255, 255, 255, 255, 255, 255, 0]);
        b[126] = 1;
        b[128] = 2;
        b[129] = 3;
        b[130] = (4 + blocks.len()) as u8;
        b[132..132 + blocks.len()].copy_from_slice(blocks);
        for start in [0, 128] {
            b[start + 127] = 0u8.wrapping_sub(
                b[start..start + 127]
                    .iter()
                    .fold(0u8, |a, b| a.wrapping_add(*b)),
            );
        }
        b
    }
    const PANEL: &[u8] = &[
        0xe3, 0x05, 0x80, 0x00, 0xe6, 0x06, 0x05, 0x01, 0x74, 0x60, 0x07,
    ];
    #[test]
    fn target_panel() {
        let c = parse(&edid(PANEL));
        assert!(c.supported());
        assert!((c.max_nits.unwrap() - 616.8843).abs() < 0.001);
        assert_eq!(c.max_fall, Some(400.0));
        assert!((c.min_nits.unwrap() - 0.0046484).abs() < 0.000001);
    }
    #[test]
    fn independent_advertisements_and_bounds() {
        assert!(!parse(&edid(&PANEL[..4])).pq);
        assert!(!parse(&edid(&PANEL[4..])).bt2020_rgb);
        assert!(!parse(&edid(&[0xe3, 6, 1, 1])).supported());
        for len in 0..256 {
            let _ = parse(&edid(PANEL)[..len]);
        }
        let mut b = edid(PANEL);
        b[140] ^= 1;
        assert!(!parse(&b).supported());
        assert!(!parse(&edid(&[0xff, 6, 4, 1])).supported());
        assert!(parse(&edid(&[0xe3, 5, 128, 0, 0xe3, 6, 4, 1])).supported());
    }
    #[test]
    fn kernel_blob_layout() {
        let b = parse(&edid(PANEL)).metadata();
        assert_eq!(&b[..6], &[0, 0, 0, 0, 2, 0]);
        let word = |offset| u16::from_ne_bytes([b[offset], b[offset + 1]]);
        assert_eq!((word(6), word(8)), (35400, 14600));
        assert_eq!((word(10), word(12)), (8500, 39850));
        assert_eq!((word(14), word(16)), (6550, 2300));
        assert_eq!((word(18), word(20)), (15635, 16450));
        assert_eq!(
            (word(22), word(24), word(26), word(28)),
            (617, 46, 617, 400)
        );
        assert_eq!(&b[30..], &[0, 0]);
    }
}
