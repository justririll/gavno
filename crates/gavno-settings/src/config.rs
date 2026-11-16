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
    let parent = path.parent().ok_or("config path has no parent")?;
    let parent = if parent.as_os_str().is_empty() {
        std::path::Path::new(".")
    } else {
        parent
    };
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let temp = parent.join(format!(
        ".config.toml.settings-{}-{}",
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
