#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use crate::runtime::monotonic_secs;
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use esp_idf_svc::http::server::{Configuration, EspHttpServer};
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use esp_idf_svc::io::{EspIOError, Write};
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs};
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use esp_idf_svc::sys::{
    esp_restart, esp_wifi_set_band_mode, wifi_band_mode_t_WIFI_BAND_MODE_2G_ONLY, EspError,
    ESP_ERR_WIFI_NOT_STARTED, ESP_FAIL,
};
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use esp_idf_svc::wifi::{
    AccessPointConfiguration, BlockingWifi, ClientConfiguration, Configuration as WifiConfiguration,
};
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use std::sync::atomic::{AtomicU32, Ordering};
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use std::sync::Arc;

#[cfg(any(test, all(feature = "esp-idf", target_os = "espidf")))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PortalWifiStep {
    ConfigureTemporaryStation,
    StartForBandSelection,
    Select2gBand,
    StopForApConfiguration,
    ConfigureAccessPoint,
    StartAccessPoint,
}

#[cfg(any(test, all(feature = "esp-idf", target_os = "espidf")))]
const PORTAL_PLAN_FROM_STOPPED: &[PortalWifiStep] = &[
    PortalWifiStep::ConfigureTemporaryStation,
    PortalWifiStep::StartForBandSelection,
    PortalWifiStep::Select2gBand,
    PortalWifiStep::StopForApConfiguration,
    PortalWifiStep::ConfigureAccessPoint,
    PortalWifiStep::StartAccessPoint,
];

#[cfg(any(test, all(feature = "esp-idf", target_os = "espidf")))]
const PORTAL_PLAN_FROM_STARTED: &[PortalWifiStep] = &[
    PortalWifiStep::Select2gBand,
    PortalWifiStep::StopForApConfiguration,
    PortalWifiStep::ConfigureAccessPoint,
    PortalWifiStep::StartAccessPoint,
];

