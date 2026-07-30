//! Protocol-agnostic IR learning/replay manager.
//!
//! `sequence_us` alternates mark/space durations and always starts with a mark.
//! Hardware-specific RMT code implements [`IrBackend`]; the manager itself is
//! bounded and contains no blocking waits, so it can run below the CSI task.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

pub const RAW_IR_SCHEMA: &str = "aethersense.ir_raw.v1";
pub const DEFAULT_IR_CARRIER_HZ: u32 = 38_000;
pub const DEFAULT_IR_DUTY_PERCENT: u8 = 33;
pub const MAX_IR_DURATIONS: usize = 2048;
pub const MAX_IR_REPEATS: u8 = 20;
pub const MAX_IR_FRAME_DURATION_US: u64 = 5_000_000;
pub const MAX_IR_COMMAND_DURATION_US: u64 = 30_000_000;
pub const ESP32_C5_IR_RX_MEMORY_BLOCK_SYMBOLS: usize = 48;

const _: () =
    assert!(ESP32_C5_IR_RX_MEMORY_BLOCK_SYMBOLS <= crate::rmt_tx::ESP32_C5_RMT_SYMBOLS_PER_CHANNEL);

#[cfg(all(feature = "esp-idf", target_os = "espidf", aethersense_esp32c5))]
const IR_RX_MEMORY_BLOCK_SYMBOLS: usize = ESP32_C5_IR_RX_MEMORY_BLOCK_SYMBOLS;
#[cfg(all(feature = "esp-idf", target_os = "espidf", not(aethersense_esp32c5)))]
const IR_RX_MEMORY_BLOCK_SYMBOLS: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawIrCode {
    #[serde(default = "default_schema")]
    pub schema: String,
    pub code_id: String,
    #[serde(default = "default_carrier")]
    pub carrier_hz: u32,
    #[serde(default = "default_duty")]
    pub duty_percent: u8,
    pub sequence_us: Vec<u32>,
}

impl RawIrCode {
    pub fn validate(&self) -> Result<(), IrError> {
        if self.schema != RAW_IR_SCHEMA {
            return Err(IrError::InvalidCode(
                "unsupported raw IR schema".to_string(),
            ));
        }
        if self.code_id.trim().is_empty() {
            return Err(IrError::InvalidCode("code_id is required".to_string()));
        }
        if !(20_000..=60_000).contains(&self.carrier_hz) {
            return Err(IrError::InvalidCode(
                "carrier must be 20-60 kHz".to_string(),
            ));
        }
        if !(1..=99).contains(&self.duty_percent) {
            return Err(IrError::InvalidCode(
                "duty percent must be 1-99".to_string(),
            ));
        }
        if self.sequence_us.len() < 2 || self.sequence_us.len() > MAX_IR_DURATIONS {
            return Err(IrError::InvalidCode(format!(
                "sequence must contain 2..={MAX_IR_DURATIONS} durations"
            )));
        }
        if self
            .sequence_us
            .iter()
            .any(|duration| *duration == 0 || *duration > 1_000_000)
        {
            return Err(IrError::InvalidCode(
                "each duration must be within 1..=1000000 us".to_string(),
            ));
        }
        if self.total_duration_us() > MAX_IR_FRAME_DURATION_US {
            return Err(IrError::InvalidCode(format!(
                "raw IR frame must be at most {MAX_IR_FRAME_DURATION_US} us"
            )));
        }
        Ok(())
    }

    pub fn validate_transmission(&self, repeats: u8) -> Result<(), IrError> {
        self.validate()?;
        if repeats == 0 || repeats > MAX_IR_REPEATS {
            return Err(IrError::InvalidCode(format!(
                "repeats must be within 1..={MAX_IR_REPEATS}"
            )));
        }
        if self.total_duration_us().saturating_mul(u64::from(repeats)) > MAX_IR_COMMAND_DURATION_US
        {
            return Err(IrError::InvalidCode(format!(
                "raw IR command must be at most {MAX_IR_COMMAND_DURATION_US} us including repeats"
            )));
        }
        Ok(())
    }

