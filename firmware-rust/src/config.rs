use std::collections::BTreeMap;
use std::env;
use std::net::{Ipv4Addr, SocketAddrV4};

use crate::profile::{DeviceProfile, ProfileError, StatusLedGpios, StatusLedKind};
use crate::provisioning::ProvisionedConfig;

const SDKCONFIG: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/sdkconfig"));

pub const DEFAULT_UDP_HOST: &str = "255.255.255.255";
pub const DEFAULT_UDP_PORT: u16 = 5005;
pub const DEFAULT_SEND_FREQUENCY_HZ: u32 = 10;
pub const DEFAULT_QUEUE_CAPACITY: usize = 256;
pub const DEFAULT_DEVICE_MAC: &str = "aa:bb:cc:dd:ee:ff";
pub const DEFAULT_HARDWARE_PROFILE: &str = "classic";
pub const DEFAULT_WIFI_BAND: &str = "auto";
pub const DEFAULT_WIFI_PREFER_HT40: bool = true;
pub const DEFAULT_RGB_LED_ENABLED: bool = false;
pub const DEFAULT_RGB_LED_GPIO: i32 = 27;
pub const DEFAULT_HEALTH_CHECK_SECS: u32 = 5;
pub const DEFAULT_REBOOT_AFTER_SECS: u32 = 300;
pub const DEFAULT_STALL_REBOOT_SECS: u32 = 180;
pub const DEFAULT_WIFI_AUTORECONNECT: bool = true;
pub const DEFAULT_CAPTIVE_PORTAL_TIMEOUT_SECS: u32 = 180;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirmwareConfig {
    pub udp_host: String,
    pub udp_port: u16,
    pub send_frequency_hz: u32,
    pub queue_capacity: usize,
    pub hardware_profile: String,
    pub wifi_band: String,
    pub wifi_prefer_ht40: bool,
    pub wifi_ssid: String,
    pub wifi_password: String,
    pub wifi_max_retry: u32,
    pub rgb_led_enabled: bool,
    pub rgb_led_gpio: i32,
    pub rgb_led_kind: StatusLedKind,
    pub rgb_led_gpios: StatusLedGpios,
    pub rgb_led_brightness_percent: u8,
    pub rgb_led_colors: BTreeMap<String, Vec<u8>>,
    /// Intervalo (s) entre verificações de saúde da conexão WiFi/UDP.
    pub health_check_secs: u32,
    /// Reinicia o ESP se a WiFi ficar desconectada por este tempo (s).
    pub reboot_after_secs: u32,
    /// Reinicia o ESP se não houver envio UDP bem-sucedido por este tempo (s).
    pub stall_reboot_secs: u32,
    /// Reconectar automaticamente quando a WiFi cair (recomendado: true).
    pub wifi_autoreconnect: bool,
    /// Tempo (s) que o captive portal aguarda antes de reiniciar para re-tentar a
    /// rede salva, quando já há credenciais mas a AP está indisponível.
    pub captive_portal_timeout_secs: u32,
}

impl Default for FirmwareConfig {
    fn default() -> Self {
        Self {
            udp_host: DEFAULT_UDP_HOST.to_string(),
            udp_port: DEFAULT_UDP_PORT,
            send_frequency_hz: DEFAULT_SEND_FREQUENCY_HZ,
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            hardware_profile: DEFAULT_HARDWARE_PROFILE.to_string(),
            wifi_band: DEFAULT_WIFI_BAND.to_string(),
            wifi_prefer_ht40: DEFAULT_WIFI_PREFER_HT40,
            wifi_ssid: sdkconfig_string("CONFIG_EXAMPLE_WIFI_SSID").unwrap_or_default(),
            wifi_password: sdkconfig_string("CONFIG_EXAMPLE_WIFI_PASSWORD").unwrap_or_default(),
            wifi_max_retry: sdkconfig_u32("CONFIG_EXAMPLE_WIFI_CONN_MAX_RETRY").unwrap_or(6),
            rgb_led_enabled: DEFAULT_RGB_LED_ENABLED,
            rgb_led_gpio: DEFAULT_RGB_LED_GPIO,
            rgb_led_kind: StatusLedKind::Ws2812,
            rgb_led_gpios: StatusLedGpios::default(),
            rgb_led_brightness_percent: 25,
            rgb_led_colors: crate::profile::StatusLedProfile::default().colors,
            health_check_secs: DEFAULT_HEALTH_CHECK_SECS,
            reboot_after_secs: DEFAULT_REBOOT_AFTER_SECS,
            stall_reboot_secs: DEFAULT_STALL_REBOOT_SECS,
            wifi_autoreconnect: DEFAULT_WIFI_AUTORECONNECT,
            captive_portal_timeout_secs: DEFAULT_CAPTIVE_PORTAL_TIMEOUT_SECS,
        }
    }
}

