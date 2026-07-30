use serde::{Deserialize, Serialize};
/// Estado compartilhado da aplicação.
///
/// Usa `Arc<RwLock<InnerState>>` para acesso thread-safe entre o
/// listener UDP, handlers HTTP e tarefas WebSocket.
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

use crate::config::*;
use crate::csi::packet::CsiPacket;

/// Relógio monotônico do processo, em segundos, para datar as amostras CSI.
///
/// Independe do firmware do ESP32 (cujo `timestamp` é opcional e de unidade
/// ambígua) e fornece a grade temporal de recepção usada pela reamostragem
/// dos sinais vitais.
fn process_start() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

/// Segundos decorridos (monotônicos) desde o início do processo.
pub fn monotonic_seconds() -> f64 {
    process_start().elapsed().as_secs_f64()
}

// ── Configurações do dashboard ────────────────────────────────────

fn default_vitals_window() -> f64 {
    30.0
}
fn default_vitals_min_samples() -> usize {
    100
}
fn default_vitals_peak_prominence() -> f64 {
    2.0
}
fn default_heart_rate_abs_snr_floor() -> f64 {
    3.0
}
fn default_vitals_harmonic_count() -> usize {
    5
}
fn default_vitals_harmonic_tolerance_hz() -> f64 {
    0.06
}
fn default_vitals_subcarrier_top_k() -> usize {
    16
}
fn default_breathing_freq_min_hz() -> f64 {
    0.1
}
fn default_breathing_freq_max_hz() -> f64 {
    0.5
}
fn default_heart_rate_freq_min_hz() -> f64 {
    0.8
}
fn default_heart_rate_freq_max_hz() -> f64 {
    3.0
}

/// Perfil amigável exibido no dashboard. Os valores efetivos continuam
/// persistidos separadamente para que o usuário possa partir de um perfil e
/// depois personalizá-lo sem perder o ajuste fino.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShowerBoxProfile {
    Comfort,
    Balanced,
    Precise,
    Custom,
}

impl Default for ShowerBoxProfile {
    fn default() -> Self {
        Self::Balanced
    }
}

/// Define quais transições da zona podem comandar a válvula no Home
/// Assistant. O backend publica este valor como atributo MQTT; a automação
/// física continua responsável por acionar a tomada.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShowerControlMode {
    /// A zona pode ligar e desligar a válvula.
    Automatic,
    /// A zona pode ligar; o desligamento fica a cargo do usuário.
    Assisted,
    /// Nenhuma transição da zona comanda a válvula.
    Manual,
}

impl Default for ShowerControlMode {
    fn default() -> Self {
        Self::Automatic
    }
}

fn default_shower_entry_confirm_seconds() -> f64 {
    2.5
}
fn default_shower_entry_decay_seconds() -> f64 {
    1.5
}
fn default_shower_exit_hold_seconds() -> f64 {
    60.0
}
fn default_shower_recovery_confirm_seconds() -> f64 {
    2.0
}
fn default_shower_sensitivity() -> u8 {
    4
}
fn default_shower_presence_assist() -> bool {
    true
}

/// Comportamento persistente da zona do box, ajustável pelo dashboard sem
/// recompilar ou reiniciar o servidor. Os limites preservam os intertravamentos
/// físicos: entrada nunca confirma em menos de 2 s e a saída nunca usa hold
/// inferior a 60 s.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShowerBoxSettings {
    #[serde(default)]
    pub profile: ShowerBoxProfile,
    #[serde(default)]
    pub control_mode: ShowerControlMode,
    #[serde(default = "default_shower_entry_confirm_seconds")]
    pub entry_confirm_seconds: f64,
    #[serde(default = "default_shower_entry_decay_seconds")]
    pub entry_decay_seconds: f64,
    #[serde(default = "default_shower_exit_hold_seconds")]
    pub exit_hold_seconds: f64,
    #[serde(default = "default_shower_recovery_confirm_seconds")]
    pub recovery_confirm_seconds: f64,
    #[serde(default = "default_shower_sensitivity")]
    pub sensitivity: u8,
    #[serde(default = "default_shower_presence_assist")]
    pub presence_assist: bool,
}

impl Default for ShowerBoxSettings {
    fn default() -> Self {
        Self {
            profile: ShowerBoxProfile::Balanced,
            control_mode: ShowerControlMode::Automatic,
            entry_confirm_seconds: default_shower_entry_confirm_seconds(),
            entry_decay_seconds: default_shower_entry_decay_seconds(),
            exit_hold_seconds: default_shower_exit_hold_seconds(),
            recovery_confirm_seconds: default_shower_recovery_confirm_seconds(),
            sensitivity: default_shower_sensitivity(),
            presence_assist: true,
        }
    }
}

impl ShowerBoxSettings {
    pub fn clamp(&mut self) {
        self.entry_confirm_seconds = self.entry_confirm_seconds.clamp(2.0, 5.0);
        self.entry_decay_seconds = self.entry_decay_seconds.clamp(0.5, 5.0);
        self.exit_hold_seconds = self.exit_hold_seconds.clamp(60.0, 300.0);
        self.recovery_confirm_seconds = self.recovery_confirm_seconds.clamp(1.0, 5.0);
        self.sensitivity = self.sensitivity.clamp(1, 5);
    }

