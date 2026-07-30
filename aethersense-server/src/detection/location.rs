/// Estimativa georreferenciada via CSI/RSSI.
///
/// C5 e WROOM são tratados como receptores georreferenciados. Cada nó produz
/// uma distância/confiança local e o estimador fusiona as âncoras disponíveis.
use std::collections::HashMap;

use crate::detection::geometry;
use crate::state::{
    ActiveZoneState, DeviceInfo, DeviceSignalHistory, EmptyRoomBaseline, LocationEstimate,
    MapConfig, ZoneScore,
};

const RESOLUTION_M: f64 = 0.05;
const DEFAULT_SINGLE_NODE_ACCURACY_M: f64 = 1.5;
const DEFAULT_MULTI_NODE_ACCURACY_M: f64 = 0.55;
const EARTH_METERS_PER_DEG_LAT: f64 = 111_320.0;
/// Atenuação típica atribuída a UMA parede interna (alvenaria/drywall) em
/// 2,4/5 GHz. O RSSI medido através de paredes superestima a distância no
/// modelo log-distance; a correção devolve esses dB antes da conversão.
const WALL_ATTENUATION_DB: f64 = 4.0;
/// Teto de paredes corrigidas por caminho (evita correção degenerada quando o
/// usuário desenha muitas divisórias sobrepostas).
const MAX_WALLS_PER_PATH: usize = 3;

#[derive(Debug, Clone)]
struct NodeEstimate {
    device: DeviceInfo,
    distance_m: f64,
    confidence: f64,
    has_baseline: bool,
    // Insumos preservados para RECOMPUTAR a distância na passada ciente de
    // paredes (correção de atenuação sobre o RSSI médio).
    rssi: f64,
    mean_amplitude: f64,
    amplitude_variance: f64,
}

/// Modelo de propagação log-distance para estimar distância via RSSI.
///
/// d = 10 ^ ((RSSI_d0 - RSSI) / (10 * n))
fn rssi_to_distance(rssi: f64, rssi_d0: f64, n: f64) -> f64 {
    let distance = 10.0_f64.powf((rssi_d0 - rssi) / (10.0 * n));
    distance.clamp(0.15, 12.0)
}

/// Compatibilidade com os testes/fluxos antigos de um único ESP32.
#[allow(dead_code)]
pub fn estimate_location_single(
    device: &DeviceInfo,
    rssi: Option<i32>,
    mean_amplitude: f64,
) -> LocationEstimate {
    let profile = normalized_profile(device);
    let rssi_val = rssi.unwrap_or(-70) as f64;
    let distance = estimate_distance_m(&profile, rssi_val, mean_amplitude, 0.0, None);
    let confidence = profile_weight(&profile) * 0.45;

    LocationEstimate {
        lat: device.lat,
        lng: device.lng,
        floor: device.floor,
        accuracy_m: distance.max(DEFAULT_SINGLE_NODE_ACCURACY_M),
        confidence,
        accuracy_dm: meters_to_decimeters(distance.max(DEFAULT_SINGLE_NODE_ACCURACY_M)),
        nodes_used: 1,
        method: format!("single_node_{profile}_fallback"),
        experimental: true,
        resolution_m: RESOLUTION_M,
        ..LocationEstimate::default()
    }
}

