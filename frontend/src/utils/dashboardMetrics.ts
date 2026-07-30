import type { CSIPacket, LocationEstimate } from '../types/csi';
import i18n from '../i18n/i18n';

export function isFiniteNumber(value: unknown): value is number {
  return typeof value === 'number' && Number.isFinite(value);
}

export function formatNumber(value: number | undefined, digits = 2) {
  return isFiniteNumber(value) ? value.toFixed(digits) : '--';
}

export function formatPercent(value: number | undefined) {
  return isFiniteNumber(value) ? `${(value * 100).toFixed(0)}%` : '--';
}

export function matrixShape(matrix: number[][] | undefined) {
  if (!matrix || matrix.length === 0) return '0 x 0';
  const width = matrix.reduce((max, row) => Math.max(max, row.length), 0);
  return `${matrix.length} x ${width}`;
}

export function movementLabel(level: string | undefined) {
  switch (level) {
    case 'static':
      return i18n.t('common.movementLevels.static', 'estático');
    case 'slight_movement':
      return i18n.t('common.movementLevels.slight', 'movimento leve');
    case 'walking':
      return i18n.t('common.movementLevels.walking', 'caminhada');
    case 'running':
      return i18n.t('common.movementLevels.running', 'corrida');
    default:
      return '--';
  }
}

export function formatLocationMethod(location?: LocationEstimate, emptyLabel?: string) {
  const defaultEmpty = emptyLabel ?? i18n.t('common.locationMethods.noData', 'sem dados');
  const method = location?.method;
  if (!method || method === 'sem_dados') return defaultEmpty;
  if (method.includes('fusion')) return i18n.t('common.locationMethods.fusion', 'fusão C5+WROOM');
  if (method.includes('esp32c5')) return i18n.t('common.locationMethods.fallbackC5', 'fallback C5');
  if (method.includes('wroom')) return i18n.t('common.locationMethods.fallbackWroom', 'fallback WROOM');
  return method.replaceAll('_', ' ');
}

export function formatLocationAccuracy(location?: LocationEstimate) {
  if (!location) return '--';
  if (isFiniteNumber(location.accuracy_dm)) return `${location.accuracy_dm.toFixed(1)} dm`;
  return isFiniteNumber(location.accuracy_m) ? `${location.accuracy_m.toFixed(2)} m` : '--';
}

export function vitalsFromPacket(data: CSIPacket | null | undefined) {
  const vitals = data?.detection?.vitals;
  const vitalsMl = data?.detection?.occupancy?.vitals_ml;

  return {
    breathingRate: isFiniteNumber(vitals?.breathing_rate) ? vitals.breathing_rate : undefined,
    heartRateBpm: isFiniteNumber(vitals?.heart_rate_bpm) ? vitals.heart_rate_bpm : undefined,
    confidence: isFiniteNumber(vitals?.confidence) ? vitals.confidence : undefined,
    breathingRateMl: isFiniteNumber(vitalsMl?.breathing_rate) ? vitalsMl.breathing_rate : undefined,
    heartRateBpmMl: isFiniteNumber(vitalsMl?.heart_rate_bpm) ? vitalsMl.heart_rate_bpm : undefined,
    confidenceMl: isFiniteNumber(vitalsMl?.confidence) ? vitalsMl.confidence : undefined,
  };
}

export function formatRate(value: number | undefined, digits: number, unit: string) {
  return isFiniteNumber(value) ? `${value.toFixed(digits)} ${unit}` : '--';
}
