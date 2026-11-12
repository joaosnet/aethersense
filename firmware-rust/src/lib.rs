#[cfg(all(target_os = "espidf", not(feature = "esp-idf")))]
compile_error!("enable the esp-idf feature when building for target_os = \"espidf\"");

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub mod bootstrap;
pub mod auth;
pub mod broker_discovery;
pub mod captive_portal;
pub mod config;
pub mod controls;
pub mod csi;
pub mod ffi;
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub mod health;
pub mod ir;
pub mod mdns_responder;
pub mod mondial_ir_codes;
pub mod mqtt;
pub mod ota;
pub mod profile;
pub mod provisioning;
pub mod relay;
pub mod rmt_tx;
pub mod runtime;
pub mod scheduler;
pub mod source;
pub mod status_led;
pub mod storage;
pub mod transport;
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub mod web_server;
pub mod wifi;
