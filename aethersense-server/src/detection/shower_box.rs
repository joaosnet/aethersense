//! Detecção dedicada à zona do box do chuveiro.
//!
//! O WROOM, instalado próximo ao box, fornece o sinal positivo. O C5 do
//! quarto funciona como referência negativa: uma perturbação que também seja
//! forte no C5 não deve abrir a válvula. A decisão usa uma janela curta de
//! aproximadamente 0,5 s, votação tolerante a oscilações e hold configurável
//! (nunca inferior a 60 s).

use std::collections::{HashMap, VecDeque};

use crate::state::{
    monotonic_seconds, DeviceInfo, DeviceSignalHistory, EmptyRoomBaseline, ShowerBoxAttemptResult,
    ShowerBoxPhase, ShowerBoxSettings, ShowerBoxState, StreamSource,
};

#[cfg(test)]
pub const EXIT_HOLD_SECONDS: f64 = 60.0;
pub const MIN_SAMPLE_RATE_HZ: f64 = 4.0;
pub const STARTUP_WARMUP_SECONDS: f64 = 5.0;

pub(crate) const SIGNAL_FRESHNESS_SECONDS: f64 = 1.5;
const RATE_WINDOW_SECONDS: f64 = 5.0;
const SCORE_WINDOW_SECONDS: f64 = 0.5;
const MIN_RATE_SAMPLES: usize = 5;
const MIN_SCORE_SAMPLES: usize = 2;
pub const MIN_BASELINE_SAMPLES: usize = 100;
const EMPTY_HISTORY_CAP: usize = 240;
const MIN_ADAPTIVE_SAMPLES: usize = 40;
const ENTRY_REQUIRED_RATIO: f64 = 2.0 / 3.0;
const ENTRY_MIN_VOTES: usize = 4;
const ENTRY_MAX_GAP_SECONDS: f64 = 1.0;
/// Margem para o jitter do loop nominal de 500 ms. Sem ela, a amostra mais
/// antiga podia ser podada microssegundos antes de completar a janela e o
/// progresso ficava permanentemente em ~80% no fluxo real.
const ENTRY_WINDOW_JITTER_SECONDS: f64 = 0.75;

const DEFAULT_ON_THRESHOLD: f64 = 0.12;
const DEFAULT_OFF_THRESHOLD: f64 = 0.07;
const DEFAULT_DOMINANCE_ON_THRESHOLD: f64 = 0.07;
const DEFAULT_DOMINANCE_OFF_THRESHOLD: f64 = 0.05;

#[derive(Debug, Clone, Copy, Default)]
pub struct ReceiverObservation {
    pub score: f64,
    pub has_baseline: bool,
    pub available: bool,
    pub sample_rate_hz: f64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ShowerBoxObservation {
    pub now_s: f64,
    pub wroom: ReceiverObservation,
    pub c5: ReceiverObservation,
    /// Confirmação independente da zona genérica. Continua publicada para
    /// diagnóstico, mas não é mais o único apoio permitido durante imobilidade.
    pub hold_corroborated: bool,
    /// Presença heurística global. Só pode renovar o hold quando o próprio
    /// tracker dedicado também confirma WROOM acima do piso e dominante ao C5.
    pub presence_detected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReceiverBaselineSignature {
    mean_amplitude_bits: u64,
    variance_bits: u64,
    sample_count: usize,
    calibrated_at: Option<String>,
    calibration_session_id: Option<String>,
    hardware_profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReceiverMappingSignature {
    role: String,
    device_id: String,
    device_name: String,
    hardware_profile: String,
    baseline: Option<ReceiverBaselineSignature>,
}

/// Assinatura dos baselines que influenciam o box. Permite ao loop invalidar
/// imediatamente Candidate/Holding e o aprendizado vazio quando uma
/// recalibração muda a referência de qualquer receptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineSignature {
    receivers: Vec<ReceiverMappingSignature>,
}

pub fn baseline_signature(
    devices: &[DeviceInfo],
    baselines: &HashMap<String, EmptyRoomBaseline>,
) -> BaselineSignature {
    let (wroom, c5) = receiver_pair(devices);
    let mut receivers = [("wroom", wroom), ("c5", c5)]
        .into_iter()
        .filter_map(|(role, device)| {
            let device = device?;
            let baseline = baselines
                .get(&device.id)
                .map(|baseline| ReceiverBaselineSignature {
                    mean_amplitude_bits: baseline.mean_amplitude.to_bits(),
                    variance_bits: baseline.amplitude_variance.to_bits(),
                    sample_count: baseline.sample_count,
                    calibrated_at: baseline.calibrated_at.clone(),
                    calibration_session_id: baseline.calibration_session_id.clone(),
                    hardware_profile: baseline.hardware_profile.clone(),
                });
            Some(ReceiverMappingSignature {
                role: role.to_string(),
                device_id: device.id.clone(),
                device_name: device.name.clone(),
                hardware_profile: device.hardware_profile.clone(),
                baseline,
            })
        })
        .collect::<Vec<_>>();
    receivers.sort_by(|left, right| left.role.cmp(&right.role));
    BaselineSignature { receivers }
}

pub(crate) fn receiver_ids(devices: &[DeviceInfo]) -> (Option<String>, Option<String>) {
    let (wroom, c5) = receiver_pair(devices);
    (
        wroom.map(|device| device.id.clone()),
        c5.map(|device| device.id.clone()),
    )
}

/// Constrói a observação do tick diretamente dos históricos coerentes por
/// receptor. `CsiPacket::rate` não é usado: ele representa a taxa PHY, não a
/// frequência de chegada de pacotes CSI.
pub fn observe(
    devices: &[DeviceInfo],
    histories: &HashMap<String, DeviceSignalHistory>,
    baselines: &HashMap<String, EmptyRoomBaseline>,
    required_calibration_session: Option<&str>,
) -> ShowerBoxObservation {
    observe_at(
        devices,
        histories,
        baselines,
        required_calibration_session,
        monotonic_seconds(),
    )
}

fn observe_at(
    devices: &[DeviceInfo],
    histories: &HashMap<String, DeviceSignalHistory>,
    baselines: &HashMap<String, EmptyRoomBaseline>,
    required_calibration_session: Option<&str>,
    now_s: f64,
) -> ShowerBoxObservation {
    let (wroom, c5) = receiver_pair(devices);
    let shared_calibration_session =
        shared_calibration_session(devices, baselines, required_calibration_session);

    ShowerBoxObservation {
        now_s,
        wroom: receiver_observation(
            wroom,
            histories,
            baselines,
            shared_calibration_session,
            now_s,
        ),
        c5: receiver_observation(c5, histories, baselines, shared_calibration_session, now_s),
        hold_corroborated: false,
        presence_detected: false,
    }
}

fn shared_calibration_session<'a>(
    devices: &[DeviceInfo],
    baselines: &'a HashMap<String, EmptyRoomBaseline>,
    required_calibration_session: Option<&str>,
) -> Option<&'a str> {
    let (wroom, c5) = receiver_pair(devices);
    let wroom_session = valid_baseline(wroom?, baselines)?
        .calibration_session_id
        .as_deref()?;
    let c5_session = valid_baseline(c5?, baselines)?
        .calibration_session_id
        .as_deref()?;
    if wroom_session != c5_session
        || required_calibration_session.is_some_and(|required| required != wroom_session)
    {
        None
    } else {
        Some(wroom_session)
    }
}

pub(crate) fn calibrated_pair_is_valid(
    devices: &[DeviceInfo],
    baselines: &HashMap<String, EmptyRoomBaseline>,
    required_calibration_session: Option<&str>,
) -> bool {
    shared_calibration_session(devices, baselines, required_calibration_session).is_some()
}

fn receiver_pair(devices: &[DeviceInfo]) -> (Option<&DeviceInfo>, Option<&DeviceInfo>) {
    let wroom = devices
        .iter()
        .find(|device| is_receiver(device) && is_wroom(device));
    let c5 = devices.iter().find(|device| {
        is_receiver(device) && is_c5(device) && wroom.is_none_or(|wroom| wroom.id != device.id)
    });
    (wroom, c5)
}