/// Estima localização georreferenciada usando todos os receptores ativos.
///
/// Com `map_config`, a planta baixa passa a ser FUNCIONAL: paredes corrigem a
/// atenuação do modelo RSSI→distância, os cômodos desenhados viram
/// `current_room` (point-in-polygon) e os bounds do floorplan confinam a
/// estimativa ao apartamento (geofence).
pub fn estimate_location_georeferenced(
    devices: &[DeviceInfo],
    histories: &HashMap<String, DeviceSignalHistory>,
    baselines: &HashMap<String, EmptyRoomBaseline>,
    previous: Option<&LocationEstimate>,
    map_config: Option<&MapConfig>,
) -> LocationEstimate {
    let mut nodes: Vec<NodeEstimate> = devices
        .iter()
        .filter(|device| is_receiver(device) && has_meaningful_coordinates(device))
        .filter_map(|device| {
            let history = histories.get(&device.id)?;
            if history.signal_history.is_empty() {
                return None;
            }

            let profile = normalized_profile(device);
            let rssi = history.mean_rssi().unwrap_or(-70.0);
            let mean_amplitude = history.mean_amplitude();
            let amplitude_variance = history.amplitude_variance();
            let baseline = baselines.get(&device.id);
            let distance_m =
                estimate_distance_m(&profile, rssi, mean_amplitude, amplitude_variance, baseline);
            let confidence = estimate_confidence(&profile, history, baseline);

            Some(NodeEstimate {
                device: device.clone(),
                distance_m,
                confidence,
                has_baseline: baseline.is_some(),
                rssi,
                mean_amplitude,
                amplitude_variance,
            })
        })
        .collect();

    nodes.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let initial = match nodes.len() {
        0 => LocationEstimate {
            method: "sem_dados".to_string(),
            experimental: true,
            resolution_m: RESOLUTION_M,
            ..LocationEstimate::default()
        },
        1 => estimate_from_single_node(&nodes[0]),
        _ => estimate_from_multiple_nodes(&nodes, previous),
    };

    match map_config {
        Some(map) if initial.nodes_used > 0 => {
            apply_map_context(initial, &nodes, baselines, map, previous)
        }
        _ => initial,
    }
}

/// Pós-processamento ciente da planta: (1) UMA passada de refinamento por
/// paredes (re-fusão com RSSI corrigido — uma única iteração, documentada,
/// para não realimentar a própria correção), (2) cômodo atual, (3) geofence.
fn apply_map_context(
    mut estimate: LocationEstimate,
    nodes: &[NodeEstimate],
    baselines: &HashMap<String, EmptyRoomBaseline>,
    map: &MapConfig,
    previous: Option<&LocationEstimate>,
) -> LocationEstimate {
    // 1. Paredes atenuam: só na fusão multi-nó (com 1 nó a posição publicada é
    //    a do próprio receptor — corrigir a distância não move nada).
    if estimate.nodes_used >= 2 {
        let mut total_walls = 0usize;
        let mut refined: Vec<NodeEstimate> = Vec::with_capacity(nodes.len());
        for node in nodes {
            let walls = geometry::walls_crossed(
                (node.device.lat, node.device.lng),
                (estimate.lat, estimate.lng),
                node.device.floor,
                &map.environment_objects,
            )
            .min(MAX_WALLS_PER_PATH);
            total_walls += walls;
            if walls == 0 {
                refined.push(node.clone());
                continue;
            }
            // Devolve ao RSSI os dB absorvidos pelas paredes antes do modelo
            // log-distance (RSSI através de parede superestima a distância).
            let corrected_rssi = node.rssi + walls as f64 * WALL_ATTENUATION_DB;
            let profile = normalized_profile(&node.device);
            let baseline = baselines.get(&node.device.id);
            let distance_m = estimate_distance_m(
                &profile,
                corrected_rssi,
                node.mean_amplitude,
                node.amplitude_variance,
                baseline,
            );
            refined.push(NodeEstimate {
                distance_m,
                ..node.clone()
            });
        }
        if total_walls > 0 {
            let mut refined_estimate = estimate_from_multiple_nodes(&refined, previous);
            refined_estimate.walls_crossed = total_walls;
            refined_estimate.method.push_str("+paredes");
            estimate = refined_estimate;
        }
    }

    // 2. Geofence: confina aos cômodos (objetos `room`) ou bounds da planta do andar.
    if let Some(((lat, lng), moved)) = geometry::clamp_to_rooms_or_floorplans(
        (estimate.lat, estimate.lng),
        estimate.floor,
        &map.environment_objects,
        &map.floorplans,
    ) {
        if moved {
            estimate.lat = lat;
            estimate.lng = lng;
            estimate.clamped_to_floorplan = true;
            estimate.method.push_str("+confinado");
        }
    }

    // 3. Cômodo atual (após o confinamento garantido).
    estimate.current_room = geometry::current_room(
        (estimate.lat, estimate.lng),
        estimate.floor,
        &map.environment_objects,
    );

    estimate
}

