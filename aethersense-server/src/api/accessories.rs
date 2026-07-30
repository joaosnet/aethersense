//! REST API for accessory control, raw infrared codes and resumable SD uploads.

use std::io::{Read, Write};
use std::path::{Path as FsPath, PathBuf};
use std::sync::OnceLock;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::accessories::{constant_time_eq, sha256_hex, AccessoryInput, CommandInput, IrCodeInput};
use crate::state::AppState;

const API_TOKEN_ENV: &str = "AETHERSENSE_API_TOKEN";
const UPLOAD_CHUNK_BYTES: usize = 1024 * 1024;
const MAX_UPLOAD_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const CHUNK_SHA256_HEADER: &str = "x-chunk-sha256";

#[derive(Debug, Deserialize)]
pub struct OnboardRequest {
    pub device_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default = "empty_object")]
    pub capabilities: Value,
    #[serde(default)]
    pub accessories: Vec<AccessoryInput>,
    /// Stored only as SHA-256. The response and config never contain this value.
    #[serde(default)]
    pub device_token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct IrLearnRequest {
    pub device_id: String,
    #[serde(default)]
    pub code_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct IrSendRequest {
    pub device_id: String,
    pub code_id: String,
    #[serde(default)]
    pub repeats: Option<u32>,
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct UploadMetadata {
    schema: String,
    upload_id: String,
    device_id: String,
    session_id: String,
    filename: String,
    total_bytes: u64,
    sha256: String,
    contains_raw_csi: bool,
    capture_context: Option<String>,
    created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct StartUploadRequest {
    pub device_id: String,
    pub session_id: String,
    pub filename: String,
    pub total_bytes: u64,
    pub sha256: String,
    #[serde(default)]
    pub contains_raw_csi: bool,
    #[serde(default)]
    pub capture_context: Option<String>,
}

pub async fn get_accessories(State(state): State<AppState>) -> Json<Value> {
    Json(state.read().await.accessories.public_snapshot())
}

pub async fn onboard_device(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<OnboardRequest>,
) -> Response {
    if let Err(response) = require_api_auth(&headers) {
        return response;
    }
    let (snapshot, service) = {
        let mut inner = state.write().await;
        let name = if request.name.trim().is_empty() {
            request.device_id.clone()
        } else {
            request.name
        };
        if let Err(error) = inner.accessories.onboard(
            request.device_id,
            name,
            request.capabilities,
            request.accessories,
            request.device_token.as_deref(),
        ) {
            return error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_onboarding",
                error,
            );
        }
        inner.config_dirty = true;
        if let Err(error) = inner.save_config() {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "persistence_failed",
                error.to_string(),
            );
        }
        (
            inner.accessories.public_snapshot(),
            inner.mqtt_service.clone(),
        )
    };
    schedule_discovery_refresh(state, service);
    (StatusCode::OK, Json(snapshot)).into_response()
}

pub async fn create_command(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CommandInput>,
) -> Response {
    if let Err(response) = require_api_auth(&headers) {
        return response;
    }
    dispatch_command(state, request).await
}

pub async fn get_command(
    State(state): State<AppState>,
    Path(request_id): Path<String>,
) -> Response {
    let command = {
        let mut inner = state.write().await;
        let previous_version = inner.accessories.version;
        let command = inner.accessories.command(&request_id);
        if inner.accessories.version != previous_version {
            inner.config_dirty = true;
            let _ = inner.save_config();
        }
        command
    };
    match command {
        Some(command) => (
            StatusCode::OK,
            Json(json!({"ok": true, "command": command})),
        )
            .into_response(),
        None => error_response(StatusCode::NOT_FOUND, "not_found", "comando não encontrado"),
    }
}

pub async fn list_ir_codes(State(state): State<AppState>) -> Json<Value> {
    let inner = state.read().await;
    Json(json!({"codes": inner.accessories.ir_codes}))
}

pub async fn get_ir_code(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let inner = state.read().await;
    match inner.accessories.ir_codes.iter().find(|code| code.id == id) {
        Some(code) => (StatusCode::OK, Json(json!({"code": code}))).into_response(),
        None => error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            "código IR não encontrado",
        ),
    }
}

