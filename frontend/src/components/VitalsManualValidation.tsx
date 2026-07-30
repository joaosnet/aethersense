import { useCallback, useEffect, useRef, useState } from 'react';
import { Download, HeartPulse, Play, RotateCcw, Square, Timer, Wind } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import Plotly from 'plotly.js-dist-min';
import { exportPlot } from '../utils/plotlyHelper';
import { apiFetch } from '../utils/apiClient';

// Estimativa do sistema (CSI) ao vivo, vinda de detection.vitals. O painel pai
// já faz polling de /api/detection — aqui só amostramos o último valor.
export interface LiveVitals {
  breathing_rate?: number;
  heart_rate_bpm?: number;
  confidence?: number;
  breathing_rate_ml?: number;
  heart_rate_bpm_ml?: number;
  confidence_ml?: number;
}

interface Props {
  liveVitals?: LiveVitals;
  // Quando presente, a sessão é salva AUTOMATICAMENTE no backend ao terminar
  // (POST /api/results/vitais-autoteste → resultados/ na raiz do repo); o
  // botão de download JSON vira apenas um backup manual.
  apiBaseUrl?: string;
}

type Mode = 'breathing' | 'heart';
type SessionState = 'idle' | 'running' | 'done';

interface Sample {
  t_s: number;
  breathing_rate?: number;
  heart_rate_bpm?: number;
  confidence?: number;
  breathing_rate_ml?: number;
  heart_rate_bpm_ml?: number;
  confidence_ml?: number;
}

// JSON exportado por sessão — ingerido por tools/vitals_autotest_report.py.
interface SessionExport {
  schema: 'aethersense.vitals_manual.v1';
  session_id: string;
  started_at_iso: string;
  mode: Mode;
  duration_s: number;
  target_rpm: number | null;
  position: string;
  click_timestamps_ms: number[];
  manual: {
    breathing_rate_count: number | null;
    breathing_rate_median: number | null;
    heart_rate_bpm: number | null;
    heart_truth_method: 'tapping' | 'oximeter_or_pulse';
  };
  estimate_samples: Sample[];
  estimate_summary: {
    breathing_rate_median: number | null;
    heart_rate_bpm_median: number | null;
    confidence_mean: number | null;
    breathing_rate_ml_median?: number | null;
    heart_rate_bpm_ml_median?: number | null;
    confidence_ml_mean?: number | null;
    n_samples: number;
  };
  device_hint: string;
  notes: string;
}

const isNum = (v: unknown): v is number => typeof v === 'number' && Number.isFinite(v);
const fmt = (v: number | null | undefined, d = 1) => (isNum(v) ? v.toFixed(d) : '—');

function median(values: number[]): number | null {
  const a = values.filter(isNum).slice().sort((x, y) => x - y);
  if (a.length === 0) return null;
  const m = Math.floor(a.length / 2);
  return a.length % 2 ? a[m] : (a[m - 1] + a[m]) / 2;
}

function mean(values: number[]): number | null {
  const a = values.filter(isNum);
  return a.length ? a.reduce((s, x) => s + x, 0) / a.length : null;
}

const card: React.CSSProperties = {
  background: 'var(--card-bg, rgba(255,255,255,0.04))',
  border: '1px solid var(--border, rgba(148,163,184,0.25))',
  borderRadius: 12,
  padding: '12px 14px',
};

/**
 * Ferramenta de PROVA MANUAL de sinais vitais (autotest N=1). O autor clica um
 * contador a cada respiração/batimento (verdade-terra) enquanto o ESP32 estima
 * a taxa via CSI; ao final, compara manual × estimado e exporta a sessão (JSON)
 * para gerar as tabelas/gráficos do TCC. Não é medida clínica.
 */
