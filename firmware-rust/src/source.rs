#[cfg(all(not(feature = "esp-idf")))]
compile_error!("enable the esp-idf feature");

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use crate::bootstrap::BootstrapResources;
use crate::config::FirmwareConfig;
#[cfg(feature = "esp-idf")]
use crate::ffi::{CaptureProfile, EspIdfCallbackBinding, EspIdfSourceHandle};
use crate::runtime::PacketIngress;
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use crate::wifi::connect_wifi;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SourceStats {
    pub produced: usize,
    pub accepted: usize,
    pub dropped: usize,
}

pub struct SourceRuntime {
    pub stats: SourceStats,
    pub subnet_broadcast_ip: Option<std::net::Ipv4Addr>,
    #[cfg(feature = "esp-idf")]
    _handle: Option<EspIdfSourceHandle>,
}

#[cfg(not(feature = "esp-idf"))]
impl SourceRuntime {
    pub fn new(stats: SourceStats) -> Self {
        Self {
            stats,
            subnet_broadcast_ip: None,
        }
    }
}

#[cfg(feature = "esp-idf")]
impl SourceRuntime {
    pub fn new(stats: SourceStats) -> Self {
        Self {
            stats,
            subnet_broadcast_ip: None,
            _handle: None,
        }
    }

    pub fn with_handle(
        stats: SourceStats,
        handle: EspIdfSourceHandle,
        subnet_broadcast_ip: Option<std::net::Ipv4Addr>,
    ) -> Self {
        Self {
            stats,
            subnet_broadcast_ip,
            _handle: Some(handle),
        }
    }
}

impl SourceRuntime {
    /// True only while the runtime owns a live Wi-Fi/CSI source handle.
    /// Network consumers must not create sockets before this becomes true.
    pub fn is_ready(&self) -> bool {
        #[cfg(feature = "esp-idf")]
        {
            self._handle.is_some()
        }
        #[cfg(not(feature = "esp-idf"))]
        {
            false
        }
    }

    /// Pause/resume CSI without tearing down the live Wi-Fi connection.
    pub fn set_capture_enabled(&self, enabled: bool) -> Result<(), i32> {
        #[cfg(feature = "esp-idf")]
        {
            return self
                ._handle
                .as_ref()
                .ok_or(-1)
                .and_then(|handle| handle.set_capture_enabled(enabled));
        }
        #[cfg(not(feature = "esp-idf"))]
        {
            let _ = enabled;
            Err(-1)
        }
    }
}

