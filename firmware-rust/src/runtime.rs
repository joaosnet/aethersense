use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::csi::CsiFrame;
use crate::storage::{RawCsiRecordingGate, StorageIngress, StorageRecord};
use crate::transport::UdpTransmitter;

const UDP_ERROR_LOG_EVERY: usize = 10;
const MAX_UDP_ERROR_BACKOFF_MILLIS: u64 = 1000;
// The C5 has one core and much less free internal heap after Wi-Fi. Sixteen
// frames still cover well over one second at the configured 10 Hz output,
// while avoiding several KiB of resident metadata and burst payloads.
pub const ESP32C5_MAX_PACKET_QUEUE_CAPACITY: usize = 16;
pub const DEFAULT_OUTBOUND_QUEUE_CAPACITY: usize = 64;
pub const ESP32C5_MAX_OUTBOUND_QUEUE_CAPACITY: usize = 16;

/// Relógio monotônico em segundos desde o boot, compartilhado entre a thread do
/// consumidor UDP (heartbeat) e o monitor de saúde. No alvo ESP-IDF usa
/// `esp_timer_get_time()` (us desde o boot); no host (testes) usa o relógio do
/// sistema. Ambas as threads usam a MESMA fonte para comparar tempos.
///
/// Retorna segundos (não ms) porque o heartbeat é guardado num `AtomicU32` — os
/// alvos ESP (xtensa/riscv32) não têm `AtomicU64`. Segundos cabem em u32 por
/// ~136 anos (sem wraparound prático), e os limiares de saúde são em segundos.
pub fn monotonic_secs() -> u32 {
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    unsafe {
        ((esp_idf_svc::sys::esp_timer_get_time() as u64) / 1_000_000) as u32
    }
    #[cfg(not(all(feature = "esp-idf", target_os = "espidf")))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0)
    }
}

/// Fila de mão-única (callback da task wifi -> thread UDP) protegida por Mutex.
/// O PRODUTOR usa `try_lock` e NUNCA bloqueia nem gira: se o consumidor estiver
/// com o lock, descarta o frame. Isso é essencial porque o callback de CSI roda
/// na task wifi (prioridade alta) e a ~200 Hz em 5 GHz — um spin aqui (como no
/// `mpsc::sync_channel` da std, que usa `Backoff::spin_light`) trava o C5 unicore
/// por inversão de prioridade (a task wifi gira esperando o consumidor de baixa
/// prioridade, que nunca roda no único núcleo). Visto no watchdog: task `wifi`
/// presa em `wifi_csi_rx_cb` -> `Backoff::spin_light`.
struct PacketBus {
    queue: Mutex<VecDeque<CsiFrame>>,
    capacity: usize,
}

#[derive(Clone)]
pub struct PacketIngress {
    bus: Arc<PacketBus>,
    dropped: Arc<AtomicUsize>,
}

pub struct PacketEgress {
    bus: Arc<PacketBus>,
}

/// Late-bound storage destination for the UDP consumer. This lets the network
/// task reserve its stack before optional SD initialization allocates DMA/FAT
/// resources, without losing raw-CSI recording when the card is available.
#[derive(Clone, Default)]
pub struct StorageIngressLink {
    destination: Arc<Mutex<Option<(StorageIngress, RawCsiRecordingGate)>>>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RuntimeStats {
    pub accepted: usize,
    pub transmitted: usize,
    pub dropped: usize,
}

pub fn effective_packet_queue_capacity(requested: usize, hardware_profile: &str) -> usize {
    let requested = requested.max(1);
    if hardware_profile.eq_ignore_ascii_case("esp32c5") {
        requested.min(ESP32C5_MAX_PACKET_QUEUE_CAPACITY)
    } else {
        requested
    }
}

pub fn effective_outbound_queue_capacity(
    requested: usize,
    hardware_profile: &str,
    mqtt_enabled: bool,
) -> usize {
    let requested = requested.max(1);
    if !mqtt_enabled {
        1
    } else if hardware_profile.eq_ignore_ascii_case("esp32c5") {
        requested.min(ESP32C5_MAX_OUTBOUND_QUEUE_CAPACITY)
    } else {
        requested
    }
}

pub fn packet_bus_for_hardware(
    requested: usize,
    hardware_profile: &str,
) -> (PacketIngress, PacketEgress) {
    let capacity = effective_packet_queue_capacity(requested, hardware_profile);
    if capacity != requested.max(1) {
        log::warn!(
            "CSI packet queue capped for {}: requested={} effective={}",
            hardware_profile,
            requested,
            capacity
        );
    }
    packet_bus(capacity)
}

pub fn packet_bus(capacity: usize) -> (PacketIngress, PacketEgress) {
    let capacity = capacity.max(1);
    let bus = Arc::new(PacketBus {
        queue: Mutex::new(VecDeque::with_capacity(capacity)),
        capacity,
    });
    let dropped = Arc::new(AtomicUsize::new(0));

    (
        PacketIngress {
            bus: bus.clone(),
            dropped,
        },
        PacketEgress { bus },
    )
}

impl PacketIngress {
    /// Enfileira um frame de forma WAIT-FREE: nunca bloqueia nem gira. Se a fila
    /// estiver cheia OU o consumidor estiver com o lock, descarta (incrementa o
    /// contador). Seguro para chamar do callback de CSI na task wifi.
    pub fn try_push(&self, frame: CsiFrame) -> bool {
        if let Ok(mut queue) = self.bus.queue.try_lock() {
            if queue.len() < self.bus.capacity {
                queue.push_back(frame);
                return true;
            }
        }
        self.dropped.fetch_add(1, Ordering::Relaxed);
        false
    }

