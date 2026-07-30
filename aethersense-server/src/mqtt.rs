use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, Event, LastWill, MqttOptions, Packet, QoS};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{Notify, RwLock};
use tracing::{info, warn};

use crate::accessories::{
    AccessoryCommand, AccessoryKind, AccessoryRegistry, CommandInput, DiscoveryKind,
};
use crate::state::{AppState, InnerState, MqttSettings};

const MQTT_AVAILABILITY_ONLINE: &str = "online";
const MQTT_AVAILABILITY_OFFLINE: &str = "offline";
const MQTT_STATE_SUFFIX: &str = "detection/state";
const MQTT_STATUS_SUFFIX: &str = "status";
const MQTT_MANUFACTURER: &str = "AetherSense";
const MQTT_MODEL: &str = "CSI Sensing Server";
const MQTT_SUPPORT_URL: &str = "https://github.com/joaosnet/aethersense";
const MQTT_DEVICE_SEGMENT: &str = "devices";
const MQTT_SERVER_COMMAND_SUFFIX: &str = "server/command";

#[derive(Debug, Clone, Serialize)]
pub struct MqttDetectionSnapshot {
    pub count: u64,
    pub rssi: Option<i32>,
    pub rate: Option<i32>,
    pub presence: crate::state::PresenceState,
    pub movement: crate::state::MovementState,
    pub location: crate::state::LocationEstimate,
    pub vitals: crate::state::VitalsEstimate,
    /// Estimativa ML (ONNX) — contagem multi-pessoa + pessoas + vitais ML.
    pub occupancy: crate::state::OccupancyEstimate,
    /// Zona (cômodo) com maior perturbação — automações por cômodo.
    pub active_zone: crate::state::ActiveZoneState,
    /// Zona dedicada do box, com confirmação temporal e diagnósticos dos
    /// receptores usados para comandar a válvula.
    pub shower_box: crate::state::ShowerBoxState,
    /// Ocupação binária GLOBAL já confirmada pela zona (subtração de baseline).
    /// O template do Home Assistant só lê este booleano, sem herdar os falsos
    /// positivos do ML nem da variância. Calculado em [`confirm_occupancy`].
    pub occupancy_confirmed: bool,
}

/// Decide a ocupação binária GLOBAL publicada ao Home Assistant.
///
/// Histórico: o sensor era "ML-first" (`occupancy.count > 0`), porque a
/// heurística de variância (`presence.detected`) dava falso positivo em sala
/// vazia. Mas em 2026-06-15, com o C5 sozinho (Cenário 2), o próprio modelo
/// ONNX (treinado no WiMANS) passou a dar falso positivo constante (`count=1`
/// com a sala vazia) — lacuna de domínio. O sinal confiável é a ZONA
/// (subtração de baseline + histerese + retenção por respiração já validada no
/// loop de detecção), que acertou "vazia". Por isso a ocupação global passa a
/// ser CONFIRMADA pela zona: `ON` apenas se alguma zona estiver ocupada. Sem
/// zonas (nenhum receptor com baseline calibrado) não há sinal de zona, então
/// mantém-se o comportamento antigo como degenerado.
fn confirm_occupancy(
    occupancy: &crate::state::OccupancyEstimate,
    presence: &crate::state::PresenceState,
    active_zone: &crate::state::ActiveZoneState,
) -> bool {
    if active_zone.zones.is_empty() {
        // Sem baseline/zonas: fallback ao comportamento anterior (ML-first,
        // ou variância quando não há modelo).
        if occupancy.available {
            occupancy.count >= 1
        } else {
            presence.detected
        }
    } else {
        active_zone.zones.iter().any(|zone| zone.occupied)
    }
}

#[derive(Clone)]
pub struct MqttPublisher {
    client: AsyncClient,
    base_topic: String,
    discovery_prefix: String,
    retain: bool,
    version: String,
}

impl MqttPublisher {
    fn state_topic(&self) -> String {
        format!("{}/{}", self.base_topic, MQTT_STATE_SUFFIX)
    }

    fn status_topic(&self) -> String {
        format!("{}/{}", self.base_topic, MQTT_STATUS_SUFFIX)
    }

    fn discovery_topic(&self, component: &str, object_id: &str) -> String {
        format!(
            "{}/{}/{}/config",
            self.discovery_prefix, component, object_id
        )
    }

    fn device_topic(&self, device_id: &str, suffix: &str) -> String {
        format!(
            "{}/{}/{}/{}",
            self.base_topic.trim_end_matches('/'),
            MQTT_DEVICE_SEGMENT,
            device_id,
            suffix
        )
    }

    fn server_command_topic(&self) -> String {
        format!(
            "{}/{}",
            self.base_topic.trim_end_matches('/'),
            MQTT_SERVER_COMMAND_SUFFIX
        )
    }

    fn device_payload(&self) -> Value {
        json!({
            "identifiers": ["aethersense-server"],
            "name": "AetherSense",
            "manufacturer": MQTT_MANUFACTURER,
            "model": MQTT_MODEL,
            "sw_version": self.version,
            "configuration_url": MQTT_SUPPORT_URL,
        })
    }

    fn origin_payload(&self) -> Value {
        json!({
            "name": "AetherSense Server",
            "sw_version": self.version,
            "support_url": MQTT_SUPPORT_URL,
        })
    }

    fn shared_payload(&self) -> Value {
        json!({
            "availability_topic": self.status_topic(),
            "payload_available": MQTT_AVAILABILITY_ONLINE,
            "payload_not_available": MQTT_AVAILABILITY_OFFLINE,
            "qos": 1,
            "device": self.device_payload(),
            "origin": self.origin_payload(),
        })
    }

    fn occupancy_payload(&self) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(
                json!({
                    "name": "Occupancy",
                    "unique_id": "aethersense_occupancy",
                    "object_id": "aethersense_occupancy",
                    "device_class": "occupancy",
                    "state_topic": self.state_topic(),
                    // Confirmada pela ZONA: tanto a variância quanto o modelo ML
                    // dão falso positivo em sala vazia (o ML com o C5 sozinho /
                    // Cenário 2, observado em 2026-06-15). O sinal confiável é a
                    // zona (baseline + histerese + retenção por respiração),
                    // consolidado em `confirm_occupancy`. O template só lê o booleano.
                    "value_template": "{{ 'ON' if value_json.occupancy_confirmed else 'OFF' }}",
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            );
        }
        payload
    }

    fn movement_payload(&self) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(
                json!({
                    "name": "Movement",
                    "unique_id": "aethersense_movement",
                    "object_id": "aethersense_movement",
                    "device_class": "enum",
                    "options": ["static", "slight_movement", "walking", "running"],
                    "state_topic": self.state_topic(),
                    "value_template": "{{ value_json.movement.level }}",
                    "icon": "mdi:run",
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            );
        }
        payload
    }