fn is_receiver(device: &DeviceInfo) -> bool {
    let role = device.role.trim();
    role.is_empty() || role.eq_ignore_ascii_case("receiver")
}

fn is_wroom(device: &DeviceInfo) -> bool {
    let profile = device.hardware_profile.to_ascii_lowercase();
    let name = device.name.to_ascii_lowercase();
    if profile.trim().is_empty() {
        name.contains("wroom")
    } else {
        profile.contains("wroom") || profile == "classic"
    }
}

fn is_c5(device: &DeviceInfo) -> bool {
    let profile = device.hardware_profile.to_ascii_lowercase();
    if profile.trim().is_empty() {
        device.name.to_ascii_lowercase().contains("c5")
    } else {
        profile.contains("c5")
    }
}

fn valid_baseline<'a>(
    device: &DeviceInfo,
    baselines: &'a HashMap<String, EmptyRoomBaseline>,
) -> Option<&'a EmptyRoomBaseline> {
    baselines.get(&device.id).filter(|baseline| {
        baseline.sample_count >= MIN_BASELINE_SAMPLES
            && baseline.mean_amplitude.is_finite()
            && baseline.amplitude_variance.is_finite()
            && baseline.mean_amplitude > 0.0
            && baseline.amplitude_variance >= 0.0
            && baseline
                .hardware_profile
                .trim()
                .eq_ignore_ascii_case(device.hardware_profile.trim())
    })
}

fn receiver_observation(
    device: Option<&DeviceInfo>,
    histories: &HashMap<String, DeviceSignalHistory>,
    baselines: &HashMap<String, EmptyRoomBaseline>,
    shared_calibration_session: Option<&str>,
    now_s: f64,
) -> ReceiverObservation {
    let Some(device) = device else {
        return ReceiverObservation::default();
    };
    let Some(history) = histories.get(&device.id) else {
        return ReceiverObservation::default();
    };

    let baseline = valid_baseline(device, baselines).filter(|baseline| {
        shared_calibration_session.is_some()
            && baseline.calibration_session_id.as_deref() == shared_calibration_session
    });
    let sample_rate_hz = recent_sample_rate_hz(history, now_s);
    let fresh = history
        .signal_time_history
        .back()
        .is_some_and(|last| now_s >= *last && now_s - *last <= SIGNAL_FRESHNESS_SECONDS);
    let available = fresh && sample_rate_hz >= MIN_SAMPLE_RATE_HZ;
    let score = baseline
        .map(|baseline| short_window_score(history, baseline, now_s))
        .unwrap_or(0.0);

    ReceiverObservation {
        score,
        has_baseline: baseline.is_some(),
        available,
        sample_rate_hz,
    }
}

pub(crate) fn recent_sample_rate_hz(history: &DeviceSignalHistory, now_s: f64) -> f64 {
    let start = now_s - RATE_WINDOW_SECONDS;
    let recent: Vec<f64> = history
        .signal_time_history
        .iter()
        .copied()
        .filter(|time| *time >= start && *time <= now_s)
        .collect();
    if recent.len() < MIN_RATE_SAMPLES {
        return 0.0;
    }
    let elapsed = recent.last().unwrap() - recent.first().unwrap();
    if elapsed <= f64::EPSILON {
        0.0
    } else {
        (recent.len() - 1) as f64 / elapsed
    }
}

fn short_window_score(
    history: &DeviceSignalHistory,
    baseline: &EmptyRoomBaseline,
    now_s: f64,
) -> f64 {
    let start = now_s - SCORE_WINDOW_SECONDS;
    let values: Vec<f64> = history
        .signal_time_history
        .iter()
        .zip(history.signal_history.iter())
        .filter_map(|(time, value)| (*time >= start && *time <= now_s).then_some(*value))
        .collect();
    if values.len() < MIN_SCORE_SAMPLES {
        return 0.0;
    }

    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let variance = values
        .iter()
        .map(|value| {
            let delta = *value - mean;
            delta * delta
        })
        .sum::<f64>()
        / values.len() as f64;
    let baseline_amp = baseline.mean_amplitude.max(1.0);
    let amplitude_delta = ((mean - baseline.mean_amplitude).abs() / baseline_amp).min(2.0);
    let variance_delta =
        (variance.sqrt() / (baseline_amp + baseline.amplitude_variance.sqrt())).min(2.0);
    (amplitude_delta * 0.65 + variance_delta * 0.35).clamp(0.0, 2.0)
}

#[derive(Debug, Default)]
pub struct ShowerBoxTracker {
    phase: ShowerBoxPhase,
    healthy_since_s: Option<f64>,
    entry_votes: VecDeque<(f64, bool)>,
    last_entry_evidence_s: Option<f64>,
    last_evidence_s: Option<f64>,
    continuous_evidence_since_s: Option<f64>,
    empty_wroom_scores: VecDeque<f64>,
    empty_dominance_scores: VecDeque<f64>,
    last_transition_reason: String,
    attempt_sequence: u64,
    last_attempt_started_at_unix_ms: Option<u64>,
    last_attempt_finished_at_unix_ms: Option<u64>,
    last_attempt_result: ShowerBoxAttemptResult,
    last_attempt_peak_progress: f64,
    session_sequence: u64,
    active_session_id: Option<String>,
    last_session_started_at_unix_ms: Option<u64>,
    last_session_ended_at_unix_ms: Option<u64>,
    last_session_end_reason: Option<String>,
    /// Impede regressão de timestamps se o relógio civil for ajustado durante
    /// o processo e também torna eventos consecutivos ordenáveis nos testes.
    last_audit_event_unix_ms: u64,
}

