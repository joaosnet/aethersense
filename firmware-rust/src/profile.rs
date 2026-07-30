//! Versioned, secret-free device profile shared by the TUI and firmware.
//!
//! The profile intentionally contains no passwords or bearer tokens. Those are
//! carried by [`crate::provisioning::ProvisionedSecrets`] and persisted in the
//! encrypted/isolated device store selected by the platform integration.

use std::collections::{BTreeMap, BTreeSet};

use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DEVICE_PROFILE_SCHEMA: &str = "aethersense.device_profile.v1";
pub const MAX_RELAYS: usize = 8;
pub const DEFAULT_SD_BLOCK_BYTES: u32 = 1_048_576;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceProfile {
    pub schema: String,
    pub identity: IdentityProfile,
    pub target: TargetProfile,
    pub network: NetworkProfile,
    pub csi: CsiProfile,
    pub runtime: RuntimeProfile,
    pub mqtt: MqttProfile,
    pub peripherals: PeripheralProfile,
    pub storage: StorageProfile,
    pub sync: SyncProfile,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secret_revisions: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityProfile {
    pub profile_uuid: String,
    pub device_id: Option<String>,
    pub name: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HardwareChip {
    Esp32c5,
    #[default]
    #[serde(alias = "classic", alias = "esp32", alias = "esp32-wroom-32")]
    Wroom,
    Esp32c6,
}

impl HardwareChip {
    pub const fn hardware_profile(self) -> &'static str {
        match self {
            Self::Esp32c5 => "esp32c5",
            Self::Wroom => "classic",
            Self::Esp32c6 => "esp32c6",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetProfile {
    pub chip: HardwareChip,
    #[serde(default = "default_board_variant")]
    pub board_variant: String,
    #[serde(default)]
    pub feature_set: Vec<String>,
    #[serde(default = "default_partition_table")]
    pub partition_table: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkProfile {
    #[serde(default)]
    pub wifi_ssid: String,
    #[serde(default = "default_udp_host")]
    pub udp_host: String,
    #[serde(default = "default_udp_port")]
    pub udp_port: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CsiProfile {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_send_frequency_hz")]
    pub send_frequency_hz: u32,
    #[serde(default = "default_queue_capacity")]
    pub queue_capacity: usize,
    #[serde(default = "default_wifi_band")]
    pub wifi_band: String,
    #[serde(default = "default_true")]
    pub prefer_ht40: bool,
    #[serde(default = "default_true")]
    pub empty_room_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeProfile {
    #[serde(default)]
    pub status_led_enabled: bool,
    pub status_led_gpio: Option<i32>,
    #[serde(default)]
    pub status_led: StatusLedProfile,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusLedKind {
    #[default]
    Ws2812,
    RgbDiscrete,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusLedGpios {
    pub data: Option<i32>,
    pub red: Option<i32>,
    pub green: Option<i32>,
    pub blue: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusLedProfile {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub kind: StatusLedKind,
    #[serde(default)]
    pub gpios: StatusLedGpios,
    #[serde(default = "default_status_led_brightness")]
    pub brightness_percent: u8,
    #[serde(default = "default_status_led_colors")]
    pub colors: BTreeMap<String, Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MqttProfile {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub broker_host: String,
    #[serde(default = "default_mqtt_port")]
    pub port: u16,
    pub username: Option<String>,
    #[serde(default)]
    pub tls: bool,
    #[serde(default = "default_qos")]
    pub qos: u8,
    #[serde(default = "default_base_topic")]
    pub base_topic: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeripheralProfile {
    #[serde(default)]
    pub ir_rx: IrRxProfile,
    #[serde(default)]
    pub ir_tx: IrTxProfile,
    #[serde(default)]
    pub relays: Vec<RelayProfile>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrRxProfile {
    #[serde(default)]
    pub enabled: bool,
    pub gpio: Option<i32>,
    #[serde(default = "default_ir_idle_threshold_us")]
    pub idle_threshold_us: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrTxProfile {
    #[serde(default)]
    pub enabled: bool,
    pub gpio: Option<i32>,
    #[serde(default = "default_ir_carrier_hz")]
    pub carrier_hz: u32,
    #[serde(default = "default_ir_duty_percent")]
    pub duty_percent: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayProfile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    pub gpio: Option<i32>,
    #[serde(default = "default_true")]
    pub active_high: bool,
    #[serde(default = "default_relay_pulse_limit_ms")]
    pub pulse_limit_ms: u32,
    #[serde(default = "default_true")]
    pub restore_state: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageProfile {
    #[serde(default)]
    pub sd: SdProfile,
    #[serde(default)]
    pub recording: RecordingProfile,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SdProfile {
    #[serde(default)]
    pub enabled: bool,
    pub sclk_gpio: Option<i32>,
    pub mosi_gpio: Option<i32>,
    pub miso_gpio: Option<i32>,
    pub cs_gpio: Option<i32>,
    #[serde(default = "default_sd_mount_point")]
    pub mount_point: String,
    #[serde(default = "default_rotate_bytes")]
    pub rotate_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyMode {
    EventsOnly,
    EmptyRoomBench,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingProfile {
    #[serde(default = "default_true")]
    pub events_enabled: bool,
    #[serde(default)]
    pub raw_csi_enabled: bool,
    #[serde(default)]
    pub privacy_mode: PrivacyMode,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncProfile {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub base_url: String,
    #[serde(default = "default_true")]
    pub network_first: bool,
    #[serde(default = "default_block_bytes")]
    pub block_bytes: u32,
    #[serde(default)]
    pub delete_after_sync: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileError {
    pub field: String,
    pub message: String,
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

impl std::error::Error for ProfileError {}

impl Default for DeviceProfile {
    fn default() -> Self {
        Self {
            schema: DEVICE_PROFILE_SCHEMA.to_string(),
            identity: IdentityProfile::default(),
            target: TargetProfile::default(),
            network: NetworkProfile::default(),
            csi: CsiProfile::default(),
            runtime: RuntimeProfile::default(),
            mqtt: MqttProfile::default(),
            peripherals: PeripheralProfile::default(),
            storage: StorageProfile::default(),
            sync: SyncProfile::default(),
            secret_revisions: BTreeMap::new(),
        }
    }
}

impl Default for TargetProfile {
    fn default() -> Self {
        Self {
            chip: HardwareChip::Wroom,
            board_variant: default_board_variant(),
            feature_set: vec!["csi".to_string()],
            partition_table: default_partition_table(),
        }
    }
}

impl Default for RuntimeProfile {
    fn default() -> Self {
        Self {
            status_led_enabled: false,
            status_led_gpio: None,
            status_led: StatusLedProfile::default(),
        }
    }
}

impl Default for StatusLedProfile {
    fn default() -> Self {
        Self {
            enabled: false,
            kind: StatusLedKind::Ws2812,
            gpios: StatusLedGpios::default(),
            brightness_percent: default_status_led_brightness(),
            colors: default_status_led_colors(),
        }
    }
}

impl Default for NetworkProfile {
    fn default() -> Self {
        Self {
            wifi_ssid: String::new(),
            udp_host: default_udp_host(),
            udp_port: default_udp_port(),
        }
    }
}

impl Default for CsiProfile {
    fn default() -> Self {
        Self {
            enabled: true,
            send_frequency_hz: default_send_frequency_hz(),
            queue_capacity: default_queue_capacity(),
            wifi_band: default_wifi_band(),
            prefer_ht40: true,
            empty_room_only: true,
        }
    }
}

impl Default for MqttProfile {
    fn default() -> Self {
        Self {
            enabled: false,
            broker_host: String::new(),
            port: default_mqtt_port(),
            username: None,
            tls: false,
            qos: default_qos(),
            base_topic: default_base_topic(),
        }
    }
}

impl Default for IrRxProfile {
    fn default() -> Self {
        Self {
            enabled: false,
            gpio: None,
            idle_threshold_us: default_ir_idle_threshold_us(),
        }
    }
}

impl Default for IrTxProfile {
    fn default() -> Self {
        Self {
            enabled: false,
            gpio: None,
            carrier_hz: default_ir_carrier_hz(),
            duty_percent: default_ir_duty_percent(),
        }
    }
}

impl Default for SdProfile {
    fn default() -> Self {
        Self {
            enabled: false,
            sclk_gpio: None,
            mosi_gpio: None,
            miso_gpio: None,
            cs_gpio: None,
            mount_point: default_sd_mount_point(),
            rotate_bytes: default_rotate_bytes(),
        }
    }
}

impl Default for PrivacyMode {
    fn default() -> Self {
        Self::EventsOnly
    }
}

impl Default for RecordingProfile {
    fn default() -> Self {
        Self {
            events_enabled: true,
            raw_csi_enabled: false,
            privacy_mode: PrivacyMode::EventsOnly,
        }
    }
}

impl Default for SyncProfile {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: String::new(),
            network_first: true,
            block_bytes: DEFAULT_SD_BLOCK_BYTES,
            delete_after_sync: false,
        }
    }
}

impl DeviceProfile {
    pub fn from_json(input: &str) -> Result<Self, ProfileError> {
        let profile: Self = serde_json::from_str(input).map_err(|err| ProfileError {
            field: "profile".to_string(),
            message: format!("invalid JSON: {err}"),
        })?;
        profile.validate()?;
        Ok(profile)
    }

    pub fn from_environment() -> Result<Option<Self>, ProfileError> {
        let raw = std::env::var("AETHERSENSE_DEVICE_PROFILE_JSON")
            .ok()
            .or_else(|| option_env!("AETHERSENSE_DEVICE_PROFILE_JSON").map(str::to_string));
        raw.filter(|value| !value.trim().is_empty())
            .map(|value| Self::from_json(&value))
            .transpose()
    }

    pub fn validate(&self) -> Result<(), ProfileError> {
        ensure(
            self.schema == DEVICE_PROFILE_SCHEMA,
            "schema",
            format!("expected {DEVICE_PROFILE_SCHEMA}"),
        )?;
        for (kind, revision) in &self.secret_revisions {
            ensure(
                matches!(
                    kind.as_str(),
                    "wifi_password" | "mqtt_password" | "server_bearer_token"
                ),
                "secret_revisions",
                "contains an unsupported secret kind",
            )?;
            ensure(
                revision.len() == 32
                    && revision
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                &format!("secret_revisions.{kind}"),
                "must be a lowercase 128-bit hexadecimal nonce",
            )?;
        }
        ensure(
            !self.identity.profile_uuid.trim().is_empty(),
            "identity.profile_uuid",
            "must not be empty",
        )?;
        ensure(
            !self.identity.name.trim().is_empty(),
            "identity.name",
            "must not be empty",
        )?;
        ensure(
            self.target.partition_table == "partitions.csv",
            "target.partition_table",
            "profile v1 requires the canonical partitions.csv table",
        )?;
        let board_variant_valid = match self.target.chip {
            HardwareChip::Wroom | HardwareChip::Esp32c6 => self.target.board_variant == "generic",
            HardwareChip::Esp32c5 => matches!(
                self.target.board_variant.as_str(),
                "generic" | "esp32-c5-devkitc-1"
            ),
        };
        ensure(
            board_variant_valid,
            "target.board_variant",
            "unknown board variant for the selected target",
        )?;
        ensure(
            self.network.udp_port != 0,
            "network.udp_port",
            "must be non-zero",
        )?;
        ensure(
            !self.network.udp_host.is_empty()
                && !self.network.udp_host.chars().any(char::is_whitespace),
            "network.udp_host",
            "must be an IP address or hostname without whitespace",
        )?;
        ensure(
            (1..=100).contains(&self.csi.send_frequency_hz),
            "csi.send_frequency_hz",
            "must be within 1..=100 Hz",
        )?;
        ensure(
            (32..=4_096).contains(&self.csi.queue_capacity),
            "csi.queue_capacity",
            "must be within 32..=4096",
        )?;
        ensure(
            matches!(self.csi.wifi_band.as_str(), "auto" | "2.4ghz" | "5ghz"),
            "csi.wifi_band",
            "must be auto, 2.4ghz or 5ghz",
        )?;
        if self.target.chip == HardwareChip::Wroom {
            ensure(
                self.csi.wifi_band != "5ghz",
                "csi.wifi_band",
                "ESP32-WROOM does not support 5 GHz Wi-Fi",
            )?;
        }
        ensure(
            self.sync.base_url.is_empty() || valid_http_url(&self.sync.base_url),
            "sync.base_url",
            "must be an http:// or https:// URL with a host",
        )?;
        ensure(self.mqtt.qos == 1, "mqtt.qos", "v1 requires QoS 1")?;
        if self.mqtt.enabled {
            ensure(
                !self.mqtt.broker_host.trim().is_empty(),
                "mqtt.broker_host",
                "must not be empty when MQTT is enabled",
            )?;
            ensure(self.mqtt.port != 0, "mqtt.port", "must be non-zero")?;
        }
        ensure(
            self.peripherals.relays.len() <= MAX_RELAYS,
            "peripherals.relays",
            format!("at most {MAX_RELAYS} relays are supported"),
        )?;
        if self.target.chip == HardwareChip::Esp32c6 && self.has_enabled_accessories() {
            return Err(ProfileError {
                field: "peripherals".to_string(),
                message: "accessories are disabled for ESP32-C6 in profile v1".to_string(),
            });
        }

        self.validate_ir()?;
        self.validate_relays()?;
        self.validate_storage()?;
        self.validate_pin_plan()?;
        Ok(())
    }

    pub fn canonical_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&CanonicalDeviceProfile::new(self, true))
    }

    pub fn config_hash(&self) -> Result<String, serde_json::Error> {
        // Build-only inputs are intentionally excluded. The selected chip,
        // feature set and partition table are covered by the build hash while
        // this value confirms the runtime/NVS configuration applied by the
        // provisioning protocol. Keep this byte-for-byte compatible with
        // aethersense_build.diffing.compute_config_hash().
        canonical_sha256(&CanonicalDeviceProfile::new(self, false))
    }

    /// Hash of the compile-time target inputs (chip/features/partition table).
    ///
    /// This is intentionally only the target fingerprint.  The source-aware
    /// firmware build hash is generated by build.py and stored in the outer
    /// provisioning envelope, because it cannot be reconstructed on-device.
    pub fn build_hash(&self) -> Result<String, serde_json::Error> {
        canonical_sha256(&CanonicalTargetProfile::from(&self.target))
    }

    pub fn enabled_relay_count(&self) -> usize {
        self.peripherals
            .relays
            .iter()
            .filter(|relay| relay.enabled)
            .count()
    }

    pub fn has_enabled_accessories(&self) -> bool {
        self.peripherals.ir_rx.enabled
            || self.peripherals.ir_tx.enabled
            || self.enabled_relay_count() > 0
            || self.storage.sd.enabled
    }

    fn validate_ir(&self) -> Result<(), ProfileError> {
        if self.peripherals.ir_rx.enabled {
            ensure(
                self.peripherals.ir_rx.gpio.is_some(),
                "peripherals.ir_rx.gpio",
                "is required when IR RX is enabled",
            )?;
            ensure(
                (1_000..=100_000).contains(&self.peripherals.ir_rx.idle_threshold_us),
                "peripherals.ir_rx.idle_threshold_us",
                "must be between 1000 and 100000 us",
            )?;
        }
        if self.peripherals.ir_tx.enabled {
            ensure(
                self.peripherals.ir_tx.gpio.is_some(),
                "peripherals.ir_tx.gpio",
                "is required when IR TX is enabled",
            )?;
            ensure(
                (20_000..=60_000).contains(&self.peripherals.ir_tx.carrier_hz),
                "peripherals.ir_tx.carrier_hz",
                "must be between 20 and 60 kHz",
            )?;
            ensure(
                (10..=50).contains(&self.peripherals.ir_tx.duty_percent),
                "peripherals.ir_tx.duty_percent",
                "must be between 10 and 50 percent",
            )?;
        }
        Ok(())
    }

    fn validate_relays(&self) -> Result<(), ProfileError> {
        let mut ids = BTreeSet::new();
        for (index, relay) in self.peripherals.relays.iter().enumerate() {
            let prefix = format!("peripherals.relays[{index}]");
            ensure(
                !relay.id.trim().is_empty(),
                &format!("{prefix}.id"),
                "must not be empty",
            )?;
            ensure(
                ids.insert(relay.id.as_str()),
                &format!("{prefix}.id"),
                "must be unique",
            )?;
            if relay.enabled {
                ensure(
                    relay.gpio.is_some(),
                    &format!("{prefix}.gpio"),
                    "is required when the relay is enabled",
                )?;
                ensure(
                    (100..=600_000).contains(&relay.pulse_limit_ms),
                    &format!("{prefix}.pulse_limit_ms"),
                    "must be between 100 and 600000 ms",
                )?;
            }
        }
        Ok(())
    }

    fn validate_storage(&self) -> Result<(), ProfileError> {
        if self.storage.recording.raw_csi_enabled {
            ensure(
                self.storage.sd.enabled,
                "storage.recording.raw_csi_enabled",
                "requires an enabled SD card",
            )?;
            ensure(
                self.storage.recording.privacy_mode == PrivacyMode::EmptyRoomBench,
                "storage.recording.privacy_mode",
                "raw local CSI is restricted to empty-room/bench sessions",
            )?;
        }
        if self.storage.sd.enabled {
            for (name, pin) in [
                ("sclk_gpio", self.storage.sd.sclk_gpio),
                ("mosi_gpio", self.storage.sd.mosi_gpio),
                ("miso_gpio", self.storage.sd.miso_gpio),
                ("cs_gpio", self.storage.sd.cs_gpio),
            ] {
                ensure(pin.is_some(), &format!("storage.sd.{name}"), "is required")?;
            }
            ensure(
                self.storage.sd.rotate_bytes >= 64 * 1_024,
                "storage.sd.rotate_bytes",
                "must be at least 65536 bytes",
            )?;
        }
        if self.sync.enabled {
            ensure(
                self.storage.sd.enabled,
                "sync.enabled",
                "network sync requires enabled microSD storage",
            )?;
            ensure(
                valid_http_url(&self.sync.base_url),
                "sync.base_url",
                "must be an http:// or https:// URL with a host",
            )?;
            ensure(
                self.sync.block_bytes == DEFAULT_SD_BLOCK_BYTES,
                "sync.block_bytes",
                "profile v1 requires 1048576-byte resumable blocks",
            )?;
        }
        Ok(())
    }

    fn validate_pin_plan(&self) -> Result<(), ProfileError> {
        let mut pins: BTreeMap<i32, (String, PinDirection)> = BTreeMap::new();
        let mut add = |field: &str, pin: Option<i32>, direction: PinDirection| {
            let Some(pin) = pin else { return Ok(()) };
            let board_owned_gpio27 = field == "runtime.status_led.gpios.data"
                && self.target.chip == HardwareChip::Esp32c5
                && self.target.board_variant == "esp32-c5-devkitc-1"
                && pin == 27;
            if !board_owned_gpio27 {
                validate_chip_pin(self.target.chip, pin, direction).map_err(|message| {
                    ProfileError {
                        field: field.to_string(),
                        message,
                    }
                })?;
            }
            if let Some((other, _)) = pins.insert(pin, (field.to_string(), direction)) {
                return Err(ProfileError {
                    field: field.to_string(),
                    message: format!("GPIO{pin} is already assigned to {other}"),
                });
            }
            Ok(())
        };

        if self.runtime.status_led_enabled != self.runtime.status_led.enabled {
            return Err(ProfileError {
                field: "runtime.status_led.enabled".to_string(),
                message: "legacy and nested status LED enabled fields differ".to_string(),
            });
        }
        if self.runtime.status_led.enabled {
            ensure(
                self.target.chip == HardwareChip::Esp32c5,
                "runtime.status_led.enabled",
                "status RGB LED v1 is supported only on ESP32-C5",
            )?;
            ensure(
                (1..=100).contains(&self.runtime.status_led.brightness_percent),
                "runtime.status_led.brightness_percent",
                "must be within 1..=100",
            )?;
            for required in [
                "boot",
                "online",
                "offline",
                "error",
                "ir_rx",
                "ir_tx",
                "provisioning",
                "recording",
            ] {
                let color = self
                    .runtime
                    .status_led
                    .colors
                    .get(required)
                    .ok_or_else(|| ProfileError {
                        field: format!("runtime.status_led.colors.{required}"),
                        message: "required status color is missing".to_string(),
                    })?;
                ensure(
                    color.len() == 3,
                    &format!("runtime.status_led.colors.{required}"),
                    "must contain exactly three RGB bytes",
                )?;
            }
            let distinct_colors = self
                .runtime
                .status_led
                .colors
                .values()
                .map(|color| [color[0], color[1], color[2]])
                .collect::<BTreeSet<_>>();
            ensure(
                distinct_colors.len() == self.runtime.status_led.colors.len(),
                "runtime.status_led.colors",
                "all status colors must be distinct",
            )?;
            match self.runtime.status_led.kind {
                StatusLedKind::Ws2812 => {
                    ensure(
                        self.runtime.status_led_gpio == self.runtime.status_led.gpios.data,
                        "runtime.status_led_gpio",
                        "legacy GPIO must equal nested WS2812 data GPIO",
                    )?;
                    add(
                        "runtime.status_led.gpios.data",
                        self.runtime.status_led.gpios.data,
                        PinDirection::Output,
                    )?;
                }
                StatusLedKind::RgbDiscrete => {
                    return Err(ProfileError {
                        field: "runtime.status_led.kind".to_string(),
                        message: "rgb_discrete requires three PWM channels and is outside profile v1; use ws2812".to_string(),
                    });
                }
            }
        }
        if self.peripherals.ir_rx.enabled {
            add(
                "peripherals.ir_rx.gpio",
                self.peripherals.ir_rx.gpio,
                PinDirection::Input,
            )?;
        }
        if self.peripherals.ir_tx.enabled {
            add(
                "peripherals.ir_tx.gpio",
                self.peripherals.ir_tx.gpio,
                PinDirection::Output,
            )?;
        }
        for relay in self.peripherals.relays.iter().filter(|relay| relay.enabled) {
            add("peripherals.relays.gpio", relay.gpio, PinDirection::Output)?;
        }
        if self.storage.sd.enabled {
            add(
                "storage.sd.sclk_gpio",
                self.storage.sd.sclk_gpio,
                PinDirection::Output,
            )?;
            add(
                "storage.sd.mosi_gpio",
                self.storage.sd.mosi_gpio,
                PinDirection::Output,
            )?;
            add(
                "storage.sd.miso_gpio",
                self.storage.sd.miso_gpio,
                PinDirection::Input,
            )?;
            add(
                "storage.sd.cs_gpio",
                self.storage.sd.cs_gpio,
                PinDirection::Output,
            )?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum PinDirection {
    Input,
    Output,
}

fn validate_chip_pin(chip: HardwareChip, pin: i32, direction: PinDirection) -> Result<(), String> {
    let max = match chip {
        HardwareChip::Wroom => 39,
        HardwareChip::Esp32c5 => 28,
        HardwareChip::Esp32c6 => 30,
    };
    if !(0..=max).contains(&pin) {
        return Err(format!("GPIO{pin} is outside the {chip:?} range 0..={max}"));
    }

    match chip {
        HardwareChip::Wroom => {
            const AVAILABLE: &[i32] = &[
                0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 21, 22, 23,
                25, 26, 27, 32, 33, 34, 35, 36, 39,
            ];
            if !AVAILABLE.contains(&pin) {
                return Err(format!("GPIO{pin} is not broken out by ESP32-WROOM-32"));
            }
            if (6..=11).contains(&pin) {
                return Err(format!("GPIO{pin} is reserved for module flash"));
            }
            if [1, 3].contains(&pin) {
                return Err(format!(
                    "GPIO{pin} is reserved for the UART0 provisioning console"
                ));
            }
            if [0, 2, 5, 12, 15].contains(&pin) {
                return Err(format!("GPIO{pin} is a boot strapping pin"));
            }
            if matches!(direction, PinDirection::Output) && (34..=39).contains(&pin) {
                return Err(format!("GPIO{pin} is input-only on ESP32-WROOM"));
            }
        }
        HardwareChip::Esp32c5 => {
            if [7, 25, 26, 27, 28].contains(&pin) {
                return Err(format!("GPIO{pin} is a boot strapping pin on ESP32-C5"));
            }
            if [2, 3, 4, 5].contains(&pin) {
                return Err(format!("GPIO{pin} is reserved for JTAG in profile v1"));
            }
            if [11, 12, 13, 14].contains(&pin) {
                return Err(format!("GPIO{pin} is reserved for UART/USB in profile v1"));
            }
            if (15..=22).contains(&pin) {
                return Err(format!("GPIO{pin} may be connected to flash/PSRAM"));
            }
        }
        HardwareChip::Esp32c6 => {}
    }
    Ok(())
}

fn ensure(
    condition: bool,
    field: impl Into<String>,
    message: impl Into<String>,
) -> Result<(), ProfileError> {
    if condition {
        Ok(())
    } else {
        Err(ProfileError {
            field: field.into(),
            message: message.into(),
        })
    }
}

fn valid_http_url(value: &str) -> bool {
    let value = value.trim();
    let authority = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
        .and_then(|rest| rest.split('/').next());
    authority.is_some_and(|host| {
        !host.is_empty() && !host.chars().any(char::is_whitespace) && !host.starts_with(':')
    })
}

/// Streams canonical JSON directly into SHA-256. Unlike the previous
/// `serde_json::Value` based implementation, this needs no second profile tree
/// and no contiguous canonical JSON buffer on the ESP32 heap.
fn canonical_sha256<T>(value: &T) -> Result<String, serde_json::Error>
where
    T: Serialize + ?Sized,
{
    let mut writer = Sha256Writer(Sha256::new());
    serde_json::to_writer(&mut writer, value)?;
    Ok(format!("{:x}", writer.0.finalize()))
}

struct Sha256Writer(Sha256);

impl std::io::Write for Sha256Writer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Borrowed canonical view of a device profile.
///
/// The Python contract uses `json.dumps(..., sort_keys=True, separators=(",",
/// ":"))`. The wrappers below declare every object field in lexicographic
/// order, while arrays retain their semantic order and `BTreeMap` fields are
/// already key ordered. All wrappers borrow the parsed profile, so hashing does
/// not clone strings, arrays, maps, or nested structures.
struct CanonicalDeviceProfile<'a> {
    profile: &'a DeviceProfile,
    include_target: bool,
}

impl<'a> CanonicalDeviceProfile<'a> {
    fn new(profile: &'a DeviceProfile, include_target: bool) -> Self {
        Self {
            profile,
            include_target,
        }
    }
}

impl Serialize for CanonicalDeviceProfile<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Nine runtime fields are always present. `secret_revisions` preserves
        // DeviceProfile's skip-if-empty wire contract, and target is omitted
        // only for the runtime config hash.
        let mut field_count = 9;
        if !self.profile.secret_revisions.is_empty() {
            field_count += 1;
        }
        if self.include_target {
            field_count += 1;
        }

        let mut map = serializer.serialize_map(Some(field_count))?;
        map.serialize_entry("csi", &CanonicalCsiProfile::from(&self.profile.csi))?;
        map.serialize_entry(
            "identity",
            &CanonicalIdentityProfile::from(&self.profile.identity),
        )?;
        map.serialize_entry("mqtt", &CanonicalMqttProfile::from(&self.profile.mqtt))?;
        map.serialize_entry(
            "network",
            &CanonicalNetworkProfile::from(&self.profile.network),
        )?;
        map.serialize_entry(
            "peripherals",
            &CanonicalPeripheralProfile::from(&self.profile.peripherals),
        )?;
        map.serialize_entry(
            "runtime",
            &CanonicalRuntimeProfile::from(&self.profile.runtime),
        )?;
        map.serialize_entry("schema", &self.profile.schema)?;
        if !self.profile.secret_revisions.is_empty() {
            map.serialize_entry("secret_revisions", &self.profile.secret_revisions)?;
        }
        map.serialize_entry(
            "storage",
            &CanonicalStorageProfile::from(&self.profile.storage),
        )?;
        map.serialize_entry("sync", &CanonicalSyncProfile::from(&self.profile.sync))?;
        if self.include_target {
            map.serialize_entry(
                "target",
                &CanonicalTargetProfile::from(&self.profile.target),
            )?;
        }
        map.end()
    }
}

#[derive(Serialize)]
struct CanonicalIdentityProfile<'a> {
    device_id: &'a Option<String>,
    name: &'a str,
    profile_uuid: &'a str,
}

impl<'a> From<&'a IdentityProfile> for CanonicalIdentityProfile<'a> {
    fn from(profile: &'a IdentityProfile) -> Self {
        Self {
            device_id: &profile.device_id,
            name: &profile.name,
            profile_uuid: &profile.profile_uuid,
        }
    }
}

#[derive(Serialize)]
struct CanonicalTargetProfile<'a> {
    board_variant: &'a str,
    chip: HardwareChip,
    feature_set: &'a [String],
    partition_table: &'a str,
}

impl<'a> From<&'a TargetProfile> for CanonicalTargetProfile<'a> {
    fn from(profile: &'a TargetProfile) -> Self {
        Self {
            board_variant: &profile.board_variant,
            chip: profile.chip,
            feature_set: &profile.feature_set,
            partition_table: &profile.partition_table,
        }
    }
}

#[derive(Serialize)]
struct CanonicalNetworkProfile<'a> {
    udp_host: &'a str,
    udp_port: u16,
    wifi_ssid: &'a str,
}

impl<'a> From<&'a NetworkProfile> for CanonicalNetworkProfile<'a> {
    fn from(profile: &'a NetworkProfile) -> Self {
        Self {
            udp_host: &profile.udp_host,
            udp_port: profile.udp_port,
            wifi_ssid: &profile.wifi_ssid,
        }
    }
}

#[derive(Serialize)]
struct CanonicalCsiProfile<'a> {
    empty_room_only: bool,
    enabled: bool,
    prefer_ht40: bool,
    queue_capacity: usize,
    send_frequency_hz: u32,
    wifi_band: &'a str,
}

impl<'a> From<&'a CsiProfile> for CanonicalCsiProfile<'a> {
    fn from(profile: &'a CsiProfile) -> Self {
        Self {
            empty_room_only: profile.empty_room_only,
            enabled: profile.enabled,
            prefer_ht40: profile.prefer_ht40,
            queue_capacity: profile.queue_capacity,
            send_frequency_hz: profile.send_frequency_hz,
            wifi_band: &profile.wifi_band,
        }
    }
}

#[derive(Serialize)]
struct CanonicalRuntimeProfile<'a> {
    status_led: CanonicalStatusLedProfile<'a>,
    status_led_enabled: bool,
    status_led_gpio: &'a Option<i32>,
}

impl<'a> From<&'a RuntimeProfile> for CanonicalRuntimeProfile<'a> {
    fn from(profile: &'a RuntimeProfile) -> Self {
        Self {
            status_led: CanonicalStatusLedProfile::from(&profile.status_led),
            status_led_enabled: profile.status_led_enabled,
            status_led_gpio: &profile.status_led_gpio,
        }
    }
}

#[derive(Serialize)]
struct CanonicalStatusLedProfile<'a> {
    brightness_percent: u8,
    colors: &'a BTreeMap<String, Vec<u8>>,
    enabled: bool,
    gpios: CanonicalStatusLedGpios<'a>,
    kind: StatusLedKind,
}

impl<'a> From<&'a StatusLedProfile> for CanonicalStatusLedProfile<'a> {
    fn from(profile: &'a StatusLedProfile) -> Self {
        Self {
            brightness_percent: profile.brightness_percent,
            colors: &profile.colors,
            enabled: profile.enabled,
            gpios: CanonicalStatusLedGpios::from(&profile.gpios),
            kind: profile.kind,
        }
    }
}

#[derive(Serialize)]
struct CanonicalStatusLedGpios<'a> {
    blue: &'a Option<i32>,
    data: &'a Option<i32>,
    green: &'a Option<i32>,
    red: &'a Option<i32>,
}

impl<'a> From<&'a StatusLedGpios> for CanonicalStatusLedGpios<'a> {
    fn from(gpios: &'a StatusLedGpios) -> Self {
        Self {
            blue: &gpios.blue,
            data: &gpios.data,
            green: &gpios.green,
            red: &gpios.red,
        }
    }
}

#[derive(Serialize)]
struct CanonicalMqttProfile<'a> {
    base_topic: &'a str,
    broker_host: &'a str,
    enabled: bool,
    port: u16,
    qos: u8,
    tls: bool,
    username: &'a Option<String>,
}

impl<'a> From<&'a MqttProfile> for CanonicalMqttProfile<'a> {
    fn from(profile: &'a MqttProfile) -> Self {
        Self {
            base_topic: &profile.base_topic,
            broker_host: &profile.broker_host,
            enabled: profile.enabled,
            port: profile.port,
            qos: profile.qos,
            tls: profile.tls,
            username: &profile.username,
        }
    }
}

#[derive(Serialize)]
struct CanonicalPeripheralProfile<'a> {
    ir_rx: CanonicalIrRxProfile<'a>,
    ir_tx: CanonicalIrTxProfile<'a>,
    relays: CanonicalRelays<'a>,
}

