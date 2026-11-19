//! The launcher's inline calculator: arithmetic and unit conversion.
//!
//! Pure functions, no I/O. `evaluate` returns `None` for everything that is not
//! clearly a calculation, so ordinary searches never grow a calculator row.

/// A finished calculation.
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    /// The text shown after "=" and copied on Enter.
    pub text: String,
    /// The question, shown under the result.
    pub detail: String,
}

/// Tries arithmetic first, then a unit conversion.
pub fn evaluate(input: &str) -> Option<Answer> {
    let input = input.trim();
    if input.is_empty() || input.len() > 200 {
        return None;
    }
    if let Some(value) = arithmetic(input) {
        return Some(Answer { text: format_number(value), detail: input.to_owned() });
    }
    convert(input)
}

// ---- arithmetic ------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Ident(String),
    Op(char),
    Open,
    Close,
}

fn tokenize(input: &str) -> Option<Vec<Tok>> {
    let chars: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => i += 1,
            '0'..='9' | '.' | ',' => {
                let start = i;
                while i < chars.len() && matches!(chars[i], '0'..='9' | '.' | ',') {
                    i += 1;
                }
                let text: String = chars[start..i].iter().map(|c| if *c == ',' { '.' } else { *c }).collect();
                out.push(Tok::Num(text.parse().ok()?));
            }
            '+' | '-' | '*' | '/' | '^' | '%' => {
                out.push(Tok::Op(c));
                i += 1;
            }
            '\u{d7}' => {
                out.push(Tok::Op('*'));
                i += 1;
            }
            '\u{f7}' => {
                out.push(Tok::Op('/'));
                i += 1;
            }
            '\u{2212}' => {
                out.push(Tok::Op('-'));
                i += 1;
            }
            '(' => {
                out.push(Tok::Open);
                i += 1;
            }
            ')' => {
                out.push(Tok::Close);
                i += 1;
            }
            c if c.is_alphabetic() => {
                let start = i;
                while i < chars.len() && chars[i].is_alphabetic() {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect::<String>().to_lowercase();
                out.push(Tok::Ident(word));
            }
            _ => return None,
        }
    }
    Some(out)
}

