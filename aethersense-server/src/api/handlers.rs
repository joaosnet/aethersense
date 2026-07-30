/// Handlers dos endpoints REST da API.
///
/// Cada handler recebe o `AppState` compartilhado e retorna JSON.
use axum::extract::{Path, Query, State};
use axum::Json;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::STFT_SAMPLE_RATE_HZ;
use crate::processing::amplitude::{compute_windowed_heatmap, smooth_signal_series};
use crate::processing::pca::compute_pca;
use crate::processing::pdp::compute_pdp;
use crate::processing::phase::sanitize_phase_subcarriers;
use crate::processing::stft::compute_stft;
use crate::replay;
use crate::state::{
    AppState, DeviceInfo, EmptyRoomBaseline, EnvironmentObject, Floorplan, ShowerBoxSettings,
    ShowerBoxState,
};

/// GET /api/settings — Retorna as configurações atuais do dashboard.
pub async fn get_settings(State(state): State<AppState>) -> Json<Value> {
    let inner = state.read().await;
    Json(serde_json::to_value(&inner.settings).unwrap_or_else(|_| json!({})))
}

/// GET /api/health — Verifica se o serviço HTTP está pronto.
pub async fn health() -> Json<Value> {
    Json(json!({ "ok": true }))
}

/// POST /api/settings — Atualiza as configurações do dashboard.
pub async fn update_settings(
    State(state): State<AppState>,
    Json(payload): Json<Value>,
) -> Json<Value> {
    let mut inner = state.write().await;
    {
        let s = &mut inner.settings;

        if let Some(v) = payload.get("heatmap_window").and_then(|v| v.as_u64()) {
            s.heatmap_window = v as usize;
        }
        if let Some(v) = payload.get("stft_window").and_then(|v| v.as_u64()) {
            s.stft_window = v as usize;
        }
        if let Some(v) = payload.get("pca_window").and_then(|v| v.as_u64()) {
            s.pca_window = v as usize;
        }
        if let Some(v) = payload.get("padding").and_then(|v| v.as_f64()) {
            s.padding = v;
        }
        if let Some(v) = payload.get("topology_scenario").and_then(|v| v.as_i64()) {
            s.topology_scenario = v as i32;
        }
        if let Some(v) = payload
            .get("vitals_window_seconds")
            .and_then(|v| v.as_f64())
        {
            s.vitals_window_seconds = v;
        }
        if let Some(v) = payload.get("vitals_min_samples").and_then(|v| v.as_u64()) {
            s.vitals_min_samples = v as usize;
        }
        if let Some(v) = payload
            .get("vitals_peak_prominence")
            .and_then(|v| v.as_f64())
        {
            s.vitals_peak_prominence = v;
        }
        if let Some(v) = payload
            .get("heart_rate_abs_snr_floor")
            .and_then(|v| v.as_f64())
        {
            s.heart_rate_abs_snr_floor = v;
        }
        if let Some(v) = payload
            .get("vitals_harmonic_count")
            .and_then(|v| v.as_u64())
        {
            s.vitals_harmonic_count = v as usize;
        }
        if let Some(v) = payload
            .get("vitals_harmonic_tolerance_hz")
            .and_then(|v| v.as_f64())
        {
            s.vitals_harmonic_tolerance_hz = v;
        }
        if let Some(v) = payload
            .get("vitals_subcarrier_top_k")
            .and_then(|v| v.as_u64())
        {
            s.vitals_subcarrier_top_k = v as usize;
        }
        if let Some(v) = payload
            .get("breathing_freq_min_hz")
            .and_then(|v| v.as_f64())
        {
            s.breathing_freq_min_hz = v;
        }
        if let Some(v) = payload
            .get("breathing_freq_max_hz")
            .and_then(|v| v.as_f64())
        {
            s.breathing_freq_max_hz = v;
        }
        if let Some(v) = payload
            .get("heart_rate_freq_min_hz")
            .and_then(|v| v.as_f64())
        {
            s.heart_rate_freq_min_hz = v;
        }
        if let Some(v) = payload
            .get("heart_rate_freq_max_hz")
            .and_then(|v| v.as_f64())
        {
            s.heart_rate_freq_max_hz = v;
        }

        s.clamp();
    }

    // Marca para persistir em disco (o detection_loop faz o flush assíncrono).
    inner.config_dirty = true;

    Json(serde_json::to_value(&inner.settings).unwrap_or_else(|_| json!({})))
}

fn shower_box_settings_response(
    settings: &ShowerBoxSettings,
    state: Option<&ShowerBoxState>,
    message: &str,
) -> Value {
    json!({
        "ok": true,
        "settings": settings,
        "state": state,
        "limits": {
            "entry_confirm_seconds": { "min": 2.0, "max": 5.0, "step": 0.5 },
            "entry_decay_seconds": { "min": 0.5, "max": 5.0, "step": 0.25 },
            "exit_hold_seconds": { "min": 60.0, "max": 300.0, "step": 15.0 },
            "recovery_confirm_seconds": { "min": 1.0, "max": 5.0, "step": 0.5 },
            "sensitivity": { "min": 1, "max": 5, "step": 1 }
        },
        "invariants": {
            "live_stream_required": true,
            "wroom_required": true,
            "c5_required_for_entry": true,
            "instant_motion_required_when_presence_assist_enabled": true,
            "static_corroboration_cannot_create_entry": true,
            "shared_empty_baseline_required": true,
            "minimum_sample_rate_hz": crate::detection::shower_box::MIN_SAMPLE_RATE_HZ,
            "startup_warmup_seconds": crate::detection::shower_box::STARTUP_WARMUP_SECONDS,
            "minimum_entry_seconds": 2.0,
            "minimum_exit_hold_seconds": 60.0
        },
        "message": message
    })
}

/// GET /api/shower-box/settings — comportamento persistente e limites seguros.
pub async fn get_shower_box_settings(State(state): State<AppState>) -> Json<Value> {
    let inner = state.read().await;
    Json(shower_box_settings_response(
        &inner.shower_box_settings,
        Some(&inner.detection.shower_box),
        "Configuração atual da zona do box",
    ))
}

/// POST /api/shower-box/settings — aplica um patch validado sem reiniciar o
/// backend. Campos ausentes preservam o valor atual.
pub async fn update_shower_box_settings(
    State(state): State<AppState>,
    Json(payload): Json<Value>,
) -> Json<Value> {
    let mut inner = state.write().await;
    {
        let settings = &mut inner.shower_box_settings;
        if let Some(value) = payload.get("profile") {
            if let Ok(parsed) = serde_json::from_value(value.clone()) {
                settings.profile = parsed;
            }
        }
        if let Some(value) = payload.get("control_mode") {
            if let Ok(parsed) = serde_json::from_value(value.clone()) {
                settings.control_mode = parsed;
            }
        }
        if let Some(value) = payload
            .get("entry_confirm_seconds")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
        {
            settings.entry_confirm_seconds = value;
        }
        if let Some(value) = payload
            .get("entry_decay_seconds")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
        {
            settings.entry_decay_seconds = value;
        }
        if let Some(value) = payload
            .get("exit_hold_seconds")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
        {
            settings.exit_hold_seconds = value;
        }
        if let Some(value) = payload
            .get("recovery_confirm_seconds")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
        {
            settings.recovery_confirm_seconds = value;
        }
        if let Some(value) = payload.get("sensitivity").and_then(Value::as_u64) {
            settings.sensitivity = value.min(u8::MAX as u64) as u8;
        }
        if let Some(value) = payload.get("presence_assist").and_then(Value::as_bool) {
            settings.presence_assist = value;
        }
        settings.clamp();
    }
    inner.config_dirty = true;
    Json(shower_box_settings_response(
        &inner.shower_box_settings,
        Some(&inner.detection.shower_box),
        "Comportamento do box atualizado e sincronizado",
    ))
}

