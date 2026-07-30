import type {
  ShowerBoxSettings,
  ShowerBoxSettingsLimits,
  ShowerBoxSettingsResponse,
} from '../types/csi';
import { apiFetch } from './apiClient';

export const BALANCED_SHOWER_BOX_SETTINGS: ShowerBoxSettings = {
  profile: 'balanced',
  control_mode: 'automatic',
  entry_confirm_seconds: 2.5,
  entry_decay_seconds: 1.5,
  exit_hold_seconds: 60,
  recovery_confirm_seconds: 2,
  sensitivity: 4,
  presence_assist: true,
};

export const DEFAULT_SHOWER_BOX_LIMITS = {
  confirm: [2, 5],
  decay: [0.5, 5],
  hold: [60, 300],
  recovery: [1, 5],
  sensitivity: [1, 5],
} as const satisfies Required<
  Pick<
    ShowerBoxSettingsLimits,
    'confirm' | 'decay' | 'hold' | 'recovery' | 'sensitivity'
  >
>;

const endpoint = (apiBaseUrl: string) => `${apiBaseUrl}/api/shower-box/settings`;

const responseMessage = (value: unknown): string | null => {
  if (!value || typeof value !== 'object' || !('message' in value)) return null;
  const message = value.message;
  return typeof message === 'string' ? message : null;
};

async function parseSettingsResponse(response: Response): Promise<ShowerBoxSettingsResponse> {
  let body: unknown;
  try {
    body = await response.json();
  } catch {
    throw new Error(`Resposta inválida do servidor (HTTP ${response.status})`);
  }

  if (!response.ok) {
    throw new Error(responseMessage(body) ?? `Falha ao atualizar (HTTP ${response.status})`);
  }

  if (
    !body
    || typeof body !== 'object'
    || !('ok' in body)
    || body.ok !== true
    || !('settings' in body)
    || !('limits' in body)
  ) {
    throw new Error('O servidor devolveu configurações incompletas');
  }

  return body as ShowerBoxSettingsResponse;
}

export async function fetchShowerBoxSettings(
  apiBaseUrl: string,
): Promise<ShowerBoxSettingsResponse> {
  const response = await apiFetch(endpoint(apiBaseUrl), {
    headers: { Accept: 'application/json' },
  });
  return parseSettingsResponse(response);
}

export async function updateShowerBoxSettings(
  apiBaseUrl: string,
  patch: Partial<ShowerBoxSettings>,
): Promise<ShowerBoxSettingsResponse> {
  const response = await apiFetch(endpoint(apiBaseUrl), {
    method: 'POST',
    headers: {
      Accept: 'application/json',
      'Content-Type': 'application/json',
    },
    body: JSON.stringify(patch),
  });
  return parseSettingsResponse(response);
}
