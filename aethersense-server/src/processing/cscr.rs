//! Seleção de subportadoras por **potência na banda fisiológica** e construção de
//! um sinal de vitais mais limpo a partir das subportadoras mais responsivas — em
//! vez da média de TODAS (que dilui a micro-vibração da respiração/batimento com
//! subportadoras inertes ou ruidosas).
//!
//! Inspirado no estado da arte monoantena (SA-WiSense, arXiv:2507.17623): com 1
//! antena não há diversidade espacial, mas há **diversidade de subportadora**. As
//! subportadoras em zonas de Fresnel sensíveis ao alvo concentram energia na banda
//! de respiração/coração; selecioná-las por **razão de potência na banda** (não por
//! variância total, que premia ruído de banda larga) e combiná-las com alinhamento
//! de sinal eleva o SNR do sinal que alimenta a FFT de vitais.
//!
//! **Segurança:** [`build_vitals_input`] só retorna o sinal combinado quando ele
//! supera a média simples na banda; caso contrário retorna `None` e o chamador usa
//! a média — garantindo que a seleção nunca degrade vs o comportamento anterior.

use rustfft::{num_complex::Complex as FftComplex, FftPlanner};

use crate::config::{
    BREATHING_FREQ_MIN_HZ, HEART_RATE_FREQ_MAX_HZ, VITALS_MIN_SAMPLES, VITALS_MIN_SUBCARRIERS,
    VITALS_WINDOW_SECONDS,
};
use crate::processing::amplitude::hampel_filter;

/// Margem mínima de melhoria (em razão de potência na banda) para preferir o sinal
/// combinado à média simples. Abaixo disso, cai para a média (sem regressão).
const VITALS_IMPROVEMENT_MARGIN: f64 = 1.05;
/// Correlação normalizada mínima com a referência para inverter o sinal de uma
/// subportadora no alinhamento (evita "casar" ruído descorrelacionado).
const ALIGN_CORR_FLOOR: f64 = 0.2;

/// Nº de subportadoras presentes em TODOS os frames (evita índice fora de faixa).
fn common_subcarriers(amp_history: &[Vec<f64>]) -> usize {
    amp_history.iter().map(|f| f.len()).min().unwrap_or(0)
}

/// Substitui amostras não-finitas (NaN/Inf vindos de pacotes malformados) pela
/// última amostra finita (ou 0), para que um único frame ruim não elimine a
/// subportadora inteira do cálculo.
fn sanitize_finite(series: &[f64]) -> Vec<f64> {
    let mut out = Vec::with_capacity(series.len());
    let mut last = 0.0_f64;
    for &v in series {
        if v.is_finite() {
            last = v;
            out.push(v);
        } else {
            out.push(last);
        }
    }
    out
}

/// Remove a tendência linear (mínimos quadrados) de uma série.
fn detrend(series: &[f64]) -> Vec<f64> {
    let n = series.len();
    if n < 2 {
        return series.to_vec();
    }
    let nf = n as f64;
    let sx: f64 = (0..n).map(|i| i as f64).sum();
    let sy: f64 = series.iter().sum();
    let sxx: f64 = (0..n).map(|i| (i as f64) * (i as f64)).sum();
    let sxy: f64 = series.iter().enumerate().map(|(i, &y)| i as f64 * y).sum();
    let denom = nf * sxx - sx * sx;
    let (intercept, slope) = if denom.abs() < 1e-12 {
        (sy / nf, 0.0)
    } else {
        let slope = (nf * sxy - sx * sy) / denom;
        (((sy - slope * sx) / nf), slope)
    };
    series
        .iter()
        .enumerate()
        .map(|(i, &y)| y - (intercept + slope * i as f64))
        .collect()
}

/// Normaliza para média 0 e desvio 1. `None` se desvio ~0 (série constante).
fn zscore(series: &[f64]) -> Option<Vec<f64>> {
    let n = series.len();
    if n == 0 {
        return None;
    }
    let mean = series.iter().sum::<f64>() / n as f64;
    let var = series.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / n as f64;
    let std = var.sqrt();
    if std < 1e-9 {
        return None;
    }
    Some(series.iter().map(|v| (v - mean) / std).collect())
}

