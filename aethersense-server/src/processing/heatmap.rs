/// Funções auxiliares de heatmap (re-exporta de amplitude).
///
/// Este módulo existe para manter a separação conceitual;
/// as funções reais estão em `amplitude.rs`.
#[allow(unused_imports)]
pub use super::amplitude::{build_heatmap_matrix, compute_windowed_heatmap};
