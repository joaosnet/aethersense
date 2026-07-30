import { useCallback, useEffect, useMemo, useState } from 'react';
import {
  CheckCircle2,
  ChevronDown,
  Circle,
  Gauge,
  Hand,
  LoaderCircle,
  RefreshCw,
  RotateCcw,
  Save,
  Settings2,
  ShieldCheck,
  ShowerHead,
  Sparkles,
  Timer,
  Waves,
  Zap,
} from 'lucide-react';
import { useTranslation } from 'react-i18next';
import type {
  NumericRange,
  NumericLimitValue,
  ShowerBoxAttemptResult,
  ShowerBoxPhase,
  ShowerBoxProfile,
  ShowerBoxSettings,
  ShowerBoxSettingsLimits,
  ShowerBoxState,
  ShowerControlMode,
} from '../types/csi';
import {
  BALANCED_SHOWER_BOX_SETTINGS,
  DEFAULT_SHOWER_BOX_LIMITS,
  fetchShowerBoxSettings,
  updateShowerBoxSettings,
} from '../utils/showerBoxApi';

interface Props {
  apiBaseUrl: string;
  showerBox?: ShowerBoxState;
  isStreamLive: boolean;
}

interface PresetDefinition {
  id: Exclude<ShowerBoxProfile, 'custom'>;
  icon: typeof Sparkles;
  settings: Omit<ShowerBoxSettings, 'control_mode'>;
}

interface GateDefinition {
  id: string;
  label: string;
  detail: string;
  complete: boolean;
}

type NumericSettingKey =
  | 'entry_confirm_seconds'
  | 'entry_decay_seconds'
  | 'exit_hold_seconds'
  | 'recovery_confirm_seconds'
  | 'sensitivity';

type Feedback = { tone: 'success' | 'error' | 'neutral'; message: string };

const PRESETS: PresetDefinition[] = [
  {
    id: 'comfort',
    icon: Sparkles,
    settings: {
      profile: 'comfort',
      entry_confirm_seconds: 2,
      entry_decay_seconds: 2,
      exit_hold_seconds: 180,
      recovery_confirm_seconds: 1.5,
      sensitivity: 4,
      presence_assist: true,
    },
  },
  {
    id: 'balanced',
    icon: ShieldCheck,
    settings: {
      profile: 'balanced',
      entry_confirm_seconds: 2.5,
      entry_decay_seconds: 1.5,
      exit_hold_seconds: 60,
      recovery_confirm_seconds: 2,
      sensitivity: 4,
      presence_assist: true,
    },
  },
  {
    id: 'precise',
    icon: Timer,
    settings: {
      profile: 'precise',
      entry_confirm_seconds: 4,
      entry_decay_seconds: 0.75,
      exit_hold_seconds: 60,
      recovery_confirm_seconds: 3,
      sensitivity: 2,
      presence_assist: true,
    },
  },
];

const clamp01 = (value: number) => Math.max(0, Math.min(1, value));

const formatSeconds = (value: number | undefined) => {
  if (value === undefined || !Number.isFinite(value)) return '—';
  const rounded = Math.max(0, value);
  return `${Number.isInteger(rounded) ? rounded.toFixed(0) : rounded.toFixed(1)} s`;
};

const validSequence = (value: number | undefined): number => (
  typeof value === 'number' && Number.isFinite(value) && value > 0
    ? Math.floor(value)
    : 0
);

const validTimestamp = (value: number | null | undefined): number | null => (
  typeof value === 'number' && Number.isFinite(value) && value > 0
    ? value
    : null
);

const formatEventTimestamp = (
  timestamp: number | null,
  nowMs: number,
  locale: string,
): string | null => {
  if (timestamp === null) return null;

  const date = new Date(timestamp);
  if (Number.isNaN(date.getTime())) return null;

  const ageSeconds = Math.max(0, Math.floor((nowMs - timestamp) / 1_000));
  const relative = new Intl.RelativeTimeFormat(locale, { numeric: 'auto' });
  let relativeLabel: string;
  if (ageSeconds < 60) {
    relativeLabel = relative.format(-ageSeconds, 'second');
  } else if (ageSeconds < 3_600) {
    relativeLabel = relative.format(-Math.floor(ageSeconds / 60), 'minute');
  } else if (ageSeconds < 86_400) {
    relativeLabel = relative.format(-Math.floor(ageSeconds / 3_600), 'hour');
  } else {
    relativeLabel = relative.format(-Math.floor(ageSeconds / 86_400), 'day');
  }

  const sameDay = new Date(nowMs).toDateString() === date.toDateString();
  const absolute = new Intl.DateTimeFormat(locale, sameDay
    ? { hour: '2-digit', minute: '2-digit' }
    : { day: '2-digit', month: 'short', hour: '2-digit', minute: '2-digit' })
    .format(date);
  return `${absolute} · ${relativeLabel}`;
};

const humanizeReason = (reason: string): string => {
  const normalized = reason.trim().replaceAll('_', ' ').replaceAll('-', ' ');
  if (!normalized) return '';
  return normalized.charAt(0).toLocaleUpperCase() + normalized.slice(1);
};

const ATTEMPT_RESULTS = new Set<ShowerBoxAttemptResult>([
  'idle',
  'in_progress',
  'confirmed',
  'rejected',
  'not_ready',
]);

