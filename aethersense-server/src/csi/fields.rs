/// Nomes dos campos presentes em cada linha `CSI_DATA` enviada pelo ESP32.
///
/// A ordem segue exatamente o formato de saída do firmware em `main.c`.
/// Os três últimos campos (`len`, `first_word_invalid`, `payload`) vêm
/// logo antes do array de samples.

#[allow(dead_code)]
pub const CSI_FIELD_NAMES: &[&str] = &[
    "seq",
    "mac",
    "rssi",
    "rate",
    "sig_mode",
    "mcs",
    "cwb",
    "smoothing",
    "not_sounding",
    "aggregation",
    "stbc",
    "fec_coding",
    "sgi",
    "noise_floor",
    "ampdu_cnt",
    "channel",
    "secondary_channel",
    "timestamp",
    "ant",
    "sig_len",
    "rx_state",
    "len",
    "first_word_invalid",
    "payload",
];
