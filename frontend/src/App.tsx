import { useState, useEffect, useCallback, useRef } from 'react';
import { Download, KeyRound, Play, Square } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import i18n from './i18n/i18n';
import { useAetherWebSocket } from './hooks/useAetherWebSocket';
import type { CSISettings, StreamStatus } from './types/csi';
import { apiFetch, setApiBearerToken } from './utils/apiClient';

import { Header } from './components/Header';
import type { DashboardTab } from './components/Header';
import { Footer } from './components/Footer';
import { ControlsDrawer } from './components/ControlsDrawer';
import { AetherMap } from './components/AetherMap';
import { ValidationPanel } from './components/ValidationPanel';
import { LocationValidationPanel } from './components/LocationValidationPanel';
import { HomeAssistantPanel } from './components/HomeAssistantPanel';
import { ShowerBoxControlPanel } from './components/ShowerBoxControlPanel';
import { AccessoriesPanel } from './components/AccessoriesPanel';
import { ChartCard } from './components/ChartCard';
import { HardwareTopology } from './components/HardwareTopology';
import { GaugeChart } from './components/GaugeChart';
import { SystemPipeline } from './components/SystemPipeline';
import { FftDiagnosticsPanel } from './components/FftDiagnosticsPanel';
import { STREAM_STALE_WARNING_MS, formatStreamAge } from './utils/streamHealth';

import {
  initHeatmap, updateHeatmap,
  initSTFT, updateSTFT,
  initPhase, updatePhase,
  initPCA, updatePCA,
  initPDP, updatePDP,
  initBreathingRate, updateBreathingRate,
  initHeartRate, updateHeartRate,
  initFftSpectrum, updateFftSpectrum,
  exportPlots,
  PACKET_RATE_PLOT_ID
} from './utils/plotlyHelper';

const DEFAULT_SETTINGS: CSISettings = {
  heatmap_window: 120,
  stft_window: 80,
  pca_window: 120,
  padding: 0.15,
  topology_scenario: 3,
  vitals_window_seconds: 30.0,
  vitals_min_samples: 100,
  vitals_peak_prominence: 2.0,
  heart_rate_abs_snr_floor: 3.0,
  vitals_harmonic_count: 5,
  vitals_harmonic_tolerance_hz: 0.06,
  vitals_subcarrier_top_k: 16,
  breathing_freq_min_hz: 0.1,
  breathing_freq_max_hz: 0.5,
  heart_rate_freq_min_hz: 0.8,
  heart_rate_freq_max_hz: 3.0,
};

const SETTINGS_SAVE_DELAY_MS = 250;

type ReplayApiResponse = {
  ok: boolean;
  message?: string;
  packets?: number;
  default_file?: string | null;
  stream?: StreamStatus;
};

const getChartGroups = (t: any) => [
  {
    id: 'vitals',
    title: t('app.chartGroups.vitals.title', 'Inferência experimental'),
    description: t('app.chartGroups.vitals.description', 'Valores publicados por detection.vitals; o gráfico apenas historiza o snapshot calculado no backend.'),
  },
  {
    id: 'signal',
    title: t('app.chartGroups.signal.title', 'Sinal base'),
    description: t('app.chartGroups.signal.description', 'Amplitude CSI usada como matéria-prima para presença, movimento, baseline e localização.'),
  },
  {
    id: 'benchmark',
    title: t('app.chartGroups.benchmark.title', 'Validação M6'),
    description: t('app.chartGroups.benchmark.description', 'Vistas auxiliares para auditar estabilidade, variância e separação espectral.'),
  },
  {
    id: 'diagnostics',
    title: t('app.chartGroups.diagnostics.title', 'Observabilidade final'),
    description: t('app.chartGroups.diagnostics.description', 'Diagnósticos que ajudam a explicar o canal, mas não acionam decisão operacional diretamente.'),
  },
];