export function VitalsManualValidation({ liveVitals, apiBaseUrl }: Props) {
  const { t } = useTranslation();
  const [mode, setMode] = useState<Mode>('breathing');
  const [state, setState] = useState<SessionState>('idle');
  const [saveStatus, setSaveStatus] = useState<string | null>(null);
  const [durationS, setDurationS] = useState(60);
  const [targetRpm, setTargetRpm] = useState('');
  const [position, setPosition] = useState('');
  const [hrAlt, setHrAlt] = useState('');
  const [remaining, setRemaining] = useState(0);
  const [, setTick] = useState(0); // força re-render a cada amostra/clique
  const [result, setResult] = useState<SessionExport | null>(null);

  const clicksRef = useRef<number[]>([]);
  const samplesRef = useRef<Sample[]>([]);
  const startRef = useRef(0);
  const startedIsoRef = useRef('');
  const liveRef = useRef<LiveVitals | undefined>(liveVitals);
  const intervalRef = useRef<number | null>(null);

  // Mantém a estimativa ao vivo acessível dentro do amostrador (setInterval).
  useEffect(() => {
    liveRef.current = liveVitals;
  }, [liveVitals]);

  const stopTimers = () => {
    if (intervalRef.current !== null) {
      window.clearInterval(intervalRef.current);
      intervalRef.current = null;
    }
  };

  const finish = useCallback((interrupted = false) => {
    stopTimers();
    const clicks = clicksRef.current;
    const samples = samplesRef.current;
    const n = clicks.length;

    let countRate: number | null = null;
    let medianRate: number | null = null;
    if (n >= 2) {
      const spanS = (clicks[n - 1] - clicks[0]) / 1000;
      if (spanS > 0) countRate = ((n - 1) / spanS) * 60;
      const intervals: number[] = [];
      for (let i = 1; i < n; i += 1) intervals.push(clicks[i] - clicks[i - 1]);
      const medMs = median(intervals);
      if (medMs && medMs > 0) medianRate = 60000 / medMs;
    }

    // Descarta o transitório inicial (a janela de vitais é ~30 s).
    const cutoff = Math.min(30, durationS / 2);
    const post = samples.filter((s) => s.t_s >= cutoff);
    const used = post.length >= 3 ? post : samples;
    const brEst = median(used.map((s) => s.breathing_rate ?? NaN));
    const hrEst = median(used.map((s) => s.heart_rate_bpm ?? NaN));
    const confMean = mean(used.map((s) => s.confidence ?? NaN));
    const brEstMl = median(used.map((s) => s.breathing_rate_ml ?? NaN));
    const hrEstMl = median(used.map((s) => s.heart_rate_bpm_ml ?? NaN));
    const confMeanMl = mean(used.map((s) => s.confidence_ml ?? NaN));

    // A HR de referência (oxímetro) só vale em sessão de BATIMENTO: em sessão
    // de respiração um hrAlt residual de sessão anterior contaminaria o export.
    const hrAltNum = parseFloat(hrAlt);
    const heartManual = mode === 'heart' ? (isNum(hrAltNum) ? hrAltNum : medianRate) : null;
    const heartMethod: 'tapping' | 'oximeter_or_pulse' =
      mode === 'heart' && isNum(hrAltNum) ? 'oximeter_or_pulse' : 'tapping';
    const targetNum = parseFloat(targetRpm);

    const session: SessionExport = {
      schema: 'aethersense.vitals_manual.v1',
      session_id: `${startedIsoRef.current}_${mode}`,
      started_at_iso: startedIsoRef.current,
      mode,
      duration_s: durationS,
      target_rpm: isNum(targetNum) ? targetNum : null,
      position: position.trim(),
      click_timestamps_ms: clicks.map((c) => Math.round(c)),
      manual: {
        breathing_rate_count: countRate,
        breathing_rate_median: medianRate,
        heart_rate_bpm: heartManual,
        heart_truth_method: heartMethod,
      },
      estimate_samples: samples,
      estimate_summary: {
        breathing_rate_median: brEst,
        heart_rate_bpm_median: hrEst,
        confidence_mean: confMean,
        breathing_rate_ml_median: brEstMl,
        heart_rate_bpm_ml_median: hrEstMl,
        confidence_ml_mean: confMeanMl,
        n_samples: samples.length,
      },
      device_hint: 'ESP32-C5',
      notes: `${targetRpm ? t('vitalsVal.targetNote', '{{mode}} a {{rpm}} de alvo', { mode, rpm: targetRpm }) : t('vitalsVal.naturalNote', '{{mode}} natural', { mode })}${interrupted ? t('vitalsVal.interruptedNote', ' — sessão interrompida antes do fim (painel fechado)') : ''}`,
    };
    setResult(session);
    setState('done');

    // Auto-save no backend → resultados/vitais-autoteste/<session_id>/session.json
    if (apiBaseUrl !== undefined) {
      setSaveStatus(t('vitalsVal.saving', 'salvando…'));
      void (async () => {
        try {
          const r = await apiFetch(`${apiBaseUrl}/api/results/vitais-autoteste`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(session),
          });
          const j = await r.json();
          setSaveStatus(j.ok ? t('vitalsVal.savedAt', '✓ salvo em {{path}}', { path: j.path }) : `⚠ ${j.message}`);
        } catch {
          setSaveStatus(t('vitalsVal.autoSaveFailed', '⚠ falha ao salvar automaticamente — use o download como backup'));
        }
      })();
    }
  }, [apiBaseUrl, durationS, hrAlt, mode, position, targetRpm]);

  const start = useCallback(() => {
    clicksRef.current = [];
    samplesRef.current = [];
    startRef.current = performance.now();
    startedIsoRef.current = new Date().toISOString();
    setResult(null);
    setRemaining(durationS);
    setState('running');

    intervalRef.current = window.setInterval(() => {
      const elapsed = (performance.now() - startRef.current) / 1000;
      const lv = liveRef.current;
      samplesRef.current.push({
        t_s: Math.round(elapsed * 10) / 10,
        breathing_rate: isNum(lv?.breathing_rate) ? lv?.breathing_rate : undefined,
        heart_rate_bpm: isNum(lv?.heart_rate_bpm) ? lv?.heart_rate_bpm : undefined,
        confidence: isNum(lv?.confidence) ? lv?.confidence : undefined,
        breathing_rate_ml: isNum(lv?.breathing_rate_ml) ? lv?.breathing_rate_ml : undefined,
        heart_rate_bpm_ml: isNum(lv?.heart_rate_bpm_ml) ? lv?.heart_rate_bpm_ml : undefined,
        confidence_ml: isNum(lv?.confidence_ml) ? lv?.confidence_ml : undefined,
      });
      const rem = durationS - elapsed;
      setRemaining(Math.max(0, Math.ceil(rem)));
      setTick((t) => t + 1);
      if (rem <= 0) finish();
    }, 1000);
  }, [durationS, finish]);

  const registerClick = useCallback(() => {
    if (state !== 'running') return;
    clicksRef.current.push(performance.now() - startRef.current);
    setTick((t) => t + 1);
  }, [state]);

  // Barra de espaço também conta (ergonomia), sem rolar a página.
  useEffect(() => {
    if (state !== 'running') return;
    const onKey = (e: KeyboardEvent) => {
      if (e.code === 'Space' || e.key === ' ') {
        e.preventDefault();
        registerClick();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [state, registerClick]);

  // Espelhos em ref para o cleanup de unmount enxergar o estado ATUAL (o
  // closure do efeito [] congelaria os valores do primeiro render).
  const stateRef = useRef(state);
  stateRef.current = state;
  const finishRef = useRef(finish);
  finishRef.current = finish;

  useEffect(
    () => () => {
      // Painel colapsado/aba trocada no MEIO de uma sessão: fecha e salva a
      // sessão parcial (o fetch do auto-save sobrevive ao desmonte) em vez de
      // descartar cliques e amostras em silêncio.
      if (stateRef.current === 'running') {
        finishRef.current(true);
      } else {
        stopTimers();
      }
    },
    [],
  );

  const exportJSON = () => {
    if (!result) return;
    const blob = new Blob([JSON.stringify(result, null, 2)], { type: 'application/json' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    const stamp = result.started_at_iso.replace(/[:.]/g, '-');
    a.href = url;
    a.download = `vitals_session_${result.mode}_${result.target_rpm ?? 'nat'}_${stamp}.json`;
    a.click();
    URL.revokeObjectURL(url);
  };

  const liveLabel =
    mode === 'breathing'
      ? `${fmt(liveVitals?.breathing_rate)} irpm`
      : `${fmt(liveVitals?.heart_rate_bpm)} bpm`;
  const unit = mode === 'breathing' ? 'irpm' : 'bpm';
  const clickCount = clicksRef.current.length;
  const liveManual =
    clickCount >= 2
      ? ((clickCount - 1) / ((clicksRef.current[clickCount - 1] - clicksRef.current[0]) / 1000 || 1)) * 60
      : null;

  return (
    <section style={{ ...card, gridColumn: '1 / -1', marginTop: 12 }} aria-label={t('vitalsVal.ariaLabel', 'Validação manual de vitais')}>
      <h4 style={{ margin: '0 0 4px', display: 'flex', alignItems: 'center', gap: 6 }}>
        <Timer size={15} /> {t('vitalsVal.panelTitle', 'Validação manual de vitais (autotest)')}
      </h4>
      <p style={{ fontSize: 11, opacity: 0.65, margin: '0 0 10px' }}>
        {t('vitalsVal.panelDesc', 'Prova de conceito técnica (N=1). Clique no contador a cada {{action}} enquanto o sistema estima pelo CSI; ao final, compare e exporte a sessão. Não é medida clínica.', { action: mode === 'breathing' ? t('vitalsVal.breathingAction', 'respiração') : t('vitalsVal.heartAction', 'batimento') })}
      </p>

      {/* Configuração da sessão */}
      {state !== 'running' && (
        <div style={{ display: 'flex', flexWrap: 'wrap', gap: 8, alignItems: 'center', marginBottom: 10 }}>
          <div style={{ display: 'inline-flex', borderRadius: 8, overflow: 'hidden', border: '1px solid var(--border, rgba(148,163,184,0.25))' }}>
            <button type="button" onClick={() => setMode('breathing')}
              className={`btn ${mode === 'breathing' ? 'btn--primary' : 'btn--secondary'}`}
              style={{ borderRadius: 0, fontSize: 12, padding: '4px 10px' }}>
              <Wind size={13} /> {t('vitalsVal.btnBreathing', 'Respiração')}
            </button>
            <button type="button" onClick={() => setMode('heart')}
              className={`btn ${mode === 'heart' ? 'btn--primary' : 'btn--secondary'}`}
              style={{ borderRadius: 0, fontSize: 12, padding: '4px 10px' }}>
              <HeartPulse size={13} /> {t('vitalsVal.btnHeart', 'Batimento')}
            </button>
          </div>
          <label style={{ fontSize: 12, opacity: 0.8 }}>
            {t('vitalsVal.targetRate', 'Ritmo-alvo:')}{' '}
            <input inputMode="numeric" placeholder={mode === 'breathing' ? 'irpm' : 'bpm'} value={targetRpm}
              onChange={(e) => setTargetRpm(e.target.value)} style={{ width: 60, fontSize: 12, padding: '3px 6px' }} />
          </label>
          <label style={{ fontSize: 12, opacity: 0.8 }}>
            {t('vitalsVal.durationS', 'Duração (s):')}{' '}
            <input inputMode="numeric" value={durationS}
              onChange={(e) => setDurationS(Math.max(20, parseInt(e.target.value, 10) || 60))}
              style={{ width: 56, fontSize: 12, padding: '3px 6px' }} />
          </label>
          {mode === 'heart' && (
            <label style={{ fontSize: 12, opacity: 0.8 }}>
              {t('vitalsVal.refHr', 'HR de referência (oxímetro/pulso 15 s×4):')}{' '}
              <input inputMode="decimal" placeholder="bpm" value={hrAlt}
                onChange={(e) => setHrAlt(e.target.value)} style={{ width: 64, fontSize: 12, padding: '3px 6px' }} />
            </label>
          )}
          <input placeholder={t('vitalsVal.posPlaceholder', 'posição (ex.: sentado, 0,8 m do C5)')} value={position}
            onChange={(e) => setPosition(e.target.value)} style={{ flex: '1 1 200px', minWidth: 160, fontSize: 12, padding: '3px 6px' }} />
        </div>
      )}

      {/* Controles / contador */}
      {state === 'idle' && (
        <button type="button" className="btn btn--primary" onClick={start} style={{ width: '100%' }}>
          <Play size={15} /> {t('vitalsVal.startSession', 'Iniciar sessão ({{dur}}s) — {{mode}}', { dur: durationS, mode: mode === 'breathing' ? t('vitalsVal.btnBreathing', 'Respiração') : t('vitalsVal.btnHeart', 'Batimento') })}
        </button>
      )}

      {state === 'running' && (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 10 }}>
          <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'baseline' }}>
            <span style={{ fontSize: 28, fontWeight: 800, fontVariantNumeric: 'tabular-nums' }}>{remaining}s</span>
            <span style={{ fontSize: 13, opacity: 0.8 }}>
              {t('vitalsVal.clicksLabel', 'cliques')}: <strong>{clickCount}</strong> · {t('vitalsVal.manualLabel', 'manual')} ≈ <strong>{fmt(liveManual)}</strong> {unit} · {t('vitalsVal.systemLabel', 'sistema')}{' '}
              <strong>{liveLabel}</strong>
            </span>
          </div>
          <button type="button" onClick={registerClick}
            style={{
              width: '100%', padding: '22px 0', fontSize: 18, fontWeight: 800, cursor: 'pointer',
              borderRadius: 14, border: 'none', color: '#fff',
              background: mode === 'breathing' ? '#0ea5a4' : '#ef4444',
            }}>
            {mode === 'breathing' ? <Wind size={20} /> : <HeartPulse size={20} />} {t('vitalsVal.countBtn', 'Contar {{action}} (ou barra de espaço)', { action: mode === 'breathing' ? t('vitalsVal.breathingAction', 'respiração') : t('vitalsVal.heartAction', 'batimento') })}
          </button>
          <button type="button" className="btn btn--secondary" onClick={() => finish()} style={{ width: '100%' }}>
            <Square size={14} /> {t('vitalsVal.stopAndCalc', 'Parar e calcular')}
          </button>
        </div>
      )}

      {state === 'done' && result && (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 10 }}>
          <div style={{ width: '100%', overflowX: 'auto', WebkitOverflowScrolling: 'touch' }}>
            <table style={{ width: '100%', fontSize: 13, borderCollapse: 'collapse', minWidth: 280 }}>
              <thead>
                <tr style={{ textAlign: 'left', opacity: 0.7, fontSize: 12 }}>
                  <th style={{ padding: '4px 6px' }}>{t('vitalsVal.thMetric', 'Métrica')}</th>
                  <th style={{ padding: '4px 6px' }}>{t('vitalsVal.thManual', 'Manual')}</th>
                  <th style={{ padding: '4px 6px' }}>{t('vitalsVal.thSystem', 'Sistema (CSI)')}</th>
                  <th style={{ padding: '4px 6px' }}>Δ</th>
                </tr>
              </thead>
              <tbody>
                {result.mode === 'breathing' ? (
                  <ComparisonRow
                    label={`${t('vitalsVal.btnBreathing', 'Respiração')} (${unit})`}
                    manual={result.manual.breathing_rate_median}
                    est={result.estimate_summary.breathing_rate_median}
                    tol={3}
                  />
                ) : (
                  <ComparisonRow
                    label={`${t('vitalsVal.btnHeart', 'Batimento')} (${unit})`}
                    manual={result.manual.heart_rate_bpm}
                    est={result.estimate_summary.heart_rate_bpm_median}
                    tol={8}
                  />
                )}
              </tbody>
            </table>
          </div>
          <p style={{ fontSize: 11, opacity: 0.65, margin: 0 }}>
            {t('vitalsVal.samplesSummary', '{{count}} amostras do sistema · confiança média {{conf}}%', { count: result.estimate_summary.n_samples, conf: fmt((result.estimate_summary.confidence_mean ?? 0) * 100, 0) })}
            {result.manual.breathing_rate_count !== null && result.mode === 'breathing' && (
              <> · {t('vitalsVal.manualCountSummary', 'manual por contagem {{val}} {{unit}}', { val: fmt(result.manual.breathing_rate_count), unit })}</>
            )}
          </p>

          <VitalsAutotestChart result={result} apiBaseUrl={apiBaseUrl} />

          {saveStatus && (
            <p style={{ fontSize: 11, opacity: 0.75, margin: 0, fontFamily: 'monospace' }}>{saveStatus}</p>
          )}
          <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
            <button type="button" className="btn btn--secondary" onClick={exportJSON} style={{ flex: '1 1 140px' }} title={t('vitalsVal.backupNote', 'Backup manual — a sessão já foi salva automaticamente em resultados/')}>
              <Download size={15} /> {t('vitalsVal.downloadJsonBtn', 'Baixar dados (JSON)')}
            </button>
            <button
              type="button"
              className="btn btn--secondary"
              onClick={() => void exportPlot('vitals-autotest-chart', `vitals_chart_${result.mode}_${result.started_at_iso.replace(/[:.]/g, '-')}`)}
              style={{ flex: '1 1 140px' }}
              title={t('vitalsVal.downloadChartNote', 'Baixar gráfico de comparação como imagem PNG')}>
              <Download size={15} /> {t('vitalsVal.downloadPngBtn', 'Baixar gráfico (PNG)')}
            </button>
            <button type="button" className="btn btn--secondary" onClick={() => setState('idle')} style={{ flex: '1 1 140px' }}>
              <RotateCcw size={15} /> {t('vitalsVal.newSessionBtn', 'Nova sessão')}
            </button>
          </div>
        </div>
      )}
    </section>
  );
}

