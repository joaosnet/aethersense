/// Power Delay Profile (PDP) via IFFT.
///
/// O PDP mostra a distribuição de potência ao longo dos atrasos de
/// propagação, útil para analisar reflexões e multicaminho do WiFi.
use num_complex::Complex;
use rustfft::FftPlanner;

/// Resultado do cálculo PDP.
pub struct PdpResult {
    /// Eixo de atraso em microsegundos
    pub delay_axis: Vec<f64>,
    /// Potência em dB
    pub power_db: Vec<f64>,
}

/// Calcula o Power Delay Profile a partir das amostras complexas CSI.
///
/// Usa IFFT de 256 pontos das amostras no domínio da frequência
/// para obter a resposta ao impulso no domínio do tempo.
///
/// `subcarrier_spacing_hz` é o espaçamento entre subportadoras (Δf, ex.:
/// 312.5 kHz em 802.11 20 MHz). A resolução de atraso da IFFT é
/// Δτ = 1 / (N · Δf), de onde se deriva o eixo de atraso físico.
pub fn compute_pdp(complex_samples: &[Complex<f64>], subcarrier_spacing_hz: f64) -> PdpResult {
    let empty = PdpResult {
        delay_axis: Vec::new(),
        power_db: Vec::new(),
    };

    if complex_samples.is_empty() {
        return empty;
    }

    let fft_size = 256;

    // Prepara o buffer de entrada para IFFT
    let mut buffer: Vec<rustfft::num_complex::Complex<f64>> = complex_samples
        .iter()
        .map(|c| rustfft::num_complex::Complex::new(c.re, c.im))
        .collect();

    // Zero-pad até fft_size
    buffer.resize(fft_size, rustfft::num_complex::Complex::new(0.0, 0.0));

    // IFFT
    let mut planner = FftPlanner::new();
    let ifft = planner.plan_fft_inverse(fft_size);
    ifft.process(&mut buffer);

    // Normalizar (IFFT do rustfft não normaliza)
    let scale = 1.0 / fft_size as f64;

    // Potência em dB
    let power_db: Vec<f64> = buffer
        .iter()
        .map(|c| {
            let magnitude = (c.re * c.re + c.im * c.im).sqrt() * scale;
            20.0 * (magnitude + 1e-9).log10()
        })
        .collect();

    // Eixo de atraso físico: Δτ = 1 / (N · Δf), convertido para microsegundos.
    // Ex.: N=256, Δf=312.5 kHz → span ≈ 3.2 µs (e não 10 µs fixos).
    let spacing = if subcarrier_spacing_hz > 0.0 {
        subcarrier_spacing_hz
    } else {
        312_500.0
    };
    let delay_step_us = 1.0e6 / (fft_size as f64 * spacing);
    let delay_axis: Vec<f64> = (0..fft_size).map(|i| i as f64 * delay_step_us).collect();

    PdpResult {
        delay_axis,
        power_db,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pdp_vazio() {
        let result = compute_pdp(&[], 312_500.0);
        assert!(result.delay_axis.is_empty());
        assert!(result.power_db.is_empty());
    }

    const TEST_SPACING_HZ: f64 = 312_500.0;

    #[test]
    fn test_pdp_formato_correto() {
        let samples: Vec<Complex<f64>> = (0..64)
            .map(|i| Complex::new((i as f64 * 0.1).cos(), (i as f64 * 0.1).sin()))
            .collect();
        let result = compute_pdp(&samples, TEST_SPACING_HZ);
        assert_eq!(result.delay_axis.len(), 256);
        assert_eq!(result.power_db.len(), 256);
    }

    #[test]
    fn test_pdp_valores_em_db() {
        let samples: Vec<Complex<f64>> = (0..32)
            .map(|i| Complex::new(10.0 * (i as f64 * 0.2).cos(), 5.0))
            .collect();
        let result = compute_pdp(&samples, TEST_SPACING_HZ);
        // Todos os valores devem ser finitos
        for &v in &result.power_db {
            assert!(v.is_finite());
        }
    }

    #[test]
    fn test_pdp_eixo_atraso_correto() {
        let samples = vec![Complex::new(1.0, 0.0); 10];
        let result = compute_pdp(&samples, TEST_SPACING_HZ);
        assert!((result.delay_axis[0] - 0.0).abs() < 1e-9);
        // Span físico: Δτ = 1/(N·Δf); com N=256 e Δf=312.5 kHz → ~3.2 µs total.
        // Último bin = 255 · 1e6/(256·312500) = 3.1875 µs.
        let last = *result.delay_axis.last().unwrap();
        assert!((last - 3.1875).abs() < 1e-4, "last delay = {}", last);
    }
}
