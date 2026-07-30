export type DeviceRole = 'receiver' | 'injector' | 'router' | string;
export type HardwareProfile = 'esp32c5' | 'wroom' | 'classic' | 'router' | string;
export type StreamSource = 'idle' | 'udp_live' | 'csv_replay';

// Config MQTT/Home Assistant devolvida por GET /api/mqtt/settings (senha nunca
// vem — só `password_set`).
export interface MqttSettingsView {
  enabled: boolean;
  host: string;
  port: number;
  username: string | null;
  password_set: boolean;
  client_id: string;
  base_topic: string;
  discovery_prefix: string;
  retain: boolean;
  publish_interval_ms: number;
  keep_alive_secs: number;
  connected: boolean;
}

export interface StreamStatus {
  source: StreamSource;
  replay_active: boolean;
  replay_file?: string | null;
  reason?: string;
}

export interface LocationEstimate {
  lat: number;
  lng: number;
  floor?: number;
  accuracy_m: number;
  confidence?: number;
  accuracy_dm?: number;
  nodes_used?: number;
  method?: string;
  experimental?: boolean;
  resolution_m?: number;
  /** Cômodo (objeto `room` da planta) que contém a estimativa — planta funcional. */
  current_room?: string | null;
  /** true quando a estimativa foi confinada aos bounds do floorplan (geofence). */
  clamped_to_floorplan?: boolean;
  /** Paredes consideradas na correção de atenuação RSSI→distância. */
  walls_crossed?: number;
}

/** Pessoa individual decodificada do heatmap do modelo ONNX.
 *  vitals_scope === 'individual' apenas com 1 pessoa; com 2+ a taxa é a
 *  estimativa global do ambiente (datasets públicos não têm GT por pessoa). */
export interface PersonEstimate {
  x_norm: number;
  y_norm: number;
  lat?: number;
  lng?: number;
  confidence: number;
  breathing_rate?: number;
  heart_rate_bpm?: number;
  vitals_scope: 'individual' | 'global' | string;
  vitals_source: string;
}

/** Estimativa ML (OccupancyHeatmapNet via ONNX), lado a lado com a heurística. */
export interface OccupancyEstimate {
  available: boolean;
  count: number;
  probabilities: number[];
  confidence: number;
  method: string;
  persons: PersonEstimate[];
  vitals_ml?: { heart_rate_bpm?: number; breathing_rate?: number; confidence?: number };
  model_run_id?: string;
  inference_ms?: number;
  /** A contagem de pessoas foi calibrada para esta sala? O modelo WiMANS é
   *  zero-shot (treinado fora do ambiente), então a contagem é uma ESTIMATIVA
   *  não calibrada quando `false`. `undefined` = backend antigo (não informa). */
  count_calibrated?: boolean;
}

export interface FftDiagnostics {
  has_presence: boolean;
  sample_count: number;
  sample_rate_hz: number;
  breathing_peak_hz?: number;
  breathing_prominence: number;
  heart_peak_hz?: number;
  heart_snr: number;
  rejected_harmonics: number[];
  spectrum_magnitudes: number[];
  spectrum_freqs: number[];
  status_message: string;
}

export type ShowerBoxProfile = 'comfort' | 'balanced' | 'precise' | 'custom';
export type ShowerControlMode = 'automatic' | 'assisted' | 'manual';
export type ShowerBoxPhase = 'vacant' | 'candidate' | 'occupied' | 'holding' | string;
export type ShowerBoxAttemptResult =
  | 'idle'
  | 'in_progress'
  | 'confirmed'
  | 'rejected'
  | 'not_ready';
export type NumericRange = readonly [number, number];
export interface NumericLimit {
  min: number;
  max: number;
  step?: number;
}
export type NumericLimitValue = NumericRange | NumericLimit;

export interface ShowerBoxSettings {
  profile: ShowerBoxProfile;
  control_mode: ShowerControlMode;
  entry_confirm_seconds: number;
  entry_decay_seconds: number;
  exit_hold_seconds: number;
  recovery_confirm_seconds: number;
  sensitivity: number;
  presence_assist: boolean;
}

/** Limites devolvidos pelo backend. Os aliases curtos mantêm compatibilidade
 * com as primeiras versões do contrato do painel. */
export interface ShowerBoxSettingsLimits {
  confirm?: NumericLimitValue;
  decay?: NumericLimitValue;
  hold?: NumericLimitValue;
  recovery?: NumericLimitValue;
  sensitivity?: NumericLimitValue;
  entry_confirm_seconds?: NumericLimitValue;
  entry_decay_seconds?: NumericLimitValue;
  exit_hold_seconds?: NumericLimitValue;
  recovery_confirm_seconds?: NumericLimitValue;
}

export interface ShowerBoxSettingsResponse {
  ok: boolean;
  settings: ShowerBoxSettings;
  limits: ShowerBoxSettingsLimits;
  message?: string;
}

