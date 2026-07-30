//! Inferência ONNX (tract) — OccupancyHeatmapNet + VitalsNet.
//!
//! Carrega os modelos exportados pelo pipeline `ml/` e roda inferência sobre
//! o histórico CSI ao vivo, publicando a estimativa ML LADO A LADO com a
//! heurística existente (sem substituí-la). Sem modelos no diretório, tudo
//! degrada graciosamente para a heurística.
//!
//! Contrato dos modelos (ver `models/occupancy_metadata.json` da run):
//!   occupancy.onnx: csi (1,1,64,50) → count_logits (1,6) + heatmap (1,1,20,20)
//!   vitals.onnx:    csi (1,1,64,500) → vitals_hz (1,2) [breathing_hz, heart_hz]

pub mod decode;
pub mod features;

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use serde::Deserialize;
use tracing::{info, warn};
use tract_onnx::prelude::*;

use crate::config::{
    ML_HEATMAP_PERSON_THRESHOLD, ML_OCCUPANCY_MIN_PACKETS, ML_OCCUPANCY_TIMESTEPS,
    ML_TARGET_SUBCARRIERS, ML_VITALS_MIN_PACKETS, ML_VITALS_TIMESTEPS,
    VITALS_AGREEMENT_TOLERANCE_RPM, VITALS_CONF_CEIL, VITALS_CONF_FLOOR,
};
use crate::detection::geometry;
use crate::state::{
    DeviceInfo, DeviceSignalHistory, MapConfig, MlRuntimeStatus, OccupancyEstimate, PersonEstimate,
    VitalsEstimate,
};

type OnnxModel = SimplePlan<TypedFact, Box<dyn TypedOp>, Graph<TypedFact, Box<dyn TypedOp>>>;

#[derive(Debug, Deserialize, Default)]
struct OccupancyMetadata {
    #[serde(default)]
    run_id: String,
    #[serde(default)]
    max_count: usize,
    #[serde(default)]
    heatmap_size: Vec<usize>,
}

pub struct MlEngine {
    occupancy: Option<OnnxModel>,
    vitals: Option<OnnxModel>,
    metadata: OccupancyMetadata,
    models_dir: String,
    load_error: Option<String>,
}

impl MlEngine {
    /// Carrega os modelos do diretório. Falhas viram aviso + fallback, nunca
    /// derrubam o servidor.
    pub fn load(models_dir: &str) -> Self {
        let dir = Path::new(models_dir);
        let mut load_error = None;

        let occupancy = match Self::load_model(
            &dir.join("occupancy.onnx"),
            &[1, 1, ML_TARGET_SUBCARRIERS, ML_OCCUPANCY_TIMESTEPS],
        ) {
            Ok(model) => {
                info!("Modelo occupancy.onnx carregado de {}", models_dir);
                Some(model)
            }
            Err(error) => {
                warn!(
                    "occupancy.onnx indisponível ({}); usando só heurística",
                    error
                );
                load_error = Some(format!("occupancy: {error}"));
                None
            }
        };

        let vitals = match Self::load_model(
            &dir.join("vitals.onnx"),
            &[1, 1, ML_TARGET_SUBCARRIERS, ML_VITALS_TIMESTEPS],
        ) {
            Ok(model) => {
                info!("Modelo vitals.onnx carregado de {}", models_dir);
                Some(model)
            }
            Err(error) => {
                warn!("vitals.onnx indisponível ({}); vitais seguem na FFT", error);
                if load_error.is_none() {
                    load_error = Some(format!("vitals: {error}"));
                }
                None
            }
        };

        let metadata: OccupancyMetadata =
            std::fs::read_to_string(dir.join("occupancy_metadata.json"))
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or_default();

        Self {
            occupancy,
            vitals,
            metadata,
            models_dir: models_dir.to_string(),
            load_error,
        }
    }

    fn load_model(path: &Path, shape: &[usize]) -> TractResult<OnnxModel> {
        tract_onnx::onnx()
            .model_for_path(path)?
            .with_input_fact(0, f32::fact(shape).into())?
            .into_optimized()?
            .into_runnable()
    }

