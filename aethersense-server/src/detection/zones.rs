//! Máquina de estados de ocupação por zona (cômodo) — ADAPTATIVA.
//!
//! Converte os scores de perturbação por receptor em decisões binárias
//! debounced NO SERVIDOR (loop de 2 Hz), para que as automações do Home
//! Assistant disparem imediatamente sobre um sinal já limpo.
//!
//! Os limiares são APRENDIDOS por zona: cada receptor tem seu próprio piso
//! de ruído (percentil 20 dos últimos ~2 min), que depende da posição, da
//! distância ao roteador e do ambiente. Medições reais (2026-06-12)
//! mostraram pisos muito diferentes entre o C5 (quarto) e o WROOM
//! (banheiro) — limiares fixos calibrados num cômodo falham no outro.
//!
//!   limiar de ENTRADA = 3.0 × piso   (clamp 0.10–0.40)
//!   limiar de SAÍDA   = 1.8 × piso   (clamp 0.05–0.25)
//!
//!   ENTRAR : score > entrada em >= 2 dos últimos 3 ticks (~1,5 s)
//!   SEGURAR: perturbação OU respiração crível (a retenção exige FFT com
//!            confiança real E concordância do ML — ver main.rs)
//!   SAIR   : MEDIANA dos últimos 20 ticks < saída (~10 s; mediana é
//!            robusta aos picos de ruído que a regra "todos quietos"
//!            tornava a saída quase impossível em piso ruidoso)

use std::collections::{HashMap, VecDeque};

use crate::state::ZoneScore;

/// Fallbacks usados até a zona acumular amostras para aprender o piso.
pub const ZONE_ON_THRESHOLD: f64 = 0.12;
pub const ZONE_OFF_THRESHOLD: f64 = 0.06;

const ON_FLOOR_MULTIPLIER: f64 = 3.0;
const OFF_FLOOR_MULTIPLIER: f64 = 1.8;
const ON_CLAMP: (f64, f64) = (0.10, 0.40);
const OFF_CLAMP: (f64, f64) = (0.05, 0.25);
const FLOOR_PERCENTILE: f64 = 0.20;
const MIN_SAMPLES_FOR_ADAPTIVE: usize = 40;

const ON_WINDOW: usize = 3;
const ON_VOTES: usize = 2;
const OFF_WINDOW: usize = 20;
/// ~2 minutos de história a 2 Hz para aprender o piso de ruído.
const HISTORY_CAP: usize = 240;

#[derive(Debug, Default)]
pub struct ZoneTracker {
    history: HashMap<String, VecDeque<f64>>,
    occupied: HashMap<String, bool>,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

impl ZoneTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Piso de ruído (percentil) da janela de uma zona, ou `None` se ainda não
    /// houver amostras suficientes para uma estimativa estável.
    fn floor_of(history: &VecDeque<f64>) -> Option<f64> {
        if history.len() < MIN_SAMPLES_FOR_ADAPTIVE {
            return None;
        }
        let mut sorted: Vec<f64> = history.iter().copied().collect();
        sorted.sort_by(|a, b| a.total_cmp(b));
        Some(percentile(&sorted, FLOOR_PERCENTILE))
    }

    /// Limiares adaptativos da zona a partir do piso de ruído aprendido.
    fn thresholds(history: &VecDeque<f64>) -> (f64, f64) {
        let Some(floor) = Self::floor_of(history) else {
            return (ZONE_ON_THRESHOLD, ZONE_OFF_THRESHOLD);
        };
        let on = (floor * ON_FLOOR_MULTIPLIER).clamp(ON_CLAMP.0, ON_CLAMP.1);
        let off = (floor * OFF_FLOOR_MULTIPLIER).clamp(OFF_CLAMP.0, OFF_CLAMP.1);
        // Garante histerese mesmo após os clamps.
        (on.max(off + 0.02), off)
    }

