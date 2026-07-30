import { useCallback, useEffect, useMemo, useState } from 'react';
import {
  CircleStop,
  Database,
  Lightbulb,
  Play,
  Power,
  Radio,
  RefreshCw,
  Send,
  TimerReset,
} from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { apiFetch } from '../utils/apiClient';

type AccessoryKind = 'relay' | 'ir_transmitter' | 'ir_receiver' | 'storage' | string;

interface Accessory {
  id: string;
  device_id: string;
  kind: AccessoryKind;
  name: string;
  available: boolean;
  state: Record<string, unknown>;
  capabilities: Record<string, unknown>;
  updated_at: string;
}

interface DeviceRuntime {
  device_id: string;
  name?: string;
  available?: boolean;
}

interface AccessoriesSnapshot {
  accessory_version: number;
  accessories: Accessory[];
  devices: DeviceRuntime[];
}

interface IrCode {
  id: string;
  device_id: string;
  appliance_id?: string;
  appliance_name?: string;
  name: string;
  carrier_hz: number;
  duty_cycle: number;
  sequence_us: number[];
}

interface IrCodesResponse {
  codes: IrCode[];
}

interface CommandResponse {
  ok?: boolean;
  message?: string;
  error?: string;
  command?: {
    request_id: string;
    status: string;
    error?: string | null;
  };
}

const COMMAND_TTL_MS = 15_000;
const COMMAND_POLL_MS = 300;
const COMMAND_ACK_GRACE_MS = 2_000;

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, ms));
}

interface Props {
  apiBaseUrl: string;
  apiToken: string;
}

const emptySnapshot: AccessoriesSnapshot = {
  accessory_version: 0,
  accessories: [],
  devices: [],
};

function boolState(accessory: Accessory, ...keys: string[]): boolean {
  for (const key of keys) {
    const value = accessory.state[key];
    if (typeof value === 'boolean') return value;
  }
  return false;
}

function numberState(accessory: Accessory, ...keys: string[]): number | null {
  for (const key of keys) {
    const value = accessory.state[key];
    if (typeof value === 'number' && Number.isFinite(value)) return value;
  }
  return null;
}

function humanBytes(value: number | null): string {
  if (value === null || value < 0) return '—';
  const units = ['B', 'KiB', 'MiB', 'GiB'];
  let current = value;
  let unit = 0;
  while (current >= 1024 && unit < units.length - 1) {
    current /= 1024;
    unit += 1;
  }
  return `${current.toFixed(unit === 0 ? 0 : 1)} ${units[unit]}`;
}

