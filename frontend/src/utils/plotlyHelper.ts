import Plotly from 'plotly.js-dist-min';
import i18n from '../i18n/i18n';
import type { CSIPacket } from '../types/csi';
import { isFiniteNumber, vitalsFromPacket } from './dashboardMetrics';

const plotConfig = { responsive: true, displayModeBar: false };
export const PACKET_RATE_PLOT_ID = 'plot-packet-rate';
const VITALS_HISTORY_LIMIT = 180;

const colors = {
  bg: 'rgba(0,0,0,0)',
  grid: 'rgba(99, 102, 241, 0.08)',
  text: '#94a3b8',
  indigo: '#6366f1',
  cyan: '#06b6d4',
  emerald: '#10b981',
  amber: '#f59e0b',
  rose: '#f43f5e',
  muted: '#64748b',
};

type VitalMetric = 'breathing' | 'heart';

interface VitalHistory {
  x: number[];
  breathing: Array<number | null>;
  heart: Array<number | null>;
  confidence: Array<number | null>;
  breathingMl: Array<number | null>;
  heartMl: Array<number | null>;
  confidenceMl: Array<number | null>;
}

const vitalHistory: VitalHistory = {
  x: [],
  breathing: [],
  heart: [],
  confidence: [],
  breathingMl: [],
  heartMl: [],
  confidenceMl: [],
};

const seenVitalPackets = new WeakSet<CSIPacket>();
let lastVitalCount: number | null = null;
let fallbackVitalIndex = 0;

function resetVitalHistory() {
  vitalHistory.x = [];
  vitalHistory.breathing = [];
  vitalHistory.heart = [];
  vitalHistory.confidence = [];
  vitalHistory.breathingMl = [];
  vitalHistory.heartMl = [];
  vitalHistory.confidenceMl = [];
  lastVitalCount = null;
  fallbackVitalIndex = 0;
}

function appendVitalSample(data: CSIPacket) {
  if (seenVitalPackets.has(data)) {
    return vitalHistory;
  }

  if (isFiniteNumber(data.count)) {
    if (lastVitalCount !== null && data.count < lastVitalCount) {
      resetVitalHistory();
    }

    if (lastVitalCount === data.count) {
      seenVitalPackets.add(data);
      return vitalHistory;
    }

    lastVitalCount = data.count;
  }

  const vitals = vitalsFromPacket(data);
  const x = isFiniteNumber(data.count) ? data.count : ++fallbackVitalIndex;

  vitalHistory.x.push(x);
  vitalHistory.breathing.push(vitals.breathingRate ?? null);
  vitalHistory.heart.push(vitals.heartRateBpm ?? null);
  vitalHistory.confidence.push(vitals.confidence ?? null);
  vitalHistory.breathingMl.push(vitals.breathingRateMl ?? null);
  vitalHistory.heartMl.push(vitals.heartRateBpmMl ?? null);
  vitalHistory.confidenceMl.push(vitals.confidenceMl ?? null);

  if (vitalHistory.x.length > VITALS_HISTORY_LIMIT) {
    vitalHistory.x.shift();
    vitalHistory.breathing.shift();
    vitalHistory.heart.shift();
    vitalHistory.confidence.shift();
    vitalHistory.breathingMl.shift();
    vitalHistory.heartMl.shift();
    vitalHistory.confidenceMl.shift();
  }

  seenVitalPackets.add(data);
  return vitalHistory;
}

function baseLayout(xTitle: string, yTitle: string): Partial<Plotly.Layout> {
  return {
    paper_bgcolor: colors.bg,
    plot_bgcolor: colors.bg,
    font: { color: colors.text, family: 'Inter, sans-serif', size: 11 },
    margin: { t: 8, b: 36, l: 48, r: 16 },
    xaxis: {
      gridcolor: colors.grid,
      zeroline: false,
      title: { text: xTitle, font: { size: 10, color: colors.muted } },
    },
    yaxis: {
      gridcolor: colors.grid,
      zeroline: false,
      title: { text: yTitle, font: { size: 10, color: colors.muted } },
    },
    uirevision: 'aethersense',
  };
}