const getEngineeringCharts = (t: any) => [
  {
    id: 'breathing-rate',
    group: 'vitals',
    tag: t('app.charts.breathing.tag', 'Comparativo IA vs FFT'),
    wrapperClassName: 'chart-card-half chart-card-priority',
    title: t('app.charts.breathing.title', 'Respiração: IA Neural vs FFT'),
    subtitle: t('app.charts.breathing.subtitle', 'Série verde (ONNX continua) vs série tracejada (FFT heurística condicional).'),
    validation: t('app.charts.breathing.validation', 'Comparação lado-a-lado dos dois estimadores. Consulte o painel de Observabilidade acima para ver os gates em tempo real.'),
    initPlot: initBreathingRate,
    onData: updateBreathingRate,
  },
  {
    id: 'heart-rate',
    group: 'vitals',
    tag: t('app.charts.heart.tag', 'Comparativo IA vs FFT'),
    wrapperClassName: 'chart-card-half chart-card-priority',
    title: t('app.charts.heart.title', 'Batimentos: IA Neural vs FFT'),
    subtitle: t('app.charts.heart.subtitle', 'Série índigo/roxo (ONNX) vs série tracejada (FFT heurística).'),
    validation: t('app.charts.heart.validation', 'Comparação lado-a-lado. O piso SNR cardíaco e os harmônicos rejeitados são visíveis no gráfico espectral abaixo.'),
    initPlot: initHeartRate,
    onData: updateHeartRate,
  },
  {
    id: 'fft-spectrum',
    group: 'vitals',
    tag: t('app.charts.fftSpectrum.tag', 'Espectro 3D & Harmônicos'),
    wrapperClassName: 'chart-card-wide chart-card-3d',
    title: t('app.charts.fftSpectrum.title', 'Espectro FFT 3D: Bandas Fisiológicas & Harmônicos Rejeitados'),
    subtitle: t('app.charts.fftSpectrum.subtitle', 'Decomposição em 3D (estilo Série de Fourier) isolando em camadas: espectro bruto, banda respiratória (0.1–0.5 Hz), harmônicos respiratórios rejeitados e banda cardíaca (0.8–2.0 Hz).'),
    validation: t('app.charts.fftSpectrum.validation', 'Gire e aproxime em 3D para inspecionar o isolamento físico: os harmônicos suprimidos evitam que a busca cardíaca alucine batimentos nos múltiplos da respiração.'),
    initPlot: initFftSpectrum,
    onData: updateFftSpectrum,
  },
  {
    id: 'heatmap',
    group: 'signal',
    tag: t('app.charts.heatmap.tag', 'Sinal base'),
    wrapperClassName: 'chart-card-wide chart-card-priority',
    title: t('app.charts.heatmap.title', 'Amplitude CSI por subportadora'),
    subtitle: t('app.charts.heatmap.subtitle', 'Matriz de amplitude usada como evidencia de sinal e base para historicos operacionais.'),
    validation: t('app.charts.heatmap.validation', 'Esperado ver continuidade entre pacotes e mudancas coerentes quando ha movimento; linhas quebradas ou blocos zerados apontam perda, parser incorreto ou payload invalido.'),
    initPlot: initHeatmap,
    onData: updateHeatmap,
  },
  {
    id: 'stft',
    group: 'benchmark',
    tag: t('app.charts.stft.tag', 'Benchmark M6'),
    wrapperClassName: 'chart-card-half',
    title: t('app.charts.stft.title', 'STFT da amplitude média'),
    subtitle: t('app.charts.stft.subtitle', 'FFT em janela de Hanning para inspecao tempo-frequencia e benchmark.'),
    validation: t('app.charts.stft.validation', 'Validar separacao temporal/frequencial e faixa dinamica do M6; vazamento excessivo ou energia espalhada sugere janela ruim, ruido ou captura instavel.'),
    initPlot: initSTFT,
    onData: updateSTFT,
  },
  {
    id: 'pca',
    group: 'benchmark',
    tag: t('app.charts.pca.tag', 'Benchmark M6'),
    wrapperClassName: 'chart-card-half',
    title: t('app.charts.pca.title', 'PCA das amplitudes CSI'),
    subtitle: t('app.charts.pca.subtitle', 'Power iteration sobre a janela de amplitude; usado para analise/benchmark.'),
    validation: t('app.charts.pca.validation', 'Esperado ver componentes com trajetoria consistente; nuvem dispersa sem estrutura indica baixa separacao entre estado estatico, movimento e ruido.'),
    initPlot: initPCA,
    onData: updatePCA,
  },
  {
    id: 'phase',
    group: 'diagnostics',
    tag: t('app.charts.phase.tag', 'Observabilidade'),
    wrapperClassName: 'chart-card-half',
    title: t('app.charts.phase.title', 'Fase bruta vs sanitizada'),
    subtitle: t('app.charts.phase.subtitle', 'Observabilidade e benchmark M6; nao aciona automacao diretamente.'),
    validation: t('app.charts.phase.validation', 'Validar se a fase sanitizada reduz inclinacao e saltos da fase bruta; residuos grandes apontam CFO/SFO, unwrap fraco ou ambiente instavel.'),
    initPlot: initPhase,
    onData: updatePhase,
  },
  {
    id: 'pdp',
    group: 'diagnostics',
    tag: t('app.charts.pdp.tag', 'Observabilidade'),
    wrapperClassName: 'chart-card-half',
    title: t('app.charts.pdp.title', 'PDP por IFFT do CSI'),
    subtitle: t('app.charts.pdp.subtitle', 'Perfil de multicaminho do pacote atual; visualizacao, nao decisao operacional.'),
    validation: t('app.charts.pdp.validation', 'Esperado ver picos de multicaminho relativamente estaveis em sala parada; picos erraticos ou achatados sugerem baixa qualidade do canal ou CSI ruidoso.'),
    initPlot: initPDP,
    onData: updatePDP,
  },
];

