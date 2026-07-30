//! Geometria da planta baixa aplicada à localização.
//!
//! Torna a planta FUNCIONAL (não só visual): cômodos desenhados viram
//! point-in-polygon ("cômodo atual"), paredes atenuam o modelo RSSI→distância
//! e os bounds do floorplan confinam a estimativa ao apartamento.
//!
//! A matemática do polígono ESPELHA `objectPolygonPoints` do frontend
//! (AetherMap.tsx): cantos em metros (±w/2, ±d/2) rotacionados por
//! `rotation_deg` e convertidos para lat/lng com 111.320 m/grau e
//! `cos(lat)` clampado em 0,05 — assim o que o usuário desenha no mapa é
//! exatamente o que o backend calcula.

use crate::state::{EnvironmentObject, Floorplan};

const EARTH_METERS_PER_DEG_LAT: f64 = 111_320.0;

/// Ponto geográfico (lat, lng) em graus.
pub type LatLng = (f64, f64);

/// Cantos (lat, lng) do retângulo rotacionado de um objeto de ambiente —
/// mesma fórmula do frontend (`objectPolygonPoints`).
pub fn object_polygon_latlng(object: &EnvironmentObject) -> [LatLng; 4] {
    let width = object.width_m.max(0.1);
    let depth = object.depth_m.max(0.1);
    let cos_lat = (object.lat.to_radians().cos()).max(0.05);
    let rad = object.rotation_deg.to_radians();
    let (sin, cos) = rad.sin_cos();

    let corners_m = [
        (-width / 2.0, -depth / 2.0),
        (width / 2.0, -depth / 2.0),
        (width / 2.0, depth / 2.0),
        (-width / 2.0, depth / 2.0),
    ];

    corners_m.map(|(east_m, north_m)| {
        let rotated_east = east_m * cos - north_m * sin;
        let rotated_north = east_m * sin + north_m * cos;
        (
            object.lat + rotated_north / EARTH_METERS_PER_DEG_LAT,
            object.lng + rotated_east / (EARTH_METERS_PER_DEG_LAT * cos_lat),
        )
    })
}