    fn confidence_payload(&self) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(json!({
                "name": "Presence Confidence",
                "unique_id": "aethersense_presence_confidence",
                "object_id": "aethersense_presence_confidence",
                "state_topic": self.state_topic(),
                "value_template": "{{ (value_json.presence.confidence | float(0) * 100) | round(1) }}",
                "unit_of_measurement": "%",
                "state_class": "measurement",
                "suggested_display_precision": 1,
            })
            .as_object()
            .cloned()
            .unwrap_or_default());
        }
        payload
    }

    fn floor_payload(&self) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(
                json!({
                    "name": "Floor",
                    "unique_id": "aethersense_floor",
                    "object_id": "aethersense_floor",
                    "state_topic": self.state_topic(),
                    "value_template": "{{ value_json.location.floor | int(0) }}",
                    "icon": "mdi:floor-plan",
                    "state_class": "measurement",
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            );
        }
        payload
    }

    fn accuracy_payload(&self) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(
                json!({
                    "name": "Location Accuracy",
                    "unique_id": "aethersense_accuracy_m",
                    "object_id": "aethersense_accuracy_m",
                    "state_topic": self.state_topic(),
                    "value_template": "{{ value_json.location.accuracy_m | float(0) | round(1) }}",
                    "unit_of_measurement": "m",
                    "device_class": "distance",
                    "state_class": "measurement",
                    "suggested_display_precision": 1,
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            );
        }
        payload
    }

    fn latitude_payload(&self) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(
                json!({
                    "name": "Location Latitude",
                    "unique_id": "aethersense_location_latitude",
                    "object_id": "aethersense_location_latitude",
                    "state_topic": self.state_topic(),
                    "value_template": "{{ value_json.location.lat | float(0) | round(8) }}",
                    "unit_of_measurement": "deg",
                    "state_class": "measurement",
                    "suggested_display_precision": 8,
                    "icon": "mdi:latitude",
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            );
        }
        payload
    }

    fn longitude_payload(&self) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(
                json!({
                    "name": "Location Longitude",
                    "unique_id": "aethersense_location_longitude",
                    "object_id": "aethersense_location_longitude",
                    "state_topic": self.state_topic(),
                    "value_template": "{{ value_json.location.lng | float(0) | round(8) }}",
                    "unit_of_measurement": "deg",
                    "state_class": "measurement",
                    "suggested_display_precision": 8,
                    "icon": "mdi:longitude",
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            );
        }
        payload
    }

    fn location_confidence_payload(&self) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(json!({
                "name": "Location Confidence",
                "unique_id": "aethersense_location_confidence",
                "object_id": "aethersense_location_confidence",
                "state_topic": self.state_topic(),
                "value_template": "{{ (value_json.location.confidence | float(0) * 100) | round(1) }}",
                "unit_of_measurement": "%",
                "state_class": "measurement",
                "suggested_display_precision": 1,
                "icon": "mdi:crosshairs-gps",
            })
            .as_object()
            .cloned()
            .unwrap_or_default());
        }
        payload
    }

    fn nodes_used_payload(&self) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(
                json!({
                    "name": "Location Nodes Used",
                    "unique_id": "aethersense_location_nodes_used",
                    "object_id": "aethersense_location_nodes_used",
                    "state_topic": self.state_topic(),
                    "value_template": "{{ value_json.location.nodes_used | int(0) }}",
                    "state_class": "measurement",
                    "icon": "mdi:access-point-network",
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            );
        }
        payload
    }

    fn location_method_payload(&self) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(
                json!({
                    "name": "Location Method",
                    "unique_id": "aethersense_location_method",
                    "object_id": "aethersense_location_method",
                    "state_topic": self.state_topic(),
                    "value_template": "{{ value_json.location.method }}",
                    "icon": "mdi:map-marker-path",
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            );
        }
        payload
    }

    /// Cômodo atual — a planta baixa FUNCIONAL: objetos `room` desenhados no
    /// mapa viram point-in-polygon no backend e o nome do cômodo é publicado
    /// para automações por cômodo GEOMÉTRICO (complementa as zonas por
    /// receptor).
    fn current_room_payload(&self) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(json!({
                "name": "Current Room",
                "unique_id": "aethersense_current_room",
                "object_id": "aethersense_current_room",
                "state_topic": self.state_topic(),
                "value_template": "{{ value_json.location.current_room | default('nenhum', true) }}",
                "icon": "mdi:floor-plan",
            })
            .as_object()
            .cloned()
            .unwrap_or_default());
        }
        payload
    }

    fn heart_rate_payload(&self) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(
                json!({
                    "name": "Heart Rate",
                    "unique_id": "aethersense_heart_rate_bpm",
                    "object_id": "aethersense_heart_rate_bpm",
                    "state_topic": self.state_topic(),
                    "value_template": "{{ value_json.vitals.heart_rate_bpm }}",
                    "unit_of_measurement": "bpm",
                    "state_class": "measurement",
                    "suggested_display_precision": 0,
                    "icon": "mdi:heart-pulse",
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            );
        }
        payload
    }

    fn breathing_rate_payload(&self) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(
                json!({
                    "name": "Breathing Rate",
                    "unique_id": "aethersense_breathing_rate",
                    "object_id": "aethersense_breathing_rate",
                    "state_topic": self.state_topic(),
                    "value_template": "{{ value_json.vitals.breathing_rate }}",
                    "unit_of_measurement": "breaths/min",
                    "state_class": "measurement",
                    "suggested_display_precision": 0,
                    "icon": "mdi:lungs",
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            );
        }
        payload
    }

    fn rssi_payload(&self) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(
                json!({
                    "name": "RSSI",
                    "unique_id": "aethersense_rssi_dbm",
                    "object_id": "aethersense_rssi_dbm",
                    "state_topic": self.state_topic(),
                    "value_template": "{{ value_json.rssi }}",
                    "unit_of_measurement": "dBm",
                    "state_class": "measurement",
                    "suggested_display_precision": 0,
                    "entity_category": "diagnostic",
                    "icon": "mdi:wifi-strength-2",
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            );
        }
        payload
    }

    async fn publish_json_topic(
        &self,
        topic: String,
        payload: Value,
    ) -> Result<(), rumqttc::ClientError> {
        self.client
            // MQTT Discovery precisa sobreviver a reinícios do Home Assistant,
            // mesmo quando o usuário desabilita retenção dos snapshots.
            .publish(topic, QoS::AtLeastOnce, true, payload.to_string())
            .await
    }

    async fn publish_text_topic(
        &self,
        topic: String,
        payload: &str,
        retain: bool,
    ) -> Result<(), rumqttc::ClientError> {
        self.client
            .publish(topic, QoS::AtLeastOnce, retain, payload)
            .await
    }

    /// Slug seguro para object_id/unique_id a partir do nome da zona.
    fn zone_slug(name: &str) -> String {
        let mut slug: String = name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        while slug.contains("__") {
            slug = slug.replace("__", "_");
        }
        slug.trim_matches('_').to_string()
    }

    /// Binary sensor de ocupação POR ZONA (decisão debounced do servidor) —
    /// automações disparam imediatamente, sem `for:` no Home Assistant.
    fn zone_occupancy_payload(&self, device_id: &str, device_name: &str, slug: &str) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(json!({
                "name": format!("Zona {device_name}"),
                "unique_id": format!("aethersense_zona_{slug}"),
                "object_id": format!("aethersense_zona_{slug}"),
                "device_class": "occupancy",
                "state_topic": self.state_topic(),
                "value_template": format!(
                    "{{{{ 'ON' if ((value_json.active_zone.zones | selectattr('device_id','eq','{device_id}') | list | first | default({{}})).occupied | default(false)) else 'OFF' }}}}"
                ),
            })
            .as_object()
            .cloned()
            .unwrap_or_default());
        }
        payload
    }

    fn discovery_payloads(&self, receivers: &[(String, String)]) -> Vec<(String, Value)> {
        let mut payloads = self.static_discovery_payloads();
        // `box` é reservado para o tracker dedicado. Também desambigua nomes
        // repetidos sem alterar os entity_ids existentes quando não há colisão.
        let mut used_zone_slugs = std::collections::HashSet::from(["box".to_string()]);
        for (device_id, device_name) in receivers {
            let base_slug = match Self::zone_slug(device_name) {
                slug if slug.is_empty() => "receptor".to_string(),
                slug => slug,
            };
            let mut slug = base_slug.clone();
            if used_zone_slugs.contains(&slug) {
                let device_suffix = match Self::zone_slug(device_id) {
                    suffix if suffix.is_empty() => "id".to_string(),
                    suffix => suffix,
                };
                slug = format!("{base_slug}_{device_suffix}");
            }
            let mut collision_index = 2usize;
            while !used_zone_slugs.insert(slug.clone()) {
                slug = format!("{base_slug}_{collision_index}");
                collision_index += 1;
            }
            payloads.push((
                self.discovery_topic("binary_sensor", &format!("aethersense_zona_{slug}")),
                self.zone_occupancy_payload(device_id, device_name, &slug),
            ));
        }
        payloads
    }

    fn static_discovery_payloads(&self) -> Vec<(String, Value)> {
        vec![
            (
                self.discovery_topic("binary_sensor", "aethersense_occupancy"),
                self.occupancy_payload(),
            ),
            (
                self.discovery_topic("binary_sensor", "aethersense_zona_box"),
                self.shower_box_payload(),
            ),
            (
                self.discovery_topic("sensor", "aethersense_presence_confidence"),
                self.confidence_payload(),
            ),
            (
                self.discovery_topic("sensor", "aethersense_floor"),
                self.floor_payload(),
            ),
            (
                self.discovery_topic("sensor", "aethersense_accuracy_m"),
                self.accuracy_payload(),
            ),
            (
                self.discovery_topic("sensor", "aethersense_location_latitude"),
                self.latitude_payload(),
            ),
            (
                self.discovery_topic("sensor", "aethersense_location_longitude"),
                self.longitude_payload(),
            ),
            (
                self.discovery_topic("sensor", "aethersense_location_confidence"),
                self.location_confidence_payload(),
            ),
            (
                self.discovery_topic("sensor", "aethersense_location_nodes_used"),
                self.nodes_used_payload(),
            ),
            (
                self.discovery_topic("sensor", "aethersense_location_method"),
                self.location_method_payload(),
            ),
            (
                self.discovery_topic("sensor", "aethersense_current_room"),
                self.current_room_payload(),
            ),
            (
                self.discovery_topic("sensor", "aethersense_heart_rate_bpm"),
                self.heart_rate_payload(),
            ),
            (
                self.discovery_topic("sensor", "aethersense_breathing_rate"),
                self.breathing_rate_payload(),
            ),
            (
                self.discovery_topic("sensor", "aethersense_movement"),
                self.movement_payload(),
            ),
            (
                self.discovery_topic("sensor", "aethersense_rssi_dbm"),
                self.rssi_payload(),
            ),
            (
                self.discovery_topic("sensor", "aethersense_ml_person_count"),
                self.ml_person_count_payload(),
            ),
            (
                self.discovery_topic("sensor", "aethersense_ml_count_confidence"),
                self.ml_count_confidence_payload(),
            ),
            (
                self.discovery_topic("sensor", "aethersense_ml_method"),
                self.ml_method_payload(),
            ),
            (
                self.discovery_topic("sensor", "aethersense_ml_breathing_rate"),
                self.ml_breathing_payload(),
            ),
            (
                self.discovery_topic("sensor", "aethersense_ml_heart_rate_bpm"),
                self.ml_heart_payload(),
            ),
            (
                self.discovery_topic("sensor", "aethersense_active_zone"),
                self.active_zone_payload(),
            ),
        ]
    }

    /// Entidade fixa do box. A disponibilidade MQTT representa a saúde do
    /// servidor; perdas internas de CSI continuam publicando `holding` e depois
    /// `OFF`, para que o Home Assistant consiga fechar a válvula ao fim dos 60 s.
    fn shower_box_payload(&self) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(json!({
                "name": "Zona Box",
                "unique_id": "aethersense_zona_box",
                "object_id": "aethersense_zona_box",
                "device_class": "occupancy",
                "state_topic": self.state_topic(),
                "value_template": "{{ 'ON' if value_json.shower_box.occupied | default(false) else 'OFF' }}",
                "json_attributes_topic": self.state_topic(),
                "json_attributes_template": "{{ value_json.shower_box | tojson }}",
                "icon": "mdi:shower-head",
            })
            .as_object()
            .cloned()
            .unwrap_or_default());
        }
        payload
    }

    fn active_zone_payload(&self) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(json!({
                "name": "Active Zone",
                "unique_id": "aethersense_active_zone",
                "object_id": "aethersense_active_zone",
                "state_topic": self.state_topic(),
                "value_template": "{{ value_json.active_zone.zone_name | default('nenhuma') }}",
                // Atributos: score por zona — permite automações por cômodo
                // baseadas no score do receptor daquele cômodo (ex.: chuveiro
                // dispara pelo score do WROOM, não pelo count do C5).
                "json_attributes_topic": self.state_topic(),
                "json_attributes_template": "{{ {'score': value_json.active_zone.score, 'zones': value_json.active_zone.zones} | tojson }}",
                "icon": "mdi:floor-plan",
            })
            .as_object()
            .cloned()
            .unwrap_or_default());
        }
        payload
    }

    /// Disponibilidade composta para sensores ML numéricos: além do status
    /// online do servidor, o próprio estado precisa indicar que o valor
    /// existe — senão o HA reclamaria de valor não numérico em sensores com
    /// `state_class: measurement`.
    fn ml_availability(&self, condition_template: &str) -> Value {
        json!([
            {
                "topic": self.status_topic(),
                "payload_available": MQTT_AVAILABILITY_ONLINE,
                "payload_not_available": MQTT_AVAILABILITY_OFFLINE,
            },
            {
                "topic": self.state_topic(),
                "value_template": condition_template,
            }
        ])
    }

    fn apply_ml_availability(&self, payload: &mut Value, condition_template: &str) {
        if let Value::Object(map) = payload {
            map.remove("availability_topic");
            map.insert(
                "availability".to_string(),
                self.ml_availability(condition_template),
            );
            map.insert("availability_mode".to_string(), json!("all"));
        }
    }

    fn ml_person_count_payload(&self) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(
                json!({
                    "name": "ML Person Count",
                    "unique_id": "aethersense_ml_person_count",
                    "object_id": "aethersense_ml_person_count",
                    "state_topic": self.state_topic(),
                    "value_template": "{{ value_json.occupancy.count }}",
                    "icon": "mdi:account-group",
                    "state_class": "measurement",
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            );
        }
        self.apply_ml_availability(
            &mut payload,
            "{{ 'online' if value_json.occupancy.available else 'offline' }}",
        );
        payload
    }

    fn ml_count_confidence_payload(&self) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(json!({
                "name": "ML Count Confidence",
                "unique_id": "aethersense_ml_count_confidence",
                "object_id": "aethersense_ml_count_confidence",
                "state_topic": self.state_topic(),
                "value_template": "{{ (value_json.occupancy.confidence | float(0) * 100) | round(1) }}",
                "unit_of_measurement": "%",
                "state_class": "measurement",
                "suggested_display_precision": 1,
            })
            .as_object()
            .cloned()
            .unwrap_or_default());
        }
        self.apply_ml_availability(
            &mut payload,
            "{{ 'online' if value_json.occupancy.available else 'offline' }}",
        );
        payload
    }

    fn ml_method_payload(&self) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(
                json!({
                    "name": "ML Method",
                    "unique_id": "aethersense_ml_method",
                    "object_id": "aethersense_ml_method",
                    "state_topic": self.state_topic(),
                    "value_template": "{{ value_json.occupancy.method }}",
                    "icon": "mdi:brain",
                })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            );
        }
        payload
    }

    fn ml_breathing_payload(&self) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(json!({
                "name": "ML Breathing Rate",
                "unique_id": "aethersense_ml_breathing_rate",
                "object_id": "aethersense_ml_breathing_rate",
                "state_topic": self.state_topic(),
                "value_template": "{{ value_json.occupancy.vitals_ml.breathing_rate | float(0) | round(1) }}",
                "unit_of_measurement": "breaths/min",
                "icon": "mdi:lungs",
                "state_class": "measurement",
                "suggested_display_precision": 1,
            })
            .as_object()
            .cloned()
            .unwrap_or_default());
        }
        self.apply_ml_availability(
            &mut payload,
            "{{ 'online' if value_json.occupancy.vitals_ml.breathing_rate is not none else 'offline' }}",
        );
        payload
    }

    fn ml_heart_payload(&self) -> Value {
        let mut payload = self.shared_payload();
        if let Value::Object(ref mut map) = payload {
            map.extend(json!({
                "name": "ML Heart Rate",
                "unique_id": "aethersense_ml_heart_rate_bpm",
                "object_id": "aethersense_ml_heart_rate_bpm",
                "state_topic": self.state_topic(),
                "value_template": "{{ value_json.occupancy.vitals_ml.heart_rate_bpm | float(0) | round(0) }}",
                "unit_of_measurement": "bpm",
                "icon": "mdi:heart-pulse",
                "state_class": "measurement",
                "suggested_display_precision": 0,
            })
            .as_object()
            .cloned()
            .unwrap_or_default());
        }
        self.apply_ml_availability(
            &mut payload,
            "{{ 'online' if value_json.occupancy.vitals_ml.heart_rate_bpm is not none else 'offline' }}",
        );
        payload
    }

    fn accessory_device_payload(
        &self,
        device_id: &str,
        device_name: Option<&str>,
    ) -> Value {
        json!({
            "identifiers": [format!("aethersense-device-{device_id}")],
            "name": device_name.filter(|name| !name.is_empty()).unwrap_or(device_id),
            "manufacturer": MQTT_MANUFACTURER,
            "model": "ESP32 AetherSense",
            "via_device": "aethersense-server",
        })
    }

    fn accessory_availability(&self, device_id: &str) -> Value {
        json!([
            {
                "topic": self.status_topic(),
                "payload_available": MQTT_AVAILABILITY_ONLINE,
                "payload_not_available": MQTT_AVAILABILITY_OFFLINE,
            },
            {
                "topic": self.device_topic(device_id, "availability"),
                "payload_available": MQTT_AVAILABILITY_ONLINE,
                "payload_not_available": MQTT_AVAILABILITY_OFFLINE,
            }
        ])
    }

    fn relay_discovery_payload(
        &self,
        registry: &AccessoryRegistry,
        accessory: &crate::accessories::Accessory,
    ) -> Value {
        let slug = Self::zone_slug(&format!("{}_{}", accessory.device_id, accessory.id));
        let device_name = registry
            .devices
            .get(&accessory.device_id)
            .map(|device| device.name.as_str());
        let payload_on = json!({
            "device_id": accessory.device_id,
            "action": "relay.set",
            "params": {"relay_id": accessory.id, "state": true},
        })
        .to_string();
        let payload_off = json!({
            "device_id": accessory.device_id,
            "action": "relay.set",
            "params": {"relay_id": accessory.id, "state": false},
        })
        .to_string();
        json!({
            "name": accessory.name,
            "unique_id": format!("aethersense_relay_{slug}"),
            "object_id": format!("aethersense_relay_{slug}"),
            "command_topic": self.server_command_topic(),
            "state_topic": self.device_topic(&accessory.device_id, "state"),
            "value_template": format!(
                "{{{{ 'ON' if value_json.accessories['{}'].state | default(false) else 'OFF' }}}}",
                accessory.id
            ),
            "payload_on": payload_on,
            "payload_off": payload_off,
            "state_on": "ON",
            "state_off": "OFF",
            "optimistic": false,
            "qos": 1,
            "availability": self.accessory_availability(&accessory.device_id),
            "availability_mode": "all",
            "device": self.accessory_device_payload(&accessory.device_id, device_name),
            "origin": self.origin_payload(),
            "icon": "mdi:electric-switch",
        })
    }

    fn ir_button_discovery_payload(
        &self,
        registry: &AccessoryRegistry,
        code: &crate::accessories::IrCode,
    ) -> Value {
        let slug = Self::zone_slug(&format!("{}_{}", code.device_id, code.id));
        let device_name = registry
            .devices
            .get(&code.device_id)
            .map(|device| device.name.as_str());
        json!({
            "name": code.name,
            "unique_id": format!("aethersense_ir_{slug}"),
            "object_id": format!("aethersense_ir_{slug}"),
            "command_topic": self.server_command_topic(),
            "payload_press": json!({
                "device_id": code.device_id,
                "action": "ir.send",
                "params": {"code_id": code.id, "repeats": 1},
            }).to_string(),
            "qos": 1,
            "availability": self.accessory_availability(&code.device_id),
            "availability_mode": "all",
            "device": self.accessory_device_payload(&code.device_id, device_name),
            "origin": self.origin_payload(),
            "icon": "mdi:remote",
        })
    }

    fn accessory_discovery_payloads(
        &self,
        registry: &AccessoryRegistry,
    ) -> Vec<(String, Value)> {
        let mut payloads = Vec::new();
        for accessory in &registry.accessories {
            if accessory.kind != AccessoryKind::Relay {
                continue;
            }
            let slug = Self::zone_slug(&format!("{}_{}", accessory.device_id, accessory.id));
            payloads.push((
                self.discovery_topic("switch", &format!("aethersense_relay_{slug}")),
                self.relay_discovery_payload(registry, accessory),
            ));
        }
        for code in &registry.ir_codes {
            let slug = Self::zone_slug(&format!("{}_{}", code.device_id, code.id));
            payloads.push((
                self.discovery_topic("button", &format!("aethersense_ir_{slug}")),
                self.ir_button_discovery_payload(registry, code),
            ));
        }
        payloads
    }

    pub async fn publish_discovery(
        &self,
        receivers: &[(String, String)],
    ) -> Result<(), rumqttc::ClientError> {
        for (topic, payload) in self.discovery_payloads(receivers) {
            self.publish_json_topic(topic, payload).await?;
        }
        Ok(())
    }

    pub async fn publish_accessory_discovery(
        &self,
        registry: &AccessoryRegistry,
    ) -> Result<(), rumqttc::ClientError> {
        for tombstone in &registry.discovery_tombstones {
            let slug = Self::zone_slug(&format!(
                "{}_{}",
                tombstone.device_id, tombstone.entity_id
            ));
            let (component, object_id) = match tombstone.kind {
                DiscoveryKind::Relay => ("switch", format!("aethersense_relay_{slug}")),
                DiscoveryKind::IrCode => ("button", format!("aethersense_ir_{slug}")),
            };
            self.publish_text_topic(
                self.discovery_topic(component, &object_id),
                "",
                true,
            )
            .await?;
        }
        for (topic, payload) in self.accessory_discovery_payloads(registry) {
            self.publish_json_topic(topic, payload).await?;
        }
        Ok(())
    }

    pub async fn publish_command(
        &self,
        command: &AccessoryCommand,
        boot_id: &str,
    ) -> Result<(), rumqttc::ClientError> {
        let envelope = self.command_envelope(command, boot_id);
        self.publish_text_topic(
            self.device_topic(&command.device_id, "command"),
            &envelope.to_string(),
            false,
        )
        .await
    }

    fn command_envelope(&self, command: &AccessoryCommand, boot_id: &str) -> Value {
        let created = chrono::DateTime::parse_from_rfc3339(&command.created_at).ok();
        let expires = chrono::DateTime::parse_from_rfc3339(&command.expires_at).ok();
        let ttl_ms = created
            .zip(expires)
            .map(|(created, expires)| (expires - created).num_milliseconds().max(0))
            .unwrap_or(15_000);
        let issued_at_ms = chrono::DateTime::parse_from_rfc3339(&command.created_at)
            .map(|created| created.timestamp_millis())
            .unwrap_or_else(|_| chrono::Utc::now().timestamp_millis());
        json!({
            "schema": "aethersense.command.v1",
            "request_id": command.request_id,
            "boot_id": boot_id,
            "issued_at_ms": issued_at_ms,
            "ttl_ms": ttl_ms,
            "action": command.action,
            "params": command.params,
        })
    }

    async fn subscribe_control_topics(&self) -> Result<(), rumqttc::ClientError> {
        for suffix in ["ack", "event", "state", "capabilities", "availability"] {
            self.client
                .subscribe(
                    format!("{}/{}/+/{}", self.base_topic, MQTT_DEVICE_SEGMENT, suffix),
                    QoS::AtLeastOnce,
                )
                .await?;
        }
        self.client
            .subscribe(self.server_command_topic(), QoS::AtLeastOnce)
            .await
    }

    pub async fn publish_status(&self, online: bool) -> Result<(), rumqttc::ClientError> {
        let payload = if online {
            MQTT_AVAILABILITY_ONLINE
        } else {
            MQTT_AVAILABILITY_OFFLINE
        };
        self.publish_text_topic(self.status_topic(), payload, true)
            .await
    }

    pub async fn publish_snapshot(
        &self,
        snapshot: &MqttDetectionSnapshot,
    ) -> Result<(), rumqttc::ClientError> {
        let payload =
            serde_json::to_string(snapshot).expect("MQTT snapshot serialization should not fail");
        self.publish_text_topic(self.state_topic(), &payload, self.retain)
            .await
    }
}