#[allow(dead_code)]
pub fn estimate_location_multi(devices: &[(DeviceInfo, f64)]) -> LocationEstimate {
    if devices.is_empty() {
        return LocationEstimate::default();
    }

    let nodes: Vec<NodeEstimate> = devices
        .iter()
        .map(|(device, distance_m)| NodeEstimate {
            device: device.clone(),
            distance_m: *distance_m,
            confidence: profile_weight(&normalized_profile(device)),
            has_baseline: true,
            rssi: -60.0,
            mean_amplitude: 0.0,
            amplitude_variance: 0.0,
        })
        .collect();

    if nodes.len() == 1 {
        estimate_from_single_node(&nodes[0])
    } else {
        estimate_from_multiple_nodes(&nodes, None)
    }
}

fn estimate_from_single_node(node: &NodeEstimate) -> LocationEstimate {
    let accuracy_m = (node.distance_m * 0.65)
        .max(DEFAULT_SINGLE_NODE_ACCURACY_M)
        .min(6.0);

    LocationEstimate {
        lat: node.device.lat,
        lng: node.device.lng,
        floor: node.device.floor,
        accuracy_m,
        confidence: (node.confidence * 0.75).clamp(0.0, 0.75),
        accuracy_dm: meters_to_decimeters(accuracy_m),
        nodes_used: 1,
        method: fallback_method(&node.device, node.has_baseline),
        experimental: true,
        resolution_m: RESOLUTION_M,
        ..LocationEstimate::default()
    }
}

fn estimate_from_multiple_nodes(
    nodes: &[NodeEstimate],
    previous: Option<&LocationEstimate>,
) -> LocationEstimate {
    let origin = &nodes[0].device;
    let avg_lat = nodes.iter().map(|node| node.device.lat).sum::<f64>() / nodes.len() as f64;

    let mut weighted_x = 0.0;
    let mut weighted_y = 0.0;
    let mut total_weight = 0.0;
    let mut floor_scores: HashMap<i32, f64> = HashMap::new();

    for node in nodes {
        let (x, y) = lat_lng_to_xy(
            node.device.lat,
            node.device.lng,
            origin.lat,
            origin.lng,
            avg_lat,
        );
        let weight = node.confidence.max(0.05) / (node.distance_m + 0.25);
        weighted_x += x * weight;
        weighted_y += y * weight;
        total_weight += weight;
        *floor_scores.entry(node.device.floor).or_insert(0.0) += weight;
    }

    let mut x = weighted_x / total_weight.max(f64::EPSILON);
    let mut y = weighted_y / total_weight.max(f64::EPSILON);

    if let Some(previous) = previous.filter(|estimate| estimate.nodes_used >= 1) {
        let (prev_x, prev_y) =
            lat_lng_to_xy(previous.lat, previous.lng, origin.lat, origin.lng, avg_lat);
        x = prev_x * 0.35 + x * 0.65;
        y = prev_y * 0.35 + y * 0.65;
    }

    let (x, y) = (quantize_m(x, RESOLUTION_M), quantize_m(y, RESOLUTION_M));
    let (lat, lng) = xy_to_lat_lng(x, y, origin.lat, origin.lng, avg_lat);
    let floor = floor_scores
        .into_iter()
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(floor, _)| floor)
        .unwrap_or(origin.floor);

    let avg_confidence = nodes.iter().map(|node| node.confidence).sum::<f64>() / nodes.len() as f64;
    let all_nodes_have_baseline = nodes.iter().all(|node| node.has_baseline);
    let baseline_factor = if all_nodes_have_baseline { 1.08 } else { 0.75 };
    let geometry_factor = geometry_quality(nodes).clamp(0.8, 1.0);
    let confidence = (avg_confidence * baseline_factor * geometry_factor).clamp(0.0, 0.98);
    let accuracy_m = (DEFAULT_MULTI_NODE_ACCURACY_M / confidence.max(0.45))
        .min(1.5)
        .max(0.05);

    LocationEstimate {
        lat,
        lng,
        floor,
        accuracy_m,
        confidence,
        accuracy_dm: meters_to_decimeters(accuracy_m),
        nodes_used: nodes.len(),
        method: if all_nodes_have_baseline {
            "fusion_c5_wroom_baseline_quantized".to_string()
        } else {
            "fusion_c5_wroom_sem_baseline".to_string()
        },
        experimental: true,
        resolution_m: RESOLUTION_M,
        ..LocationEstimate::default()
    }
}