    pub fn status(&self) -> MlRuntimeStatus {
        MlRuntimeStatus {
            occupancy_loaded: self.occupancy.is_some(),
            vitals_loaded: self.vitals.is_some(),
            models_dir: self.models_dir.clone(),
            model_run_id: self.metadata.run_id.clone(),
            last_inference_ms: 0.0,
            last_error: self.load_error.clone(),
        }
    }

    /// Roda a inferência completa sobre o histórico do melhor receptor.
    ///
    /// `fft_vitals` é a estimativa FFT já calculada — usada como fallback de
    /// atribuição por pessoa quando o VitalsNet não está disponível.
    pub fn infer(
        &self,
        devices: &[DeviceInfo],
        histories: &HashMap<String, DeviceSignalHistory>,
        map_config: &MapConfig,
        fft_vitals: &VitalsEstimate,
    ) -> OccupancyEstimate {
        let Some(model) = &self.occupancy else {
            return OccupancyEstimate {
                method: "indisponivel_sem_modelo".to_string(),
                ..Default::default()
            };
        };

        // Receptor preferido: ESP32-C5 (contrato do projeto: 1× C5); senão o
        // histórico com mais pacotes.
        let Some((device_id, history)) = features::pick_history(devices, histories) else {
            return OccupancyEstimate {
                method: "indisponivel_sem_historico".to_string(),
                model_run_id: self.metadata.run_id.clone(),
                ..Default::default()
            };
        };

        let amp: Vec<Vec<f64>> = history.amplitude_history.iter().cloned().collect();
        if amp.len() < ML_OCCUPANCY_MIN_PACKETS {
            return OccupancyEstimate {
                method: format!(
                    "aguardando_historico_{}_de_{}",
                    amp.len(),
                    ML_OCCUPANCY_MIN_PACKETS
                ),
                model_run_id: self.metadata.run_id.clone(),
                ..Default::default()
            };
        }

        let started = Instant::now();
        let window = features::build_window(&amp, ML_TARGET_SUBCARRIERS, ML_OCCUPANCY_TIMESTEPS);

        let (probabilities, heatmap, heat_h, heat_w) = match self.run_occupancy(model, &window) {
            Ok(result) => result,
            Err(error) => {
                warn!("Inferência occupancy falhou: {}", error);
                return OccupancyEstimate {
                    method: "erro_inferencia".to_string(),
                    model_run_id: self.metadata.run_id.clone(),
                    ..Default::default()
                };
            }
        };

        let count = probabilities
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i)
            .unwrap_or(0);
        let confidence = probabilities.get(count).copied().unwrap_or(0.0);

        // Vitais ML (janela longa) — opcional. A confiança é derivada da
        // concordância com a FFT (sinal real), não mais fixa.
        let vitals_ml = self.infer_vitals(&amp, fft_vitals);

        // Pessoas: picos do heatmap → lat/lng via floorplan/dispositivos.
        let bounds = decode::resolve_bounds(map_config, devices);
        let mut peaks = decode::heatmap_to_persons(
            &heatmap,
            heat_h,
            heat_w,
            ML_HEATMAP_PERSON_THRESHOLD,
            self.metadata.max_count.max(5),
        );
        if peaks.is_empty() && count >= 1 {
            peaks = decode::heatmap_to_persons(&heatmap, heat_h, heat_w, 0.01, count);
            if peaks.is_empty() && !heatmap.is_empty() && heat_h > 0 && heat_w > 0 {
                let mut best_idx = 0;
                let mut best_val = f64::NEG_INFINITY;
                for (idx, &val) in heatmap.iter().enumerate() {
                    if val > best_val {
                        best_val = val;
                        best_idx = idx;
                    }
                }
                let y = best_idx / heat_w;
                let x = best_idx % heat_w;
                peaks.push(decode::PersonPeak {
                    x: x as f64 / (heat_w.max(2) - 1) as f64,
                    y: y as f64 / (heat_h.max(2) - 1) as f64,
                    confidence: confidence.max(0.5),
                });
            }
        }

        // Atribuição honesta de vitais: individual somente com 1 pessoa.
        let scope = if count <= 1 { "individual" } else { "global" };
        let (vit_b, vit_h, vit_source) = if vitals_ml.breathing_rate.is_some() {
            (
                vitals_ml.breathing_rate,
                vitals_ml.heart_rate_bpm,
                "vitals_net_onnx",
            )
        } else {
            (
                fft_vitals.breathing_rate,
                fft_vitals.heart_rate_bpm,
                "fft_heuristica",
            )
        };