export const layouts = {
  get heatmap() {
    return {
      ...baseLayout(i18n.t('plotly.timePackets', 'Tempo (pacotes)'), i18n.t('plotly.subcarrier', 'Subportadora')),
      yaxis: {
        ...baseLayout('', '').yaxis,
        autorange: 'reversed' as const,
        title: { text: i18n.t('plotly.subcarrier', 'Subportadora'), font: { size: 10, color: colors.muted } },
      },
    };
  },
  get stft() { return baseLayout(i18n.t('plotly.timeSec', 'Tempo (s)'), i18n.t('plotly.freqHz', 'Frequência (Hz)')); },
  get phase() { return baseLayout(i18n.t('plotly.subcarrier', 'Subportadora'), i18n.t('plotly.phaseRad', 'Fase (rad)')); },
  get pca() { return baseLayout(i18n.t('plotly.packet', 'Pacote'), i18n.t('plotly.pcaScore', 'Score PCA')); },
  get pdp() { return baseLayout(i18n.t('plotly.delayUs', 'Atraso (μs)'), i18n.t('plotly.powerDb', 'Potência (dB)')); },
  get breathing() {
    return {
      ...baseLayout(i18n.t('plotly.packet', 'Pacote'), i18n.t('plotly.irpm', 'irpm')),
      showlegend: true,
      legend: { orientation: 'h' as const, y: 1.15, x: 0, font: { size: 10, color: colors.text } },
    };
  },
  get heart() {
    return {
      ...baseLayout(i18n.t('plotly.packet', 'Pacote'), i18n.t('plotly.bpm', 'BPM')),
      showlegend: true,
      legend: { orientation: 'h' as const, y: 1.15, x: 0, font: { size: 10, color: colors.text } },
    };
  },
  get fftSpectrum() {
    return {
      paper_bgcolor: colors.bg,
      plot_bgcolor: colors.bg,
      font: { color: colors.text, family: 'Inter, sans-serif', size: 11 },
      margin: { t: 16, b: 16, l: 16, r: 16 },
      showlegend: true,
      legend: { orientation: 'h' as const, y: 1.02, x: 0, font: { size: 10, color: colors.text } },
      scene: {
        xaxis: { title: { text: i18n.t('plotly.freqHz', 'Frequência (Hz)') }, color: colors.muted, gridcolor: colors.grid, zerolinecolor: colors.grid, showbackground: false },
        yaxis: {
          title: { text: i18n.t('plotly.physioDecomp', 'Decomposição Fisiológica') },
          color: colors.muted,
          gridcolor: colors.grid,
          zerolinecolor: colors.grid,
          showbackground: false,
          tickmode: 'array' as const,
          tickvals: [0, 1, 2, 3],
          ticktext: [i18n.t('plotly.rawSpectrum', 'Espectro Bruto'), i18n.t('plotly.breathing', 'Respiração'), i18n.t('plotly.harmonics', 'Harmônicos'), i18n.t('plotly.cardiac', 'Cardíaco')],
        },
        zaxis: { title: { text: i18n.t('plotly.magnitude', 'Magnitude') }, color: colors.muted, gridcolor: colors.grid, zerolinecolor: colors.grid, showbackground: false },
        camera: { eye: { x: -1.6, y: -1.8, z: 0.9 }, center: { x: 0, y: 0, z: -0.1 } },
        aspectratio: { x: 1.6, y: 1.1, z: 0.8 },
      } as any,
      uirevision: 'aethersense-3d',
    };
  },
};

function paddingRange(values: number[], ratio: number): [number, number] | undefined {
  if (!values || values.length === 0) return undefined;
  const min = Math.min(...values);
  const max = Math.max(...values);
  const span = Math.max(1e-6, max - min);
  return [min - span * ratio, max + span * ratio];
}

