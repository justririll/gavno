use crate::{
    config::Config,
    config_schema::{Kind, SCHEMA},
};
use serde_json::{json, Value};
fn parts(path: &str) -> Vec<&str> {
    path.split('.')
        .map(|p| if p == "*" { "eDP-1" } else { p })
        .collect()
}
fn set(table: &mut toml::Table, path: &[&str], value: Value) {
    if path.len() == 1 {
        table.insert(path[0].into(), toml::Value::try_from(value).unwrap());
        return;
    }
    let child = table
        .entry(path[0])
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    set(child.as_table_mut().unwrap(), &path[1..], value);
}
fn get<'a>(root: &'a Value, path: &[&str]) -> &'a Value {
    path.iter().fold(root, |value, key| &value[*key])
}
#[test]
fn every_declared_default_matches_absence() {
    for nested in [false, true] {
        let absent = Config::build(nested, None, |_| None).0;
        let values = absent.values();
        let metadata = crate::config_schema::json(nested).unwrap();
        for key in metadata.as_array().unwrap() {
            let path = key["path"].as_str().unwrap();
            let default = key["default"].clone();
            assert_eq!(get(&values, &parts(path)), &default, "{path}");
            let mut table = toml::Table::new();
            if !default.is_null() {
                set(&mut table, &parts(path), default);
            }
            let text = toml::to_string(&table).unwrap();
            assert_eq!(
                Config::parse_checked(nested, &text, |_| None).unwrap(),
                absent,
                "{path}"
            );
        }
    }
}
#[test]
fn every_key_loads_a_non_default_valid_value_together() {
    let mut table = toml::Table::new();
    let mut expected = Vec::new();
    for key in SCHEMA {
        let default = serde_json::to_value(key.default).unwrap();
        let value = match key.kind {
            Kind::Bool => json!(!default.as_bool().unwrap_or(true)),
            Kind::Int { min, max, .. } => json!(if default == min {
                max.min(min + 1)
            } else {
                min
            }),
            Kind::Float { min, max, step } => json!(if default == min {
                max.min(min + step)
            } else {
                min
            }),
            Kind::Enum { values } => json!(values.iter().find(|v| default != **v).unwrap()),
            Kind::StringList => json!(["kitty"]),
            Kind::String => json!(match key.path {
                "output.mode" | "outputs.*.mode" => "1920x1080@59.94",
                "keyboard.layout" => "de",
                "keyboard.variant" => "nodeadkeys",
                "keyboard.options" => "caps:escape",
                _ => "alternate-command",
            }),
        };
        assert_ne!(value, default, "{}", key.path);
        set(&mut table, &parts(key.path), value.clone());
        expected.push((key.path, value));
    }
    let text = toml::to_string(&table).unwrap();
    let loaded = Config::parse_checked(false, &text, |_| None)
        .unwrap()
        .values();
    for (path, value) in expected {
        assert_eq!(get(&loaded, &parts(path)), &value, "{path}");
    }
}
fn walk_doc(table: &toml::Table, prefix: &str, keys: &mut Vec<String>) {
    for (key, value) in table {
        let key = if prefix == "outputs" {
            "*"
        } else {
            key.as_str()
        };
        let path = if prefix.is_empty() {
            key.into()
        } else {
            format!("{prefix}.{key}")
        };
        if let Some(table) = value.as_table() {
            walk_doc(table, &path, keys);
        } else {
            keys.push(path);
        }
    }
}
#[test]
fn every_documented_key_is_in_the_schema() {
    let doc = include_str!("../../../docs/config.md");
    let mut keys = Vec::new();
    let mut block = None;
    for line in doc.lines() {
        if line == "```toml" {
            block = Some(String::new());
            continue;
        }
        if line == "```" {
            if let Some(text) = block.take() {
                walk_doc(
                    &toml::from_str::<toml::Table>(&text).unwrap(),
                    "",
                    &mut keys,
                );
            }
            continue;
        }
        if let Some(text) = &mut block {
            text.push_str(line);
            text.push('\n');
        }
        if let Some(cell) = line
            .strip_prefix("| `")
            .and_then(|s| s.split_once('`').map(|(cell, _)| cell))
        {
            let key = if let Some((section, key)) =
                cell.strip_prefix('[').and_then(|s| s.split_once("] "))
            {
                format!("{section}.{key}")
            } else {
                cell.into()
            };
            keys.push(key);
        }
    }
    assert!(
        keys.len() > 40,
        "documentation parser must examine actual key declarations"
    );
    for key in keys {
        assert!(
            SCHEMA.iter().any(|s| s.path == key),
            "documented key {key} has no schema entry"
        );
    }
}

/// Manual, isolated RSS comparison. Run separately so other test allocations and
/// workers cannot contaminate the samples. No graphics device or D-Bus is used.
#[test]
#[ignore = "manual Linux RSS measurement"]
fn rss_probe() {
    fn sample() -> (u64, u64) {
        let text = std::fs::read_to_string("/proc/self/status").unwrap();
        let field = |key: &str| {
            text.lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap()
        };
        (field("VmRSS:"), field("Threads:"))
    }
    let mut h = crate::state::tests::Harness::new();
    let before = sample();
    let schema = serde_json::to_vec(SCHEMA).unwrap();
    std::hint::black_box(&schema);
    drop(schema);
    let after_schema = sample();
    let previous = h.gv.config.clone();
    let old = previous.clone();
    let prepared = std::thread::spawn(move || {
        let next = Config::parse_checked(true, "gap=13\n[dock]\nmagnify=false", |_| None).unwrap();
        crate::config_reload::Prepared::new(next, &old).unwrap()
    })
    .join()
    .unwrap();
    let result = crate::config_reload::apply(&mut h.gv, prepared).unwrap();
    drop(result);
    let restored = crate::config_reload::Prepared::new(previous, &h.gv.config).unwrap();
    drop(crate::config_reload::apply(&mut h.gv, restored).unwrap());
    let after_first_reload = sample();
    for _ in 0..12 {
        let previous = h.gv.config.clone();
        let old = previous.clone();
        let prepared = std::thread::spawn(move || {
            let next =
                Config::parse_checked(true, "gap=13\n[dock]\nmagnify=false", |_| None).unwrap();
            crate::config_reload::Prepared::new(next, &old).unwrap()
        })
        .join()
        .unwrap();
        drop(crate::config_reload::apply(&mut h.gv, prepared).unwrap());
        let restored = crate::config_reload::Prepared::new(previous, &h.gv.config).unwrap();
        drop(crate::config_reload::apply(&mut h.gv, restored).unwrap());
    }
    let after_reload = sample();
    println!(
        "RSS_PROBE {}",
        json!({"before_kib":before.0,"after_schema_kib":after_schema.0,"after_first_reload_kib":after_first_reload.0,"after_reload_kib":after_reload.0,"threads_before":before.1,"threads_after":after_reload.1})
    );
    assert_eq!(before.1, after_reload.1, "reload must not retain a worker");
}