        let floor = devices
            .first()
            .map(|d| d.floor)
            .or_else(|| map_config.floorplans.first().map(|f| f.floor))
            .unwrap_or(3);

        let persons: Vec<PersonEstimate> = peaks
            .into_iter()
            .map(|peak| {
                let (lat, lng) = bounds
                    .as_ref()
                    .map(|b| {
                        let (raw_lat, raw_lng) = decode::norm_to_latlng(peak.x, peak.y, b);
                        geometry::clamp_to_rooms_or_floorplans(
                            (raw_lat, raw_lng),
                            floor,
                            &map_config.environment_objects,
                            &map_config.floorplans,
                        )
                        .map(|(clamped, _)| clamped)
                        .unwrap_or((raw_lat, raw_lng))
                    })
                    .map(|(lat, lng)| (Some(lat), Some(lng)))
                    .unwrap_or((None, None));
                PersonEstimate {
                    x_norm: peak.x,
                    y_norm: peak.y,
                    lat,
                    lng,
                    confidence: peak.confidence,
                    breathing_rate: vit_b,
                    heart_rate_bpm: vit_h,
                    vitals_scope: scope.to_string(),
                    vitals_source: vit_source.to_string(),
                }
            })
            .collect();

        OccupancyEstimate {
            available: true,
            count,
            probabilities,
            confidence,
            // Zero-shot WiMANS: a contagem ainda NÃO é calibrada para a sala
            // (sem modelo-da-sala/few-shot). Mantemos `false` por enquanto.
            count_calibrated: false,
            method: format!("occupancy_heatmap_onnx[{}]", device_id),
            persons,
            vitals_ml,
            model_run_id: self.metadata.run_id.clone(),
            inference_ms: started.elapsed().as_secs_f64() * 1000.0,
        }
    }

    fn run_occupancy(
        &self,
        model: &OnnxModel,
        window: &[f32],
    ) -> TractResult<(Vec<f64>, Vec<f64>, usize, usize)> {
        let input = tract_ndarray::Array4::from_shape_vec(
            (1, 1, ML_TARGET_SUBCARRIERS, ML_OCCUPANCY_TIMESTEPS),
            window.to_vec(),
        )
        .map_err(|e| TractError::msg(format!("shape de entrada inválido: {e}")))?;
        let outputs = model.run(tvec!(Tensor::from(input).into()))?;

        let logits = outputs[0].to_array_view::<f32>()?;
        let logits: Vec<f64> = logits.iter().map(|v| *v as f64).collect();
        let probabilities = softmax(&logits);

        let heat_view = outputs[1].to_array_view::<f32>()?;
        let shape = heat_view.shape().to_vec();
        let (heat_h, heat_w) = match shape.as_slice() {
            [_, _, h, w] => (*h, *w),
            _ => {
                let fallback = self.metadata.heatmap_size.clone();
                (
                    fallback.first().copied().unwrap_or(20),
                    fallback.get(1).copied().unwrap_or(20),
                )
            }
        };
        let heatmap: Vec<f64> = heat_view.iter().map(|v| *v as f64).collect();
        Ok((probabilities, heatmap, heat_h, heat_w))
    }

    fn infer_vitals(&self, amp: &[Vec<f64>], fft_vitals: &VitalsEstimate) -> VitalsEstimate {
        let Some(model) = &self.vitals else {
            return VitalsEstimate::default();
        };
        if amp.len() < ML_VITALS_MIN_PACKETS {
            return VitalsEstimate::default();
        }
        let window = features::build_window(amp, ML_TARGET_SUBCARRIERS, ML_VITALS_TIMESTEPS);
        let input = match tract_ndarray::Array4::from_shape_vec(
            (1, 1, ML_TARGET_SUBCARRIERS, ML_VITALS_TIMESTEPS),
            window,
        ) {
            Ok(a) => a,
            Err(_) => return VitalsEstimate::default(),
        };
        match model.run(tvec!(Tensor::from(input).into())) {
            Ok(outputs) => match outputs[0].to_array_view::<f32>() {
                Ok(view) => {
                    let values: Vec<f64> = view.iter().map(|v| *v as f64).collect();
                    let breathing_hz = values.first().copied();
                    let heart_hz = values.get(1).copied();
                    let breathing_rate = breathing_hz.map(|hz| hz * 60.0);
                    VitalsEstimate {
                        breathing_rate,
                        heart_rate_bpm: heart_hz.map(|hz| hz * 60.0),
                        // Confiança DERIVADA da concordância com a FFT (sinal
                        // real), em vez de fixa: o VitalsNet sempre emite um valor
                        // na faixa fisiológica, então o número só ganha crédito
                        // quando a estimativa independente da FFT concorda.
                        confidence: vitals_agreement_confidence(breathing_rate, fft_vitals),
                    }
                }
                Err(_) => VitalsEstimate::default(),
            },
            Err(error) => {
                warn!("Inferência vitals falhou: {}", error);
                VitalsEstimate::default()
            }
        }
    }
}

