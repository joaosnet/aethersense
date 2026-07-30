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
use esp_idf_svc::wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi};
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use std::net::Ipv4Addr;
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use std::time::{Duration, Instant};

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
const IP_READY_TIMEOUT_SECS: u64 = 20;
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
const IP_READY_POLL_INTERVAL_MS: u64 = 200;

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
    let mut final_ssid = config.wifi_ssid.clone();
    let mut final_password = config.wifi_password.clone();
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
    }

    let ssid = final_ssid.trim();
    let password = final_password.trim();

    let mut esp_wifi = EspWifi::new(modem, sys_loop.clone(), Some(nvs.clone()))?;

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
            channel: None,
            ..Default::default()
        });

        wifi.set_configuration(&wifi_configuration)?;

        log::warn!("wifi started");
        wifi.start()?;
        configure_wifi_band_mode(config);
        disable_wifi_power_save();
        configure_wifi_phy(config);

        log::warn!("wifi connecting to {}", ssid);

        let mut retries = 0;
        while let Err(e) = wifi.connect() {
            retries += 1;
            log::warn!(
                "wifi connection failed (attempt {}/{}): {}",
                retries,
                config.wifi_max_retry,
                e
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
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        log::warn!("wifi connected! waiting for IP...");

        match wifi.wait_netif_up() {
            Ok(_) => log::warn!("wifi netif up"),
            Err(e) => log::warn!("wifi wait_netif_up failed: {}", e),
        }

        let (gateway_ip, subnet_broadcast_ip) = wait_for_station_ip_info(&mut wifi)?;
        let station_mac = read_station_mac()?;

        let ap_info = wifi.wifi().get_ap_info()?;
        log::warn!(
            "wifi connected to {} ({})",
            ssid,
            format_mac(&ap_info.bssid)
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
fn wait_for_station_ip_info(
    wifi: &mut BlockingWifi<&mut EspWifi<'static>>,
) -> Result<(Option<Ipv4Addr>, Option<Ipv4Addr>), EspError> {
    let deadline = Instant::now() + Duration::from_secs(IP_READY_TIMEOUT_SECS);

    loop {
        if let Ok(info) = wifi.wifi().sta_netif().get_ip_info() {
            if info.ip != Ipv4Addr::UNSPECIFIED {
                let ip_u32 = u32::from_be_bytes(info.ip.octets());
                let netmask_ip: Ipv4Addr = info.subnet.mask.into();
                let netmask_u32 = u32::from_be_bytes(netmask_ip.octets());
                let broadcast_u32 = ip_u32 | !netmask_u32;

                log::warn!("wifi got IP address {}", info.ip);
                return Ok((
                    Some(info.subnet.gateway),
                    Some(Ipv4Addr::from(broadcast_u32.to_be_bytes())),
                ));
            }
        }

        if Instant::now() >= deadline {
            log::warn!("wifi timed out waiting for a DHCP lease");
            return Err(EspError::from_infallible::<ESP_FAIL>());
        }

        std::thread::sleep(Duration::from_millis(IP_READY_POLL_INTERVAL_MS));
    }
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
