/// Detecção de presença por análise de variância do CSI.
///
/// Quando uma pessoa está presente no ambiente, a variância das
/// amplitudes CSI aumenta significativamente em comparação com
/// um ambiente vazio (devido às reflexões do corpo humano).
use crate::config::{
    PRESENCE_BASELINE_MULT, PRESENCE_VARIANCE_FLOOR, PRESENCE_VARIANCE_THRESHOLD,
    PRESENCE_WINDOW_SIZE,
};
use crate::state::PresenceState;

/// Analisa o histórico de amplitudes (de UM dispositivo) para determinar presença.
///
/// # Algoritmo
/// 1. Pega as últimas `PRESENCE_WINDOW_SIZE` amostras de amplitude média
/// 2. Calcula a variância dessa série temporal
/// 3. O limiar é RELATIVO ao baseline de sala vazia daquele nó, quando disponível
///    (`baseline_variance × PRESENCE_BASELINE_MULT`, com piso `PRESENCE_VARIANCE_FLOOR`);
///    sem baseline, cai para o limiar absoluto `PRESENCE_VARIANCE_THRESHOLD`.
/// 4. Se variância > limiar → presença; a confiança cresce com a razão variância/limiar.
pub fn detect_presence(signal_history: &[f64], baseline_variance: Option<f64>) -> PresenceState {
    if signal_history.len() < 3 {
        return PresenceState {
            detected: false,
            confidence: 0.0,
            ..Default::default()
        };
    }

    let window = PRESENCE_WINDOW_SIZE.min(signal_history.len());
    let start = signal_history.len() - window;
    let recent = &signal_history[start..];

    let mean: f64 = recent.iter().sum::<f64>() / recent.len() as f64;
    let variance: f64 =
        recent.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / recent.len() as f64;

    // Limiar relativo ao baseline de sala vazia do nó (auto-adapta ao piso de ruído
    // de cada link); cai para o absoluto quando não há baseline calibrado.
    let threshold = match baseline_variance {
        Some(bv) if bv.is_finite() && bv > 0.0 => {
            (bv * PRESENCE_BASELINE_MULT).max(PRESENCE_VARIANCE_FLOOR)
        }
        _ => PRESENCE_VARIANCE_THRESHOLD,
    };

    let detected = variance > threshold;

    // Confiança: sigmoid suave baseado na razão variância/threshold
    let ratio = variance / threshold;
    let confidence = if detected {
        (1.0 - 1.0 / (1.0 + ratio)).min(1.0)
    } else {
        (ratio * 0.3).min(0.3)
    };

    let status = if detected {
        "ACTIVE".to_string()
    } else {
        "EMPTY".to_string()
    };

    PresenceState {
        detected,
        confidence,
        status,
        hold_seconds_remaining: 0.0,
    }
}

/// Rastreador temporal de presença com Hold-Time Adaptativo (Schmitt Trigger / Histerese).
///
/// Mantém a presença detectada por um período configurável (`hold_duration_secs`)
/// após a última perturbação de movimento, estendendo-a automaticamente caso haja
/// indicação de repouso/respiração viável (`breathing_hold`).
#[derive(Debug, Clone)]
pub struct PresenceTracker {
    last_detected_instant: Option<std::time::Instant>,
    pub hold_duration_secs: f64,
}

impl Default for PresenceTracker {
    fn default() -> Self {
        Self::new(crate::config::PRESENCE_HOLD_SECONDS)
    }
}

impl PresenceTracker {
    pub fn new(hold_duration_secs: f64) -> Self {
        Self {
            last_detected_instant: None,
            hold_duration_secs,
        }
    }

    pub fn has_active_hold(&self) -> bool {
        match self.last_detected_instant {
            Some(last_seen) => last_seen.elapsed().as_secs_f64() < self.hold_duration_secs,
            None => false,
        }
    }