/// POST /api/shower-box/settings/reset — restaura o perfil equilibrado.
pub async fn reset_shower_box_settings(State(state): State<AppState>) -> Json<Value> {
    let mut inner = state.write().await;
    inner.shower_box_settings = ShowerBoxSettings::default();
    inner.config_dirty = true;
    Json(shower_box_settings_response(
        &inner.shower_box_settings,
        Some(&inner.detection.shower_box),
        "Perfil equilibrado restaurado",
    ))
}

/// POST /api/clear — Limpa todos os históricos de dados.
pub async fn clear_history(State(state): State<AppState>) -> Json<Value> {
    let mut inner = state.write().await;
    inner.clear();
    // Persiste o bloqueio de recalibração pendente: um restart entre clear e
    // baseline não pode reabilitar silenciosamente a geometria anterior.
    match inner.save_config() {
        Ok(true) => Json(json!({ "ok": true })),
        Ok(false) => Json(json!({
            "ok": false,
            "message": "A configuração foi substituída por uma revisão mais nova"
        })),
        Err(error) => Json(json!({
            "ok": false,
            "message": format!("Falha ao persistir o bloqueio de recalibração: {error}")
        })),
    }
}

/// GET /api/replay — Retorna estado atual e captura default do replay CSV.
pub async fn get_replay(State(state): State<AppState>) -> Json<Value> {
    let default_file = replay::find_latest_m6_capture(replay::replay_dir_from_env())
        .map(|path| replay::display_name(&path));
    let inner = state.read().await;

    Json(replay::replay_response(&inner.stream, default_file))
}

/// POST /api/replay/start — Inicia replay da captura M6 mais recente.
pub async fn start_replay(State(state): State<AppState>) -> Json<Value> {
    Json(start_replay_from_dir(state, replay::replay_dir_from_env()).await)
}

async fn start_replay_from_dir(state: AppState, replay_dir: std::path::PathBuf) -> Value {
    let (path, packets) = match replay::load_default_capture(&replay_dir) {
        Ok(result) => result,
        Err(message) => {
            let inner = state.read().await;
            return json!({
                "ok": false,
                "message": message,
                "stream": &inner.stream,
            });
        }
    };

    let packet_count = packets.len();
    let replay_file = replay::display_name(&path);
    let stream = {
        let mut inner = state.write().await;
        let generation = inner.start_replay(replay_file.clone());
        let stream = inner.stream.clone();
        replay::spawn_replay_loop(state.clone(), packets, generation, replay_file);
        stream
    };

    json!({
        "ok": true,
        "message": "Replay CSV iniciado",
        "packets": packet_count,
        "stream": stream,
    })
}

/// POST /api/replay/stop — Para replay CSV mantendo o ultimo snapshot visivel.
pub async fn stop_replay(State(state): State<AppState>) -> Json<Value> {
    let stream = {
        let mut inner = state.write().await;
        inner.stop_replay("Replay CSV parado; exibindo ultimo snapshot local");
        inner.stream.clone()
    };

    Json(json!({
        "ok": true,
        "message": "Replay CSV parado",
        "stream": stream,
    }))
}

/// GET /api/devices — Lista os dispositivos ESP32 registrados.
pub async fn get_devices(State(state): State<AppState>) -> Json<Value> {
    let inner = state.read().await;
    Json(json!({ "devices": inner.devices_with_band() }))
}

/// GET /api/map-config — Retorna planta baixa e objetos do ambiente.
pub async fn get_map_config(State(state): State<AppState>) -> Json<Value> {
    let inner = state.read().await;
    Json(json!({ "map_config": inner.map_config }))
}

/// POST /api/map-config/floorplans — Registra ou atualiza uma planta baixa.
pub async fn upsert_floorplan(
    State(state): State<AppState>,
    Json(mut payload): Json<Floorplan>,
) -> Json<Value> {
    let mut inner = state.write().await;

    if payload.id.trim().is_empty() {
        payload.id = generated_id("floorplan");
    }
    payload.opacity = payload.opacity.clamp(0.05, 1.0);

    if let Some(existing) = inner
        .map_config
        .floorplans
        .iter_mut()
        .find(|floorplan| floorplan.id == payload.id)
    {
        *existing = payload;
    } else {
        inner.map_config.floorplans.push(payload);
    }

    let _ = inner.save_config();

    Json(json!({ "ok": true, "map_config": inner.map_config }))
}