const normalizeAttemptResult = (
  value: unknown,
  phase: ShowerBoxPhase,
): ShowerBoxAttemptResult => {
  if (typeof value === 'string' && ATTEMPT_RESULTS.has(value as ShowerBoxAttemptResult)) {
    return value as ShowerBoxAttemptResult;
  }
  return phase === 'candidate' ? 'in_progress' : 'idle';
};

const settingsEqual = (left: ShowerBoxSettings, right: ShowerBoxSettings) => (
  JSON.stringify(left) === JSON.stringify(right)
);

const limitFor = (
  limits: ShowerBoxSettingsLimits,
  fullKey: keyof ShowerBoxSettingsLimits,
  shortKey: keyof ShowerBoxSettingsLimits,
  fallback: NumericRange,
): NumericRange => {
  const value: NumericLimitValue = limits[fullKey] ?? limits[shortKey] ?? fallback;
  if (Array.isArray(value)) return [value[0], value[1]];
  const objectLimit = value as { min: number; max: number };
  return [objectLimit.min, objectLimit.max];
};

const limitStepFor = (
  limits: ShowerBoxSettingsLimits,
  fullKey: keyof ShowerBoxSettingsLimits,
  shortKey: keyof ShowerBoxSettingsLimits,
  fallback: number,
) => {
  const value = limits[fullKey] ?? limits[shortKey];
  if (!value || Array.isArray(value)) return fallback;
  return (value as { step?: number }).step ?? fallback;
};

const statusTone = (
  showerBox: ShowerBoxState | undefined,
  isStreamLive: boolean,
): 'ready' | 'candidate' | 'active' | 'holding' | 'blocked' => {
  if (!showerBox || !isStreamLive || !showerBox.ready) return 'blocked';
  if (showerBox.tracker_state === 'candidate') return 'candidate';
  if (showerBox.tracker_state === 'holding') return 'holding';
  if (showerBox.occupied) return 'active';
  return 'ready';
};

