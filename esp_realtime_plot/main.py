import argparse
import asyncio
import csv
import io
import threading
import socket
from contextlib import asynccontextmanager
from collections import deque
from dataclasses import dataclass, field
from typing import List, Optional
from loguru import logger
import numpy as np
from fastapi import FastAPI, WebSocket, WebSocketDisconnect
from fastapi.responses import HTMLResponse
import uvicorn


UDP_PORT = 5005
WEBSOCKET_INTERVAL_SECONDS = 0.10
HEATMAP_HISTORY_LENGTH = 180
HEATMAP_MAX_SUBCARRIERS = 256
DEFAULT_HEATMAP_WINDOW = 120
DEFAULT_STFT_WINDOW = 80
DEFAULT_PCA_WINDOW = 120
DEFAULT_PADDING = 0.15
STFT_SAMPLE_RATE_HZ = 10.0


CSI_FIELD_NAMES = [
    "seq",
    "mac",
    "rssi",
    "rate",
    "sig_mode",
    "mcs",
    "cwb",
    "smoothing",
    "not_sounding",
    "aggregation",
    "stbc",
    "fec_coding",
    "sgi",
    "noise_floor",
    "ampdu_cnt",
    "channel",
    "secondary_channel",
    "timestamp",
    "ant",
    "sig_len",
    "rx_state",
    "len",
    "first_word_invalid",
    "payload",
]


@dataclass
class CsiPacket:
    raw_line: str
    seq: Optional[int] = None
    mac: str = ""
    rssi: Optional[int] = None
    rate: Optional[int] = None
    sig_mode: Optional[int] = None
    mcs: Optional[int] = None
    cwb: Optional[int] = None
    smoothing: Optional[int] = None
    not_sounding: Optional[int] = None
    aggregation: Optional[int] = None
    stbc: Optional[int] = None
    fec_coding: Optional[int] = None
    sgi: Optional[int] = None
    noise_floor: Optional[int] = None
    ampdu_cnt: Optional[int] = None
    channel: Optional[int] = None
    secondary_channel: Optional[int] = None
    timestamp: Optional[int] = None
    ant: Optional[int] = None
    sig_len: Optional[int] = None
    rx_state: Optional[int] = None
    length: Optional[int] = None
    first_word_invalid: Optional[int] = None
    payload: str = ""
    samples: List[float] = field(default_factory=list)
    complex_samples: List[complex] = field(default_factory=list)
    amplitudes: List[float] = field(default_factory=list)
    phases: List[float] = field(default_factory=list)


def _parse_int(value: str) -> Optional[int]:
    value = value.strip()
    if not value:
        return None
    try:
        return int(value)
    except ValueError:
        try:
            return int(float(value))
        except ValueError:
            return None


def _sanitize_float(value: float) -> float:
    try:
        numeric_value = float(value)
    except (TypeError, ValueError):
        return 0.0

    if np.isfinite(numeric_value):
        return numeric_value
    return 0.0


def _normalize_series(values: List[float], max_length: int) -> List[float]:
    normalized = [_sanitize_float(value) for value in values[:max_length]]
    if len(normalized) < max_length:
        normalized.extend([0.0] * (max_length - len(normalized)))
    return normalized


def _build_heatmap_matrix(history: List[List[float]]) -> List[List[float]]:
    if not history:
        return []

    width = min(HEATMAP_MAX_SUBCARRIERS, max(len(row) for row in history))
    if width <= 0:
        return []

    return [_normalize_series(row, width) for row in history]


