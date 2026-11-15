//! Newline framing lives in transport; this module validates individual requests.
use serde_json::{json, Value};

pub fn reply(line: &[u8], dispatch: impl FnOnce(&str, &Value) -> Result<Value, String>) -> Vec<u8> {
    let request = serde_json::from_slice::<Value>(line);
    let id = request
        .as_ref()
        .ok()
        .and_then(|r| r.get("id"))
        .cloned()
        .unwrap_or(Value::Null);
    let result = request
        .map_err(|e| format!("invalid JSON: {e}"))
        .and_then(|r| {
            if !id.is_u64() {
                return Err("id must be a non-negative integer".into());
            }
            let method = r
                .get("method")
                .and_then(Value::as_str)
                .ok_or("method must be a string")?;
            let empty = json!({});
            let params = r.get("params").unwrap_or(&empty);
            if !params.is_object() {
                return Err("params must be an object".into());
            }
            dispatch(method, params)
        });
    let response = match result {
        Ok(value) => json!({"id": id, "result": value}),
        Err(error) => json!({"id": id, "error": error}),
    };
    let mut bytes = serde_json::to_vec(&response).expect("JSON values serialize");
    bytes.push(b'\n');
    bytes
}

pub fn integer(params: &Value, name: &str) -> Result<i32, String> {
    params
        .get(name)
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(|| format!("{name} must be a 32-bit integer"))
}

pub fn dimension(params: &Value, name: &str) -> Result<Option<i32>, String> {
    if params.get(name).is_none() {
        return Ok(None);
    }
    let n = integer(params, name)?;
    if n <= 0 {
        return Err(format!("{name} must be positive"));
    }
    Ok(Some(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_envelope_and_preserves_request_id() {
        for line in [
            r#"{"id":7,"method":false}"#,
            r#"{"id":7,"method":"screen","params":[]}"#,
        ] {
            let response: Value =
                serde_json::from_slice(&reply(line.as_bytes(), |_, _| panic!("invalid"))).unwrap();
            assert_eq!(response["id"], 7);
            assert!(response["error"].is_string());
        }
        let bytes = reply(br#"{"id":8,"method":"screen"}"#, |method, params| {
            assert_eq!(method, "screen");
            assert_eq!(params, &json!({}));
            Ok(json!({"windows": []}))
        });
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"id":8,"result":{"windows":[]}})
        );
        for line in ["{", "[]", r#"{"id":-1,"method":"screen"}"#] {
            let response: Value =
                serde_json::from_slice(&reply(line.as_bytes(), |_, _| panic!("invalid"))).unwrap();
            assert!(response["error"].is_string());
        }
    }

    #[test]
    fn rejects_invalid_geometry() {
        assert_eq!(integer(&json!({"x": -20}), "x"), Ok(-20));
        assert!(integer(&json!({"x": 2147483648_i64}), "x").is_err());
        assert!(integer(&json!({"x": 1.5}), "x").is_err());
        assert_eq!(dimension(&json!({}), "w"), Ok(None));
        assert_eq!(dimension(&json!({"w": 640}), "w"), Ok(Some(640)));
        assert!(dimension(&json!({"w": 0}), "w").is_err());
        assert!(dimension(&json!({"w": null}), "w").is_err());
    }
}
