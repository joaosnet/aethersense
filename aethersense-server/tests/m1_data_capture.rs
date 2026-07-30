use aethersense_server::state::InnerState;
use std::collections::BTreeMap;
#[macro_use]
mod common;

#[tokio::test]
async fn m1_live_capture_recebe_pacotes() {
    let packets = common::capture_and_save("m1", "recebe_pacotes", 10).await;
    assert!(!packets.is_empty(), "Falha: Nenhum pacote foi recebido. O ESP32 esta ligado?");
    print_summary!("m1_live_capture_recebe_pacotes", "Verifica se o servidor consegue receber pacotes UDP do ESP32.", packets, &[] as &[&str]);
}

#[tokio::test]
async fn m1_live_linha_csi_reconhecida() {
    let packets = common::capture_and_save("m1", "linha_csi", 10).await;
    assert!(!packets.is_empty());
    print_summary!("m1_live_linha_csi_reconhecida", "Verifica se as linhas brutas foram corretamente convertidas em CsiPacket sem panic.", packets, &[] as &[&str]);
}

#[tokio::test]
async fn m1_live_campo_mac_extraido() {
    let packets = common::capture_and_save("m1", "mac_extraido", 10).await;
    assert!(!packets.is_empty());

    let mut by_mac: BTreeMap<String, usize> = BTreeMap::new();
    for p in &packets {
        assert!(is_valid_mac(&p.mac), "MAC invalido: {}", p.mac);
        *by_mac.entry(p.mac.clone()).or_default() += 1;
    }

    print_summary!(
        "m1_live_campo_mac_extraido",
        "Verifica se MAC addresses foram extraidos e agrupam pacotes por dispositivo.",
        packets,
        &by_mac
            .iter()
            .map(|(mac, count)| format!("{}: {} pacotes", mac, count))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn m1_live_campo_rssi_valido() {
    let packets = common::capture_and_save("m1", "rssi_valido", 10).await;
    assert!(!packets.is_empty());
    for p in &packets {
        if let Some(rssi) = p.rssi {
            assert!(rssi >= -100 && rssi <= 0, "RSSI {} out of valid range [-100, 0]", rssi);
        }
    }
    print_summary!("m1_live_campo_rssi_valido", "Verifica se RSSI extraido esta entre -100 e 0.", packets, &[] as &[&str]);
}

#[tokio::test]
async fn m1_live_samples_extraidos() {
    let packets = common::capture_and_save("m1", "samples_extraidos", 10).await;
    assert!(!packets.is_empty());
    for p in &packets {
        assert!(p.complex_samples.len() >= 32, "Nao ha subportadoras suficientes: {}", p.complex_samples.len());
    }
    print_summary!("m1_live_samples_extraidos", "Verifica se mais de 32 subportadoras complexas foram extraidas.", packets, &[] as &[&str]);
}

#[tokio::test]
async fn m1_live_amplitudes_positivas() {
    let packets = common::capture_and_save("m1", "amplitudes_positivas", 10).await;
    assert!(!packets.is_empty());
    for p in &packets {
        for &amp in &p.amplitudes {
            assert!(amp >= 0.0, "Amplitude {} is negative", amp);
        }
    }
    print_summary!("m1_live_amplitudes_positivas", "Verifica se calculo de amplitude resulta em modulos reais positivos.", packets, &[] as &[&str]);
}

#[tokio::test]
async fn m1_live_seq_incrementando() {
    let packets = common::capture_and_save("m1", "seq_incrementando", 50).await;
    assert!(!packets.is_empty());

    let mut by_mac: BTreeMap<String, Vec<_>> = BTreeMap::new();
    for packet in &packets {
        by_mac.entry(packet.mac.clone()).or_default().push(packet);
    }

    let mut summaries = Vec::new();
    for (mac, device_packets) in by_mac.iter_mut() {
        device_packets.sort_by_key(|p| p.seq.unwrap_or(0));

        let mut seq = device_packets[0].seq.unwrap_or(0);
        let mut drops = 0;
        for p in device_packets.iter().skip(1) {
            let current = p.seq.unwrap_or(seq + 1);
            if current > seq + 1 {
                drops += current - seq - 1;
            }
            seq = current;
        }

        summaries.push(format!("{}: {} pacotes pulados", mac, drops));
    }

    print_summary!(
        "m1_live_seq_incrementando",
        "Verifica a contagem de sequencia e perdas por dispositivo.",
        packets,
        &summaries
    );
}

#[tokio::test]
async fn m1_live_estado_acumula() {
    let packets = common::capture_and_save("m1", "estado_acumula", 20).await;
    assert!(!packets.is_empty());
    let mut state = InnerState::new();
    for p in packets.iter().cloned() {
        state.push_packet(p);
    }
    assert!(state.signal_history.len() > 0, "Historico de sinais no InnerState deveria crescer");
    print_summary!("m1_live_estado_acumula", "Verifica se InnerState insere corretamente sem dar panic.", packets, &[format!("History len: {}", state.signal_history.len())]);
}

fn is_valid_mac(mac: &str) -> bool {
    let parts: Vec<_> = mac.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|part| part.len() == 2 && part.chars().all(|c| c.is_ascii_hexdigit()))
}