/// Razão entre a potência espectral na banda fisiológica (`lo`..`hi` Hz) e a
/// potência total — proxy de SNR de sensoriamento, robusto a ruído fora de banda
/// (ex.: a componente ~fs/2 da intercalação de receptores). Em [0, 1].
fn band_power_ratio(series: &[f64], fs: f64, lo: f64, hi: f64) -> f64 {
    let n = series.len();
    if n < 8 || !fs.is_finite() || fs <= 0.0 {
        return 0.0;
    }
    let detrended = detrend(series);
    let nm1 = (n - 1) as f64;
    let windowed: Vec<f64> = detrended
        .iter()
        .enumerate()
        .map(|(i, &x)| {
            let w = 0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / nm1).cos());
            x * w
        })
        .collect();

    let size = n.next_power_of_two();
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(size);
    let mut buf: Vec<FftComplex<f64>> = windowed.iter().map(|&x| FftComplex::new(x, 0.0)).collect();
    buf.resize(size, FftComplex::new(0.0, 0.0));
    fft.process(&mut buf);

    let nfreq = size / 2;
    let mut total = 0.0;
    let mut band = 0.0;
    for (bin, c) in buf[1..nfreq].iter().enumerate() {
        let f = (bin + 1) as f64 * fs / size as f64; // pula DC (bin 0)
        let p = c.re * c.re + c.im * c.im;
        total += p;
        if f >= lo && f <= hi {
            band += p;
        }
    }
    if total <= 0.0 {
        0.0
    } else {
        band / total
    }
}

/// Razão entre duas subportadoras ao longo do tempo (cross-subcarrier ratio).
/// Cancela o ganho multiplicativo de modo-comum (AGC, deriva de distância).
/// Primitiva da literatura (SA-WiSense/FarSense) mantida para experimentação.
#[allow(dead_code)]
pub fn cscr_ratio(amp_history: &[Vec<f64>], a: usize, b: usize) -> Vec<f64> {
    amp_history
        .iter()
        .map(|frame| {
            let num = frame.get(a).copied().unwrap_or(0.0);
            let den = frame.get(b).copied().unwrap_or(0.0);
            if den.abs() < 1e-9 {
                0.0
            } else {
                num / den
            }
        })
        .collect()
}

/// Seleciona as `top_k` subportadoras com maior **razão de potência na banda**
/// fisiológica (não variância total — esta premiaria ruído de banda larga).
/// Ignora subportadoras "mortas" (média ~0). Ordenadas da mais responsiva à menos.
pub fn select_subcarriers_by_snr(amp_history: &[Vec<f64>], top_k: usize, fs: f64) -> Vec<usize> {
    let k = common_subcarriers(amp_history);
    if k == 0 || amp_history.len() < 2 {
        return Vec::new();
    }
    let (lo, hi) = (BREATHING_FREQ_MIN_HZ, HEART_RATE_FREQ_MAX_HZ);

    let mut scored: Vec<(usize, f64)> = Vec::with_capacity(k);
    for sc in 0..k {
        let raw: Vec<f64> = amp_history.iter().map(|f| f[sc]).collect();
        let series = sanitize_finite(&raw);
        let mean = series.iter().sum::<f64>() / series.len() as f64;
        if mean.abs() <= 1e-6 {
            continue; // subportadora morta / sem energia
        }
        let score = band_power_ratio(&series, fs, lo, hi);
        if score.is_finite() && score > 0.0 {
            scored.push((sc, score));
        }
    }

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(top_k.max(1));
    scored.into_iter().map(|(idx, _)| idx).collect()
}

/// Média por frame das subportadoras (`window`), saneada — a base "ingênua"
/// anterior, usada como referência de não-regressão.
fn naive_mean(window: &[Vec<f64>], k: usize) -> Vec<f64> {
    window
        .iter()
        .map(|frame| {
            let mut sum = 0.0;
            let mut n = 0.0;
            for &v in frame.iter().take(k) {
                if v.is_finite() {
                    sum += v;
                    n += 1.0;
                }
            }
            if n > 0.0 {
                sum / n
            } else {
                0.0
            }
        })
        .collect()
}

