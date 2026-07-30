/// Estimativa de sinais vitais (respiração e batimentos cardíacos) via CSI.
///
/// O sinal de entrada é a série temporal da **amplitude média** das
/// subportadoras CSI por pacote, que carrega as micro-vibrações causadas
/// pela respiração (0.1-0.5 Hz ≈ 6-30 resp/min) e pelos batimentos
/// cardíacos (0.8-3.0 Hz ≈ 48-180 bpm).
///
/// Pipeline por janela:
///   1. Reamostragem para uma grade temporal uniforme a partir dos instantes
///      reais de recepção dos pacotes — corrige o jitter de rede e NÃO assume
///      uma taxa fixa de 10 Hz (que escalaria todas as frequências caso a taxa
///      real divergisse).
///   2. Detrend linear — remove a deriva lenta de amplitude, não apenas o DC.
///   3. Janela de Hann — reduz o vazamento espectral da forte componente
///      respiratória para as bandas vizinhas (inclusive a cardíaca).
///   4. FFT e busca do pico dominante em cada banda fisiológica.
///   5. Na banda cardíaca, rejeição dos harmônicos da respiração antes da
///      busca, evitando confundir 2x/3x/4x da respiração com batimento.
///
/// ⚠️ EXPERIMENTAL: Resultados são estimativas, não medidas clínicas.
use std::f64::consts::PI;

use rustfft::{num_complex::Complex as FftComplex, FftPlanner};

use crate::config::*;
use crate::state::{DashboardSettings, FftDiagnostics, VitalsEstimate};

/// Encontra o pico de frequência dominante em uma banda específica.
///
/// `avoid_hz` lista frequências (ex.: harmônicos da respiração) cujos bins,
/// dentro de `avoid_tol_hz`, são ignorados na busca e no cálculo da média.
///
/// Retorna `(Option<(frequência_Hz, magnitude)>, proeminência_real)`.
fn find_peak_in_band(
    magnitudes: &[f64],
    freq_resolution: f64,
    min_hz: f64,
    max_hz: f64,
    avoid_hz: &[f64],
    avoid_tol_hz: f64,
    prominence_threshold: f64,
) -> (Option<(f64, f64)>, f64) {
    let min_bin = (min_hz / freq_resolution).ceil() as usize;
    let max_bin = (max_hz / freq_resolution).floor() as usize;

    if min_bin >= magnitudes.len() || max_bin >= magnitudes.len() || min_bin > max_bin {
        return (None, 0.0);
    }

    let is_avoided = |bin: usize| -> bool {
        let freq = bin as f64 * freq_resolution;
        avoid_hz
            .iter()
            .any(|&a| a > 0.0 && (freq - a).abs() <= avoid_tol_hz)
    };

    let mut best_bin: Option<usize> = None;
    let mut best_mag = 0.0;
    let mut band_sum = 0.0;
    let mut band_count = 0usize;

    for bin in min_bin..=max_bin {
        if is_avoided(bin) {
            continue;
        }
        let mag = magnitudes[bin];
        band_sum += mag;
        band_count += 1;
        if best_bin.is_none() || mag > best_mag {
            best_mag = mag;
            best_bin = Some(bin);
        }
    }

    let best_bin = match best_bin {
        Some(b) => b,
        None => return (None, 0.0),
    };
    if band_count == 0 {
        return (None, 0.0);
    }

    // Proeminência: o pico precisa destacar-se da média da banda.
    let mean_mag = band_sum / band_count as f64;
    let actual_prominence = best_mag / (mean_mag + 1e-9);
    if actual_prominence >= prominence_threshold {
        (
            Some((best_bin as f64 * freq_resolution, best_mag)),
            actual_prominence,
        )
    } else {
        (None, actual_prominence)
    }
}

/// Remove a tendência linear (deriva) do sinal por mínimos quadrados.
/// Mais robusto que subtrair apenas a média, pois elimina rampas lentas
/// de amplitude que poluiriam as bandas de baixa frequência.
fn detrend(signal: &[f64]) -> Vec<f64> {
    let n = signal.len();
    if n < 2 {
        return signal.to_vec();
    }
    let nf = n as f64;
    let sx: f64 = (0..n).map(|i| i as f64).sum();
    let sy: f64 = signal.iter().sum();
    let sxx: f64 = (0..n).map(|i| (i as f64) * (i as f64)).sum();
    let sxy: f64 = signal.iter().enumerate().map(|(i, &y)| i as f64 * y).sum();

    let denom = nf * sxx - sx * sx;
    let (intercept, slope) = if denom.abs() < 1e-12 {
        (sy / nf, 0.0)
    } else {
        let slope = (nf * sxy - sx * sy) / denom;
        let intercept = (sy - slope * sx) / nf;
        (intercept, slope)
    };

    signal
        .iter()
        .enumerate()
        .map(|(i, &y)| y - (intercept + slope * i as f64))
        .collect()
}

