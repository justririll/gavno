//! The rollback deadline is owned by a worker, independent of the GTK loop.
use futures_channel::oneshot;
use serde_json::{json, Value};
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

pub fn run(
    params: Value,
    ready: oneshot::Sender<Instant>,
    decisions: mpsc::Receiver<bool>,
) -> Result<bool, String> {
    preview(
        params,
        ready,
        decisions,
        Duration::from_secs(15),
        crate::ipc::call,
        crate::config::save,
    )
}
fn preview(
    params: Value,
    ready: oneshot::Sender<Instant>,
    decisions: mpsc::Receiver<bool>,
    deadline: Duration,
    mut call: impl FnMut(&str, &Value) -> Result<Value, String>,
    save: impl FnOnce(&str, &Value) -> Result<(), String>,
) -> Result<bool, String> {
    let outputs = call("outputs", &json!({}))?;
    let old = outputs
        .as_array()
        .and_then(|list| list.iter().find(|o| o["name"] == params["output"]))
        .ok_or("Display disconnected")?;
    let mut rollback = json!({"output":params["output"],"mode":{"w":old["mode"]["w"],"h":old["mode"]["h"],"refresh":old["mode"]["refresh"]},"scale":old["scale"]});
    if params.get("hdr").is_some() {
        rollback["hdr"] = old["hdr"]["pending"]
            .as_bool()
            .map(Value::Bool)
            .unwrap_or_else(|| old["hdr"]["enabled"].clone());
    }
    call("set_output", &params)?;
    let _ = ready.send(Instant::now());
    if decisions.recv_timeout(deadline).unwrap_or(false) {
        save("set_output", &params)
            .map_err(|e| format!("Settings applied, but saving failed: {e}"))?;
        Ok(true)
    } else {
        call("set_output", &rollback)
            .map_err(|e| format!("Could not restore display settings: {e}"))?;
        Ok(false)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    fn previous() -> Value {
        json!([{"name":"eDP-1","mode":{"w":2880,"h":1620,"refresh":120000},"scale":1.75,"hdr":{"enabled":false,"pending":null}}])
    }
    #[test]
    fn expiry_and_explicit_revert_restore_without_saving() {
        for decision in [None, Some(false)] {
            let (tx, rx) = mpsc::channel();
            if let Some(decision) = decision {
                tx.send(decision).unwrap();
            }
            let (ready, _prepared) = oneshot::channel();
            let mut calls = Vec::new();
            let result = preview(
                json!({"output":"eDP-1","scale":2.0,"hdr":true}),
                ready,
                rx,
                Duration::ZERO,
                |method, params| {
                    calls.push((method.to_owned(), params.clone()));
                    Ok(if method == "outputs" {
                        previous()
                    } else {
                        json!({})
                    })
                },
                |_, _| panic!("unconfirmed settings must not be saved"),
            )
            .unwrap();
            assert!(!result);
            assert_eq!(calls.len(), 3);
            assert_eq!(
                calls[2],
                (
                    "set_output".into(),
                    json!({"output":"eDP-1","mode":{"w":2880,"h":1620,"refresh":120000},"scale":1.75,"hdr":false})
                )
            );
        }
    }
    #[test]
    fn keep_saves_only_after_successful_apply() {
        let (tx, rx) = mpsc::channel();
        tx.send(true).unwrap();
        let (ready, _prepared) = oneshot::channel();
        let applied = Cell::new(false);
        let saved = Cell::new(false);
        let params = json!({"output":"eDP-1","scale":2.0});
        assert!(preview(
            params.clone(),
            ready,
            rx,
            Duration::ZERO,
            |method, _| {
                if method == "outputs" {
                    Ok(previous())
                } else {
                    applied.set(true);
                    Ok(json!({}))
                }
            },
            |method, p| {
                assert!(applied.get());
                assert_eq!(method, "set_output");
                assert_eq!(p, &params);
                saved.set(true);
                Ok(())
            }
        )
        .unwrap());
        assert!(saved.get());
    }
    #[test]
    fn failed_apply_never_saves_or_starts_confirmation() {
        let (_tx, rx) = mpsc::channel();
        let (ready, mut prepared) = oneshot::channel();
        assert!(preview(
            json!({"output":"eDP-1","scale":2.0}),
            ready,
            rx,
            Duration::ZERO,
            |method, _| if method == "outputs" {
                Ok(previous())
            } else {
                Err("modeset failed".into())
            },
            |_, _| panic!("failed settings must not be saved")
        )
        .is_err());
        assert!(prepared.try_recv().is_err());
    }
}