    /// Semeia o tracker com o piso de ruído persistido por zona (carregado do
    /// config no boot). Pré-popula a janela de cada zona com o valor do piso,
    /// de modo que os limiares adaptativos já valem desde o 1º tick — eliminando
    /// os ~2 min de janela cega/ruidosa após cada restart do container. As
    /// amostras semeadas saem naturalmente da janela conforme dados reais chegam.
    pub fn seed_floors(&mut self, floors: &HashMap<String, f64>) {
        for (device_id, &floor) in floors {
            if !floor.is_finite() || floor < 0.0 {
                continue;
            }
            let history = self
                .history
                .entry(device_id.clone())
                .or_insert_with(|| VecDeque::with_capacity(HISTORY_CAP));
            // Enche a janela com o piso: como ele é o percentil baixo, repeti-lo
            // faz `floor_of` reproduzir exatamente o piso aprendido.
            history.clear();
            for _ in 0..MIN_SAMPLES_FOR_ADAPTIVE {
                history.push_back(floor);
            }
        }
    }

    /// Exporta o piso de ruído atualmente aprendido por zona, para persistir no
    /// config. Só inclui zonas com amostras suficientes (piso estável).
    pub fn learned_floors(&self) -> HashMap<String, f64> {
        self.history
            .iter()
            .filter_map(|(id, history)| Self::floor_of(history).map(|f| (id.clone(), f)))
            .collect()
    }

