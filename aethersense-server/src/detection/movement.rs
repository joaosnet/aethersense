/// Detecção de movimento por taxa de variação da amplitude CSI.
///
/// Analisa a diferença entre amplitudes consecutivas para classificar
/// o nível de atividade no ambiente.
use crate::config::MOVEMENT_DIFF_THRESHOLD;
use crate::state::{MovementLevel, MovementState};

/// Analisa o histórico de amplitudes para determinar movimento.
///
/// # Algoritmo
/// 1. Calcula a diferença absoluta média entre pacotes consecutivos
/// 2. Classifica o nível de movimento com base em thresholds:
///    - Static: diff < threshold
///    - SlightMovement: diff < 2 * threshold
///    - Walking: diff < 5 * threshold
///    - Running: diff >= 5 * threshold
pub fn detect_movement(amplitude_history: &[Vec<f64>]) -> MovementState {
    if amplitude_history.len() < 2 {
        return MovementState {
            level: MovementLevel::Static,
            velocity_estimate: 0.0,
        };
    }

    // Calcular diferenças absolutas médias entre pacotes consecutivos
    let mut diffs: Vec<f64> = Vec::new();

    for window in amplitude_history.windows(2) {
        let prev = &window[0];
        let curr = &window[1];
        let len = prev.len().min(curr.len());
        if len == 0 {
            continue;
        }

        let diff: f64 = prev
            .iter()
            .zip(curr.iter())
            .map(|(&a, &b)| (a - b).abs())
            .sum::<f64>()
            / len as f64;

        diffs.push(diff);
    }

    if diffs.is_empty() {
        return MovementState {
            level: MovementLevel::Static,
            velocity_estimate: 0.0,
        };
    }

    // Usar as últimas 10 diferenças para suavizar
    let recent_count = diffs.len().min(10);
    let recent = &diffs[diffs.len() - recent_count..];
    let avg_diff: f64 = recent.iter().sum::<f64>() / recent.len() as f64;

    let level = if avg_diff < MOVEMENT_DIFF_THRESHOLD {
        MovementLevel::Static
    } else if avg_diff < MOVEMENT_DIFF_THRESHOLD * 2.0 {
        MovementLevel::SlightMovement
    } else if avg_diff < MOVEMENT_DIFF_THRESHOLD * 5.0 {
        MovementLevel::Walking
    } else {
        MovementLevel::Running
    };

    // Estimativa de velocidade simplificada (m/s, heurística)
    let velocity_estimate = avg_diff * 0.1;

    MovementState {
        level,
        velocity_estimate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sem_historico_estatico() {
        let result = detect_movement(&[]);
        assert_eq!(result.level, MovementLevel::Static);
        assert_eq!(result.velocity_estimate, 0.0);
    }

    #[test]
    fn test_sinal_constante_estatico() {
        let history: Vec<Vec<f64>> = (0..20).map(|_| vec![10.0; 64]).collect();
        let result = detect_movement(&history);
        assert_eq!(result.level, MovementLevel::Static);
    }

    #[test]
    fn test_sinal_variavel_detecta_movimento() {
        let history: Vec<Vec<f64>> = (0..20)
            .map(|i| vec![if i % 2 == 0 { 100.0 } else { 0.0 }; 64])
            .collect();
        let result = detect_movement(&history);
        assert_ne!(result.level, MovementLevel::Static);
        assert!(result.velocity_estimate > 0.0);
    }

    #[test]
    fn test_classificacao_niveis() {
        // Diferenças pequenas → slight movement
        let history_slight: Vec<Vec<f64>> =
            (0..20).map(|i| vec![10.0 + (i as f64) * 0.3; 64]).collect();
        let result = detect_movement(&history_slight);
        // Deve ser Static ou SlightMovement (depende do threshold)
        assert!(
            result.level == MovementLevel::Static || result.level == MovementLevel::SlightMovement
        );
    }
}
