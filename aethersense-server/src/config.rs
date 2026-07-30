/// Constantes e configurações do servidor AetherSense.
///
/// Valores migrados do dashboard Python original, com adições
/// para os novos módulos de detecção e localização.
use clap::Parser;

// ── Rede ──────────────────────────────────────────────────────────
pub const UDP_PORT: u16 = 5005;
pub const WEB_PORT: u16 = 8000;
pub const WEBSOCKET_INTERVAL_MS: u64 = 200;
pub const MQTT_DEFAULT_BASE_TOPIC: &str = "aethersense";
pub const MQTT_DEFAULT_DISCOVERY_PREFIX: &str = "homeassistant";
pub const MQTT_DEFAULT_CLIENT_ID: &str = "aethersense-server";
pub const MQTT_DEFAULT_BROKER_HOST: &str = "core-mosquitto";
pub const MQTT_DEFAULT_BROKER_PORT: u16 = 1883;
pub const MQTT_DEFAULT_KEEP_ALIVE_SECS: u16 = 30;
pub const MQTT_DEFAULT_PUBLISH_INTERVAL_MS: u64 = 500;

// ── Histórico ─────────────────────────────────────────────────────
// 512 (antes 180) para acomodar a janela do VitalsNet ONNX (500 timesteps);
// custo de memória: 512 × ~64 subportadoras × 8 B ≈ 256 KB por dispositivo.
pub const HEATMAP_HISTORY_LENGTH: usize = 512;
pub const HEATMAP_MAX_SUBCARRIERS: usize = 256;

// ── Inferência ML (ONNX) ──────────────────────────────────────────
pub const ML_TARGET_SUBCARRIERS: usize = 64;
pub const ML_OCCUPANCY_TIMESTEPS: usize = 50;
pub const ML_VITALS_TIMESTEPS: usize = 500;
/// Mínimo de pacotes no histórico para rodar o occupancy (janela de 50).
pub const ML_OCCUPANCY_MIN_PACKETS: usize = 25;
/// Mínimo de pacotes para rodar o VitalsNet (janela de 500).
pub const ML_VITALS_MIN_PACKETS: usize = 250;
/// Limiar de confiança para extrair pessoas do heatmap (igual ao Python).
pub const ML_HEATMAP_PERSON_THRESHOLD: f64 = 0.35;

// ── Defaults dos sliders ──────────────────────────────────────────
pub const DEFAULT_HEATMAP_WINDOW: usize = 120;
pub const DEFAULT_STFT_WINDOW: usize = 80;
pub const DEFAULT_PCA_WINDOW: usize = 120;
pub const DEFAULT_PADDING: f64 = 0.15;
pub const STFT_SAMPLE_RATE_HZ: f64 = 10.0;

/// Espaçamento entre subportadoras 802.11 em canal de 20 MHz (20 MHz / 64).
/// Usado para derivar o eixo de atraso (delay) físico do PDP.
pub const CSI_SUBCARRIER_SPACING_HZ: f64 = 312_500.0;

// ── Detecção ──────────────────────────────────────────────────────
/// Limiar absoluto de variância (fallback quando não há baseline de sala vazia).
pub const PRESENCE_VARIANCE_THRESHOLD: f64 = 5.0;
/// Presença RELATIVA ao baseline de sala vazia (por dispositivo): detecta quando
/// a variância atual supera `baseline_vazio × PRESENCE_BASELINE_MULT`. Um limiar
/// absoluto fixo é frágil — o piso de ruído varia por nó/distância (ex.: o C5 tem
/// variância de fundo ~17, bem acima de 5, e ficava sempre "presente").
pub const PRESENCE_BASELINE_MULT: f64 = 3.0;
/// Piso do limiar relativo: evita hipersensibilidade quando o baseline é minúsculo.
pub const PRESENCE_VARIANCE_FLOOR: f64 = 4.0;
pub const PRESENCE_WINDOW_SIZE: usize = 30;
pub const PRESENCE_HOLD_SECONDS: f64 = 20.0;
pub const MOVEMENT_DIFF_THRESHOLD: f64 = 2.0;
pub const VITALS_WINDOW_SECONDS: f64 = 30.0;
pub const VITALS_MIN_SAMPLES: usize = 100;
pub const BREATHING_FREQ_MIN_HZ: f64 = 0.1;
#[allow(dead_code)]
pub const BREATHING_FREQ_MAX_HZ: f64 = 0.5;
#[allow(dead_code)]
pub const HEART_RATE_FREQ_MIN_HZ: f64 = 0.8;
pub const HEART_RATE_FREQ_MAX_HZ: f64 = 3.0;

