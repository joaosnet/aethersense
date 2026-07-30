import { useMemo, useState, type CSSProperties, type ReactNode } from 'react';
import { Cpu, Wifi, Activity, Crosshair, Database, RadioTower, Waves, ShieldCheck, ShieldAlert, WifiOff } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { prepareEmptyRoomCalibration } from '../utils/emptyRoomCalibration';
import type { CSISettings, ESPDevice, LocationEstimate } from '../types/csi';
import { formatLocationAccuracy, formatLocationMethod, formatPercent, isFiniteNumber } from '../utils/dashboardMetrics';
import { apiFetch } from '../utils/apiClient';

type Scenario = 1 | 2 | 3 | 4 | 5 | 6;

interface HardwareTopologyProps {
  isLive: boolean;
  isReplay?: boolean;
  hasSnapshot?: boolean;
  devices: ESPDevice[];
  location?: LocationEstimate;
  apiBaseUrl: string;
  settings: CSISettings;
  onSettingsChange: (settings: CSISettings) => void;
  rssi?: number;
  packetRate?: number;
  subcarriers?: number;
  count?: number;
  showScenario?: boolean;
  showCalibrate?: boolean;
}

type Band = '5 GHz' | '2.4 GHz' | '2,4/5 GHz' | null;
type TopoNode = { name: string; band: Band; receiver: boolean };
const SCENARIO_TOPOLOGY: Record<Scenario, TopoNode[]> = {
  1: [
    { name: 'ESP32-WROOM', band: '2.4 GHz', receiver: true },
    { name: 'Router Wi-Fi 2.4GHz', band: null, receiver: false },
  ],
  2: [
    { name: 'ESP32-C5', band: '2.4 GHz', receiver: true },
    { name: 'Router Wi-Fi 6', band: null, receiver: false },
  ],
  3: [
    { name: 'ESP32-C5', band: '2.4 GHz', receiver: true },
    { name: 'ESP32-WROOM', band: '2.4 GHz', receiver: true },
  ],
  4: [
    { name: 'ESP32-C5', band: '5 GHz', receiver: true },
    { name: 'Router Wi-Fi 6', band: null, receiver: false },
  ],
  5: [
    { name: 'ESP32-C5', band: '5 GHz', receiver: true },
    { name: 'ESP32-WROOM', band: '2.4 GHz', receiver: true },
  ],
  6: [
    { name: 'ESP32-WROOM', band: '2.4 GHz', receiver: true },
    { name: 'ESP32-C5', band: '2,4/5 GHz', receiver: true },
    { name: 'ESP32', band: '2.4 GHz', receiver: true },
    { name: 'Router Wi-Fi', band: null, receiver: false },
  ],
};

function isReceiver(device: ESPDevice) {
  const role = (device.role || 'receiver').toLowerCase();
  return role === 'receiver' || role === '';
}

function isC5(device: ESPDevice) {
  return (device.hardware_profile || '').toLowerCase().includes('c5');
}

function isWroom(device: ESPDevice) {
  const profile = (device.hardware_profile || '').toLowerCase();
  return profile.includes('wroom') || profile.includes('classic');
}

function nodeKind(name: string): { hue: number; Icon: typeof Cpu } {
  const n = name.toUpperCase();
  if (n.includes('C5')) return { hue: 170, Icon: Cpu };
  if (n.includes('WROOM')) return { hue: 265, Icon: Cpu };
  if (n.includes('C6')) return { hue: 200, Icon: Cpu };
  if (n.includes('ROUTER') || n.includes('WI-FI') || n.includes('WIFI')) return { hue: 210, Icon: Wifi };
  if (n.includes('ESP32')) return { hue: 32, Icon: Cpu };
  return { hue: 210, Icon: Wifi };
}

const bandClassOf = (band: string | null) => (band && band.includes('5') ? 'hw-band--5g' : band === '2.4 GHz' ? 'hw-band--24g' : '');