struct Parser {
    toks: Vec<Tok>,
    at: usize,
    /// Set once the input contains an operator, function or constant.
    active: bool,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at)
    }
    fn eat_op(&mut self, ops: &[char]) -> Option<char> {
        match self.peek() {
            Some(Tok::Op(c)) if ops.contains(c) => {
                let c = *c;
                self.at += 1;
                Some(c)
            }
            _ => None,
        }
    }
    fn starts_operand(&self, at: usize) -> bool {
        matches!(self.toks.get(at), Some(Tok::Num(_) | Tok::Open | Tok::Ident(_)))
    }

    fn expr(&mut self) -> Option<f64> {
        let mut value = self.term()?;
        while let Some(op) = self.eat_op(&['+', '-']) {
            self.active = true;
            let rhs = self.term()?;
            value = if op == '+' { value + rhs } else { value - rhs };
        }
        Some(value)
    }

    fn term(&mut self) -> Option<f64> {
        let mut value = self.unary()?;
        loop {
            match self.peek() {
                Some(Tok::Op('*' | '/')) => {
                    let Some(Tok::Op(op)) = self.peek().cloned() else { return None };
                    self.at += 1;
                    self.active = true;
                    let rhs = self.unary()?;
                    if op == '/' && rhs == 0.0 {
                        return None;
                    }
                    value = if op == '*' { value * rhs } else { value / rhs };
                }
                Some(Tok::Op('%')) => {
                    self.active = true;
                    if self.starts_operand(self.at + 1) {
                        self.at += 1;
                        let rhs = self.unary()?;
                        if rhs == 0.0 {
                            return None;
                        }
                        value %= rhs;
                    } else {
                        // "50%" is a percentage.
                        self.at += 1;
                        value /= 100.0;
                    }
                }
                _ => return Some(value),
            }
        }
    }

    fn unary(&mut self) -> Option<f64> {
        match self.eat_op(&['-', '+']) {
            Some('-') => Some(-self.unary()?),
            Some(_) => self.unary(),
            None => self.power(),
        }
    }

    fn power(&mut self) -> Option<f64> {
        let base = self.primary()?;
        if self.eat_op(&['^']).is_some() {
            self.active = true;
            let exponent = self.unary()?;
            return Some(base.powf(exponent));
        }
        Some(base)
    }

    fn primary(&mut self) -> Option<f64> {
        match self.toks.get(self.at)?.clone() {
            Tok::Num(n) => {
                self.at += 1;
                Some(n)
            }
            Tok::Open => {
                self.at += 1;
                let value = self.expr()?;
                if self.peek() != Some(&Tok::Close) {
                    return None;
                }
                self.at += 1;
                Some(value)
            }
            Tok::Ident(name) => {
                self.at += 1;
                self.active = true;
                match name.as_str() {
                    "pi" => return Some(std::f64::consts::PI),
                    "tau" => return Some(std::f64::consts::TAU),
                    _ => {}
                }
                if self.peek() != Some(&Tok::Open) {
                    return None;
                }
                self.at += 1;
                let arg = self.expr()?;
                if self.peek() != Some(&Tok::Close) {
                    return None;
                }
                self.at += 1;
                match name.as_str() {
                    "sqrt" if arg >= 0.0 => Some(arg.sqrt()),
                    "sin" => Some(arg.sin()),
                    "cos" => Some(arg.cos()),
                    "tan" => Some(arg.tan()),
                    "abs" => Some(arg.abs()),
                    "ln" if arg > 0.0 => Some(arg.ln()),
                    "log" if arg > 0.0 => Some(arg.log10()),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

/// Evaluates `input` when it is an arithmetic expression that uses at least one
/// operator, function or constant (a bare number is not a calculation).
pub fn arithmetic(input: &str) -> Option<f64> {
    let toks = tokenize(input)?;
    if toks.is_empty() {
        return None;
    }
    let mut parser = Parser { toks, at: 0, active: false };
    let value = parser.expr()?;
    if parser.at != parser.toks.len() || !parser.active || !value.is_finite() {
        return None;
    }
    Some(value)
}

/// At most ten significant digits, no trailing zeros.
pub fn format_number(value: f64) -> String {
    if value.abs() < 1e-12 {
        return "0".into();
    }
    let magnitude = value.abs().log10().floor() as i32;
    if !(-6..15).contains(&magnitude) {
        let text = format!("{value:.9e}");
        let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
        let mantissa = if mantissa.contains('.') { mantissa.trim_end_matches('0').trim_end_matches('.') } else { mantissa };
        return format!("{mantissa}e{exponent}");
    }
    let decimals = (9 - magnitude).clamp(0, 15) as usize;
    let text = format!("{value:.decimals$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        text
    }
}

// ---- units -----------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Dim {
    Length,
    Mass,
    Temperature,
    Data,
}

struct Unit {
    /// Lower-case names; a trailing `*` matches any ending.
    names: &'static [&'static str],
    dim: Dim,
    /// Size in the base unit (metre, kilogram, byte); unused for temperature.
    factor: f64,
    label: &'static str,
}

const fn unit(names: &'static [&'static str], dim: Dim, factor: f64, label: &'static str) -> Unit {
    Unit { names, dim, factor, label }
}

/// Order matters: the first unit whose name matches wins, so a longer stem
/// ("миллиметр") comes before a shorter one ("мил").
static UNITS: &[Unit] = &[
    unit(&["mm", "мм", "millimeter*", "millimetre*", "миллиметр*"], Dim::Length, 0.001, "mm"),
    unit(&["cm", "см", "centimeter*", "centimetre*", "сантиметр*"], Dim::Length, 0.01, "cm"),
    unit(&["km", "км", "kilometer*", "kilometre*", "километр*"], Dim::Length, 1000.0, "km"),
    unit(&["m", "м", "meter*", "metre*", "метр*"], Dim::Length, 1.0, "m"),
    unit(&["in", "inch*", "дюйм*"], Dim::Length, 0.0254, "in"),
    unit(&["ft", "foot", "feet", "фут*"], Dim::Length, 0.3048, "ft"),
    unit(&["yd", "yard*", "ярд*"], Dim::Length, 0.9144, "yd"),
    unit(&["mi", "mile*", "мил*"], Dim::Length, 1609.344, "mi"),
    unit(&["mg", "мг", "milligram*", "миллиграмм*"], Dim::Mass, 1e-6, "mg"),
    unit(&["kg", "кг", "kilogram*", "килограмм*", "кило"], Dim::Mass, 1.0, "kg"),
    unit(&["g", "г", "гр", "gram*", "грамм*"], Dim::Mass, 0.001, "g"),
    unit(&["t", "т", "tonne*", "ton", "tons", "тонн*"], Dim::Mass, 1000.0, "t"),
    unit(&["oz", "ounce*", "унц*"], Dim::Mass, 0.028349523125, "oz"),
    unit(&["lb", "lbs", "pound*", "фунт*"], Dim::Mass, 0.45359237, "lb"),
    unit(&["c", "\u{b0}c", "с", "\u{b0}с", "celsius", "цельси*"], Dim::Temperature, 1.0, "\u{b0}C"),
    unit(&["f", "\u{b0}f", "fahrenheit", "фаренгейт*"], Dim::Temperature, 1.0, "\u{b0}F"),
    unit(&["k", "\u{b0}k", "kelvin*", "кельвин*"], Dim::Temperature, 1.0, "K"),
    unit(&["b", "б", "byte*", "байт*"], Dim::Data, 1.0, "B"),
    unit(&["kb", "кб", "kilobyte*", "килобайт*"], Dim::Data, 1e3, "kB"),
    unit(&["mb", "мб", "megabyte*", "мегабайт*"], Dim::Data, 1e6, "MB"),
    unit(&["gb", "гб", "gigabyte*", "гигабайт*"], Dim::Data, 1e9, "GB"),
    unit(&["tb", "тб", "terabyte*", "терабайт*"], Dim::Data, 1e12, "TB"),
    unit(&["kib", "кибибайт*"], Dim::Data, 1024.0, "KiB"),
    unit(&["mib", "мебибайт*"], Dim::Data, 1_048_576.0, "MiB"),
    unit(&["gib", "гибибайт*"], Dim::Data, 1_073_741_824.0, "GiB"),
    unit(&["tib", "тебибайт*"], Dim::Data, 1_099_511_627_776.0, "TiB"),
];

fn find_unit(name: &str) -> Option<&'static Unit> {
    let name = name.trim().trim_end_matches('.');
    if name.is_empty() {
        return None;
    }
    UNITS.iter().find(|u| {
        u.names.iter().any(|n| match n.strip_suffix('*') {
            Some(stem) => name.starts_with(stem),
            None => *n == name,
        })
    })
}

