//! Monitor de saúde da conexão: reconexão automática (L2 + L3) e auto-reboot de
//! segurança. Resolve o sintoma de "ESP some da rede até power-cycle" — sem este
//! monitor, uma queda transitória de WiFi deixa o dispositivo morto, pois o
//! firmware não reassocia nem renova o DHCP após o boot.
//!
//! Verifica DUAS camadas, porque o sintoma mais comum é a STA continuar
//! associada (L2 up) mas perder o IP/lease DHCP (L3 down) — checar só associação
//! deixaria esse caso descoberto:
//! 1. Reconexão L2: se a STA cair, chama `esp_wifi_connect()` para reassociar.
//! 2. Reconexão L3: se associada mas sem IP por uma carência (contada DESDE a
//!    reassociação), força `esp_wifi_disconnect()` + `esp_wifi_connect()` para
//!    re-disparar o DHCP — com throttle para não picotar um lease em negociação.
//! 3. Auto-reboot de segurança: dois backstops independentes —
//!    - insalubre (sem L2 ou sem L3) por `reboot_after_secs` ⇒ `esp_restart()`;
//!    - saudável (L2+L3 OK) mas sem envio UDP por `stall_reboot_secs` (tráfego/CSI
//!      morto) ⇒ `esp_restart()`.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use esp_idf_svc::sys::{
    esp_netif_get_handle_from_ifkey, esp_netif_get_ip_info, esp_netif_ip_info_t, esp_restart,
    esp_wifi_connect, esp_wifi_disconnect, esp_wifi_sta_get_ap_info, wifi_ap_record_t,
};

use crate::config::FirmwareConfig;
use crate::runtime::monotonic_secs;
use crate::status_led::{StatusColor, StatusLed};

/// Stack da thread do monitor. O default de pthread do ESP-IDF (~3 KB) é apertado
/// para o `log::warn!`/formatação dos ramos de transição; reservamos folga.
const MONITOR_STACK_BYTES: usize = 6144;

/// Carência (s) no estado "associado mas sem IP" antes de forçar re-DHCP, contada
/// desde a reassociação, para não interromper um lease ainda em negociação.
const L3_GRACE_SECS: u32 = 15;

/// `true` se a STA está associada a um AP (camada de enlace, L2).
fn wifi_associated() -> bool {
    let mut ap: wifi_ap_record_t = unsafe { core::mem::zeroed() };
    // `esp_wifi_sta_get_ap_info` retorna ESP_OK (0) só quando associada.
    unsafe { esp_wifi_sta_get_ap_info(&mut ap) == 0 }
}

/// Checagem L3 da interface STA:
/// - `Some(true)`: tem IPv4 atribuído;
/// - `Some(false)`: associada mas sem IP (lease ausente/expirado);
/// - `None`: não foi possível determinar (netif não encontrado) — nesse caso o
///   chamador NÃO penaliza, degradando para a checagem L2 apenas (evita reboot
///   falso se a chave do netif diferir do esperado).
fn wifi_has_ip() -> Option<bool> {
    unsafe {
        // Chave padrão do netif STA do ESP-IDF (usada pelo esp-idf-svc).
        let netif = esp_netif_get_handle_from_ifkey(b"WIFI_STA_DEF\0".as_ptr() as *const _);
        if netif.is_null() {
            return None;
        }
        let mut ip_info: esp_netif_ip_info_t = core::mem::zeroed();
        if esp_netif_get_ip_info(netif, &mut ip_info) != 0 {
            return None;
        }
        // `addr == 0` ⇒ sem IP. O teste != 0 independe de endianness.
        Some(ip_info.ip.addr != 0)
    }
}

