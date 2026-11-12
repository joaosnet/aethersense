//! Servidor Web HTTP embarcado para o Hub IR & Automação AetherSense C5.
//!
//! Serve a interface Web Tokyo Night (HTML/CSS/JS) e provê endpoints REST para:
//! - Disparo de comandos IR (`POST /api/ir/send?code=<id>`)
//! - Configuração e persistência do LED RGB por evento (`GET` e `POST /api/led/config`)
//! - Escaneamento dinâmico de redes Wi-Fi (`GET /api/wifi/scan`)
//! - Salvamento de credenciais Wi-Fi (`POST /api/wifi/save`)
//! - Reset de credenciais Wi-Fi e retorno ao Portal Cativo (`POST /api/wifi/reset`)
//! - Status e telemetria (`GET /api/status`)

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use esp_idf_svc::http::server::{Configuration, EspHttpConnection, EspHttpServer, Request};
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use esp_idf_svc::io::{EspIOError, Write};
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs};
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use esp_idf_sys::{esp_restart, EspError, ESP_FAIL};
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use log::{error, warn};
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use std::sync::Mutex;

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use crate::mondial_ir_codes;
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use crate::mqtt::{AccessoryCommand, AccessoryDispatch, CommandEnvelope};
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use crate::status_led::{LedEventMapping, StatusColor, StatusLed};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ScannedNetwork {
    pub ssid: String,
    pub channel: u8,
    pub rssi: i8,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct IrLearnStatus {
    pub state: String,
    pub command_id: String,
    pub command_name: String,
    pub error: Option<String>,
    pub sequence_us: Vec<u32>,
    pub pulse_count: usize,
    pub carrier_hz: u32,
    pub duty_percent: u8,
    pub quality_percent: u8,
    pub jitter_us: u32,
    pub signal_grade: String,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct IrRadarStatus {
    pub active: bool,
    pub remaining_seconds: u32,
    pub latest_quality_percent: u8,
    pub latest_jitter_us: u32,
    pub latest_grade: String,
    pub latest_pulse_count: usize,
    pub captures_count: u32,
    pub last_updated_ms: u64,
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
#[derive(Clone, Debug)]
struct LoopbackCapture {
    sequence_us: Vec<u32>,
    captured_at_us: i64,
    quality_percent: u8,
    jitter_us: u32,
    signal_grade: String,
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
static SCANNED_NETWORKS: Mutex<Vec<ScannedNetwork>> = Mutex::new(Vec::new());
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
static GLOBAL_DISPATCH: Mutex<Option<AccessoryDispatch>> = Mutex::new(None);
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
static GLOBAL_STATUS_LED: Mutex<Option<StatusLed>> = Mutex::new(None);
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
static CURRENT_IP: Mutex<String> = Mutex::new(String::new());
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
static CURRENT_SSID: Mutex<String> = Mutex::new(String::new());
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
static IR_LEARN_STATUS: Mutex<Option<IrLearnStatus>> = Mutex::new(None);
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
static LEARNED_CODES_CACHE: Mutex<Option<std::collections::HashMap<String, Vec<u32>>>> = Mutex::new(None);
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
static IR_RADAR_STATUS: Mutex<Option<IrRadarStatus>> = Mutex::new(None);
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
static RADAR_START_TIME_MS: Mutex<u64> = Mutex::new(0);
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
static LOOPBACK_LAST_CAPTURE: Mutex<Option<LoopbackCapture>> = Mutex::new(None);

fn grade_from_quality(quality: u8) -> &'static str {
    if quality >= 85 {
        "excellent"
    } else if quality >= 70 {
        "good"
    } else if quality >= 50 {
        "fair"
    } else {
        "weak"
    }
}

/// Scores a symmetric loopback burst (`1200 us` nominal pulses) without applying Mondial 17-pulse assumptions.
pub fn evaluate_loopback_quality(sequence_us: &[u32]) -> (u8, u32, String) {
    if sequence_us.is_empty() {
        return (0, 0, "none".to_string());
    }
    let total_dev: u32 = sequence_us
        .iter()
        .map(|&d| d.abs_diff(1200))
        .fold(0u32, u32::saturating_add);
    let avg_jitter = total_dev / (sequence_us.len() as u32);
    // Demodulators normally stretch pulses by ~40-90us at close range; reward consistent widths.
    let jitter_score = 100u32.saturating_sub(avg_jitter.saturating_sub(20) / 3);
    let count_factor = if sequence_us.len() >= 4 {
        100u32
    } else {
        (sequence_us.len() as u32) * 100 / 4
    };
    let final_quality = ((jitter_score * count_factor) / 100).clamp(10, 100) as u8;
    (
        final_quality,
        avg_jitter,
        grade_from_quality(final_quality).to_string(),
    )
}

pub fn calculate_signal_quality(sequence_us: &[u32]) -> (u8, u32, String) {
    if sequence_us.is_empty() {
        return (0, 0, "none".to_string());
    }

    let nominal_bins = [700u32, 1000, 1500, 3040, 5400];
    let mut total_jitter: u32 = 0;

    for &duration in sequence_us {
        let min_diff = nominal_bins
            .iter()
            .map(|&bin| duration.abs_diff(bin))
            .min()
            .unwrap_or(0);
        total_jitter = total_jitter.saturating_add(min_diff);
    }

    let avg_jitter = total_jitter / (sequence_us.len() as u32);
    // VS1838B adds ~30-60us optical rise/fall bias at close range; penalize primarily excess drift.
    let jitter_score = 100u32.saturating_sub(avg_jitter.saturating_sub(15) / 2);

    let completeness_percent = if sequence_us.len() >= 17 {
        100u32
    } else if sequence_us.len() >= 9 {
        92u32
    } else {
        (sequence_us.len() as u32) * 100 / 17
    };

    let final_quality = ((jitter_score * completeness_percent) / 100).clamp(5, 100) as u8;
    (
        final_quality,
        avg_jitter,
        grade_from_quality(final_quality).to_string(),
    )
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub fn record_learned_ir(
    request_id: &str,
    code_id: &str,
    carrier_hz: u32,
    duty_percent: u8,
    sequence_us: &[u32],
) {
    let now_us = unsafe { esp_idf_sys::esp_timer_get_time() };
    let now_ms = (now_us as u64) / 1000;

    if code_id == "loopback_test" {
        let (quality, jitter, grade) = evaluate_loopback_quality(sequence_us);
        warn!(
            "Web API: Loopback IR RX req='{}' ({} pulsos, Q={}%, jitter=±{}us, grade={})",
            request_id,
            sequence_us.len(),
            quality,
            jitter,
            grade
        );
        if let Ok(mut lock) = LOOPBACK_LAST_CAPTURE.lock() {
            *lock = Some(LoopbackCapture {
                sequence_us: sequence_us.to_vec(),
                captured_at_us: now_us,
                quality_percent: quality,
                jitter_us: jitter,
                signal_grade: grade,
            });
        }
        return;
    }

    let (quality, jitter, grade) = calculate_signal_quality(sequence_us);
    warn!(
        "Web API: IR Capturado! cmd='{}' (req='{}', {} pulsos, Q={}%, jitter=±{}us, grade={})",
        code_id,
        request_id,
        sequence_us.len(),
        quality,
        jitter,
        grade
    );

    if code_id == "radar_sweep" {
        if let Ok(mut r_lock) = IR_RADAR_STATUS.lock() {
            if let Some(radar) = r_lock.as_mut() {
                if radar.active {
                    radar.latest_quality_percent = quality;
                    radar.latest_jitter_us = jitter;
                    radar.latest_grade = grade.clone();
                    radar.latest_pulse_count = sequence_us.len();
                    radar.captures_count = radar.captures_count.saturating_add(1);
                    radar.last_updated_ms = now_ms;

                    if let Some(led) = get_global_status_led() {
                        let color = if quality >= 80 {
                            StatusColor::IrTx
                        } else if quality >= 50 {
                            StatusColor::Provisioning
                        } else {
                            StatusColor::Error
                        };
                        led.pulse(color, 450);
                    }
                }
            }
        }
        return;
    }

    if let Ok(mut lock) = IR_LEARN_STATUS.lock() {
        let name = lock
            .as_ref()
            .and_then(|s| if !s.command_name.is_empty() { Some(s.command_name.clone()) } else { None })
            .unwrap_or_else(|| {
                mondial_ir_codes::find_command(code_id)
                    .map(|c| c.name.to_string())
                    .unwrap_or_else(|| code_id.to_string())
            });
        *lock = Some(IrLearnStatus {
            state: "captured".to_string(),
            command_id: code_id.to_string(),
            command_name: name,
            error: None,
            sequence_us: sequence_us.to_vec(),
            pulse_count: sequence_us.len(),
            carrier_hz,
            duty_percent,
            quality_percent: quality,
            jitter_us: jitter,
            signal_grade: grade,
            updated_at_ms: now_ms,
        });
    }
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub fn record_learning_failed(request_id: &str, message: &str) {
    let now_ms = unsafe { (esp_idf_sys::esp_timer_get_time() as u64) / 1000 };
    warn!("Web API: IR Learning falhou (req='{}'): {}", request_id, message);
    if let Ok(mut lock) = IR_LEARN_STATUS.lock() {
        if let Some(status) = lock.as_mut() {
            status.state = if message.contains("timed out") {
                "timeout".to_string()
            } else {
                "failed".to_string()
            };
            status.error = Some(message.to_string());
            status.updated_at_ms = now_ms;
        }
    }
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub fn record_learning_cancelled(request_id: &str) {
    let now_ms = unsafe { (esp_idf_sys::esp_timer_get_time() as u64) / 1000 };
    warn!("Web API: IR Learning cancelado (req='{}')", request_id);
    if let Ok(mut lock) = IR_LEARN_STATUS.lock() {
        if let Some(status) = lock.as_mut() {
            status.state = "idle".to_string();
            status.updated_at_ms = now_ms;
        }
    }
}

#[cfg(not(all(feature = "esp-idf", target_os = "espidf")))]
pub fn record_learned_ir(_: &str, _: &str, _: u32, _: u8, _: &[u32]) {}
#[cfg(not(all(feature = "esp-idf", target_os = "espidf")))]
pub fn record_learning_failed(_: &str, _: &str) {}
#[cfg(not(all(feature = "esp-idf", target_os = "espidf")))]
pub fn record_learning_cancelled(_: &str) {}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub fn update_network_status(ip: &str, ssid: &str) {
    if let Ok(mut lock) = CURRENT_IP.lock() {
        *lock = ip.to_string();
    }
    if let Ok(mut lock) = CURRENT_SSID.lock() {
        *lock = ssid.to_string();
    }
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub fn set_global_handles(dispatch: AccessoryDispatch, status_led: StatusLed) {
    if let Ok(mut d) = GLOBAL_DISPATCH.lock() {
        *d = Some(dispatch);
    }
    if let Ok(mut l) = GLOBAL_STATUS_LED.lock() {
        *l = Some(status_led);
    }
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub fn get_global_dispatch() -> Option<AccessoryDispatch> {
    GLOBAL_DISPATCH.lock().ok().and_then(|g| g.clone())
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub fn get_global_status_led() -> Option<StatusLed> {
    GLOBAL_STATUS_LED.lock().ok().and_then(|g| g.clone())
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub fn update_scanned_networks(networks: &[ScannedNetwork]) {
    if let Ok(mut lock) = SCANNED_NETWORKS.lock() {
        *lock = networks.to_vec();
    }
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
const WEB_UI_HTML: &str = include_str!("web_ui.html");
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
fn send_json(
    request: Request<&mut EspHttpConnection<'_>>,
    status_code: u16,
    reason: &str,
    body: &[u8],
) -> Result<(), EspIOError> {
    let mut response = request.into_response(
        status_code,
        Some(reason),
        &[
            ("Content-Type", "application/json; charset=utf-8"),
            ("Connection", "close"),
            ("Cache-Control", "no-cache"),
        ],
    )?;
    response.write_all(body)?;
    Ok(())
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
fn is_portal_authorized(request: &Request<&mut EspHttpConnection<'_>>) -> bool {
    let sta_mac = crate::wifi::read_station_mac().unwrap_or([0u8; 6]);
    let expected_pin = crate::auth::portal_password_from_mac(sta_mac);
    request
        .header("Authorization")
        .map_or(false, |h| crate::auth::verify_portal_basic_auth(h, &expected_pin))
}

/// Shared IR command dispatcher used by REST API and daily scheduler (checks learned NVS overrides first).
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub fn dispatch_ir_by_code_id(dispatch: &AccessoryDispatch, code_id: &str) {
    let canon = mondial_ir_codes::canonical_key(code_id);
    let learned_seq = LEARNED_CODES_CACHE
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().and_then(|m| m.get(canon).cloned()));

    if code_id == "test" || code_id == "camera_test" {
        let now_ms = unsafe { (esp_idf_sys::esp_timer_get_time() as u64) / 1000 };
        let envelope = CommandEnvelope {
            schema: "aethersense.command.v1".to_string(),
            request_id: format!("web-ir-test-{now_ms}"),
            boot_id: None,
            issued_at_ms: now_ms,
            ttl_ms: 5000,
            command: AccessoryCommand::IrSend {
                code_id: "test-camera-burst".to_string(),
                carrier_hz: mondial_ir_codes::CARRIER_HZ,
                duty_percent: mondial_ir_codes::DUTY_PERCENT,
                sequence_us: vec![10000; 10],
                repeats: 20,
            },
        };
        if let Err(err) = dispatch.dispatch(envelope) {
            error!("Web API: Falha ao despachar teste IR: {err}");
        }
    } else if let Some(custom_seq) = learned_seq {
        let now_ms = unsafe { (esp_idf_sys::esp_timer_get_time() as u64) / 1000 };
        let envelope = CommandEnvelope {
            schema: "aethersense.command.v1".to_string(),
            request_id: format!("web-ir-{now_ms}"),
            boot_id: None,
            issued_at_ms: now_ms,
            ttl_ms: 5000,
            command: AccessoryCommand::IrSend {
                code_id: code_id.to_string(),
                carrier_hz: mondial_ir_codes::CARRIER_HZ,
                duty_percent: mondial_ir_codes::DUTY_PERCENT,
                sequence_us: custom_seq.clone(),
                repeats: 4,
            },
        };
        match dispatch.dispatch(envelope) {
            Ok(()) => warn!(
                "Web API: IR Gravado disparado: {} ({} pulsos, canon={}, duty={}%)",
                code_id,
                custom_seq.len(),
                canon,
                mondial_ir_codes::DUTY_PERCENT
            ),
            Err(err) => error!("Web API: Falha ao despachar IR Gravado: {err}"),
        }
    } else if let Some(cmd) = mondial_ir_codes::find_command(code_id) {
        let now_ms = unsafe { (esp_idf_sys::esp_timer_get_time() as u64) / 1000 };
        let envelope = CommandEnvelope {
            schema: "aethersense.command.v1".to_string(),
            request_id: format!("web-ir-{now_ms}"),
            boot_id: None,
            issued_at_ms: now_ms,
            ttl_ms: 5000,
            command: AccessoryCommand::IrSend {
                code_id: cmd.id.to_string(),
                carrier_hz: mondial_ir_codes::CARRIER_HZ,
                duty_percent: mondial_ir_codes::DUTY_PERCENT,
                sequence_us: cmd.sequence_us.to_vec(),
                repeats: 4,
            },
        };
        match dispatch.dispatch(envelope) {
            Ok(()) => warn!(
                "Web API: IR Padrão disparado: {} ({}) [repeats: 4, duty: {}%]",
                cmd.name,
                cmd.id,
                mondial_ir_codes::DUTY_PERCENT
            ),
            Err(err) => error!("Web API: Falha ao despachar IR: {err}"),
        }
    } else {
        warn!("Web API: Requisicao IR com codigo desconhecido: {}", code_id);
    }
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
fn reset_reason_label() -> &'static str {
    let reason = unsafe { esp_idf_sys::esp_reset_reason() } as u32;
    match reason {
        1 => "poweron",
        2 => "ext",
        3 => "sw",
        4 => "panic",
        5 => "int_wdt",
        6 => "task_wdt",
        7 => "wdt",
        8 => "deepsleep",
        9 => "brownout",
        10 => "sdio",
        11 => "usb",
        12 => "jtag",
        _ => "unknown",
    }
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub fn register_web_routes(
    server: &mut EspHttpServer<'static>,
    accessory_dispatch: AccessoryDispatch,
    status_led: StatusLed,
    nvs_partition: EspDefaultNvsPartition,
    require_auth: bool,
) -> Result<(), EspError> {
    // GET / -> Serve a interface Web Tokyo Night completa
    server.fn_handler("/", esp_idf_svc::http::Method::Get, move |request| {
        if require_auth && !is_portal_authorized(&request) {
            let mut response = request.into_response(
                401,
                Some("Unauthorized"),
                &[
                    ("WWW-Authenticate", "Basic realm=\"AetherSense Config\""),
                    ("Content-Type", "text/html; charset=utf-8"),
                ],
            )?;
            response.write_all(b"<!DOCTYPE html><html><body style=\"background:#1a1b26;color:#c0caf5;font-family:sans-serif;text-align:center;padding:50px;\"><h2>401 Nao Autorizado</h2><p>Informe o usuario e senha do portal.</p></body></html>")?;
            return Ok::<(), EspIOError>(());
        }

        let mut response = request.into_response(
            200,
            Some("OK"),
            &[
                ("Content-Type", "text/html; charset=utf-8"),
                ("Connection", "close"),
                ("Cache-Control", "no-store, no-cache, must-revalidate, max-age=0"),
                ("Pragma", "no-cache"),
            ],
        )?;
        for chunk in WEB_UI_HTML.as_bytes().chunks(4096) {
            response.write_all(chunk)?;
        }
        Ok::<(), EspIOError>(())
    })?;

    // Captive OS Probes
    for probe in ["/generate_204", "/hotspot-detect.html", "/ncsi.txt"] {
        server.fn_handler(probe, esp_idf_svc::http::Method::Get, |request| {
            let _response = request.into_response(
                302,
                Some("Found"),
                &[("Location", "http://192.168.71.1/"), ("Content-Length", "0")],
            )?;
            Ok::<(), EspIOError>(())
        })?;
    }

    // Inicialização do cache de códigos IR aprendidos a partir da NVS
    let nvs_ir_init = nvs_partition.clone();
    {
        let mut map = std::collections::HashMap::new();
        if let Ok(store) = EspNvs::new(nvs_ir_init, "ir_learned", false) {
            for cmd in mondial_ir_codes::ALL_COMMANDS {
                let key = mondial_ir_codes::canonical_key(cmd.id);
                let mut buf = [0u8; 1024];
                if let Ok(Some(json_str)) = store.get_str(key, &mut buf) {
                    if let Ok(seq) = serde_json::from_str::<Vec<u32>>(json_str) {
                        warn!("NVS IR: Código gravado recuperado para {}: {} pulsos", cmd.name, seq.len());
                        map.insert(key.to_string(), seq);
                    }
                }
            }
        }
        if let Ok(mut cache) = LEARNED_CODES_CACHE.lock() {
            *cache = Some(map);
        }
    }

    // POST /api/ir/send -> Disparo de comando Infravermelho
    let dispatch_ir = accessory_dispatch.clone();
    server.fn_handler("/api/ir/send", esp_idf_svc::http::Method::Post, move |request| {
        let uri = request.uri();
        let mut code_id = String::new();
        if let Some(pos) = uri.find("code=") {
            let query = &uri[pos + 5..];
            let end = query.find('&').unwrap_or(query.len());
            code_id = query[..end].to_string();
        }
        dispatch_ir_by_code_id(&dispatch_ir, &code_id);
        send_json(request, 200, "OK", b"{\"status\":\"ok\"}")
    })?;

    // POST /api/ir/learn/start?cmd=<id_or_name> -> Inicia modo de aprendizado no GPIO 8
    let dispatch_learn = accessory_dispatch.clone();
    let status_led_learn = status_led.clone();
    server.fn_handler("/api/ir/learn/start", esp_idf_svc::http::Method::Post, move |request| {
        let uri = request.uri();
        let mut target_cmd = "start_pause".to_string();
        if let Some(pos) = uri.find("cmd=") {
            let query = &uri[pos + 4..];
            let end = query.find('&').unwrap_or(query.len());
            target_cmd = query[..end].to_string();
        }

        let friendly_name = mondial_ir_codes::find_command(&target_cmd)
            .map(|c| c.name.to_string())
            .unwrap_or_else(|| target_cmd.clone());

        status_led_learn.pulse(StatusColor::Recording, 15000);
        let now_ms = unsafe { (esp_idf_sys::esp_timer_get_time() as u64) / 1000 };

        if let Ok(mut lock) = IR_LEARN_STATUS.lock() {
            *lock = Some(IrLearnStatus {
                state: "listening".to_string(),
                command_id: target_cmd.clone(),
                command_name: friendly_name.clone(),
                error: None,
                sequence_us: Vec::new(),
                pulse_count: 0,
                carrier_hz: mondial_ir_codes::CARRIER_HZ,
                duty_percent: mondial_ir_codes::DUTY_PERCENT,
                quality_percent: 0,
                jitter_us: 0,
                signal_grade: "listening".to_string(),
                updated_at_ms: now_ms,
            });
        }

        let envelope = CommandEnvelope {
            schema: "aethersense.command.v1".to_string(),
            request_id: format!("learn-{target_cmd}-{now_ms}"),
            boot_id: None,
            issued_at_ms: now_ms,
            ttl_ms: 15000,
            command: AccessoryCommand::IrLearnStart {
                code_id: Some(target_cmd.clone()),
                name: Some(friendly_name),
                timeout_ms: Some(15000),
                idle_threshold_us: Some(12000),
            },
        };

        let result = dispatch_learn.dispatch(envelope);
        if let Err(e) = &result {
            error!("Web API: Falha ao despachar IrLearnStart: {e}");
            let msg = serde_json::json!({"status": "error", "error": e.to_string()}).to_string();
            return send_json(request, 500, "Internal Server Error", msg.as_bytes());
        }

        send_json(request, 200, "OK", b"{\"status\":\"listening\",\"timeout_ms\":15000}")
    })?;

    // GET /api/ir/learn/status -> Retorna status do aprendizado (polling do frontend)
    server.fn_handler("/api/ir/learn/status", esp_idf_svc::http::Method::Get, |request| {
        let status = IR_LEARN_STATUS
            .lock()
            .ok()
            .and_then(|lock| lock.clone())
            .unwrap_or_else(|| IrLearnStatus {
                state: "idle".to_string(),
                command_id: String::new(),
                command_name: String::new(),
                error: None,
                sequence_us: Vec::new(),
                pulse_count: 0,
                carrier_hz: mondial_ir_codes::CARRIER_HZ,
                duty_percent: mondial_ir_codes::DUTY_PERCENT,
                quality_percent: 0,
                jitter_us: 0,
                signal_grade: "none".to_string(),
                updated_at_ms: 0,
            });

        let json_str = serde_json::to_string(&status).unwrap_or_else(|_| "{}".to_string());
        send_json(request, 200, "OK", json_str.as_bytes())
    })?;

    // POST /api/ir/learn/cancel -> Cancela aprendizado
    let dispatch_cancel = accessory_dispatch.clone();
    server.fn_handler("/api/ir/learn/cancel", esp_idf_svc::http::Method::Post, move |request| {
        let now_ms = unsafe { (esp_idf_sys::esp_timer_get_time() as u64) / 1000 };
        let envelope = CommandEnvelope {
            schema: "aethersense.command.v1".to_string(),
            request_id: format!("cancel-{now_ms}"),
            boot_id: None,
            issued_at_ms: now_ms,
            ttl_ms: 2000,
            command: AccessoryCommand::IrLearnCancel {},
        };
        let _ = dispatch_cancel.dispatch(envelope);
        if let Ok(mut lock) = IR_LEARN_STATUS.lock() {
            if let Some(status) = lock.as_mut() {
                status.state = "idle".to_string();
            }
        }
        send_json(request, 200, "OK", b"{\"status\":\"cancelled\"}")
    })?;

    // POST /api/ir/learn/test -> Dispara imediatamente o código recém-capturado para teste
    // POST /api/ir/learn/test -> Dispara imediatamente o código recém-capturado para teste
    let dispatch_test_learned = accessory_dispatch.clone();
    server.fn_handler("/api/ir/learn/test", esp_idf_svc::http::Method::Post, move |request| {
        let captured_seq = IR_LEARN_STATUS
            .lock()
            .ok()
            .and_then(|lock| lock.as_ref().map(|s| s.sequence_us.clone()))
            .unwrap_or_default();

        if captured_seq.is_empty() {
            return send_json(
                request,
                400,
                "Bad Request",
                b"{\"error\":\"Nenhum pulso gravado disponivel para teste\"}",
            );
        }

        let now_ms = unsafe { (esp_idf_sys::esp_timer_get_time() as u64) / 1000 };
        let envelope = CommandEnvelope {
            schema: "aethersense.command.v1".to_string(),
            request_id: format!("test-learned-{now_ms}"),
            boot_id: None,
            issued_at_ms: now_ms,
            ttl_ms: 5000,
            command: AccessoryCommand::IrSend {
                code_id: "test-learned".to_string(),
                carrier_hz: mondial_ir_codes::CARRIER_HZ,
                duty_percent: mondial_ir_codes::DUTY_PERCENT,
                sequence_us: captured_seq.clone(),
                repeats: 4,
            },
        };
        match dispatch_test_learned.dispatch(envelope) {
            Ok(()) => warn!("Web API: Codigo gravado testado com sucesso! ({} pulsos)", captured_seq.len()),
            Err(e) => error!("Web API: Falha ao despachar teste de gravacao: {e}"),
        }

        let payload = serde_json::json!({
            "status": "ok",
            "tested_pulses": captured_seq.len(),
        });
        let json_str = payload.to_string();
        send_json(request, 200, "OK", json_str.as_bytes())
    })?;

    // POST /api/ir/learn/save -> Salva permanentemente o código capturado na NVS
    let nvs_ir_save = nvs_partition.clone();
    server.fn_handler("/api/ir/learn/save", esp_idf_svc::http::Method::Post, move |request| {
        let (cmd_id, seq) = {
            let lock = IR_LEARN_STATUS.lock().ok();
            match lock.as_deref().and_then(|s| s.as_ref()) {
                Some(status) if !status.sequence_us.is_empty() => {
                    (status.command_id.clone(), status.sequence_us.clone())
                }
                _ => (String::new(), Vec::new()),
            }
        };

        if seq.is_empty() {
            return send_json(
                request,
                400,
                "Bad Request",
                b"{\"error\":\"Nenhum pulso para salvar\"}",
            );
        }

        let key = mondial_ir_codes::canonical_key(&cmd_id);
        let json_seq = serde_json::to_string(&seq).unwrap_or_else(|_| "[]".to_string());

        if let Ok(store) = EspNvs::new(nvs_ir_save.clone(), "ir_learned", true) {
            if let Ok(()) = store.set_str(key, &json_seq) {
                warn!("NVS IR: Gravacao salva com sucesso! key='{}', pulsos={}", key, seq.len());
            }
        }

        if let Ok(mut cache_guard) = LEARNED_CODES_CACHE.lock() {
            if let Some(cache) = cache_guard.as_mut() {
                cache.insert(key.to_string(), seq.clone());
            } else {
                let mut m = std::collections::HashMap::new();
                m.insert(key.to_string(), seq.clone());
                *cache_guard = Some(m);
            }
        }

        let payload = serde_json::json!({
            "status": "saved",
            "key": key,
            "pulses": seq.len(),
        });
        let json_str = payload.to_string();
        send_json(request, 200, "OK", json_str.as_bytes())
    })?;

    // GET /api/ir/learn/list -> Lista comandos e status de aprendizado
    server.fn_handler("/api/ir/learn/list", esp_idf_svc::http::Method::Get, |request| {
        let cache = LEARNED_CODES_CACHE.lock().ok().and_then(|c| c.clone()).unwrap_or_default();
        let mut list = Vec::new();
        for cmd in mondial_ir_codes::ALL_COMMANDS {
            let key = mondial_ir_codes::canonical_key(cmd.id);
            let is_learned = cache.contains_key(key);
            let pulses = cache.get(key).map(|v| v.len()).unwrap_or(cmd.sequence_us.len());
            list.push(serde_json::json!({
                "id": cmd.id,
                "key": key,
                "name": cmd.name,
                "is_learned": is_learned,
                "pulses": pulses,
            }));
        }
        let json_str = serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_string());
        send_json(request, 200, "OK", json_str.as_bytes())
    })?;

    // POST /api/ir/test -> Teste de emissão contínua de 2s para validação com câmera
    let dispatch_test = accessory_dispatch.clone();
    let status_led_test = status_led.clone();
    server.fn_handler("/api/ir/test", esp_idf_svc::http::Method::Post, move |request| {
        status_led_test.pulse(StatusColor::IrTx, 2000);
        dispatch_ir_by_code_id(&dispatch_test, "test");
        warn!("Web API: Rajada de Teste IR disparada (2s contínuo no GPIO 9)");
        send_json(request, 200, "OK", b"{\"status\":\"ok\",\"test\":\"camera_burst_2s\"}")
    })?;

    // POST /api/ir/test/loopback -> Auto-Diagnóstico Óptico do Emissor sem Câmera
    let dispatch_loopback = accessory_dispatch.clone();
    let status_led_loopback = status_led.clone();
    server.fn_handler("/api/ir/test/loopback", esp_idf_svc::http::Method::Post, move |request| {
        let now_ms = unsafe { (esp_idf_sys::esp_timer_get_time() as u64) / 1000 };

        if let Ok(mut lock) = LOOPBACK_LAST_CAPTURE.lock() {
            *lock = None;
        }

        // 1. Arma o canal RMT RX no GPIO 8 com timeout de 350ms
        let learn_envelope = CommandEnvelope {
            schema: "aethersense.command.v1".to_string(),
            request_id: format!("loopback-rx-{now_ms}"),
            boot_id: None,
            issued_at_ms: now_ms,
            ttl_ms: 1000,
            command: AccessoryCommand::IrLearnStart {
                code_id: Some("loopback_test".to_string()),
                name: Some("Loopback Test".to_string()),
                timeout_ms: Some(350),
                idle_threshold_us: Some(6000),
            },
        };
        let _ = dispatch_loopback.dispatch(learn_envelope);

        // Aguarda breve armação do RMT RX
        std::thread::sleep(std::time::Duration::from_millis(25));

        // 2. Dispara rajada curta no emissor (GPIO 9) - 6 pulsos simétricos
        let start_us = unsafe { esp_idf_sys::esp_timer_get_time() };
        let test_pulses: Vec<u32> = vec![1200, 1200, 1200, 1200, 1200, 1200];
        let tx_envelope = CommandEnvelope {
            schema: "aethersense.command.v1".to_string(),
            request_id: format!("loopback-tx-{now_ms}"),
            boot_id: None,
            issued_at_ms: now_ms,
            ttl_ms: 1000,
            command: AccessoryCommand::IrSend {
                code_id: "test-loopback-burst".to_string(),
                carrier_hz: mondial_ir_codes::CARRIER_HZ,
                duty_percent: mondial_ir_codes::DUTY_PERCENT,
                sequence_us: test_pulses,
                repeats: 1,
            },
        };
        let _ = dispatch_loopback.dispatch(tx_envelope);

        // 3. Aguarda confirmação do eco óptico no fotodiodo (até 250ms)
        let mut capture: Option<LoopbackCapture> = None;
        for _ in 0..25 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            if let Ok(mut lock) = LOOPBACK_LAST_CAPTURE.lock() {
                if let Some(found) = lock.take() {
                    capture = Some(found);
                    break;
                }
            }
        }

        let (success, detected_pulses, latency_us, quality, jitter, grade, seq) = match capture {
            Some(c) if !c.sequence_us.is_empty() => {
                let measured_us = c.captured_at_us.saturating_sub(start_us).max(1) as u64;
                (
                    true,
                    c.sequence_us.len(),
                    measured_us,
                    c.quality_percent,
                    c.jitter_us,
                    c.signal_grade,
                    c.sequence_us,
                )
            }
            _ => (false, 0, 0, 0, 0, "none".to_string(), Vec::new()),
        };

        if success {
            status_led_loopback.pulse(StatusColor::IrTx, 1000);
            warn!(
                "Web API: Auto-Diagnóstico Óptico SUCESSO! {} pulsos capturados no GPIO 8 (lat={}us, Q={}%)",
                detected_pulses, latency_us, quality
            );
        } else {
            status_led_loopback.pulse(StatusColor::Error, 1000);
            warn!("Web API: Auto-Diagnóstico Óptico FALHOU: nenhum pulso detectado no GPIO 8");
        }

        let resp_json = serde_json::json!({
            "status": if success { "ok" } else { "warning" },
            "success": success,
            "hardware_ok": success,
            "detected_pulses": detected_pulses,
            "latency_us": latency_us,
            "quality_percent": quality,
            "jitter_us": jitter,
            "signal_grade": grade,
            "sequence_us": seq,
        })
        .to_string();

        send_json(request, 200, "OK", resp_json.as_bytes())
    })?;

    // POST /api/ir/radar/start -> Inicia modo Radar de Alcance contínuo por 60s
    let dispatch_radar = accessory_dispatch.clone();
    server.fn_handler("/api/ir/radar/start", esp_idf_svc::http::Method::Post, move |request| {
        let now_ms = unsafe { (esp_idf_sys::esp_timer_get_time() as u64) / 1000 };
        if let Ok(mut lock) = IR_RADAR_STATUS.lock() {
            *lock = Some(IrRadarStatus {
                active: true,
                remaining_seconds: 60,
                latest_quality_percent: 0,
                latest_jitter_us: 0,
                latest_grade: "waiting".to_string(),
                latest_pulse_count: 0,
                captures_count: 0,
                last_updated_ms: now_ms,
            });
        }
        if let Ok(mut start_time) = RADAR_START_TIME_MS.lock() {
            *start_time = now_ms;
        }

        let envelope = CommandEnvelope {
            schema: "aethersense.command.v1".to_string(),
            request_id: format!("radar-{now_ms}"),
            boot_id: None,
            issued_at_ms: now_ms,
            ttl_ms: 60000,
            command: AccessoryCommand::IrLearnStart {
                code_id: Some("radar_sweep".to_string()),
                name: Some("Radar de Alcance".to_string()),
                timeout_ms: Some(60000),
                idle_threshold_us: Some(12000),
            },
        };
        let _ = dispatch_radar.dispatch(envelope);

        send_json(request, 200, "OK", b"{\"status\":\"started\",\"duration_seconds\":60}")
    })?;

    // GET /api/ir/radar/status -> Retorna estado do Radar
    let dispatch_radar_expire = accessory_dispatch.clone();
    server.fn_handler("/api/ir/radar/status", esp_idf_svc::http::Method::Get, move |request| {
        let now_ms = unsafe { (esp_idf_sys::esp_timer_get_time() as u64) / 1000 };
        let start_ms = RADAR_START_TIME_MS.lock().map(|g| *g).unwrap_or(0);
        let elapsed_secs = (now_ms.saturating_sub(start_ms) / 1000) as u32;

        let mut expired_now = false;
        let mut status = IR_RADAR_STATUS
            .lock()
            .ok()
            .and_then(|mut lock| {
                if let Some(radar) = lock.as_mut() {
                    if radar.active && elapsed_secs >= 60 {
                        radar.active = false;
                        radar.remaining_seconds = 0;
                        expired_now = true;
                    } else if radar.active {
                        radar.remaining_seconds = 60 - elapsed_secs;
                    }
                }
                lock.clone()
            })
            .unwrap_or(IrRadarStatus {
                active: false,
                remaining_seconds: 0,
                latest_quality_percent: 0,
                latest_jitter_us: 0,
                latest_grade: "off".to_string(),
                latest_pulse_count: 0,
                captures_count: 0,
                last_updated_ms: 0,
            });

        if expired_now {
            status.active = false;
            status.remaining_seconds = 0;
            let _ = dispatch_radar_expire.dispatch(CommandEnvelope {
                schema: "aethersense.command.v1".to_string(),
                request_id: format!("radar-expire-{now_ms}"),
                boot_id: None,
                issued_at_ms: now_ms,
                ttl_ms: 2000,
                command: AccessoryCommand::IrLearnCancel {},
            });
        }

        let json_str = serde_json::to_string(&status).unwrap_or_else(|_| "{}".to_string());
        send_json(request, 200, "OK", json_str.as_bytes())
    })?;

    // POST /api/ir/radar/stop -> Cancela modo Radar
    let dispatch_radar_stop = accessory_dispatch.clone();
    server.fn_handler("/api/ir/radar/stop", esp_idf_svc::http::Method::Post, move |request| {
        if let Ok(mut lock) = IR_RADAR_STATUS.lock() {
            if let Some(radar) = lock.as_mut() {
                radar.active = false;
                radar.remaining_seconds = 0;
            }
        }
        let now_ms = unsafe { (esp_idf_sys::esp_timer_get_time() as u64) / 1000 };
        let envelope = CommandEnvelope {
            schema: "aethersense.command.v1".to_string(),
            request_id: format!("radar-cancel-{now_ms}"),
            boot_id: None,
            issued_at_ms: now_ms,
            ttl_ms: 2000,
            command: AccessoryCommand::IrLearnCancel {},
        };
        let _ = dispatch_radar_stop.dispatch(envelope);
        send_json(request, 200, "OK", b"{\"status\":\"stopped\"}")
    })?;

    // GET /api/led/config -> Retorna configuração atual de cores por evento
    let status_led_get = status_led.clone();
    server.fn_handler("/api/led/config", esp_idf_svc::http::Method::Get, move |request| {
        let mapping = status_led_get.get_mapping();
        let json_str = serde_json::to_string(&mapping).unwrap_or_else(|_| "{}".to_string());
        send_json(request, 200, "OK", json_str.as_bytes())
    })?;

    // POST /api/led/config -> Atualiza e persiste o mapeamento de cores na NVS
    let status_led_post = status_led.clone();
    let nvs_led = nvs_partition.clone();
    server.fn_handler("/api/led/config", esp_idf_svc::http::Method::Post, move |mut request| {
        let mut buf = [0u8; 512];
        let len = request.read(&mut buf).unwrap_or(0);
        let body = std::str::from_utf8(&buf[..len]).unwrap_or("");

        if let Ok(mapping) = serde_json::from_str::<LedEventMapping>(body) {
            let _ = status_led_post.save_mapping_to_nvs(&mapping, &nvs_led);
            warn!("Web API LED: Novas configuracoes de evento salvas na NVS!");
            send_json(request, 200, "OK", b"{\"status\":\"ok\"}")
        } else {
            warn!("Web API LED: JSON de mapeamento invalido: {}", body);
            send_json(request, 400, "Bad Request", b"{\"error\":\"JSON LED invalido\"}")
        }
    })?;

    // GET /api/schedule -> Retorna agendamento salvo e status NTP/RTC ao vivo
    let nvs_sched_get = nvs_partition.clone();
    server.fn_handler("/api/schedule", esp_idf_svc::http::Method::Get, move |request| {
        let cfg = crate::scheduler::esp::load_from_nvs(&nvs_sched_get);
        let status = crate::scheduler::esp::get_status();
        let payload = serde_json::json!({
            "enabled": cfg.enabled,
            "departure": cfg.departure,
            "command": cfg.command,
            "dock_after_mins": cfg.dock_after_mins,
            "plug_on": cfg.plug_on,
            "plug_off": cfg.plug_off,
            "status": status,
        })
        .to_string();
        send_json(request, 200, "OK", payload.as_bytes())
    })?;

    // POST /api/schedule -> Valida, recalcula janela Tuya e salva na NVS + runtime
    let nvs_sched_post = nvs_partition.clone();
    server.fn_handler("/api/schedule", esp_idf_svc::http::Method::Post, move |mut request| {
        let mut buf = [0u8; 512];
        let len = request.read(&mut buf).unwrap_or(0);
        let body = std::str::from_utf8(&buf[..len]).unwrap_or("");

        let parsed: crate::scheduler::ScheduleConfig = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(err) => {
                let msg = serde_json::json!({"error": format!("JSON invalido: {err}")}).to_string();
                return send_json(request, 400, "Bad Request", msg.as_bytes());
            }
        };

        match crate::scheduler::esp::update_in_memory_and_nvs(&nvs_sched_post, parsed) {
            Ok(saved) => {
                let payload = serde_json::json!({
                    "status": "ok",
                    "message": "Agendamento salvo na NVS e ativo no relogio NTP",
                    "schedule": saved,
                })
                .to_string();
                send_json(request, 200, "OK", payload.as_bytes())
            }
            Err(err) => {
                let msg = serde_json::json!({"error": err}).to_string();
                send_json(request, 400, "Bad Request", msg.as_bytes())
            }
        }
    })?;

    // POST /api/schedule/sync_time -> Ajuste auxiliar de relógio caso UDP/NTP esteja bloqueado
    server.fn_handler("/api/schedule/sync_time", esp_idf_svc::http::Method::Post, move |mut request| {
        #[derive(serde::Deserialize)]
        struct SyncTimeReq {
            epoch_secs: i64,
        }
        let mut buf = [0u8; 128];
        let len = request.read(&mut buf).unwrap_or(0);
        let body = std::str::from_utf8(&buf[..len]).unwrap_or("");
        if let Ok(req) = serde_json::from_str::<SyncTimeReq>(body) {
            if req.epoch_secs >= 1_700_000_000 {
                let tv = esp_idf_sys::timeval {
                    tv_sec: req.epoch_secs as _,
                    tv_usec: 0,
                };
                unsafe {
                    esp_idf_sys::setenv(b"TZ\0".as_ptr() as *const _, b"BRT3\0".as_ptr() as *const _, 1);
                    esp_idf_sys::tzset();
                    esp_idf_sys::settimeofday(&tv, core::ptr::null());
                }
                let local = crate::scheduler::esp::current_local_clock()
                    .map(|(_, _, _, _, fmt)| fmt)
                    .unwrap_or_else(|| "--:--".to_string());
                let payload = serde_json::json!({"status": "ok", "local_time": local}).to_string();
                return send_json(request, 200, "OK", payload.as_bytes());
            }
        }
        send_json(request, 400, "Bad Request", b"{\"error\":\"epoch_secs invalido\"}")
    })?;

    // GET /api/ota/status -> Estado dos slots OTA e rollback
    server.fn_handler("/api/ota/status", esp_idf_svc::http::Method::Get, |request| {
        let info = crate::ota::esp::get_status();
        let json_str = serde_json::to_string(&info).unwrap_or_else(|_| "{}".to_string());
        send_json(request, 200, "OK", json_str.as_bytes())
    })?;

    // POST /api/ota/upload -> Gravação em streaming de firmware .bin protegida por Basic Auth
    server.fn_handler("/api/ota/upload", esp_idf_svc::http::Method::Post, |mut request| {
        if !is_portal_authorized(&request) {
            return send_json(
                request,
                401,
                "Unauthorized",
                b"{\"error\":\"Nao autorizado. Informe a senha de administracao do portal.\"}",
            );
        }

        let expected_len = request
            .header("Content-Length")
            .and_then(|v| v.trim().parse::<usize>().ok());

        warn!(
            "OTA Upload iniciado (expected_len={:?} bytes)...",
            expected_len
        );

        let result = crate::ota::esp::stream_update(expected_len, |buf| {
            request
                .read(buf)
                .map_err(|e| format!("Falha na leitura do stream HTTP: {e}"))
        });

        match result {
            Ok((next_slot, bytes)) => {
                let payload = serde_json::json!({
                    "status": "ok",
                    "next_slot": next_slot,
                    "bytes": bytes,
                    "rebooting": true,
                    "message": format!("Atualizacao gravada em {} ({} bytes). Reiniciando...", next_slot, bytes),
                })
                .to_string();
                let res = send_json(request, 200, "OK", payload.as_bytes());
                std::thread::spawn(|| {
                    std::thread::sleep(std::time::Duration::from_millis(800));
                    unsafe { esp_restart() };
                });
                res
            }
            Err(err) => {
                error!("OTA Upload falhou: {err}");
                let payload = serde_json::json!({
                    "status": "error",
                    "error": err,
                })
                .to_string();
                send_json(request, 400, "Bad Request", payload.as_bytes())
            }
        }
    })?;

    // GET /api/wifi/scan -> Retorna lista de redes visíveis detectadas
    server.fn_handler("/api/wifi/scan", esp_idf_svc::http::Method::Get, move |request| {
        let networks = SCANNED_NETWORKS.lock().map(|g| g.clone()).unwrap_or_default();
        let json_str = serde_json::to_string(&networks).unwrap_or_else(|_| "[]".to_string());
        send_json(request, 200, "OK", json_str.as_bytes())
    })?;

    // POST /api/wifi/save -> Salva novas credenciais e reinicia
    let nvs_wifi_save = nvs_partition.clone();
    server.fn_handler("/api/wifi/save", esp_idf_svc::http::Method::Post, move |mut request| {
        let mut buf = [0u8; 512];
        let len = request.read(&mut buf).unwrap_or(0);
        let body = std::str::from_utf8(&buf[..len]).unwrap_or("");

        #[derive(serde::Deserialize)]
        struct WifiSaveReq {
            ssid: String,
            password: String,
        }

        if let Ok(req) = serde_json::from_str::<WifiSaveReq>(body) {
            let ssid = req.ssid.trim();
            let pass = req.password.trim();
            warn!("Web API: Salvando credenciais Wi-Fi na NVS: ssid='{}'", ssid);
            if let Ok(store) = EspNvs::new(nvs_wifi_save.clone(), "aethersense", true) {
                let _ = store.set_str("ssid", ssid);
                let _ = store.set_str("password", pass);
                let _ = store.remove("wifi_channel");
            }
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(800));
                unsafe { esp_restart() };
            });
        }

        send_json(request, 200, "OK", b"{\"status\":\"saved\",\"message\":\"Reiniciando para conectar...\"}")
    })?;

    // POST /api/wifi/reset -> Apaga credenciais Wi-Fi da NVS e reinicia para abrir Portal Cativo
    let nvs_reset = nvs_partition.clone();
    server.fn_handler("/api/wifi/reset", esp_idf_svc::http::Method::Post, move |request| {
        warn!("Web API: Reset de Wi-Fi solicitado. Limpando credenciais da NVS...");
        if let Ok(store) = EspNvs::new(nvs_reset.clone(), "aethersense", true) {
            let _ = store.remove("ssid");
            let _ = store.remove("password");
            let _ = store.remove("wifi_channel");
            warn!("NVS limpa. Agendando reinicializacao para o Portal Cativo...");
        }

        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(800));
            unsafe { esp_restart() };
        });

        send_json(request, 200, "OK", b"{\"status\":\"resetting\",\"message\":\"Reiniciando em modo Portal...\"}")
    })?;

    // GET /api/status -> Telemetria completa do dispositivo, OTA e relógio NTP
    server.fn_handler("/api/status", esp_idf_svc::http::Method::Get, |request| {
        let ip = CURRENT_IP.lock().map(|g| g.clone()).unwrap_or_default();
        let ssid = CURRENT_SSID.lock().map(|g| g.clone()).unwrap_or_default();
        let uptime_secs = unsafe { (esp_idf_sys::esp_timer_get_time() as u64) / 1_000_000 };
        let free_heap_bytes = unsafe { esp_idf_sys::esp_get_free_heap_size() };
        let ota_info = crate::ota::esp::get_status();
        let sched = crate::scheduler::esp::get_status();
        let payload = serde_json::json!({
            "device": "ESP32-C5",
            "profile": "Tokyo Night Hub",
            "status": "online",
            "ip": ip,
            "connected_ssid": ssid,
            "mdns_host": "aethersense.local",
            "uptime_secs": uptime_secs,
            "free_heap_bytes": free_heap_bytes,
            "reset_reason": reset_reason_label(),
            "active_slot": ota_info.active_slot,
            "next_slot": ota_info.next_slot,
            "boot_validated": ota_info.boot_validated,
            "app_version": ota_info.app_version,
            "build_hash": ota_info.build_hash,
            "ntp_synced": sched.ntp_synced,
            "local_time": sched.local_time,
        })
        .to_string();
        send_json(request, 200, "OK", payload.as_bytes())
    })?;

    Ok(())
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub fn start_web_server(
    accessory_dispatch: AccessoryDispatch,
    status_led: StatusLed,
    nvs_partition: EspDefaultNvsPartition,
) -> Result<EspHttpServer<'static>, EspError> {
    let mut config = Configuration::default();
    config.stack_size = 10240;
    config.max_uri_handlers = 42;
    config.lru_purge_enable = true;
    config.max_open_sockets = 4;

    let mut server = EspHttpServer::new(&config)
        .map_err(|_| EspError::from_infallible::<ESP_FAIL>())?;

    register_web_routes(&mut server, accessory_dispatch, status_led, nvs_partition, false)?;
    warn!("Servidor Web Tokyo Night rodando com sucesso na porta 80!");
    Ok(server)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_quality_handles_symmetrical_1200us_burst() {
        let (q, jitter, grade) = evaluate_loopback_quality(&[1279, 1116, 1274, 1146, 1221]);
        assert!(q >= 80, "expected >=80, got {q} ({grade}, jitter={jitter})");
        assert!(jitter < 80);
    }

    #[test]
    fn signal_quality_evaluates_mondial_frames_accurately() {
        // Real captured 17-pulse Mondial Auto frame with optical demod stretch
        let (q17, _, grade17) = calculate_signal_quality(&[
            3118, 934, 724, 1461, 1554, 630, 765, 1471, 717, 1467, 1553, 631, 1583, 654, 716, 1494, 1531,
        ]);
        assert!(q17 >= 80, "expected >=80 on valid 17-pulse frame, got {q17} ({grade17})");

        // Short 9-pulse Arrow Down frame shouldn't be unfairly tanked below 60 when jitter is low
        let (q9, _, _) = calculate_signal_quality(&[692, 1508, 691, 1509, 691, 1510, 1508, 669, 1533]);
        assert!(q9 >= 70, "expected >=70 on valid 9-pulse frame, got {q9}");
    }
}
