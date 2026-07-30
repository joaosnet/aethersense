//! Persistent accessory, infrared-code and command domain.
//!
//! The wire contract deliberately uses JSON objects for peripheral-specific
//! state/capabilities.  That keeps the server compatible with firmware
//! revisions while the stable envelope (ids, status, timestamps and actions)
//! remains strongly typed here.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

pub const ACCESSORY_SCHEMA: &str = "aethersense.accessories.v1";
pub const MAX_COMMANDS: usize = 256;
pub const MAX_DISCOVERY_TOMBSTONES: usize = 256;
pub const DEFAULT_COMMAND_TTL_MS: u64 = 15_000;
pub const MAX_COMMAND_TTL_MS: u64 = 300_000;
pub const IMPLICIT_COMMAND_DEDUP_MS: i64 = 1_500;
pub const MAX_MQTT_ACCESSORY_PAYLOAD_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccessoryKind {
    Relay,
    IrTransmitter,
    IrReceiver,
    Storage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryKind {
    Relay,
    IrCode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveryTombstone {
    pub kind: DiscoveryKind,
    pub device_id: String,
    pub entity_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Accessory {
    pub id: String,
    pub device_id: String,
    pub kind: AccessoryKind,
    pub name: String,
    #[serde(default)]
    pub available: bool,
    #[serde(default = "empty_object")]
    pub state: Value,
    #[serde(default = "empty_object")]
    pub capabilities: Value,
    #[serde(default = "now_rfc3339")]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccessoryInput {
    pub id: String,
    pub kind: AccessoryKind,
    pub name: String,
    #[serde(default = "empty_object")]
    pub capabilities: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceRuntime {
    pub device_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default = "empty_object")]
    pub capabilities: Value,
    #[serde(default)]
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IrCode {
    pub id: String,
    pub device_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub appliance_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub appliance_name: String,
    pub name: String,
    #[serde(default = "default_carrier_hz")]
    pub carrier_hz: u32,
    #[serde(default = "default_duty_cycle")]
    pub duty_cycle: f32,
    pub sequence_us: Vec<u32>,
    #[serde(default = "now_rfc3339")]
    pub created_at: String,
    #[serde(default = "now_rfc3339")]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IrCodeInput {
    #[serde(default)]
    pub id: Option<String>,
    pub device_id: String,
    #[serde(default)]
    pub appliance_id: String,
    #[serde(default)]
    pub appliance_name: String,
    pub name: String,
    #[serde(default = "default_carrier_hz")]
    pub carrier_hz: u32,
    #[serde(default = "default_duty_cycle")]
    pub duty_cycle: f32,
    pub sequence_us: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    Pending,
    Acknowledged,
    Completed,
    Failed,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccessoryCommand {
    pub request_id: String,
    pub device_id: String,
    pub action: String,
    #[serde(default = "empty_object")]
    pub params: Value,
    pub status: CommandStatus,
    pub created_at: String,
    pub expires_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommandInput {
    #[serde(default)]
    pub request_id: Option<String>,
    pub device_id: String,
    pub action: String,
    #[serde(default = "empty_object")]
    pub params: Value,
    #[serde(default)]
    pub ttl_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessoryRegistry {
    #[serde(default = "default_schema")]
    pub schema: String,
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub accessories: Vec<Accessory>,
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceRuntime>,
    #[serde(default)]
    pub ir_codes: Vec<IrCode>,
    #[serde(default)]
    pub commands: VecDeque<AccessoryCommand>,
    /// SHA-256 only; the original per-device upload token is never persisted.
    #[serde(default)]
    pub device_credential_hashes: BTreeMap<String, String>,
    /// Retained MQTT Discovery configs that must be cleared on the next
    /// connection. Kept as idempotent tombstones so offline deletion is safe.
    #[serde(default)]
    pub discovery_tombstones: Vec<DiscoveryTombstone>,
}

impl Default for AccessoryRegistry {
    fn default() -> Self {
        Self {
            schema: default_schema(),
            version: 0,
            accessories: Vec::new(),
            devices: BTreeMap::new(),
            ir_codes: Vec::new(),
            commands: VecDeque::new(),
            device_credential_hashes: BTreeMap::new(),
            discovery_tombstones: Vec::new(),
        }
    }
}

impl AccessoryRegistry {
    pub fn public_snapshot(&self) -> Value {
        json!({
            "accessory_version": self.version,
            "accessories": self.accessories,
            "devices": self.devices.values().collect::<Vec<_>>(),
        })
    }

    pub fn onboard(
        &mut self,
        device_id: String,
        name: String,
        capabilities: Value,
        accessories: Vec<AccessoryInput>,
        device_token: Option<&str>,
    ) -> Result<(), String> {
        validate_identifier(&device_id, "device_id")?;
        if !capabilities.is_object() {
            return Err("capabilities deve ser um objeto JSON".to_string());
        }
        let now = now_rfc3339();
        self.devices.insert(
            device_id.clone(),
            DeviceRuntime {
                device_id: device_id.clone(),
                name,
                capabilities,
                available: false,
                boot_id: None,
                last_seen_at: None,
            },
        );
        let removed_relays: Vec<(String, String)> = self
            .accessories
            .iter()
            .filter(|item| item.device_id == device_id && item.kind == AccessoryKind::Relay)
            .map(|item| (item.device_id.clone(), item.id.clone()))
            .collect();
        self.accessories.retain(|item| item.device_id != device_id);
        for (removed_device, removed_id) in removed_relays {
            self.queue_discovery_tombstone(DiscoveryKind::Relay, removed_device, removed_id);
        }
        for input in accessories {
            validate_identifier(&input.id, "accessory.id")?;
            if !input.capabilities.is_object() {
                return Err(format!("capabilities de {} deve ser objeto", input.id));
            }
            self.accessories.push(Accessory {
                id: input.id,
                device_id: device_id.clone(),
                kind: input.kind,
                name: input.name,
                available: false,
                state: empty_object(),
                capabilities: input.capabilities,
                updated_at: now.clone(),
            });
        }
        if let Some(token) = device_token.filter(|token| !token.is_empty()) {
            self.device_credential_hashes
                .insert(device_id, sha256_hex(token.as_bytes()));
        }
        self.bump();
        Ok(())
    }

    pub fn device_token_matches(&self, device_id: &str, candidate: &str) -> bool {
        self.device_credential_hashes
            .get(device_id)
            .map(|expected| constant_time_eq(expected, &sha256_hex(candidate.as_bytes())))
            .unwrap_or(false)
    }

    /// Estado transitório nunca sobrevive a um restart do servidor. O ESP deve
    /// republicar disponibilidade e um `boot_id` antes de receber comandos.
    pub fn reset_runtime_presence(&mut self) {
        for device in self.devices.values_mut() {
            device.available = false;
            device.boot_id = None;
        }
        for accessory in &mut self.accessories {
            accessory.available = false;
        }
    }

    /// Retorna o boot atual somente quando há evidência de que o dispositivo
    /// está online. O código de erro é estável para REST, MQTT e testes.
    pub fn ready_boot_id(&self, device_id: &str) -> Result<&str, &'static str> {
        let device = self.devices.get(device_id).ok_or("device_not_found")?;
        if !device.available {
            return Err("device_offline");
        }
        device
            .boot_id
            .as_deref()
            .filter(|boot_id| !boot_id.trim().is_empty())
            .ok_or("device_boot_unknown")
    }

    pub fn create_command(
        &mut self,
        mut input: CommandInput,
    ) -> Result<(AccessoryCommand, bool), String> {
        validate_identifier(&input.device_id, "device_id")?;
        validate_action(&input.action)?;
        if !input.params.is_object() {
            return Err("params deve ser um objeto JSON".to_string());
        }
        self.normalize_command(&mut input)?;
        validate_command_params(&input.action, &input.params)?;
        self.expire_commands();
        if input.request_id.is_none() {
            let now = Utc::now();
            if let Some(existing) = self.commands.iter().rev().find(|command| {
                command.device_id == input.device_id
                    && command.action == input.action
                    && command.params == input.params
                    && command.status != CommandStatus::Failed
                    && DateTime::parse_from_rfc3339(&command.created_at)
                        .map(|created| {
                            (now - created.with_timezone(&Utc)).num_milliseconds()
                                <= IMPLICIT_COMMAND_DEDUP_MS
                        })
                        .unwrap_or(false)
            }) {
                return Ok((existing.clone(), false));
            }
        }
        let request_id = input.request_id.unwrap_or_else(|| new_id("cmd"));
        validate_identifier(&request_id, "request_id")?;
        if let Some(existing) = self
            .commands
            .iter()
            .find(|command| command.request_id == request_id)
        {
            if existing.device_id == input.device_id
                && existing.action == input.action
                && existing.params == input.params
            {
                return Ok((existing.clone(), false));
            }
            return Err("request_id já existe com outro comando".to_string());
        }

        let ttl_ms = input
            .ttl_ms
            .unwrap_or(DEFAULT_COMMAND_TTL_MS)
            .clamp(1_000, MAX_COMMAND_TTL_MS);
        let created = Utc::now();
        let command = AccessoryCommand {
            request_id,
            device_id: input.device_id,
            action: input.action,
            params: input.params,
            status: CommandStatus::Pending,
            created_at: created.to_rfc3339(),
            expires_at: (created + Duration::milliseconds(ttl_ms as i64)).to_rfc3339(),
            acknowledged_at: None,
            error: None,
        };
        self.commands.push_back(command.clone());
        while self.commands.len() > MAX_COMMANDS {
            self.commands.pop_front();
        }
        self.bump();
        Ok((command, true))
    }

    fn normalize_command(&self, input: &mut CommandInput) -> Result<(), String> {
        let Some(params) = input.params.as_object_mut() else {
            return Err("params deve ser um objeto JSON".to_string());
        };
        match input.action.as_str() {
            "ir.send" => {
                let code_id = params
                    .get("code_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "params.code_id é obrigatório".to_string())?;
                let code = self
                    .ir_codes
                    .iter()
                    .find(|code| code.id == code_id && code.device_id == input.device_id)
                    .ok_or_else(|| "código IR não encontrado para este dispositivo".to_string())?;
                let repeats = params
                    .get("repeats")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .clamp(1, 20);
                params.insert("carrier_hz".to_string(), json!(code.carrier_hz));
                params.insert(
                    "duty_percent".to_string(),
                    json!((code.duty_cycle * 100.0).round().clamp(1.0, 99.0) as u8),
                );
                params.insert("sequence_us".to_string(), json!(code.sequence_us));
                params.insert("repeats".to_string(), json!(repeats));
            }
            "storage.record.start" => {
                let streams = params.entry("streams").or_insert_with(|| json!(["events"]));
                let Some(streams) = streams.as_array() else {
                    return Err("params.streams deve ser uma lista".to_string());
                };
                let raw_csi = streams.iter().any(|stream| stream.as_str() == Some("csi"));
                if raw_csi
                    && !matches!(
                        params.get("capture_context").and_then(Value::as_str),
                        Some("empty_room" | "bench")
                    )
                {
                    return Err(
                        "CSI bruto exige params.capture_context=empty_room ou bench".to_string()
                    );
                }
                params.insert("raw_csi".to_string(), json!(raw_csi));
                params.entry("session_id").or_insert_with(|| {
                    json!(format!(
                        "{}-{}",
                        Utc::now().format("%Y-%m-%dT%H-%M-%S%.3fZ"),
                        input.device_id.replace(':', "-")
                    ))
                });
            }
            _ => {}
        }
        Ok(())
    }

    pub fn command(&mut self, request_id: &str) -> Option<AccessoryCommand> {
        self.expire_commands();
        self.commands
            .iter()
            .find(|command| command.request_id == request_id)
            .cloned()
    }

    pub fn mark_command_failed(&mut self, request_id: &str, error: impl Into<String>) {
        if let Some(command) = self
            .commands
            .iter_mut()
            .find(|command| command.request_id == request_id)
        {
            command.status = CommandStatus::Failed;
            command.acknowledged_at = Some(now_rfc3339());
            command.error = Some(error.into());
            self.bump();
        }
    }

    pub fn apply_mqtt_message(&mut self, topic: &str, payload: &[u8], base_topic: &str) -> bool {
        if payload.len() > MAX_MQTT_ACCESSORY_PAYLOAD_BYTES {
            return false;
        }
        let Some((device_id, suffix)) = parse_device_topic(topic, base_topic) else {
            return false;
        };
        if validate_identifier(device_id, "device_id").is_err() {
            return false;
        }
        let now = now_rfc3339();
        match suffix {
            "availability" => {
                let online = payload == b"online";
                let device = self.ensure_runtime(device_id);
                device.available = online;
                if !online {
                    // Impede que um retained `online` ou uma reconexão use o
                    // boot_id da execução anterior antes de capabilities/state.
                    device.boot_id = None;
                }
                device.last_seen_at = Some(now.clone());
                for accessory in self
                    .accessories
                    .iter_mut()
                    .filter(|item| item.device_id == device_id)
                {
                    accessory.available = online;
                    accessory.updated_at = now.clone();
                }
                self.bump();
                true
            }
            "capabilities" => {
                let Ok(value) = serde_json::from_slice::<Value>(payload) else {
                    return false;
                };
                self.apply_capabilities(device_id, value, &now);
                self.bump();
                true
            }
            "state" => {
                let Ok(value) = serde_json::from_slice::<Value>(payload) else {
                    return false;
                };
                self.apply_device_state(device_id, value, &now);
                self.bump();
                true
            }
            "ack" => {
                let Ok(value) = serde_json::from_slice::<Value>(payload) else {
                    return false;
                };
                let Some(request_id) = value.get("request_id").and_then(Value::as_str) else {
                    return false;
                };
                let Some(ack_boot_id) = value
                    .get("boot_id")
                    .and_then(Value::as_str)
                    .filter(|boot_id| !boot_id.trim().is_empty())
                else {
                    return false;
                };
                if self
                    .devices
                    .get(device_id)
                    .and_then(|device| device.boot_id.as_deref())
                    != Some(ack_boot_id)
                {
                    return false;
                }
                if let Some(command) = self.commands.iter_mut().find(|command| {
                    command.request_id == request_id && command.device_id == device_id
                }) {
                    command.status = match value.get("status").and_then(Value::as_str) {
                        Some("acknowledged") => CommandStatus::Acknowledged,
                        Some("ok" | "completed") => CommandStatus::Completed,
                        _ => CommandStatus::Failed,
                    };
                    command.acknowledged_at = Some(now.clone());
                    command.error = value
                        .get("error")
                        .and_then(Value::as_str)
                        .map(|error| error.chars().take(512).collect());
                    if let Some(state) = value.get("state").cloned() {
                        self.apply_device_state(device_id, state, &now);
                    }
                    self.bump();
                    return true;
                }
                false
            }
            "event" => {
                let Ok(value) = serde_json::from_slice::<Value>(payload) else {
                    return false;
                };
                self.apply_event(device_id, value, &now);
                self.bump();
                true
            }
            _ => false,
        }
    }

    pub fn create_ir_code(&mut self, input: IrCodeInput) -> Result<IrCode, String> {
        validate_ir_input(&input)?;
        let id = input.id.unwrap_or_else(|| new_id("ir"));
        validate_identifier(&id, "id")?;
        if self.ir_codes.iter().any(|code| code.id == id) {
            return Err("id de código IR já existe".to_string());
        }
        let now = now_rfc3339();
        let code = IrCode {
            id,
            device_id: input.device_id,
            appliance_id: input.appliance_id,
            appliance_name: input.appliance_name,
            name: input.name,
            carrier_hz: input.carrier_hz,
            duty_cycle: input.duty_cycle,
            sequence_us: input.sequence_us,
            created_at: now.clone(),
            updated_at: now,
        };
        self.ir_codes.push(code.clone());
        self.ir_codes
            .sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        self.bump();
        Ok(code)
    }

    pub fn update_ir_code(&mut self, id: &str, input: IrCodeInput) -> Result<IrCode, String> {
        validate_ir_input(&input)?;
        let Some(existing) = self.ir_codes.iter_mut().find(|code| code.id == id) else {
            return Err("not_found".to_string());
        };
        let previous_device_id = existing.device_id.clone();
        existing.device_id = input.device_id;
        existing.appliance_id = input.appliance_id;
        existing.appliance_name = input.appliance_name;
        existing.name = input.name;
        existing.carrier_hz = input.carrier_hz;
        existing.duty_cycle = input.duty_cycle;
        existing.sequence_us = input.sequence_us;
        existing.updated_at = now_rfc3339();
        let result = existing.clone();
        if previous_device_id != result.device_id {
            self.queue_discovery_tombstone(
                DiscoveryKind::IrCode,
                previous_device_id,
                id.to_string(),
            );
        }
        self.bump();
        Ok(result)
    }

    pub fn delete_ir_code(&mut self, id: &str) -> bool {
        let removed = self
            .ir_codes
            .iter()
            .find(|code| code.id == id)
            .map(|code| (code.device_id.clone(), code.id.clone()));
        self.ir_codes.retain(|code| code.id != id);
        if let Some((device_id, entity_id)) = removed {
            self.queue_discovery_tombstone(DiscoveryKind::IrCode, device_id, entity_id);
            self.bump();
            true
        } else {
            false
        }
    }

    fn ensure_runtime(&mut self, device_id: &str) -> &mut DeviceRuntime {
        self.devices
            .entry(device_id.to_string())
            .or_insert_with(|| DeviceRuntime {
                device_id: device_id.to_string(),
                name: device_id.to_string(),
                capabilities: empty_object(),
                available: false,
                boot_id: None,
                last_seen_at: None,
            })
    }

    fn apply_capabilities(&mut self, device_id: &str, value: Value, now: &str) {
        let device = self.ensure_runtime(device_id);
        device.available = true;
        device.last_seen_at = Some(now.to_string());
        if let Some(name) = value.get("name").and_then(Value::as_str) {
            device.name = name.to_string();
        }
        device.boot_id = value
            .get("boot_id")
            .and_then(Value::as_str)
            .filter(|boot_id| !boot_id.trim().is_empty())
            .map(str::to_string);
        device.capabilities = value
            .get("capabilities")
            .cloned()
            .unwrap_or_else(|| value.clone());

        if let Some(items) = value.get("accessories").and_then(Value::as_array) {
            let parsed: Vec<AccessoryInput> = items
                .iter()
                .filter_map(|raw| serde_json::from_value(raw.clone()).ok())
                .filter(|input: &AccessoryInput| {
                    validate_identifier(&input.id, "accessory.id").is_ok()
                })
                .collect();
            let reported_ids: Vec<&str> = parsed.iter().map(|input| input.id.as_str()).collect();
            let removed_relays: Vec<(String, String)> = self
                .accessories
                .iter()
                .filter(|item| {
                    item.device_id == device_id
                        && item.kind == AccessoryKind::Relay
                        && !reported_ids.contains(&item.id.as_str())
                })
                .map(|item| (item.device_id.clone(), item.id.clone()))
                .collect();
            self.accessories.retain(|item| {
                item.device_id != device_id || reported_ids.contains(&item.id.as_str())
            });
            for (removed_device, removed_id) in removed_relays {
                self.queue_discovery_tombstone(DiscoveryKind::Relay, removed_device, removed_id);
            }
            for input in parsed {
                if let Some(existing) = self
                    .accessories
                    .iter_mut()
                    .find(|item| item.device_id == device_id && item.id == input.id)
                {
                    existing.name = input.name;
                    existing.kind = input.kind;
                    existing.capabilities = input.capabilities;
                    existing.available = true;
                    existing.updated_at = now.to_string();
                } else {
                    self.accessories.push(Accessory {
                        id: input.id,
                        device_id: device_id.to_string(),
                        kind: input.kind,
                        name: input.name,
                        available: true,
                        state: empty_object(),
                        capabilities: input.capabilities,
                        updated_at: now.to_string(),
                    });
                }
            }
        }
    }

    fn apply_device_state(&mut self, device_id: &str, value: Value, now: &str) {
        let device = self.ensure_runtime(device_id);
        device.available = true;
        device.last_seen_at = Some(now.to_string());
        device.boot_id = value
            .get("boot_id")
            .and_then(Value::as_str)
            .filter(|boot_id| !boot_id.trim().is_empty())
            .map(str::to_string);
        if let Some(states) = value.get("accessories").and_then(Value::as_object) {
            for (id, state) in states {
                if let Some(accessory) = self
                    .accessories
                    .iter_mut()
                    .find(|item| item.device_id == device_id && item.id == *id)
                {
                    accessory.state = state.clone();
                    accessory.available = true;
                    accessory.updated_at = now.to_string();
                }
            }
        }
    }

    fn apply_event(&mut self, device_id: &str, value: Value, now: &str) {
        self.ensure_runtime(device_id).last_seen_at = Some(now.to_string());
        if value.get("type").and_then(Value::as_str) == Some("ir.learned") {
            if let Some(code) = value.get("code") {
                let mut code = code.clone();
                if let Some(object) = code.as_object_mut() {
                    object
                        .entry("device_id")
                        .or_insert_with(|| json!(device_id));
                }
                if let Ok(input) = serde_json::from_value::<IrCodeInput>(code) {
                    if let Some(id) = input
                        .id
                        .clone()
                        .filter(|id| self.ir_codes.iter().any(|existing| existing.id == *id))
                    {
                        let _ = self.update_ir_code(&id, input);
                    } else {
                        let _ = self.create_ir_code(input);
                    }
                }
            }
        }
    }

    fn expire_commands(&mut self) {
        let now = Utc::now();
        let mut changed = false;
        for command in &mut self.commands {
            if !matches!(
                command.status,
                CommandStatus::Pending | CommandStatus::Acknowledged
            ) {
                continue;
            }
            if DateTime::parse_from_rfc3339(&command.expires_at)
                .map(|expires| expires.with_timezone(&Utc) <= now)
                .unwrap_or(true)
            {
                command.status = CommandStatus::Expired;
                command.error = Some("ttl_expired".to_string());
                changed = true;
            }
        }
        if changed {
            self.bump();
        }
    }

    fn bump(&mut self) {
        self.version = self.version.wrapping_add(1).max(1);
    }

    fn queue_discovery_tombstone(
        &mut self,
        kind: DiscoveryKind,
        device_id: String,
        entity_id: String,
    ) {
        let tombstone = DiscoveryTombstone {
            kind,
            device_id,
            entity_id,
        };
        if !self.discovery_tombstones.contains(&tombstone) {
            self.discovery_tombstones.push(tombstone);
            if self.discovery_tombstones.len() > MAX_DISCOVERY_TOMBSTONES {
                self.discovery_tombstones.remove(0);
            }
        }
    }
}

pub fn validate_identifier(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 120
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':' | '.'))
    {
        Err(format!("{field} inválido"))
    } else {
        Ok(())
    }
}

pub fn validate_action(action: &str) -> Result<(), String> {
    const ACTIONS: &[&str] = &[
        "relay.set",
        "relay.pulse",
        "ir.learn.start",
        "ir.learn.cancel",
        "ir.send",
        "storage.record.start",
        "storage.record.stop",
        "storage.sync",
    ];
    if ACTIONS.contains(&action) {
        Ok(())
    } else {
        Err(format!("ação não suportada: {action}"))
    }
}

pub fn validate_command_params(action: &str, params: &Value) -> Result<(), String> {
    let identifier = |field: &str| -> Result<&str, String> {
        let value = params
            .get(field)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("params.{field} é obrigatório"))?;
        validate_identifier(value, &format!("params.{field}"))?;
        Ok(value)
    };
    match action {
        "relay.set" => {
            identifier("relay_id")?;
            if params.get("state").and_then(Value::as_bool).is_none() {
                return Err("params.state booleano é obrigatório".to_string());
            }
        }
        "relay.pulse" => {
            identifier("relay_id")?;
            let duration = params
                .get("duration_ms")
                .and_then(Value::as_u64)
                .ok_or_else(|| "params.duration_ms é obrigatório".to_string())?;
            if !(1..=300_000).contains(&duration) {
                return Err("params.duration_ms deve ficar entre 1 e 300000".to_string());
            }
        }
        "ir.send" => {
            identifier("code_id")?;
            let duty_percent = params
                .get("duty_percent")
                .and_then(Value::as_u64)
                .ok_or_else(|| "params.duty_percent é obrigatório".to_string())?;
            if !(1..=99).contains(&duty_percent) {
                return Err("params.duty_percent deve ficar entre 1 e 99".to_string());
            }
            if params
                .get("sequence_us")
                .and_then(Value::as_array)
                .is_none_or(|sequence| sequence.len() < 2 || sequence.len() > 4096)
            {
                return Err("params.sequence_us deve conter de 2 a 4096 durações".to_string());
            }
            if let Some(repeats) = params.get("repeats").and_then(Value::as_u64) {
                if !(1..=20).contains(&repeats) {
                    return Err("params.repeats deve ficar entre 1 e 20".to_string());
                }
            }
        }
        action if action.starts_with("storage.") => {
            identifier("storage_id")?;
            if action == "storage.record.start" {
                identifier("session_id")?;
                if params.get("raw_csi").and_then(Value::as_bool).is_none() {
                    return Err("params.raw_csi booleano é obrigatório".to_string());
                }
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn validate_ir_input(input: &IrCodeInput) -> Result<(), String> {
    validate_identifier(&input.device_id, "device_id")?;
    if !input.appliance_id.is_empty() {
        validate_identifier(&input.appliance_id, "appliance_id")?;
    }
    if input.appliance_name.len() > 120 {
        return Err("appliance_name inválido".to_string());
    }
    if input.name.trim().is_empty() || input.name.len() > 120 {
        return Err("name inválido".to_string());
    }
    if !(10_000..=100_000).contains(&input.carrier_hz) {
        return Err("carrier_hz deve ficar entre 10000 e 100000".to_string());
    }
    if !input.duty_cycle.is_finite() || !(0.05..=0.95).contains(&input.duty_cycle) {
        return Err("duty_cycle deve ficar entre 0.05 e 0.95".to_string());
    }
    if input.sequence_us.len() < 2 || input.sequence_us.len() > 4096 {
        return Err("sequence_us deve conter de 2 a 4096 durações".to_string());
    }
    if input
        .sequence_us
        .iter()
        .any(|duration| *duration == 0 || *duration > 1_000_000)
    {
        return Err("cada duração IR deve ficar entre 1 e 1000000 us".to_string());
    }
    Ok(())
}

pub fn new_id(prefix: &str) -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{unix_ms:x}-{sequence:x}")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut different = left.len() ^ right.len();
    let max = left.len().max(right.len());
    for index in 0..max {
        different |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right.get(index).copied().unwrap_or_default(),
        );
    }
    different == 0
}

fn parse_device_topic<'a>(topic: &'a str, base_topic: &str) -> Option<(&'a str, &'a str)> {
    let prefix = format!("{}/devices/", base_topic.trim_end_matches('/'));
    let remainder = topic.strip_prefix(&prefix)?;
    let (device_id, suffix) = remainder.split_once('/')?;
    (!device_id.is_empty()).then_some((device_id, suffix))
}

fn default_schema() -> String {
    ACCESSORY_SCHEMA.to_string()
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

fn default_carrier_hz() -> u32 {
    38_000
}

fn default_duty_cycle() -> f32 {
    0.33
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay() -> AccessoryInput {
        AccessoryInput {
            id: "relay-1".to_string(),
            kind: AccessoryKind::Relay,
            name: "Lâmpada".to_string(),
            capabilities: json!({"pulse_max_ms": 5000}),
        }
    }

    #[test]
    fn onboarding_e_mqtt_atualizam_snapshot_sem_expor_credencial() {
        let mut registry = AccessoryRegistry::default();
        registry
            .onboard(
                "esp-c5-01".into(),
                "Sala".into(),
                json!({"ir": true}),
                vec![relay()],
                Some("secret"),
            )
            .unwrap();
        assert!(registry.device_token_matches("esp-c5-01", "secret"));
        assert!(!registry.device_token_matches("esp-c5-01", "wrong"));

        assert!(registry.apply_mqtt_message(
            "aethersense/devices/esp-c5-01/state",
            br#"{"boot_id":"boot-1","accessories":{"relay-1":{"on":true}}}"#,
            "aethersense",
        ));
        let snapshot = registry.public_snapshot();
        assert_eq!(snapshot["accessories"][0]["state"]["on"], true);
        assert!(snapshot.get("device_credential_hashes").is_none());
    }

    #[test]
    fn request_id_e_idempotente_mas_rejeita_reuso_divergente() {
        let mut registry = AccessoryRegistry::default();
        let input = CommandInput {
            request_id: Some("req-1".into()),
            device_id: "esp-1".into(),
            action: "relay.set".into(),
            params: json!({"relay_id":"relay-1","state":true}),
            ttl_ms: None,
        };
        let (first, created) = registry.create_command(input.clone()).unwrap();
        assert!(created);
        let (second, created) = registry.create_command(input).unwrap();
        assert!(!created);
        assert_eq!(first, second);

        assert!(registry
            .create_command(CommandInput {
                request_id: Some("req-1".into()),
                device_id: "esp-1".into(),
                action: "relay.set".into(),
                params: json!({"relay_id":"relay-1","state":false}),
                ttl_ms: None,
            })
            .is_err());

        let mut implicit = AccessoryRegistry::default();
        let make_implicit = || CommandInput {
            request_id: None,
            device_id: "esp-1".into(),
            action: "relay.pulse".into(),
            params: json!({"relay_id":"relay-1","duration_ms":250}),
            ttl_ms: None,
        };
        let (first, created) = implicit.create_command(make_implicit()).unwrap();
        assert!(created);
        let (duplicate, created) = implicit.create_command(make_implicit()).unwrap();
        assert!(!created, "reentrega QoS 1 deve ser deduplicada");
        assert_eq!(duplicate.request_id, first.request_id);
        implicit.commands.back_mut().unwrap().expires_at = "2000-01-01T00:00:00Z".into();
        let expired = implicit.command(&first.request_id).unwrap();
        assert_eq!(expired.status, CommandStatus::Expired);
        assert_eq!(expired.error.as_deref(), Some("ttl_expired"));
    }

    #[test]
    fn ack_mqtt_conclui_comando_e_atualiza_estado() {
        let mut registry = AccessoryRegistry::default();
        registry
            .onboard(
                "esp-1".into(),
                "Sala".into(),
                json!({}),
                vec![relay()],
                None,
            )
            .unwrap();
        registry
            .create_command(CommandInput {
                request_id: Some("req-2".into()),
                device_id: "esp-1".into(),
                action: "relay.set".into(),
                params: json!({"relay_id":"relay-1","state":true}),
                ttl_ms: None,
            })
            .unwrap();
        assert!(registry.apply_mqtt_message(
            "aethersense/devices/esp-1/state",
            br#"{"boot_id":"boot-1","accessories":{}}"#,
            "aethersense"
        ));
        assert!(!registry.apply_mqtt_message(
            "aethersense/devices/esp-1/ack",
            br#"{"request_id":"req-2","boot_id":"stale-boot","status":"ok"}"#,
            "aethersense"
        ));
        assert_eq!(
            registry.command("req-2").unwrap().status,
            CommandStatus::Pending
        );
        assert!(registry.apply_mqtt_message(
            "aethersense/devices/esp-1/ack",
            br#"{"request_id":"req-2","boot_id":"boot-1","status":"acknowledged"}"#,
            "aethersense"
        ));
        assert_eq!(
            registry.command("req-2").unwrap().status,
            CommandStatus::Acknowledged
        );
        assert!(registry.apply_mqtt_message(
            "aethersense/devices/esp-1/ack",
            br#"{"request_id":"req-2","boot_id":"boot-1","status":"completed","state":{"accessories":{"relay-1":{"on":true}}}}"#,
            "aethersense"
        ));
        assert_eq!(
            registry.command("req-2").unwrap().status,
            CommandStatus::Completed
        );
        assert_eq!(registry.accessories[0].state["on"], true);
    }

    #[test]
    fn codigo_ir_valida_limites() {
        let mut registry = AccessoryRegistry::default();
        let code = registry
            .create_ir_code(IrCodeInput {
                id: Some("power".into()),
                device_id: "esp-1".into(),
                appliance_id: "tv-sala".into(),
                appliance_name: "TV da sala".into(),
                name: "Power".into(),
                carrier_hz: 38_000,
                duty_cycle: 0.33,
                sequence_us: vec![9_000, 4_500, 560, 560],
            })
            .unwrap();
        assert_eq!(code.id, "power");
        let (command, _) = registry
            .create_command(CommandInput {
                request_id: Some("ir-send-1".into()),
                device_id: "esp-1".into(),
                action: "ir.send".into(),
                params: json!({"code_id":"power","repeats":2}),
                ttl_ms: None,
            })
            .unwrap();
        assert_eq!(command.params["duty_percent"], 33);
        assert!(command.params.get("duty_cycle").is_none());
        assert_eq!(
            command.params["sequence_us"],
            json!([9_000, 4_500, 560, 560])
        );
        assert!(registry.delete_ir_code("power"));
        assert_eq!(registry.discovery_tombstones.len(), 1);
        assert_eq!(registry.discovery_tombstones[0].kind, DiscoveryKind::IrCode);
    }

    #[test]
    fn dominio_de_acessorios_persiste_no_config_sem_segredo_em_claro() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("aethersense.json");
        let mut state = crate::state::InnerState::new_with_config_path(&config);
        state
            .accessories
            .onboard(
                "esp-1".into(),
                "Sala".into(),
                json!({"relays": 1}),
                vec![relay()],
                Some("device-secret"),
            )
            .unwrap();
        state
            .accessories
            .create_ir_code(IrCodeInput {
                id: Some("power".into()),
                device_id: "esp-1".into(),
                appliance_id: String::new(),
                appliance_name: String::new(),
                name: "Power".into(),
                carrier_hz: 38_000,
                duty_cycle: 0.33,
                sequence_us: vec![9_000, 4_500],
            })
            .unwrap();
        {
            let runtime = state.accessories.devices.get_mut("esp-1").unwrap();
            runtime.available = true;
            runtime.boot_id = Some("boot-antigo".into());
        }
        state.save_config().unwrap();

        let raw = std::fs::read_to_string(&config).unwrap();
        assert!(!raw.contains("device-secret"));
        let reloaded = crate::state::InnerState::new_with_config_path(&config);
        assert_eq!(reloaded.accessories.accessories.len(), 1);
        assert_eq!(reloaded.accessories.ir_codes.len(), 1);
        assert_eq!(
            reloaded.accessories.ready_boot_id("esp-1"),
            Err("device_offline")
        );
        assert!(reloaded.accessories.devices["esp-1"].boot_id.is_none());
        assert!(reloaded
            .accessories
            .device_token_matches("esp-1", "device-secret"));
    }

    #[test]
    fn gravacao_sd_gera_sessao_e_bloqueia_csi_humano_local() {
        let mut registry = AccessoryRegistry::default();
        let (events, _) = registry
            .create_command(CommandInput {
                request_id: Some("sd-events".into()),
                device_id: "esp-1".into(),
                action: "storage.record.start".into(),
                params: json!({"storage_id":"sd-1","streams":["events"]}),
                ttl_ms: None,
            })
            .unwrap();
        assert_eq!(events.params["raw_csi"], false);
        assert!(events.params["session_id"]
            .as_str()
            .is_some_and(|id| id.contains("esp-1")));

        let rejected = registry.create_command(CommandInput {
            request_id: Some("sd-human".into()),
            device_id: "esp-1".into(),
            action: "storage.record.start".into(),
            params: json!({
                "storage_id":"sd-1",
                "streams":["events","csi"],
                "capture_context":"human_walk"
            }),
            ttl_ms: None,
        });
        assert!(rejected.is_err());
    }

    #[test]
    fn capabilities_sao_fonte_de_verdade_e_removem_rele_antigo_com_tombstone() {
        let mut registry = AccessoryRegistry::default();
        registry
            .onboard(
                "esp-1".into(),
                "Sala".into(),
                json!({}),
                vec![relay()],
                None,
            )
            .unwrap();
        assert_eq!(registry.accessories.len(), 1);
        assert!(registry.apply_mqtt_message(
            "aethersense/devices/esp-1/capabilities",
            br#"{"boot_id":"boot-2","capabilities":{},"accessories":[]}"#,
            "aethersense"
        ));
        assert!(registry.accessories.is_empty());
        assert!(registry.discovery_tombstones.iter().any(|tombstone| {
            tombstone.kind == DiscoveryKind::Relay && tombstone.entity_id == "relay-1"
        }));
    }

    #[test]
    fn comando_so_tem_boot_id_quando_dispositivo_esta_online_na_execucao_atual() {
        let mut registry = AccessoryRegistry::default();
        registry
            .onboard(
                "esp-1".into(),
                "Sala".into(),
                json!({}),
                vec![relay()],
                None,
            )
            .unwrap();
        assert_eq!(registry.ready_boot_id("esp-1"), Err("device_offline"));

        assert!(registry.apply_mqtt_message(
            "aethersense/devices/esp-1/capabilities",
            br#"{"boot_id":"boot-atual","capabilities":{},"accessories":[]}"#,
            "aethersense"
        ));
        assert_eq!(registry.ready_boot_id("esp-1"), Ok("boot-atual"));

        assert!(registry.apply_mqtt_message(
            "aethersense/devices/esp-1/availability",
            b"offline",
            "aethersense"
        ));
        assert_eq!(registry.ready_boot_id("esp-1"), Err("device_offline"));
        assert!(registry.devices["esp-1"].boot_id.is_none());

        assert!(registry.apply_mqtt_message(
            "aethersense/devices/esp-1/availability",
            b"online",
            "aethersense"
        ));
        assert_eq!(
            registry.ready_boot_id("esp-1"),
            Err("device_boot_unknown")
        );
    }
}
