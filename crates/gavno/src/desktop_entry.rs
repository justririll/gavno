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
    /// Location of the desktop file, used to expand `%k`.
    pub path: Option<PathBuf>,
    pub generic_name: String,
    pub keywords: String,
    pub comment: String,
    /// `NoDisplay=true`, or `OnlyShowIn`/`NotShowIn` excluding GAVNO.
    pub hidden_from_menus: bool,
}
impl DesktopEntry {
    pub fn parse(id: &str, text: &str, path: Option<&Path>) -> Option<Self> {
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
        let lists = |key: &str| fields.get(key).map(|v| v.split(';').any(|d| d == "GAVNO"));
        let hidden_from_menus = fields.get("NoDisplay") == Some(&"true")
            || lists("OnlyShowIn") == Some(false)
            || lists("NotShowIn") == Some(true);
        Some(Self {
            id: id.trim_end_matches(".desktop").into(),
            name: fields.get("Name").unwrap_or(&id).to_string(),
            exec,
            icon: fields.get("Icon").unwrap_or(&"").to_string(),
            startup_wm_class: fields.get("StartupWMClass").unwrap_or(&"").to_string(),
            terminal: fields.get("Terminal") == Some(&"true"),
            path: path.map(Path::to_owned),
            generic_name: fields.get("GenericName").unwrap_or(&"").to_string(),
            keywords: fields.get("Keywords").unwrap_or(&"").replace(';', " "),
            comment: fields.get("Comment").unwrap_or(&"").to_string(),
            hidden_from_menus,
        })
    }
    pub fn command(&self, terminal: &str) -> Option<String> {
        let args = self.arguments()?;
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
    /// Expand the Exec field using this entry's metadata.
    pub fn arguments(&self) -> Option<Vec<String>> {
        expand_exec(
            &self.exec,
            Some(self.name.as_str()).filter(|name| !name.is_empty()),
            Some(self.icon.as_str()).filter(|icon| !icon.is_empty()),
            self.path.as_deref(),
        )
    }
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Desktop Exec uses double quotes and backslash, not shell substitution.
/// File/URL field codes disappear; metadata codes expand into their values and
/// `%%` denotes a literal percent.
fn expand_exec(
    exec: &str,
    name: Option<&str>,
    icon: Option<&str>,
    path: Option<&Path>,
) -> Option<Vec<String>> {
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
            '%' => match chars.next()? {
                // A literal percent.
                '%' => {
                    arg.push('%');
                    started = true;
                }
                // The application name is a single argument even if it has
                // spaces; it expands in place so `--title=%c` keeps meaning.
                'c' => {
                    if let Some(name) = name {
                        arg.push_str(name);
                        started = true;
                    }
                }
                // An icon expands to the two arguments `--icon <Icon>`, and is
                // omitted entirely when the entry has no icon.
                'i' => {
                    if let Some(icon) = icon {
                        if started {
                            args.push(std::mem::take(&mut arg));
                            started = false;
                        }
                        args.push("--icon".into());
                        args.push(icon.into());
                    }
                }
                // The desktop file path expands in place.
                'k' => {
                    if let Some(path) = path {
                        arg.push_str(&path.to_string_lossy());
                        started = true;
                    }
                }
                // File, URL and deprecated field codes are removed.
                'f' | 'F' | 'u' | 'U' | 'd' | 'D' | 'n' | 'N' | 'v' | 'm' => {}
                // Unknown field codes are discarded with their code letter.
                _ => {}
            },
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
/// Every application entry on the system, highest-priority data dir first.
pub fn load_all() -> Vec<DesktopEntry> {
    load(&data_dirs())
}
fn load(dirs: &[PathBuf]) -> Vec<DesktopEntry> {
    let _startup = crate::perf::Startup::new("desktop_entries.load.begin", "desktop_entries.load.end");
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
        tracing::info!(target: "gavno::startup", stage = "desktop_entries.directory_scanned", ms = crate::perf::elapsed_ms(), path = %root.display(), files = files.len());
        for (id, path) in files {
            // Hidden entries also mask lower-priority system entries.
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(entry) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| DesktopEntry::parse(&id, &s, Some(&path)))
            {
                result.push(entry);
            }
        }
    }
    crate::perf::startup("desktop_entries.parsed");
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_only_main_group_and_expands_field_codes() {
        let e = DesktopEntry::parse(
            "org.test.desktop",
            "[Desktop Entry]\nType=Application\nName=Test App\nExec=\"/opt/Test App/run\" --new %U %f %% %i\nIcon=test\nStartupWMClass=TEST\n[Desktop Action New]\nExec=wrong",
            Some(Path::new("/usr/share/applications/org.test.desktop")),
        )
        .unwrap();
        assert_eq!(e.id, "org.test");
        assert_eq!(e.startup_wm_class, "TEST");
        assert_eq!(
            e.arguments().unwrap(),
            ["/opt/Test App/run", "--new", "%", "--icon", "test"]
        );
        assert_eq!(
            expand_exec("app \"a b\" x\\ y --file=%u %c %i %k", None, None, None).unwrap(),
            ["app", "a b", "x y", "--file="]
        );
        assert_eq!(expand_exec("app \"unterminated", None, None, None), None);
        assert_eq!(expand_exec("app %", None, None, None), None);
        assert_eq!(
            shell_quote("a'$(touch /tmp/no)"),
            "'a'\\''$(touch /tmp/no)'"
        );
        assert!(
            DesktopEntry::parse("hidden", "[Desktop Entry]\nExec=x\nHidden=true", None).is_none()
        );
    }
    #[test]
    fn metadata_field_codes_keep_argument_boundaries() {
        let exec = "app --title %c --new %i --file=%k %f %F %u %U %d %D %n %N %v %m %%";
        let e = DesktopEntry::parse(
            "org.test",
            &format!(
                "[Desktop Entry]\nType=Application\nName=My App\nIcon=my-icon\nExec={exec}"
            ),
            Some(Path::new("/usr/share/applications/org.test.desktop")),
        )
        .unwrap();
        assert_eq!(
            e.arguments().unwrap(),
            [
                "app",
                "--title",
                "My App",
                "--new",
                "--icon",
                "my-icon",
                "--file=/usr/share/applications/org.test.desktop",
                "%",
            ]
        );

        // %i expands to nothing (not an empty argument) when Icon is absent.
        let no_icon = DesktopEntry::parse(
            "no-icon",
            "[Desktop Entry]\nName=No Icon\nExec=app %i --new",
            None,
        )
        .unwrap();
        assert_eq!(no_icon.arguments().unwrap(), ["app", "--new"]);

        // %k expands to nothing when the desktop file path is unknown.
        let no_path = DesktopEntry::parse(
            "no-path",
            "[Desktop Entry]\nName=No Path\nExec=app %k --new",
            None,
        )
        .unwrap();
        assert_eq!(no_path.arguments().unwrap(), ["app", "--new"]);

        // %c is one argument even when the name contains spaces.
        let spaced = DesktopEntry::parse(
            "spaced",
            "[Desktop Entry]\nName=Two  Words\nExec=app --name=%c",
            None,
        )
        .unwrap();
        assert_eq!(spaced.arguments().unwrap(), ["app", "--name=Two  Words"]);
    }
}
