/// Processamento de amplitude das subportadoras CSI.
///
/// Inclui normalização de séries e construção de matrizes para heatmaps.
use crate::config::HEATMAP_MAX_SUBCARRIERS;

/// Garante que um valor float é finito; substitui NaN/Inf por 0.0.
pub fn sanitize_float(value: f64) -> f64 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

/// Normaliza uma série para um comprimento fixo, preenchendo com zeros.
pub fn normalize_series(values: &[f64], max_length: usize) -> Vec<f64> {
    let mut result: Vec<f64> = values
        .iter()
        .take(max_length)
        .map(|&v| sanitize_float(v))
        .collect();

    if result.len() < max_length {
        result.resize(max_length, 0.0);
    }
    result
}

/// Constrói uma matriz de heatmap a partir do histórico de amplitudes.
///
/// Cada linha é normalizada para ter a mesma largura.
pub fn build_heatmap_matrix(history: &[Vec<f64>]) -> Vec<Vec<f64>> {
    if history.is_empty() {
        return Vec::new();
    }

    let width = history
        .iter()
        .map(|row| row.len())
        .max()
        .unwrap_or(0)
        .min(HEATMAP_MAX_SUBCARRIERS);

    if width == 0 {
        return Vec::new();
    }

    history
        .iter()
        .map(|row| normalize_series(row, width))
        .collect()
}

/// Filtro de Hampel para remover outliers impulsivos em uma série temporal.
pub fn hampel_filter(values: &[f64], window_size: usize, n_sigmas: f64) -> Vec<f64> {
    if values.is_empty() {
        return Vec::new();
    }

    let ws = window_size.max(3).min(values.len());
    let half = ws / 2;
    let threshold = n_sigmas.max(0.5);
    let mut result = Vec::with_capacity(values.len());

    for i in 0..values.len() {
        let start = i.saturating_sub(half);
        let end = (i + half + 1).min(values.len());
        let window = &values[start..end];
        let median = median(window);
        let mad = median_absolute_deviation(window, median);
        let robust_sigma = 1.4826 * mad;
        let value = sanitize_float(values[i]);

        if robust_sigma > 1e-12 && (value - median).abs() > threshold * robust_sigma {
            result.push(median);
        } else {
            result.push(value);
        }
    }

    result
}

/// Média móvel exponencial para reduzir jitter preservando tendência recente.
pub fn exponential_moving_average(values: &[f64], alpha: f64) -> Vec<f64> {
    if values.is_empty() {
        return Vec::new();
    }

    let alpha = alpha.clamp(0.01, 1.0);
    let mut result = Vec::with_capacity(values.len());
    let mut previous = sanitize_float(values[0]);
    result.push(previous);

    for &value in values.iter().skip(1) {
        let current = sanitize_float(value);
        previous = alpha * current + (1.0 - alpha) * previous;
        result.push(previous);
    }

    result
}

/// Pipeline padrao para metricas temporais de amplitude CSI.
pub fn smooth_signal_series(values: &[f64]) -> Vec<f64> {
    let filtered = hampel_filter(values, 7, 3.0);
    exponential_moving_average(&filtered, 0.35)
}

/// Retorna um heatmap limitado às últimas `window_size` entradas.
pub fn compute_windowed_heatmap(history: &[Vec<f64>], window_size: usize) -> Vec<Vec<f64>> {
    if history.is_empty() {
        return Vec::new();
    }

    let window = window_size.max(1);
    let start = if history.len() > window {
        history.len() - window
    } else {
        0
    };

    build_heatmap_matrix(&history[start..])
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

fn median_absolute_deviation(values: &[f64], median_value: f64) -> f64 {
    let deviations: Vec<f64> = values
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .map(|value| (value - median_value).abs())
        .collect();
    median(&deviations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_float_finito() {
        assert_eq!(sanitize_float(3.14), 3.14);
        assert_eq!(sanitize_float(-1.0), -1.0);
    }

    #[test]
    fn test_sanitize_float_nan_inf() {
        assert_eq!(sanitize_float(f64::NAN), 0.0);
        assert_eq!(sanitize_float(f64::INFINITY), 0.0);
        assert_eq!(sanitize_float(f64::NEG_INFINITY), 0.0);
    }

    #[test]
    fn test_normalize_series_comprimento() {
        let v = vec![1.0, 2.0, 3.0];
        let n = normalize_series(&v, 5);
        assert_eq!(n.len(), 5);
        assert_eq!(n[3], 0.0);
        assert_eq!(n[4], 0.0);
    }

    #[test]
    fn test_normalize_series_trunca() {
        let v = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let n = normalize_series(&v, 3);
        assert_eq!(n.len(), 3);
        assert_eq!(n, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn test_build_heatmap_dimensoes() {
        let history = vec![vec![1.0, 2.0], vec![3.0, 4.0, 5.0]];
        let matrix = build_heatmap_matrix(&history);
        assert_eq!(matrix.len(), 2);
        assert_eq!(matrix[0].len(), 3); // largura máxima
        assert_eq!(matrix[1].len(), 3);
    }

    #[test]
    fn test_windowed_heatmap_limita_janela() {
        let history: Vec<Vec<f64>> = (0..10).map(|i| vec![i as f64]).collect();
        let hm = compute_windowed_heatmap(&history, 3);
        assert_eq!(hm.len(), 3);
    }

    #[test]
    fn test_hampel_substitui_outlier() {
        let values = vec![10.0, 10.2, 9.9, 100.0, 10.1, 10.0, 9.8];
        let filtered = hampel_filter(&values, 5, 2.0);
        assert!(filtered[3] < 20.0);
    }

    #[test]
    fn test_ema_preserva_comprimento() {
        let values = vec![1.0, 3.0, 5.0, 7.0];
        let smoothed = exponential_moving_average(&values, 0.5);
        assert_eq!(smoothed.len(), values.len());
        assert!(smoothed[3] < values[3]);
    }
}
