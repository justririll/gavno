//! gavno-ai: the local AI helper of the GAVNO launcher (`design/ai.md`).
//!
//! A small session-bus service, `org.gavno.AI1`, that is D-Bus activated on the
//! first call and exits after `ai.idle_stop_minutes` without a request. It
//! starts and stops the `llama-gavno` systemd unit, talks plain HTTP to its
//! llama-server on 127.0.0.1, and turns the model's JSON into validated,
//! whitelisted actions. The compositor links none of this.
pub mod action;
pub mod config;
pub mod engine;
pub mod http;
pub mod prompt;
pub mod service;
pub mod unit;