/// DELETE /api/map-config/floorplans/{id} — Remove uma planta baixa.
pub async fn delete_floorplan(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Json<Value> {
    let mut inner = state.write().await;
    let before = inner.map_config.floorplans.len();
    inner
        .map_config
        .floorplans
        .retain(|floorplan| floorplan.id != id);
    let removed = inner.map_config.floorplans.len() != before;

    if removed {
        let _ = inner.save_config();
    }

    Json(json!({ "ok": removed, "map_config": inner.map_config }))
}

/// POST /api/map-config/environment-objects — Registra ou atualiza um móvel/elemento.
pub async fn upsert_environment_object(
    State(state): State<AppState>,
    Json(mut payload): Json<EnvironmentObject>,
) -> Json<Value> {
    let mut inner = state.write().await;

    if payload.id.trim().is_empty() {
        payload.id = generated_id("object");
    }
    payload.width_m = payload.width_m.clamp(0.1, 50.0);
    payload.depth_m = payload.depth_m.clamp(0.1, 50.0);
    payload.rotation_deg = payload.rotation_deg.rem_euclid(360.0);

    if let Some(existing) = inner
        .map_config
        .environment_objects
        .iter_mut()
        .find(|object| object.id == payload.id)
    {
        *existing = payload;
    } else {
        inner.map_config.environment_objects.push(payload);
    }

    let _ = inner.save_config();

    Json(json!({ "ok": true, "map_config": inner.map_config }))
}

/// DELETE /api/map-config/environment-objects/{id} — Remove um móvel/elemento.
pub async fn delete_environment_object(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Json<Value> {
    let mut inner = state.write().await;
    let before = inner.map_config.environment_objects.len();
    inner
        .map_config
        .environment_objects
        .retain(|object| object.id != id);
    let removed = inner.map_config.environment_objects.len() != before;

    if removed {
        let _ = inner.save_config();
    }

    Json(json!({ "ok": removed, "map_config": inner.map_config }))
}

/// POST /api/devices — Registra ou atualiza um dispositivo ESP32.
pub async fn register_device(
    State(state): State<AppState>,
    Json(payload): Json<DeviceInfo>,
) -> Json<Value> {
    // Coordenadas não-finitas (NaN/Inf) contaminariam a fusão de localização
    // e a serialização MQTT a jusante — rejeita antes de tocar no estado.
    if !payload.lat.is_finite() || !payload.lng.is_finite() {
        return Json(json!({
            "ok": false,
            "error": "lat/lng devem ser numeros finitos"
        }));
    }

    let mut inner = state.write().await;

    // Atualiza se já existe, senão adiciona
    if let Some(existing) = inner.devices.iter_mut().find(|d| d.id == payload.id) {
        existing.name = payload.name;
        existing.lat = payload.lat;
        existing.lng = payload.lng;
        existing.floor = payload.floor;
        existing.icon = payload.icon;
        existing.size = payload.size;
        existing.role = payload.role;
        existing.hardware_profile = payload.hardware_profile;
    } else {
        inner.devices.push(payload);
    }

    let _ = inner.save_config();

    Json(json!({ "devices": inner.devices_with_band() }))
}

/// POST /api/location/baseline — Salva baseline de sala vazia por receptor ativo.
#[derive(Debug, Deserialize)]
pub struct CalibrateParams {
    /// Calibra apenas este receptor (ex.: o WROOM recém-instalado em outro
    /// cômodo, sem exigir que o cômodo dos demais esteja vazio).
    pub device_id: Option<String>,
}

pub async fn calibrate_empty_room(
    State(state): State<AppState>,
    Query(params): Query<CalibrateParams>,
) -> Json<Value> {
    let device_filter = params.device_id;
    let mut inner = state.write().await;
    if inner.stream.replay_active {
        return Json(json!({
            "ok": false,
            "calibrated": [],
            "rejected": [{ "reason": "replay_not_allowed" }],
            "message": "A calibração exige CSI ao vivo; encerre o replay, limpe as medições e tente novamente"
        }));
    }

    let devices = inner.devices.clone();
    let map_config = inner.map_config.clone();
    let now_s = crate::state::monotonic_seconds();
    let clear_at_s = inner.calibration_clear_at_s;
    let calibration_session_id = inner.calibration_session_id.clone();
    let (shower_wroom_id, shower_c5_id) = crate::detection::shower_box::receiver_ids(&devices);
    let require_complete_shower_pair =
        device_filter.is_none() && shower_wroom_id.is_some() && shower_c5_id.is_some();
    // A calibração controla uma válvula de água: mudanças em memória só podem
    // ficar visíveis ao detector depois que o mesmo snapshot foi confirmado no
    // disco. Guarde o estado anterior para rollback em qualquer falha de I/O.
    let baselines_before = inner.location_baselines.clone();
    let calibrated_before = inner.calibrated_since_clear.clone();
    let required_session_before = inner.shower_box_required_session_id.clone();
    let mut calibrated = Vec::new();
    let mut rejected = Vec::new();

    for device in devices.iter().cloned() {
        let role = device.role.trim().to_ascii_lowercase();
        if !(role.is_empty() || role == "receiver") {
            continue;
        }
        if let Some(filter) = &device_filter {
            if &device.id != filter {
                continue;
            }
        }

        let Some(clear_at_s) = clear_at_s else {
            rejected.push(json!({ "id": device.id, "reason": "clear_required" }));
            continue;
        };
        let Some(calibration_session_id) = calibration_session_id.as_deref() else {
            rejected.push(json!({ "id": device.id, "reason": "clear_required" }));
            continue;
        };
        if inner.calibrated_since_clear.contains(&device.id) {
            rejected.push(json!({ "id": device.id, "reason": "already_calibrated_since_clear" }));
            continue;
        }

        let Some(history) = inner.device_histories.get(&device.id) else {
            rejected.push(json!({ "id": device.id, "reason": "no_live_samples" }));
            continue;
        };
        let minimum = crate::detection::shower_box::MIN_BASELINE_SAMPLES;
        if history.signal_history.len() < minimum || history.signal_time_history.len() < minimum {
            rejected.push(json!({ "id": device.id, "reason": "insufficient_new_samples" }));
            continue;
        }

        let first_recent_time =
            history.signal_time_history[history.signal_time_history.len() - minimum];
        let latest_time = *history
            .signal_time_history
            .back()
            .unwrap_or(&f64::NEG_INFINITY);
        if first_recent_time < clear_at_s {
            rejected.push(json!({ "id": device.id, "reason": "samples_predate_clear" }));
            continue;
        }
        if latest_time > now_s
            || now_s - latest_time > crate::detection::shower_box::SIGNAL_FRESHNESS_SECONDS
        {
            rejected.push(json!({ "id": device.id, "reason": "stale_receiver" }));
            continue;
        }

        let sample_rate_hz = crate::detection::shower_box::recent_sample_rate_hz(history, now_s);
        if sample_rate_hz < crate::detection::shower_box::MIN_SAMPLE_RATE_HZ {
            rejected.push(json!({
                "id": device.id,
                "reason": "insufficient_sample_rate",
                "sample_rate_hz": sample_rate_hz,
            }));
            continue;
        }

        let baseline = EmptyRoomBaseline {
            mean_amplitude: history.recent_mean_amplitude(minimum),
            amplitude_variance: history.recent_amplitude_variance(minimum),
            rssi_mean: history.mean_rssi().unwrap_or(-70.0),
            sample_count: minimum,
            hardware_profile: device.hardware_profile.clone(),
            calibrated_at: Some(Utc::now().to_rfc3339()),
            calibration_session_id: Some(calibration_session_id.to_string()),
            floor: Some(device.floor),
            floorplan_id: map_config
                .floorplans
                .iter()
                .find(|floorplan| floorplan.floor == device.floor)
                .map(|floorplan| floorplan.id.clone()),
            environment_object_ids: map_config
                .environment_objects
                .iter()
                .filter(|object| object.floor == device.floor && object.calibration_relevant)
                .map(|object| object.id.clone())
                .collect(),
        };

        inner
            .location_baselines
            .insert(device.id.clone(), baseline.clone());
        inner.calibrated_since_clear.insert(device.id.clone());
        calibrated.push(json!({
            "id": device.id,
            "name": device.name,
            "sample_rate_hz": sample_rate_hz,
            "baseline": baseline,
        }));
    }

    let complete_for_shower_box = calibration_session_id.as_deref().is_some_and(|session_id| {
        crate::detection::shower_box::calibrated_pair_is_valid(
            &devices,
            &inner.location_baselines,
            Some(session_id),
        )
    });
    if complete_for_shower_box
        && inner.shower_box_required_session_id.as_deref() == calibration_session_id.as_deref()
    {
        inner.shower_box_required_session_id = None;
    }
    let persistence_error = match inner.save_config() {
        Ok(true) => None,
        Ok(false) => Some("a configuração foi substituída por uma revisão mais nova".to_string()),
        Err(error) => Some(error.to_string()),
    };
    if persistence_error.is_some() {
        inner.location_baselines = baselines_before;
        inner.calibrated_since_clear = calibrated_before;
        inner.shower_box_required_session_id = required_session_before;
    }
    let any_calibrated = !calibrated.is_empty();
    let logical_ok = if require_complete_shower_pair {
        complete_for_shower_box
    } else {
        any_calibrated
    };
    let committed_complete = complete_for_shower_box && persistence_error.is_none();
    let ok = logical_ok && persistence_error.is_none();
    let mut message = if complete_for_shower_box {
        "Baselines WROOM e C5 da mesma sessão vazia salvos".to_string()
    } else if any_calibrated && require_complete_shower_pair {
        "Calibração parcial: a zona do box permanece bloqueada até WROOM e C5 concluírem a mesma sessão vazia".to_string()
    } else if any_calibrated {
        "Baseline de sala vazia salvo".to_string()
    } else {
        "Nenhum receptor elegível; use /api/clear, aguarde 100 amostras CSI ao vivo recentes a pelo menos 4 Hz e tente novamente".to_string()
    };
    if let Some(error) = &persistence_error {
        message = format!("Falha ao persistir a calibração com segurança: {error}");
    }

    Json(json!({
        "ok": ok,
        "complete_for_shower_box": committed_complete,
        "persistence_error": persistence_error,
        "rolled_back": persistence_error.is_some(),
        "calibrated": calibrated,
        "rejected": rejected,
        "message": message,
    }))
}

/// DELETE /api/location/baseline — Apaga o baseline de sala vazia. Sem
/// `device_id` apaga o de TODOS os receptores; com `?device_id=X` apaga só o
/// desse receptor. Útil quando a calibração ficou errada e o usuário quer
/// recomeçar do zero (sem baseline, o sistema cai no limiar absoluto).
pub async fn clear_empty_room_baseline(
    State(state): State<AppState>,
    Query(params): Query<CalibrateParams>,
) -> Json<Value> {
    let mut inner = state.write().await;
    let baselines_before = inner.location_baselines.clone();
    let removed: Vec<String> = match params.device_id {
        Some(id) => {
            if inner.location_baselines.remove(&id).is_some() {
                vec![id]
            } else {
                Vec::new()
            }
        }
        None => {
            let ids: Vec<String> = inner.location_baselines.keys().cloned().collect();
            inner.location_baselines.clear();
            ids
        }
    };

    let persistence_error = if removed.is_empty() {
        None
    } else {
        match inner.save_config() {
            Ok(true) => None,
            Ok(false) => {
                Some("a configuração foi substituída por uma revisão mais nova".to_string())
            }
            Err(error) => Some(error.to_string()),
        }
    };
    if persistence_error.is_some() {
        inner.location_baselines = baselines_before;
    }

    let ok = !removed.is_empty() && persistence_error.is_none();
    let message = if let Some(error) = &persistence_error {
        format!("Falha ao persistir a remoção do baseline: {error}")
    } else if removed.is_empty() {
        "Nenhum baseline para apagar".to_string()
    } else {
        "Baseline(s) de sala vazia apagado(s)".to_string()
    };

    Json(json!({
        "ok": ok,
        "removed": removed,
        "persistence_error": persistence_error,
        "rolled_back": persistence_error.is_some(),
        "message": message,
    }))
}

fn generated_id(prefix: &str) -> String {
    format!("{}_{}", prefix, Utc::now().timestamp_millis())
}

/// GET /api/detection — Retorna o estado atual de detecção.
pub async fn get_detection(State(state): State<AppState>) -> Json<Value> {
    let inner = state.read().await;
    Json(json!({
        "presence": inner.detection.presence,
        "movement": inner.detection.movement,
        "location": inner.detection.location,
        "vitals": inner.detection.vitals,
        "occupancy": inner.detection.occupancy,
        "active_zone": inner.detection.active_zone,
        "shower_box": inner.detection.shower_box,
    }))
}

/// GET /api/ml/status — Estado dos modelos ONNX + comparação ML × heurística.
pub async fn get_ml_status(State(state): State<AppState>) -> Json<Value> {
    let inner = state.read().await;
    let ml = &inner.detection.occupancy;
    let heuristic = &inner.detection.location;
    Json(json!({
        "runtime": inner.ml_status,
        "comparison": {
            "ml": {
                "available": ml.available,
                "method": ml.method,
                "count": ml.count,
                // Honestidade do zero-shot WiMANS: a contagem é estimativa
                // NÃO-calibrada para a sala (ver OccupancyEstimate.count_calibrated).
                "count_calibrated": ml.count_calibrated,
                "confidence": ml.confidence,
                "persons": ml.persons.len(),
                "vitals": ml.vitals_ml,
                "inference_ms": ml.inference_ms,
            },
            "heuristic": {
                "method": heuristic.method,
                "lat": heuristic.lat,
                "lng": heuristic.lng,
                "accuracy_m": heuristic.accuracy_m,
                "confidence": heuristic.confidence,
                "nodes_used": heuristic.nodes_used,
                "vitals_fft": inner.detection.vitals,
                "fft_diagnostics": inner.detection.fft_diagnostics,
            },
            "offline_benchmark": {
                "ml_mean_distance_error_m": 0.435,
                "ml_count_exact_acc": 0.517,
                "ml_count_within1_acc": 0.786,
                "heuristic_live_accuracy_m": 0.6848,
                "note": "ML: validação WiMANS (run 20260612_121755); heurística: validação em hardware real 2026-05-11",
            },
        },
    }))
}

/// GET /api/mqtt/settings — Config MQTT/Home Assistant (senha MASCARADA) + status.
pub async fn get_mqtt_settings(State(state): State<AppState>) -> Json<Value> {
    let inner = state.read().await;
    let m = &inner.mqtt;
    let connected = inner
        .mqtt_service
        .as_ref()
        .map(|s| s.is_connected())
        .unwrap_or(false);
    Json(json!({
        "enabled": m.enabled,
        "host": m.host,
        "port": m.port,
        "username": m.username,
        // Nunca devolve a senha; só informa se há uma definida.
        "password_set": m.password.as_ref().map(|p| !p.is_empty()).unwrap_or(false),
        "password_source": m.password_source(),
        "client_id": m.client_id,
        "base_topic": m.base_topic,
        "discovery_prefix": m.discovery_prefix,
        "retain": m.retain,
        "publish_interval_ms": m.publish_interval_ms,
        "keep_alive_secs": m.keep_alive_secs,
        "connected": connected,
    }))
}

/// Atualização parcial da config MQTT. Campos ausentes preservam o valor atual.
/// Senha: ausente = mantém; string vazia = limpa; preenchida = define somente
/// nesta execução. Para sobreviver a restart deve vir de
/// `AETHERSENSE_MQTT_PASSWORD`; o JSON persistido nunca contém o segredo.
#[derive(Debug, Deserialize)]
pub struct MqttSettingsUpdate {
    pub enabled: Option<bool>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub client_id: Option<String>,
    pub base_topic: Option<String>,
    pub discovery_prefix: Option<String>,
    pub retain: Option<bool>,
    pub publish_interval_ms: Option<u64>,
    pub keep_alive_secs: Option<u16>,
}

/// POST /api/mqtt/settings — Atualiza a config, persiste e reconecta o broker.
pub async fn update_mqtt_settings(
    State(state): State<AppState>,
    Json(update): Json<MqttSettingsUpdate>,
) -> Json<Value> {
    let (service, password_is_volatile) = {
        let mut inner = state.write().await;
        let m = &mut inner.mqtt;
        if let Some(v) = update.enabled {
            m.enabled = v;
        }
        if let Some(v) = update.host {
            m.host = v.trim().to_string();
        }
        if let Some(v) = update.port {
            m.port = v;
        }
        if let Some(v) = update.username {
            m.username = if v.trim().is_empty() { None } else { Some(v) };
        }
        if let Some(v) = update.password {
            m.set_runtime_password((!v.is_empty()).then_some(v));
        }
        if let Some(v) = update.client_id {
            if !v.trim().is_empty() {
                m.client_id = v;
            }
        }
        if let Some(v) = update.base_topic {
            if !v.trim().is_empty() {
                m.base_topic = v;
            }
        }
        if let Some(v) = update.discovery_prefix {
            if !v.trim().is_empty() {
                m.discovery_prefix = v;
            }
        }
        if let Some(v) = update.retain {
            m.retain = v;
        }
        if let Some(v) = update.publish_interval_ms {
            m.publish_interval_ms = v.max(100);
        }
        if let Some(v) = update.keep_alive_secs {
            m.keep_alive_secs = v.max(5);
        }
        inner.mqtt_configured = true;
        inner.config_dirty = true;
        let password_is_volatile = inner.mqtt.password.is_some()
            && inner.mqtt.password_source() == "memory";
        (inner.mqtt_service.clone(), password_is_volatile)
    };

    if let Some(s) = service {
        s.reconfigure();
    }
    Json(json!({
        "ok": true,
        "message": if password_is_volatile {
            "Configuração MQTT salva; senha ativa apenas nesta execução. Defina AETHERSENSE_MQTT_PASSWORD para preservá-la após reinício"
        } else {
            "Configuração MQTT salva; reconectando ao broker"
        },
        "password_persistent": !password_is_volatile,
    }))
}

/// POST /api/mqtt/discovery — Republica o discovery do Home Assistant sob demanda.
pub async fn republish_discovery(State(state): State<AppState>) -> Json<Value> {
    let service = { state.read().await.mqtt_service.clone() };
    let ok = if let Some(s) = service {
        s.publish_discovery_now(&state).await
    } else {
        false
    };
    Json(json!({
        "ok": ok,
        "message": if ok { "Discovery republicado" } else { "MQTT desconectado — habilite e salve primeiro" },
    }))
}

/// Normaliza um segmento de caminho vindo do cliente para um slug seguro
/// (`[a-z0-9_-]`), impedindo path traversal (`../`, barras, unidades de disco).
fn sanitize_path_segment(raw: &str) -> Option<String> {
    let slug: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-').trim_matches('_').to_string();
    if slug.is_empty() || slug.len() > 120 {
        None
    } else {
        Some(slug)
    }
}

/// POST /api/results/{categoria} — Persiste AUTOMATICAMENTE uma sessão de
/// validação/resultado em `resultados/<categoria>/<session_id>/session.json`
/// (bind mount da pasta `resultados/` na raiz do repositório). Idempotente:
/// re-salvar a mesma sessão sobrescreve o arquivo (auto-save incremental do
/// dashboard). A categoria vem da rota e o `session_id` do corpo JSON.
pub async fn save_result(
    State(state): State<AppState>,
    Path(category): Path<String>,
    Json(payload): Json<Value>,
) -> Json<Value> {
    let Some(category) = sanitize_path_segment(&category) else {
        return Json(json!({ "ok": false, "message": "categoria inválida" }));
    };
    let session_id = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .and_then(sanitize_path_segment)
        .unwrap_or_else(|| format!("sessao-{}", Utc::now().format("%Y-%m-%dT%H-%M-%SZ")));

    let pretty = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    // Sessões reais têm poucos KB; um teto agressivo limita o enchimento de
    // disco por requisição (o endpoint é local/sem auth, mas o bind é no host).
    const MAX_SESSION_BYTES: usize = 8 * 1024 * 1024;
    if pretty.len() > MAX_SESSION_BYTES {
        return Json(json!({ "ok": false, "message": "sessão grande demais (limite 8 MB)" }));
    }

    let results_dir = { state.read().await.results_dir.clone() };
    let dir = results_dir.join(&category).join(&session_id);
    // I/O de disco fora do runtime async (não prende worker do tokio) e escrita
    // ATÔMICA (temp + rename): POSTs concorrentes na mesma sessão nunca deixam
    // um session.json truncado/intercalado para os leitores.
    let result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join("session.json.tmp");
        std::fs::write(&tmp, pretty)?;
        std::fs::rename(&tmp, dir.join("session.json"))
    })
    .await;

    match result {
        Ok(Ok(())) => Json(json!({
            "ok": true,
            "path": format!("resultados/{category}/{session_id}/session.json"),
            "message": "Sessão salva automaticamente em resultados/",
        })),
        Ok(Err(error)) => {
            Json(json!({ "ok": false, "message": format!("falha ao salvar: {error}") }))
        }
        Err(error) => Json(json!({ "ok": false, "message": format!("falha interna: {error}") })),
    }
}

