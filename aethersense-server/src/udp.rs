/// Listener UDP assíncrono para receber dados CSI do ESP32.
///
/// O ESP32 faz broadcast dos pacotes CSI via UDP na porta configurada.
/// Esta task escuta continuamente, parseia cada linha e atualiza o estado.
use tokio::net::UdpSocket;
use tracing::{info, warn};

use crate::csi::parser::parse_csi_packet;
use crate::state::AppState;

/// Inicia o loop de escuta UDP.
///
/// Deve ser spawnado como uma tarefa Tokio independente.
/// Roda até que `state.running` seja setado como `false`.
pub async fn udp_listener_loop(state: AppState, port: u16) {
    let bind_addr = std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
        std::net::Ipv4Addr::UNSPECIFIED,
        port,
    ));
    let socket = match bind_udp_socket(bind_addr) {
        Ok(s) => {
            info!("[*] Escutando UDP na porta {}...", port);
            s
        }
        Err(e) => {
            warn!("Falha ao fazer bind UDP em {}: {}", bind_addr, e);
            return;
        }
    };

    let mut buf = [0u8; 8192];
    let mut datagrams_received: u64 = 0;
    let mut parsed_packets: u64 = 0;
    let mut parse_failures: u64 = 0;

    loop {
        // Verifica se deve continuar rodando
        {
            let inner = state.read().await;
            if !inner.running {
                break;
            }
        }

        // `timeout` garante que a flag `running` seja reavaliada periodicamente
        // mesmo sem tráfego, permitindo encerramento limpo (sem ficar preso em
        // `recv_from`). Em caso de timeout, apenas reitera o loop.
        let recv = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            socket.recv_from(&mut buf),
        )
        .await;

        let recv = match recv {
            Ok(result) => result,
            Err(_) => continue, // timeout: reavalia `running` no topo do loop
        };

        match recv {
            Ok((len, addr)) => {
                datagrams_received += 1;
                let line = match std::str::from_utf8(&buf[..len]) {
                    Ok(s) => s.trim().to_string(),
                    Err(_) => {
                        parse_failures += 1;
                        if parse_failures <= 3 || parse_failures % 50 == 0 {
                            warn!(
                                "Datagrama UDP CSI invalido de {}: payload nao UTF-8 ({} bytes, falhas={})",
                                addr, len, parse_failures
                            );
                        }
                        continue;
                    }
                };

                if line.is_empty() {
                    continue;
                }

                if let Some(packet) = parse_csi_packet(&line) {
                    parsed_packets += 1;
                    if parsed_packets <= 3 || parsed_packets % 100 == 0 {
                        info!(
                            "Pacote CSI UDP #{} recebido de {} (datagramas={}, mac={}, rssi={:?}, subportadoras={})",
                            parsed_packets,
                            addr,
                            datagrams_received,
                            packet.mac,
                            packet.rssi,
                            packet.amplitudes.len()
                        );
                    }
                    let mut inner = state.write().await;
                    inner.ingest_udp_packet(packet);
                } else {
                    parse_failures += 1;
                    if parse_failures <= 3 || parse_failures % 50 == 0 {
                        let preview: String = line.chars().take(96).collect();
                        warn!(
                            "Datagrama UDP de {} nao reconhecido como CSI_DATA ({} bytes, falhas={}): {}",
                            addr, len, parse_failures, preview
                        );
                    }
                }
            }
            Err(e) => {
                warn!("Erro UDP: {}", e);
                // Backoff curto: evita busy-loop quente em erros persistentes
                // de socket (ex.: WSAECONNRESET no Windows após ICMP
                // port-unreachable em UDP).
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }

    info!("UDP listener encerrado.");
}

fn bind_udp_socket(bind_addr: std::net::SocketAddr) -> std::io::Result<UdpSocket> {
    let std_socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None)?;
    std_socket.set_reuse_address(true)?;
    #[cfg(not(windows))]
    std_socket.set_reuse_port(true)?;
    std_socket.bind(&bind_addr.into())?;
    std_socket.set_nonblocking(true)?;
    UdpSocket::from_std(std_socket.into())
}