export function withPadding(layout: Partial<Plotly.Layout>, values: number[], ratio: number): Partial<Plotly.Layout> {
  const l = JSON.parse(JSON.stringify(layout));
  const range = paddingRange(values, ratio);
  if (range) l.yaxis.range = range;
  return l;
}

export function transpose(matrix: number[][]): number[][] {
  if (!matrix || matrix.length === 0) return [];
  const cols = matrix.reduce((m, row) => Math.max(m, row.length), 0);
  return Array.from({ length: cols }, (_, c) =>
    matrix.map((row) => (c < row.length ? row[c] : null))
  ) as number[][];
}

export function initHeatmap(plotId: string) {
  Plotly.newPlot(plotId, [{ z: [], type: 'heatmap', colorscale: 'Viridis', zsmooth: false }], layouts.heatmap, plotConfig);
}

export function initSTFT(plotId: string) {
  Plotly.newPlot(plotId, [{ z: [], type: 'heatmap', colorscale: 'Magma', zsmooth: false }], layouts.stft, plotConfig);
}

export function initPhase(plotId: string) {
  Plotly.newPlot(plotId, [
    { x: [], y: [], type: 'scatter', mode: 'lines', name: 'Bruta', line: { color: colors.muted, width: 1.5 } },
    { x: [], y: [], type: 'scatter', mode: 'lines', name: 'Sanitizada', line: { color: colors.rose, width: 2 } },
  ], layouts.phase, plotConfig);
}

export function initPCA(plotId: string) {
  Plotly.newPlot(plotId, [
    { x: [], y: [], type: 'scatter', mode: 'lines', name: 'PC1', line: { color: colors.emerald, width: 2 } },
    { x: [], y: [], type: 'scatter', mode: 'lines', name: 'PC2', line: { color: colors.cyan, width: 2 } },
  ], layouts.pca, plotConfig);
}

export function initPDP(plotId: string) {
  Plotly.newPlot(plotId, [{ x: [], y: [], type: 'bar', marker: { color: colors.amber } }], layouts.pdp, plotConfig);
}

export function initFftSpectrum(plotId: string) {
  // Config 3D dedicado: scrollZoom habilitado para zoom com roda do mouse na cena 3D
  const plotConfig3d = { responsive: true, displayModeBar: false, scrollZoom: true };
  Plotly.newPlot(plotId, [
    { x: [], y: [], z: [], type: 'scatter3d' as any, mode: 'lines', name: 'Espectro CSI Bruto', line: { color: colors.cyan, width: 4 } },
    { x: [], y: [], z: [], type: 'scatter3d' as any, mode: 'lines', name: 'Banda Respiração (0.1-0.5Hz)', line: { color: colors.emerald, width: 5 } },
    { x: [], y: [], z: [], type: 'scatter3d' as any, mode: 'lines+markers', name: 'Pico Respiração', marker: { color: colors.emerald, size: 8, symbol: 'diamond' }, line: { color: colors.emerald, width: 6 } },
    { x: [], y: [], z: [], type: 'scatter3d' as any, mode: 'lines+markers', name: 'Harmônicos Rejeitados', marker: { color: colors.amber, size: 6, symbol: 'x' }, line: { color: colors.amber, width: 3 } },
    { x: [], y: [], z: [], type: 'scatter3d' as any, mode: 'lines', name: 'Banda Cardíaca (0.8-2.0Hz)', line: { color: colors.rose, width: 5 } },
    { x: [], y: [], z: [], type: 'scatter3d' as any, mode: 'lines+markers', name: 'Pico Cardíaco', marker: { color: colors.rose, size: 8, symbol: 'circle' }, line: { color: colors.rose, width: 6 } },
  ], layouts.fftSpectrum, plotConfig3d);
}