fn to_celsius(unit: &Unit, v: f64) -> f64 {
    match unit.label {
        "\u{b0}F" => (v - 32.0) * 5.0 / 9.0,
        "K" => v - 273.15,
        _ => v,
    }
}

fn from_celsius(unit: &Unit, c: f64) -> f64 {
    match unit.label {
        "\u{b0}F" => c * 9.0 / 5.0 + 32.0,
        "K" => c + 273.15,
        _ => c,
    }
}

/// Converts `value` between two units of the same kind.
fn convert_value(value: f64, from: &Unit, to: &Unit) -> Option<f64> {
    if from.dim != to.dim {
        return None;
    }
    Some(if from.dim == Dim::Temperature {
        from_celsius(to, to_celsius(from, value))
    } else {
        value * from.factor / to.factor
    })
}

/// "5 km in miles", "5 \u{43a}\u{43c} \u{432} \u{43c}\u{438}\u{43b}\u{44f}\u{445}", "100 f to c".
fn convert(input: &str) -> Option<Answer> {
    let lower = input.to_lowercase();
    for sep in [" in ", " into ", " to ", " as ", " \u{432} ", " -> ", "->"] {
        let mut from = 0;
        while let Some(pos) = lower[from..].find(sep) {
            let at = from + pos;
            from = at + sep.len().max(1);
            if let Some(answer) = convert_parts(lower[..at].trim(), lower[at + sep.len()..].trim()) {
                return Some(answer);
            }
        }
    }
    None
}

