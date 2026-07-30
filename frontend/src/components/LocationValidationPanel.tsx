import { useCallback, useEffect, useRef, useState } from 'react';
import { Check, Crosshair, Download, MapPin, RotateCcw, Trash2, UserX, X } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { latLngToPolar } from '../utils/coords';
import { apiFetch } from '../utils/apiClient';

// ── Tipos do snapshot inferido (subconjunto de /api/detection) ───────────────
interface InferredPerson {
  lat?: number;
  lng?: number;
  confidence?: number;
}
interface DetectionSnap {
  presence?: { detected?: boolean; confidence?: number };
  location?: {
    lat?: number;
    lng?: number;
    accuracy_m?: number;
    confidence?: number;
    nodes_used?: number;
    method?: string;
  };
  occupancy?: { available?: boolean; count?: number; persons?: InferredPerson[] };
}

type Verdict = 'tp' | 'fp' | 'fn';

interface ValRecord {
  id: string;
  verdict: Verdict;
  t_iso: string;
  inferred?: { lat: number; lng: number; confidence?: number; label: string };
  ground_truth?: { lat: number; lng: number };
  error_m?: number;
  method?: string;
  nodes_used?: number;
}

// JSON exportado por sessão — ingerido por tools/location_validation_report.py.
interface LocationValidationExport {
  schema: 'aethersense.location_validation.v1';
  session_id: string;
  started_at_iso: string;
  records: ValRecord[];
  summary: {
    n_true_positives: number;
    n_false_positives: number;
    n_false_negatives: number;
    precision: number | null;
    recall: number | null;
    n_errors: number;
    mean_error_m: number | null;
    median_error_m: number | null;
    p50_error_m: number | null;
    p95_error_m: number | null;
  };
  notes: string;
}

export interface LatLngPick {
  lat: number;
  lng: number;
  seq: number;
}

interface Props {
  apiBaseUrl: string;
  // Coordenação com o mapa (estado no App): pickActive liga o modo de coleta;
  // onRequestPick pede um clique; lastPick chega quando o usuário clica no mapa;
  // onGtMarkersChange informa quais marcadores de verdade-terra desenhar.
  pickActive: boolean;
  onRequestPick: () => void;
  onCancelPick: () => void;
  lastPick: LatLngPick | null;
  onGtMarkersChange: (markers: { id: string; lat: number; lng: number; label?: string }[]) => void;
}

const isNum = (v: unknown): v is number => typeof v === 'number' && Number.isFinite(v);
const hasCoords = (lat?: number, lng?: number) =>
  isNum(lat) && isNum(lng) && (Math.abs(lat) > 1e-9 || Math.abs(lng) > 1e-9);
const fmt = (v: number | null | undefined, d = 2) => (isNum(v) ? v.toFixed(d) : '—');

function median(values: number[]): number | null {
  const a = values.filter(isNum).slice().sort((x, y) => x - y);
  if (!a.length) return null;
  const m = Math.floor(a.length / 2);
  return a.length % 2 ? a[m] : (a[m - 1] + a[m]) / 2;
}
function mean(values: number[]): number | null {
  const a = values.filter(isNum);
  return a.length ? a.reduce((s, x) => s + x, 0) / a.length : null;
}
function percentile(values: number[], p: number): number | null {
  const a = values.filter(isNum).slice().sort((x, y) => x - y);
  if (!a.length) return null;
  const idx = (a.length - 1) * p;
  const lo = Math.floor(idx);
  const hi = Math.ceil(idx);
  return lo === hi ? a[lo] : a[lo] + (a[hi] - a[lo]) * (idx - lo);
}

const card: React.CSSProperties = {
  background: 'var(--card-bg, rgba(255,255,255,0.04))',
  border: '1px solid var(--border, rgba(148,163,184,0.25))',
  borderRadius: 12,
  padding: '12px 14px',
};

let recordSeq = 0;
const newId = () => {
  recordSeq += 1;
  return `loc_${recordSeq}_${Math.round(performance.now())}`;
};

/**
 * Validação da localização georreferenciada no mapa. O operador confirma (✓) ou
 * nega (✗) cada detecção inferida e marca a posição REAL clicando no mapa (erro
 * em metros por Haversine). Calcula TP/FP/FN + precisão/recall + percentis do
 * erro e exporta a sessão em JSON. Tudo client-side (nenhuma mudança de backend).
 */