export function updateFftSpectrum(data: CSIPacket, plotId: string) {
  const diag = data?.detection?.fft_diagnostics;
  if (!diag || !diag.spectrum_freqs || diag.spectrum_freqs.length === 0) return;

  // Guard: espera initFftSpectrum ter criado o plot
  const el = document.getElementById(plotId);
  if (!el || !(el as any).data || (el as any).data.length < 6) return;

  const freqs = diag.spectrum_freqs;
  const mags = diag.spectrum_magnitudes;
  const maxMag = Math.max(...mags, 1e-4);
  const maxFreq = Math.min(3, freqs[freqs.length - 1] || 3);

  // 0: Espectro Global Bruto (y = 0) - Trilha completa
  const rawX = freqs;
  const rawY = freqs.map(() => 0);
  const rawZ = mags;

  // 1: Banda Respiratória (y = 1) - Trilha completa (z=0 fora de 0.1 a 0.5 Hz)
  const breathBandX = freqs;
  const breathBandY = freqs.map(() => 1);
  const breathBandZ = freqs.map((f, i) => (f >= 0.1 && f <= 0.5 ? (mags[i] ?? 0) : 0));

  // 2: Pico Respiratório Stem (y = 1)
  const breathPeakX: number[] = [];
  const breathPeakY: number[] = [];
  const breathPeakZ: number[] = [];
  let breathLabel = i18n.t('plotly.breathPeakEmpty', 'Pico Resp. (—)');
  if (diag.breathing_peak_hz != null && diag.breathing_peak_hz > 0) {
    const idx = freqs.reduce((best, f, i) => Math.abs(f - diag.breathing_peak_hz!) < Math.abs(freqs[best] - diag.breathing_peak_hz!) ? i : best, 0);
    const peakMag = mags[idx] ?? 0;
    breathPeakX.push(diag.breathing_peak_hz, diag.breathing_peak_hz);
    breathPeakY.push(1, 1);
    breathPeakZ.push(0, peakMag * 1.05);
    breathLabel = i18n.t('plotly.breathPeakVal', 'Pico Resp. ({{val}} irpm)', { val: (diag.breathing_peak_hz * 60).toFixed(0) });
  }

  // 3: Harmônicos Rejeitados (y = 2) - Linha base + Stems verticais
  const harmX: (number | null)[] = [0, maxFreq, null];
  const harmY: (number | null)[] = [2, 2, null];
  const harmZ: (number | null)[] = [0, 0, null];
  let harmCount = 0;

  // Se o backend não enviou harmônicos (respiração abaixo do limiar), calcular localmente
  let harmonicsList: number[] = diag.rejected_harmonics && diag.rejected_harmonics.length > 0
    ? [...diag.rejected_harmonics]
    : [];
  if (harmonicsList.length === 0) {
    let baseFreq: number | null | undefined = diag.breathing_peak_hz;
    if (baseFreq == null || baseFreq <= 0) {
      // Fallback: pico mais forte na banda respiratória
      let bestMag = -1;
      freqs.forEach((f, i) => {
        if (f >= 0.1 && f <= 0.5 && mags[i] > bestMag) {
          bestMag = mags[i];
          baseFreq = f;
        }
      });
    }
    if (baseFreq != null && baseFreq > 0) {
      for (let k = 2; k <= 5; k++) {
        const h = baseFreq * k;
        if (h <= maxFreq) harmonicsList.push(h);
      }
    }
  }

  harmonicsList.forEach(h => {
    if (h > 0 && h <= maxFreq) {
      const idx = freqs.reduce((best, f, i) => Math.abs(f - h) < Math.abs(freqs[best] - h) ? i : best, 0);
      const mag = mags[idx] ?? 0;
      const stemZ = Math.max(mag * 1.15, maxMag * 0.35);
      harmX.push(h, h, null);
      harmY.push(2, 2, null);
      harmZ.push(0, stemZ, null);
      harmCount++;
    }
  });

  // 4: Banda Cardíaca (y = 3) - Trilha completa (z=0 fora de 0.8 a 2.0 Hz)
  const heartBandX = freqs;
  const heartBandY = freqs.map(() => 3);
  const heartBandZ = freqs.map((f, i) => (f >= 0.8 && f <= 2.0 ? (mags[i] ?? 0) : 0));

  // 5: Pico Cardíaco Stem (y = 3)
  const heartPeakX: number[] = [];
  const heartPeakY: number[] = [];
  const heartPeakZ: number[] = [];
  let heartLabel = i18n.t('plotly.heartPeakEmpty', 'Pico Cardíaco (—)');
  if (diag.heart_peak_hz != null && diag.heart_peak_hz > 0) {
    const idx = freqs.reduce((best, f, i) => Math.abs(f - diag.heart_peak_hz!) < Math.abs(freqs[best] - diag.heart_peak_hz!) ? i : best, 0);
    const peakMag = mags[idx] ?? 0;
    heartPeakX.push(diag.heart_peak_hz, diag.heart_peak_hz);
    heartPeakY.push(3, 3);
    heartPeakZ.push(0, peakMag * 1.05);
    heartLabel = i18n.t('plotly.heartPeakVal', 'Pico Cardíaco ({{val}} bpm)', { val: (diag.heart_peak_hz * 60).toFixed(0) });
  }

  // ── Atualizar APENAS os dados dos traces via Plotly.restyle ──
  // Isso NÃO toca no layout, na câmera ou na cena WebGL,
  // permitindo rotação, zoom e inclinação livres durante o streaming.
  const traceIndices = [0, 1, 2, 3, 4, 5];
  Plotly.restyle(plotId, {
    x: [rawX, breathBandX, breathPeakX, harmX, heartBandX, heartPeakX],
    y: [rawY, breathBandY, breathPeakY, harmY, heartBandY, heartPeakY],
    z: [rawZ, breathBandZ, breathPeakZ, harmZ, heartBandZ, heartPeakZ],
    name: [
      i18n.t('plotly.rawCsiSpectrum', 'Espectro CSI Bruto'),
      i18n.t('plotly.breathBand', 'Banda Respiração (0.1-0.5Hz)'),
      breathLabel,
      i18n.t('plotly.rejectedHarmonics', 'Harmônicos Rejeitados ({{count}})', { count: harmCount }),
      i18n.t('plotly.cardiacBand', 'Banda Cardíaca (0.8-2.0Hz)'),
      heartLabel,
    ],
  } as any, traceIndices);
}

