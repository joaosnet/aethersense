//! Low-priority, bounded SD session writer.
//!
//! The filesystem backend expects an SDSPI/FAT VFS mount to exist. Mounting is
//! deliberately a platform adapter concern; this module owns the portable
//! schema, queue semantics, rotation and crash-tolerant manifest writes.

use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::profile::PrivacyMode;

pub const SD_SESSION_SCHEMA: &str = "aethersense.firmware_sd_session.v1";
pub const SESSION_ROOT: &str = "aethersense/sessions";
pub const DEFAULT_STORAGE_QUEUE_CAPACITY: usize = 256;
pub const ESP32C5_MAX_STORAGE_QUEUE_CAPACITY: usize = 16;
/// Resident SD worker stack. Network synchronization runs in a temporary task,
/// so this stack only needs to cover mount, FAT metadata and bounded writes.
pub const SD_WORKER_STACK_BYTES: usize = 14 * 1024;
/// Temporary stack used only while an explicit network sync command is active.
pub const SD_SYNC_STACK_BYTES: usize = 14 * 1024;
pub const SD_IO_BUFFER_BYTES: usize = 1024;
pub const SD_DMA_MAX_TRANSFER_BYTES: usize = 1024;
pub const SD_VFS_MAX_OPEN_FILES: usize = 3;
/// FreeRTOS priority used by the SD worker: above idle housekeeping, but below
/// the networking/CSI path. The worker also remains bounded and does no I/O in
/// a CSI callback.
pub const SD_TASK_PRIORITY: u32 = 2;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SdSessionManifest {
    #[serde(default = "default_schema")]
    pub schema: String,
    pub session_id: String,
    pub device_id: String,
    pub started_at_iso: String,
    pub finished_at_iso: Option<String>,
    pub build_hash: String,
    pub config_hash: String,
    pub privacy_mode: PrivacyMode,
    pub raw_csi_enabled: bool,
    pub events_enabled: bool,
    #[serde(default)]
    pub event_count: u64,
    #[serde(default)]
    pub csi_count: u64,
    #[serde(default)]
    pub dropped_count: u64,
    #[serde(default)]
    pub files_sha256: BTreeMap<String, String>,
    #[serde(default)]
    pub synced: bool,
}