export function LocationValidationPanel({
  apiBaseUrl,
  pickActive,
  onRequestPick,
  onCancelPick,
  lastPick,
  onGtMarkersChange,
}: Props) {
  const { t } = useTranslation();
  const [det, setDet] = useState<DetectionSnap | null>(null);
  const [records, setRecords] = useState<ValRecord[]>([]);
  const [notes, setNotes] = useState('');
  const [saveStatus, setSaveStatus] = useState<string | null>(null);
  const startedIsoRef = useRef<string>(new Date().toISOString());
  const saveTimerRef = useRef<number | null>(null);
  // Flush no unmount: o painel desmonta a cada troca de aba; sem isso, os
  // registros dos últimos ~1,2 s (debounce) seriam perdidos em silêncio.
  const payloadRef = useRef<string | null>(null);
  const dirtyRef = useRef(false);
  const hasSavedRef = useRef(false);
  const apiBaseUrlRef = useRef(apiBaseUrl);
  apiBaseUrlRef.current = apiBaseUrl;

  // Pendência de clique: o que fazer quando o próximo pick do mapa chegar.
  const pendingRef = useRef<
    | { mode: 'confirm'; inferred: { lat: number; lng: number; confidence?: number; label: string } }
    | { mode: 'fn' }
    | null
  >(null);
  const lastPickSeqRef = useRef<number>(lastPick?.seq ?? 0);

  // Polling do snapshot inferido (mesma cadência do ValidationPanel).
  useEffect(() => {
    let active = true;
    const poll = async () => {
      try {
        const r = await apiFetch(`${apiBaseUrl}/api/detection`);
        if (r.ok && active) setDet((await r.json()) as DetectionSnap);
      } catch {
        /* mantém o último snapshot */
      }
    };
    void poll();
    const id = window.setInterval(poll, 1200);
    return () => {
      active = false;
      window.clearInterval(id);
    };
  }, [apiBaseUrl]);

  // Marcadores de verdade-terra para o mapa desenhar.
  useEffect(() => {
    onGtMarkersChange(
      records
        .filter((r) => r.ground_truth)
        .map((r) => ({
          id: r.id,
          lat: r.ground_truth!.lat,
          lng: r.ground_truth!.lng,
          label: r.verdict === 'fn' ? t('locVal.realUndetected', 'real (não detectada)') : t('locVal.real', 'real'),
        })),
    );
  }, [records, onGtMarkersChange, t]);

  const addRecord = useCallback((rec: ValRecord) => setRecords((prev) => [...prev, rec]), []);

  // Consome o clique do mapa quando chega (seq incrementa).
  useEffect(() => {
    if (!lastPick || lastPick.seq === lastPickSeqRef.current) return;
    lastPickSeqRef.current = lastPick.seq;
    const pending = pendingRef.current;
    pendingRef.current = null;
    if (!pending) return;

    if (pending.mode === 'confirm') {
      const err = latLngToPolar(lastPick.lat, lastPick.lng, pending.inferred.lat, pending.inferred.lng).distance;
      addRecord({
        id: newId(),
        verdict: 'tp',
        t_iso: new Date().toISOString(),
        inferred: pending.inferred,
        ground_truth: { lat: lastPick.lat, lng: lastPick.lng },
        error_m: err,
        method: det?.location?.method,
        nodes_used: det?.location?.nodes_used,
      });
    } else {
      addRecord({
        id: newId(),
        verdict: 'fn',
        t_iso: new Date().toISOString(),
        ground_truth: { lat: lastPick.lat, lng: lastPick.lng },
      });
    }
  }, [lastPick, addRecord, det]);

  // Detecções inferidas ATUAIS (presença heurística + pessoas do ML).
  const currentInferred: { key: string; label: string; lat: number; lng: number; confidence?: number }[] = [];
  if (det?.presence?.detected && hasCoords(det.location?.lat, det.location?.lng)) {
    currentInferred.push({
      key: 'presence',
      label: t('locVal.presenceHeuristic', 'Presença (heurística)'),
      lat: det.location!.lat as number,
      lng: det.location!.lng as number,
      confidence: det.location?.confidence ?? det.presence?.confidence,
    });
  }
  (det?.occupancy?.persons ?? []).forEach((p, i) => {
    if (hasCoords(p.lat, p.lng)) {
      currentInferred.push({
        key: `person-${i}`,
        label: t('locVal.personMl', 'Pessoa {{num}} (ML)', { num: i + 1 }),
        lat: p.lat as number,
        lng: p.lng as number,
        confidence: p.confidence,
      });
    }
  });

  const confirmDetection = (inf: { lat: number; lng: number; confidence?: number; label: string }, verdict: 'tp' | 'fp') => {
    addRecord({
      id: newId(),
      verdict,
      t_iso: new Date().toISOString(),
      inferred: inf,
      method: det?.location?.method,
      nodes_used: det?.location?.nodes_used,
    });
  };

  const markRealFor = (inf: { lat: number; lng: number; confidence?: number; label: string }) => {
    pendingRef.current = { mode: 'confirm', inferred: inf };
    onRequestPick();
  };
  const markFalseNegative = () => {
    pendingRef.current = { mode: 'fn' };
    onRequestPick();
  };
  const cancelPick = () => {
    pendingRef.current = null;
    onCancelPick();
  };

  // Métricas ao vivo.
  const tp = records.filter((r) => r.verdict === 'tp').length;
  const fp = records.filter((r) => r.verdict === 'fp').length;
  const fn = records.filter((r) => r.verdict === 'fn').length;
  const precision = tp + fp > 0 ? tp / (tp + fp) : null;
  const recall = tp + fn > 0 ? tp / (tp + fn) : null;
  const errs = records.map((r) => r.error_m).filter(isNum) as number[];
  const meanErr = mean(errs);
  const medErr = median(errs);
  const p95Err = percentile(errs, 0.95);

  const buildExport = (): LocationValidationExport => ({
    schema: 'aethersense.location_validation.v1',
    session_id: `${startedIsoRef.current}_loc`,
    started_at_iso: startedIsoRef.current,
    records,
    summary: {
      n_true_positives: tp,
      n_false_positives: fp,
      n_false_negatives: fn,
      precision,
      recall,
      n_errors: errs.length,
      mean_error_m: meanErr,
      median_error_m: medErr,
      p50_error_m: percentile(errs, 0.5),
      p95_error_m: p95Err,
    },
    notes: notes.trim(),
  });

  // Auto-save: persiste a sessão em resultados/validacao-localizacao/ (backend)
  // a cada mudança nos registros/notas, com debounce. Idempotente por
  // session_id — re-salvar sobrescreve o mesmo session.json. Uma lista VAZIA
  // ainda é salva quando a sessão já existe em disco (exclusão do último
  // registro precisa chegar ao arquivo, senão o FP apagado viraria métrica).
  useEffect(() => {
    if (records.length === 0 && !hasSavedRef.current) return;
    payloadRef.current = JSON.stringify(buildExport());
    dirtyRef.current = true;
    if (saveTimerRef.current) window.clearTimeout(saveTimerRef.current);
    saveTimerRef.current = window.setTimeout(() => {
      const body = payloadRef.current;
      if (!body) return;
      void (async () => {
        try {
          const r = await apiFetch(`${apiBaseUrl}/api/results/validacao-localizacao`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body,
          });
          const j = await r.json();
          if (j.ok) {
            hasSavedRef.current = true;
            dirtyRef.current = false;
          }
          setSaveStatus(j.ok ? t('locVal.savedAt', '✓ salvo em {{path}}', { path: j.path }) : `⚠ ${j.message}`);
        } catch {
          setSaveStatus(t('locVal.autoSaveFailed', '⚠ falha ao salvar automaticamente — use o download como backup'));
        }
      })();
    }, 1200);
    return () => {
      if (saveTimerRef.current) window.clearTimeout(saveTimerRef.current);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [records, notes, apiBaseUrl]);

  // Flush síncrono no unmount (troca de aba / colapso): se há mudanças ainda
  // não persistidas, envia com keepalive para o POST sobreviver ao desmonte.
  useEffect(() => {
    return () => {
      if (dirtyRef.current && payloadRef.current) {
        void apiFetch(`${apiBaseUrlRef.current}/api/results/validacao-localizacao`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: payloadRef.current,
          keepalive: true,
        }).catch(() => {});
      }
    };
  }, []);

  const exportJSON = () => {
    const blob = new Blob([JSON.stringify(buildExport(), null, 2)], { type: 'application/json' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    const stamp = startedIsoRef.current.replace(/[:.]/g, '-');
    a.href = url;
    a.download = `location_validation_${stamp}.json`;
    a.click();
    URL.revokeObjectURL(url);
  };

  const resetSession = () => {
    // Zera o estado de persistência ANTES de limpar os registros: o efeito de
    // auto-save vê records=[] + hasSaved=false e não grava sessão nova vazia.
    hasSavedRef.current = false;
    dirtyRef.current = false;
    payloadRef.current = null;
    setRecords([]);
    setNotes('');
    setSaveStatus(null);
    startedIsoRef.current = new Date().toISOString();
    pendingRef.current = null;
    onCancelPick();
  };

  return (
    <section style={{ ...card }} aria-label={t('locVal.ariaLabel', 'Validação de localização')}>
      <h4 style={{ margin: '0 0 4px', display: 'flex', alignItems: 'center', gap: 6 }}>
        <Crosshair size={15} /> {t('locVal.panelTitle', 'Validação da localização no mapa')}
      </h4>
      <p style={{ fontSize: 11, opacity: 0.65, margin: '0 0 10px' }}>
        {t('locVal.panelDesc', 'Confirme (✓) ou negue (✗) cada detecção inferida e marque a posição real no mapa para medir o erro (m). O sistema calcula precisão/recall e o erro por percentil.')}
      </p>

      {pickActive && (
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'space-between',
            gap: 8,
            marginBottom: 10,
            padding: '8px 10px',
            borderRadius: 8,
            background: 'rgba(245,158,11,0.14)',
            border: '1px solid rgba(245,158,11,0.5)',
            fontSize: 12,
          }}
        >
          <span>
            <MapPin size={13} /> {t('locVal.clickMapPrompt', 'Clique no mapa para marcar a posição real…')}
          </span>
          <button type="button" className="btn btn--secondary" onClick={cancelPick} style={{ fontSize: 11, padding: '3px 8px' }}>
            <X size={12} /> {t('locVal.cancel', 'Cancelar')}
          </button>
        </div>
      )}

      <div style={{ marginBottom: 10 }}>
        <div style={{ fontSize: 12, fontWeight: 700, opacity: 0.8, marginBottom: 4 }}>
          {t('locVal.inferredNow', 'Detecções inferidas agora ({{count}})', { count: currentInferred.length })}
        </div>
        {currentInferred.length === 0 ? (
          <p style={{ fontSize: 11, opacity: 0.6, margin: 0 }}>{t('locVal.noGeoDetections', 'Nenhuma detecção georreferenciada no snapshot atual.')}</p>
        ) : (
          <div style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
            {currentInferred.map((inf) => (
              <div
                key={inf.key}
                style={{
                  display: 'flex',
                  alignItems: 'center',
                  justifyContent: 'space-between',
                  gap: 6,
                  padding: '6px 8px',
                  borderRadius: 8,
                  border: '1px solid var(--border, rgba(148,163,184,0.25))',
                }}
              >
                <span style={{ fontSize: 12, minWidth: 0 }}>
                  <strong>{inf.label}</strong>
                  {isNum(inf.confidence) && <> · {(inf.confidence * 100).toFixed(0)}%</>}
                  <br />
                  <span style={{ fontSize: 10.5, opacity: 0.6, fontFamily: 'monospace' }}>
                    {inf.lat.toFixed(6)}, {inf.lng.toFixed(6)}
                  </span>
                </span>
                <span style={{ display: 'flex', gap: 4, flexShrink: 0 }}>
                  <button
                    type="button"
                    className="btn btn--secondary"
                    title={t('locVal.truePositiveTitle', 'Existe mesmo (verdadeiro positivo)')}
                    onClick={() => confirmDetection(inf, 'tp')}
                    style={{ padding: '4px 7px', color: '#16a34a' }}
                  >
                    <Check size={14} />
                  </button>
                  <button
                    type="button"
                    className="btn btn--secondary"
                    title={t('locVal.falsePositiveTitle', 'Falso positivo (não há ninguém aqui)')}
                    onClick={() => confirmDetection(inf, 'fp')}
                    style={{ padding: '4px 7px', color: '#ef4444' }}
                  >
                    <X size={14} />
                  </button>
                  <button
                    type="button"
                    className="btn btn--secondary"
                    title={t('locVal.markRealTitle', 'Marcar a posição real no mapa (mede o erro)')}
                    onClick={() => markRealFor(inf)}
                    style={{ padding: '4px 7px' }}
                  >
                    <MapPin size={14} />
                  </button>
                </span>
              </div>
            ))}
          </div>
        )}
        <button
          type="button"
          className="btn btn--secondary"
          onClick={markFalseNegative}
          style={{ width: '100%', marginTop: 6, fontSize: 12 }}
        >
          <UserX size={14} /> {t('locVal.falseNegativeBtn', 'Falso negativo — marcar pessoa real não detectada')}
        </button>
      </div>

      <div
        style={{
          display: 'grid',
          gridTemplateColumns: 'repeat(3, 1fr)',
          gap: 6,
          marginBottom: 10,
          fontSize: 12,
        }}
      >
        <Metric label={t('locVal.tpFpFn', 'TP / FP / FN')} value={`${tp} / ${fp} / ${fn}`} />
        <Metric label={t('locVal.precision', 'Precisão')} value={precision === null ? '—' : `${(precision * 100).toFixed(0)}%`} />
        <Metric label={t('locVal.recall', 'Recall')} value={recall === null ? '—' : `${(recall * 100).toFixed(0)}%`} />
        <Metric label={t('locVal.meanErr', 'Erro médio')} value={meanErr === null ? '—' : `${fmt(meanErr)} m`} />
        <Metric label={t('locVal.medErr', 'Erro mediano')} value={medErr === null ? '—' : `${fmt(medErr)} m`} />
        <Metric label={t('locVal.p95Err', 'Erro p95')} value={p95Err === null ? '—' : `${fmt(p95Err)} m`} />
      </div>

      {records.length > 0 && (
        <div style={{ maxHeight: 160, overflowY: 'auto', marginBottom: 10 }}>
          <table style={{ width: '100%', fontSize: 11.5, borderCollapse: 'collapse' }}>
            <thead>
              <tr style={{ textAlign: 'left', opacity: 0.7 }}>
                <th style={{ padding: '3px 4px' }}>{t('locVal.thType', 'Tipo')}</th>
                <th style={{ padding: '3px 4px' }}>{t('locVal.thDetection', 'Detecção')}</th>
                <th style={{ padding: '3px 4px' }}>{t('locVal.thError', 'Erro')}</th>
                <th style={{ padding: '3px 4px' }} />
              </tr>
            </thead>
            <tbody>
              {records.map((r) => (
                <tr key={r.id} style={{ borderTop: '1px solid rgba(148,163,184,0.2)' }}>
                  <td style={{ padding: '4px', fontWeight: 700, color: r.verdict === 'tp' ? '#16a34a' : r.verdict === 'fp' ? '#ef4444' : '#f59e0b' }}>
                    {r.verdict.toUpperCase()}
                  </td>
                  <td style={{ padding: '4px' }}>{r.inferred?.label ?? '—'}</td>
                  <td style={{ padding: '4px' }}>{isNum(r.error_m) ? `${r.error_m.toFixed(2)} m` : '—'}</td>
                  <td style={{ padding: '4px', textAlign: 'right' }}>
                    <button
                      type="button"
                      onClick={() => setRecords((prev) => prev.filter((x) => x.id !== r.id))}
                      title={t('locVal.removeRecord', 'Remover registro')}
                      style={{ background: 'none', border: 'none', color: 'var(--text-muted)', cursor: 'pointer' }}
                    >
                      <Trash2 size={13} />
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      <input
        placeholder={t('locVal.notesPlaceholder', 'notas da sessão (ambiente, cenário, observações)')}
        value={notes}
        onChange={(e) => setNotes(e.target.value)}
        style={{ width: '100%', fontSize: 12, padding: '4px 6px', marginBottom: 8 }}
      />

      {saveStatus && (
        <p style={{ fontSize: 11, opacity: 0.75, margin: '0 0 8px', fontFamily: 'monospace' }}>{saveStatus}</p>
      )}
      <div style={{ display: 'flex', gap: 8 }}>
        <button
          type="button"
          className="btn btn--secondary"
          onClick={exportJSON}
          disabled={records.length === 0}
          style={{ flex: 1 }}
          title={t('locVal.downloadBackupTitle', 'Backup manual — a sessão é salva automaticamente em resultados/ a cada registro')}
        >
          <Download size={15} /> {t('locVal.downloadJsonBtn', 'Baixar cópia (JSON)')}
        </button>
        <button type="button" className="btn btn--secondary" onClick={resetSession} style={{ flex: 1 }}>
          <RotateCcw size={15} /> {t('locVal.newSessionBtn', 'Nova sessão')}
        </button>
      </div>
    </section>
  );
}

function Metric({ label, value }: { label: string; value: string }) {
  return (
    <div style={{ padding: '6px 8px', borderRadius: 8, border: '1px solid var(--border, rgba(148,163,184,0.25))' }}>
      <div style={{ fontSize: 10, opacity: 0.6 }}>{label}</div>
      <div style={{ fontWeight: 800, fontVariantNumeric: 'tabular-nums' }}>{value}</div>
    </div>
  );
}
