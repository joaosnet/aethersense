#[macro_use]
mod common;

use std::collections::BTreeMap;

const MAX_ACCEPTABLE_PACKET_LOSS_PERCENT: f32 = 1.0;

#[tokio::test]
async fn m7_real_packet_loss_monitor() {
    let min_packets_to_capture = 300; // Amostra focada em perda; taxa fica coberta pelo M6.

    // Tenta capturar ao vivo ou faz fallback para CSV
    let packets = common::capture_and_save("m7", "packet_loss", min_packets_to_capture).await;

    assert!(!packets.is_empty(), "Nenhum pacote capturado e sem arquivo de fallback disponível!");

    let mut by_mac: BTreeMap<String, Vec<_>> = BTreeMap::new();
    for packet in &packets {
        by_mac.entry(packet.mac.clone()).or_default().push(packet.clone());
    }

    let mut summaries = Vec::new();
    let mut worst_loss_rate = 0.0_f32;

    for (mac, device_packets) in by_mac.iter_mut() {
        device_packets.sort_by_key(|p| p.seq.unwrap_or(0));

        let mut drops = 0;
        let mut seq = device_packets[0].seq.unwrap_or(0);

        for p in device_packets.iter().skip(1) {
            let current = p.seq.unwrap_or(seq + 1);
            if current > seq + 1 {
                drops += current - seq - 1;
            }
            seq = current;
        }

        let loss_rate = if device_packets.len() + (drops as usize) > 0 {
            (drops as f32 / (device_packets.len() as f32 + drops as f32)) * 100.0
        } else {
            0.0
        };

        worst_loss_rate = worst_loss_rate.max(loss_rate);
        summaries.push(format!(
            "{}: {} pacotes, {} drops, perda {:.2}%",
            mac,
            device_packets.len(),
            drops,
            loss_rate
        ));
    }

    print_summary!(
        "m7_real_packet_loss_monitor",
        "Valida as perdas de pacotes reais vindos diretamente do ESP32.",
        packets,
        &{
            let mut params = summaries;
            params.extend([
            format!("Pior taxa de perda por dispositivo: {:.2}%", worst_loss_rate),
            format!("Limite aceitavel: <= {:.2}%", MAX_ACCEPTABLE_PACKET_LOSS_PERCENT)
            ]);
            params
        }
    );

    let require_live_capture = std::env::var("AETHERSENSE_REQUIRE_LIVE_CAPTURE")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    if packets.len() < min_packets_to_capture && !require_live_capture {
        println!(
            "⚠️ Amostra insuficiente para reprovar packet loss real: {} de {} pacotes.",
            packets.len(),
            min_packets_to_capture
        );
        return;
    }

    // O fluxo real aceita uma pequena perda residual para evitar falhas espurias da captura viva.
    assert!(
        worst_loss_rate <= MAX_ACCEPTABLE_PACKET_LOSS_PERCENT,
        "Ao menos um ESP32 real apresentou perda de pacotes acima do limite aceitavel (taxa: {:.2}%, limite: {:.2}%)!",
        worst_loss_rate,
        MAX_ACCEPTABLE_PACKET_LOSS_PERCENT
    );
}
