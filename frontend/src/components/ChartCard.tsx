import { useCallback, useEffect, useRef } from 'react';
import { Download } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import type { CSIPacket } from '../types/csi';
import { exportPlot } from '../utils/plotlyHelper';

interface ChartCardProps {
  id: string;
  title: string;
  subtitle?: string;
  validation?: string;
  tag?: string;
  className?: string;
  isLive: boolean;
  isReplay?: boolean;
  data?: CSIPacket | null;
  hasData?: boolean;
  subscribe: (listener: (data: CSIPacket) => void) => () => void;
  onData: (data: CSIPacket, plotId: string) => void;
  initPlot: (plotId: string) => void;
  exportName?: string;
}

export function ChartCard({ id, title, subtitle, validation, tag, className = '', isLive, isReplay = false, data, hasData = false, subscribe, onData, initPlot, exportName }: ChartCardProps) {
  const { t } = useTranslation();
  const isInitialized = useRef(false);
  const renderedData = useRef<CSIPacket | null>(null);
  const plotId = `plot-${id}`;

  const pendingPacket = useRef<CSIPacket | null>(null);
  const rafId = useRef<number | null>(null);

  const flushRender = useCallback(() => {
    rafId.current = null;
    const packet = pendingPacket.current;
    if (!packet || renderedData.current === packet) return;
    renderedData.current = packet;
    onData(packet, plotId);
  }, [onData, plotId]);

  const scheduleRender = useCallback((packet: CSIPacket) => {
    pendingPacket.current = packet;
    if (rafId.current === null) {
      rafId.current = requestAnimationFrame(flushRender);
    }
  }, [flushRender]);

  useEffect(() => {
    if (!isInitialized.current) {
      initPlot(plotId);
      isInitialized.current = true;
    } else if (!isLive) {
      initPlot(plotId);
      renderedData.current = null;
    }

    const unsubscribe = subscribe((data) => {
      scheduleRender(data);
    });

    return () => {
      unsubscribe();
      if (rafId.current !== null) {
        cancelAnimationFrame(rafId.current);
        rafId.current = null;
      }
      pendingPacket.current = null;
    };
  }, [isLive, plotId, initPlot, scheduleRender, subscribe]);

  useEffect(() => {
    if (data && isInitialized.current) {
      scheduleRender(data);
    }
  }, [data, scheduleRender]);

  return (
    <div className={`bento-card ${className}`} id={`card-${id}`} style={{ padding: '16px' }}>
      <div className="chart-card__header">
        <div className="chart-card__heading">
          {tag && <span className="chart-card__tag">{tag}</span>}
          <h3 className="bento-card__title">{title}</h3>
        </div>
        <button
          className="chart-export-button"
          type="button"
          onClick={() => void exportPlot(plotId, exportName ?? title)}
          aria-label={t('chartCard.export', 'Exportar {{title}}', { title })}
          title={t('chartCard.export', 'Exportar {{title}}', { title })}
        >
          <Download size={16} />
        </button>
      </div>
      {subtitle && <p className="chart-card__subtitle">{subtitle}</p>}
      {validation && (
        <p className="chart-card__validation">
          <strong>{t('chartCard.validate', 'Validar:')}</strong> {validation}
        </p>
      )}
      <div className="chart-plot-shell">
        <div id={plotId} className="chart-plot chart-plot__surface"></div>

        {!isLive && !hasData && (
          <div className="chart-card__empty">
            <strong>{t('chartCard.noLivePackets', 'Sem pacotes CSI ao vivo')}</strong>
            <span>{isReplay ? t('chartCard.replayPaused', 'Replay CSV pausado; inicie novamente para animar esta captura.') : t('chartCard.waitingData', 'O gráfico será atualizado assim que o ESP32 enviar novos dados via UDP ou o replay CSV for iniciado.')}</span>
          </div>
        )}
      </div>
    </div>
  );
}
