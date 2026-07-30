//! USB-serial provisioning contract and atomic A/B profile persistence.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use crate::mqtt::AccessoryCommissioning;
use crate::mqtt::{CommandAck, CommandEnvelope};
use crate::profile::{DeviceProfile, ProfileError};
use crate::status_led::StatusColor;

pub const PROVISION_PROTOCOL: &str = "aethersense.provision.v1";
pub const PROVISION_WIRE_VERSION: u8 = 1;
pub const MAX_PROVISION_PAYLOAD_BYTES: usize = 128 * 1024;
/// Largest raw payload accepted in one JSON-encoded provisioning chunk.
///
/// A 1 KiB `Vec<u8>` made serde's internally tagged enum deserializer request
/// a 16 KiB temporary allocation on ESP32-C5. Keeping chunks at 256 bytes
/// avoids that allocation while retaining bounded, acknowledged transfers.
pub const MAX_PROVISION_CHUNK_BYTES: usize = 128;
pub const MIN_DIAGNOSTIC_STATUS_LED_DURATION_MS: u32 = 100;
pub const MAX_DIAGNOSTIC_STATUS_LED_DURATION_MS: u32 = 5_000;
const SLOT_MAGIC: &[u8; 4] = b"ASCF";
const SLOT_FORMAT_VERSION: u8 = 1;
const SLOT_HEADER_BYTES: usize = 20;
#[cfg(any(test, all(feature = "esp-idf", target_os = "espidf")))]
const SERIAL_READ_ERROR_LOG_EVERY: u32 = 50;

#[cfg(any(test, all(feature = "esp-idf", target_os = "espidf")))]
fn serial_read_count(result: i32, capacity: usize) -> Result<Option<usize>, i32> {
    if result < 0 {
        return Err(result);
    }
    let count = result as usize;
    if count > capacity {
        return Err(result);
    }
    Ok((count != 0).then_some(count))
}

#[cfg(any(test, all(feature = "esp-idf", target_os = "espidf")))]
fn serial_write_is_complete(result: i32, expected: usize) -> bool {
    result >= 0 && result as usize == expected
}

#[cfg(any(test, all(feature = "esp-idf", target_os = "espidf")))]
fn freertos_ticks_for_millis(millis: u32, tick_rate_hz: u32) -> u32 {
    let ticks = (u64::from(millis) * u64::from(tick_rate_hz)).div_ceil(1_000);
    ticks.max(1).min(u64::from(u32::MAX)) as u32
}