export function ShowerBoxControlPanel({ apiBaseUrl, showerBox, isStreamLive }: Props) {
  const { t, i18n } = useTranslation();
  const [savedSettings, setSavedSettings] = useState<ShowerBoxSettings | null>(null);
  const [draft, setDraft] = useState<ShowerBoxSettings>(BALANCED_SHOWER_BOX_SETTINGS);
  const [limits, setLimits] = useState<ShowerBoxSettingsLimits>(DEFAULT_SHOWER_BOX_LIMITS);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [feedback, setFeedback] = useState<Feedback | null>(null);
  const [nowMs, setNowMs] = useState(() => Date.now());

  const loadSettings = useCallback(async () => {
    setLoading(true);
    setFeedback(null);
    try {
      const response = await fetchShowerBoxSettings(apiBaseUrl);
      setSavedSettings(response.settings);
      setDraft(response.settings);
      setLimits(response.limits);
    } catch (error) {
      setFeedback({
        tone: 'error',
        message: error instanceof Error
          ? error.message
          : t('showerBox.settings.loadError', 'Não foi possível carregar os ajustes da zona.'),
      });
    } finally {
      setLoading(false);
    }
  }, [apiBaseUrl, t]);

  useEffect(() => {
    void loadSettings();
  }, [loadSettings]);

  useEffect(() => {
    const timer = window.setInterval(() => setNowMs(Date.now()), 10_000);
    return () => window.clearInterval(timer);
  }, []);

  const phase: ShowerBoxPhase = showerBox?.tracker_state ?? 'vacant';
  const tone = statusTone(showerBox, isStreamLive);
  const entryProgress = clamp01(
    showerBox?.entry_progress
      ?? (phase === 'candidate' ? 0 : showerBox?.occupied ? 1 : 0),
  );
  const warmupRemaining = showerBox?.warmup_seconds_remaining;
  const wroomRate = showerBox?.wroom_sample_rate_hz ?? 0;
  const c5Rate = showerBox?.c5_sample_rate_hz ?? 0;
  const locale = i18n.resolvedLanguage ?? i18n.language ?? 'pt-BR';

  const attemptSequence = validSequence(showerBox?.attempt_sequence);
  const attemptResult = normalizeAttemptResult(showerBox?.last_attempt_result, phase);
  const attemptStartedAt = validTimestamp(showerBox?.last_attempt_started_at_unix_ms);
  const attemptFinishedAt = validTimestamp(showerBox?.last_attempt_finished_at_unix_ms);
  const attemptTimestamp = attemptResult === 'in_progress'
    ? attemptStartedAt
    : attemptFinishedAt ?? attemptStartedAt;
  const hasAttemptHistoryContract = showerBox?.attempt_sequence !== undefined
    || showerBox?.last_attempt_result !== undefined;
  const hasAttemptEvent = attemptSequence > 0
    || attemptTimestamp !== null
    || attemptResult !== 'idle';
  const attemptPeakRaw = showerBox?.last_attempt_peak_progress;
  const attemptPeak = hasAttemptEvent && typeof attemptPeakRaw === 'number' && Number.isFinite(attemptPeakRaw)
    ? clamp01(attemptPeakRaw)
    : attemptResult === 'in_progress' && phase === 'candidate'
      ? entryProgress
      : null;
  const attemptPeakPercent = attemptPeak === null ? null : Math.round(attemptPeak * 100);
  const attemptTimeText = formatEventTimestamp(attemptTimestamp, nowMs, locale);
  const attemptPresentation: Record<ShowerBoxAttemptResult, {
    title: string;
    detail: string;
    tone: 'neutral' | 'candidate' | 'success' | 'blocked';
    icon: typeof Timer;
  }> = {
    idle: {
      title: hasAttemptHistoryContract
        ? t('showerBox.activity.attempt.idle', 'Aguardando uma chegada')
        : t('showerBox.activity.legacy', 'Histórico disponível após atualizar'),
      detail: hasAttemptHistoryContract
        ? t('showerBox.activity.attempt.idleHelp', 'A próxima aproximação aparecerá aqui.')
        : t('showerBox.activity.legacyHelp', 'A detecção ao vivo continua funcionando normalmente.'),
      tone: 'neutral',
      icon: Timer,
    },
    in_progress: {
      title: t('showerBox.activity.attempt.inProgress', 'Confirmando sua chegada'),
      detail: t('showerBox.activity.attempt.inProgressHelp', 'A missão avança enquanto a evidência permanece estável.'),
      tone: 'candidate',
      icon: LoaderCircle,
    },
    confirmed: {
      title: t('showerBox.activity.attempt.confirmed', 'Entrada confirmada'),
      detail: t('showerBox.activity.attempt.confirmedHelp', 'A zona reconheceu uma entrada válida no box.'),
      tone: 'success',
      icon: CheckCircle2,
    },
    rejected: {
      title: t('showerBox.activity.attempt.rejected', 'Sinal breve ignorado'),
      detail: t('showerBox.activity.attempt.rejectedHelp', 'A confirmação terminou antes de abrir uma sessão.'),
      tone: 'neutral',
      icon: ShieldCheck,
    },
    not_ready: {
      title: t('showerBox.activity.attempt.notReady', 'Entrada bloqueada com segurança'),
      detail: t('showerBox.activity.attempt.notReadyHelp', 'Os sensores ainda não estavam prontos para decidir.'),
      tone: 'blocked',
      icon: ShieldCheck,
    },
  };
  const latestAttempt = attemptResult === 'rejected'
    && showerBox?.last_transition_reason === 'settings_applied_candidate_rejected'
    ? {
        title: t('showerBox.activity.attempt.settingsRestarted', 'Confirmação reiniciada pelos ajustes'),
        detail: t('showerBox.activity.attempt.settingsRestartedHelp', 'Os votos antigos foram descartados sem abrir nem fechar a válvula.'),
        tone: 'neutral' as const,
        icon: Settings2,
      }
    : attemptPresentation[attemptResult];
  const AttemptIcon = latestAttempt.icon;

  const sessionSequence = validSequence(showerBox?.session_sequence);
  const activeSessionId = showerBox?.active_session_id;
  const sessionActive = typeof activeSessionId === 'string' && activeSessionId.trim().length > 0;
  const sessionStartedAt = validTimestamp(showerBox?.last_session_started_at_unix_ms);
  const sessionEndedAt = validTimestamp(showerBox?.last_session_ended_at_unix_ms);
  const sessionTimestamp = sessionActive ? sessionStartedAt : sessionEndedAt ?? sessionStartedAt;
  const sessionTimeText = formatEventTimestamp(sessionTimestamp, nowMs, locale);
  const hasSessionHistoryContract = showerBox?.session_sequence !== undefined
    || showerBox?.active_session_id !== undefined;
  const hasSessionHistory = sessionActive
    || sessionSequence > 0
    || sessionStartedAt !== null
    || sessionEndedAt !== null;
  const endReason = showerBox?.last_session_end_reason?.trim() ?? '';
  const sessionEndLabels: Record<string, string> = {
    hold_expired: t('showerBox.activity.session.reason.holdExpired', 'Saída confirmada após o tempo de proteção.'),
    exit_confirmed: t('showerBox.activity.session.reason.exitConfirmed', 'Saída do box confirmada.'),
    manual: t('showerBox.activity.session.reason.manual', 'Sessão encerrada manualmente.'),
    manual_override: t('showerBox.activity.session.reason.manualOverride', 'Controle devolvido ao usuário.'),
    mode_changed: t('showerBox.activity.session.reason.modeChanged', 'Modo alterado; controle devolvido ao usuário.'),
    restart: t('showerBox.activity.session.reason.restart', 'Sessão encerrada durante a reinicialização segura.'),
    input_unavailable: t('showerBox.activity.session.reason.inputUnavailable', 'Sessão encerrada por proteção dos sensores.'),
  };
  const sessionTitle = sessionActive
    ? t('showerBox.activity.session.active', 'Proteção em andamento')
    : hasSessionHistory
      ? t('showerBox.activity.session.finished', 'Sessão encerrada')
      : hasSessionHistoryContract
        ? t('showerBox.activity.session.empty', 'Nenhuma sessão ainda')
        : t('showerBox.activity.legacy', 'Histórico disponível após atualizar');
  const sessionDetail = sessionActive
    ? t('showerBox.activity.session.activeHelp', 'A zona está protegendo a permanência no box.')
    : endReason
      ? sessionEndLabels[endReason]
        ?? t('showerBox.activity.session.reason.other', 'Encerrada: {{reason}}.', { reason: humanizeReason(endReason) })
      : hasSessionHistory
        ? t('showerBox.activity.session.finishedHelp', 'A última sessão terminou sem novo acionamento.')
        : hasSessionHistoryContract
          ? t('showerBox.activity.session.emptyHelp', 'A primeira sessão válida aparecerá aqui.')
          : t('showerBox.activity.legacyHelp', 'A detecção ao vivo continua funcionando normalmente.');

  const gates = useMemo<GateDefinition[]>(() => [
    {
      id: 'stream',
      label: t('showerBox.gates.stream', 'Fluxo CSI ao vivo'),
      detail: isStreamLive
        ? t('showerBox.gates.streamOk', 'Recebendo agora')
        : t('showerBox.gates.streamWait', 'Aguardando UDP ao vivo'),
      complete: isStreamLive,
    },
    {
      id: 'wroom',
      label: t('showerBox.gates.wroom', 'Sensor próximo ao box'),
      detail: showerBox?.wroom_available
        ? t('showerBox.gates.receiverOk', 'Disponível')
        : t('showerBox.gates.receiverWait', 'Sem sinal recente'),
      complete: showerBox?.wroom_available === true,
    },
    {
      id: 'c5',
      label: t('showerBox.gates.c5', 'Referência do quarto'),
      detail: showerBox?.c5_available
        ? t('showerBox.gates.receiverOk', 'Disponível')
        : t('showerBox.gates.receiverWait', 'Sem sinal recente'),
      complete: showerBox?.c5_available === true,
    },
    {
      id: 'baselines',
      label: t('showerBox.gates.baselines', 'Ambiente vazio calibrado'),
      detail: showerBox?.wroom_has_baseline && showerBox?.c5_has_baseline
        ? t('showerBox.gates.baselinesOk', 'Duas referências válidas')
        : t('showerBox.gates.baselinesWait', 'Calibração necessária'),
      complete: showerBox?.wroom_has_baseline === true && showerBox?.c5_has_baseline === true,
    },
    {
      id: 'rates',
      label: t('showerBox.gates.rates', 'Ritmo dos dois sensores'),
      detail: `${wroomRate.toFixed(1)} / ${c5Rate.toFixed(1)} Hz`,
      complete: wroomRate >= 4 && c5Rate >= 4,
    },
    {
      id: 'warmup',
      label: t('showerBox.gates.warmup', 'Partida segura'),
      detail: warmupRemaining !== undefined && warmupRemaining > 0
        ? t('showerBox.gates.warmupWait', 'Mais {{seconds}}', { seconds: formatSeconds(warmupRemaining) })
        : t('showerBox.gates.warmupOk', 'Inicialização concluída'),
      complete: warmupRemaining !== undefined
        ? warmupRemaining <= 0
        : showerBox?.ready === true,
    },
    {
      id: 'ready',
      label: t('showerBox.gates.ready', 'Zona pronta para decidir'),
      detail: showerBox?.ready
        ? t('showerBox.gates.readyOk', 'Intertravamentos liberados')
        : t('showerBox.gates.readyWait', 'Proteção bloqueando a entrada'),
      complete: showerBox?.ready === true,
    },
  ], [c5Rate, isStreamLive, showerBox, t, warmupRemaining, wroomRate]);

  const completedGates = gates.filter((gate) => gate.complete).length;
  const setupProgress = Math.round((completedGates / gates.length) * 100);
  const dirty = savedSettings !== null && !settingsEqual(savedSettings, draft);

  const reasonLabel = (() => {
    // O topo descreve o estado atual; o cartão abaixo já preserva o último
    // evento. Assim uma tentativa rejeitada não deixa a zona pronta parecendo
    // ainda bloqueada ou em processamento.
    if (showerBox?.ready && phase === 'vacant') {
      return t('showerBox.reason.readyVacant', 'Zona pronta para uma nova chegada');
    }
    const reason = showerBox?.last_transition_reason;
    const labels: Record<string, string> = {
      startup_not_ready: t('showerBox.reason.startup', 'Preparando os sensores'),
      ready_vacant: t('showerBox.reason.readyVacant', 'Zona pronta para uma nova chegada'),
      live_stream_required: t('showerBox.reason.liveRequired', 'Aguardando fluxo CSI ao vivo'),
      replay_blocked: t('showerBox.reason.replayBlocked', 'Replay não controla água'),
      entry_candidate: t('showerBox.reason.candidate', 'Confirmando entrada no box'),
      candidate_input_not_ready: t('showerBox.reason.candidateNotReady', 'Entrada pausada até o sinal estabilizar'),
      candidate_rejected: t('showerBox.reason.rejected', 'Sinal breve ignorado'),
      entry_confirmed: t('showerBox.reason.confirmed', 'Entrada confirmada'),
      evidence_reconfirmed: t('showerBox.reason.reconfirmed', 'Presença reconfirmada'),
      evidence_recovery_pending: t('showerBox.reason.recovery', 'Reconhecendo a presença novamente'),
      hold_expired: t('showerBox.reason.exit', 'Saída confirmada'),
      input_unavailable_holding: t('showerBox.reason.inputHold', 'Sinal indisponível; proteção temporária'),
      weak_signal_holding: t('showerBox.reason.weakHold', 'Proteção ativa enquanto o sinal é verificado'),
      settings_applied_candidate_rejected: t('showerBox.reason.settingsCandidate', 'Ajustes aplicados; confirmação reiniciada'),
      settings_applied_vacant: t('showerBox.reason.settingsVacant', 'Ajustes aplicados com a zona livre'),
      settings_applied_session_preserved: t('showerBox.reason.settingsSession', 'Ajustes aplicados sem interromper a sessão'),
    };
    return reason ? labels[reason] ?? reason.replaceAll('_', ' ') : t('showerBox.reason.waiting', 'Aguardando dados da zona');
  })();

  const statusTitle = (() => {
    if (!showerBox) return t('showerBox.status.noData', 'Aguardando a zona do box');
    if (!isStreamLive) return t('showerBox.status.noLive', 'Automação protegida');
    if (!showerBox.ready) return t('showerBox.status.preparing', 'Preparando detecção');
    if (phase === 'candidate') return t('showerBox.status.candidate', 'Entrada em confirmação');
    if (phase === 'holding') return t('showerBox.status.holding', 'Presença protegida');
    if (showerBox.occupied) return t('showerBox.status.occupied', 'Box ocupado');
    return t('showerBox.status.vacant', 'Pronto para sua chegada');
  })();

  const modeSummary = (() => {
    if (draft.control_mode === 'manual') {
      return t('showerBox.mode.manualSummary', 'A zona apenas informa; nenhuma transição solicita comandos de água.');
    }
    if (draft.control_mode === 'assisted') {
      return t('showerBox.mode.assistedSummary', 'A entrada pode ser solicitada automaticamente; o encerramento fica manual.');
    }
    return t('showerBox.mode.automaticSummary', 'A zona abre e fecha somente a sessão que ela própria iniciou. Se você ligar, o encerramento continua manual.');
  })();

  const applyPreset = (preset: PresetDefinition) => {
    setDraft((current) => ({ ...current, ...preset.settings }));
    setFeedback({
      tone: 'neutral',
      message: t('showerBox.settings.review', 'Perfil aplicado. Revise e salve para ativar.'),
    });
  };

  const setMode = (mode: ShowerControlMode) => {
    setDraft((current) => ({
      ...current,
      control_mode: mode,
      presence_assist: mode === 'assisted' ? true : current.presence_assist,
    }));
    setFeedback({
      tone: 'neutral',
      message: t('showerBox.settings.reviewMode', 'Modo selecionado. Salve para ativar.'),
    });
  };

  const setNumericSetting = (key: NumericSettingKey, value: number) => {
    setDraft((current) => ({ ...current, [key]: value, profile: 'custom' }));
  };

  const saveSettings = async () => {
    if (!savedSettings || !dirty) return;
    setSaving(true);
    setFeedback({ tone: 'neutral', message: t('showerBox.settings.saving', 'Salvando ajustes seguros...') });
    try {
      const response = await updateShowerBoxSettings(apiBaseUrl, draft);
      setSavedSettings(response.settings);
      setDraft(response.settings);
      setLimits(response.limits);
      setFeedback({ tone: 'success', message: t('showerBox.settings.saved', 'Ajustes da zona ativados.') });
    } catch (error) {
      setFeedback({
        tone: 'error',
        message: error instanceof Error
          ? error.message
          : t('showerBox.settings.saveError', 'Não foi possível salvar os ajustes.'),
      });
    } finally {
      setSaving(false);
    }
  };

  const sliderDefinitions: Array<{
    key: NumericSettingKey;
    label: string;
    description: string;
    range: NumericRange;
    step: number;
    unit: string;
  }> = [
    {
      key: 'entry_confirm_seconds',
      label: t('showerBox.advanced.confirm', 'Tempo para confirmar entrada'),
      description: t('showerBox.advanced.confirmHelp', 'Ignora passagens mais curtas que este tempo.'),
      range: limitFor(limits, 'entry_confirm_seconds', 'confirm', DEFAULT_SHOWER_BOX_LIMITS.confirm),
      step: limitStepFor(limits, 'entry_confirm_seconds', 'confirm', 0.5),
      unit: 's',
    },
    {
      key: 'entry_decay_seconds',
      label: t('showerBox.advanced.decay', 'Tolerância durante a confirmação'),
      description: t('showerBox.advanced.decayHelp', 'Evita zerar o progresso por uma oscilação breve.'),
      range: limitFor(limits, 'entry_decay_seconds', 'decay', DEFAULT_SHOWER_BOX_LIMITS.decay),
      step: limitStepFor(limits, 'entry_decay_seconds', 'decay', 0.25),
      unit: 's',
    },
    {
      key: 'exit_hold_seconds',
      label: t('showerBox.advanced.hold', 'Proteção após perder evidência'),
      description: t('showerBox.advanced.holdHelp', 'Mantém a decisão durante imobilidade ou perda breve de pacotes.'),
      range: limitFor(limits, 'exit_hold_seconds', 'hold', DEFAULT_SHOWER_BOX_LIMITS.hold),
      step: limitStepFor(limits, 'exit_hold_seconds', 'hold', 15),
      unit: 's',
    },
    {
      key: 'recovery_confirm_seconds',
      label: t('showerBox.advanced.recovery', 'Tempo para reconfirmar presença'),
      description: t('showerBox.advanced.recoveryHelp', 'Exige evidência contínua antes de sair do estado protegido.'),
      range: limitFor(limits, 'recovery_confirm_seconds', 'recovery', DEFAULT_SHOWER_BOX_LIMITS.recovery),
      step: limitStepFor(limits, 'recovery_confirm_seconds', 'recovery', 0.5),
      unit: 's',
    },
    {
      key: 'sensitivity',
      label: t('showerBox.advanced.sensitivity', 'Sensibilidade'),
      description: t('showerBox.advanced.sensitivityHelp', 'Valores altos reagem mais cedo; a referência do quarto continua obrigatória.'),
      range: limitFor(limits, 'sensitivity', 'sensitivity', DEFAULT_SHOWER_BOX_LIMITS.sensitivity),
      step: limitStepFor(limits, 'sensitivity', 'sensitivity', 1),
      unit: '/5',
    },
  ];

  return (
    <section className="bento-card shower-box-panel" aria-labelledby="shower-box-title">
      <header className="shower-box-panel__header">
        <div className="shower-box-panel__heading">
          <span className={`shower-box-panel__icon shower-box-panel__icon--${tone}`} aria-hidden="true">
            <ShowerHead size={24} />
          </span>
          <div>
            <span className="shower-box-panel__eyebrow">{t('showerBox.eyebrow', 'Zona inteligente')}</span>
            <h2 id="shower-box-title">{t('showerBox.title', 'Box do chuveiro')}</h2>
          </div>
        </div>
        <span className={`shower-box-live-pill shower-box-live-pill--${isStreamLive ? 'live' : 'paused'}`}>
          <span aria-hidden="true" />
          {isStreamLive ? t('showerBox.live', 'Ao vivo') : t('showerBox.paused', 'Protegido')}
        </span>
      </header>

      <div className={`shower-box-status shower-box-status--${tone}`} aria-live="polite">
        <div className="shower-box-status__copy">
          <span>{t('showerBox.status.label', 'Decisão da zona')}</span>
          <strong>{statusTitle}</strong>
          <p>{reasonLabel}</p>
        </div>
        <div className="shower-box-status__timer" aria-label={t('showerBox.status.timerLabel', 'Temporizador atual')}>
          {phase === 'candidate' ? (
            <>
              <span>{t('showerBox.status.confirmIn', 'Confirma em')}</span>
              <strong>{formatSeconds(showerBox?.entry_seconds_remaining)}</strong>
            </>
          ) : phase === 'holding' || showerBox?.occupied ? (
            <>
              <span>{t('showerBox.status.protection', 'Proteção')}</span>
              <strong>{formatSeconds(showerBox?.hold_seconds_remaining)}</strong>
            </>
          ) : (
            <>
              <span>{t('showerBox.status.entryTime', 'Entrada')}</span>
              <strong>{formatSeconds(showerBox?.configured_entry_confirm_seconds ?? draft.entry_confirm_seconds)}</strong>
            </>
          )}
        </div>
        <div
          className="shower-box-status__progress"
          role="progressbar"
          aria-label={t('showerBox.status.entryProgress', 'Progresso da confirmação de entrada')}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={Math.round(entryProgress * 100)}
        >
          <span style={{ width: `${entryProgress * 100}%` }} />
        </div>
      </div>

      <p className="shower-box-panel__truth-note">
        {t(
          'showerBox.truthNote',
          'Este painel mostra a decisão do AetherSense. Confirme o estado físico da tomada no Home Assistant sobre o mapa.',
        )}
      </p>

      <section
        className="shower-box-activity"
        aria-label={t('showerBox.activity.label', 'Histórico recente da zona do box')}
      >
        <article className={`shower-box-activity-card is-${latestAttempt.tone}`}>
          <header className="shower-box-activity-card__header">
            <span className="shower-box-activity-card__icon" aria-hidden="true">
              <AttemptIcon
                className={attemptResult === 'in_progress' ? 'shower-box-gate-spinner' : undefined}
                size={17}
              />
            </span>
            <div>
              <span>{t('showerBox.activity.attempt.kicker', 'Detecção')}</span>
              <h3>{t('showerBox.activity.attempt.title', 'Última tentativa')}</h3>
            </div>
            <span
              className="shower-box-activity-card__sequence"
              aria-label={attemptSequence > 0
                ? t('showerBox.activity.attempt.number', 'Tentativa número {{number}}', { number: attemptSequence })
                : t('showerBox.activity.noNumber', 'Sem número ainda')}
            >
              {attemptSequence > 0 ? `#${attemptSequence}` : '—'}
            </span>
          </header>
          <strong className="shower-box-activity-card__result">{latestAttempt.title}</strong>
          <p className="shower-box-activity-card__detail">{latestAttempt.detail}</p>
          <div className="shower-box-activity-card__time">
            <Timer size={13} aria-hidden="true" />
            {attemptTimestamp !== null && attemptTimeText ? (
              <time
                dateTime={new Date(attemptTimestamp).toISOString()}
                title={new Date(attemptTimestamp).toLocaleString(locale)}
              >
                {attemptTimeText}
              </time>
            ) : (
              <span>{t('showerBox.activity.noTime', 'Sem horário registrado')}</span>
            )}
          </div>
          <div className="shower-box-activity-card__peak">
            <span>{t('showerBox.activity.attempt.peak', 'Pico da missão')}</span>
            <strong>{attemptPeakPercent === null ? '—' : `${attemptPeakPercent}%`}</strong>
          </div>
          <div
            className={`shower-box-activity-card__bar ${attemptPeakPercent === null ? 'is-empty' : ''}`}
            role="progressbar"
            aria-label={t('showerBox.activity.attempt.peakLabel', 'Maior progresso da confirmação')}
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={attemptPeakPercent ?? undefined}
            aria-valuetext={attemptPeakPercent === null
              ? t('showerBox.activity.unavailable', 'Ainda não disponível')
              : undefined}
          >
            <span style={{ width: `${attemptPeakPercent ?? 0}%` }} />
          </div>
        </article>

        <article className={`shower-box-activity-card ${sessionActive ? 'is-success' : 'is-neutral'}`}>
          <header className="shower-box-activity-card__header">
            <span className="shower-box-activity-card__icon" aria-hidden="true">
              <ShowerHead size={17} />
            </span>
            <div>
              <span>{t('showerBox.activity.session.kicker', 'Proteção')}</span>
              <h3>
                {sessionActive
                  ? t('showerBox.activity.session.activeTitle', 'Sessão ativa')
                  : t('showerBox.activity.session.title', 'Última sessão')}
              </h3>
            </div>
            <span
              className="shower-box-activity-card__sequence"
              aria-label={sessionSequence > 0
                ? t('showerBox.activity.session.number', 'Sessão número {{number}}', { number: sessionSequence })
                : t('showerBox.activity.noNumber', 'Sem número ainda')}
            >
              {sessionSequence > 0 ? `#${sessionSequence}` : '—'}
            </span>
          </header>
          <strong className="shower-box-activity-card__result">{sessionTitle}</strong>
          <p className="shower-box-activity-card__detail">{sessionDetail}</p>
          <div className="shower-box-activity-card__time">
            <Timer size={13} aria-hidden="true" />
            {sessionTimestamp !== null && sessionTimeText ? (
              <time
                dateTime={new Date(sessionTimestamp).toISOString()}
                title={new Date(sessionTimestamp).toLocaleString(locale)}
              >
                {sessionActive
                  ? t('showerBox.activity.session.startedAt', 'Iniciada {{time}}', { time: sessionTimeText })
                  : t('showerBox.activity.session.endedAt', 'Encerrada {{time}}', { time: sessionTimeText })}
              </time>
            ) : (
              <span>{t('showerBox.activity.noTime', 'Sem horário registrado')}</span>
            )}
          </div>
          <div className={`shower-box-activity-card__session-state ${sessionActive ? 'is-active' : ''}`}>
            <span aria-hidden="true" />
            {sessionActive
              ? t('showerBox.activity.session.live', 'Permanência protegida agora')
              : hasSessionHistory
                ? t('showerBox.activity.session.closed', 'Ciclo concluído')
                : t('showerBox.activity.session.waiting', 'Aguardando a primeira sessão')}
          </div>
        </article>
      </section>

      <section className="shower-box-mission" aria-labelledby="shower-box-mission-title">
        <div className="shower-box-section-heading">
          <div>
            <span>{t('showerBox.mission.kicker', 'Checklist ao vivo')}</span>
            <h3 id="shower-box-mission-title">{t('showerBox.mission.title', 'Missão: banho inteligente')}</h3>
          </div>
          <strong>{completedGates}/{gates.length}</strong>
        </div>
        <div
          className="shower-box-mission__progress"
          role="progressbar"
          aria-label={t('showerBox.mission.progress', 'Progresso dos intertravamentos')}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={setupProgress}
        >
          <span style={{ width: `${setupProgress}%` }} />
        </div>
        <ul className="shower-box-gates">
          {gates.map((gate) => (
            <li className={gate.complete ? 'is-complete' : 'is-waiting'} key={gate.id}>
              {gate.complete
                ? <CheckCircle2 size={18} aria-hidden="true" />
                : isStreamLive && showerBox
                  ? <LoaderCircle className="shower-box-gate-spinner" size={18} aria-hidden="true" />
                  : <Circle size={18} aria-hidden="true" />}
              <span>
                <strong>{gate.label}</strong>
                <small>{gate.detail}</small>
              </span>
            </li>
          ))}
        </ul>
      </section>

      <section className="shower-box-config" aria-labelledby="shower-box-presets-title">
        <div className="shower-box-section-heading">
          <div>
            <span>{t('showerBox.presets.kicker', 'Como você prefere')}</span>
            <h3 id="shower-box-presets-title">{t('showerBox.presets.title', 'Perfil de detecção')}</h3>
          </div>
          {draft.profile === 'custom' && <em>{t('showerBox.presets.custom', 'Personalizado')}</em>}
        </div>
        <div className="shower-box-choice-grid shower-box-choice-grid--presets">
          {PRESETS.map((preset) => {
            const Icon = preset.icon;
            const selected = draft.profile === preset.id;
            const content = {
              comfort: {
                title: t('showerBox.presets.comfort', 'Conforto'),
                description: t('showerBox.presets.comfortHelp', 'Resposta rápida e proteção por até 3 minutos.'),
              },
              balanced: {
                title: t('showerBox.presets.balanced', 'Equilibrado'),
                description: t('showerBox.presets.balancedHelp', 'Confirma em 2,5 s e mantém por 60 s.'),
              },
              precise: {
                title: t('showerBox.presets.precise', 'Evitar disparos'),
                description: t('showerBox.presets.preciseHelp', 'Exige uma entrada mais estável por até 4 s.'),
              },
            }[preset.id];

            return (
              <button
                className={`shower-box-choice ${selected ? 'is-selected' : ''}`}
                type="button"
                onClick={() => applyPreset(preset)}
                disabled={!savedSettings || loading || saving}
                aria-pressed={selected}
                key={preset.id}
              >
                <span className="shower-box-choice__icon"><Icon size={19} aria-hidden="true" /></span>
                <span className="shower-box-choice__copy">
                  <strong>
                    {content.title}
                    {preset.id === 'balanced' && <em>{t('showerBox.recommended', 'Recomendado')}</em>}
                  </strong>
                  <small>{content.description}</small>
                </span>
              </button>
            );
          })}
        </div>
      </section>

      <section className="shower-box-config" aria-labelledby="shower-box-mode-title">
        <div className="shower-box-section-heading">
          <div>
            <span>{t('showerBox.mode.kicker', 'Nível de automação')}</span>
            <h3 id="shower-box-mode-title">{t('showerBox.mode.title', 'Quem encerra a água?')}</h3>
          </div>
        </div>
        <div className="shower-box-choice-grid shower-box-choice-grid--modes">
          {([
            {
              id: 'automatic' as const,
              icon: Zap,
              title: t('showerBox.mode.automatic', 'Automático'),
              description: t('showerBox.mode.automaticHelp', 'Abre e fecha só a própria sessão.'),
            },
            {
              id: 'assisted' as const,
              icon: Waves,
              title: t('showerBox.mode.assisted', 'Assistido'),
              description: t('showerBox.mode.assistedHelp', 'Entrada automática; saída manual.'),
            },
            {
              id: 'manual' as const,
              icon: Hand,
              title: t('showerBox.mode.manual', 'Manual'),
              description: t('showerBox.mode.manualHelp', 'Somente informa a decisão.'),
            },
          ]).map((mode) => {
            const Icon = mode.icon;
            const selected = draft.control_mode === mode.id;
            return (
              <button
                className={`shower-box-choice shower-box-choice--compact ${selected ? 'is-selected' : ''}`}
                type="button"
                onClick={() => setMode(mode.id)}
                disabled={!savedSettings || loading || saving}
                aria-pressed={selected}
                key={mode.id}
              >
                <span className="shower-box-choice__icon"><Icon size={18} aria-hidden="true" /></span>
                <span className="shower-box-choice__copy">
                  <strong>{mode.title}</strong>
                  <small>{mode.description}</small>
                </span>
              </button>
            );
          })}
        </div>
        <p className="shower-box-mode-summary"><ShieldCheck size={15} aria-hidden="true" /> {modeSummary}</p>
      </section>

      <details className="shower-box-advanced">
        <summary>
          <span><Settings2 size={18} aria-hidden="true" /> {t('showerBox.advanced.title', 'Ajustes avançados')}</span>
          <ChevronDown size={18} aria-hidden="true" />
        </summary>
        <div className="shower-box-advanced__body">
          {sliderDefinitions.map((slider) => {
            const value = draft[slider.key];
            return (
              <label className="shower-box-slider" key={slider.key}>
                <span className="shower-box-slider__heading">
                  <span>
                    <strong>{slider.label}</strong>
                    <small>{slider.description}</small>
                  </span>
                  <output>{value}{slider.unit}</output>
                </span>
                <input
                  type="range"
                  min={slider.range[0]}
                  max={slider.range[1]}
                  step={slider.step}
                  value={value}
                  onChange={(event) => setNumericSetting(slider.key, Number(event.target.value))}
                  disabled={!savedSettings || loading || saving}
                  aria-valuetext={`${value} ${slider.unit}`}
                />
                <span className="shower-box-slider__range" aria-hidden="true">
                  <span>{slider.range[0]}{slider.unit}</span>
                  <span>{slider.range[1]}{slider.unit}</span>
                </span>
              </label>
            );
          })}

          <div className="shower-box-assist-toggle">
            <span>
              <strong>{t('showerBox.advanced.presenceAssist', 'Confirmação humana independente')}</strong>
              <small>{t('showerBox.advanced.presenceAssistHelp', 'Recomendado: exige movimento instantâneo além do WROOM–C5. Desativar permite que ruído forte abra a válvula.')}</small>
            </span>
            <button
              className={`shower-box-switch ${draft.presence_assist ? 'is-on' : ''}`}
              type="button"
              role="switch"
              aria-checked={draft.presence_assist}
              aria-label={t('showerBox.advanced.presenceAssist', 'Confirmação humana independente')}
              onClick={() => setDraft((current) => ({
                ...current,
                presence_assist: !current.presence_assist,
                profile: 'custom',
              }))}
              disabled={!savedSettings || loading || saving}
            >
              <span />
            </button>
          </div>
        </div>
      </details>

      <footer className="shower-box-actions">
        <div className={`shower-box-feedback ${feedback ? `is-${feedback.tone}` : ''}`} role="status" aria-live="polite">
          {loading ? (
            <><LoaderCircle className="shower-box-gate-spinner" size={16} aria-hidden="true" /> {t('showerBox.settings.loading', 'Carregando ajustes...')}</>
          ) : feedback ? feedback.message : (
            <><Gauge size={16} aria-hidden="true" /> {dirty ? t('showerBox.settings.unsaved', 'Alterações ainda não salvas') : t('showerBox.settings.synced', 'Ajustes sincronizados')}</>
          )}
        </div>
        <div className="shower-box-actions__buttons">
          {!savedSettings && !loading ? (
            <button className="btn btn--secondary" type="button" onClick={() => void loadSettings()}>
              <RefreshCw size={16} aria-hidden="true" />
              {t('showerBox.settings.retry', 'Tentar novamente')}
            </button>
          ) : (
            <>
              <button
                className="btn btn--secondary"
                type="button"
                onClick={() => savedSettings && setDraft(savedSettings)}
                disabled={!dirty || saving}
              >
                <RotateCcw size={16} aria-hidden="true" />
                {t('showerBox.settings.undo', 'Desfazer')}
              </button>
              <button
                className="btn btn--primary"
                type="button"
                onClick={() => void saveSettings()}
                disabled={!dirty || saving || loading}
              >
                {saving
                  ? <LoaderCircle className="shower-box-gate-spinner" size={16} aria-hidden="true" />
                  : <Save size={16} aria-hidden="true" />}
                {saving ? t('showerBox.settings.savingShort', 'Salvando...') : t('showerBox.settings.save', 'Salvar ajustes')}
              </button>
            </>
          )}
        </div>
      </footer>
    </section>
  );
}