/// Confiança do VitalsNet derivada da concordância com a FFT (sinal real).
///
/// `conf = clamp(1 − |fft − onnx| / tolerância, piso, teto) × conf_fft`, onde
/// `conf_fft` é a confiança da própria FFT (SNR dos picos). Sem estimativa FFT
/// crível naquele tick (taxa ausente ou confiança ~0), cai para um default
/// conservador (`VITALS_CONF_FLOOR`) — não há com que concordar, então não se
/// afirma certeza. Usa a taxa RESPIRATÓRIA por ser a banda mais limpa e a única
/// que a FFT reporta de forma robusta.
fn vitals_agreement_confidence(
    onnx_breathing_rpm: Option<f64>,
    fft_vitals: &VitalsEstimate,
) -> f64 {
    match (onnx_breathing_rpm, fft_vitals.breathing_rate) {
        (Some(onnx), Some(fft)) if fft_vitals.confidence > 0.0 => {
            let disagreement = (fft - onnx).abs() / VITALS_AGREEMENT_TOLERANCE_RPM;
            let agreement = (1.0 - disagreement).clamp(VITALS_CONF_FLOOR, VITALS_CONF_CEIL);
            // Pondera pela confiança da FFT: concordar com uma FFT incerta vale
            // menos. O piso garante que nunca zera enquanto há respiração ONNX.
            (agreement * fft_vitals.confidence).clamp(VITALS_CONF_FLOOR, VITALS_CONF_CEIL)
        }
        // FFT indisponível/sem evidência neste tick: default conservador.
        _ => VITALS_CONF_FLOOR,
    }
}