pub async fn create_ir_code(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<IrCodeInput>,
) -> Response {
    if let Err(response) = require_api_auth(&headers) {
        return response;
    }
    let (code, service) = {
        let mut inner = state.write().await;
        let code = match inner.accessories.create_ir_code(input) {
            Ok(code) => code,
            Err(error) => {
                return error_response(StatusCode::UNPROCESSABLE_ENTITY, "invalid_ir_code", error)
            }
        };
        inner.config_dirty = true;
        if let Err(error) = inner.save_config() {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "persistence_failed",
                error.to_string(),
            );
        }
        (code, inner.mqtt_service.clone())
    };
    schedule_discovery_refresh(state, service);
    (StatusCode::CREATED, Json(json!({"ok": true, "code": code}))).into_response()
}

pub async fn update_ir_code(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(input): Json<IrCodeInput>,
) -> Response {
    if let Err(response) = require_api_auth(&headers) {
        return response;
    }
    let (code, service) = {
        let mut inner = state.write().await;
        let code = match inner.accessories.update_ir_code(&id, input) {
            Ok(code) => code,
            Err(error) if error == "not_found" => {
                return error_response(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    "código IR não encontrado",
                )
            }
            Err(error) => {
                return error_response(StatusCode::UNPROCESSABLE_ENTITY, "invalid_ir_code", error)
            }
        };
        inner.config_dirty = true;
        if let Err(error) = inner.save_config() {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "persistence_failed",
                error.to_string(),
            );
        }
        (code, inner.mqtt_service.clone())
    };
    schedule_discovery_refresh(state, service);
    (StatusCode::OK, Json(json!({"ok": true, "code": code}))).into_response()
}

pub async fn delete_ir_code(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_api_auth(&headers) {
        return response;
    }
    let service = {
        let mut inner = state.write().await;
        if !inner.accessories.delete_ir_code(&id) {
            return error_response(
                StatusCode::NOT_FOUND,
                "not_found",
                "código IR não encontrado",
            );
        }
        inner.config_dirty = true;
        if let Err(error) = inner.save_config() {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "persistence_failed",
                error.to_string(),
            );
        }
        inner.mqtt_service.clone()
    };
    schedule_discovery_refresh(state, service);
    (StatusCode::OK, Json(json!({"ok": true, "id": id}))).into_response()
}

pub async fn learn_ir(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<IrLearnRequest>,
) -> Response {
    if let Err(response) = require_api_auth(&headers) {
        return response;
    }
    dispatch_command(
        state,
        CommandInput {
            request_id: request.request_id,
            device_id: request.device_id,
            action: "ir.learn.start".to_string(),
            params: json!({
                "code_id": request.code_id,
                "name": request.name,
                "timeout_ms": request.timeout_ms.unwrap_or(15_000).clamp(1_000, 60_000),
            }),
            ttl_ms: request.timeout_ms.map(|value| value.saturating_add(5_000)),
        },
    )
    .await
}

pub async fn send_ir(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<IrSendRequest>,
) -> Response {
    if let Err(response) = require_api_auth(&headers) {
        return response;
    }
    let code = {
        let inner = state.read().await;
        inner
            .accessories
            .ir_codes
            .iter()
            .find(|code| code.id == request.code_id && code.device_id == request.device_id)
            .cloned()
    };
    let Some(code) = code else {
        return error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            "código IR não encontrado para este dispositivo",
        );
    };
    dispatch_command(
        state,
        CommandInput {
            request_id: request.request_id,
            device_id: request.device_id,
            action: "ir.send".to_string(),
            params: json!({
                "code_id": code.id,
                "repeats": request.repeats.unwrap_or(1).clamp(1, 20),
            }),
            ttl_ms: None,
        },
    )
    .await
}

