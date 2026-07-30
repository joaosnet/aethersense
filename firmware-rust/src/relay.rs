//! Boot-safe relay state machine with bounded pulse duration and guarded restore.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::profile::{RelayProfile, MAX_RELAYS};

pub const RELAY_STATE_SCHEMA: &str = "aethersense.relay_state.v1";
pub const DEFAULT_RESTORE_DELAY_MS: u64 = 2_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelaySnapshot {
    #[serde(default = "default_schema")]
    pub schema: String,
    pub config_hash: String,
    pub states: BTreeMap<String, bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayError {
    TooManyRelays,
    DuplicateId(String),
    UnknownRelay(String),
    Disabled(String),
    InvalidPulse { requested_ms: u32, limit_ms: u32 },
    Hardware(String),
    StateStore(String),
}

impl std::fmt::Display for RelayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyRelays => write!(f, "at most {MAX_RELAYS} relays are supported"),
            Self::DuplicateId(id) => write!(f, "duplicate relay id {id}"),
            Self::UnknownRelay(id) => write!(f, "unknown relay {id}"),
            Self::Disabled(id) => write!(f, "relay {id} is disabled"),
            Self::InvalidPulse {
                requested_ms,
                limit_ms,
            } => {
                write!(f, "pulse {requested_ms}ms exceeds limit {limit_ms}ms")
            }
            Self::Hardware(message) | Self::StateStore(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for RelayError {}

pub trait RelayHardware {
    /// Configure the physical output and leave it electrically inactive.
    fn configure_inactive(&mut self, relay: &RelayProfile) -> Result<(), String>;
    fn set_energized(&mut self, relay: &RelayProfile, state: bool) -> Result<(), String>;
}

pub trait RelayStateStore {
    fn load(&self) -> Result<Option<RelaySnapshot>, String>;
    fn save(&mut self, snapshot: &RelaySnapshot) -> Result<(), String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayEvent {
    StateChanged { relay_id: String, state: bool },
    PulseStarted { relay_id: String, duration_ms: u32 },
    PulseFinished { relay_id: String },
    StateRestored { relay_id: String, state: bool },
    RestoreSkipped { reason: String },
}

pub struct RelayManager<H, S> {
    hardware: H,
    store: S,
    configs: BTreeMap<String, RelayProfile>,
    states: BTreeMap<String, bool>,
    pulse_deadlines_ms: BTreeMap<String, u64>,
    config_hash: String,
    restore_not_before_ms: u64,
    restore_processed: bool,
}

impl<H: RelayHardware, S: RelayStateStore> RelayManager<H, S> {
    pub fn new(
        mut hardware: H,
        store: S,
        relays: &[RelayProfile],
        config_hash: impl Into<String>,
        boot_now_ms: u64,
    ) -> Result<Self, RelayError> {
        if relays.len() > MAX_RELAYS {
            return Err(RelayError::TooManyRelays);
        }
        let mut configs = BTreeMap::new();
        let mut states = BTreeMap::new();
        for relay in relays {
            if configs.contains_key(&relay.id) {
                return Err(RelayError::DuplicateId(relay.id.clone()));
            }
            if relay.enabled {
                // This is the boot safety invariant: configure and drive every
                // output inactive before considering persisted state.
                hardware
                    .configure_inactive(relay)
                    .map_err(RelayError::Hardware)?;
                hardware
                    .set_energized(relay, false)
                    .map_err(RelayError::Hardware)?;
                states.insert(relay.id.clone(), false);
            }
            configs.insert(relay.id.clone(), relay.clone());
        }
        Ok(Self {
            hardware,
            store,
            configs,
            states,
            pulse_deadlines_ms: BTreeMap::new(),
            config_hash: config_hash.into(),
            restore_not_before_ms: boot_now_ms.saturating_add(DEFAULT_RESTORE_DELAY_MS),
            restore_processed: false,
        })
    }

    pub fn restore_if_due(&mut self, now_ms: u64) -> Result<Vec<RelayEvent>, RelayError> {
        if self.restore_processed || now_ms < self.restore_not_before_ms {
            return Ok(Vec::new());
        }
        self.restore_processed = true;
        let Some(snapshot) = self.store.load().map_err(RelayError::StateStore)? else {
            return Ok(vec![RelayEvent::RestoreSkipped {
                reason: "no persisted state".to_string(),
            }]);
        };
        if snapshot.schema != RELAY_STATE_SCHEMA || snapshot.config_hash != self.config_hash {
            return Ok(vec![RelayEvent::RestoreSkipped {
                reason: "state schema/config hash mismatch".to_string(),
            }]);
        }
        let mut events = Vec::new();
        for (id, state) in snapshot.states {
            let Some(config) = self.configs.get(&id) else {
                continue;
            };
            if !config.enabled || !config.restore_state {
                continue;
            }
            self.hardware
                .set_energized(config, state)
                .map_err(RelayError::Hardware)?;
            self.states.insert(id.clone(), state);
            events.push(RelayEvent::StateRestored {
                relay_id: id,
                state,
            });
        }
        Ok(events)
    }

    pub fn set(&mut self, relay_id: &str, state: bool) -> Result<RelayEvent, RelayError> {
        let config = self.config(relay_id)?.clone();
        self.hardware
            .set_energized(&config, state)
            .map_err(RelayError::Hardware)?;
        self.states.insert(relay_id.to_string(), state);
        // An explicit stable state always supersedes a pending pulse.
        self.pulse_deadlines_ms.remove(relay_id);
        self.persist()?;
        Ok(RelayEvent::StateChanged {
            relay_id: relay_id.to_string(),
            state,
        })
    }

    pub fn pulse(
        &mut self,
        relay_id: &str,
        duration_ms: u32,
        now_ms: u64,
    ) -> Result<RelayEvent, RelayError> {
        let config = self.config(relay_id)?.clone();
        if duration_ms == 0 || duration_ms > config.pulse_limit_ms {
            return Err(RelayError::InvalidPulse {
                requested_ms: duration_ms,
                limit_ms: config.pulse_limit_ms,
            });
        }
        // A pulse is transient and must never be restored after a reboot. Save
        // OFF as the stable state before energizing; the in-memory state tracks
        // the physical pulse until its monotonic deadline.
        self.states.insert(relay_id.to_string(), false);
        self.persist()?;
        self.hardware
            .set_energized(&config, true)
            .map_err(RelayError::Hardware)?;
        self.states.insert(relay_id.to_string(), true);
        self.pulse_deadlines_ms.insert(
            relay_id.to_string(),
            now_ms.saturating_add(u64::from(duration_ms)),
        );
        Ok(RelayEvent::PulseStarted {
            relay_id: relay_id.to_string(),
            duration_ms,
        })
    }

    pub fn tick(&mut self, now_ms: u64) -> Result<Vec<RelayEvent>, RelayError> {
        let due: Vec<_> = self
            .pulse_deadlines_ms
            .iter()
            .filter(|(_, deadline)| **deadline <= now_ms)
            .map(|(id, _)| id.clone())
            .collect();
        let mut events = Vec::with_capacity(due.len());
        for id in due {
            let config = self.config(&id)?.clone();
            self.hardware
                .set_energized(&config, false)
                .map_err(RelayError::Hardware)?;
            self.states.insert(id.clone(), false);
            self.pulse_deadlines_ms.remove(&id);
            events.push(RelayEvent::PulseFinished { relay_id: id });
        }
        if !events.is_empty() {
            self.persist()?;
        }
        Ok(events)
    }

    pub fn state(&self, relay_id: &str) -> Option<bool> {
        self.states.get(relay_id).copied()
    }

    pub fn states(&self) -> &BTreeMap<String, bool> {
        &self.states
    }

    pub fn into_parts(self) -> (H, S) {
        (self.hardware, self.store)
    }

    fn config(&self, relay_id: &str) -> Result<&RelayProfile, RelayError> {
        let config = self
            .configs
            .get(relay_id)
            .ok_or_else(|| RelayError::UnknownRelay(relay_id.to_string()))?;
        if !config.enabled {
            return Err(RelayError::Disabled(relay_id.to_string()));
        }
        Ok(config)
    }

    fn persist(&mut self) -> Result<(), RelayError> {
        self.store
            .save(&RelaySnapshot {
                schema: RELAY_STATE_SCHEMA.to_string(),
                config_hash: self.config_hash.clone(),
                states: self.states.clone(),
            })
            .map_err(RelayError::StateStore)
    }
}

fn default_schema() -> String {
    RELAY_STATE_SCHEMA.to_string()
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub mod esp {
    use super::*;
    use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};

    pub struct EspRelayHardware {
        pins: BTreeMap<String, i32>,
    }

    impl EspRelayHardware {
        pub fn new() -> Self {
            Self {
                pins: BTreeMap::new(),
            }
        }

        fn physical_level(active_high: bool, energized: bool) -> u32 {
            u32::from(active_high == energized)
        }

        fn drive(gpio: i32, active_high: bool, energized: bool) -> Result<(), String> {
            esp_idf_sys::esp!(unsafe {
                esp_idf_sys::gpio_set_level(gpio, Self::physical_level(active_high, energized))
            })
            .map_err(|err| err.to_string())
        }
    }

    impl Default for EspRelayHardware {
        fn default() -> Self {
            Self::new()
        }
    }

    impl RelayHardware for EspRelayHardware {
        fn configure_inactive(&mut self, relay: &RelayProfile) -> Result<(), String> {
            let gpio = relay
                .gpio
                .ok_or_else(|| "relay GPIO is missing".to_string())?;
            let inactive = Self::physical_level(relay.active_high, false);
            // Preload the GPIO output latch *before* enabling the output driver.
            // This avoids the active-low relay glitch that can occur when a
            // generic output constructor briefly drives its reset-low level.
            esp_idf_sys::esp!(unsafe { esp_idf_sys::gpio_set_level(gpio, inactive) })
                .map_err(|err| err.to_string())?;
            esp_idf_sys::esp!(unsafe {
                esp_idf_sys::gpio_set_direction(gpio, esp_idf_sys::gpio_mode_t_GPIO_MODE_OUTPUT)
            })
            .map_err(|err| err.to_string())?;
            Self::drive(gpio, relay.active_high, false)?;
            self.pins.insert(relay.id.clone(), gpio);
            Ok(())
        }

        fn set_energized(&mut self, relay: &RelayProfile, state: bool) -> Result<(), String> {
            let gpio = self
                .pins
                .get(&relay.id)
                .ok_or_else(|| format!("relay {} GPIO is not configured", relay.id))?;
            Self::drive(*gpio, relay.active_high, state)
        }
    }

    pub struct NvsRelayStateStore {
        nvs: EspNvs<NvsDefault>,
    }

    impl NvsRelayStateStore {
        pub fn new(partition: EspDefaultNvsPartition) -> Result<Self, String> {
            EspNvs::new(partition, "aetherrelay", true)
                .map(|nvs| Self { nvs })
                .map_err(|err| err.to_string())
        }
    }

    impl RelayStateStore for NvsRelayStateStore {
        fn load(&self) -> Result<Option<RelaySnapshot>, String> {
            let Some(len) = self.nvs.blob_len("state").map_err(|err| err.to_string())? else {
                return Ok(None);
            };
            let mut bytes = vec![0; len];
            let Some(value) = self
                .nvs
                .get_blob("state", &mut bytes)
                .map_err(|err| err.to_string())?
            else {
                return Ok(None);
            };
            serde_json::from_slice(value)
                .map(Some)
                .map_err(|err| err.to_string())
        }

        fn save(&mut self, snapshot: &RelaySnapshot) -> Result<(), String> {
            let bytes = serde_json::to_vec(snapshot).map_err(|err| err.to_string())?;
            self.nvs
                .set_blob("state", &bytes)
                .map_err(|err| err.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockHardware(BTreeMap<String, bool>);
    impl RelayHardware for MockHardware {
        fn configure_inactive(&mut self, relay: &RelayProfile) -> Result<(), String> {
            self.0.insert(relay.id.clone(), false);
            Ok(())
        }
        fn set_energized(&mut self, relay: &RelayProfile, state: bool) -> Result<(), String> {
            self.0.insert(relay.id.clone(), state);
            Ok(())
        }
    }

    #[derive(Default)]
    struct MockStore(Option<RelaySnapshot>);
    impl RelayStateStore for MockStore {
        fn load(&self) -> Result<Option<RelaySnapshot>, String> {
            Ok(self.0.clone())
        }
        fn save(&mut self, snapshot: &RelaySnapshot) -> Result<(), String> {
            self.0 = Some(snapshot.clone());
            Ok(())
        }
    }

    fn relay() -> RelayProfile {
        RelayProfile {
            id: "lamp".to_string(),
            name: "Lamp".to_string(),
            enabled: true,
            gpio: Some(23),
            active_high: true,
            pulse_limit_ms: 1_000,
            restore_state: true,
        }
    }

    #[test]
    fn boot_is_inactive_and_restore_waits_two_seconds() {
        let snapshot = RelaySnapshot {
            schema: RELAY_STATE_SCHEMA.to_string(),
            config_hash: "hash".to_string(),
            states: BTreeMap::from([("lamp".to_string(), true)]),
        };
        let mut manager = RelayManager::new(
            MockHardware::default(),
            MockStore(Some(snapshot)),
            &[relay()],
            "hash",
            100,
        )
        .unwrap();
        assert_eq!(manager.state("lamp"), Some(false));
        assert!(manager.restore_if_due(2_099).unwrap().is_empty());
        assert_eq!(manager.restore_if_due(2_100).unwrap().len(), 1);
        assert_eq!(manager.state("lamp"), Some(true));
    }

    #[test]
    fn pulse_turns_off_at_deadline() {
        let mut manager = RelayManager::new(
            MockHardware::default(),
            MockStore::default(),
            &[relay()],
            "hash",
            0,
        )
        .unwrap();
        manager.pulse("lamp", 500, 100).unwrap();
        assert_eq!(manager.state("lamp"), Some(true));
        assert!(manager.tick(599).unwrap().is_empty());
        assert_eq!(manager.tick(600).unwrap().len(), 1);
        assert_eq!(manager.state("lamp"), Some(false));
    }

    #[test]
    fn reboot_during_pulse_never_restores_energized_state() {
        let mut manager = RelayManager::new(
            MockHardware::default(),
            MockStore::default(),
            &[relay()],
            "hash",
            0,
        )
        .unwrap();
        manager.pulse("lamp", 500, 100).unwrap();
        let (_, store) = manager.into_parts();
        assert_eq!(store.0.as_ref().unwrap().states.get("lamp"), Some(&false));

        let mut rebooted =
            RelayManager::new(MockHardware::default(), store, &[relay()], "hash", 1_000).unwrap();
        rebooted.restore_if_due(3_000).unwrap();
        assert_eq!(rebooted.state("lamp"), Some(false));
    }
}