#[cfg(any(test, all(feature = "esp-idf", target_os = "espidf")))]
fn portal_wifi_plan(already_started: bool) -> &'static [PortalWifiStep] {
    if already_started {
        PORTAL_PLAN_FROM_STARTED
    } else {
        PORTAL_PLAN_FROM_STOPPED
    }
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
fn decode_url(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(b) = u8::from_str_radix(&hex, 16) {
                out.push(b as char);
            }
        } else if c == '+' {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
fn esp_error_or_fail(code: i32) -> EspError {
    EspError::from(code).unwrap_or_else(|| EspError::from_infallible::<ESP_FAIL>())
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
fn select_portal_2g_band(
    wifi: &mut BlockingWifi<&mut esp_idf_svc::wifi::EspWifi<'static>>,
) -> Result<(), EspError> {
    let mut result = unsafe { esp_wifi_set_band_mode(wifi_band_mode_t_WIFI_BAND_MODE_2G_ONLY) };

    // A stale wrapper status must not turn a recoverable driver state into an
    // app_main panic. ESP-IDF requires esp_wifi_start() before band selection.
    if result == ESP_ERR_WIFI_NOT_STARTED {
        log::warn!("Captive Portal band selection found Wi-Fi stopped; starting and retrying once");
        wifi.start()?;
        result = unsafe { esp_wifi_set_band_mode(wifi_band_mode_t_WIFI_BAND_MODE_2G_ONLY) };
    }

    if result != 0 {
        log::error!(
            "Captive Portal could not select 2.4 GHz band (error {})",
            result
        );
        return Err(esp_error_or_fail(result));
    }

    log::warn!("Captive Portal band mode: 2.4 GHz only");
    Ok(())
}

/// Inicia o AP de configuração `AetherSense-Config` (192.168.71.1) e bloqueia.
///
/// `timeout_secs`:
/// - `None`: aguarda indefinidamente (use quando NÃO há credenciais salvas — é o
///   setup inicial legítimo, que exige intervenção do usuário).
/// - `Some(s)`: agenda um `esp_restart()` após `s` segundos se ninguém configurar.
///   Use quando JÁ existem credenciais mas a AP está indisponível: assim o device
///   re-tenta a rede salva sozinho em vez de ficar preso até o power-cycle (o
///   salvamento de novas credenciais também reinicia, então a janela cobre uma
///   senha trocada/errada).
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub fn start_captive_portal(
    mut wifi: BlockingWifi<&mut esp_idf_svc::wifi::EspWifi<'static>>,
    nvs_partition: EspDefaultNvsPartition,
    timeout_secs: Option<u32>,
) -> Result<(), EspError> {
    let ap_config = WifiConfiguration::AccessPoint(AccessPointConfiguration {
        ssid: "AetherSense-Config".try_into().unwrap(),
        password: "".try_into().unwrap(),
        channel: 6,
        ..Default::default()
    });

    // ESP-IDF 5.5 requires esp_wifi_start() before esp_wifi_set_band_mode().
    // A fresh device therefore starts a channel-independent temporary STA,
    // selects 2.4 GHz, stops, and only then applies AP channel 6. If STA was
    // already running (including a failed 5 GHz profile), band selection can
    // happen immediately. The portal blocks until reboot, so the provisioned
    // STA band preference is preserved and reapplied on the next boot.
    let already_started = wifi.is_started()?;
    for step in portal_wifi_plan(already_started) {
        match step {
            PortalWifiStep::ConfigureTemporaryStation => {
                wifi.set_configuration(&WifiConfiguration::Client(ClientConfiguration::default()))?
            }
            PortalWifiStep::StartForBandSelection => wifi.start()?,
            PortalWifiStep::Select2gBand => select_portal_2g_band(&mut wifi)?,
            PortalWifiStep::StopForApConfiguration => wifi.stop()?,
            PortalWifiStep::ConfigureAccessPoint => wifi.set_configuration(&ap_config)?,
            PortalWifiStep::StartAccessPoint => wifi.start()?,
        }
    }
    log::warn!("Captive Portal AP started: AetherSense-Config");
    wifi.wait_netif_up()?;

    std::thread::spawn(|| {
        if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:53") {
            let mut buf = [0u8; 512];
            while let Ok((size, src)) = socket.recv_from(&mut buf) {
                if size >= 12 {
                    let mut reply = buf[..size].to_vec();
                    reply[2] |= 0x80;
                    reply[3] = 0x00;
                    reply[7] = 1;
                    let answer: [u8; 16] = [
                        0xc0, 0x0c, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x3c, 0x00, 0x04,
                        192, 168, 71, 1,
                    ];
                    reply.extend_from_slice(&answer);
                    let _ = socket.send_to(&reply, src);
                }
            }
        }
    });

    let mut server = EspHttpServer::new(&Configuration::default())
        .map_err(|_| EspError::from_infallible::<ESP_FAIL>())?;

    // Marca de atividade do usuário (segundos desde o boot), tocada pelos handlers
    // de UI; o watchdog só reinicia se ficar ocioso, nunca no meio da configuração.
    let last_activity = Arc::new(AtomicU32::new(monotonic_secs()));

    let activity_index = last_activity.clone();
    server.fn_handler("/", esp_idf_svc::http::Method::Get, move |request| {
        activity_index.store(monotonic_secs(), Ordering::Relaxed);
        let html = r#"<!DOCTYPE html>
<html>
<head>
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>AetherSense Config</title>
    <style>
        body { font-family: -apple-system, sans-serif; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; background: #f0f2f5; }
        .card { background: white; padding: 2rem; border-radius: 12px; box-shadow: 0 4px 6px rgba(0,0,0,0.1); width: 90%; max-width: 400px; }
        h1 { color: #1a1a1a; text-align: center; margin-top: 0; }
        input { width: 100%; padding: 12px; margin: 8px 0 20px; border: 1px solid #ccc; border-radius: 6px; box-sizing: border-box; font-size: 16px; }
        button { width: 100%; padding: 14px; background: #0066cc; color: white; border: none; border-radius: 6px; font-size: 16px; font-weight: bold; cursor: pointer; }
        button:hover { background: #0052a3; }
    </style>
</head>
<body>
    <div class="card">
        <h1>Wi-Fi Config</h1>
        <form action="/save" method="post">
            <label>SSID</label>
            <input type="text" name="ssid" required>
            <label>Password</label>
            <input type="password" name="password">
            <button type="submit">Connect</button>
        </form>
    </div>
</body>
</html>"#;
        let mut response = request.into_ok_response()?;
        response.write_all(html.as_bytes())?;
        Ok::<(), EspIOError>(())
    })?;

    let nvs_clone = nvs_partition.clone();
    let activity_save = last_activity.clone();
    server.fn_handler("/save", esp_idf_svc::http::Method::Post, move |mut request| {
        activity_save.store(monotonic_secs(), Ordering::Relaxed);
        let mut buf = [0u8; 256];
        let len = request.read(&mut buf)?;
        let body = std::str::from_utf8(&buf[..len]).unwrap_or("");

        let mut ssid = "";
        let mut password = "";
        for pair in body.split('&') {
            let mut parts = pair.split('=');
            if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
                if k == "ssid" { ssid = v; }
                if k == "password" { password = v; }
            }
        }

        let ssid = decode_url(ssid);
        let password = decode_url(password);

        if let Ok(store) = EspNvs::new(nvs_clone.clone(), "aethersense", true) {
            let _ = store.set_str("ssid", &ssid);
            let _ = store.set_str("password", &password);
            log::warn!("Credentials saved to NVS. Restarting...");
        }

        let mut response = request.into_ok_response()?;
        response.write_all(b"<!DOCTYPE html><html><head><meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\"><title>Saved</title><style>body{font-family:sans-serif;text-align:center;padding:50px;}</style></head><body><h2>Credentials Saved</h2><p>Device is restarting to connect.</p></body></html>")?;

        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(2));
            unsafe { esp_restart() };
        });

        Ok::<(), EspIOError>(())
    })?;

    server.fn_handler("/generate_204", esp_idf_svc::http::Method::Get, |request| {
        let _response = request.into_response(204, None, &[])?;
        Ok::<(), EspIOError>(())
    })?;

    // Watchdog: se há credenciais salvas (timeout_secs=Some) e o portal fica OCIOSO
    // (sem atividade do usuário) por toda a janela, reinicia para re-tentar a rede
    // salva — evita ficar preso no AP de config quando a AP real só estava fora.
    // Rearma a cada interação (GET / ou POST /save), então NUNCA reinicia um usuário
    // no meio da configuração.
    if let Some(secs) = timeout_secs {
        let secs = secs.max(30);
        let slice = (secs / 4).max(5);
        let wd_activity = last_activity.clone();
        log::warn!(
            "Captive Portal: reinicia após {}s ocioso (rearma com atividade).",
            secs
        );
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(slice as u64));
            let idle = monotonic_secs().saturating_sub(wd_activity.load(Ordering::Relaxed));
            if idle >= secs {
                log::warn!(
                    "Captive Portal ocioso por >= {}s — reiniciando para re-tentar a rede salva.",
                    secs
                );
                unsafe { esp_restart() };
            }
        });
    }

    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::{portal_wifi_plan, PortalWifiStep};

    #[test]
    fn stopped_wifi_is_started_before_band_selection_and_ap_configuration() {
        assert_eq!(
            portal_wifi_plan(false),
            &[
                PortalWifiStep::ConfigureTemporaryStation,
                PortalWifiStep::StartForBandSelection,
                PortalWifiStep::Select2gBand,
                PortalWifiStep::StopForApConfiguration,
                PortalWifiStep::ConfigureAccessPoint,
                PortalWifiStep::StartAccessPoint,
            ]
        );
    }

    #[test]
    fn started_sta_selects_2g_before_reconfiguring_channel_six_ap() {
        assert_eq!(
            portal_wifi_plan(true),
            &[
                PortalWifiStep::Select2gBand,
                PortalWifiStep::StopForApConfiguration,
                PortalWifiStep::ConfigureAccessPoint,
                PortalWifiStep::StartAccessPoint,
            ]
        );
    }
}