fn convert_parts(left: &str, to: &str) -> Option<Answer> {
    let digits = left
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_digit() || matches!(c, '.' | ',' | '-' | '+')))
        .map_or(left.len(), |(i, _)| i);
    let (number, from) = left.split_at(digits);
    let value: f64 = number.trim().replace(',', ".").parse().ok()?;
    let (from_unit, to_unit) = (find_unit(from)?, find_unit(to)?);
    let result = convert_value(value, from_unit, to_unit)?;
    if !result.is_finite() {
        return None;
    }
    let text = format!("{} {}", format_number(result), to_unit.label);
    Some(Answer { text, detail: format!("{} {}", format_number(value), from_unit.label) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calc(input: &str) -> Option<String> {
        evaluate(input).map(|a| a.text)
    }

    #[test]
    fn arithmetic_follows_precedence() {
        assert_eq!(calc("2+3*4").as_deref(), Some("14"));
        assert_eq!(calc("(2+3)*4").as_deref(), Some("20"));
        assert_eq!(calc("2^3^2").as_deref(), Some("512"));
        assert_eq!(calc("-2^2").as_deref(), Some("-4"));
        assert_eq!(calc("2^-1").as_deref(), Some("0.5"));
        assert_eq!(calc("10 - 4 - 3").as_deref(), Some("3"));
        assert_eq!(calc("7 % 4").as_deref(), Some("3"));
        assert_eq!(calc("50%").as_deref(), Some("0.5"));
        assert_eq!(calc("84 / 2").as_deref(), Some("42"));
    }

    #[test]
    fn decimals_accept_comma_and_dot() {
        assert_eq!(calc("1,5+1,5").as_deref(), Some("3"));
        assert_eq!(calc("0.1+0.2").as_deref(), Some("0.3"));
        assert_eq!(calc("10/4").as_deref(), Some("2.5"));
        assert_eq!(calc("1/3").as_deref(), Some("0.3333333333"));
    }

    #[test]
    fn functions_and_constants() {
        assert_eq!(calc("sqrt(16)+1").as_deref(), Some("5"));
        assert_eq!(calc("cos(0)").as_deref(), Some("1"));
        assert_eq!(calc("sin(pi)").as_deref(), Some("0"));
        assert_eq!(calc("2*pi").as_deref(), Some("6.283185307"));
        assert_eq!(calc("sin(pi/2)").as_deref(), Some("1"));
    }

    #[test]
    fn plain_text_and_bad_input_are_not_calculations() {
        for input in ["", "42", "firefox", "-5", "(5)", "2+", "2 +* 3", "sqrt(-1)", "1/0", "foo(2)", "sqrt 4", "(1+2", "1+2)", "5 km", "1.2.3+1"] {
            assert_eq!(calc(input), None, "{input:?}");
        }
    }

    #[test]
    fn units_convert_length_mass_data() {
        assert_eq!(calc("5 km in miles").as_deref(), Some("3.106855961 mi"));
        assert_eq!(calc("5km to m").as_deref(), Some("5000 m"));
        assert_eq!(calc("1 mi to km").as_deref(), Some("1.609344 km"));
        assert_eq!(calc("10 in to cm").as_deref(), Some("25.4 cm"));
        assert_eq!(calc("2 kg in lb").as_deref(), Some("4.409245244 lb"));
        assert_eq!(calc("1 GB to MB").as_deref(), Some("1000 MB"));
        assert_eq!(calc("1 GiB in MiB").as_deref(), Some("1024 MiB"));
        assert_eq!(calc("1,5 km in m").as_deref(), Some("1500 m"));
    }

    #[test]
    fn units_convert_temperature() {
        assert_eq!(calc("100 f to c").as_deref(), Some("37.77777778 \u{b0}C"));
        assert_eq!(calc("0 c in f").as_deref(), Some("32 \u{b0}F"));
        assert_eq!(calc("0 c to k").as_deref(), Some("273.15 K"));
        assert_eq!(calc("-40 f to c").as_deref(), Some("-40 \u{b0}C"));
    }

    #[test]
    fn russian_units_and_separator() {
        assert_eq!(calc("5 \u{43a}\u{43c} \u{432} \u{43c}\u{438}\u{43b}\u{44f}\u{445}").as_deref(), Some("3.106855961 mi"));
        assert_eq!(calc("3 \u{43c}\u{438}\u{43b}\u{44c} \u{432} \u{43c}\u{435}\u{442}\u{440}\u{430}\u{445}").as_deref(), Some("4828.032 m"));
        assert_eq!(calc("2 \u{43a}\u{433} \u{432} \u{444}\u{443}\u{43d}\u{442}\u{430}\u{445}").as_deref(), Some("4.409245244 lb"));
        assert_eq!(calc("5 \u{43c}\u{438}\u{43b}\u{43b}\u{438}\u{43c}\u{435}\u{442}\u{440}\u{43e}\u{432} \u{432} \u{441}\u{43c}").as_deref(), Some("0.5 cm"));
    }

    #[test]
    fn mismatched_dimensions_do_not_convert() {
        assert_eq!(calc("5 km to kg"), None);
        assert_eq!(calc("5 foo to km"), None);
    }

    #[test]
    fn number_formatting() {
        assert_eq!(format_number(42.0), "42");
        assert_eq!(format_number(-0.5), "-0.5");
        assert_eq!(format_number(1e20), "1e20");
        assert_eq!(format_number(1.5e-9), "1.5e-9");
        assert_eq!(format_number(123456789.123456), "123456789.1");
    }
}
