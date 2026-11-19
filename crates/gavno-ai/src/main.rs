//! `gavno-ai`: D-Bus activated, exits after the idle timeout (see `lib.rs`).
use gavno_ai::{
    config::AiConfig,
    engine::{Engine, Settings, UnitControl},
    service::{Ai, NAME, PATH},
    unit::{NoUnit, SystemdUnit, PORT, UNIT},
};
use std::{sync::Arc, time::Duration};

fn main() {
    let config = AiConfig::load();
    let unit: Box<dyn UnitControl> = match std::env::var("GAVNO_AI_UNIT") {
        Ok(name) if name.is_empty() => Box::new(NoUnit),
        Ok(name) => Box::new(SystemdUnit { name }),
        Err(_) => Box::new(SystemdUnit { name: UNIT.into() }),
    };
    let port = std::env::var("GAVNO_AI_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(PORT);
    let settings = Settings::new(port, Duration::from_secs(config.idle_stop_minutes * 60));
    let engine = Arc::new(Engine::new(unit, settings));
    let service = Ai { engine: engine.clone(), enabled: config.enabled };
    let connection = zbus::blocking::connection::Builder::session()
        .and_then(|b| b.name(NAME))
        .and_then(|b| b.serve_at(PATH, service))
        .and_then(|b| b.build());
    let _connection = match connection {
        Ok(connection) => connection,
        Err(error) => {
            eprintln!("gavno-ai: cannot own {NAME}: {error}");
            std::process::exit(1);
        }
    };
    engine.run_until_idle();
}
