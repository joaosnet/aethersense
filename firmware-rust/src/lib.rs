#[cfg(all(target_os = "espidf", not(feature = "esp-idf")))]
compile_error!("enable the esp-idf feature when building for target_os = \"espidf\"");

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub mod bootstrap;
pub mod captive_portal;
pub mod config;
pub mod csi;
pub mod ffi;
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub mod health;
pub mod ir;
pub mod mqtt;
pub mod profile;
pub mod provisioning;
pub mod relay;
pub mod rmt_tx;
pub mod runtime;
pub mod source;
pub mod status_led;
pub mod storage;
pub mod transport;
pub mod wifi;
