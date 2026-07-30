import { apiFetch } from './apiClient';

export const EMPTY_ROOM_CAPTURE_SECONDS = 27;

type ClearResponse = {
  ok?: boolean;
  message?: string;
};

/**
 * Inicia uma sessão nova e espera amostras suficientes até mesmo no limite
 * mínimo aceito pelo backend (4 Hz × 25 s, com 2 s de margem).
 */
export async function prepareEmptyRoomCalibration(
  apiBaseUrl: string,
  onWaiting?: (seconds: number) => void,
): Promise<void> {
  const response = await apiFetch(`${apiBaseUrl}/api/clear`, { method: 'POST' });
  const result = (await response.json()) as ClearResponse;
  if (!response.ok || !result.ok) {
    throw new Error(result.message || `HTTP ${response.status} ao iniciar calibração`);
  }

  onWaiting?.(EMPTY_ROOM_CAPTURE_SECONDS);
  await new Promise<void>((resolve) => {
    window.setTimeout(resolve, EMPTY_ROOM_CAPTURE_SECONDS * 1000);
  });
}
