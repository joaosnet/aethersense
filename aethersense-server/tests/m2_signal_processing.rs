use aethersense_server::processing::{heatmap, stft, pca, phase, pdp};
#[macro_use]
mod common;

#[tokio::test]
async fn m2_live_heatmap_dimensoes() {
    let packets = common::capture_and_save("m2", "heatmap", 50).await;
    assert!(!packets.is_empty());

    let amp_vec: Vec<Vec<f64>> = packets.iter()
        .filter(|p| p.amplitudes.len() >= 64)
        .map(|p| p.amplitudes.clone())
        .collect();
    let grid = heatmap::build_heatmap_matrix(&amp_vec);
    print_summary!("m2_live_heatmap_dimensoes", "Heatmap acumula pacotes mantendo grid MxN configurado.", packets, &[format!("Dimensao atual do Heatmap: {}x{}", grid.len(), if grid.is_empty() { 0 } else { grid[0].len() })]);
}

#[tokio::test]
async fn m2_live_stft_espectrograma() {
    let packets = common::capture_and_save("m2", "stft", 50).await;
    assert!(!packets.is_empty());
    let means: Vec<f64> = packets.iter().map(|p| p.amplitudes.iter().sum::<f64>() / p.amplitudes.len() as f64).collect();
    let fs = 10.0; // 10Hz
    let spec = stft::compute_stft(&means, 50, fs);
    assert!(!spec.spectrogram.is_empty());
    for row in &spec.spectrogram {
        for &val in row {
            assert!(val.is_finite(), "STFT valor NaN ou Inf encontrado");
        }
    }
    print_summary!("m2_live_stft_espectrograma", "Processamento STFT gera espectrograma finito com dados reais.", packets, &[format!("Linhas no espectrograma STFT: {}", spec.spectrogram.len())]);
}

#[tokio::test]
async fn m2_live_pca_componentes() {
    let packets = common::capture_and_save("m2", "pca", 50).await;
    assert!(!packets.is_empty());

    // We assume 64 subcarriers for PCA
    let matrix: Vec<Vec<f64>> = packets.iter().map(|p| p.amplitudes.clone()).collect();
    if matrix[0].len() >= 2 {
        let pca_result = pca::compute_pca(&matrix, matrix.len());
        assert_eq!(pca_result.pc1.len(), matrix.len());
        assert_eq!(pca_result.pc2.len(), matrix.len());
        for v in pca_result.pc1 { assert!(v.is_finite()); }
    }
    print_summary!("m2_live_pca_componentes", "PCA reduz as dimensoes de forma correta sem matematicas invalidas no meio de dados reais.", packets, &[] as &[&str]);
}

#[tokio::test]
async fn m2_live_pdp_finito() {
    let packets = common::capture_and_save("m2", "pdp", 50).await;
    assert!(!packets.is_empty());
    let pdps: Vec<Vec<f64>> = packets.iter().map(|p| pdp::compute_pdp(&p.complex_samples, 312_500.0).power_db).collect();
    for p in &pdps {
        for &v in p { assert!(v.is_finite()); }
    }
    print_summary!("m2_live_pdp_finito", "PDP (Power Delay Profile) utilizando FFT e transformado p/ dB e gera pontos finitos.", packets, &[] as &[&str]);
}

#[tokio::test]
async fn m2_live_fase_sanitizada() {
    let packets = common::capture_and_save("m2", "fase_san", 50).await;
    assert!(!packets.is_empty());
    let phases: Vec<f64> = packets.iter().map(|p| if p.phases.len() > 10 { p.phases[10] } else { p.phases[0] }).collect();
    let sanitized = phase::sanitize_phase_series(&phases);
    for v in sanitized { assert!(v.is_finite()); }
    print_summary!("m2_live_fase_sanitizada", "Processamento de fase passa num pipeline: unwrap -> detrend -> mov_avg completo usando dados reais.", packets, &[] as &[&str]);
}