/// Janela curta do score de zona: ~2–4 s a 10–20 pps. O histórico completo
/// (512 amostras) amortece demais — a entrada de uma pessoa no cômodo
/// precisa aparecer no score em segundos.
const ZONE_WINDOW_SAMPLES: usize = 40;

/// Perturbação de canal de cada receptor em relação ao seu baseline de sala
/// vazia → zona (cômodo) ativa. Sem baseline, usa a variância normalizada
/// como aproximação (menos confiável; calibre a sala vazia pelo dashboard).
pub fn estimate_active_zone(
    devices: &[DeviceInfo],
    histories: &HashMap<String, DeviceSignalHistory>,
    baselines: &HashMap<String, EmptyRoomBaseline>,
) -> ActiveZoneState {
    let mut zones: Vec<ZoneScore> = Vec::new();

    for device in devices.iter().filter(|d| d.role == "receiver") {
        let Some(history) = histories.get(&device.id) else {
            continue;
        };
        if history.signal_history.len() < 10 {
            continue;
        }
        let mean_amplitude = history.recent_mean_amplitude(ZONE_WINDOW_SAMPLES);
        let amplitude_variance = history.recent_amplitude_variance(ZONE_WINDOW_SAMPLES);

        let (score, has_baseline) = match baselines.get(&device.id) {
            Some(baseline) => {
                let baseline_amp = baseline.mean_amplitude.max(1.0);
                let amp_delta =
                    ((mean_amplitude - baseline.mean_amplitude).abs() / baseline_amp).min(2.0);
                let variance_delta = (amplitude_variance.sqrt()
                    / (baseline_amp + baseline.amplitude_variance.sqrt()))
                .min(2.0);
                (
                    (amp_delta * 0.65 + variance_delta * 0.35).clamp(0.0, 2.0),
                    true,
                )
            }
            None => {
                let normalized_std = (amplitude_variance.sqrt() / mean_amplitude.max(1.0)).min(2.0);
                (normalized_std, false)
            }
        };

        zones.push(ZoneScore {
            device_id: device.id.clone(),
            device_name: device.name.clone(),
            score,
            has_baseline,
            // Decisão binária e limiares preenchidos pelo ZoneTracker.
            occupied: false,
            on_threshold: 0.0,
            off_threshold: 0.0,
        });
    }

    zones.sort_by(|a, b| b.score.total_cmp(&a.score));
    let best = zones.first().cloned();
    ActiveZoneState {
        zone_id: best.as_ref().map(|z| z.device_id.clone()),
        zone_name: best.as_ref().map(|z| z.device_name.clone()),
        score: best.map(|z| z.score).unwrap_or(0.0),
        zones,
    }
}

