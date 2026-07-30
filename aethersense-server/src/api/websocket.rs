/// Handler WebSocket para streaming de dados em tempo real.
///
/// Envia snapshots JSON periodicamente para cada cliente conectado.
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use std::collections::HashMap;
use tracing::info;

use crate::api::handlers::build_snapshot;
use crate::config::WEBSOCKET_INTERVAL_MS;
use crate::state::AppState;

/// Upgrade HTTP → WebSocket.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let once = params
        .get("once")
        .map(|value| matches!(value.as_str(), "1" | "true" | "healthcheck"))
        .unwrap_or(false);

    ws.on_upgrade(move |socket| async move {
        if once {
            send_single_snapshot(socket, state).await
        } else {
            handle_socket(socket, state).await
        }
    })
}

async fn send_single_snapshot(mut socket: WebSocket, state: AppState) {
    let json_str = {
        let inner = state.read().await;
        let snapshot = build_snapshot(&inner);
        match serde_json::to_string(&snapshot) {
            Ok(s) => s,
            Err(e) => {
                info!("Erro ao serializar snapshot unico de healthcheck: {}", e);
                return;
            }
        }
    };

    if socket.send(Message::Text(json_str.into())).await.is_err() {
        info!("Cliente WebSocket de healthcheck desconectou antes do snapshot");
        return;
    }

    let _ = socket.send(Message::Close(None)).await;
}

/// Loop principal do WebSocket: envia dados a cada intervalo.
async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let interval = tokio::time::Duration::from_millis(WEBSOCKET_INTERVAL_MS);
    let mut cached_json: Option<String> = None;
    let mut last_packet_count: u64 = u64::MAX;
    let mut last_replay_gen: u64 = u64::MAX;
    let mut last_detection_version: u64 = u64::MAX;
    let mut last_accessory_version: u64 = u64::MAX;

    loop {
        let json_str = {
            let inner = state.read().await;
            if !inner.running {
                break;
            }

            let changed = inner.packet_count != last_packet_count
                || inner.replay_generation != last_replay_gen
                || inner.detection_version != last_detection_version
                || inner.accessories.version != last_accessory_version;

            if changed || cached_json.is_none() {
                last_packet_count = inner.packet_count;
                last_replay_gen = inner.replay_generation;
                last_detection_version = inner.detection_version;
                last_accessory_version = inner.accessories.version;
                let snapshot = build_snapshot(&inner);
                match serde_json::to_string(&snapshot) {
                    Ok(s) => {
                        cached_json = Some(s.clone());
                        s
                    }
                    Err(e) => {
                        info!("Erro ao serializar snapshot: {}", e);
                        break;
                    }
                }
            } else {
                cached_json.as_ref().unwrap().clone()
            }
        };

        if socket.send(Message::Text(json_str.into())).await.is_err() {
            info!("Cliente WebSocket desconectou");
            break;
        }

        tokio::time::sleep(interval).await;
    }
}
