/// Sanitização de fase CSI.
///
/// Remove drift linear e aplica média móvel, produzindo uma série
/// de fase "limpa" para análise de micro-movimentos.

/// Filtro de média móvel (moving average).
pub fn moving_average(values: &[f64], window_size: usize) -> Vec<f64> {
    if values.is_empty() {
        return Vec::new();
    }

    let ws = window_size.max(1).min(values.len());
    if ws == 1 {
        return values.to_vec();
    }

    let half = ws / 2;
    let mut result = Vec::with_capacity(values.len());

    for i in 0..values.len() {
        let start = if i >= half { i - half } else { 0 };
        let end = (i + ws - half).min(values.len());
        let sum: f64 = values[start..end].iter().sum();
        let count = (end - start) as f64;
        result.push(sum / count);
    }

    result
}

/// Unwrap de fase: remove saltos de 2π entre amostras consecutivas.
pub fn unwrap_phase(phases: &[f64]) -> Vec<f64> {
    if phases.is_empty() {
        return Vec::new();
    }
    const TWO_PI: f64 = 2.0 * std::f64::consts::PI;
    // Valores não-finitos na entrada contaminariam toda a série (NaN propaga),
    // então o primeiro é saneado para 0.0 e os demais mantêm o valor anterior.
    let first = if phases[0].is_finite() {
        phases[0]
    } else {
        0.0
    };
    let mut result = vec![first];
    for i in 1..phases.len() {
        let current = if phases[i].is_finite() {
            phases[i]
        } else {
            result[i - 1]
        };
        let raw_diff = current - result[i - 1];
        // Reduz a diferença para (-π, π] sem laços (robusto a saltos grandes).
        let diff = raw_diff - TWO_PI * (raw_diff / TWO_PI).round();
        result.push(result[i - 1] + diff);
    }
    result
}

/// Remove tendência linear (detrend) de uma série.
pub fn detrend_linear(values: &[f64]) -> Vec<f64> {
    let n = values.len();
    if n < 2 {
        return values.to_vec();
    }

    // Regressão linear: y = slope * x + intercept
    let n_f = n as f64;
    let sum_x: f64 = (0..n).map(|i| i as f64).sum();
    let sum_y: f64 = values.iter().sum();
    let sum_xy: f64 = values.iter().enumerate().map(|(i, &y)| i as f64 * y).sum();
    let sum_x2: f64 = (0..n).map(|i| (i as f64).powi(2)).sum();

    let denom = n_f * sum_x2 - sum_x.powi(2);
    if denom.abs() < 1e-15 {
        return values.to_vec();
    }

    let slope = (n_f * sum_xy - sum_x * sum_y) / denom;
    let intercept = (sum_y - slope * sum_x) / n_f;

    values
        .iter()
        .enumerate()
        .map(|(i, &y)| y - (slope * i as f64 + intercept))
        .collect()
}

/// Sanitizacao linear por subportadora para reduzir CFO/SFO em um pacote CSI.
pub fn sanitize_phase_subcarriers(raw_phases: &[f64]) -> Vec<f64> {
    if raw_phases.is_empty() {
        return Vec::new();
    }
    if raw_phases.len() < 3 {
        return raw_phases.to_vec();
    }

    let unwrapped = unwrap_phase(raw_phases);
    let detrended = detrend_linear(&unwrapped);
    let window = 15.min(detrended.len()).max(1);
    moving_average(&detrended, window)
}

/// Pipeline completa de sanitização de fase:
/// 1. Unwrap
/// 2. Detrend (remove drift linear)
/// 3. Moving average (suavização)
#[allow(dead_code)]
pub fn sanitize_phase_series(raw_phases: &[f64]) -> Vec<f64> {
    if raw_phases.is_empty() {
        return Vec::new();
    }
    if raw_phases.len() < 3 {
        return raw_phases.to_vec();
    }

    let unwrapped = unwrap_phase(raw_phases);
    let detrended = detrend_linear(&unwrapped);
    moving_average(&detrended, 7)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_moving_average_suaviza() {
        let values = vec![0.0, 10.0, 0.0, 10.0, 0.0];
        let smoothed = moving_average(&values, 3);
        assert_eq!(smoothed.len(), 5);
        // Valores suavizados devem estar entre 0 e 10
        for &v in &smoothed {
            assert!(v >= 0.0 && v <= 10.0);
        }
    }

    #[test]
    fn test_unwrap_phase_remove_saltos() {
        let phases = vec![0.0, 3.0, -3.0, 0.0];
        let unwrapped = unwrap_phase(&phases);
        for i in 1..unwrapped.len() {
            let diff = (unwrapped[i] - unwrapped[i - 1]).abs();
            assert!(diff <= std::f64::consts::PI + 1e-9);
        }
    }

    #[test]
    fn test_detrend_remove_tendencia_linear() {
        // Sinal com tendência linear: y = 2x + ruído
        let values: Vec<f64> = (0..20).map(|i| 2.0 * i as f64 + 1.0).collect();
        let detrended = detrend_linear(&values);
        // A média do detrended deve ser próxima de zero
        let mean: f64 = detrended.iter().sum::<f64>() / detrended.len() as f64;
        assert!(mean.abs() < 1e-9);
    }

    #[test]
    fn test_sanitize_phase_pipeline_completa() {
        let raw = vec![0.1, 0.2, 3.5, -3.0, 0.1, 0.3, 0.4];
        let sanitized = sanitize_phase_series(&raw);
        assert_eq!(sanitized.len(), raw.len());
        for &v in &sanitized {
            assert!(v.is_finite());
        }
    }

    #[test]
    fn test_sanitize_phase_subcarriers_remove_inclinacao_linear() {
        let raw: Vec<f64> = (0..32)
            .map(|i| 0.08 * i as f64 + (i as f64 * 0.3).sin() * 0.01)
            .collect();
        let sanitized = sanitize_phase_subcarriers(&raw);
        let detrended_again = detrend_linear(&sanitized);
        let residual_mean = detrended_again.iter().sum::<f64>() / detrended_again.len() as f64;

        assert_eq!(sanitized.len(), raw.len());
        assert!(residual_mean.abs() < 1e-9);
    }
}