    /// Fator aplicado aos limiares adaptativos. Sensibilidade alta reduz os
    /// limiares de forma limitada; nunca remove a dominância WROOM-C5.
    pub fn threshold_factor(&self) -> f64 {
        match self.sensitivity.clamp(1, 5) {
            1 => 1.15,
            2 => 1.075,
            3 => 1.0,
            4 => 0.925,
            _ => 0.85,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardSettings {
    pub heatmap_window: usize,
    pub stft_window: usize,
    pub pca_window: usize,
    pub padding: f64,
    pub topology_scenario: i32,
    #[serde(default = "default_vitals_window")]
    pub vitals_window_seconds: f64,
    #[serde(default = "default_vitals_min_samples")]
    pub vitals_min_samples: usize,
    #[serde(default = "default_vitals_peak_prominence")]
    pub vitals_peak_prominence: f64,
    #[serde(default = "default_heart_rate_abs_snr_floor")]
    pub heart_rate_abs_snr_floor: f64,
    #[serde(default = "default_vitals_harmonic_count")]
    pub vitals_harmonic_count: usize,
    #[serde(default = "default_vitals_harmonic_tolerance_hz")]
    pub vitals_harmonic_tolerance_hz: f64,
    #[serde(default = "default_vitals_subcarrier_top_k")]
    pub vitals_subcarrier_top_k: usize,
    #[serde(default = "default_breathing_freq_min_hz")]
    pub breathing_freq_min_hz: f64,
    #[serde(default = "default_breathing_freq_max_hz")]
    pub breathing_freq_max_hz: f64,
    #[serde(default = "default_heart_rate_freq_min_hz")]
    pub heart_rate_freq_min_hz: f64,
    #[serde(default = "default_heart_rate_freq_max_hz")]
    pub heart_rate_freq_max_hz: f64,
}

impl Default for DashboardSettings {
    fn default() -> Self {
        Self {
            heatmap_window: DEFAULT_HEATMAP_WINDOW,
            stft_window: DEFAULT_STFT_WINDOW,
            pca_window: DEFAULT_PCA_WINDOW,
            padding: DEFAULT_PADDING,
            topology_scenario: 2,
            vitals_window_seconds: 30.0,
            vitals_min_samples: 100,
            vitals_peak_prominence: 2.0,
            heart_rate_abs_snr_floor: 3.0,
            vitals_harmonic_count: 5,
            vitals_harmonic_tolerance_hz: 0.06,
            vitals_subcarrier_top_k: 16,
            breathing_freq_min_hz: 0.1,
            breathing_freq_max_hz: 0.5,
            heart_rate_freq_min_hz: 0.8,
            heart_rate_freq_max_hz: 3.0,
        }
    }
}

impl DashboardSettings {
    /// Garante que os valores estão dentro dos limites permitidos.
    pub fn clamp(&mut self) {
        self.heatmap_window = self.heatmap_window.clamp(10, HEATMAP_HISTORY_LENGTH);
        self.stft_window = self.stft_window.clamp(16, HEATMAP_HISTORY_LENGTH);
        self.pca_window = self.pca_window.clamp(10, HEATMAP_HISTORY_LENGTH);
        self.padding = self.padding.clamp(0.0, 0.6);
        self.topology_scenario = self.topology_scenario.clamp(1, 6);
        self.vitals_window_seconds = self.vitals_window_seconds.clamp(5.0, 120.0);
        self.vitals_min_samples = self.vitals_min_samples.clamp(10, 1000);
        self.vitals_peak_prominence = self.vitals_peak_prominence.clamp(1.0, 10.0);
        self.heart_rate_abs_snr_floor = self.heart_rate_abs_snr_floor.clamp(1.0, 15.0);
        self.vitals_harmonic_count = self.vitals_harmonic_count.clamp(0, 10);
        self.vitals_harmonic_tolerance_hz = self.vitals_harmonic_tolerance_hz.clamp(0.01, 0.2);
        self.vitals_subcarrier_top_k = self.vitals_subcarrier_top_k.clamp(1, 64);
        self.breathing_freq_min_hz = self.breathing_freq_min_hz.clamp(0.05, 0.4);
        self.breathing_freq_max_hz = self.breathing_freq_max_hz.clamp(0.3, 1.0);
        self.heart_rate_freq_min_hz = self.heart_rate_freq_min_hz.clamp(0.5, 1.5);
        self.heart_rate_freq_max_hz = self.heart_rate_freq_max_hz.clamp(1.5, 4.0);
    }
}

// ── Configuração MQTT / Home Assistant (persistida + reconfigurável) ──

pub const MQTT_PASSWORD_ENV: &str = "AETHERSENSE_MQTT_PASSWORD";
pub const MQTT_PASSWORD_ENV_REF: &str = "env:AETHERSENSE_MQTT_PASSWORD";

/// Configuração do broker MQTT usada para integrar ao Home Assistant. Começa a
/// partir das variáveis de ambiente/CLI (`CliArgs`) no boot, mas pode ser
/// reconfigurada em runtime pelo dashboard (`POST /api/mqtt/settings`). Os
/// campos não secretos são persistidos no `aethersense_config.json`; a senha
/// existe apenas em memória e deve vir de `AETHERSENSE_MQTT_PASSWORD` para
/// sobreviver a um reinício. No disco fica, no máximo, a referência ao nome da
/// variável de ambiente, nunca o valor nem um hash reversível por dicionário.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MqttSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_mqtt_host")]
    pub host: String,
    #[serde(default = "default_mqtt_port")]
    pub port: u16,
    #[serde(default)]
    pub username: Option<String>,
    /// Segredo de runtime. `serde(skip)` é uma barreira adicional para impedir
    /// que qualquer snapshot de `MqttSettings` grave a credencial por engano.
    #[serde(skip)]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_secret_ref: Option<String>,
    #[serde(default = "default_mqtt_client_id")]
    pub client_id: String,
    #[serde(default = "default_mqtt_base_topic")]
    pub base_topic: String,
    #[serde(default = "default_mqtt_discovery_prefix")]
    pub discovery_prefix: String,
    #[serde(default = "default_mqtt_retain")]
    pub retain: bool,
    #[serde(default = "default_mqtt_publish_interval_ms")]
    pub publish_interval_ms: u64,
    #[serde(default = "default_mqtt_keep_alive_secs")]
    pub keep_alive_secs: u16,
}

fn default_mqtt_host() -> String {
    MQTT_DEFAULT_BROKER_HOST.to_string()
}
fn default_mqtt_port() -> u16 {
    MQTT_DEFAULT_BROKER_PORT
}
fn default_mqtt_client_id() -> String {
    MQTT_DEFAULT_CLIENT_ID.to_string()
}
fn default_mqtt_base_topic() -> String {
    MQTT_DEFAULT_BASE_TOPIC.to_string()
}
fn default_mqtt_discovery_prefix() -> String {
    MQTT_DEFAULT_DISCOVERY_PREFIX.to_string()
}
fn default_mqtt_retain() -> bool {
    true
}
fn default_mqtt_publish_interval_ms() -> u64 {
    MQTT_DEFAULT_PUBLISH_INTERVAL_MS
}
fn default_mqtt_keep_alive_secs() -> u16 {
    MQTT_DEFAULT_KEEP_ALIVE_SECS
}

impl Default for MqttSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            host: default_mqtt_host(),
            port: default_mqtt_port(),
            username: None,
            password: None,
            password_secret_ref: None,
            client_id: default_mqtt_client_id(),
            base_topic: default_mqtt_base_topic(),
            discovery_prefix: default_mqtt_discovery_prefix(),
            retain: default_mqtt_retain(),
            publish_interval_ms: default_mqtt_publish_interval_ms(),
            keep_alive_secs: default_mqtt_keep_alive_secs(),
        }
    }
}

impl From<&CliArgs> for MqttSettings {
    fn from(args: &CliArgs) -> Self {
        Self {
            enabled: args.mqtt_enabled,
            host: args.mqtt_host.clone(),
            port: args.mqtt_port,
            username: args.mqtt_username.clone(),
            password: args.mqtt_password.clone(),
            password_secret_ref: None,
            client_id: args.mqtt_client_id.clone(),
            base_topic: args.mqtt_base_topic.clone(),
            discovery_prefix: args.mqtt_discovery_prefix.clone(),
            retain: args.mqtt_retain,
            publish_interval_ms: args.mqtt_publish_interval_ms,
            keep_alive_secs: args.mqtt_keep_alive_secs,
        }
    }
}

impl MqttSettings {
    /// Aplica a senha somente ao processo atual. Se o mesmo valor veio da
    /// variável de ambiente oficial, persiste apenas a referência ao nome dela.
    pub fn set_runtime_password(&mut self, password: Option<String>) {
        let password = password.filter(|value| !value.is_empty());
        let from_environment = password.as_deref().is_some_and(|candidate| {
            std::env::var(MQTT_PASSWORD_ENV)
                .ok()
                .filter(|value| !value.is_empty())
                .as_deref()
                == Some(candidate)
        });
        self.password = password;
        self.password_secret_ref = from_environment.then(|| MQTT_PASSWORD_ENV_REF.to_string());
    }

    pub fn password_source(&self) -> &'static str {
        match (
            self.password.is_some(),
            self.password_secret_ref.as_deref() == Some(MQTT_PASSWORD_ENV_REF),
        ) {
            (true, true) => "environment",
            (true, false) => "memory",
            (false, true) => "environment_unavailable",
            (false, false) => "none",
        }
    }
}

// ── Dispositivos ESP32 ────────────────────────────────────────────

/// Representa um ESP32 registrado no sistema, com sua posição geográfica.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Identificador único (normalmente o MAC address)
    pub id: String,
    /// Nome amigável do dispositivo
    pub name: String,
    /// Latitude em graus decimais
    pub lat: f64,
    /// Longitude em graus decimais
    pub lng: f64,
    /// Andar do prédio (0 = térreo)
    pub floor: i32,
    /// Imcone do dispositivo (ex: /esp32-real.svg)
    #[serde(default)]
    pub icon: String,
    /// Tamanho do icone
    #[serde(default = "default_device_size")]
    pub size: i32,
    /// Papel do dispositivo na topologia: receiver, injector, router.
    #[serde(default = "default_device_role")]
    pub role: String,
    /// Perfil de hardware: esp32c5, wroom/classic, router.
    #[serde(default = "default_hardware_profile")]
    pub hardware_profile: String,
}

fn default_device_size() -> i32 {
    32
}

fn default_device_role() -> String {
    "receiver".to_string()
}

fn default_hardware_profile() -> String {
    "wroom".to_string()
}