/// Inicia a thread de monitor de saúde (roda indefinidamente). `last_send_secs` é
/// atualizado pelo consumidor UDP a cada envio bem-sucedido (ver
/// [`crate::runtime::spawn_udp_consumer`]). Tempos em segundos desde o boot.
pub fn spawn_health_monitor(
    last_send_secs: Arc<AtomicU32>,
    config: &FirmwareConfig,
    mut status_led: StatusLed,
) -> std::io::Result<JoinHandle<()>> {
    let check = Duration::from_secs(config.health_check_secs.max(1) as u64);
    let reboot_after_secs = config.reboot_after_secs.max(10);
    let stall_reboot_secs = config.stall_reboot_secs.max(10);
    let autoreconnect = config.wifi_autoreconnect;

    thread::Builder::new()
        .name("aether-health".to_string())
        .stack_size(MONITOR_STACK_BYTES)
        .spawn(move || {
            let mut unhealthy_since: Option<u32> = None;
            let mut healthy_since: Option<u32> = None;
            let mut l3_down_since: Option<u32> = None;
            let mut last_l3_kick: Option<u32> = None;
            let mut l3_unknown_warned = false;
            log::warn!(
                "health: monitor iniciado (check={}s reboot_unhealthy={}s reboot_stall={}s autoreconnect={})",
                check.as_secs(),
                reboot_after_secs,
                stall_reboot_secs,
                autoreconnect
            );

            loop {
                thread::sleep(check);
                let now = monotonic_secs();

                let associated = wifi_associated();
                let l3 = if associated { wifi_has_ip() } else { Some(false) };
                if l3.is_none() && !l3_unknown_warned {
                    log::warn!("health: netif STA não encontrado — checando só L2 (associação).");
                    l3_unknown_warned = true;
                }
                // None (indeterminado) não penaliza: trata como "tem IP".
                let has_ip = l3.unwrap_or(true);
                let healthy = associated && has_ip;

                // Carência L3 contada desde a reassociação: zera fora do estado
                // "associado sem IP" (inclui !associated), e arma quando ele começa.
                if associated && l3 == Some(false) {
                    l3_down_since.get_or_insert(now);
                } else {
                    l3_down_since = None;
                    last_l3_kick = None;
                }

                if healthy {
                    if unhealthy_since.take().is_some() {
                        log::warn!("health: conexão restabelecida (L2+L3 OK)");
                        status_led.set(StatusColor::Streaming);
                    }
                    // Backstop de stall: saudável mas sem envio UDP ⇒ tráfego/CSI
                    // morto. A referência usa o momento em que ficou saudável, para
                    // dar uma janela fresca ao tráfego retomar após uma recuperação
                    // (evita reboot espúrio com `last_send_secs` antigo).
                    let became = *healthy_since.get_or_insert(now);
                    let last = last_send_secs.load(Ordering::Relaxed);
                    let reference = last.max(became);
                    if now.saturating_sub(reference) >= stall_reboot_secs {
                        status_led.set(StatusColor::Error);
                        log::warn!(
                            "health: saudável mas sem envio UDP por >= {}s — reiniciando",
                            stall_reboot_secs
                        );
                        unsafe { esp_restart() };
                    }
                } else {
                    healthy_since = None;
                    let became_unhealthy = unhealthy_since.is_none();
                    let since = *unhealthy_since.get_or_insert(now);
                    let elapsed = now.saturating_sub(since);
                    if became_unhealthy {
                        status_led.set(StatusColor::Offline);
                    }
                    if elapsed == 0 {
                        log::warn!(
                            "health: insalubre (associado={} ip={}) — recuperando",
                            associated, has_ip
                        );
                    }

                    if autoreconnect {
                        unsafe {
                            if !associated {
                                // L2 caída: (re)associa. Chamar já-conectando retorna
                                // erro benigno (não aborta a tentativa em andamento).
                                let _ = esp_wifi_connect();
                            } else if l3 == Some(false) {
                                // Associado sem IP: força re-DHCP só após a carência
                                // (desde a reassociação) e com throttle entre kicks.
                                let l3_elapsed = l3_down_since.map_or(0, |t| now.saturating_sub(t));
                                let due =
                                    last_l3_kick.map_or(true, |t| now.saturating_sub(t) >= L3_GRACE_SECS);
                                if l3_elapsed >= L3_GRACE_SECS && due {
                                    let _ = esp_wifi_disconnect();
                                    let _ = esp_wifi_connect();
                                    last_l3_kick = Some(now);
                                }
                            }
                        }
                    }

                    // Reboot 1: insalubre (sem L2 ou sem L3) por tempo demais.
                    if elapsed >= reboot_after_secs {
                        status_led.set(StatusColor::Error);
                        log::warn!("health: insalubre por >= {}s — reiniciando", reboot_after_secs);
                        unsafe { esp_restart() };
                    }
                }
            }
        })
}
