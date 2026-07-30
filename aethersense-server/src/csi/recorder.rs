#![allow(dead_code)]
use crate::csi::packet::CsiPacket;
use crate::csi::parser::parse_csi_packet;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use tokio::net::UdpSocket;
use tokio::time::{timeout, Duration};

/// Captures live packets from the ESP32 via UDP.
pub async fn capture_live(port: u16, duration_secs: u64, min_packets: usize) -> Vec<CsiPacket> {
    // We create a standard socket to set reuseaddr
    let std_socket =
        socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None).unwrap();

    std_socket.set_reuse_address(true).unwrap();
    #[cfg(not(windows))]
    std_socket.set_reuse_port(true).unwrap();

    let sockaddr = std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
        std::net::Ipv4Addr::UNSPECIFIED,
        port,
    ));
    if let Err(e) = std_socket.bind(&sockaddr.into()) {
        eprintln!(
            "Erro: bind UDP na porta {} falhou ({}). Captura abortada (feche o servidor que ja usa a porta).",
            port, e
        );
        return Vec::new();
    }

    std_socket.set_nonblocking(true).unwrap();
    let socket = UdpSocket::from_std(std_socket.into()).expect("Failed to create tokio UdpSocket");

    let mut buf = [0u8; 8192];
    let mut packets = Vec::new();

    let start = std::time::Instant::now();
    // Temporarily increase duration to compensate for the slow 1 packet/sec ESP32 current firmware
    let max_duration = Duration::from_secs(duration_secs.max(50));

    while start.elapsed() < max_duration && packets.len() < min_packets {
        let receive_future = socket.recv_from(&mut buf);
        match timeout(Duration::from_millis(500), receive_future).await {
            Ok(Ok((len, _addr))) => {
                let msg = String::from_utf8_lossy(&buf[..len]);
                for line in msg.lines() {
                    let line = line.trim();
                    if line.starts_with("CSI_DATA") {
                        if let Some(packet) = parse_csi_packet(line) {
                            packets.push(packet);
                            if packets.len() >= min_packets {
                                break;
                            }
                        }
                    }
                }
            }
            Ok(Err(_)) => {
                // Read error
            }
            Err(_) => {
                // Timeout, just continue and check loop condition
            }
        }
    }

    packets
}

/// Saves raw captured packets to a CSV file.
pub fn save_capture(packets: &[CsiPacket], path: &Path) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap_or_default();
    }
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .expect("Failed to open capture file for writing");

    for pkt in packets {
        writeln!(file, "{}", pkt.raw_line).expect("Failed to write packet");
    }
}

/// Replays a saved packet capture from a CSV file.
pub fn replay_from_file(path: &Path) -> Vec<CsiPacket> {
    let file = std::fs::File::open(path).expect("Failed to open capture file for replay");
    let reader = BufReader::new(file);
    let mut packets = Vec::new();

    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let line = line.trim();
        if line.starts_with("CSI_DATA") {
            if let Some(packet) = parse_csi_packet(line) {
                packets.push(packet);
            }
        }
    }

    packets
}
