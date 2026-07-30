use aethersense_server::csi::packet::CsiPacket;
use aethersense_server::processing::{amplitude, pca, phase, stft};
#[macro_use]
mod common;

use std::collections::BTreeMap;

fn class_snr(snr: f64) -> &'static str {
    if snr > 25.0 { "OTIMO" }
    else if snr >= 15.0 { "BOM" }
    else if snr >= 5.0 { "REGULAR" }
    else { "RUIM" }
}

fn class_cv(cv: f64) -> &'static str {
    if cv < 0.05 { "OTIMO" }
    else if cv <= 0.15 { "BOM" }
    else if cv <= 0.3 { "REGULAR" }
    else { "RUIM" }
}

fn class_phase(var: f64) -> &'static str {
    if var < 0.01 { "OTIMO" }
    else if var <= 0.1 { "BOM" }
    else if var <= 0.5 { "REGULAR" }
    else { "RUIM" }
}

fn class_pl(loss: f64) -> &'static str {
    if loss == 0.0 { "OTIMO" }
    else if loss < 1.0 { "BOM" }
    else if loss <= 5.0 { "REGULAR" }
    else { "RUIM" }
}

fn class_fs(fs: f64) -> &'static str {
    if fs >= 10.0 { "OTIMO" }
    else if fs >= 5.0 { "BOM" }
    else if fs >= 2.0 { "REGULAR" }
    else { "RUIM" }
}

fn class_dr(dr: f64) -> &'static str {
    if dr > 40.0 { "OTIMO" }
    else if dr >= 20.0 { "BOM" }
    else if dr >= 10.0 { "REGULAR" }
    else { "RUIM" }
}

fn class_pca(var: f64) -> &'static str {
    if var > 80.0 { "OTIMO" }
    else if var >= 50.0 { "BOM" }
    else if var >= 30.0 { "REGULAR" }
    else { "RUIM" }
}

fn class_corr(corr: f64) -> &'static str {
    if corr >= 0.9 { "OTIMO" }
    else if corr >= 0.7 { "BOM" }
    else if corr >= 0.5 { "REGULAR" }
    else { "RUIM" }
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }

    values.iter().sum::<f64>() / values.len() as f64
}

fn variance(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }

    let avg = mean(values);
    values.iter().map(|value| (value - avg).powi(2)).sum::<f64>() / values.len() as f64
}

fn median(values: &[f64]) -> f64 {
    let mut sorted: Vec<f64> = values
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .collect();
    if sorted.is_empty() {
        return 0.0;
    }

    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    }
}

fn mean_amplitude(packet: &CsiPacket) -> Option<f64> {
    if packet.amplitudes.is_empty() {
        None
    } else {
        Some(mean(&packet.amplitudes))
    }
}

fn central_phase(packet: &CsiPacket, sanitize_subcarriers: bool) -> Option<f64> {
    if packet.phases.is_empty() {
        return None;
    }

    let phases = if sanitize_subcarriers {
        phase::sanitize_phase_subcarriers(&packet.phases)
    } else {
        packet.phases.clone()
    };

    phases.get(phases.len() / 2).copied()
}

fn phase_residual_variance(packet: &CsiPacket) -> Option<f64> {
    if packet.phases.len() < 8 {
        return None;
    }

    let mut segment_vars = Vec::new();
    let mut segment = Vec::new();

    for (phase_value, amplitude_value) in packet.phases.iter().zip(packet.amplitudes.iter()) {
        let valid = phase_value.is_finite() && amplitude_value.is_finite() && *amplitude_value > 15.0;
        if valid {
            segment.push(*phase_value);
            continue;
        }

        if segment.len() >= 8 {
            if let Some(value) = segment_residual_variance(&segment) {
                segment_vars.push(value);
            }
        }
        segment.clear();
    }

    if segment.len() >= 8 {
        if let Some(value) = segment_residual_variance(&segment) {
            segment_vars.push(value);
        }
    }

    if segment_vars.is_empty() {
        return None;
    }

    Some(median(&segment_vars))
}

