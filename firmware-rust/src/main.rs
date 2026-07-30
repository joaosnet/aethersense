use aethersense_firmware_rust::config::FirmwareConfig;
use aethersense_firmware_rust::runtime::{
    packet_bus_for_hardware, spawn_udp_consumer, StorageIngressLink,
};
#[cfg(not(all(feature = "esp-idf", target_os = "espidf")))]
use aethersense_firmware_rust::source::run_selected_source;
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use aethersense_firmware_rust::source::run_selected_source_with_resources;
use aethersense_firmware_rust::status_led::{StatusColor, StatusLed};
use aethersense_firmware_rust::transport::UdpTransmitter;

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
fn record_accessory_event(
    storage: &Option<aethersense_firmware_rust::storage::StorageIngress>,
    kind: &str,
    data: serde_json::Value,
) {
    let Some(storage) = storage else { return };
    let timestamp_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let _ = storage.try_push(aethersense_firmware_rust::storage::StorageRecord::Event(
        aethersense_firmware_rust::storage::FirmwareEvent {
            timestamp_ms,
            kind: kind.to_string(),
            data,
        },
    ));
}

fn compiled_hardware_profile() -> &'static str {
    if cfg!(aethersense_esp32c5) {
        "esp32c5"
    } else if cfg!(aethersense_esp32c6) {
        "esp32c6"
    } else if cfg!(aethersense_esp32_classic) {
        "classic"
    } else {
        "unknown"
    }
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
fn spawn_relay_runtime(
    profile: &aethersense_firmware_rust::profile::DeviceProfile,
    nvs: esp_idf_svc::nvs::EspDefaultNvsPartition,
    outbound: aethersense_firmware_rust::mqtt::AccessoryOutbound,
    boot_id: String,
    storage: Option<aethersense_firmware_rust::storage::StorageIngress>,
) -> Option<(
    std::sync::mpsc::SyncSender<aethersense_firmware_rust::mqtt::CommandEnvelope>,
    std::thread::JoinHandle<()>,
)> {
    if profile.enabled_relay_count() == 0 {
        return None;
    }
    use aethersense_firmware_rust::mqtt::{
        AccessoryCommand, AccessoryStateDocument, CommandAck, MqttOutbound,
    };
    use aethersense_firmware_rust::relay::esp::{EspRelayHardware, NvsRelayStateStore};
    use aethersense_firmware_rust::relay::RelayManager;
    use std::collections::BTreeMap;

    let config_hash = match profile.config_hash() {
        Ok(hash) => hash,
        Err(error) => {
            log::error!("relay config hash failed: {error}");
            return None;
        }
    };
    let store = match NvsRelayStateStore::new(nvs) {
        Ok(store) => store,
        Err(error) => {
            log::error!("relay NVS init failed: {error}");
            return None;
        }
    };
    let now_ms = unsafe { (esp_idf_sys::esp_timer_get_time() as u64) / 1_000 };
    let manager = match RelayManager::new(
        EspRelayHardware::new(),
        store,
        &profile.peripherals.relays,
        config_hash,
        now_ms,
    ) {
        Ok(manager) => manager,
        Err(error) => {
            log::error!("relay boot-safe initialization failed: {error}");
            return None;
        }
    };
    let (commands, command_rx) =
        std::sync::mpsc::sync_channel::<aethersense_firmware_rust::mqtt::CommandEnvelope>(16);
    let handle = std::thread::Builder::new()
        .name("aether-relay".to_string())
        .stack_size(8 * 1024)
        .spawn(move || {
            let mut manager = manager;
            loop {
                let now_ms = unsafe { (esp_idf_sys::esp_timer_get_time() as u64) / 1_000 };
                if let Err(error) = manager.restore_if_due(now_ms) {
                    log::warn!("relay restore failed: {error}");
                }
                if let Err(error) = manager.tick(now_ms) {
                    log::warn!("relay pulse tick failed: {error}");
                }
                match command_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                    Ok(command) => {
                        let result = match &command.command {
                            AccessoryCommand::RelaySet { relay_id, state } => {
                                manager.set(relay_id, *state).map(|_| ())
                            }
                            AccessoryCommand::RelayPulse {
                                relay_id,
                                duration_ms,
                            } => manager.pulse(relay_id, *duration_ms, now_ms).map(|_| ()),
                            _ => continue,
                        };
                        let succeeded = result.is_ok();
                        let ack = match result {
                            Ok(()) => CommandAck::completed(&command, &boot_id),
                            Err(error) => CommandAck::error(&command, &boot_id, error.to_string()),
                        };
                        let _ = outbound.try_send(MqttOutbound::Ack(ack));
                        if succeeded {
                            match &command.command {
                                AccessoryCommand::RelaySet { relay_id, state } => {
                                    record_accessory_event(
                                        &storage,
                                        "relay.set",
                                        serde_json::json!({
                                            "relay_id": relay_id,
                                            "state": state,
                                            "request_id": command.request_id,
                                        }),
                                    );
                                }
                                AccessoryCommand::RelayPulse {
                                    relay_id,
                                    duration_ms,
                                } => {
                                    record_accessory_event(
                                        &storage,
                                        "relay.pulse",
                                        serde_json::json!({
                                            "relay_id": relay_id,
                                            "duration_ms": duration_ms,
                                            "request_id": command.request_id,
                                        }),
                                    );
                                }
                                _ => {}
                            }
                        }
                        let state = AccessoryStateDocument {
                            schema: aethersense_firmware_rust::mqtt::STATE_SCHEMA.to_string(),
                            boot_id: boot_id.clone(),
                            accessories: manager
                                .states()
                                .iter()
                                .map(|(id, state)| {
                                    (id.clone(), serde_json::json!({"state": state}))
                                })
                                .collect::<BTreeMap<_, _>>(),
                        };
                        let _ = outbound.try_send(MqttOutbound::State(state));
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        })
        .map_err(|error| log::error!("relay worker spawn failed: {error}"))
        .ok()?;
    Some((commands, handle))
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
fn spawn_ir_runtime(
    profile: &aethersense_firmware_rust::profile::DeviceProfile,
    outbound: aethersense_firmware_rust::mqtt::AccessoryOutbound,
    boot_id: String,
    status_led: StatusLed,
    storage: Option<aethersense_firmware_rust::storage::StorageIngress>,
) -> Option<(
    std::sync::mpsc::SyncSender<aethersense_firmware_rust::mqtt::CommandEnvelope>,
    std::thread::JoinHandle<()>,
)> {
    use aethersense_firmware_rust::ir::esp::EspRmtIrBackend;
    use aethersense_firmware_rust::ir::{IrEvent, IrManager, RawIrCode, RAW_IR_SCHEMA};
    use aethersense_firmware_rust::mqtt::{
        AccessoryCommand, CommandAck, DeviceEvent, LearnedIrCode, MqttOutbound,
    };
    use std::collections::BTreeMap;

    let tx_gpio = profile
        .peripherals
        .ir_tx
        .enabled
        .then_some(profile.peripherals.ir_tx.gpio)
        .flatten();
    let rx_gpio = profile
        .peripherals
        .ir_rx
        .enabled
        .then_some(profile.peripherals.ir_rx.gpio)
        .flatten();
    if tx_gpio.is_none() && rx_gpio.is_none() {
        return None;
    }
    let defaults = profile.peripherals.ir_tx.clone();
    let idle_default = profile.peripherals.ir_rx.idle_threshold_us;
    let device_id = profile.identity.device_id.clone().unwrap_or_default();
    let (commands, command_rx) =
        std::sync::mpsc::sync_channel::<aethersense_firmware_rust::mqtt::CommandEnvelope>(16);
    let handle = std::thread::Builder::new()
        .name("aether-ir".to_string())
        .stack_size(10 * 1024)
        .spawn(move || {
            // ESP-IDF's RMT drivers are intentionally !Send; construct and own
            // them on the worker thread instead of moving raw driver handles
            // across a pthread boundary.
            let backend = match EspRmtIrBackend::new(tx_gpio, rx_gpio) {
                Ok(backend) => backend,
                Err(error) => {
                    log::error!("IR RMT initialization failed: {error}");
                    return;
                }
            };
            log::warn!(
                "IR runtime ready: rx_gpio={} tx_gpio={} carrier_hz={} duty_percent={}",
                rx_gpio
                    .map(|gpio| gpio.to_string())
                    .unwrap_or_else(|| "disabled".to_string()),
                tx_gpio
                    .map(|gpio| gpio.to_string())
                    .unwrap_or_else(|| "disabled".to_string()),
                defaults.carrier_hz,
                defaults.duty_percent,
            );
            let mut manager = IrManager::new(backend, 8);
            let mut pending = BTreeMap::new();
            let mut learned_names = BTreeMap::new();
            loop {
                match command_rx.recv_timeout(std::time::Duration::from_millis(20)) {
                    Ok(command) => {
                        let completes_immediately =
                            matches!(&command.command, AccessoryCommand::IrLearnCancel);
                        let mut cancelled_request_id = None;
                        let result = match &command.command {
                            AccessoryCommand::IrLearnStart {
                                code_id,
                                name,
                                idle_threshold_us,
                                ..
                            } => {
                                let id = code_id
                                    .clone()
                                    .unwrap_or_else(|| command.request_id.clone());
                                learned_names.insert(
                                    command.request_id.clone(),
                                    name.clone().unwrap_or_else(|| id.clone()),
                                );
                                manager
                                    .start_learning(
                                        command.request_id.clone(),
                                        id,
                                        idle_threshold_us.unwrap_or(idle_default),
                                        defaults.carrier_hz,
                                        defaults.duty_percent,
                                    )
                                    .map(|_| ())
                            }
                            AccessoryCommand::IrLearnCancel => {
                                manager.cancel_learning().map(|event| {
                                    if let Some(IrEvent::LearningCancelled { request_id }) = event {
                                        cancelled_request_id = Some(request_id);
                                    }
                                })
                            }
                            AccessoryCommand::IrSend {
                                code_id,
                                carrier_hz,
                                duty_percent,
                                sequence_us,
                                repeats,
                            } => {
                                let queued = manager.queue_send(
                                    command.request_id.clone(),
                                    RawIrCode {
                                        schema: RAW_IR_SCHEMA.to_string(),
                                        code_id: code_id.clone(),
                                        carrier_hz: *carrier_hz,
                                        duty_percent: *duty_percent,
                                        sequence_us: sequence_us.clone(),
                                    },
                                    *repeats,
                                );
                                queued
                            }
                            _ => continue,
                        };
                        match result {
                            Ok(()) => {
                                if completes_immediately {
                                    let _ = outbound.try_send(MqttOutbound::Ack(
                                        CommandAck::completed_with_state(
                                            &command,
                                            &boot_id,
                                            serde_json::json!({"event": "ir.learning_cancelled"}),
                                        ),
                                    ));
                                    if let Some(cancelled_request_id) = cancelled_request_id {
                                        record_accessory_event(
                                            &storage,
                                            "ir.learning_cancelled",
                                            serde_json::json!({
                                                "request_id": cancelled_request_id,
                                            }),
                                        );
                                        if let Some(cancelled) =
                                            pending.remove(&cancelled_request_id)
                                        {
                                            let _ = outbound.try_send(MqttOutbound::Ack(
                                                CommandAck::completed_with_state(
                                                    &cancelled,
                                                    &boot_id,
                                                    serde_json::json!({
                                                        "event": "ir.learning_cancelled",
                                                    }),
                                                ),
                                            ));
                                        }
                                    }
                                } else {
                                    pending.insert(command.request_id.clone(), command);
                                }
                            }
                            Err(error) => {
                                let ack = CommandAck::error(&command, &boot_id, error.to_string());
                                let _ = outbound.try_send(MqttOutbound::Ack(ack));
                            }
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                }
                for event in manager.tick() {
                    match event {
                        IrEvent::Learned { request_id, code } => {
                            status_led.pulse(StatusColor::IrRx, 180);
                            let learned_state = serde_json::json!({
                                "event": "ir.learned",
                                "code": code.clone(),
                            });
                            let name = learned_names
                                .remove(&request_id)
                                .unwrap_or_else(|| code.code_id.clone());
                            record_accessory_event(
                                &storage,
                                "ir.learned",
                                serde_json::json!({
                                    "request_id": request_id,
                                    "code_id": code.code_id,
                                    "carrier_hz": code.carrier_hz,
                                    "duty_percent": code.duty_percent,
                                    "sequence_length": code.sequence_us.len(),
                                }),
                            );
                            let learned = DeviceEvent::IrLearned {
                                code: LearnedIrCode {
                                    id: Some(code.code_id),
                                    device_id: device_id.clone(),
                                    name,
                                    carrier_hz: code.carrier_hz,
                                    duty_cycle: f32::from(code.duty_percent) / 100.0,
                                    sequence_us: code.sequence_us,
                                },
                            };
                            let _ = outbound.try_send(MqttOutbound::Event(learned));
                            if let Some(command) = pending.remove(&request_id) {
                                let _ = outbound.try_send(MqttOutbound::Ack(
                                    CommandAck::completed_with_state(
                                        &command,
                                        &boot_id,
                                        learned_state,
                                    ),
                                ));
                            }
                        }
                        IrEvent::Sent {
                            request_id,
                            code_id,
                        } => {
                            // Signal TX only after the backend confirms the RMT
                            // transaction; queue acceptance alone is not emission.
                            status_led.pulse(StatusColor::IrTx, 180);
                            record_accessory_event(
                                &storage,
                                "ir.sent",
                                serde_json::json!({
                                    "request_id": request_id,
                                    "code_id": code_id,
                                }),
                            );
                            if let Some(command) = pending.remove(&request_id) {
                                let _ = outbound.try_send(MqttOutbound::Ack(
                                    CommandAck::completed_with_state(
                                        &command,
                                        &boot_id,
                                        serde_json::json!({
                                            "event": "ir.sent",
                                            "code_id": code_id,
                                        }),
                                    ),
                                ));
                            }
                        }
                        IrEvent::LearningCancelled { request_id } => {
                            record_accessory_event(
                                &storage,
                                "ir.learning_cancelled",
                                serde_json::json!({"request_id": request_id}),
                            );
                            if let Some(command) = pending.remove(&request_id) {
                                let _ = outbound.try_send(MqttOutbound::Ack(
                                    CommandAck::completed(&command, &boot_id),
                                ));
                            }
                        }
                        IrEvent::Failed {
                            request_id,
                            message,
                        } => {
                            if let Some(command) = pending.remove(&request_id) {
                                let _ = outbound.try_send(MqttOutbound::Ack(CommandAck::error(
                                    &command, &boot_id, message,
                                )));
                            }
                        }
                        IrEvent::LearningStarted { .. } => {}
                    }
                }
            }
        })
        .map_err(|error| log::error!("IR worker spawn failed: {error}"))
        .ok()?;
    Some((commands, handle))
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
fn supervise_degraded_runtime() -> ! {
    // Keep app_main, provisioning, the status LED and all owned ESP resources
    // alive after a recoverable startup failure. Returning would drop those
    // resources while detached IDF tasks continue running.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

fn main() {
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    esp_idf_sys::link_patches();

    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    esp_idf_svc::log::EspLogger::initialize_default();

    log::warn!(
        "AetherSense firmware boot: compiled_profile={} target_os={} arch={} version={}",
        compiled_hardware_profile(),
        std::env::consts::OS,
        std::env::consts::ARCH,
        env!("CARGO_PKG_VERSION")
    );

    // Atraso inicial para garantir que o espflash consiga colocar a placa em modo
    // bootloader no caso de loops de panic. O C5 rev v1.0 é mais sensível ao WDT
    // nesse ponto da inicialização, então usa uma janela menor.
    #[cfg(all(feature = "esp-idf", target_os = "espidf", aethersense_esp32c5))]
    std::thread::sleep(std::time::Duration::from_millis(200));

    #[cfg(not(all(feature = "esp-idf", target_os = "espidf", aethersense_esp32c5)))]
    std::thread::sleep(std::time::Duration::from_secs(3));

    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    log::set_max_level(log::LevelFilter::Warn);

    // Suprime os warnings internos do driver WiFi (como 'unknown csi bug!')
    // que poluem o console serial e fazem o script Python abortar a execução.
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    unsafe {
        esp_idf_sys::esp_log_level_set(
            b"wifi\0".as_ptr() as *const _,
            esp_idf_sys::esp_log_level_t_ESP_LOG_ERROR,
        );
    }

    #[allow(unused_mut)]
    let (mut config, mut active_profile) = FirmwareConfig::from_env_and_profile()
        .unwrap_or_else(|err| panic!("invalid AETHERSENSE_DEVICE_PROFILE_JSON: {err}"));

    // Take all process-wide ESP resources in one place. Wi-Fi receives the
    // modem/event loop/NVS handles from this owner and never calls `take()`.
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    let mut bootstrap = aethersense_firmware_rust::bootstrap::BootstrapResources::take()
        .unwrap_or_else(|err| panic!("failed to take ESP bootstrap resources: {err}"));

    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    let mut mqtt_password = aethersense_firmware_rust::provisioning::SecretString::default();
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    let mut server_bearer_token = aethersense_firmware_rust::provisioning::SecretString::default();

    // A committed A/B NVS profile overrides build-time runtime values. Invalid
    // or interrupted staged writes are ignored by the atomic store, preserving
    // the previous active configuration.
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    if let Ok(Some(stored)) =
        aethersense_firmware_rust::provisioning::esp_nvs::load_active(bootstrap.default_nvs.clone())
    {
        let target_matches =
            stored.config.profile.target.chip.hardware_profile() == compiled_hardware_profile();
        let build_matches = option_env!("AETHERSENSE_BUILD_HASH").is_none_or(|embedded| {
            stored.config.build_hash.eq_ignore_ascii_case(embedded)
        });
        if target_matches && build_matches {
            log::warn!(
                "Loaded provisioned profile generation={} config_hash={}",
                stored.generation,
                stored.config_hash
            );
            config.apply_provisioned_config(&stored.config);
            mqtt_password = stored.config.secrets.mqtt_password.clone();
            server_bearer_token = stored.config.secrets.server_bearer_token.clone();
            active_profile = Some(stored.config.profile);
        } else {
            log::error!(
                "Ignoring provisioned profile incompatible with compiled target/build: profile={} compiled={}",
                stored.config.profile.target.chip.hardware_profile(),
                compiled_hardware_profile()
            );
        }
    }

    if let Some(profile) = active_profile.as_ref() {
        let hash = profile
            .config_hash()
            .unwrap_or_else(|_| "invalid".to_string());
        log::warn!(
            "AetherSense device profile: schema={} name={} device_id={} config_hash={}",
            profile.schema,
            profile.identity.name,
            profile.identity.device_id.as_deref().unwrap_or("unpaired"),
            hash
        );
    }

    log::warn!(
        "AetherSense config: hardware_profile={} udp_host={} udp_port={} send_frequency_hz={} queue_capacity={} wifi_band={} wifi_prefer_ht40={}",
        config.hardware_profile,
        config.udp_host,
        config.udp_port,
        config.send_frequency_hz,
        config.queue_capacity,
        config.wifi_band,
        config.wifi_prefer_ht40
    );

    let mut status_led = StatusLed::new(&config);
    status_led.set(StatusColor::Boot);

    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    let boot_id = aethersense_firmware_rust::mqtt::esp::generate_boot_id();
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    let local_commissioning =
        aethersense_firmware_rust::mqtt::AccessoryCommissioning::new(boot_id.clone());

    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    let _provisioning_server = {
        let mut features = active_profile
            .as_ref()
            .map(|profile| profile.target.feature_set.clone())
            .unwrap_or_else(|| vec!["csi".to_string()]);
        for required in [
            "serial-commissioning-v1",
            "serial-hardware-validation-v1",
            "runtime-diagnostic-v1",
            "safe-reboot-diagnostic-v1",
        ] {
            if !features.iter().any(|feature| feature == required) {
                features.push(required.to_string());
            }
        }
        if config.rgb_led_enabled
            && !features
                .iter()
                .any(|feature| feature == "status-led-diagnostic-v1")
        {
            features.push("status-led-diagnostic-v1".to_string());
        }
        aethersense_firmware_rust::provisioning::esp_nvs::spawn_serial_server(
            bootstrap.default_nvs.clone(),
            option_env!("AETHERSENSE_BUILD_HASH")
                .unwrap_or(env!("CARGO_PKG_VERSION"))
                .to_string(),
            features,
            status_led.clone(),
            local_commissioning.clone(),
        )
        .map_err(|error| log::warn!("provisioning serial server disabled: {error}"))
        .ok()
    };

    // Wi-Fi owns the remaining bootstrap bundle. Preserve only the handles
    // needed by optional accessory runtimes, which are deliberately started
    // after the network/CSI source has reserved its heap.
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    let accessory_nvs = bootstrap.default_nvs.clone();
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    let storage_spi2 = bootstrap.take_spi2();

    // Local IR/relay commissioning must not depend on DHCP or on the broker.
    // Reserve their small bounded queues before the blocking Wi-Fi association
    // and attach them to USB immediately. SD remains after the network hot
    // path because FAT mounting temporarily fragments the C5 heap.
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    let mqtt_enabled = active_profile
        .as_ref()
        .is_some_and(|profile| profile.mqtt.enabled);
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    let outbound_queue_capacity =
        aethersense_firmware_rust::runtime::effective_outbound_queue_capacity(
            aethersense_firmware_rust::runtime::DEFAULT_OUTBOUND_QUEUE_CAPACITY,
            &config.hardware_profile,
            mqtt_enabled,
        );
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    let (outbound_tx, outbound_rx) = std::sync::mpsc::sync_channel(outbound_queue_capacity);
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    let accessory_outbound = aethersense_firmware_rust::mqtt::AccessoryOutbound::new(
        outbound_tx,
        local_commissioning.clone(),
    );
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    let relay_runtime = active_profile.as_ref().and_then(|profile| {
        spawn_relay_runtime(
            profile,
            accessory_nvs.clone(),
            accessory_outbound.clone(),
            boot_id.clone(),
            None,
        )
    });
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    let ir_runtime = active_profile.as_ref().and_then(|profile| {
        spawn_ir_runtime(
            profile,
            accessory_outbound.clone(),
            boot_id.clone(),
            status_led.clone(),
            None,
        )
    });
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    let accessory_dispatch = aethersense_firmware_rust::mqtt::AccessoryDispatch::new(
        relay_runtime.as_ref().map(|(commands, _)| commands.clone()),
        ir_runtime.as_ref().map(|(commands, _)| commands.clone()),
    );
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    if let Err(error) = local_commissioning.attach(accessory_dispatch.clone()) {
        log::error!("early USB accessory commissioning disabled: {error}");
    } else {
        log::warn!("local IR/relay commissioning ready before Wi-Fi");
    }

    let (ingress, egress) =
        packet_bus_for_hardware(config.queue_capacity, &config.hardware_profile);
    let dropped_counter = ingress.dropped_counter();

    status_led.set(StatusColor::WifiConnecting);
    log::warn!(
        "Starting Wi-Fi and CSI source for {}",
        config.hardware_profile
    );
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    let source_runtime = run_selected_source_with_resources(&config, &ingress, bootstrap);
    #[cfg(not(all(feature = "esp-idf", target_os = "espidf")))]
    let source_runtime = run_selected_source(&config, &ingress);

    if !source_runtime.is_ready() {
        status_led.set(StatusColor::Error);
        log::error!(
            "Wi-Fi/CSI source did not become ready; network consumers and accessory runtimes remain disabled while provisioning stays available"
        );
        #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
        supervise_degraded_runtime();
        #[cfg(not(all(feature = "esp-idf", target_os = "espidf")))]
        return;
    }
    log::warn!("Wi-Fi and CSI source initialized");
    status_led.set(StatusColor::WifiConnected);

    // SDSPI/FAT initialization temporarily consumes and fragments internal
    // heap. Stop new CSI payload allocations until all optional peripheral
    // workers have either completed their startup handshake or failed
    // recoverably. The callback stays installed and resumes below.
    // The source installs the callback but deliberately leaves capture off.
    // It is enabled only after every optional runtime completes its startup
    // handshake, eliminating the SDSPI/CSI allocation race entirely.
    let csi_paused_for_peripheral_startup = true;
    log::warn!("CSI capture deferred while peripheral runtimes initialize");

    let destination = if config.udp_host == "255.255.255.255" {
        if let Some(subnet_ip) = source_runtime.subnet_broadcast_ip {
            log::warn!(
                "Using subnet broadcast IP for UDP destination: {}",
                subnet_ip
            );
            std::net::SocketAddrV4::new(subnet_ip, config.udp_port)
        } else {
            config.udp_destination()
        }
    } else {
        config.udp_destination()
    };
    log::warn!("UDP destination resolved to {}", destination);

    // Reserve the lwIP socket before optional peripheral workers consume the
    // remaining heap. This constructor is unreachable when the source is not
    // ready, which avoids calling into an uninitialized TCP/IP mailbox.
    let transmitter = match UdpTransmitter::from_destination(destination) {
        Ok(transmitter) => transmitter,
        Err(err) => {
            status_led.set(StatusColor::Error);
            log::error!("failed to create UDP transmitter; provisioning remains available: {err}");
            #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
            supervise_degraded_runtime();
            #[cfg(not(all(feature = "esp-idf", target_os = "espidf")))]
            return;
        }
    };

    // Reserve the consumer task before SD/FAT, relay, IR or MQTT can consume
    // the remaining task heap. Storage is attached later if its startup
    // handshake confirms that a card was mounted successfully.
    let last_send_secs = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let transmitted_counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let storage_link = StorageIngressLink::default();
    let consumer = match spawn_udp_consumer(
        egress,
        transmitter,
        dropped_counter.clone(),
        config.send_interval_millis(),
        last_send_secs.clone(),
        transmitted_counter.clone(),
        storage_link.clone(),
    ) {
        Ok(consumer) => consumer,
        Err(error) => {
            status_led.set(StatusColor::Error);
            log::error!(
                "UDP consumer task could not start; provisioning remains available: {error}"
            );
            #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
            supervise_degraded_runtime();
            #[cfg(not(all(feature = "esp-idf", target_os = "espidf")))]
            return;
        }
    };
    log::warn!("UDP streaming task ready");

    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    if let Err(error) = local_commissioning.attach_runtime_diagnostics(
        last_send_secs.clone(),
        dropped_counter.clone(),
        transmitted_counter.clone(),
    ) {
        log::warn!("runtime diagnostics unavailable over USB: {error}");
    }

    // Reserve the health task while the network hot path still has heap
    // headroom. Optional SD/FAT, relay, IR and MQTT workers are deliberately
    // created only after both long-lived network tasks are alive.
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    let _health_monitor = match aethersense_firmware_rust::health::spawn_health_monitor(
        last_send_secs.clone(),
        &config,
        status_led.clone(),
    ) {
        Ok(monitor) => monitor,
        Err(error) => {
            status_led.set(StatusColor::Error);
            log::error!(
                "health monitor task could not start; provisioning and UDP remain available: {error}"
            );
            supervise_degraded_runtime();
        }
    };

    // Only allocate accessory queues/stacks after lwIP, Wi-Fi and CSI own the
    // memory they need. A source failure takes the degraded supervisor path
    // above and therefore cannot reach any socket constructor.
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    if outbound_queue_capacity
        != aethersense_firmware_rust::runtime::DEFAULT_OUTBOUND_QUEUE_CAPACITY
    {
        log::warn!(
            "accessory outbound queue capped: hardware_profile={} mqtt_enabled={} requested={} effective={}",
            config.hardware_profile,
            mqtt_enabled,
            aethersense_firmware_rust::runtime::DEFAULT_OUTBOUND_QUEUE_CAPACITY,
            outbound_queue_capacity
        );
    }

    // MQTT creates an ESP event loop and two long-lived tasks. Allocate those
    // while the C5 heap is still contiguous, then mount FAT/SDSPI. Storage is
    // attached to the shared dispatch below, so commands become available to
    // both MQTT and USB without restarting either transport.
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    let _mqtt_runtime = active_profile
        .as_ref()
        .filter(|profile| profile.mqtt.enabled)
        .and_then(|profile| {
            aethersense_firmware_rust::mqtt::esp::spawn_mqtt_runtime(
                profile.clone(),
                mqtt_password,
                boot_id.clone(),
                accessory_dispatch.clone(),
                outbound_rx,
            )
            .map_err(|error| log::error!("MQTT runtime disabled: {error}"))
            .ok()
        });

    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    let storage_runtime = if let Some(profile) = active_profile
        .as_ref()
        .filter(|profile| profile.storage.sd.enabled)
    {
        match storage_spi2 {
            Some(spi2) => {
                let identity = aethersense_firmware_rust::storage::esp::SdRuntimeIdentity {
                    device_id: profile.identity.device_id.clone().unwrap_or_default(),
                    build_hash: option_env!("AETHERSENSE_BUILD_HASH")
                        .unwrap_or(env!("CARGO_PKG_VERSION"))
                        .to_string(),
                    config_hash: profile
                        .config_hash()
                        .unwrap_or_else(|_| "invalid".to_string()),
                    boot_id: boot_id.clone(),
                };
                aethersense_firmware_rust::storage::esp::spawn_sd_runtime(
                    spi2,
                    profile.storage.sd.clone(),
                    profile.storage.recording.clone(),
                    profile.sync.clone(),
                    server_bearer_token,
                    identity,
                    accessory_outbound.clone(),
                    status_led.clone(),
                )
                .map_err(|error| log::error!("SD runtime disabled: {error}"))
                .ok()
            }
            None => {
                log::error!("SD runtime disabled: SPI2 is unavailable");
                None
            }
        }
    } else {
        None
    };

    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    let storage_ingress = storage_runtime
        .as_ref()
        .map(|runtime| runtime.ingress.clone());
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    storage_link.attach(
        storage_runtime
            .as_ref()
            .filter(|_| {
                active_profile
                    .as_ref()
                    .is_some_and(|profile| profile.storage.recording.raw_csi_enabled)
            })
            .map(|runtime| (runtime.ingress.clone(), runtime.raw_csi_gate.clone())),
    );
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    if let Err(error) = accessory_dispatch.attach_storage(
        storage_runtime
            .as_ref()
            .map(|runtime| runtime.commands.clone()),
    ) {
        log::error!("storage command dispatch disabled: {error}");
    } else {
        log::warn!("USB/MQTT accessory commissioning ready");
    }

    if csi_paused_for_peripheral_startup {
        match source_runtime.set_capture_enabled(true) {
            Ok(()) => {
                status_led.set(StatusColor::Streaming);
                log::warn!("CSI capture resumed; UDP streaming active");
            }
            Err(code) => {
                status_led.set(StatusColor::Error);
                log::error!(
                    "CSI capture could not be resumed after peripheral startup (esp_err_t={code})"
                );
            }
        }
    } else {
        status_led.set(StatusColor::Streaming);
        log::warn!("UDP streaming active");
    }

    // ESP-IDF deletes only `main_task` when `app_main` returns; detached pthreads
    // continue, but the owned Wi-Fi/profile/channel resources above would be
    // dropped and their workers would gradually disconnect. Keep one producer
    // alive so the healthy UDP consumer remains the blocking supervisor.
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    let runtime_ingress_keepalive = ingress;
    #[cfg(not(all(feature = "esp-idf", target_os = "espidf")))]
    drop(ingress);

    let stats = match consumer.join() {
        Ok(stats) => stats,
        Err(_) => {
            status_led.set(StatusColor::Error);
            log::error!("UDP consumer thread panicked; provisioning remains available");
            #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
            supervise_degraded_runtime();
            #[cfg(not(all(feature = "esp-idf", target_os = "espidf")))]
            return;
        }
    };

    log::warn!(
        "runtime complete: produced={} source_accepted={} source_dropped={} transmitted={} consumer_dropped={}",
        source_runtime.stats.produced,
        source_runtime.stats.accepted,
        source_runtime.stats.dropped,
        stats.transmitted,
        stats.dropped
    );

    // Make the keepalive lifetime explicit. In a healthy ESP runtime `join()`
    // never returns; if the consumer ever gains a graceful shutdown path, this
    // remains the single point that releases the final producer.
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    drop(runtime_ingress_keepalive);
}