async fn dispatch_command(state: AppState, request: CommandInput) -> Response {
    let (command, created, service, boot_id) = {
        let mut inner = state.write().await;
        let boot_id = match inner.accessories.ready_boot_id(&request.device_id) {
            Ok(boot_id) => boot_id.to_string(),
            Err("device_not_found") => {
                return error_response(
                    StatusCode::NOT_FOUND,
                    "device_not_found",
                    "dispositivo ainda não foi registrado em /api/accessories/onboard",
                )
            }
            Err("device_offline") => {
                return error_response(
                    StatusCode::CONFLICT,
                    "device_offline",
                    "dispositivo offline; nenhum comando foi enfileirado",
                )
            }
            Err(_) => {
                return error_response(
                    StatusCode::CONFLICT,
                    "device_boot_unknown",
                    "aguarde capabilities/state com boot_id atual; nenhum comando foi enfileirado",
                )
            }
        };
        let Some(service) = inner
            .mqtt_service
            .clone()
            .filter(|service| service.is_connected())
        else {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "mqtt_unavailable",
                "MQTT desconectado; nenhum comando foi enfileirado",
            );
        };
        let (command, created) = match inner.accessories.create_command(request) {
            Ok(result) => result,
            Err(error) if error.contains("já existe") => {
                return error_response(StatusCode::CONFLICT, "request_id_conflict", error)
            }
            Err(error) => {
                return error_response(StatusCode::UNPROCESSABLE_ENTITY, "invalid_command", error)
            }
        };
        inner.config_dirty = true;
        if let Err(error) = inner.save_config() {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "persistence_failed",
                error.to_string(),
            );
        }
        (command, created, service, boot_id)
    };

    if created {
        let publish_result = service.publish_command(&command, &boot_id).await;
        if let Err(error) = publish_result {
            let failed = {
                let mut inner = state.write().await;
                inner
                    .accessories
                    .mark_command_failed(&command.request_id, error.clone());
                inner.config_dirty = true;
                let _ = inner.save_config();
                inner.accessories.command(&command.request_id)
            };
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "ok": false,
                    "error": "mqtt_unavailable",
                    "message": error,
                    "command": failed,
                })),
            )
                .into_response();
        }
    }

    (
        StatusCode::ACCEPTED,
        Json(json!({"ok": true, "command": command})),
    )
        .into_response()
}

pub async fn start_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<StartUploadRequest>,
) -> Response {
    if let Err(response) = require_device_auth(&state, &headers, &request.device_id).await {
        return response;
    }
    let session_id = match sanitize_segment(&request.session_id) {
        Some(value) => value,
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_session_id",
                "session_id inválido",
            )
        }
    };
    let filename = match sanitize_filename(&request.filename) {
        Some(value) => value,
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_filename",
                "filename inválido",
            )
        }
    };
    let sha256 = request.sha256.to_ascii_lowercase();
    if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_sha256",
            "sha256 deve ter 64 dígitos hexadecimais",
        );
    }
    if request.total_bytes > MAX_UPLOAD_BYTES {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "upload_too_large",
            "arquivo excede 4 GiB",
        );
    }
    let contains_raw_csi = match validate_upload_privacy(
        &filename,
        request.contains_raw_csi,
        request.capture_context.as_deref(),
    ) {
        Ok(value) => value,
        Err(message) => {
            return error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "raw_csi_context_required",
                message,
            )
        }
    };

    let results_dir = state.read().await.results_dir.clone();
    let seed = format!(
        "{}\0{}\0{}\0{}",
        request.device_id, session_id, filename, sha256
    );
    let upload_id = format!("upload-{}", &sha256_hex(seed.as_bytes())[..24]);
    let metadata = UploadMetadata {
        schema: "aethersense.firmware_sd_upload.v1".to_string(),
        upload_id: upload_id.clone(),
        device_id: request.device_id,
        session_id,
        filename,
        total_bytes: request.total_bytes,
        sha256,
        contains_raw_csi,
        capture_context: request.capture_context,
        created_at: Utc::now().to_rfc3339(),
    };
    let directory = upload_directory(&results_dir, &upload_id);
    let _upload_guard = upload_mutex().lock().await;
    let response = tokio::task::spawn_blocking(move || {
        initialise_or_deduplicate(&results_dir, &directory, &metadata)
    })
    .await;
    match response {
        Ok(Ok((offset, deduplicated))) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "upload_id": upload_id,
                "offset": offset,
                "chunk_size": UPLOAD_CHUNK_BYTES,
                "complete": deduplicated,
                "deduplicated": deduplicated,
            })),
        )
            .into_response(),
        Ok(Err(error)) => {
            error_response(StatusCode::CONFLICT, "upload_conflict", error.to_string())
        }
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "upload_failed",
            error.to_string(),
        ),
    }
}

