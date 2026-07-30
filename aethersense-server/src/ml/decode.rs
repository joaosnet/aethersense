//! Decodificação do heatmap do modelo em pessoas georreferenciadas.
//!
//! Porta fiel de `ml/preprocessing.py::heatmap_to_persons` (busca iterativa
//! de picos com supressão 5×5) + mapeamento da grade normalizada para
//! lat/lng usando os bounds do floorplan cadastrado ou, na ausência dele,
//! uma caixa em volta dos receptores.

use crate::state::{DeviceInfo, FloorplanBounds, MapConfig};

#[derive(Debug, Clone, Copy)]
pub struct PersonPeak {
    /// Coordenadas normalizadas [0,1]: x cresce para leste, y para o sul.
    pub x: f64,
    pub y: f64,
    pub confidence: f64,
}

/// Extrai pessoas do heatmap por picos iterativos com NMS 5×5 — o MESMO
/// decoder usado no benchmark offline, garantindo paridade treino/produção.
pub fn heatmap_to_persons(
    heatmap: &[f64],
    height: usize,
    width: usize,
    threshold: f64,
    max_people: usize,
) -> Vec<PersonPeak> {
    if heatmap.len() < height * width || height == 0 || width == 0 {
        return Vec::new();
    }
    let mut hm = heatmap[..height * width].to_vec();
    let mut persons = Vec::new();

    for _ in 0..max_people {
        let (mut best_idx, mut best_val) = (0usize, f64::NEG_INFINITY);
        for (idx, value) in hm.iter().enumerate() {
            if *value > best_val {
                best_val = *value;
                best_idx = idx;
            }
        }
        if best_val < threshold {
            break;
        }
        let y = best_idx / width;
        let x = best_idx % width;
        persons.push(PersonPeak {
            x: x as f64 / (width.max(2) - 1) as f64,
            y: y as f64 / (height.max(2) - 1) as f64,
            confidence: best_val,
        });

        // Zera vizinhança 5×5 ao redor do pico (supressão de não-máximos).
        let y0 = y.saturating_sub(2);
        let y1 = (y + 3).min(height);
        let x0 = x.saturating_sub(2);
        let x1 = (x + 3).min(width);
        for yy in y0..y1 {
            for xx in x0..x1 {
                hm[yy * width + xx] = 0.0;
            }
        }
    }
    persons
}

/// Resolve os bounds geográficos para mapear o heatmap:
/// 1º floorplan cadastrado; senão caixa de ~10 m ao redor dos receptores.
pub fn resolve_bounds(map_config: &MapConfig, devices: &[DeviceInfo]) -> Option<FloorplanBounds> {
    if let Some(plan) = map_config.floorplans.first() {
        return Some(plan.bounds.clone());
    }

    let receivers: Vec<&DeviceInfo> = devices
        .iter()
        .filter(|d| d.role == "receiver" && d.lat.is_finite() && d.lng.is_finite())
        .collect();
    if receivers.is_empty() {
        return None;
    }

    let (mut north, mut south) = (f64::NEG_INFINITY, f64::INFINITY);
    let (mut east, mut west) = (f64::NEG_INFINITY, f64::INFINITY);
    for device in &receivers {
        north = north.max(device.lat);
        south = south.min(device.lat);
        east = east.max(device.lng);
        west = west.min(device.lng);
    }

    // Margem de ~5 m em graus (1° lat ≈ 111 320 m; lng corrigido pela latitude).
    let mid_lat = (north + south) / 2.0;
    let lat_margin = 5.0 / 111_320.0;
    let lng_margin = 5.0 / (111_320.0 * mid_lat.to_radians().cos().abs().max(1e-6));
    Some(FloorplanBounds {
        north: north + lat_margin,
        south: south - lat_margin,
        east: east + lng_margin,
        west: west - lng_margin,
    })
}

/// Converte coordenadas normalizadas do heatmap em lat/lng.
/// y=0 é a borda norte (linha 0 do heatmap), x=0 a borda oeste — consistente
/// com os anchors `a..e` do WiMANS usados no treino (origem no canto).
pub fn norm_to_latlng(x_norm: f64, y_norm: f64, bounds: &FloorplanBounds) -> (f64, f64) {
    let lat = bounds.north - y_norm.clamp(0.0, 1.0) * (bounds.north - bounds.south);
    let lng = bounds.west + x_norm.clamp(0.0, 1.0) * (bounds.east - bounds.west);
    (lat, lng)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodifica_dois_picos_separados() {
        let (h, w) = (20, 20);
        let mut hm = vec![0.0; h * w];
        hm[3 * w + 4] = 0.9; // pico 1 em (y=3, x=4)
        hm[15 * w + 16] = 0.8; // pico 2 em (y=15, x=16)
        let persons = heatmap_to_persons(&hm, h, w, 0.35, 5);
        assert_eq!(persons.len(), 2);
        assert!(persons[0].confidence > persons[1].confidence);
    }

    #[test]
    fn pico_abaixo_do_limiar_nao_vira_pessoa() {
        let (h, w) = (20, 20);
        let mut hm = vec![0.0; h * w];
        hm[5 * w + 5] = 0.2;
        assert!(heatmap_to_persons(&hm, h, w, 0.35, 5).is_empty());
    }

    #[test]
    fn nms_suprime_vizinhos_do_pico() {
        let (h, w) = (20, 20);
        let mut hm = vec![0.0; h * w];
        hm[10 * w + 10] = 0.9;
        hm[10 * w + 11] = 0.85; // vizinho imediato — deve ser suprimido
        let persons = heatmap_to_persons(&hm, h, w, 0.35, 5);
        assert_eq!(persons.len(), 1);
    }

    #[test]
    fn norm_to_latlng_mapeia_cantos() {
        let bounds = FloorplanBounds {
            north: -1.0,
            south: -1.001,
            east: -48.0,
            west: -48.001,
        };
        let (lat_nw, lng_nw) = norm_to_latlng(0.0, 0.0, &bounds);
        assert!((lat_nw - bounds.north).abs() < 1e-12);
        assert!((lng_nw - bounds.west).abs() < 1e-12);
        let (lat_se, lng_se) = norm_to_latlng(1.0, 1.0, &bounds);
        assert!((lat_se - bounds.south).abs() < 1e-12);
        assert!((lng_se - bounds.east).abs() < 1e-12);
    }
}
