#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use core::convert::TryInto;

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use crate::config::FirmwareConfig;
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use crate::ffi::format_mac;
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use esp_idf_hal::modem::Modem;
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use esp_idf_svc::eventloop::EspSystemEventLoop;
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs};
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use esp_idf_svc::sys::{self, esp_wifi_set_ps, EspError, ESP_FAIL};
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use esp_idf_svc::wifi::{
    AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi, PmfConfiguration,
    ScanMethod, WifiEvent,
};
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use std::net::Ipv4Addr;
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use std::time::{Duration, Instant};

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
const WIFI_CHANNEL_CACHE_KEY: &str = "wifi_channel";

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
const ASSOCIATION_PENDING: u32 = 0;
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
const ASSOCIATION_CONNECTED: u32 = 1;
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
const ASSOCIATION_DISCONNECTED: u32 = 1 << 31;

#[cfg(any(test, all(feature = "esp-idf", target_os = "espidf")))]
fn cached_channel_for_band(band: &str, cached: Option<u8>) -> Option<u8> {
    let channel = cached?;
    let band = band.trim().to_ascii_lowercase();
    let valid = match band.as_str() {
        "2g" | "2.4g" | "2.4ghz" => (1..=14).contains(&channel),
        "5g" | "5ghz" => (36..=177).contains(&channel),
        "" | "auto" => (1..=14).contains(&channel) || (36..=177).contains(&channel),
        _ => false,
    };
    valid.then_some(channel)
}