impl SdSessionManifest {
    pub fn validate(&self) -> Result<(), StorageError> {
        if self.schema != SD_SESSION_SCHEMA {
            return Err(StorageError::InvalidSession(
                "unsupported session schema".to_string(),
            ));
        }
        if !valid_session_component(&self.session_id) {
            return Err(StorageError::InvalidSession(
                "unsafe session_id".to_string(),
            ));
        }
        if self.raw_csi_enabled && self.privacy_mode != PrivacyMode::EmptyRoomBench {
            return Err(StorageError::PrivacyViolation);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FirmwareEvent {
    pub timestamp_ms: u64,
    pub kind: String,
    #[serde(default)]
    pub data: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StorageRecord {
    Event(FirmwareEvent),
    CsiCsv(String),
}

/// Shared recording latch for the UDP consumer and the SD worker. Authorizing
/// raw CSI in a profile does not activate collection: the worker flips this
/// gate only after an explicit start command has created a valid session.
#[derive(Clone, Default)]
pub struct RawCsiRecordingGate {
    active: Arc<AtomicBool>,
}

impl RawCsiRecordingGate {
    pub fn activate(&self) {
        self.active.store(true, Ordering::Release);
    }

    pub fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StorageError {
    InvalidSession(String),
    PrivacyViolation,
    QueueFull,
    Backend(String),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSession(message) | Self::Backend(message) => f.write_str(message),
            Self::PrivacyViolation => {
                f.write_str("raw CSI recording requires empty-room/bench mode")
            }
            Self::QueueFull => f.write_str("storage writer queue is full or busy"),
        }
    }
}

impl std::error::Error for StorageError {}

struct StorageBus {
    queue: Mutex<VecDeque<StorageRecord>>,
    capacity: usize,
}

#[derive(Clone)]
pub struct StorageIngress {
    bus: Arc<StorageBus>,
    dropped: Arc<AtomicUsize>,
}

pub struct StorageEgress {
    bus: Arc<StorageBus>,
    dropped: Arc<AtomicUsize>,
}

impl StorageEgress {
    fn discard_one(&self) -> bool {
        self.bus
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front()
            .is_some()
    }

    #[cfg(test)]
    pub(crate) fn pending_count(&self) -> usize {
        self.bus
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

pub fn storage_queue(capacity: usize) -> (StorageIngress, StorageEgress) {
    let capacity = capacity.max(1);
    let bus = Arc::new(StorageBus {
        queue: Mutex::new(VecDeque::with_capacity(capacity)),
        capacity,
    });
    let dropped = Arc::new(AtomicUsize::new(0));
    (
        StorageIngress {
            bus: bus.clone(),
            dropped: dropped.clone(),
        },
        StorageEgress { bus, dropped },
    )
}

pub fn effective_storage_queue_capacity(requested: usize, hardware_profile: &str) -> usize {
    let requested = requested.max(1);
    if hardware_profile.eq_ignore_ascii_case("esp32c5") {
        requested.min(ESP32C5_MAX_STORAGE_QUEUE_CAPACITY)
    } else {
        requested
    }
}

impl StorageIngress {
    /// Wait-free producer path: never blocks the Wi-Fi/CSI callback.
    pub fn try_push(&self, record: StorageRecord) -> Result<(), StorageError> {
        if let Ok(mut queue) = self.bus.queue.try_lock() {
            if queue.len() < self.bus.capacity {
                queue.push_back(record);
                return Ok(());
            }
        }
        self.dropped.fetch_add(1, Ordering::Relaxed);
        Err(StorageError::QueueFull)
    }

    pub fn dropped_count(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }
}

pub trait SessionStorage {
    fn begin(&mut self, manifest: &SdSessionManifest) -> Result<(), String>;
    fn append_event(&mut self, event_json: &str) -> Result<(), String>;
    fn append_csi(&mut self, csi_csv: &str) -> Result<(), String>;
    /// Hashes rotated payload files only. Called exclusively by the
    /// low-priority writer task, never from the CSI callback.
    fn file_hashes(&self) -> Result<BTreeMap<String, String>, String> {
        Ok(BTreeMap::new())
    }
    fn checkpoint(&mut self, manifest: &SdSessionManifest) -> Result<(), String>;
    fn finish(&mut self, manifest: &SdSessionManifest) -> Result<(), String>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StorageWriterStats {
    pub events_written: u64,
    pub csi_written: u64,
    pub dropped: u64,
    pub failed: bool,
}

pub struct StorageWriter<B> {
    backend: B,
    egress: StorageEgress,
    manifest: SdSessionManifest,
    stats: StorageWriterStats,
    checkpoint_every: u64,
}

impl<B: SessionStorage> StorageWriter<B> {
    pub fn new(
        mut backend: B,
        egress: StorageEgress,
        manifest: SdSessionManifest,
    ) -> Result<Self, StorageError> {
        manifest.validate()?;
        backend.begin(&manifest).map_err(StorageError::Backend)?;
        Ok(Self {
            backend,
            egress,
            manifest,
            stats: StorageWriterStats::default(),
            checkpoint_every: 64,
        })
    }

    /// Drains at most one item. Call from a low-priority task/thread.
    pub fn drain_one(&mut self) -> Result<bool, StorageError> {
        if self.stats.failed {
            return Ok(false);
        }
        let record = self
            .egress
            .bus
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front();
        let Some(record) = record else {
            return Ok(false);
        };
        let result = match record {
            StorageRecord::Event(event) if self.manifest.events_enabled => {
                let json = serde_json::to_string(&event)
                    .map_err(|err| StorageError::Backend(err.to_string()))?;
                self.backend.append_event(&json).map(|_| {
                    self.stats.events_written += 1;
                    self.manifest.event_count += 1;
                })
            }
            StorageRecord::CsiCsv(line) if self.manifest.raw_csi_enabled => {
                self.backend.append_csi(&line).map(|_| {
                    self.stats.csi_written += 1;
                    self.manifest.csi_count += 1;
                })
            }
            _ => Ok(()),
        };
        if let Err(message) = result {
            // Keep all existing files and stop consuming. A card-full/removal
            // condition must never delete unsynchronized sessions.
            self.stats.failed = true;
            return Err(StorageError::Backend(message));
        }
        let total = self.stats.events_written + self.stats.csi_written;
        if total > 0 && total % self.checkpoint_every == 0 {
            self.checkpoint()?;
        }
        Ok(true)
    }

    pub fn checkpoint(&mut self) -> Result<(), StorageError> {
        let dropped = self.egress.dropped.load(Ordering::Relaxed) as u64;
        self.stats.dropped = dropped;
        self.manifest.dropped_count = dropped;
        self.manifest.files_sha256 = self.backend.file_hashes().map_err(StorageError::Backend)?;
        self.backend
            .checkpoint(&self.manifest)
            .map_err(StorageError::Backend)
    }

    pub fn finish(
        mut self,
        finished_at_iso: impl Into<String>,
    ) -> Result<(B, StorageEgress, SdSessionManifest), StorageError> {
        self.manifest.finished_at_iso = Some(finished_at_iso.into());
        self.checkpoint()?;
        self.backend
            .finish(&self.manifest)
            .map_err(StorageError::Backend)?;
        Ok((self.backend, self.egress, self.manifest))
    }

    pub fn stats(&self) -> &StorageWriterStats {
        &self.stats
    }
    pub fn manifest(&self) -> &SdSessionManifest {
        &self.manifest
    }
}

/// FAT/VFS-compatible filesystem backend with deterministic rotated filenames.
pub struct FilesystemSessionStorage {
    mount_point: PathBuf,
    rotate_bytes: u64,
    session_dir: Option<PathBuf>,
    event_part: u32,
    csi_part: u32,
    event_bytes: u64,
    csi_bytes: u64,
}

impl FilesystemSessionStorage {
    pub fn new(mount_point: impl Into<PathBuf>, rotate_bytes: u64) -> Self {
        Self {
            mount_point: mount_point.into(),
            rotate_bytes: rotate_bytes.max(4_096),
            session_dir: None,
            event_part: 1,
            csi_part: 1,
            event_bytes: 0,
            csi_bytes: 0,
        }
    }

    fn dir(&self) -> Result<&Path, String> {
        self.session_dir
            .as_deref()
            .ok_or_else(|| "session not started".to_string())
    }

    fn append_rotated(&mut self, kind: FileKind, line: &str) -> Result<(), String> {
        let line_bytes = line.len() as u64 + 1;
        let rotate = match kind {
            FileKind::Events => {
                self.event_bytes > 0 && self.event_bytes + line_bytes > self.rotate_bytes
            }
            FileKind::Csi => self.csi_bytes > 0 && self.csi_bytes + line_bytes > self.rotate_bytes,
        };
        if rotate {
            match kind {
                FileKind::Events => {
                    self.event_part += 1;
                    self.event_bytes = 0;
                }
                FileKind::Csi => {
                    self.csi_part += 1;
                    self.csi_bytes = 0;
                }
            }
        }
        let (name, counter) = match kind {
            FileKind::Events => (
                format!("events-{:04}.jsonl", self.event_part),
                &mut self.event_bytes,
            ),
            FileKind::Csi => (format!("csi-{:04}.csv", self.csi_part), &mut self.csi_bytes),
        };
        let path = self
            .session_dir
            .as_ref()
            .ok_or_else(|| "session not started".to_string())?
            .join(name);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|err| err.to_string())?;
        file.write_all(line.as_bytes())
            .map_err(|err| err.to_string())?;
        file.write_all(b"\n").map_err(|err| err.to_string())?;
        file.flush().map_err(|err| err.to_string())?;
        *counter += line_bytes;
        Ok(())
    }

    fn write_manifest(&self, manifest: &SdSessionManifest) -> Result<(), String> {
        let dir = self.dir()?;
        let final_path = dir.join("session.json");
        let temp_path = dir.join("session.json.tmp");
        let bytes = serde_json::to_vec_pretty(manifest).map_err(|err| err.to_string())?;
        std::fs::write(&temp_path, bytes).map_err(|err| err.to_string())?;
        if final_path.exists() {
            std::fs::remove_file(&final_path).map_err(|err| err.to_string())?;
        }
        std::fs::rename(temp_path, final_path).map_err(|err| err.to_string())
    }

    fn hash_payload_files(&self) -> Result<BTreeMap<String, String>, String> {
        let dir = self.dir()?;
        let mut paths = std::fs::read_dir(dir)
            .map_err(|err| err.to_string())?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        (name.starts_with("events-") && name.ends_with(".jsonl"))
                            || (name.starts_with("csi-") && name.ends_with(".csv"))
                    })
            })
            .collect::<Vec<_>>();
        paths.sort();
        let mut hashes = BTreeMap::new();
        for path in paths {
            let mut file = std::fs::File::open(&path).map_err(|err| err.to_string())?;
            let mut hasher = Sha256::new();
            let mut buffer = [0u8; SD_IO_BUFFER_BYTES];
            loop {
                let read = file.read(&mut buffer).map_err(|err| err.to_string())?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            let digest = hasher.finalize();
            let hash = digest.iter().map(|byte| format!("{byte:02x}")).collect();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| "non-UTF8 session filename".to_string())?;
            hashes.insert(name.to_string(), hash);
        }
        Ok(hashes)
    }
}

enum FileKind {
    Events,
    Csi,
}

impl SessionStorage for FilesystemSessionStorage {
    fn begin(&mut self, manifest: &SdSessionManifest) -> Result<(), String> {
        let dir = self
            .mount_point
            .join(SESSION_ROOT)
            .join(&manifest.session_id);
        std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
        self.session_dir = Some(dir);
        self.write_manifest(manifest)
    }

