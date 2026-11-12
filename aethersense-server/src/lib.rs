#![allow(
    clippy::result_large_err,
    clippy::manual_clamp,
    clippy::unnecessary_unwrap,
    clippy::implicit_saturating_sub,
    clippy::manual_is_multiple_of,
    clippy::empty_line_after_doc_comments,
    clippy::cloned_ref_to_slice_refs,
    clippy::needless_range_loop,
    clippy::derivable_impls,
    clippy::new_without_default,
    clippy::field_reassign_with_default,
    clippy::manual_range_contains,
    clippy::approx_constant
)]

/// AetherSense Server — Biblioteca pública para testes de integração.
///
/// Este módulo re-exporta todos os módulos internos para que os testes
/// em `tests/` consigam acessar as structs e funções.
pub mod config;
pub mod accessories;
pub mod broker_discovery;
pub mod csi;
pub mod detection;
pub mod ml;
pub mod processing;
pub mod replay;
pub mod state;
pub mod mqtt;
pub mod udp;
pub mod api;