impl<'a> From<&'a PeripheralProfile> for CanonicalPeripheralProfile<'a> {
    fn from(profile: &'a PeripheralProfile) -> Self {
        Self {
            ir_rx: CanonicalIrRxProfile::from(&profile.ir_rx),
            ir_tx: CanonicalIrTxProfile::from(&profile.ir_tx),
            relays: CanonicalRelays(&profile.relays),
        }
    }
}

#[derive(Serialize)]
struct CanonicalIrRxProfile<'a> {
    enabled: bool,
    gpio: &'a Option<i32>,
    idle_threshold_us: u32,
}

impl<'a> From<&'a IrRxProfile> for CanonicalIrRxProfile<'a> {
    fn from(profile: &'a IrRxProfile) -> Self {
        Self {
            enabled: profile.enabled,
            gpio: &profile.gpio,
            idle_threshold_us: profile.idle_threshold_us,
        }
    }
}

#[derive(Serialize)]
struct CanonicalIrTxProfile<'a> {
    carrier_hz: u32,
    duty_percent: u8,
    enabled: bool,
    gpio: &'a Option<i32>,
}

impl<'a> From<&'a IrTxProfile> for CanonicalIrTxProfile<'a> {
    fn from(profile: &'a IrTxProfile) -> Self {
        Self {
            carrier_hz: profile.carrier_hz,
            duty_percent: profile.duty_percent,
            enabled: profile.enabled,
            gpio: &profile.gpio,
        }
    }
}