    pub fn total_duration_us(&self) -> u64 {
        self.sequence_us.iter().map(|value| u64::from(*value)).sum()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IrError {
    InvalidCode(String),
    Busy,
    QueueFull,
    Backend(String),
}

impl std::fmt::Display for IrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCode(message) | Self::Backend(message) => f.write_str(message),
            Self::Busy => f.write_str("IR learner is busy"),
            Self::QueueFull => f.write_str("IR transmit queue is full"),
        }
    }
}

impl std::error::Error for IrError {}

pub trait IrBackend {
    fn start_learning(&mut self, idle_threshold_us: u32) -> Result<(), String>;
    fn cancel_learning(&mut self) -> Result<(), String>;
    fn poll_learned(&mut self) -> Result<Option<Vec<u32>>, String>;
    fn transmit(&mut self, code: &RawIrCode, repeats: u8) -> Result<(), String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IrEvent {
    LearningStarted { request_id: String },
    Learned { request_id: String, code: RawIrCode },
    LearningCancelled { request_id: String },
    Sent { request_id: String, code_id: String },
    Failed { request_id: String, message: String },
}

#[derive(Clone, Debug)]
struct PendingSend {
    request_id: String,
    code: RawIrCode,
    repeats: u8,
}

#[derive(Clone, Debug)]
struct Learning {
    request_id: String,
    code_id: String,
    idle_threshold_us: u32,
    carrier_hz: u32,
    duty_percent: u8,
}

pub struct IrManager<B> {
    backend: B,
    sends: VecDeque<PendingSend>,
    queue_capacity: usize,
    learning: Option<Learning>,
}

impl<B: IrBackend> IrManager<B> {
    pub fn new(backend: B, queue_capacity: usize) -> Self {
        Self {
            backend,
            sends: VecDeque::with_capacity(queue_capacity.max(1)),
            queue_capacity: queue_capacity.max(1),
            learning: None,
        }
    }

    pub fn start_learning(
        &mut self,
        request_id: impl Into<String>,
        code_id: impl Into<String>,
        idle_threshold_us: u32,
        carrier_hz: u32,
        duty_percent: u8,
    ) -> Result<IrEvent, IrError> {
        if self.learning.is_some() {
            return Err(IrError::Busy);
        }
        let request_id = request_id.into();
        self.backend
            .start_learning(idle_threshold_us)
            .map_err(IrError::Backend)?;
        self.learning = Some(Learning {
            request_id: request_id.clone(),
            code_id: code_id.into(),
            idle_threshold_us,
            carrier_hz,
            duty_percent,
        });
        Ok(IrEvent::LearningStarted { request_id })
    }

    pub fn cancel_learning(&mut self) -> Result<Option<IrEvent>, IrError> {
        let Some(learning) = self.learning.take() else {
            return Ok(None);
        };
        self.backend.cancel_learning().map_err(IrError::Backend)?;
        Ok(Some(IrEvent::LearningCancelled {
            request_id: learning.request_id,
        }))
    }

    pub fn queue_send(
        &mut self,
        request_id: impl Into<String>,
        code: RawIrCode,
        repeats: u8,
    ) -> Result<(), IrError> {
        code.validate_transmission(repeats)?;
        if self.sends.len() >= self.queue_capacity {
            return Err(IrError::QueueFull);
        }
        self.sends.push_back(PendingSend {
            request_id: request_id.into(),
            code,
            repeats,
        });
        Ok(())
    }