impl FirmwareConfig {
    /// Loads legacy environment/sdkconfig values and overlays the normalized
    /// secret-free v1 profile when supplied by the build TUI.
    pub fn from_env_and_profile() -> Result<(Self, Option<DeviceProfile>), ProfileError> {
        let mut config = Self::from_env();
        let profile = DeviceProfile::from_environment()?;
        if let Some(profile) = profile.as_ref() {
            config.apply_device_profile(profile);
        }
        Ok((config, profile))
    }

    pub fn from_env() -> Self {
        let default = Self::default();
        Self {
            udp_host: read_string("AETHERSENSE_UDP_HOST", &default.udp_host),
            udp_port: read_u16("AETHERSENSE_UDP_PORT", default.udp_port),
            send_frequency_hz: read_u32("AETHERSENSE_SEND_FREQUENCY_HZ", default.send_frequency_hz),
            queue_capacity: read_usize("AETHERSENSE_QUEUE_CAPACITY", default.queue_capacity),
            hardware_profile: read_string(
                "AETHERSENSE_HARDWARE_PROFILE",
                &default.hardware_profile,
            ),
            wifi_band: read_string("AETHERSENSE_WIFI_BAND", &default.wifi_band),
            wifi_prefer_ht40: read_bool("AETHERSENSE_WIFI_PREFER_HT40", default.wifi_prefer_ht40),
            wifi_ssid: read_string("AETHERSENSE_WIFI_SSID", &default.wifi_ssid),
            wifi_password: read_string("AETHERSENSE_WIFI_PASSWORD", &default.wifi_password),
            wifi_max_retry: read_u32("AETHERSENSE_WIFI_MAX_RETRY", default.wifi_max_retry),
            rgb_led_enabled: read_bool("AETHERSENSE_RGB_LED_ENABLED", default.rgb_led_enabled),
            rgb_led_gpio: read_i32("AETHERSENSE_RGB_LED_GPIO", default.rgb_led_gpio),
            rgb_led_kind: default.rgb_led_kind,
            rgb_led_gpios: default.rgb_led_gpios,
            rgb_led_brightness_percent: default.rgb_led_brightness_percent,
            rgb_led_colors: default.rgb_led_colors,
            health_check_secs: read_u32("AETHERSENSE_HEALTH_CHECK_SECS", default.health_check_secs),
            reboot_after_secs: read_u32("AETHERSENSE_REBOOT_AFTER_SECS", default.reboot_after_secs),
            stall_reboot_secs: read_u32("AETHERSENSE_STALL_REBOOT_SECS", default.stall_reboot_secs),
            wifi_autoreconnect: read_bool(
                "AETHERSENSE_WIFI_AUTORECONNECT",
                default.wifi_autoreconnect,
            ),
            captive_portal_timeout_secs: read_u32(
                "AETHERSENSE_CAPTIVE_PORTAL_TIMEOUT_SECS",
                default.captive_portal_timeout_secs,
            ),
        }
    }

    pub fn udp_destination(&self) -> SocketAddrV4 {
        let host = self
            .udp_host
            .trim()
            .parse::<Ipv4Addr>()
            .unwrap_or(Ipv4Addr::BROADCAST);
        SocketAddrV4::new(host, self.udp_port)
    }

    pub fn send_interval_millis(&self) -> u64 {
        let frequency = self.send_frequency_hz.max(1) as u64;
        (1000_u64 / frequency).max(1)
    }

    pub fn apply_device_profile(&mut self, profile: &DeviceProfile) {
        self.udp_host = profile.network.udp_host.clone();
        self.udp_port = profile.network.udp_port;
        self.send_frequency_hz = profile.csi.send_frequency_hz;
        self.queue_capacity = profile.csi.queue_capacity;
        self.hardware_profile = profile.target.chip.hardware_profile().to_string();
        self.wifi_band = profile.csi.wifi_band.clone();
        self.wifi_prefer_ht40 = profile.csi.prefer_ht40;
        self.wifi_ssid = profile.network.wifi_ssid.clone();
        self.rgb_led_enabled = profile.runtime.status_led_enabled;
        if let Some(gpio) = profile.runtime.status_led_gpio {
            self.rgb_led_gpio = gpio;
        }
        self.rgb_led_kind = profile.runtime.status_led.kind;
        self.rgb_led_gpios = profile.runtime.status_led.gpios.clone();
        self.rgb_led_brightness_percent = profile.runtime.status_led.brightness_percent;
        self.rgb_led_colors = profile.runtime.status_led.colors.clone();
    }

    pub fn apply_provisioned_config(&mut self, provisioned: &ProvisionedConfig) {
        self.apply_device_profile(&provisioned.profile);
        self.wifi_password = provisioned.secrets.wifi_password.expose().to_string();
    }
}

fn read_string(key: &str, default: &str) -> String {
    env::var(key)
        .ok()
        .or_else(|| build_env(key).map(str::to_string))
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn read_u16(key: &str, default: u16) -> u16 {
    env::var(key)
        .ok()
        .or_else(|| build_env(key).map(str::to_string))
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|value| *value != 0)
        .unwrap_or(default)
}

