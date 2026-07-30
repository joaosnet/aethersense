import { Activity, BarChart3, BrainCircuit, Calculator, RadioTower } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import type { CSIPacket } from '../types/csi';
import {
  formatLocationMethod,
  formatNumber,
  formatPercent,
  formatRate,
  matrixShape,
  movementLabel,
  vitalsFromPacket,
} from '../utils/dashboardMetrics';

interface SystemPipelineProps {
  data: CSIPacket | null;
  isLive: boolean;
  isReplay?: boolean;
  packetRate: number;
}

function PipelineMetric({ label, value }: { label: string; value: string }) {
  return (
    <div className="pipeline-metric">
      <span>{label}</span>
      <strong>{value}</strong>
    </div>
  );
}

export function SystemPipeline({ data, isLive, isReplay = false, packetRate }: SystemPipelineProps) {
  const { t } = useTranslation();
  const detection = data?.detection;
  const location = detection?.location;
  const presence = detection?.presence;
  const movement = detection?.movement;
  const occupancy = detection?.occupancy;
  const vitals = vitalsFromPacket(data);
  const heatmapShape = matrixShape(data?.heatmap);
  const stftShape = matrixShape(data?.stft?.z);
  const pcaPoints = data?.pca?.packet_axis?.length ?? 0;
  const pdpPoints = data?.pdp?.x?.length ?? 0;
  const phasePoints = Math.min(data?.phase_raw?.length ?? 0, data?.phase_sanitized?.length ?? 0);

  return (
    <section className="bento-card pipeline-card">
      <div className="pipeline-card__header">
        <h3 className="bento-card__title">{t('pipeline.title', 'Pipeline real do sistema')}</h3>
        <span className={`pipeline-status ${isLive ? 'pipeline-status--live' : ''}`}>
          {isLive ? t('pipeline.statusLive', 'ao vivo') : isReplay ? t('pipeline.statusReplay', 'replay CSV') : t('pipeline.statusNoCsi', 'sem CSI')}
        </span>
      </div>

      <div className="pipeline-flow" aria-label={t('pipeline.ariaLabel', 'Pipeline real de processamento CSI')}>
        <div className="pipeline-stage">
          <div className="pipeline-stage__title">
            <RadioTower size={16} />
            {t('pipeline.stages.capture.title', 'Captura UDP')}
          </div>
          <div className="pipeline-stage__formula">{t('pipeline.stages.capture.formula', 'CSI_DATA -> parser CSV')}</div>
          <div className="pipeline-stage__grid">
            <PipelineMetric label={t('pipeline.metrics.packets', 'pacotes')} value={`${data?.count ?? '--'}`} />
            <PipelineMetric label={t('pipeline.metrics.rate', 'taxa')} value={isLive ? `${packetRate} ${t('pipeline.metrics.pktPerSec', 'pkt/s')}` : '--'} />
            <PipelineMetric label="RSSI" value={typeof data?.rssi === 'number' ? `${data.rssi} dBm` : '--'} />
            <PipelineMetric label={t('pipeline.metrics.subcarriers', 'subport.')} value={`${data?.subcarriers ?? '--'}`} />
          </div>
        </div>

        <div className="pipeline-stage">
          <div className="pipeline-stage__title">
            <Calculator size={16} />
            {t('pipeline.stages.calculation.title', 'Cálculo por pacote')}
          </div>
          <div className="pipeline-stage__formula">{t('pipeline.calcFormula', 'amp = sqrt(I^2 + Q^2) | fase = atan2(Q, I)')}</div>
          <div className="pipeline-stage__grid">
            <PipelineMetric label={t('pipeline.heatmap', 'heatmap')} value={heatmapShape} />
            <PipelineMetric label={t('pipeline.metrics.phase', 'fase')} value={`${phasePoints} pts`} />
            <PipelineMetric label={t('pipeline.metrics.amplitude', 'amplitude')} value={`${data?.subcarriers ?? 0} pts`} />
            <PipelineMetric label={t('pipeline.padding', 'padding')} value={formatNumber(data?.padding, 2)} />
          </div>
        </div>

        <div className="pipeline-stage pipeline-stage--operational">
          <div className="pipeline-stage__title">
            <Activity size={16} />
            {t('pipeline.stages.inference.title', 'Inferência operacional (Heurística FFT)')}
          </div>
          <div className="pipeline-stage__formula">{t('pipeline.fftFormula', 'var(amp), diff_t(amp), RSSI → dist, baseline, fusão, FFT')}</div>
          <div className="pipeline-stage__grid">
            <PipelineMetric label={t('pipeline.metrics.presence', 'presença')} value={presence?.detected ? t('pipeline.presence.detected', 'detectada') : data ? t('pipeline.presence.free', 'livre') : '--'} />
            <PipelineMetric label={t('pipeline.metrics.confidence', 'conf.')} value={formatPercent(presence?.confidence)} />
            <PipelineMetric label={t('pipeline.metrics.movement', 'movimento')} value={movementLabel(movement?.level)} />
            <PipelineMetric label={t('pipeline.metrics.velocity', 'vel.')} value={`${formatNumber(movement?.velocity_estimate, 2)} u/s`} />
            <PipelineMetric label={t('pipeline.metrics.nodes', 'nós')} value={`${location?.nodes_used ?? '--'}`} />
            <PipelineMetric label={t('pipeline.metrics.accuracy', 'precisão')} value={location ? `${formatNumber(location.accuracy_m, 2)} m` : '--'} />
            <PipelineMetric label={t('pipeline.metrics.locConf', 'loc conf.')} value={formatPercent(location?.confidence)} />
            <PipelineMetric label={t('pipeline.metrics.method', 'método')} value={formatLocationMethod(location)} />
            <PipelineMetric label={t('pipeline.metrics.room', 'cômodo')} value={location?.current_room ?? '--'} />
            <PipelineMetric label={t('pipeline.metrics.respFft', 'resp. FFT')} value={formatRate(vitals.breathingRate, 1, 'irpm')} />
            <PipelineMetric label={t('pipeline.metrics.batFft', 'bat. FFT')} value={formatRate(vitals.heartRateBpm, 0, 'bpm')} />
          </div>
        </div>

        <div className="pipeline-stage pipeline-stage--operational">
          <div className="pipeline-stage__title">
            <BrainCircuit size={16} />
            {t('pipeline.stages.ml.title', 'Inferência ML (ONNX - Independente de Presença)')}
          </div>
          <div className="pipeline-stage__formula">
            {occupancy?.available
              ? `CSI (1,64,50) -> CNN -> ${t('pipeline.stages.ml.countHeatmap', 'contagem + heatmap')} | ${formatNumber(occupancy.inference_ms, 1)} ms`
              : t('pipeline.stages.ml.fallback', 'modelo indisponível — fallback heurístico')}
          </div>
          <div className="pipeline-stage__grid">
            <PipelineMetric label={t('pipeline.metrics.people', 'pessoas')} value={occupancy?.available ? `${occupancy.count}` : '--'} />
            <PipelineMetric label={t('pipeline.metrics.mlConf', 'ML conf.')} value={formatPercent(occupancy?.confidence)} />
            {(occupancy?.probabilities ?? []).map((prob, idx, arr) => (
              <PipelineMetric
                key={`p${idx}`}
                label={idx === arr.length - 1 ? `P(${idx}+)` : `P(${idx})`}
                value={formatPercent(prob)}
              />
            ))}
            <PipelineMetric
              label={t('pipeline.metrics.respMl', 'resp. ML')}
              value={formatRate(occupancy?.vitals_ml?.breathing_rate ?? vitals.breathingRateMl, 1, 'irpm')}
            />
            <PipelineMetric
              label={t('pipeline.metrics.batMl', 'bat. ML')}
              value={formatRate(occupancy?.vitals_ml?.heart_rate_bpm ?? vitals.heartRateBpmMl, 0, 'bpm')}
            />
          </div>
        </div>


        <div className="pipeline-stage pipeline-stage--observability">
          <div className="pipeline-stage__title">
            <BarChart3 size={16} />
            {t('pipeline.stages.observability.title', 'Observabilidade e benchmark')}
          </div>
          <div className="pipeline-stage__formula">{t('pipeline.stages.observability.formula', 'não aciona automação diretamente')}</div>
          <div className="pipeline-stage__grid">
            <PipelineMetric label="STFT" value={stftShape} />
            <PipelineMetric label="PCA" value={`${pcaPoints} pts`} />
            <PipelineMetric label="PDP" value={`${pdpPoints} pts`} />
            <PipelineMetric label={t('pipeline.metrics.phaseSan', 'fase san.')} value={`${phasePoints} pts`} />
          </div>
        </div>
      </div>
    </section>
  );
}