/// Constrói o sinal temporal de vitais a partir das melhores subportadoras da
/// **janela analisada** (últimos `VITALS_WINDOW_SECONDS`): seleção por potência de
/// banda → sanear → Hampel → z-score → alinhamento de sinal (só inverte quem é
/// correlacionado) → média por frame. Só retorna o resultado se ele superar a
/// média simples na banda (margem [`VITALS_IMPROVEMENT_MARGIN`]); senão `None`
/// (o chamador usa a média simples — sem regressão).
///
/// O comprimento retornado corresponde ao da janela analisada.
pub fn build_vitals_input(amp_history: &[Vec<f64>], top_k: usize, fs: f64) -> Option<Vec<f64>> {
    if amp_history.len() < VITALS_MIN_SAMPLES {
        return None;
    }
    let k = common_subcarriers(amp_history);
    if k < VITALS_MIN_SUBCARRIERS {
        return None;
    }

    // Recorta a MESMA janela que a FFT de vitais analisa (coerência seleção×análise).
    let window_samples = ((VITALS_WINDOW_SECONDS * fs.max(1.0)) as usize).max(VITALS_MIN_SAMPLES);
    let start = amp_history.len().saturating_sub(window_samples);
    let window = &amp_history[start..];
    if window.len() < VITALS_MIN_SAMPLES {
        return None;
    }
    let (lo, hi) = (BREATHING_FREQ_MIN_HZ, HEART_RATE_FREQ_MAX_HZ);

    let selected = select_subcarriers_by_snr(window, top_k, fs);
    if selected.is_empty() {
        return None;
    }

    // Séries z-scored (saneadas) das subportadoras selecionadas, em ordem de SNR.
    let mut zseries: Vec<Vec<f64>> = Vec::with_capacity(selected.len());
    for &sc in &selected {
        let raw: Vec<f64> = window.iter().map(|f| f[sc]).collect();
        let cleaned = hampel_filter(&sanitize_finite(&raw), 7, 3.0);
        if let Some(z) = zscore(&cleaned) {
            zseries.push(z);
        }
    }
    if zseries.is_empty() {
        return None;
    }

    // Alinhamento de sinal: a 1ª (maior SNR) é referência; inverte só séries com
    // correlação NEGATIVA significativa (|corr| >= piso) — não "casa" ruído.
    let reference = zseries[0].clone();
    let n = reference.len() as f64;
    for z in zseries.iter_mut().skip(1) {
        let corr: f64 = z
            .iter()
            .zip(reference.iter())
            .map(|(a, b)| a * b)
            .sum::<f64>()
            / n;
        if corr <= -ALIGN_CORR_FLOOR {
            for v in z.iter_mut() {
                *v = -*v;
            }
        }
    }

    // Combinação: média por frame das séries alinhadas.
    let mut combined = vec![0.0_f64; window.len()];
    for z in &zseries {
        for (o, v) in combined.iter_mut().zip(z.iter()) {
            *o += *v;
        }
    }
    let inv = 1.0 / zseries.len() as f64;
    for o in combined.iter_mut() {
        *o *= inv;
    }

    // Garantia de não-regressão: só usa o combinado se concentrar MAIS potência na
    // banda fisiológica que a média simples da mesma janela.
    let naive = naive_mean(window, k);
    let combined_ratio = band_power_ratio(&combined, fs, lo, hi);
    let naive_ratio = band_power_ratio(&naive, fs, lo, hi);
    if combined_ratio >= naive_ratio * VITALS_IMPROVEMENT_MARGIN {
        Some(combined)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    /// Ruído pseudo-aleatório de BANDA LARGA em [-0.5, 0.5] por (t, sc), via hash
    /// splitmix64 (evita a periodicidade espúria de um LCG simples, que criaria um
    /// tom dentro da banda fisiológica e invalidaria o teste).
    fn noise01(t: usize, sc: usize) -> f64 {
        let mut h = (t as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (sc as u64)
                .wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
                .wrapping_add(0x1656_67B1_9E37_79F9);
        h ^= h >> 30;
        h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        h ^= h >> 27;
        h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
        h ^= h >> 31;
        (h % 100_000) as f64 / 100_000.0 - 0.5
    }

    /// Histórico sintético: `k` subportadoras, `frames` amostras; respiração 0.25 Hz
    /// nas subportadoras de `breathing_subs` (com o sinal indicado) e ruído de banda
    /// larga de amplitude `noise` no resto (e somado às de respiração).
    fn synth(
        frames: usize,
        k: usize,
        fs: f64,
        breathing_subs: &[(usize, f64)],
        noise: f64,
    ) -> Vec<Vec<f64>> {
        let mut hist = Vec::with_capacity(frames);
        for t in 0..frames {
            let tt = t as f64 / fs;
            let mut frame = vec![0.0_f64; k];
            for (sc, item) in frame.iter_mut().enumerate() {
                *item = 50.0 + noise01(t, sc) * noise;
            }
            for &(sc, sign) in breathing_subs {
                if sc < k {
                    frame[sc] += sign * 5.0 * (2.0 * PI * 0.25 * tt).sin();
                }
            }
            hist.push(frame);
        }
        hist
    }

    fn var(s: &[f64]) -> f64 {
        let m = s.iter().sum::<f64>() / s.len() as f64;
        s.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / s.len() as f64
    }

    #[test]
    fn seleciona_por_potencia_de_banda_nao_por_variancia() {
        // sc 3,7,11 = respiração limpa (baixa amplitude); sc 1 = ruído de ALTA
        // variância (sem periodicidade). A seleção por banda deve preferir as de
        // respiração, NÃO a de maior variância.
        let mut hist = synth(300, 20, 10.0, &[(3, 1.0), (7, 1.0), (11, 1.0)], 0.4);
        for (t, frame) in hist.iter_mut().enumerate() {
            frame[1] = 50.0 + noise01(t, 999) * 40.0; // ruído de banda larga ENORME
        }
        let sel = select_subcarriers_by_snr(&hist, 4, 10.0);
        for sc in [3usize, 7, 11] {
            assert!(
                sel.contains(&sc),
                "subportadora {} de respiração deveria entrar: {:?}",
                sc,
                sel
            );
        }
        assert!(
            !sel.contains(&1),
            "subportadora de ruído de alta variância NÃO deveria entrar: {:?}",
            sel
        );
    }

    #[test]
    fn nao_regride_vs_media_quando_ruido_domina() {
        // Poucas subportadoras de respiração entre MUITO ruído: a seleção pode não
        // melhorar — então deve cair para None (chamador usa a média), nunca pior.
        let hist = synth(300, 20, 10.0, &[(5, 1.0)], 8.0);
        if let Some(sig) = build_vitals_input(&hist, 16, 10.0) {
            // Se retornou algo, tem de ser melhor na banda que a média ingênua.
            let naive: Vec<f64> = hist
                .iter()
                .map(|f| f.iter().sum::<f64>() / f.len() as f64)
                .collect();
            let r_sig = band_power_ratio(&sig, 10.0, BREATHING_FREQ_MIN_HZ, HEART_RATE_FREQ_MAX_HZ);
            let r_naive =
                band_power_ratio(&naive, 10.0, BREATHING_FREQ_MIN_HZ, HEART_RATE_FREQ_MAX_HZ);
            assert!(
                r_sig >= r_naive,
                "retornou sinal pior na banda (sig={}, naive={})",
                r_sig,
                r_naive
            );
        }
    }

    #[test]
    fn melhora_e_alinha_sinais_opostos() {
        // Poucas subportadoras de respiração (sinais OPOSTOS) diluídas em MUITAS com
        // ruído moderado: a média simples fica pobre na banda, então a seleção +
        // alinhamento genuinamente melhoram e o combinado supera a média.
        let hist = synth(300, 40, 10.0, &[(3, 1.0), (7, -1.0), (11, 1.0)], 3.0);
        let sig = build_vitals_input(&hist, 8, 10.0).expect("deveria melhorar e retornar Some");
        assert!(var(&sig) > 0.1, "combinado quase plano");
        let naive: Vec<f64> = hist
            .iter()
            .map(|f| f.iter().sum::<f64>() / f.len() as f64)
            .collect();
        let r_sig = band_power_ratio(&sig, 10.0, BREATHING_FREQ_MIN_HZ, HEART_RATE_FREQ_MAX_HZ);
        let r_naive = band_power_ratio(&naive, 10.0, BREATHING_FREQ_MIN_HZ, HEART_RATE_FREQ_MAX_HZ);
        assert!(
            r_sig > r_naive,
            "combinado não superou a média (sig={}, naive={})",
            r_sig,
            r_naive
        );
    }

    #[test]
    fn nan_em_um_frame_nao_elimina_a_subportadora() {
        let mut hist = synth(300, 20, 10.0, &[(3, 1.0), (7, 1.0), (11, 1.0)], 0.4);
        hist[100][3] = f64::NAN; // 1 amostra ruim na melhor portadora
        let sel = select_subcarriers_by_snr(&hist, 4, 10.0);
        assert!(
            sel.contains(&3),
            "NaN num frame eliminou a subportadora boa: {:?}",
            sel
        );
        // E o resultado final deve permanecer finito.
        if let Some(sig) = build_vitals_input(&hist, 8, 10.0) {
            assert!(sig.iter().all(|v| v.is_finite()), "saída contém não-finito");
        }
    }

    #[test]
    fn cscr_ratio_cancela_modo_comum() {
        let frames = 200;
        let fs = 10.0;
        let mut hist = Vec::with_capacity(frames);
        for t in 0..frames {
            let tt = t as f64 / fs;
            let gain = 1.0 + 0.5 * (2.0 * PI * 0.02 * tt).sin();
            let breath = 1.0 + 0.1 * (2.0 * PI * 0.25 * tt).sin();
            hist.push(vec![gain * breath, gain]);
        }
        let ratio = cscr_ratio(&hist, 0, 1);
        let mean = ratio.iter().sum::<f64>() / ratio.len() as f64;
        assert!((mean - 1.0).abs() < 0.05, "razão fora de ~1.0: {}", mean);
    }

    #[test]
    fn dados_insuficientes_retorna_none() {
        let hist = synth(50, 20, 10.0, &[(3, 1.0)], 0.4); // < VITALS_MIN_SAMPLES
        assert!(build_vitals_input(&hist, 8, 10.0).is_none());
        let poucas = synth(200, 3, 10.0, &[(1, 1.0)], 0.4); // < VITALS_MIN_SUBCARRIERS
        assert!(build_vitals_input(&poucas, 8, 10.0).is_none());
    }
}
