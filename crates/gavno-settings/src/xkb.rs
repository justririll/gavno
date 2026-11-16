#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub code: String,
    pub name: String,
}
pub fn parse(text: &str) -> Vec<Layout> {
    let mut section = "";
    let mut layouts = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(header) = line.strip_prefix('!') {
            section = header.trim();
            continue;
        }
        if section != "layout" || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((code, name)) = line.split_once(char::is_whitespace) {
            if code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && !name.trim().is_empty()
                && !layouts.iter().any(|l: &Layout| l.code == code)
            {
                layouts.push(Layout {
                    code: code.into(),
                    name: name.trim().into(),
                });
            }
        }
    }
    layouts
}
pub fn load() -> Result<Vec<Layout>, String> {
    for rules in ["evdev", "base"] {
        if let Ok(text) = std::fs::read_to_string(format!("/usr/share/X11/xkb/rules/{rules}.lst")) {
            let layouts = parse(&text);
            if !layouts.is_empty() {
                return Ok(layouts);
            }
        }
    }
    Err("No XKB layout list found in /usr/share/X11/xkb/rules".into())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_only_layout_section_with_whitespace_and_duplicates() {
        let text = "! model\npc105 Generic\n! layout\n  us    English (US)\n\tru\tRussian\n# comment\nus Duplicate\nbroken\n! variant\n  nodeadkeys us: No dead keys\n! option\n  grp:caps_toggle Caps Lock\n";
        assert_eq!(
            parse(text),
            vec![
                Layout {
                    code: "us".into(),
                    name: "English (US)".into()
                },
                Layout {
                    code: "ru".into(),
                    name: "Russian".into()
                }
            ]
        );
        assert!(parse("! variant\nx us: variant").is_empty());
    }
}