/** Decisão ao vivo do tracker dedicado ao box. Ela representa a intenção da
 * automação; o estado físico da tomada continua sendo responsabilidade do HA. */
export interface ShowerBoxState {
  occupied: boolean;
  tracker_state: ShowerBoxPhase;
  ready: boolean;
  wroom_available: boolean;
  c5_available: boolean;
  wroom_has_baseline: boolean;
  c5_has_baseline: boolean;
  wroom_sample_rate_hz: number;
  c5_sample_rate_hz: number;
  wroom_score: number;
  c5_score: number;
  dominance: number;
  on_threshold: number;
  off_threshold: number;
  dominance_on_threshold: number;
  dominance_off_threshold: number;
  hold_corroborated: boolean;
  hold_seconds_remaining: number;
  last_transition_reason: string;
  entry_progress?: number;
  entry_seconds_remaining?: number;
  warmup_seconds_remaining?: number;
  presence_assisted?: boolean;
  hold_evidence?: boolean;
  profile?: ShowerBoxProfile;
  control_mode?: ShowerControlMode;
  configured_entry_confirm_seconds?: number;
  configured_entry_decay_seconds?: number;
  configured_exit_hold_seconds?: number;
  configured_recovery_confirm_seconds?: number;
  sensitivity?: number;
  presence_assist_enabled?: boolean;
  /** Diagnóstico agregado da tentativa mais recente. Todos são opcionais para
   * que o dashboard continue funcionando durante upgrade gradual do backend. */
  attempt_sequence?: number;
  last_attempt_started_at_unix_ms?: number | null;
  last_attempt_finished_at_unix_ms?: number | null;
  last_attempt_result?: ShowerBoxAttemptResult;
  /** Maior progresso de confirmação da tentativa, normalizado em 0..1. */
  last_attempt_peak_progress?: number;
  /** Diagnóstico agregado de sessões; não contém CSI, trajetória ou identidade. */
  session_sequence?: number;
  active_session_id?: string | null;
  last_session_started_at_unix_ms?: number | null;
  last_session_ended_at_unix_ms?: number | null;
  last_session_end_reason?: string | null;
}

export interface CSIPacket {
  count?: number;
  rssi?: number;
  rate?: number;
  subcarriers?: number;
  padding?: number;
  amplitudes?: number[][];
  heatmap?: number[][];
  stft?: { x: number[]; y: number[]; z: number[][] };
  phase_raw?: number[];
  phase_sanitized?: number[];
  pca?: { packet_axis: number[]; pc1: number[]; pc2: number[] };
  pdp?: { x: number[]; y: number[] };
  detection?: {
    presence?: {
      detected: boolean;
      confidence: number;
      status?: 'ACTIVE' | 'HOLDING_STATIC' | 'EMPTY' | string;
      hold_seconds_remaining?: number;
    };
    movement?: { level: 'static' | 'slight_movement' | 'walking' | 'running'; velocity_estimate?: number };
    vitals?: { heart_rate_bpm?: number; breathing_rate?: number; confidence?: number };
    location?: LocationEstimate;
    occupancy?: OccupancyEstimate;
    fft_diagnostics?: FftDiagnostics;
    shower_box?: ShowerBoxState;
  };
  devices?: ESPDevice[];
  footer_text?: string;
  stream?: StreamStatus;
}

export interface ESPDevice {
  id: string;
  name: string;
  lat: number;
  lng: number;
  floor: number;
  icon?: string;
  size?: number;
  role?: DeviceRole;
  hardware_profile?: HardwareProfile;
  /** Canal WiFi e banda REAL observados ao vivo (do CSI), quando transmitindo. */
  channel?: number;
  band?: string;
}

export interface FloorplanBounds {
  north: number;
  south: number;
  east: number;
  west: number;
}

export interface FloorplanConfig {
  id: string;
  name: string;
  floor: number;
  image_data_url: string;
  bounds: FloorplanBounds;
  opacity: number;
}

export interface EnvironmentObjectConfig {
  id: string;
  name: string;
  type: string;
  lat: number;
  lng: number;
  floor: number;
  width_m: number;
  depth_m: number;
  rotation_deg: number;
  calibration_relevant: boolean;
}

export interface MapConfig {
  floorplans: FloorplanConfig[];
  environment_objects: EnvironmentObjectConfig[];
}

export interface CSISettings {
  heatmap_window: number;
  stft_window: number;
  pca_window: number;
  padding: number;
  topology_scenario?: number;
  vitals_window_seconds?: number;
  vitals_min_samples?: number;
  vitals_peak_prominence?: number;
  heart_rate_abs_snr_floor?: number;
  vitals_harmonic_count?: number;
  vitals_harmonic_tolerance_hz?: number;
  vitals_subcarrier_top_k?: number;
  breathing_freq_min_hz?: number;
  breathing_freq_max_hz?: number;
  heart_rate_freq_min_hz?: number;
  heart_rate_freq_max_hz?: number;
}
