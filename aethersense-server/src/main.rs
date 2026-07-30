/// AetherSense Server — Ponto de entrada principal.
///
/// Inicializa o runtime Tokio, configura o estado compartilhado,
/// inicia o listener UDP e serve a API HTTP + WebSocket.
mod api;
mod accessories;
mod config;
mod csi;
mod detection;
mod ml;
mod mqtt;
mod processing;
mod replay;
mod state;
mod udp;

use clap::Parser;
use tracing::{info, warn};

use crate::api::routes::create_router;
use crate::config::CliArgs;
use crate::state::new_app_state;
use crate::udp::udp_listener_loop;

/// Task periódica que executa os algoritmos de detecção.
///
/// Roda a cada 500ms e atualiza o estado de detecção (presença,
/// movimento, localização, vitais).
async fn detection_loop(
    state: state::AppState,
    mqtt: mqtt::MqttService,
    mqtt_publish_interval_ms: u64,
    ml_engine: std::sync::Arc<ml::MlEngine>,
) {
    let interval = tokio::time::Duration::from_millis(500);
    let mqtt_publish_interval = tokio::time::Duration::from_millis(mqtt_publish_interval_ms.max(1));
    let mut last_mqtt_publish = tokio::time::Instant::now() - mqtt_publish_interval;
    // Máquina de estados de ocupação por zona — vive entre os ticks do loop.
    let mut zone_tracker = detection::zones::ZoneTracker::new();
    // Zona dedicada do box: usa janela curta WROOM−C5, confirmação de 2,5 s,
    // warm-up e hold de 60 s. É independente da ocupação genérica do banheiro.
    let mut shower_box_tracker = detection::shower_box::ShowerBoxTracker::new();
    let mut presence_tracker = detection::presence::PresenceTracker::default();
    // Semeia o tracker com o piso de ruído persistido no boot: sem isso, todo
    // restart zera os limiares adaptativos e reintroduz ~2 min de janela
    // cega/ruidosa nas automações (justo durante o redeploy).
    let mut last_replay_generation;
    let mut last_shower_baseline_signature;
    {
        let inner = state.read().await;
        zone_tracker.seed_floors(&inner.zone_floors);
        last_replay_generation = inner.replay_generation;
        last_shower_baseline_signature =
            detection::shower_box::baseline_signature(&inner.devices, &inner.location_baselines);
    }
    // Persistência do piso aprendido: NÃO a cada tick (custo de I/O). Reavalia a
    // cada FLOOR_PERSIST_INTERVAL e marca o config como dirty apenas quando o
    // piso mudou de forma significativa, deixando o flush assíncrono já existente
    // gravar em disco.
    const FLOOR_PERSIST_INTERVAL: tokio::time::Duration = tokio::time::Duration::from_secs(30);
    /// Mudança mínima de piso (absoluta ou relativa) para reescrever o config.
    const FLOOR_PERSIST_EPSILON: f64 = 0.01;
    let mut last_floor_persist = tokio::time::Instant::now();

    loop {
        tokio::time::sleep(interval).await;

        // 1. Snapshot dos dados de entrada sob *read lock* (não bloqueia o
        //    ingest UDP exclusivamente, e o clone pesado ocorre fora do write).
        let (
            signal_history,
            signal_times,
            amp_history,
            devices,
            histories,
            baselines,
            previous_location,
            map_config,
            settings,
            shower_box_settings,
            stream_source,
            shower_box_required_session_id,
            replay_generation,
        ) = {
            let inner = state.read().await;
            if !inner.running {
                break;
            }
            (
                inner.signal_history.iter().copied().collect::<Vec<f64>>(),
                inner
                    .signal_time_history
                    .iter()
                    .copied()
                    .collect::<Vec<f64>>(),
                inner
                    .amplitude_history
                    .iter()
                    .cloned()
                    .collect::<Vec<Vec<f64>>>(),
                inner.devices.clone(),
                inner.device_histories.clone(),
                inner.location_baselines.clone(),
                inner.detection.location.clone(),
                inner.map_config.clone(),
                inner.settings.clone(),
                inner.shower_box_settings.clone(),
                inner.stream.source.clone(),
                inner.shower_box_required_session_id.clone(),
                inner.replay_generation,
            )
        };

        // Limpar medições/trocar replay não pode preservar uma válvula em
        // hold dentro do tracker local do loop.
        if replay_generation != last_replay_generation {
            shower_box_tracker.reset();
            last_replay_generation = replay_generation;
        }
        let shower_baseline_signature =
            detection::shower_box::baseline_signature(&devices, &baselines);
        if shower_baseline_signature != last_shower_baseline_signature {
            shower_box_tracker.reset();
            last_shower_baseline_signature = shower_baseline_signature.clone();
        }

        // 2. Computar a detecção FORA de qualquer lock.
        // Presença/movimento/vitais operam sobre UM nó sensor coerente (por-device),
        // nunca sobre o histórico GLOBAL — este intercala pacotes de C5+WROOM, o que
        // inflava a variância (presença falsa) e disparava "running" falso (diferença
        // entre frames de nós distintos), suprimindo os vitais. A localização segue
        // usando todos os nós (fusão). Escolhe o nó não-roteador com mais amostras
        // (empate por MAC, estável); cai para o global se não houver nó.
        let router_macs: std::collections::HashSet<String> = devices
            .iter()
            .filter(|d| {
                d.hardware_profile.eq_ignore_ascii_case("router")
                    || d.role.eq_ignore_ascii_case("router")
            })
            .map(|d| d.id.clone())
            .collect();
        let mut sensing: Vec<(&String, &crate::state::DeviceSignalHistory)> = histories
            .iter()
            .filter(|(mac, _)| !router_macs.contains(mac.as_str()))
            .collect();
        // Nó primário: o de mais amostras (empate por MAC = estável). Movimento e
        // vitais usam o sinal COERENTE dele; cai para o global se não houver nó.
        sensing.sort_by(|a, b| {
            b.1.signal_history
                .len()
                .cmp(&a.1.signal_history.len())
                .then_with(|| a.0.cmp(b.0))
        });
        let (det_signal, det_times, det_amp): (Vec<f64>, Vec<f64>, Vec<Vec<f64>>) =
            match sensing.first() {
                Some((_, h)) => (
                    h.signal_history.iter().copied().collect(),
                    h.signal_time_history.iter().copied().collect(),
                    h.amplitude_history.iter().cloned().collect(),
                ),
                None => (
                    signal_history.clone(),
                    signal_times.clone(),
                    amp_history.clone(),
                ),
            };

        // Presença: fusão-OR sobre os nós sensores, cada um RELATIVO ao seu baseline
        // de sala vazia (variância). Um limiar absoluto fixo era frágil por-device
        // (o C5 tem variância de fundo ~17 e ficava sempre "presente"). Detecta se
        // QUALQUER nó vê perturbação acima do próprio baseline; confiança = a do nó
        // mais confiante. Robusto a um nó ruidoso sem baseline adequado.
        let instant_presence = sensing
            .iter()
            .map(|(mac, h)| {
                let sig: Vec<f64> = h.signal_history.iter().copied().collect();
                let baseline_var = baselines.get(*mac).map(|b| b.amplitude_variance);
                detection::presence::detect_presence(&sig, baseline_var)
            })
            .max_by(|a, b| {
                a.confidence
                    .partial_cmp(&b.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or_else(|| detection::presence::detect_presence(&det_signal, None));

        let shower_motion_detected = instant_presence.detected;
        let movement = detection::movement::detect_movement(&det_amp);
        let location = detection::location::estimate_location_georeferenced(
            &devices,
            &histories,
            &baselines,
            Some(&previous_location),
            Some(&map_config),
        );

        let (vitals, fft_diagnostics) = {
            let improved = processing::cscr::build_vitals_input(
                &det_amp,
                settings.vitals_subcarrier_top_k,
                crate::config::STFT_SAMPLE_RATE_HZ,
            );
            let signal = improved.as_deref().unwrap_or(&det_signal);
            let vitals_presence = instant_presence.detected || presence_tracker.has_active_hold();
            detection::vitals::estimate_vitals_resampled(
                signal,
                &det_times,
                crate::config::STFT_SAMPLE_RATE_HZ,
                vitals_presence,
                &settings,
            )
        };

        // 2b. Inferência ML (ONNX) lado a lado com a heurística
        let occupancy = ml_engine.infer(&devices, &histories, &map_config, &vitals);
        let mut ml_status = ml_engine.status();
        ml_status.last_inference_ms = occupancy.inference_ms;

        // 2c. Zona (cômodo) ativa por perturbação relativa ao baseline
        let mut active_zone =
            detection::location::estimate_active_zone(&devices, &histories, &baselines);
        let fft_breathing_credible = vitals.breathing_rate.is_some() && vitals.confidence >= 0.3;
        let ml_agrees_someone = !occupancy.available || occupancy.count >= 1;
        let breathing_hold = fft_breathing_credible && ml_agrees_someone;
        let presence = presence_tracker.update(instant_presence, breathing_hold);
        // Aplica primeiro os limiares adaptativos/persistidos. A corroboração
        // abaixo não pode usar os defaults baixos de estimate_active_zone.
        zone_tracker.update(&mut active_zone.zones, breathing_hold);
        let (shower_wroom_id, _) = detection::shower_box::receiver_ids(&devices);
        // A zona generica pode sustentar imobilidade, mas nunca criar a entrada
        // do box. Respiracao/ocupacao ML nao entram neste intertravamento: no
        // ambiente vazio real ambas produziram falsos positivos simultaneos.
        let shower_hold_corroborated = shower_wroom_id.as_deref().is_some_and(|wroom_id| {
            active_zone
                .zones
                .iter()
                .find(|zone| zone.device_id == wroom_id)
                .is_some_and(|zone| zone.score >= zone.on_threshold)
        });
        let mut shower_observation = detection::shower_box::observe(
            &devices,
            &histories,
            &baselines,
            shower_box_required_session_id.as_deref(),
        );
        shower_observation.hold_corroborated = shower_hold_corroborated;
        // O hold global de presenca dura 20 s e pode sobreviver a uma passagem.
        // Para confirmar o box usamos somente a evidencia instantanea; a
        // imobilidade continua coberta pela zona corroborada e pela retencao de
        // 60 s do tracker dedicado.
        shower_observation.presence_detected = shower_motion_detected;
        let shower_box = shower_box_tracker.update_for_stream_with_settings(
            &stream_source,
            shower_observation,
            &shower_box_settings,
        );

        // 3. Gravar resultados sob *write lock* breve (apenas atribuições).
        let (mqtt_snapshot, config_to_flush) = {
            let mut inner = state.write().await;
            if !inner.running {
                break;
            }
            // Um clear/replay pode ocorrer entre o snapshot de entrada e este
            // write. Nunca ressuscitar nem publicar um estado do box calculado
            // com dados anteriores à nova geração.
            if inner.replay_generation != replay_generation {
                shower_box_tracker.reset();
                last_replay_generation = inner.replay_generation;
                continue;
            }
            let current_baseline_signature = detection::shower_box::baseline_signature(
                &inner.devices,
                &inner.location_baselines,
            );
            if current_baseline_signature != shower_baseline_signature {
                shower_box_tracker.reset();
                last_shower_baseline_signature = current_baseline_signature;
                continue;
            }
            // Um ajuste pode mudar enquanto este tick calcula fora do lock.
            // Nunca publique um modo/limiar antigo depois de o dashboard já ter
            // confirmado a nova configuração. Descarta este snapshot, mas não
            // derruba uma sessão ocupada: apenas uma tentativa ainda candidata
            // precisa perder os votos obtidos com os limiares antigos.
            if inner.shower_box_settings != shower_box_settings {
                shower_box_tracker.apply_settings_change();
                continue;
            }
            inner.detection.presence = presence;
            inner.detection.movement = movement;
            inner.detection.location = location;
            inner.detection.vitals = vitals;
            inner.detection.fft_diagnostics = fft_diagnostics;
            inner.detection.occupancy = occupancy;
            inner.detection.active_zone = active_zone;
            inner.detection.shower_box = shower_box;
            inner.ml_status = ml_status;
            inner.detection_version = inner.detection_version.wrapping_add(1);

            // Persistência periódica do piso de ruído aprendido por zona.
            // Reavalia a cada FLOOR_PERSIST_INTERVAL e só marca dirty quando algum
            // piso mudou além de FLOOR_PERSIST_EPSILON (evita I/O por ruído).
            if last_floor_persist.elapsed() >= FLOOR_PERSIST_INTERVAL {
                last_floor_persist = tokio::time::Instant::now();
                let learned = zone_tracker.learned_floors();
                let changed =
                    learned
                        .iter()
                        .any(|(id, &new_floor)| match inner.zone_floors.get(id) {
                            Some(&old) => (new_floor - old).abs() > FLOOR_PERSIST_EPSILON,
                            None => true,
                        });
                if changed {
                    inner.zone_floors = learned;
                    inner.config_dirty = true;
                }
            }

            let config_to_flush = if inner.config_dirty {
                match inner.serialize_config_json() {
                    Some(json) => {
                        inner.config_dirty = false;
                        let revision = inner.config_writer.reserve_revision();
                        Some((
                            inner.config_path.clone(),
                            json,
                            inner.config_writer.clone(),
                            revision,
                        ))
                    }
                    None => {
                        warn!("Falha ao serializar config; flush permanecerá pendente");
                        None
                    }
                }
            } else {
                None
            };

            let snapshot = if mqtt.is_connected() {
                Some(mqtt::MqttDetectionSnapshot::from(&*inner))
            } else {
                None
            };
            (snapshot, config_to_flush)
        };

        // 4. Persistir config alterada fora do lock, em thread de bloqueio.
        if let Some((path, json, writer, revision)) = config_to_flush {
            let state_for_retry = state.clone();
            tokio::spawn(async move {
                let display_path = path.clone();
                let result = tokio::task::spawn_blocking(move || {
                    writer.write_if_current(&path, &json, revision)
                })
                .await;

                match result {
                    Ok(Ok(true)) => {}
                    Ok(Ok(false)) => {
                        // Uma revisão síncrona mais nova venceu a corrida. Regrave
                        // o estado atual para também cobrir a hipótese de ela ter
                        // falhado e sofrido rollback no endpoint.
                        state_for_retry.write().await.config_dirty = true;
                    }
                    Ok(Err(error)) => {
                        warn!(
                            "Falha ao persistir config ({}): {} — nova tentativa agendada",
                            display_path.display(),
                            error
                        );
                        state_for_retry.write().await.config_dirty = true;
                    }
                    Err(error) => {
                        warn!(
                            "Tarefa de persistência da config falhou ({}): {} — nova tentativa agendada",
                            display_path.display(),
                            error
                        );
                        state_for_retry.write().await.config_dirty = true;
                    }
                }
            });
        }

        if let Some(snapshot) = mqtt_snapshot {
            if last_mqtt_publish.elapsed() >= mqtt_publish_interval {
                if let Some(publisher) = mqtt.publisher().await {
                    if let Err(error) = publisher.publish_snapshot(&snapshot).await {
                        warn!("Falha ao publicar snapshot MQTT: {}", error);
                    } else {
                        last_mqtt_publish = tokio::time::Instant::now();
                    }
                }
            }
        }
    }
}

#[tokio::main]
async fn main() {
    // Inicializa logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "aethersense_server=info".into()),
        )
        .init();

    let args = CliArgs::parse();
    let state = new_app_state();

    // Semeia os campos MQTT do ambiente/CLI no primeiro boot. A senha é sempre
    // reaplicada ao runtime (inclusive quando os campos não secretos vieram do
    // disco), pois `MqttSettings` nunca serializa o segredo.
    {
        let mut inner = state.write().await;
        if !inner.mqtt_configured {
            inner.mqtt = state::MqttSettings::from(&args);
        }
        // Authentication is process-owned just like the password.  A saved
        // configuration created while the broker was anonymous may contain no
        // username; when the managed broker is enabled, the explicit
        // environment credential must take precedence on every restart.
        if args.mqtt_username.is_some() {
            inner.mqtt.username = args.mqtt_username.clone();
        }
        if args.mqtt_password.is_some() {
            inner
                .mqtt
                .set_runtime_password(args.mqtt_password.clone());
        }
        inner.results_dir = std::path::PathBuf::from(&args.results_dir);
    }

    // Serviço MQTT reconfigurável em runtime (supervisor + publisher swappável).
    let mqtt_service = mqtt::start_mqtt_service(state.clone());
    {
        let mut inner = state.write().await;
        inner.mqtt_service = Some(mqtt_service.clone());
    }

    // Inicia o listener UDP em background
    let udp_state = state.clone();
    tokio::spawn(async move {
        udp_listener_loop(udp_state, args.udp_port).await;
    });

    // Carrega os modelos ONNX (ausência = fallback heurístico, sem pânico)
    let ml_engine = std::sync::Arc::new(ml::MlEngine::load(&args.models_dir));
    {
        let mut inner = state.write().await;
        inner.ml_status = ml_engine.status();
    }

    // Inicia o loop de detecção em background
    let det_state = state.clone();
    let det_mqtt = mqtt_service.clone();
    let mqtt_publish_interval_ms = { state.read().await.mqtt.publish_interval_ms };
    let det_ml = ml_engine.clone();
    tokio::spawn(async move {
        detection_loop(det_state, det_mqtt, mqtt_publish_interval_ms, det_ml).await;
    });

    let addr = format!("{}:{}", args.host, args.port);
    info!("🚀 AetherSense Server rodando em http://{}", addr);
    info!("📡 Escutando dados CSI via UDP na porta {}", args.udp_port);

    let router = create_router(state);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, router).await.unwrap();
}