pub fn run_selected_source(_config: &FirmwareConfig, _ingress: &PacketIngress) -> SourceRuntime {
    #[cfg(all(feature = "esp-idf", not(target_os = "espidf")))]
    {
        return run_esp_idf_source(_config, _ingress);
    }

    #[allow(unreachable_code)]
    SourceRuntime::new(SourceStats::default())
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub fn run_selected_source_with_resources(
    config: &FirmwareConfig,
    ingress: &PacketIngress,
    resources: BootstrapResources,
) -> SourceRuntime {
    run_esp_idf_source(config, ingress, resources)
}

#[cfg(feature = "esp-idf")]
#[cfg(not(target_os = "espidf"))]
pub fn run_esp_idf_source(config: &FirmwareConfig, ingress: &PacketIngress) -> SourceRuntime {
    let capture_profile = CaptureProfile::from_hardware_profile(&config.hardware_profile);
    let handle = EspIdfSourceHandle::new(
        ingress.clone(),
        capture_profile
            .capture_config()
            .apply_band(&config.wifi_band),
    );
    let binding: EspIdfCallbackBinding = handle.binding();
    debug_assert!(!binding.context.is_null());
    SourceRuntime::with_handle(SourceStats::default(), handle, None)
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub fn run_esp_idf_source(
    config: &FirmwareConfig,
    ingress: &PacketIngress,
    resources: BootstrapResources,
) -> SourceRuntime {
    let capture_profile = CaptureProfile::from_hardware_profile(&config.hardware_profile);
    let wifi_connection = match connect_wifi(
        config,
        resources.modem,
        resources.system_event_loop,
        resources.default_nvs,
    ) {
        Ok(connection) => connection,
        Err(error) => {
            // A network/setup failure must not turn into a reboot loop. The
            // captive portal normally blocks on success; if it cannot start,
            // keep the firmware/provisioning service alive without CSI.
            log::error!("Wi-Fi/CSI source unavailable: {error}");
            return SourceRuntime::new(SourceStats::default());
        }
    };

    if let Some(gw_ip) = wifi_connection.gateway_ip() {
        let ping_interval = core::time::Duration::from_millis(config.send_interval_millis());
        let ping_timeout = core::cmp::max(ping_interval, core::time::Duration::from_millis(100));
        let udp_probe_interval =
            core::time::Duration::from_millis((config.send_interval_millis() * 5).max(500));
        let use_udp_probe = capture_profile == CaptureProfile::Esp32C5Like;

        let probe_worker = std::thread::Builder::new()
            .name("aether-csi-probe".to_string())
            .stack_size(8 * 1024)
            .spawn(move || {
            if use_udp_probe {
                let probe_addr = std::net::SocketAddr::V4(std::net::SocketAddrV4::new(gw_ip, 53));
                let dns_probe = [
                    0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x01, 0x00, 0x01,
                ];
                // Se o bind falhar, NÃO mata a thread: cai no ping ICMP abaixo como
                // fallback. Sem isso, a thread morreria, o tráfego (que induz CSI)
                // pararia, e o monitor de saúde reiniciaria o device em loop.
                match std::net::UdpSocket::bind("0.0.0.0:0") {
                    Ok(socket) => {
                        let _ =
                            socket.set_read_timeout(Some(core::time::Duration::from_millis(20)));
                        let mut response = [0_u8; 96];

                        loop {
                            let _ = socket.send_to(&dns_probe, probe_addr);
                            let _ = socket.recv_from(&mut response);
                            std::thread::sleep(udp_probe_interval);
                        }
                    }
                    Err(err) => {
                        log::warn!(
                            "warning: background UDP probe socket failed ({}); falling back to ICMP ping",
                            err
                        );
                    }
                }
            }

            let mut ping = esp_idf_svc::ping::EspPing::new(0);
            let mut ping_config = esp_idf_svc::ping::Configuration::default();
            ping_config.count = u32::MAX;
            ping_config.interval = ping_interval;
            ping_config.timeout = ping_timeout;

            loop {
                if let Err(err) = ping.ping(gw_ip, &ping_config) {
                    log::warn!("warning: background ping traffic failed: {}", err);
                    std::thread::sleep(core::time::Duration::from_secs(1));
                }
            }
            });
        if let Err(error) = probe_worker {
            log::warn!("background CSI traffic worker could not start: {error}");
        }
    } else {
        log::warn!("Warning: No gateway IP found, cannot start background ping traffic. CSI capture might be empty.");
    }

    let subnet_broadcast_ip = wifi_connection.subnet_broadcast_ip();
    let handle = EspIdfSourceHandle::new(
        ingress.clone(),
        capture_profile
            .capture_config()
            .apply_band(&config.wifi_band),
        wifi_connection,
    );
    let binding: EspIdfCallbackBinding = handle.binding();
    debug_assert!(!binding.context.is_null());
    SourceRuntime::with_handle(SourceStats::default(), handle, subnet_broadcast_ip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::packet_bus;

    #[cfg(feature = "esp-idf")]
    #[test]
    fn esp_idf_source_uses_capture_profile_from_hardware_profile() {
        let mut config = FirmwareConfig::default();
        config.hardware_profile = "esp32c6".to_string();
        let (ingress, _egress) = packet_bus(config.queue_capacity);

        let runtime = run_esp_idf_source(&config, &ingress);

        assert_eq!(runtime.stats, SourceStats::default());
        assert!(runtime.is_ready());
    }

    #[test]
    fn runtime_without_source_handle_is_not_network_ready() {
        let runtime = SourceRuntime::new(SourceStats::default());

        assert!(!runtime.is_ready());
    }
}
