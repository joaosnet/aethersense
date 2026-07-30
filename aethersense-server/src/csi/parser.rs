/// Parser de linhas CSI_DATA recebidas do ESP32.
///
/// Converte uma linha de texto no formato `CSI_DATA,seq,mac,rssi,...,"[s0,s1,...]"`
/// em um `CsiPacket` com amostras complexas, amplitudes e fases.
use num_complex::Complex;

use super::packet::CsiPacket;

/// Tenta converter uma string em inteiro, retornando `None` se falhar.
fn parse_int(value: &str) -> Option<i32> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed
        .parse::<i32>()
        .ok()
        .or_else(|| trimmed.parse::<f64>().ok().map(|f| f as i32))
}

/// Tenta converter uma string em inteiro i64 (para timestamps grandes).
fn parse_i64(value: &str) -> Option<i64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed
        .parse::<i64>()
        .ok()
        .or_else(|| trimmed.parse::<f64>().ok().map(|f| f as i64))
}

/// Faz a unwrap de fase: remove saltos maiores que π entre amostras consecutivas.
fn unwrap_phases(phases: &[f64]) -> Vec<f64> {
    if phases.is_empty() {
        return Vec::new();
    }
    let mut result = vec![phases[0]];
    for i in 1..phases.len() {
        let mut diff = phases[i] - result[i - 1];
        while diff > std::f64::consts::PI {
            diff -= 2.0 * std::f64::consts::PI;
        }
        while diff < -std::f64::consts::PI {
            diff += 2.0 * std::f64::consts::PI;
        }
        result.push(result[i - 1] + diff);
    }
    result
}