const ENGINEERING_CHART_IDS = [
  'breathing-rate',
  'heart-rate',
  'fft-spectrum',
  'heatmap',
  'stft',
  'pca',
  'phase',
  'pdp',
];

const EXPORTABLE_PLOT_IDS = [
  ...ENGINEERING_CHART_IDS.map((id) => `plot-${id}`),
  PACKET_RATE_PLOT_ID,
];

const readStoredTheme = () => {
  try {
    return localStorage.getItem('theme') === 'dark';
  } catch {
    return false;
  }
};

const readStoredTab = (): DashboardTab => {
  try {
    const stored = localStorage.getItem('activeTab');
    if (stored === 'debug' || stored === 'calibration' || stored === 'client') {
      return stored;
    }
  } catch {
    /* ignore */
  }
  return 'client';
};

const getBackendHttpBaseUrl = () => {
  const configuredUrl = import.meta.env.VITE_BACKEND_HTTP_URL;

  if (typeof configuredUrl === 'string' && configuredUrl.trim().length > 0) {
    return configuredUrl.replace(/\/$/, '');
  }

  // Usa rota relativa por padrão para funcionar com:
  // - Vite proxy (local e docker dev)
  // - servidor frontend em produção com proxy integrado.
  return '';
};

const getBackendWebSocketBaseUrl = () => {
  const configuredUrl = import.meta.env.VITE_BACKEND_WS_URL;

  if (typeof configuredUrl === 'string' && configuredUrl.trim().length > 0) {
    return configuredUrl.replace(/\/$/, '');
  }

  const wsProtocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${wsProtocol}//${window.location.host}`;
};

const replayFileLabel = (file: string | null | undefined) => {
  if (!file) return i18n.t('app.replayLatest', 'captura M6 mais recente');
  return file.split(/[\\/]/).pop() || file;
};