pub async fn get_upload(
    State(state): State<AppState>,
    Path(upload_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(upload_id) = sanitize_upload_id(&upload_id) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_upload_id",
            "upload_id inválido",
        );
    };
    let results_dir = state.read().await.results_dir.clone();
    let directory = upload_directory(&results_dir, &upload_id);
    let metadata = match read_upload_metadata(&directory) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return error_response(StatusCode::NOT_FOUND, "not_found", "upload não encontrado")
        }
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "upload_failed",
                error.to_string(),
            )
        }
    };
    if let Err(response) = require_device_auth(&state, &headers, &metadata.device_id).await {
        return response;
    }
    let offset = std::fs::metadata(directory.join("data.part"))
        .map(|metadata| metadata.len())
        .unwrap_or_default();
    (StatusCode::OK, Json(json!({"ok": true, "upload": metadata, "offset": offset, "chunk_size": UPLOAD_CHUNK_BYTES}))).into_response()
}

pub async fn append_upload(
    State(state): State<AppState>,
    Path(upload_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(upload_id) = sanitize_upload_id(&upload_id) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_upload_id",
            "upload_id inválido",
        );
    };
    if body.len() > UPLOAD_CHUNK_BYTES {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "chunk_too_large",
            "chunk excede 1 MiB",
        );
    }
    let Some(offset) =
        header_string(&headers, "x-upload-offset").and_then(|value| value.parse::<u64>().ok())
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "missing_offset",
            "cabeçalho X-Upload-Offset é obrigatório",
        );
    };
    let chunk_sha256 = match chunk_sha256_header(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let _upload_guard = upload_mutex().lock().await;
    let results_dir = state.read().await.results_dir.clone();
    let directory = upload_directory(&results_dir, &upload_id);
    let metadata = match read_upload_metadata(&directory) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return error_response(StatusCode::NOT_FOUND, "not_found", "upload não encontrado")
        }
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "upload_failed",
                error.to_string(),
            )
        }
    };
    if let Err(response) = require_device_auth(&state, &headers, &metadata.device_id).await {
        return response;
    }
    if let Err(response) = verify_chunk_sha256(&chunk_sha256, &body) {
        return response;
    }
    let part_path = directory.join("data.part");
    let current = tokio::fs::metadata(&part_path)
        .await
        .map(|metadata| metadata.len())
        .unwrap_or_default();
    if current != offset {
        return (
            StatusCode::CONFLICT,
            Json(json!({"ok": false, "error": "offset_mismatch", "offset": current})),
        )
            .into_response();
    }
    if current.saturating_add(body.len() as u64) > metadata.total_bytes {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file_size_exceeded",
            "chunk ultrapassa total_bytes",
        );
    }
    let mut file = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&part_path)
        .await
    {
        Ok(file) => file,
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "upload_failed",
                error.to_string(),
            )
        }
    };
    if let Err(error) = file.write_all(&body).await {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "upload_failed",
            error.to_string(),
        );
    }
    if let Err(error) = file.sync_data().await {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "upload_failed",
            error.to_string(),
        );
    }
    let next_offset = current + body.len() as u64;
    (StatusCode::OK, Json(json!({"ok": true, "offset": next_offset, "complete": next_offset == metadata.total_bytes}))).into_response()
}