#[cfg(any(test, all(feature = "esp-idf", target_os = "espidf")))]
fn strongest_matching_channel<'a, I>(target_ssid: &str, candidates: I) -> Option<(u8, i8)>
where
    I: IntoIterator<Item = (&'a str, u8, i8)>,
{
    candidates
        .into_iter()
        .filter(|(ssid, _, _)| *ssid == target_ssid)
        .max_by_key(|(_, _, rssi)| *rssi)
        .map(|(_, channel, rssi)| (channel, rssi))
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub struct WifiConnection {
    _wifi: EspWifi<'static>,
    ap_bssid: [u8; 6],
    station_mac: [u8; 6],
    gateway_ip: Option<std::net::Ipv4Addr>,
    subnet_broadcast_ip: Option<std::net::Ipv4Addr>,
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
impl WifiConnection {
    pub fn ap_bssid(&self) -> [u8; 6] {
        self.ap_bssid
    }

    pub fn station_mac(&self) -> [u8; 6] {
        self.station_mac
    }

    pub fn gateway_ip(&self) -> Option<std::net::Ipv4Addr> {
        self.gateway_ip
    }

    pub fn subnet_broadcast_ip(&self) -> Option<std::net::Ipv4Addr> {
        self.subnet_broadcast_ip
    }
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub fn connect_wifi(
    config: &FirmwareConfig,
    modem: Modem<'static>,
    sys_loop: EspSystemEventLoop,
    nvs: EspDefaultNvsPartition,
) -> Result<WifiConnection, EspError> {
    let wifi_started_at = Instant::now();
    let mut final_ssid = config.wifi_ssid.clone();
    let mut final_password = config.wifi_password.clone();
    let mut cached_channel = None;
    // Distingue credenciais REAIS (salvas pelo usuário na NVS via captive portal)
    // do SSID embutido no sdkconfig (fallback de build). Só credenciais da NVS
    // habilitam o timeout do portal; um device fresco com SSID embutido fora da
    // rede deve abrir um portal ESTÁVEL para o usuário configurar (ver abaixo).
    let mut creds_from_nvs = false;

    if let Ok(store) = EspNvs::new(nvs.clone(), "aethersense", true) {
        let mut buf = [0u8; 128];
        if let Ok(Some(s)) = store.get_str("ssid", &mut buf) {
            if !s.trim().is_empty() {
                final_ssid = s.to_string();
                creds_from_nvs = true;
            }
        }
        if let Ok(Some(p)) = store.get_str("password", &mut buf) {
            final_password = p.to_string();
        }
        cached_channel = store.get_u8(WIFI_CHANNEL_CACHE_KEY).ok().flatten();
    }

    let channel_hint = cached_channel_for_band(&config.wifi_band, cached_channel);
    if cached_channel.is_some() && channel_hint.is_none() {
        log::warn!(
            "wifi ignored cached channel {:?}: incompatible with band {}",
            cached_channel,
            config.wifi_band
        );
    }

    let ssid = final_ssid.trim();
    let password = final_password.trim();

    let mut esp_wifi = EspWifi::new(modem, sys_loop.clone(), Some(nvs.clone()))?;
    let association_state = Arc::new(AtomicU32::new(ASSOCIATION_PENDING));
    let association_event_state = association_state.clone();
    let _association_subscription =
        sys_loop.subscribe::<WifiEvent, _>(move |event: WifiEvent| match event {
            WifiEvent::StaConnected(_) => {
                association_event_state.store(ASSOCIATION_CONNECTED, Ordering::Release);
            }
            WifiEvent::StaDisconnected(details) => {
                let encoded_rssi = (i16::from(details.rssi()) + 128).clamp(0, 255) as u32;
                let encoded =
                    ASSOCIATION_DISCONNECTED | (u32::from(details.reason()) << 8) | encoded_rssi;
                association_event_state.store(encoded, Ordering::Release);
            }
            _ => {}
        })?;

    let wifi_details = {
        let mut wifi = BlockingWifi::wrap(&mut esp_wifi, sys_loop)?;

        if ssid.is_empty() {
            // Sem credenciais: setup inicial legítimo, aguarda indefinidamente.
            log::warn!("No SSID configured. Starting Captive Portal...");
            crate::captive_portal::start_captive_portal(wifi, nvs, None)?;
            return Err(EspError::from_infallible::<ESP_FAIL>());
        }

        let wifi_configuration = Configuration::Client(ClientConfiguration {
            ssid: ssid.try_into().unwrap(),
            bssid: None,
            auth_method: if password.is_empty() {
                AuthMethod::None
            } else {
                AuthMethod::WPA2Personal
            },
            password: password.try_into().unwrap(),
            channel: channel_hint,
            scan_method: ScanMethod::FastScan,
            pmf_cfg: PmfConfiguration::Capable { required: false },
            ..Default::default()
        });

        wifi.set_configuration(&wifi_configuration)?;

        log::warn!("wifi started");
        wifi.start()?;
        configure_wifi_band_mode(config);
        disable_wifi_power_save();
        configure_wifi_phy(config);

        // No primeiro boot não há canal NVS. Nesse caso, uma varredura explícita
        // escolhe o rádio de mesmo SSID com melhor RSSI sem prender a configuração
        // a um BSSID. Boots seguintes usam diretamente o canal cacheado.
        let mut selected_channel = channel_hint;
        if selected_channel.is_none() {
            let scan_started_at = Instant::now();
            match wifi.scan_n::<32>() {
                Ok((access_points, found_count)) => {
                    let candidate = strongest_matching_channel(
                        ssid,
                        access_points
                            .iter()
                            .map(|ap| (ap.ssid.as_str(), ap.channel, ap.signal_strength)),
                    )
                    .and_then(|(channel, rssi)| {
                        cached_channel_for_band(&config.wifi_band, Some(channel))
                            .map(|valid_channel| (valid_channel, rssi))
                    });

                    if let Some((channel, rssi)) = candidate {
                        selected_channel = Some(channel);
                        log::warn!(
                            "wifi scan completed in {} ms: visible={} selected channel={} rssi={} dBm",
                            scan_started_at.elapsed().as_millis(),
                            found_count,
                            channel,
                            rssi
                        );
                    } else {
                        log::warn!(
                            "wifi scan completed in {} ms: visible={} target SSID not found; using cached hint {:?}",
                            scan_started_at.elapsed().as_millis(),
                            found_count,
                            selected_channel
                        );
                    }
                }
                Err(error) => log::warn!(
                    "wifi scan failed after {} ms: {}; using cached hint {:?}",
                    scan_started_at.elapsed().as_millis(),
                    error,
                    selected_channel
                ),
            }

            wifi.set_configuration(&Configuration::Client(ClientConfiguration {
                ssid: ssid.try_into().unwrap(),
                bssid: None,
                auth_method: if password.is_empty() {
                    AuthMethod::None
                } else {
                    AuthMethod::WPA2Personal
                },
                password: password.try_into().unwrap(),
                channel: selected_channel,
                scan_method: ScanMethod::FastScan,
                pmf_cfg: PmfConfiguration::Capable { required: false },
                ..Default::default()
            }))?;
        }

        let mut retries = 0;
        loop {
            let attempt = retries + 1;
            let attempt_started_at = Instant::now();
            log::warn!(
                "wifi association attempt {}/{}: ssid={} scan=fast channel_hint={:?}",
                attempt,
                config.wifi_max_retry,
                ssid,
                selected_channel
            );
            association_state.store(ASSOCIATION_PENDING, Ordering::Release);
            let association_failure = match wifi.wifi_mut().connect() {
                Err(error) => Some(format!("connect call failed: {error}")),
                Ok(()) => loop {
                    let state = association_state.load(Ordering::Acquire);
                    if state == ASSOCIATION_CONNECTED {
                        break None;
                    }
                    if state & ASSOCIATION_DISCONNECTED != 0 {
                        let reason = (state >> 8) & 0xffff;
                        let rssi = (state & 0xff) as i16 - 128;
                        break Some(format!(
                            "driver disconnected: reason={reason} rssi={rssi} dBm"
                        ));
                    }
                    if attempt_started_at.elapsed() >= Duration::from_secs(5) {
                        let _ = wifi.wifi_mut().disconnect();
                        break Some("local 5000 ms association deadline reached".to_string());
                    }
                    std::thread::sleep(Duration::from_millis(25));
                },
            };

            if let Some(failure) = association_failure {
                retries += 1;
                log::warn!(
                    "wifi association failed (attempt {}/{} after {} ms): {}",
                    retries,
                    config.wifi_max_retry,
                    attempt_started_at.elapsed().as_millis(),
                    failure
                );
                if retries >= config.wifi_max_retry {
                    // Só usa timeout (re-tenta a rede salva sozinho, sem ficar preso até
                    // power-cycle) quando as credenciais vieram da NVS. Se for só o SSID
                    // embutido do sdkconfig (device fresco em outra rede), abre um portal
                    // ESTÁVEL (None) para o usuário configurar sem ser reiniciado.
                    let portal_timeout = if creds_from_nvs {
                        Some(config.captive_portal_timeout_secs)
                    } else {
                        None
                    };
                    log::warn!(
                        "max retries reached. starting captive portal (timeout={:?}).",
                        portal_timeout
                    );
                    crate::captive_portal::start_captive_portal(wifi, nvs, portal_timeout)?;
                    return Err(EspError::from_infallible::<ESP_FAIL>());
                }
                let radio_restart_started_at = Instant::now();
                wifi.stop()?;
                wifi.start()?;
                configure_wifi_band_mode(config);
                disable_wifi_power_save();
                configure_wifi_phy(config);
                log::warn!(
                    "wifi radio restarted for retry in {} ms",
                    radio_restart_started_at.elapsed().as_millis()
                );
                std::thread::sleep(Duration::from_secs(2));
            } else {
                log::warn!(
                    "wifi associated on attempt {} after {} ms",
                    attempt,
                    attempt_started_at.elapsed().as_millis()
                );
                break;
            }
        }

        let dhcp_started_at = Instant::now();
        log::warn!("wifi associated; waiting for DHCP...");
        wifi.wait_netif_up()?;
        let (gateway_ip, subnet_broadcast_ip) = station_ip_info(&mut wifi)?;
        log::warn!(
            "wifi DHCP ready after {} ms",
            dhcp_started_at.elapsed().as_millis()
        );
        let station_mac = read_station_mac()?;

        let ap_info = wifi.wifi().get_ap_info()?;
        if let Ok(store) = EspNvs::new(nvs.clone(), "aethersense", true) {
            if let Err(error) = store.set_u8(WIFI_CHANNEL_CACHE_KEY, ap_info.channel) {
                log::warn!("wifi could not cache AP channel: {}", error);
            }
        }
        log::warn!(
            "wifi connected to {} ({}) channel={} rssi={} dBm total={} ms",
            ssid,
            format_mac(&ap_info.bssid),
            ap_info.channel,
            ap_info.signal_strength,
            wifi_started_at.elapsed().as_millis()
        );
        log::warn!("wifi station identity {}", format_mac(&station_mac));
        (ap_info.bssid, station_mac, gateway_ip, subnet_broadcast_ip)
    };

    Ok(WifiConnection {
        _wifi: esp_wifi,
        ap_bssid: wifi_details.0,
        station_mac: wifi_details.1,
        gateway_ip: wifi_details.2,
        subnet_broadcast_ip: wifi_details.3,
    })
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
fn disable_wifi_power_save() {
    unsafe {
        let result = esp_wifi_set_ps(0);
        if result != 0 {
            log::warn!(
                "warning: esp_wifi_set_ps(WIFI_PS_NONE) failed with code {}",
                result
            );
        }
    }
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
fn read_station_mac() -> Result<[u8; 6], EspError> {
    let mut mac = [0u8; 6];
    unsafe {
        let result = sys::esp_wifi_get_mac(
            sys::wifi_interface_t_WIFI_IF_STA,
            mac.as_mut_ptr() as *mut _,
        );
        if result != 0 {
            return Err(EspError::from(result).expect("esp_wifi_get_mac returned an ESP error"));
        }
    }
    Ok(mac)
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
fn station_ip_info(
    wifi: &mut BlockingWifi<&mut EspWifi<'static>>,
) -> Result<(Option<Ipv4Addr>, Option<Ipv4Addr>), EspError> {
    let info = wifi.wifi().sta_netif().get_ip_info()?;
    if info.ip == Ipv4Addr::UNSPECIFIED {
        log::warn!("wifi netif reported ready without a DHCP lease");
        return Err(EspError::from_infallible::<ESP_FAIL>());
    }

    let ip_u32 = u32::from_be_bytes(info.ip.octets());
    let netmask_ip: Ipv4Addr = info.subnet.mask.into();
    let netmask_u32 = u32::from_be_bytes(netmask_ip.octets());
    let broadcast_u32 = ip_u32 | !netmask_u32;

    log::warn!("wifi got IP address {}", info.ip);
    Ok((
        Some(info.subnet.gateway),
        Some(Ipv4Addr::from(broadcast_u32.to_be_bytes())),
    ))
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
fn configure_wifi_phy(config: &FirmwareConfig) {
    let prefer_ht40 = config.wifi_prefer_ht40;
    let bandwidth = if prefer_ht40 {
        sys::wifi_bandwidth_t_WIFI_BW_HT40
    } else {
        sys::wifi_bandwidth_t_WIFI_BW_HT20
    };

    unsafe {
        let result = sys::esp_wifi_set_bandwidth(sys::wifi_interface_t_WIFI_IF_STA, bandwidth);
        if result != 0 {
            log::warn!(
                "warning: esp_wifi_set_bandwidth({}) failed with code {}",
                if prefer_ht40 { "HT40" } else { "HT20" },
                result
            );
        }
    }

    match config.wifi_band.trim().to_ascii_lowercase().as_str() {
        "auto" | "" => {}
        "2g" | "2.4g" | "2.4ghz" => {
            log::warn!("wifi band preference: 2.4 GHz (selected by SSID/AP negotiation)");
        }
        "5g" | "5ghz" => {
            log::warn!("wifi band preference: 5 GHz (selected by SSID/AP negotiation)");
        }
        other => log::warn!(
            "warning: unknown AETHERSENSE_WIFI_BAND '{}', using auto",
            other
        ),
    }
}

#[cfg(all(
    feature = "esp-idf",
    target_os = "espidf",
    not(any(aethersense_esp32_classic, target_arch = "xtensa"))
))]
fn configure_wifi_band_mode(config: &FirmwareConfig) {
    if !is_multiband_profile(&config.hardware_profile) {
        return;
    }

    let band = config.wifi_band.trim().to_ascii_lowercase();
    let (mode, label) = match band.as_str() {
        "" | "auto" | "2g" | "2.4g" | "2.4ghz" => {
            (sys::wifi_band_mode_t_WIFI_BAND_MODE_2G_ONLY, "2.4 GHz only")
        }
        "5g" | "5ghz" => (sys::wifi_band_mode_t_WIFI_BAND_MODE_5G_ONLY, "5 GHz only"),
        other => {
            log::warn!(
                "warning: unknown AETHERSENSE_WIFI_BAND '{}', using 2.4 GHz only for CSI",
                other
            );
            (sys::wifi_band_mode_t_WIFI_BAND_MODE_2G_ONLY, "2.4 GHz only")
        }
    };

    unsafe {
        let result = sys::esp_wifi_set_band_mode(mode);
        if result == 0 {
            log::warn!("wifi band mode: {}", label);
        } else {
            log::warn!(
                "warning: esp_wifi_set_band_mode({}) failed with code {}",
                label,
                result
            );
        }
    }
}

#[cfg(all(
    feature = "esp-idf",
    target_os = "espidf",
    any(aethersense_esp32_classic, target_arch = "xtensa")
))]
fn configure_wifi_band_mode(_config: &FirmwareConfig) {}

#[cfg(all(
    feature = "esp-idf",
    target_os = "espidf",
    not(any(aethersense_esp32_classic, target_arch = "xtensa"))
))]
fn is_multiband_profile(profile: &str) -> bool {
    matches!(
        profile.trim().to_ascii_lowercase().as_str(),
        "c5" | "esp32c5" | "c6" | "esp32c6" | "c61" | "esp32c61"
    )
}

