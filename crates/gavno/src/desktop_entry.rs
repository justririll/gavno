//! Desktop application discovery and safe conversion of Exec fields to shell arguments.
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopEntry {
    pub id: String,
    pub name: String,
    pub exec: String,
    pub icon: String,
    pub startup_wm_class: String,
    pub terminal: bool,
}
impl DesktopEntry {
    pub fn parse(id: &str, text: &str) -> Option<Self> {
        let mut fields = HashMap::new();
        let mut main = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                main = line == "[Desktop Entry]";
                continue;
            }
            if main && !line.starts_with('#') {
                if let Some((key, value)) = line.split_once('=') {
                    fields.insert(key.trim(), value.trim());
                }
            }
        }
        if fields.get("Hidden") == Some(&"true")
            || fields.get("Type").is_some_and(|v| *v != "Application")
        {
            return None;
        }
        let exec = fields.get("Exec")?.to_string();
        Some(Self {
            id: id.trim_end_matches(".desktop").into(),
            name: fields.get("Name").unwrap_or(&id).to_string(),
            exec,
            icon: fields.get("Icon").unwrap_or(&"").to_string(),
            startup_wm_class: fields.get("StartupWMClass").unwrap_or(&"").to_string(),
            terminal: fields.get("Terminal") == Some(&"true"),
        })
    }
    pub fn command(&self, terminal: &str) -> Option<String> {
        let args = exec_arguments(&self.exec)?;
        if args.is_empty() {
            return None;
        }
        let command = args
            .iter()
            .map(|s| shell_quote(s))
            .collect::<Vec<_>>()
            .join(" ");
        Some(if self.terminal {
            format!("{terminal} -e {command}")
        } else {
            command
        })
    }
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Desktop Exec uses double quotes and backslash, not shell substitution.
/// File/URL/metadata field codes disappear; %% denotes a literal percent.
pub fn exec_arguments(exec: &str) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut arg = String::new();
    let mut quoted = false;
    let mut started = false;
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' => {
                arg.push(chars.next()?);
                started = true;
            }
            '%' => {
                if chars.next()? == '%' {
                    arg.push('%');
                    started = true;
                }
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    args.push(std::mem::take(&mut arg));
                    started = false;
                }
            }
            c => {
                arg.push(c);
                started = true;
            }
        }
    }
    if quoted {
        return None;
    }
    if started {
        args.push(arg);
    }
    Some(args)
}

pub fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(p) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
    {
        dirs.push(p);
    }
    dirs.extend(
        std::env::var("XDG_DATA_DIRS")
            .unwrap_or_else(|_| "/usr/local/share:/usr/share".into())
            .split(':')
            .map(PathBuf::from)
            .filter(|p| p.is_absolute()),
    );
    dirs
}

#[derive(Default)]
pub struct DesktopEntries {
    entries: Option<Vec<DesktopEntry>>,
}
impl DesktopEntries {
    pub fn resolve(&mut self, app: &str) -> Option<DesktopEntry> {
        let entries = self.entries.get_or_insert_with(|| load(&data_dirs()));
        let key = app.trim_end_matches(".desktop");
        entries
            .iter()
            .find(|e| e.id.eq_ignore_ascii_case(key))
            .or_else(|| {
                entries.iter().find(|e| {
                    !e.startup_wm_class.is_empty() && e.startup_wm_class.eq_ignore_ascii_case(key)
                })
            })
            .cloned()
    }
}
fn load(dirs: &[PathBuf]) -> Vec<DesktopEntry> {
    fn walk(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>, depth: usize) {
        if depth > 8 {
            return;
        }
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<_> = read.flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                walk(root, &path, files, depth + 1);
            } else if path.extension().is_some_and(|s| s == "desktop") {
                if let Ok(relative) = path.strip_prefix(root) {
                    files.push((relative.to_string_lossy().replace('/', "-"), path));
                }
            }
        }
    }
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in dirs {
        let root = dir.join("applications");
        let mut files = Vec::new();
        walk(&root, &root, &mut files, 0);
        for (id, path) in files {
            // Hidden entries also mask lower-priority system entries.
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(entry) = std::fs::read_to_string(path)
                .ok()
                .and_then(|s| DesktopEntry::parse(&id, &s))
            {
                result.push(entry);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_only_main_group_and_strips_field_codes() {
        let e = DesktopEntry::parse("org.test.desktop", "[Desktop Entry]\nType=Application\nName=Test App\nExec=\"/opt/Test App/run\" --new %U %f %% %i\nIcon=test\nStartupWMClass=TEST\n[Desktop Action New]\nExec=wrong").unwrap();
        assert_eq!(e.id, "org.test");
        assert_eq!(e.startup_wm_class, "TEST");
        assert_eq!(
            exec_arguments(&e.exec).unwrap(),
            ["/opt/Test App/run", "--new", "%"]
        );
        assert_eq!(
            exec_arguments("app \"a b\" x\\ y --file=%u").unwrap(),
            ["app", "a b", "x y", "--file="]
        );
        assert_eq!(exec_arguments("app \"unterminated"), None);
        assert_eq!(exec_arguments("app %"), None);
        assert_eq!(
            shell_quote("a'$(touch /tmp/no)"),
            "'a'\\''$(touch /tmp/no)'"
        );
        assert!(DesktopEntry::parse("hidden", "[Desktop Entry]\nExec=x\nHidden=true").is_none());
    }
}