// ── Configuração do mapa indoor ─────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FloorplanBounds {
    pub north: f64,
    pub south: f64,
    pub east: f64,
    pub west: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Floorplan {
    pub id: String,
    pub name: String,
    pub floor: i32,
    pub image_data_url: String,
    pub bounds: FloorplanBounds,
    #[serde(default = "default_floorplan_opacity")]
    pub opacity: f64,
}

fn default_floorplan_opacity() -> f64 {
    0.55
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentObject {
    pub id: String,
    pub name: String,
    #[serde(rename = "type", default = "default_environment_object_type")]
    pub object_type: String,
    pub lat: f64,
    pub lng: f64,
    pub floor: i32,
    #[serde(default = "default_environment_object_width_m")]
    pub width_m: f64,
    #[serde(default = "default_environment_object_depth_m")]
    pub depth_m: f64,
    #[serde(default)]
    pub rotation_deg: f64,
    #[serde(default = "default_true")]
    pub calibration_relevant: bool,
}

fn default_environment_object_type() -> String {
    "furniture".to_string()
}

fn default_environment_object_width_m() -> f64 {
    1.0
}

fn default_environment_object_depth_m() -> f64 {
    0.6
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MapConfig {
    #[serde(default)]
    pub floorplans: Vec<Floorplan>,
    #[serde(default)]
    pub environment_objects: Vec<EnvironmentObject>,
}

// ── Detecção ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MovementLevel {
    Static,
    SlightMovement,
    Walking,
    Running,
}

impl Default for MovementLevel {
    fn default() -> Self {
        Self::Static
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PresenceState {
    pub detected: bool,
    pub confidence: f64,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub hold_seconds_remaining: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MovementState {
    pub level: MovementLevel,
    pub velocity_estimate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LocationEstimate {
    pub lat: f64,
    pub lng: f64,
    pub floor: i32,
    pub accuracy_m: f64,
    pub confidence: f64,
    pub accuracy_dm: f64,
    pub nodes_used: usize,
    pub method: String,
    pub experimental: bool,
    pub resolution_m: f64,
    /// Cômodo (objeto `room` da planta) que contém a estimativa — planta
    /// baixa FUNCIONAL: point-in-polygon em `detection::geometry`.
    #[serde(default)]
    pub current_room: Option<String>,
    /// `true` quando a estimativa foi confinada aos bounds do floorplan do
    /// andar (geofence — a pessoa nunca "sai" do apartamento).
    #[serde(default)]
    pub clamped_to_floorplan: bool,
    /// Paredes (objetos `wall`) consideradas na correção de atenuação do
    /// modelo RSSI→distância (soma sobre os caminhos receptor→estimativa).
    #[serde(default)]
    pub walls_crossed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EmptyRoomBaseline {
    pub mean_amplitude: f64,
    pub amplitude_variance: f64,
    pub rssi_mean: f64,
    pub sample_count: usize,
    pub hardware_profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibrated_at: Option<String>,
    /// Identifica a mesma coleta vazia iniciada por `POST /api/clear`.
    /// A zona do box só aceita WROOM e C5 calibrados na mesma sessão.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floorplan_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environment_object_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DeviceSignalHistory {
    pub packet_count: u64,
    pub latest_packet: Option<CsiPacket>,
    pub amplitude_history: VecDeque<Vec<f64>>,
    pub phase_history: VecDeque<Vec<f64>>,
    pub signal_history: VecDeque<f64>,
    /// Instante de recepção (segundos monotônicos) de cada amostra de
    /// `signal_history`, alinhado índice a índice. Permite reamostrar os sinais
    /// vitais POR DISPOSITIVO (grade coerente), sem misturar nós.
    pub signal_time_history: VecDeque<f64>,
    pub rssi_history: VecDeque<i32>,
}

impl DeviceSignalHistory {
    pub fn new() -> Self {
        Self {
            packet_count: 0,
            latest_packet: None,
            amplitude_history: VecDeque::with_capacity(HEATMAP_HISTORY_LENGTH),
            phase_history: VecDeque::with_capacity(HEATMAP_HISTORY_LENGTH),
            signal_history: VecDeque::with_capacity(HEATMAP_HISTORY_LENGTH),
            signal_time_history: VecDeque::with_capacity(HEATMAP_HISTORY_LENGTH),
            rssi_history: VecDeque::with_capacity(HEATMAP_HISTORY_LENGTH),
        }
    }

    pub fn push_packet(&mut self, packet: CsiPacket) {
        self.packet_count += 1;

        if !packet.amplitudes.is_empty() {
            let amplitudes = packet.amplitudes.clone();
            let phases = packet.phases.clone();
            let mean_amp = amplitudes.iter().copied().sum::<f64>() / amplitudes.len() as f64;

            if self.amplitude_history.len() >= HEATMAP_HISTORY_LENGTH {
                self.amplitude_history.pop_front();
            }
            self.amplitude_history.push_back(amplitudes);

            if self.phase_history.len() >= HEATMAP_HISTORY_LENGTH {
                self.phase_history.pop_front();
            }
            self.phase_history.push_back(phases);

            if self.signal_history.len() >= HEATMAP_HISTORY_LENGTH {
                self.signal_history.pop_front();
            }
            self.signal_history.push_back(mean_amp);

            // Datar a amostra (grade temporal por dispositivo p/ os vitais).
            if self.signal_time_history.len() >= HEATMAP_HISTORY_LENGTH {
                self.signal_time_history.pop_front();
            }
            self.signal_time_history.push_back(monotonic_seconds());
        }

        if let Some(rssi) = packet.rssi {
            if self.rssi_history.len() >= HEATMAP_HISTORY_LENGTH {
                self.rssi_history.pop_front();
            }
            self.rssi_history.push_back(rssi);
        }

        self.latest_packet = Some(packet);
    }

    pub fn mean_amplitude(&self) -> f64 {
        mean(self.signal_history.iter().copied())
    }

    pub fn amplitude_variance(&self) -> f64 {
        variance(self.signal_history.iter().copied())
    }

    /// Média das ÚLTIMAS *n* amostras — janela curta para sinais de
    /// responsividade (ex.: score de zona), em contraste com o histórico
    /// inteiro (512 amostras ≈ 25 s+), que amortece transições rápidas.
    pub fn recent_mean_amplitude(&self, n: usize) -> f64 {
        let skip = self.signal_history.len().saturating_sub(n);
        mean(self.signal_history.iter().skip(skip).copied())
    }

    /// Variância das últimas *n* amostras (janela curta).
    pub fn recent_amplitude_variance(&self, n: usize) -> f64 {
        let skip = self.signal_history.len().saturating_sub(n);
        variance(self.signal_history.iter().skip(skip).copied())
    }

    pub fn mean_rssi(&self) -> Option<f64> {
        if self.rssi_history.is_empty() {
            None
        } else {
            Some(mean(self.rssi_history.iter().map(|value| *value as f64)))
        }
    }
}

impl Default for DeviceSignalHistory {
    fn default() -> Self {
        Self::new()
    }
}

fn mean<I>(values: I) -> f64
where
    I: Iterator<Item = f64>,
{
    let mut count = 0.0;
    let mut sum = 0.0;
    for value in values {
        if value.is_finite() {
            count += 1.0;
            sum += value;
        }
    }
    if count > 0.0 {
        sum / count
    } else {
        0.0
    }
}

fn variance<I>(values: I) -> f64
where
    I: Iterator<Item = f64>,
{
    let values: Vec<f64> = values.filter(|value| value.is_finite()).collect();
    if values.len() < 2 {
        return 0.0;
    }

    let avg = values.iter().sum::<f64>() / values.len() as f64;
    values
        .iter()
        .map(|value| {
            let diff = value - avg;
            diff * diff
        })
        .sum::<f64>()
        / values.len() as f64
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FftDiagnostics {
    pub has_presence: bool,
    pub sample_count: usize,
    pub sample_rate_hz: f64,
    pub breathing_peak_hz: Option<f64>,
    pub breathing_prominence: f64,
    pub heart_peak_hz: Option<f64>,
    pub heart_snr: f64,
    pub rejected_harmonics: Vec<f64>,
    pub spectrum_magnitudes: Vec<f64>,
    pub spectrum_freqs: Vec<f64>,
    pub status_message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VitalsEstimate {
    pub breathing_rate: Option<f64>,
    pub heart_rate_bpm: Option<f64>,
    pub confidence: f64,
}

/// Pessoa individual decodificada do heatmap do modelo de ocupação.
///
/// `vitals_scope == "individual"` somente quando há 1 pessoa detectada —
/// os datasets públicos não têm ground truth de vitais multi-pessoa, então
/// com 2+ pessoas a taxa publicada é a estimativa GLOBAL do ambiente
/// (scope "global"), honestamente sinalizada.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersonEstimate {
    /// Posição normalizada [0,1] na grade do heatmap (x→leste, y→sul).
    pub x_norm: f64,
    pub y_norm: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lng: Option<f64>,
    pub confidence: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breathing_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heart_rate_bpm: Option<f64>,
    pub vitals_scope: String,
    pub vitals_source: String,
}

/// Estimativa ML de ocupação (OccupancyHeatmapNet via ONNX), publicada LADO A
/// LADO com a heurística (`DetectionState::location`) para comparação.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OccupancyEstimate {
    /// false = modelo ausente/sem histórico suficiente (fallback heurístico).
    pub available: bool,
    pub count: usize,
    /// P(0), P(1), …, P(5+) — softmax dos logits de contagem.
    pub probabilities: Vec<f64>,
    pub confidence: f64,
    /// `false` = contagem NÃO calibrada para esta sala (estimativa zero-shot do
    /// modelo treinado no WiMANS, sujeito a "pico-fantasma na sala vazia / cego
    /// com pessoa"). Será `true` somente quando houver um modelo-da-sala/few-shot
    /// calibrado — que ainda não existe. Permite ao frontend e ao TCC sinalizarem
    /// honestamente que `count` é estimativa, não medida confiável.
    #[serde(default)]
    pub count_calibrated: bool,
    pub method: String,
    pub persons: Vec<PersonEstimate>,
    /// Vitais do VitalsNet ONNX (globais) — comparar com `vitals` (FFT).
    pub vitals_ml: VitalsEstimate,
    pub model_run_id: String,
    pub inference_ms: f64,
}

/// Status de runtime dos modelos ONNX, exposto em `GET /api/ml/status`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MlRuntimeStatus {
    pub occupancy_loaded: bool,
    pub vitals_loaded: bool,
    pub models_dir: String,
    pub model_run_id: String,
    pub last_inference_ms: f64,
    pub last_error: Option<String>,
}

/// Perturbação relativa ao baseline de sala vazia em UM receptor — quanto
/// maior, mais provável que a pessoa esteja na zona (cômodo) desse receptor.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ZoneScore {
    pub device_id: String,
    pub device_name: String,
    pub score: f64,
    pub has_baseline: bool,
    /// Decisão debounced do servidor (histerese + votação a 2 Hz +
    /// retenção por respiração): pronta para automação SEM espera no HA.
    pub occupied: bool,
    /// Limiares adaptativos aprendidos do piso de ruído desta zona
    /// (observabilidade/ajuste — ver detection::zones).
    #[serde(default)]
    pub on_threshold: f64,
    #[serde(default)]
    pub off_threshold: f64,
}

/// Zona ativa: receptor com maior perturbação. Com 1 receptor por cômodo,
/// permite automações por cômodo (ex.: chuveiro só liga se a zona ativa for
/// a do receptor do banheiro).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ActiveZoneState {
    pub zone_id: Option<String>,
    pub zone_name: Option<String>,
    pub score: f64,
    pub zones: Vec<ZoneScore>,
}

/// Fase temporal da detecção dedicada ao box do chuveiro.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShowerBoxPhase {
    Vacant,
    Candidate,
    Occupied,
    Holding,
}

impl Default for ShowerBoxPhase {
    fn default() -> Self {
        Self::Vacant
    }
}

/// Resultado agregado da tentativa de entrada mais recente no box.
///
/// Não contém CSI nem uma série histórica: registra somente o desfecho da
/// confirmação temporal atual/mais recente para diagnóstico no dashboard.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShowerBoxAttemptResult {
    Idle,
    InProgress,
    Confirmed,
    Rejected,
    NotReady,
}

impl Default for ShowerBoxAttemptResult {
    fn default() -> Self {
        Self::Idle
    }
}

/// Estado auditável da zona do box. Diferentemente de `ActiveZoneState`, este
/// estado exige dominância do WROOM sobre o C5, qualidade mínima do fluxo CSI
/// e confirmação temporal antes de permitir uma automação de água.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ShowerBoxState {
    pub occupied: bool,
    #[serde(rename = "tracker_state")]
    pub phase: ShowerBoxPhase,
    pub ready: bool,
    pub wroom_available: bool,
    pub c5_available: bool,
    pub wroom_has_baseline: bool,
    pub c5_has_baseline: bool,
    pub wroom_sample_rate_hz: f64,
    pub c5_sample_rate_hz: f64,
    pub wroom_score: f64,
    pub c5_score: f64,
    pub dominance: f64,
    pub on_threshold: f64,
    pub off_threshold: f64,
    pub dominance_on_threshold: f64,
    pub dominance_off_threshold: f64,
    /// Confirmação da zona genérica (diagnóstico; não é mais a única forma de
    /// sustentar um banho imóvel).
    pub hold_corroborated: bool,
    /// Presença global ajudou a sustentar o hold, ainda sob a geometria
    /// WROOM-C5 do tracker dedicado.
    pub presence_assisted: bool,
    /// Evidência combinada efetivamente aceita neste tick.
    pub hold_evidence: bool,
    /// Progresso normalizado da confirmação de entrada (0–1).
    pub entry_progress: f64,
    pub entry_seconds_remaining: f64,
    pub warmup_seconds_remaining: f64,
    pub hold_seconds_remaining: f64,
    pub profile: ShowerBoxProfile,
    pub control_mode: ShowerControlMode,
    pub configured_entry_confirm_seconds: f64,
    pub configured_entry_decay_seconds: f64,
    pub configured_exit_hold_seconds: f64,
    pub configured_recovery_confirm_seconds: f64,
    pub sensitivity: u8,
    pub presence_assist_enabled: bool,
    pub last_transition_reason: String,
    /// Contador monotônico, local ao processo, de tentativas que chegaram à
    /// fase `candidate`.
    pub attempt_sequence: u64,
    pub last_attempt_started_at_unix_ms: Option<u64>,
    pub last_attempt_finished_at_unix_ms: Option<u64>,
    pub last_attempt_result: ShowerBoxAttemptResult,
    /// Maior progresso agregado (0–1) observado na tentativa mais recente.
    pub last_attempt_peak_progress: f64,
    /// Contador monotônico, local ao processo, de sessões confirmadas.
    pub session_sequence: u64,
    /// Identificador efêmero da sessão atualmente ocupada; nunca é persistido.
    pub active_session_id: Option<String>,
    pub last_session_started_at_unix_ms: Option<u64>,
    pub last_session_ended_at_unix_ms: Option<u64>,
    pub last_session_end_reason: Option<String>,
}

