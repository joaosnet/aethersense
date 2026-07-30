use aethersense_server::detection::{presence, movement};
#[macro_use]
mod common;

#[tokio::test]
async fn m3_live_presenca_executa() {
    let packets = common::capture_and_save("m3", "presenca_executa", 100).await;
    assert!(!packets.is_empty());

    let signal_history: Vec<f64> = packets.iter().map(|p| p.amplitudes.iter().sum::<f64>() / p.amplitudes.len() as f64).collect();
    let result = presence::detect_presence(&signal_history, None);
    print_summary!("m3_live_presenca_executa", "Executa `detect_presence` sem panic e retorna StateDetect válido.", packets, &[format!("Presenca: {}", result.detected)]);
}

#[tokio::test]
async fn m3_live_presenca_confianca() {
    let packets = common::capture_and_save("m3", "presenca_confianca", 50).await;
    assert!(!packets.is_empty());

    let signal_history: Vec<f64> = packets.iter().map(|p| p.amplitudes.iter().sum::<f64>() / p.amplitudes.len() as f64).collect();
    let result = presence::detect_presence(&signal_history, None);
    assert!(result.confidence >= 0.0 && result.confidence <= 1.0);
    print_summary!("m3_live_presenca_confianca", "Verifica se a confianca da estimativa ta contida em [0.0, 1.0].", packets, &[format!("Confianca presenca: {:.2}", result.confidence)]);
}

#[tokio::test]
async fn m3_live_movimento_executa() {
    let packets = common::capture_and_save("m3", "mov_executa", 100).await;
    assert!(!packets.is_empty());

    let amp_history: Vec<Vec<f64>> = packets.iter().map(|p| p.amplitudes.clone()).collect();
    let result = movement::detect_movement(&amp_history);
    print_summary!("m3_live_movimento_executa", "Verifica se o `detect_movement` avalia variance corretamente em dados reais sem crash.", packets, &[format!("Nivel de Movimento: {:?}", result.level)]);
}

#[tokio::test]
async fn m3_live_velocidade_valida() {
    let packets = common::capture_and_save("m3", "velocidade", 100).await;
    assert!(!packets.is_empty());

    let amp_history: Vec<Vec<f64>> = packets.iter().map(|p| p.amplitudes.clone()).collect();
    let result = movement::detect_movement(&amp_history);
    assert!(result.velocity_estimate >= 0.0);
    print_summary!("m3_live_velocidade_valida", "A estimativa de velocidade, sendo modulo/norma, sempre deve ser ≥ 0.", packets, &[format!("Velocidade Estimada: {:.2}", result.velocity_estimate)]);
}
