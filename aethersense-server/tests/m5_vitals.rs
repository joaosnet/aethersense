use aethersense_server::detection::vitals;
#[macro_use]
mod common;

#[tokio::test]
async fn m5_live_vitais_executa() {
    let packets = common::capture_and_save("m5", "vitais_exec", 150).await;
    assert!(!packets.is_empty());

    let signal_history: Vec<f64> = packets.iter().map(|p| p.amplitudes.iter().sum::<f64>() / p.amplitudes.len() as f64).collect();
    let fs = 10.0; // 10Hz
    let result = vitals::estimate_vitals(&signal_history, fs, true); // force presence true

    print_summary!("m5_live_vitais_executa", "Garante que analise STFT/FFT para frequências vitais conclui com dados de 15s.", packets, &[format!("Breathing Detectado? {}", result.breathing_rate.is_some())]);
}

#[tokio::test]
async fn m5_live_breathing_range() {
    let packets = common::capture_and_save("m5", "breath_range", 150).await;
    assert!(!packets.is_empty());

    let signal_history: Vec<f64> = packets.iter().map(|p| p.amplitudes.iter().sum::<f64>() / p.amplitudes.len() as f64).collect();
    let result = vitals::estimate_vitals(&signal_history, 10.0, true);

    if let Some(br) = result.breathing_rate {
        assert!(br >= 6.0 && br <= 60.0, "Breathing rate {} fora da faixa humana realista", br);
    }

    print_summary!("m5_live_breathing_range", "As frequências obtidas pela decomposição devem estar modeladas para a faixa realista (0.1 - 1 Hz -> 6 - 60 RPM).", packets, &[] as &[&str]);
}

#[tokio::test]
async fn m5_live_heart_rate_range() {
    let packets = common::capture_and_save("m5", "heart_range", 150).await;
    assert!(!packets.is_empty());

    let signal_history: Vec<f64> = packets.iter().map(|p| p.amplitudes.iter().sum::<f64>() / p.amplitudes.len() as f64).collect();
    let result = vitals::estimate_vitals(&signal_history, 10.0, true);

    if let Some(hr) = result.heart_rate_bpm {
         assert!(hr >= 30.0 && hr <= 200.0, "Heart rate {} fora da faixa humana realista", hr);
    }

    print_summary!("m5_live_heart_rate_range", "Se detectado e não camuflado, heart rate deve estar na faixa (30 - 200 BPM).", packets, &[] as &[&str]);
}

#[tokio::test]
async fn m5_live_confianca_valida() {
    let packets = common::capture_and_save("m5", "vitais_conf", 150).await;
    assert!(!packets.is_empty());

    let signal_history: Vec<f64> = packets.iter().map(|p| p.amplitudes.iter().sum::<f64>() / p.amplitudes.len() as f64).collect();
    let result = vitals::estimate_vitals(&signal_history, 10.0, true);

    assert!(result.confidence >= 0.0 && result.confidence <= 1.0);
    print_summary!("m5_live_confianca_valida", "A confiança informada tem que estar limitada ao range 0.0 - 1.0.", packets, &[format!("Confiança dos Vitais: {:.2}", result.confidence)]);
}