fn segment_residual_variance(raw_segment: &[f64]) -> Option<f64> {
    let sanitized = phase::sanitize_phase_subcarriers(raw_segment);
    if sanitized.len() < 8 {
        return None;
    }

    let center = median(&sanitized);
    let deviations: Vec<f64> = sanitized
        .iter()
        .map(|value| (value - center).abs())
        .collect();
    let robust_sigma = 1.4826 * median(&deviations);
    Some(robust_sigma * robust_sigma)
}

fn device_phase_variance(packets: &[CsiPacket]) -> Option<f64> {
    let phase_residual_vars: Vec<f64> = packets.iter().filter_map(phase_residual_variance).collect();
    if phase_residual_vars.len() < 10 {
        None
    } else {
        Some(median(&phase_residual_vars))
    }
}

fn sample_rate_hz(packets: &[CsiPacket]) -> f64 {
    let timestamps: Vec<i64> = packets.iter().filter_map(|packet| packet.timestamp).collect();
    if timestamps.len() < 2 {
        return 0.0;
    }

    let min_ts = timestamps.iter().min().copied().unwrap_or(0);
    let max_ts = timestamps.iter().max().copied().unwrap_or(min_ts);
    let elapsed_secs = (max_ts - min_ts) as f64 / 1_000_000.0;
    if elapsed_secs > 0.0 {
        timestamps.len() as f64 / elapsed_secs
    } else {
        0.0
    }
}

fn packet_loss_by_mac(by_mac: &mut BTreeMap<String, Vec<CsiPacket>>) -> (f64, Vec<String>) {
    let mut worst_loss_rate = 0.0_f64;
    let mut summaries = Vec::new();

    for (mac, device_packets) in by_mac.iter_mut() {
        device_packets.sort_by_key(|packet| packet.seq.unwrap_or(0));
        if device_packets.is_empty() {
            continue;
        }

        let mut expected_seq = device_packets[0].seq.unwrap_or(0);
        let mut lost = 0;
        for packet in device_packets.iter().skip(1) {
            let current_seq = packet.seq.unwrap_or(expected_seq + 1);
            if current_seq > expected_seq + 1 {
                lost += current_seq - expected_seq - 1;
            }
            expected_seq = current_seq;
        }

        let first_seq = device_packets[0].seq.unwrap_or(0);
        let last_seq = device_packets.last().and_then(|packet| packet.seq).unwrap_or(first_seq);
        let total_expected = (last_seq - first_seq + 1).max(device_packets.len() as i32);
        let device_loss_rate = if total_expected > 0 {
            lost as f64 / total_expected as f64 * 100.0
        } else {
            0.0
        };

        worst_loss_rate = worst_loss_rate.max(device_loss_rate);
        summaries.push(format!(
            "{}: {:.1}% loss ({} pacotes, {} drops)",
            mac,
            device_loss_rate,
            device_packets.len(),
            lost
        ));
    }

    (worst_loss_rate, summaries)
}

fn dynamic_range_db(spec: &stft::StftResult) -> f64 {
    let mut min_stft = f64::MAX;
    let mut max_stft = f64::MIN;

    for row in &spec.spectrogram {
        for &value in row {
            min_stft = min_stft.min(value);
            max_stft = max_stft.max(value);
        }
    }

    if min_stft.is_finite() && max_stft.is_finite() {
        max_stft - min_stft
    } else {
        0.0
    }
}

fn pca_pc1_variance_percent(matrix: &[Vec<f64>]) -> f64 {
    let result = pca::compute_pca(matrix, matrix.len());
    let pc1_var = variance(&result.pc1);
    let pc2_var = variance(&result.pc2);
    let total = pc1_var + pc2_var;
    if total > 0.0 {
        pc1_var / total * 100.0
    } else {
        0.0
    }
}

fn adjacent_subcarrier_correlation(packets: &[CsiPacket]) -> f64 {
    let width = packets
        .iter()
        .filter(|packet| !packet.amplitudes.is_empty())
        .map(|packet| packet.amplitudes.len())
        .min()
        .unwrap_or(0);

    if packets.len() < 3 || width < 2 {
        return 0.0;
    }

    let mut correlations = Vec::new();
    for col in 0..(width - 1) {
        let left: Vec<f64> = packets.iter().map(|packet| packet.amplitudes[col]).collect();
        let right: Vec<f64> = packets.iter().map(|packet| packet.amplitudes[col + 1]).collect();
        let left_mean = mean(&left);
        let right_mean = mean(&right);
        let cov = left
            .iter()
            .zip(right.iter())
            .map(|(left, right)| (left - left_mean) * (right - right_mean))
            .sum::<f64>()
            / left.len() as f64;
        let denom = variance(&left).sqrt() * variance(&right).sqrt();
        if denom > 1e-12 {
            correlations.push((cov / denom).abs());
        }
    }

    mean(&correlations)
}

