import { useState, useEffect, useRef, useCallback } from 'react';
import type { CSIPacket, StreamStatus } from '../types/csi';
import { formatStreamAge, STREAM_STALE_WARNING_MS } from '../utils/streamHealth';
import i18n from '../i18n/i18n';

const DEFAULT_STREAM_STATUS: StreamStatus = {
  source: 'idle',
  replay_active: false,
  replay_file: null,
  reason: i18n.t('stream.noPacketsYet', 'Sem pacotes CSI ainda'),
};

interface WSStatus {
  connected: boolean;
  latencyMs: number;
  packetsPerSecond: number;
  message: string;
  reconnectAttempt: number;
  hasPacket: boolean;
  stream: StreamStatus;
}

const RECONNECT_DELAY_MS = 2000;
const METRICS_REFRESH_MS = 1000;

function streamMessage(connected: boolean, hasPacket: boolean, ageMs: number, stream: StreamStatus) {
  if (!connected) {
    return i18n.t('stream.disconnected', 'Desconectado - reconectando');
  }

  if (stream.source === 'csv_replay' && stream.replay_active) {
    return i18n.t('stream.replayActive', 'Replay CSV M6 ativo');
  }

  if (stream.source === 'idle' && hasPacket) {
    return stream.reason || i18n.t('stream.replayStopped', 'Replay CSV parado; exibindo ultimo snapshot');
  }

  if (ageMs >= STREAM_STALE_WARNING_MS) {
    return hasPacket
      ? i18n.t('stream.udpStopped', 'Stream UDP parado há {{age}}', { age: formatStreamAge(ageMs) })
      : i18n.t('stream.noCsiReceived', 'Nenhum pacote CSI recebido há {{age}}', { age: formatStreamAge(ageMs) });
  }

  if (stream.source === 'udp_live' && hasPacket) {
    return i18n.t('stream.csiActive', 'Fluxo CSI ativo');
  }

  return i18n.t('stream.waitingCsi', 'Conectado - aguardando pacotes CSI');
}

