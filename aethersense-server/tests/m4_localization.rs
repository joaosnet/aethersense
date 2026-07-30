use aethersense_server::detection::location;
use aethersense_server::state::{DeviceInfo, DeviceSignalHistory, EmptyRoomBaseline};
use std::collections::HashMap;
#[macro_use]
mod common;

#[tokio::test]
async fn m4_live_localizacao_executa() {
    let packets = common::capture_and_save("m4", "loc_exec" , 50).await;
    assert!(!packets.is_empty());

    let dev = DeviceInfo {
        id: packets[0].mac.clone(),
        name: "TestDevice".to_string(),
        lat: 0.0,
        lng: 0.0,
        floor: 0,
        icon: "/esp32-real.svg".to_string(),
        size: 32,
        role: "receiver".to_string(),
        hardware_profile: "wroom".to_string(),
    };

    let mean_amp = packets.iter().map(|p| p.amplitudes.iter().sum::<f64>() / p.amplitudes.len() as f64).sum::<f64>() / packets.len() as f64;
    let rssi = packets.last().unwrap().rssi;
    let loc = location::estimate_location_single(&dev, rssi, mean_amp);

    assert!(loc.lat.is_finite() && loc.lng.is_finite());

    print_summary!("m4_live_localizacao_executa", "Usa um DeviceInfo simples para calcular distance por Path Loss Model, retornando coords finitas.", packets, &[format!("Coordenadas Loc: lat={}, lng={}", loc.lat, loc.lng)]);
}

#[tokio::test]
async fn m4_live_localizacao_georreferenciada_c5_wroom() {
    let packets = common::capture_and_save("m4", "geo_c5_wroom", 80).await;
    assert!(!packets.is_empty());

    let mut histories: HashMap<String, DeviceSignalHistory> = HashMap::new();
    for packet in packets.iter().cloned() {
        histories
            .entry(packet.mac.clone())
            .or_default()
            .push_packet(packet);
    }

    let mut macs: Vec<String> = histories.keys().cloned().collect();
    macs.sort();

    let mut devices = Vec::new();
    for (index, mac) in macs.iter().take(2).enumerate() {
        let profile = infer_test_hardware_profile(mac);
        devices.push(DeviceInfo {
            id: mac.clone(),
            name: if profile == "esp32c5" {
                "ESP32-C5".to_string()
            } else {
                "ESP32-WROOM".to_string()
            },
            lat: -23.5504802 + index as f64 * 0.000004,
            lng: -46.6333161 + index as f64 * 0.000006,
            floor: 4,
            icon: "/esp32-real.svg".to_string(),
            size: 32,
            role: "receiver".to_string(),
            hardware_profile: profile,
        });
    }

    let baselines: HashMap<String, EmptyRoomBaseline> = devices
        .iter()
        .filter_map(|device| {
            let history = histories.get(&device.id)?;
            Some((
                device.id.clone(),
                EmptyRoomBaseline {
                    mean_amplitude: (history.mean_amplitude() * 0.92).max(1.0),
                    amplitude_variance: history.amplitude_variance(),
                    rssi_mean: history.mean_rssi().unwrap_or(-70.0),
                    sample_count: history.signal_history.len(),
                    hardware_profile: device.hardware_profile.clone(),
                    calibrated_at: None,
                    calibration_session_id: None,
                    floor: None,
                    floorplan_id: None,
                    environment_object_ids: Vec::new(),
                },
            ))
        })
        .collect();

    let loc =
        location::estimate_location_georeferenced(&devices, &histories, &baselines, None, None);
    assert!(loc.lat.is_finite() && loc.lng.is_finite());
    assert!(loc.accuracy_m >= 0.05);
    assert!(loc.resolution_m <= 0.05);
    assert!(loc.nodes_used >= 1);
    if devices.len() >= 2 {
        assert_eq!(loc.nodes_used, 2);
        assert!(loc.method.contains("fusion"));
        assert!(
            loc.accuracy_m <= 1.0,
            "localizacao fusionada deve reportar accuracy_m submetrico; obtido {:.2}",
            loc.accuracy_m
        );
        let reported_confidence = (loc.confidence * 100.0).round() / 100.0;
        assert!(
            reported_confidence >= 0.60,
            "localizacao fusionada deve reportar confidence >= 0.60; bruto {:.4}, reportado {:.2}",
            loc.confidence,
            reported_confidence
        );
    }

    print_summary!(
        "m4_live_localizacao_georreferenciada_c5_wroom",
        "Valida estimativa georreferenciada com C5+WROOM quando dois MACs existem, e fallback quando só um nó está disponível.",
        packets,
        &[
            format!("lat={}, lng={}, accuracy_m={:.2}", loc.lat, loc.lng, loc.accuracy_m),
            format!("nodes_used={}, method={}, confidence={:.2}", loc.nodes_used, loc.method, loc.confidence),
        ]
    );
}

fn infer_test_hardware_profile(mac: &str) -> String {
    let lower = mac.to_ascii_lowercase();
    if lower.starts_with("d0:cf") {
        "esp32c5".to_string()
    } else {
        "wroom".to_string()
    }
}

#[tokio::test]
async fn m4_live_precisao_positiva() {
    let packets = common::capture_and_save("m4", "precisao", 50).await;
    assert!(!packets.is_empty());

    let dev = DeviceInfo {
        id: packets[0].mac.clone(),
        name: "TestDevice".to_string(),
        lat: 0.0,
        lng: 0.0,
        floor: 0,
        icon: "/esp32-real.svg".to_string(),
        size: 32,
        role: "receiver".to_string(),
        hardware_profile: "wroom".to_string(),
    };

    let mean_amp = packets.iter().map(|p| p.amplitudes.iter().sum::<f64>() / p.amplitudes.len() as f64).sum::<f64>() / packets.len() as f64;
    let rssi = packets.last().unwrap().rssi;
    let loc = location::estimate_location_single(&dev, rssi, mean_amp);

    assert!(loc.accuracy_m >= 0.0);
    print_summary!("m4_live_precisao_positiva", "A acuracia calculada (ou default) nunca deve ser negativa.", packets, &[format!("Acuracia (accuracy_m): {}", loc.accuracy_m)]);
}
