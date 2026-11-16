//! Comment-preserving writes, performed only after a live change is accepted.
use serde_json::Value;
use std::{fs, io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf};
use toml_edit::{value, DocumentMut, Item, Table};

fn table<'a>(root: &'a mut Table, key: &str) -> Result<&'a mut Table, String> {
    if !root.contains_key(key) {
        root.insert(key, Item::Table(Table::new()));
    }
    root.get_mut(key)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| format!("{key} must be a TOML table"))
}
fn put(table: &mut Table, key: &str, mut item: Item) {
    if let (Some(old), Some(new)) = (table.get(key).and_then(Item::as_value), item.as_value_mut()) {
        *new.decor_mut() = old.decor().clone();
    }
    table.insert(key, item);
}
pub fn update(text: &str, method: &str, params: &Value) -> Result<String, String> {
    let mut doc = text.parse::<DocumentMut>().map_err(|e| e.to_string())?;
    // A comment-only document stores its introductory text as trailing decor.
    // Keep that text before the first table we add rather than moving it below.
    let header = if doc.as_table().is_empty() {
        let mut header = doc.trailing().as_str().unwrap_or_default().to_owned();
        if !header.is_empty() && !header.ends_with('\n') {
            header.push('\n');
        }
        doc.set_trailing("");
        header
    } else {
        String::new()
    };
    match method {
        "set_output" => {
            let name = params["output"].as_str().ok_or("missing output")?;
            let output = table(table(doc.as_table_mut(), "outputs")?, name)?;
            if let Some(mode) = params.get("mode") {
                let w = mode["w"].as_i64().ok_or("invalid mode width")?;
                let h = mode["h"].as_i64().ok_or("invalid mode height")?;
                let refresh = mode["refresh"].as_i64().ok_or("invalid mode refresh")?;
                put(
                    output,
                    "mode",
                    value(format!("{w}x{h}@{}", refresh as f64 / 1000.0)),
                );
            }
            if let Some(scale) = params.get("scale") {
                put(
                    output,
                    "scale",
                    value(scale.as_f64().ok_or("invalid scale")?),
                );
            }
            if let Some(hdr) = params.get("hdr") {
                put(output, "hdr", value(hdr.as_bool().ok_or("invalid HDR")?));
            }
        }
        "set_keyboard" => {
            let keyboard = table(doc.as_table_mut(), "keyboard")?;
            for key in ["layout", "variant", "options"] {
                if let Some(v) = params.get(key) {
                    put(
                        keyboard,
                        key,
                        value(v.as_str().ok_or("invalid keyboard setting")?),
                    );
                }
            }
        }
        _ => return Err("unsupported settings method".into()),
    }
    Ok(format!("{header}{doc}"))
}
fn path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("GAVNO_CONFIG").filter(|s| !s.is_empty()) {
        return Ok(path.into());
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|s| !s.is_empty())
                .map(|p| PathBuf::from(p).join(".config"))
        })
        .ok_or("HOME is not set")?;
    Ok(base.join("gavno/config.toml"))
}
pub fn save(method: &str, params: &Value) -> Result<(), String> {
    save_to(&path()?, method, params)
}
fn save_to(path: &std::path::Path, method: &str, params: &Value) -> Result<(), String> {
    let old = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.to_string()),
    };
    let text = update(&old, method, params)?;
    atomic_write(path, &text)
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn preserves_comments_and_untouched_keys() {
        let old = "# desktop\nterminal = 'foot' # custom\n[outputs.\"eDP-1\"]\nscale = 1.0 # zoom\nenabled = true\n[outputs.\"DP-2\"]\nscale = 2.0\n[keyboard]\nlayout = 'us' # language\nrepeat_rate = 25\n";
        let new = update(old, "set_output", &json!({"output":"eDP-1","scale":1.75})).unwrap();
        assert_eq!(
            new,
            old.replace("scale = 1.0 # zoom", "scale = 1.75 # zoom")
        );
        let next = update(
            &new,
            "set_keyboard",
            &json!({"layout":"us,ru","options":"grp:win_space_toggle"}),
        )
        .unwrap();
        assert!(next.contains("# language"));
        assert!(next.contains("repeat_rate = 25"));
        assert!(next.contains("terminal = 'foot' # custom"));
    }
    #[test]
    fn fractional_mode_and_new_tables_round_trip() {
        for original in ["# hello\n", "# hello"] {
            let text = update(
                original,
                "set_output",
                &json!({"output":"DP-1","mode":{"w":1920,"h":1080,"refresh":59940},"hdr":false}),
            )
            .unwrap();
            let doc = text.parse::<DocumentMut>().unwrap();
            assert_eq!(
                doc["outputs"]["DP-1"]["mode"].as_str(),
                Some("1920x1080@59.94")
            );
            assert!(text.starts_with("# hello\n"));
        }
        assert!(update("[keyboard]\n", "set_output", &json!({})).is_err());
    }
    #[test]
    fn atomic_writer_retains_previous_file_on_invalid_toml() {
        let dir = std::env::temp_dir().join(format!("gavno-settings-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, "# retained\nterminal = 'foot'\n").unwrap();
        save_to(&path, "set_keyboard", &json!({"layout":"de"})).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("# retained"));
        fs::write(&path, "invalid [").unwrap();
        assert!(save_to(&path, "set_keyboard", &json!({"layout":"us"})).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "invalid [");
        fs::remove_dir_all(dir).unwrap();
    }
}

