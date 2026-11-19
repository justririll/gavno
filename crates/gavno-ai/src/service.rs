//! The `org.gavno.AI1` D-Bus interface. Blocking work runs on its own thread
//! per call, so a slow `Interpret` never delays `Prewarm` or `Status`.
use crate::engine::Engine;
use std::{collections::HashMap, sync::Arc};
use zbus::{fdo, interface, zvariant::OwnedValue};

pub const NAME: &str = "org.gavno.AI1";
pub const PATH: &str = "/org/gavno/AI1";

pub struct Ai {
    pub engine: Arc<Engine>,
    pub enabled: bool,
}

/// Runs `f` on a new thread and awaits it without blocking the bus executor.
async fn offload<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(f());
    });
    rx.recv().await.expect("worker thread dropped its result")
}

fn reply(result: Result<serde_json::Value, String>) -> String {
    match result {
        Ok(value) => value.to_string(),
        Err(error) => serde_json::json!({"error": error}).to_string(),
    }
}

fn disabled() -> String {
    reply(Err("local AI is disabled (set ai.enabled = true)".into()))
}

#[interface(name = "org.gavno.AI1")]
impl Ai {
    /// Starts the model unit if needed and returns at once. `context` is the
    /// same JSON as for `Interpret` (or empty); its stable part is evaluated
    /// in the background so the first request is fast.
    async fn prewarm(&self, context: String) -> fdo::Result<()> {
        if self.enabled {
            self.engine.prewarm(&context).map_err(fdo::Error::Failed)?;
        }
        Ok(())
    }

    /// `context` and the result are JSON; failures are `{"error": "..."}`.
    async fn interpret(&self, query: String, context: String) -> String {
        if !self.enabled {
            return disabled();
        }
        let engine = self.engine.clone();
        reply(offload(move || engine.interpret(&query, &context)).await)
    }

    async fn name_workspace(&self, windows: String) -> String {
        if !self.enabled {
            return disabled();
        }
        let engine = self.engine.clone();
        reply(offload(move || engine.name_workspace(&windows)).await)
    }

    async fn status(&self) -> HashMap<String, OwnedValue> {
        let engine = self.engine.clone();
        let (running, latency) = offload(move || engine.status()).await;
        let mut map = HashMap::new();
        map.insert("enabled".to_owned(), OwnedValue::from(self.enabled));
        map.insert("running".to_owned(), OwnedValue::from(running));
        map.insert("backend".to_owned(), OwnedValue::try_from(zbus::zvariant::Value::from("cpu")).unwrap());
        map.insert("model".to_owned(), OwnedValue::try_from(zbus::zvariant::Value::from("qwen3-4b-instruct-2507-q4_0")).unwrap());
        map.insert("last_latency_ms".to_owned(), OwnedValue::from(latency));
        map
    }
}
