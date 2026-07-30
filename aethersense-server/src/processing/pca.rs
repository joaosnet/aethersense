/// Análise de Componentes Principais (PCA) das amplitudes CSI.
///
/// Usa SVD (Singular Value Decomposition) para extrair as duas
/// componentes principais, úteis para visualizar padrões de
/// variação no CSI ao longo do tempo.
use crate::processing::amplitude::build_heatmap_matrix;

/// Resultado do cálculo PCA.
pub struct PcaResult {
    /// Índices dos pacotes
    pub packet_axis: Vec<usize>,
    /// Scores da primeira componente principal
    pub pc1: Vec<f64>,
    /// Scores da segunda componente principal
    pub pc2: Vec<f64>,
}

/// Calcula PCA via SVD rudimentar (power iteration para 2 componentes).
///
/// Para uma implementação simples sem dependência de LAPACK, usamos
/// o método de iteração de potência para encontrar os dois maiores
/// valores/vetores singulares.
pub fn compute_pca(history: &[Vec<f64>], window_size: usize) -> PcaResult {
    let empty = PcaResult {
        packet_axis: Vec::new(),
        pc1: Vec::new(),
        pc2: Vec::new(),
    };

    if history.is_empty() {
        return empty;
    }

    let window = window_size.max(2);
    let start = if history.len() > window {
        history.len() - window
    } else {
        0
    };
    let selected = &history[start..];

    let matrix = build_heatmap_matrix(selected);
    if matrix.is_empty() {
        return empty;
    }

    let n_rows = matrix.len();
    let n_cols = matrix[0].len();

    if n_rows < 2 || n_cols < 2 {
        return empty;
    }

    // Centralizar os dados (subtrair média de cada coluna)
    let mut data: Vec<Vec<f64>> = matrix.clone();
    let mut col_means = vec![0.0; n_cols];
    for row in &data {
        for (j, &val) in row.iter().enumerate() {
            col_means[j] += val;
        }
    }
    for mean in &mut col_means {
        *mean /= n_rows as f64;
    }
    for row in &mut data {
        for (j, val) in row.iter_mut().enumerate() {
            *val -= col_means[j];
        }
    }

    // Calcular covariância: C = (1/n) * Dᵀ * D
    // Para PCA, precisamos dos autovetores de C
    // Usamos power iteration para os 2 maiores

    let pc1_vec = power_iteration(&data, n_cols, 100);
    let pc1_scores = project(&data, &pc1_vec);

    // Para PC2: deflacionar a matriz (remover componente de PC1)
    let mut deflated = data.clone();
    for (i, row) in deflated.iter_mut().enumerate() {
        for (j, val) in row.iter_mut().enumerate() {
            *val -= pc1_scores[i] * pc1_vec[j];
        }
    }

    let pc2_vec = power_iteration(&deflated, n_cols, 100);
    let pc2_scores = project(&deflated, &pc2_vec);

    let packet_axis: Vec<usize> = (0..n_rows).collect();

    PcaResult {
        packet_axis,
        pc1: pc1_scores,
        pc2: pc2_scores,
    }
}

/// Power iteration para encontrar o autovetor dominante de DᵀD.
fn power_iteration(data: &[Vec<f64>], n_cols: usize, max_iter: usize) -> Vec<f64> {
    let mut v = vec![1.0 / (n_cols as f64).sqrt(); n_cols];

    for _ in 0..max_iter {
        // w = Dᵀ * D * v
        // Primeiro: u = D * v
        let u: Vec<f64> = data
            .iter()
            .map(|row| row.iter().zip(v.iter()).map(|(&a, &b)| a * b).sum())
            .collect();

        // Depois: w = Dᵀ * u
        let mut w = vec![0.0; n_cols];
        for (row, &ui) in data.iter().zip(u.iter()) {
            for (j, &val) in row.iter().enumerate() {
                w[j] += val * ui;
            }
        }

        // Normalizar
        let norm: f64 = w.iter().map(|x| x * x).sum::<f64>().sqrt();
        if norm < 1e-15 {
            break;
        }
        for val in &mut w {
            *val /= norm;
        }
        v = w;
    }

    v
}

/// Projeta os dados no vetor dado, retornando os scores.
fn project(data: &[Vec<f64>], vec: &[f64]) -> Vec<f64> {
    data.iter()
        .map(|row| row.iter().zip(vec.iter()).map(|(&a, &b)| a * b).sum())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pca_vazio() {
        let result = compute_pca(&[], 10);
        assert!(result.packet_axis.is_empty());
    }

    #[test]
    fn test_pca_retorna_duas_componentes() {
        let history: Vec<Vec<f64>> = (0..20)
            .map(|i| {
                (0..10)
                    .map(|j| (i as f64 * 0.1 + j as f64 * 0.3).sin())
                    .collect()
            })
            .collect();
        let result = compute_pca(&history, 20);
        assert_eq!(result.pc1.len(), 20);
        assert_eq!(result.pc2.len(), 20);
        assert_eq!(result.packet_axis.len(), 20);
    }

    #[test]
    fn test_pca_pc1_tem_mais_variancia() {
        let history: Vec<Vec<f64>> = (0..30)
            .map(|i| {
                (0..5)
                    .map(|j| (i as f64 * 0.2 + j as f64 * 0.5).sin() + i as f64 * 0.01)
                    .collect()
            })
            .collect();
        let result = compute_pca(&history, 30);
        let var1: f64 = result.pc1.iter().map(|x| x * x).sum::<f64>() / result.pc1.len() as f64;
        let var2: f64 = result.pc2.iter().map(|x| x * x).sum::<f64>() / result.pc2.len() as f64;
        // PC1 deve ter variância >= PC2
        assert!(var1 >= var2 - 1e-6);
    }
}
