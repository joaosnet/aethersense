import { useCallback, useEffect, useState } from 'react';
import { Activity, ChevronDown, ChevronUp, Crosshair, HeartPulse, MapPin, RefreshCw, Trash2, Wind } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { prepareEmptyRoomCalibration } from '../utils/emptyRoomCalibration';
import { VitalsManualValidation } from './VitalsManualValidation';
import { apiFetch } from '../utils/apiClient';

interface ValidationPanelProps {
  apiBaseUrl: string;
}

interface ZoneInfo {
  device_id: string;
  device_name: string;
  has_baseline: boolean;
  occupied: boolean;
  score: number;
  on_threshold: number;
  off_threshold: number;
}

interface DetectionResponse {
  presence?: { detected: boolean; confidence: number };
  movement?: { level: string; velocity_estimate?: number };
  vitals?: { breathing_rate?: number; heart_rate_bpm?: number; confidence?: number };
  occupancy?: {
    available: boolean;
    count: number;
    confidence: number;
    count_calibrated?: boolean;
    vitals_ml?: { breathing_rate?: number; heart_rate_bpm?: number; confidence?: number };
  };
  active_zone?: { zone_id?: string; zone_name?: string; zones?: ZoneInfo[] };
  location?: {
    method?: string;
    accuracy_m?: number;
    confidence?: number;
    nodes_used?: number;
    current_room?: string | null;
  };
}

const fmt = (v: number | undefined, digits = 1) =>
  typeof v === 'number' && Number.isFinite(v) ? v.toFixed(digits) : '—';

const delta = (estimate: number | undefined, truth: string) => {
  const t = parseFloat(truth);
  if (!Number.isFinite(t) || typeof estimate !== 'number' || !Number.isFinite(estimate)) return null;
  return estimate - t;
};