pub async fn complete_upload(
    State(state): State<AppState>,
    Path(upload_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(upload_id) = sanitize_upload_id(&upload_id) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_upload_id",
            "upload_id inválido",
        );
    };
    let _upload_guard = upload_mutex().lock().await;
    let results_dir = state.read().await.results_dir.clone();
    let directory = upload_directory(&results_dir, &upload_id);
    let metadata = match read_upload_metadata(&directory) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return error_response(StatusCode::NOT_FOUND, "not_found", "upload não encontrado")
        }
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "upload_failed",
                error.to_string(),
            )
        }
    };
    if let Err(response) = require_device_auth(&state, &headers, &metadata.device_id).await {
        return response;
    }
    let metadata_for_task = metadata.clone();
    let results_for_task = results_dir.clone();
    let result = tokio::task::spawn_blocking(move || {
        finalise_upload(&results_for_task, &directory, &metadata_for_task)
    })
    .await;
    match result {
        Ok(Ok(path)) => (
            StatusCode::OK,
            Json(json!({"ok": true, "path": path, "sha256": metadata.sha256})),
        )
            .into_response(),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::InvalidData => error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "integrity_failed",
            error.to_string(),
        ),
        Ok(Err(error)) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "upload_failed",
            error.to_string(),
        ),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "upload_failed",
            error.to_string(),
        ),
    }
}

fn require_api_auth(headers: &HeaderMap) -> Result<(), Response> {
    let Some(expected) = std::env::var(API_TOKEN_ENV)
        .ok()
        .filter(|value| !value.is_empty())
    else {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "auth_not_configured",
            format!("configure {API_TOKEN_ENV} para habilitar mutações"),
        ));
    };
    if api_token_matches(headers, &expected) {
        Ok(())
    } else {
        Err(error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_token",
            "Bearer token ausente ou inválido",
        ))
    }
}

fn api_token_matches(headers: &HeaderMap, expected: &str) -> bool {
    bearer_token(headers)
        .map(|candidate| constant_time_eq(expected, candidate))
        .unwrap_or(false)
}

fn schedule_discovery_refresh(state: AppState, service: Option<crate::mqtt::MqttService>) {
    let Some(service) = service else {
        return;
    };
    tokio::spawn(async move {
        let _ = service.publish_discovery_now(&state).await;
    });
}

async fn require_device_auth(
    state: &AppState,
    headers: &HeaderMap,
    device_id: &str,
) -> Result<(), Response> {
    let candidate = bearer_token(headers).unwrap_or_default();
    if candidate.is_empty() {
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_device_token",
            "Bearer token do dispositivo é obrigatório",
        ));
    }
    let matches = state
        .read()
        .await
        .accessories
        .device_token_matches(device_id, candidate);
    if matches {
        Ok(())
    } else {
        Err(error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_device_token",
            "credencial do dispositivo inválida",
        ))
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

fn header_string<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

fn chunk_sha256_header(headers: &HeaderMap) -> Result<String, Response> {
    let Some(value) = header_string(headers, CHUNK_SHA256_HEADER) else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "missing_chunk_sha256",
            "cabeçalho X-Chunk-SHA256 é obrigatório",
        ));
    };
    let value = value.trim().to_ascii_lowercase();
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_chunk_sha256",
            "X-Chunk-SHA256 deve ter 64 dígitos hexadecimais",
        ));
    }
    Ok(value)
}

fn verify_chunk_sha256(expected: &str, body: &[u8]) -> Result<(), Response> {
    let actual = sha256_hex(body);
    if constant_time_eq(expected, &actual) {
        Ok(())
    } else {
        Err(error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "chunk_integrity_failed",
            "SHA-256 do chunk não corresponde a X-Chunk-SHA256",
        ))
    }
}