impl ShowerBoxTracker {
    pub fn new() -> Self {
        Self {
            last_transition_reason: "startup_not_ready".to_string(),
            ..Self::default()
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Aplica uma alteração de configuração sem derrubar uma sessão já
    /// confirmada. A chamada ocorre quando o dashboard salva enquanto um tick
    /// ainda estava sendo calculado com o snapshot anterior.
    ///
    /// O piso adaptativo e o warm-up saudável são preservados. Uma tentativa
    /// ainda candidata é encerrada para que votos obtidos com limiares antigos
    /// não sejam reaproveitados; sessões `occupied`/`holding` mantêm sua
    /// identidade e o instante da última evidência.
    pub fn apply_settings_change(&mut self) {
        self.entry_votes.clear();
        self.last_entry_evidence_s = None;
        self.continuous_evidence_since_s = None;

        match self.phase {
            ShowerBoxPhase::Candidate => {
                self.finish_attempt(ShowerBoxAttemptResult::Rejected);
                self.phase = ShowerBoxPhase::Vacant;
                self.last_transition_reason = "settings_applied_candidate_rejected".to_string();
            }
            ShowerBoxPhase::Vacant => {
                self.last_transition_reason = "settings_applied_vacant".to_string();
            }
            ShowerBoxPhase::Occupied | ShowerBoxPhase::Holding => {
                self.last_transition_reason = "settings_applied_session_preserved".to_string();
            }
        }
    }

    /// Somente CSI UDP ao vivo pode comandar a zona física. Replays e estado
    /// ocioso sempre resetam a máquina e publicam `vacant`, mesmo que as séries
    /// reproduzidas tenham scores suficientes para uma entrada.
    #[cfg(test)]
    pub fn update_for_stream(
        &mut self,
        source: &StreamSource,
        observation: ShowerBoxObservation,
    ) -> ShowerBoxState {
        self.update_for_stream_with_settings(source, observation, &ShowerBoxSettings::default())
    }

    pub fn update_for_stream_with_settings(
        &mut self,
        source: &StreamSource,
        observation: ShowerBoxObservation,
        settings: &ShowerBoxSettings,
    ) -> ShowerBoxState {
        if *source == StreamSource::UdpLive {
            return self.update_with_settings(observation, settings);
        }

        self.reset();
        self.last_transition_reason = if *source == StreamSource::CsvReplay {
            "replay_blocked"
        } else {
            "live_stream_required"
        }
        .to_string();
        let safe_observation = ShowerBoxObservation {
            now_s: observation.now_s,
            wroom: ReceiverObservation {
                has_baseline: observation.wroom.has_baseline,
                ..ReceiverObservation::default()
            },
            c5: ReceiverObservation {
                has_baseline: observation.c5.has_baseline,
                ..ReceiverObservation::default()
            },
            hold_corroborated: false,
            presence_detected: false,
        };
        self.update_with_settings(safe_observation, settings)
    }

    #[cfg(test)]
    pub fn update(&mut self, observation: ShowerBoxObservation) -> ShowerBoxState {
        self.update_with_settings(observation, &ShowerBoxSettings::default())
    }

    pub fn update_with_settings(
        &mut self,
        observation: ShowerBoxObservation,
        settings: &ShowerBoxSettings,
    ) -> ShowerBoxState {
        let now_s = observation.now_s;
        let raw_ready = observation.wroom.available
            && observation.c5.available
            && observation.wroom.has_baseline
            && observation.c5.has_baseline;

        if raw_ready {
            self.healthy_since_s.get_or_insert(now_s);
        } else {
            self.healthy_since_s = None;
        }
        let ready = raw_ready
            && self
                .healthy_since_s
                .is_some_and(|since| now_s - since >= STARTUP_WARMUP_SECONDS);

        let warmup_seconds_remaining = if raw_ready {
            self.healthy_since_s
                .map(|since| (STARTUP_WARMUP_SECONDS - (now_s - since)).max(0.0))
                .unwrap_or(STARTUP_WARMUP_SECONDS)
        } else {
            STARTUP_WARMUP_SECONDS
        };
        let (on_threshold, off_threshold, dominance_on, _dominance_off) = self.thresholds(settings);
        let dominance = observation.wroom.score - observation.c5.score;
        let strong_box_evidence =
            raw_ready && observation.wroom.score >= on_threshold && dominance >= dominance_on;
        // O WROOM vazio apresentou em producao rajadas dominantes por varios
        // segundos. Portanto, no perfil seguro padrao, WROOM+C5 define a zona,
        // mas nao pode sozinho abrir ou renovar a valvula: exige confirmacao
        // independente de movimento instantaneo. Corroboracao estatica so pode
        // sustentar uma sessao que ja foi aberta legitimamente; nunca cria uma
        // entrada. Desativar presence_assist preserva um fallback explicito.
        let entry_evidence = ready
            && strong_box_evidence
            && (!settings.presence_assist || observation.presence_detected);
        let presence_assisted =
            strong_box_evidence && settings.presence_assist && observation.presence_detected;
        let corroborated_static =
            raw_ready && observation.hold_corroborated && observation.wroom.score >= off_threshold;
        let unassisted_box_evidence = !settings.presence_assist && strong_box_evidence;
        let hold_evidence = unassisted_box_evidence || presence_assisted || corroborated_static;

        match self.phase {
            ShowerBoxPhase::Vacant => {
                self.last_evidence_s = None;
                self.continuous_evidence_since_s = None;
                self.entry_votes.clear();
                self.last_entry_evidence_s = None;
                // `startup_not_ready` descreve o boot, não o estado estável.
                // Sem esta promoção o dashboard podia mostrar "Preparando"
                // ao mesmo tempo em que `ready=true` e todos os gates estavam
                // liberados. Preserva desfechos reais (rejeição/saída) até a
                // próxima transição, trocando apenas motivos de bloqueio que
                // deixaram de ser verdadeiros.
                if ready
                    && matches!(
                        self.last_transition_reason.as_str(),
                        "startup_not_ready" | "live_stream_required" | "replay_blocked"
                    )
                {
                    self.last_transition_reason = "ready_vacant".to_string();
                }
                if entry_evidence {
                    self.record_entry_vote(now_s, true, settings.entry_confirm_seconds);
                    self.last_entry_evidence_s = Some(now_s);
                    self.start_attempt();
                    self.transition(ShowerBoxPhase::Candidate, "entry_candidate");
                } else if ready
                    && !observation.presence_detected
                    && !observation.hold_corroborated
                    && observation.wroom.score < off_threshold
                    && observation.c5.score < off_threshold
                {
                    self.record_empty(observation.wroom.score, dominance);
                }
            }
            ShowerBoxPhase::Candidate => {
                if !ready {
                    self.entry_votes.clear();
                    self.last_entry_evidence_s = None;
                    self.finish_attempt(ShowerBoxAttemptResult::NotReady);
                    self.transition(ShowerBoxPhase::Vacant, "candidate_input_not_ready");
                } else {
                    self.record_entry_vote(now_s, entry_evidence, settings.entry_confirm_seconds);
                    if entry_evidence {
                        self.last_entry_evidence_s = Some(now_s);
                    }
                    let (vote_span_s, vote_ratio) = self.entry_vote_status();
                    if entry_evidence
                        && vote_span_s + f64::EPSILON >= settings.entry_confirm_seconds
                        && vote_ratio + f64::EPSILON >= ENTRY_REQUIRED_RATIO
                        && self.entry_votes.len() >= ENTRY_MIN_VOTES
                    {
                        self.entry_votes.clear();
                        self.last_entry_evidence_s = None;
                        self.last_evidence_s = Some(now_s);
                        self.continuous_evidence_since_s = Some(now_s);
                        self.finish_attempt(ShowerBoxAttemptResult::Confirmed);
                        self.start_session();
                        self.transition(ShowerBoxPhase::Occupied, "entry_confirmed");
                    } else if !entry_evidence
                        && self
                            .last_entry_evidence_s
                            .is_none_or(|last| now_s - last > settings.entry_decay_seconds)
                    {
                        self.entry_votes.clear();
                        self.last_entry_evidence_s = None;
                        self.finish_attempt(ShowerBoxAttemptResult::Rejected);
                        self.transition(ShowerBoxPhase::Vacant, "candidate_rejected");
                    }
                }
            }
            ShowerBoxPhase::Occupied | ShowerBoxPhase::Holding => {
                self.entry_votes.clear();
                self.last_entry_evidence_s = None;
                if hold_evidence {
                    self.last_evidence_s.get_or_insert(now_s);

                    if let Some(since) = self.continuous_evidence_since_s {
                        if now_s - since >= settings.recovery_confirm_seconds {
                            self.last_evidence_s = Some(now_s);
                            self.transition(ShowerBoxPhase::Occupied, "evidence_reconfirmed");
                        } else {
                            self.transition(ShowerBoxPhase::Holding, "evidence_recovery_pending");
                        }
                    } else {
                        // Mesmo no limite do hold, nunca publique OFF no mesmo
                        // tick em que há evidência forte. Concede apenas a curta
                        // janela configurada de recuperação; um pico que sumir
                        // no tick seguinte ainda desliga sem renovar 60 s.
                        self.continuous_evidence_since_s = Some(now_s);
                        self.transition(ShowerBoxPhase::Holding, "evidence_recovery_pending");
                    }
                } else {
                    self.continuous_evidence_since_s = None;
                    let last = self.last_evidence_s.get_or_insert(now_s);
                    if now_s - *last >= settings.exit_hold_seconds {
                        self.last_evidence_s = None;
                        self.finish_session("hold_expired");
                        self.transition(ShowerBoxPhase::Vacant, "hold_expired");
                    } else {
                        let reason = if raw_ready {
                            "weak_signal_holding"
                        } else {
                            "input_unavailable_holding"
                        };
                        self.transition(ShowerBoxPhase::Holding, reason);
                    }
                }
            }
        }

        let occupied = matches!(
            self.phase,
            ShowerBoxPhase::Occupied | ShowerBoxPhase::Holding
        );
        let hold_seconds_remaining = if occupied {
            self.last_evidence_s
                .map(|last| {
                    (settings.exit_hold_seconds - (now_s - last))
                        .clamp(0.0, settings.exit_hold_seconds)
                })
                .unwrap_or(settings.exit_hold_seconds)
        } else {
            0.0
        };
        let (entry_vote_span_s, entry_vote_ratio) = self.entry_vote_status();
        let entry_progress = if occupied {
            1.0
        } else {
            let time_progress = (entry_vote_span_s
                / settings.entry_confirm_seconds.max(f64::EPSILON))
            .clamp(0.0, 1.0);
            let quorum_progress = (entry_vote_ratio / ENTRY_REQUIRED_RATIO).clamp(0.0, 1.0);
            let sample_progress =
                (self.entry_votes.len() as f64 / ENTRY_MIN_VOTES as f64).clamp(0.0, 1.0);
            (time_progress * quorum_progress * sample_progress).clamp(0.0, 1.0)
        };
        self.update_attempt_peak(entry_progress);
        let entry_seconds_remaining = if occupied {
            0.0
        } else {
            (settings.entry_confirm_seconds - entry_vote_span_s).max(0.0)
        };
        let (on_threshold, off_threshold, dominance_on, dominance_off) = self.thresholds(settings);

        ShowerBoxState {
            occupied,
            phase: self.phase,
            ready,
            wroom_available: observation.wroom.available,
            c5_available: observation.c5.available,
            wroom_has_baseline: observation.wroom.has_baseline,
            c5_has_baseline: observation.c5.has_baseline,
            wroom_sample_rate_hz: observation.wroom.sample_rate_hz,
            c5_sample_rate_hz: observation.c5.sample_rate_hz,
            wroom_score: observation.wroom.score,
            c5_score: observation.c5.score,
            dominance,
            on_threshold,
            off_threshold,
            dominance_on_threshold: dominance_on,
            dominance_off_threshold: dominance_off,
            hold_corroborated: observation.hold_corroborated,
            presence_assisted,
            hold_evidence,
            entry_progress,
            entry_seconds_remaining,
            warmup_seconds_remaining,
            hold_seconds_remaining,
            profile: settings.profile,
            control_mode: settings.control_mode,
            configured_entry_confirm_seconds: settings.entry_confirm_seconds,
            configured_entry_decay_seconds: settings.entry_decay_seconds,
            configured_exit_hold_seconds: settings.exit_hold_seconds,
            configured_recovery_confirm_seconds: settings.recovery_confirm_seconds,
            sensitivity: settings.sensitivity,
            presence_assist_enabled: settings.presence_assist,
            last_transition_reason: self.last_transition_reason.clone(),
            attempt_sequence: self.attempt_sequence,
            last_attempt_started_at_unix_ms: self.last_attempt_started_at_unix_ms,
            last_attempt_finished_at_unix_ms: self.last_attempt_finished_at_unix_ms,
            last_attempt_result: self.last_attempt_result,
            last_attempt_peak_progress: self.last_attempt_peak_progress,
            session_sequence: self.session_sequence,
            active_session_id: self.active_session_id.clone(),
            last_session_started_at_unix_ms: self.last_session_started_at_unix_ms,
            last_session_ended_at_unix_ms: self.last_session_ended_at_unix_ms,
            last_session_end_reason: self.last_session_end_reason.clone(),
        }
    }

    fn start_attempt(&mut self) {
        let started_at = self.next_audit_unix_ms();
        self.attempt_sequence = self.attempt_sequence.saturating_add(1);
        self.last_attempt_started_at_unix_ms = Some(started_at);
        self.last_attempt_finished_at_unix_ms = None;
        self.last_attempt_result = ShowerBoxAttemptResult::InProgress;
        self.last_attempt_peak_progress = 0.0;
    }

    fn finish_attempt(&mut self, result: ShowerBoxAttemptResult) {
        if self.last_attempt_result != ShowerBoxAttemptResult::InProgress {
            return;
        }
        self.last_attempt_finished_at_unix_ms = Some(self.next_audit_unix_ms());
        self.last_attempt_result = result;
        if result == ShowerBoxAttemptResult::Confirmed {
            self.last_attempt_peak_progress = 1.0;
        }
    }

    fn update_attempt_peak(&mut self, progress: f64) {
        if self.last_attempt_result == ShowerBoxAttemptResult::InProgress && progress.is_finite() {
            self.last_attempt_peak_progress = self
                .last_attempt_peak_progress
                .max(progress.clamp(0.0, 1.0));
        }
    }

    fn start_session(&mut self) {
        let started_at = self.next_audit_unix_ms();
        self.session_sequence = self.session_sequence.saturating_add(1);
        self.active_session_id = Some(format!(
            "shower-box-{}-{}",
            self.session_sequence, started_at
        ));
        self.last_session_started_at_unix_ms = Some(started_at);
        self.last_session_ended_at_unix_ms = None;
        self.last_session_end_reason = None;
    }

    fn finish_session(&mut self, reason: &str) {
        if self.active_session_id.take().is_none() {
            return;
        }
        self.last_session_ended_at_unix_ms = Some(self.next_audit_unix_ms());
        self.last_session_end_reason = Some(reason.to_string());
    }

    fn next_audit_unix_ms(&mut self) -> u64 {
        let wall_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        let timestamp = if self.last_audit_event_unix_ms == 0 {
            wall_time
        } else {
            wall_time.max(self.last_audit_event_unix_ms.saturating_add(1))
        };
        self.last_audit_event_unix_ms = timestamp;
        timestamp
    }

    fn transition(&mut self, phase: ShowerBoxPhase, reason: &str) {
        if self.phase != phase {
            self.phase = phase;
            self.last_transition_reason = reason.to_string();
        }
    }

    fn record_entry_vote(&mut self, now_s: f64, evidence: bool, window_seconds: f64) {
        if self
            .entry_votes
            .back()
            .is_some_and(|(last_time, _)| now_s - *last_time > ENTRY_MAX_GAP_SECONDS)
        {
            self.entry_votes.clear();
        }
        self.entry_votes.push_back((now_s, evidence));
        let window_start = now_s - window_seconds - ENTRY_WINDOW_JITTER_SECONDS;
        while self
            .entry_votes
            .front()
            .is_some_and(|(time, _)| *time < window_start)
        {
            self.entry_votes.pop_front();
        }
    }

    fn entry_vote_status(&self) -> (f64, f64) {
        let Some((first_time, _)) = self.entry_votes.front() else {
            return (0.0, 0.0);
        };
        let Some((last_time, _)) = self.entry_votes.back() else {
            return (0.0, 0.0);
        };
        let positives = self
            .entry_votes
            .iter()
            .filter(|(_, evidence)| *evidence)
            .count();
        (
            (last_time - first_time).max(0.0),
            positives as f64 / self.entry_votes.len().max(1) as f64,
        )
    }

    fn record_empty(&mut self, wroom_score: f64, dominance: f64) {
        if self.empty_wroom_scores.len() >= EMPTY_HISTORY_CAP {
            self.empty_wroom_scores.pop_front();
            self.empty_dominance_scores.pop_front();
        }
        self.empty_wroom_scores.push_back(wroom_score);
        self.empty_dominance_scores.push_back(dominance);
    }

    fn thresholds(&self, settings: &ShowerBoxSettings) -> (f64, f64, f64, f64) {
        let base = if self.empty_wroom_scores.len() < MIN_ADAPTIVE_SAMPLES {
            (
                DEFAULT_ON_THRESHOLD,
                DEFAULT_OFF_THRESHOLD,
                DEFAULT_DOMINANCE_ON_THRESHOLD,
                DEFAULT_DOMINANCE_OFF_THRESHOLD,
            )
        } else {
            let mut wroom: Vec<f64> = self.empty_wroom_scores.iter().copied().collect();
            let mut dominance: Vec<f64> = self.empty_dominance_scores.iter().copied().collect();
            wroom.sort_by(|a, b| a.total_cmp(b));
            dominance.sort_by(|a, b| a.total_cmp(b));

            let floor = percentile(&wroom, 0.20);
            let on = (percentile(&wroom, 0.90) + 0.035)
                .max(floor * 1.8)
                .clamp(0.10, 0.35);
            let off = (percentile(&wroom, 0.75) + 0.015)
                .max(floor * 1.25)
                .clamp(0.05, 0.25)
                .min(on - 0.02);
            let dominance_on = (percentile(&dominance, 0.90) + 0.025).clamp(0.04, 0.30);
            let dominance_off = (percentile(&dominance, 0.75) + 0.010)
                .clamp(0.02, 0.20)
                .min(dominance_on - 0.01);
            (on, off, dominance_on, dominance_off)
        };

        let factor = settings.threshold_factor();
        let on = (base.0 * factor).clamp(0.08, 0.40);
        let off = (base.1 * factor).clamp(0.035, 0.30).min(on - 0.015);
        let dominance_on = (base.2 * factor).clamp(0.03, 0.35);
        let dominance_off = (base.3 * factor)
            .clamp(0.015, 0.25)
            .min(dominance_on - 0.008);
        (on, off, dominance_on, dominance_off)
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, name: &str, profile: &str) -> DeviceInfo {
        DeviceInfo {
            id: id.to_string(),
            name: name.to_string(),
            lat: 0.0,
            lng: 0.0,
            floor: 0,
            icon: String::new(),
            size: 32,
            role: "receiver".to_string(),
            hardware_profile: profile.to_string(),
        }
    }

    fn timed_history(step_s: f64, end_s: f64) -> DeviceSignalHistory {
        timed_history_with(step_s, end_s, |_| 20.0)
    }

    fn timed_history_with(
        step_s: f64,
        end_s: f64,
        signal: impl Fn(f64) -> f64,
    ) -> DeviceSignalHistory {
        let mut history = DeviceSignalHistory::new();
        let mut time = 0.0;
        while time <= end_s + f64::EPSILON {
            history.signal_time_history.push_back(time);
            history.signal_history.push_back(signal(time));
            time += step_s;
        }
        history
    }

    fn baseline(profile: &str, samples: usize) -> EmptyRoomBaseline {
        EmptyRoomBaseline {
            mean_amplitude: 20.0,
            amplitude_variance: 0.1,
            sample_count: samples,
            hardware_profile: profile.to_string(),
            calibration_session_id: Some("test-empty-session".to_string()),
            ..Default::default()
        }
    }

    fn receiver(score: f64) -> ReceiverObservation {
        ReceiverObservation {
            score,
            has_baseline: true,
            available: true,
            sample_rate_hz: 10.0,
        }
    }

    fn observation(now_s: f64, wroom: f64, c5: f64) -> ShowerBoxObservation {
        ShowerBoxObservation {
            now_s,
            wroom: receiver(wroom),
            c5: receiver(c5),
            hold_corroborated: true,
            presence_detected: true,
        }
    }

    fn uncorroborated_observation(now_s: f64, wroom: f64, c5: f64) -> ShowerBoxObservation {
        ShowerBoxObservation {
            hold_corroborated: false,
            presence_detected: false,
            ..observation(now_s, wroom, c5)
        }
    }

    fn static_only_observation(now_s: f64, wroom: f64, c5: f64) -> ShowerBoxObservation {
        ShowerBoxObservation {
            hold_corroborated: true,
            presence_detected: false,
            ..observation(now_s, wroom, c5)
        }
    }

    fn unavailable(now_s: f64) -> ShowerBoxObservation {
        ShowerBoxObservation {
            now_s,
            wroom: ReceiverObservation::default(),
            c5: ReceiverObservation::default(),
            hold_corroborated: false,
            presence_detected: false,
        }
    }

    fn warm_up(tracker: &mut ShowerBoxTracker) {
        tracker.update(observation(0.0, 0.02, 0.02));
        let state = tracker.update(observation(5.0, 0.02, 0.02));
        assert!(state.ready);
    }

    fn physical_observation(
        now_s: f64,
        pulse_start_s: f64,
        pulse_end_s: Option<f64>,
    ) -> ShowerBoxObservation {
        let devices = vec![
            device("w", "ESP32-WROOM", "wroom"),
            device("c", "ESP32-C5", "esp32c5"),
        ];
        let histories = HashMap::from([
            (
                "w".to_string(),
                timed_history_with(0.1, now_s, |time| {
                    let pulse_active =
                        time >= pulse_start_s && pulse_end_s.map(|end| time < end).unwrap_or(true);
                    if pulse_active {
                        30.0
                    } else {
                        20.0
                    }
                }),
            ),
            ("c".to_string(), timed_history(0.1, now_s)),
        ]);
        let baselines = HashMap::from([
            ("w".to_string(), baseline("wroom", 100)),
            ("c".to_string(), baseline("esp32c5", 100)),
        ]);
        let mut observed = observe_at(&devices, &histories, &baselines, None, now_s);
        observed.hold_corroborated = observed.wroom.score >= DEFAULT_OFF_THRESHOLD;
        observed.presence_detected = observed.hold_corroborated;
        observed
    }

    #[test]
    fn observacao_mapeia_perfis_e_calcula_taxa_csi_real() {
        let devices = vec![
            device("w", "ESP32-WROOM", "wroom"),
            device("c", "ESP32-C5", "esp32c5"),
        ];
        let histories = HashMap::from([
            ("w".to_string(), timed_history(0.25, 5.0)),
            ("c".to_string(), timed_history(0.20, 5.0)),
        ]);
        let baselines = HashMap::from([
            ("w".to_string(), baseline("wroom", 100)),
            ("c".to_string(), baseline("esp32c5", 100)),
        ]);

        let observed = observe_at(&devices, &histories, &baselines, None, 5.0);
        assert!(observed.wroom.available);
        assert!(observed.c5.available);
        assert!((observed.wroom.sample_rate_hz - 4.0).abs() < 0.01);
        assert!((observed.c5.sample_rate_hz - 5.0).abs() < 0.01);
    }

    #[test]
    fn taxa_baixa_ou_baseline_curto_bloqueiam_prontidao() {
        let devices = vec![
            device("w", "ESP32-WROOM", "wroom"),
            device("c", "ESP32-C5", "esp32c5"),
        ];
        let histories = HashMap::from([
            ("w".to_string(), timed_history(1.0, 5.0)),
            ("c".to_string(), timed_history(0.20, 5.0)),
        ]);
        let baselines = HashMap::from([
            ("w".to_string(), baseline("wroom", 99)),
            ("c".to_string(), baseline("esp32c5", 100)),
        ]);

        let observed = observe_at(&devices, &histories, &baselines, None, 5.0);
        assert!(!observed.wroom.available);
        assert!(!observed.wroom.has_baseline);
        let mut tracker = ShowerBoxTracker::new();
        assert!(!tracker.update(observed).ready);
    }

    #[test]
    fn assinatura_muda_quando_baseline_e_recalibrado_ou_removido() {
        let devices = vec![
            device("w", "ESP32-WROOM", "wroom"),
            device("c", "ESP32-C5", "esp32c5"),
        ];
        let mut baselines = HashMap::from([
            ("w".to_string(), baseline("wroom", 100)),
            ("c".to_string(), baseline("esp32c5", 100)),
        ]);
        let before = baseline_signature(&devices, &baselines);

        baselines.get_mut("w").unwrap().mean_amplitude = 24.0;
        let recalibrated = baseline_signature(&devices, &baselines);
        assert_ne!(before, recalibrated);

        baselines.remove("w");
        let removed = baseline_signature(&devices, &baselines);
        assert_ne!(recalibrated, removed);
    }

    #[test]
    fn assinatura_muda_quando_wroom_e_c5_sao_remapeados() {
        let devices = vec![
            device("w", "ESP32-WROOM", "wroom"),
            device("c", "ESP32-C5", "esp32c5"),
        ];
        let baselines = HashMap::from([
            ("w".to_string(), baseline("wroom", 100)),
            ("c".to_string(), baseline("esp32c5", 100)),
        ]);
        let before = baseline_signature(&devices, &baselines);

        let mut remapped = devices.clone();
        remapped[0].hardware_profile = "esp32c5".to_string();
        remapped[1].hardware_profile = "wroom".to_string();
        let after = baseline_signature(&remapped, &baselines);
        assert_ne!(before, after);
    }

    #[test]
    fn baselines_de_sessoes_ou_perfis_diferentes_bloqueiam_box() {
        let devices = vec![
            device("w", "ESP32-WROOM", "wroom"),
            device("c", "ESP32-C5", "esp32c5"),
        ];
        let histories = HashMap::from([
            ("w".to_string(), timed_history(0.1, 5.0)),
            ("c".to_string(), timed_history(0.1, 5.0)),
        ]);
        let mut baselines = HashMap::from([
            ("w".to_string(), baseline("wroom", 100)),
            ("c".to_string(), baseline("esp32c5", 100)),
        ]);

        baselines.get_mut("c").unwrap().calibration_session_id = Some("outra-sessao".to_string());
        let different_sessions = observe_at(&devices, &histories, &baselines, None, 5.0);
        assert!(!different_sessions.wroom.has_baseline);
        assert!(!different_sessions.c5.has_baseline);

        baselines.get_mut("c").unwrap().calibration_session_id =
            Some("test-empty-session".to_string());
        baselines.get_mut("c").unwrap().hardware_profile = "wroom".to_string();
        let wrong_profile = observe_at(&devices, &histories, &baselines, None, 5.0);
        assert!(!wrong_profile.wroom.has_baseline);
        assert!(!wrong_profile.c5.has_baseline);

        baselines.get_mut("c").unwrap().hardware_profile = "esp32c5".to_string();
        let old_pair_during_new_clear =
            observe_at(&devices, &histories, &baselines, Some("nova-sessao"), 5.0);
        assert!(!old_pair_during_new_clear.wroom.has_baseline);
        assert!(!old_pair_during_new_clear.c5.has_baseline);

        let required_pair = observe_at(
            &devices,
            &histories,
            &baselines,
            Some("test-empty-session"),
            5.0,
        );
        assert!(required_pair.wroom.has_baseline);
        assert!(required_pair.c5.has_baseline);
    }

    fn enter(tracker: &mut ShowerBoxTracker) -> ShowerBoxState {
        warm_up(tracker);
        let mut state = ShowerBoxState::default();
        for time in [5.1, 5.6, 6.1, 6.6, 7.1, 7.6] {
            state = tracker.update(observation(time, 0.30, 0.02));
        }
        state
    }

    #[test]
    fn reinicio_exige_aquecimento_estavel() {
        let mut tracker = ShowerBoxTracker::new();
        assert!(!tracker.update(observation(0.0, 0.30, 0.01)).ready);
        let before = tracker.update(observation(4.9, 0.30, 0.01));
        assert_eq!(before.phase, ShowerBoxPhase::Vacant);
        let ready = tracker.update(observation(5.0, 0.30, 0.01));
        assert!(ready.ready);
        assert_eq!(ready.phase, ShowerBoxPhase::Candidate);
    }

    #[test]
    fn warmup_concluido_publica_motivo_vago_coerente() {
        let mut tracker = ShowerBoxTracker::new();
        tracker.update(observation(0.0, 0.01, 0.01));
        let before = tracker.update(observation(4.9, 0.01, 0.01));
        assert!(!before.ready);
        assert_eq!(before.last_transition_reason, "startup_not_ready");

        let ready = tracker.update(observation(5.0, 0.01, 0.01));
        assert!(ready.ready);
        assert_eq!(ready.phase, ShowerBoxPhase::Vacant);
        assert_eq!(ready.last_transition_reason, "ready_vacant");
    }

    #[test]
    fn replay_nunca_ocupa_e_reseta_estado_ao_vivo() {
        let mut tracker = ShowerBoxTracker::new();
        let occupied = enter(&mut tracker);
        assert!(occupied.occupied);

        for time in [8.0, 11.0, 20.0] {
            let replay =
                tracker.update_for_stream(&StreamSource::CsvReplay, observation(time, 0.50, 0.01));
            assert!(!replay.occupied);
            assert!(!replay.ready);
            assert_eq!(replay.phase, ShowerBoxPhase::Vacant);
            assert_eq!(replay.last_transition_reason, "replay_blocked");
        }

        let live_again =
            tracker.update_for_stream(&StreamSource::UdpLive, observation(20.5, 0.50, 0.01));
        assert!(!live_again.ready);
        assert!(!live_again.occupied);
    }

    #[test]
    fn pulso_menor_que_dois_segundos_nao_ocupa() {
        let mut tracker = ShowerBoxTracker::new();
        warm_up(&mut tracker);
        tracker.update(observation(5.1, 0.30, 0.02));
        let state = tracker.update(observation(7.0, 0.02, 0.02));
        assert!(!state.occupied);
        assert_eq!(state.phase, ShowerBoxPhase::Vacant);
    }

    #[test]
    fn pulso_fisico_de_um_e_nove_segundos_nao_sobrevive_a_janela_curta() {
        let mut tracker = ShowerBoxTracker::new();
        tracker.update(physical_observation(5.0, 10.1, Some(12.0)));
        assert!(
            tracker
                .update(physical_observation(10.0, 10.1, Some(12.0)))
                .ready
        );

        for time in [10.5, 11.0, 11.5, 12.0, 12.5] {
            let state = tracker.update(physical_observation(time, 10.1, Some(12.0)));
            assert!(!state.occupied, "pulso curto ocupou em t={time}");
        }
    }

    #[test]
    fn pulso_fisico_desalinhado_de_um_e_nove_segundos_nao_ocupa() {
        let mut tracker = ShowerBoxTracker::new();
        tracker.update(physical_observation(5.0, 10.25, Some(12.15)));
        assert!(
            tracker
                .update(physical_observation(10.0, 10.25, Some(12.15)))
                .ready
        );

        for time in [10.5, 11.0, 11.5, 12.0, 12.5, 13.0] {
            let state = tracker.update(physical_observation(time, 10.25, Some(12.15)));
            assert!(!state.occupied, "pulso desalinhado ocupou em t={time}");
        }
    }

    #[test]
    fn presenca_fisica_continua_liga_dentro_de_tres_segundos() {
        let mut tracker = ShowerBoxTracker::new();
        tracker.update(physical_observation(5.0, 10.1, None));
        tracker.update(physical_observation(10.0, 10.1, None));

        let mut last = ShowerBoxState::default();
        for time in [10.5, 11.0, 11.5, 12.0, 12.5, 13.0] {
            last = tracker.update(physical_observation(time, 10.1, None));
        }
        assert!(last.occupied);
        assert_eq!(last.phase, ShowerBoxPhase::Occupied);
    }

    #[test]
    fn perturbacao_dominante_no_c5_nao_ocupa() {
        let mut tracker = ShowerBoxTracker::new();
        warm_up(&mut tracker);
        for time in [5.1, 6.0, 7.0, 8.0] {
            let state = tracker.update(observation(time, 0.25, 0.30));
            assert_eq!(state.phase, ShowerBoxPhase::Vacant);
        }
    }

    #[test]
    fn passagem_fora_do_box_nao_ocupa() {
        let mut tracker = ShowerBoxTracker::new();
        warm_up(&mut tracker);
        for time in [5.1, 6.0, 7.0, 8.0] {
            let state = tracker.update(observation(time, 0.10, 0.02));
            assert!(!state.occupied);
        }
    }

    #[test]
    fn entrada_continua_confirma_em_dois_segundos_e_meio() {
        let mut tracker = ShowerBoxTracker::new();
        let state = enter(&mut tracker);
        assert!(state.occupied);
        assert_eq!(state.phase, ShowerBoxPhase::Occupied);
        assert_eq!(state.last_transition_reason, "entry_confirmed");
    }

    #[test]
    fn auditoria_agregada_ordena_tentativa_confirmada_e_sessao() {
        let mut tracker = ShowerBoxTracker::new();
        warm_up(&mut tracker);

        let candidate = tracker.update(observation(5.1, 0.30, 0.02));
        assert_eq!(candidate.attempt_sequence, 1);
        assert_eq!(
            candidate.last_attempt_result,
            ShowerBoxAttemptResult::InProgress
        );
        let attempt_started = candidate
            .last_attempt_started_at_unix_ms
            .expect("candidate deve registrar início");
        assert!(candidate.last_attempt_finished_at_unix_ms.is_none());
        assert_eq!(candidate.session_sequence, 0);
        assert!(candidate.active_session_id.is_none());

        let mut confirmed = candidate;
        for time in [5.6, 6.1, 6.6, 7.1, 7.6] {
            confirmed = tracker.update(observation(time, 0.30, 0.02));
        }
        assert!(confirmed.occupied);
        assert_eq!(confirmed.attempt_sequence, 1);
        assert_eq!(
            confirmed.last_attempt_result,
            ShowerBoxAttemptResult::Confirmed
        );
        let attempt_finished = confirmed
            .last_attempt_finished_at_unix_ms
            .expect("confirmação deve finalizar tentativa");
        assert!(attempt_finished > attempt_started);
        assert_eq!(confirmed.last_attempt_peak_progress, 1.0);
        assert_eq!(confirmed.session_sequence, 1);
        assert!(confirmed
            .active_session_id
            .as_deref()
            .is_some_and(|id| id.starts_with("shower-box-1-")));
        let session_started = confirmed
            .last_session_started_at_unix_ms
            .expect("confirmação deve iniciar sessão");
        assert!(session_started > attempt_finished);
        assert!(confirmed.last_session_ended_at_unix_ms.is_none());
        assert!(confirmed.last_session_end_reason.is_none());

        let ended = tracker.update(observation(67.7, 0.01, 0.01));
        assert!(!ended.occupied);
        assert_eq!(ended.session_sequence, 1);
        assert!(ended.active_session_id.is_none());
        assert_eq!(
            ended.last_session_end_reason.as_deref(),
            Some("hold_expired")
        );
        assert!(ended
            .last_session_ended_at_unix_ms
            .is_some_and(|finished| finished > session_started));
    }

    #[test]
    fn auditoria_distingue_tentativa_rejeitada_de_entrada_nao_pronta() {
        let mut tracker = ShowerBoxTracker::new();
        warm_up(&mut tracker);

        tracker.update(observation(5.1, 0.30, 0.02));
        for time in [5.6, 6.1] {
            tracker.update(observation(time, 0.30, 0.02));
        }
        let rejected = tracker.update(observation(12.0, 0.01, 0.01));
        assert_eq!(rejected.attempt_sequence, 1);
        assert_eq!(
            rejected.last_attempt_result,
            ShowerBoxAttemptResult::Rejected
        );
        assert!(rejected.last_attempt_peak_progress > 0.0);
        let first_finished = rejected
            .last_attempt_finished_at_unix_ms
            .expect("rejeição deve finalizar tentativa");

        let second = tracker.update(observation(12.1, 0.30, 0.02));
        assert_eq!(second.attempt_sequence, 2);
        assert_eq!(
            second.last_attempt_result,
            ShowerBoxAttemptResult::InProgress
        );
        assert!(second
            .last_attempt_started_at_unix_ms
            .is_some_and(|started| started > first_finished));

        let not_ready = tracker.update(unavailable(12.2));
        assert_eq!(not_ready.attempt_sequence, 2);
        assert_eq!(
            not_ready.last_attempt_result,
            ShowerBoxAttemptResult::NotReady
        );
        assert!(not_ready.last_attempt_finished_at_unix_ms.is_some());
        assert_eq!(not_ready.session_sequence, 0);
    }

    #[test]
    fn ajuste_rejeita_candidate_sem_perder_warmup_ou_piso_adaptativo() {
        let mut tracker = ShowerBoxTracker::new();
        warm_up(&mut tracker);
        for index in 0..5 {
            tracker.update(uncorroborated_observation(
                5.1 + index as f64 * 0.5,
                0.02,
                0.01,
            ));
        }
        let candidate = tracker.update(observation(8.0, 0.30, 0.02));
        assert_eq!(candidate.phase, ShowerBoxPhase::Candidate);
        let healthy_since = tracker.healthy_since_s;
        let empty_samples = tracker.empty_wroom_scores.len();

        tracker.apply_settings_change();
        assert_eq!(tracker.phase, ShowerBoxPhase::Vacant);
        assert_eq!(tracker.healthy_since_s, healthy_since);
        assert_eq!(tracker.empty_wroom_scores.len(), empty_samples);
        assert!(tracker.entry_votes.is_empty());
        assert_eq!(
            tracker.last_attempt_result,
            ShowerBoxAttemptResult::Rejected
        );
        assert_eq!(
            tracker.last_transition_reason,
            "settings_applied_candidate_rejected"
        );

        let next_candidate = tracker.update(observation(8.1, 0.30, 0.02));
        assert!(next_candidate.ready, "ajuste não deve reiniciar warm-up");
        assert_eq!(next_candidate.phase, ShowerBoxPhase::Candidate);
        assert_eq!(next_candidate.attempt_sequence, 2);
        assert_eq!(
            tracker.entry_votes.len(),
            1,
            "votos antigos não podem vazar"
        );
    }

    #[test]
    fn ajuste_preserva_sessao_ocupada_e_holding() {
        let mut tracker = ShowerBoxTracker::new();
        let occupied = enter(&mut tracker);
        let session_id = occupied
            .active_session_id
            .clone()
            .expect("entrada confirmada deve ter sessão");
        let last_evidence = tracker.last_evidence_s;

        tracker.apply_settings_change();
        assert_eq!(tracker.phase, ShowerBoxPhase::Occupied);
        assert_eq!(tracker.last_evidence_s, last_evidence);
        assert_eq!(
            tracker.active_session_id.as_deref(),
            Some(session_id.as_str())
        );
        assert_eq!(tracker.session_sequence, 1);
        assert!(tracker.continuous_evidence_since_s.is_none());
        assert_eq!(
            tracker.last_transition_reason,
            "settings_applied_session_preserved"
        );

        let holding = tracker.update(observation(8.0, 0.01, 0.01));
        assert_eq!(holding.phase, ShowerBoxPhase::Holding);
        tracker.apply_settings_change();
        assert_eq!(tracker.phase, ShowerBoxPhase::Holding);
        assert_eq!(tracker.last_evidence_s, last_evidence);
        assert_eq!(
            tracker.active_session_id.as_deref(),
            Some(session_id.as_str())
        );
        assert_eq!(tracker.session_sequence, 1);
        assert_eq!(
            tracker.last_transition_reason,
            "settings_applied_session_preserved"
        );
    }

    #[test]
    fn um_tick_fraco_nao_zera_a_votacao_de_entrada() {
        let mut tracker = ShowerBoxTracker::new();
        warm_up(&mut tracker);
        assert_eq!(
            tracker.update(observation(5.1, 0.30, 0.02)).phase,
            ShowerBoxPhase::Candidate
        );
        tracker.update(observation(5.605, 0.30, 0.02));
        tracker.update(observation(6.110, 0.30, 0.02));
        let dip = tracker.update(observation(6.615, 0.02, 0.02));
        assert_eq!(dip.phase, ShowerBoxPhase::Candidate);
        tracker.update(observation(7.120, 0.30, 0.02));
        let confirmed = tracker.update(observation(7.625, 0.30, 0.02));
        assert!(confirmed.occupied);
        assert_eq!(confirmed.phase, ShowerBoxPhase::Occupied);
    }

    #[test]
    fn rajada_forte_sem_confirmacao_independente_nao_renova_hold() {
        let mut tracker = ShowerBoxTracker::new();
        enter(&mut tracker);
        let recovery = tracker.update(uncorroborated_observation(8.0, 0.14, 0.01));
        assert!(recovery.occupied);
        assert!(!recovery.hold_evidence);
        let renewed = tracker.update(uncorroborated_observation(10.1, 0.14, 0.01));
        assert_eq!(renewed.phase, ShowerBoxPhase::Holding);
        assert_eq!(renewed.last_transition_reason, "weak_signal_holding");
        assert!(renewed.hold_seconds_remaining < EXIT_HOLD_SECONDS);
    }

    #[test]
    fn confirmacao_independente_no_limite_do_hold_recebe_graca_curta() {
        let mut tracker = ShowerBoxTracker::new();
        enter(&mut tracker);
        assert!(tracker.update(unavailable(67.5)).occupied);

        let at_deadline = tracker.update(observation(67.7, 0.14, 0.01));
        assert!(at_deadline.occupied);
        assert_eq!(at_deadline.phase, ShowerBoxPhase::Holding);
        let renewed = tracker.update(observation(69.8, 0.14, 0.01));
        assert!(renewed.occupied);
        assert_eq!(renewed.phase, ShowerBoxPhase::Occupied);
    }

    #[test]
    fn rajada_forte_sem_presenca_nao_abre_no_modo_seguro() {
        let mut tracker = ShowerBoxTracker::new();
        warm_up(&mut tracker);
        for time in [5.1, 6.0, 7.0, 8.0, 9.0] {
            let state = tracker.update(uncorroborated_observation(time, 0.30, 0.01));
            assert!(!state.occupied);
            assert_eq!(state.phase, ShowerBoxPhase::Vacant);
        }
    }

    #[test]
    fn corroboracao_estatica_sozinha_nao_cria_entrada() {
        let mut tracker = ShowerBoxTracker::new();
        warm_up(&mut tracker);
        for time in [5.1, 5.6, 6.1, 6.6, 7.1, 7.6, 8.1] {
            let state = tracker.update(static_only_observation(time, 0.30, 0.01));
            assert!(!state.occupied);
            assert_eq!(state.phase, ShowerBoxPhase::Vacant);
        }
    }

    #[test]
    fn fallback_sem_auxilio_de_presenca_e_explicitamente_configuravel() {
        let mut tracker = ShowerBoxTracker::new();
        let mut settings = ShowerBoxSettings::default();
        settings.presence_assist = false;
        tracker.update_with_settings(observation(0.0, 0.02, 0.02), &settings);
        tracker.update_with_settings(observation(5.0, 0.02, 0.02), &settings);
        let mut occupied = ShowerBoxState::default();
        for time in [5.1, 5.6, 6.1, 6.6, 7.1, 7.6] {
            occupied = tracker
                .update_with_settings(uncorroborated_observation(time, 0.30, 0.01), &settings);
        }
        assert!(occupied.occupied);
    }

    #[test]
    fn duas_amostras_esparsas_nao_simulam_presenca_continua() {
        let mut tracker = ShowerBoxTracker::new();
        warm_up(&mut tracker);
        assert_eq!(
            tracker.update(observation(5.1, 0.30, 0.01)).phase,
            ShowerBoxPhase::Candidate
        );
        let sparse = tracker.update(observation(7.6, 0.30, 0.01));
        assert!(!sparse.occupied);
        assert_eq!(sparse.phase, ShowerBoxPhase::Candidate);
        assert!(sparse.entry_progress < 0.5);
    }

    #[test]
    fn evidencia_fraca_de_imobilidade_renova_o_hold() {
        let mut tracker = ShowerBoxTracker::new();
        enter(&mut tracker);
        for time in [20.0, 50.0, 80.0, 140.0] {
            let state = tracker.update(observation(time, 0.09, 0.01));
            assert!(state.occupied);
            assert_eq!(state.phase, ShowerBoxPhase::Occupied);
        }
    }

    #[test]
    fn picos_isolados_nao_renovam_o_hold() {
        let mut tracker = ShowerBoxTracker::new();
        enter(&mut tracker);
        tracker.update(observation(8.0, 0.01, 0.01));

        for time in [20.0, 30.0, 40.0, 50.0, 60.0, 66.0] {
            let spike = tracker.update(observation(time, 0.09, 0.01));
            assert!(spike.occupied, "hold encerrou durante o pico em t={time}");
            let quiet = tracker.update(observation(time + 0.1, 0.01, 0.01));
            assert!(quiet.occupied, "hold encerrou cedo em t={time}");
        }

        let off = tracker.update(observation(67.7, 0.01, 0.01));
        assert!(!off.occupied);
        assert_eq!(off.phase, ShowerBoxPhase::Vacant);
        assert_eq!(off.last_transition_reason, "hold_expired");
    }

    #[test]
    fn ruido_fraco_sem_apoio_nao_renova_o_hold() {
        let mut tracker = ShowerBoxTracker::new();
        enter(&mut tracker);

        for time in [8.0, 20.0, 40.0, 60.0, 67.7] {
            let state = tracker.update(uncorroborated_observation(time, 0.08, 0.02));
            if time < 67.6 {
                assert!(state.occupied, "hold encerrou cedo em t={time}");
                assert_eq!(state.phase, ShowerBoxPhase::Holding);
            } else {
                assert!(!state.occupied, "ruído não corroborado renovou o hold");
                assert_eq!(state.phase, ShowerBoxPhase::Vacant);
            }
        }
    }

    #[test]
    fn perda_breve_de_sinal_nao_desocupa() {
        let mut tracker = ShowerBoxTracker::new();
        enter(&mut tracker);
        let state = tracker.update(unavailable(67.0));
        assert!(state.occupied);
        assert_eq!(state.phase, ShowerBoxPhase::Holding);
        assert!(!state.ready);
    }

    #[test]
    fn evidencia_que_volta_no_segundo_cinquenta_e_nove_renova_hold() {
        let mut tracker = ShowerBoxTracker::new();
        enter(&mut tracker);
        let lost = tracker.update(unavailable(66.0));
        assert!(lost.occupied);
        assert_eq!(lost.phase, ShowerBoxPhase::Holding);

        // O receptor voltou saudável antes do fim do hold, mas `ready` ainda
        // está em warm-up. A recuperação ganha uma curta tolerância para
        // confirmar 2,5 s contínuos, sem aceitar um único pico.
        for time in [66.1, 67.0, 68.0] {
            let pending = tracker.update(observation(time, 0.09, 0.01));
            assert!(pending.occupied);
            assert_eq!(pending.phase, ShowerBoxPhase::Holding);
        }
        let recovered = tracker.update(observation(68.6, 0.09, 0.01));
        assert!(recovered.occupied);
        assert_eq!(recovered.phase, ShowerBoxPhase::Occupied);
        assert!(!recovered.ready);
        assert_eq!(recovered.hold_seconds_remaining, EXIT_HOLD_SECONDS);
    }

    #[test]
    fn ausencia_do_wroom_nunca_liga() {
        let mut tracker = ShowerBoxTracker::new();
        for time in [0.0, 5.0, 10.0] {
            assert!(!tracker.update(unavailable(time)).occupied);
        }
    }

    #[test]
    fn desliga_uma_vez_apos_sessenta_segundos_sem_evidencia() {
        let mut tracker = ShowerBoxTracker::new();
        enter(&mut tracker);
        let holding = tracker.update(observation(67.5, 0.01, 0.01));
        assert!(holding.occupied);
        let off = tracker.update(observation(67.7, 0.01, 0.01));
        assert!(!off.occupied);
        assert_eq!(off.last_transition_reason, "hold_expired");
        let still_off = tracker.update(observation(80.0, 0.01, 0.01));
        assert!(!still_off.occupied);
        assert_eq!(still_off.last_transition_reason, "hold_expired");
    }

    #[test]
    fn aprendizado_so_muda_durante_vago_confiavel() {
        let mut tracker = ShowerBoxTracker::new();
        warm_up(&mut tracker);
        for index in 0..45 {
            tracker.update(uncorroborated_observation(
                5.5 + index as f64 * 0.5,
                0.06,
                0.01,
            ));
        }
        let learned = tracker.update(uncorroborated_observation(30.0, 0.06, 0.01));
        assert!(learned.on_threshold >= 0.08);
        let threshold = learned.on_threshold;
        let empty_samples = tracker.empty_wroom_scores.len();
        let mut entered = ShowerBoxState::default();
        for time in [30.1, 30.6, 31.1, 31.6, 32.1, 32.6] {
            entered = tracker.update(observation(time, 0.30, 0.01));
        }
        assert!(entered.occupied);
        for index in 0..100 {
            tracker.update(observation(33.0 + index as f64, 0.30, 0.01));
        }
        let occupied = tracker.update(observation(140.0, 0.30, 0.01));
        assert_eq!(occupied.on_threshold, threshold);
        assert_eq!(tracker.empty_wroom_scores.len(), empty_samples);
    }

    #[test]
    fn aprendizado_vazio_ignora_amostras_com_presenca_ou_corrobacao() {
        let mut tracker = ShowerBoxTracker::new();
        warm_up(&mut tracker);
        for index in 0..20 {
            tracker.update(observation(5.5 + index as f64 * 0.5, 0.02, 0.01));
        }
        assert!(tracker.empty_wroom_scores.is_empty());
    }
}