export function useAetherWebSocket(url: string) {
  const [data, setData] = useState<CSIPacket | null>(null);
  const [status, setStatus] = useState<WSStatus>({
    connected: false,
    latencyMs: 0,
    packetsPerSecond: 0,
    message: i18n.t('header.status.connecting', 'Conectando...'),
    reconnectAttempt: 0,
    hasPacket: false,
    stream: DEFAULT_STREAM_STATUS,
  });

  const ws = useRef<WebSocket | null>(null);
  const reconnectTimer = useRef<number | null>(null);
  const lastActivityTime = useRef<number | null>(null);
  const lastSeenCount = useRef<number | null>(null);
  const lastRateCount = useRef<number>(0);
  const rateWindowStart = useRef<number>(0);
  const reconnectAttempt = useRef<number>(0);
  const connectionToken = useRef<number>(0);
  const isMounted = useRef<boolean>(false);
  const dataRef = useRef<CSIPacket | null>(null);
  const lastDataFlush = useRef<number>(0);
  const dataFlushTimer = useRef<number | null>(null);

  // Subscriber pattern for charts/map to avoid React re-renders on every packet
  const listeners = useRef<((data: CSIPacket) => void)[]>([]);

  const subscribe = useCallback((listener: (data: CSIPacket) => void) => {
    listeners.current.push(listener);
    return () => {
      listeners.current = listeners.current.filter((l) => l !== listener);
    };
  }, []);

  function connectSocket() {
    if (reconnectTimer.current) {
      window.clearTimeout(reconnectTimer.current);
      reconnectTimer.current = null;
    }

    const socketToken = connectionToken.current + 1;
    connectionToken.current = socketToken;

    if (ws.current) {
      ws.current.close();
    }

    const socket = new WebSocket(url);
    ws.current = socket;

    socket.onopen = () => {
      if (connectionToken.current !== socketToken) {
        return;
      }

      reconnectAttempt.current = 0;
      rateWindowStart.current = Date.now();
      lastActivityTime.current = Date.now();
      lastSeenCount.current = null;
      lastRateCount.current = 0;

      setStatus((s) => ({
        ...s,
        connected: true,
        latencyMs: 0,
        packetsPerSecond: 0,
        message: i18n.t('stream.waitingCsi', 'Conectado - aguardando pacotes CSI'),
        reconnectAttempt: 0,
        hasPacket: false,
        stream: DEFAULT_STREAM_STATUS,
      }));
    };

    socket.onmessage = (event) => {
      if (connectionToken.current !== socketToken) {
        return;
      }

      try {
        const parsed: CSIPacket = JSON.parse(event.data);
        dataRef.current = parsed;
        const currentCount = typeof parsed.count === 'number' ? parsed.count : 0;
        const previousCount = lastSeenCount.current;
        const countChanged = previousCount === null || currentCount !== previousCount;
        const hasPacket = currentCount > 0;
        const stream = parsed.stream ?? (hasPacket
          ? { source: 'udp_live', replay_active: false, replay_file: null, reason: i18n.t('stream.csiActive', 'Fluxo CSI ativo') } as StreamStatus
          : DEFAULT_STREAM_STATUS);

        if (countChanged && hasPacket) {
          lastActivityTime.current = Date.now();
        }

        setStatus((s) => {
          const latencyMs = countChanged && hasPacket ? 0 : s.latencyMs;
          return {
            ...s,
            connected: true,
            message: streamMessage(true, hasPacket, latencyMs, stream),
            hasPacket,
            latencyMs,
            stream,
          };
        });

        lastSeenCount.current = currentCount;

        // Notify direct listeners (for high-performance charts/maps)
        listeners.current.forEach((listener) => listener(parsed));

        // Update UI state for summary metrics and connection freshness.
        const now = performance.now();
        if (now - lastDataFlush.current >= 200) {
          lastDataFlush.current = now;
          setData(parsed);
        } else if (!dataFlushTimer.current) {
          dataFlushTimer.current = window.setTimeout(() => {
            dataFlushTimer.current = null;
            lastDataFlush.current = performance.now();
            if (dataRef.current) setData(dataRef.current);
          }, 200 - (now - lastDataFlush.current));
        }
      } catch (err) {
        console.warn('Invalid JSON from WS', err);
      }
    };

    socket.onclose = () => {
      if (connectionToken.current !== socketToken) {
        return;
      }

      if (ws.current === socket) {
        ws.current = null;
      }

      if (!isMounted.current) {
        return;
      }

      const nextAttempt = reconnectAttempt.current + 1;
      reconnectAttempt.current = nextAttempt;

      setStatus((s) => ({
        ...s,
        connected: false,
        message: i18n.t('stream.reconnecting', 'Desconectado - reconectando em {{sec}}s', { sec: RECONNECT_DELAY_MS / 1000 }),
        reconnectAttempt: nextAttempt,
        stream: s.stream,
      }));

      if (!reconnectTimer.current) {
        reconnectTimer.current = window.setTimeout(() => {
          reconnectTimer.current = null;
          connectSocket();
        }, RECONNECT_DELAY_MS);
      }
    };

    socket.onerror = () => {
      if (connectionToken.current !== socketToken) {
        return;
      }

      setStatus((s) => ({
        ...s,
        connected: false,
        message: i18n.t('stream.wsFailed', 'Falha no WebSocket - tentando reconectar'),
      }));

      socket.close();
    };
  }

  useEffect(() => {
    isMounted.current = true;
    connectSocket();

    return () => {
      isMounted.current = false;
      connectionToken.current += 1;

      if (ws.current) {
        ws.current.close();
        ws.current = null;
      }

      if (reconnectTimer.current) {
        window.clearTimeout(reconnectTimer.current);
        reconnectTimer.current = null;
      }
      if (dataFlushTimer.current) {
        window.clearTimeout(dataFlushTimer.current);
        dataFlushTimer.current = null;
      }
    };
  }, [url]); // eslint-disable-line react-hooks/exhaustive-deps

  // PPS and freshness calculation interval.
  useEffect(() => {
    const timer = window.setInterval(() => {
      const now = Date.now();
      const elapsedSec = Math.max(1, (now - rateWindowStart.current) / 1000);
      const latestCount = dataRef.current?.count ?? 0;
      const stream = dataRef.current?.stream ?? DEFAULT_STREAM_STATUS;
      const deltaPackets = latestCount >= lastRateCount.current
        ? latestCount - lastRateCount.current
        : latestCount;
      const pps = deltaPackets / elapsedSec;
      const ageMs = lastActivityTime.current ? now - lastActivityTime.current : 0;

      setStatus((s) => ({
        ...s,
        packetsPerSecond: Math.round(pps),
        latencyMs: ageMs,
        stream,
        message: streamMessage(s.connected, s.hasPacket, ageMs, stream),
      }));

      lastRateCount.current = latestCount;
      rateWindowStart.current = now;
    }, METRICS_REFRESH_MS);

    return () => clearInterval(timer);
  }, []);

  return { data, status, subscribe, latestDataRef: dataRef };
}