/// Analisa uma linha CSV no formato CSI_DATA e produz um `CsiPacket`.
///
/// Retorna `None` se a linha não começar com `CSI_DATA,` ou se o parsing falhar.
pub fn parse_csi_packet(line: &str) -> Option<CsiPacket> {
    if !line.starts_with("CSI_DATA,") {
        return None;
    }

    // Usa o crate csv para lidar com campos entre aspas
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .from_reader(line.as_bytes());
    let record = reader.records().next()?.ok()?;
    let parts: Vec<&str> = record.iter().collect();

    if parts.len() < 3 {
        return None;
    }

    // Encontra o índice do payload (campo que começa com "[")
    let payload_index = parts.iter().position(|p| {
        let trimmed = p.trim().trim_matches('"');
        trimmed.starts_with('[')
    })?;

    if payload_index < 2 {
        return None;
    }

    // Campos de dados entre o "CSI_DATA" e o payload
    let data_fields: Vec<&str> = parts[1..payload_index].to_vec();

    // Reconstrói o payload (pode ter sido separado por vírgulas internas)
    let payload_parts: Vec<&str> = parts[payload_index..].to_vec();
    let payload_raw = payload_parts.join(",");
    let payload = payload_raw
        .trim()
        .trim_matches('"')
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();

    let mut packet = CsiPacket::new(line.to_string());
    packet.payload = payload.clone();

    // Mapeia os campos de dados aos atributos do pacote
    let field_names = [
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
    ];

    for (i, &field_name) in field_names.iter().enumerate() {
        if i >= data_fields.len() {
            break;
        }
        let raw_value = data_fields[i];
        match field_name {
            "seq" => packet.seq = parse_int(raw_value),
            "mac" => packet.mac = raw_value.to_string(),
            "rssi" => packet.rssi = parse_int(raw_value),
            "rate" => packet.rate = parse_int(raw_value),
            "sig_mode" => packet.sig_mode = parse_int(raw_value),
            "mcs" => packet.mcs = parse_int(raw_value),
            "cwb" => packet.cwb = parse_int(raw_value),
            "smoothing" => packet.smoothing = parse_int(raw_value),
            "not_sounding" => packet.not_sounding = parse_int(raw_value),
            "aggregation" => packet.aggregation = parse_int(raw_value),
            "stbc" => packet.stbc = parse_int(raw_value),
            "fec_coding" => packet.fec_coding = parse_int(raw_value),
            "sgi" => packet.sgi = parse_int(raw_value),
            "noise_floor" => packet.noise_floor = parse_int(raw_value),
            "ampdu_cnt" => packet.ampdu_cnt = parse_int(raw_value),
            "channel" => packet.channel = parse_int(raw_value),
            "secondary_channel" => packet.secondary_channel = parse_int(raw_value),
            "timestamp" => packet.timestamp = parse_i64(raw_value),
            "ant" => packet.ant = parse_int(raw_value),
            "sig_len" => packet.sig_len = parse_int(raw_value),
            "rx_state" => packet.rx_state = parse_int(raw_value),
            "len" => packet.length = parse_int(raw_value),
            "first_word_invalid" => packet.first_word_invalid = parse_int(raw_value),
            _ => {}
        }
    }

    // Extrai amostras numéricas do payload
    let mut samples: Vec<f64> = Vec::new();
    if !payload.is_empty() {
        for token in payload.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            if let Ok(v) = token.parse::<f64>() {
                samples.push(v);
            }
        }
    }
    packet.samples = samples.clone();

    // Converte pares I/Q em amostras complexas, amplitudes e fases
    if samples.len() >= 2 {
        let pair_count = samples.len() / 2;
        let mut complex_samples = Vec::with_capacity(pair_count);
        let mut amplitudes = Vec::with_capacity(pair_count);
        let mut raw_phases = Vec::with_capacity(pair_count);

        for i in 0..pair_count {
            let imag = samples[i * 2];
            let real = samples[i * 2 + 1];
            let c = Complex::new(real, imag);
            complex_samples.push(c);
            amplitudes.push(c.norm());
            raw_phases.push(c.arg());
        }

        packet.complex_samples = complex_samples;
        packet.amplitudes = amplitudes;
        packet.phases = unwrap_phases(&raw_phases);
    } else {
        packet.complex_samples = Vec::new();
        packet.amplitudes = samples.clone();
        packet.phases = Vec::new();
    }

    Some(packet)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_LINE: &str = r#"CSI_DATA,0,aa:bb:cc:dd:ee:ff,-45,11,1,0,0,0,0,0,0,0,0,-90,0,6,0,12345,0,66,0,128,0,"[10,20,30,40,50,60,70,80]""#;

    #[test]
    fn test_parse_int_valido() {
        assert_eq!(parse_int("42"), Some(42));
        assert_eq!(parse_int(" -10 "), Some(-10));
        assert_eq!(parse_int("3.7"), Some(3));
    }

    #[test]
    fn test_parse_int_invalido() {
        assert_eq!(parse_int(""), None);
        assert_eq!(parse_int("abc"), None);
    }

    #[test]
    fn test_linha_invalida_retorna_none() {
        assert!(parse_csi_packet("GARBAGE_DATA,1,2,3").is_none());
        assert!(parse_csi_packet("").is_none());
        assert!(parse_csi_packet("CSI_DATA,1").is_none());
    }

    #[test]
    fn test_parse_campos_basicos() {
        let pkt = parse_csi_packet(SAMPLE_LINE).expect("deve parsear");
        assert_eq!(pkt.seq, Some(0));
        assert_eq!(pkt.mac, "aa:bb:cc:dd:ee:ff");
        assert_eq!(pkt.rssi, Some(-45));
        assert_eq!(pkt.rate, Some(11));
        assert_eq!(pkt.channel, Some(6));
    }

    #[test]
    fn test_parse_samples_extraidos() {
        let pkt = parse_csi_packet(SAMPLE_LINE).expect("deve parsear");
        assert_eq!(pkt.samples.len(), 8);
        assert_eq!(pkt.samples[0], 10.0);
        assert_eq!(pkt.samples[7], 80.0);
    }

    #[test]
    fn test_complex_samples_computados() {
        let pkt = parse_csi_packet(SAMPLE_LINE).expect("deve parsear");
        // 8 samples → 4 pares I/Q
        assert_eq!(pkt.complex_samples.len(), 4);
        // Primeiro par: imag=10, real=20
        assert!((pkt.complex_samples[0].re - 20.0).abs() < 1e-9);
        assert!((pkt.complex_samples[0].im - 10.0).abs() < 1e-9);
    }

    #[test]
    fn test_amplitudes_calculadas() {
        let pkt = parse_csi_packet(SAMPLE_LINE).expect("deve parsear");
        assert_eq!(pkt.amplitudes.len(), 4);
        // Amplitude do par (real=20, imag=10) = sqrt(400+100) ≈ 22.36
        let expected = (20.0_f64.powi(2) + 10.0_f64.powi(2)).sqrt();
        assert!((pkt.amplitudes[0] - expected).abs() < 1e-6);
    }

    #[test]
    fn test_fases_calculadas() {
        let pkt = parse_csi_packet(SAMPLE_LINE).expect("deve parsear");
        assert_eq!(pkt.phases.len(), 4);
        // Cada fase deve ser um valor finito
        for phase in &pkt.phases {
            assert!(phase.is_finite());
        }
    }

    #[test]
    fn test_unwrap_phases_corrige_saltos() {
        let phases = vec![0.0, 3.0, -3.0, 0.0];
        let unwrapped = unwrap_phases(&phases);
        // Não deve haver saltos maiores que π entre consecutivos
        for i in 1..unwrapped.len() {
            let diff = (unwrapped[i] - unwrapped[i - 1]).abs();
            assert!(diff <= std::f64::consts::PI + 1e-9);
        }
    }
}