export function ValidationPanel({ apiBaseUrl }: ValidationPanelProps) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [det, setDet] = useState<DetectionResponse | null>(null);
  const [calibMsg, setCalibMsg] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [gtZone, setGtZone] = useState('');
  const [gtBreath, setGtBreath] = useState('');
  const [gtHeart, setGtHeart] = useState('');

  useEffect(() => {
    if (!open) return;
    let active = true;
    const poll = async () => {
      try {
        const r = await apiFetch(`${apiBaseUrl}/api/detection`);
        if (r.ok && active) {
          setDet((await r.json()) as DetectionResponse);
        }
      } catch {
        /* mantém o último snapshot */
      }
    };
    void poll();
    const id = window.setInterval(() => void poll(), 1200);
    return () => {
      active = false;
      window.clearInterval(id);
    };
  }, [apiBaseUrl, open]);

  const calibrate = useCallback(
    async (deviceId?: string, label?: string) => {
      setBusy(true);
      setCalibMsg(deviceId ? t('val.calibratingDevice', 'Calibrando {{label}}...', { label }) : t('val.calibratingAll', 'Calibrando sala vazia (todos os receptores)...'));
      try {
        if (!deviceId) {
          await prepareEmptyRoomCalibration(apiBaseUrl, (seconds) => {
            setCalibMsg(t(
              'val.collectingEmptyRoom',
              'Mantenha quarto, banheiro e box vazios: coletando o fundo por {{seconds}} s...',
              { seconds },
            ));
          });
        }
        const query = deviceId ? `?device_id=${encodeURIComponent(deviceId)}` : '';
        const r = await apiFetch(`${apiBaseUrl}/api/location/baseline${query}`, { method: 'POST' });
        const p = await r.json();
        if (!r.ok) throw new Error(`HTTP ${r.status}`);
        const n = Array.isArray(p?.calibrated) ? p.calibrated.length : 0;
        setCalibMsg(
          p?.ok
            ? t('val.baselineSaved', '✓ {{msg}} ({{n}} receptor(es)).', { msg: p?.message ?? 'Baseline salvo', n })
            : t('val.notEnoughPackets', '⚠ {{msg}}.', { msg: p?.message ?? 'Calibração incompleta' }),
        );
      } catch (e) {
        setCalibMsg(t('val.calibFailed', 'Falha na calibração: {{err}}', { err: e instanceof Error ? e.message : 'erro' }));
      } finally {
        setBusy(false);
      }
    },
    [apiBaseUrl, t],
  );

  const clearBaseline = useCallback(
    async (deviceId?: string, label?: string) => {
      if (!deviceId && !window.confirm(t('val.confirmClearAll', 'Apagar o baseline de sala vazia de TODOS os receptores? Será preciso recalibrar.'))) {
        return;
      }
      setBusy(true);
      setCalibMsg(deviceId ? t('val.clearingDevice', 'Apagando baseline de {{label}}...', { label }) : t('val.clearingAll', 'Apagando todos os baselines...'));
      try {
        const query = deviceId ? `?device_id=${encodeURIComponent(deviceId)}` : '';
        const r = await apiFetch(`${apiBaseUrl}/api/location/baseline${query}`, { method: 'DELETE' });
        const p = await r.json();
        if (!r.ok) throw new Error(`HTTP ${r.status}`);
        const n = Array.isArray(p?.removed) ? p.removed.length : 0;
        setCalibMsg(n > 0 ? t('val.baselineCleared', '✓ {{msg}} ({{n}}).', { msg: p?.message ?? 'Baseline apagado', n }) : t('val.noBaselineToClear', '{{msg}}.', { msg: p?.message ?? 'Nenhum baseline para apagar' }));
      } catch (e) {
        setCalibMsg(t('val.clearFailed', 'Falha ao apagar baseline: {{err}}', { err: e instanceof Error ? e.message : 'erro' }));
      } finally {
        setBusy(false);
      }
    },
    [apiBaseUrl, t],
  );

  const zones = det?.active_zone?.zones ?? [];
  const presence = det?.presence;
  const vitals = det?.vitals;
  const occupancy = det?.occupancy;
  const location = det?.location;

  const breathDelta = delta(vitals?.breathing_rate, gtBreath);
  const heartDelta = delta(vitals?.heart_rate_bpm, gtHeart);
  const estimatedZone = det?.active_zone?.zone_id;
  const zoneMatch = gtZone ? (estimatedZone === gtZone ? 'acerto' : 'erro') : null;

  const badge = (ok: boolean, on: string, off: string) => (
    <span style={{
      padding: '2px 8px', borderRadius: 999, fontSize: 12, fontWeight: 700,
      background: ok ? 'rgba(34,197,94,0.18)' : 'rgba(148,163,184,0.18)',
      color: ok ? '#16a34a' : '#64748b',
    }}>{ok ? on : off}</span>
  );

  const card: React.CSSProperties = {
    background: 'var(--card-bg, rgba(255,255,255,0.04))',
    border: '1px solid var(--border, rgba(148,163,184,0.25))',
    borderRadius: 12, padding: '12px 14px',
  };

  return (
    <section style={{ ...card, margin: '0 0 12px' }} aria-label={t('val.ariaLabel', 'Validação e calibração')}>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="btn btn--secondary"
        style={{ width: '100%', display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 8 }}
      >
        <span style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
          <Crosshair size={16} /> {t('val.panelTitle', 'Validação em campo & calibração de sala vazia')}
        </span>
        {open ? <ChevronUp size={16} /> : <ChevronDown size={16} />}
      </button>

      {open && (
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(260px, 1fr))', gap: 12, marginTop: 12 }}>
          <div style={card}>
            <h4 style={{ margin: '0 0 6px', display: 'flex', alignItems: 'center', gap: 6 }}>
              <RefreshCw size={15} /> {t('val.calibEmptyRoom', 'Calibrar sala vazia')}
            </h4>
            <p style={{ fontSize: 12, opacity: 0.75, margin: '0 0 10px' }}>
              {t('val.calibDesc', 'Deixe quarto, banheiro e box vazios e clique. O dashboard iniciará uma sessão nova, coletará o fundo e salvará os dois receptores juntos.')}
            </p>
            <button type="button" className="btn btn--primary" disabled={busy} onClick={() => void calibrate()} style={{ width: '100%', marginBottom: 8 }}>
              {busy ? t('val.calibratingBtn', 'Calibrando...') : t('val.calibrateAllBtn', 'Calibrar todos os receptores')}
            </button>
            <button type="button" className="btn btn--secondary" disabled={busy} onClick={() => void clearBaseline()}
              style={{ width: '100%', marginBottom: 8, display: 'flex', alignItems: 'center', justifyContent: 'center', gap: 6 }}>
              <Trash2 size={14} /> {t('val.clearAllBtn', 'Apagar todos os baselines')}
            </button>
            <div style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
              {zones.map((z) => (
                <div key={z.device_id} style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 8 }}>
                  <span style={{ fontSize: 13, display: 'flex', alignItems: 'center', gap: 6 }}>
                    {z.device_name} {badge(z.has_baseline, t('val.baselineOk', 'baseline ok'), t('val.noBaseline', 'sem baseline'))}
                  </span>
                  <span style={{ display: 'flex', gap: 4 }}>
                    <button type="button" className="btn btn--secondary" disabled={busy}
                      onClick={() => void calibrate(z.device_id, z.device_name)} style={{ fontSize: 12, padding: '2px 8px' }}>
                      {t('val.onlyThis', 'só este')}
                    </button>
                    {z.has_baseline && (
                      <button type="button" className="btn btn--secondary" disabled={busy} title={t('val.clearReceiverTitle', 'Apagar o baseline deste receptor')}
                        onClick={() => void clearBaseline(z.device_id, z.device_name)} style={{ fontSize: 12, padding: '2px 6px' }}>
                        <Trash2 size={12} />
                      </button>
                    )}
                  </span>
                </div>
              ))}
            </div>
            {calibMsg && <p style={{ fontSize: 12, marginTop: 10, opacity: 0.9 }}>{calibMsg}</p>}
          </div>

          <div style={card}>
            <h4 style={{ margin: '0 0 8px', display: 'flex', alignItems: 'center', gap: 6 }}>
              <Activity size={15} /> {t('val.liveDetection', 'Detecção ao vivo')}
            </h4>
            <div style={{ display: 'flex', flexDirection: 'column', gap: 6, fontSize: 13 }}>
              <div style={{ display: 'flex', justifyContent: 'space-between' }}>
                <span>{t('val.presence', 'Presença')}</span>
                <span>{badge(Boolean(presence?.detected), t('val.detected', 'detectada'), t('val.absent', 'ausente'))} <small style={{ opacity: 0.6 }}>{fmt((presence?.confidence ?? 0) * 100, 0)}%</small></span>
              </div>
              <div style={{ display: 'flex', justifyContent: 'space-between' }}>
                <span>{t('val.movement', 'Movimento')}</span><span><strong>{det?.movement?.level ?? '—'}</strong></span>
              </div>
              <div style={{ display: 'flex', justifyContent: 'space-between' }}>
                <span>{t('val.peopleMl', 'Pessoas (ML)')}</span><span><strong>{occupancy?.count ?? '—'}</strong> <small style={{ opacity: 0.6 }}>{fmt((occupancy?.confidence ?? 0) * 100, 0)}%</small>{occupancy?.available && occupancy.count_calibrated === false && (
                  <span
                    className="metric__est-badge"
                    style={{ marginLeft: 6 }}
                    title={t('val.uncalibratedTitle', 'Contagem do ML não calibrada para esta sala (modelo zero-shot)')}
                  >{t('val.estBadge', '~est')}</span>
                )}</span>
              </div>
              <div style={{ display: 'flex', justifyContent: 'space-between' }}>
                <span><MapPin size={12} /> {t('val.location', 'Localização')}</span>
                <span><small style={{ opacity: 0.7 }}>{location?.current_room ? `${location.current_room} · ` : ''}{location?.nodes_used ?? 0} {t('val.nodes', 'nós')} · {fmt(location?.accuracy_m, 2)} m</small></span>
              </div>
            </div>
            <div style={{ marginTop: 10, display: 'flex', flexDirection: 'column', gap: 6 }}>
              {zones.map((z) => {
                const range = Math.max(z.on_threshold, 1e-6);
                const pct = Math.min(100, (z.score / range) * 100);
                return (
                  <div key={z.device_id}>
                    <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: 12 }}>
                      <span>{z.device_name} {badge(z.occupied, t('val.occupied', 'ocupado'), t('val.free', 'livre'))}</span>
                      <small style={{ opacity: 0.6 }}>{fmt(z.score, 3)} / {fmt(z.on_threshold, 3)}</small>
                    </div>
                    <div style={{ height: 6, borderRadius: 4, background: 'rgba(148,163,184,0.2)', overflow: 'hidden', marginTop: 3 }}>
                      <div style={{ width: `${pct}%`, height: '100%', background: z.occupied ? '#22c55e' : '#60a5fa' }} />
                    </div>
                  </div>
                );
              })}
            </div>
          </div>

          <div style={card}>
            <h4 style={{ margin: '0 0 8px', display: 'flex', alignItems: 'center', gap: 6 }}>
              <HeartPulse size={15} /> {t('val.vitalsGt', 'Vitais × verdade-terra')}
            </h4>
            <div style={{ display: 'flex', flexDirection: 'column', gap: 8, fontSize: 13 }}>
              <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
                <span><Wind size={12} /> {t('val.breathing', 'Respiração')}</span>
                <div style={{ textAlign: 'right', fontSize: 12 }}>
                  <div>FFT: <strong>{fmt(vitals?.breathing_rate)}</strong> irpm</div>
                  <div style={{ color: '#10b981' }}>{t('val.aiLabel', 'IA:')} <strong>{fmt(occupancy?.vitals_ml?.breathing_rate)}</strong> irpm</div>
                </div>
              </div>
              <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
                <input inputMode="decimal" placeholder={t('val.realIrpm', 'real (irpm)')} value={gtBreath} onChange={(e) => setGtBreath(e.target.value)}
                  style={{ width: 90, fontSize: 12, padding: '3px 6px' }} />
                {breathDelta !== null && <small style={{ color: Math.abs(breathDelta) <= 3 ? '#16a34a' : '#ef4444' }}>Δ {breathDelta > 0 ? '+' : ''}{breathDelta.toFixed(1)} irpm</small>}
              </div>
              <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginTop: 4 }}>
                <span><HeartPulse size={12} /> {t('val.heartbeat', 'Batimento')}</span>
                <div style={{ textAlign: 'right', fontSize: 12 }}>
                  <div>FFT: <strong>{fmt(vitals?.heart_rate_bpm)}</strong> bpm <small style={{ opacity: 0.6 }}>{fmt((vitals?.confidence ?? 0) * 100, 0)}%</small></div>
                  <div style={{ color: '#6366f1' }}>{t('val.aiLabel', 'IA:')} <strong>{fmt(occupancy?.vitals_ml?.heart_rate_bpm)}</strong> bpm</div>
                </div>
              </div>
              <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
                <input inputMode="decimal" placeholder={t('val.realBpm', 'real (bpm)')} value={gtHeart} onChange={(e) => setGtHeart(e.target.value)}
                  style={{ width: 90, fontSize: 12, padding: '3px 6px' }} />
                {heartDelta !== null && <small style={{ color: Math.abs(heartDelta) <= 8 ? '#16a34a' : '#ef4444' }}>Δ {heartDelta > 0 ? '+' : ''}{heartDelta.toFixed(1)} bpm</small>}
              </div>

              <div style={{ marginTop: 6, borderTop: '1px solid rgba(148,163,184,0.2)', paddingTop: 8 }}>
                <span style={{ fontSize: 12, opacity: 0.75 }}>{t('val.whichZone', 'Em qual zona você está?')}</span>
                <select value={gtZone} onChange={(e) => setGtZone(e.target.value)} style={{ width: '100%', fontSize: 12, padding: '4px 6px', marginTop: 4 }}>
                  <option value="">{t('val.select', '— selecione —')}</option>
                  {zones.map((z) => <option key={z.device_id} value={z.device_id}>{z.device_name}</option>)}
                </select>
                {zoneMatch && (
                  <p style={{ fontSize: 12, marginTop: 6, color: zoneMatch === 'acerto' ? '#16a34a' : '#ef4444' }}>
                    {zoneMatch === 'acerto' ? t('val.zoneMatchOk', '✓ zona ativa bate com a sua') : t('val.zoneMatchErr', '✗ sistema aponta "{{zone}}"', { zone: det?.active_zone?.zone_name ?? '?' })}
                  </p>
                )}
              </div>
            </div>
            <p style={{ fontSize: 11, opacity: 0.6, marginTop: 10 }}>
              {t('val.vitalsDesc', 'Para validar vitais: fique parado/sentado perto de um receptor, respire normal, informe a taxa real (conte 30 s) e compare o Δ. Note que a FFT depende do gate de presença e 10s de buffer, enquanto a IA calcula sem trava.')}
            </p>
          </div>

          <VitalsManualValidation
            liveVitals={{
              ...vitals,
              breathing_rate_ml: occupancy?.vitals_ml?.breathing_rate,
              heart_rate_bpm_ml: occupancy?.vitals_ml?.heart_rate_bpm,
              confidence_ml: occupancy?.vitals_ml?.confidence,
            }}
            apiBaseUrl={apiBaseUrl}
          />
        </div>
      )}
    </section>
  );
}