fn error_response(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({"ok": false, "error": code, "message": message.into()})),
    )
        .into_response()
}

fn empty_object() -> Value {
    json!({})
}

fn sanitize_segment(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 120
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        || value.contains("..")
    {
        None
    } else {
        Some(value.to_string())
    }
}

fn sanitize_filename(value: &str) -> Option<String> {
    let value = sanitize_segment(value)?;
    (!value.starts_with('.')).then_some(value)
}

fn sanitize_upload_id(value: &str) -> Option<String> {
    let value = sanitize_segment(value)?;
    value.starts_with("upload-").then_some(value)
}

fn validate_upload_privacy(
    filename: &str,
    declared_raw_csi: bool,
    capture_context: Option<&str>,
) -> Result<bool, &'static str> {
    let contains_raw_csi = declared_raw_csi || filename.starts_with("csi-");
    if contains_raw_csi && !matches!(capture_context, Some("empty_room" | "bench")) {
        Err("CSI bruto exige capture_context=empty_room ou bench; coleta humana local é proibida")
    } else {
        Ok(contains_raw_csi)
    }
}

fn upload_directory(results_dir: &FsPath, upload_id: &str) -> PathBuf {
    results_dir
        .join("firmware-sd")
        .join(".uploads")
        .join(upload_id)
}

fn upload_mutex() -> &'static tokio::sync::Mutex<()> {
    static UPLOAD_MUTEX: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    UPLOAD_MUTEX.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn initialise_upload(directory: &FsPath, metadata: &UploadMetadata) -> std::io::Result<u64> {
    std::fs::create_dir_all(directory)?;
    let metadata_path = directory.join("metadata.json");
    if metadata_path.exists() {
        let existing: UploadMetadata = serde_json::from_slice(&std::fs::read(&metadata_path)?)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        if existing.upload_id != metadata.upload_id
            || existing.device_id != metadata.device_id
            || existing.session_id != metadata.session_id
            || existing.filename != metadata.filename
            || existing.total_bytes != metadata.total_bytes
            || existing.sha256 != metadata.sha256
            || existing.contains_raw_csi != metadata.contains_raw_csi
            || existing.capture_context != metadata.capture_context
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "upload_id já existe com metadados diferentes",
            ));
        }
    } else {
        let bytes = serde_json::to_vec_pretty(metadata)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let temporary = directory.join("metadata.json.tmp");
        std::fs::write(&temporary, bytes)?;
        std::fs::rename(temporary, &metadata_path)?;
    }
    let part = directory.join("data.part");
    if !part.exists() {
        std::fs::File::create(&part)?.sync_all()?;
    }
    let offset = std::fs::metadata(part)?.len();
    if offset > metadata.total_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "arquivo parcial excede total_bytes",
        ));
    }
    Ok(offset)
}

fn initialise_or_deduplicate(
    results_dir: &FsPath,
    directory: &FsPath,
    metadata: &UploadMetadata,
) -> std::io::Result<(u64, bool)> {
    let target = results_dir
        .join("firmware-sd")
        .join(&metadata.session_id)
        .join(&metadata.filename);
    if target.exists()
        && !(metadata.filename == "session.json" && is_generated_session_manifest(&target))
    {
        let existing = sha256_file(&target)?;
        if constant_time_eq(&existing, &metadata.sha256) {
            return Ok((metadata.total_bytes, true));
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "arquivo final já existe com conteúdo diferente",
        ));
    }
    initialise_upload(directory, metadata).map(|offset| (offset, false))
}