    /// Performs at most one receive poll and one transmit operation.
    pub fn tick(&mut self) -> Vec<IrEvent> {
        let mut events = Vec::with_capacity(2);
        if let Some(learning) = self.learning.as_ref() {
            match self.backend.poll_learned() {
                Ok(Some(sequence_us)) => {
                    // RMT can complete a receive on a lone electrical edge
                    // while the receiver/channel is settling. That is not a
                    // usable IR frame and must not terminate the user's whole
                    // learning window. Re-arm the same bounded session and
                    // wait for at least one mark/space pair.
                    if sequence_us.len() < 2 {
                        if let Err(message) = self
                            .backend
                            .start_learning(learning.idle_threshold_us)
                        {
                            let request_id = learning.request_id.clone();
                            self.learning = None;
                            events.push(IrEvent::Failed {
                                request_id,
                                message: format!(
                                    "IR receiver could not re-arm after a short capture: {message}"
                                ),
                            });
                        }
                        return events;
                    }
                    let learning = self.learning.take().unwrap();
                    let code = RawIrCode {
                        schema: RAW_IR_SCHEMA.to_string(),
                        code_id: learning.code_id,
                        carrier_hz: learning.carrier_hz,
                        duty_percent: learning.duty_percent,
                        sequence_us,
                    };
                    let event = match code.validate() {
                        Ok(()) => IrEvent::Learned {
                            request_id: learning.request_id,
                            code,
                        },
                        Err(error) => IrEvent::Failed {
                            request_id: learning.request_id,
                            message: error.to_string(),
                        },
                    };
                    events.push(event);
                }
                Ok(None) => {}
                Err(message) => {
                    let request_id = learning.request_id.clone();
                    self.learning = None;
                    let message = match self.backend.cancel_learning() {
                        Ok(()) => message,
                        Err(cancel_error) => {
                            format!("{message}; IR receive cancellation failed: {cancel_error}")
                        }
                    };
                    events.push(IrEvent::Failed {
                        request_id,
                        message,
                    });
                }
            }
        }
        if let Some(send) = self.sends.pop_front() {
            events.push(match self.backend.transmit(&send.code, send.repeats) {
                Ok(()) => IrEvent::Sent {
                    request_id: send.request_id,
                    code_id: send.code.code_id,
                },
                Err(message) => IrEvent::Failed {
                    request_id: send.request_id,
                    message,
                },
            });
        }
        events
    }

