let bearerToken = '';

const READ_ONLY_METHODS = new Set(['GET', 'HEAD', 'OPTIONS']);

/** Keep the administrative credential in JavaScript memory for this app run. */
export function setApiBearerToken(value: string): void {
  bearerToken = value.trim();
}

/**
 * Fetch wrapper shared by the dashboard. It adds the session Bearer only to
 * mutations and never persists it in local/session storage or a URL.
 */
export function apiFetch(input: RequestInfo | URL, init: RequestInit = {}): Promise<Response> {
  const method = (init.method ?? 'GET').toUpperCase();
  if (READ_ONLY_METHODS.has(method) || !bearerToken) {
    return fetch(input, init);
  }

  const headers = new Headers(init.headers);
  if (!headers.has('Authorization')) {
    headers.set('Authorization', `Bearer ${bearerToken}`);
  }
  return fetch(input, { ...init, headers });
}