/// Normaliza um nome de arquivo mantendo extensões seguras (`[a-z0-9_.-]`),
/// sem permitir path traversal (`../`).
fn sanitize_filename(raw: &str) -> Option<String> {
    let slug: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug
        .trim_matches('-')
        .trim_matches('_')
        .trim_matches('.')
        .to_string();
    if slug.is_empty() || slug.len() > 120 || slug.contains("..") {
        None
    } else {
        Some(slug)
    }
}

/// POST /api/results/{categoria}/{session_id}/{filename} — Persiste um arquivo
/// derivado (como gráficos PNG ou resumos MD) diretamente na pasta da sessão em
/// `resultados/<categoria>/<session_id>/<filename>`.
pub async fn save_result_file(
    State(state): State<AppState>,
    Path((category, session_id, filename)): Path<(String, String, String)>,
    body: axum::body::Bytes,
) -> Json<Value> {
    let Some(category) = sanitize_path_segment(&category) else {
        return Json(json!({ "ok": false, "message": "categoria inválida" }));
    };
    let Some(session_id) = sanitize_path_segment(&session_id) else {
        return Json(json!({ "ok": false, "message": "session_id inválido" }));
    };
    let Some(filename) = sanitize_filename(&filename) else {
        return Json(json!({ "ok": false, "message": "filename inválido" }));
    };

    const MAX_FILE_BYTES: usize = 32 * 1024 * 1024;
    if body.len() > MAX_FILE_BYTES {
        return Json(json!({ "ok": false, "message": "arquivo grande demais (limite 32 MB)" }));
    }

    let results_dir = { state.read().await.results_dir.clone() };
    let dir = results_dir.join(&category).join(&session_id);
    let filepath = dir.join(&filename);
    let fname = filename.clone();

    let result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!("{}.tmp", fname));
        std::fs::write(&tmp, &body)?;
        std::fs::rename(&tmp, filepath)
    })
    .await;

    match result {
        Ok(Ok(())) => Json(json!({
            "ok": true,
            "path": format!("resultados/{category}/{session_id}/{filename}"),
            "message": "Arquivo salvo automaticamente em resultados/",
        })),
        Ok(Err(error)) => {
            Json(json!({ "ok": false, "message": format!("falha ao salvar: {error}") }))
        }
        Err(error) => Json(json!({ "ok": false, "message": format!("falha interna: {error}") })),
    }
}