impl Default for ShowerBoxState {
    fn default() -> Self {
        Self {
            occupied: false,
            phase: ShowerBoxPhase::Vacant,
            ready: false,
            wroom_available: false,
            c5_available: false,
            wroom_has_baseline: false,
            c5_has_baseline: false,
            wroom_sample_rate_hz: 0.0,
            c5_sample_rate_hz: 0.0,
            wroom_score: 0.0,
            c5_score: 0.0,
            dominance: 0.0,
            on_threshold: 0.12,
            off_threshold: 0.07,
            dominance_on_threshold: 0.07,
            dominance_off_threshold: 0.05,
            hold_corroborated: false,
            presence_assisted: false,
            hold_evidence: false,
            entry_progress: 0.0,
            entry_seconds_remaining: default_shower_entry_confirm_seconds(),
            warmup_seconds_remaining: 5.0,
            hold_seconds_remaining: 0.0,
            profile: ShowerBoxProfile::Balanced,
            control_mode: ShowerControlMode::Automatic,
            configured_entry_confirm_seconds: default_shower_entry_confirm_seconds(),
            configured_entry_decay_seconds: default_shower_entry_decay_seconds(),
            configured_exit_hold_seconds: default_shower_exit_hold_seconds(),
            configured_recovery_confirm_seconds: default_shower_recovery_confirm_seconds(),
            sensitivity: default_shower_sensitivity(),
            presence_assist_enabled: true,
            last_transition_reason: "startup_not_ready".to_string(),
            attempt_sequence: 0,
            last_attempt_started_at_unix_ms: None,
            last_attempt_finished_at_unix_ms: None,
            last_attempt_result: ShowerBoxAttemptResult::Idle,
            last_attempt_peak_progress: 0.0,
            session_sequence: 0,
            active_session_id: None,
            last_session_started_at_unix_ms: None,
            last_session_ended_at_unix_ms: None,
            last_session_end_reason: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DetectionState {
    pub presence: PresenceState,
    pub movement: MovementState,
    pub location: LocationEstimate,
    pub vitals: VitalsEstimate,
    #[serde(default)]
    pub fft_diagnostics: FftDiagnostics,
    /// Estimativa ML (ONNX) — aditivo; clientes antigos ignoram o campo.
    #[serde(default)]
    pub occupancy: OccupancyEstimate,
    /// Zona (cômodo) com maior perturbação de canal — base das automações
    /// por cômodo no Home Assistant.
    #[serde(default)]
    pub active_zone: ActiveZoneState,
    /// Zona dedicada do box: confirmação WROOM−C5, warm-up e retenção
    /// longa para comandar a válvula sem ciclos liga/desliga.
    #[serde(default)]
    pub shower_box: ShowerBoxState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamSource {
    Idle,
    UdpLive,
    CsvReplay,
}

impl Default for StreamSource {
    fn default() -> Self {
        Self::Idle
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStatus {
    pub source: StreamSource,
    pub replay_active: bool,
    pub replay_file: Option<String>,
    pub reason: String,
}

impl Default for StreamStatus {
    fn default() -> Self {
        Self {
            source: StreamSource::Idle,
            replay_active: false,
            replay_file: None,
            reason: "Sem pacotes CSI ainda".to_string(),
        }
    }
}

impl StreamStatus {
    pub fn udp_live() -> Self {
        Self {
            source: StreamSource::UdpLive,
            replay_active: false,
            replay_file: None,
            reason: "Fluxo UDP ao vivo".to_string(),
        }
    }

    pub fn csv_replay(replay_file: String) -> Self {
        Self {
            source: StreamSource::CsvReplay,
            replay_active: true,
            replay_file: Some(replay_file),
            reason: "Replay CSV M6: dados de captura local, nao fluxo ao vivo".to_string(),
        }
    }

    pub fn idle(reason: impl Into<String>, replay_file: Option<String>) -> Self {
        Self {
            source: StreamSource::Idle,
            replay_active: false,
            replay_file,
            reason: reason.into(),
        }
    }
}

// ── Estado interno ────────────────────────────────────────────────

/// Serializa e ordena todas as gravações do arquivo de configuração. Cada
/// snapshot recebe uma revisão; uma tarefa atrasada não pode sobrescrever uma
/// configuração mais nova já confirmada por um endpoint.
#[derive(Clone, Default)]
pub struct ConfigWriteCoordinator {
    latest_revision: Arc<AtomicU64>,
    write_lock: Arc<Mutex<()>>,
}

impl ConfigWriteCoordinator {
    pub fn reserve_revision(&self) -> u64 {
        self.latest_revision.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn write_if_current(
        &self,
        path: &std::path::Path,
        json: &str,
        revision: u64,
    ) -> std::io::Result<bool> {
        self.write_if_current_with_hook(path, json, revision, |_| Ok(()))
    }

    fn write_if_current_with_hook<F>(
        &self,
        path: &std::path::Path,
        json: &str,
        revision: u64,
        before_persist: F,
    ) -> std::io::Result<bool>
    where
        F: FnOnce(&std::path::Path) -> std::io::Result<()>,
    {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| std::io::Error::other("config write lock poisoned"))?;
        if revision != self.latest_revision.load(Ordering::Acquire) {
            return Ok(false);
        }

        // Nunca trunque o arquivo canônico antes de confirmar o conteúdo. O
        // temporário fica no mesmo diretório para que `persist` faça uma troca
        // atômica tanto no Unix (`rename`) quanto no Windows (`MoveFileExW`).
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        let mut temporary = tempfile::Builder::new()
            .prefix(".aethersense-config-")
            .tempfile_in(parent)?;
        temporary.write_all(json.as_bytes())?;
        temporary.as_file_mut().sync_all()?;
        before_persist(temporary.path())?;
        temporary.persist(path).map_err(|error| error.error)?;
        Ok(true)
    }
}

pub struct InnerState {
    pub config_path: PathBuf,
    pub config_writer: ConfigWriteCoordinator,
    pub packet_count: u64,
    pub latest_packet: Option<CsiPacket>,
    pub amplitude_history: VecDeque<Vec<f64>>,
    pub phase_history: VecDeque<Vec<f64>>,
    pub signal_history: VecDeque<f64>,
    /// Instante de recepção (segundos monotônicos) de cada amostra de
    /// `signal_history`, alinhado índice a índice. Usado para reamostrar os
    /// sinais vitais em uma grade temporal uniforme.
    pub signal_time_history: VecDeque<f64>,
    pub heatmap_history: VecDeque<Vec<f64>>,
    pub device_histories: HashMap<String, DeviceSignalHistory>,
    /// Canal WiFi observado por dispositivo (MAC → canal), da CSI ao vivo, para
    /// derivar a BANDA REAL no card de topologia. Transitório (não persistido).
    /// Canal ≤ 14 = 2,4 GHz; ≥ 32 = 5 GHz.
    pub device_channels: HashMap<String, i32>,
    pub location_baselines: HashMap<String, EmptyRoomBaseline>,
    pub map_config: MapConfig,
    pub settings: DashboardSettings,
    /// Comportamento da zona dedicada do box, persistido e editável pelo
    /// dashboard comum por endpoint próprio.
    pub shower_box_settings: ShowerBoxSettings,
    /// Configuração MQTT/Home Assistant persistida e reconfigurável em runtime.
    pub mqtt: MqttSettings,
    /// `true` se o `mqtt` veio do arquivo de config (senão o boot sobrepõe com
    /// os valores de ambiente/CLI em `main.rs`). Transitório (não persistido).
    pub mqtt_configured: bool,
    /// Handle de runtime do serviço MQTT (supervisor + publisher swappável +
    /// status de conexão). Transitório; preenchido no boot por `main.rs`.
    pub mqtt_service: Option<crate::mqtt::MqttService>,
    /// Diretório de persistência automática das sessões de validação/resultado
    /// (`resultados/` na raiz do repo via bind mount). Definido no boot.
    pub results_dir: PathBuf,
    /// Domínio persistente de recursos, códigos IR e comandos recentes.
    pub accessories: crate::accessories::AccessoryRegistry,
    pub devices: Vec<DeviceInfo>,
    /// Piso de ruído por zona (`device_id` → percentil aprendido) que o
    /// `ZoneTracker` usa para derivar os limiares adaptativos. Persistido no
    /// config e recarregado no boot para SEMEAR o tracker — sem isso, todo
    /// restart do container zera ~2 min de janela cega/ruidosa nas automações,
    /// justo durante o redeploy. Aditivo: config antigo sem o campo carrega
    /// vazio (`#[serde(default)]` na desserialização).
    pub zone_floors: HashMap<String, f64>,
    pub detection: DetectionState,
    pub stream: StreamStatus,
    pub replay_generation: u64,
    /// Início monotônico da coleta vazia aberta pelo último `POST /api/clear`.
    /// A calibração rejeita históricos anteriores ou sem esse marco explícito.
    pub calibration_clear_at_s: Option<f64>,
    /// ID persistido nos baselines criados a partir do último clear.
    pub calibration_session_id: Option<String>,
    /// Enquanto presente, o box rejeita qualquer par de baselines que não
    /// pertença a esta nova coleta. É persistido para sobreviver a reinícios.
    pub shower_box_required_session_id: Option<String>,
    /// Receptores já calibrados usando a coleta aberta pelo último clear.
    /// Evita reutilizar silenciosamente o mesmo histórico em outra calibração.
    pub calibrated_since_clear: HashSet<String>,
    /// Incrementado a cada atualização da detecção. Usado pelo WebSocket para
    /// reenviar snapshots quando a detecção evolui mesmo sem novos pacotes.
    pub detection_version: u64,
    /// Marca que a configuração mudou e precisa ser persistida fora do
    /// caminho quente (flush assíncrono em vez de `std::fs::write` sob lock).
    pub config_dirty: bool,
    /// Status de runtime dos modelos ONNX (atualizado pelo detection_loop).
    pub ml_status: MlRuntimeStatus,
    pub running: bool,
}

impl InnerState {
    pub fn new() -> Self {
        let config_path = std::env::var_os("AETHERSENSE_CONFIG_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("aethersense_config.json"));
        Self::new_with_config_path(config_path)
    }

    pub fn new_with_config_path<P: Into<PathBuf>>(config_path: P) -> Self {
        let config_path = config_path.into();
        let mut settings = DashboardSettings::default();
        let mut shower_box_settings = ShowerBoxSettings::default();
        let mut devices = Vec::new();
        let mut location_baselines = HashMap::new();
        let mut map_config = MapConfig::default();
        let mut zone_floors: HashMap<String, f64> = HashMap::new();
        let mut shower_box_required_session_id: Option<String> = None;
        let mut mqtt = MqttSettings::default();
        let mut mqtt_configured = false;
        let mut accessories = crate::accessories::AccessoryRegistry::default();
        if let Ok(content) = std::fs::read_to_string(&config_path) {
            if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&content) {
                let mut scrubbed_legacy_mqtt_password = false;
                let mut legacy_mqtt_password = None;
                if let Some(s) = val.get("settings") {
                    if let Ok(s_deser) = serde_json::from_value(s.clone()) {
                        settings = s_deser;
                    }
                }
                if let Some(s) = val.get("shower_box_settings") {
                    if let Ok(s_deser) = serde_json::from_value(s.clone()) {
                        shower_box_settings = s_deser;
                    }
                }
                if let Some(d) = val.get("devices") {
                    if let Ok(d_deser) = serde_json::from_value(d.clone()) {
                        devices = d_deser;
                    }
                }
                if let Some(b) = val.get("location_baselines") {
                    if let Ok(b_deser) = serde_json::from_value(b.clone()) {
                        location_baselines = b_deser;
                    }
                }
                if let Some(m) = val.get("map_config") {
                    if let Ok(m_deser) = serde_json::from_value(m.clone()) {
                        map_config = m_deser;
                    }
                }
                // Piso de ruído por zona aprendido (aditivo; ausente em config
                // antigo → mapa vazio, tracker reaprende como antes).
                if let Some(z) = val.get("zone_floors") {
                    if let Ok(z_deser) = serde_json::from_value(z.clone()) {
                        zone_floors = z_deser;
                    }
                }
                if let Some(required) = val
                    .get("shower_box_required_session_id")
                    .and_then(|value| value.as_str())
                {
                    shower_box_required_session_id = Some(required.to_string());
                }
                // Configuração MQTT/HA (aditiva; ausente em config antigo → boot
                // sobrepõe com os valores de ambiente/CLI).
                if let Some(mq) = val.get_mut("mqtt") {
                    if let Some(object) = mq.as_object_mut() {
                        if let Some(password) = object.remove("password") {
                            scrubbed_legacy_mqtt_password = true;
                            legacy_mqtt_password = password
                                .as_str()
                                .filter(|value| !value.is_empty())
                                .map(str::to_string);
                        }
                    }
                    if let Ok(mq_deser) = serde_json::from_value(mq.clone()) {
                        mqtt = mq_deser;
                        // Compatibilidade de uma única execução: a credencial
                        // legada continua em memória após ser removida do disco.
                        // No próximo boot ela deverá vir do ambiente.
                        if legacy_mqtt_password.is_some() {
                            mqtt.password = legacy_mqtt_password;
                        }
                        mqtt_configured = true;
                    }
                }
                if let Some(registry) = val.get("accessories") {
                    if let Ok(deserialized) = serde_json::from_value(registry.clone()) {
                        accessories = deserialized;
                        accessories.reset_runtime_presence();
                    }
                }
                if scrubbed_legacy_mqtt_password {
                    let scrubbed = serde_json::to_string_pretty(&val)
                        .expect("config MQTT já desserializada deve serializar");
                    let writer = ConfigWriteCoordinator::default();
                    let revision = writer.reserve_revision();
                    writer
                        .write_if_current(&config_path, &scrubbed, revision)
                        .unwrap_or_else(|error| {
                            panic!(
                                "falha ao remover senha MQTT legada de {}: {}",
                                config_path.display(),
                                error
                            )
                        });
                }
            }
        }
        settings.clamp();
        shower_box_settings.clamp();

        Self {
            config_path,
            config_writer: ConfigWriteCoordinator::default(),
            packet_count: 0,
            latest_packet: None,
            amplitude_history: VecDeque::with_capacity(HEATMAP_HISTORY_LENGTH),
            phase_history: VecDeque::with_capacity(HEATMAP_HISTORY_LENGTH),
            signal_history: VecDeque::with_capacity(HEATMAP_HISTORY_LENGTH),
            signal_time_history: VecDeque::with_capacity(HEATMAP_HISTORY_LENGTH),
            heatmap_history: VecDeque::with_capacity(HEATMAP_HISTORY_LENGTH),
            device_histories: HashMap::new(),
            device_channels: HashMap::new(),
            location_baselines,
            map_config,
            settings,
            shower_box_settings,
            mqtt,
            mqtt_configured,
            mqtt_service: None,
            results_dir: PathBuf::from("resultados"),
            accessories,
            devices,
            zone_floors,
            detection: DetectionState::default(),
            stream: StreamStatus::default(),
            replay_generation: 0,
            calibration_clear_at_s: None,
            calibration_session_id: None,
            shower_box_required_session_id,
            calibrated_since_clear: HashSet::new(),
            detection_version: 0,
            config_dirty: false,
            ml_status: MlRuntimeStatus::default(),
            running: true,
        }
    }

    /// Serializa a configuração persistível para JSON (sem tocar no disco).
    pub fn serialize_config_json(&self) -> Option<String> {
        serde_json::to_string_pretty(&serde_json::json!({
            "settings": self.settings,
            "shower_box_settings": self.shower_box_settings,
            "mqtt": self.mqtt,
            "accessories": self.accessories,
            "devices": self.devices,
            "location_baselines": self.location_baselines,
            "map_config": self.map_config,
            "zone_floors": self.zone_floors,
            "shower_box_required_session_id": self.shower_box_required_session_id
        }))
        .ok()
    }

    pub fn save_config(&self) -> std::io::Result<bool> {
        let json = self.serialize_config_json().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "falha ao serializar configuração",
            )
        })?;
        let revision = self.config_writer.reserve_revision();
        self.config_writer
            .write_if_current(&self.config_path, &json, revision)
    }

    /// Serializa os dispositivos com a BANDA REAL observada (do canal da CSI ao
    /// vivo), para o frontend. Junta cada device (id == MAC) ao último canal em
    /// `device_channels`. Sem CSI ao vivo de um device, ele sai sem `band`/
    /// `channel` (o frontend cai no rótulo nominal do cenário).
    pub fn devices_with_band(&self) -> Vec<serde_json::Value> {
        self.devices
            .iter()
            .map(|d| {
                let mut v = serde_json::to_value(d).unwrap_or_else(|_| serde_json::json!({}));
                if let (Some(ch), Some(obj)) = (self.device_channels.get(&d.id), v.as_object_mut())
                {
                    let band = if *ch <= 14 { "2.4 GHz" } else { "5 GHz" };
                    obj.insert("channel".to_string(), serde_json::json!(ch));
                    obj.insert("band".to_string(), serde_json::json!(band));
                }
                v
            })
            .collect()
    }

    /// Adiciona um novo pacote CSI ao histórico, mantendo o tamanho máximo.
    pub fn push_packet(&mut self, packet: CsiPacket) {
        self.packet_count += 1;
        let mac = packet.mac.clone();
        if let Some(ch) = packet.channel {
            self.device_channels.insert(mac.clone(), ch);
        }

        if !packet.amplitudes.is_empty() {
            let amplitudes = packet.amplitudes.clone();
            let phases = packet.phases.clone();
            let mean_amp: f64 = amplitudes.iter().copied().sum::<f64>() / amplitudes.len() as f64;

            if self.amplitude_history.len() >= HEATMAP_HISTORY_LENGTH {
                self.amplitude_history.pop_front();
            }
            self.amplitude_history.push_back(amplitudes.clone());

            if self.phase_history.len() >= HEATMAP_HISTORY_LENGTH {
                self.phase_history.pop_front();
            }
            self.phase_history.push_back(phases);

            if self.signal_history.len() >= HEATMAP_HISTORY_LENGTH {
                self.signal_history.pop_front();
            }
            self.signal_history.push_back(mean_amp);

            // Datar a amostra com o instante de recepção (grade para os vitais).
            if self.signal_time_history.len() >= HEATMAP_HISTORY_LENGTH {
                self.signal_time_history.pop_front();
            }
            self.signal_time_history.push_back(monotonic_seconds());

            if self.heatmap_history.len() >= HEATMAP_HISTORY_LENGTH {
                self.heatmap_history.pop_front();
            }
            self.heatmap_history.push_back(amplitudes);
        }

        if !mac.trim().is_empty() {
            self.device_histories
                .entry(mac)
                .or_default()
                .push_packet(packet.clone());
        }

        self.latest_packet = Some(packet);
    }

    pub fn ingest_udp_packet(&mut self, packet: CsiPacket) {
        if self.stream.replay_active {
            self.replay_generation += 1;
            // Nunca misturar quadros reproduzidos com a retomada ao vivo.
            self.clear_measurements();
            self.invalidate_calibration_capture();
        }

        ensure_device_registered(self, &packet);
        self.stream = StreamStatus::udp_live();
        self.push_packet(packet);
    }

    pub fn ingest_replay_packet(&mut self, packet: CsiPacket, replay_file: String) {
        ensure_device_registered(self, &packet);
        self.stream = StreamStatus::csv_replay(replay_file);
        self.push_packet(packet);
    }

    pub fn start_replay(&mut self, replay_file: String) -> u64 {
        self.clear_measurements();
        self.invalidate_calibration_capture();
        self.replay_generation += 1;
        self.stream = StreamStatus::csv_replay(replay_file);
        self.replay_generation
    }

    pub fn stop_replay(&mut self, reason: impl Into<String>) {
        let replay_file = self.stream.replay_file.clone();
        self.invalidate_calibration_capture();
        self.replay_generation += 1;
        self.stream = StreamStatus::idle(reason, replay_file);
    }

    /// Limpa todos os históricos e reseta contadores.
    pub fn clear(&mut self) {
        self.replay_generation += 1;
        self.clear_measurements();
        self.calibration_clear_at_s = Some(monotonic_seconds());
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let session_id = format!("empty-{unix_ms}-{}", self.replay_generation);
        self.calibration_session_id = Some(session_id.clone());
        self.shower_box_required_session_id = Some(session_id);
        self.calibrated_since_clear.clear();
        self.stream = StreamStatus::default();
    }

    fn clear_measurements(&mut self) {
        self.packet_count = 0;
        self.latest_packet = None;
        self.amplitude_history.clear();
        self.phase_history.clear();
        self.signal_history.clear();
        self.signal_time_history.clear();
        self.heatmap_history.clear();
        self.device_histories.clear();
        self.detection = DetectionState::default();
    }

    fn invalidate_calibration_capture(&mut self) {
        self.calibration_clear_at_s = None;
        self.calibration_session_id = None;
        self.calibrated_since_clear.clear();
    }
}

fn ensure_device_registered(inner: &mut InnerState, packet: &CsiPacket) {
    let mac = packet.mac.trim();
    if mac.is_empty() || mac.eq_ignore_ascii_case("aa:bb:cc:dd:ee:ff") {
        return;
    }

    if inner.devices.iter().any(|device| device.id == mac) {
        return;
    }

    let suffix = device_suffix(mac);
    let (lat, lng, floor) = next_device_position(inner);

    inner.devices.push(DeviceInfo {
        id: mac.to_string(),
        name: format!("ESP32 {}", suffix),
        lat,
        lng,
        floor,
        icon: String::new(),
        size: 32,
        role: "receiver".to_string(),
        hardware_profile: infer_hardware_profile(mac),
    });
    // Caminho quente (ingest UDP/replay): não escrever em disco sob o lock.
    // O `detection_loop` faz o flush assíncrono quando `config_dirty` está set.
    inner.config_dirty = true;
}

fn infer_hardware_profile(mac: &str) -> String {
    let lower = mac.to_ascii_lowercase();
    if lower.starts_with("d0:cf") {
        "esp32c5".to_string()
    } else {
        "wroom".to_string()
    }
}

fn device_suffix(mac: &str) -> String {
    let compact = mac
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_uppercase();

    if compact.len() <= 4 {
        compact
    } else {
        compact[compact.len() - 4..].to_string()
    }
}

fn next_device_position(inner: &InnerState) -> (f64, f64, i32) {
    let base = inner.devices.first();
    let offset_index = inner.devices.len() as f64 + 1.0;
    let offset = 0.000015 * offset_index;

    match base {
        Some(device) => (device.lat + offset, device.lng + offset, device.floor),
        None => (0.0 + offset, 0.0 + offset, 0),
    }
}

// ── Handle compartilhado ──────────────────────────────────────────

/// Handle clonável para o estado da aplicação.
///
/// Para obter acesso, use `.read().await` ou `.write().await`.
pub type AppState = Arc<RwLock<InnerState>>;

/// Cria um novo `AppState` pronto para uso.
pub fn new_app_state() -> AppState {
    Arc::new(RwLock::new(InnerState::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn senha_mqtt_legada_e_removida_atomicamente_e_nunca_reserializada() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("aethersense_config.json");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "unknown_future_field": {"preserved": true},
                "mqtt": {
                    "enabled": true,
                    "host": "broker.local",
                    "username": "device-1",
                    "password": "segredo-legado"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let state = InnerState::new_with_config_path(&path);
        assert_eq!(state.mqtt.password.as_deref(), Some("segredo-legado"));
        assert_eq!(state.mqtt.password_source(), "memory");

        let migrated: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(migrated.pointer("/mqtt/password").is_none());
        assert_eq!(
            migrated.pointer("/unknown_future_field/preserved"),
            Some(&json!(true))
        );

        let snapshot = state.serialize_config_json().unwrap();
        assert!(!snapshot.contains("segredo-legado"));
        assert!(serde_json::from_str::<serde_json::Value>(&snapshot)
            .unwrap()
            .pointer("/mqtt/password")
            .is_none());

        // Compatibilidade deliberada: o segredo legado vale somente para o
        // processo que fez a migração. Sem ambiente, o próximo boot não o tem.
        let restarted = InnerState::new_with_config_path(&path);
        assert!(restarted.mqtt.password.is_none());
        assert_eq!(restarted.mqtt.host, "broker.local");
    }

    #[test]
    fn legacy_empty_room_baseline_deserializes_without_map_context() {
        let baseline: EmptyRoomBaseline = serde_json::from_value(json!({
            "mean_amplitude": 42.0,
            "amplitude_variance": 1.5,
            "rssi_mean": -62.0,
            "sample_count": 24,
            "hardware_profile": "wroom"
        }))
        .expect("legacy baseline should deserialize");

        assert_eq!(baseline.floor, None);
        assert_eq!(baseline.floorplan_id, None);
        assert_eq!(baseline.calibration_session_id, None);
        assert!(baseline.environment_object_ids.is_empty());
    }

    #[test]
    fn estado_legado_do_box_recebe_defaults_da_auditoria_agregada() {
        let mut legacy =
            serde_json::to_value(ShowerBoxState::default()).expect("estado padrão deve serializar");
        let object = legacy
            .as_object_mut()
            .expect("estado do box deve ser objeto JSON");
        for field in [
            "attempt_sequence",
            "last_attempt_started_at_unix_ms",
            "last_attempt_finished_at_unix_ms",
            "last_attempt_result",
            "last_attempt_peak_progress",
            "session_sequence",
            "active_session_id",
            "last_session_started_at_unix_ms",
            "last_session_ended_at_unix_ms",
            "last_session_end_reason",
        ] {
            object.remove(field);
        }

        let state: ShowerBoxState =
            serde_json::from_value(legacy).expect("snapshot legado deve continuar compatível");
        assert_eq!(state.attempt_sequence, 0);
        assert_eq!(state.last_attempt_result, ShowerBoxAttemptResult::Idle);
        assert_eq!(state.last_attempt_peak_progress, 0.0);
        assert_eq!(state.session_sequence, 0);
        assert!(state.active_session_id.is_none());
        assert!(state.last_session_end_reason.is_none());
    }

    #[test]
    fn legacy_config_without_map_config_loads_empty_indoor_map() {
        let path = std::env::temp_dir().join(format!(
            "aethersense_legacy_config_{}_{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time before epoch")
                .as_nanos()
        ));

        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "settings": DashboardSettings::default(),
                "devices": [],
                "location_baselines": {}
            }))
            .expect("config serializes"),
        )
        .expect("legacy config can be written");

        let state = InnerState::new_with_config_path(&path);
        assert!(state.map_config.floorplans.is_empty());
        assert!(state.map_config.environment_objects.is_empty());
        // Config antigo (sem zone_floors) carrega o mapa de pisos vazio.
        assert!(state.zone_floors.is_empty());
        assert_eq!(state.shower_box_settings, ShowerBoxSettings::default());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn configuracao_do_box_e_limitada_persistida_e_recarregada() {
        let path = std::env::temp_dir().join(format!(
            "aethersense_shower_settings_{}_{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time before epoch")
                .as_nanos()
        ));
        let mut state = InnerState::new_with_config_path(&path);
        state.shower_box_settings = ShowerBoxSettings {
            profile: ShowerBoxProfile::Custom,
            control_mode: ShowerControlMode::Manual,
            entry_confirm_seconds: 0.2,
            entry_decay_seconds: 99.0,
            exit_hold_seconds: 10.0,
            recovery_confirm_seconds: 20.0,
            sensitivity: 9,
            presence_assist: false,
        };
        state.shower_box_settings.clamp();
        state.save_config().expect("config do box deve persistir");

        let reloaded = InnerState::new_with_config_path(&path);
        assert_eq!(
            reloaded.shower_box_settings.profile,
            ShowerBoxProfile::Custom
        );
        assert_eq!(
            reloaded.shower_box_settings.control_mode,
            ShowerControlMode::Manual
        );
        assert_eq!(reloaded.shower_box_settings.entry_confirm_seconds, 2.0);
        assert_eq!(reloaded.shower_box_settings.entry_decay_seconds, 5.0);
        assert_eq!(reloaded.shower_box_settings.exit_hold_seconds, 60.0);
        assert_eq!(reloaded.shower_box_settings.recovery_confirm_seconds, 5.0);
        assert_eq!(reloaded.shower_box_settings.sensitivity, 5);
        assert!(!reloaded.shower_box_settings.presence_assist);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn zone_floors_persistem_e_recarregam() {
        let path = std::env::temp_dir().join(format!(
            "aethersense_zone_floors_{}_{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time before epoch")
                .as_nanos()
        ));

        // Escreve um config com o piso aprendido por zona e recarrega.
        let mut state = InnerState::new_with_config_path(&path);
        state
            .zone_floors
            .insert("02:00:00:00:00:01".to_string(), 0.08);
        state
            .zone_floors
            .insert("02:00:00:00:00:02".to_string(), 0.13);
        state
            .save_config()
            .expect("config de pisos deve ser persistida");

        let reloaded = InnerState::new_with_config_path(&path);
        assert_eq!(reloaded.zone_floors.get("02:00:00:00:00:01"), Some(&0.08));
        assert_eq!(reloaded.zone_floors.get("02:00:00:00:00:02"), Some(&0.13));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn recalibracao_pendente_sobrevive_restart_e_replay_invalida_coleta() {
        let path = std::env::temp_dir().join(format!(
            "aethersense_shower_pending_{}_{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time before epoch")
                .as_nanos()
        ));

        let mut state = InnerState::new_with_config_path(&path);
        state.clear();
        let required = state
            .shower_box_required_session_id
            .clone()
            .expect("clear deve bloquear o par anterior");
        assert_eq!(
            state.calibration_session_id.as_deref(),
            Some(required.as_str())
        );
        state
            .save_config()
            .expect("bloqueio de recalibração deve ser persistido");

        let reloaded = InnerState::new_with_config_path(&path);
        assert_eq!(
            reloaded.shower_box_required_session_id.as_deref(),
            Some(required.as_str())
        );

        state.start_replay("fixture.csv".to_string());
        assert!(state.calibration_clear_at_s.is_none());
        assert!(state.calibration_session_id.is_none());
        assert_eq!(
            state.shower_box_required_session_id.as_deref(),
            Some(required.as_str())
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn snapshot_assincrono_antigo_nao_sobrescreve_bloqueio_de_recalibracao() {
        let path = std::env::temp_dir().join(format!(
            "aethersense_config_revision_{}_{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time before epoch")
                .as_nanos()
        ));

        let mut state = InnerState::new_with_config_path(&path);
        let stale_json = state
            .serialize_config_json()
            .expect("snapshot antigo deve serializar");
        let stale_revision = state.config_writer.reserve_revision();

        state.clear();
        let required = state
            .shower_box_required_session_id
            .clone()
            .expect("clear deve criar uma sessão obrigatória");
        assert!(state
            .save_config()
            .expect("bloqueio mais novo deve ser persistido"));

        assert!(!state
            .config_writer
            .write_if_current(&path, &stale_json, stale_revision)
            .expect("snapshot antigo deve ser descartado sem erro de I/O"));

        let reloaded = InnerState::new_with_config_path(&path);
        assert_eq!(
            reloaded.shower_box_required_session_id.as_deref(),
            Some(required.as_str())
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn falha_apos_sync_do_temporario_preserva_config_canonico() {
        let path = std::env::temp_dir().join(format!(
            "aethersense_atomic_config_{}_{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time before epoch")
                .as_nanos()
        ));
        std::fs::write(&path, "config-anterior").expect("config anterior deve existir");

        let writer = ConfigWriteCoordinator::default();
        let revision = writer.reserve_revision();
        let error = writer
            .write_if_current_with_hook(&path, "config-nova", revision, |_| {
                Err(std::io::Error::other("falha simulada antes do persist"))
            })
            .expect_err("falha simulada deve ser propagada");

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(
            std::fs::read_to_string(&path).expect("config anterior deve continuar legível"),
            "config-anterior"
        );

        let _ = std::fs::remove_file(path);
    }
}