fn softmax(logits: &[f64]) -> Vec<f64> {
    let max = logits.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let exps: Vec<f64> = logits.iter().map(|v| (v - max).exp()).collect();
    let sum: f64 = exps.iter().sum();
    if sum > 0.0 {
        exps.iter().map(|v| v / sum).collect()
    } else {
        vec![0.0; logits.len()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_history(packets: usize, subcarriers: usize) -> DeviceSignalHistory {
        let mut history = DeviceSignalHistory::new();
        for t in 0..packets {
            let amplitudes: Vec<f64> = (0..subcarriers)
                .map(|s| 10.0 + (t as f64 * 0.3 + s as f64 * 0.1).sin() * 2.0)
                .collect();
            history.amplitude_history.push_back(amplitudes);
            history.packet_count += 1;
        }
        history
    }

    fn c5_device() -> DeviceInfo {
        DeviceInfo {
            id: "02:00:00:00:00:01".to_string(),
            name: "C5 teste".to_string(),
            lat: -1.45,
            lng: -48.49,
            floor: 0,
            icon: String::new(),
            size: 32,
            role: "receiver".to_string(),
            hardware_profile: "esp32c5".to_string(),
        }
    }

    #[test]
    fn confianca_vitals_deriva_da_concordancia_com_a_fft() {
        // FFT crível e concordante (mesma taxa) → confiança alta, ponderada pela
        // confiança da FFT; teto respeitado.
        let fft_forte = VitalsEstimate {
            breathing_rate: Some(15.0),
            heart_rate_bpm: None,
            confidence: 0.9,
        };
        let conf_concorda = vitals_agreement_confidence(Some(15.0), &fft_forte);
        assert!(
            conf_concorda > 0.5,
            "concordância deve elevar a confiança: {conf_concorda}"
        );
        assert!(conf_concorda <= VITALS_CONF_CEIL + 1e-9);

        // Discrepância grande (> tolerância) → cai ao piso.
        let conf_discorda = vitals_agreement_confidence(Some(40.0), &fft_forte);
        assert!(
            (conf_discorda - VITALS_CONF_FLOOR).abs() < 1e-9,
            "discrepância → piso: {conf_discorda}"
        );
        // E é estritamente menor que quando concorda.
        assert!(conf_discorda < conf_concorda);

        // FFT ausente/sem evidência → default conservador (piso), nunca 0.5 fixo.
        let conf_sem_fft = vitals_agreement_confidence(Some(15.0), &VitalsEstimate::default());
        assert!((conf_sem_fft - VITALS_CONF_FLOOR).abs() < 1e-9);

        // Toda saída fica dentro de [piso, teto].
        for c in [conf_concorda, conf_discorda, conf_sem_fft] {
            assert!((VITALS_CONF_FLOOR..=VITALS_CONF_CEIL).contains(&c));
        }
    }

    #[test]
    fn occupancy_count_nao_calibrado_por_padrao_e_serializa() {
        // Default e serialização do flag de honestidade do zero-shot WiMANS.
        let est = OccupancyEstimate::default();
        assert!(!est.count_calibrated, "contagem deve nascer NÃO-calibrada");
        let json = serde_json::to_value(&est).expect("serializa");
        assert_eq!(json["count_calibrated"], serde_json::json!(false));
    }

    #[test]
    fn sem_modelos_degrada_para_heuristica_sem_panico() {
        let engine = MlEngine::load("diretorio_que_nao_existe");
        let status = engine.status();
        assert!(!status.occupancy_loaded);
        assert!(!status.vitals_loaded);

        let occupancy = engine.infer(
            &[c5_device()],
            &HashMap::new(),
            &MapConfig::default(),
            &VitalsEstimate::default(),
        );
        assert!(!occupancy.available);
        assert_eq!(occupancy.method, "indisponivel_sem_modelo");
        assert!(occupancy.persons.is_empty());
    }

    #[test]
    fn com_modelos_reais_roda_inferencia_completa() {
        // Roda apenas quando os modelos exportados existem em models/ —
        // garante a paridade contrato ONNX ↔ servidor antes do deploy.
        if !std::path::Path::new("models/occupancy.onnx").exists() {
            eprintln!("models/occupancy.onnx ausente — teste pulado");
            return;
        }
        let engine = MlEngine::load("models");
        assert!(
            engine.status().occupancy_loaded,
            "occupancy.onnx deve carregar"
        );

        let device = c5_device();
        let mut histories = HashMap::new();
        histories.insert(device.id.clone(), synthetic_history(300, 56));

        let occupancy = engine.infer(
            &[device],
            &histories,
            &MapConfig::default(),
            &VitalsEstimate::default(),
        );
        assert!(occupancy.available, "method = {}", occupancy.method);
        assert_eq!(occupancy.probabilities.len(), 6, "6 classes de contagem");
        let prob_sum: f64 = occupancy.probabilities.iter().sum();
        assert!((prob_sum - 1.0).abs() < 1e-6, "softmax deve somar 1");
        assert!(occupancy.inference_ms > 0.0);
        // Pessoas têm lat/lng porque o fallback de bounds usa o receptor.
        for person in &occupancy.persons {
            assert!(person.lat.is_some() && person.lng.is_some());
            assert!(!person.vitals_scope.is_empty());
        }
        // Vitais ML exigem 250+ pacotes — com 300 deve rodar se o modelo carregou.
        if engine.status().vitals_loaded {
            assert!(occupancy.vitals_ml.breathing_rate.is_some());
            let rpm = occupancy.vitals_ml.breathing_rate.unwrap();
            assert!(
                (3.0..=42.0).contains(&rpm),
                "respiração fora da faixa: {rpm}"
            );
        }
    }
}
