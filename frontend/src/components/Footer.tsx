import { useTranslation } from 'react-i18next';
import { STREAM_STALE_WARNING_MS, formatStreamAge } from '../utils/streamHealth';

interface FooterProps {
  status: {
    connected: boolean;
    latencyMs: number;
    packetsPerSecond: number;
    message: string;
    reconnectAttempt: number;
  };
  hasPacket: boolean;
  serverMessage?: string;
}

export function Footer({ status, hasPacket, serverMessage }: FooterProps) {
  const { t } = useTranslation();
  const isStreamStale = status.connected && status.latencyMs >= STREAM_STALE_WARNING_MS;
  const statusTone = !status.connected
    ? 'var(--accent-rose)'
    : isStreamStale
      ? 'var(--accent-rose)'
      : hasPacket
        ? 'var(--accent-emerald)'
        : 'var(--accent-amber)';
  const statusIcon = !status.connected
    ? '🔴'
    : isStreamStale
      ? '⚠️'
      : hasPacket
        ? '🟢'
        : '🟡';

  return (
    <footer className={`footer ${isStreamStale ? 'footer--stale' : ''}`}>
      <div className="footer__status-stack">
        <span className="footer__status-item" style={{ color: statusTone, fontWeight: 600 }}>
          {statusIcon} {status.message}
        </span>

        <span className="footer__status-item" style={{ color: 'var(--text-muted)' }}>
          ⚡ {status.packetsPerSecond} {t('footer.pps', 'pps')}
        </span>

        <span className="footer__status-item" style={{ color: 'var(--text-muted)' }}>
          ⏱ {status.connected
            ? hasPacket
              ? t('footer.lastPacket', 'último pacote há {{age}}', { age: formatStreamAge(status.latencyMs) })
              : t('footer.waitingFirst', 'aguardando primeiro pacote há {{age}}', { age: formatStreamAge(status.latencyMs) })
            : t('footer.noStream', 'sem stream por {{age}}', { age: formatStreamAge(status.latencyMs) })}
        </span>

        {!status.connected && status.reconnectAttempt > 0 && (
          <span className="footer__status-item" style={{ color: 'var(--text-muted)' }}>
            {t('footer.attempt', '↻ Tentativa {{count}}', { count: status.reconnectAttempt })}
          </span>
        )}

        {serverMessage && (
          <span className="footer__status-item footer__status-item--server">
            {serverMessage}
          </span>
        )}
      </div>
      <span className="footer__version">{t('footer.version', 'AetherSense v0.2.0 (React)')}</span>
    </footer>
  );
}