fn estimate_distance_m(
    profile: &str,
    rssi: f64,
    mean_amplitude: f64,
    amplitude_variance: f64,
    baseline: Option<&EmptyRoomBaseline>,
) -> f64 {
    let (rssi_d0, path_loss) = if profile == "esp32c5" {
        (-38.0, 2.65)
    } else {
        (-42.0, 3.05)
    };

    let rssi_distance = rssi_to_distance(rssi, rssi_d0, path_loss);
    let csi_distance = if let Some(baseline) = baseline {
        let baseline_amp = baseline.mean_amplitude.max(1.0);
        let amp_delta = ((mean_amplitude - baseline.mean_amplitude).abs() / baseline_amp).min(2.0);
        let variance_delta = (amplitude_variance.sqrt()
            / (baseline_amp + baseline.amplitude_variance.sqrt()))
        .min(2.0);
        let perturbation = (amp_delta * 0.65 + variance_delta * 0.35).clamp(0.0, 2.0);
        (2.4 - perturbation).clamp(0.25, 4.5)
    } else if mean_amplitude > 0.0 {
        (100.0 / mean_amplitude).clamp(0.35, 5.0)
    } else {
        rssi_distance
    };

    (rssi_distance * 0.55 + csi_distance * 0.45).clamp(0.15, 8.0)
}

fn estimate_confidence(
    profile: &str,
    history: &DeviceSignalHistory,
    baseline: Option<&EmptyRoomBaseline>,
) -> f64 {
    let samples = history.signal_history.len().min(history.rssi_history.len()) as f64;
    let sample_score = (samples / 40.0).clamp(0.15, 1.0);
    let profile_score = profile_weight(profile);
    let baseline_score = baseline.map(|_| 1.0).unwrap_or(0.58);
    let rssi_score = history
        .mean_rssi()
        .map(|rssi| ((rssi + 90.0) / 45.0).clamp(0.2, 1.0))
        .unwrap_or(0.35);

    (sample_score * 0.25 + profile_score * 0.3 + baseline_score * 0.25 + rssi_score * 0.2)
        .clamp(0.0, 1.0)
}

fn fallback_method(device: &DeviceInfo, has_baseline: bool) -> String {
    let profile = normalized_profile(device);
    let suffix = if has_baseline {
        "baseline"
    } else {
        "sem_baseline"
    };
    format!("fallback_{profile}_{suffix}")
}

fn profile_weight(profile: &str) -> f64 {
    match profile {
        "esp32c5" => 0.95,
        "wroom" | "classic" => 0.72,
        _ => 0.6,
    }
}

fn normalized_profile(device: &DeviceInfo) -> String {
    let raw = device.hardware_profile.trim().to_ascii_lowercase();
    if raw.contains("c5") {
        "esp32c5".to_string()
    } else if raw.contains("classic") || raw.contains("wroom") || raw.contains("esp32") {
        "wroom".to_string()
    } else {
        raw
    }
}

fn is_receiver(device: &DeviceInfo) -> bool {
    let role = device.role.trim().to_ascii_lowercase();
    let profile = normalized_profile(device);
    (role.is_empty() || role == "receiver") && profile != "router"
}

fn has_meaningful_coordinates(device: &DeviceInfo) -> bool {
    device.lat.is_finite()
        && device.lng.is_finite()
        && (device.lat.abs() > 1e-9 || device.lng.abs() > 1e-9)
}

