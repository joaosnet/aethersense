//! MQTT command/state contracts and a retained/TTL/deduplication guard.
//!
//! The ESP MQTT transport is kept behind [`MqttTransport`] so broker I/O can
//! run in its own bounded task and command parsing remains host-testable.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::profile::DeviceProfile;

pub const COMMAND_SCHEMA: &str = "aethersense.command.v1";
pub const ACK_SCHEMA: &str = "aethersense.command_ack.v1";
pub const CAPABILITIES_SCHEMA: &str = "aethersense.capabilities.v1";
pub const STATE_SCHEMA: &str = "aethersense.accessory_state.v1";
pub const MIN_VALID_EPOCH_MS: u64 = 1_577_836_800_000; // 2020-01-01 UTC
pub const MAX_FUTURE_SKEW_MS: u64 = 30_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandEnvelope {
    #[serde(default = "default_command_schema")]
    pub schema: String,
    pub request_id: String,
    pub boot_id: Option<String>,
    pub issued_at_ms: u64,
    pub ttl_ms: u32,
    #[serde(flatten)]
    pub command: AccessoryCommand,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", content = "params")]
pub enum AccessoryCommand {
    #[serde(rename = "relay.set")]
    RelaySet { relay_id: String, state: bool },
    #[serde(rename = "relay.pulse")]
    RelayPulse { relay_id: String, duration_ms: u32 },
    #[serde(rename = "ir.learn.start")]
    IrLearnStart {
        #[serde(default)]
        code_id: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        timeout_ms: Option<u32>,
        #[serde(default)]
        idle_threshold_us: Option<u32>,
    },
    #[serde(rename = "ir.learn.cancel")]
    IrLearnCancel,
    #[serde(rename = "ir.send")]
    IrSend {
        code_id: String,
        carrier_hz: u32,
        duty_percent: u8,
        sequence_us: Vec<u32>,
        repeats: u8,
    },
    #[serde(rename = "storage.record.start")]
    StorageRecordStart {
        storage_id: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        raw_csi: bool,
    },
    #[serde(rename = "storage.record.stop")]
    StorageRecordStop { storage_id: String },
    #[serde(rename = "storage.sync")]
    StorageSync { storage_id: String },
    /// Destructive-free FAT/SDSPI commissioning check. The worker writes one
    /// bounded temporary payload, reads and hashes it, then removes it.
    #[serde(rename = "storage.self_test")]
    StorageSelfTest {
        storage_id: String,
        payload: String,
        expected_sha256: String,
    },
    /// Local USB commissioning command used to prove that the physical status
    /// LED can display a requested state. MQTT deliberately does not route it.
    #[serde(rename = "diagnostic.status_led")]
    DiagnosticStatusLed {
        color: String,
        duration_ms: u32,
    },
    /// Local USB snapshot backed by counters updated in the Wi-Fi/UDP runtime.
    #[serde(rename = "diagnostic.runtime")]
    DiagnosticRuntime,
    /// Local USB-only reboot used to prove boot/readback stability.
    #[serde(rename = "diagnostic.reboot")]
    DiagnosticReboot,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandAck {
    #[serde(default = "default_ack_schema")]
    pub schema: String,
    pub request_id: String,
    pub boot_id: String,
    /// `acknowledged`, `completed`, `ok` or `error`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<serde_json::Value>,
}

impl CommandAck {
    pub fn acknowledged(command: &CommandEnvelope, current_boot_id: &str) -> Self {
        Self {
            schema: ACK_SCHEMA.to_string(),
            request_id: command.request_id.clone(),
            boot_id: current_boot_id.to_string(),
            status: "acknowledged".to_string(),
            error: None,
            state: None,
        }
    }

    pub fn error(
        command: &CommandEnvelope,
        current_boot_id: &str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            schema: ACK_SCHEMA.to_string(),
            request_id: command.request_id.clone(),
            boot_id: current_boot_id.to_string(),
            status: "error".to_string(),
            error: Some(message.into()),
            state: None,
        }
    }

    pub fn completed(command: &CommandEnvelope, current_boot_id: &str) -> Self {
        Self {
            schema: ACK_SCHEMA.to_string(),
            request_id: command.request_id.clone(),
            boot_id: current_boot_id.to_string(),
            status: "completed".to_string(),
            error: None,
            state: None,
        }
    }

    pub fn completed_with_state(
        command: &CommandEnvelope,
        current_boot_id: &str,
        state: serde_json::Value,
    ) -> Self {
        Self {
            schema: ACK_SCHEMA.to_string(),
            request_id: command.request_id.clone(),
            boot_id: current_boot_id.to_string(),
            status: "completed".to_string(),
            error: None,
            state: Some(state),
        }
    }
}

#[derive(Clone, Debug)]
pub enum MqttOutbound {
    Ack(CommandAck),
    Event(DeviceEvent),
    State(AccessoryStateDocument),
}

/// Bounded command fan-out. MQTT callbacks only enqueue; GPIO/RMT/SD I/O is
/// performed by the dedicated accessory tasks.
#[derive(Clone, Default)]
pub struct AccessoryDispatch {
    pub relay: Option<SyncSender<CommandEnvelope>>,
    pub ir: Option<SyncSender<CommandEnvelope>>,
    storage: Arc<Mutex<Option<SyncSender<CommandEnvelope>>>>,
}

impl AccessoryDispatch {
    pub fn new(
        relay: Option<SyncSender<CommandEnvelope>>,
        ir: Option<SyncSender<CommandEnvelope>>,
    ) -> Self {
        Self {
            relay,
            ir,
            storage: Arc::new(Mutex::new(None)),
        }
    }

    /// Attach storage after the network and MQTT clients have reserved their
    /// long-lived heap. Every clone observes the new sender, so MQTT and local
    /// USB commissioning can start safely before FAT/SDSPI is mounted.
    pub fn attach_storage(
        &self,
        storage: Option<SyncSender<CommandEnvelope>>,
    ) -> Result<(), String> {
        *self
            .storage
            .lock()
            .map_err(|_| "accessory storage dispatch is unavailable".to_string())? = storage;
        Ok(())
    }

    pub fn dispatch(&self, envelope: CommandEnvelope) -> Result<(), String> {
        let sender = match &envelope.command {
            AccessoryCommand::RelaySet { .. } | AccessoryCommand::RelayPulse { .. } => {
                self.relay.clone()
            }
            AccessoryCommand::IrLearnStart { .. }
            | AccessoryCommand::IrLearnCancel
            | AccessoryCommand::IrSend { .. } => self.ir.clone(),
            AccessoryCommand::StorageRecordStart { .. }
            | AccessoryCommand::StorageRecordStop { .. }
            | AccessoryCommand::StorageSync { .. }
            | AccessoryCommand::StorageSelfTest { .. } => self
                .storage
                .lock()
                .map_err(|_| "accessory storage dispatch is unavailable".to_string())?
                .clone(),
            AccessoryCommand::DiagnosticStatusLed { .. }
            | AccessoryCommand::DiagnosticRuntime
            | AccessoryCommand::DiagnosticReboot => {
                return Err(
                    "diagnostic.status_led is available only over local USB commissioning"
                        .to_string(),
                )
            }
        }
        .ok_or_else(|| "requested accessory is disabled or unavailable".to_string())?;
        sender.try_send(envelope).map_err(|error| match error {
            TrySendError::Full(_) => "accessory command queue is full".to_string(),
            TrySendError::Disconnected(_) => "accessory task is offline".to_string(),
        })
    }
}

const MAX_LOCAL_COMMISSION_RESULTS: usize = 32;

#[derive(Default)]
struct LocalCommissioningState {
    dispatch: AccessoryDispatch,
    results: BTreeMap<String, CommandAck>,
    order: VecDeque<String>,
    runtime_diagnostics: Option<RuntimeDiagnosticCounters>,
}

#[derive(Clone)]
struct RuntimeDiagnosticCounters {
    last_udp_send_secs: Arc<AtomicU32>,
    dropped_packets: Arc<AtomicUsize>,
    transmitted_packets: Arc<AtomicUsize>,
}

/// Local, physical commissioning bridge shared by the serial server and the
/// accessory workers. It deliberately bypasses MQTT authentication because a
/// caller must have the board's USB console open, while still using the same
/// bounded worker queues and command contracts as remote control.
#[derive(Clone)]
pub struct AccessoryCommissioning {
    boot_id: String,
    state: Arc<Mutex<LocalCommissioningState>>,
}

impl AccessoryCommissioning {
    pub fn new(boot_id: impl Into<String>) -> Self {
        Self {
            boot_id: boot_id.into(),
            state: Arc::new(Mutex::new(LocalCommissioningState::default())),
        }
    }

    pub fn attach(&self, dispatch: AccessoryDispatch) -> Result<(), String> {
        self.state
            .lock()
            .map_err(|_| "local commissioning state is unavailable".to_string())?
            .dispatch = dispatch;
        Ok(())
    }

    pub fn attach_runtime_diagnostics(
        &self,
        last_udp_send_secs: Arc<AtomicU32>,
        dropped_packets: Arc<AtomicUsize>,
        transmitted_packets: Arc<AtomicUsize>,
    ) -> Result<(), String> {
        self.state
            .lock()
            .map_err(|_| "local commissioning state is unavailable".to_string())?
            .runtime_diagnostics = Some(RuntimeDiagnosticCounters {
                last_udp_send_secs,
                dropped_packets,
                transmitted_packets,
            });
        Ok(())
    }

    pub fn runtime_diagnostics(&self) -> Result<serde_json::Value, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "local commissioning state is unavailable".to_string())?;
        let counters = state
            .runtime_diagnostics
            .as_ref()
            .ok_or_else(|| "network/CSI runtime diagnostics are not ready".to_string())?;
        let last_udp_send_secs = counters.last_udp_send_secs.load(Ordering::Relaxed);
        Ok(serde_json::json!({
            "event": "diagnostic.runtime",
            "boot_id": self.boot_id.clone(),
            "network_ready": last_udp_send_secs > 0,
            "csi_streaming": last_udp_send_secs > 0,
            "last_udp_send_secs": last_udp_send_secs,
            "dropped_packets": counters.dropped_packets.load(Ordering::Relaxed),
            "transmitted_packets": counters.transmitted_packets.load(Ordering::Relaxed),
            "hardware_observation_required": false,
        }))
    }

    pub fn boot_id(&self) -> &str {
        &self.boot_id
    }

    pub fn dispatch(&self, command: CommandEnvelope) -> CommandAck {
        if let Some(actual) = command.boot_id.as_ref() {
            if actual != &self.boot_id {
                return CommandAck::error(
                    &command,
                    &self.boot_id,
                    format!(
                        "command boot_id {actual} does not match {}",
                        self.boot_id
                    ),
                );
            }
        }
        let dispatch = match self.state.lock() {
            Ok(state) => state.dispatch.clone(),
            Err(_) => {
                return CommandAck::error(
                    &command,
                    &self.boot_id,
                    "local commissioning state is unavailable",
                )
            }
        };
        let ack = match dispatch.dispatch(command.clone()) {
            Ok(()) => CommandAck::acknowledged(&command, &self.boot_id),
            Err(error) => CommandAck::error(&command, &self.boot_id, error),
        };
        self.record(ack.clone());
        ack
    }

    pub fn observe(&self, outbound: &MqttOutbound) {
        if let MqttOutbound::Ack(ack) = outbound {
            self.record(ack.clone());
        }
    }

    pub fn result(&self, request_id: &str) -> Option<CommandAck> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.results.get(request_id).cloned())
    }

    fn record(&self, ack: CommandAck) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if !state.results.contains_key(&ack.request_id) {
            while state.order.len() >= MAX_LOCAL_COMMISSION_RESULTS {
                if let Some(expired) = state.order.pop_front() {
                    state.results.remove(&expired);
                }
            }
            state.order.push_back(ack.request_id.clone());
        }
        state.results.insert(ack.request_id.clone(), ack);
    }
}

