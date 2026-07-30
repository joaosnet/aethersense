/// AetherSense Server — Biblioteca pública para testes de integração.
///
/// Este módulo re-exporta todos os módulos internos para que os testes
/// em `tests/` consigam acessar as structs e funções.
pub mod config;
pub mod accessories;
pub mod csi;
pub mod detection;
pub mod ml;
pub mod processing;
pub mod replay;
pub mod state;
pub mod mqtt;
pub mod udp;
pub mod api;