export function AccessoriesPanel({ apiBaseUrl, apiToken }: Props) {
  const { t } = useTranslation();
  const [snapshot, setSnapshot] = useState<AccessoriesSnapshot>(emptySnapshot);
  const [codes, setCodes] = useState<IrCode[]>([]);
  const [loading, setLoading] = useState(true);
  const [busyKey, setBusyKey] = useState<string | null>(null);
  const [feedback, setFeedback] = useState<string | null>(null);

  const load = useCallback(async (quiet = false) => {
    if (!quiet) setLoading(true);
    try {
      const [accessoriesResponse, codesResponse] = await Promise.all([
        apiFetch(`${apiBaseUrl}/api/accessories`),
        apiFetch(`${apiBaseUrl}/api/ir/codes`),
      ]);
      if (!accessoriesResponse.ok) throw new Error(`HTTP ${accessoriesResponse.status}`);
      const nextSnapshot = (await accessoriesResponse.json()) as AccessoriesSnapshot;
      setSnapshot({
        accessory_version: nextSnapshot.accessory_version ?? 0,
        accessories: Array.isArray(nextSnapshot.accessories) ? nextSnapshot.accessories : [],
        devices: Array.isArray(nextSnapshot.devices) ? nextSnapshot.devices : [],
      });
      if (codesResponse.ok) {
        const payload = (await codesResponse.json()) as IrCodesResponse;
        setCodes(Array.isArray(payload.codes) ? payload.codes : []);
      }
    } catch (error) {
      if (!quiet) {
        setFeedback(
          error instanceof Error
            ? t('accessories.loadFailedWithReason', 'Falha ao carregar acessórios: {{reason}}', { reason: error.message })
            : t('accessories.loadFailed', 'Falha ao carregar acessórios'),
        );
      }
    } finally {
      if (!quiet) setLoading(false);
    }
  }, [apiBaseUrl, t]);

  useEffect(() => {
    void load();
    const timer = window.setInterval(() => void load(true), 5000);
    return () => window.clearInterval(timer);
  }, [load]);

  const command = useCallback(async (
    deviceId: string,
    action: string,
    params: Record<string, unknown>,
    key: string,
  ) => {
    if (!apiToken.trim()) {
      setFeedback(t('accessories.tokenRequired', 'Informe o token Bearer desta sessão para enviar comandos.'));
      return;
    }
    setBusyKey(key);
    setFeedback(null);
    try {
      const requestId = crypto.randomUUID();
      const response = await apiFetch(`${apiBaseUrl}/api/accessories/commands`, {
        method: 'POST',
        headers: {
          Authorization: `Bearer ${apiToken.trim()}`,
          'Content-Type': 'application/json',
        },
        body: JSON.stringify({
          request_id: requestId,
          device_id: deviceId,
          action,
          params,
          ttl_ms: COMMAND_TTL_MS,
        }),
      });
      const payload = (await response.json()) as CommandResponse;
      if (!response.ok) {
        throw new Error(payload.message || payload.error || `HTTP ${response.status}`);
      }
      const acceptedId = payload.command?.request_id ?? requestId;
      setFeedback(
        t('accessories.commandQueued', 'Comando {{id}} enfileirado ({{status}}).', {
          id: acceptedId,
          status: payload.command?.status ?? 'pending',
        }),
      );

      const deadline = Date.now() + COMMAND_TTL_MS + COMMAND_ACK_GRACE_MS;
      let terminal: CommandResponse['command'] | undefined;
      while (Date.now() < deadline) {
        await delay(COMMAND_POLL_MS);
        const statusResponse = await apiFetch(
          `${apiBaseUrl}/api/accessories/commands/${encodeURIComponent(acceptedId)}`,
          { headers: { Authorization: `Bearer ${apiToken.trim()}` } },
        );
        const statusPayload = (await statusResponse.json()) as CommandResponse;
        if (!statusResponse.ok) {
          throw new Error(statusPayload.message || statusPayload.error || `HTTP ${statusResponse.status}`);
        }
        const status = statusPayload.command?.status?.toLowerCase();
        if (status === 'completed' || status === 'failed' || status === 'expired') {
          terminal = statusPayload.command;
          break;
        }
      }

      if (!terminal) {
        throw new Error(t('accessories.commandAckTimeout', 'Tempo esgotado aguardando confirmação do ESP.'));
      }
      if (terminal.status.toLowerCase() !== 'completed') {
        throw new Error(
          terminal.error
          || t('accessories.commandRejected', 'O ESP rejeitou o comando ({{status}}).', { status: terminal.status }),
        );
      }
      setFeedback(
        t('accessories.commandAcknowledged', 'Comando {{id}} executado e confirmado pelo ESP.', { id: acceptedId }),
      );
      await load(true);
    } catch (error) {
      setFeedback(
        error instanceof Error
          ? t('accessories.commandFailedWithReason', 'Falha no comando: {{reason}}', { reason: error.message })
          : t('accessories.commandFailed', 'Falha ao enviar comando'),
      );
    } finally {
      setBusyKey(null);
    }
  }, [apiBaseUrl, apiToken, load, t]);

  const relays = useMemo(
    () => snapshot.accessories.filter((accessory) => accessory.kind === 'relay'),
    [snapshot.accessories],
  );
  const irAccessories = useMemo(
    () => snapshot.accessories.filter((accessory) => accessory.kind === 'ir_transmitter' || accessory.kind === 'ir_receiver'),
    [snapshot.accessories],
  );
  const storage = useMemo(
    () => snapshot.accessories.filter((accessory) => accessory.kind === 'storage'),
    [snapshot.accessories],
  );
  const irAppliances = useMemo(() => {
    const grouped = new Map<string, { id: string; name: string; codes: IrCode[] }>();
    for (const code of codes) {
      const id = code.appliance_id?.trim() || `device:${code.device_id}`;
      const current = grouped.get(id) ?? {
        id,
        name: code.appliance_name?.trim() || t('accessories.defaultAppliance', 'Controle IR'),
        codes: [],
      };
      current.codes.push(code);
      grouped.set(id, current);
    }
    return [...grouped.values()]
      .map((appliance) => ({
        ...appliance,
        codes: appliance.codes.sort((left, right) => left.name.localeCompare(right.name)),
      }))
      .sort((left, right) => left.name.localeCompare(right.name));
  }, [codes, t]);

  return (
    <section className="bento-card accessories-panel" aria-labelledby="accessories-title">
      <header className="accessories-panel__header">
        <div>
          <span className="accessories-panel__eyebrow">
            {t('accessories.eyebrow', 'ESP32 • MQTT direto')}
          </span>
          <h2 id="accessories-title">
            <Power size={19} /> {t('accessories.title', 'Acessórios')}
          </h2>
          <p>
            {t('accessories.subtitle', 'Relés, infravermelho e armazenamento provisionados pela TUI.')}
          </p>
        </div>
        <button
          type="button"
          className="btn btn--secondary"
          onClick={() => void load()}
          disabled={loading}
          title={t('accessories.refresh', 'Atualizar acessórios')}
        >
          <RefreshCw size={15} className={loading ? 'accessories-panel__spin' : undefined} />
        </button>
      </header>

      {snapshot.accessories.length === 0 && !loading ? (
        <div className="accessories-panel__empty">
          <Radio size={22} />
          <strong>{t('accessories.none', 'Nenhum acessório anunciado')}</strong>
          <span>{t('accessories.onboardHint', 'Conecte o ESP32 e conclua o onboarding na TUI do build.py.')}</span>
        </div>
      ) : (
        <div className="accessories-panel__sections">
          {relays.length > 0 && (
            <div className="accessories-panel__section">
              <h3><Lightbulb size={16} /> {t('accessories.relays', 'Relés')}</h3>
              {relays.map((relay) => {
                const isOn = boolState(relay, 'on', 'enabled', 'value');
                const busy = busyKey === `relay:${relay.id}`;
                return (
                  <div className="accessories-panel__row" key={relay.id}>
                    <div>
                      <strong>{relay.name}</strong>
                      <small>{relay.device_id} · {relay.available ? t('common.online', 'online') : t('common.offline', 'offline')}</small>
                    </div>
                    <div className="accessories-panel__actions">
                      <button
                        type="button"
                        className={`btn ${isOn ? 'btn--primary' : 'btn--secondary'}`}
                        disabled={!relay.available || busy}
                        onClick={() => void command(
                          relay.device_id,
                          'relay.set',
                          { relay_id: relay.id, state: !isOn },
                          `relay:${relay.id}`,
                        )}
                      >
                        <Power size={14} /> {isOn ? t('common.turnOff', 'Desligar') : t('common.turnOn', 'Ligar')}
                      </button>
                      <button
                        type="button"
                        className="btn btn--secondary"
                        disabled={!relay.available || busy}
                        onClick={() => void command(
                          relay.device_id,
                          'relay.pulse',
                          { relay_id: relay.id, duration_ms: 1000 },
                          `relay:${relay.id}`,
                        )}
                        title={t('accessories.pulseTitle', 'Pulso de um segundo')}
                      >
                        <TimerReset size={14} /> 1s
                      </button>
                    </div>
                  </div>
                );
              })}
            </div>
          )}

          {(irAccessories.length > 0 || codes.length > 0) && (
            <div className="accessories-panel__section">
              <h3><Send size={16} /> {t('accessories.infrared', 'Infravermelho')}</h3>
              <div className="accessories-panel__appliances">
                {irAppliances.map((appliance) => (
                  <article className="accessories-panel__appliance" key={appliance.id}>
                    <header>
                      <strong>{appliance.name}</strong>
                      <small>{t('accessories.savedButtons', '{{count}} botão(ões)', { count: appliance.codes.length })}</small>
                    </header>
                    <div className="accessories-panel__chips">
                      {appliance.codes.map((code) => (
                        <button
                          type="button"
                          className="btn btn--secondary"
                          key={code.id}
                          disabled={busyKey === `ir:${code.id}`}
                          onClick={() => void command(
                            code.device_id,
                            'ir.send',
                            { code_id: code.id, repeats: 1 },
                            `ir:${code.id}`,
                          )}
                          title={`${code.carrier_hz} Hz · ${code.sequence_us.length} pulsos`}
                        >
                          <Send size={13} /> {code.name}
                        </button>
                      ))}
                    </div>
                  </article>
                ))}
              </div>
              <div className="accessories-panel__chips">
                {irAccessories.find((accessory) => accessory.kind === 'ir_receiver') && (
                  <button
                    type="button"
                    className="btn btn--secondary"
                    disabled={busyKey === 'ir:learn'}
                    onClick={() => {
                      const receiver = irAccessories.find((accessory) => accessory.kind === 'ir_receiver');
                      if (receiver) {
                        void command(receiver.device_id, 'ir.learn.start', { timeout_ms: 10_000 }, 'ir:learn');
                      }
                    }}
                  >
                    <Radio size={13} /> {t('accessories.learn10s', 'Aprender 10s')}
                  </button>
                )}
              </div>
            </div>
          )}

          {storage.length > 0 && (
            <div className="accessories-panel__section">
              <h3><Database size={16} /> microSD</h3>
              {storage.map((card) => {
                const mounted = boolState(card, 'mounted', 'available');
                const recording = boolState(card, 'recording');
                const freeBytes = numberState(card, 'free_bytes', 'available_bytes');
                return (
                  <div className="accessories-panel__row accessories-panel__row--storage" key={card.id}>
                    <div>
                      <strong>{card.name}</strong>
                      <small>{mounted ? t('accessories.freeSpace', '{{space}} livres', { space: humanBytes(freeBytes) }) : t('accessories.notMounted', 'não montado')}</small>
                    </div>
                    <div className="accessories-panel__actions">
                      <button
                        type="button"
                        className={`btn ${recording ? 'btn--primary' : 'btn--secondary'}`}
                        disabled={!card.available || busyKey === `storage:${card.id}`}
                        onClick={() => void command(
                          card.device_id,
                          recording ? 'storage.record.stop' : 'storage.record.start',
                          // A gravação cotidiana nunca inicia CSI bruto. Sessões de
                          // bancada/sala vazia exigem capture_context explícito e são
                          // comissionadas pela TUI para preservar a restrição ética.
                          {
                            storage_id: card.id,
                            session_id: `dashboard-${Date.now().toString(36)}`,
                            raw_csi: false,
                          },
                          `storage:${card.id}`,
                        )}
                      >
                        {recording ? <CircleStop size={14} /> : <Play size={14} />}
                        {recording ? t('common.stop', 'Parar') : t('common.record', 'Gravar')}
                      </button>
                      <button
                        type="button"
                        className="btn btn--secondary"
                        disabled={!card.available || busyKey === `sync:${card.id}`}
                        onClick={() => void command(card.device_id, 'storage.sync', { storage_id: card.id }, `sync:${card.id}`)}
                      >
                        <RefreshCw size={14} /> Sync
                      </button>
                    </div>
                  </div>
                );
              })}
            </div>
          )}
        </div>
      )}

      {feedback && <p className="accessories-panel__feedback" role="status">{feedback}</p>}
      <footer className="accessories-panel__footer">
        v{snapshot.accessory_version} · {snapshot.devices.length} {t('accessories.devices', 'dispositivo(s)')}
      </footer>
    </section>
  );
}