    fn append_event(&mut self, event_json: &str) -> Result<(), String> {
        self.append_rotated(FileKind::Events, event_json)
    }

    fn append_csi(&mut self, csi_csv: &str) -> Result<(), String> {
        self.append_rotated(FileKind::Csi, csi_csv)
    }

    fn file_hashes(&self) -> Result<BTreeMap<String, String>, String> {
        self.hash_payload_files()
    }

    fn checkpoint(&mut self, manifest: &SdSessionManifest) -> Result<(), String> {
        self.write_manifest(manifest)
    }

    fn finish(&mut self, manifest: &SdSessionManifest) -> Result<(), String> {
        self.write_manifest(manifest)
    }
}

fn valid_session_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            // FAT long file names still reject `:`, even though it is common
            // in RFC3339 timestamps. Session directory ids therefore use a
            // compact UTC timestamp while started_at_iso retains RFC3339.
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
        })
        && !value.contains("..")
}

fn compact_session_timestamp(iso: &str) -> String {
    iso.bytes()
        .filter(|byte| !matches!(byte, b'-' | b':'))
        .map(char::from)
        .collect()
}

fn default_schema() -> String {
    SD_SESSION_SCHEMA.to_string()
}

fn iso8601_now() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let days = (seconds / 86_400) as i64;
    let seconds_of_day = seconds % 86_400;
    // Gregorian civil date conversion (Howard Hinnant's civil_from_days).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    let hour = seconds_of_day / 3_600;
    let minute = seconds_of_day / 60 % 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub mod esp {
    use super::*;
    use std::io::{Seek, SeekFrom};
    use std::sync::mpsc::{sync_channel, SyncSender};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use esp_idf_hal::gpio::{AnyIOPin, AnyInputPin, AnyOutputPin};
    use esp_idf_hal::sd::{spi::SdSpiHostDriver, SdCardConfiguration, SdCardDriver};
    use esp_idf_hal::spi::{config::DriverConfig, Dma, SpiDriver, SPI2};
    use esp_idf_svc::fs::fatfs::Fatfs;
    use esp_idf_svc::http::client::{Configuration as HttpConfiguration, EspHttpConnection};
    use esp_idf_svc::http::Method;
    use esp_idf_svc::io::vfs::MountedFatfs;

    use crate::mqtt::{
        AccessoryCommand, AccessoryOutbound, AccessoryStateDocument, CommandAck, CommandEnvelope,
        DeviceEvent, MqttOutbound,
    };
    use crate::profile::{RecordingProfile, SdProfile, SyncProfile};
    use crate::provisioning::SecretString;
    use crate::status_led::{StatusColor, StatusLed};

    pub struct EspStorageRuntime {
        pub ingress: StorageIngress,
        pub raw_csi_gate: RawCsiRecordingGate,
        pub commands: SyncSender<CommandEnvelope>,
        pub handle: JoinHandle<()>,
    }

    #[derive(Clone)]
    pub struct SdRuntimeIdentity {
        pub device_id: String,
        pub build_hash: String,
        pub config_hash: String,
        pub boot_id: String,
    }

    #[derive(Debug, Deserialize)]
    struct StartUploadResponse {
        upload_id: String,
        offset: u64,
        chunk_size: u64,
        #[serde(default)]
        complete: bool,
    }

    fn upload_collection_url(base_url: &str) -> Result<String, String> {
        let base = base_url.trim().trim_end_matches('/');
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err("sync base_url must use http:// or https://".to_string());
        }
        if base.ends_with("/api/storage/uploads") {
            Ok(base.to_string())
        } else if let Some(prefix) = base.strip_suffix("/api/firmware-sd") {
            // Compatibility with profiles emitted before the resumable API was
            // named. New profiles should store the collection endpoint.
            Ok(format!("{prefix}/api/storage/uploads"))
        } else if base.ends_with("/api") {
            Ok(format!("{base}/storage/uploads"))
        } else {
            Ok(format!("{base}/api/storage/uploads"))
        }
    }

    fn response_json(connection: &mut EspHttpConnection) -> Result<serde_json::Value, String> {
        connection
            .initiate_response()
            .map_err(|error| format!("HTTP response failed: {error}"))?;
        let status = connection.status();
        let mut body = Vec::with_capacity(1_024);
        let mut buffer = [0u8; 1_024];
        loop {
            let read = connection
                .read(&mut buffer)
                .map_err(|error| format!("HTTP response read failed: {error}"))?;
            if read == 0 {
                break;
            }
            if body.len().saturating_add(read) > 32 * 1_024 {
                return Err("HTTP response exceeds 32 KiB".to_string());
            }
            body.extend_from_slice(&buffer[..read]);
        }
        let value = if body.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&body)
                .map_err(|error| format!("invalid JSON from upload server: {error}"))?
        };
        if !(200..300).contains(&status) {
            let code = value
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("http_error");
            let offset = value
                .get("offset")
                .and_then(serde_json::Value::as_u64)
                .map(|value| format!(" (server offset {value})"))
                .unwrap_or_default();
            return Err(format!(
                "upload server returned HTTP {status}/{code}{offset}"
            ));
        }
        Ok(value)
    }

    fn request_json(
        connection: &mut EspHttpConnection,
        method: Method,
        url: &str,
        bearer: &str,
        body: &[u8],
    ) -> Result<serde_json::Value, String> {
        let authorization = format!("Bearer {bearer}");
        let content_length = body.len().to_string();
        let headers = [
            ("authorization", authorization.as_str()),
            ("content-type", "application/json"),
            ("content-length", content_length.as_str()),
        ];
        connection
            .initiate_request(method, url, &headers)
            .map_err(|error| format!("HTTP request failed: {error}"))?;
        let mut written = 0;
        while written < body.len() {
            let count = connection
                .write(&body[written..])
                .map_err(|error| format!("HTTP request write failed: {error}"))?;
            if count == 0 {
                return Err("HTTP request made no write progress".to_string());
            }
            written += count;
        }
        response_json(connection)
    }

    fn hash_file(path: &Path) -> Result<String, String> {
        let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; SD_IO_BUFFER_BYTES];
        loop {
            let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
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

    fn append_file_chunk(
        connection: &mut EspHttpConnection,
        url: &str,
        bearer: &str,
        file: &mut std::fs::File,
        offset: u64,
        length: u64,
    ) -> Result<u64, String> {
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| error.to_string())?;
        let mut remaining = length;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; SD_IO_BUFFER_BYTES];
        while remaining > 0 {
            let requested = remaining.min(buffer.len() as u64) as usize;
            let read = file
                .read(&mut buffer[..requested])
                .map_err(|error| error.to_string())?;
            if read == 0 {
                return Err("microSD file ended before advertised chunk length".to_string());
            }
            hasher.update(&buffer[..read]);
            remaining -= read as u64;
        }
        let chunk_sha256: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| error.to_string())?;

        let authorization = format!("Bearer {bearer}");
        let content_length = length.to_string();
        let upload_offset = offset.to_string();
        let headers = [
            ("authorization", authorization.as_str()),
            ("content-type", "application/octet-stream"),
            ("content-length", content_length.as_str()),
            ("x-upload-offset", upload_offset.as_str()),
            ("x-chunk-sha256", chunk_sha256.as_str()),
        ];
        connection
            .initiate_request(Method::Put, url, &headers)
            .map_err(|error| format!("HTTP chunk request failed: {error}"))?;
        let mut remaining = length;
        while remaining > 0 {
            let requested = remaining.min(buffer.len() as u64) as usize;
            let read = file
                .read(&mut buffer[..requested])
                .map_err(|error| error.to_string())?;
            if read == 0 {
                return Err("microSD file ended while streaming chunk".to_string());
            }
            let mut written = 0;
            while written < read {
                let count = connection
                    .write(&buffer[written..read])
                    .map_err(|error| format!("HTTP chunk write failed: {error}"))?;
                if count == 0 {
                    return Err("HTTP chunk made no write progress".to_string());
                }
                written += count;
            }
            remaining -= read as u64;
        }
        let response = response_json(connection)?;
        response
            .get("offset")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| "upload server omitted next offset".to_string())
    }

    fn upload_file(
        connection: &mut EspHttpConnection,
        collection_url: &str,
        bearer: &str,
        sync: &SyncProfile,
        identity: &SdRuntimeIdentity,
        manifest: &SdSessionManifest,
        path: &Path,
    ) -> Result<(), String> {
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "non-UTF8 filename on microSD".to_string())?;
        let total_bytes = std::fs::metadata(path)
            .map_err(|error| error.to_string())?
            .len();
        let sha256 = hash_file(path)?;
        let contains_raw_csi = filename.starts_with("csi-");
        let capture_context = contains_raw_csi.then_some("empty_room");
        let start = serde_json::json!({
            "device_id": identity.device_id,
            "session_id": manifest.session_id,
            "filename": filename,
            "total_bytes": total_bytes,
            "sha256": sha256,
            "contains_raw_csi": contains_raw_csi,
            "capture_context": capture_context,
        });
        let start_bytes = serde_json::to_vec(&start).map_err(|error| error.to_string())?;
        let response = request_json(
            connection,
            Method::Post,
            collection_url,
            bearer,
            &start_bytes,
        )?;
        let start: StartUploadResponse =
            serde_json::from_value(response).map_err(|error| error.to_string())?;
        if start.complete {
            return Ok(());
        }
        if start.offset > total_bytes {
            return Err("upload server offset exceeds local file size".to_string());
        }
        let upload_url = format!("{collection_url}/{}", start.upload_id);
        let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
        let logical_block = u64::from(sync.block_bytes)
            .min(start.chunk_size)
            .min(1_048_576)
            .max(1);
        let mut offset = start.offset;
        while offset < total_bytes {
            let length = logical_block.min(total_bytes - offset);
            let next =
                append_file_chunk(connection, &upload_url, bearer, &mut file, offset, length)?;
            if next != offset + length {
                return Err(format!(
                    "upload server advanced to unexpected offset {next}"
                ));
            }
            offset = next;
        }
        let complete_url = format!("{upload_url}/complete");
        let _ = request_json(connection, Method::Post, &complete_url, bearer, &[])?;
        Ok(())
    }

    fn write_manifest_atomic(path: &Path, manifest: &SdSessionManifest) -> Result<(), String> {
        let temporary = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(manifest).map_err(|error| error.to_string())?;
        std::fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
        if path.exists() {
            std::fs::remove_file(path).map_err(|error| error.to_string())?;
        }
        std::fs::rename(temporary, path).map_err(|error| error.to_string())
    }

    fn sync_pending_sessions(
        connection: &mut EspHttpConnection,
        mount_point: &str,
        sync: &SyncProfile,
        bearer: &SecretString,
        identity: &SdRuntimeIdentity,
    ) -> Result<usize, String> {
        if !sync.enabled || !sync.network_first {
            return Err("network-first storage sync is not enabled".to_string());
        }
        if bearer.expose().is_empty() {
            return Err("device server credential is not provisioned".to_string());
        }
        let collection_url = upload_collection_url(&sync.base_url)?;
        let root = Path::new(mount_point).join(SESSION_ROOT);
        let mut session_dirs = std::fs::read_dir(&root)
            .map_err(|error| error.to_string())?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        session_dirs.sort();
        let mut synced_count = 0;
        for session_dir in session_dirs {
            let manifest_path = session_dir.join("session.json");
            let bytes = std::fs::read(&manifest_path).map_err(|error| error.to_string())?;
            let mut manifest: SdSessionManifest =
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            manifest.validate().map_err(|error| error.to_string())?;
            if manifest.synced || manifest.finished_at_iso.is_none() {
                continue;
            }
            if manifest.device_id != identity.device_id {
                return Err("SD session belongs to a different device identity".to_string());
            }
            let mut payloads = manifest
                .files_sha256
                .keys()
                .map(|name| session_dir.join(name))
                .collect::<Vec<_>>();
            payloads.sort();
            for payload in &payloads {
                if !payload.is_file() {
                    return Err(format!("session payload is missing: {}", payload.display()));
                }
                upload_file(
                    connection,
                    &collection_url,
                    bearer.expose(),
                    sync,
                    identity,
                    &manifest,
                    payload,
                )?;
            }
            // Upload the firmware manifest last so a server-side session is
            // never presented as complete before every payload is confirmed.
            upload_file(
                connection,
                &collection_url,
                bearer.expose(),
                sync,
                identity,
                &manifest,
                &manifest_path,
            )?;
            manifest.synced = true;
            write_manifest_atomic(&manifest_path, &manifest)?;
            synced_count += 1;
            if sync.delete_after_sync {
                std::fs::remove_dir_all(&session_dir).map_err(|error| error.to_string())?;
            }
        }
        Ok(synced_count)
    }

    fn sync_pending_sessions_in_temporary_worker(
        mount_point: String,
        sync: SyncProfile,
        bearer: SecretString,
        identity: SdRuntimeIdentity,
    ) -> Result<usize, String> {
        let handle = std::thread::Builder::new()
            .name("aether-sd-sync".to_string())
            .stack_size(SD_SYNC_STACK_BYTES)
            .spawn(move || {
                unsafe {
                    esp_idf_sys::vTaskPrioritySet(core::ptr::null_mut(), SD_TASK_PRIORITY as _);
                }
                let http_configuration = HttpConfiguration {
                    crt_bundle_attach: sync
                        .base_url
                        .starts_with("https://")
                        .then_some(esp_idf_svc::sys::esp_crt_bundle_attach),
                    timeout: Some(Duration::from_secs(45)),
                    keep_alive_enable: true,
                    ..Default::default()
                };
                EspHttpConnection::new(&http_configuration)
                    .map_err(|error| format!("HTTP upload client unavailable: {error}"))
                    .and_then(|mut connection| {
                        sync_pending_sessions(
                            &mut connection,
                            &mount_point,
                            &sync,
                            &bearer,
                            &identity,
                        )
                    })
            })
            .map_err(|error| format!("storage sync task unavailable: {error}"))?;
        handle
            .join()
            .map_err(|_| "storage sync task panicked".to_string())?
    }

    fn reply(
        outbound: &AccessoryOutbound,
        command: &CommandEnvelope,
        boot_id: &str,
        result: Result<(), String>,
    ) {
        let ack = match result {
            Ok(()) => CommandAck::completed(command, boot_id),
            Err(error) => CommandAck::error(command, boot_id, error),
        };
        let _ = outbound.try_send(MqttOutbound::Ack(ack));
    }

    fn reply_with_state(
        outbound: &AccessoryOutbound,
        command: &CommandEnvelope,
        boot_id: &str,
        result: Result<serde_json::Value, String>,
    ) {
        let ack = match result {
            Ok(state) => CommandAck::completed_with_state(command, boot_id, state),
            Err(error) => CommandAck::error(command, boot_id, error),
        };
        let _ = outbound.try_send(MqttOutbound::Ack(ack));
    }

    fn storage_state(boot_id: &str, mounted: bool, recording: bool) -> AccessoryStateDocument {
        AccessoryStateDocument {
            schema: crate::mqtt::STATE_SCHEMA.to_string(),
            boot_id: boot_id.to_string(),
            accessories: BTreeMap::from([(
                "sd".to_string(),
                serde_json::json!({
                    "mounted": mounted,
                    "recording": recording,
                }),
            )]),
        }
    }

    fn storage_state_value(boot_id: &str, mounted: bool, recording: bool) -> serde_json::Value {
        serde_json::to_value(storage_state(boot_id, mounted, recording))
            .unwrap_or_else(|_| serde_json::json!({"accessories": {}}))
    }

    fn self_test_file(
        mount_point: &str,
        request_id: &str,
        payload: &str,
        expected_sha256: &str,
    ) -> Result<serde_json::Value, String> {
        let bytes = payload.as_bytes();
        if !(16..=4_096).contains(&bytes.len()) {
            return Err("storage self-test payload must contain 16..4096 bytes".to_string());
        }
        if expected_sha256.len() != 64
            || !expected_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("storage self-test expected_sha256 is invalid".to_string());
        }
        let request_digest: String = Sha256::digest(request_id.as_bytes())
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        // Keep the commissioning probe compatible with the strictest FAT 8.3
        // implementation. Session writers use the normal directory layout,
        // but this destructive-free health check must also work when long-file
        // name/stat extensions are unavailable in the VFS build.
        let path = Path::new(mount_point).join(format!("AH{}.TMP", &request_digest[..6]));
        let operation = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                // ESP-IDF FAT VFS can return EINVAL for O_CREAT|O_EXCL even
                // though the card is mounted. The request-derived name is
                // unique; truncate also makes an interrupted prior test safe.
                .create(true)
                .truncate(true)
                .open(&path)
                .map_err(|error| format!("storage self-test open failed: {error}"))?;
            file.write_all(bytes)
                .map_err(|error| format!("storage self-test write failed: {error}"))?;
            drop(file);
            // FAT VFS on ESP-IDF does not implement every fstat/metadata flag
            // used by std and can return EINVAL. Read the file back directly;
            // this is also stronger evidence than trusting directory metadata.
            let readback = std::fs::read(&path)
                .map_err(|error| format!("storage self-test readback failed: {error}"))?;
            let actual_sha256: String = Sha256::digest(&readback)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let read_bytes = readback.len();
            if !actual_sha256.eq_ignore_ascii_case(expected_sha256) {
                return Err(format!(
                    "storage self-test SHA-256 mismatch: expected {expected_sha256}, got {actual_sha256}"
                ));
            }
            Ok(serde_json::json!({
                "event": "storage.self_test",
                "storage_id": "sd",
                "bytes_written": bytes.len(),
                "bytes_read": read_bytes,
                "sha256": actual_sha256,
                "temporary_file_removed": true,
            }))
        })();
        let cleanup = match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        };
        match (operation, cleanup) {
            (Ok(state), Ok(())) => Ok(state),
            (Err(error), Ok(())) => Err(error),
            (_, Err(error)) => Err(format!("storage self-test cleanup failed: {error}")),
        }
    }

    fn storage_event(kind: &str, data: serde_json::Value) -> StorageRecord {
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        StorageRecord::Event(FirmwareEvent {
            timestamp_ms,
            kind: kind.to_string(),
            data,
        })
    }

    pub fn spawn_sd_runtime(
        spi2: SPI2<'static>,
        sd: SdProfile,
        recording: RecordingProfile,
        sync: SyncProfile,
        server_bearer_token: SecretString,
        identity: SdRuntimeIdentity,
        outbound: AccessoryOutbound,
        status_led: StatusLed,
    ) -> Result<EspStorageRuntime, String> {
        let sclk = sd
            .sclk_gpio
            .ok_or_else(|| "SD SCLK GPIO missing".to_string())?;
        let mosi = sd
            .mosi_gpio
            .ok_or_else(|| "SD MOSI GPIO missing".to_string())?;
        let miso = sd
            .miso_gpio
            .ok_or_else(|| "SD MISO GPIO missing".to_string())?;
        let cs = sd.cs_gpio.ok_or_else(|| "SD CS GPIO missing".to_string())?;
        let requested_queue_capacity = DEFAULT_STORAGE_QUEUE_CAPACITY;
        let queue_capacity = if cfg!(aethersense_esp32c5) {
            effective_storage_queue_capacity(requested_queue_capacity, "esp32c5")
        } else {
            requested_queue_capacity
        };
        if queue_capacity != requested_queue_capacity {
            log::warn!(
                "storage queue capped for esp32c5: requested={} effective={}",
                requested_queue_capacity,
                queue_capacity
            );
        }
        let (ingress, egress) = storage_queue(queue_capacity);
        let raw_csi_gate = RawCsiRecordingGate::default();
        let thread_raw_csi_gate = raw_csi_gate.clone();
        let (commands, command_rx) = sync_channel::<CommandEnvelope>(16);
        let (startup_tx, startup_rx) = sync_channel::<Result<(), String>>(1);
        let thread_ingress = ingress.clone();
        let handle = std::thread::Builder::new()
            .name("aether-sd".to_string())
            // HTTP upload has its own temporary task. Keeping this resident
            // stack small leaves enough contiguous heap for SDSPI/FAT mount.
            .stack_size(SD_WORKER_STACK_BYTES)
            .spawn(move || {
                // `std::thread` starts with the pthread default priority. Lower
                // this task immediately so FAT/SDSPI and resumable HTTP uploads
                // cannot compete with the CSI/network hot path.
                unsafe {
                    esp_idf_sys::vTaskPrioritySet(
                        core::ptr::null_mut(),
                        SD_TASK_PRIORITY as _,
                    );
                }
                let mount = (|| -> Result<_, String> {
                    let heap_caps = esp_idf_sys::MALLOC_CAP_8BIT as u32;
                    let free_heap = unsafe { esp_idf_sys::heap_caps_get_free_size(heap_caps) };
                    let largest_block =
                        unsafe { esp_idf_sys::heap_caps_get_largest_free_block(heap_caps) };
                    let dma_caps = esp_idf_sys::MALLOC_CAP_DMA as u32;
                    let free_dma = unsafe { esp_idf_sys::heap_caps_get_free_size(dma_caps) };
                    let largest_dma =
                        unsafe { esp_idf_sys::heap_caps_get_largest_free_block(dma_caps) };
                    log::warn!(
                        "SD mount heap: free_8bit={} largest_8bit={} free_dma={} largest_dma={} worker_stack={} dma_max_transfer={} max_files={}",
                        free_heap,
                        largest_block,
                        free_dma,
                        largest_dma,
                        SD_WORKER_STACK_BYTES,
                        SD_DMA_MAX_TRANSFER_BYTES,
                        SD_VFS_MAX_OPEN_FILES
                    );
                    let spi = SpiDriver::new(
                        spi2,
                        unsafe { AnyOutputPin::steal(sclk as u8) },
                        unsafe { AnyOutputPin::steal(mosi as u8) },
                        Some(unsafe { AnyInputPin::steal(miso as u8) }),
                        &DriverConfig::default().dma(Dma::Auto(SD_DMA_MAX_TRANSFER_BYTES)),
                    )
                    .map_err(|error| format!("SPI bus initialization failed: {error}"))?;
                    let host = SdSpiHostDriver::new(
                        spi,
                        Some(unsafe { AnyOutputPin::steal(cs as u8) }),
                        AnyIOPin::none(),
                        AnyIOPin::none(),
                        AnyIOPin::none(),
                        #[cfg(not(any(
                            esp_idf_version_major = "4",
                            all(esp_idf_version_major = "5", esp_idf_version_minor = "0"),
                            all(esp_idf_version_major = "5", esp_idf_version_minor = "1"),
                        )))]
                        None,
                    )
                    .map_err(|error| format!("SDSPI host initialization failed: {error}"))?;
                    let card = SdCardDriver::new_spi(host, &SdCardConfiguration::new())
                        .map_err(|error| format!("SD card initialization failed: {error}"))?;
                    let fatfs = Fatfs::new_sdcard(0, card)
                        .map_err(|error| format!("FAT disk registration failed: {error}"))?;
                    MountedFatfs::mount(
                        fatfs,
                        &sd.mount_point,
                        SD_VFS_MAX_OPEN_FILES,
                    )
                    .map_err(|error| format!("FAT VFS mount registration failed: {error}"))
                })();
                let _mounted = match mount {
                    Ok(mounted) => {
                        let heap_caps = esp_idf_sys::MALLOC_CAP_8BIT as u32;
                        let free_heap = unsafe { esp_idf_sys::heap_caps_get_free_size(heap_caps) };
                        let largest_block = unsafe {
                            esp_idf_sys::heap_caps_get_largest_free_block(heap_caps)
                        };
                        log::warn!(
                            "SD mounted: free_8bit={} largest_8bit={}",
                            free_heap,
                            largest_block
                        );
                        let _ = outbound.try_send(MqttOutbound::State(storage_state(
                            &identity.boot_id,
                            true,
                            false,
                        )));
                        if startup_tx.send(Ok(())).is_err() {
                            return;
                        }
                        mounted
                    }
                    Err(error) => {
                        let message = format!(
                            "SDSPI/FAT mount failed (card is never autoformatted): {error}"
                        );
                        log::error!("{message}");
                        let _ = outbound.try_send(MqttOutbound::State(storage_state(
                            &identity.boot_id,
                            false,
                            false,
                        )));
                        let _ = startup_tx.send(Err(message));
                        return;
                    }
                };
                drop(startup_tx);
                let mut idle_egress = Some(egress);
                let mut writer: Option<StorageWriter<FilesystemSessionStorage>> = None;
                loop {
                    match command_rx.recv_timeout(Duration::from_millis(20)) {
                        Ok(command) => match &command.command {
                            AccessoryCommand::StorageRecordStart {
                                storage_id,
                                session_id,
                                raw_csi,
                            } => {
                                let result = (|| {
                                    if storage_id != "sd" {
                                        return Err("unknown storage_id".to_string());
                                    }
                                    if writer.is_some() {
                                        return Err("recording already active".to_string());
                                    }
                                    if *raw_csi
                                        && (!recording.raw_csi_enabled
                                            || recording.privacy_mode
                                                != PrivacyMode::EmptyRoomBench)
                                    {
                                        return Err("raw CSI is not authorized for this empty-room profile".to_string());
                                    }
                                    let started = iso8601_now();
                                    let session_id = session_id
                                        .clone()
                                        .unwrap_or_else(|| {
                                            format!(
                                                "{}_{}",
                                                compact_session_timestamp(&started),
                                                identity.boot_id
                                            )
                                        });
                                    let manifest = SdSessionManifest {
                                        schema: SD_SESSION_SCHEMA.to_string(),
                                        session_id,
                                        device_id: identity.device_id.clone(),
                                        started_at_iso: started,
                                        finished_at_iso: None,
                                        build_hash: identity.build_hash.clone(),
                                        config_hash: identity.config_hash.clone(),
                                        privacy_mode: recording.privacy_mode.clone(),
                                        raw_csi_enabled: *raw_csi,
                                        events_enabled: recording.events_enabled,
                                        event_count: 0,
                                        csi_count: 0,
                                        dropped_count: 0,
                                        files_sha256: BTreeMap::new(),
                                        synced: false,
                                    };
                                    let egress = idle_egress
                                        .take()
                                        .ok_or_else(|| "storage queue unavailable".to_string())?;
                                    writer = Some(
                                        StorageWriter::new(
                                            FilesystemSessionStorage::new(
                                                &sd.mount_point,
                                                sd.rotate_bytes,
                                            ),
                                            egress,
                                            manifest,
                                        )
                                        .map_err(|error| error.to_string())?,
                                    );
                                    if *raw_csi {
                                        thread_raw_csi_gate.activate();
                                    } else {
                                        thread_raw_csi_gate.deactivate();
                                    }
                                    let active_session = writer
                                        .as_ref()
                                        .map(|writer| writer.manifest().session_id.clone())
                                        .unwrap_or_default();
                                    let _ = thread_ingress.try_push(storage_event(
                                        "storage.record.start",
                                        serde_json::json!({
                                            "storage_id": "sd",
                                            "session_id": active_session,
                                            "raw_csi": raw_csi,
                                        }),
                                    ));
                                    Ok(storage_state_value(&identity.boot_id, true, true))
                                })();
                                reply_with_state(&outbound, &command, &identity.boot_id, result);
                            }
                            AccessoryCommand::StorageRecordStop { storage_id } => {
                                let result = if storage_id != "sd" {
                                    Err("unknown storage_id".to_string())
                                } else if writer.is_some() {
                                    thread_raw_csi_gate.deactivate();
                                    let _ = thread_ingress.try_push(storage_event(
                                        "storage.record.stop",
                                        serde_json::json!({"storage_id": "sd"}),
                                    ));
                                    if let Some(active) = writer.as_mut() {
                                        while active.drain_one().unwrap_or(false) {}
                                    }
                                    match writer.take().unwrap().finish(iso8601_now()) {
                                        Ok((_backend, egress, _manifest)) => {
                                            idle_egress = Some(egress);
                                            status_led.clear(StatusColor::Recording);
                                            Ok(storage_state_value(
                                                &identity.boot_id,
                                                true,
                                                false,
                                            ))
                                        }
                                        Err(error) => {
                                            status_led.clear(StatusColor::Recording);
                                            Err(error.to_string())
                                        }
                                    }
                                } else {
                                    Err("recording is not active".to_string())
                                };
                                reply_with_state(&outbound, &command, &identity.boot_id, result);
                            }
                            AccessoryCommand::StorageSync { storage_id } => {
                                let result = if storage_id != "sd" {
                                    Err("unknown storage_id".to_string())
                                } else if writer.is_some() {
                                    Err("stop the active recording before synchronizing".to_string())
                                } else {
                                    sync_pending_sessions_in_temporary_worker(
                                        sd.mount_point.clone(),
                                        sync.clone(),
                                        server_bearer_token.clone(),
                                        identity.clone(),
                                    )
                                        .map(|count| {
                                            let _ = outbound.try_send(MqttOutbound::Event(
                                                DeviceEvent::StorageStatus {
                                                    storage_id: "sd".to_string(),
                                                    status: "synced".to_string(),
                                                    message: Some(format!(
                                                        "{count} session(s) synchronized"
                                                    )),
                                                },
                                            ));
                                            storage_state_value(
                                                &identity.boot_id,
                                                true,
                                                false,
                                            )
                                        })
                                };
                                reply_with_state(&outbound, &command, &identity.boot_id, result);
                            }
                            AccessoryCommand::StorageSelfTest {
                                storage_id,
                                payload,
                                expected_sha256,
                            } => {
                                let result = if storage_id != "sd" {
                                    Err("unknown storage_id".to_string())
                                } else if writer.is_some() {
                                    Err("stop the active recording before testing the card".to_string())
                                } else {
                                    self_test_file(
                                        &sd.mount_point,
                                        &command.request_id,
                                        payload,
                                        expected_sha256,
                                    )
                                };
                                reply_with_state(
                                    &outbound,
                                    &command,
                                    &identity.boot_id,
                                    result,
                                );
                            }
                            _ => {}
                        },
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            thread_raw_csi_gate.deactivate();
                            return;
                        }
                    }
                    if let Some(active) = writer.as_mut() {
                        if active.stats().failed {
                            thread_raw_csi_gate.deactivate();
                            status_led.clear(StatusColor::Recording);
                        } else {
                            status_led.pulse(StatusColor::Recording, 250);
                        }
                        for _ in 0..8 {
                            match active.drain_one() {
                                Ok(true) => {}
                                Ok(false) => break,
                                Err(error) => {
                                    thread_raw_csi_gate.deactivate();
                                    status_led.clear(StatusColor::Recording);
                                    log::error!("microSD writer stopped without deleting session: {error}");
                                    let _ = outbound.try_send(MqttOutbound::Event(
                                        DeviceEvent::StorageStatus {
                                            storage_id: "sd".to_string(),
                                            status: "error".to_string(),
                                            message: Some(error.to_string()),
                                        },
                                    ));
                                    let _ = outbound.try_send(MqttOutbound::State(storage_state(
                                        &identity.boot_id,
                                        true,
                                        false,
                                    )));
                                    break;
                                }
                            }
                        }
                    } else if let Some(egress) = idle_egress.as_ref() {
                        while egress.discard_one() {}
                    }
                    let _ = &thread_ingress;
                }
            })
            .map_err(|error| error.to_string())?;
        match startup_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = handle.join();
                return Err(error);
            }
            Err(error) => {
                let _ = handle.join();
                return Err(format!(
                    "SD worker terminated before reporting mount status: {error}"
                ));
            }
        }
        Ok(EspStorageRuntime {
            ingress,
            raw_csi_gate,
            commands,
            handle,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sd_worker_priority_is_explicitly_low() {
        assert!((1..=3).contains(&SD_TASK_PRIORITY));
    }

    #[test]
    fn resident_sd_memory_is_bounded_before_mount() {
        assert!(SD_WORKER_STACK_BYTES < 20 * 1024);
        assert!(SD_IO_BUFFER_BYTES <= 1024);
        assert!(SD_DMA_MAX_TRANSFER_BYTES >= 512);
        assert!(SD_VFS_MAX_OPEN_FILES >= 3);
    }

    #[test]
    fn esp32c5_storage_queue_is_capped_before_allocation() {
        assert_eq!(
            effective_storage_queue_capacity(DEFAULT_STORAGE_QUEUE_CAPACITY, "esp32c5"),
            ESP32C5_MAX_STORAGE_QUEUE_CAPACITY
        );
        assert_eq!(
            effective_storage_queue_capacity(32, "esp32c5"),
            ESP32C5_MAX_STORAGE_QUEUE_CAPACITY
        );
        assert_eq!(
            effective_storage_queue_capacity(DEFAULT_STORAGE_QUEUE_CAPACITY, "classic"),
            DEFAULT_STORAGE_QUEUE_CAPACITY
        );

        let capacity = effective_storage_queue_capacity(DEFAULT_STORAGE_QUEUE_CAPACITY, "esp32c5");
        let (ingress, _egress) = storage_queue(capacity);
        assert_eq!(ingress.bus.capacity, ESP32C5_MAX_STORAGE_QUEUE_CAPACITY);
    }

    #[derive(Default)]
    struct MemoryBackend {
        events: Vec<String>,
        csi: Vec<String>,
    }
    impl SessionStorage for MemoryBackend {
        fn begin(&mut self, _: &SdSessionManifest) -> Result<(), String> {
            Ok(())
        }
        fn append_event(&mut self, value: &str) -> Result<(), String> {
            self.events.push(value.to_string());
            Ok(())
        }
        fn append_csi(&mut self, value: &str) -> Result<(), String> {
            self.csi.push(value.to_string());
            Ok(())
        }
        fn checkpoint(&mut self, _: &SdSessionManifest) -> Result<(), String> {
            Ok(())
        }
        fn finish(&mut self, _: &SdSessionManifest) -> Result<(), String> {
            Ok(())
        }
    }

    fn manifest() -> SdSessionManifest {
        SdSessionManifest {
            schema: SD_SESSION_SCHEMA.to_string(),
            session_id: "20260715T120000Z_test".to_string(),
            device_id: "esp-test".to_string(),
            started_at_iso: "2026-07-15T12:00:00Z".to_string(),
            finished_at_iso: None,
            build_hash: "build".to_string(),
            config_hash: "config".to_string(),
            privacy_mode: PrivacyMode::EmptyRoomBench,
            raw_csi_enabled: true,
            events_enabled: true,
            event_count: 0,
            csi_count: 0,
            dropped_count: 0,
            files_sha256: BTreeMap::new(),
            synced: false,
        }
    }

    #[test]
    fn queue_is_wait_free_and_bounded() {
        let (ingress, _egress) = storage_queue(1);
        ingress
            .try_push(StorageRecord::CsiCsv("one".to_string()))
            .unwrap();
        assert_eq!(
            ingress.try_push(StorageRecord::CsiCsv("two".to_string())),
            Err(StorageError::QueueFull)
        );
        assert_eq!(ingress.dropped_count(), 1);
    }

    #[test]
    fn writer_tracks_event_and_csi_counts() {
        let (ingress, egress) = storage_queue(4);
        ingress
            .try_push(StorageRecord::Event(FirmwareEvent {
                timestamp_ms: 1,
                kind: "boot".to_string(),
                data: serde_json::Value::Null,
            }))
            .unwrap();
        ingress
            .try_push(StorageRecord::CsiCsv("CSI_DATA,1".to_string()))
            .unwrap();
        let mut writer = StorageWriter::new(MemoryBackend::default(), egress, manifest()).unwrap();
        assert!(writer.drain_one().unwrap());
        assert!(writer.drain_one().unwrap());
        assert_eq!(writer.manifest().event_count, 1);
        assert_eq!(writer.manifest().csi_count, 1);
    }

    #[test]
    fn filesystem_uses_deterministic_names_and_payload_hashes() {
        let unique = format!(
            "aethersense-storage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        let mut backend = FilesystemSessionStorage::new(&root, 4_096);
        let session = manifest();
        backend.begin(&session).unwrap();
        backend.append_event("first").unwrap();
        backend.append_csi("CSI").unwrap();
        let hashes = backend.file_hashes().unwrap();
        assert_eq!(
            hashes.get("events-0001.jsonl").map(String::as_str),
            Some("b640e840b19d378660b32fb51ae18d67dccb4a8596a29e7bd72c1b2ae5928f41")
        );
        assert_eq!(
            hashes.get("csi-0001.csv").map(String::as_str),
            Some("fd4d6d4f94094341af78abc2dc70f706d34b0d6bf1a2db44e8cda65c9e2b2de1")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn card_failure_stops_consumption_without_deleting_session() {
        struct RemovedCardBackend {
            session_exists: bool,
        }
        impl SessionStorage for RemovedCardBackend {
            fn begin(&mut self, _: &SdSessionManifest) -> Result<(), String> {
                self.session_exists = true;
                Ok(())
            }
            fn append_event(&mut self, _: &str) -> Result<(), String> {
                Err("card removed or full".to_string())
            }
            fn append_csi(&mut self, _: &str) -> Result<(), String> {
                Err("card removed or full".to_string())
            }
            fn checkpoint(&mut self, _: &SdSessionManifest) -> Result<(), String> {
                Ok(())
            }
            fn finish(&mut self, _: &SdSessionManifest) -> Result<(), String> {
                Ok(())
            }
        }
        let (ingress, egress) = storage_queue(2);
        ingress
            .try_push(StorageRecord::Event(FirmwareEvent {
                timestamp_ms: 1,
                kind: "card-test".to_string(),
                data: serde_json::Value::Null,
            }))
            .unwrap();
        let mut writer = StorageWriter::new(
            RemovedCardBackend {
                session_exists: false,
            },
            egress,
            manifest(),
        )
        .unwrap();
        assert!(writer.drain_one().is_err());
        assert!(writer.stats().failed);
        assert!(writer.backend.session_exists);
        assert!(writer.manifest().finished_at_iso.is_none());
    }
}