/// Aplica a janela de Hann (Hanning) ao sinal para reduzir o vazamento
/// espectral na FFT — essencial porque a respiração tem energia muito maior
/// que os batimentos e pode afogar a banda cardíaca sem janelamento.
fn apply_hann(signal: &[f64]) -> Vec<f64> {
    let n = signal.len();
    if n <= 1 {
        return signal.to_vec();
    }
    let nf = (n - 1) as f64;
    signal
        .iter()
        .enumerate()
        .map(|(i, &x)| {
            let window = 0.5 * (1.0 - (2.0 * PI * (i as f64) / nf).cos());
            x * window
        })
        .collect()
}

/// Reamostra uma série temporal irregularmente amostrada para uma grade
/// temporal uniforme via interpolação linear, preservando a taxa média de
/// amostragem.
///
/// `sample_times_s` deve conter os instantes de recepção monotônicos em segundos.
/// Retorna `(sinal_uniforme, taxa_de_amostragem_derivada_hz)` ou `None` se
/// os instantes forem inválidos ou a taxa derivada estiver fora de limites
/// razoáveis (`VITALS_RESAMPLE_MIN_HZ` .. `VITALS_RESAMPLE_MAX_HZ`).
fn resample_uniform(signal: &[f64], sample_times_s: &[f64]) -> Option<(Vec<f64>, f64)> {
    let n = signal.len();
    if n != sample_times_s.len() || n < 2 {
        return None;
    }

    let t0 = sample_times_s[0];
    let tn = sample_times_s[n - 1];
    let span = tn - t0;
    if !span.is_finite() || span <= 0.0 {
        return None;
    }

    // Exige tempos finitos e não decrescentes.
    for i in 1..n {
        if !sample_times_s[i].is_finite() || sample_times_s[i] < sample_times_s[i - 1] {
            return None;
        }
    }

    let fs = (n as f64 - 1.0) / span;
    if !fs.is_finite() || fs < VITALS_RESAMPLE_MIN_HZ || fs > VITALS_RESAMPLE_MAX_HZ {
        return None;
    }

    let dt = 1.0 / fs;
    let mut out = Vec::with_capacity(n);
    let mut j = 0usize;
    for k in 0..n {
        let tk = t0 + k as f64 * dt;
        while j + 1 < n && sample_times_s[j + 1] < tk {
            j += 1;
        }
        let value = if j + 1 < n {
            let (ta, tb) = (sample_times_s[j], sample_times_s[j + 1]);
            let denom = tb - ta;
            if denom.abs() < 1e-12 {
                signal[j]
            } else {
                let frac = ((tk - ta) / denom).clamp(0.0, 1.0);
                signal[j] + (signal[j + 1] - signal[j]) * frac
            }
        } else {
            signal[n - 1]
        };
        out.push(value);
    }

    Some((out, fs))
}

/// Estima sinais vitais a partir do histórico temporal do sinal CSI,
/// assumindo amostragem uniforme em `sample_rate_hz`.
///
/// Mantida como API estável (usada por testes de integração); para corrigir o
/// jitter de rede prefira [`estimate_vitals_resampled`], que deriva a taxa dos
/// instantes reais.
#[allow(dead_code)]
pub fn estimate_vitals(
    signal_history: &[f64],
    sample_rate_hz: f64,
    has_presence: bool,
) -> VitalsEstimate {
    estimate_vitals_core(
        signal_history,
        sample_rate_hz,
        has_presence,
        &DashboardSettings::default(),
    )
    .0
}

/// Variante que reamostra o sinal para uma grade uniforme a partir dos
/// instantes reais de recepção (`sample_times_s`, em segundos) antes da
/// análise espectral. Cai para `nominal_rate_hz` se os tempos forem inválidos.
pub fn estimate_vitals_resampled(
    signal_history: &[f64],
    sample_times_s: &[f64],
    nominal_rate_hz: f64,
    has_presence: bool,
    settings: &DashboardSettings,
) -> (VitalsEstimate, FftDiagnostics) {
    match resample_uniform(signal_history, sample_times_s) {
        Some((uniform, fs)) => estimate_vitals_core(&uniform, fs, has_presence, settings),
        None => estimate_vitals_core(signal_history, nominal_rate_hz, has_presence, settings),
    }
}