    pub fn into_backend(self) -> B {
        self.backend
    }
}

fn default_schema() -> String {
    RAW_IR_SCHEMA.to_string()
}
fn default_carrier() -> u32 {
    DEFAULT_IR_CARRIER_HZ
}
fn default_duty() -> u8 {
    DEFAULT_IR_DUTY_PERCENT
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub mod esp {
    use super::*;
    use crate::rmt_tx::{shared_rmt_tx, RmtTxClient, SHARED_RMT_TX_MEMORY_BLOCK_SYMBOLS};
    use core::time::Duration;

    use esp_idf_hal::gpio::{AnyInputPin, AnyOutputPin};
    use esp_idf_hal::rmt::config::{
        CarrierConfig, DutyPercent, MemoryAccess, ReceiveConfig, RxChannelConfig, TransmitConfig,
        TxChannelConfig,
    };
    use esp_idf_hal::rmt::encoder::CopyEncoder;
    use esp_idf_hal::rmt::{
        PinState, Pulse, PulseTicks, RmtChannel, RxChannelDriver, Symbol, TxChannelDriver,
    };
    use esp_idf_hal::units::FromValueType;

    const RX_SYMBOL_CAPACITY: usize = 1_024;
    const MAX_ENCODED_SYMBOLS: usize = 8_192;

    /// ESP-IDF 5.x RMT adapter. A demodulating receiver is assumed active-low,
    /// so RX input inversion makes learned marks consistently high.
    pub struct EspRmtIrBackend {
        tx_gpio: Option<i32>,
        rx: Option<RxChannelDriver<'static>>,
        learning: bool,
        idle_threshold_us: u32,
    }

    impl EspRmtIrBackend {
        pub fn new(tx_gpio: Option<i32>, rx_gpio: Option<i32>) -> Result<Self, String> {
            let rx = rx_gpio
                .map(|gpio| {
                    let pin = unsafe { AnyInputPin::steal(gpio as u8) };
                    let driver = RxChannelDriver::new(
                        pin,
                        &RxChannelConfig {
                            resolution: 1.MHz().into(),
                            memory_access: MemoryAccess::Indirect {
                                memory_block_symbols: IR_RX_MEMORY_BLOCK_SYMBOLS,
                            },
                            invert_in: true,
                            ..Default::default()
                        },
                    )
                    .map_err(|error| error.to_string())?;

                    // Demodulating IR receivers normally idle high. Some bare
                    // receivers and inexpensive modules expose an open-drain
                    // output without a reliable board-level pull-up. RMT owns
                    // the GPIO matrix route at this point, but the GPIO pull
                    // configuration remains independent, so apply a weak
                    // internal pull-up after creating the channel. A module
                    // with a push-pull output continues to override it.
                    esp_idf_sys::esp!(unsafe { esp_idf_sys::gpio_pullup_en(gpio) })
                        .map_err(|error| format!("IR RX GPIO{gpio} pull-up failed: {error}"))?;
                    esp_idf_sys::esp!(unsafe { esp_idf_sys::gpio_pulldown_dis(gpio) })
                        .map_err(|error| format!("IR RX GPIO{gpio} pull-down disable failed: {error}"))?;
                    Ok::<RxChannelDriver<'static>, String>(driver)
                })
                .transpose()?;
            Ok(Self {
                tx_gpio,
                rx,
                learning: false,
                idle_threshold_us: 12_000,
            })
        }

        fn symbols(code: &RawIrCode) -> Result<Vec<Symbol>, String> {
            let mut pulses = Vec::new();
            for (index, duration_us) in code.sequence_us.iter().copied().enumerate() {
                let state = if index % 2 == 0 {
                    PinState::High
                } else {
                    PinState::Low
                };
                let mut remaining = duration_us;
                while remaining > 0 {
                    let ticks = remaining.min(u32::from(PulseTicks::max().ticks())) as u16;
                    pulses.push(Pulse::new(
                        state,
                        PulseTicks::new(ticks).map_err(|error| error.to_string())?,
                    ));
                    remaining -= u32::from(ticks);
                    if pulses.len() / 2 > MAX_ENCODED_SYMBOLS {
                        return Err("IR sequence expands beyond RMT memory limit".to_string());
                    }
                }
            }
            Ok(pulses
                .chunks(2)
                .map(|pair| Symbol::new(pair[0], pair.get(1).copied().unwrap_or_else(Pulse::zero)))
                .collect())
        }

        fn learned_sequence(symbols: &[Symbol]) -> Vec<u32> {
            let mut sequence = Vec::<(PinState, u32)>::new();
            for symbol in symbols {
                for pulse in [symbol.level0(), symbol.level1()] {
                    let duration = u32::from(pulse.ticks.ticks());
                    if duration == 0 {
                        continue;
                    }
                    if sequence.is_empty() && pulse.pin_state == PinState::Low {
                        continue;
                    }
                    if let Some((last_state, last_duration)) = sequence.last_mut() {
                        if *last_state == pulse.pin_state {
                            *last_duration = last_duration.saturating_add(duration);
                            continue;
                        }
                    }
                    sequence.push((pulse.pin_state, duration));
                }
            }
            sequence.into_iter().map(|(_, duration)| duration).collect()
        }
    }

    impl IrBackend for EspRmtIrBackend {
        fn start_learning(&mut self, idle_threshold_us: u32) -> Result<(), String> {
            if self.rx.is_none() {
                return Err("IR receiver is not configured".to_string());
            }
            self.idle_threshold_us = idle_threshold_us.clamp(1_000, 1_000_000);
            self.learning = true;
            Ok(())
        }

        fn cancel_learning(&mut self) -> Result<(), String> {
            self.learning = false;
            if let Some(rx) = self.rx.as_mut() {
                if rx.is_enabled() {
                    rx.disable().map_err(|error| error.to_string())?;
                }
            }
            Ok(())
        }

        fn poll_learned(&mut self) -> Result<Option<Vec<u32>>, String> {
            if !self.learning {
                return Ok(None);
            }
            let rx = self
                .rx
                .as_mut()
                .ok_or_else(|| "IR receiver is not configured".to_string())?;
            let mut symbols = vec![Symbol::default(); RX_SYMBOL_CAPACITY];
            let receive = ReceiveConfig {
                // ESP32-C5's RMT glitch filter accepts at most ~3.187 us at
                // this clock. One microsecond still rejects electrical spikes
                // while preserving the shortest useful demodulated IR pulse.
                signal_range_min: Duration::from_micros(1),
                signal_range_max: Duration::from_micros(u64::from(self.idle_threshold_us)),
                timeout: Some(2),
                ..Default::default()
            };
            match rx.receive(&mut symbols, &receive) {
                Ok(count) => {
                    self.learning = false;
                    let sequence = Self::learned_sequence(&symbols[..count]);
                    Ok(Some(sequence))
                }
                Err(error) if error.code() == esp_idf_sys::ESP_ERR_TIMEOUT => Ok(None),
                Err(error) => Err(error.to_string()),
            }
        }

        fn transmit(&mut self, code: &RawIrCode, repeats: u8) -> Result<(), String> {
            code.validate_transmission(repeats)
                .map_err(|error| error.to_string())?;
            let gpio = self
                .tx_gpio
                .ok_or_else(|| "IR transmitter is not configured".to_string())?;
            let carrier = CarrierConfig {
                frequency: code.carrier_hz.Hz(),
                duty_cycle: DutyPercent::new(f32::from(code.duty_percent) / 100.0)
                    .map_err(|error| error.to_string())?,
                ..Default::default()
            };
            let symbols = Self::symbols(code)?;

            // Keep all repeats contiguous under one lease. The temporary pin
            // token and channel are dropped before the lease wakes the LED.
            let _lease = shared_rmt_tx().acquire(RmtTxClient::Infrared);
            let pin = unsafe { AnyOutputPin::steal(gpio as u8) };
            let mut tx = TxChannelDriver::new(
                pin,
                &TxChannelConfig {
                    resolution: 1.MHz().into(),
                    memory_access: MemoryAccess::Indirect {
                        memory_block_symbols: SHARED_RMT_TX_MEMORY_BLOCK_SYMBOLS,
                    },
                    transaction_queue_depth: 2,
                    ..Default::default()
                },
            )
            .map_err(|error| format!("IR RMT TX initialization failed: {error}"))?;
            tx.apply_carrier(Some(&carrier))
                .map_err(|error| error.to_string())?;
            let transmit = TransmitConfig::default();
            let timeout = Duration::from_micros(code.total_duration_us().saturating_add(250_000));
            for _ in 0..repeats {
                let mut encoder = CopyEncoder::new().map_err(|error| error.to_string())?;
                // SAFETY: encoder and symbols remain alive and unmoved until
                // the bounded wait below. A failed wait disables the channel,
                // cancelling the transfer before these locals are dropped.
                unsafe {
                    tx.start_send(&mut encoder, &symbols, &transmit)
                        .map_err(|error| error.to_string())?;
                }
                if let Err(error) = tx.wait_all_done(Some(timeout)) {
                    if let Err(disable_error) = tx.disable() {
                        log::error!(
                            "IR RMT transfer and cancellation failed ({error}; {disable_error}); restarting before borrowed encoder data can be released"
                        );
                        unsafe { esp_idf_sys::esp_restart() };
                    }
                    return Err(format!("IR RMT transmit timeout/failure: {error}"));
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockBackend {
        learned: Option<Vec<u32>>,
        sent: Vec<(RawIrCode, u8)>,
    }

    impl IrBackend for MockBackend {
        fn start_learning(&mut self, _: u32) -> Result<(), String> {
            Ok(())
        }
        fn cancel_learning(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn poll_learned(&mut self) -> Result<Option<Vec<u32>>, String> {
            Ok(self.learned.take())
        }
        fn transmit(&mut self, code: &RawIrCode, repeats: u8) -> Result<(), String> {
            self.sent.push((code.clone(), repeats));
            Ok(())
        }
    }

    fn code() -> RawIrCode {
        RawIrCode {
            schema: RAW_IR_SCHEMA.to_string(),
            code_id: "power".to_string(),
            carrier_hz: 38_000,
            duty_percent: 33,
            sequence_us: vec![9_000, 4_500, 560, 560],
        }
    }

    #[test]
    fn raw_golden_vector_keeps_exact_timings() {
        let original = code();
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(serde_json::from_str::<RawIrCode>(&json).unwrap(), original);
    }

    #[test]
    fn raw_transmission_duration_is_bounded() {
        let mut too_long_frame = code();
        too_long_frame.sequence_us = vec![1_000_000; 6];
        assert!(too_long_frame.validate().is_err());

        let mut bounded_frame = code();
        bounded_frame.sequence_us = vec![1_000_000; 5];
        assert!(bounded_frame.validate_transmission(6).is_ok());
        assert!(bounded_frame.validate_transmission(7).is_err());
    }

    #[test]
    fn c5_rx_uses_only_one_physical_rmt_memory_block() {
        assert_eq!(ESP32_C5_IR_RX_MEMORY_BLOCK_SYMBOLS, 48);
        assert!(
            ESP32_C5_IR_RX_MEMORY_BLOCK_SYMBOLS <= crate::rmt_tx::ESP32_C5_RMT_SYMBOLS_PER_CHANNEL
        );
    }

    #[test]
    fn queue_is_bounded_and_tick_sends_once() {
        let mut manager = IrManager::new(MockBackend::default(), 1);
        manager.queue_send("r1", code(), 2).unwrap();
        assert_eq!(manager.queue_send("r2", code(), 1), Err(IrError::QueueFull));
        assert!(matches!(manager.tick().as_slice(), [IrEvent::Sent { .. }]));
        assert_eq!(manager.into_backend().sent.len(), 1);
    }

    #[test]
    fn backend_failure_never_emits_sent_event() {
        #[derive(Default)]
        struct FailingBackend;
        impl IrBackend for FailingBackend {
            fn start_learning(&mut self, _idle_threshold_us: u32) -> Result<(), String> {
                Ok(())
            }
            fn cancel_learning(&mut self) -> Result<(), String> {
                Ok(())
            }
            fn poll_learned(&mut self) -> Result<Option<Vec<u32>>, String> {
                Ok(None)
            }
            fn transmit(&mut self, _code: &RawIrCode, _repeats: u8) -> Result<(), String> {
                Err("RMT write failed".to_string())
            }
        }

        let mut manager = IrManager::new(FailingBackend, 1);
        manager.queue_send("r1", code(), 1).unwrap();
        let events = manager.tick();
        assert!(matches!(events.as_slice(), [IrEvent::Failed { .. }]));
        assert!(!events
            .iter()
            .any(|event| matches!(event, IrEvent::Sent { .. })));
    }

    #[test]
    fn receive_failure_stops_learning_and_is_reported_once() {
        #[derive(Default)]
        struct FailingReceiveBackend {
            polls: usize,
            cancels: usize,
        }
        impl IrBackend for FailingReceiveBackend {
            fn start_learning(&mut self, _idle_threshold_us: u32) -> Result<(), String> {
                Ok(())
            }
            fn cancel_learning(&mut self) -> Result<(), String> {
                self.cancels += 1;
                Ok(())
            }
            fn poll_learned(&mut self) -> Result<Option<Vec<u32>>, String> {
                self.polls += 1;
                Err("invalid RMT receive configuration".to_string())
            }
            fn transmit(&mut self, _code: &RawIrCode, _repeats: u8) -> Result<(), String> {
                Ok(())
            }
        }

        let mut manager = IrManager::new(FailingReceiveBackend::default(), 1);
        manager
            .start_learning("rx-1", "power", 12_000, 38_000, 33)
            .unwrap();
        assert!(matches!(
            manager.tick().as_slice(),
            [IrEvent::Failed { request_id, .. }] if request_id == "rx-1"
        ));
        assert!(manager.tick().is_empty());
        let backend = manager.into_backend();
        assert_eq!(backend.polls, 1);
        assert_eq!(backend.cancels, 1);
    }

    #[test]
    fn short_settling_capture_is_ignored_and_learning_rearms() {
        struct ShortThenValidBackend {
            starts: usize,
            polls: usize,
        }
        impl IrBackend for ShortThenValidBackend {
            fn start_learning(&mut self, _idle_threshold_us: u32) -> Result<(), String> {
                self.starts += 1;
                Ok(())
            }
            fn cancel_learning(&mut self) -> Result<(), String> {
                Ok(())
            }
            fn poll_learned(&mut self) -> Result<Option<Vec<u32>>, String> {
                self.polls += 1;
                Ok(Some(if self.polls == 1 {
                    vec![320]
                } else {
                    vec![9_000, 4_500, 560, 560]
                }))
            }
            fn transmit(&mut self, _code: &RawIrCode, _repeats: u8) -> Result<(), String> {
                Ok(())
            }
        }

        let mut manager = IrManager::new(
            ShortThenValidBackend {
                starts: 0,
                polls: 0,
            },
            1,
        );
        manager
            .start_learning("rx-short", "power", 12_000, 38_000, 33)
            .unwrap();
        assert!(manager.tick().is_empty());
        assert!(matches!(
            manager.tick().as_slice(),
            [IrEvent::Learned { request_id, code }]
                if request_id == "rx-short" && code.sequence_us.len() == 4
        ));
        let backend = manager.into_backend();
        assert_eq!(backend.starts, 2);
        assert_eq!(backend.polls, 2);
    }
}
