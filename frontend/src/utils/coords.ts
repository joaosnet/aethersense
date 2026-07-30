// Conversão e formatação de coordenadas para o dashboard (lat/long, UTM, polar).
import i18n from '../i18n/i18n';

export type CoordUnit = 'latlng' | 'utm' | 'polar';

export interface LatLng {
  lat: number;
  lng: number;
}

const RAD = Math.PI / 180;

/** Letra da banda latitudinal UTM (C..X, sem I/O). */
function utmBand(lat: number): string {
  if (lat < -80 || lat > 84) return '';
  const bands = 'CDEFGHJKLMNPQRSTUVWX';
  const idx = Math.floor((lat + 80) / 8);
  return bands[Math.min(bands.length - 1, Math.max(0, idx))];
}

/** WGS84 → UTM (Transverse Mercator). Retorna zona, banda, E/N em metros. */
export function latLngToUTM(lat: number, lng: number) {
  const a = 6378137.0;
  const f = 1 / 298.257223563;
  const e2 = f * (2 - f);
  const ep2 = e2 / (1 - e2);
  const k0 = 0.9996;

  const zone = Math.floor((lng + 180) / 6) + 1;
  const lng0 = (zone - 1) * 6 - 180 + 3;

  const phi = lat * RAD;
  const lam = lng * RAD;
  const lam0 = lng0 * RAD;

  const sinPhi = Math.sin(phi);
  const cosPhi = Math.cos(phi);
  const tanPhi = Math.tan(phi);

  const N = a / Math.sqrt(1 - e2 * sinPhi * sinPhi);
  const T = tanPhi * tanPhi;
  const C = ep2 * cosPhi * cosPhi;
  const A = cosPhi * (lam - lam0);

  const M =
    a *
    ((1 - e2 / 4 - (3 * e2 * e2) / 64 - (5 * e2 * e2 * e2) / 256) * phi -
      ((3 * e2) / 8 + (3 * e2 * e2) / 32 + (45 * e2 * e2 * e2) / 1024) * Math.sin(2 * phi) +
      ((15 * e2 * e2) / 256 + (45 * e2 * e2 * e2) / 1024) * Math.sin(4 * phi) -
      ((35 * e2 * e2 * e2) / 3072) * Math.sin(6 * phi));

  const easting =
    k0 * N * (A + ((1 - T + C) * A ** 3) / 6 + ((5 - 18 * T + T * T + 72 * C - 58 * ep2) * A ** 5) / 120) +
    500000;

  let northing =
    k0 *
    (M +
      N * tanPhi * ((A * A) / 2 + ((5 - T + 9 * C + 4 * C * C) * A ** 4) / 24 +
        ((61 - 58 * T + T * T + 600 * C - 330 * ep2) * A ** 6) / 720));

  if (lat < 0) northing += 10000000;

  return {
    zone,
    band: utmBand(lat),
    hemisphere: lat >= 0 ? 'N' : 'S',
    easting,
    northing,
  };
}

/** Distância (m) e azimute (graus, 0=N horário) de um ponto relativo a `ref`. */
export function latLngToPolar(lat: number, lng: number, refLat: number, refLng: number) {
  const R = 6371000;
  const dLat = (lat - refLat) * RAD;
  const dLng = (lng - refLng) * RAD;
  const a =
    Math.sin(dLat / 2) ** 2 +
    Math.cos(refLat * RAD) * Math.cos(lat * RAD) * Math.sin(dLng / 2) ** 2;
  const distance = 2 * R * Math.asin(Math.min(1, Math.sqrt(a)));

  const y = Math.sin(dLng) * Math.cos(lat * RAD);
  const x =
    Math.cos(refLat * RAD) * Math.sin(lat * RAD) -
    Math.sin(refLat * RAD) * Math.cos(lat * RAD) * Math.cos(dLng);
  const bearing = ((Math.atan2(y, x) / RAD) + 360) % 360;

  return { distance, bearing };
}

/** Formata uma coordenada na unidade escolhida. `ref` é a origem do modo polar. */
export function formatCoord(lat: number, lng: number, unit: CoordUnit, ref?: LatLng | null): string {
  if (!Number.isFinite(lat) || !Number.isFinite(lng)) return '—';
  if (unit === 'utm') {
    const u = latLngToUTM(lat, lng);
    return `UTM ${u.zone}${u.band} ${u.easting.toFixed(1)}m E ${u.northing.toFixed(1)}m N`;
  }
  if (unit === 'polar') {
    if (!ref) return i18n.t('coords.setOrigin', 'defina a origem (1º receptor)');
    const p = latLngToPolar(lat, lng, ref.lat, ref.lng);
    return `${p.distance.toFixed(2)} m @ ${p.bearing.toFixed(0)}° (${i18n.t('coords.from', 'de')} ${ref.lat.toFixed(5)},${ref.lng.toFixed(5)})`;
  }
  return `${lat.toFixed(6)}, ${lng.toFixed(6)}`;
}

export const COORD_UNIT_LABEL: Record<CoordUnit, string> = {
  latlng: 'Lat/Long',
  utm: 'UTM',
  polar: 'Polar',
};