export function HardwareTopology({
  isLive,
  isReplay = false,
  hasSnapshot = false,
  devices,
  location,
  apiBaseUrl,
  settings,
  onSettingsChange,
  rssi,
  packetRate,
  subcarriers,
  count,
  showScenario = true,
  showCalibrate = true,
}: HardwareTopologyProps) {
  const { t } = useTranslation();
  const scenario = (settings.topology_scenario as Scenario) || 3;
  const [baselineFeedback, setBaselineFeedback] = useState<string | null>(null);
  const [isCalibrating, setIsCalibrating] = useState(false);

  const receiverDevices = useMemo(() => devices.filter(isReceiver), [devices]);
  const c5Count = receiverDevices.filter(isC5).length;
  const wroomCount = receiverDevices.filter(isWroom).length;
  const nodesUsed = location?.nodes_used ?? 0;
  const confidence = location?.confidence ?? 0;
  const method = location?.method ?? '';
  const methodLabel = formatLocationMethod(location, '');

  const topologyStatus = !isLive
    ? isReplay
      ? t('topology.replayCsv', 'replay CSV')
      : hasSnapshot
        ? t('topology.noLiveCsi', 'sem CSI ao vivo')
        : t('topology.waiting', 'aguardando')
    : nodesUsed >= 2
      ? t('topology.twoNodesActive', '2 nós ativos')
      : method.includes('esp32c5')
        ? t('topology.fallbackC5', 'fallback C5')
        : method.includes('wroom')
          ? t('topology.fallbackWroom', 'fallback WROOM')
          : method.includes('sem_baseline')
            ? t('topology.noBaseline', 'sem baseline')
            : t('topology.calibrating', 'calibrando');

  const verdict: { head: string; hue: number; sat: string; icon: ReactNode; live: boolean; beat: boolean } =
    isLive && nodesUsed >= 2
      ? { head: t('topology.systemOk', 'SISTEMA OK'), hue: 95, sat: '78%', icon: <ShieldCheck size={19} />, live: true, beat: true }
      : isLive && method.includes('sem_baseline')
        ? { head: t('topology.calibratingUpper', 'CALIBRANDO'), hue: 38, sat: '85%', icon: <ShieldAlert size={19} />, live: true, beat: false }
        : isLive
          ? { head: t('topology.degradedFusion', 'FUSÃO DEGRADADA'), hue: 38, sat: '85%', icon: <ShieldAlert size={19} />, live: true, beat: false }
          : isReplay
            ? { head: t('topology.replayCsvUpper', 'REPLAY CSV'), hue: 265, sat: '55%', icon: <Activity size={19} />, live: false, beat: false }
            : hasSnapshot
              ? { head: t('topology.noLiveCsiUpper', 'SEM CSI AO VIVO'), hue: 5, sat: '45%', icon: <WifiOff size={19} />, live: false, beat: false }
              : { head: t('topology.waitingUpper', 'AGUARDANDO'), hue: 210, sat: '12%', icon: <WifiOff size={19} />, live: false, beat: false };
  const verdictSub = `${topologyStatus}${methodLabel ? ` · ${methodLabel}` : ''}`;

  const profileForNode = (name: string): string | null => {
    const n = name.toUpperCase();
    if (n.includes('C5')) return 'c5';
    if (n.includes('WROOM')) return 'wroom';
    if (n.includes('C6')) return 'c6';
    return null;
  };
  const liveDeviceForNode = (name: string) => {
    const p = profileForNode(name);
    if (!p) return undefined;
    return devices.find((d) => (d.hardware_profile || '').toLowerCase().includes(p) && d.band);
  };

  const topoNodes = SCENARIO_TOPOLOGY[scenario];
  const receiverCount = topoNodes.filter((n) => n.receiver).length;
  let receiverSeen = 0;
  const nodeViews = topoNodes.map((node) => {
    const active = node.receiver && isLive && nodesUsed >= receiverSeen + 1;
    if (node.receiver) receiverSeen += 1;
    const live = liveDeviceForNode(node.name);
    return { node, active, band: live?.band ?? node.band, liveChannel: live?.channel };
  });
  const activeReceivers = isLive ? Math.min(receiverCount, nodesUsed) : 0;

  const linkHue = nodesUsed >= 2 ? 95 : method.includes('esp32c5') ? 170 : method.includes('wroom') ? 265 : 95;

  const rssiVal = isFiniteNumber(rssi) ? rssi : undefined;
  const rssiLevel = rssiVal !== undefined ? Math.max(0, Math.min(5, Math.round(((rssiVal + 90) / 50) * 5))) : 0;
  const rssiColor =
    rssiVal === undefined
      ? 'var(--border)'
      : rssiVal > -60
        ? 'var(--accent-emerald)'
        : rssiVal > -75
          ? 'var(--accent-amber)'
          : 'var(--accent-rose)';

  const confColor = confidence > 0.66 ? 'var(--accent-emerald)' : confidence > 0.33 ? 'var(--accent-amber)' : 'var(--accent-rose)';

  const modePill = isLive
    ? { cls: 'hw-mode-pill--live', label: t('topology.live', 'Ao vivo') }
    : isReplay
      ? { cls: 'hw-mode-pill--replay', label: t('topology.replay', 'Replay') }
      : hasSnapshot
        ? { cls: 'hw-mode-pill--snapshot', label: t('topology.snapshot', 'Snapshot') }
        : { cls: 'hw-mode-pill--offline', label: t('topology.offline', 'Offline') };

  const setScenario = (s: Scenario) => {
    onSettingsChange({ ...settings, topology_scenario: s });
  };

  const calibrateBaseline = async () => {
    setIsCalibrating(true);
    setBaselineFeedback(t('topology.calibratingEmptyRoom', 'calibrando sala vazia...'));

    try {
      await prepareEmptyRoomCalibration(apiBaseUrl, (seconds) => {
        setBaselineFeedback(t(
          'topology.collectingEmptyRoom',
          'mantenha quarto, banheiro e box vazios por {{seconds}} s...',
          { seconds },
        ));
      });
      const response = await apiFetch(`${apiBaseUrl}/api/location/baseline`, { method: 'POST' });
      const result = await response.json();
      const calibratedCount = Array.isArray(result.calibrated) ? result.calibrated.length : 0;

      setBaselineFeedback(
        result.ok
          ? t('topology.baselineSaved', 'baseline salvo em {{count}} receptor(es)', { count: calibratedCount })
          : (result.message || t('topology.notEnoughPackets', 'sem pacotes suficientes para baseline'))
      );
    } catch (error) {
      console.error('Falha ao calibrar baseline', error);
      setBaselineFeedback(t('topology.baselineFailed', 'falha ao calibrar baseline'));
    } finally {
      setIsCalibrating(false);
    }
  };

  const renderNode = (name: string, band: string | null, k: ReturnType<typeof nodeKind>, active: boolean, isReceiverNode: boolean, key: number, liveChannel?: number) => {
    const dot = active ? 'on' : isReceiverNode && isLive ? 'fallback' : 'off';
    const health = active ? t('topology.contributing', 'contribuindo') : !isReceiverNode ? t('topology.wifiRef', 'referência Wi-Fi') : isLive ? t('topology.fallbackWord', 'fallback') : t('topology.offlineWord', 'offline');
    const idle = !active && (!isReceiverNode || !isLive);
    const Icon = k.Icon;
    return (
      <div
        key={key}
        className={`hw-node${active ? ' hw-node--active' : ''}${idle ? ' hw-node--idle' : ''}`}
        title={`${name}${band ? ` · ${band}` : ''}${liveChannel != null ? ` (${t('topology.live', 'ao vivo')} · canal ${liveChannel})` : ''} · ${isReceiverNode ? t('topology.csiReceiver', 'receptor CSI') : t('topology.wifiRef', 'referência Wi-Fi')}`}
      >
        <div className="hw-node__core" style={{ '--hue': k.hue } as CSSProperties}>
          <Icon size={15} />
        </div>
        <div className="hw-node__body">
          <span className="hw-node__name">{name}</span>
          <span className="hw-node__meta">
            {band && <span className={`hw-band ${bandClassOf(band)}${liveChannel != null ? ' hw-band--live' : ''}`} title={liveChannel != null ? t('topology.liveBandChannel', 'banda ao vivo (canal {{ch}})', { ch: liveChannel }) : undefined}>{band}</span>}
            <span className="hw-node__health">{health}</span>
          </span>
        </div>
        <span className={`hw-node__dot hw-node__dot--${dot}`} />
      </div>
    );
  };

  return (
    <div className="bento-card hw-card" style={{ display: 'flex', flexDirection: 'column', height: '100%', gap: '10px' }}>
      <div className="hw-head-row">
        <h3 className="bento-card__title" style={{ border: 'none', margin: 0, padding: 0 }}>
          <RadioTower size={16} /> {t('topology.cardTitle', 'STATUS DE HARDWARE & TOPOLOGIA')}
        </h3>
        <span className={`hw-mode-pill ${modePill.cls}`}>
          <span className="hw-mode-pill__dot" /> {modePill.label}
        </span>
      </div>

      <div
        className={`hw-verdict${verdict.live ? '' : ' hw-verdict--steady'}${verdict.beat ? ' hw-verdict--beat' : ''}`}
        style={{ '--hue': verdict.hue, '--sat': verdict.sat } as CSSProperties}
      >
        <div className="hw-verdict__disc">{verdict.icon}</div>
        <div className="hw-verdict__body">
          <span className="hw-verdict__head">{verdict.head}</span>
          <span className="hw-verdict__sub">{verdictSub}</span>
        </div>
      </div>

      <div className="hw-topo">
        {nodeViews.map((nv, i) =>
          renderNode(nv.node.name, nv.band, nodeKind(nv.node.name), nv.active, nv.node.receiver, i, nv.liveChannel),
        )}
      </div>
      {receiverCount >= 1 && (
        <div
          className={`hw-fusion${isLive && activeReceivers >= 1 ? '' : ' hw-fusion--idle'}`}
          style={{ '--hue': linkHue } as CSSProperties}
        >
          <Waves size={12} className="hw-fusion__icon" />
          <div className="hw-fusion__track">
            <span className="hw-flow-dot" />
          </div>
          <span className="hw-fusion__label">
            {activeReceivers >= 2
              ? t('topology.fusionNodes', 'fusão de {{count}} nós', { count: activeReceivers })
              : activeReceivers === 1
                ? t('topology.oneNodeActive', '1 nó ativo')
                : t('topology.receiversCount', '{{count}} receptores', { count: receiverCount })}
          </span>
        </div>
      )}

      <div className="hw-signal__cap">
        <Activity size={11} /> {t('topology.liveSignal', 'Sinal ao vivo · stream compartilhado')}
      </div>
      <div className="hw-gauges">
        <div className={`hw-gauge${rssiVal === undefined ? ' hw-gauge--empty' : ''}`}>
          <span className="hw-gauge__top"><Wifi size={12} /> RSSI</span>
          <span className="hw-gauge__val">{rssiVal !== undefined ? rssiVal.toFixed(0) : '--'}<small>dBm</small></span>
          <div className="hw-bars">
            {[0, 1, 2, 3, 4].map((i) => (
              <span
                key={i}
                className={`hw-bars__seg${i < rssiLevel ? ' hw-bars__seg--on' : ''}`}
                style={i < rssiLevel ? { background: rssiColor } : undefined}
              />
            ))}
          </div>
        </div>
        <div className={`hw-gauge${!isFiniteNumber(packetRate) || packetRate === 0 ? ' hw-gauge--empty' : ''}`}>
          <span className="hw-gauge__top"><Activity size={12} style={{ animation: 'presence-pulse 1.5s infinite' }} /> {t('topology.rate', 'Taxa')}</span>
          <span className="hw-gauge__val">{isFiniteNumber(packetRate) ? packetRate.toFixed(0) : '--'}<small>pps</small></span>
        </div>
        <div className={`hw-gauge${!isFiniteNumber(subcarriers) ? ' hw-gauge--empty' : ''}`}>
          <span className="hw-gauge__top"><Waves size={12} /> {t('topology.subcarriers', 'Subport.')}</span>
          <span className="hw-gauge__val">{isFiniteNumber(subcarriers) ? subcarriers : '--'}</span>
        </div>
        <div className={`hw-gauge${!isFiniteNumber(count) ? ' hw-gauge--empty' : ''}`}>
          <span className="hw-gauge__top"><Database size={12} /> {t('topology.window', 'Janela')}</span>
          <span className="hw-gauge__val">{isFiniteNumber(count) ? count : '--'}<small>frames</small></span>
        </div>
      </div>

      <div className="hw-loc">
        <div className="hw-loc__pill">
          <span className="hw-loc__lbl"><Crosshair size={11} /> {t('topology.accuracy', 'Precisão')}</span>
          <span className="hw-loc__val">{formatLocationAccuracy(location)}</span>
        </div>
        <div className="hw-loc__pill">
          <span className="hw-loc__lbl"><Waves size={11} /> {t('topology.confidence', 'Confiança')}</span>
          <span className="hw-loc__val">{location ? formatPercent(confidence) : '--'}</span>
          <div className="hw-loc__bar">
            <span style={{ width: `${Math.round((location ? confidence : 0) * 100)}%`, background: confColor }} />
          </div>
        </div>
        <div className="hw-loc__pill">
          <span className="hw-loc__lbl"><Database size={11} /> {t('topology.resolution', 'Resolução')}</span>
          <span className="hw-loc__val">{location?.resolution_m ? `${(location.resolution_m * 100).toFixed(0)} cm` : '--'}</span>
        </div>
      </div>

      <div style={{ flex: 1 }} />

      {showScenario && (
        <div className="hw-segmented">
          {([1, 2, 3, 4, 5, 6] as Scenario[]).map((n) => {
            const desc = SCENARIO_TOPOLOGY[n].map((nd) => `${nd.name}${nd.band ? ` ${nd.band}` : ''}`).join(' + ');
            return (
              <button
                key={n}
                className={`btn ${scenario === n ? 'btn--primary' : 'btn--secondary'}`}
                onClick={() => setScenario(n)}
                style={{ fontSize: '0.62rem' }}
                title={desc}
              >
                {t('topology.scenario', 'Cenário')} {n}
              </button>
            );
          })}
        </div>
      )}

      {showCalibrate && (
        <>
          <button
            className="btn btn--secondary"
            onClick={calibrateBaseline}
            disabled={!isLive || isCalibrating}
            style={{ fontSize: '0.68rem', opacity: !isLive ? 0.6 : 1, justifyContent: 'center' }}
          >
            <Database size={14} /> {isCalibrating ? t('topology.calibratingBtn', 'Calibrando...') : t('topology.saveBaseline', 'Salvar baseline sala vazia')}
          </button>

          {baselineFeedback && <p className="hw-baseline-toast">{baselineFeedback}</p>}
        </>
      )}

      <div className="hw-foot">
        {receiverDevices.length > 0
          ? `${receiverDevices.length} ${t('topology.receiverWord', 'receptor(es)')} · ${c5Count} C5 · ${wroomCount} WROOM`
          : isLive
            ? t('topology.liveUpdate', 'Atualização ao vivo')
            : isReplay
              ? t('topology.replayCapture', 'Replay de captura local')
              : t('topology.waitingCsi', 'Aguardando pacotes CSI')}
      </div>
    </div>
  );
}