function ComparisonRow({ label, manual, est, tol }: { label: string; manual: number | null; est: number | null; tol: number }) {
  const d = isNum(manual) && isNum(est) ? est - manual : null;
  return (
    <tr style={{ borderTop: '1px solid rgba(148,163,184,0.2)' }}>
      <td style={{ padding: '6px 6px' }}>{label}</td>
      <td style={{ padding: '6px 6px' }}><strong>{fmt(manual)}</strong></td>
      <td style={{ padding: '6px 6px' }}><strong>{fmt(est)}</strong></td>
      <td style={{ padding: '6px 6px', color: d === null ? 'inherit' : Math.abs(d) <= tol ? '#16a34a' : '#ef4444' }}>
        {d === null ? '—' : `${d > 0 ? '+' : ''}${d.toFixed(1)}`}
      </td>
    </tr>
  );
}

function VitalsAutotestChart({ result, apiBaseUrl }: { result: SessionExport; apiBaseUrl?: string }) {
  const { t } = useTranslation();
  const plotRef = useRef<HTMLDivElement>(null);
  const isInitialized = useRef(false);

  useEffect(() => {
    const plotElement = plotRef.current;
    if (!plotElement) return;

    const unit = result.mode === 'breathing' ? 'irpm' : 'bpm';
    const mainColor = result.mode === 'breathing' ? '#0ea5a4' : '#ef4444';

    const xVals = result.estimate_samples.map((s) => s.t_s);
    const yVals = result.estimate_samples.map((s) =>
      result.mode === 'breathing' ? s.breathing_rate ?? null : s.heart_rate_bpm ?? null,
    );
    const yValsMl = result.estimate_samples.map((s) =>
      result.mode === 'breathing' ? s.breathing_rate_ml ?? null : s.heart_rate_bpm_ml ?? null,
    );
    const confVals = result.estimate_samples.map((s) => (s.confidence ?? 0) * 100);

    const manualVal =
      result.mode === 'breathing'
        ? (result.manual.breathing_rate_median ?? result.manual.breathing_rate_count)
        : result.manual.heart_rate_bpm;

    const traces: Plotly.Data[] = [
      {
        x: xVals,
        y: yVals,
        type: 'scatter',
        mode: 'lines+markers',
        line: { color: mainColor, width: 2.5, shape: 'spline' },
        marker: { size: 5, color: mainColor },
        name: t('vitalsVal.traceFft', 'Sistema FFT (CSI)'),
        hovertemplate: `<b>FFT:</b> %{y:.1f} ${unit}<br><b>${t('vitalsVal.timeLabel', 'Tempo')}:</b> %{x:.1f}s<extra></extra>`,
      } as Plotly.Data,
      {
        x: xVals,
        y: yValsMl,
        type: 'scatter',
        mode: 'lines+markers',
        line: { color: result.mode === 'breathing' ? '#10b981' : '#6366f1', width: 2.5, shape: 'spline' },
        marker: { size: 5, color: result.mode === 'breathing' ? '#10b981' : '#6366f1' },
        name: t('vitalsVal.traceIa', 'Sistema IA (ONNX)'),
        hovertemplate: `<b>IA:</b> %{y:.1f} ${unit}<br><b>${t('vitalsVal.timeLabel', 'Tempo')}:</b> %{x:.1f}s<extra></extra>`,
      } as Plotly.Data,
      {
        x: xVals,
        y: confVals,
        type: 'scatter',
        mode: 'lines',
        fill: 'tozeroy',
        line: { color: 'rgba(148, 163, 184, 0.35)', width: 1, dash: 'dot' },
        fillcolor: 'rgba(148, 163, 184, 0.1)',
        name: t('vitalsVal.traceConf', 'Confiança (%)'),
        yaxis: 'y2',
        hovertemplate: `<b>${t('vitalsVal.traceConf', 'Confiança (%)')}:</b> %{y:.0f}%<extra></extra>`,
      } as Plotly.Data,
    ];

    if (manualVal !== null && isNum(manualVal)) {
      traces.push({
        x: [0, result.duration_s],
        y: [manualVal, manualVal],
        type: 'scatter',
        mode: 'lines',
        line: { color: '#22c55e', width: 2, dash: 'dash' },
        name: `${t('vitalsVal.thManual', 'Manual')} (${manualVal.toFixed(1)} ${unit})`,
        hovertemplate: `<b>${t('vitalsVal.thManual', 'Manual')}:</b> ${manualVal.toFixed(1)} ${unit}<extra></extra>`,
      } as Plotly.Data);
    }

    if (result.target_rpm !== null && isNum(result.target_rpm)) {
      traces.push({
        x: [0, result.duration_s],
        y: [result.target_rpm, result.target_rpm],
        type: 'scatter',
        mode: 'lines',
        line: { color: '#f59e0b', width: 1.5, dash: 'dot' },
        name: `${t('vitalsVal.traceTarget', 'Alvo')} (${result.target_rpm} ${unit})`,
        hovertemplate: `<b>${t('vitalsVal.traceTarget', 'Alvo')}:</b> ${result.target_rpm} ${unit}<extra></extra>`,
      } as Plotly.Data);
    }

    const clickSecs = result.click_timestamps_ms.map((ms) => ms / 1000);
    if (clickSecs.length > 0) {
      const clickY = manualVal ?? result.target_rpm ?? (result.mode === 'breathing' ? 15 : 70);
      traces.push({
        x: clickSecs,
        y: clickSecs.map(() => clickY),
        type: 'scatter',
        mode: 'markers',
        marker: { symbol: 'diamond', size: 9, color: '#22c55e', line: { color: '#ffffff', width: 1.5 } },
        name: `${t('vitalsVal.clicksLabel', 'cliques')} (${clickSecs.length})`,
        hovertemplate: `<b>${t('vitalsVal.clickManualHover', 'Clique Manual')}</b><br>${t('vitalsVal.timeLabel', 'Tempo')}: %{x:.1f}s<extra></extra>`,
      } as Plotly.Data);
    }

    const layout = {
      autosize: true,
      height: 250,
      margin: { t: 30, r: 40, b: 35, l: 45 },
      paper_bgcolor: 'transparent',
      plot_bgcolor: 'transparent',
      font: { color: '#94a3b8', family: 'inherit', size: 11 },
      legend: { orientation: 'h', y: 1.22, x: 0, bgcolor: 'transparent' },
      xaxis: {
        title: { text: t('vitalsVal.xAxisTitle', 'Tempo (s)'), font: { size: 11 } },
        gridcolor: 'rgba(148, 163, 184, 0.12)',
        zerolinecolor: 'rgba(148, 163, 184, 0.25)',
        range: [0, result.duration_s],
      },
      yaxis: {
        title: { text: result.mode === 'breathing' ? `${t('vitalsVal.btnBreathing', 'Respiração')} (irpm)` : `${t('vitalsVal.btnHeart', 'Batimento')} (bpm)`, font: { size: 11 } },
        gridcolor: 'rgba(148, 163, 184, 0.12)',
        zerolinecolor: 'rgba(148, 163, 184, 0.25)',
      },
      yaxis2: {
        title: { text: t('vitalsVal.confAxis', 'Confiança (%)'), font: { size: 10 } },
        overlaying: 'y',
        side: 'right',
        range: [0, 105],
        showgrid: false,
        zeroline: false,
      },
      hovermode: 'x unified',
    } as Partial<Plotly.Layout>;

    const config = { displayModeBar: false, responsive: true };

    const renderPlot = async () => {
      if (!isInitialized.current) {
        await Plotly.newPlot(plotElement, traces, layout, config);
        isInitialized.current = true;
      } else {
        await Plotly.react(plotElement, traces, layout, config);
      }

      if (apiBaseUrl !== undefined && plotElement) {
        setTimeout(() => {
          void (async () => {
            try {
              const dataUrl = await Plotly.toImage(plotElement, {
                format: 'png',
                width: 900,
                height: 450,
              });
              const res = await apiFetch(dataUrl);
              const blob = await res.blob();
              await apiFetch(`${apiBaseUrl}/api/results/vitais-autoteste/${result.session_id}/comparison.png`, {
                method: 'POST',
                headers: { 'Content-Type': 'image/png' },
                body: blob,
              });
            } catch (e) {
              console.error('Falha ao salvar gráfico PNG automaticamente no backend:', e);
            }
          })();
        }, 500);
      }
    };

    void renderPlot();

    return () => {
      if (plotElement) {
        Plotly.purge(plotElement);
      }
      isInitialized.current = false;
    };
  }, [result, apiBaseUrl]);

  return (
    <div style={{ marginTop: 12, padding: '10px 6px', background: 'rgba(0,0,0,0.2)', borderRadius: 10, border: '1px solid rgba(148,163,184,0.18)' }}>
      <div id="vitals-autotest-chart" ref={plotRef} style={{ width: '100%', minHeight: 250 }} />
    </div>
  );
}