    /// Processa a detecção instantânea com retenção adaptativa (hold-time + respiração).
    pub fn update(&mut self, instantaneous: PresenceState, breathing_hold: bool) -> PresenceState {
        if instantaneous.detected {
            self.last_detected_instant = Some(std::time::Instant::now());
            PresenceState {
                detected: true,
                confidence: instantaneous.confidence,
                status: "ACTIVE".to_string(),
                hold_seconds_remaining: self.hold_duration_secs,
            }
        } else if let Some(last_seen) = self.last_detected_instant {
            let elapsed = last_seen.elapsed().as_secs_f64();
            if breathing_hold {
                self.last_detected_instant = Some(std::time::Instant::now());
                PresenceState {
                    detected: true,
                    confidence: 0.65_f64.max(instantaneous.confidence),
                    status: "HOLDING_STATIC".to_string(),
                    hold_seconds_remaining: self.hold_duration_secs,
                }
            } else if elapsed < self.hold_duration_secs {
                let remaining = (self.hold_duration_secs - elapsed).max(0.0);
                PresenceState {
                    detected: true,
                    confidence: 0.45_f64.max(instantaneous.confidence),
                    status: "HOLDING_STATIC".to_string(),
                    hold_seconds_remaining: remaining,
                }
            } else {
                self.last_detected_instant = None;
                PresenceState {
                    detected: false,
                    confidence: instantaneous.confidence,
                    status: "EMPTY".to_string(),
                    hold_seconds_remaining: 0.0,
                }
            }
        } else {
            PresenceState {
                detected: false,
                confidence: instantaneous.confidence,
                status: "EMPTY".to_string(),
                hold_seconds_remaining: 0.0,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_historico_vazio_sem_presenca() {
        let result = detect_presence(&[], None);
        assert!(!result.detected);
        assert_eq!(result.confidence, 0.0);
    }

    #[test]
    fn test_sinal_constante_sem_presenca() {
        // Sinal constante = variância zero = sem presença
        let signal: Vec<f64> = vec![10.0; 50];
        let result = detect_presence(&signal, None);
        assert!(!result.detected);
    }

    #[test]
    fn test_sinal_variavel_com_presenca() {
        // Sinal com alta variação = presença detectada
        let signal: Vec<f64> = (0..50)
            .map(|i| if i % 2 == 0 { 100.0 } else { 0.0 })
            .collect();
        let result = detect_presence(&signal, None);
        assert!(result.detected);
        assert!(result.confidence > 0.5);
    }

    #[test]
    fn test_confianca_aumenta_com_variancia() {
        // Mais variação → mais confiança
        let signal_low: Vec<f64> = (0..50).map(|i| 10.0 + (i % 5) as f64 * 2.0).collect();
        let signal_high: Vec<f64> = (0..50)
            .map(|i| if i % 2 == 0 { 200.0 } else { 0.0 })
            .collect();

        let result_low = detect_presence(&signal_low, None);
        let result_high = detect_presence(&signal_high, None);

        if result_low.detected && result_high.detected {
            assert!(result_high.confidence >= result_low.confidence);
        }
    }

    #[test]
    fn test_presenca_relativa_ao_baseline() {
        // Mesma série (variância = 9): com baseline alto (sala ruidosa) NÃO detecta;
        // com baseline baixo (sala limpa) detecta. É a essência do limiar relativo.
        let signal: Vec<f64> = (0..50)
            .map(|i| if i % 2 == 0 { 13.0 } else { 7.0 })
            .collect();
        let ruidoso = detect_presence(&signal, Some(10.0)); // limiar = max(30, 4) = 30 > 9
        let limpo = detect_presence(&signal, Some(0.5)); // limiar = max(1.5, 4) = 4 < 9
        assert!(
            !ruidoso.detected,
            "baseline alto não deveria detectar var=9"
        );
        assert!(limpo.detected, "baseline limpo deveria detectar var=9");
    }

    #[test]
    fn test_presence_tracker_hold_and_breathing() {
        let mut tracker = PresenceTracker::new(20.0);

        // 1. Início vazio
        let empty = PresenceState::default();
        let s0 = tracker.update(empty.clone(), false);
        assert!(!s0.detected);
        assert_eq!(s0.status, "EMPTY");

        // 2. Detecção instantânea ativa
        let active = PresenceState {
            detected: true,
            confidence: 0.8,
            ..Default::default()
        };
        let s1 = tracker.update(active, false);
        assert!(s1.detected);
        assert_eq!(s1.status, "ACTIVE");

        // 3. Fica imóvel (detecção instantânea false), entra em HOLDING_STATIC
        let s2 = tracker.update(empty.clone(), false);
        assert!(s2.detected);
        assert_eq!(s2.status, "HOLDING_STATIC");

        // 4. Se detectar respiração, sustenta o hold indefinidamente com confiança adaptada
        let s3 = tracker.update(empty.clone(), true);
        assert!(s3.detected);
        assert_eq!(s3.status, "HOLDING_STATIC");
        assert!(s3.confidence >= 0.65);
    }
}
