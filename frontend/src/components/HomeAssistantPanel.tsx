import { useCallback, useEffect, useState } from 'react';
import { HousePlug, RefreshCw, Save, Send, Wifi, WifiOff, X } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import type { ESPDevice, MqttSettingsView } from '../types/csi';
import { apiFetch } from '../utils/apiClient';

interface Props {
  apiBaseUrl: string;
  devices: ESPDevice[];
}

function zoneSlug(name: string): string {
  let slug = (name || '').toLowerCase().replace(/[^\p{L}\p{N}]/gu, '_');
  while (slug.includes('__')) slug = slug.replace(/__/g, '_');
  return slug.replace(/^_+|_+$/g, '');
}

const isReceiver = (d: ESPDevice) => {
  const role = (d.role || 'receiver').toLowerCase();
  return role === 'receiver' || role === '';
};

type Form = {
  enabled: boolean;
  host: string;
  port: string;
  username: string;
  base_topic: string;
  discovery_prefix: string;
};

const emptyForm: Form = {
  enabled: false,
  host: '',
  port: '1883',
  username: '',
  base_topic: 'aethersense',
  discovery_prefix: 'homeassistant',
};

export function HomeAssistantPanel({ apiBaseUrl, devices }: Props) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [view, setView] = useState<MqttSettingsView | null>(null);
  const [form, setForm] = useState<Form>(emptyForm);
  const [password, setPassword] = useState('');
  const [clearPassword, setClearPassword] = useState(false);
  const [busy, setBusy] = useState(false);
  const [feedback, setFeedback] = useState<string | null>(null);
  const [deviceNames, setDeviceNames] = useState<Record<string, string>>({});

  const load = useCallback(async () => {
    try {
      const r = await apiFetch(`${apiBaseUrl}/api/mqtt/settings`);
      if (!r.ok) return;
      const v = (await r.json()) as MqttSettingsView;
      setView(v);
      setForm({
        enabled: v.enabled,
        host: v.host,
        port: String(v.port),
        username: v.username ?? '',
        base_topic: v.base_topic,
        discovery_prefix: v.discovery_prefix,
      });
      setPassword('');
      setClearPassword(false);
    } catch {
      /* mantém o estado anterior */
    }
  }, [apiBaseUrl]);

  useEffect(() => {
    if (open) void load();
  }, [open, load]);

  const save = async () => {
    setBusy(true);
    setFeedback(null);
    const body: Record<string, unknown> = {
      enabled: form.enabled,
      host: form.host.trim(),
      port: parseInt(form.port, 10) || 1883,
      username: form.username.trim(),
      base_topic: form.base_topic.trim(),
      discovery_prefix: form.discovery_prefix.trim(),
    };
    if (password) body.password = password;
    else if (clearPassword) body.password = '';
    try {
      const r = await apiFetch(`${apiBaseUrl}/api/mqtt/settings`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      });
      const j = await r.json();
      setFeedback(j.message ?? (r.ok ? t('ha.saved', 'Salvo') : t('ha.saveFailed', 'Falha ao salvar')));
    } catch {
      setFeedback(t('ha.networkError', 'Falha de rede ao salvar'));
    } finally {
      setBusy(false);
      window.setTimeout(() => void load(), 1400);
    }
  };

  const republish = async () => {
    setBusy(true);
    setFeedback(null);
    try {
      const r = await apiFetch(`${apiBaseUrl}/api/mqtt/discovery`, { method: 'POST' });
      const j = await r.json();
      setFeedback(j.message ?? t('ha.discoveryRepublished', 'Discovery republicado'));
    } catch {
      setFeedback(t('ha.discoveryFailed', 'Falha ao republicar discovery'));
    } finally {
      setBusy(false);
    }
  };

  const saveDeviceName = async (device: ESPDevice) => {
    const name = (deviceNames[device.id] ?? device.name).trim();
    if (!name || name === device.name) return;
    setBusy(true);
    try {
      await apiFetch(`${apiBaseUrl}/api/devices`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ ...device, name }),
      });
      setFeedback(t('ha.receiverRenamed', 'Receptor renomeado para "{{name}}" — republique o discovery', { name }));
    } catch {
      setFeedback(t('ha.renameFailed', 'Falha ao renomear receptor'));
    } finally {
      setBusy(false);
    }
  };

  const receivers = devices.filter(isReceiver);
  const connected = view?.connected ?? false;
  const enabled = view?.enabled ?? false;

  return (
    <>
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        title={t('ha.configTitle', 'Configurar Home Assistant')}
        style={{
          position: 'absolute',
          top: 16,
          right: 16,
          zIndex: 1200,
          display: 'flex',
          alignItems: 'center',
          gap: 8,
          padding: '10px 14px',
          borderRadius: 999,
          border: '1px solid var(--border)',
          background: 'var(--bg-card)',
          color: 'var(--text-secondary)',
          boxShadow: 'var(--shadow-md)',
          cursor: 'pointer',
          fontSize: 13,
          fontWeight: 600,
        }}
      >
        <HousePlug size={18} />
        Home Assistant
        <span
          style={{
            width: 9,
            height: 9,
            borderRadius: '50%',
            background: connected ? 'var(--accent-emerald, #10b981)' : enabled ? '#f59e0b' : '#94a3b8',
            boxShadow: connected ? '0 0 8px rgba(16,185,129,0.8)' : 'none',
          }}
        />
      </button>

      {open && (
        <div
          style={{
            position: 'absolute',
            top: 64,
            right: 16,
            zIndex: 1200,
            width: 'min(400px, calc(100% - 32px))',
            maxHeight: 'calc(100% - 88px)',
            overflowY: 'auto',
            background: 'var(--bg-card)',
            border: '1px solid var(--border)',
            borderRadius: 'var(--radius-md)',
            boxShadow: 'var(--shadow-lg)',
            padding: 16,
          }}
        >
          <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 8 }}>
            <h3 style={{ margin: 0, display: 'flex', alignItems: 'center', gap: 8, fontSize: 15 }}>
              <HousePlug size={18} /> {t('ha.panelTitle', 'Home Assistant (MQTT)')}
            </h3>
            <button type="button" onClick={() => setOpen(false)} style={{ background: 'none', border: 'none', color: 'var(--text-muted)', cursor: 'pointer' }}>
              <X size={18} />
            </button>
          </div>

          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 8,
              padding: '8px 10px',
              borderRadius: 8,
              marginBottom: 12,
              fontSize: 12.5,
              background: connected ? 'rgba(16,185,129,0.12)' : 'rgba(148,163,184,0.12)',
              border: `1px solid ${connected ? 'rgba(16,185,129,0.5)' : 'rgba(148,163,184,0.4)'}`,
            }}
          >
            {connected ? <Wifi size={15} /> : <WifiOff size={15} />}
            <span>
              {connected
                ? t('ha.connectedToBroker', 'Conectado ao broker {{host}}:{{port}}', { host: view?.host, port: view?.port })
                : enabled
                  ? t('ha.tryingConnect', 'Habilitado, tentando conectar em {{host}}:{{port}}…', { host: view?.host, port: view?.port })
                  : t('ha.mqttDisabled', 'MQTT desabilitado')}
            </span>
          </div>

          <label style={{ display: 'flex', alignItems: 'center', gap: 8, fontSize: 13, marginBottom: 8 }}>
            <input type="checkbox" checked={form.enabled} onChange={(e) => setForm({ ...form, enabled: e.target.checked })} />
            {t('ha.enableMqtt', 'Habilitar integração MQTT')}
          </label>

          <div style={{ display: 'grid', gridTemplateColumns: '1fr 80px', gap: 8, marginBottom: 8 }}>
            <Field label="Broker (host)">
              <input value={form.host} onChange={(e) => setForm({ ...form, host: e.target.value })} placeholder="core-mosquitto" style={inputStyle} />
            </Field>
            <Field label={t('ha.port', 'Porta')}>
              <input value={form.port} inputMode="numeric" onChange={(e) => setForm({ ...form, port: e.target.value })} style={inputStyle} />
            </Field>
          </div>

          <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 8, marginBottom: 8 }}>
            <Field label={t('ha.username', 'Usuário (opcional)')}>
              <input value={form.username} onChange={(e) => setForm({ ...form, username: e.target.value })} style={inputStyle} />
            </Field>
            <Field label={view?.password_set ? t('ha.pwdSet', 'Senha (definida)') : t('ha.pwdOptional', 'Senha (opcional)')}>
              <input
                type="password"
                value={password}
                onChange={(e) => {
                  setPassword(e.target.value);
                  if (e.target.value) setClearPassword(false);
                }}
                placeholder={view?.password_set ? t('ha.keepPwd', '•••••• (manter)') : ''}
                style={inputStyle}
                disabled={clearPassword}
              />
            </Field>
          </div>
          {view?.password_set && (
            <label style={{ display: 'flex', alignItems: 'center', gap: 6, fontSize: 11.5, opacity: 0.8, marginBottom: 8 }}>
              <input type="checkbox" checked={clearPassword} onChange={(e) => setClearPassword(e.target.checked)} /> {t('ha.removePwd', 'Remover a senha salva')}
            </label>
          )}

          <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 8, marginBottom: 12 }}>
            <Field label="Base topic">
              <input value={form.base_topic} onChange={(e) => setForm({ ...form, base_topic: e.target.value })} style={inputStyle} />
            </Field>
            <Field label="Discovery prefix">
              <input value={form.discovery_prefix} onChange={(e) => setForm({ ...form, discovery_prefix: e.target.value })} style={inputStyle} />
            </Field>
          </div>

          <div style={{ display: 'flex', gap: 8, marginBottom: 6 }}>
            <button type="button" className="btn btn--primary" onClick={save} disabled={busy} style={{ flex: 1 }}>
              <Save size={15} /> {t('ha.saveReconnect', 'Salvar & reconectar')}
            </button>
            <button type="button" className="btn btn--secondary" onClick={republish} disabled={busy} title={t('ha.republishTitle', 'Republicar as entidades no Home Assistant')}>
              <Send size={15} /> Discovery
            </button>
            <button type="button" className="btn btn--secondary" onClick={() => void load()} disabled={busy} title={t('ha.reloadTitle', 'Recarregar status')}>
              <RefreshCw size={15} />
            </button>
          </div>
          {feedback && <p style={{ fontSize: 12, opacity: 0.8, margin: '4px 0 12px' }}>{feedback}</p>}

          <div style={{ borderTop: '1px solid var(--border)', paddingTop: 12 }}>
            <h4 style={{ margin: '0 0 4px', fontSize: 13 }}>{t('ha.zonesByRoom', 'Zonas por cômodo')}</h4>
            <p style={{ fontSize: 11.5, opacity: 0.65, margin: '0 0 8px' }}>
              {t('ha.zonesDesc', 'Renomeie cada receptor com o cômodo que ele cobre. O entity_id do sensor de ocupação por zona no Home Assistant deriva desse nome.')}
            </p>
            {receivers.length === 0 ? (
              <p style={{ fontSize: 11.5, opacity: 0.6 }}>{t('ha.noReceivers', 'Nenhum receptor cadastrado.')}</p>
            ) : (
              receivers.map((d) => {
                const name = deviceNames[d.id] ?? d.name;
                return (
                  <div key={d.id} style={{ marginBottom: 8 }}>
                    <div style={{ display: 'flex', gap: 6, alignItems: 'center' }}>
                      <input
                        value={name}
                        onChange={(e) => setDeviceNames({ ...deviceNames, [d.id]: e.target.value })}
                        style={{ ...inputStyle, flex: 1 }}
                      />
                      <button type="button" className="btn btn--secondary" onClick={() => saveDeviceName(d)} disabled={busy} style={{ fontSize: 11, padding: '4px 8px' }}>
                        {t('ha.saveBtn', 'Salvar')}
                      </button>
                    </div>
                    <code style={{ fontSize: 10.5, opacity: 0.6 }}>binary_sensor.aethersense_zona_{zoneSlug(name)}</code>
                  </div>
                );
              })
            )}

            {receivers.length > 0 && (
              <details style={{ marginTop: 8 }}>
                <summary style={{ fontSize: 12, cursor: 'pointer' }}>{t('ha.yamlExample', 'Exemplo de automação (YAML)')}</summary>
                <pre
                  style={{
                    fontSize: 10.5,
                    background: 'var(--bg-surface)',
                    border: '1px solid var(--border)',
                    borderRadius: 8,
                    padding: 10,
                    overflowX: 'auto',
                    marginTop: 6,
                  }}
                >
{`automation:
  - alias: "Luz pela zona de ${receivers[0].name}"
    trigger:
      - platform: state
        entity_id: binary_sensor.aethersense_zona_${zoneSlug(receivers[0].name)}
        to: "on"
    action:
      - service: light.turn_on
        target:
          entity_id: light.${zoneSlug(receivers[0].name)}`}
                </pre>
              </details>
            )}
          </div>
        </div>
      )}
    </>
  );
}

const inputStyle: React.CSSProperties = {
  width: '100%',
  fontSize: 12.5,
  padding: '5px 7px',
  borderRadius: 6,
  border: '1px solid var(--border)',
  background: 'var(--bg-surface)',
  color: 'var(--text-primary)',
};

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label style={{ display: 'flex', flexDirection: 'column', gap: 3, fontSize: 11, color: 'var(--text-muted)' }}>
      {label}
      {children}
    </label>
  );
}