export function initBreathingRate(plotId: string) {
  Plotly.newPlot(plotId, vitalTraces('breathing'), layouts.breathing, plotConfig);
}

export function initHeartRate(plotId: string) {
  Plotly.newPlot(plotId, vitalTraces('heart'), layouts.heart, plotConfig);
}

function vitalTraces(metric: VitalMetric): Plotly.Data[] {
  const isBreathing = metric === 'breathing';
  const valuesFft = isBreathing ? vitalHistory.breathing : vitalHistory.heart;
  const valuesMl = isBreathing ? vitalHistory.breathingMl : vitalHistory.heartMl;
  const confFft = vitalHistory.confidence;
  const confMl = vitalHistory.confidenceMl;
  const units = isBreathing ? 'irpm' : 'bpm';

  const traceMl: Plotly.Data = {
    x: [...vitalHistory.x],
    y: [...valuesMl] as Plotly.Datum[],
    customdata: [...confMl] as Plotly.Datum[],
    type: 'scatter',
    mode: 'lines+markers',
    name: i18n.t('plotly.aiNeural', 'IA Neural (ONNX)'),
    connectgaps: true,
    line: {
      color: isBreathing ? colors.emerald : colors.indigo,
      width: 2.5,
      shape: 'spline',
    },
    marker: {
      color: isBreathing ? colors.emerald : colors.indigo,
      size: 6,
    },
    hovertemplate: `Pacote %{x}<br>${i18n.t('plotly.aiNeural', 'IA Neural')}: %{y:.1f} ${units}<br>${i18n.t('plotly.confWord', 'Confiança')} %{customdata:.0%}<extra></extra>`,
  };

  const traceFft: Plotly.Data = {
    x: [...vitalHistory.x],
    y: [...valuesFft] as Plotly.Datum[],
    customdata: [...confFft] as Plotly.Datum[],
    type: 'scatter',
    mode: 'lines+markers',
    name: i18n.t('plotly.fftHeuristic', 'Heurística FFT'),
    connectgaps: false,
    line: {
      color: isBreathing ? colors.cyan : colors.rose,
      width: 2,
      dash: 'dot',
      shape: 'spline',
    },
    marker: {
      color: isBreathing ? colors.cyan : colors.rose,
      size: 5,
    },
    hovertemplate: `Pacote %{x}<br>${i18n.t('plotly.fftHeuristic', 'Heurística FFT')}: %{y:.1f} ${units}<br>${i18n.t('plotly.confWord', 'Confiança')} %{customdata:.0%}<extra></extra>`,
  };

  return [traceMl, traceFft];
}

