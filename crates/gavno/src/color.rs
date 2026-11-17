//! Color constants shared by the output shader and its CPU reference tests.
const SRGB_BREAK: f64 = 0.04045;
const SRGB_SLOPE: f64 = 12.92;
const SRGB_OFFSET: f64 = 0.055;
const SRGB_SCALE: f64 = 1.055;
const SRGB_EXPONENT: f64 = 2.4;
const PQ_M1: f64 = 2610.0 / 16384.0;
const PQ_M2: f64 = 2523.0 / 32.0;
const PQ_C1: f64 = 3424.0 / 4096.0;
const PQ_C2: f64 = 2413.0 / 128.0;
const PQ_C3: f64 = 2392.0 / 128.0;
// Rows map linear BT.709 RGB to linear BT.2020 RGB (both D65).
pub const BT709_TO_BT2020: [[f64; 3]; 3] = [
    [0.62740390, 0.32928304, 0.04331307],
    [0.06909729, 0.91954040, 0.01136232],
    [0.01639144, 0.08801331, 0.89559525],
];

pub fn output_shader() -> String {
    let mut definitions = String::new();
    for (name, value) in [
        ("srgb_break", SRGB_BREAK),
        ("srgb_slope", SRGB_SLOPE),
        ("srgb_offset", SRGB_OFFSET),
        ("srgb_scale", SRGB_SCALE),
        ("srgb_exponent", SRGB_EXPONENT),
        ("pq_m1", PQ_M1),
        ("pq_m2", PQ_M2),
        ("pq_c1", PQ_C1),
        ("pq_c2", PQ_C2),
        ("pq_c3", PQ_C3),
    ] {
        definitions.push_str(&format!("const float {name} = {value:.17};\n"));
    }
    include_str!("hdr/pq.frag").replace("// COLOR_CONSTANTS", &definitions)
}

#[cfg(test)]
fn srgb_to_linear(v: f64) -> f64 {
    if v <= SRGB_BREAK {
        v / SRGB_SLOPE
    } else {
        ((v + SRGB_OFFSET) / SRGB_SCALE).powf(SRGB_EXPONENT)
    }
}
#[cfg(test)]
fn pq_encode(nits: f64) -> f64 {
    let p = (nits.clamp(0.0, 10000.0) / 10000.0).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * p) / (1.0 + PQ_C3 * p)).powf(PQ_M2)
}
#[cfg(test)]
fn to_bt2020(c: [f64; 3]) -> [f64; 3] {
    BT709_TO_BT2020.map(|row| row.iter().zip(c).map(|(m, v)| m * v).sum())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reference_transfer_values() {
        for (v, linear) in [
            (0.0, 0.0),
            (0.04045, 0.00313080495),
            (0.5, 0.21404114048),
            (1.0, 1.0),
        ] {
            assert!((srgb_to_linear(v) - linear).abs() < 1e-9);
        }
        for (nits, pq) in [
            (0.0, 0.0),
            (100.0, 0.5080784215),
            (203.0, 0.5806888810),
            (1000.0, 0.7518270962),
            (10000.0, 1.0),
        ] {
            assert!((pq_encode(nits) - pq).abs() < 1e-6);
        }
    }
    #[test]
    fn bt2020_reference_primaries_and_neutral() {
        for v in to_bt2020([1.0; 3]) {
            assert!((v - 1.0).abs() < 2e-8);
        }
        assert_eq!(
            to_bt2020([1.0, 0.0, 0.0]),
            [0.62740390, 0.06909729, 0.01639144]
        );
        assert_eq!(
            to_bt2020([0.0, 1.0, 0.0]),
            [0.32928304, 0.91954040, 0.08801331]
        );
        assert_eq!(
            to_bt2020([0.0, 0.0, 1.0]),
            [0.04331307, 0.01136232, 0.89559525]
        );
    }
}

/// Named transfer functions advertised by color-management version 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Transfer { Srgb = 9, Linear = 5, Pq = 11, Gamma22 = 2 }

impl Transfer {
    pub fn from_named(value: u32) -> Option<Self> {
        match value { 9 => Some(Self::Srgb), 5 => Some(Self::Linear), 11 => Some(Self::Pq), 2 => Some(Self::Gamma22), _ => None }
    }
    /// Absolute TFs retain their physical luminance; relative TFs anchor white.
    pub fn scale(self, white: f32, luminances: [f64; 3]) -> [f32; 2] {
        match self {
            Self::Pq => [10000.0, 0.0],
            Self::Linear => [80.0, 0.0],
            _ => [((luminances[1] - luminances[0]) / luminances[2]) as f32 * white,
                  (luminances[0] / luminances[2]) as f32 * white],
        }
    }
    #[cfg(test)]
    pub fn decode(self, v: f64) -> f64 {
        match self {
            Self::Linear => v,
            Self::Gamma22 => v.signum() * v.abs().powf(2.2),
            Self::Srgb => if v.abs() <= SRGB_BREAK { v / SRGB_SLOPE } else { v.signum() * ((v.abs() + SRGB_OFFSET) / SRGB_SCALE).powf(SRGB_EXPONENT) },
            Self::Pq => { let p = v.max(0.0).powf(1.0 / PQ_M2); ((p - PQ_C1).max(0.0) / (PQ_C2 - PQ_C3 * p)).powf(1.0 / PQ_M1) }
        }
    }
    #[cfg(test)]
    pub fn encode(self, v: f64) -> f64 {
        match self {
            Self::Linear => v,
            Self::Gamma22 => v.signum() * v.abs().powf(1.0 / 2.2),
            Self::Srgb => if v.abs() <= 0.0031308 { v * SRGB_SLOPE } else { v.signum() * (SRGB_SCALE * v.abs().powf(1.0 / SRGB_EXPONENT) - SRGB_OFFSET) },
            Self::Pq => { let p = v.max(0.0).powf(PQ_M1); ((PQ_C1 + PQ_C2 * p) / (1.0 + PQ_C3 * p)).powf(PQ_M2) }
        }
    }
}