/// GET /api/results — Índice das sessões persistidas (categoria → sessões),
/// para navegação por humanos, pelo dashboard e por agentes de IA.
pub async fn list_results(State(state): State<AppState>) -> Json<Value> {
    let results_dir = { state.read().await.results_dir.clone() };
    let mut categories = serde_json::Map::new();
    if let Ok(cats) = std::fs::read_dir(&results_dir) {
        for cat in cats.flatten() {
            if !cat.path().is_dir() {
                continue;
            }
            let name = cat.file_name().to_string_lossy().to_string();
            let mut sessions: Vec<String> = std::fs::read_dir(cat.path())
                .map(|entries| {
                    entries
                        .flatten()
                        .filter(|e| e.path().is_dir())
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect()
                })
                .unwrap_or_default();
            sessions.sort();
            categories.insert(name, json!(sessions));
        }
    }
    Json(json!({
        "ok": true,
        "results_dir": results_dir.display().to_string(),
        "categories": categories,
    }))
}

/// Monta o snapshot completo dos dados para envio via WebSocket.
///
/// Reproduz exatamente a mesma estrutura JSON do Python original,
/// com campos adicionais de detecção e dispositivos.
pub fn build_snapshot(state_inner: &crate::state::InnerState) -> Value {
    let settings = &state_inner.settings;
    let packet = &state_inner.latest_packet;

    if packet.is_none() {
        return json!({
            "count": state_inner.packet_count,
            "rssi": null,
            "rate": null,
            "subcarriers": null,
            "amplitudes": [],
            "phase_raw": [],
            "phase_sanitized": [],
            "heatmap": [],
            "stft": { "x": [], "y": [], "z": [] },
            "pca": { "packet_axis": [], "pc1": [], "pc2": [] },
            "pdp": { "x": [], "y": [] },
            "padding": settings.padding,
            "footer_text": "Sem pacotes CSI ainda",
            "devices": state_inner.devices_with_band(),
            "accessories": state_inner.accessories.accessories,
            "accessory_version": state_inner.accessories.version,
            "detection": state_inner.detection,
            "stream": &state_inner.stream,
        });
    }

    let pkt = packet.as_ref().unwrap();

    // Amplitude history como slices
    let amp_history: Vec<Vec<f64>> = state_inner.amplitude_history.iter().cloned().collect();
    let signal_history: Vec<f64> = state_inner.signal_history.iter().copied().collect();

    // Fase bruta e sanitizada
    let (phase_raw, phase_sanitized) = if !pkt.complex_samples.is_empty() {
        let raw: Vec<f64> = pkt.complex_samples.iter().map(|c| c.arg()).collect();
        let sanitized = sanitize_phase_subcarriers(&raw);
        (raw, sanitized)
    } else {
        let raw = pkt.phases.clone();
        let sanitized = sanitize_phase_subcarriers(&raw);
        (raw, sanitized)
    };

    // PDP
    let (pdp_x, pdp_y) = if !pkt.complex_samples.is_empty() {
        let pdp = compute_pdp(
            &pkt.complex_samples,
            crate::config::CSI_SUBCARRIER_SPACING_HZ,
        );
        (pdp.delay_axis, pdp.power_db)
    } else {
        (Vec::new(), Vec::new())
    };

    // Heatmap
    let heatmap = compute_windowed_heatmap(&amp_history, settings.heatmap_window);

    // STFT
    let stft_signal_end = signal_history.len();
    let stft_signal_start = if stft_signal_end > settings.stft_window {
        stft_signal_end - settings.stft_window
    } else {
        0
    };
    let stft_signal = smooth_signal_series(&signal_history[stft_signal_start..]);
    let stft_result = compute_stft(&stft_signal, settings.stft_window, STFT_SAMPLE_RATE_HZ);

    // PCA
    let pca_result = compute_pca(&amp_history, settings.pca_window);

    let footer = format!(
        "Porta: UDP | Pacotes: {} | Heatmap: {} | STFT: {} | PCA: {}",
        state_inner.packet_count,
        settings.heatmap_window,
        settings.stft_window,
        settings.pca_window,
    );

    json!({
        "count": state_inner.packet_count,
        "rssi": pkt.rssi,
        "rate": pkt.rate,
        "subcarriers": pkt.amplitudes.len(),
        "amplitudes": pkt.amplitudes,
        "phase_raw": phase_raw,
        "phase_sanitized": phase_sanitized,
        "heatmap": heatmap,
        "stft": {
            "x": stft_result.freq_axis,
            "y": stft_result.time_axis,
            "z": stft_result.spectrogram,
        },
        "pca": {
            "packet_axis": pca_result.packet_axis,
            "pc1": pca_result.pc1,
            "pc2": pca_result.pc2,
        },
        "pdp": { "x": pdp_x, "y": pdp_y },
        "padding": settings.padding,
        "footer_text": footer,
        "devices": state_inner.devices_with_band(),
        "accessories": state_inner.accessories.accessories,
        "accessory_version": state_inner.accessories.version,
        "detection": state_inner.detection,
        "stream": &state_inner.stream,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csi::parser::parse_csi_packet;
    use crate::state::{AppState, DeviceSignalHistory, FloorplanBounds, InnerState};
    use std::sync::Arc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio::sync::RwLock;

    const SAMPLE_CSI_LINE: &str = r#"CSI_DATA,0,02:00:00:00:00:01,-45,11,1,0,0,0,0,0,0,0,0,-90,0,6,0,12345,0,66,0,128,0,"[10,20,30,40,50,60,70,80]""#;

    fn temp_config_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "aethersense_{}_{}_{}.json",
            name,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time before epoch")
                .as_nanos()
        ))
    }

    fn temp_state(name: &str) -> AppState {
        Arc::new(RwLock::new(InnerState::new_with_config_path(
            temp_config_path(name),
        )))
    }

    #[tokio::test]
    async fn get_mqtt_mascara_segredo_e_informa_apenas_origem() {
        let state = temp_state("mqtt_secret_masking");
        {
            let mut inner = state.write().await;
            inner.mqtt.password = Some("nao-expor".into());
            inner.mqtt.password_secret_ref = None;
        }

        let response = get_mqtt_settings(State(state)).await;
        assert_eq!(response.0["password_set"], true);
        assert_eq!(response.0["password_source"], "memory");
        assert!(response.0.get("password").is_none());
        assert!(!response.0.to_string().contains("nao-expor"));
    }

    #[tokio::test]
    async fn api_do_box_aplica_patch_clamp_e_preserva_campos_ausentes() {
        let state = temp_state("shower_settings_patch");
        let response = update_shower_box_settings(
            State(state.clone()),
            Json(json!({
                "profile": "custom",
                "control_mode": "manual",
                "entry_confirm_seconds": 0.5,
                "exit_hold_seconds": 999.0,
                "sensitivity": 8,
                "presence_assist": false
            })),
        )
        .await;

        assert_eq!(response.0["ok"], true);
        assert_eq!(response.0["settings"]["entry_confirm_seconds"], 2.0);
        assert_eq!(response.0["settings"]["exit_hold_seconds"], 300.0);
        assert_eq!(response.0["settings"]["sensitivity"], 5);
        assert_eq!(
            response.0["settings"]["entry_decay_seconds"],
            ShowerBoxSettings::default().entry_decay_seconds
        );
        assert_eq!(response.0["settings"]["control_mode"], "manual");
        assert_eq!(response.0["settings"]["presence_assist"], false);

        let inner = state.read().await;
        assert!(inner.config_dirty);
        assert_eq!(
            inner.shower_box_settings.profile,
            crate::state::ShowerBoxProfile::Custom
        );
    }

    fn temp_replay_dir(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "aethersense_replay_{}_{}_{}",
            name,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time before epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("replay temp dir can be created");
        path
    }

    fn test_receiver(id: &str, hardware_profile: &str) -> DeviceInfo {
        DeviceInfo {
            id: id.to_string(),
            name: format!("Receptor {id}"),
            lat: 0.0,
            lng: 0.0,
            floor: 0,
            icon: String::new(),
            size: 32,
            role: "receiver".to_string(),
            hardware_profile: hardware_profile.to_string(),
        }
    }

    fn fresh_baseline_history(now_s: f64) -> DeviceSignalHistory {
        let mut history = DeviceSignalHistory::new();
        for index in 0..crate::detection::shower_box::MIN_BASELINE_SAMPLES {
            history.signal_history.push_back(20.0);
            history
                .signal_time_history
                .push_back(now_s - 0.99 + index as f64 * 0.01);
            history.rssi_history.push_back(-60);
        }
        history
    }

    async fn remove_config_file(state: &AppState) {
        let path = state.read().await.config_path.clone();
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn map_config_handlers_upsert_and_delete_entries() {
        let state = temp_state("map_handlers");

        let floorplan = Floorplan {
            id: "floor-1".to_string(),
            name: "Laboratorio".to_string(),
            floor: 4,
            image_data_url: "data:image/png;base64,AAAA".to_string(),
            bounds: FloorplanBounds {
                north: -1.0,
                south: -1.0001,
                east: -48.0,
                west: -48.0001,
            },
            opacity: 1.4,
        };

        let Json(value) = upsert_floorplan(State(state.clone()), Json(floorplan)).await;
        assert_eq!(value["ok"], true);
        assert_eq!(value["map_config"]["floorplans"][0]["id"], "floor-1");
        assert_eq!(value["map_config"]["floorplans"][0]["opacity"], 1.0);

        let object = EnvironmentObject {
            id: "desk-1".to_string(),
            name: "Mesa central".to_string(),
            object_type: "desk".to_string(),
            lat: -1.00005,
            lng: -48.00004,
            floor: 4,
            width_m: 1.4,
            depth_m: 0.8,
            rotation_deg: 15.0,
            calibration_relevant: true,
        };

        let Json(value) = upsert_environment_object(State(state.clone()), Json(object)).await;
        assert_eq!(value["ok"], true);
        assert_eq!(
            value["map_config"]["environment_objects"][0]["type"],
            "desk"
        );

        let Json(value) =
            delete_environment_object(Path("desk-1".to_string()), State(state.clone())).await;
        assert_eq!(value["ok"], true);
        assert!(value["map_config"]["environment_objects"]
            .as_array()
            .expect("objects should be an array")
            .is_empty());

        let Json(value) = delete_floorplan(Path("floor-1".to_string()), State(state.clone())).await;
        assert_eq!(value["ok"], true);
        assert!(value["map_config"]["floorplans"]
            .as_array()
            .expect("floorplans should be an array")
            .is_empty());

        remove_config_file(&state).await;
    }

    #[tokio::test]
    async fn empty_room_baseline_records_indoor_map_context() {
        let state = temp_state("baseline_context");

        {
            let mut inner = state.write().await;
            inner.devices.push(DeviceInfo {
                id: "rx-1".to_string(),
                name: "ESP32-C5 Sala".to_string(),
                lat: -23.5504802,
                lng: -46.6333161,
                floor: 4,
                icon: "/esp32-real.svg".to_string(),
                size: 32,
                role: "receiver".to_string(),
                hardware_profile: "esp32c5".to_string(),
            });
            inner.map_config.floorplans.push(Floorplan {
                id: "lab-floor".to_string(),
                name: "Laboratorio".to_string(),
                floor: 4,
                image_data_url: "data:image/png;base64,AAAA".to_string(),
                bounds: FloorplanBounds {
                    north: -23.5504000,
                    south: -23.5506000,
                    east: -46.6332000,
                    west: -46.6334000,
                },
                opacity: 0.6,
            });
            inner
                .map_config
                .environment_objects
                .push(EnvironmentObject {
                    id: "desk-1".to_string(),
                    name: "Mesa central".to_string(),
                    object_type: "desk".to_string(),
                    lat: -23.5504900,
                    lng: -46.6333100,
                    floor: 4,
                    width_m: 1.4,
                    depth_m: 0.8,
                    rotation_deg: 0.0,
                    calibration_relevant: true,
                });
            inner
                .map_config
                .environment_objects
                .push(EnvironmentObject {
                    id: "chair-ignored".to_string(),
                    name: "Cadeira fora da calibracao".to_string(),
                    object_type: "chair".to_string(),
                    lat: -23.5504900,
                    lng: -46.6333100,
                    floor: 4,
                    width_m: 0.6,
                    depth_m: 0.6,
                    rotation_deg: 0.0,
                    calibration_relevant: false,
                });

            inner.clear();
            let now_s = crate::state::monotonic_seconds();
            inner.calibration_clear_at_s = Some(now_s - 2.0);
            inner.stream = crate::state::StreamStatus::udp_live();

            let mut history = DeviceSignalHistory::new();
            for index in 0..crate::detection::shower_box::MIN_BASELINE_SAMPLES {
                history.signal_history.push_back(50.0 + index as f64);
                history
                    .signal_time_history
                    .push_back(now_s - 0.99 + index as f64 * 0.01);
                history.rssi_history.push_back(-58);
            }
            inner.device_histories.insert("rx-1".to_string(), history);
        }

        let Json(value) = calibrate_empty_room(
            State(state.clone()),
            Query(CalibrateParams { device_id: None }),
        )
        .await;
        assert_eq!(value["ok"], true);

        let baseline = &value["calibrated"][0]["baseline"];
        assert_eq!(baseline["floor"], 4);
        assert_eq!(baseline["floorplan_id"], "lab-floor");
        assert_eq!(baseline["environment_object_ids"], json!(["desk-1"]));
        assert!(baseline["calibrated_at"].as_str().is_some());
        assert!(value["calibrated"][0]["sample_rate_hz"]
            .as_f64()
            .is_some_and(|rate| rate >= crate::detection::shower_box::MIN_SAMPLE_RATE_HZ));

        let Json(repeated) = calibrate_empty_room(
            State(state.clone()),
            Query(CalibrateParams {
                device_id: Some("rx-1".to_string()),
            }),
        )
        .await;
        assert_eq!(repeated["ok"], false);
        assert_eq!(
            repeated["rejected"][0]["reason"],
            "already_calibrated_since_clear"
        );

        remove_config_file(&state).await;
    }

    #[tokio::test]
    async fn empty_room_baseline_rejects_missing_clear_and_stale_samples() {
        let state = temp_state("baseline_requires_fresh_clear");
        let now_s = crate::state::monotonic_seconds();

        {
            let mut inner = state.write().await;
            inner.devices.push(DeviceInfo {
                id: "rx-1".to_string(),
                name: "ESP32-WROOM Box".to_string(),
                lat: 0.0,
                lng: 0.0,
                floor: 0,
                icon: String::new(),
                size: 32,
                role: "receiver".to_string(),
                hardware_profile: "wroom".to_string(),
            });
            inner.stream = crate::state::StreamStatus::udp_live();

            let mut history = DeviceSignalHistory::new();
            for index in 0..crate::detection::shower_box::MIN_BASELINE_SAMPLES {
                history.signal_history.push_back(20.0);
                history
                    .signal_time_history
                    .push_back(now_s - 2.99 + index as f64 * 0.01);
            }
            inner.device_histories.insert("rx-1".to_string(), history);
        }

        let Json(without_clear) = calibrate_empty_room(
            State(state.clone()),
            Query(CalibrateParams { device_id: None }),
        )
        .await;
        assert_eq!(without_clear["ok"], false);
        assert_eq!(without_clear["rejected"][0]["reason"], "clear_required");

        {
            let mut inner = state.write().await;
            inner.calibration_clear_at_s = Some(now_s - 4.0);
            inner.calibration_session_id = Some("test-stale-session".to_string());
        }
        let Json(stale) = calibrate_empty_room(
            State(state.clone()),
            Query(CalibrateParams { device_id: None }),
        )
        .await;
        assert_eq!(stale["ok"], false);
        assert_eq!(stale["rejected"][0]["reason"], "stale_receiver");

        remove_config_file(&state).await;
    }

    #[tokio::test]
    async fn global_calibration_only_reports_success_with_wroom_and_c5_same_session() {
        let state = temp_state("baseline_requires_pair");

        {
            let mut inner = state.write().await;
            inner.devices.extend([
                DeviceInfo {
                    id: "w".to_string(),
                    name: "ESP32-WROOM Box".to_string(),
                    lat: 0.0,
                    lng: 0.0,
                    floor: 0,
                    icon: String::new(),
                    size: 32,
                    role: "receiver".to_string(),
                    hardware_profile: "wroom".to_string(),
                },
                DeviceInfo {
                    id: "c".to_string(),
                    name: "ESP32-C5 Quarto".to_string(),
                    lat: 0.0,
                    lng: 0.0,
                    floor: 0,
                    icon: String::new(),
                    size: 32,
                    role: "receiver".to_string(),
                    hardware_profile: "esp32c5".to_string(),
                },
            ]);
            inner.clear();
            let now_s = crate::state::monotonic_seconds();
            inner.calibration_clear_at_s = Some(now_s - 2.0);
            inner.stream = crate::state::StreamStatus::udp_live();

            for (id, samples) in [("w", 100usize), ("c", 99usize)] {
                let mut history = DeviceSignalHistory::new();
                for index in 0..samples {
                    history.signal_history.push_back(20.0);
                    history
                        .signal_time_history
                        .push_back(now_s - 0.99 + index as f64 * 0.01);
                }
                inner.device_histories.insert(id.to_string(), history);
            }
        }

        let Json(partial) = calibrate_empty_room(
            State(state.clone()),
            Query(CalibrateParams { device_id: None }),
        )
        .await;
        assert_eq!(partial["ok"], false);
        assert_eq!(partial["complete_for_shower_box"], false);
        assert_eq!(partial["calibrated"][0]["id"], "w");

        {
            let mut inner = state.write().await;
            let now_s = crate::state::monotonic_seconds();
            let mut history = DeviceSignalHistory::new();
            for index in 0..crate::detection::shower_box::MIN_BASELINE_SAMPLES {
                history.signal_history.push_back(20.0);
                history
                    .signal_time_history
                    .push_back(now_s - 0.99 + index as f64 * 0.01);
            }
            inner.device_histories.insert("c".to_string(), history);
        }

        let Json(complete) = calibrate_empty_room(
            State(state.clone()),
            Query(CalibrateParams { device_id: None }),
        )
        .await;
        assert_eq!(complete["ok"], true);
        assert_eq!(complete["complete_for_shower_box"], true);

        let inner = state.read().await;
        let wroom_session = inner.location_baselines["w"]
            .calibration_session_id
            .as_deref();
        let c5_session = inner.location_baselines["c"]
            .calibration_session_id
            .as_deref();
        assert!(wroom_session.is_some());
        assert_eq!(wroom_session, c5_session);
        assert!(inner.shower_box_required_session_id.is_none());
        drop(inner);

        remove_config_file(&state).await;
    }

    #[tokio::test]
    async fn calibration_persistence_failure_rolls_back_and_keeps_box_blocked() {
        let state = temp_state("baseline_persistence_rollback");
        let missing_parent = temp_config_path("missing_calibration_parent");

        let required_session = {
            let mut inner = state.write().await;
            inner
                .devices
                .extend([test_receiver("w", "wroom"), test_receiver("c", "esp32c5")]);
            inner.clear();
            let required = inner
                .shower_box_required_session_id
                .clone()
                .expect("clear deve criar sessão obrigatória");
            let now_s = crate::state::monotonic_seconds();
            inner.calibration_clear_at_s = Some(now_s - 2.0);
            inner.stream = crate::state::StreamStatus::udp_live();
            inner
                .device_histories
                .insert("w".to_string(), fresh_baseline_history(now_s));
            inner
                .device_histories
                .insert("c".to_string(), fresh_baseline_history(now_s));
            inner.config_path = missing_parent.join("config.json");
            required
        };

        let Json(result) = calibrate_empty_room(
            State(state.clone()),
            Query(CalibrateParams { device_id: None }),
        )
        .await;
        assert_eq!(result["ok"], false);
        assert_eq!(result["complete_for_shower_box"], false);
        assert_eq!(result["rolled_back"], true);
        assert!(result["persistence_error"].as_str().is_some());

        let inner = state.read().await;
        assert!(inner.location_baselines.is_empty());
        assert!(inner.calibrated_since_clear.is_empty());
        assert_eq!(
            inner.shower_box_required_session_id.as_deref(),
            Some(required_session.as_str())
        );
    }

    #[tokio::test]
    async fn baseline_removal_persistence_failure_is_reported_and_rolled_back() {
        let state = temp_state("baseline_delete_rollback");
        let missing_parent = temp_config_path("missing_delete_parent");
        {
            let mut inner = state.write().await;
            inner.location_baselines.insert(
                "w".to_string(),
                EmptyRoomBaseline {
                    mean_amplitude: 20.0,
                    amplitude_variance: 1.0,
                    rssi_mean: -60.0,
                    sample_count: crate::detection::shower_box::MIN_BASELINE_SAMPLES,
                    hardware_profile: "wroom".to_string(),
                    ..EmptyRoomBaseline::default()
                },
            );
            inner.config_path = missing_parent.join("config.json");
        }

        let Json(result) = clear_empty_room_baseline(
            State(state.clone()),
            Query(CalibrateParams {
                device_id: Some("w".to_string()),
            }),
        )
        .await;
        assert_eq!(result["ok"], false);
        assert_eq!(result["rolled_back"], true);
        assert!(result["persistence_error"].as_str().is_some());
        assert!(state.read().await.location_baselines.contains_key("w"));
    }

    #[tokio::test]
    async fn replay_start_stop_uses_latest_m6_capture_and_preserves_snapshot() {
        let state = temp_state("replay_start_stop");
        let replay_dir = temp_replay_dir("start_stop");
        std::fs::write(
            replay_dir.join("20260510T100000_m6_benchmark.csv"),
            SAMPLE_CSI_LINE,
        )
        .expect("old replay fixture can be written");
        std::fs::write(
            replay_dir.join("20260511T100000_m6_benchmark.csv"),
            SAMPLE_CSI_LINE,
        )
        .expect("new replay fixture can be written");

        let value = start_replay_from_dir(state.clone(), replay_dir.clone()).await;
        assert_eq!(value["ok"], true);
        assert_eq!(value["stream"]["source"], "csv_replay");
        assert_eq!(value["stream"]["replay_active"], true);
        assert_eq!(
            value["stream"]["replay_file"],
            "20260511T100000_m6_benchmark.csv"
        );

        tokio::time::sleep(Duration::from_millis(150)).await;

        let snapshot = {
            let inner = state.read().await;
            build_snapshot(&inner)
        };
        assert_eq!(snapshot["stream"]["source"], "csv_replay");
        assert!(snapshot["count"].as_u64().unwrap_or(0) >= 1);
        assert!(snapshot["heatmap"].as_array().is_some());

        let Json(value) = stop_replay(State(state.clone())).await;
        assert_eq!(value["ok"], true);
        assert_eq!(value["stream"]["source"], "idle");
        assert_eq!(value["stream"]["replay_active"], false);

        let snapshot_after_stop = {
            let inner = state.read().await;
            build_snapshot(&inner)
        };
        assert!(snapshot_after_stop["count"].as_u64().unwrap_or(0) >= 1);
        assert_eq!(snapshot_after_stop["stream"]["source"], "idle");

        remove_config_file(&state).await;
        let _ = std::fs::remove_dir_all(replay_dir);
    }

    #[tokio::test]
    async fn udp_packet_cancels_active_replay_metadata() {
        let state = temp_state("udp_cancels_replay");

        {
            let mut inner = state.write().await;
            inner.clear();
            let required_session = inner.shower_box_required_session_id.clone();
            inner.start_replay("fixture_m6_benchmark.csv".to_string());
            let packet = parse_csi_packet(SAMPLE_CSI_LINE).expect("sample packet parses");
            inner.ingest_replay_packet(packet.clone(), "fixture_m6_benchmark.csv".to_string());
            assert_eq!(inner.device_histories[&packet.mac].signal_history.len(), 1);

            inner.ingest_udp_packet(packet.clone());
            assert_eq!(inner.stream.source, crate::state::StreamSource::UdpLive);
            assert!(!inner.stream.replay_active);
            assert_eq!(inner.device_histories[&packet.mac].signal_history.len(), 1);
            assert!(inner.calibration_clear_at_s.is_none());
            assert!(inner.calibration_session_id.is_none());
            assert_eq!(inner.shower_box_required_session_id, required_session);
        }

        remove_config_file(&state).await;
    }

    #[tokio::test]
    async fn replay_encerrado_nao_pode_virar_baseline() {
        let state = temp_state("replay_cannot_calibrate");

        {
            let mut inner = state.write().await;
            inner.devices.push(DeviceInfo {
                id: "w".to_string(),
                name: "ESP32-WROOM Box".to_string(),
                lat: 0.0,
                lng: 0.0,
                floor: 0,
                icon: String::new(),
                size: 32,
                role: "receiver".to_string(),
                hardware_profile: "wroom".to_string(),
            });
            inner.clear();
            inner.start_replay("fixture.csv".to_string());

            let now_s = crate::state::monotonic_seconds();
            let mut replay_history = DeviceSignalHistory::new();
            for index in 0..crate::detection::shower_box::MIN_BASELINE_SAMPLES {
                replay_history.signal_history.push_back(20.0);
                replay_history
                    .signal_time_history
                    .push_back(now_s - 0.99 + index as f64 * 0.01);
            }
            inner
                .device_histories
                .insert("w".to_string(), replay_history);
            inner.stop_replay("replay finalizado");
        }

        let Json(result) = calibrate_empty_room(
            State(state.clone()),
            Query(CalibrateParams {
                device_id: Some("w".to_string()),
            }),
        )
        .await;
        assert_eq!(result["ok"], false);
        assert_eq!(result["rejected"][0]["reason"], "clear_required");

        remove_config_file(&state).await;
    }
}