function finiteVitalValues(values: Array<number | null>) {
  return values.filter(isFiniteNumber);
}

function updateVitalMetric(data: CSIPacket, plotId: string, metric: VitalMetric) {
  const history = appendVitalSample(data);
  const valuesFft = metric === 'breathing' ? history.breathing : history.heart;
  const valuesMl = metric === 'breathing' ? history.breathingMl : history.heartMl;
  const allValues = [...valuesFft, ...valuesMl];
  const layout = metric === 'breathing' ? layouts.breathing : layouts.heart;

  Plotly.react(
    plotId,
    vitalTraces(metric),
    withPadding(layout, finiteVitalValues(allValues), data.padding ?? 0.15),
    plotConfig
  );
}

export function updateHeatmap(data: CSIPacket, plotId: string) {
  const hasHeatmap = Boolean(data.heatmap && data.heatmap.length > 0);

  Plotly.react(plotId, [
    hasHeatmap
      ? {
          z: transpose(data.heatmap || []),
          type: 'heatmap',
          colorscale: 'Viridis',
          zsmooth: false,
          hovertemplate: i18n.t('plotly.hoverHeatmap', 'Subportadora %{y}<br>Tempo %{x}<br>Amplitude %{z:.2f}<extra></extra>'),
        }
      : { z: [], type: 'heatmap', colorscale: 'Viridis', zsmooth: false },
  ], layouts.heatmap, plotConfig);
}

export function updateSTFT(data: CSIPacket, plotId: string) {
  const hasStft = Boolean(data.stft && data.stft.z && data.stft.z.length > 0);

  Plotly.react(plotId, [
    hasStft
      ? {
          z: data.stft!.z,
          x: data.stft!.x,
          y: data.stft!.y,
          type: 'heatmap',
          colorscale: 'Magma',
          zsmooth: false,
          hovertemplate: i18n.t('plotly.hoverStft', 'Freq %{y:.2f} Hz<br>Tempo %{x:.2f} s<br>Energia %{z:.2f} dB<extra></extra>'),
        }
      : { z: [], type: 'heatmap', colorscale: 'Magma', zsmooth: false },
  ], layouts.stft, plotConfig);
}

export function updatePhase(data: CSIPacket, plotId: string) {
  const pad = data.padding ?? 0.15;
  const hasPhase = Boolean(data.phase_raw && data.phase_raw.length > 0 && data.phase_sanitized && data.phase_sanitized.length > 0);

  Plotly.react(plotId, hasPhase ? [
    {
      x: Array.from({ length: data.phase_raw?.length ?? 0 }, (_, i) => i),
      y: data.phase_raw ?? [],
      type: 'scatter',
      mode: 'lines',
      name: i18n.t('plotly.raw', 'Bruta'),
      line: { color: colors.muted, width: 1.5 },
    },
    {
      x: Array.from({ length: data.phase_sanitized?.length ?? 0 }, (_, i) => i),
      y: data.phase_sanitized ?? [],
      type: 'scatter',
      mode: 'lines',
      name: i18n.t('plotly.sanitized', 'Sanitizada'),
      line: { color: colors.rose, width: 2 },
    },
  ] : [
    { x: [], y: [], type: 'scatter', mode: 'lines', name: i18n.t('plotly.raw', 'Bruta'), line: { color: colors.muted, width: 1.5 } },
    { x: [], y: [], type: 'scatter', mode: 'lines', name: i18n.t('plotly.sanitized', 'Sanitizada'), line: { color: colors.rose, width: 2 } },
  ], hasPhase ? withPadding(layouts.phase, [...(data.phase_raw ?? []), ...(data.phase_sanitized ?? [])], pad) : layouts.phase, plotConfig);
}