pub type Matrix = [[f64; 3]; 3];
pub const SRGB: [i32; 8] = [640000,330000,300000,600000,150000,60000,312700,329000];
pub const BT2020: [i32; 8] = [708000,292000,170000,797000,131000,46000,312700,329000];
pub const DISPLAY_P3: [i32; 8] = [680000,320000,265000,690000,150000,60000,312700,329000];

fn inverse(m: Matrix) -> Option<Matrix> {
    let mut c = [[0.0; 3]; 3];
    for (i, row) in c.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = m[(j+1)%3][(i+1)%3]*m[(j+2)%3][(i+2)%3] - m[(j+1)%3][(i+2)%3]*m[(j+2)%3][(i+1)%3];
        }
    }
    let d: f64 = (0..3).map(|i| m[0][i]*c[i][0]).sum();
    if !d.is_finite() || d.abs() < 1e-12 { return None; }
    for row in &mut c { for v in row { *v /= d; } }
    Some(c)
}
pub fn multiply(a: Matrix, b: Matrix) -> Matrix {
    std::array::from_fn(|i| std::array::from_fn(|j| (0..3).map(|k| a[i][k]*b[k][j]).sum()))
}
fn apply(m: Matrix, v: [f64; 3]) -> [f64; 3] {
    m.map(|r| (0..3).map(|i| r[i]*v[i]).sum())
}
fn rgb_xyz(p: [i32; 8]) -> Option<Matrix> {
    // Homogeneous XYZ avoids division by y, admitting imaginary primaries.
    let m = std::array::from_fn(|i| std::array::from_fn(|j| {
        let x = f64::from(p[j*2])/1e6; let y = f64::from(p[j*2+1])/1e6;
        [x,y,1.0-x-y][i]
    }));
    let wy = f64::from(p[7])/1e6;
    if wy <= 0.0 { return None; }
    let wx = f64::from(p[6])/1e6;
    let scale = apply(inverse(m)?, [wx/wy,1.0,(1.0-wx-wy)/wy]);
    let result = std::array::from_fn(|i| std::array::from_fn(|j| m[i][j]*scale[j]));
    inverse(result)?;
    Some(result)
}
/// Bradford adaptation makes arbitrary, non-D65 client white points neutral.
pub fn conversion(from: [i32; 8], to: [i32; 8]) -> Option<Matrix> {
    let a = rgb_xyz(from)?; let b = rgb_xyz(to)?;
    let bradford = [[0.8951,0.2664,-0.1614],[-0.7502,1.7135,0.0367],[0.0389,-0.0685,1.0296]];
    let src = apply(bradford, apply(a, [1.0;3]));
    let dst = apply(bradford, apply(b, [1.0;3]));
    if src.iter().any(|v| v.abs() < 1e-12) { return None; }
    let diagonal = std::array::from_fn(|i| std::array::from_fn(|j| if i == j { dst[i]/src[i] } else { 0.0 }));
    let result = multiply(inverse(b)?, multiply(inverse(bradford)?, multiply(diagonal, multiply(bradford, a))));
    result.iter().flatten().all(|v| v.is_finite() && v.abs() < 1e5).then_some(result)
}

#[cfg(test)]
mod management_tests {
    use super::*;
    #[test]
    fn transfer_round_trips() {
        for tf in [Transfer::Srgb, Transfer::Linear, Transfer::Pq, Transfer::Gamma22] {
            for v in [0.0001,0.003,0.18,0.5,1.0] { assert!((tf.decode(tf.encode(v))-v).abs() < 1e-8, "{tf:?}: {v}"); }
        }
        for tf in [Transfer::Srgb, Transfer::Linear, Transfer::Gamma22] {
            for v in [-2.0,-0.1,2.0,12.0] { assert!((tf.decode(tf.encode(v))-v).abs() < 1e-8); }
        }
    }
    #[test]
    fn primary_matrices() {
        for p in [SRGB, BT2020, DISPLAY_P3] {
            for v in apply(conversion(p, BT2020).unwrap(), [1.0;3]) { assert!((v-1.0).abs() < 1e-6); }
        }
        let actual = conversion(SRGB, BT2020).unwrap();
        for i in 0..3 { for j in 0..3 { assert!((actual[i][j]-BT709_TO_BT2020[i][j]).abs() < 1e-4); } }
        assert!(conversion([0;8], BT2020).is_none());
    }
    #[test]
    fn luminance_scales() {
        for tf in [Transfer::Srgb,Transfer::Gamma22] { let s=tf.scale(203.0,[0.0,80.0,80.0]); assert_eq!(s,[203.0,0.0]); }
        assert_eq!(Transfer::Linear.scale(250.0,[0.0,80.0,80.0]),[80.0,0.0]);
        assert_eq!(Transfer::Pq.scale(250.0,[0.005,10000.0,203.0]),[10000.0,0.0]);
        let s=Transfer::Gamma22.scale(200.0,[0.0,400.0,200.0]); assert_eq!(s,[400.0,0.0]);
    }
}
