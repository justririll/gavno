//! Line protocol between `gavno-cam` and the compositor over a private
//! socketpair. One ASCII line per message:
//!
//! ```text
//! M <x> <y>        pointer position, 0..1 on the active output
//! B l|r 1|0        left/right button pressed/released
//! S <dy>           scroll in pixels, positive scrolls down
//! H 1|0            hand present / lost
//! ```

/// Longest line the reader keeps; anything longer is garbage and is dropped.
pub const MAX_LINE: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Message {
    Motion { x: f32, y: f32 },
    Button { right: bool, pressed: bool },
    Scroll { dy: f32 },
    Hand { present: bool },
}

impl Message {
    /// Appends the line, including its newline, to `out`.
    pub fn encode(&self, out: &mut String) {
        use std::fmt::Write;
        let _ = match *self {
            Self::Motion { x, y } => writeln!(out, "M {x:.5} {y:.5}"),
            Self::Button { right, pressed } => {
                writeln!(out, "B {} {}", if right { 'r' } else { 'l' }, u8::from(pressed))
            }
            Self::Scroll { dy } => writeln!(out, "S {dy:.2}"),
            Self::Hand { present } => writeln!(out, "H {}", u8::from(present)),
        };
    }

    /// Parses one line without its newline. Out-of-range or non-finite numbers
    /// are rejected, so a corrupt child cannot push the cursor off-screen.
    pub fn parse(line: &str) -> Option<Self> {
        let mut parts = line.split(' ');
        let kind = parts.next()?;
        let message = match kind {
            "M" => {
                let x = number(parts.next()?)?;
                let y = number(parts.next()?)?;
                if !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y) {
                    return None;
                }
                Self::Motion { x, y }
            }
            "B" => {
                let right = match parts.next()? {
                    "l" => false,
                    "r" => true,
                    _ => return None,
                };
                let pressed = flag(parts.next()?)?;
                Self::Button { right, pressed }
            }
            "S" => {
                let dy = number(parts.next()?)?;
                if dy.abs() > 10_000.0 {
                    return None;
                }
                Self::Scroll { dy }
            }
            "H" => Self::Hand { present: flag(parts.next()?)? },
            _ => return None,
        };
        parts.next().is_none().then_some(message)
    }
}

fn number(text: &str) -> Option<f32> {
    text.parse::<f32>().ok().filter(|v| v.is_finite())
}
fn flag(text: &str) -> Option<bool> {
    match text {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

/// Reassembles messages from arbitrary read chunks. Holds at most `MAX_LINE`
/// bytes; malformed lines are skipped.
#[derive(Default)]
pub struct LineReader {
    line: Vec<u8>,
    overflow: bool,
}

impl LineReader {
    pub fn feed(&mut self, bytes: &[u8], mut message: impl FnMut(Message)) {
        for &byte in bytes {
            if byte == b'\n' {
                let overflow = std::mem::take(&mut self.overflow);
                if !overflow {
                    if let Some(parsed) = std::str::from_utf8(&self.line).ok().and_then(Message::parse) {
                        message(parsed);
                    }
                }
                self.line.clear();
            } else if self.line.len() < MAX_LINE {
                self.line.push(byte);
            } else {
                self.overflow = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(message: Message) -> Message {
        let mut text = String::new();
        message.encode(&mut text);
        assert!(text.ends_with('\n'));
        Message::parse(text.trim_end_matches('\n')).unwrap()
    }

    #[test]
    fn every_message_round_trips() {
        assert_eq!(round_trip(Message::Motion { x: 0.25, y: 0.75 }), Message::Motion { x: 0.25, y: 0.75 });
        for right in [false, true] {
            for pressed in [false, true] {
                assert_eq!(round_trip(Message::Button { right, pressed }), Message::Button { right, pressed });
            }
        }
        assert_eq!(round_trip(Message::Scroll { dy: -12.5 }), Message::Scroll { dy: -12.5 });
        assert_eq!(round_trip(Message::Hand { present: true }), Message::Hand { present: true });
    }

    #[test]
    fn bad_lines_are_rejected() {
        for line in ["", "M", "M 0.5", "M 1.5 0.5", "M nan 0.5", "M 0.5 0.5 0.5", "B x 1", "B l 2", "S inf", "S 1e9", "H", "X 1"] {
            assert_eq!(Message::parse(line), None, "{line:?}");
        }
    }

    #[test]
    fn reader_handles_split_chunks_and_garbage() {
        let mut reader = LineReader::default();
        let mut got = Vec::new();
        for chunk in [&b"M 0.5 0."[..], b"5\nB l", b" 1\ngarbage\nH 0\n"] {
            reader.feed(chunk, |m| got.push(m));
        }
        assert_eq!(got, [
            Message::Motion { x: 0.5, y: 0.5 },
            Message::Button { right: false, pressed: true },
            Message::Hand { present: false },
        ]);
    }

    #[test]
    fn reader_drops_overlong_lines_and_recovers() {
        let mut reader = LineReader::default();
        let mut got = Vec::new();
        let mut long = vec![b'M'; MAX_LINE * 3];
        long.push(b'\n');
        reader.feed(&long, |m| got.push(m));
        reader.feed(b"H 1\n", |m| got.push(m));
        assert_eq!(got, [Message::Hand { present: true }]);
    }
}