#[cfg(any(test, all(feature = "esp-idf", target_os = "espidf")))]
fn should_log_serial_read_error(consecutive_errors: u32) -> bool {
    consecutive_errors == 1 || consecutive_errors % SERIAL_READ_ERROR_LOG_EVERY == 0
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretString(pub String);

impl SecretString {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString([REDACTED])")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisionedSecrets {
    #[serde(default)]
    pub wifi_password: SecretString,
    #[serde(default)]
    pub mqtt_password: SecretString,
    #[serde(default)]
    pub server_bearer_token: SecretString,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisionedConfig {
    #[serde(default = "default_protocol")]
    pub protocol: String,
    #[serde(default)]
    pub build_hash: String,
    pub profile: DeviceProfile,
    #[serde(default)]
    pub secrets: ProvisionedSecrets,
}

impl ProvisionedConfig {
    pub fn validate(&self) -> Result<(), ProvisionError> {
        if self.protocol != PROVISION_PROTOCOL {
            return Err(ProvisionError::Protocol(format!(
                "expected {PROVISION_PROTOCOL}, got {}",
                self.protocol
            )));
        }
        self.profile.validate().map_err(ProvisionError::Profile)
    }

    pub fn validate_for_commit(&self) -> Result<(), ProvisionError> {
        self.validate()?;
        if !is_sha256_hex(&self.build_hash) {
            return Err(ProvisionError::Protocol(
                "build_hash must be 64 hexadecimal characters".to_string(),
            ));
        }
        Ok(())
    }

    pub fn validate_device_identity(&self, expected_device_id: &str) -> Result<(), ProvisionError> {
        match self.profile.identity.device_id.as_deref() {
            Some(device_id) if device_id == expected_device_id => Ok(()),
            Some(device_id) => Err(ProvisionError::Protocol(format!(
                "profile device_id {device_id} does not match device eFuse identity {expected_device_id}"
            ))),
            None => Err(ProvisionError::Protocol(format!(
                "profile device_id is required; use the identity returned by hello ({expected_device_id})"
            ))),
        }
    }
}

/// Stable public identity derived from the ESP base/eFuse MAC.
pub fn device_id_from_mac(mac: [u8; 6]) -> String {
    format!(
        "esp32-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

#[cfg(test)]
fn mac48_from_eui64(eui64: [u8; 8]) -> [u8; 6] {
    [eui64[0], eui64[1], eui64[2], eui64[5], eui64[6], eui64[7]]
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProvisionRequest {
    Hello {
        protocol: String,
        client: String,
    },
    Capabilities,
    CurrentConfig,
    Begin {
        request_id: String,
        total_len: u32,
        crc32: u32,
        sha256: String,
    },
    Chunk {
        request_id: String,
        sequence: u32,
        data: Vec<u8>,
        sha256: String,
    },
    Validate {
        request_id: String,
    },
    Commit {
        request_id: String,
    },
    Abort {
        request_id: String,
    },
    Readback {
        config_hash: String,
        #[serde(default)]
        payload_sha256: Option<String>,
    },
    AccessoryCommand {
        command: CommandEnvelope,
    },
    AccessoryResult {
        request_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProvisionResponse {
    Hello {
        protocol: String,
        device_id: Option<String>,
        firmware_version: String,
        build_hash: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        boot_id: Option<String>,
    },
    Capabilities {
        profile_schema: String,
        max_chunk_bytes: u32,
        atomic_slots: u8,
        features: Vec<String>,
    },
    Ack {
        request_id: String,
        sequence: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sha256: Option<String>,
    },
    Valid {
        request_id: String,
        config_hash: String,
        payload_sha256: String,
    },
    Committed {
        request_id: String,
        config_hash: String,
        payload_sha256: String,
        generation: u32,
    },
    CurrentConfig {
        profile: Option<DeviceProfile>,
        config_hash: Option<String>,
        payload_sha256: Option<String>,
        generation: Option<u32>,
    },
    AccessoryResult {
        ack: CommandAck,
    },
    Error {
        request_id: Option<String>,
        code: String,
        message: String,
    },
}

#[cfg(any(test, all(feature = "esp-idf", target_os = "espidf")))]
fn capabilities_response(features: &[String]) -> ProvisionResponse {
    ProvisionResponse::Capabilities {
        profile_schema: crate::profile::DEVICE_PROFILE_SCHEMA.to_string(),
        max_chunk_bytes: MAX_PROVISION_CHUNK_BYTES as u32,
        atomic_slots: 2,
        features: features.to_vec(),
    }
}

#[cfg(any(test, all(feature = "esp-idf", target_os = "espidf")))]
fn validate_diagnostic_status_led(
    color: &str,
    duration_ms: u32,
) -> Result<StatusColor, String> {
    if !(MIN_DIAGNOSTIC_STATUS_LED_DURATION_MS..=MAX_DIAGNOSTIC_STATUS_LED_DURATION_MS)
        .contains(&duration_ms)
    {
        return Err(format!(
            "status LED diagnostic duration must be between {MIN_DIAGNOSTIC_STATUS_LED_DURATION_MS} and {MAX_DIAGNOSTIC_STATUS_LED_DURATION_MS} ms"
        ));
    }
    StatusColor::from_diagnostic_name(color).ok_or_else(|| {
        format!(
            "unsupported status LED diagnostic color {color:?}; use a canonical StatusColor name"
        )
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProvisionError {
    EmptyFrame,
    FrameTooLarge,
    InvalidCobs,
    UnsupportedWireVersion(u8),
    CrcMismatch { expected: u32, actual: u32 },
    Json(String),
    Protocol(String),
    Profile(ProfileError),
    Storage(String),
    NoActiveConfig,
    InvalidSlot(String),
    Staging(String),
}

impl std::fmt::Display for ProvisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyFrame => f.write_str("empty provisioning frame"),
            Self::FrameTooLarge => f.write_str("provisioning frame exceeds limit"),
            Self::InvalidCobs => f.write_str("invalid COBS frame"),
            Self::UnsupportedWireVersion(version) => {
                write!(f, "unsupported wire version {version}")
            }
            Self::CrcMismatch { expected, actual } => {
                write!(f, "CRC mismatch: expected {expected:08x}, got {actual:08x}")
            }
            Self::Json(message)
            | Self::Protocol(message)
            | Self::Storage(message)
            | Self::InvalidSlot(message)
            | Self::Staging(message) => f.write_str(message),
            Self::Profile(error) => error.fmt(f),
            Self::NoActiveConfig => f.write_str("no valid active configuration"),
        }
    }
}

impl std::error::Error for ProvisionError {}

pub struct ProvisionCodec;

impl ProvisionCodec {
    /// Encodes `version || JSON || CRC32` using COBS and appends a zero delimiter.
    pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, ProvisionError> {
        let json =
            serde_json::to_vec(message).map_err(|err| ProvisionError::Json(err.to_string()))?;
        if json.len() > MAX_PROVISION_PAYLOAD_BYTES {
            return Err(ProvisionError::FrameTooLarge);
        }
        let mut body = Vec::with_capacity(json.len() + 5);
        body.push(PROVISION_WIRE_VERSION);
        body.extend_from_slice(&json);
        body.extend_from_slice(&crc32(&body).to_le_bytes());
        Ok(cobs_encode(&body))
    }

    pub fn decode<T: DeserializeOwned>(frame: &[u8]) -> Result<T, ProvisionError> {
        let decoded = cobs_decode(frame)?;
        if decoded.len() < 5 {
            return Err(ProvisionError::EmptyFrame);
        }
        if decoded.len() > MAX_PROVISION_PAYLOAD_BYTES + 5 {
            return Err(ProvisionError::FrameTooLarge);
        }
        let body_len = decoded.len() - 4;
        let expected = u32::from_le_bytes(decoded[body_len..].try_into().unwrap());
        let actual = crc32(&decoded[..body_len]);
        if expected != actual {
            return Err(ProvisionError::CrcMismatch { expected, actual });
        }
        let version = decoded[0];
        if version != PROVISION_WIRE_VERSION {
            return Err(ProvisionError::UnsupportedWireVersion(version));
        }
        serde_json::from_slice(&decoded[1..body_len])
            .map_err(|err| ProvisionError::Json(err.to_string()))
    }
}

#[derive(Clone, Debug)]
struct IncomingTransfer {
    request_id: String,
    total_len: usize,
    expected_crc: u32,
    expected_sha256: String,
    next_sequence: u32,
    payload: Vec<u8>,
}

#[derive(Default)]
pub struct ProvisionStagingReceiver {
    transfer: Option<IncomingTransfer>,
}

impl ProvisionStagingReceiver {
    pub fn begin(
        &mut self,
        request_id: impl Into<String>,
        total_len: usize,
        expected_crc: u32,
        expected_sha256: impl Into<String>,
    ) -> Result<(), ProvisionError> {
        if total_len == 0 || total_len > MAX_PROVISION_PAYLOAD_BYTES {
            return Err(ProvisionError::Staging(
                "invalid transfer length".to_string(),
            ));
        }
        let expected_sha256 = expected_sha256.into();
        if !is_sha256_hex(&expected_sha256) {
            return Err(ProvisionError::Staging(
                "payload sha256 must be 64 hexadecimal characters".to_string(),
            ));
        }
        self.transfer = Some(IncomingTransfer {
            request_id: request_id.into(),
            total_len,
            expected_crc,
            expected_sha256,
            next_sequence: 0,
            payload: Vec::with_capacity(total_len),
        });
        Ok(())
    }

    pub fn push_chunk(
        &mut self,
        request_id: &str,
        sequence: u32,
        data: &[u8],
        chunk_sha256: &str,
    ) -> Result<(), ProvisionError> {
        if data.len() > MAX_PROVISION_CHUNK_BYTES {
            return Err(ProvisionError::Staging(format!(
                "chunk exceeds {MAX_PROVISION_CHUNK_BYTES}-byte device limit"
            )));
        }
        if !is_sha256_hex(chunk_sha256) {
            return Err(ProvisionError::Staging(
                "chunk sha256 must be 64 hexadecimal characters".to_string(),
            ));
        }
        let transfer = self
            .transfer
            .as_mut()
            .ok_or_else(|| ProvisionError::Staging("no transfer in progress".to_string()))?;
        if transfer.request_id != request_id {
            return Err(ProvisionError::Staging(
                "request_id does not match active transfer".to_string(),
            ));
        }
        if sequence != transfer.next_sequence {
            return Err(ProvisionError::Staging(format!(
                "expected sequence {}, got {sequence}",
                transfer.next_sequence
            )));
        }
        if transfer.payload.len().saturating_add(data.len()) > transfer.total_len {
            return Err(ProvisionError::Staging(
                "chunk exceeds declared length".to_string(),
            ));
        }
        let actual_chunk_sha256 = sha256_hex(data);
        if !actual_chunk_sha256.eq_ignore_ascii_case(chunk_sha256) {
            return Err(ProvisionError::Staging(format!(
                "chunk SHA-256 mismatch: expected {chunk_sha256}, got {actual_chunk_sha256}"
            )));
        }
        transfer.payload.extend_from_slice(data);
        transfer.next_sequence += 1;
        Ok(())
    }

    pub fn validate(&self, request_id: &str) -> Result<ProvisionedConfig, ProvisionError> {
        let transfer = self
            .transfer
            .as_ref()
            .ok_or_else(|| ProvisionError::Staging("no transfer in progress".to_string()))?;
        if transfer.request_id != request_id {
            return Err(ProvisionError::Staging(
                "request_id does not match active transfer".to_string(),
            ));
        }
        if transfer.payload.len() != transfer.total_len {
            return Err(ProvisionError::Staging(format!(
                "incomplete transfer: {} of {} bytes",
                transfer.payload.len(),
                transfer.total_len
            )));
        }
        let actual = crc32(&transfer.payload);
        if actual != transfer.expected_crc {
            return Err(ProvisionError::CrcMismatch {
                expected: transfer.expected_crc,
                actual,
            });
        }
        let actual_sha256 = sha256_hex(&transfer.payload);
        if !actual_sha256.eq_ignore_ascii_case(&transfer.expected_sha256) {
            return Err(ProvisionError::Staging(format!(
                "payload SHA-256 mismatch: expected {}, got {actual_sha256}",
                transfer.expected_sha256
            )));
        }
        let config: ProvisionedConfig = serde_json::from_slice(&transfer.payload)
            .map_err(|err| ProvisionError::Json(err.to_string()))?;
        config.validate_for_commit()?;
        Ok(config)
    }

    /// Returns the exact validated wire payload so the NVS slot preserves the
    /// client-side canonical JSON and its SHA-256 across commit/readback.
    pub fn payload(&self, request_id: &str) -> Result<&[u8], ProvisionError> {
        let transfer = self
            .transfer
            .as_ref()
            .ok_or_else(|| ProvisionError::Staging("no transfer in progress".to_string()))?;
        if transfer.request_id != request_id {
            return Err(ProvisionError::Staging(
                "request_id does not match active transfer".to_string(),
            ));
        }
        if transfer.payload.len() != transfer.total_len {
            return Err(ProvisionError::Staging(
                "transfer has not been completely received".to_string(),
            ));
        }
        Ok(&transfer.payload)
    }

    pub fn abort(&mut self, request_id: &str) -> bool {
        if self
            .transfer
            .as_ref()
            .is_some_and(|transfer| transfer.request_id == request_id)
        {
            self.transfer = None;
            true
        } else {
            false
        }
    }

    pub fn finish(&mut self, request_id: &str) -> Result<(), ProvisionError> {
        if self.abort(request_id) {
            Ok(())
        } else {
            Err(ProvisionError::Staging(
                "request_id does not match active transfer".to_string(),
            ))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigSlot {
    A,
    B,
}

impl ConfigSlot {
    fn opposite(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }
}

pub trait ConfigStorage {
    fn read_slot(&self, slot: ConfigSlot) -> Result<Option<Vec<u8>>, String>;
    fn write_slot(&mut self, slot: ConfigSlot, bytes: &[u8]) -> Result<(), String>;
    fn erase_slot(&mut self, slot: ConfigSlot) -> Result<(), String>;
    fn active_slot(&self) -> Result<Option<ConfigSlot>, String>;
    fn set_active_slot(&mut self, slot: ConfigSlot) -> Result<(), String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredConfig {
    pub slot: ConfigSlot,
    pub generation: u32,
    pub config_hash: String,
    pub payload_sha256: String,
    pub config: ProvisionedConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedConfig {
    pub slot: ConfigSlot,
    pub generation: u32,
    pub config_hash: String,
    pub payload_sha256: String,
}

pub struct AtomicConfigStore<S> {
    storage: S,
}

impl<S: ConfigStorage> AtomicConfigStore<S> {
    pub fn new(storage: S) -> Self {
        Self { storage }
    }

    pub fn into_inner(self) -> S {
        self.storage
    }

    pub fn load_active(&self) -> Result<StoredConfig, ProvisionError> {
        let active = self
            .storage
            .active_slot()
            .map_err(ProvisionError::Storage)?
            .ok_or(ProvisionError::NoActiveConfig)?;

        // The marker is the atomic commit point. With no marker, a valid slot
        // can only be an interrupted staging write and must never become live.
        // When a marker does exist, recover from a missing/corrupt active slot
        // using only the opposite slot, without mutating the marker at boot.
        let active_error = match self.read_valid(active) {
            Ok(Some(config)) => return Ok(config),
            Ok(None) => None,
            Err(error @ ProvisionError::Storage(_)) => return Err(error),
            Err(error) => Some(error),
        };
        match self.read_valid(active.opposite()) {
            Ok(Some(config)) => Ok(config),
            Ok(None) => Err(active_error.unwrap_or(ProvisionError::NoActiveConfig)),
            Err(error) => Err(error),
        }
    }

    pub fn stage(&mut self, config: &ProvisionedConfig) -> Result<StagedConfig, ProvisionError> {
        config.validate_for_commit()?;
        let payload =
            serde_json::to_vec(config).map_err(|err| ProvisionError::Json(err.to_string()))?;
        self.stage_validated_payload(&payload, config)
    }

    /// Stages the exact JSON bytes received over serial. Parsing and validation
    /// happen before the inactive slot is written, while retaining canonical
    /// byte identity for SHA-256 readback.
    pub fn stage_payload(&mut self, payload: &[u8]) -> Result<StagedConfig, ProvisionError> {
        let config: ProvisionedConfig =
            serde_json::from_slice(payload).map_err(|err| ProvisionError::Json(err.to_string()))?;
        config.validate_for_commit()?;
        self.stage_validated_payload(payload, &config)
    }

    /// Stages payload bytes whose parsed `ProvisionedConfig` has already been
    /// validated by the caller. The payload and config must describe the same
    /// value; `commit` independently verifies that invariant before cut-over.
    ///
    /// This path avoids parsing a second deeply nested profile during serial
    /// `Validate`, and verifies the NVS write as raw bytes instead.
    pub fn stage_validated_payload(
        &mut self,
        payload: &[u8],
        config: &ProvisionedConfig,
    ) -> Result<StagedConfig, ProvisionError> {
        let active = self
            .storage
            .active_slot()
            .map_err(ProvisionError::Storage)?;
        let slot = active.map(ConfigSlot::opposite).unwrap_or(ConfigSlot::A);
        let mut newest_generation = 0;
        for candidate in [ConfigSlot::A, ConfigSlot::B] {
            if let Some(generation) = self.read_record_generation(candidate)? {
                newest_generation = newest_generation.max(generation);
            }
        }
        let generation = newest_generation.saturating_add(1);
        let bytes = encode_slot_record(generation, payload)?;
        self.storage
            .write_slot(slot, &bytes)
            .map_err(ProvisionError::Storage)?;
        drop(bytes);
        self.verify_staged_payload(slot, generation, payload)?;

        let config_hash = config
            .profile
            .config_hash()
            .map_err(|err| ProvisionError::Json(err.to_string()))?;
        let payload_sha256 = sha256_hex(payload);
        Ok(StagedConfig {
            slot,
            generation,
            config_hash,
            payload_sha256,
        })
    }

    pub fn commit(&mut self, staged: &StagedConfig) -> Result<StoredConfig, ProvisionError> {
        let readback = self
            .read_valid(staged.slot)?
            .ok_or_else(|| ProvisionError::InvalidSlot("staged slot is invalid".to_string()))?;
        if readback.generation != staged.generation
            || readback.config_hash != staged.config_hash
            || readback.payload_sha256 != staged.payload_sha256
        {
            return Err(ProvisionError::InvalidSlot(
                "staged slot readback does not match validation".to_string(),
            ));
        }
        // This single marker write is the atomic cut-over. Until it succeeds,
        // the old slot remains authoritative.
        self.storage
            .set_active_slot(staged.slot)
            .map_err(ProvisionError::Storage)?;
        Ok(readback)
    }

    pub fn abort(&mut self, staged: &StagedConfig) -> Result<(), ProvisionError> {
        if self
            .storage
            .active_slot()
            .map_err(ProvisionError::Storage)?
            == Some(staged.slot)
        {
            return Err(ProvisionError::Storage(
                "refusing to erase the active slot".to_string(),
            ));
        }
        self.storage
            .erase_slot(staged.slot)
            .map_err(ProvisionError::Storage)
    }

    fn read_valid(&self, slot: ConfigSlot) -> Result<Option<StoredConfig>, ProvisionError> {
        let Some(bytes) = self
            .storage
            .read_slot(slot)
            .map_err(ProvisionError::Storage)?
        else {
            return Ok(None);
        };
        if bytes.is_empty() {
            return Ok(None);
        }
        let (generation, payload) = decode_slot_record(&bytes)?;
        let config: ProvisionedConfig = serde_json::from_slice(payload)
            .map_err(|err| ProvisionError::InvalidSlot(err.to_string()))?;
        config.validate()?;
        let config_hash = config
            .profile
            .config_hash()
            .map_err(|err| ProvisionError::Json(err.to_string()))?;
        let payload_sha256 = sha256_hex(payload);
        Ok(Some(StoredConfig {
            slot,
            generation,
            config_hash,
            payload_sha256,
            config,
        }))
    }

    fn read_record_generation(&self, slot: ConfigSlot) -> Result<Option<u32>, ProvisionError> {
        let Some(bytes) = self
            .storage
            .read_slot(slot)
            .map_err(ProvisionError::Storage)?
        else {
            return Ok(None);
        };
        if bytes.is_empty() {
            return Ok(None);
        }
        let (generation, _) = decode_slot_record(&bytes)?;
        Ok(Some(generation))
    }

    fn verify_staged_payload(
        &self,
        slot: ConfigSlot,
        expected_generation: u32,
        expected_payload: &[u8],
    ) -> Result<(), ProvisionError> {
        let bytes = self
            .storage
            .read_slot(slot)
            .map_err(ProvisionError::Storage)?
            .ok_or_else(|| ProvisionError::InvalidSlot("staged slot disappeared".to_string()))?;
        let (generation, payload) = decode_slot_record(&bytes)?;
        if generation != expected_generation || payload != expected_payload {
            return Err(ProvisionError::InvalidSlot(
                "staged slot readback does not match written payload".to_string(),
            ));
        }
        Ok(())
    }
}

fn encode_slot_record(generation: u32, payload: &[u8]) -> Result<Vec<u8>, ProvisionError> {
    if payload.len() > MAX_PROVISION_PAYLOAD_BYTES {
        return Err(ProvisionError::FrameTooLarge);
    }
    let mut bytes = Vec::with_capacity(SLOT_HEADER_BYTES + payload.len());
    bytes.extend_from_slice(SLOT_MAGIC);
    bytes.push(SLOT_FORMAT_VERSION);
    bytes.extend_from_slice(&[0, 0, 0]);
    bytes.extend_from_slice(&generation.to_le_bytes());
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&crc32(payload).to_le_bytes());
    bytes.extend_from_slice(payload);
    Ok(bytes)
}

fn decode_slot_record(bytes: &[u8]) -> Result<(u32, &[u8]), ProvisionError> {
    if bytes.len() < SLOT_HEADER_BYTES || &bytes[..4] != SLOT_MAGIC {
        return Err(ProvisionError::InvalidSlot("bad slot header".to_string()));
    }
    if bytes[4] != SLOT_FORMAT_VERSION {
        return Err(ProvisionError::InvalidSlot(
            "unsupported slot version".to_string(),
        ));
    }
    let generation = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let payload_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let expected = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
    if payload_len > MAX_PROVISION_PAYLOAD_BYTES || bytes.len() != SLOT_HEADER_BYTES + payload_len {
        return Err(ProvisionError::InvalidSlot("bad slot length".to_string()));
    }
    let payload = &bytes[SLOT_HEADER_BYTES..];
    let actual = crc32(payload);
    if expected != actual {
        return Err(ProvisionError::CrcMismatch { expected, actual });
    }
    Ok((generation, payload))
}

/// IEEE CRC-32 used for serial frames, transfer validation and slot records.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

/// Lowercase SHA-256 used by chunk, payload and readback verification.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String is infallible");
    }
    output
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn cobs_encode(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len() + input.len() / 254 + 2);
    let mut code_index = 0;
    let mut code = 1u8;
    output.push(0);
    for byte in input {
        if *byte == 0 {
            output[code_index] = code;
            code_index = output.len();
            output.push(0);
            code = 1;
        } else {
            output.push(*byte);
            code = code.wrapping_add(1);
            if code == 0xff {
                output[code_index] = code;
                code_index = output.len();
                output.push(0);
                code = 1;
            }
        }
    }
    output[code_index] = code;
    output.push(0);
    output
}

fn cobs_decode(frame: &[u8]) -> Result<Vec<u8>, ProvisionError> {
    let frame = frame.strip_suffix(&[0]).unwrap_or(frame);
    if frame.is_empty() {
        return Err(ProvisionError::EmptyFrame);
    }
    let mut output = Vec::with_capacity(frame.len());
    let mut index = 0usize;
    while index < frame.len() {
        let code = frame[index] as usize;
        if code == 0 {
            return Err(ProvisionError::InvalidCobs);
        }
        index += 1;
        let end = index.saturating_add(code - 1);
        if end > frame.len() {
            return Err(ProvisionError::InvalidCobs);
        }
        output.extend_from_slice(&frame[index..end]);
        index = end;
        if code != 0xff && index < frame.len() {
            output.push(0);
        }
    }
    Ok(output)
}

fn default_protocol() -> String {
    PROVISION_PROTOCOL.to_string()
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub mod esp_nvs {
    use super::*;
    use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
    use std::io::Write;
    use std::thread::JoinHandle;

    use crate::status_led::StatusLed;

    const NAMESPACE: &str = "aethercfg";
    const SLOT_A_KEY: &str = "cfg_a";
    const SLOT_B_KEY: &str = "cfg_b";
    const ACTIVE_KEY: &str = "cfg_active";
    // A provisioning chunk is JSON-encoded as an integer array on the wire.
    // Keep enough headroom in the ISR-backed RX ring for the encoded 256-byte
    // chunk plus concurrent console output while the worker validates it.
    const SERIAL_UART_RX_BUFFER_BYTES: i32 = 4 * 1024;
    const SERIAL_UART_READ_TIMEOUT_MILLIS: u32 = 50;
    // Eight KiB overflowed by roughly 180 bytes on C5. The low-memory hashing
    // and validated-payload staging paths keep large profile buffers on the
    // heap, so 16 KiB retains measured headroom without starving Wi-Fi startup.
    const SERIAL_SERVER_STACK_BYTES: usize = 16 * 1024;

    fn diagnostic_status_led_ack(
        command: &CommandEnvelope,
        commissioning: &AccessoryCommissioning,
        status_led: &StatusLed,
    ) -> Option<CommandAck> {
        let crate::mqtt::AccessoryCommand::DiagnosticStatusLed { color, duration_ms } =
            &command.command
        else {
            return None;
        };
        if let Some(actual) = command.boot_id.as_ref() {
            if actual != commissioning.boot_id() {
                return Some(CommandAck::error(
                    command,
                    commissioning.boot_id(),
                    format!(
                        "command boot_id {actual} does not match {}",
                        commissioning.boot_id()
                    ),
                ));
            }
        }
        let requested = match validate_diagnostic_status_led(color, *duration_ms) {
            Ok(requested) => requested,
            Err(error) => {
                return Some(CommandAck::error(
                    command,
                    commissioning.boot_id(),
                    error,
                ))
            }
        };
        if !status_led.is_available() {
            return Some(CommandAck::error(
                command,
                commissioning.boot_id(),
                "status LED is disabled or unavailable on this board",
            ));
        }
        if !status_led.diagnostic_pulse(requested, u64::from(*duration_ms)) {
            return Some(CommandAck::error(
                command,
                commissioning.boot_id(),
                "status LED is unavailable while the device is in an error state",
            ));
        }
        Some(CommandAck::completed_with_state(
            command,
            commissioning.boot_id(),
            serde_json::json!({
                "event": "diagnostic.status_led",
                "color": requested.diagnostic_name(),
                "duration_ms": duration_ms,
                "scheduled": true,
                "hardware_observation_required": true,
            }),
        ))
    }

    fn diagnostic_runtime_ack(
        command: &CommandEnvelope,
        commissioning: &AccessoryCommissioning,
    ) -> Option<CommandAck> {
        if !matches!(
            &command.command,
            crate::mqtt::AccessoryCommand::DiagnosticRuntime
        ) {
            return None;
        }
        if let Some(actual) = command.boot_id.as_ref() {
            if actual != commissioning.boot_id() {
                return Some(CommandAck::error(
                    command,
                    commissioning.boot_id(),
                    format!(
                        "command boot_id {actual} does not match {}",
                        commissioning.boot_id()
                    ),
                ));
            }
        }
        Some(match commissioning.runtime_diagnostics() {
            Ok(state) => CommandAck::completed_with_state(
                command,
                commissioning.boot_id(),
                state,
            ),
            Err(error) => CommandAck::error(command, commissioning.boot_id(), error),
        })
    }

    fn diagnostic_reboot_ack(
        command: &CommandEnvelope,
        commissioning: &AccessoryCommissioning,
    ) -> Option<CommandAck> {
        if !matches!(
            &command.command,
            crate::mqtt::AccessoryCommand::DiagnosticReboot
        ) {
            return None;
        }
        if let Some(actual) = command.boot_id.as_ref() {
            if actual != commissioning.boot_id() {
                return Some(CommandAck::error(
                    command,
                    commissioning.boot_id(),
                    "diagnostic reboot rejected because boot_id is stale",
                ));
            }
        }
        let scheduled = std::thread::Builder::new()
            .name("aether-reboot".to_string())
            .stack_size(2 * 1024)
            .spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(350));
                unsafe { esp_idf_sys::esp_restart() };
            });
        Some(match scheduled {
            Ok(_handle) => CommandAck::completed_with_state(
                command,
                commissioning.boot_id(),
                serde_json::json!({
                    "event": "diagnostic.reboot",
                    "scheduled": true,
                    "delay_ms": 350,
                }),
            ),
            Err(error) => CommandAck::error(
                command,
                commissioning.boot_id(),
                format!("diagnostic reboot task could not start: {error}"),
            ),
        })
    }

    fn enable_console_uart_driver() -> Result<(), String> {
        let uart_num = esp_idf_sys::CONFIG_ESP_CONSOLE_UART_NUM as esp_idf_sys::uart_port_t;

        // The default ESP console VFS performs non-blocking reads directly
        // from the UART FIFO. It consequently reports EAGAIN whenever no byte
        // happens to be present at that instant. Provisioning needs the UART
        // driver's interrupt-backed RX ring so stdin can wait for complete
        // frames without racing the host.
        let already_installed = unsafe { esp_idf_sys::uart_is_driver_installed(uart_num) };
        if !already_installed {
            let result = unsafe {
                esp_idf_sys::uart_driver_install(
                    uart_num,
                    SERIAL_UART_RX_BUFFER_BYTES,
                    0,
                    0,
                    std::ptr::null_mut(),
                    0,
                )
            };
            // ESP_ERR_INVALID_STATE means another runtime component won the
            // harmless install race. In either success case, attach VFS below.
            if result != esp_idf_sys::ESP_OK && result != esp_idf_sys::ESP_ERR_INVALID_STATE {
                return Err(format!(
                    "failed to install console UART driver on UART {uart_num}: esp_err_t {result}"
                ));
            }
        }

        // Drain text already buffered by stdout before switching the console
        // VFS. A zero-sized driver TX ring preserves the existing synchronous
        // stdout/log behavior while enabling interrupt-driven stdin reads.
        let _ = std::io::stdout().flush();
        let uart_num = uart_num as i32;
        // COBS excludes zero bytes, but CR and LF are valid payload/CRC bytes.
        // The sdkconfig console defaults translate both directions, so force
        // LF mode, which ESP-IDF defines as byte-for-byte passthrough.
        let raw_line_endings = esp_idf_sys::esp_line_endings_t_ESP_LINE_ENDINGS_LF;
        let rx_result = unsafe {
            esp_idf_sys::esp_vfs_dev_uart_port_set_rx_line_endings(uart_num, raw_line_endings)
        };
        if rx_result != 0 {
            return Err(format!(
                "failed to disable console UART RX line-ending conversion on UART {uart_num}"
            ));
        }
        let tx_result = unsafe {
            esp_idf_sys::esp_vfs_dev_uart_port_set_tx_line_endings(uart_num, raw_line_endings)
        };
        if tx_result != 0 {
            return Err(format!(
                "failed to disable console UART TX line-ending conversion on UART {uart_num}"
            ));
        }
        unsafe { esp_idf_sys::esp_vfs_dev_uart_use_driver(uart_num) };
        Ok(())
    }

    pub struct NvsConfigStorage {
        store: EspNvs<NvsDefault>,
    }

    impl NvsConfigStorage {
        pub fn new(partition: EspDefaultNvsPartition) -> Result<Self, String> {
            EspNvs::new(partition, NAMESPACE, true)
                .map(|store| Self { store })
                .map_err(|err| err.to_string())
        }

        fn key(slot: ConfigSlot) -> &'static str {
            match slot {
                ConfigSlot::A => SLOT_A_KEY,
                ConfigSlot::B => SLOT_B_KEY,
            }
        }
    }

    impl ConfigStorage for NvsConfigStorage {
        fn read_slot(&self, slot: ConfigSlot) -> Result<Option<Vec<u8>>, String> {
            let key = Self::key(slot);
            log::warn!("provisioning NVS: querying blob length for slot {slot:?}");
            let Some(len) = self.store.blob_len(key).map_err(|err| err.to_string())? else {
                log::warn!("provisioning NVS: slot {slot:?} is empty");
                return Ok(None);
            };
            log::warn!("provisioning NVS: reading slot {slot:?} ({len} bytes)");
            let mut bytes = vec![0; len];
            let value = self
                .store
                .get_blob(key, &mut bytes)
                .map_err(|err| err.to_string())?;
            log::warn!("provisioning NVS: slot {slot:?} read completed");
            Ok(value.map(|value| value.to_vec()))
        }

        fn write_slot(&mut self, slot: ConfigSlot, bytes: &[u8]) -> Result<(), String> {
            self.store
                .set_blob(Self::key(slot), bytes)
                .map_err(|err| err.to_string())
        }

        fn erase_slot(&mut self, slot: ConfigSlot) -> Result<(), String> {
            self.store
                .remove(Self::key(slot))
                .map(|_| ())
                .map_err(|err| err.to_string())
        }

        fn active_slot(&self) -> Result<Option<ConfigSlot>, String> {
            log::warn!("provisioning NVS: reading active-slot marker");
            let value = self
                .store
                .get_u8(ACTIVE_KEY)
                .map_err(|err| err.to_string())?;
            let active = match value {
                Some(0) => Some(ConfigSlot::A),
                Some(1) => Some(ConfigSlot::B),
                _ => None,
            };
            log::warn!("provisioning NVS: active-slot marker is {active:?}");
            Ok(active)
        }

        fn set_active_slot(&mut self, slot: ConfigSlot) -> Result<(), String> {
            self.store
                .set_u8(ACTIVE_KEY, if slot == ConfigSlot::A { 0 } else { 1 })
                .map_err(|err| err.to_string())
        }
    }

    pub fn load_active(
        partition: EspDefaultNvsPartition,
    ) -> Result<Option<StoredConfig>, ProvisionError> {
        let storage = NvsConfigStorage::new(partition).map_err(ProvisionError::Storage)?;
        match AtomicConfigStore::new(storage).load_active() {
            Ok(config) => Ok(Some(config)),
            Err(ProvisionError::NoActiveConfig) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Runs the `aethersense.provision.v1` protocol over the ESP console UART.
    /// Responses are prefixed with a zero delimiter so a host can discard any
    /// interleaved text logs and synchronize on the following COBS frame.
    pub fn spawn_serial_server(
        partition: EspDefaultNvsPartition,
        build_hash: String,
        features: Vec<String>,
        status_led: StatusLed,
        commissioning: AccessoryCommissioning,
    ) -> Result<JoinHandle<()>, String> {
        enable_console_uart_driver()?;
        let uart_num = esp_idf_sys::CONFIG_ESP_CONSOLE_UART_NUM as esp_idf_sys::uart_port_t;
        let read_timeout_ticks = freertos_ticks_for_millis(
            SERIAL_UART_READ_TIMEOUT_MILLIS,
            esp_idf_sys::configTICK_RATE_HZ,
        ) as esp_idf_sys::TickType_t;
        // `esp_efuse_mac_get_default` writes 8 bytes on IEEE 802.15.4 targets
        // such as ESP32-C5, but only 6 on classic ESP32. `esp_read_mac` with
        // WIFI_STA has a stable six-byte contract on both and matches the MAC
        // reported by espflash/device association.
        let mut wifi_sta_mac = [0u8; 6];
        esp_idf_sys::esp!(unsafe {
            esp_idf_sys::esp_read_mac(
                wifi_sta_mac.as_mut_ptr(),
                esp_idf_sys::esp_mac_type_t_ESP_MAC_WIFI_STA,
            )
        })
        .map_err(|error| error.to_string())?;
        let device_id = device_id_from_mac(wifi_sta_mac);
        std::thread::Builder::new()
            .name("aether-provision".to_string())
            .stack_size(SERIAL_SERVER_STACK_BYTES)
            .spawn(move || {
                // Open the task-owned NVS handle in the same pthread that
                // performs every subsequent NVS operation. This avoids
                // transferring a live IDF handle from `main_task` while Wi-Fi
                // initializes and begins using the same default partition.
                let storage = match NvsConfigStorage::new(partition) {
                    Ok(storage) => storage,
                    Err(error) => {
                        log::warn!("provisioning NVS worker initialization failed: {error}");
                        return;
                    }
                };
                log::warn!("provisioning NVS handle opened in worker task");
                let mut store = AtomicConfigStore::new(storage);
                let mut receiver = ProvisionStagingReceiver::default();
                let mut pending: Option<(String, StagedConfig)> = None;
                let mut encoded = Vec::with_capacity(1_024);
                let mut buffer = [0u8; 256];
                let mut consecutive_read_errors = 0u32;
                let mut first_rx_batch_logged = false;

                loop {
                    let read_result = unsafe {
                        esp_idf_sys::uart_read_bytes(
                            uart_num,
                            buffer.as_mut_ptr().cast(),
                            buffer.len() as u32,
                            read_timeout_ticks,
                        )
                    };
                    let count = match serial_read_count(read_result, buffer.len()) {
                        Ok(None) => {
                            consecutive_read_errors = 0;
                            continue;
                        }
                        Ok(Some(count)) => {
                            consecutive_read_errors = 0;
                            count
                        }
                        Err(error_code) => {
                            consecutive_read_errors = consecutive_read_errors.saturating_add(1);
                            if should_log_serial_read_error(consecutive_read_errors) {
                                log::warn!(
                                    "provisioning UART read failed (attempt {}, result={})",
                                    consecutive_read_errors,
                                    error_code
                                );
                            }
                            std::thread::sleep(std::time::Duration::from_millis(100));
                            continue;
                        }
                    };
                    if !first_rx_batch_logged {
                        log::warn!(
                            "provisioning UART RX active: received first batch ({} bytes)",
                            count
                        );
                        first_rx_batch_logged = true;
                    }

                    for byte in &buffer[..count] {
                        if *byte != 0 {
                            if encoded.len() < MAX_PROVISION_PAYLOAD_BYTES + 1024 {
                                encoded.push(*byte);
                            } else {
                                encoded.clear();
                            }
                            continue;
                        }
                        if encoded.is_empty() {
                            continue;
                        }
                        encoded.push(0);
                        let response = match ProvisionCodec::decode::<ProvisionRequest>(&encoded) {
                            Ok(request) => {
                                status_led.pulse(StatusColor::Provisioning, 500);
                                process_request(
                                    request,
                                    &mut receiver,
                                    &mut store,
                                    &mut pending,
                                    &build_hash,
                                    &features,
                                    &device_id,
                                    &commissioning,
                                    &status_led,
                                )
                            }
                            Err(error) => {
                                log::warn!(
                                    "provisioning UART frame decode failed; returning invalid_frame"
                                );
                                ProvisionResponse::Error {
                                    request_id: None,
                                    code: "invalid_frame".to_string(),
                                    message: error.to_string(),
                                }
                            }
                        };
                        encoded.clear();
                        let is_current_config =
                            matches!(&response, ProvisionResponse::CurrentConfig { .. });
                        if is_current_config {
                            log::warn!("provisioning current-config response encode started");
                        }

                        match ProvisionCodec::encode(&response) {
                            Ok(frame) => {
                                if is_current_config {
                                    log::warn!(
                                        "provisioning current-config response encoded ({} bytes)",
                                        frame.len()
                                    );
                                }
                                let mut synchronized = Vec::with_capacity(frame.len() + 1);
                                synchronized.push(0);
                                synchronized.extend_from_slice(&frame);
                                let written = unsafe {
                                    esp_idf_sys::uart_write_bytes(
                                        uart_num,
                                        synchronized.as_ptr().cast(),
                                        synchronized.len(),
                                    )
                                };
                                if !serial_write_is_complete(written, synchronized.len()) {
                                    log::warn!(
                                        "provisioning UART response write incomplete: expected={} written={}",
                                        synchronized.len(),
                                        written
                                    );
                                } else if is_current_config {
                                    log::warn!(
                                        "provisioning current-config response write completed ({} bytes)",
                                        synchronized.len()
                                    );
                                }
                            }
                            Err(error) => {
                                log::warn!("provisioning response encode failed: {error}")
                            }
                        }
                    }
                }
            })
            .map_err(|error| error.to_string())
    }

    fn process_request(
        request: ProvisionRequest,
        receiver: &mut ProvisionStagingReceiver,
        store: &mut AtomicConfigStore<NvsConfigStorage>,
        pending: &mut Option<(String, StagedConfig)>,
        build_hash: &str,
        features: &[String],
        device_id: &str,
        commissioning: &AccessoryCommissioning,
        status_led: &StatusLed,
    ) -> ProvisionResponse {
        let result: Result<ProvisionResponse, ProvisionError> = (|| match request {
            ProvisionRequest::Hello { protocol, .. } => {
                if protocol != PROVISION_PROTOCOL {
                    return Err(ProvisionError::Protocol(format!(
                        "expected protocol {PROVISION_PROTOCOL}"
                    )));
                }
                Ok(ProvisionResponse::Hello {
                    protocol: PROVISION_PROTOCOL.to_string(),
                    device_id: Some(device_id.to_string()),
                    firmware_version: env!("CARGO_PKG_VERSION").to_string(),
                    build_hash: build_hash.to_string(),
                    boot_id: Some(commissioning.boot_id().to_string()),
                })
            }
            ProvisionRequest::Capabilities => Ok(capabilities_response(features)),
            ProvisionRequest::CurrentConfig => {
                log::warn!("provisioning NVS: current-config read started");
                let stored = match store.load_active() {
                    Ok(stored) => Some(stored),
                    Err(ProvisionError::NoActiveConfig) => None,
                    Err(error) => return Err(error),
                };
                log::warn!(
                    "provisioning NVS: current-config read completed (present={})",
                    stored.is_some()
                );
                Ok(match stored {
                    Some(StoredConfig {
                        generation,
                        config_hash,
                        payload_sha256,
                        ..
                    }) => ProvisionResponse::CurrentConfig {
                        // The profile is already present in the host catalogue.
                        // Re-serializing the full ~3 KiB document while Wi-Fi,
                        // SDSPI/FAT and CSI are resident can exhaust the C5's
                        // largest contiguous heap block. Hashes and generation
                        // are the authoritative atomic readback contract.
                        profile: None,
                        config_hash: Some(config_hash),
                        payload_sha256: Some(payload_sha256),
                        generation: Some(generation),
                    },
                    None => ProvisionResponse::CurrentConfig {
                        profile: None,
                        config_hash: None,
                        payload_sha256: None,
                        generation: None,
                    },
                })
            }
            ProvisionRequest::Begin {
                request_id,
                total_len,
                crc32,
                sha256,
            } => {
                receiver.begin(request_id.clone(), total_len as usize, crc32, sha256)?;
                *pending = None;
                Ok(ProvisionResponse::Ack {
                    request_id,
                    sequence: None,
                    sha256: None,
                })
            }
            ProvisionRequest::Chunk {
                request_id,
                sequence,
                data,
                sha256,
            } => {
                receiver.push_chunk(&request_id, sequence, &data, &sha256)?;
                Ok(ProvisionResponse::Ack {
                    request_id,
                    sequence: Some(sequence),
                    sha256: Some(sha256),
                })
            }
            ProvisionRequest::Validate { request_id } => {
                let config = receiver.validate(&request_id)?;
                config.validate_device_identity(device_id)?;
                let staged =
                    store.stage_validated_payload(receiver.payload(&request_id)?, &config)?;
                let config_hash = staged.config_hash.clone();
                let payload_sha256 = staged.payload_sha256.clone();
                *pending = Some((request_id.clone(), staged));
                Ok(ProvisionResponse::Valid {
                    request_id,
                    config_hash,
                    payload_sha256,
                })
            }
            ProvisionRequest::Commit { request_id } => {
                let (pending_id, staged) = pending
                    .as_ref()
                    .ok_or_else(|| ProvisionError::Staging("nothing validated".to_string()))?;
                if pending_id != &request_id {
                    return Err(ProvisionError::Staging(
                        "request_id does not match validated transfer".to_string(),
                    ));
                }
                let committed = store.commit(staged)?;
                receiver.finish(&request_id)?;
                *pending = None;
                Ok(ProvisionResponse::Committed {
                    request_id,
                    config_hash: committed.config_hash,
                    payload_sha256: committed.payload_sha256,
                    generation: committed.generation,
                })
            }
            ProvisionRequest::Abort { request_id } => {
                receiver.abort(&request_id);
                if let Some((pending_id, staged)) = pending.as_ref() {
                    if pending_id == &request_id {
                        store.abort(staged)?;
                        *pending = None;
                    }
                }
                Ok(ProvisionResponse::Ack {
                    request_id,
                    sequence: None,
                    sha256: None,
                })
            }
            ProvisionRequest::Readback {
                config_hash,
                payload_sha256,
            } => {
                let stored = store.load_active()?;
                if stored.config_hash != config_hash {
                    return Err(ProvisionError::Staging(format!(
                        "active config hash is {}, not {config_hash}",
                        stored.config_hash
                    )));
                }
                if let Some(expected) = payload_sha256.as_ref() {
                    if !stored.payload_sha256.eq_ignore_ascii_case(expected) {
                        return Err(ProvisionError::Staging(format!(
                            "active payload SHA-256 is {}, not {expected}",
                            stored.payload_sha256
                        )));
                    }
                }
                Ok(ProvisionResponse::CurrentConfig {
                    // Keep readback bounded for the same reason as
                    // `current_config`; the host verifies both hashes below.
                    profile: None,
                    config_hash: Some(stored.config_hash),
                    payload_sha256: Some(stored.payload_sha256),
                    generation: Some(stored.generation),
                })
            }
            ProvisionRequest::AccessoryCommand { command } => {
                let ack = diagnostic_status_led_ack(&command, commissioning, status_led)
                    .or_else(|| diagnostic_runtime_ack(&command, commissioning))
                    .or_else(|| diagnostic_reboot_ack(&command, commissioning))
                    .unwrap_or_else(|| commissioning.dispatch(command));
                Ok(ProvisionResponse::AccessoryResult {
                    ack,
                })
            }
            ProvisionRequest::AccessoryResult { request_id } => {
                let ack = commissioning.result(&request_id).ok_or_else(|| {
                    ProvisionError::Protocol(format!(
                        "local commissioning request {request_id} is unknown or expired"
                    ))
                })?;
                Ok(ProvisionResponse::AccessoryResult { ack })
            }
        })();

        result.unwrap_or_else(|error| ProvisionResponse::Error {
            request_id: None,
            code: "provisioning_error".to_string(),
            message: error.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::DeviceProfile;

    #[derive(Default)]
    struct MemoryStorage {
        a: Option<Vec<u8>>,
        b: Option<Vec<u8>>,
        active: Option<ConfigSlot>,
        fail_active_write: bool,
    }

    impl ConfigStorage for MemoryStorage {
        fn read_slot(&self, slot: ConfigSlot) -> Result<Option<Vec<u8>>, String> {
            Ok(match slot {
                ConfigSlot::A => self.a.clone(),
                ConfigSlot::B => self.b.clone(),
            })
        }
        fn write_slot(&mut self, slot: ConfigSlot, bytes: &[u8]) -> Result<(), String> {
            *match slot {
                ConfigSlot::A => &mut self.a,
                ConfigSlot::B => &mut self.b,
            } = Some(bytes.to_vec());
            Ok(())
        }
        fn erase_slot(&mut self, slot: ConfigSlot) -> Result<(), String> {
            *match slot {
                ConfigSlot::A => &mut self.a,
                ConfigSlot::B => &mut self.b,
            } = None;
            Ok(())
        }
        fn active_slot(&self) -> Result<Option<ConfigSlot>, String> {
            Ok(self.active)
        }
        fn set_active_slot(&mut self, slot: ConfigSlot) -> Result<(), String> {
            if self.fail_active_write {
                return Err("power lost".to_string());
            }
            self.active = Some(slot);
            Ok(())
        }
    }

    fn config(name: &str) -> ProvisionedConfig {
        let mut profile = DeviceProfile::default();
        profile.identity.profile_uuid = format!("uuid-{name}");
        profile.identity.name = name.to_string();
        ProvisionedConfig {
            protocol: PROVISION_PROTOCOL.to_string(),
            build_hash: "a".repeat(64),
            profile,
            secrets: ProvisionedSecrets::default(),
        }
    }

    #[test]
    fn provisioned_config_is_bound_to_a_source_aware_build_hash() {
        let mut provisioned = config("build-binding");
        assert!(provisioned.validate_for_commit().is_ok());

        provisioned.build_hash.clear();
        // Legacy slots remain readable so a new atomic commit can replace
        // them, but they can never be accepted as a newly staged payload.
        assert!(provisioned.validate().is_ok());
        assert!(matches!(
            provisioned.validate_for_commit(),
            Err(ProvisionError::Protocol(message))
                if message.contains("build_hash")
        ));
    }

    #[test]
    fn cobs_crc_round_trip() {
        let data = vec![0, 1, 0, 2, 3];
        let message = ProvisionRequest::Chunk {
            request_id: "abc".to_string(),
            sequence: 2,
            sha256: sha256_hex(&data),
            data,
        };
        let encoded = ProvisionCodec::encode(&message).unwrap();
        assert_eq!(encoded.last(), Some(&0));
        assert!(!encoded[..encoded.len() - 1].contains(&0));
        assert_eq!(
            ProvisionCodec::decode::<ProvisionRequest>(&encoded).unwrap(),
            message
        );
    }

    #[test]
    fn serial_accessory_command_round_trip_preserves_raw_ir_timings() {
        let request = ProvisionRequest::AccessoryCommand {
            command: CommandEnvelope {
                schema: crate::mqtt::COMMAND_SCHEMA.to_string(),
                request_id: "usb-ir-1".to_string(),
                boot_id: None,
                issued_at_ms: 0,
                ttl_ms: 20_000,
                command: crate::mqtt::AccessoryCommand::IrSend {
                    code_id: "power".to_string(),
                    carrier_hz: 38_000,
                    duty_percent: 33,
                    sequence_us: vec![9_000, 4_500, 560, 560],
                    repeats: 1,
                },
            },
        };
        let encoded = ProvisionCodec::encode(&request).unwrap();
        assert_eq!(
            ProvisionCodec::decode::<ProvisionRequest>(&encoded).unwrap(),
            request
        );
    }

    #[test]
    fn interrupted_commit_preserves_old_active_slot() {
        let mut store = AtomicConfigStore::new(MemoryStorage::default());
        let first = store.stage(&config("first")).unwrap();
        store.commit(&first).unwrap();
        let second = store.stage(&config("second")).unwrap();
        store.storage.fail_active_write = true;
        assert!(store.commit(&second).is_err());
        assert_eq!(
            store.load_active().unwrap().config.profile.identity.name,
            "first"
        );
    }

    #[test]
    fn staged_without_commit_never_becomes_active() {
        let mut store = AtomicConfigStore::new(MemoryStorage::default());
        let staged = store.stage(&config("staged-only")).unwrap();

        assert_eq!(staged.slot, ConfigSlot::A);
        assert!(matches!(
            store.load_active(),
            Err(ProvisionError::NoActiveConfig)
        ));

        let storage = store.into_inner();
        assert!(storage.a.is_some(), "the staged record must exist on disk");
        assert_eq!(storage.active, None, "commit marker must remain absent");
    }

    #[test]
    fn validated_staging_does_not_reparse_and_commit_remains_fail_closed() {
        let config = config("already-validated");
        config.validate().unwrap();
        let mut store = AtomicConfigStore::new(MemoryStorage::default());

        // Deliberately violate the trusted API contract. Staging verifies raw
        // NVS byte identity without parsing again, while commit still parses
        // and refuses to publish a payload that differs from the validation.
        let staged = store
            .stage_validated_payload(b"{not-valid-json", &config)
            .unwrap();
        assert!(matches!(
            store.load_active(),
            Err(ProvisionError::NoActiveConfig)
        ));
        assert!(matches!(
            store.commit(&staged),
            Err(ProvisionError::InvalidSlot(_))
        ));
        assert_eq!(store.into_inner().active, None);
    }

    #[test]
    fn staging_generation_scan_does_not_parse_stored_profiles() {
        let storage = MemoryStorage {
            b: Some(encode_slot_record(41, b"not-json").unwrap()),
            ..MemoryStorage::default()
        };
        let config = config("next-generation");
        let payload = serde_json::to_vec(&config).unwrap();
        let mut store = AtomicConfigStore::new(storage);

        let staged = store.stage_validated_payload(&payload, &config).unwrap();

        assert_eq!(staged.slot, ConfigSlot::A);
        assert_eq!(staged.generation, 42);
    }

    #[test]
    fn transfer_rejects_out_of_order_chunks() {
        let payload = serde_json::to_vec(&config("test")).unwrap();
        let mut receiver = ProvisionStagingReceiver::default();
        receiver
            .begin(
                "request",
                payload.len(),
                crc32(&payload),
                sha256_hex(&payload),
            )
            .unwrap();
        assert!(receiver
            .push_chunk("request", 1, &payload, &sha256_hex(&payload))
            .is_err());
    }

    #[test]
    fn transfer_rejects_corrupted_chunk_sha256() {
        let payload = serde_json::to_vec(&config("test")).unwrap();
        let mut receiver = ProvisionStagingReceiver::default();
        receiver
            .begin(
                "request",
                payload.len(),
                crc32(&payload),
                sha256_hex(&payload),
            )
            .unwrap();
        assert!(receiver
            .push_chunk("request", 0, &payload, &sha256_hex(b"different"))
            .is_err());
    }

    #[test]
    fn capabilities_advertise_the_memory_safe_chunk_limit() {
        let response = capabilities_response(&["atomic-config".to_string()]);
        let ProvisionResponse::Capabilities {
            max_chunk_bytes, ..
        } = response
        else {
            panic!("expected capabilities response");
        };

        assert_eq!(max_chunk_bytes as usize, MAX_PROVISION_CHUNK_BYTES);
    }

    #[test]
    fn transfer_accepts_maximum_safe_chunk_and_rejects_larger_chunks() {
        let supported = vec![0x5a; MAX_PROVISION_CHUNK_BYTES];
        let mut receiver = ProvisionStagingReceiver::default();
        receiver
            .begin(
                "supported",
                supported.len(),
                crc32(&supported),
                sha256_hex(&supported),
            )
            .unwrap();
        receiver
            .push_chunk("supported", 0, &supported, &sha256_hex(&supported))
            .unwrap();

        let oversized = vec![0xa5; MAX_PROVISION_CHUNK_BYTES + 1];
        let mut receiver = ProvisionStagingReceiver::default();
        receiver
            .begin(
                "oversized",
                oversized.len(),
                crc32(&oversized),
                sha256_hex(&oversized),
            )
            .unwrap();
        let error = receiver
            .push_chunk("oversized", 0, &oversized, &sha256_hex(&oversized))
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            format!("chunk exceeds {MAX_PROVISION_CHUNK_BYTES}-byte device limit")
        );
    }

    #[test]
    fn secret_debug_output_is_redacted() {
        let secret = SecretString("super-secret".to_string());
        assert!(!format!("{secret:?}").contains("super-secret"));
    }

    #[test]
    fn efuse_mac_identity_is_normalized_and_immutable() {
        let expected = device_id_from_mac([0x02, 0x0a, 0xff, 0x10, 0x00, 0xb7]);
        assert_eq!(expected, "esp32-020aff1000b7");

        let mut value = config("identity");
        assert!(value.validate_device_identity(&expected).is_err());
        value.profile.identity.device_id = Some(expected.clone());
        value.validate_device_identity(&expected).unwrap();
        value.profile.identity.device_id = Some("esp32-ffffffffffff".to_string());
        assert!(value.validate_device_identity(&expected).is_err());
    }

    #[test]
    fn c5_eui64_is_never_written_into_a_six_byte_mac_buffer() {
        let efuse_eui64 = [0xd0, 0xcf, 0x13, 0xfe, 0xff, 0xeb, 0xf3, 0x8c];
        let wifi_mac = mac48_from_eui64(efuse_eui64);

        assert_eq!(wifi_mac, [0xd0, 0xcf, 0x13, 0xeb, 0xf3, 0x8c]);
        assert_eq!(device_id_from_mac(wifi_mac), "esp32-d0cf13ebf38c");
    }

    #[test]
    fn direct_uart_read_policy_distinguishes_idle_data_and_errors() {
        assert_eq!(serial_read_count(0, 256), Ok(None));
        assert_eq!(serial_read_count(17, 256), Ok(Some(17)));
        assert_eq!(serial_read_count(-1, 256), Err(-1));
        assert_eq!(serial_read_count(257, 256), Err(257));
    }

    #[test]
    fn direct_uart_write_requires_the_complete_frame() {
        assert!(serial_write_is_complete(201, 201));
        assert!(!serial_write_is_complete(200, 201));
        assert!(!serial_write_is_complete(-1, 201));
    }

    #[test]
    fn freertos_timeout_rounds_up_to_at_least_one_tick() {
        assert_eq!(freertos_ticks_for_millis(50, 100), 5);
        assert_eq!(freertos_ticks_for_millis(1, 100), 1);
        assert_eq!(freertos_ticks_for_millis(1_001, 100), 101);
    }

    #[test]
    fn persistent_serial_errors_are_rate_limited() {
        assert!(should_log_serial_read_error(1));
        assert!(!should_log_serial_read_error(2));
        assert!(!should_log_serial_read_error(
            SERIAL_READ_ERROR_LOG_EVERY - 1
        ));
        assert!(should_log_serial_read_error(SERIAL_READ_ERROR_LOG_EVERY));
    }
}