impl From<&InnerState> for MqttDetectionSnapshot {
    fn from(inner: &InnerState) -> Self {
        let latest_packet = inner.latest_packet.as_ref();

        Self {
            count: inner.packet_count,
            rssi: latest_packet.and_then(|packet| packet.rssi),
            rate: latest_packet.and_then(|packet| packet.rate),
            presence: inner.detection.presence.clone(),
            movement: inner.detection.movement.clone(),
            location: inner.detection.location.clone(),
            vitals: inner.detection.vitals.clone(),
            occupancy: inner.detection.occupancy.clone(),
            active_zone: inner.detection.active_zone.clone(),
            shower_box: inner.detection.shower_box.clone(),
            occupancy_confirmed: confirm_occupancy(
                &inner.detection.occupancy,
                &inner.detection.presence,
                &inner.detection.active_zone,
            ),
        }
    }
}

async fn publish_initial_state(publisher: &MqttPublisher, state: &AppState) {
    let snapshot = {
        let inner = state.read().await;
        MqttDetectionSnapshot::from(&*inner)
    };

    if let Err(error) = publisher.publish_snapshot(&snapshot).await {
        warn!("Falha ao publicar estado MQTT inicial: {}", error);
    }

    if let Err(error) = publisher.publish_status(true).await {
        warn!("Falha ao publicar status MQTT online: {}", error);
    }
}