fn read_upload_metadata(directory: &FsPath) -> std::io::Result<UploadMetadata> {
    serde_json::from_slice(&std::fs::read(directory.join("metadata.json"))?)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn finalise_upload(
    results_dir: &FsPath,
    directory: &FsPath,
    metadata: &UploadMetadata,
) -> std::io::Result<String> {
    let part = directory.join("data.part");
    let actual_size = std::fs::metadata(&part)?.len();
    if actual_size != metadata.total_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("tamanho incompleto: {actual_size}/{}", metadata.total_bytes),
        ));
    }
    let mut file = std::fs::File::open(&part)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual_hash: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if !constant_time_eq(&actual_hash, &metadata.sha256) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "SHA-256 divergente: esperado {}, recebido {actual_hash}",
                metadata.sha256
            ),
        ));
    }

    let session_dir = results_dir.join("firmware-sd").join(&metadata.session_id);
    std::fs::create_dir_all(&session_dir)?;
    let target = session_dir.join(&metadata.filename);
    if target.exists()
        && metadata.filename == "session.json"
        && is_generated_session_manifest(&target)
    {
        std::fs::remove_file(&target)?;
    }
    if target.exists() {
        let existing = sha256_file(&target)?;
        if !constant_time_eq(&existing, &metadata.sha256) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "arquivo final já existe com conteúdo diferente",
            ));
        }
        std::fs::remove_dir_all(directory)?;
    } else {
        std::fs::rename(&part, &target)?;
        let _ = std::fs::remove_dir_all(directory);
    }

    let session_manifest = session_dir.join("session.json");
    if metadata.filename != "session.json" && !session_manifest.exists() {
        let manifest = json!({
            "schema": "aethersense.firmware_sd_session.v1",
            "session_id": metadata.session_id,
            "device_id": metadata.device_id,
            "imported_at": Utc::now().to_rfc3339(),
            "source": "firmware_network_upload",
            "generated_placeholder": true,
            "privacy": {"empty_room_or_bench_required_for_raw_csi": true},
            "contains_raw_csi": metadata.contains_raw_csi,
            "capture_context": metadata.capture_context,
        });
        let mut output = std::fs::File::create(&session_manifest)?;
        output.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())?;
        output.sync_all()?;
    }
    Ok(format!(
        "resultados/firmware-sd/{}/{}",
        metadata.session_id, metadata.filename
    ))
}

fn is_generated_session_manifest(path: &FsPath) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.get("generated_placeholder").and_then(Value::as_bool))
        .unwrap_or(false)
}