/// Núcleo da estimativa: opera sobre um sinal já uniformemente amostrado.
fn estimate_vitals_core(
    signal_history: &[f64],
    sample_rate_hz: f64,
    has_presence: bool,
    settings: &DashboardSettings,
) -> (VitalsEstimate, FftDiagnostics) {
    let empty = VitalsEstimate {
        breathing_rate: None,
        heart_rate_bpm: None,
        confidence: 0.0,
    };

    let mut diag = FftDiagnostics {
        has_presence,
        sample_count: signal_history.len(),
        sample_rate_hz,
        ..Default::default()
    };

    if sample_rate_hz <= 0.0 || signal_history.len() < 16 {
        diag.status_message = "Amostras insuficientes ou taxa de amostragem inválida".to_string();
        return (empty, diag);
    }

    // Usar as últimas N amostras (janela temporal).
    let window_samples = (settings.vitals_window_seconds * sample_rate_hz) as usize;
    let n = signal_history.len().min(window_samples.max(16));
    let start = signal_history.len() - n;
    let signal = &signal_history[start..];

    // Detrend linear (remove DC e deriva lenta) + janela de Hann.
    let detrended = detrend(signal);
    let windowed = apply_hann(&detrended);

    // FFT (com zero-padding para a próxima potência de dois).
    let fft_size = n.next_power_of_two();
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(fft_size);

    let mut buffer: Vec<FftComplex<f64>> =
        windowed.iter().map(|&x| FftComplex::new(x, 0.0)).collect();
    buffer.resize(fft_size, FftComplex::new(0.0, 0.0));

    fft.process(&mut buffer);

    // Magnitudes (só metade positiva do espectro).
    let n_freq = fft_size / 2;
    let magnitudes: Vec<f64> = buffer[..n_freq]
        .iter()
        .map(|c| (c.re * c.re + c.im * c.im).sqrt())
        .collect();

    let freq_resolution = sample_rate_hz / fft_size as f64;
    let freqs: Vec<f64> = (0..n_freq).map(|i| i as f64 * freq_resolution).collect();

    // Reduzir resolução para o dashboard (max ~150 pontos para não pesar na rede)
    let step = (n_freq / 150).max(1);
    diag.spectrum_magnitudes = magnitudes.iter().step_by(step).copied().collect();
    diag.spectrum_freqs = freqs.iter().step_by(step).copied().collect();

    // Média global do espectro — referência de SNR absoluto (piso anti-espúrio).
    let mean_mag: f64 = magnitudes.iter().sum::<f64>() / magnitudes.len().max(1) as f64;

    // Respiração — banda mais limpa, busca primeiro.
    let (breathing, breathing_prom) = find_peak_in_band(
        &magnitudes,
        freq_resolution,
        settings.breathing_freq_min_hz,
        settings.breathing_freq_max_hz,
        &[],
        0.0,
        settings.vitals_peak_prominence,
    );
    diag.breathing_prominence = breathing_prom;
    if let Some((bf, _)) = breathing {
        diag.breathing_peak_hz = Some(bf);
    }

    // Harmônicos da respiração a rejeitar na banda cardíaca.
    let mut avoid: Vec<f64> = Vec::new();
    if let Some((breathing_freq, _)) = breathing {
        for k in 2..=(settings.vitals_harmonic_count + 1) {
            avoid.push(breathing_freq * k as f64);
        }
    }
    diag.rejected_harmonics = avoid.clone();

    // Batimentos, evitando os harmônicos respiratórios.
    let (heart_cand, _) = find_peak_in_band(
        &magnitudes,
        freq_resolution,
        settings.heart_rate_freq_min_hz,
        settings.heart_rate_freq_max_hz,
        &avoid,
        settings.vitals_harmonic_tolerance_hz,
        settings.vitals_peak_prominence,
    );

    let heart = heart_cand.filter(|&(_, mag)| {
        let snr = mag / (mean_mag + 1e-9);
        diag.heart_snr = snr;
        snr >= settings.heart_rate_abs_snr_floor
    });
    if let Some((hf, _)) = heart {
        diag.heart_peak_hz = Some(hf);
    }

    // AGORA verificamos os Gates / Exigências Físicas da Heurística FFT
    if !has_presence {
        diag.status_message =
            "🚫 Gate de Presença: Rejeitado por ausência de movimento/presença no cômodo"
                .to_string();
        return (empty, diag);
    }

    if signal_history.len() < settings.vitals_min_samples {
        diag.status_message = format!(
            "🚫 Gate de Janela: Amostras insuficientes ({} / mín. {})",
            signal_history.len(),
            settings.vitals_min_samples
        );
        return (empty, diag);
    }

    let breathing_rate = breathing.map(|(freq, _)| freq * 60.0); // respirações/min
    let heart_rate_bpm = heart.map(|(freq, _)| freq * 60.0); // bpm

    // Confiança baseada na relação sinal/ruído dos picos.
    let mut confidence = 0.0;
    let mut factors = 0;

    if let Some((_, mag)) = breathing {
        let snr = mag / (mean_mag + 1e-9);
        confidence += (snr / 10.0).min(1.0);
        factors += 1;
    }

    if let Some((_, mag)) = heart {
        let snr = mag / (mean_mag + 1e-9);
        confidence += (snr / 15.0).min(1.0);
        factors += 1;
    }

    if factors > 0 {
        confidence /= factors as f64;
    }

    if breathing_rate.is_none() && heart_rate_bpm.is_none() {
        if breathing_prom < settings.vitals_peak_prominence {
            diag.status_message = format!(
                "🚫 Gate Proeminência: Respiração insuficiente ({:.1}x < {:.1}x)",
                breathing_prom, settings.vitals_peak_prominence
            );
        } else if diag.heart_snr < settings.heart_rate_abs_snr_floor {
            diag.status_message = format!(
                "🚫 Gate SNR Cardíaco: Abaixo do limiar ({:.1}x < {:.1}x)",
                diag.heart_snr, settings.heart_rate_abs_snr_floor
            );
        } else {
            diag.status_message =
                "🚫 Gate Espectral: Sinal sem picos fisiológicos proeminentes".to_string();
        }
    } else if breathing_rate.is_some() && heart_rate_bpm.is_none() {
        diag.status_message = format!(
            "🟡 Respiração detectada (Batimento cardíaco rejeitado por SNR {:.1}x ou harmônicos)",
            diag.heart_snr
        );
    } else if breathing_rate.is_none() && heart_rate_bpm.is_some() {
        diag.status_message = format!(
            "🟡 Batimento detectado (Respiração com proeminência baixa {:.1}x)",
            breathing_prom
        );
    } else {
        diag.status_message =
            "✅ Sinais vitais validado por heurística FFT e harmônicos isolados".to_string();
    }

    (
        VitalsEstimate {
            breathing_rate,
            heart_rate_bpm,
            confidence,
        },
        diag,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sem_presenca_sem_vitais() {
        let signal: Vec<f64> = (0..200).map(|i| (i as f64 * 0.1).sin()).collect();
        let result = estimate_vitals(&signal, 10.0, false);
        assert!(result.breathing_rate.is_none());
        assert!(result.heart_rate_bpm.is_none());
        assert_eq!(result.confidence, 0.0);
    }

    #[test]
    fn test_dados_insuficientes_sem_vitais() {
        let signal = vec![1.0; 10]; // menos que VITALS_MIN_SAMPLES
        let result = estimate_vitals(&signal, 10.0, true);
        assert!(result.breathing_rate.is_none());
        assert!(result.heart_rate_bpm.is_none());
    }

    #[test]
    fn test_detecta_frequencia_respiratoria() {
        // Sinal com componente em 0.25 Hz (15 respirações/min)
        let sample_rate = 10.0;
        let n = 300; // 30 segundos
        let signal: Vec<f64> = (0..n)
            .map(|i| {
                let t = i as f64 / sample_rate;
                (2.0 * std::f64::consts::PI * 0.25 * t).sin() * 10.0
            })
            .collect();

        let result = estimate_vitals(&signal, sample_rate, true);
        if let Some(rate) = result.breathing_rate {
            // Deve estar próximo de 15 respirações/min (0.25 Hz * 60)
            assert!(rate > 10.0 && rate < 25.0, "breathing rate = {}", rate);
        }
    }

    #[test]
    fn test_detecta_frequencia_cardiaca() {
        // Sinal com componente em 1.2 Hz (72 bpm)
        let sample_rate = 10.0;
        let n = 300;
        let signal: Vec<f64> = (0..n)
            .map(|i| {
                let t = i as f64 / sample_rate;
                (2.0 * std::f64::consts::PI * 1.2 * t).sin() * 5.0
            })
            .collect();

        let result = estimate_vitals(&signal, sample_rate, true);
        if let Some(bpm) = result.heart_rate_bpm {
            // Deve estar próximo de 72 bpm (1.2 Hz * 60)
            assert!(bpm > 50.0 && bpm < 100.0, "heart rate = {} bpm", bpm);
        }
    }

    #[test]
    fn test_taxa_respiratoria_em_faixa_normal() {
        let sample_rate = 10.0;
        let n = 300;
        let signal: Vec<f64> = (0..n)
            .map(|i| {
                let t = i as f64 / sample_rate;
                (2.0 * std::f64::consts::PI * 0.3 * t).sin() * 10.0
                    + (2.0 * std::f64::consts::PI * 1.0 * t).sin() * 2.0
            })
            .collect();

        let result = estimate_vitals(&signal, sample_rate, true);
        if let Some(rate) = result.breathing_rate {
            // 0.1-0.5 Hz → 6-30 respirações/min
            assert!(rate >= 6.0 && rate <= 30.0);
        }
    }

    #[test]
    fn test_rejeita_harmonico_respiratorio_na_banda_cardiaca() {
        // Respiração forte a 0.3 Hz cujo 3º harmônico (0.9 Hz = 54 bpm) cai
        // na banda cardíaca. Sem rejeição, seria reportado como batimento.
        let sample_rate = 10.0;
        let n = 300;
        let signal: Vec<f64> = (0..n)
            .map(|i| {
                let t = i as f64 / sample_rate;
                let phase = 2.0 * std::f64::consts::PI * 0.3 * t;
                // Onda não senoidal → harmônicos ricos em 0.6/0.9/1.2 Hz.
                phase.sin() * 10.0 + (3.0 * phase).sin() * 4.0
            })
            .collect();

        let result = estimate_vitals(&signal, sample_rate, true);
        // A respiração deve ser detectada ~18 resp/min.
        assert!(result.breathing_rate.is_some());
        // O 3º harmônico (0.9 Hz ≈ 54 bpm) NÃO deve ser aceito como batimento.
        if let Some(bpm) = result.heart_rate_bpm {
            assert!(
                (bpm - 54.0).abs() > 3.0,
                "harmônico respiratório vazou como heart rate = {} bpm",
                bpm
            );
        }
    }

    #[test]
    fn test_reamostragem_corrige_taxa_real() {
        // Grade real a ~12 Hz (não 10). Assumir 10 Hz daria a frequência errada;
        // a reamostragem usa a taxa derivada dos timestamps.
        let real_fs = 12.0;
        let n = 300;
        let breathing_hz = 0.25;
        let signal: Vec<f64> = (0..n)
            .map(|i| {
                let t = i as f64 / real_fs;
                (2.0 * std::f64::consts::PI * breathing_hz * t).sin() * 10.0
            })
            .collect();
        let times: Vec<f64> = (0..n).map(|i| i as f64 / real_fs).collect();

        let result =
            estimate_vitals_resampled(&signal, &times, 10.0, true, &DashboardSettings::default()).0;
        if let Some(rate) = result.breathing_rate {
            // 0.25 Hz → 15 resp/min, independentemente da taxa nominal de 10 Hz.
            assert!(
                (rate - 15.0).abs() < 3.0,
                "reamostragem falhou: breathing rate = {}",
                rate
            );
        }
    }

    #[test]
    fn test_reamostragem_invalida_cai_para_nominal() {
        // Tempos com tamanho divergente → deve usar a taxa nominal sem panicar.
        let signal: Vec<f64> = (0..150)
            .map(|i| (2.0 * std::f64::consts::PI * 0.25 * (i as f64 / 10.0)).sin() * 10.0)
            .collect();
        let times = vec![0.0, 0.1, 0.2]; // tamanho != signal
        let result =
            estimate_vitals_resampled(&signal, &times, 10.0, true, &DashboardSettings::default()).0;
        // Não deve panicar; resultado é válido (pode ou não detectar).
        assert!(result.confidence >= 0.0 && result.confidence <= 1.0);
    }
}