struct CanonicalRelays<'a>(&'a [RelayProfile]);

impl Serialize for CanonicalRelays<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for relay in self.0 {
            sequence.serialize_element(&CanonicalRelayProfile::from(relay))?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
struct CanonicalRelayProfile<'a> {
    active_high: bool,
    enabled: bool,
    gpio: &'a Option<i32>,
    id: &'a str,
    name: &'a str,
    pulse_limit_ms: u32,
    restore_state: bool,
}

impl<'a> From<&'a RelayProfile> for CanonicalRelayProfile<'a> {
    fn from(profile: &'a RelayProfile) -> Self {
        Self {
            active_high: profile.active_high,
            enabled: profile.enabled,
            gpio: &profile.gpio,
            id: &profile.id,
            name: &profile.name,
            pulse_limit_ms: profile.pulse_limit_ms,
            restore_state: profile.restore_state,
        }
    }
}

#[derive(Serialize)]
struct CanonicalStorageProfile<'a> {
    recording: CanonicalRecordingProfile<'a>,
    sd: CanonicalSdProfile<'a>,
}

impl<'a> From<&'a StorageProfile> for CanonicalStorageProfile<'a> {
    fn from(profile: &'a StorageProfile) -> Self {
        Self {
            recording: CanonicalRecordingProfile::from(&profile.recording),
            sd: CanonicalSdProfile::from(&profile.sd),
        }
    }
}