fn sha256_file(path: &FsPath) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accessories::{AccessoryKind, AccessoryRegistry};
    use crate::state::InnerState;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::sync::RwLock;

    #[test]
    fn chunk_exige_sha256_valido_e_rejeita_conteudo_divergente() {
        let payload = b"chunk-aethersense";

        let missing = chunk_sha256_header(&HeaderMap::new()).unwrap_err();
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);

        let mut headers = HeaderMap::new();
        headers.insert(
            CHUNK_SHA256_HEADER,
            axum::http::HeaderValue::from_static("sha-invalido"),
        );
        let invalid = chunk_sha256_header(&headers).unwrap_err();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);

        headers.insert(
            CHUNK_SHA256_HEADER,
            axum::http::HeaderValue::from_str(&sha256_hex(b"outro-chunk")).unwrap(),
        );
        let expected = chunk_sha256_header(&headers).unwrap();
        let mismatch = verify_chunk_sha256(&expected, payload).unwrap_err();
        assert_eq!(mismatch.status(), StatusCode::UNPROCESSABLE_ENTITY);

        headers.insert(
            CHUNK_SHA256_HEADER,
            axum::http::HeaderValue::from_str(&sha256_hex(payload)).unwrap(),
        );
        let expected = chunk_sha256_header(&headers).unwrap();
        assert!(verify_chunk_sha256(&expected, payload).is_ok());
    }

    #[test]
    fn upload_retomavel_valida_hash_e_deduplica() {
        let root = tempdir().unwrap();
        let bytes = b"firmware sd fixture";
        let metadata = UploadMetadata {
            schema: "aethersense.firmware_sd_upload.v1".into(),
            upload_id: "upload-test".into(),
            device_id: "esp-1".into(),
            session_id: "2026-07-15T10-00-00Z-test".into(),
            filename: "events-0001.jsonl".into(),
            total_bytes: bytes.len() as u64,
            sha256: sha256_hex(bytes),
            contains_raw_csi: false,
            capture_context: None,
            created_at: Utc::now().to_rfc3339(),
        };
        let directory = upload_directory(root.path(), &metadata.upload_id);
        assert_eq!(initialise_upload(&directory, &metadata).unwrap(), 0);
        let mut resumed = metadata.clone();
        resumed.created_at = "2026-07-15T12:00:00Z".into();
        assert_eq!(initialise_upload(&directory, &resumed).unwrap(), 0);
        std::fs::write(directory.join("data.part"), bytes).unwrap();
        let path = finalise_upload(root.path(), &directory, &metadata).unwrap();
        assert!(path.ends_with("events-0001.jsonl"));
        assert!(root
            .path()
            .join("firmware-sd")
            .join(&metadata.session_id)
            .join("session.json")
            .exists());

        assert_eq!(
            initialise_or_deduplicate(root.path(), &directory, &metadata).unwrap(),
            (bytes.len() as u64, true)
        );

        let manifest_bytes =
            br#"{"schema":"aethersense.firmware_sd_session.v1","session_id":"real"}"#;
        let mut manifest = metadata.clone();
        manifest.upload_id = "upload-manifest".into();
        manifest.filename = "session.json".into();
        manifest.total_bytes = manifest_bytes.len() as u64;
        manifest.sha256 = sha256_hex(manifest_bytes);
        let manifest_directory = upload_directory(root.path(), &manifest.upload_id);
        assert_eq!(
            initialise_or_deduplicate(root.path(), &manifest_directory, &manifest).unwrap(),
            (0, false)
        );
        std::fs::write(manifest_directory.join("data.part"), manifest_bytes).unwrap();
        finalise_upload(root.path(), &manifest_directory, &manifest).unwrap();
        let final_manifest = std::fs::read_to_string(
            root.path()
                .join("firmware-sd")
                .join(&manifest.session_id)
                .join("session.json"),
        )
        .unwrap();
        assert!(final_manifest.contains("\"session_id\":\"real\""));
    }

    #[tokio::test]
    async fn snapshot_publico_nao_expoe_token_de_onboarding() {
        let root = tempdir().unwrap();
        let mut inner = InnerState::new_with_config_path(root.path().join("config.json"));
        inner.accessories = AccessoryRegistry::default();
        inner
            .accessories
            .onboard(
                "esp-1".into(),
                "Sala".into(),
                json!({}),
                vec![AccessoryInput {
                    id: "relay-1".into(),
                    kind: AccessoryKind::Relay,
                    name: "Luz".into(),
                    capabilities: json!({}),
                }],
                Some("upload-secret"),
            )
            .unwrap();
        let response = get_accessories(State(Arc::new(RwLock::new(inner)))).await;
        let serialized = response.0.to_string();
        assert!(!serialized.contains("upload-secret"));
        assert!(!serialized.contains("credential"));
    }

    #[test]
    fn bearer_exige_esquema_e_token_exatos() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer server-secret".parse().unwrap(),
        );
        assert!(api_token_matches(&headers, "server-secret"));
        assert!(!api_token_matches(&headers, "server-secrex"));

        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Basic server-secret".parse().unwrap(),
        );
        assert!(!api_token_matches(&headers, "server-secret"));
    }

    #[test]
    fn upload_de_csi_bruto_so_aceita_bancada_ou_sala_vazia() {
        assert!(validate_upload_privacy("csi-0001.csv", false, None).is_err());
        assert!(validate_upload_privacy("csi-0001.csv", false, Some("human_walk")).is_err());
        assert_eq!(
            validate_upload_privacy("csi-0001.csv", false, Some("empty_room")),
            Ok(true)
        );
        assert_eq!(
            validate_upload_privacy("events-0001.jsonl", false, None),
            Ok(false)
        );
    }
}