    pub fn dropped_count(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn dropped_counter(&self) -> Arc<AtomicUsize> {
        self.dropped.clone()
    }

    pub fn capacity(&self) -> usize {
        self.bus.capacity
    }
}

impl StorageIngressLink {
    pub fn attach(&self, destination: Option<(StorageIngress, RawCsiRecordingGate)>) {
        *self
            .destination
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = destination;
    }

    pub fn is_attached(&self) -> bool {
        self.destination
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    fn try_push_csi(&self, frame: &CsiFrame) -> bool {
        let ingress = self
            .destination
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .and_then(|(ingress, gate)| gate.is_active().then(|| ingress.clone()));
        let Some(ingress) = ingress else {
            return false;
        };
        ingress
            .try_push(StorageRecord::CsiCsv(frame.serialize_csv()))
            .is_ok()
    }
}

fn pace_next_send(next_send: &mut std::time::Instant, interval: std::time::Duration) {
    *next_send += interval;
    let now = std::time::Instant::now();
    if *next_send > now {
        std::thread::sleep(*next_send - now);
    } else {
        *next_send = now;
    }
}

pub fn spawn_udp_consumer(
    egress: PacketEgress,
    transmitter: UdpTransmitter,
    dropped: Arc<AtomicUsize>,
    send_interval_millis: u64,
    last_send_secs: Arc<AtomicU32>,
    transmitted_counter: Arc<AtomicUsize>,
    storage: StorageIngressLink,
) -> std::io::Result<JoinHandle<RuntimeStats>> {
    thread::Builder::new()
        .name("aether-udp".to_string())
        .stack_size(8 * 1024)
        .spawn(move || {
        let mut accepted = 0usize;
        let mut transmitted = 0usize;
        let mut consecutive_send_errors = 0usize;
        let send_interval = std::time::Duration::from_millis(send_interval_millis.max(1));
        let error_backoff = std::time::Duration::from_millis(
            send_interval_millis
                .max(1)
                .saturating_mul(2)
                .min(MAX_UDP_ERROR_BACKOFF_MILLIS),
        );
        let mut next_send = std::time::Instant::now();
        let mut wire_line = String::new();

        loop {
            // Lock BLOQUEANTE no consumidor é seguro: o produtor usa try_lock e
            // só segura o lock por um push (µs), então isto praticamente não trava
            // — e nunca causa inversão de prioridade (o produtor jamais espera).
            let frame = {
                let mut queue = egress
                    .bus
                    .queue
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                queue.pop_front()
            };

            let Some(mut frame) = frame else {
                // Fila vazia: encerra se não há mais produtores (todos os
                // PacketIngress dropados); senão aguarda o próximo frame.
                if Arc::strong_count(&egress.bus) <= 1 {
                    break;
                }
                thread::sleep(send_interval);
                continue;
            };

            accepted += 1;
            frame.seq = transmitted as u32;
            // This runs in the consumer task, never in the high-priority CSI
            // callback. The SD ingress itself is bounded and non-blocking.
            let _ = storage.try_push_csi(&frame);
            let serialized = frame.serialize_csv_into(&mut wire_line);
            // The UDP stack needs its own PBUF and Wi-Fi TX buffer. Release the
            // sample Vec before asking lwIP to allocate those copies.
            drop(frame);
            let send_result = match serialized {
                Ok(()) => transmitter.send_line(&wire_line),
                Err(_) => Err(std::io::Error::from_raw_os_error(12)),
            };
            match send_result {
                Ok(_) => {
                    consecutive_send_errors = 0;
                    transmitted += 1;
                    transmitted_counter.store(transmitted, Ordering::Relaxed);
                    // `.max(1)`: 0 é o sentinel "nenhum envio ainda" no monitor de
                    // saúde; durante o 1o segundo de boot monotonic_secs() é 0, então
                    // sem o max um envio real gravaria o sentinel por engano.
                    last_send_secs.store(monotonic_secs().max(1), Ordering::Relaxed);
                    if transmitted % 100 == 0 {
                        log::warn!("Transmitted {} frames", transmitted);
                    }
                    pace_next_send(&mut next_send, send_interval);
                }
                Err(err) => {
                    consecutive_send_errors += 1;
                    if consecutive_send_errors == 1
                        || consecutive_send_errors % UDP_ERROR_LOG_EVERY == 0
                    {
                        log::warn!(
                            "Failed to send frame ({} consecutive errors, accepted={}, transmitted={}): {}",
                            consecutive_send_errors, accepted, transmitted, err
                        );
                    }
                    pace_next_send(&mut next_send, error_backoff);
                }
            }
        }

        RuntimeStats {
            accepted,
            transmitted,
            dropped: dropped.load(Ordering::Relaxed),
        }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csi::sample_frame;

    #[test]
    fn queue_drops_when_full() {
        let (ingress, _egress) = packet_bus(1);

        assert!(ingress.try_push(sample_frame(0)));
        assert!(!ingress.try_push(sample_frame(1)));
        assert_eq!(ingress.dropped_count(), 1);
    }

    #[test]
    fn esp32c5_queue_is_capped_before_allocation() {
        assert_eq!(
            effective_packet_queue_capacity(256, "esp32c5"),
            ESP32C5_MAX_PACKET_QUEUE_CAPACITY
        );
        assert_eq!(
            effective_packet_queue_capacity(32, "esp32c5"),
            ESP32C5_MAX_PACKET_QUEUE_CAPACITY
        );
        assert_eq!(effective_packet_queue_capacity(256, "classic"), 256);

        let (ingress, _egress) = packet_bus_for_hardware(256, "esp32c5");
        assert_eq!(ingress.capacity(), ESP32C5_MAX_PACKET_QUEUE_CAPACITY);
    }

    #[test]
    fn outbound_queue_tracks_mqtt_and_c5_memory_limits() {
        assert_eq!(
            effective_outbound_queue_capacity(DEFAULT_OUTBOUND_QUEUE_CAPACITY, "esp32c5", false,),
            1
        );
        assert_eq!(
            effective_outbound_queue_capacity(DEFAULT_OUTBOUND_QUEUE_CAPACITY, "esp32c5", true,),
            ESP32C5_MAX_OUTBOUND_QUEUE_CAPACITY
        );
        assert_eq!(
            effective_outbound_queue_capacity(DEFAULT_OUTBOUND_QUEUE_CAPACITY, "classic", true,),
            DEFAULT_OUTBOUND_QUEUE_CAPACITY
        );
    }

    #[test]
    fn ingress_keepalive_prevents_idle_consumer_shutdown() {
        let (ingress, egress) = packet_bus(1);
        let dropped = ingress.dropped_counter();
        let transmitter = UdpTransmitter::from_destination(std::net::SocketAddrV4::new(
            std::net::Ipv4Addr::LOCALHOST,
            9,
        ))
        .unwrap();
        let consumer = spawn_udp_consumer(
            egress,
            transmitter,
            dropped,
            1,
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicUsize::new(0)),
            StorageIngressLink::default(),
        )
        .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(
            !consumer.is_finished(),
            "an idle consumer must stay alive while a producer keepalive exists"
        );

        drop(ingress);
        assert_eq!(consumer.join().unwrap(), RuntimeStats::default());
    }

    #[test]
    fn raw_csi_storage_requires_an_active_recording_gate() {
        let (storage_ingress, storage_egress) = crate::storage::storage_queue(2);
        let gate = RawCsiRecordingGate::default();
        let link = StorageIngressLink::default();
        link.attach(Some((storage_ingress, gate.clone())));
        let frame = sample_frame(7);

        assert!(!link.try_push_csi(&frame));
        assert_eq!(storage_egress.pending_count(), 0);

        gate.activate();
        assert!(link.try_push_csi(&frame));
        assert_eq!(storage_egress.pending_count(), 1);

        gate.deactivate();
        assert!(!link.try_push_csi(&frame));
        assert_eq!(storage_egress.pending_count(), 1);
    }
}