#[derive(Serialize)]
struct CanonicalSdProfile<'a> {
    cs_gpio: &'a Option<i32>,
    enabled: bool,
    miso_gpio: &'a Option<i32>,
    mosi_gpio: &'a Option<i32>,
    mount_point: &'a str,
    rotate_bytes: u64,
    sclk_gpio: &'a Option<i32>,
}

impl<'a> From<&'a SdProfile> for CanonicalSdProfile<'a> {
    fn from(profile: &'a SdProfile) -> Self {
        Self {
            cs_gpio: &profile.cs_gpio,
            enabled: profile.enabled,
            miso_gpio: &profile.miso_gpio,
            mosi_gpio: &profile.mosi_gpio,
            mount_point: &profile.mount_point,
            rotate_bytes: profile.rotate_bytes,
            sclk_gpio: &profile.sclk_gpio,
        }
    }
}

#[derive(Serialize)]
struct CanonicalRecordingProfile<'a> {
    events_enabled: bool,
    privacy_mode: &'a PrivacyMode,
    raw_csi_enabled: bool,
}

impl<'a> From<&'a RecordingProfile> for CanonicalRecordingProfile<'a> {
    fn from(profile: &'a RecordingProfile) -> Self {
        Self {
            events_enabled: profile.events_enabled,
            privacy_mode: &profile.privacy_mode,
            raw_csi_enabled: profile.raw_csi_enabled,
        }
    }
}