#[cfg(test)]
mod tests {
    use super::{cached_channel_for_band, strongest_matching_channel};

    #[test]
    fn cached_channel_is_validated_against_selected_band() {
        assert_eq!(cached_channel_for_band("5ghz", Some(36)), Some(36));
        assert_eq!(cached_channel_for_band("5g", Some(177)), Some(177));
        assert_eq!(cached_channel_for_band("5ghz", Some(6)), None);
        assert_eq!(cached_channel_for_band("2.4ghz", Some(6)), Some(6));
        assert_eq!(cached_channel_for_band("2g", Some(36)), None);
    }

    #[test]
    fn auto_accepts_supported_channels_and_missing_cache_stays_empty() {
        assert_eq!(cached_channel_for_band("auto", Some(11)), Some(11));
        assert_eq!(cached_channel_for_band("auto", Some(149)), Some(149));
        assert_eq!(cached_channel_for_band("auto", Some(20)), None);
        assert_eq!(cached_channel_for_band("5ghz", None), None);
    }

    #[test]
    fn unknown_band_rejects_cached_channel() {
        assert_eq!(cached_channel_for_band("future-band", Some(36)), None);
    }

    #[test]
    fn strongest_candidate_uses_exact_ssid_and_best_rssi() {
        let candidates = [
            ("other", 36, -30),
            ("target", 44, -67),
            ("target", 149, -48),
            ("target-guest", 40, -20),
        ];

        assert_eq!(
            strongest_matching_channel("target", candidates),
            Some((149, -48))
        );
    }

    #[test]
    fn strongest_candidate_returns_none_when_ssid_is_absent() {
        let candidates = [("other", 36, -30), ("guest", 44, -45)];

        assert_eq!(strongest_matching_channel("target", candidates), None);
    }
}
