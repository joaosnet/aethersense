//! Non-blocking status LED with restorable transient overlays.

use crate::config::FirmwareConfig;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusColor {
    Boot,
    WifiConnecting,
    WifiConnected,
    Streaming,
    Provisioning,
    Recording,
    Offline,
    IrRx,
    IrTx,
    Error,
}

impl StatusColor {
    /// Stable names accepted by the local hardware-diagnostics protocol.
    pub fn from_diagnostic_name(name: &str) -> Option<Self> {
        match name {
            "boot" => Some(Self::Boot),
            "wifi_connecting" => Some(Self::WifiConnecting),
            "wifi_connected" => Some(Self::WifiConnected),
            "streaming" => Some(Self::Streaming),
            "provisioning" => Some(Self::Provisioning),
            "recording" => Some(Self::Recording),
            "offline" => Some(Self::Offline),
            "ir_rx" => Some(Self::IrRx),
            "ir_tx" => Some(Self::IrTx),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    pub const fn diagnostic_name(self) -> &'static str {
        match self {
            Self::Boot => "boot",
            Self::WifiConnecting => "wifi_connecting",
            Self::WifiConnected => "wifi_connected",
            Self::Streaming => "streaming",
            Self::Provisioning => "provisioning",
            Self::Recording => "recording",
            Self::Offline => "offline",
            Self::IrRx => "ir_rx",
            Self::IrTx => "ir_tx",
            Self::Error => "error",
        }
    }

    const fn priority(self) -> u8 {
        match self {
            Self::Error => 100,
            Self::IrTx => 80,
            Self::IrRx => 70,
            Self::Provisioning | Self::Recording => 50,
            _ => 10,
        }
    }

    fn rgb(self) -> Color {
        match self {
            Self::Boot => Color::new(255, 160, 0),
            Self::WifiConnecting => Color::new(0, 80, 255),
            Self::WifiConnected | Self::Streaming => Color::new(0, 220, 70),
            Self::Provisioning => Color::new(0, 220, 220),
            Self::Recording => Color::new(155, 70, 255),
            Self::Offline => Color::new(255, 90, 0),
            Self::IrRx => Color::new(255, 230, 90),
            Self::IrTx => Color::new(255, 0, 190),
            Self::Error => Color::new(255, 0, 0),
        }
    }

    const fn profile_key(self) -> &'static str {
        match self {
            Self::Boot => "boot",
            Self::WifiConnecting | Self::Offline => "offline",
            Self::WifiConnected | Self::Streaming => "online",
            Self::Provisioning => "provisioning",
            Self::Recording => "recording",
            Self::IrRx => "ir_rx",
            Self::IrTx => "ir_tx",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Overlay {
    color: StatusColor,
    expires_at_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusLedState {
    base: StatusColor,
    overlay: Option<Overlay>,
}

impl Default for StatusLedState {
    fn default() -> Self {
        Self {
            base: StatusColor::Boot,
            overlay: None,
        }
    }
}

impl StatusLedState {
    pub fn set_base(&mut self, base: StatusColor) {
        self.base = base;
        if base == StatusColor::Error {
            self.overlay = None;
        }
    }

    pub fn pulse(&mut self, color: StatusColor, duration_ms: u64, now_ms: u64) -> bool {
        if self.base == StatusColor::Error {
            return false;
        }
        if self
            .overlay
            .is_some_and(|current| current.color.priority() > color.priority())
        {
            return false;
        }
        self.overlay = Some(Overlay {
            color,
            expires_at_ms: now_ms.saturating_add(duration_ms.max(1)),
        });
        true
    }

    /// An explicit, time-bounded physical test may replace another transient
    /// overlay, but it must never hide a persistent health error.
    pub fn diagnostic_pulse(
        &mut self,
        color: StatusColor,
        duration_ms: u64,
        now_ms: u64,
    ) -> bool {
        if self.base == StatusColor::Error {
            return false;
        }
        self.overlay = Some(Overlay {
            color,
            expires_at_ms: now_ms.saturating_add(duration_ms.max(1)),
        });
        true
    }

    pub fn clear_overlay(&mut self, color: StatusColor) {
        if self.overlay.is_some_and(|overlay| overlay.color == color) {
            self.overlay = None;
        }
    }

    pub fn effective(&mut self, now_ms: u64) -> StatusColor {
        if self
            .overlay
            .is_some_and(|overlay| overlay.expires_at_ms <= now_ms)
        {
            self.overlay = None;
        }
        self.overlay
            .map(|overlay| overlay.color)
            .unwrap_or(self.base)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
struct Color {
    red: u8,
    green: u8,
    blue: u8,
}

const LED_SYMBOLS_PER_COLOR: usize = 25;

/// Plans one ISR-safe encoder chunk. ESP32-C5 exposes 48 symbols per RMT
/// channel, which the driver serves as two 24-symbol ping-pong halves. The
/// WS2812 frame is 25 symbols (reset + 24 data bits), so it must be split while
/// still making progress on every callback.
fn led_symbol_chunk(total: usize, written: usize, free: usize) -> (usize, usize, bool) {
    let remaining = total.saturating_sub(written);
    let count = remaining.min(free);
    (written, count, count == remaining)
}

fn ws2812_bytes(color: Color) -> [u8; 3] {
    [color.green, color.red, color.blue]
}

impl Color {
    const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    fn scaled(self, percent: u8) -> Self {
        let scale = u16::from(percent.min(100));
        Self {
            red: (u16::from(self.red) * scale / 100) as u8,
            green: (u16::from(self.green) * scale / 100) as u8,
            blue: (u16::from(self.blue) * scale / 100) as u8,
        }
    }
}

#[cfg(all(feature = "esp-idf", target_os = "espidf", aethersense_esp32c5))]
mod imp {
    use super::{
        led_symbol_chunk, ws2812_bytes, Color, FirmwareConfig, StatusColor, StatusLedState,
        LED_SYMBOLS_PER_COLOR,
    };
    use crate::rmt_tx::{shared_rmt_tx, RmtTxClient, SHARED_RMT_TX_MEMORY_BLOCK_SYMBOLS};
    use core::time::Duration;
    use std::sync::{Arc, Mutex};

    use esp_idf_hal::gpio::AnyOutputPin;
    use esp_idf_hal::rmt::config::{MemoryAccess, TransmitConfig, TxChannelConfig};
    use esp_idf_hal::rmt::encoder::simple_encoder::{
        EncoderCallback, NotEnoughSpace, SimpleEncoder, SimpleEncoderConfig, SymbolBuffer,
    };
    use esp_idf_hal::rmt::{PinState, RmtChannel, Symbol, TxChannelDriver};
    use esp_idf_hal::units::Hertz;

    const RMT_RESOLUTION_HZ: Hertz = Hertz(10_000_000);
    const T0H: Duration = Duration::from_nanos(350);
    const T0L: Duration = Duration::from_nanos(800);
    const T1H: Duration = Duration::from_nanos(700);
    const T1L: Duration = Duration::from_nanos(600);
    const TRESET: Duration = Duration::from_micros(281);

    #[derive(Clone)]
    pub struct StatusLed {
        state: Option<Arc<Mutex<StatusLedState>>>,
    }

    impl StatusLed {
        pub fn new(config: &FirmwareConfig) -> Self {
            if !config.rgb_led_enabled {
                return Self { state: None };
            }
            let state = Arc::new(Mutex::new(StatusLedState::default()));
            let worker_state = state.clone();
            let gpio = config.rgb_led_gpios.data.unwrap_or(config.rgb_led_gpio);
            let brightness = config.rgb_led_brightness_percent;
            let colors = config.rgb_led_colors.clone();
            let spawned = std::thread::Builder::new()
                .name("aether-status-led".to_string())
                .stack_size(8 * 1024)
                .spawn(move || {
                    let mut displayed = None;
                    let mut failed = None;
                    let mut retry_after_ms = 0;
                    loop {
                        let now_ms = unsafe { (esp_idf_sys::esp_timer_get_time() as u64) / 1_000 };
                        let effective = worker_state
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .effective(now_ms);
                        // Blue breathing indication while associating Wi-Fi.
                        let phase_brightness = if effective == StatusColor::WifiConnecting
                            && (now_ms / 500) % 2 == 0
                        {
                            brightness / 4
                        } else {
                            brightness
                        };
                        let key = (effective, phase_brightness);
                        let changed_since_failure = failed != Some(key);
                        if displayed != Some(key)
                            && (changed_since_failure || now_ms >= retry_after_ms)
                        {
                            let configured = colors
                                .get(effective.profile_key())
                                .and_then(|channels| {
                                    (channels.len() == 3)
                                        .then(|| Color::new(channels[0], channels[1], channels[2]))
                                })
                                .unwrap_or_else(|| effective.rgb());
                            match transmit_color(gpio, configured.scaled(phase_brightness)) {
                                Ok(()) => {
                                    displayed = Some(key);
                                    failed = None;
                                    retry_after_ms = 0;
                                }
                                Err(error) => {
                                    if changed_since_failure {
                                        log::warn!("RGB status LED update failed: {error}");
                                    }
                                    failed = Some(key);
                                    retry_after_ms = now_ms.saturating_add(1_000);
                                }
                            }
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                });
            if let Err(error) = spawned {
                log::warn!("RGB status LED worker failed: {error}");
                Self { state: None }
            } else {
                Self { state: Some(state) }
            }
        }

        /// Coalescing, non-blocking base-state update.
        pub fn set(&mut self, color: StatusColor) {
            if let Some(state) = self.state.as_ref() {
                if let Ok(mut state) = state.try_lock() {
                    state.set_base(color);
                }
            }
        }

        /// Coalescing, non-blocking transient overlay which restores the base.
        pub fn pulse(&self, color: StatusColor, duration_ms: u64) -> bool {
            if let Some(state) = self.state.as_ref() {
                if let Ok(mut state) = state.try_lock() {
                    let now_ms = unsafe { (esp_idf_sys::esp_timer_get_time() as u64) / 1_000 };
                    return state.pulse(color, duration_ms, now_ms);
                }
            }
            false
        }

        /// Reliable scheduling path for an explicit USB hardware test. The LED
        /// worker never sleeps while holding this mutex, so this only waits for
        /// a short in-memory state update and performs no RMT I/O inline.
        pub fn diagnostic_pulse(&self, color: StatusColor, duration_ms: u64) -> bool {
            let Some(state) = self.state.as_ref() else {
                return false;
            };
            let now_ms = unsafe { (esp_idf_sys::esp_timer_get_time() as u64) / 1_000 };
            state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .diagnostic_pulse(color, duration_ms, now_ms)
        }

        pub fn is_available(&self) -> bool {
            self.state.is_some()
        }

        /// Removes a matching operational overlay immediately, restoring the
        /// latest network/health base state.
        pub fn clear(&self, color: StatusColor) {
            if let Some(state) = self.state.as_ref() {
                if let Ok(mut state) = state.try_lock() {
                    state.clear_overlay(color);
                }
            }
        }
    }

    fn transmit_color(gpio: i32, color: Color) -> Result<(), String> {
        // The pin token and driver are created only inside this lease. Local
        // drop order destroys the channel before releasing the next waiter.
        let _lease = shared_rmt_tx().acquire(RmtTxClient::StatusLed);
        let pin = unsafe { AnyOutputPin::steal(gpio as u8) };
        let mut channel = TxChannelDriver::new(
            pin,
            &TxChannelConfig {
                memory_access: MemoryAccess::Indirect {
                    memory_block_symbols: SHARED_RMT_TX_MEMORY_BLOCK_SYMBOLS,
                },
                resolution: RMT_RESOLUTION_HZ,
                transaction_queue_depth: 2,
                ..Default::default()
            },
        )
        .map_err(|error| error.to_string())?;
        let mut encoder = SimpleEncoder::with_config(
            LedEncoder,
            &SimpleEncoderConfig {
                // The callback can always emit one symbol. The default (64)
                // exceeds a C5 ping-pong half and can prevent any callback.
                min_chunk_size: 1,
                ..Default::default()
            },
        )
        .map_err(|error| error.to_string())?;
        let signal = [color];
        let transmit = TransmitConfig::default();
        // SAFETY: encoder and signal remain pinned in this stack frame until
        // the bounded wait completes; on error disable cancels the transfer
        // before either value or the channel is dropped.
        unsafe {
            channel
                .start_send(&mut encoder, &signal, &transmit)
                .map_err(|error| error.to_string())?;
        }
        match channel.wait_all_done(Some(Duration::from_millis(50))) {
            Ok(()) => Ok(()),
            Err(error) => {
                if let Err(disable_error) = channel.disable() {
                    log::error!(
                        "RGB RMT transfer and cancellation failed ({error}; {disable_error}); restarting before borrowed encoder data can be released"
                    );
                    unsafe { esp_idf_sys::esp_restart() };
                }
                Err(format!("RGB status LED transmit timeout/failure: {error}"))
            }
        }
    }

    struct LedEncoder;

    impl EncoderCallback for LedEncoder {
        type Item = Color;

        fn encode(
            &mut self,
            input_data: &[Self::Item],
            buffer: &mut SymbolBuffer<'_>,
        ) -> Result<(), NotEnoughSpace> {
            let total = input_data.len().saturating_mul(LED_SYMBOLS_PER_COLOR);
            let (start, count, done) =
                led_symbol_chunk(total, buffer.position(), buffer.remaining());
            for absolute in start..start + count {
                let color = input_data[absolute / LED_SYMBOLS_PER_COLOR];
                let position = absolute % LED_SYMBOLS_PER_COLOR;
                let symbol = color_symbol(color, position);
                // `count` is capped by `buffer.remaining()`, so this always
                // writes one symbol and guarantees ISR callback progress.
                buffer.write_all(core::slice::from_ref(&symbol))?;
            }
            if done {
                Ok(())
            } else {
                Err(NotEnoughSpace)
            }
        }
    }

    fn color_symbol(color: Color, position: usize) -> Symbol {
        if position == 0 {
            return Symbol::new_half_split(RMT_RESOLUTION_HZ, PinState::Low, PinState::Low, TRESET)
                .unwrap();
        }
        let bit = position - 1;
        let byte = ws2812_bytes(color)[bit / 8];
        let mask = 0x80 >> (bit % 8);
        let zero =
            Symbol::new_with(RMT_RESOLUTION_HZ, PinState::High, T0H, PinState::Low, T0L).unwrap();
        let one =
            Symbol::new_with(RMT_RESOLUTION_HZ, PinState::High, T1H, PinState::Low, T1L).unwrap();
        if byte & mask != 0 {
            one
        } else {
            zero
        }
    }
}

#[cfg(not(all(feature = "esp-idf", target_os = "espidf", aethersense_esp32c5)))]
mod imp {
    use super::{FirmwareConfig, StatusColor};

    #[derive(Clone, Default)]
    pub struct StatusLed;

    impl StatusLed {
        pub fn new(_config: &FirmwareConfig) -> Self {
            Self
        }
        pub fn set(&mut self, _color: StatusColor) {}
        pub fn pulse(&self, _color: StatusColor, _duration_ms: u64) -> bool {
            false
        }
        pub fn diagnostic_pulse(&self, _color: StatusColor, _duration_ms: u64) -> bool {
            false
        }
        pub fn is_available(&self) -> bool {
            false
        }
        pub fn clear(&self, _color: StatusColor) {}
    }
}

pub use imp::StatusLed;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ir_overlay_restores_base_without_blocking_state_machine() {
        let mut state = StatusLedState::default();
        state.set_base(StatusColor::Streaming);
        state.pulse(StatusColor::IrRx, 100, 1_000);
        assert_eq!(state.effective(1_050), StatusColor::IrRx);
        assert_eq!(state.effective(1_100), StatusColor::Streaming);
    }

    #[test]
    fn tx_overlay_has_priority_and_error_cannot_be_overridden() {
        let mut state = StatusLedState::default();
        state.pulse(StatusColor::IrTx, 100, 10);
        state.pulse(StatusColor::IrRx, 100, 20);
        assert_eq!(state.effective(30), StatusColor::IrTx);
        state.set_base(StatusColor::Error);
        state.pulse(StatusColor::IrTx, 100, 40);
        assert_eq!(state.effective(50), StatusColor::Error);
    }

    #[test]
    fn operational_overlays_restore_latest_health_state() {
        let mut state = StatusLedState::default();
        state.set_base(StatusColor::Offline);
        state.pulse(StatusColor::Recording, 1_000, 0);
        assert_eq!(state.effective(10), StatusColor::Recording);
        state.set_base(StatusColor::Streaming);
        assert_eq!(state.effective(20), StatusColor::Recording);
        state.clear_overlay(StatusColor::Recording);
        assert_eq!(state.effective(21), StatusColor::Streaming);
        state.pulse(StatusColor::Provisioning, 100, 30);
        assert_eq!(state.effective(50), StatusColor::Provisioning);
        assert_eq!(state.effective(130), StatusColor::Streaming);
    }

    #[test]
    fn ws2812_frame_progresses_across_c5_ping_pong_halves() {
        let first = led_symbol_chunk(LED_SYMBOLS_PER_COLOR, 0, 24);
        assert_eq!(first, (0, 24, false));
        let second = led_symbol_chunk(LED_SYMBOLS_PER_COLOR, 24, 24);
        assert_eq!(second, (24, 1, true));

        let color = Color::new(0x11, 0x22, 0x33);
        assert_eq!(ws2812_bytes(color), [0x22, 0x11, 0x33]);
    }
}