fn label(classification: &str) -> &'static str {
    match classification {
        "OTIMO" => "🏆 OTIMO",
        "BOM" => "✅ BOM",
        "REGULAR" => "🟡 REGULAR",
        _ => "❌ RUIM",
    }
}

#[tokio::test]
async fn m6_benchmark_academico() {
    let captured_packets = common::capture_and_save("m6", "benchmark", 300).await;
    assert!(captured_packets.len() >= 10, "Pacotes insuficientes para o benchmark");

    let mut by_mac: BTreeMap<String, Vec<CsiPacket>> = BTreeMap::new();
    for packet in &captured_packets {
        by_mac.entry(packet.mac.clone()).or_default().push(packet.clone());
    }

    if common::require_live_capture() {
        assert!(
            by_mac.contains_key("02:00:00:00:00:02")
                && by_mac.contains_key("02:00:00:00:00:01"),
            "Benchmark live exige WROOM e C5 presentes; MACs vistos: {:?}",
            by_mac.keys().collect::<Vec<_>>()
        );
    }

    let mut candidates: Vec<(String, Vec<CsiPacket>, f64, f64)> = by_mac
        .iter()
        .filter_map(|(mac, device_packets)| {
            let phase_var = device_phase_variance(device_packets)?;
            let fs = sample_rate_hz(device_packets);
            Some((mac.clone(), device_packets.clone(), phase_var, fs))
        })
        .collect();

    candidates.sort_by(|left, right| {
        let left_rate_ok = left.3 >= 10.0;
        let right_rate_ok = right.3 >= 10.0;

        right_rate_ok
            .cmp(&left_rate_ok)
            .then_with(|| {
                left.2
                    .partial_cmp(&right.2)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| right.1.len().cmp(&left.1.len()))
    });

    let (benchmark_mac, packets) = candidates
        .into_iter()
        .next()
        .map(|(mac, device_packets, _phase_var, _fs)| (mac, device_packets))
        .or_else(|| {
            by_mac
                .iter()
                .max_by_key(|(_, device_packets)| device_packets.len())
                .map(|(mac, device_packets)| (mac.clone(), device_packets.clone()))
        })
        .expect("ao menos um grupo de pacotes por MAC");

    println!(
        "Benchmark usando {} pacotes do dispositivo selecionado {} ({} pacotes totais).",
        packets.len(),
        benchmark_mac,
        captured_packets.len()
    );

    let raw_mean_amps: Vec<f64> = packets.iter().filter_map(mean_amplitude).collect();
    assert!(
        raw_mean_amps.len() >= 10,
        "Pacotes insuficientes com amplitudes validas para o benchmark"
    );

    let mean_amps = amplitude::smooth_signal_series(&raw_mean_amps);
    let residual_noise: Vec<f64> = raw_mean_amps
        .iter()
        .zip(mean_amps.iter())
        .map(|(raw, smooth)| raw - smooth)
        .collect();

    let signal_power = mean(&mean_amps.iter().map(|value| value * value).collect::<Vec<_>>());
    let noise_power = variance(&residual_noise).max(1e-9);
    let snr_db = 10.0 * (signal_power / noise_power).log10();
    let snr_class = class_snr(snr_db);

    let global_mean = mean(&mean_amps);
    let cv = if global_mean.abs() > 1e-12 {
        variance(&mean_amps).sqrt() / global_mean.abs()
    } else {
        1.0
    };
    let cv_class = class_cv(cv);

    let raw_phase_series: Vec<f64> = packets
        .iter()
        .filter_map(|packet| central_phase(packet, false))
        .collect();
    let raw_phase_var = variance(&phase::unwrap_phase(&raw_phase_series));
    let phase_var = device_phase_variance(&packets).expect("Pacotes insuficientes com fases validas para o benchmark");
    assert!(
        packets.iter().filter_map(phase_residual_variance).count() >= 10,
        "Pacotes insuficientes com fases validas para o benchmark"
    );
    let phase_class = class_phase(phase_var);

    let (pl_rate, loss_summaries) = packet_loss_by_mac(&mut by_mac);
    let pl_class = class_pl(pl_rate);

    let fs = sample_rate_hz(&packets);
    let fs_class = class_fs(fs);

    let spec = stft::compute_stft(&mean_amps, 50, fs.max(1.0));
    let dr_db = dynamic_range_db(&spec);
    let dr_class = class_dr(dr_db);

    let matrix: Vec<Vec<f64>> = packets.iter().map(|packet| packet.amplitudes.clone()).collect();
    let pc1_var_pct = pca_pc1_variance_percent(&matrix);
    let pca_class = class_pca(pc1_var_pct);

    let corr_mean = adjacent_subcarrier_correlation(&packets);
    let corr_class = class_corr(corr_mean);

    let classes = [
        snr_class,
        cv_class,
        phase_class,
        pl_class,
        fs_class,
        dr_class,
        pca_class,
        corr_class,
    ];
    let critical_ok = phase_class == "OTIMO" && pl_class == "OTIMO" && fs_class == "OTIMO";
    let num_excelentes = classes.iter().filter(|&&class| class == "OTIMO").count();
    let num_ok = classes
        .iter()
        .filter(|&&class| matches!(class, "OTIMO" | "BOM" | "REGULAR"))
        .count();
    let has_bad_metric = classes.iter().any(|&class| class == "RUIM");
    let result_geral = if critical_ok && num_excelentes >= 4 && !has_bad_metric {
        "OTIMO"
    } else if num_ok >= 6 {
        "BOM"
    } else {
        "REGULAR"
    };

    println!("╔═══════════════════════════════════════════════════════════════╗");
    println!("║               🔬 BENCHMARK AetherSense — Resumo               ║");
    println!("╠══════════════════════════╦══════════╦════════════╦════════════╣");
    println!("║ Métrica                  ║  Valor   ║ Classif.   ║ Referência ║");
    println!("╠══════════════════════════╬══════════╬════════════╬════════════╣");
    println!("║ SNR Estimado             ║ {:>6.1} dB ║ {:<10} ║ >25 dB     ║", snr_db, label(snr_class));
    println!("║ CV Amplitude filtrada    ║ {:>6.2}    ║ {:<10} ║ <0.05      ║", cv, label(cv_class));
    println!("║ Fase sanitizada          ║ {:>6.3}rad ║ {:<10} ║ <0.01      ║", phase_var, label(phase_class));
    println!("║ Packet Loss              ║ {:>6.1} %  ║ {:<10} ║ 0%         ║", pl_rate, label(pl_class));
    println!("║ Taxa Amostragem          ║ {:>6.1} Hz ║ {:<10} ║ >=10 Hz    ║", fs, label(fs_class));
    println!("║ Dynamic Range STFT       ║ {:>6.1} dB ║ {:<10} ║ >40 dB     ║", dr_db, label(dr_class));
    println!("║ Variância PC1 (real)     ║ {:>6.1} %  ║ {:<10} ║ >80%       ║", pc1_var_pct, label(pca_class));
    println!("║ Correl. Subportadoras    ║ {:>6.2}    ║ {:<10} ║ >0.9       ║", corr_mean, label(corr_class));
    println!("╠══════════════════════════╬══════════╬════════════╬════════════╣");
    println!("║ RESULTADO GERAL          ║          ║ {:<10} ║            ║", label(result_geral));
    println!("╚══════════════════════════╩══════════╩════════════╩════════════╝\n");
    println!("Fase bruta central: variancia {:.3} rad antes da sanitizacao.", raw_phase_var);
    println!("Perda por dispositivo:");
    for summary in loss_summaries {
        println!(" • {}", summary);
    }

    if common::require_live_capture() {
        assert!(
            critical_ok,
            "Benchmark live nao atingiu metricas criticas OTIMO: fase={}, perda={}, taxa={}",
            phase_class,
            pl_class,
            fs_class
        );
        assert_eq!(
            result_geral, "OTIMO",
            "Benchmark live nao atingiu resultado geral OTIMO"
        );
    }
}
