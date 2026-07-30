import { useEffect, useRef } from 'react';
import { Download } from 'lucide-react';
import Plotly from 'plotly.js-dist-min';
import { useTranslation } from 'react-i18next';
import { exportPlot, PACKET_RATE_PLOT_ID } from '../utils/plotlyHelper';

interface GaugeChartProps {
  packetRate: number;
  isLive: boolean;
  isReplay?: boolean;
}

export function GaugeChart({ packetRate, isLive, isReplay = false }: GaugeChartProps) {
  const { t } = useTranslation();
  const plotRef = useRef<HTMLDivElement | null>(null);
  const isInitialized = useRef(false);

  useEffect(() => {
    const plotElement = plotRef.current;

    if (!plotElement) {
      return;
    }

    const trace = {
      type: 'indicator',
      mode: 'gauge+number',
      value: packetRate,
      number: { suffix: t('gauge.csiPerSec', ' CSI/s'), font: { color: 'var(--text-primary)', size: 16 } },
      gauge: {
        axis: { range: [0, 25], tickwidth: 1, tickcolor: 'var(--text-muted)' },
        bar: { color: 'var(--accent-emerald)' },
        bgcolor: 'transparent',
        borderwidth: 0,
        bordercolor: 'transparent',
        steps: [
          { range: [0, 5], color: 'rgba(239, 154, 154, 0.4)' },
          { range: [5, 10], color: 'rgba(255, 183, 77, 0.4)' },
          { range: [10, 25], color: 'rgba(139, 195, 74, 0.4)' },
        ],
        threshold: {
          line: { color: 'var(--accent-indigo)', width: 4 },
          thickness: 0.75,
          value: packetRate,
        },
      },
    } as Plotly.Data;

    const layout = {
      autosize: true,
      margin: { t: 25, r: 25, l: 25, b: 25 },
      paper_bgcolor: 'transparent',
      plot_bgcolor: 'transparent',
      font: { color: 'var(--text-secondary)', family: 'var(--font)' },
    } as Partial<Plotly.Layout>;

    const config = { displayModeBar: false, responsive: true };

    const renderPlot = async () => {
      if (!isInitialized.current) {
        await Plotly.newPlot(plotElement, [trace], layout, config);
        isInitialized.current = true;
      } else {
        await Plotly.react(plotElement, [trace], layout, config);
      }
    };

    void renderPlot();
  }, [packetRate]);

  useEffect(() => {
    const plotElement = plotRef.current;

    return () => {
      if (plotElement) {
        Plotly.purge(plotElement);
      }
      isInitialized.current = false;
    };
  }, []);

  return (
    <div className="bento-card" style={{ height: '100%', display: 'flex', flexDirection: 'column' }}>
      <div className="chart-card__header">
        <div className="chart-card__heading">
          <span className="chart-card__tag">{t('gauge.tag', 'Saude do stream')}</span>
          <h3 className="bento-card__title">{t('gauge.title', 'Pacotes CSI recebidos/s')}</h3>
        </div>
        <button
          className="chart-export-button"
          type="button"
          onClick={() => void exportPlot(PACKET_RATE_PLOT_ID, t('gauge.exportTitle', 'Pacotes CSI recebidos por segundo'))}
          aria-label={t('gauge.exportLabel', 'Exportar pacotes CSI recebidos por segundo')}
          title={t('gauge.exportLabel', 'Exportar pacotes CSI recebidos por segundo')}
        >
          <Download size={16} />
        </button>
      </div>
      <p className="chart-card__subtitle">
        {isLive ? t('gauge.subtitleLive', 'Backend: janela móvel de 1 segundo') : isReplay ? t('gauge.subtitleReplay', 'Replay CSV M6: captura local em janela de 1 segundo') : t('gauge.subtitleWaiting', 'Aguardando pacotes CSI ao vivo')}
      </p>
      <p className="chart-card__validation">
        <strong>{t('gauge.validateLabel', 'Validar:')}</strong> {t('gauge.validateText', 'fluxo em torno da meta de 10 CSI/s e sem quedas para zero; oscilacao forte indica perda UDP, fila saturada ou firmware parado.')}
      </p>

      <div style={{ flex: 1, minHeight: '150px', position: 'relative' }}>
        <div id={PACKET_RATE_PLOT_ID} ref={plotRef} style={{ width: '100%', height: '100%', position: 'absolute', top: 0, left: 0 }} />
      </div>
    </div>
  );
}
