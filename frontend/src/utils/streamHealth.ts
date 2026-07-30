export const STREAM_STALE_WARNING_MS = 3000;

export function formatStreamAge(ageMs: number) {
  if (ageMs <= 0) {
    return '0 ms';
  }

  if (ageMs < 1000) {
    return `${Math.round(ageMs)} ms`;
  }

  const seconds = ageMs / 1000;
  if (seconds < 60) {
    return `${seconds.toFixed(seconds < 10 ? 1 : 0)} s`;
  }

  const minutes = Math.floor(seconds / 60);
  const remainingSeconds = Math.round(seconds % 60);
  return `${minutes}m ${String(remainingSeconds).padStart(2, '0')}s`;
}