fn read_u32(key: &str, default: u32) -> u32 {
    env::var(key)
        .ok()
        .or_else(|| build_env(key).map(str::to_string))
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value != 0)
        .unwrap_or(default)
}

fn read_usize(key: &str, default: usize) -> usize {
    env::var(key)
        .ok()
        .or_else(|| build_env(key).map(str::to_string))
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value != 0)
        .unwrap_or(default)
}

fn read_i32(key: &str, default: i32) -> i32 {
    env::var(key)
        .ok()
        .or_else(|| build_env(key).map(str::to_string))
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|value| *value >= 0)
        .unwrap_or(default)
}

fn read_bool(key: &str, default: bool) -> bool {
    env::var(key)
        .ok()
        .or_else(|| build_env(key).map(str::to_string))
        .and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "y" | "on" => Some(true),
            "0" | "false" | "no" | "n" | "off" => Some(false),
            _ => None,
        })
        .unwrap_or(default)
}

fn build_env(key: &str) -> Option<&'static str> {
    match key {
        "AETHERSENSE_UDP_HOST" => option_env!("AETHERSENSE_UDP_HOST"),
        "AETHERSENSE_UDP_PORT" => option_env!("AETHERSENSE_UDP_PORT"),
        "AETHERSENSE_SEND_FREQUENCY_HZ" => option_env!("AETHERSENSE_SEND_FREQUENCY_HZ"),
        "AETHERSENSE_QUEUE_CAPACITY" => option_env!("AETHERSENSE_QUEUE_CAPACITY"),
        "AETHERSENSE_HARDWARE_PROFILE" => option_env!("AETHERSENSE_HARDWARE_PROFILE"),
        "AETHERSENSE_WIFI_BAND" => option_env!("AETHERSENSE_WIFI_BAND"),
        "AETHERSENSE_WIFI_PREFER_HT40" => option_env!("AETHERSENSE_WIFI_PREFER_HT40"),
        "AETHERSENSE_WIFI_SSID" => option_env!("AETHERSENSE_WIFI_SSID"),
        "AETHERSENSE_WIFI_PASSWORD" => option_env!("AETHERSENSE_WIFI_PASSWORD"),
        "AETHERSENSE_WIFI_MAX_RETRY" => option_env!("AETHERSENSE_WIFI_MAX_RETRY"),
        "AETHERSENSE_RGB_LED_ENABLED" => option_env!("AETHERSENSE_RGB_LED_ENABLED"),
        "AETHERSENSE_RGB_LED_GPIO" => option_env!("AETHERSENSE_RGB_LED_GPIO"),
        "AETHERSENSE_HEALTH_CHECK_SECS" => option_env!("AETHERSENSE_HEALTH_CHECK_SECS"),
        "AETHERSENSE_REBOOT_AFTER_SECS" => option_env!("AETHERSENSE_REBOOT_AFTER_SECS"),
        "AETHERSENSE_STALL_REBOOT_SECS" => option_env!("AETHERSENSE_STALL_REBOOT_SECS"),
        "AETHERSENSE_WIFI_AUTORECONNECT" => option_env!("AETHERSENSE_WIFI_AUTORECONNECT"),
        "AETHERSENSE_CAPTIVE_PORTAL_TIMEOUT_SECS" => {
            option_env!("AETHERSENSE_CAPTIVE_PORTAL_TIMEOUT_SECS")
        }
        _ => None,
    }
}

fn sdkconfig_string(key: &str) -> Option<String> {
    SDKCONFIG.lines().rev().find_map(|line| {
        let line = line.trim();
        let (name, value) = line.split_once('=')?;

        if name.trim() == key {
            Some(value.trim().trim_matches('"').to_string())
        } else {
            None
        }
    })
}

fn sdkconfig_u32(key: &str) -> Option<u32> {
    sdkconfig_string(key).and_then(|val| val.parse::<u32>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_destination_uses_broadcast() {
        let config = FirmwareConfig::default();
        assert_eq!(config.udp_destination().ip(), &Ipv4Addr::BROADCAST);
        assert_eq!(config.udp_destination().port(), DEFAULT_UDP_PORT);
    }

    #[test]
    fn send_interval_has_lower_bound() {
        let mut config = FirmwareConfig::default();
        config.send_frequency_hz = 2000;
        assert_eq!(config.send_interval_millis(), 1);
    }

    #[test]
    fn device_profile_overlays_legacy_runtime_fields() {
        let mut profile = DeviceProfile::default();
        profile.identity.profile_uuid = "profile".to_string();
        profile.identity.name = "receiver".to_string();
        profile.network.udp_port = 6000;
        profile.csi.send_frequency_hz = 20;
        let mut config = FirmwareConfig::default();
        config.apply_device_profile(&profile);
        assert_eq!(config.udp_port, 6000);
        assert_eq!(config.send_frequency_hz, 20);
    }
}
