import { Activity, AlertTriangle, CheckCircle2, XCircle, Zap, ShieldAlert } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import type { CSIPacket, CSISettings } from '../types/csi';

interface FftDiagnosticsPanelProps {
  data?: CSIPacket | null;
  settings?: CSISettings;
}

export function FftDiagnosticsPanel({ data, settings }: FftDiagnosticsPanelProps) {
  const { t } = useTranslation();
  const diag = data?.detection?.fft_diagnostics;
  const vitals = data?.detection?.vitals;

  if (!diag) {
    return (
      <div className="card" style={{ padding: '16px', background: 'var(--bg-surface)', border: '1px solid var(--border)', borderRadius: '12px', marginBottom: '16px' }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: '8px', color: 'var(--text-muted)' }}>
          <Activity size={18} className="animate-spin" />
          <span style={{ fontSize: '0.85rem', fontWeight: 500 }}>{t('fftDiag.waiting', 'Aguardando pacotes de diagnóstico espectral FFT...')}</span>
        </div>
      </div>
    );
  }

  // Gates / Exigências Físicas
  const minSamples = settings?.vitals_min_samples ?? 100;
  const reqProminence = settings?.vitals_peak_prominence ?? 2.0;
  const reqSnrFloor = settings?.heart_rate_abs_snr_floor ?? 3.0;

  const gatePresence = diag.has_presence;
  const gateSamples = diag.sample_count >= minSamples;
  const gateProminence = diag.breathing_prominence >= reqProminence;
  const gateHeartSnr = diag.heart_snr >= reqSnrFloor;

  const isSuccess = Boolean(vitals?.breathing_rate || vitals?.heart_rate_bpm);
  const isBlocked = !gatePresence || !gateSamples || (!gateProminence && !gateHeartSnr);

  const formatHz = (val?: number) => val !== undefined && val > 0 ? `${val.toFixed(2)} Hz (${(val * 60).toFixed(0)} rpm/bpm)` : '—';

  return (
    <div className="card" style={{ padding: '18px', background: 'var(--bg-surface)', border: '1px solid var(--border)', borderRadius: '12px', marginBottom: '20px', boxShadow: 'var(--shadow-sm)' }}>
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: '14px', flexWrap: 'wrap', gap: '10px' }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
          <Zap size={20} style={{ color: 'var(--accent-cyan)' }} />
          <h3 style={{ margin: 0, fontSize: '1rem', fontWeight: 600, color: 'var(--text-primary)' }}>
            {t('fftDiag.title', 'Observabilidade & Exigências Físicas (Heurística FFT)')}
          </h3>
        </div>
        <div style={{
          display: 'inline-flex',
          alignItems: 'center',
          gap: '6px',
          padding: '6px 12px',
          borderRadius: '999px',
          fontSize: '0.75rem',
          fontWeight: 600,
          background: isSuccess ? 'rgba(16, 185, 129, 0.15)' : isBlocked ? 'rgba(239, 68, 68, 0.15)' : 'rgba(245, 158, 11, 0.15)',
          color: isSuccess ? '#10b981' : isBlocked ? '#ef4444' : '#f59e0b',
          border: `1px solid ${isSuccess ? 'rgba(16, 185, 129, 0.3)' : isBlocked ? 'rgba(239, 68, 68, 0.3)' : 'rgba(245, 158, 11, 0.3)'}`
        }}>
          {isSuccess ? <CheckCircle2 size={14} /> : isBlocked ? <XCircle size={14} /> : <AlertTriangle size={14} />}
          <span>{diag.status_message || (isSuccess ? t('fftDiag.validated', 'Vitais validados') : t('fftDiag.analyzing', 'Analisando canal...'))}</span>
        </div>
      </div>

      <p style={{ margin: '0 0 16px 0', fontSize: '0.8rem', color: 'var(--text-secondary)', lineHeight: 1.5 }}>
        {t('fftDiag.desc', 'A heurística FFT opera de forma estritamente condicional para evitar falsos positivos em ruído de RF. Abaixo você confere em qual exigência física o estimador pode estar retido ou validado:')}
      </p>

      {/* Grid de Gates Físicos */}
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(220px, 1fr))', gap: '12px', marginBottom: '16px' }}>
        {/* Gate 1: Presença */}
        <div style={{
          padding: '12px',
          borderRadius: '10px',
          background: gatePresence ? 'rgba(16, 185, 129, 0.08)' : 'rgba(239, 68, 68, 0.08)',
          border: `1px solid ${gatePresence ? 'rgba(16, 185, 129, 0.25)' : 'rgba(239, 68, 68, 0.25)'}`,
          display: 'flex',
          flexDirection: 'column',
          gap: '4px'
        }}>
          <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
            <span style={{ fontSize: '0.75rem', fontWeight: 600, color: 'var(--text-secondary)' }}>{t('fftDiag.gatePresence', '1. GATE DE PRESENÇA')}</span>
            {gatePresence ? <CheckCircle2 size={16} color="#10b981" /> : <XCircle size={16} color="#ef4444" />}
          </div>
          <div style={{ fontSize: '0.9rem', fontWeight: 700, color: gatePresence ? '#10b981' : '#ef4444' }}>
            {gatePresence ? t('fftDiag.activeRoom', 'Ativa no Cômodo') : t('fftDiag.emptyRoom', 'Sala Vazía (Estático)')}
          </div>
          <span style={{ fontSize: '0.7rem', color: 'var(--text-muted)' }}>{t('fftDiag.presenceReq', 'Exige perturbação no sinal sobre o baseline')}</span>
        </div>

        {/* Gate 2: Janela Temporal */}
        <div style={{
          padding: '12px',
          borderRadius: '10px',
          background: gateSamples ? 'rgba(16, 185, 129, 0.08)' : 'rgba(239, 68, 68, 0.08)',
          border: `1px solid ${gateSamples ? 'rgba(16, 185, 129, 0.25)' : 'rgba(239, 68, 68, 0.25)'}`,
          display: 'flex',
          flexDirection: 'column',
          gap: '4px'
        }}>
          <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
            <span style={{ fontSize: '0.75rem', fontWeight: 600, color: 'var(--text-secondary)' }}>{t('fftDiag.gateWindow', '2. JANELA MÍNIMA')}</span>
            {gateSamples ? <CheckCircle2 size={16} color="#10b981" /> : <XCircle size={16} color="#ef4444" />}
          </div>
          <div style={{ fontSize: '0.9rem', fontWeight: 700, color: gateSamples ? '#10b981' : '#ef4444' }}>
            {t('fftDiag.samplesCount', '{{current}} / {{min}} amostras', { current: diag.sample_count, min: minSamples })}
          </div>
          <span style={{ fontSize: '0.7rem', color: 'var(--text-muted)' }}>{t('fftDiag.effectiveRate', 'Taxa efetiva: {{rate}} Hz', { rate: diag.sample_rate_hz?.toFixed(1) ?? '—' })}</span>
        </div>

        {/* Gate 3: Proeminência Respiratória */}
        <div style={{
          padding: '12px',
          borderRadius: '10px',
          background: gateProminence ? 'rgba(16, 185, 129, 0.08)' : 'rgba(245, 158, 11, 0.08)',
          border: `1px solid ${gateProminence ? 'rgba(16, 185, 129, 0.25)' : 'rgba(245, 158, 11, 0.25)'}`,
          display: 'flex',
          flexDirection: 'column',
          gap: '4px'
        }}>
          <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
            <span style={{ fontSize: '0.75rem', fontWeight: 600, color: 'var(--text-secondary)' }}>{t('fftDiag.gateProminence', '3. PROEMINÊNCIA RESP.')}</span>
            {gateProminence ? <CheckCircle2 size={16} color="#10b981" /> : <AlertTriangle size={16} color="#f59e0b" />}
          </div>
          <div style={{ fontSize: '0.9rem', fontWeight: 700, color: gateProminence ? '#10b981' : '#f59e0b' }}>
            {diag.breathing_prominence?.toFixed(2) ?? '0.00'}x <small style={{ fontWeight: 400, fontSize: '0.75rem' }}>({t('fftDiag.min', 'mín.')} {reqProminence.toFixed(1)}x)</small>
          </div>
          <span style={{ fontSize: '0.7rem', color: 'var(--text-muted)' }}>{t('fftDiag.peak', 'Pico:')} {formatHz(diag.breathing_peak_hz)}</span>
        </div>

        {/* Gate 4: SNR Cardíaco */}
        <div style={{
          padding: '12px',
          borderRadius: '10px',
          background: gateHeartSnr ? 'rgba(16, 185, 129, 0.08)' : 'rgba(245, 158, 11, 0.08)',
          border: `1px solid ${gateHeartSnr ? 'rgba(16, 185, 129, 0.25)' : 'rgba(245, 158, 11, 0.25)'}`,
          display: 'flex',
          flexDirection: 'column',
          gap: '4px'
        }}>
          <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
            <span style={{ fontSize: '0.75rem', fontWeight: 600, color: 'var(--text-secondary)' }}>{t('fftDiag.gateHeartSnr', '4. SNR CARDÍACO (PISO)')}</span>
            {gateHeartSnr ? <CheckCircle2 size={16} color="#10b981" /> : <AlertTriangle size={16} color="#f59e0b" />}
          </div>
          <div style={{ fontSize: '0.9rem', fontWeight: 700, color: gateHeartSnr ? '#10b981' : '#f59e0b' }}>
            {diag.heart_snr?.toFixed(2) ?? '0.00'}x <small style={{ fontWeight: 400, fontSize: '0.75rem' }}>({t('fftDiag.min', 'mín.')} {reqSnrFloor.toFixed(1)}x)</small>
          </div>
          <span style={{ fontSize: '0.7rem', color: 'var(--text-muted)' }}>{t('fftDiag.peak', 'Pico:')} {formatHz(diag.heart_peak_hz)}</span>
        </div>
      </div>

      {/* Rejeição de Harmônicos */}
      {diag.rejected_harmonics && diag.rejected_harmonics.length > 0 && (
        <div style={{ padding: '10px 14px', background: 'rgba(99, 102, 241, 0.08)', border: '1px solid rgba(99, 102, 241, 0.2)', borderRadius: '8px', display: 'flex', alignItems: 'center', gap: '10px', fontSize: '0.75rem', color: 'var(--text-secondary)' }}>
          <ShieldAlert size={16} style={{ color: 'var(--accent-indigo)', flexShrink: 0 }} />
          <div>
            <strong>{t('fftDiag.spectralIsolation', 'Isolamento Espectral: Os seguintes harmônicos respiratórios foram suprimidos da busca cardíaca para evitar falso acoplamento:')}</strong> {' '}
            <span style={{ fontFamily: 'monospace', color: 'var(--accent-indigo)', fontWeight: 600 }}>
              {diag.rejected_harmonics.map(h => `${h.toFixed(2)} Hz`).join(', ')}
            </span>
          </div>
        </div>
      )}
    </div>
  );
}