fn meters_to_decimeters(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn quantize_m(value: f64, resolution_m: f64) -> f64 {
    (value / resolution_m).round() * resolution_m
}

fn geometry_quality(nodes: &[NodeEstimate]) -> f64 {
    if nodes.len() < 2 {
        return 0.4;
    }

    let first = &nodes[0].device;
    let second = &nodes[1].device;
    let avg_lat = (first.lat + second.lat) / 2.0;
    let (x, y) = lat_lng_to_xy(second.lat, second.lng, first.lat, first.lng, avg_lat);
    let separation_m = (x * x + y * y).sqrt();
    (separation_m / 2.0).clamp(0.65, 1.0)
}

fn lat_lng_to_xy(lat: f64, lng: f64, origin_lat: f64, origin_lng: f64, avg_lat: f64) -> (f64, f64) {
    let meters_per_lng = EARTH_METERS_PER_DEG_LAT * avg_lat.to_radians().cos().abs().max(0.05);
    (
        (lng - origin_lng) * meters_per_lng,
        (lat - origin_lat) * EARTH_METERS_PER_DEG_LAT,
    )
}

fn xy_to_lat_lng(x: f64, y: f64, origin_lat: f64, origin_lng: f64, avg_lat: f64) -> (f64, f64) {
    let meters_per_lng = EARTH_METERS_PER_DEG_LAT * avg_lat.to_radians().cos().abs().max(0.05);
    (
        origin_lat + y / EARTH_METERS_PER_DEG_LAT,
        origin_lng + x / meters_per_lng,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, profile: &str, lat: f64, lng: f64) -> DeviceInfo {
        DeviceInfo {
            id: id.to_string(),
            name: id.to_string(),
            lat,
            lng,
            floor: 4,
            icon: "/esp32-real.svg".to_string(),
            size: 32,
            role: "receiver".to_string(),
            hardware_profile: profile.to_string(),
        }
    }

    fn history(rssi: i32, amp: f64) -> DeviceSignalHistory {
        let mut history = DeviceSignalHistory::new();
        for _ in 0..50 {
            history.signal_history.push_back(amp);
            history.rssi_history.push_back(rssi);
        }
        history
    }

    fn baseline(profile: &str, amp: f64) -> EmptyRoomBaseline {
        EmptyRoomBaseline {
            mean_amplitude: amp,
            amplitude_variance: 0.5,
            rssi_mean: -55.0,
            sample_count: 50,
            hardware_profile: profile.to_string(),
            calibrated_at: None,
            calibration_session_id: None,
            floor: None,
            floorplan_id: None,
            environment_object_ids: Vec::new(),
        }
    }

    #[test]
    fn dois_nos_geram_coordenada_finita() {
        let devices = vec![
            device("c5", "esp32c5", -23.5504802, -46.6333161),
            device("wroom", "wroom", -23.5504830, -46.6333100),
        ];
        let histories = HashMap::from([
            ("c5".to_string(), history(-48, 80.0)),
            ("wroom".to_string(), history(-55, 70.0)),
        ]);
        let baselines = HashMap::from([
            ("c5".to_string(), baseline("esp32c5", 60.0)),
            ("wroom".to_string(), baseline("wroom", 58.0)),
        ]);

        let loc = estimate_location_georeferenced(&devices, &histories, &baselines, None, None);
        assert!(loc.lat.is_finite() && loc.lng.is_finite());
        assert_eq!(loc.nodes_used, 2);
        assert!(loc.confidence >= 0.6);
        assert!(loc.accuracy_m <= 1.0);
    }

    #[test]
    fn wroom_melhora_accuracy_do_c5_sozinho() {
        let c5 = device("c5", "esp32c5", -23.5504802, -46.6333161);
        let wroom = device("wroom", "wroom", -23.5504830, -46.6333100);
        let histories = HashMap::from([
            ("c5".to_string(), history(-50, 80.0)),
            ("wroom".to_string(), history(-57, 70.0)),
        ]);
        let baselines = HashMap::from([
            ("c5".to_string(), baseline("esp32c5", 60.0)),
            ("wroom".to_string(), baseline("wroom", 58.0)),
        ]);

        let single =
            estimate_location_georeferenced(&[c5.clone()], &histories, &baselines, None, None);
        let fused =
            estimate_location_georeferenced(&[c5, wroom], &histories, &baselines, None, None);
        assert!(fused.accuracy_m < single.accuracy_m);
    }

    #[test]
    fn fallback_funciona_com_um_no() {
        let devices = vec![device("c5", "esp32c5", -23.5504802, -46.6333161)];
        let histories = HashMap::from([("c5".to_string(), history(-50, 80.0))]);
        let loc =
            estimate_location_georeferenced(&devices, &histories, &HashMap::new(), None, None);
        assert_eq!(loc.nodes_used, 1);
        assert!(loc.method.contains("fallback"));
    }

    fn map_with_room_and_walls() -> MapConfig {
        use crate::state::{EnvironmentObject, Floorplan, FloorplanBounds};
        MapConfig {
            floorplans: vec![Floorplan {
                id: "apto".into(),
                name: "Apartamento".into(),
                floor: 4,
                image_data_url: String::new(),
                bounds: FloorplanBounds {
                    north: -23.5504000,
                    south: -23.5505500,
                    east: -46.6332000,
                    west: -46.6334000,
                },
                opacity: 0.5,
            }],
            environment_objects: vec![
                EnvironmentObject {
                    id: "quarto".into(),
                    name: "Quarto".into(),
                    object_type: "room".into(),
                    lat: -23.5504816,
                    lng: -46.6333130,
                    floor: 4,
                    width_m: 12.0,
                    depth_m: 12.0,
                    rotation_deg: 0.0,
                    calibration_relevant: false,
                },
                EnvironmentObject {
                    id: "parede".into(),
                    name: "Parede da sala".into(),
                    object_type: "wall".into(),
                    lat: -23.5504816,
                    lng: -46.6333130,
                    floor: 4,
                    width_m: 0.15,
                    depth_m: 8.0,
                    rotation_deg: 0.0,
                    calibration_relevant: false,
                },
            ],
        }
    }

    #[test]
    fn planta_da_comodo_atual_e_confina() {
        let devices = vec![
            device("c5", "esp32c5", -23.5504802, -46.6333161),
            device("wroom", "wroom", -23.5504830, -46.6333100),
        ];
        let histories = HashMap::from([
            ("c5".to_string(), history(-48, 80.0)),
            ("wroom".to_string(), history(-55, 70.0)),
        ]);
        let baselines = HashMap::from([
            ("c5".to_string(), baseline("esp32c5", 60.0)),
            ("wroom".to_string(), baseline("wroom", 58.0)),
        ]);
        let map = map_with_room_and_walls();

        let loc =
            estimate_location_georeferenced(&devices, &histories, &baselines, None, Some(&map));
        // O cômodo de 12 m × 12 m cobre os dois receptores → estimativa dentro.
        assert_eq!(loc.current_room.as_deref(), Some("Quarto"));
        // A estimativa (média ponderada entre receptores) já cai dentro dos
        // bounds → sem clamp; e a parede entre os nós ativa a correção.
        assert!(!loc.clamped_to_floorplan);
        assert!(loc.lat.is_finite() && loc.lng.is_finite());
        assert!(loc.lat <= -23.5504000 && loc.lat >= -23.5505500);
        assert!(loc.lng <= -46.6332000 && loc.lng >= -46.6334000);
    }

    #[test]
    fn parede_entre_receptores_corrige_distancia() {
        let devices = vec![
            device("c5", "esp32c5", -23.5504802, -46.6333161),
            device("wroom", "wroom", -23.5504830, -46.6333100),
        ];
        let histories = HashMap::from([
            ("c5".to_string(), history(-48, 80.0)),
            ("wroom".to_string(), history(-55, 70.0)),
        ]);
        let baselines = HashMap::from([
            ("c5".to_string(), baseline("esp32c5", 60.0)),
            ("wroom".to_string(), baseline("wroom", 58.0)),
        ]);
        let map = map_with_room_and_walls();

        let sem_mapa =
            estimate_location_georeferenced(&devices, &histories, &baselines, None, None);
        let com_mapa =
            estimate_location_georeferenced(&devices, &histories, &baselines, None, Some(&map));
        // A parede vertical fica entre os receptores (lng da parede está entre
        // os lngs dos nós), então pelo menos um caminho a cruza.
        assert!(com_mapa.walls_crossed >= 1);
        assert!(com_mapa.method.contains("+paredes"));
        assert_eq!(sem_mapa.walls_crossed, 0);
        assert!(!sem_mapa.method.contains("+paredes"));
    }

    #[test]
    fn quantizacao_5cm_preserva_lat_lng() {
        let value = quantize_m(0.123, RESOLUTION_M);
        assert!((value - 0.1).abs() < 1e-9);
    }
}