export function updatePCA(data: CSIPacket, plotId: string) {
  const pad = data.padding ?? 0.15;
  const hasPca = Boolean(data.pca && data.pca.packet_axis && data.pca.packet_axis.length > 0 && data.pca.pc1 && data.pca.pc1.length > 0 && data.pca.pc2 && data.pca.pc2.length > 0);

  Plotly.react(plotId, hasPca ? [
    { x: data.pca!.packet_axis, y: data.pca!.pc1, type: 'scatter', mode: 'lines', name: 'PC1', line: { color: colors.emerald, width: 2 } },
    { x: data.pca!.packet_axis, y: data.pca!.pc2, type: 'scatter', mode: 'lines', name: 'PC2', line: { color: colors.cyan, width: 2 } },
  ] : [
    { x: [], y: [], type: 'scatter', mode: 'lines', name: 'PC1', line: { color: colors.emerald, width: 2 } },
    { x: [], y: [], type: 'scatter', mode: 'lines', name: 'PC2', line: { color: colors.cyan, width: 2 } },
  ], hasPca ? withPadding(layouts.pca, [...(data.pca?.pc1 ?? []), ...(data.pca?.pc2 ?? [])], pad) : layouts.pca, plotConfig);
}

export function updatePDP(data: CSIPacket, plotId: string) {
  const pad = data.padding ?? 0.15;
  const hasPdp = Boolean(data.pdp && data.pdp.x && data.pdp.x.length > 0 && data.pdp.y && data.pdp.y.length > 0);

  Plotly.react(plotId, [
    hasPdp
      ? {
          x: data.pdp!.x,
          y: data.pdp!.y,
          type: 'bar',
          marker: { color: colors.amber },
        }
      : { x: [], y: [], type: 'bar', marker: { color: colors.amber } },
  ], hasPdp ? withPadding(layouts.pdp, data.pdp!.y, pad) : layouts.pdp, plotConfig);
}

export function updateBreathingRate(data: CSIPacket, plotId: string) {
  updateVitalMetric(data, plotId, 'breathing');
}

export function updateHeartRate(data: CSIPacket, plotId: string) {
  updateVitalMetric(data, plotId, 'heart');
}

function exportFilename(filename: string) {
  return filename
    .normalize('NFD')
    .replace(/[\u0300-\u036f]/g, '')
    .replace(/[^a-zA-Z0-9_-]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .toLowerCase() || i18n.t('plotly.chartFilename', 'grafico');
}

export async function exportPlot(plotId: string, filename = plotId) {
  const plotElement = document.getElementById(plotId);
  if (!plotElement) {
    return false;
  }

  await Plotly.downloadImage(plotElement, {
    format: 'png',
    filename: exportFilename(filename),
    width: Math.max(900, plotElement.clientWidth || 900),
    height: Math.max(520, plotElement.clientHeight || 520),
  });

  return true;
}

export async function exportPlots(plotIds: string[], filenamePrefix = 'aethersense') {
  let exported = 0;

  for (const plotId of plotIds) {
    const ok = await exportPlot(plotId, `${filenamePrefix}-${plotId.replace(/^plot-/, '')}`);
    if (ok) exported += 1;
  }

  return exported;
}