/// Quão maior que a média da banda o pico precisa ser para ser aceito
/// (proeminência mínima; filtra picos espúrios em bandas estreitas).
#[allow(dead_code)]
pub const VITALS_PEAK_PROMINENCE: f64 = 2.0;
/// Número de harmônicos da respiração rejeitados na banda cardíaca.
/// A respiração a 0.3 Hz produz harmônicos em 0.6/0.9/1.2 Hz que caem
/// na banda do coração e seriam confundidos com batimento.
#[allow(dead_code)]
pub const VITALS_HARMONIC_COUNT: usize = 5;
/// Tolerância (Hz) ao redor de cada harmônico para mascarar bins.
#[allow(dead_code)]
pub const VITALS_HARMONIC_TOLERANCE_HZ: f64 = 0.06;
/// Piso de SNR ABSOLUTO (pico / média global do espectro) para aceitar o pico
/// cardíaco. A banda cardíaca quase plana sempre tem um pico alguns x acima da
/// própria média; sem este piso vs o espectro GLOBAL, ruído vira "batimento"
/// espúrio publicado ao HA. Respiração pura não deve gerar BPM.
#[allow(dead_code)]
pub const HEART_RATE_ABS_SNR_FLOOR: f64 = 3.0;
/// Faixa plausível para a taxa de amostragem derivada dos timestamps reais.
/// Fora dela, a reamostragem é descartada e usa-se a taxa nominal.
pub const VITALS_RESAMPLE_MIN_HZ: f64 = 0.5;
pub const VITALS_RESAMPLE_MAX_HZ: f64 = 50.0;

/// Nº de subportadoras de maior SNR de sensoriamento combinadas no sinal de
/// vitais (em vez da média de TODAS, que dilui a micro-vibração com subportadoras
/// inertes). Diversidade de subportadora supre a falta de diversidade espacial
/// numa única antena (ver `processing::cscr`).
#[allow(dead_code)]
pub const VITALS_SUBCARRIER_TOP_K: usize = 16;
/// Mínimo de subportadoras úteis para usar a seleção; abaixo disso, cai para a
/// média simples de amplitude (compatível com o comportamento anterior).
pub const VITALS_MIN_SUBCARRIERS: usize = 6;

// ── Confiança do VitalsNet (ONNX) ────────────────────────────────
// A confiança do VitalsNet deixa de ser fixa: passa a ser DERIVADA da
// concordância entre a taxa respiratória do ONNX e a da FFT (detection::vitals),
// ponderada pela confiança da própria FFT. Modelo-de-regressão sempre emite um
// valor na faixa fisiológica; só quando a FFT (sinal real) concorda é que
// ganhamos evidência de que o número significa algo.
/// Tolerância (resp/min) de concordância ONNX×FFT na respiração: discrepância
/// nula → confiança máxima; ≥ esta tolerância → confiança mínima.
pub const VITALS_AGREEMENT_TOLERANCE_RPM: f64 = 6.0;
/// Piso de confiança quando a FFT não está disponível/sem evidência (não há com
/// que concordar; mantém-se conservador em vez de afirmar certeza).
pub const VITALS_CONF_FLOOR: f64 = 0.2;
/// Teto de confiança do VitalsNet (mesmo em concordância perfeita, é estimativa).
pub const VITALS_CONF_CEIL: f64 = 0.9;