/// Outbound sink used by accessory workers. Local serial results are recorded
/// before the best-effort MQTT enqueue, so USB commissioning continues to work
/// when MQTT is disabled, disconnected or its queue is full.
#[derive(Clone)]
pub struct AccessoryOutbound {
    mqtt: SyncSender<MqttOutbound>,
    commissioning: AccessoryCommissioning,
}

impl AccessoryOutbound {
    pub fn new(mqtt: SyncSender<MqttOutbound>, commissioning: AccessoryCommissioning) -> Self {
        Self {
            mqtt,
            commissioning,
        }
    }

    pub fn try_send(&self, outbound: MqttOutbound) -> Result<(), TrySendError<MqttOutbound>> {
        self.commissioning.observe(&outbound);
        self.mqtt.try_send(outbound)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapabilitiesDocument {
    #[serde(default = "default_capabilities_schema")]
    pub schema: String,
    pub name: Option<String>,
    pub boot_id: Option<String>,
    #[serde(default)]
    pub capabilities: BTreeMap<String, serde_json::Value>,
    pub accessories: Vec<AccessoryCapability>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccessoryCapability {
    pub id: String,
    pub kind: AccessoryKind,
    pub name: String,
    #[serde(default)]
    pub capabilities: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessoryKind {
    Relay,
    IrTransmitter,
    IrReceiver,
    Storage,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccessoryStateDocument {
    #[serde(default = "default_state_schema")]
    pub schema: String,
    pub boot_id: String,
    pub accessories: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DeviceEvent {
    #[serde(rename = "ir.learned")]
    IrLearned { code: LearnedIrCode },
    #[serde(rename = "storage.status")]
    StorageStatus {
        storage_id: String,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LearnedIrCode {
    pub id: Option<String>,
    pub device_id: String,
    pub name: String,
    pub carrier_hz: u32,
    /// Fractional duty cycle accepted by the server (`0.33` for 33%).
    pub duty_cycle: f32,
    pub sequence_us: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceTopics {
    pub command: String,
    pub ack: String,
    pub event: String,
    pub state: String,
    pub capabilities: String,
    pub availability: String,
}

impl DeviceTopics {
    pub fn new(base_topic: &str, device_id: &str) -> Result<Self, CommandError> {
        let base = base_topic.trim_matches('/');
        let device = device_id.trim_matches('/');
        if base.is_empty() || device.is_empty() || device.contains('+') || device.contains('#') {
            return Err(CommandError::InvalidTopic);
        }
        let prefix = format!("{base}/{device}");
        Ok(Self {
            command: format!("{prefix}/command"),
            ack: format!("{prefix}/ack"),
            event: format!("{prefix}/event"),
            state: format!("{prefix}/state"),
            capabilities: format!("{prefix}/capabilities"),
            availability: format!("{prefix}/availability"),
        })
    }
}

impl CapabilitiesDocument {
    pub fn from_profile(profile: &DeviceProfile, boot_id: impl Into<String>) -> Self {
        let mut accessories = Vec::new();
        for relay in profile
            .peripherals
            .relays
            .iter()
            .filter(|relay| relay.enabled)
        {
            accessories.push(AccessoryCapability {
                id: relay.id.clone(),
                kind: AccessoryKind::Relay,
                name: relay.name.clone(),
                capabilities: BTreeMap::from([
                    ("pulse".to_string(), serde_json::Value::Bool(true)),
                    ("pulse_limit_ms".to_string(), relay.pulse_limit_ms.into()),
                ]),
            });
        }
        if profile.peripherals.ir_tx.enabled {
            accessories.push(AccessoryCapability {
                id: "ir-tx".to_string(),
                kind: AccessoryKind::IrTransmitter,
                name: "IR transmitter".to_string(),
                capabilities: BTreeMap::from([(
                    "raw_replay".to_string(),
                    serde_json::Value::Bool(true),
                )]),
            });
        }
        if profile.peripherals.ir_rx.enabled {
            accessories.push(AccessoryCapability {
                id: "ir-rx".to_string(),
                kind: AccessoryKind::IrReceiver,
                name: "IR receiver".to_string(),
                capabilities: BTreeMap::from([(
                    "raw_learning".to_string(),
                    serde_json::Value::Bool(true),
                )]),
            });
        }
        if profile.storage.sd.enabled {
            accessories.push(AccessoryCapability {
                id: "sd".to_string(),
                kind: AccessoryKind::Storage,
                name: "microSD".to_string(),
                capabilities: BTreeMap::from([
                    (
                        "events".to_string(),
                        profile.storage.recording.events_enabled.into(),
                    ),
                    (
                        "raw_csi".to_string(),
                        profile.storage.recording.raw_csi_enabled.into(),
                    ),
                    ("network_sync".to_string(), profile.sync.enabled.into()),
                ]),
            });
        }
        Self {
            schema: CAPABILITIES_SCHEMA.to_string(),
            name: Some(profile.identity.name.clone()),
            boot_id: Some(boot_id.into()),
            capabilities: BTreeMap::from([
                ("command_qos".to_string(), 1.into()),
                ("raw_ir".to_string(), true.into()),
            ]),
            accessories,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandError {
    InvalidTopic,
    RetainedCommand,
    InvalidJson(String),
    InvalidSchema,
    InvalidRequestId,
    WrongBoot { expected: String, actual: String },
    ClockUnsynchronized,
    IssuedInFuture,
    Expired,
    Duplicate,
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTopic => f.write_str("invalid MQTT topic"),
            Self::RetainedCommand => f.write_str("retained commands are forbidden"),
            Self::InvalidJson(message) => write!(f, "invalid command JSON: {message}"),
            Self::InvalidSchema => f.write_str("unsupported command schema"),
            Self::InvalidRequestId => f.write_str("request_id is required"),
            Self::WrongBoot { expected, actual } => {
                write!(f, "command boot_id {actual} does not match {expected}")
            }
            Self::ClockUnsynchronized => {
                f.write_str("device clock is not synchronized; command rejected fail-closed")
            }
            Self::IssuedInFuture => f.write_str("command issued_at is too far in the future"),
            Self::Expired => f.write_str("command TTL expired"),
            Self::Duplicate => f.write_str("duplicate request_id"),
        }
    }
}

impl std::error::Error for CommandError {}

pub struct CommandGuard {
    boot_id: String,
    seen: VecDeque<String>,
    capacity: usize,
}

impl CommandGuard {
    pub fn new(boot_id: impl Into<String>, dedupe_capacity: usize) -> Self {
        Self {
            boot_id: boot_id.into(),
            seen: VecDeque::with_capacity(dedupe_capacity.max(1)),
            capacity: dedupe_capacity.max(1),
        }
    }

    pub fn parse_and_accept(
        &mut self,
        payload: &[u8],
        retained: bool,
        now_ms: u64,
    ) -> Result<CommandEnvelope, CommandError> {
        if retained {
            return Err(CommandError::RetainedCommand);
        }
        let command: CommandEnvelope = serde_json::from_slice(payload)
            .map_err(|err| CommandError::InvalidJson(err.to_string()))?;
        if command.schema != COMMAND_SCHEMA {
            return Err(CommandError::InvalidSchema);
        }
        if command.request_id.trim().is_empty() {
            return Err(CommandError::InvalidRequestId);
        }
        if let Some(actual) = command.boot_id.as_ref() {
            if actual != &self.boot_id {
                return Err(CommandError::WrongBoot {
                    expected: self.boot_id.clone(),
                    actual: actual.clone(),
                });
            }
        }
        if now_ms < MIN_VALID_EPOCH_MS {
            return Err(CommandError::ClockUnsynchronized);
        }
        if command.issued_at_ms > now_ms.saturating_add(MAX_FUTURE_SKEW_MS) {
            return Err(CommandError::IssuedInFuture);
        }
        if command.ttl_ms == 0
            || now_ms.saturating_sub(command.issued_at_ms) > u64::from(command.ttl_ms)
        {
            return Err(CommandError::Expired);
        }
        if self.seen.contains(&command.request_id) {
            return Err(CommandError::Duplicate);
        }
        if self.seen.len() == self.capacity {
            self.seen.pop_front();
        }
        self.seen.push_back(command.request_id.clone());
        Ok(command)
    }
}

pub trait MqttTransport {
    fn subscribe_qos1(&mut self, topic: &str) -> Result<(), String>;
    fn publish(
        &mut self,
        topic: &str,
        payload: &[u8],
        qos: u8,
        retained: bool,
    ) -> Result<(), String>;
}

pub struct MqttCommandChannel<T> {
    transport: T,
    topics: DeviceTopics,
    guard: CommandGuard,
}

impl<T: MqttTransport> MqttCommandChannel<T> {
    pub fn new(
        mut transport: T,
        topics: DeviceTopics,
        boot_id: impl Into<String>,
    ) -> Result<Self, String> {
        transport.subscribe_qos1(&topics.command)?;
        Ok(Self {
            transport,
            topics,
            guard: CommandGuard::new(boot_id, 128),
        })
    }

    pub fn receive(
        &mut self,
        topic: &str,
        payload: &[u8],
        retained: bool,
        now_ms: u64,
    ) -> Result<CommandEnvelope, CommandError> {
        if topic != self.topics.command {
            return Err(CommandError::InvalidTopic);
        }
        self.guard.parse_and_accept(payload, retained, now_ms)
    }

    pub fn publish_ack(&mut self, ack: &CommandAck) -> Result<(), String> {
        let payload = serde_json::to_vec(ack).map_err(|err| err.to_string())?;
        self.transport.publish(&self.topics.ack, &payload, 1, false)
    }

    pub fn publish_capabilities(
        &mut self,
        capabilities: &CapabilitiesDocument,
    ) -> Result<(), String> {
        let payload = serde_json::to_vec(capabilities).map_err(|err| err.to_string())?;
        self.transport
            .publish(&self.topics.capabilities, &payload, 1, true)
    }

    pub fn publish_state(&mut self, state: &AccessoryStateDocument) -> Result<(), String> {
        let payload = serde_json::to_vec(state).map_err(|err| err.to_string())?;
        self.transport
            .publish(&self.topics.state, &payload, 1, true)
    }

    pub fn into_transport(self) -> T {
        self.transport
    }
}

fn default_command_schema() -> String {
    COMMAND_SCHEMA.to_string()
}
fn default_ack_schema() -> String {
    ACK_SCHEMA.to_string()
}
fn default_capabilities_schema() -> String {
    CAPABILITIES_SCHEMA.to_string()
}
fn default_state_schema() -> String {
    STATE_SCHEMA.to_string()
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub mod esp {
    use super::*;
    use std::sync::mpsc::{sync_channel, Receiver, TryRecvError};
    use std::thread::JoinHandle;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use esp_idf_svc::mqtt::client::{
        Details, EspMqttClient, EventPayload, LwtConfiguration, MqttClientConfiguration, QoS,
    };
    use esp_idf_svc::sntp::EspSntp;

    use crate::provisioning::SecretString;

    enum Inbound {
        Connected,
        Disconnected,
        Received { topic: String, data: Vec<u8> },
    }

    fn epoch_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0)
    }

    fn publish_json<T: Serialize>(
        client: &mut EspMqttClient<'static>,
        topic: &str,
        value: &T,
        retained: bool,
    ) -> Result<(), String> {
        let payload = serde_json::to_vec(value).map_err(|error| error.to_string())?;
        client
            .enqueue(topic, QoS::AtLeastOnce, retained, &payload)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub fn generate_boot_id() -> String {
        format!(
            "{:08x}{:08x}",
            unsafe { esp_idf_sys::esp_random() },
            unsafe { esp_idf_sys::esp_random() }
        )
    }

    pub fn spawn_mqtt_runtime(
        profile: DeviceProfile,
        password: SecretString,
        boot_id: String,
        dispatch: AccessoryDispatch,
        outbound: Receiver<MqttOutbound>,
    ) -> Result<JoinHandle<()>, String> {
        if !profile.mqtt.enabled {
            return Err("MQTT is disabled".to_string());
        }
        let device_id = profile
            .identity
            .device_id
            .clone()
            .ok_or_else(|| "MQTT requires paired device_id".to_string())?;
        let topics = DeviceTopics::new(&profile.mqtt.base_topic, &device_id)
            .map_err(|error| error.to_string())?;
        let (startup_tx, startup_rx) = sync_channel::<Result<(), String>>(1);
        let handle = std::thread::Builder::new()
            .name("aether-mqtt".to_string())
            .stack_size(6 * 1024)
            .spawn(move || {
                let heap_caps = esp_idf_sys::MALLOC_CAP_8BIT as u32;
                log::warn!(
                    "MQTT startup heap: free_8bit={} largest_8bit={}",
                    unsafe { esp_idf_sys::heap_caps_get_free_size(heap_caps) },
                    unsafe { esp_idf_sys::heap_caps_get_largest_free_block(heap_caps) }
                );
                // SNTP owns the wall-clock contract used by command TTL. Until
                // it reaches a valid epoch, CommandGuard rejects fail-closed.
                let _sntp = match EspSntp::new_default() {
                    Ok(service) => Some(service),
                    Err(error) => {
                        log::error!("SNTP initialization failed; MQTT commands remain fail-closed: {error}");
                        None
                    }
                };
                let scheme = if profile.mqtt.tls { "mqtts" } else { "mqtt" };
                let url = format!(
                    "{scheme}://{}:{}",
                    profile.mqtt.broker_host, profile.mqtt.port
                );
                let (inbound_tx, inbound_rx) = sync_channel::<Inbound>(32);
                let callback_tx = inbound_tx.clone();
                let lwt_payload = b"offline";
                let configuration = MqttClientConfiguration {
                    client_id: Some(&device_id),
                    username: profile.mqtt.username.as_deref(),
                    password: (!password.expose().is_empty()).then_some(password.expose()),
                    lwt: Some(LwtConfiguration {
                        topic: &topics.availability,
                        payload: lwt_payload,
                        qos: QoS::AtLeastOnce,
                        retain: true,
                    }),
                    keep_alive_interval: Some(Duration::from_secs(30)),
                    reconnect_timeout: Some(Duration::from_secs(5)),
                    // C5 has no PSRAM on the supported development board. Keep
                    // enough room for raw IR commands while leaving FAT/SDSPI
                    // headroom for its mount and worker queues.
                    task_stack: 6 * 1024,
                    buffer_size: 4 * 1024,
                    out_buffer_size: 2 * 1024,
                    outbox_limit: Some(8 * 1024),
                    ..Default::default()
                };
                let mut client = match EspMqttClient::new_cb(&url, &configuration, move |event| {
                    let message = match event.payload() {
                        EventPayload::Connected(_) => Some(Inbound::Connected),
                        EventPayload::Disconnected => Some(Inbound::Disconnected),
                        EventPayload::Received {
                            topic,
                            data,
                            details: Details::Complete,
                            ..
                        } if data.len() <= 16 * 1024 => Some(Inbound::Received {
                            topic: topic.unwrap_or_default().to_string(),
                            data: data.to_vec(),
                        }),
                        _ => None,
                    };
                    if let Some(message) = message {
                        let _ = callback_tx.try_send(message);
                    }
                }) {
                    Ok(client) => client,
                    Err(error) => {
                        let message = format!("MQTT client initialization failed: {error}");
                        log::error!("{message}");
                        let _ = startup_tx.send(Err(message));
                        return;
                    }
                };
                log::warn!(
                    "MQTT client allocated: free_8bit={} largest_8bit={}",
                    unsafe { esp_idf_sys::heap_caps_get_free_size(heap_caps) },
                    unsafe { esp_idf_sys::heap_caps_get_largest_free_block(heap_caps) }
                );
                let _ = startup_tx.send(Ok(()));
                let capabilities = CapabilitiesDocument::from_profile(&profile, boot_id.clone());
                let mut guard = CommandGuard::new(boot_id.clone(), 128);
                let mut connected_at_ms = u64::MAX;
                loop {
                    match inbound_rx.recv_timeout(Duration::from_millis(50)) {
                        Ok(Inbound::Connected) => {
                            connected_at_ms = epoch_ms();
                            if let Err(error) = client.subscribe(&topics.command, QoS::AtLeastOnce) {
                                log::warn!("MQTT command subscribe failed: {error}");
                            }
                            let _ = client.enqueue(
                                &topics.availability,
                                QoS::AtLeastOnce,
                                true,
                                b"online",
                            );
                            let _ = publish_json(
                                &mut client,
                                &topics.capabilities,
                                &capabilities,
                                true,
                            );
                        }
                        Ok(Inbound::Disconnected) => log::warn!("MQTT disconnected; reconnecting"),
                        Ok(Inbound::Received { topic, data }) => {
                            // esp-idf-svc does not expose the retain bit in its safe
                            // event payload. TTL + deduplication still reject stale
                            // retained commands; broker ACL must forbid retained
                            // publishes on the command topic.
                            let preliminary = serde_json::from_slice::<CommandEnvelope>(&data);
                            if let Ok(command) = preliminary.as_ref() {
                                if command.boot_id.as_deref() != Some(boot_id.as_str())
                                    || command.issued_at_ms < connected_at_ms
                                {
                                    let ack = CommandAck::error(
                                        command,
                                        &boot_id,
                                        "command must target the current boot and be issued after this MQTT connection",
                                    );
                                    let _ = publish_json(&mut client, &topics.ack, &ack, false);
                                    continue;
                                }
                            }
                            match guard.parse_and_accept(&data, false, epoch_ms()) {
                                Ok(command) if topic == topics.command => {
                                    let ack = match dispatch.dispatch(command.clone()) {
                                        Ok(()) => CommandAck::acknowledged(&command, &boot_id),
                                        Err(error) => CommandAck::error(&command, &boot_id, error),
                                    };
                                    let _ = publish_json(&mut client, &topics.ack, &ack, false);
                                }
                                Ok(_) => log::warn!("MQTT command arrived on unexpected topic"),
                                Err(error) => {
                                    if let Ok(command) = serde_json::from_slice::<CommandEnvelope>(&data) {
                                        let ack = CommandAck::error(&command, &boot_id, error.to_string());
                                        let _ = publish_json(&mut client, &topics.ack, &ack, false);
                                    }
                                }
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                    loop {
                        match outbound.try_recv() {
                            Ok(MqttOutbound::Ack(ack)) => {
                                let _ = publish_json(&mut client, &topics.ack, &ack, false);
                            }
                            Ok(MqttOutbound::Event(event)) => {
                                let _ = publish_json(&mut client, &topics.event, &event, false);
                            }
                            Ok(MqttOutbound::State(state)) => {
                                let _ = publish_json(&mut client, &topics.state, &state, true);
                            }
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => break,
                        }
                    }
                }
            })
            .map_err(|error| error.to_string())?;

        match startup_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(())) => Ok(handle),
            Ok(Err(error)) => {
                let _ = handle.join();
                Err(error)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                Err("MQTT client startup timed out before allocating its runtime".to_string())
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let _ = handle.join();
                Err("MQTT client stopped during startup".to_string())
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn command() -> CommandEnvelope {
        CommandEnvelope {
            schema: COMMAND_SCHEMA.to_string(),
            request_id: "request-1".to_string(),
            boot_id: Some("boot-1".to_string()),
            issued_at_ms: 1_784_116_800_000,
            ttl_ms: 5_000,
            command: AccessoryCommand::RelaySet {
                relay_id: "lamp".to_string(),
                state: true,
            },
        }
    }

    #[test]
    fn relay_command_wire_shape_uses_action_and_params() {
        let json = serde_json::to_value(command()).unwrap();
        assert_eq!(json["action"], "relay.set");
        assert_eq!(json["params"]["relay_id"], "lamp");
        assert_eq!(json["params"]["state"], true);
    }

    #[test]
    fn local_usb_commissioning_observes_completion_without_mqtt() {
        let commissioning = AccessoryCommissioning::new("boot-local");
        let (relay_tx, relay_rx) = std::sync::mpsc::sync_channel(1);
        commissioning
            .attach(AccessoryDispatch {
                relay: Some(relay_tx),
                ..AccessoryDispatch::default()
            })
            .unwrap();

        let mut envelope = command();
        envelope.boot_id = Some("boot-local".to_string());
        let accepted = commissioning.dispatch(envelope.clone());
        assert_eq!(accepted.status, "acknowledged");
        assert_eq!(relay_rx.recv().unwrap(), envelope);

        let (mqtt_tx, mqtt_rx) = std::sync::mpsc::sync_channel(1);
        let outbound = AccessoryOutbound::new(mqtt_tx, commissioning.clone());
        outbound
            .try_send(MqttOutbound::Ack(CommandAck::completed_with_state(
                &envelope,
                "boot-local",
                serde_json::json!({"state": true}),
            )))
            .unwrap();

        let completed = commissioning.result(&envelope.request_id).unwrap();
        assert_eq!(completed.status, "completed");
        assert_eq!(completed.state, Some(serde_json::json!({"state": true})));
        assert!(matches!(mqtt_rx.recv().unwrap(), MqttOutbound::Ack(_)));
    }

    #[test]
    fn runtime_diagnostics_report_live_counters_without_exposing_csi_payloads() {
        let commissioning = AccessoryCommissioning::new("boot-local");
        let last_send = Arc::new(AtomicU32::new(12));
        let dropped = Arc::new(AtomicUsize::new(3));
        commissioning
            .attach_runtime_diagnostics(
                last_send.clone(),
                dropped.clone(),
                Arc::new(AtomicUsize::new(100)),
            )
            .unwrap();

        let snapshot = commissioning.runtime_diagnostics().unwrap();
        assert_eq!(snapshot["network_ready"], true);
        assert_eq!(snapshot["csi_streaming"], true);
        assert_eq!(snapshot["last_udp_send_secs"], 12);
        assert_eq!(snapshot["dropped_packets"], 3);
        assert_eq!(snapshot["transmitted_packets"], 100);
        assert!(snapshot.get("sequence_us").is_none());

        last_send.store(13, Ordering::Relaxed);
        assert_eq!(
            commissioning.runtime_diagnostics().unwrap()["last_udp_send_secs"],
            13
        );
    }

    #[test]
    fn local_usb_commissioning_rejects_a_stale_boot_id_before_hardware_dispatch() {
        let commissioning = AccessoryCommissioning::new("boot-current");
        let (relay_tx, relay_rx) = std::sync::mpsc::sync_channel(1);
        commissioning
            .attach(AccessoryDispatch {
                relay: Some(relay_tx),
                ..AccessoryDispatch::default()
            })
            .unwrap();

        let rejected = commissioning.dispatch(command());
        assert_eq!(rejected.status, "error");
        assert_eq!(rejected.boot_id, "boot-current");
        assert!(rejected.error.unwrap().contains("does not match"));
        assert!(relay_rx.try_recv().is_err());
    }

    #[test]
    fn status_led_diagnostic_is_serializable_but_never_routed_over_mqtt() {
        let mut envelope = command();
        envelope.command = AccessoryCommand::DiagnosticStatusLed {
            color: "ir_rx".to_string(),
            duration_ms: 750,
        };
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["action"], "diagnostic.status_led");
        assert_eq!(json["params"]["color"], "ir_rx");
        assert_eq!(json["params"]["duration_ms"], 750);

        let error = AccessoryDispatch::default().dispatch(envelope).unwrap_err();
        assert!(error.contains("only over local USB"));
    }

    #[test]
    fn server_golden_command_without_known_boot_id_is_accepted() {
        let payload = br#"{
            "schema":"aethersense.command.v1",
            "request_id":"server-42",
            "boot_id":null,
            "issued_at_ms":1784116800000,
            "ttl_ms":5000,
            "action":"ir.learn.start",
            "params":{"name":"Ar condicionado","timeout_ms":15000}
        }"#;
        let mut guard = CommandGuard::new("boot-1", 8);
        let parsed = guard
            .parse_and_accept(payload, false, 1_784_116_800_500)
            .unwrap();
        assert!(matches!(
            parsed.command,
            AccessoryCommand::IrLearnStart { name: Some(_), .. }
        ));
    }

    #[test]
    fn shared_server_golden_ir_command_is_accepted() {
        let payload = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../protocol/aethersense-command-v1.golden.json"
        ));
        let mut guard = CommandGuard::new("boot-not-known-to-server", 8);
        let parsed = guard
            .parse_and_accept(payload, false, 1_784_116_800_500)
            .unwrap();
        assert!(matches!(
            parsed.command,
            AccessoryCommand::IrSend {
                duty_percent: 33,
                repeats: 1,
                ..
            }
        ));
    }

    #[test]
    fn retained_expired_and_duplicate_commands_are_rejected() {
        let payload = serde_json::to_vec(&command()).unwrap();
        let mut guard = CommandGuard::new("boot-1", 8);
        assert_eq!(
            guard.parse_and_accept(&payload, true, 1_784_116_800_001),
            Err(CommandError::RetainedCommand)
        );
        assert!(guard
            .parse_and_accept(&payload, false, 1_784_116_800_001)
            .is_ok());
        assert_eq!(
            guard.parse_and_accept(&payload, false, 1_784_116_800_002),
            Err(CommandError::Duplicate)
        );
        let mut guard = CommandGuard::new("boot-1", 8);
        assert_eq!(
            guard.parse_and_accept(&payload, false, 1_784_116_807_000),
            Err(CommandError::Expired)
        );
    }

    #[test]
    fn clock_is_fail_closed_and_future_skew_is_bounded() {
        let payload = serde_json::to_vec(&command()).unwrap();
        let mut guard = CommandGuard::new("boot-1", 8);
        assert_eq!(
            guard.parse_and_accept(&payload, false, 42_000),
            Err(CommandError::ClockUnsynchronized)
        );

        let mut future = command();
        future.issued_at_ms = 1_784_116_900_000;
        let payload = serde_json::to_vec(&future).unwrap();
        assert_eq!(
            guard.parse_and_accept(&payload, false, 1_784_116_800_000),
            Err(CommandError::IssuedInFuture)
        );
    }

    #[test]
    fn topics_match_server_contract() {
        let topics = DeviceTopics::new("aethersense/devices", "esp-1").unwrap();
        assert_eq!(topics.command, "aethersense/devices/esp-1/command");
        assert_eq!(
            topics.availability,
            "aethersense/devices/esp-1/availability"
        );
    }

    #[test]
    fn learned_event_uses_fractional_server_duty_cycle() {
        let event = DeviceEvent::IrLearned {
            code: LearnedIrCode {
                id: Some("power".to_string()),
                device_id: "esp32-020aff1000b7".to_string(),
                name: "Power".to_string(),
                carrier_hz: 38_000,
                duty_cycle: 0.33,
                sequence_us: vec![9_000, 4_500],
            },
        };
        let json = serde_json::to_value(event).unwrap();
        let duty = json["code"]["duty_cycle"].as_f64().unwrap();
        assert!((duty - 0.33).abs() < 0.000_001);
    }
}