#[derive(Serialize)]
struct CanonicalSyncProfile<'a> {
    base_url: &'a str,
    block_bytes: u32,
    delete_after_sync: bool,
    enabled: bool,
    network_first: bool,
}

impl<'a> From<&'a SyncProfile> for CanonicalSyncProfile<'a> {
    fn from(profile: &'a SyncProfile) -> Self {
        Self {
            base_url: &profile.base_url,
            block_bytes: profile.block_bytes,
            delete_after_sync: profile.delete_after_sync,
            enabled: profile.enabled,
            network_first: profile.network_first,
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_partition_table() -> String {
    "partitions.csv".to_string()
}
fn default_board_variant() -> String {
    "generic".to_string()
}
fn default_status_led_brightness() -> u8 {
    25
}
fn default_status_led_colors() -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([
        ("boot".to_string(), vec![255, 160, 60]),
        ("online".to_string(), vec![0, 180, 40]),
        ("offline".to_string(), vec![0, 80, 255]),
        ("error".to_string(), vec![255, 0, 0]),
        ("ir_rx".to_string(), vec![255, 220, 80]),
        ("ir_tx".to_string(), vec![255, 0, 255]),
        ("provisioning".to_string(), vec![0, 220, 220]),
        ("recording".to_string(), vec![120, 40, 255]),
    ])
}
fn default_udp_host() -> String {
    "255.255.255.255".to_string()
}
fn default_udp_port() -> u16 {
    5005
}
fn default_send_frequency_hz() -> u32 {
    10
}
fn default_queue_capacity() -> usize {
    256
}
fn default_wifi_band() -> String {
    "auto".to_string()
}
fn default_mqtt_port() -> u16 {
    1883
}
fn default_qos() -> u8 {
    1
}
fn default_base_topic() -> String {
    "aethersense/devices".to_string()
}
fn default_ir_idle_threshold_us() -> u32 {
    15_000
}
fn default_ir_carrier_hz() -> u32 {
    38_000
}
fn default_ir_duty_percent() -> u8 {
    33
}
fn default_relay_pulse_limit_ms() -> u32 {
    30_000
}
fn default_sd_mount_point() -> String {
    "/sdcard".to_string()
}
fn default_rotate_bytes() -> u64 {
    4 * 1024 * 1024
}
fn default_block_bytes() -> u32 {
    DEFAULT_SD_BLOCK_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_profile() -> DeviceProfile {
        let mut profile = DeviceProfile::default();
        profile.identity.profile_uuid = "profile-1".to_string();
        profile.identity.name = "C5 quarto".to_string();
        profile.target.chip = HardwareChip::Esp32c5;
        profile
    }

    #[test]
    fn parses_the_tui_v1_shape() {
        let mut profile = valid_profile();
        profile.peripherals.ir_tx.enabled = true;
        profile.peripherals.ir_tx.gpio = Some(8);
        let json = serde_json::to_string(&profile).unwrap();
        assert_eq!(DeviceProfile::from_json(&json).unwrap(), profile);
    }

    #[test]
    fn duplicate_pin_is_rejected() {
        let mut profile = valid_profile();
        profile.peripherals.ir_rx.enabled = true;
        profile.peripherals.ir_rx.gpio = Some(8);
        profile.peripherals.ir_tx.enabled = true;
        profile.peripherals.ir_tx.gpio = Some(8);
        let err = profile.validate().unwrap_err();
        assert!(err.message.contains("already assigned"));
    }

    #[test]
    fn raw_csi_requires_explicit_empty_room_mode() {
        let mut profile = valid_profile();
        profile.storage.sd.enabled = true;
        profile.storage.sd.sclk_gpio = Some(8);
        profile.storage.sd.mosi_gpio = Some(9);
        profile.storage.sd.miso_gpio = Some(10);
        profile.storage.sd.cs_gpio = Some(23);
        profile.storage.recording.raw_csi_enabled = true;
        profile.storage.recording.privacy_mode = PrivacyMode::EventsOnly;
        assert!(profile.validate().is_err());
    }

    #[test]
    fn canonical_hash_is_stable() {
        let profile = valid_profile();
        assert_eq!(
            profile.config_hash().unwrap(),
            profile.config_hash().unwrap()
        );
        assert_eq!(profile.config_hash().unwrap().len(), 64);
    }

    #[test]
    fn config_hash_excludes_build_only_target() {
        let profile = valid_profile();
        let mut changed = profile.clone();
        changed.target.partition_table = "different.csv".to_string();
        changed.target.feature_set.push("mqtt".to_string());
        assert_eq!(
            profile.config_hash().unwrap(),
            changed.config_hash().unwrap()
        );
    }

    #[test]
    fn golden_tui_profile_is_accepted() {
        let json = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/device_profile_v1.json"
        ));
        let profile = DeviceProfile::from_json(json).unwrap();
        assert_eq!(profile.target.chip, HardwareChip::Esp32c5);
        assert_eq!(profile.enabled_relay_count(), 1);
        // Golden value produced by Python's
        // aethersense_build.diffing.compute_config_hash contract.
        assert_eq!(
            profile.config_hash().unwrap(),
            "849ad61ec3343183ea5275236198dfe241b2f30d44065fb6d77b1e4a31554331"
        );
        assert_eq!(
            profile.build_hash().unwrap(),
            "5a8ee1fda1296fa7c2071f331b7879ee87631f80ac5b84f19fad1eef0d1bb9fb"
        );
        // Full canonical document produced by Python with sort_keys=True,
        // separators=(",", ":") and ensure_ascii=False.
        assert_eq!(
            crate::provisioning::sha256_hex(profile.canonical_json().unwrap().as_bytes()),
            "5a79f7aae6512a6dffb557615f93fe41a2d46e8c98d4dca6b1e938f254b002f1"
        );
    }

    #[test]
    fn opaque_secret_revision_is_part_of_cross_language_config_hash() {
        let mut profile = golden_profile();
        profile.secret_revisions.insert(
            "wifi_password".to_string(),
            "0123456789abcdef0123456789abcdef".to_string(),
        );
        profile.validate().unwrap();
        assert_eq!(
            profile.config_hash().unwrap(),
            "0cbc4a32fc0c3ab1b9a3a97ea15caa1c4d2e043f68baa124b8f5eb69cdbb8efa"
        );

        profile
            .secret_revisions
            .insert("wifi_password".to_string(), "not-a-nonce".to_string());
        assert!(profile.validate().is_err());
    }

    fn golden_profile() -> DeviceProfile {
        DeviceProfile::from_json(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/device_profile_v1.json"
        )))
        .unwrap()
    }

    #[test]
    fn serial_validation_enforces_tui_resource_boundaries() {
        let invalid_mutations: Vec<Box<dyn Fn(&mut DeviceProfile)>> = vec![
            Box::new(|profile| profile.csi.send_frequency_hz = 0),
            Box::new(|profile| profile.csi.send_frequency_hz = 101),
            Box::new(|profile| profile.csi.queue_capacity = 31),
            Box::new(|profile| profile.csi.queue_capacity = 4_097),
            Box::new(|profile| profile.peripherals.ir_rx.idle_threshold_us = 999),
            Box::new(|profile| profile.peripherals.ir_rx.idle_threshold_us = 100_001),
            Box::new(|profile| profile.peripherals.ir_tx.duty_percent = 9),
            Box::new(|profile| profile.peripherals.ir_tx.duty_percent = 51),
            Box::new(|profile| profile.peripherals.relays[0].pulse_limit_ms = 99),
            Box::new(|profile| profile.peripherals.relays[0].pulse_limit_ms = 600_001),
            Box::new(|profile| profile.storage.sd.rotate_bytes = 65_535),
            Box::new(|profile| profile.sync.block_bytes = 64 * 1_024),
            Box::new(|profile| profile.sync.base_url = "ftp://unsafe.invalid".to_string()),
            Box::new(|profile| profile.target.partition_table = "custom.csv".to_string()),
        ];
        for mutate in invalid_mutations {
            let mut profile = golden_profile();
            mutate(&mut profile);
            assert!(profile.validate().is_err());
        }

        for (minimum, maximum) in [(1, 100)] {
            let mut profile = golden_profile();
            profile.csi.send_frequency_hz = minimum;
            profile.validate().unwrap();
            profile.csi.send_frequency_hz = maximum;
            profile.validate().unwrap();
        }
        let mut profile = golden_profile();
        profile.csi.queue_capacity = 32;
        profile.peripherals.ir_rx.idle_threshold_us = 1_000;
        profile.peripherals.ir_tx.duty_percent = 10;
        profile.peripherals.relays[0].pulse_limit_ms = 100;
        profile.storage.sd.rotate_bytes = 65_536;
        profile.validate().unwrap();
        profile.csi.queue_capacity = 4_096;
        profile.peripherals.ir_rx.idle_threshold_us = 100_000;
        profile.peripherals.ir_tx.duty_percent = 50;
        profile.peripherals.relays[0].pulse_limit_ms = 600_000;
        profile.validate().unwrap();
    }

    #[test]
    fn wroom_rejects_nonexistent_and_provisioning_uart_pins() {
        for pin in [1, 3, 20, 24, 28, 29, 30, 31, 37, 38] {
            let mut profile = valid_profile();
            profile.target.chip = HardwareChip::Wroom;
            profile.peripherals.ir_tx.enabled = true;
            profile.peripherals.ir_tx.gpio = Some(pin);
            assert!(profile.validate().is_err(), "GPIO{pin} should be rejected");
        }
    }

    #[test]
    fn status_led_colors_must_be_distinct() {
        let mut profile = valid_profile();
        profile.target.board_variant = "esp32-c5-devkitc-1".to_string();
        profile.runtime.status_led_enabled = true;
        profile.runtime.status_led_gpio = Some(27);
        profile.runtime.status_led.enabled = true;
        profile.runtime.status_led.gpios.data = Some(27);
        let boot = profile.runtime.status_led.colors["boot"].clone();
        profile
            .runtime
            .status_led
            .colors
            .insert("error".to_string(), boot);
        assert!(profile.validate().is_err());
    }
}