/// Ray casting clássico. Correto mesmo com eixos em escalas diferentes
/// (lat/lng): o teste é topológico, não métrico.
pub fn point_in_polygon(point: LatLng, polygon: &[LatLng]) -> bool {
    let (py, px) = point; // (lat, lng) → y = lat, x = lng
    let mut inside = false;
    let n = polygon.len();
    let mut j = n - 1;
    for i in 0..n {
        let (yi, xi) = polygon[i];
        let (yj, xj) = polygon[j];
        if ((yi > py) != (yj > py)) && (px < (xj - xi) * (py - yi) / (yj - yi + f64::EPSILON) + xi)
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn is_room(object: &EnvironmentObject) -> bool {
    let t = object.object_type.to_lowercase();
    t.contains("room") || t.contains("comodo") || t.contains("cômodo")
}

fn is_wall(object: &EnvironmentObject) -> bool {
    let t = object.object_type.to_lowercase();
    t.contains("wall") || t.contains("parede")
}

/// Cômodo (objeto tipo `room`) que contém o ponto, no andar dado. Com cômodos
/// sobrepostos vence o de MENOR área (mais específico).
pub fn current_room(point: LatLng, floor: i32, objects: &[EnvironmentObject]) -> Option<String> {
    objects
        .iter()
        .filter(|o| o.floor == floor && is_room(o))
        .filter(|o| point_in_polygon(point, &object_polygon_latlng(o)))
        .min_by(|a, b| {
            let area_a = a.width_m.max(0.1) * a.depth_m.max(0.1);
            let area_b = b.width_m.max(0.1) * b.depth_m.max(0.1);
            area_a
                .partial_cmp(&area_b)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|o| o.name.clone())
}

/// Orientação do trio de pontos (produto vetorial 2D).
fn cross(o: LatLng, a: LatLng, b: LatLng) -> f64 {
    (a.1 - o.1) * (b.0 - o.0) - (a.0 - o.0) * (b.1 - o.1)
}

/// Interseção de segmentos (com colinearidade tratada como não-cruzamento —
/// suficiente para contagem de paredes; um raio raspando a borda não conta).
fn segments_intersect(p1: LatLng, p2: LatLng, q1: LatLng, q2: LatLng) -> bool {
    let d1 = cross(q1, q2, p1);
    let d2 = cross(q1, q2, p2);
    let d3 = cross(p1, p2, q1);
    let d4 = cross(p1, p2, q2);
    (d1 * d2 < 0.0) && (d3 * d4 < 0.0)
}

/// Quantas PAREDES (objetos tipo `wall`) do andar o segmento `from→to` cruza.
/// Uma parede conta uma única vez mesmo que o segmento cruze duas arestas do
/// seu retângulo (entra e sai).
pub fn walls_crossed(from: LatLng, to: LatLng, floor: i32, objects: &[EnvironmentObject]) -> usize {
    objects
        .iter()
        .filter(|o| o.floor == floor && is_wall(o))
        .filter(|o| {
            let poly = object_polygon_latlng(o);
            let n = poly.len();
            (0..n).any(|i| segments_intersect(from, to, poly[i], poly[(i + 1) % n]))
                || point_in_polygon(from, &poly)
                || point_in_polygon(to, &poly)
        })
        .count()
}

/// Confina (lat, lng) à UNIÃO dos bounds das plantas do andar. `None` quando o
/// andar não tem planta cadastrada (sem planta, sem confinamento). Retorna o
/// ponto (possivelmente inalterado) e se houve clamp.
pub fn clamp_to_floorplans(
    point: LatLng,
    floor: i32,
    floorplans: &[Floorplan],
) -> Option<(LatLng, bool)> {
    let mut bounds: Option<(f64, f64, f64, f64)> = None; // (south, north, west, east)
    for plan in floorplans.iter().filter(|p| p.floor == floor) {
        let b = &plan.bounds;
        let (south, north) = (b.south.min(b.north), b.south.max(b.north));
        let (west, east) = (b.west.min(b.east), b.west.max(b.east));
        bounds = Some(match bounds {
            None => (south, north, west, east),
            Some((s, n, w, e)) => (s.min(south), n.max(north), w.min(west), e.max(east)),
        });
    }
    let (south, north, west, east) = bounds?;
    let clamped = (point.0.clamp(south, north), point.1.clamp(west, east));
    let moved = (clamped.0 - point.0).abs() > 1e-12 || (clamped.1 - point.1).abs() > 1e-12;
    Some((clamped, moved))
}

/// Confina (lat, lng) aos CÔMODOS (objetos `room`) do andar.
/// Se o ponto já está dentro de um cômodo, retorna inalterado.
/// Se está fora de todos os cômodos, encontra o cômodo mais próximo e confina
/// o ponto para dentro da sua fronteira.
/// Se não houver cômodos cadastrados no andar, faz fallback para `clamp_to_floorplans`.
pub fn clamp_to_rooms_or_floorplans(
    point: LatLng,
    floor: i32,
    objects: &[EnvironmentObject],
    floorplans: &[Floorplan],
) -> Option<(LatLng, bool)> {
    let rooms: Vec<&EnvironmentObject> = objects
        .iter()
        .filter(|o| o.floor == floor && is_room(o))
        .collect();

    if !rooms.is_empty() {
        if current_room(point, floor, objects).is_some() {
            return Some((point, false));
        }

        let cos_lat = (point.0.to_radians()).cos().abs().max(0.05);
        let mut best_dist_sq = f64::INFINITY;
        let mut best_clamped = point;

        for room in rooms {
            let poly = object_polygon_latlng(room);
            let n = poly.len();
            if n < 3 {
                continue;
            }
            let center = (room.lat, room.lng);

            for i in 0..n {
                let a = poly[i];
                let b = poly[(i + 1) % n];

                let v_lat = b.0 - a.0;
                let v_lng = (b.1 - a.1) * cos_lat;
                let w_lat = point.0 - a.0;
                let w_lng = (point.1 - a.1) * cos_lat;

                let len_sq = v_lat * v_lat + v_lng * v_lng;
                let t = if len_sq > 1e-18 {
                    ((w_lat * v_lat + w_lng * v_lng) / len_sq).clamp(0.0, 1.0)
                } else {
                    0.0
                };

                let c_lat = a.0 + t * (b.0 - a.0);
                let c_lng = a.1 + t * (b.1 - a.1);

                let d_lat = point.0 - c_lat;
                let d_lng = (point.1 - c_lng) * cos_lat;
                let dist_sq = d_lat * d_lat + d_lng * d_lng;

                if dist_sq < best_dist_sq {
                    best_dist_sq = dist_sq;
                    best_clamped = (
                        c_lat * 0.995 + center.0 * 0.005,
                        c_lng * 0.995 + center.1 * 0.005,
                    );
                }
            }
        }

        let moved =
            (best_clamped.0 - point.0).abs() > 1e-12 || (best_clamped.1 - point.1).abs() > 1e-12;
        return Some((best_clamped, moved));
    }

    clamp_to_floorplans(point, floor, floorplans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::FloorplanBounds;

    const LAT0: f64 = -23.5505000;
    const LNG0: f64 = -46.6333000;

    fn object(name: &str, kind: &str, w: f64, d: f64, rot: f64) -> EnvironmentObject {
        EnvironmentObject {
            id: name.to_string(),
            name: name.to_string(),
            object_type: kind.to_string(),
            lat: LAT0,
            lng: LNG0,
            floor: 4,
            width_m: w,
            depth_m: d,
            rotation_deg: rot,
            calibration_relevant: false,
        }
    }

    /// Desloca (lat, lng) por metros — mesma escala do módulo.
    fn offset_m(north_m: f64, east_m: f64) -> LatLng {
        let cos_lat = LAT0.to_radians().cos().max(0.05);
        (
            LAT0 + north_m / EARTH_METERS_PER_DEG_LAT,
            LNG0 + east_m / (EARTH_METERS_PER_DEG_LAT * cos_lat),
        )
    }

    #[test]
    fn ponto_dentro_e_fora_do_comodo() {
        let room = object("Quarto", "room", 4.0, 3.0, 0.0);
        let poly = object_polygon_latlng(&room);
        assert!(point_in_polygon(offset_m(0.0, 0.0), &poly));
        assert!(point_in_polygon(offset_m(1.0, 1.5), &poly));
        assert!(!point_in_polygon(offset_m(2.0, 0.0), &poly)); // fora (depth/2 = 1,5)
        assert!(!point_in_polygon(offset_m(0.0, 3.0), &poly)); // fora (width/2 = 2,0)
    }

    #[test]
    fn rotacao_gira_o_retangulo() {
        // 4 m × 0,5 m rotacionado 90°: o eixo LONGO vira norte-sul.
        let room = object("Faixa", "room", 4.0, 0.5, 90.0);
        let poly = object_polygon_latlng(&room);
        assert!(point_in_polygon(offset_m(1.5, 0.0), &poly)); // ao norte, dentro
        assert!(!point_in_polygon(offset_m(0.0, 1.5), &poly)); // a leste, fora
    }

    #[test]
    fn comodo_mais_especifico_vence() {
        let casa = object("Casa", "room", 10.0, 10.0, 0.0);
        let quarto = object("Quarto", "room", 3.0, 3.0, 0.0);
        let objs = vec![casa, quarto];
        assert_eq!(
            current_room(offset_m(0.0, 0.0), 4, &objs),
            Some("Quarto".to_string())
        );
    }

    #[test]
    fn andar_diferente_nao_conta() {
        let room = object("Quarto", "room", 4.0, 3.0, 0.0);
        assert_eq!(current_room(offset_m(0.0, 0.0), 5, &[room]), None);
    }

    #[test]
    fn parede_cruzada_conta_uma_vez() {
        // Parede fina norte-sul em x=0; segmento oeste→leste cruza (entra e sai).
        let wall = object("Parede", "wall", 0.15, 4.0, 0.0);
        let from = offset_m(0.0, -2.0);
        let to = offset_m(0.0, 2.0);
        assert_eq!(walls_crossed(from, to, 4, &[wall.clone()]), 1);
        // Segmento paralelo, longe da parede: zero.
        let from2 = offset_m(3.0, -2.0);
        let to2 = offset_m(3.0, 2.0);
        assert_eq!(walls_crossed(from2, to2, 4, &[wall]), 0);
    }

    #[test]
    fn movel_nao_e_parede() {
        let desk = object("Mesa", "desk", 2.0, 1.0, 0.0);
        assert_eq!(
            walls_crossed(offset_m(0.0, -2.0), offset_m(0.0, 2.0), 4, &[desk]),
            0
        );
    }

    #[test]
    fn clamp_confina_a_planta() {
        let plan = Floorplan {
            id: "p".into(),
            name: "Planta".into(),
            floor: 4,
            image_data_url: String::new(),
            bounds: FloorplanBounds {
                north: LAT0 + 0.0001,
                south: LAT0 - 0.0001,
                east: LNG0 + 0.0001,
                west: LNG0 - 0.0001,
            },
            opacity: 0.5,
        };
        // Ponto ao norte, fora dos bounds → clampado à borda norte.
        let ((lat, lng), moved) =
            clamp_to_floorplans((LAT0 + 0.001, LNG0), 4, &[plan.clone()]).unwrap();
        assert!(moved);
        assert!((lat - (LAT0 + 0.0001)).abs() < 1e-12);
        assert!((lng - LNG0).abs() < 1e-12);
        // Ponto dentro → inalterado.
        let ((lat2, _), moved2) = clamp_to_floorplans((LAT0, LNG0), 4, &[plan.clone()]).unwrap();
        assert!(!moved2);
        assert!((lat2 - LAT0).abs() < 1e-12);
        // Andar sem planta → None.
        assert!(clamp_to_floorplans((LAT0, LNG0), 5, &[plan]).is_none());
    }

    #[test]
    fn clamp_confina_ao_comodo_mais_proximo() {
        let room = object("Sala", "room", 4.0, 4.0, 0.0);
        // Ponto fora (a oeste do cômodo)
        let out_point = offset_m(0.0, -5.0);
        let ((clamped_lat, clamped_lng), moved) =
            clamp_to_rooms_or_floorplans(out_point, 4, &[room.clone()], &[]).unwrap();
        assert!(moved);
        // Garante que o ponto confinado agora cai DENTRO do cômodo
        assert_eq!(
            current_room((clamped_lat, clamped_lng), 4, &[room]),
            Some("Sala".to_string())
        );
    }
}
