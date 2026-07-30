/// Short-Time Fourier Transform (STFT) do sinal de movimento.
///
/// Divide o sinal em segmentos sobrepostos, aplica janela de Hanning
/// e calcula a FFT de cada segmento para produzir um espectrograma.
use rustfft::{num_complex::Complex as FftComplex, FftPlanner};

/// Gera uma janela de Hanning de tamanho `n`.
fn hanning_window(n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| 0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos()))
        .collect()
}

/// Resultado do cálculo STFT.
pub struct StftResult {
    /// Eixo de frequências (Hz)
    pub freq_axis: Vec<f64>,
    /// Eixo de tempo (segundos)
    pub time_axis: Vec<f64>,
    /// Matriz do espectrograma em dB (freq × tempo)
    pub spectrogram: Vec<Vec<f64>>,
}

/// Calcula a STFT de um sinal 1D.
///
/// # Argumentos
/// - `signal`: sinal de entrada (e.g. média das amplitudes ao longo do tempo)
/// - `window_size`: tamanho da janela em amostras
/// - `sample_rate_hz`: taxa de amostragem em Hz
pub fn compute_stft(signal: &[f64], window_size: usize, sample_rate_hz: f64) -> StftResult {
    let empty = StftResult {
        freq_axis: Vec::new(),
        time_axis: Vec::new(),
        spectrogram: Vec::new(),
    };

    if signal.is_empty() {
        return empty;
    }

    let ws = window_size.max(8).min(signal.len());

    // Se o sinal é menor que a janela, faz padding com repetição da borda
    let padded: Vec<f64> = if signal.len() < ws {
        let mut p = signal.to_vec();
        let last = *signal.last().unwrap_or(&0.0);
        p.resize(ws, last);
        p
    } else {
        signal.to_vec()
    };

    let hop_size = (ws / 4).max(1);
    let window = hanning_window(ws);
    // Ganho coerente da janela: compensa a atenuação introduzida pela janela
    // de Hanning (~0.5), tornando as magnitudes em dB comparáveis entre
    // diferentes tamanhos de janela.
    let coherent_gain = window.iter().sum::<f64>().max(1e-9);

    // Eixo de frequências para rfft (0..N/2+1)
    let n_freq = ws / 2 + 1;
    let freq_axis: Vec<f64> = (0..n_freq)
        .map(|i| i as f64 * sample_rate_hz / ws as f64)
        .collect();

    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(ws);

    let mut time_axis: Vec<f64> = Vec::new();
    let mut columns: Vec<Vec<f64>> = Vec::new();

    let mut start = 0;
    while start + ws <= padded.len() {
        let segment = &padded[start..start + ws];

        // Centraliza o segmento (subtrai média)
        let mean: f64 = segment.iter().sum::<f64>() / ws as f64;

        // Aplica janela de Hanning e centralização
        let mut fft_input: Vec<FftComplex<f64>> = segment
            .iter()
            .zip(window.iter())
            .map(|(&s, &w)| FftComplex::new((s - mean) * w, 0.0))
            .collect();

        fft.process(&mut fft_input);

        // Magnitude em dB (só a metade positiva)
        let magnitude_db: Vec<f64> = fft_input[..n_freq]
            .iter()
            .map(|c| 20.0 * (c.norm() / coherent_gain + 1e-9).log10())
            .collect();

        columns.push(magnitude_db);
        time_axis.push((start as f64 + ws as f64 / 2.0) / sample_rate_hz);

        start += hop_size;
    }

    // Se nenhuma coluna foi gerada, usa o último segmento
    if columns.is_empty() && padded.len() >= ws {
        let segment = &padded[padded.len() - ws..];
        let mean: f64 = segment.iter().sum::<f64>() / ws as f64;
        let mut fft_input: Vec<FftComplex<f64>> = segment
            .iter()
            .zip(window.iter())
            .map(|(&s, &w)| FftComplex::new((s - mean) * w, 0.0))
            .collect();
        fft.process(&mut fft_input);
        let magnitude_db: Vec<f64> = fft_input[..n_freq]
            .iter()
            .map(|c| 20.0 * (c.norm() / coherent_gain + 1e-9).log10())
            .collect();
        columns.push(magnitude_db);
        time_axis.push(padded.len() as f64 / sample_rate_hz);
    }

    // A matriz do espectrograma: cada Vec interno é uma coluna de frequência
    // Format: spectrogram[freq_index][time_index]
    if columns.is_empty() {
        return empty;
    }

    let n_time = columns.len();
    let mut spectrogram = vec![vec![0.0; n_time]; n_freq];
    for (t, col) in columns.iter().enumerate() {
        for (f, &val) in col.iter().enumerate() {
            spectrogram[f][t] = val;
        }
    }

    StftResult {
        freq_axis,
        time_axis,
        spectrogram,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hanning_window_periodic_symmetry() {
        let w = hanning_window(8);
        assert_eq!(w.len(), 8);
        assert!((w[0] - 0.0).abs() < 1e-10);
        // Hanning window com divisor N é periódica na janela
        for i in 1..4 {
            assert!((w[i] - w[8 - i]).abs() < 1e-10);
        }
    }

    #[test]
    fn test_stft_vazio() {
        let result = compute_stft(&[], 8, 10.0);
        assert!(result.freq_axis.is_empty());
        assert!(result.time_axis.is_empty());
        assert!(result.spectrogram.is_empty());
    }

    #[test]
    fn test_stft_produz_espectrograma() {
        // Sinal senoidal simples
        let signal: Vec<f64> = (0..100)
            .map(|i| (2.0 * std::f64::consts::PI * 2.0 * i as f64 / 10.0).sin())
            .collect();
        let result = compute_stft(&signal, 32, 10.0);
        assert!(!result.freq_axis.is_empty());
        assert!(!result.time_axis.is_empty());
        assert!(!result.spectrogram.is_empty());
    }

    #[test]
    fn test_stft_eixo_frequencia_correto() {
        let signal: Vec<f64> = (0..64).map(|i| i as f64).collect();
        let result = compute_stft(&signal, 16, 10.0);
        // Eixo de freq deve ir de 0 a sample_rate/2
        assert_eq!(result.freq_axis[0], 0.0);
        let nyquist = 10.0 / 2.0;
        assert!((result.freq_axis.last().unwrap() - nyquist).abs() < 1e-9);
    }
}