#[derive(Clone, Debug)]
pub struct Change {
    pub path: Vec<String>,
    pub value: Value,
}
fn item(value: &Value) -> Result<Item, String> {
    if let Some(v) = value.as_bool() {
        Ok(toml_edit::value(v))
    } else if let Some(v) = value.as_i64() {
        Ok(toml_edit::value(v))
    } else if let Some(v) = value.as_f64() {
        Ok(toml_edit::value(v))
    } else if let Some(v) = value.as_str() {
        Ok(toml_edit::value(v))
    } else if let Some(v) = value.as_array() {
        let mut array = toml_edit::Array::new();
        for v in v {
            array.push(v.as_str().ok_or("List entries must be strings")?);
        }
        Ok(Item::Value(toml_edit::Value::Array(array)))
    } else {
        Err("Unsupported config value".into())
    }
}
fn set_item(root: &mut Item, parts: &[String], value: &Value) -> Result<(), String> {
    let (key, rest) = parts.split_first().ok_or("Empty config path")?;
    let table = root
        .as_table_like_mut()
        .ok_or_else(|| format!("{key}: parent is not a table"))?;
    if rest.is_empty() {
        if value.is_null() {
            table.remove(key);
        } else {
            let mut next = item(value)?;
            if let (Some(old), Some(new)) =
                (table.get(key).and_then(Item::as_value), next.as_value_mut())
            {
                *new.decor_mut() = old.decor().clone();
            }
            table.insert(key, next);
        }
    } else {
        if !table.contains_key(key) {
            if value.is_null() {
                return Ok(());
            }
            table.insert(key, Item::Table(Table::new()));
        }
        set_item(
            table.get_mut(key).ok_or("Missing parent table")?,
            rest,
            value,
        )?;
    }
    Ok(())
}
pub fn update_many(text: &str, changes: &[Change]) -> Result<String, String> {
    let mut doc = text.parse::<DocumentMut>().map_err(|e| e.to_string())?;
    let mut header = String::new();
    if doc.as_table().is_empty() {
        header = doc.trailing().as_str().unwrap_or_default().to_owned();
        doc.set_trailing("");
        if !header.is_empty() && !header.ends_with('\n') {
            header.push('\n');
        }
    }
    for change in changes {
        set_item(doc.as_item_mut(), &change.path, &change.value)?;
    }
    Ok(format!("{header}{doc}"))
}
pub fn read() -> Result<String, String> {
    read_from(&path()?)
}
fn read_from(path: &std::path::Path) -> Result<String, String> {
    use std::io::Read;
    match fs::File::open(path) {
        Ok(file) => {
            let mut text = String::new();
            file.take(1024 * 1024 + 1)
                .read_to_string(&mut text)
                .map_err(|e| e.to_string())?;
            if text.len() > 1024 * 1024 {
                Err("Config exceeds 1 MiB".into())
            } else {
                Ok(text)
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e.to_string()),
    }
}
/// Validation happens before either config.toml or its backup is touched.
pub fn save_raw(text: &str, specs: &[crate::model::Spec]) -> Result<String, String> {
    save_raw_to(&path()?, text, specs)
}
fn save_raw_to(
    path: &std::path::Path,
    text: &str,
    specs: &[crate::model::Spec],
) -> Result<String, String> {
    crate::model::validate(text, specs)?;
    let old = read_from(path)?;
    atomic_write(&path.with_extension("toml.bak"), &old)?;
    atomic_write(path, text)?;
    Ok(old)
}
pub fn save_changes(
    changes: &[Change],
    specs: &[crate::model::Spec],
) -> Result<(String, String), String> {
    let path = path()?;
    let old = read_from(&path)?;
    let next = update_many(&old, changes)?;
    crate::model::validate(&next, specs)?;
    atomic_write(&path, &next)?;
    Ok((old, next))
}
pub fn restore(text: &str) -> Result<(), String> {
    atomic_write(&path()?, text)
}
fn atomic_write(path: &std::path::Path, text: &str) -> Result<(), String> {
    // A symlinked config (a dotfiles repo) is written through to its target;
    // renaming over the link would silently replace it with a plain file.
    let resolved = fs::symlink_metadata(path)
        .ok()
        .filter(|m| m.file_type().is_symlink())
        .and_then(|_| fs::canonicalize(path).ok());
    let path = resolved.as_deref().unwrap_or(path);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let temp = parent.join(format!(
        ".settings-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos()
    ));
    let result = (|| -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        if let Ok(meta) = fs::metadata(path) {
            file.set_permissions(meta.permissions())?;
        }
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        fs::File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result.map_err(|e| e.to_string())
}
#[cfg(test)]
mod symlink_tests {
    #[test]
    fn saving_keeps_a_symlinked_config_linked() {
        let dir = std::env::temp_dir().join(format!("gavno-settings-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dotfiles")).unwrap();
        let target = dir.join("dotfiles/config.toml");
        std::fs::write(&target, "# mine\ngap = 8\n").unwrap();
        let link = dir.join("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        super::atomic_write(&link, "# mine\ngap = 20\n").unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "# mine\ngap = 20\n");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
#[cfg(test)]
mod editor_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn paths_preserve_comments_unknown_keys_and_inline_tables() {
        let old="# retained\nunknown=42\n[dock]\npinned=['foot'] # apps\n[outputs.'DP.foo']\nscale=1.0\n";
        let next = update_many(
            old,
            &[
                Change {
                    path: vec!["dock".into(), "pinned".into()],
                    value: json!(["firefox"]),
                },
                Change {
                    path: vec!["outputs".into(), "DP.foo".into(), "scale".into()],
                    value: json!(1.75),
                },
            ],
        )
        .unwrap();
        assert!(next.contains("# retained"));
        assert!(next.contains("# apps"));
        assert!(next.contains("unknown=42"));
        let parsed = next.parse::<DocumentMut>().unwrap();
        assert_eq!(parsed["outputs"]["DP.foo"]["scale"].as_float(), Some(1.75));
        assert!(update_many(
            "dock={magnify=true, other=42}",
            &[Change {
                path: vec!["dock".into(), "magnify".into()],
                value: json!(false)
            }]
        )
        .unwrap()
        .contains("other=42"));
        let reset = update_many(
            &next,
            &[Change {
                path: vec!["dock".into(), "pinned".into()],
                value: Value::Null,
            }],
        )
        .unwrap();
        assert!(reset.parse::<DocumentMut>().unwrap()["dock"]
            .get("pinned")
            .is_none());
    }
    #[test]
    fn invalid_raw_edits_do_not_touch_file_or_backup() {
        let dir = std::env::temp_dir().join(format!("settings-raw-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let old = "# previous\ngap=12\n";
        fs::write(&path, old).unwrap();
        let specs = crate::model::fallback().unwrap();
        for bad in ["gap=[", "gap='big'", "mod_key='invalid'"] {
            assert!(save_raw_to(&path, bad, &specs).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), old);
            assert!(!path.with_extension("toml.bak").exists());
        }
        save_raw_to(&path, "gap=20\n", &specs).unwrap();
        assert_eq!(
            fs::read_to_string(path.with_extension("toml.bak")).unwrap(),
            old
        );
        fs::remove_dir_all(dir).unwrap();
    }
}

pub fn set_value(doc: &mut DocumentMut, change: &Change) -> Result<(), String> {
    set_item(doc.as_item_mut(), &change.path, &change.value)
}
