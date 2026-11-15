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
const BT709_TO_BT2020: [[f64; 3]; 3] = [
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
    // GLSL matrix constructors list columns, not rows.
    let values: Vec<_> = (0..3)
        .flat_map(|column| (0..3).map(move |row| format!("{:.8}", BT709_TO_BT2020[row][column])))
        .collect();
    definitions.push_str(&format!(
        "const mat3 to2020 = mat3({});\n",
        values.join(", ")
    ));
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
