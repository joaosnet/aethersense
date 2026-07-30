//! Extração de features CSI para os modelos ONNX.
//!
//! Replica `ml/preprocessing.py::finalize_window` + `resample_subcarriers`:
//!   1. reamostra cada pacote para 64 subportadoras (interpolação linear);
//!   2. janela temporal = últimos N pacotes (semântica live), com zero-pad à
//!      esquerda quando o histórico é menor que a janela;
//!   3. Z-score por subportadora sobre a janela.
//!
//! O resultado é a matriz achatada (subportadoras-major) esperada pelo
//! tensor (1, 1, S, T).

use std::collections::HashMap;

use crate::state::{DeviceInfo, DeviceSignalHistory};

/// Escolhe o histórico do receptor preferido: ESP32-C5 primeiro (contrato do
/// projeto), senão o dispositivo com mais pacotes acumulados.
pub fn pick_history<'h>(
    devices: &[DeviceInfo],
    histories: &'h HashMap<String, DeviceSignalHistory>,
) -> Option<(String, &'h DeviceSignalHistory)> {
    let c5_ids: Vec<&String> = devices
        .iter()
        .filter(|d| d.role == "receiver" && d.hardware_profile == "esp32c5")
        .map(|d| &d.id)
        .collect();

    for id in &c5_ids {
        if let Some(history) = histories.get(*id) {
            if !history.amplitude_history.is_empty() {
                return Some(((*id).clone(), history));
            }
        }
    }

    histories
        .iter()
        .filter(|(_, h)| !h.amplitude_history.is_empty())
        .max_by_key(|(_, h)| h.amplitude_history.len())
        .map(|(id, h)| (id.clone(), h))
}

/// Interpola linearmente um vetor de amplitudes para `target` subportadoras.
fn resample_subcarriers(values: &[f64], target: usize) -> Vec<f64> {
    let n = values.len();
    if n == 0 {
        return vec![0.0; target];
    }
    if n == target {
        return values.to_vec();
    }
    if n == 1 {
        return vec![values[0]; target];
    }
    (0..target)
        .map(|i| {
            let pos = i as f64 / (target - 1) as f64 * (n - 1) as f64;
            let lo = pos.floor() as usize;
            let hi = (lo + 1).min(n - 1);
            let frac = pos - lo as f64;
            values[lo] * (1.0 - frac) + values[hi] * frac
        })
        .collect()
}

/// Monta a janela (subcarriers × timesteps) achatada em f32, com Z-score por
/// subportadora — pronto para o tensor (1, 1, S, T).
pub fn build_window(amp_history: &[Vec<f64>], subcarriers: usize, timesteps: usize) -> Vec<f32> {
    // Últimos `timesteps` pacotes (janela live); pad à esquerda se faltar.
    let take = amp_history.len().min(timesteps);
    let recent = &amp_history[amp_history.len() - take..];
    let pad = timesteps - take;

    // matriz [subcarriers][timesteps]
    let mut matrix = vec![vec![0.0f64; timesteps]; subcarriers];
    for (t, packet) in recent.iter().enumerate() {
        let resampled = resample_subcarriers(packet, subcarriers);
        for (s, value) in resampled.iter().enumerate() {
            let v = if value.is_finite() { *value } else { 0.0 };
            matrix[s][pad + t] = v;
        }
    }

    // Z-score por subportadora (idêntico ao finalize_window do Python).
    let mut out = Vec::with_capacity(subcarriers * timesteps);
    for row in &matrix {
        let mean = row.iter().sum::<f64>() / timesteps as f64;
        let var = row.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / timesteps as f64;
        let std = var.sqrt() + 1e-6;
        for value in row {
            out.push(((value - mean) / std) as f32);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_window_tem_shape_e_zscore() {
        let history: Vec<Vec<f64>> = (0..60)
            .map(|t| (0..30).map(|s| (t * s) as f64).collect())
            .collect();
        let window = build_window(&history, 64, 50);
        assert_eq!(window.len(), 64 * 50);
        // Cada linha (subportadora) deve ter média ~0 após o z-score.
        let row: Vec<f32> = window[64 * 50 - 50..].to_vec();
        let mean: f32 = row.iter().sum::<f32>() / 50.0;
        assert!(mean.abs() < 1e-3, "média da linha = {mean}");
    }

    #[test]
    fn build_window_com_historico_curto_faz_pad() {
        let history: Vec<Vec<f64>> = (0..10)
            .map(|_| (0..64).map(|s| s as f64).collect())
            .collect();
        let window = build_window(&history, 64, 50);
        assert_eq!(window.len(), 64 * 50);
        assert!(window.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn resample_interpola_extremos() {
        let values = vec![0.0, 10.0];
        let resampled = resample_subcarriers(&values, 5);
        assert_eq!(resampled.len(), 5);
        assert!((resampled[0] - 0.0).abs() < 1e-9);
        assert!((resampled[4] - 10.0).abs() < 1e-9);
        assert!((resampled[2] - 5.0).abs() < 1e-9);
    }
}