// ── CLI ───────────────────────────────────────────────────────────
#[derive(Parser, Debug, Clone)]
#[command(name = "aethersense-server")]
#[command(about = "Servidor de captura e análise de dados CSI do ESP32")]
pub struct CliArgs {
    /// Endereço para escutar conexões HTTP
    #[arg(long, env = "AETHERSENSE_HOST", default_value = "0.0.0.0")]
    pub host: String,

    /// Porta do servidor web
    #[arg(long, env = "AETHERSENSE_PORT", default_value_t = WEB_PORT)]
    pub port: u16,

    /// Porta UDP para receber dados CSI
    #[arg(long, env = "AETHERSENSE_UDP_PORT", default_value_t = UDP_PORT)]
    pub udp_port: u16,

    /// Habilita publicação MQTT para Home Assistant
    #[arg(long, env = "AETHERSENSE_MQTT_ENABLED", default_value_t = false)]
    pub mqtt_enabled: bool,

    /// Host do broker MQTT
    #[arg(long, env = "AETHERSENSE_MQTT_HOST", default_value = MQTT_DEFAULT_BROKER_HOST)]
    pub mqtt_host: String,

    /// Porta do broker MQTT
    #[arg(long, env = "AETHERSENSE_MQTT_PORT", default_value_t = MQTT_DEFAULT_BROKER_PORT)]
    pub mqtt_port: u16,

    /// Usuário do broker MQTT
    #[arg(long, env = "AETHERSENSE_MQTT_USERNAME")]
    pub mqtt_username: Option<String>,

    /// Senha do broker MQTT
    #[arg(long, env = "AETHERSENSE_MQTT_PASSWORD")]
    pub mqtt_password: Option<String>,

    /// Client ID do backend no broker MQTT
    #[arg(long, env = "AETHERSENSE_MQTT_CLIENT_ID", default_value = MQTT_DEFAULT_CLIENT_ID)]
    pub mqtt_client_id: String,

    /// Base topic usado para estado e disponibilidade
    #[arg(long, env = "AETHERSENSE_MQTT_BASE_TOPIC", default_value = MQTT_DEFAULT_BASE_TOPIC)]
    pub mqtt_base_topic: String,

    /// Prefixo do MQTT discovery do Home Assistant
    #[arg(long, env = "AETHERSENSE_MQTT_DISCOVERY_PREFIX", default_value = MQTT_DEFAULT_DISCOVERY_PREFIX)]
    pub mqtt_discovery_prefix: String,

    /// Mantém retained os tópicos MQTT publicados
    #[arg(long, env = "AETHERSENSE_MQTT_RETAIN", default_value_t = true)]
    pub mqtt_retain: bool,

    /// Intervalo, em ms, para publicar estado via MQTT
    #[arg(long, env = "AETHERSENSE_MQTT_PUBLISH_INTERVAL_MS", default_value_t = MQTT_DEFAULT_PUBLISH_INTERVAL_MS)]
    pub mqtt_publish_interval_ms: u64,

    /// Keep alive do cliente MQTT em segundos
    #[arg(long, env = "AETHERSENSE_MQTT_KEEP_ALIVE_SECS", default_value_t = MQTT_DEFAULT_KEEP_ALIVE_SECS)]
    pub mqtt_keep_alive_secs: u16,

    /// Diretório com os modelos ONNX (occupancy.onnx, occupancy_metadata.json,
    /// vitals.onnx). Sem modelos, o servidor segue 100% na heurística.
    #[arg(long, env = "AETHERSENSE_MODELS_DIR", default_value = "models")]
    pub models_dir: String,

    /// Diretório onde as sessões de validação/resultados são persistidas
    /// automaticamente (POST /api/results/{categoria}). No Docker é montado
    /// como bind na pasta `resultados/` da raiz do repositório, categorizada
    /// por tipo de sessão — legível por humanos e agentes de IA.
    #[arg(long, env = "AETHERSENSE_RESULTS_DIR", default_value = "resultados")]
    pub results_dir: String,
}