/// Reúne os receptores (role=receiver) para o discovery por zona.
fn collect_receivers(inner: &InnerState) -> Vec<(String, String)> {
    inner
        .devices
        .iter()
        .filter(|d| {
            let role = d.role.trim().to_ascii_lowercase();
            role.is_empty() || role == "receiver"
        })
        .map(|d| (d.id.clone(), d.name.clone()))
        .collect()
}

/// Handle de runtime do serviço MQTT: publisher swappável, status de conexão e
/// sinal de reconfiguração. Todos os campos são `Arc`, então clonar é barato e
/// todas as cópias compartilham o mesmo estado (dashboard, loop de detecção e
/// supervisor).
#[derive(Clone)]
pub struct MqttService {
    handle: Arc<RwLock<Option<MqttPublisher>>>,
    connected: Arc<AtomicBool>,
    reconfigure: Arc<Notify>,
}

impl MqttService {
    /// Publisher atual (clone barato) ou `None` se desabilitado/desconectado.
    pub async fn publisher(&self) -> Option<MqttPublisher> {
        self.handle.read().await.clone()
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Sinaliza o supervisor para reconstruir o cliente com as `MqttSettings`
    /// atuais do estado (após `POST /api/mqtt/settings`).
    pub fn reconfigure(&self) {
        self.reconfigure.notify_one();
    }

    /// Republica o discovery do Home Assistant sob demanda.
    pub async fn publish_discovery_now(&self, state: &AppState) -> bool {
        let Some(publisher) = self.publisher().await else {
            return false;
        };
        let (receivers, registry) = {
            let inner = state.read().await;
            (collect_receivers(&inner), inner.accessories.clone())
        };
        publisher.publish_discovery(&receivers).await.is_ok()
            && publisher
                .publish_accessory_discovery(&registry)
                .await
                .is_ok()
    }

    pub async fn publish_command(
        &self,
        command: &AccessoryCommand,
        boot_id: &str,
    ) -> Result<(), String> {
        if boot_id.trim().is_empty() {
            return Err("boot_id do dispositivo é obrigatório".to_string());
        }
        let Some(publisher) = self.publisher().await else {
            return Err("MQTT desconectado".to_string());
        };
        publisher
            .publish_command(command, boot_id)
            .await
            .map_err(|error| error.to_string())
    }
}

async fn handle_home_assistant_command(
    publisher: &MqttPublisher,
    state: &AppState,
    payload: &[u8],
) {
    let input = match serde_json::from_slice::<CommandInput>(payload) {
        Ok(input) => input,
        Err(error) => {
            warn!("Comando Home Assistant inválido: {}", error);
            return;
        }
    };

    let (command, boot_id, created) = {
        let mut inner = state.write().await;
        let boot_id = match inner.accessories.ready_boot_id(&input.device_id) {
            Ok(boot_id) => boot_id.to_string(),
            Err(reason) => {
                warn!("Comando Home Assistant rejeitado: {}", reason);
                return;
            }
        };
        let (command, created) = match inner.accessories.create_command(input) {
            Ok(result) => result,
            Err(error) => {
                warn!("Comando Home Assistant rejeitado: {}", error);
                return;
            }
        };
        inner.config_dirty = true;
        (command, boot_id, created)
    };

    if !created {
        return;
    }

    if let Err(error) = publisher
        .publish_command(&command, &boot_id)
        .await
    {
        let mut inner = state.write().await;
        inner
            .accessories
            .mark_command_failed(&command.request_id, error.to_string());
        inner.config_dirty = true;
    }
}

async fn mqtt_eventloop_loop(
    mut eventloop: rumqttc::EventLoop,
    publisher: MqttPublisher,
    state: AppState,
    connected: Arc<AtomicBool>,
) {
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                // Uma conexão MQTT nova invalida qualquer presença/boot_id
                // persistido ou observado na sessão anterior. Aguarde a nova
                // capabilities/state antes de aceitar comandos.
                state.write().await.accessories.reset_runtime_presence();
                connected.store(true, Ordering::Relaxed);
                info!("MQTT conectado, publicando discovery Home Assistant");

                if let Err(error) = publisher.subscribe_control_topics().await {
                    warn!("Falha ao assinar tópicos de acessórios: {}", error);
                }

                let (receivers, registry) = {
                    let inner = state.read().await;
                    (collect_receivers(&inner), inner.accessories.clone())
                };
                if let Err(error) = publisher.publish_discovery(&receivers).await {
                    warn!("Falha ao publicar discovery MQTT: {}", error);
                }
                if let Err(error) = publisher.publish_accessory_discovery(&registry).await {
                    warn!("Falha ao publicar discovery de acessórios: {}", error);
                }

                publish_initial_state(&publisher, &state).await;
            }
            Ok(Event::Incoming(Packet::Publish(message))) => {
                if message.topic == publisher.server_command_topic() {
                    handle_home_assistant_command(&publisher, &state, &message.payload).await;
                    continue;
                }
                let persistent = message.topic.ends_with("/ack")
                    || message.topic.ends_with("/event")
                    || message.topic.ends_with("/capabilities");
                let refresh_discovery = message.topic.ends_with("/event")
                    || message.topic.ends_with("/capabilities");
                let registry = {
                    let mut inner = state.write().await;
                    let base_topic = publisher.base_topic.clone();
                    let changed = inner.accessories.apply_mqtt_message(
                        &message.topic,
                        &message.payload,
                        &base_topic,
                    );
                    if changed && persistent {
                        inner.config_dirty = true;
                    }
                    (changed && refresh_discovery).then(|| inner.accessories.clone())
                };
                if let Some(registry) = registry {
                    if let Err(error) = publisher.publish_accessory_discovery(&registry).await {
                        warn!("Falha ao atualizar discovery de acessórios: {}", error);
                    }
                }
            }
            Ok(_) => {}
            Err(error) => {
                connected.store(false, Ordering::Relaxed);
                warn!("MQTT eventloop retornou erro: {}", error);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

/// Constrói um cliente rumqttc a partir das `MqttSettings` atuais.
fn build_publisher(settings: &MqttSettings) -> (MqttPublisher, rumqttc::EventLoop) {
    let mut options = MqttOptions::new(
        settings.client_id.clone(),
        settings.host.clone(),
        settings.port,
    );
    options.set_keep_alive(Duration::from_secs(u64::from(settings.keep_alive_secs)));

    if let Some(username) = settings.username.as_deref() {
        if !username.is_empty() {
            options.set_credentials(username, settings.password.as_deref().unwrap_or(""));
        }
    }

    options.set_last_will(LastWill::new(
        format!("{}/{}", settings.base_topic, MQTT_STATUS_SUFFIX),
        MQTT_AVAILABILITY_OFFLINE,
        QoS::AtLeastOnce,
        true,
    ));

    let (client, eventloop) = AsyncClient::new(options, 32);
    let publisher = MqttPublisher {
        client,
        base_topic: settings.base_topic.clone(),
        discovery_prefix: settings.discovery_prefix.clone(),
        retain: settings.retain,
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    (publisher, eventloop)
}

/// Supervisor: (re)constrói o cliente MQTT conforme as `MqttSettings` do estado
/// e reage a sinais de reconfiguração, abortando o eventloop antigo e criando um
/// novo — permite trocar broker/credenciais/habilitar sem reiniciar o processo.
async fn mqtt_supervisor(service: MqttService, state: AppState) {
    loop {
        let settings = { state.read().await.mqtt.clone() };
        let eventloop_task: Option<tokio::task::JoinHandle<()>> = if settings.enabled {
            let (publisher, eventloop) = build_publisher(&settings);
            {
                *service.handle.write().await = Some(publisher.clone());
            }
            let ev_state = state.clone();
            let ev_connected = service.connected.clone();
            info!(
                "MQTT habilitado: conectando ao broker {}:{}",
                settings.host, settings.port
            );
            Some(tokio::spawn(async move {
                mqtt_eventloop_loop(eventloop, publisher, ev_state, ev_connected).await;
            }))
        } else {
            service.connected.store(false, Ordering::Relaxed);
            *service.handle.write().await = None;
            info!("MQTT desabilitado (configure pelo dashboard para conectar)");
            None
        };

        // Aguarda reconfiguração (POST /api/mqtt/settings dispara notify_one).
        service.reconfigure.notified().await;

        // Encerra o cliente atual antes de reconstruir: publica offline (best
        // effort) e aborta o eventloop; o LastWill cobre a desconexão abrupta.
        service.connected.store(false, Ordering::Relaxed);
        if let Some(publisher) = service.handle.write().await.take() {
            let _ = publisher.publish_status(false).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if let Some(task) = eventloop_task {
            task.abort();
        }
    }
}

/// Inicia o serviço MQTT reconfigurável. Retorna um `MqttService` clonável; o
/// supervisor roda em background e lê as `MqttSettings` do estado a cada
/// (re)configuração.
pub fn start_mqtt_service(state: AppState) -> MqttService {
    let service = MqttService {
        handle: Arc::new(RwLock::new(None)),
        connected: Arc::new(AtomicBool::new(false)),
        reconfigure: Arc::new(Notify::new()),
    };
    let supervisor = service.clone();
    tokio::spawn(async move {
        mqtt_supervisor(supervisor, state).await;
    });
    service
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    fn test_publisher() -> MqttPublisher {
        let (client, _eventloop) =
            AsyncClient::new(MqttOptions::new("test", "localhost", 1883), 32);
        MqttPublisher {
            client,
            base_topic: "aethersense".to_string(),
            discovery_prefix: "homeassistant".to_string(),
            retain: true,
            version: "test".to_string(),
        }
    }

    fn zone(occupied: bool) -> crate::state::ZoneScore {
        crate::state::ZoneScore {
            occupied,
            ..Default::default()
        }
    }

    #[test]
    fn ocupacao_global_so_confirma_quando_a_zona_confirma() {
        // ML diz 1 pessoa (count=1) mas a zona está vazia (sala vazia real,
        // Cenário 2 em 2026-06-15): NÃO deve confirmar ocupação.
        let occ = crate::state::OccupancyEstimate {
            available: true,
            count: 1,
            ..Default::default()
        };
        let pres = crate::state::PresenceState {
            detected: true,
            ..Default::default()
        };
        let az = crate::state::ActiveZoneState {
            zones: vec![zone(false)],
            ..Default::default()
        };
        assert!(!confirm_occupancy(&occ, &pres, &az));
    }

    #[test]
    fn ocupacao_global_confirma_quando_alguma_zona_ocupada() {
        let occ = crate::state::OccupancyEstimate {
            available: true,
            count: 1,
            ..Default::default()
        };
        let pres = crate::state::PresenceState::default();
        let az = crate::state::ActiveZoneState {
            zones: vec![zone(false), zone(true)],
            ..Default::default()
        };
        assert!(confirm_occupancy(&occ, &pres, &az));
    }

    #[test]
    fn sem_zonas_cai_no_fallback_ml_first() {
        // Sem baseline/zonas calibradas: mantém o comportamento anterior.
        let pres = crate::state::PresenceState::default();
        let az = crate::state::ActiveZoneState::default();
        let occ_on = crate::state::OccupancyEstimate {
            available: true,
            count: 2,
            ..Default::default()
        };
        let occ_off = crate::state::OccupancyEstimate {
            available: true,
            count: 0,
            ..Default::default()
        };
        assert!(confirm_occupancy(&occ_on, &pres, &az));
        assert!(!confirm_occupancy(&occ_off, &pres, &az));
    }

    #[test]
    fn sem_zonas_sem_modelo_usa_presenca_heuristica() {
        let az = crate::state::ActiveZoneState::default();
        let occ = crate::state::OccupancyEstimate {
            available: false,
            count: 0,
            ..Default::default()
        };
        let pres_on = crate::state::PresenceState {
            detected: true,
            ..Default::default()
        };
        let pres_off = crate::state::PresenceState::default();
        assert!(confirm_occupancy(&occ, &pres_on, &az));
        assert!(!confirm_occupancy(&occ, &pres_off, &az));
    }

    async fn read_mqtt_packet(stream: &mut TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
        let fixed_header = stream.read_u8().await?;
        let mut multiplier = 1usize;
        let mut remaining_len = 0usize;

        loop {
            let encoded = stream.read_u8().await?;
            remaining_len += usize::from(encoded & 127) * multiplier;
            if encoded & 128 == 0 {
                break;
            }
            multiplier *= 128;
        }

        let mut payload = vec![0; remaining_len];
        stream.read_exact(&mut payload).await?;
        Ok((fixed_header, payload))
    }

    async fn run_mock_broker(listener: TcpListener, topics: mpsc::UnboundedSender<(String, bool)>) {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };

        while let Ok((header, packet)) = read_mqtt_packet(&mut stream).await {
            match header >> 4 {
                1 => {
                    let _ = stream.write_all(&[0x20, 0x02, 0x00, 0x00]).await;
                }
                3 => {
                    if packet.len() < 2 {
                        continue;
                    }
                    let topic_len = u16::from_be_bytes([packet[0], packet[1]]) as usize;
                    if packet.len() < 2 + topic_len {
                        continue;
                    }

                    let topic = String::from_utf8_lossy(&packet[2..2 + topic_len]).to_string();
                    let qos = (header & 0b0000_0110) >> 1;
                    let packet_id_start = 2 + topic_len;

                    if qos > 0 && packet.len() >= packet_id_start + 2 {
                        let packet_id = [packet[packet_id_start], packet[packet_id_start + 1]];
                        let _ = stream
                            .write_all(&[0x40, 0x02, packet_id[0], packet_id[1]])
                            .await;
                    }

                    let retained = header & 0b0000_0001 != 0;
                    let _ = topics.send((topic, retained));
                }
                12 => {
                    let _ = stream.write_all(&[0xD0, 0x00]).await;
                }
                14 => break,
                _ => {}
            }
        }
    }

    fn encode_mqtt_publish(topic: &str, payload: &[u8]) -> Vec<u8> {
        let remaining = 2 + topic.len() + payload.len();
        let mut packet = vec![0x30];
        let mut value = remaining;
        loop {
            let mut encoded = (value % 128) as u8;
            value /= 128;
            if value > 0 {
                encoded |= 0x80;
            }
            packet.push(encoded);
            if value == 0 {
                break;
            }
        }
        packet.extend_from_slice(&(topic.len() as u16).to_be_bytes());
        packet.extend_from_slice(topic.as_bytes());
        packet.extend_from_slice(payload);
        packet
    }

    async fn run_ack_broker(listener: TcpListener) {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let mut ack_sent = false;
        while let Ok((header, packet)) = read_mqtt_packet(&mut stream).await {
            match header >> 4 {
                1 => {
                    let _ = stream.write_all(&[0x20, 0x02, 0x00, 0x00]).await;
                }
                3 => {
                    if packet.len() < 2 {
                        continue;
                    }
                    let topic_len = u16::from_be_bytes([packet[0], packet[1]]) as usize;
                    let packet_id_start = 2 + topic_len;
                    let qos = (header & 0b0000_0110) >> 1;
                    if qos > 0 && packet.len() >= packet_id_start + 2 {
                        let packet_id = [packet[packet_id_start], packet[packet_id_start + 1]];
                        let _ = stream
                            .write_all(&[0x40, 0x02, packet_id[0], packet_id[1]])
                            .await;
                    }
                }
                8 if packet.len() >= 2 => {
                    let packet_id = [packet[0], packet[1]];
                    let _ = stream
                        .write_all(&[0x90, 0x03, packet_id[0], packet_id[1], 0x01])
                        .await;
                    if !ack_sent {
                        ack_sent = true;
                        let capabilities = encode_mqtt_publish(
                            "aethersense/devices/esp-1/capabilities",
                            br#"{"boot_id":"boot-1","capabilities":{},"accessories":[]}"#,
                        );
                        let _ = stream.write_all(&capabilities).await;
                        let packet = encode_mqtt_publish(
                            "aethersense/devices/esp-1/ack",
                            br#"{"request_id":"req-broker","boot_id":"boot-1","status":"ok"}"#,
                        );
                        let _ = stream.write_all(&packet).await;
                    }
                }
                12 => {
                    let _ = stream.write_all(&[0xD0, 0x00]).await;
                }
                14 => break,
                _ => {}
            }
        }
    }

    #[test]
    fn discovery_payloads_preservam_sensores_existentes_e_adicionam_localizacao() {
        let publisher = test_publisher();
        let receivers = vec![("02:00:00:00:00:02".to_string(), "ESP32-WROOM".to_string())];
        let payloads = publisher.discovery_payloads(&receivers);

        let by_unique_id: BTreeMap<String, Value> = payloads
            .iter()
            .map(|(_, payload)| {
                (
                    payload["unique_id"]
                        .as_str()
                        .expect("discovery payload deve ter unique_id")
                        .to_string(),
                    payload.clone(),
                )
            })
            .collect();

        let expected_unique_ids = [
            "aethersense_occupancy",
            "aethersense_zona_box",
            "aethersense_presence_confidence",
            "aethersense_floor",
            "aethersense_accuracy_m",
            "aethersense_location_latitude",
            "aethersense_location_longitude",
            "aethersense_location_confidence",
            "aethersense_location_nodes_used",
            "aethersense_location_method",
            "aethersense_current_room",
            "aethersense_heart_rate_bpm",
            "aethersense_breathing_rate",
            "aethersense_movement",
            "aethersense_rssi_dbm",
            "aethersense_ml_person_count",
            "aethersense_ml_count_confidence",
            "aethersense_ml_method",
            "aethersense_ml_breathing_rate",
            "aethersense_ml_heart_rate_bpm",
            "aethersense_active_zone",
            "aethersense_zona_esp32_wroom",
        ];

        assert_eq!(payloads.len(), expected_unique_ids.len());
        for unique_id in expected_unique_ids {
            assert!(
                by_unique_id.contains_key(unique_id),
                "discovery MQTT sem sensor esperado {unique_id}"
            );
        }
    }

    #[test]
    fn discovery_do_box_publica_estado_e_diagnosticos_sem_remover_disponibilidade() {
        let publisher = test_publisher();
        let payload = publisher
            .discovery_payloads(&[])
            .into_iter()
            .map(|(_, payload)| payload)
            .find(|payload| payload["unique_id"] == "aethersense_zona_box")
            .expect("discovery deve conter a zona fixa do box");

        assert_eq!(payload["object_id"], "aethersense_zona_box");
        assert_eq!(payload["device_class"], "occupancy");
        assert_eq!(
            payload["value_template"],
            "{{ 'ON' if value_json.shower_box.occupied | default(false) else 'OFF' }}"
        );
        assert_eq!(
            payload["json_attributes_template"],
            "{{ value_json.shower_box | tojson }}"
        );
        assert_eq!(payload["availability_topic"], "aethersense/status");
    }

    #[test]
    fn nome_box_de_receptor_nao_sobrescreve_discovery_dedicado() {
        let publisher = test_publisher();
        let payloads = publisher.discovery_payloads(&[("aa:bb".to_string(), "Box".to_string())]);
        let dedicated = payloads
            .iter()
            .filter(|(_, payload)| payload["unique_id"] == "aethersense_zona_box")
            .count();
        assert_eq!(dedicated, 1);

        let dynamic = payloads
            .iter()
            .find(|(_, payload)| payload["unique_id"] == "aethersense_zona_box_aa_bb")
            .expect("zona genérica colidente deve receber sufixo estável do dispositivo");
        assert!(dynamic.0.ends_with("aethersense_zona_box_aa_bb/config"));
        assert_ne!(
            dynamic.1["value_template"],
            "{{ 'ON' if value_json.shower_box.occupied | default(false) else 'OFF' }}"
        );
    }

    #[test]
    fn snapshot_inicial_do_box_falha_fechado() {
        let config_path = std::env::temp_dir().join(format!(
            "aethersense_mqtt_box_missing_{}_{}.json",
            std::process::id(),
            std::thread::current().name().unwrap_or("thread")
        ));
        let inner = InnerState::new_with_config_path(config_path);
        let snapshot = MqttDetectionSnapshot::from(&inner);
        let value = serde_json::to_value(snapshot).unwrap();

        assert_eq!(value["shower_box"]["occupied"], false);
        assert_eq!(value["shower_box"]["ready"], false);
        assert_eq!(value["shower_box"]["tracker_state"], "vacant");
        assert_eq!(value["shower_box"]["attempt_sequence"], 0);
        assert_eq!(value["shower_box"]["last_attempt_result"], "idle");
        assert_eq!(value["shower_box"]["last_attempt_peak_progress"], 0.0);
        assert_eq!(value["shower_box"]["session_sequence"], 0);
        assert!(value["shower_box"]["active_session_id"].is_null());
        assert!(value["shower_box"]["last_session_end_reason"].is_null());
    }

    #[test]
    fn discovery_payloads_de_localizacao_usam_snapshot_location() {
        let publisher = test_publisher();
        let by_unique_id: BTreeMap<String, Value> = publisher
            .discovery_payloads(&[])
            .into_iter()
            .map(|(_, payload)| {
                (
                    payload["unique_id"]
                        .as_str()
                        .expect("discovery payload deve ter unique_id")
                        .to_string(),
                    payload,
                )
            })
            .collect();

        let expected_templates = [
            (
                "aethersense_location_latitude",
                "{{ value_json.location.lat | float(0) | round(8) }}",
            ),
            (
                "aethersense_location_longitude",
                "{{ value_json.location.lng | float(0) | round(8) }}",
            ),
            (
                "aethersense_location_confidence",
                "{{ (value_json.location.confidence | float(0) * 100) | round(1) }}",
            ),
            (
                "aethersense_location_nodes_used",
                "{{ value_json.location.nodes_used | int(0) }}",
            ),
            (
                "aethersense_location_method",
                "{{ value_json.location.method }}",
            ),
            (
                "aethersense_current_room",
                "{{ value_json.location.current_room | default('nenhum', true) }}",
            ),
        ];

        for (unique_id, template) in expected_templates {
            assert_eq!(
                by_unique_id[unique_id]["value_template"].as_str(),
                Some(template),
                "template inesperado para {unique_id}"
            );
        }
    }

    #[tokio::test]
    async fn publish_discovery_entrega_topicos_para_broker_mqtt_mock() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (topics_tx, mut topics_rx) = mpsc::unbounded_channel();
        tokio::spawn(run_mock_broker(listener, topics_tx));

        let mut options = MqttOptions::new("test-discovery", "127.0.0.1", port);
        options.set_keep_alive(Duration::from_secs(5));
        let (client, mut eventloop) = AsyncClient::new(options, 32);
        let publisher = MqttPublisher {
            client,
            base_topic: "aethersense".to_string(),
            discovery_prefix: "homeassistant".to_string(),
            // Discovery deve continuar retido mesmo com snapshots não retidos.
            retain: false,
            version: "test".to_string(),
        };
        let expected_topics: BTreeSet<String> = publisher
            .discovery_payloads(&[])
            .into_iter()
            .map(|(topic, _)| topic)
            .collect();

        let eventloop_task = tokio::spawn(async move {
            loop {
                if eventloop.poll().await.is_err() {
                    break;
                }
            }
        });

        publisher.publish_discovery(&[]).await.unwrap();

        let mut received_topics = BTreeSet::new();
        for _ in 0..expected_topics.len() {
            let (topic, retained) = timeout(Duration::from_secs(5), topics_rx.recv())
                .await
                .expect("broker mock nao recebeu discovery MQTT a tempo")
                .expect("broker mock encerrou antes de receber discovery MQTT");
            assert!(retained, "discovery MQTT precisa usar o bit RETAIN");
            received_topics.insert(topic);
        }

        eventloop_task.abort();
        assert_eq!(received_topics, expected_topics);
    }

    #[test]
    fn discovery_de_acessorios_publica_switch_nao_otimista_e_botao_ir() {
        use crate::accessories::{AccessoryInput, IrCodeInput};

        let publisher = test_publisher();
        let mut registry = AccessoryRegistry::default();
        registry
            .onboard(
                "esp-c5-01".into(),
                "Controle da sala".into(),
                json!({"ir": true}),
                vec![AccessoryInput {
                    id: "relay-1".into(),
                    kind: AccessoryKind::Relay,
                    name: "Lâmpada".into(),
                    capabilities: json!({}),
                }],
                None,
            )
            .unwrap();
        registry
            .create_ir_code(IrCodeInput {
                id: Some("ar-power".into()),
                device_id: "esp-c5-01".into(),
                appliance_id: "ar-sala".into(),
                appliance_name: "Ar da sala".into(),
                name: "Ar Power".into(),
                carrier_hz: 38_000,
                duty_cycle: 0.33,
                sequence_us: vec![9_000, 4_500],
            })
            .unwrap();

        let payloads = publisher.accessory_discovery_payloads(&registry);
        assert_eq!(payloads.len(), 2);
        let switch = payloads
            .iter()
            .find(|(topic, _)| topic.contains("/switch/"))
            .map(|(_, payload)| payload)
            .unwrap();
        assert_eq!(switch["optimistic"], false);
        assert_eq!(
            switch["command_topic"],
            "aethersense/server/command"
        );
        let on: Value = serde_json::from_str(switch["payload_on"].as_str().unwrap()).unwrap();
        assert_eq!(on["action"], "relay.set");
        assert_eq!(on["params"]["relay_id"], "relay-1");
        assert_eq!(on["params"]["state"], true);

        let button = payloads
            .iter()
            .find(|(topic, _)| topic.contains("/button/"))
            .map(|(_, payload)| payload)
            .unwrap();
        let press: Value =
            serde_json::from_str(button["payload_press"].as_str().unwrap()).unwrap();
        assert_eq!(press["action"], "ir.send");
        assert_eq!(press["params"]["code_id"], "ar-power");
    }

    #[tokio::test]
    async fn comando_de_acessorio_usa_envelope_v1_qos1_e_nao_retido() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (topics_tx, mut topics_rx) = mpsc::unbounded_channel();
        tokio::spawn(run_mock_broker(listener, topics_tx));

        let (client, mut eventloop) =
            AsyncClient::new(MqttOptions::new("test-command", "127.0.0.1", port), 32);
        let publisher = MqttPublisher {
            client,
            base_topic: "aethersense".into(),
            discovery_prefix: "homeassistant".into(),
            retain: true,
            version: "test".into(),
        };
        let command = AccessoryCommand {
            request_id: "req-1".into(),
            device_id: "esp-c5-01".into(),
            action: "relay.set".into(),
            params: json!({"relay_id":"relay-1","state":true}),
            status: crate::accessories::CommandStatus::Pending,
            created_at: "2026-07-15T12:00:00Z".into(),
            expires_at: "2026-07-15T12:00:15Z".into(),
            acknowledged_at: None,
            error: None,
        };
        let envelope = publisher.command_envelope(&command, "boot-7");
        assert_eq!(envelope["schema"], "aethersense.command.v1");
        assert_eq!(envelope["boot_id"], "boot-7");
        assert_eq!(
            envelope["issued_at_ms"],
            chrono::DateTime::parse_from_rfc3339(&command.created_at)
                .unwrap()
                .timestamp_millis()
        );
        assert_eq!(envelope["ttl_ms"], 15_000);

        let eventloop_task = tokio::spawn(async move {
            loop {
                if eventloop.poll().await.is_err() {
                    break;
                }
            }
        });
        publisher
            .publish_command(&command, "boot-7")
            .await
            .unwrap();
        let (topic, retained) = timeout(Duration::from_secs(5), topics_rx.recv())
            .await
            .expect("broker não recebeu comando")
            .expect("broker encerrou antes do comando");
        eventloop_task.abort();
        assert_eq!(topic, "aethersense/devices/esp-c5-01/command");
        assert!(!retained, "comandos nunca podem ser retained");
    }

    #[test]
    fn golden_com_boot_nulo_permanece_fixture_de_compatibilidade_do_parser() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../protocol/aethersense-command-v1.golden.json"
        ))
        .unwrap();
        assert_eq!(fixture["schema"], "aethersense.command.v1");
        assert_eq!(fixture["request_id"], "golden-ir-001");
        assert!(fixture["boot_id"].is_null());
        assert_eq!(fixture["action"], "ir.send");
        assert!(fixture["issued_at_ms"].as_i64().is_some());
        assert!(fixture["ttl_ms"].as_u64().is_some());
    }

    #[tokio::test]
    async fn eventloop_bidirecional_processa_ack_do_broker_mock() {
        use crate::accessories::CommandStatus;
        use std::sync::Arc;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(run_ack_broker(listener));

        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(RwLock::new(InnerState::new_with_config_path(
            directory.path().join("config.json"),
        )));
        {
            let mut inner = state.write().await;
            inner
                .accessories
                .onboard(
                    "esp-1".into(),
                    "ESP teste".into(),
                    json!({}),
                    Vec::new(),
                    None,
                )
                .unwrap();
            inner
                .accessories
                .devices
                .get_mut("esp-1")
                .unwrap()
                .boot_id = Some("boot-1".into());
            inner
                .accessories
                .create_command(CommandInput {
                    request_id: Some("req-broker".into()),
                    device_id: "esp-1".into(),
                    action: "ir.learn.cancel".into(),
                    params: json!({}),
                    ttl_ms: Some(30_000),
                })
                .unwrap();
        }

        let settings = MqttSettings {
            enabled: true,
            host: "127.0.0.1".into(),
            port,
            client_id: "ack-integration-test".into(),
            keep_alive_secs: 5,
            ..MqttSettings::default()
        };
        let (publisher, eventloop) = build_publisher(&settings);
        let connected = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(mqtt_eventloop_loop(
            eventloop,
            publisher,
            state.clone(),
            connected,
        ));

        timeout(Duration::from_secs(5), async {
            loop {
                let status = state
                    .write()
                    .await
                    .accessories
                    .command("req-broker")
                    .unwrap()
                    .status;
                if status == CommandStatus::Completed {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("ACK MQTT não atualizou o comando");
        task.abort();
    }
}