export default function App() {
  const { t } = useTranslation();
  const chartGroups = getChartGroups(t);
  const engineeringCharts = getEngineeringCharts(t);

  const wsUrl = `${getBackendWebSocketBaseUrl()}/ws`;
  const apiBaseUrl = getBackendHttpBaseUrl();
  const { data, status, subscribe } = useAetherWebSocket(wsUrl);

  const [darkMode, setDarkMode] = useState(readStoredTheme);

  const [activeTab, setActiveTab] = useState<DashboardTab>(readStoredTab);

  const [settings, setSettings] = useState<CSISettings>(DEFAULT_SETTINGS);
  const [settingsFeedback, setSettingsFeedback] = useState<{ tone: 'neutral' | 'success' | 'error'; message: string } | null>(null);
  const [apiToken, setApiToken] = useState('');
  // Coordenação da validação de localização no mapa (Feature 2): o mapa entra em
  // pickMode e devolve o clique; os marcadores de verdade-terra vêm do painel.
  const [locPickActive, setLocPickActive] = useState(false);
  const [locGtMarkers, setLocGtMarkers] = useState<{ id: string; lat: number; lng: number; label?: string }[]>([]);
  const [locLastPick, setLocLastPick] = useState<{ lat: number; lng: number; seq: number } | null>(null);
  const locPickSeqRef = useRef(0);
  const handleLocMapPick = useCallback((lat: number, lng: number) => {
    locPickSeqRef.current += 1;
    setLocLastPick({ lat, lng, seq: locPickSeqRef.current });
    setLocPickActive(false);
  }, []);
  const [exportFeedback, setExportFeedback] = useState<string | null>(null);
  const [replayStatus, setReplayStatus] = useState<StreamStatus | null>(null);
  const [replayDefaultFile, setReplayDefaultFile] = useState<string | null>(null);
  const [replayFeedback, setReplayFeedback] = useState<string | null>(null);
  const [isReplayBusy, setIsReplayBusy] = useState(false);
  const settingsSaveTimer = useRef<number | null>(null);
  const exportFeedbackTimer = useRef<number | null>(null);

  const isStreamStale = status.connected && status.latencyMs >= STREAM_STALE_WARNING_MS;
  const streamStatus = data?.stream ?? replayStatus ?? status.stream;
  const isReplayActive = status.connected && streamStatus?.source === 'csv_replay' && Boolean(streamStatus.replay_active);
  const isStreamLive = status.connected && status.hasPacket && !isStreamStale && streamStatus?.source === 'udp_live';
  const hasDisplaySnapshot = Boolean(data);
  const displayPacket = data;
  const hasDataFlow = isStreamLive || isReplayActive;
  const hasChartData = Boolean(displayPacket && (displayPacket.count ?? 0) > 0);
  const alertTitle = !status.connected
    ? t('app.alert.disconnectedTitle', 'WebSocket desconectado')
    : isReplayActive
      ? t('app.alert.replayTitle', 'Replay CSV M6 ativo')
      : !status.hasPacket
        ? t('app.alert.noPacketTitle', 'Nenhum pacote CSI recebido')
        : streamStatus?.source === 'idle' && hasDisplaySnapshot
          ? t('app.alert.noFlowTitle', 'Sem fluxo ao vivo')
          : t('app.alert.udpStoppedTitle', 'Stream UDP parado');
  const alertMessage = !status.connected
    ? t('app.alert.disconnectedMsg', 'O dashboard mantém o último snapshot disponível enquanto tenta reconectar ao backend.')
    : isReplayActive
      ? t('app.alert.replayMsg', 'Mostrando {{file}} de tests/captures. Esses dados são captura local, não fluxo UDP ao vivo.', { file: replayFileLabel(streamStatus?.replay_file ?? replayDefaultFile) })
      : !status.hasPacket
        ? t('app.alert.noPacketMsg', 'O WebSocket está conectado e a configuração continua visível, mas o backend ainda não recebeu pacotes CSI há {{age}}.', { age: formatStreamAge(status.latencyMs) })
        : streamStatus?.source === 'idle' && hasDisplaySnapshot
          ? (streamStatus.reason || t('app.alert.noFlowDefaultMsg', 'Mantendo o último snapshot local até chegar UDP ao vivo ou iniciar novo replay.'))
          : t('app.alert.udpStoppedMsg', 'Sem novos pacotes há {{age}}. Se o ESP32 estiver enviando em broadcast para o Docker no Windows, confirme o IP do host como destino UDP.', { age: formatStreamAge(status.latencyMs) });
  const showStreamAlert = !status.connected || isReplayActive || !status.hasPacket || isStreamStale || (streamStatus?.source === 'idle' && hasDisplaySnapshot);

  useEffect(() => {
    try {
      document.documentElement.setAttribute('data-theme', darkMode ? 'dark' : 'light');
      localStorage.setItem('theme', darkMode ? 'dark' : 'light');
    } catch {
      document.documentElement.setAttribute('data-theme', darkMode ? 'dark' : 'light');
    }
  }, [darkMode]);

  useEffect(() => {
    try {
      localStorage.setItem('activeTab', activeTab);
    } catch {
      /* ignore */
    }
  }, [activeTab]);

  useEffect(() => {
    return () => {
      if (settingsSaveTimer.current) {
        window.clearTimeout(settingsSaveTimer.current);
      }
      if (exportFeedbackTimer.current) {
        window.clearTimeout(exportFeedbackTimer.current);
      }
    };
  }, []);

  useEffect(() => {
    void (async () => {
      try {
        const response = await apiFetch(`${apiBaseUrl}/api/replay`);
        if (!response.ok) {
          throw new Error(`HTTP ${response.status}`);
        }

        const payload = await response.json() as ReplayApiResponse;
        if (payload.stream) {
          setReplayStatus(payload.stream);
        }
        setReplayDefaultFile(payload.default_file ?? null);
      } catch (error) {
        console.warn('Falha ao carregar estado do replay', error);
      }
    })();
  }, [apiBaseUrl]);

  // Carrega as configurações salvas no backend ao montar (cenário de topologia,
  // janelas, padding) — sem isso o dashboard reabre sempre nos defaults e a
  // seleção de cenário parecia "não salvar".
  useEffect(() => {
    void (async () => {
      try {
        const response = await apiFetch(`${getBackendHttpBaseUrl()}/api/settings`);
        if (response.ok) {
          const saved = (await response.json()) as Partial<CSISettings>;
          setSettings((prev) => ({ ...prev, ...saved }));
        }
      } catch (error) {
        console.warn('Falha ao carregar configurações salvas', error);
      }
    })();
  }, []);

  const showExportFeedback = useCallback((message: string) => {
    setExportFeedback(message);

    if (exportFeedbackTimer.current) {
      window.clearTimeout(exportFeedbackTimer.current);
    }

    exportFeedbackTimer.current = window.setTimeout(() => {
      setExportFeedback(null);
    }, 2600);
  }, []);

  const handleSettingsChange = useCallback((newSettings: CSISettings) => {
    setSettings(newSettings);
    setSettingsFeedback({ tone: 'neutral', message: t('app.toolbar.applyingSettings', 'Aplicando ajustes ao backend...') });

    if (settingsSaveTimer.current) {
      window.clearTimeout(settingsSaveTimer.current);
    }

    settingsSaveTimer.current = window.setTimeout(() => {
      void (async () => {
        try {
          const response = await apiFetch(`${getBackendHttpBaseUrl()}/api/settings`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(newSettings),
          });

          if (!response.ok) {
            throw new Error(`HTTP ${response.status}`);
          }

          setSettingsFeedback({ tone: 'success', message: t('app.toolbar.settingsSynced', 'Configurações sincronizadas') });
        } catch (error) {
          console.error('Falha ao salvar configurações', error);
          setSettingsFeedback({ tone: 'error', message: t('app.toolbar.settingsFailed', 'Falha ao salvar ajustes no backend') });
        }
      })();
    }, SETTINGS_SAVE_DELAY_MS);
  }, [t]);

  const handleExportAllCharts = useCallback(() => {
    showExportFeedback(t('app.toolbar.exporting', 'Exportando gráficos...'));

    void (async () => {
      try {
        const exported = await exportPlots(EXPORTABLE_PLOT_IDS, 'aethersense-dashboard');
        showExportFeedback(exported > 0 ? t('app.toolbar.exportedSuccess', '{{count}} gráficos exportados', { count: exported }) : t('app.toolbar.exportedNone', 'Nenhum gráfico pronto para exportação'));
      } catch (error) {
        console.error('Falha ao exportar gráficos', error);
        showExportFeedback(t('app.toolbar.exportFailed', 'Falha ao exportar gráficos'));
      }
    })();
  }, [showExportFeedback, t]);

  const handleToggleReplay = useCallback(() => {
    setIsReplayBusy(true);
    setReplayFeedback(isReplayActive ? t('app.toolbar.stopReplayBtn', 'Parando replay CSV...') : t('app.toolbar.startReplayBtn', 'Iniciando replay CSV M6...'));

    void (async () => {
      try {
        const action = isReplayActive ? 'stop' : 'start';
        const response = await apiFetch(`${apiBaseUrl}/api/replay/${action}`, { method: 'POST' });
        const payload = await response.json() as ReplayApiResponse;

        if (!response.ok || !payload.ok) {
          throw new Error(payload.message || `HTTP ${response.status}`);
        }

        if (payload.stream) {
          setReplayStatus(payload.stream);
        }
        if (payload.default_file !== undefined) {
          setReplayDefaultFile(payload.default_file);
        }

        const file = replayFileLabel(payload.stream?.replay_file ?? replayDefaultFile);
        setReplayFeedback(
          isReplayActive
            ? t('app.toolbar.replayStopped', 'Replay CSV parado')
            : t('app.toolbar.replayActive', 'Replay ativo: {{file}}', { file })
        );
      } catch (error) {
        console.error('Falha ao alternar replay CSV', error);
        setReplayFeedback(error instanceof Error ? error.message : t('app.toolbar.replayFailed', 'Falha ao alternar replay CSV'));
      } finally {
        setIsReplayBusy(false);
      }
    })();
  }, [apiBaseUrl, isReplayActive, replayDefaultFile, t]);

  return (
    <>
      <Header
        data={displayPacket}
        darkMode={darkMode}
        onToggleDarkMode={() => setDarkMode((prev) => !prev)}
        activeTab={activeTab}
        onTabChange={setActiveTab}
      />

      <label className="api-session-auth">
        <KeyRound size={15} aria-hidden="true" />
        <span>{t('accessories.sessionToken', 'Token administrativo desta sessão')}</span>
        <input
          type="password"
          value={apiToken}
          onChange={(event) => {
            setApiToken(event.target.value);
            setApiBearerToken(event.target.value);
          }}
          autoComplete="off"
          placeholder={t('accessories.tokenPlaceholder', 'Bearer configurado pela TUI')}
          aria-label={t('accessories.sessionToken', 'Token administrativo desta sessão')}
        />
      </label>

      <main className="main" style={{ display: 'flex', flexDirection: 'column' }}>
        {showStreamAlert && (
          <div
            className={`stream-alert ${isStreamStale ? 'stream-alert--stale' : 'stream-alert--waiting'}`}
            role="alert"
            aria-live="assertive"
          >
            <div className="stream-alert__icon">!</div>
            <div className="stream-alert__content">
              <strong>{alertTitle}</strong>
              <span>{alertMessage}</span>
            </div>
          </div>
        )}

        <div className="bento-grid">
          {activeTab === 'debug' ? (
            <>
              <div className="bento-area-map">
                <AetherMap
                  subscribe={subscribe}
                  isClientView={false}
                  isLive={isStreamLive}
                  isReplay={isReplayActive}
                  hasSnapshot={hasDisplaySnapshot}
                  apiBaseUrl={apiBaseUrl}
                  editable={false}
                />
              </div>

              <div className="bento-area-pipeline">
                <SystemPipeline
                  data={displayPacket}
                  isLive={isStreamLive}
                  isReplay={isReplayActive}
                  packetRate={hasDataFlow ? status.packetsPerSecond : 0}
                />
              </div>

              <div className="bento-area-hardware">
                <HardwareTopology
                  isLive={isStreamLive}
                  isReplay={isReplayActive}
                  hasSnapshot={hasDisplaySnapshot}
                  devices={displayPacket?.devices ?? []}
                  location={displayPacket?.detection?.location}
                  apiBaseUrl={apiBaseUrl}
                  settings={settings}
                  onSettingsChange={handleSettingsChange}
                  rssi={displayPacket?.rssi}
                  packetRate={hasDataFlow ? status.packetsPerSecond : 0}
                  subcarriers={displayPacket?.subcarriers}
                  count={displayPacket?.count}
                  showScenario={false}
                  showCalibrate={false}
                />
              </div>

              <div className="bento-area-charts">
                <div className="chart-toolbar">
                  <div>
                    <h2>{t('app.toolbar.title', 'Gráficos do dashboard')}</h2>
                    <p>{t('app.toolbar.subtitle', 'Prioridade visual: inferência, sinal base, validação e observabilidade.')}</p>
                  </div>
                  <div className="chart-toolbar__actions">
                    {replayFeedback && <span className="chart-toolbar__feedback">{replayFeedback}</span>}
                    {exportFeedback && <span className="chart-toolbar__feedback">{exportFeedback}</span>}
                    <button
                      className={`btn ${isReplayActive ? 'btn--primary' : 'btn--secondary'} chart-toolbar__export`}
                      type="button"
                      onClick={handleToggleReplay}
                      disabled={isReplayBusy}
                      title={isReplayActive ? t('app.toolbar.stopReplay', 'Parar replay CSV') : t('app.toolbar.startReplay', 'Iniciar replay CSV M6')}
                    >
                      {isReplayActive ? <Square size={16} /> : <Play size={16} />}
                      {isReplayBusy ? t('common.pleaseWait', 'Aguarde...') : isReplayActive ? t('app.toolbar.stopReplayBtn', 'Parar replay') : t('app.toolbar.startReplayBtn', 'Replay M6')}
                    </button>
                    <button
                      className="btn btn--secondary chart-toolbar__export"
                      type="button"
                      onClick={handleExportAllCharts}
                    >
                      <Download size={16} />
                      {t('app.toolbar.exportAll', 'Exportar todos')}
                    </button>
                  </div>
                </div>
                <FftDiagnosticsPanel data={displayPacket} settings={settings} />
                {chartGroups.map((group) => {
                  const charts = engineeringCharts.filter((chart) => chart.group === group.id);

                  return (
                    <section className="chart-group" key={group.id} aria-label={group.title}>
                      <div className="chart-group__header">
                        <h3>{group.title}</h3>
                        <span>{group.description}</span>
                      </div>
                      <div className="chart-group__grid">
                        {charts.map((chart) => (
                          <div className={chart.wrapperClassName} key={chart.id}>
                            <ChartCard
                              id={chart.id}
                              title={chart.title}
                              subtitle={chart.subtitle}
                              validation={chart.validation}
                              tag={chart.tag}
                              isLive={hasDataFlow}
                              isReplay={isReplayActive}
                              data={displayPacket}
                              hasData={hasChartData}
                              subscribe={subscribe}
                              onData={chart.onData}
                              initPlot={chart.initPlot}
                              exportName={chart.title}
                            />
                          </div>
                        ))}
                      </div>
                    </section>
                  );
                })}
              </div>

              <div className="bento-area-gauge">
                <GaugeChart packetRate={hasDataFlow ? status.packetsPerSecond : 0} isLive={isStreamLive} isReplay={isReplayActive} />
              </div>
            </>
          ) : activeTab === 'calibration' ? (
            <div className="calib-grid">
              <div className="calib-row">
                <div className="calib-map">
                  <AetherMap
                    subscribe={subscribe}
                    isClientView={false}
                    isLive={isStreamLive}
                    isReplay={isReplayActive}
                    hasSnapshot={hasDisplaySnapshot}
                    apiBaseUrl={apiBaseUrl}
                    editable={true}
                    pickMode={locPickActive}
                    onMapPick={handleLocMapPick}
                    groundTruthMarkers={locGtMarkers}
                  />
                </div>
                <div className="calib-side">
                  <HardwareTopology
                    isLive={isStreamLive}
                    isReplay={isReplayActive}
                    hasSnapshot={hasDisplaySnapshot}
                    devices={displayPacket?.devices ?? []}
                    location={displayPacket?.detection?.location}
                    apiBaseUrl={apiBaseUrl}
                    settings={settings}
                    onSettingsChange={handleSettingsChange}
                    rssi={displayPacket?.rssi}
                    packetRate={hasDataFlow ? status.packetsPerSecond : 0}
                    subcarriers={displayPacket?.subcarriers}
                    count={displayPacket?.count}
                    showScenario={true}
                    showCalibrate={false}
                  />
                  <ControlsDrawer
                    inline
                    settings={settings}
                    onSettingsChange={handleSettingsChange}
                    feedbackMessage={settingsFeedback?.message}
                    feedbackTone={settingsFeedback?.tone}
                  />
                </div>
              </div>
              <LocationValidationPanel
                apiBaseUrl={apiBaseUrl}
                pickActive={locPickActive}
                onRequestPick={() => setLocPickActive(true)}
                onCancelPick={() => setLocPickActive(false)}
                lastPick={locLastPick}
                onGtMarkersChange={setLocGtMarkers}
              />
              <ValidationPanel apiBaseUrl={apiBaseUrl} />
            </div>
          ) : (
            <div className="client-dashboard-grid">
              <div className="client-control-stack">
                <ShowerBoxControlPanel
                  apiBaseUrl={apiBaseUrl}
                  showerBox={displayPacket?.detection?.shower_box}
                  isStreamLive={isStreamLive}
                />
                <AccessoriesPanel
                  apiBaseUrl={apiBaseUrl}
                  apiToken={apiToken}
                />
              </div>
              <div className="client-map-shell">
                <AetherMap
                  subscribe={subscribe}
                  isClientView={true}
                  isLive={isStreamLive}
                  isReplay={isReplayActive}
                  hasSnapshot={hasDisplaySnapshot}
                  apiBaseUrl={apiBaseUrl}
                />
                <HomeAssistantPanel apiBaseUrl={apiBaseUrl} devices={displayPacket?.devices ?? []} />
              </div>
            </div>
          )}
        </div>
      </main>

      <Footer
        status={status}
        hasPacket={status.hasPacket}
        serverMessage={displayPacket?.footer_text ?? streamStatus?.reason ?? status.message}
      />
    </>
  );
}