def _moving_average(values: np.ndarray, window_size: int) -> np.ndarray:
    if values.size == 0:
        return values

    window_size = max(1, min(window_size, values.size))
    if window_size == 1:
        return values.astype(float, copy=True)

    kernel = np.ones(window_size, dtype=float) / window_size
    padded = np.pad(values, (window_size // 2,), mode="edge")
    filtered = np.convolve(padded, kernel, mode="valid")
    return filtered[: values.size]


def _sanitize_phase_series(phase_values: List[float]) -> List[float]:
    if not phase_values:
        return []

    phase_array = np.asarray(phase_values, dtype=float)
    if phase_array.size < 3:
        return phase_array.astype(float).tolist()

    unwrapped = np.unwrap(phase_array)
    x_axis = np.arange(unwrapped.size, dtype=float)
    slope, intercept = np.polyfit(x_axis, unwrapped, 1)
    detrended = unwrapped - (slope * x_axis + intercept)
    sanitized = _moving_average(detrended, 7)
    return sanitized.astype(float).tolist()


def _compute_pdp(complex_samples: List[complex]) -> tuple[List[float], List[float]]:
    if not complex_samples:
        return [], []

    complex_array = np.asarray(complex_samples, dtype=complex)
    fft_size = 256
    spectrum = np.fft.ifft(complex_array, n=fft_size)
    power_db = 20.0 * np.log10(np.abs(spectrum) + 1e-9)
    delay_axis = np.linspace(0.0, 10.0, power_db.size)
    return delay_axis.astype(float).tolist(), power_db.astype(float).tolist()


def _compute_stft(signal: List[float], window_size: int, sample_rate_hz: float) -> tuple[List[float], List[float], List[List[float]]]:
    if not signal:
        return [], [], []

    signal_array = np.asarray(signal, dtype=float)
    window_size = max(8, min(window_size, signal_array.size))
    if signal_array.size < window_size:
        pad_width = window_size - signal_array.size
        pad_mode = "edge" if signal_array.size > 1 else "constant"
        signal_array = np.pad(signal_array, (0, pad_width), mode=pad_mode)

    hop_size = max(1, window_size // 4)
    window = np.hanning(window_size)
    freq_axis = np.fft.rfftfreq(window_size, d=1.0 / sample_rate_hz)
    time_axis: List[float] = []
    spectrum_columns: List[np.ndarray] = []

    for start in range(0, signal_array.size - window_size + 1, hop_size):
        segment = signal_array[start : start + window_size]
        centered = segment - np.mean(segment)
        fft_values = np.fft.rfft(centered * window)
        magnitude_db = 20.0 * np.log10(np.abs(fft_values) + 1e-9)
        spectrum_columns.append(magnitude_db)
        time_axis.append((start + window_size / 2.0) / sample_rate_hz)

    if not spectrum_columns:
        segment = signal_array[-window_size:]
        centered = segment - np.mean(segment)
        fft_values = np.fft.rfft(centered * window)
        spectrum_columns.append(20.0 * np.log10(np.abs(fft_values) + 1e-9))
        time_axis.append(signal_array.size / sample_rate_hz)

    stft_matrix = np.stack(spectrum_columns, axis=1)
    return freq_axis.astype(float).tolist(), time_axis, stft_matrix.astype(float).tolist()


def _compute_pca(history: List[List[float]], window_size: int) -> tuple[List[int], List[float], List[float]]:
    if not history:
        return [], [], []

    selected = history[-max(2, window_size) :]
    matrix = _build_heatmap_matrix(selected)
    if not matrix:
        return [], [], []

    data = np.asarray(matrix, dtype=float)
    data = data - np.mean(data, axis=0, keepdims=True)
    if data.shape[0] < 2 or data.shape[1] < 2:
        return [], [], []

    u, s, _ = np.linalg.svd(data, full_matrices=False)
    scores = u[:, :2] * s[:2]
    packet_axis = list(range(len(selected)))
    pc1 = scores[:, 0].astype(float).tolist()
    pc2 = scores[:, 1].astype(float).tolist()
    return packet_axis, pc1, pc2


def _compute_windowed_heatmap(history: List[List[float]], window_size: int) -> List[List[float]]:
    if not history:
        return []

    selected = history[-max(1, window_size) :]
    return _build_heatmap_matrix(selected)


@dataclass
class DashboardSettings:
    heatmap_window: int = DEFAULT_HEATMAP_WINDOW
    stft_window: int = DEFAULT_STFT_WINDOW
    pca_window: int = DEFAULT_PCA_WINDOW
    padding: float = DEFAULT_PADDING


def _clamp_settings(settings: DashboardSettings) -> DashboardSettings:
    settings.heatmap_window = int(np.clip(settings.heatmap_window, 10, HEATMAP_HISTORY_LENGTH))
    settings.stft_window = int(np.clip(settings.stft_window, 16, HEATMAP_HISTORY_LENGTH))
    settings.pca_window = int(np.clip(settings.pca_window, 10, HEATMAP_HISTORY_LENGTH))
    settings.padding = float(np.clip(settings.padding, 0.0, 0.6))
    return settings


def parse_csi_packet(line: str) -> Optional[CsiPacket]:
    if not line.startswith("CSI_DATA,"):
        return None
    try:
        parts = next(csv.reader(io.StringIO(line)))
    except csv.Error:
        return None
    if len(parts) < 3:
        return None

    payload_index = None
    for index, p in enumerate(parts):
        if p.startswith("[") or p.startswith('"['):
            payload_index = index
            break

    if payload_index is None or payload_index < 2:
        return None

    data_fields = parts[1:payload_index]
    payload = ",".join(parts[payload_index:]).strip().strip('"')
    if payload.startswith("[") and payload.endswith("]"):
        payload = payload[1:-1]

    packet = CsiPacket(raw_line=line, payload=payload)

    for field_name, raw_value in zip(CSI_FIELD_NAMES, data_fields):
        parsed_int = _parse_int(raw_value)
        if field_name in ("mac", "payload"):
            setattr(packet, field_name, raw_value)
        else:
            setattr(packet, field_name, parsed_int)

    samples: List[float] = []
    if payload:
        for token in payload.split(","):
            token = token.strip()
            if not token:
                continue
            try:
                samples.append(float(token))
            except ValueError:
                continue
    packet.samples = samples
    if len(samples) >= 2:
        pair_count = len(samples) // 2
        complex_values = np.asarray(samples[: pair_count * 2], dtype=float).reshape(
            pair_count, 2
        )
        imaginary_part = complex_values[:, 0]
        real_part = complex_values[:, 1]
        complex_samples = real_part + 1j * imaginary_part
        packet.complex_samples = complex_samples.tolist()
        packet.amplitudes = np.abs(complex_samples).astype(float).tolist()
        packet.phases = np.unwrap(np.angle(complex_samples)).astype(float).tolist()
    else:
        packet.complex_samples = []
        packet.amplitudes = samples[:]
        packet.phases = []

    return packet


# Global Thread-Safe State
class AppState:
    def __init__(self):
        self.lock = threading.Lock()
        self.packet_count = 0
        self.latest_packet: Optional[CsiPacket] = None
        self.amplitude_history = deque(maxlen=HEATMAP_HISTORY_LENGTH)
        self.phase_history = deque(maxlen=HEATMAP_HISTORY_LENGTH)
        self.signal_history = deque(maxlen=HEATMAP_HISTORY_LENGTH)
        self.heatmap_history = deque(maxlen=HEATMAP_HISTORY_LENGTH)
        self.settings = _clamp_settings(DashboardSettings())
        self.running = True


state = AppState()


def udp_listener_loop(port: int):
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind(("0.0.0.0", port))
    sock.settimeout(1.0)
    logger.info(f"[*] Escutando UDP na porta {port}...")

    while state.running:
        try:
            data, addr = sock.recvfrom(4096)
            line = data.decode("utf-8", errors="ignore").strip()
            if not line:
                continue

            packet = parse_csi_packet(line)
            if packet:
                with state.lock:
                    state.packet_count += 1
                    state.latest_packet = packet
                    if packet.amplitudes:
                        amplitudes = list(packet.amplitudes)
                        phases = list(packet.phases)
                        state.amplitude_history.append(amplitudes)
                        state.phase_history.append(phases)
                        state.signal_history.append(float(np.mean(amplitudes)))
                        state.heatmap_history.append(amplitudes)
        except socket.timeout:
            continue
        except Exception as e:
            logger.info(f"UDP Error: {e}")


@asynccontextmanager
async def lifespan(app: FastAPI):
    # Startup
    state.running = True
    udp_thread = threading.Thread(target=udp_listener_loop, args=(UDP_PORT,), daemon=True)
    udp_thread.start()
    yield
    # Shutdown
    state.running = False


app = FastAPI(title="ESP32 CSI Real-Time Dashboard", lifespan=lifespan)


def _get_snapshot(settings: DashboardSettings) -> dict:
    with state.lock:
        packet = state.latest_packet
        packet_count = state.packet_count
        amplitude_history = list(state.amplitude_history)
        phase_history = list(state.phase_history)
        signal_history = list(state.signal_history)

    if packet is None:
        return {
            "count": packet_count,
            "rssi": None,
            "rate": None,
            "subcarriers": None,
            "amplitudes": [],
            "phase_raw": [],
            "phase_sanitized": [],
            "heatmap": [],
            "stft": {"x": [], "y": [], "z": []},
            "pca": {"packet_axis": [], "pc1": [], "pc2": []},
            "pdp": {"x": [], "y": []},
            "padding": settings.padding,
            "footer_text": "Sem pacotes CSI ainda",
        }

    latest_amplitudes = list(packet.amplitudes)
    if packet.complex_samples:
        latest_complex = np.asarray(packet.complex_samples, dtype=complex)
        latest_phase_raw = np.angle(latest_complex).astype(float).tolist()
        latest_phase_sanitized = _sanitize_phase_series(latest_phase_raw)
        pdp_x, pdp_y = _compute_pdp(packet.complex_samples)
    else:
        latest_phase_raw = list(packet.phases)
        latest_phase_sanitized = _sanitize_phase_series(latest_phase_raw)
        pdp_x, pdp_y = [], []

    heatmap_matrix = _compute_windowed_heatmap(amplitude_history, settings.heatmap_window)
    stft_x, stft_y, stft_z = _compute_stft(signal_history[-settings.stft_window :], settings.stft_window, STFT_SAMPLE_RATE_HZ)
    packet_axis, pc1, pc2 = _compute_pca(amplitude_history, settings.pca_window)

    return {
        "count": packet_count,
        "rssi": packet.rssi,
        "rate": packet.rate,
        "subcarriers": len(latest_amplitudes),
        "amplitudes": latest_amplitudes,
        "phase_raw": latest_phase_raw,
        "phase_sanitized": latest_phase_sanitized,
        "heatmap": heatmap_matrix,
        "stft": {"x": stft_x, "y": stft_y, "z": stft_z},
        "pca": {"packet_axis": packet_axis, "pc1": pc1, "pc2": pc2},
        "pdp": {"x": pdp_x, "y": pdp_y},
        "padding": settings.padding,
        "footer_text": (
            f"Porta: {UDP_PORT} | Pacotes: {packet_count} | "
            f"Heatmap: {settings.heatmap_window} | STFT: {settings.stft_window} | PCA: {settings.pca_window}"
        ),
    }


@app.get("/api/settings")
def get_settings():
    with state.lock:
        settings = state.settings
    return {
        "heatmap_window": settings.heatmap_window,
        "stft_window": settings.stft_window,
        "pca_window": settings.pca_window,
        "padding": settings.padding,
    }


@app.post("/api/settings")
async def update_settings(payload: dict):
    with state.lock:
        current = state.settings
        current.heatmap_window = payload.get("heatmap_window", current.heatmap_window)
        current.stft_window = payload.get("stft_window", current.stft_window)
        current.pca_window = payload.get("pca_window", current.pca_window)
        current.padding = payload.get("padding", current.padding)
        state.settings = _clamp_settings(current)
        settings = state.settings

    return {
        "heatmap_window": settings.heatmap_window,
        "stft_window": settings.stft_window,
        "pca_window": settings.pca_window,
        "padding": settings.padding,
    }


@app.post("/api/clear")
async def clear_history():
    with state.lock:
        state.amplitude_history.clear()
        state.phase_history.clear()
        state.signal_history.clear()
        state.heatmap_history.clear()
        state.packet_count = 0
        state.latest_packet = None
    return {"ok": True}


HTML_DASHBOARD = """
<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>ESP32 CSI Dashboard</title>
    <script src="https://cdn.plot.ly/plotly-2.24.1.min.js"></script>
    <style>
        :root {
            color-scheme: dark;
            --bg: #f4f4f0;
            --bg-2: #e9ecef;
            --panel: rgba(255, 255, 255, 0.88);
            --panel-border: rgba(20, 30, 48, 0.08);
            --text: #222831;
            --muted: #607085;
            --accent: #1f77b4;
            --accent-2: #ff7f0e;
            --danger: #d84a4a;
        }

        * { box-sizing: border-box; }

        body {
            margin: 0;
            min-height: 100vh;
            font-family: Inter, "Segoe UI", Tahoma, Geneva, Verdana, sans-serif;
            color: var(--text);
            background:
                radial-gradient(circle at top left, rgba(31, 119, 180, 0.12), transparent 28%),
                radial-gradient(circle at top right, rgba(255, 127, 14, 0.10), transparent 24%),
                linear-gradient(180deg, var(--bg) 0%, var(--bg-2) 100%);
        }

        .shell {
            max-width: 1600px;
            margin: 0 auto;
            padding: 28px;
        }

        .title {
            text-align: center;
            font-size: clamp(20px, 2.4vw, 28px);
            font-weight: 800;
            margin: 0 0 20px;
            letter-spacing: 0.01em;
        }

        .header {
            display: grid;
            grid-template-columns: 1.5fr 1fr;
            gap: 20px;
            align-items: stretch;
            margin-bottom: 22px;
        }

        .hero, .status {
            background: var(--panel);
            border: 1px solid var(--panel-border);
            border-radius: 22px;
            box-shadow: 0 24px 70px rgba(0, 0, 0, 0.28);
            backdrop-filter: blur(14px);
        }

        .hero {
            padding: 24px 26px;
            position: relative;
            overflow: hidden;
        }

        .hero::after {
            content: '';
            position: absolute;
            inset: auto -30px -40px auto;
            width: 220px;
            height: 220px;
            background: radial-gradient(circle, rgba(79, 209, 197, 0.22), transparent 68%);
            pointer-events: none;
        }

        .eyebrow {
            text-transform: uppercase;
            letter-spacing: 0.24em;
            color: var(--accent);
            font-size: 11px;
            margin-bottom: 10px;
        }

        .hero h1 {
            margin: 0;
            font-size: clamp(24px, 2.6vw, 38px);
            line-height: 1.05;
        }

        .hero p {
            margin: 14px 0 0;
            max-width: 760px;
            color: var(--muted);
            font-size: 14px;
            line-height: 1.55;
        }

        .status {
            padding: 18px;
            display: grid;
            grid-template-columns: repeat(2, minmax(0, 1fr));
            gap: 12px;
        }

        .metric {
            background: rgba(255, 255, 255, 0.03);
            border: 1px solid rgba(255, 255, 255, 0.05);
            border-radius: 18px;
            padding: 14px 16px;
        }

        .metric .label {
            display: block;
            color: var(--muted);
            font-size: 11px;
            letter-spacing: 0.08em;
            text-transform: uppercase;
            margin-bottom: 6px;
        }

        .metric .value {
            font-size: 21px;
            font-weight: 700;
            color: var(--text);
        }

        .controls {
            display: grid;
            gap: 10px;
            margin-top: 18px;
        }

        .control {
            display: grid;
            grid-template-columns: 120px 1fr 58px;
            gap: 10px;
            align-items: center;
            font-size: 13px;
            color: var(--text);
        }

        .control input[type="range"] {
            width: 100%;
        }

        .control output {
            text-align: right;
            color: var(--muted);
            font-variant-numeric: tabular-nums;
        }

        .grid {
            display: grid;
            grid-template-columns: repeat(2, minmax(0, 1fr));
            gap: 20px;
        }

        .card {
            background: var(--panel);
            border: 1px solid var(--panel-border);
            border-radius: 22px;
            padding: 18px;
            box-shadow: 0 20px 40px rgba(0, 0, 0, 0.10);
            backdrop-filter: blur(10px);
            min-height: 340px;
        }

        .card.wide {
            grid-column: 1 / -1;
        }

        h2 {
            margin: 0 0 12px;
            font-size: 16px;
            color: var(--text);
            letter-spacing: 0.02em;
        }

        .plot {
            width: 100%;
            height: 300px;
        }

        .footer {
            display: grid;
            grid-template-columns: 1fr auto auto;
            gap: 12px;
            align-items: center;
            margin-top: 16px;
            padding: 12px 16px;
            color: var(--muted);
            font-size: 12px;
        }

        .actions {
            display: flex;
            gap: 10px;
            justify-content: flex-end;
        }

        .button {
            min-width: 112px;
            border: 0;
            border-radius: 10px;
            padding: 10px 16px;
            font-weight: 700;
            cursor: pointer;
            color: white;
        }

        .button.primary { background: var(--accent); }
        .button.secondary { background: var(--accent-2); }

        .button:hover {
            filter: brightness(1.04);
        }

        @media (max-width: 1100px) {
            .header, .grid {
                grid-template-columns: 1fr;
            }
        }

        @media (max-width: 720px) {
            .shell {
                padding: 16px;
            }

            .control {
                grid-template-columns: 1fr;
            }

            .status {
                grid-template-columns: 1fr;
            }

            .card {
                min-height: 320px;
            }
        }
    </style>
</head>
<body>
    <div class="shell">
        <h1 class="title">ESP32 CSI em tempo real</h1>
        <div class="header">
            <section class="hero">
                <div class="eyebrow">ESP32 CSI real-time</div>
                <h1>Controle os gráficos com os sliders e acompanhe o CSI em múltiplas visões.</h1>
                <p>O painel usa waterfall, espectrograma STFT, fase bruta vs sanitizada, PCA das amplitudes e PDP. Os controles alteram a janela de cada gráfico em tempo real.</p>
                <div class="controls">
                    <label class="control">Heatmap <input id="ctl-heatmap" type="range" min="20" max="180" step="1" value="120"><output id="ctl-heatmap-value">120</output></label>
                    <label class="control">STFT <input id="ctl-stft" type="range" min="16" max="180" step="1" value="80"><output id="ctl-stft-value">80</output></label>
                    <label class="control">PCA <input id="ctl-pca" type="range" min="20" max="180" step="1" value="120"><output id="ctl-pca-value">120</output></label>
                    <label class="control">Padding <input id="ctl-padding" type="range" min="0" max="60" step="1" value="15"><output id="ctl-padding-value">0.15</output></label>
                </div>
            </section>
            <section class="status">
                <div class="metric"><span class="label">Pacotes</span><span class="value" id="stat-count">0</span></div>
                <div class="metric"><span class="label">RSSI</span><span class="value" id="stat-rssi">--</span></div>
                <div class="metric"><span class="label">Rate</span><span class="value" id="stat-rate">--</span></div>
                <div class="metric"><span class="label">Subportadoras</span><span class="value" id="stat-subcarriers">--</span></div>
            </section>
        </div>

        <div class="grid">
            <div class="card wide">
                <h2>Mapa de calor da amplitude das subportadoras</h2>
                <div id="plot-heatmap" class="plot"></div>
            </div>
            <div class="card">
                <h2>Espectrograma STFT do sinal de movimento</h2>
                <div id="plot-stft" class="plot"></div>
            </div>
            <div class="card">
                <h2>Fase bruta vs fase sanitizada</h2>
                <div id="plot-phase" class="plot"></div>
            </div>
            <div class="card">
                <h2>PCA das amplitudes CSI</h2>
                <div id="plot-pca" class="plot"></div>
            </div>
            <div class="card">
                <h2>Perfil de atraso de potência (PDP)</h2>
                <div id="plot-pdp" class="plot"></div>
            </div>
        </div>

        <div class="footer">
            <div id="footer-status">Porta: 5005 | Atualizando...</div>
            <div class="actions">
                <button class="button primary" id="btn-pause">Pausar</button>
                <button class="button secondary" id="btn-clear">Limpar</button>
            </div>
        </div>
    </div>

    <script>
        const plotConfig = { responsive: true, displayModeBar: false };
        const baseLayout = {
            paper_bgcolor: 'rgba(0, 0, 0, 0)',
            plot_bgcolor: 'rgba(0, 0, 0, 0)',
            font: { color: '#1f2d3d' },
            margin: { t: 10, b: 40, l: 50, r: 20 },
            xaxis: { gridcolor: 'rgba(31, 45, 61, 0.08)', zeroline: false },
            yaxis: { gridcolor: 'rgba(31, 45, 61, 0.08)', zeroline: false },
            uirevision: 'csi-dashboard'
        };

        const layoutHeatmap = {
            ...baseLayout,
            xaxis: { ...baseLayout.xaxis, title: 'Tempo (pacotes)' },
            yaxis: { ...baseLayout.yaxis, title: 'Subportadora', autorange: 'reversed' }
        };
        const layoutStft = {
            ...baseLayout,
            xaxis: { ...baseLayout.xaxis, title: 'Tempo (s)' },
            yaxis: { ...baseLayout.yaxis, title: 'Frequência (Hz)' }
        };
        const layoutPhase = {
            ...baseLayout,
            xaxis: { ...baseLayout.xaxis, title: 'Subportadora' },
            yaxis: { ...baseLayout.yaxis, title: 'Fase (rad)' }
        };
        const layoutPca = {
            ...baseLayout,
            xaxis: { ...baseLayout.xaxis, title: 'Pacote' },
            yaxis: { ...baseLayout.yaxis, title: 'Score PCA' }
        };
        const layoutPdp = {
            ...baseLayout,
            xaxis: { ...baseLayout.xaxis, title: 'Atraso (us)' },
            yaxis: { ...baseLayout.yaxis, title: 'Potência (dB)' }
        };

        function paddingRange(values, ratio) {
            if (!values || values.length === 0) {
                return undefined;
            }
            const minValue = Math.min(...values);
            const maxValue = Math.max(...values);
            const span = Math.max(1e-6, maxValue - minValue);
            return [minValue - span * ratio, maxValue + span * ratio];
        }

        function layoutWithPadding(layout, values, ratio) {
            const nextLayout = JSON.parse(JSON.stringify(layout));
            const range = paddingRange(values, ratio);
            if (range) {
                nextLayout.yaxis.range = range;
            }
            return nextLayout;
        }

        function transposeHeatmap(matrix) {
            if (!matrix || matrix.length === 0) {
                return [];
            }
            const maxColumns = matrix.reduce((maxValue, row) => Math.max(maxValue, row.length), 0);
            return Array.from({ length: maxColumns }, (_, columnIndex) => (
                matrix.map((row) => (columnIndex < row.length ? row[columnIndex] : null))
            ));
        }

        function lineTrace(values, color, name) {
            return [{
                x: Array.from({ length: values.length }, (_, index) => index),
                y: values,
                type: 'scatter',
                mode: 'lines',
                name,
                line: { color, width: 2 }
            }];
        }

        Plotly.newPlot('plot-heatmap', [{ z: [], type: 'heatmap', colorscale: 'Viridis', zsmooth: false }], layoutHeatmap, plotConfig);
        Plotly.newPlot('plot-stft', [{ z: [], type: 'heatmap', colorscale: 'Magma', zsmooth: false }], layoutStft, plotConfig);
        Plotly.newPlot('plot-phase', [
            { x: [], y: [], type: 'scatter', mode: 'lines', name: 'Bruta', line: { color: '#95a5a6', width: 1.5 } },
            { x: [], y: [], type: 'scatter', mode: 'lines', name: 'Sanitizada', line: { color: '#ff4136', width: 2 } }
        ], layoutPhase, plotConfig);
        Plotly.newPlot('plot-pca', [
            { x: [], y: [], type: 'scatter', mode: 'lines', name: 'PC1', line: { color: '#2ecc71', width: 2 } },
            { x: [], y: [], type: 'scatter', mode: 'lines', name: 'PC2', line: { color: '#3498db', width: 2 } }
        ], layoutPca, plotConfig);
        Plotly.newPlot('plot-pdp', [{ x: [], y: [], type: 'bar', marker: { color: '#f39c12' } }], layoutPdp, plotConfig);

        const ws = new WebSocket(`ws://${window.location.host}/ws`);
        let paused = false;
        let lastHeatmapUpdate = 0;
        const controls = {
            heatmap: document.getElementById('ctl-heatmap'),
            stft: document.getElementById('ctl-stft'),
            pca: document.getElementById('ctl-pca'),
            padding: document.getElementById('ctl-padding')
        };
        const outputs = {
            heatmap: document.getElementById('ctl-heatmap-value'),
            stft: document.getElementById('ctl-stft-value'),
            pca: document.getElementById('ctl-pca-value'),
            padding: document.getElementById('ctl-padding-value')
        };

        function readSettings() {
            return {
                heatmap_window: parseInt(controls.heatmap.value, 10),
                stft_window: parseInt(controls.stft.value, 10),
                pca_window: parseInt(controls.pca.value, 10),
                padding: parseInt(controls.padding.value, 10) / 100.0
            };
        }

        function refreshLabels() {
            outputs.heatmap.innerText = controls.heatmap.value;
            outputs.stft.innerText = controls.stft.value;
            outputs.pca.innerText = controls.pca.value;
            outputs.padding.innerText = (parseInt(controls.padding.value, 10) / 100.0).toFixed(2);
        }

        let settingsTimer = null;
        function pushSettingsDebounced() {
            refreshLabels();
            if (settingsTimer) {
                clearTimeout(settingsTimer);
            }
            settingsTimer = setTimeout(() => {
                fetch('/api/settings', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify(readSettings())
                });
            }, 120);
        }

        Object.values(controls).forEach((control) => control.addEventListener('input', pushSettingsDebounced));
        refreshLabels();
        pushSettingsDebounced();

        document.getElementById('btn-pause').addEventListener('click', () => {
            paused = !paused;
            document.getElementById('btn-pause').innerText = paused ? 'Continuar' : 'Pausar';
        });

        document.getElementById('btn-clear').addEventListener('click', async () => {
            await fetch('/api/clear', { method: 'POST' });
        });

        ws.onmessage = (event) => {
            if (paused) {
                return;
            }

            const data = JSON.parse(event.data);

            document.getElementById('stat-count').innerText = data.count ?? 0;
            document.getElementById('stat-rssi').innerText = data.rssi !== null ? `${data.rssi} dBm` : '--';
            document.getElementById('stat-rate').innerText = data.rate !== null ? `${data.rate}` : '--';
            document.getElementById('stat-subcarriers').innerText = data.subcarriers !== null ? `${data.subcarriers}` : '--';
            document.getElementById('footer-status').innerText = data.footer_text || 'Atualizando...';

            if (data.amplitudes && data.amplitudes.length > 0) {
                Plotly.react('plot-heatmap', [{
                    z: transposeHeatmap(data.heatmap),
                    type: 'heatmap',
                    colorscale: 'Viridis',
                    zsmooth: false,
                    hovertemplate: 'Subportadora %{y}<br>Tempo %{x}<br>Amplitude %{z:.2f}<extra></extra>'
                }], layoutHeatmap, plotConfig);
            }

            if (data.stft && data.stft.z && data.stft.z.length > 0) {
                Plotly.react('plot-stft', [{
                    z: data.stft.z,
                    x: data.stft.x,
                    y: data.stft.y,
                    type: 'heatmap',
                    colorscale: 'Magma',
                    zsmooth: false,
                    hovertemplate: 'Freq %{y:.2f} Hz<br>Tempo %{x:.2f} s<br>Energia %{z:.2f} dB<extra></extra>'
                }], layoutStft, plotConfig);
            }

            if (data.phase_raw && data.phase_raw.length > 0) {
                Plotly.react('plot-phase', [
                    {
                        x: Array.from({ length: data.phase_raw.length }, (_, index) => index),
                        y: data.phase_raw,
                        type: 'scatter',
                        mode: 'lines',
                        name: 'Bruta',
                        line: { color: '#95a5a6', width: 1.5 }
                    },
                    {
                        x: Array.from({ length: data.phase_sanitized.length }, (_, index) => index),
                        y: data.phase_sanitized,
                        type: 'scatter',
                        mode: 'lines',
                        name: 'Sanitizada',
                        line: { color: '#ff4136', width: 2 }
                    }
                ], layoutWithPadding(layoutPhase, [...data.phase_raw, ...data.phase_sanitized], data.padding ?? 0.15), plotConfig);
            }

            if (data.pca && data.pca.packet_axis && data.pca.packet_axis.length > 0) {
                Plotly.react('plot-pca', [
                    { x: data.pca.packet_axis, y: data.pca.pc1, type: 'scatter', mode: 'lines', name: 'PC1', line: { color: '#2ecc71', width: 2 } },
                    { x: data.pca.packet_axis, y: data.pca.pc2, type: 'scatter', mode: 'lines', name: 'PC2', line: { color: '#3498db', width: 2 } }
                ], layoutWithPadding(layoutPca, [...data.pca.pc1, ...data.pca.pc2], data.padding ?? 0.15), plotConfig);
            }

            if (data.pdp && data.pdp.x && data.pdp.x.length > 0) {
                Plotly.react('plot-pdp', [{
                    x: data.pdp.x,
                    y: data.pdp.y,
                    type: 'bar',
                    marker: { color: '#f39c12' }
                }], layoutWithPadding(layoutPdp, data.pdp.y, data.padding ?? 0.15), plotConfig);
            }
        };

        ws.onclose = () => { console.log('WebSocket disconnected'); };
    </script>
</body>
</html>
"""


@app.get("/")
def get_dashboard():
    return HTMLResponse(content=HTML_DASHBOARD)


@app.websocket("/ws")
async def websocket_endpoint(websocket: WebSocket):
    await websocket.accept()
    try:
        while state.running:
            with state.lock:
                settings = _clamp_settings(state.settings)

            snapshot = _get_snapshot(settings)
            await websocket.send_json(snapshot)
            await asyncio.sleep(WEBSOCKET_INTERVAL_SECONDS)
    except WebSocketDisconnect:
        logger.info("Client disconnected")
    except Exception as e:
        logger.info(f"WS Error: {e}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="0.0.0.0", help="Host address")
    parser.add_argument("--port", type=int, default=8000, help="Web server port")
    args = parser.parse_args()

    # Executar o uvicorn (FastAPI) na thread principal
    uvicorn.run(app, host=args.host, port=args.port, loop="asyncio")
