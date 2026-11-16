//! GTK-free row model and validation, shared by generated forms and raw edits.
use serde::Deserialize;
use serde_json::{json, Value};
use toml_edit::{DocumentMut, Item};
#[derive(Clone, Debug, Deserialize)]
pub struct Spec {
    pub path: String,
    pub kind: Kind,
    pub default: Value,
    pub description: String,
    pub section: String,
    pub live: bool,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Kind {
    Bool,
    Int { min: i64, max: i64, step: i64 },
    Float { min: f64, max: f64, step: f64 },
    String,
    Enum { values: Vec<String> },
    StringList,
}
#[derive(Debug, PartialEq)]
pub enum Control {
    Switch,
    Spin,
    Entry,
    Combo,
    List,
}
impl Spec {
    pub fn control(&self) -> Control {
        match self.kind {
            Kind::Bool => Control::Switch,
            Kind::Int { .. } | Kind::Float { .. } => Control::Spin,
            Kind::String => Control::Entry,
            Kind::Enum { .. } => Control::Combo,
            Kind::StringList => Control::List,
        }
    }
    pub fn accepts(&self, value: &Value) -> bool {
        if value.is_null() {
            return self.default.is_null();
        }
        match &self.kind {
            Kind::Bool => value.is_boolean(),
            Kind::String => value.is_string(),
            Kind::StringList => value
                .as_array()
                .is_some_and(|a| a.iter().all(Value::is_string)),
            Kind::Enum { values } => value
                .as_str()
                .is_some_and(|s| values.iter().any(|v| v == s)),
            Kind::Int { min, max, .. } => {
                value.as_i64().is_some_and(|n| (*min..=*max).contains(&n))
            }
            Kind::Float { min, max, .. } => value
                .as_f64()
                .is_some_and(|n| n.is_finite() && (*min..=*max).contains(&n)),
        }
    }
}
pub fn schema(value: Value) -> Result<Vec<Spec>, String> {
    let specs: Vec<Spec> = serde_json::from_value(value).map_err(|e| e.to_string())?;
    if specs.is_empty() || specs.len() > 256 {
        return Err("Invalid config schema size".into());
    }
    for s in &specs {
        if !s.accepts(&s.default) || s.path.is_empty() || s.description.is_empty() {
            return Err(format!("Invalid schema entry {}", s.path));
        }
        match &s.kind {
            Kind::Int { min, max, step } if min > max || *step <= 0 => {
                return Err("Invalid integer range".into())
            }
            Kind::Float { min, max, step }
                if !min.is_finite()
                    || !max.is_finite()
                    || !step.is_finite()
                    || min > max
                    || *step <= 0.0 =>
            {
                return Err("Invalid numeric range".into())
            }
            Kind::Enum { values } if values.is_empty() => return Err("Empty enum".into()),
            _ => {}
        }
    }
    Ok(specs)
}
pub fn fallback() -> Result<Vec<Spec>, String> {
    schema(crate::config_schema::json(false)?)
}
pub fn lookup(doc: &DocumentMut, parts: &[String]) -> Value {
    let mut item = doc.as_item();
    for part in parts {
        let Some(child) = item.get(part) else {
            return Value::Null;
        };
        item = child;
    }
    item_value(item)
}
fn scalar(value: &toml_edit::Value) -> Value {
    if let Some(v) = value.as_bool() {
        json!(v)
    } else if let Some(v) = value.as_integer() {
        json!(v)
    } else if let Some(v) = value.as_float() {
        if v.is_finite() {
            json!(v)
        } else {
            json!({})
        }
    } else if let Some(v) = value.as_str() {
        json!(v)
    } else if let Some(v) = value.as_array() {
        Value::Array(v.iter().map(scalar).collect())
    } else {
        json!({})
    }
}
fn item_value(item: &Item) -> Value {
    item.as_value().map(scalar).unwrap_or_else(|| json!({}))
}
pub fn validate(text: &str, specs: &[Spec]) -> Result<DocumentMut, String> {
    if text.len() > 1024 * 1024 {
        return Err("Config exceeds 1 MiB".into());
    }
    let doc = toml_edit::Document::parse(text).map_err(|e| e.to_string())?;
    for spec in specs {
        let paths = if spec.path.contains(".*.") {
            if let Some(outputs) = doc.get("outputs") {
                let table = outputs.as_table_like().ok_or_else(|| {
                    error_line(text, "outputs", "must be a table", outputs.span())
                })?;
                table
                    .iter()
                    .map(|(name, _)| {
                        vec![
                            "outputs".into(),
                            name.into(),
                            spec.path.rsplit('.').next().unwrap_or("").into(),
                        ]
                    })
                    .collect::<Vec<Vec<String>>>()
            } else {
                Vec::new()
            }
        } else {
            vec![spec.path.split('.').map(str::to_owned).collect()]
        };
        for path in paths {
            let mut item = doc.as_item();
            let mut present = true;
            for (i, part) in path.iter().enumerate() {
                if i > 0 && item.as_table_like().is_none() {
                    return Err(error_line(
                        text,
                        &path[..i].join("."),
                        "must be a table",
                        item.span(),
                    ));
                }
                if let Some(child) = item.get(part) {
                    item = child;
                } else {
                    present = false;
                    break;
                }
            }
            if present && !spec.accepts(&item_value(item)) {
                return Err(error_line(
                    text,
                    &path.join("."),
                    "has an invalid type or value",
                    item.span(),
                ));
            }
        }
    }
    Ok(doc.into_mut())
}
fn error_line(
    text: &str,
    path: &str,
    message: &str,
    span: Option<std::ops::Range<usize>>,
) -> String {
    let offset = span.map_or(0, |span| span.start).min(text.len());
    let prefix = &text.as_bytes()[..offset];
    let line = prefix.iter().filter(|b| **b == b'\n').count() + 1;
    let column = prefix
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(offset + 1, |i| offset - i);
    format!("{path} {message} (line {line}, column {column})")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema_json_generates_each_row_type() {
        let specs = fallback().unwrap();
        for (name, expected) in [
            ("modules.dock", Control::Switch),
            ("gap", Control::Spin),
            ("output.internal_scale", Control::Spin),
            ("terminal", Control::Entry),
            ("mod_key", Control::Combo),
            ("dock.pinned", Control::List),
        ] {
            assert_eq!(
                specs.iter().find(|s| s.path == name).unwrap().control(),
                expected
            );
        }
        assert!(specs.iter().any(|s| !s.live));
        assert!(schema(json!([{"path":"x","kind":{"type":"int","min":2,"max":1,"step":1},"default":null,"description":"x","section":"x","live":true}])).is_err());
    }
    #[test]
    fn raw_validation_accepts_unknown_keys_and_rejects_bad_known_values() {
        let specs = fallback().unwrap();
        assert!(validate(
            "# comment\nunknown = true\n[dock]\npinned=['foot','firefox']",
            &specs
        )
        .is_ok());
        let repeated = "[outputs.'DP-1']\nscale=1.0\n[outputs.'DP-2']\nscale='bad'";
        assert!(validate(repeated, &specs).unwrap_err().contains("line 4"));
        for bad in [
            "x = [",
            "gap='big'",
            "mod_key='ctrl'",
            "[dock]\npinned=[1]",
            "dock=true",
            "[outputs.'eDP-1']\nscale=nan",
        ] {
            let error = validate(bad, &specs).unwrap_err();
            assert!(error.contains("line"), "{error}");
        }
    }
}