    /// Atualiza o rastreador com os scores do tick e marca `occupied` em
    /// cada zona. `breathing_hold` impede a desocupação da zona ATIVA.
    pub fn update(&mut self, zones: &mut [ZoneScore], breathing_hold: bool) {
        let active_id = zones
            .iter()
            .max_by(|a, b| a.score.total_cmp(&b.score))
            .map(|z| z.device_id.clone());

        for zone in zones.iter_mut() {
            let history = self
                .history
                .entry(zone.device_id.clone())
                .or_insert_with(|| VecDeque::with_capacity(HISTORY_CAP));
            if history.len() >= HISTORY_CAP {
                history.pop_front();
            }
            history.push_back(zone.score);

            let (on_threshold, off_threshold) = Self::thresholds(history);
            zone.on_threshold = on_threshold;
            zone.off_threshold = off_threshold;

            let entry = self.occupied.entry(zone.device_id.clone()).or_insert(false);

            if !*entry {
                // ENTRAR: votação rápida sobre os últimos ON_WINDOW ticks.
                let recent: Vec<f64> = history.iter().rev().take(ON_WINDOW).copied().collect();
                let votes = recent.iter().filter(|s| **s > on_threshold).count();
                if recent.len() >= ON_VOTES && votes >= ON_VOTES {
                    *entry = true;
                }
            } else {
                // SAIR: mediana da janela abaixo do limiar (robusta a picos)
                // e sem respiração segurando a zona ativa.
                let is_active = active_id.as_deref() == Some(zone.device_id.as_str());
                let held_by_breathing = breathing_hold && is_active;
                let mut recent: Vec<f64> = history.iter().rev().take(OFF_WINDOW).copied().collect();
                recent.sort_by(|a, b| a.total_cmp(b));
                let median = percentile(&recent, 0.5);
                let quiet = recent.len() >= OFF_WINDOW && median < off_threshold;
                if quiet && !held_by_breathing {
                    *entry = false;
                }
            }

            zone.occupied = *entry;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(id: &str, score: f64) -> ZoneScore {
        ZoneScore {
            device_id: id.to_string(),
            device_name: id.to_string(),
            score,
            has_baseline: true,
            occupied: false,
            on_threshold: 0.0,
            off_threshold: 0.0,
        }
    }

    fn tick(tracker: &mut ZoneTracker, score: f64, breathing: bool) -> ZoneScore {
        let mut zones = vec![zone("a", score)];
        tracker.update(&mut zones, breathing);
        zones.remove(0)
    }

    #[test]
    fn entra_em_dois_ticks_acima_do_limiar() {
        let mut tracker = ZoneTracker::new();
        assert!(!tick(&mut tracker, 0.5, false).occupied, "1 tick não ocupa");
        assert!(tick(&mut tracker, 0.5, false).occupied, "2 ticks ocupam");
    }

    #[test]
    fn pico_isolado_nao_ocupa() {
        let mut tracker = ZoneTracker::new();
        for score in [0.5, 0.02, 0.02, 0.5, 0.02] {
            assert!(!tick(&mut tracker, score, false).occupied);
        }
    }

    #[test]
    fn sai_pela_mediana_mesmo_com_picos_de_ruido() {
        let mut tracker = ZoneTracker::new();
        for _ in 0..3 {
            tick(&mut tracker, 0.6, false);
        }
        // Janela de saída com ~30% de picos (como o banheiro real): a regra
        // antiga ("todos quietos") nunca sairia; a mediana sai.
        let mut last = zone("a", 0.0);
        for i in 0..25 {
            let score = if i % 3 == 0 { 0.09 } else { 0.02 };
            last = tick(&mut tracker, score, false);
        }
        assert!(!last.occupied, "mediana quieta deve desocupar");
    }

    #[test]
    fn respiracao_segura_zona_ativa() {
        let mut tracker = ZoneTracker::new();
        for _ in 0..3 {
            tick(&mut tracker, 0.6, true);
        }
        for _ in 0..40 {
            assert!(tick(&mut tracker, 0.02, true).occupied);
        }
        let mut last = zone("a", 0.0);
        for _ in 0..21 {
            last = tick(&mut tracker, 0.02, false);
        }
        assert!(!last.occupied);
    }

    #[test]
    fn piso_semeado_aplica_limiares_adaptativos_no_primeiro_tick() {
        // Sem semente, o 1º tick usa o fallback fixo. Com a semente do piso
        // persistido, os limiares adaptativos já valem desde o 1º tick (sem os
        // ~2 min de janela cega após o restart).
        let mut seeded: HashMap<String, f64> = HashMap::new();
        seeded.insert("a".to_string(), 0.05);

        let mut tracker = ZoneTracker::new();
        tracker.seed_floors(&seeded);
        let first = tick(&mut tracker, 0.05, false);
        assert!(
            first.on_threshold > ZONE_ON_THRESHOLD,
            "piso semeado deve elevar o limiar de entrada no 1º tick: {}",
            first.on_threshold
        );

        // E o piso aprendido exportado bate, aproximadamente, com o semeado.
        let learned = tracker.learned_floors();
        let floor = learned
            .get("a")
            .copied()
            .expect("zona semeada deve exportar piso");
        assert!((floor - 0.05).abs() < 0.02, "piso exportado = {floor}");
    }

    #[test]
    fn learned_floors_ignora_zonas_sem_amostras_suficientes() {
        // Poucas amostras → piso ainda instável → não exporta (não persiste lixo).
        let mut tracker = ZoneTracker::new();
        for _ in 0..(MIN_SAMPLES_FOR_ADAPTIVE - 1) {
            tick(&mut tracker, 0.05, false);
        }
        assert!(tracker.learned_floors().is_empty());
    }

    #[test]
    fn limiares_se_adaptam_ao_piso_de_ruido() {
        let mut tracker = ZoneTracker::new();
        // Piso ruidoso (~0.05): após 40+ amostras, limiar de entrada deve
        // subir acima do fallback (0.12) para 3×piso ≈ 0.15.
        let mut last = zone("a", 0.0);
        for _ in 0..60 {
            last = tick(&mut tracker, 0.05, false);
        }
        assert!(last.on_threshold > ZONE_ON_THRESHOLD);
        assert!(last.off_threshold > ZONE_OFF_THRESHOLD);
        assert!(
            last.on_threshold > last.off_threshold,
            "histerese preservada"
        );
    }
}
