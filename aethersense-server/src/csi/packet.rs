/// Estrutura de um pacote CSI recebido do ESP32.
///
/// Cada pacote contém metadados da camada física (RSSI, MCS, canal, etc.)
/// e o payload com as amostras I/Q das subportadoras OFDM.
use num_complex::Complex;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct CsiPacket {
    pub raw_line: String,
    pub seq: Option<i32>,
    pub mac: String,
    pub rssi: Option<i32>,
    pub rate: Option<i32>,
    pub sig_mode: Option<i32>,
    pub mcs: Option<i32>,
    pub cwb: Option<i32>,
    pub smoothing: Option<i32>,
    pub not_sounding: Option<i32>,
    pub aggregation: Option<i32>,
    pub stbc: Option<i32>,
    pub fec_coding: Option<i32>,
    pub sgi: Option<i32>,
    pub noise_floor: Option<i32>,
    pub ampdu_cnt: Option<i32>,
    pub channel: Option<i32>,
    pub secondary_channel: Option<i32>,
    pub timestamp: Option<i64>,
    pub ant: Option<i32>,
    pub sig_len: Option<i32>,
    pub rx_state: Option<i32>,
    pub length: Option<i32>,
    pub first_word_invalid: Option<i32>,
    pub payload: String,
    pub samples: Vec<f64>,
    #[serde(skip)]
    pub complex_samples: Vec<Complex<f64>>,
    pub amplitudes: Vec<f64>,
    pub phases: Vec<f64>,
}

impl CsiPacket {
    /// Cria um pacote vazio a partir de uma linha bruta.
    pub fn new(raw_line: String) -> Self {
        Self {
            raw_line,
            seq: None,
            mac: String::new(),
            rssi: None,
            rate: None,
            sig_mode: None,
            mcs: None,
            cwb: None,
            smoothing: None,
            not_sounding: None,
            aggregation: None,
            stbc: None,
            fec_coding: None,
            sgi: None,
            noise_floor: None,
            ampdu_cnt: None,
            channel: None,
            secondary_channel: None,
            timestamp: None,
            ant: None,
            sig_len: None,
            rx_state: None,
            length: None,
            first_word_invalid: None,
            payload: String::new(),
            samples: Vec::new(),
            complex_samples: Vec::new(),
            amplitudes: Vec::new(),
            phases: Vec::new(),
        }
    }
}
