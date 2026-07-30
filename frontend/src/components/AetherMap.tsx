import { useCallback, useEffect, useRef, useState, type ChangeEvent } from 'react';
import { useTranslation } from 'react-i18next';
import i18n from '../i18n/i18n';
import L from 'leaflet';
import 'leaflet/dist/leaflet.css';
import 'leaflet.heat';
import type { CSIPacket, ESPDevice, EnvironmentObjectConfig, FloorplanConfig, MapConfig } from '../types/csi';
import { Box, Crosshair, Home, Image as ImageIcon, Layers, MapPin, Maximize2, Minimize2, Move, Plus, Shield, Trash2, Upload, Users, X } from 'lucide-react';
import { COORD_UNIT_LABEL, formatCoord, type CoordUnit, type LatLng } from '../utils/coords';
import { apiFetch } from '../utils/apiClient';

interface AetherMapProps {
  subscribe: (listener: (data: CSIPacket) => void) => () => void;
  isClientView?: boolean;
  isLive?: boolean;
  isReplay?: boolean;
  hasSnapshot?: boolean;
  apiBaseUrl: string;
  // Edição contínua (subir planta, posicionar dispositivos/objetos, opacidade).
  // Default true; a aba Debug passa false (mapa só-leitura) e a Cliente força
  // false via isClientView. A aba "Ajustes & Calibração" passa true.
  editable?: boolean;
  // Modo de coleta de verdade-terra de localização (Feature 2): quando ativo, um
  // clique no mapa devolve lat/lng em vez de abrir edição.
  pickMode?: boolean;
  onMapPick?: (lat: number, lng: number) => void;
  // Marcadores de verdade-terra a desenhar sobre o mapa (validação de localização).
  groundTruthMarkers?: { id: string; lat: number; lng: number; label?: string }[];
}

type MapLayers = {
  floorplanLayer: L.LayerGroup;
  objectLayer: L.LayerGroup;
  espLayer: L.LayerGroup;
  presenceLayer: L.LayerGroup;
  mlPersonsLayer: L.LayerGroup;
  editLayer: L.LayerGroup;
  validationLayer: L.LayerGroup;
};

type DetectionLocation = NonNullable<NonNullable<CSIPacket['detection']>['location']>;

type LatLngPoint = { lat: number; lng: number };

type PendingFloorplan = Omit<FloorplanConfig, 'bounds'> & {
  bounds?: FloorplanConfig['bounds'];
  firstCorner?: LatLngPoint;
};

const EMPTY_MAP_CONFIG: MapConfig = {
  floorplans: [],
  environment_objects: [],
};

function hasMeaningfulCoordinates(lat: number, lng: number) {
  return Number.isFinite(lat) && Number.isFinite(lng) && (Math.abs(lat) > 1e-9 || Math.abs(lng) > 1e-9);
}

function buildClientId(prefix: string) {
  return `${prefix}_${Date.now()}_${Math.floor(Math.random() * 1000)}`;
}

function readMapConfigResponse(value: unknown): MapConfig {
  const candidate = value && typeof value === 'object' && 'map_config' in value
    ? (value as { map_config?: Partial<MapConfig> }).map_config
    : value as Partial<MapConfig> | undefined;

  return {
    floorplans: Array.isArray(candidate?.floorplans) ? candidate.floorplans as FloorplanConfig[] : [],
    environment_objects: Array.isArray(candidate?.environment_objects) ? candidate.environment_objects as EnvironmentObjectConfig[] : [],
  };
}

function boundsFromCorners(first: LatLngPoint, second: LatLngPoint) {
  return {
    north: Math.max(first.lat, second.lat),
    south: Math.min(first.lat, second.lat),
    east: Math.max(first.lng, second.lng),
    west: Math.min(first.lng, second.lng),
  };
}

function hasUsableBounds(bounds: FloorplanConfig['bounds'] | undefined) {
  if (!bounds) return false;
  return [bounds.north, bounds.south, bounds.east, bounds.west].every(Number.isFinite)
    && bounds.north !== bounds.south
    && bounds.east !== bounds.west;
}

function objectFillColor(type: string | undefined) {
  const normalized = (type || '').toLowerCase();
  if (isRoomType(normalized)) return '#0ea5e9';
  if (normalized.includes('wall') || normalized.includes('parede')) return '#64748b';
  if (normalized.includes('desk') || normalized.includes('mesa')) return '#14b8a6';
  if (normalized.includes('chair') || normalized.includes('cadeira')) return '#f59e0b';
  if (normalized.includes('equipment') || normalized.includes('equip')) return '#6366f1';
  return '#8b5cf6';
}

/** Cômodos e paredes são FUNCIONAIS: o backend usa os cômodos para
 *  point-in-polygon ("cômodo atual") e as paredes para corrigir a atenuação
 *  do modelo RSSI→distância. Os demais objetos são contexto de calibração. */
function isRoomType(type: string | undefined) {
  const normalized = (type || '').toLowerCase();
  return normalized.includes('room') || normalized.includes('comodo') || normalized.includes('cômodo');
}

function objectPolygonPoints(object: EnvironmentObjectConfig): [number, number][] {
  const width = Math.max(0.1, object.width_m || 1);
  const depth = Math.max(0.1, object.depth_m || 0.6);
  const lat = object.lat;
  const lng = object.lng;
  const cosLat = Math.max(0.05, Math.cos(lat * Math.PI / 180));
  const rad = ((object.rotation_deg || 0) * Math.PI) / 180;
  const cos = Math.cos(rad);
  const sin = Math.sin(rad);
  const corners = [
    [-width / 2, -depth / 2],
    [width / 2, -depth / 2],
    [width / 2, depth / 2],
    [-width / 2, depth / 2],
  ];

  return corners.map(([eastM, northM]) => {
    const rotatedEast = eastM * cos - northM * sin;
    const rotatedNorth = eastM * sin + northM * cos;
    return [
      lat + rotatedNorth / 111_320,
      lng + rotatedEast / (111_320 * cosLat),
    ];
  });
}

function escapeHtml(value: string) {
  return value
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#039;');
}

function profileLabel(device: Pick<ESPDevice, 'hardware_profile' | 'role'>) {
  const role = (device.role || '').toLowerCase();
  const profile = (device.hardware_profile || '').toLowerCase();

  if (profile.includes('c5')) return 'C5';
  if (profile.includes('wroom') || profile.includes('classic')) return 'WROOM';
  if (profile.includes('router') || role === 'router') return 'ROUTER';
  if (role === 'receiver') return 'RX';
  if (role === 'injector') return 'TX';
  return '';
}

function accuracyMeters(location: DetectionLocation | undefined) {
  if (!location) return 0;
  if (Number.isFinite(location.accuracy_m)) return location.accuracy_m;
  if (Number.isFinite(location.accuracy_dm)) return (location.accuracy_dm ?? 0) / 10;
  return 0;
}

function accuracyLabel(location: DetectionLocation | undefined) {
  if (!location) return i18n.t('aetherMap.noAccuracy', 'sem precisão');
  if (Number.isFinite(location.accuracy_dm)) return `${location.accuracy_dm?.toFixed(1)} dm`;
  const meters = accuracyMeters(location);
  return meters > 0 ? `${meters.toFixed(2)} m` : i18n.t('aetherMap.noAccuracy', 'sem precisão');
}

function methodLabel(method: string | undefined) {
  if (!method || method === 'sem_dados') return i18n.t('aetherMap.calibrating', 'calibrando');
  if (method.includes('fusion')) return i18n.t('aetherMap.fusionC5Wroom', 'fusão C5+WROOM');
  if (method.includes('esp32c5')) return i18n.t('aetherMap.fallbackC5', 'fallback C5');
  if (method.includes('wroom')) return i18n.t('aetherMap.fallbackWroom', 'fallback WROOM');
  return method.replaceAll('_', ' ');
}

function createDeviceIcon(device: ESPDevice, angleDeg?: number) {
  const label = escapeHtml((device.name || device.id).slice(0, 15).toUpperCase());
  const role = (device.role || '').toLowerCase();
  const profile = (device.hardware_profile || '').toLowerCase();
  const isRouter = role === 'router' || profile === 'router' || label.includes('ROTEADOR') || label.includes('ROUTER') || (device.name || '').toLowerCase().includes('injetor') || (device.name || '').toLowerCase().includes('tx');
  const iconUrl = device.icon || (isRouter ? '/router-real.svg' : '/esp32-real.svg');
  const size = device.size || (isRouter ? 40 : 32);
  const badge = profileLabel(device);
  const angleText = angleDeg !== undefined ? `${Math.round(angleDeg)}°` : 'LoS';

  return L.divIcon({
    className: 'aether-device-marker',
    html: `
      <div style="display:flex; flex-direction:column; align-items:center; gap:3px; pointer-events: auto; cursor: pointer;">
        <img src="${iconUrl}" style="width: ${size}px; height: auto; filter: drop-shadow(0 4px 6px rgba(0,0,0,0.5));" />
        <div style="font-size:0.6rem; font-weight:bold; color:white; background:rgba(0,0,0,0.78); border:1px solid rgba(255,255,255,0.25); border-radius:4px; padding:2px 6px; backdrop-filter: blur(4px); white-space:nowrap;">
          ${label}
        </div>
        <div style="display:flex; align-items:center; gap:4px; font-size:0.53rem; font-weight:700; color:#38bdf8; background:rgba(12,74,110,0.88); border:1px solid rgba(56,189,248,0.4); border-radius:4px; padding:1px 5px; white-space:nowrap;">
          <span>↕ 1.2m</span>
          <span>•</span>
          <span>↻ ${angleText}</span>
        </div>
        ${badge ? `<div style="font-size:0.52rem; font-weight:bold; color:#d1fae5; background:rgba(6,78,59,0.85); border-radius:4px; padding:1px 5px; white-space:nowrap;">${badge}</div>` : ''}
      </div>
    `,
    iconSize: [size * 2, size * 2.8],
    iconAnchor: [size, size * 1.35],
  });
}


function createPresenceIcon(
  location: DetectionLocation,
  confidence: number,
  presenceStatus?: string,
  holdRemaining?: number
) {
  const confidenceFill = Math.max(0.2, Math.min(1, confidence));
  const ringOpacity = Math.max(0.15, Math.min(0.55, confidenceFill * 0.6));
  const label = accuracyLabel(location);
  const nodesLabel = location.nodes_used ? i18n.t('aetherMap.nodesCount', '{{count}} nó', { count: location.nodes_used }) : i18n.t('aetherMap.nodesEmpty', 'nós --');
  const isHolding = presenceStatus === 'HOLDING_STATIC';
  const ringColor = isHolding ? '245, 158, 11' : '139, 195, 74';
  const accentColor = isHolding ? '#f59e0b' : 'var(--accent-emerald)';
  const statusBadge = isHolding
    ? `<span style="color:#f59e0b;font-weight:700;">Estático / Repouso${holdRemaining ? ` (${holdRemaining.toFixed(0)}s)` : ''}</span>`
    : `<span style="color:#10b981;font-weight:700;">Ativo</span>`;

  return L.divIcon({
    className: 'aether-presence-marker',
    html: `
      <div style="position:relative; width:28px; height:28px; display:flex; align-items:center; justify-content:center;">
        <div style="position:absolute; inset:0; border-radius:50%; background:rgba(${ringColor}, ${ringOpacity}); box-shadow:0 0 18px rgba(${ringColor}, ${ringOpacity}); animation:presence-pulse 1.8s ease-in-out infinite;"></div>
        <div style="position:absolute; inset:6px; border-radius:50%; background:${accentColor}; border:3px solid white; box-shadow:0 0 20px rgba(${ringColor}, 0.45);"></div>
        <div style="position:absolute; top:-46px; left:18px; width:max-content; background:var(--bg-card); backdrop-filter:blur(10px); padding:6px 8px; border-radius:8px; border:1px solid var(--border); box-shadow:var(--shadow-sm); font-size:0.65rem; color:var(--text-secondary);">
          ${statusBadge} • Confiança ${(confidenceFill * 100).toFixed(0)}% • ${label} • ${nodesLabel}
        </div>
      </div>
    `,
    iconSize: [28, 28],
    iconAnchor: [14, 14],
  });
}

function locationStatus(location: DetectionLocation | undefined, hasPacket: boolean) {
  if (!hasPacket) return i18n.t('aetherMap.waitingCsi', 'Aguardando pacotes CSI...');
  if (!location || !location.method || location.method === 'sem_dados') return i18n.t('aetherMap.calibrating', 'calibrando');
  if ((location.nodes_used ?? 0) >= 2) return i18n.t('aetherMap.twoNodesActive', '2 nós ativos');
  if (location.method.includes('sem_baseline')) return i18n.t('aetherMap.noBaseline', 'sem baseline');
  if (location.method.includes('esp32c5')) return i18n.t('aetherMap.fallbackC5', 'fallback C5');
  if (location.method.includes('wroom')) return i18n.t('aetherMap.fallbackWroom', 'fallback WROOM');
  return i18n.t('aetherMap.calibrating', 'calibrando');
}

export function AetherMap({ subscribe, isClientView, isLive = false, isReplay = false, hasSnapshot = false, apiBaseUrl, editable: editableProp = true, pickMode = false, onMapPick, groundTruthMarkers }: AetherMapProps) {
  const { t } = useTranslation();
  // A vista de cliente é sempre só-leitura; nas demais, respeita a prop.
  const editable = !isClientView && editableProp;
  // Refs para os handlers de clique do Leaflet (fechados por efeito) não ficarem
  // com valores obsoletos de pickMode/onMapPick, que mudam em runtime.
  const pickModeRef = useRef(pickMode);
  pickModeRef.current = pickMode;
  const onMapPickRef = useRef(onMapPick);
  onMapPickRef.current = onMapPick;
  const editableRef = useRef(editable);
  editableRef.current = editable;
  const mapRef = useRef<L.Map | null>(null);
  const mapContainerRef = useRef<HTMLDivElement>(null);
  const floorplanInputRef = useRef<HTMLInputElement>(null);
  const layersRef = useRef<MapLayers | null>(null);
  const mapConfigRef = useRef<MapConfig>(EMPTY_MAP_CONFIG);
  const lastCenterRef = useRef<{lat: number, lng: number, type: 'person' | 'devices' | 'none'}>({ lat: 0, lng: 0, type: 'none' });
  // Marcadores das pessoas ML reusados por índice (não recriados a cada tick):
  // preserva o DOM/animações do avatar e mantém o HUD de vitais aberto durante o
  // hover/toque, mesmo enquanto novos dados chegam pelo WebSocket. `lastSeen` dá
  // um período de carência para que piscas de detecção (a confiança do ML cruza o
  // limiar frame-a-frame) não destruam/recriem o avatar — e fechem o HUD.
  const personMarkersRef = useRef<Map<number, { marker: L.Marker; lastSeen: number }>>(new Map());
  const lastMapRenderRef = useRef<number>(0);
  const editLayoutRef = useRef(false);
  const coordUnitRef = useRef<CoordUnit>('latlng');
  const polarOriginRef = useRef<LatLng | null>(null);
  const latestDevicesRef = useRef<ESPDevice[]>([]);
  const smoothedPresenceLocationRef = useRef<{ lat: number; lng: number } | null>(null);
  const setCursorCoordRef = useRef<(value: string) => void>(() => {});

  const [isAddingMode, setIsAddingMode] = useState(false);
  const [isAddingObjectMode, setIsAddingObjectMode] = useState(false);
  const [isAddingRoomMode, setIsAddingRoomMode] = useState(false);
  const [isAddingWallMode, setIsAddingWallMode] = useState(false);
  const [mapConfig, setMapConfig] = useState<MapConfig>(EMPTY_MAP_CONFIG);
  const [pendingFloorplan, setPendingFloorplan] = useState<PendingFloorplan | null>(null);
  const [mapFeedback, setMapFeedback] = useState<string | null>(null);
  const [floorplanFloor, setFloorplanFloor] = useState(3);

  const [editingDevice, setEditingDevice] = useState<Partial<ESPDevice> | null>(null);
  const [editingObject, setEditingObject] = useState<Partial<EnvironmentObjectConfig> | null>(null);

  const [occupiedRoom, setOccupiedRoom] = useState<string | null>(null);
  const [deviceCount, setDeviceCount] = useState(0);
  const [statusText, setStatusText] = useState(() => i18n.t('aetherMap.waitingCsi', 'Aguardando pacotes CSI...'));
  // Quantas pessoas o modelo ML está desenhando como avatares — usado só para
  // ativar a legenda/affordance do mapa quando há gente em cena.
  const [mlPersonCount, setMlPersonCount] = useState(0);

  // Personalização: modo de edição (arrastar planta/dispositivos) e unidade de coordenadas.
  const [editLayout, setEditLayout] = useState(false);
  const [coordUnit, setCoordUnit] = useState<CoordUnit>('latlng');
  const [cursorCoord, setCursorCoord] = useState('');
  const [editDevices, setEditDevices] = useState<ESPDevice[]>([]);
  const [isCardMinimized, setIsCardMinimized] = useState(false);
  const [showEspVisualGuide, setShowEspVisualGuide] = useState(false);


  useEffect(() => { editLayoutRef.current = editLayout; }, [editLayout]);
  useEffect(() => { coordUnitRef.current = coordUnit; }, [coordUnit]);
  useEffect(() => { setCursorCoordRef.current = setCursorCoord; }, []);

  const shouldShowSnapshot = isLive || isReplay || hasSnapshot;
  const visibleStatusText = shouldShowSnapshot ? statusText : t('aetherMap.waitingCsi', 'Aguardando pacotes CSI...');
  const visibleDeviceCount = shouldShowSnapshot ? deviceCount : 0;
  const visibleOccupiedRoom = shouldShowSnapshot ? occupiedRoom : null;
  const activeFloorplan = mapConfig.floorplans.find((floorplan) => floorplan.floor === floorplanFloor) ?? mapConfig.floorplans[0];
  const isFloorplanPlacementActive = Boolean(pendingFloorplan);

  useEffect(() => {
    mapConfigRef.current = mapConfig;
  }, [mapConfig]);

  const loadMapConfig = useCallback(async () => {
    try {
      const response = await apiFetch(`${apiBaseUrl}/api/map-config`);
      if (!response.ok) {
        throw new Error(`HTTP ${response.status}`);
      }
      const payload = await response.json();
      setMapConfig(readMapConfigResponse(payload));
    } catch (error) {
      console.error('Falha ao carregar configuração do mapa', error);
      setMapFeedback(t('aetherMap.feedback.loadIndoorFailed', 'falha ao carregar mapa indoor'));
    }
  }, [apiBaseUrl]);

  const saveFloorplan = useCallback(async (floorplan: FloorplanConfig) => {
    const response = await apiFetch(`${apiBaseUrl}/api/map-config/floorplans`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(floorplan),
    });
    if (!response.ok) {
      throw new Error(`HTTP ${response.status}`);
    }
    const payload = await response.json();
    setMapConfig(readMapConfigResponse(payload));
  }, [apiBaseUrl]);

  const persistDevice = useCallback(async (device: ESPDevice) => {
    const response = await apiFetch(`${apiBaseUrl}/api/devices`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(device),
    });
    if (!response.ok) {
      throw new Error(`HTTP ${response.status}`);
    }
  }, [apiBaseUrl]);

  const saveEnvironmentObject = useCallback(async (object: EnvironmentObjectConfig) => {
    const response = await apiFetch(`${apiBaseUrl}/api/map-config/environment-objects`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(object),
    });
    if (!response.ok) {
      throw new Error(`HTTP ${response.status}`);
    }
    const payload = await response.json();
    setMapConfig(readMapConfigResponse(payload));
  }, [apiBaseUrl]);

  const deleteFloorplan = useCallback(async (id: string) => {
    const response = await apiFetch(`${apiBaseUrl}/api/map-config/floorplans/${encodeURIComponent(id)}`, {
      method: 'DELETE',
    });
    if (!response.ok) {
      throw new Error(`HTTP ${response.status}`);
    }
    const payload = await response.json();
    setMapConfig(readMapConfigResponse(payload));
  }, [apiBaseUrl]);

  const deleteEnvironmentObject = useCallback(async (id: string) => {
    const response = await apiFetch(`${apiBaseUrl}/api/map-config/environment-objects/${encodeURIComponent(id)}`, {
      method: 'DELETE',
    });
    if (!response.ok) {
      throw new Error(`HTTP ${response.status}`);
    }
    const payload = await response.json();
    setMapConfig(readMapConfigResponse(payload));
  }, [apiBaseUrl]);

  const handleFloorplanFileChange = useCallback((event: ChangeEvent<HTMLInputElement>) => {
    const file = event.target.files?.[0];
    event.target.value = '';
    if (!file) {
      return;
    }

    const reader = new FileReader();
    reader.onload = () => {
      const imageData = typeof reader.result === 'string' ? reader.result : '';
      if (!imageData) {
        setMapFeedback(t('aetherMap.feedback.readFloorplanFailed', 'falha ao ler imagem da planta'));
        return;
      }

      const name = file.name.replace(/\.[^.]+$/, '') || t('aetherMap.defaultFloorplanName', 'Planta baixa');
      const map = mapRef.current;

      // Coloca a planta IMEDIATAMENTE, centrada na vista atual (~60% dela), para
      // ela aparecer na hora (antes era preciso clicar 2 cantos — pouco óbvio, daí
      // "nada acontece"). Ajuste fino depois por opacidade ou "posicionar".
      if (map) {
        const view = map.getBounds();
        const center = view.getCenter();
        const latSpan = (view.getNorth() - view.getSouth()) * 0.3;
        const lngSpan = (view.getEast() - view.getWest()) * 0.3;
        const floorplan: FloorplanConfig = {
          id: buildClientId('floorplan'),
          name,
          floor: floorplanFloor,
          image_data_url: imageData,
          opacity: 0.58,
          bounds: {
            north: center.lat + latSpan,
            south: center.lat - latSpan,
            east: center.lng + lngSpan,
            west: center.lng - lngSpan,
          },
        };
        setPendingFloorplan(null);
        setIsAddingMode(false);
        setIsAddingObjectMode(false);
        setMapFeedback(t('aetherMap.feedback.savingFloorplan', 'salvando planta...'));
        void (async () => {
          try {
            await saveFloorplan(floorplan);
            setMapFeedback(t('aetherMap.feedback.floorplanPlaced', 'planta colocada — ajuste a opacidade ou use "posicionar" para mover'));
          } catch (error) {
            console.error('Falha ao salvar planta baixa', error);
            const detail = error instanceof Error ? error.message : 'erro desconhecido';
            setMapFeedback(t('aetherMap.feedback.saveFloorplanFailedDetail', 'falha ao salvar planta ({{detail}})', { detail }));
          }
        })();
        return;
      }

      // Fallback (mapa ainda não pronto): fluxo manual de 2 cantos.
      setPendingFloorplan({
        id: buildClientId('floorplan'),
        name,
        floor: floorplanFloor,
        image_data_url: imageData,
        opacity: 0.58,
      });
      setIsAddingMode(false);
      setIsAddingObjectMode(false);
      setMapFeedback(t('aetherMap.feedback.selectFirstCorner', 'selecione o primeiro canto da planta'));
    };
    reader.onerror = () => setMapFeedback(t('aetherMap.feedback.readFloorplanFailed', 'falha ao ler imagem da planta'));
    reader.readAsDataURL(file);
  }, [floorplanFloor, saveFloorplan, t]);

  const updateFloorplanOpacity = useCallback((floorplan: FloorplanConfig, opacity: number) => {
    const nextFloorplan = { ...floorplan, opacity };
    setMapConfig((current) => ({
      ...current,
      floorplans: current.floorplans.map((item) => item.id === floorplan.id ? nextFloorplan : item),
    }));

    void (async () => {
      try {
        await saveFloorplan(nextFloorplan);
      } catch (error) {
        console.error('Falha ao atualizar opacidade da planta', error);
        setMapFeedback(t('aetherMap.feedback.updateOpacityFailed', 'falha ao atualizar opacidade'));
      }
    })();
  }, [saveFloorplan, t]);

  useEffect(() => {
    void loadMapConfig();
  }, [loadMapConfig]);

  useEffect(() => {
    if (!mapContainerRef.current || mapRef.current) {
      return;
    }

    const map = L.map(mapContainerRef.current, {
      center: [-22.9068, -43.1729],
      zoom: 20,
      maxZoom: 25,
      zoomSnap: 0.25,
      zoomDelta: 0.5,
      wheelPxPerZoomLevel: 40,
      zoomControl: false,
      attributionControl: true,
    });
    L.control.zoom({ position: 'bottomleft' }).addTo(map);
    mapRef.current = map;

    // Satélite (Esri World Imagery): mostra o telhado real do prédio na foto
    // aérea — útil quando o footprint vetorial não existe no OSM. É a base
    // padrão. O OSM fica como alternativa no seletor de camadas. Para a planta
    // interna do apartamento, use o overlay de "Planta baixa" (floorplan).
    const satelliteTile = L.tileLayer(
      'https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{z}/{y}/{x}',
      {
        maxZoom: 25,
        maxNativeZoom: 19,
        attribution: 'Tiles © Esri — Source: Esri, Maxar, Earthstar Geographics, and the GIS User Community',
      },
    );

    // Rótulos de ruas/locais sobre o satélite (foto aérea não tem nomes).
    const satelliteLabels = L.tileLayer(
      'https://server.arcgisonline.com/ArcGIS/rest/services/Reference/World_Boundaries_and_Places/MapServer/tile/{z}/{y}/{x}',
      { maxZoom: 25, maxNativeZoom: 19, attribution: 'Labels © Esri' },
    );

    const osmTile = L.tileLayer('https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png', {
      maxZoom: 25,
      maxNativeZoom: 19,
      attribution: '© <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> contributors',
    });

    const floorplanLayer = L.layerGroup();
    const objectLayer = L.layerGroup();
    const espLayer = L.layerGroup();
    const presenceLayer = L.layerGroup();
    const mlPersonsLayer = L.layerGroup();
    const editLayer = L.layerGroup();
    const validationLayer = L.layerGroup();
    layersRef.current = { floorplanLayer, objectLayer, espLayer, presenceLayer, mlPersonsLayer, editLayer, validationLayer };

    satelliteTile.addTo(map);
    satelliteLabels.addTo(map);
    floorplanLayer.addTo(map);
    objectLayer.addTo(map);
    espLayer.addTo(map);
    presenceLayer.addTo(map);
    mlPersonsLayer.addTo(map);
    editLayer.addTo(map);
    validationLayer.addTo(map);

    // Readout de coordenadas sob o cursor, na unidade escolhida (lat/long, UTM, polar).
    map.on('mousemove', (event: L.LeafletMouseEvent) => {
      setCursorCoordRef.current(
        formatCoord(event.latlng.lat, event.latlng.lng, coordUnitRef.current, polarOriginRef.current),
      );
    });

    L.control.layers({
      [t('aetherMap.layerSatellite', 'Satélite (Esri)')]: satelliteTile,
      [t('aetherMap.layerOsm', 'Mapa (OSM)')]: osmTile,
    }, {
      [t('aetherMap.layerLabels', 'Rótulos de ruas')]: satelliteLabels,
      [t('aetherMap.layerFloorplan', 'Planta baixa')]: floorplanLayer,
      [t('aetherMap.layerObjects', 'Móveis / calibração')]: objectLayer,
      [t('aetherMap.layerDevices', 'Dispositivos ESP32')]: espLayer,
      [t('aetherMap.layerEstimated', 'Localização estimada + precisão')]: presenceLayer,
    }, { collapsed: false, position: 'bottomright' }).addTo(map);

    window.requestAnimationFrame(() => {
      map.invalidateSize();
    });

    return () => {
      map.remove();
      mapRef.current = null;
      layersRef.current = null;
    };
  }, []);

  useEffect(() => {
    const map = mapRef.current;
    if (!map) {
      return;
    }

    const handleMapClick = (event: L.LeafletMouseEvent) => {
      const point = { lat: event.latlng.lat, lng: event.latlng.lng };

      // Feature 2: coleta de verdade-terra de localização (tem prioridade).
      if (pickModeRef.current) {
        onMapPickRef.current?.(point.lat, point.lng);
        return;
      }

      // Mapa só-leitura (aba Debug / Cliente): nenhuma mutação por clique.
      if (!editableRef.current) {
        return;
      }

      if (pendingFloorplan) {
        if (!pendingFloorplan.firstCorner) {
          setPendingFloorplan({ ...pendingFloorplan, firstCorner: point });
          setMapFeedback(t('aetherMap.feedback.selectOppositeCorner', 'selecione o canto oposto da planta'));
          return;
        }

        const completeFloorplan: FloorplanConfig = {
          id: pendingFloorplan.id,
          name: pendingFloorplan.name,
          floor: pendingFloorplan.floor,
          image_data_url: pendingFloorplan.image_data_url,
          opacity: pendingFloorplan.opacity,
          bounds: boundsFromCorners(pendingFloorplan.firstCorner, point),
        };

        void (async () => {
          try {
            await saveFloorplan(completeFloorplan);
            setPendingFloorplan(null);
            setMapFeedback(t('aetherMap.feedback.floorplanSaved', 'planta baixa salva'));
          } catch (error) {
            console.error('Falha ao salvar planta baixa', error);
            setMapFeedback(t('aetherMap.feedback.saveFloorplanFailed', 'falha ao salvar planta baixa'));
          }
        })();
        return;
      }

      if (isAddingObjectMode) {
        setEditingObject({
          id: buildClientId('obj'),
          name: t('aetherMap.defaultElementName', 'Novo elemento'),
          type: 'furniture',
          lat: point.lat,
          lng: point.lng,
          floor: floorplanFloor,
          width_m: 1.2,
          depth_m: 0.7,
          rotation_deg: 0,
          calibration_relevant: true,
        });
        setIsAddingObjectMode(false);
        return;
      }

      if (isAddingRoomMode) {
        setEditingObject({
          id: buildClientId('room'),
          name: t('aetherMap.defaultRoomName', 'Novo Cômodo'),
          type: 'room',
          lat: point.lat,
          lng: point.lng,
          floor: floorplanFloor,
          width_m: 3.5,
          depth_m: 3.5,
          rotation_deg: 0,
          calibration_relevant: true,
        });
        setIsAddingRoomMode(false);
        return;
      }

      if (isAddingWallMode) {
        setEditingObject({
          id: buildClientId('wall'),
          name: t('aetherMap.defaultWallName', 'Nova Parede'),
          type: 'wall',
          lat: point.lat,
          lng: point.lng,
          floor: floorplanFloor,
          width_m: 3.0,
          depth_m: 0.2,
          rotation_deg: 0,
          calibration_relevant: true,
        });
        setIsAddingWallMode(false);
        return;
      }

      if (isAddingMode) {
        setEditingDevice({
          id: buildClientId('esp'),
          name: t('aetherMap.defaultDeviceName', 'Novo Dispositivo'),
          lat: point.lat,
          lng: point.lng,
          floor: floorplanFloor,
          icon: '/esp32-real.svg',
          size: 32,
          role: 'receiver',
          hardware_profile: 'wroom'
        });
        setIsAddingMode(false);
      }
    };

    map.on('click', handleMapClick);
    return () => {
      map.off('click', handleMapClick);
    };
  }, [floorplanFloor, isAddingMode, isAddingObjectMode, isAddingRoomMode, isAddingWallMode, pendingFloorplan, saveFloorplan]);

  useEffect(() => {
    if (shouldShowSnapshot) {
      return;
    }

    const layers = layersRef.current;
    if (!layers) {
      return;
    }

    layers.espLayer.clearLayers();
    layers.presenceLayer.clearLayers();
    layers.mlPersonsLayer.clearLayers();
    personMarkersRef.current.clear();
    setMlPersonCount(0);
  }, [shouldShowSnapshot]);

  useEffect(() => {
    const map = mapRef.current;
    const layers = layersRef.current;
    if (!map || !layers) {
      return;
    }

    layers.floorplanLayer.clearLayers();
    layers.objectLayer.clearLayers();

    const bounds = L.latLngBounds([]);
    let hasBounds = false;

    mapConfig.floorplans.forEach((floorplan) => {
      if (!hasUsableBounds(floorplan.bounds)) {
        return;
      }

      const overlayBounds: L.LatLngBoundsExpression = [
        [floorplan.bounds.south, floorplan.bounds.west],
        [floorplan.bounds.north, floorplan.bounds.east],
      ];
      L.imageOverlay(floorplan.image_data_url, overlayBounds, {
        opacity: floorplan.opacity,
      })
        .bindTooltip(`${floorplan.name} • ${t('aetherMap.floorWord', 'Andar')} ${floorplan.floor}`, {
          direction: 'top',
          opacity: 0.92,
        })
        .addTo(layers.floorplanLayer);

      bounds.extend([floorplan.bounds.south, floorplan.bounds.west]);
      bounds.extend([floorplan.bounds.north, floorplan.bounds.east]);
      hasBounds = true;
    });

    mapConfig.environment_objects.forEach((object) => {
      if (!hasMeaningfulCoordinates(object.lat, object.lng)) {
        return;
      }

      const color = objectFillColor(object.type);
      const room = isRoomType(object.type);
      const polygon = L.polygon(objectPolygonPoints(object), {
        color,
        fillColor: color,
        // Cômodo: contorno tracejado + preenchimento leve (área funcional da
        // localização, não um móvel sólido).
        fillOpacity: room ? 0.07 : object.calibration_relevant ? 0.32 : 0.14,
        opacity: room ? 0.85 : object.calibration_relevant ? 0.9 : 0.55,
        weight: 2,
        dashArray: room ? '8, 6' : undefined,
      });
      polygon.bindTooltip(
        `${object.name} • ${object.type} • ${object.width_m.toFixed(1)}m x ${object.depth_m.toFixed(1)}m${object.calibration_relevant ? ' • baseline' : ''}`,
        { direction: 'top', opacity: 0.95 }
      );
      polygon.on('click', () => { if (editableRef.current) setEditingObject(object); });
      polygon.addTo(layers.objectLayer);

      const label = escapeHtml((object.name || object.type).slice(0, 14));
      const canDrag = editableRef.current || editLayoutRef.current;
      const marker = L.marker([object.lat, object.lng], {
        icon: L.divIcon({
          className: 'aether-object-marker',
          html: `
            <div style="display:flex; align-items:center; gap:4px; padding:3px 6px; border-radius:6px; background:rgba(15,23,42,0.74); color:white; font-size:0.58rem; font-weight:700; white-space:nowrap; box-shadow:0 4px 10px rgba(0,0,0,0.22); cursor:${canDrag ? 'move' : 'pointer'};">
              <span style="width:8px; height:8px; border-radius:2px; background:${color}; display:inline-block;"></span>${canDrag ? '✋ ' : ''}${label}
            </div>
          `,
          iconSize: [92, 22],
          iconAnchor: [46, 11],
        }),
        interactive: true,
        draggable: canDrag,
        autoPan: true,
      });
      const cornerMarkers: L.Marker[] = [];
      if (canDrag) {
        marker.bindTooltip(`✋ ${object.name} — ${t('aetherMap.dragToMove', 'arraste para mover pelo mapa')}`, { direction: 'top' });
        marker.on('drag', () => {
          const p = marker.getLatLng();
          setCursorCoordRef.current(formatCoord(p.lat, p.lng, coordUnitRef.current, polarOriginRef.current));
          const updated: EnvironmentObjectConfig = { ...object, lat: p.lat, lng: p.lng };
          const newPolygonPoints = objectPolygonPoints(updated);
          polygon.setLatLngs(newPolygonPoints);
          cornerMarkers.forEach((cm, idx) => cm.setLatLng(newPolygonPoints[idx]));
        });
        marker.on('dragend', () => {
          const p = marker.getLatLng();
          const updated: EnvironmentObjectConfig = { ...object, lat: p.lat, lng: p.lng };
          setMapConfig((cur) => ({
            ...cur,
            environment_objects: cur.environment_objects.map((o) => (o.id === object.id ? updated : o)),
          }));
          setEditingObject((cur) => (cur && cur.id === object.id ? { ...cur, lat: p.lat, lng: p.lng } : cur));
          void saveEnvironmentObject(updated).catch((error) => {
            console.error('Falha ao salvar posição do elemento', error);
            setMapFeedback(t('aetherMap.feedback.saveElementPosFailed', 'falha ao salvar posição do elemento'));
          });
          setMapFeedback(t('aetherMap.feedback.elementMoved', '{{name}}: movido com sucesso', { name: object.name }));
        });
      }
      marker.on('click', () => { if (editableRef.current) setEditingObject(object); });
      marker.addTo(layers.objectLayer);

      const showResizeHandles = canDrag && (editLayoutRef.current || (editingObject && editingObject.id === object.id));
      if (showResizeHandles) {
        const currentCorners = objectPolygonPoints(object);
        currentCorners.forEach(([cornerLat, cornerLng], i) => {
          const cornerMarker = L.marker([cornerLat, cornerLng], {
            icon: L.divIcon({
              className: 'aether-resize-handle',
              html: `<div style="background:${color};width:14px;height:14px;border-radius:50%;border:2px solid #fff;box-shadow:0 2px 5px rgba(0,0,0,.4); cursor:nwse-resize;"></div>`,
              iconSize: [14, 14],
              iconAnchor: [7, 7],
            }),
            interactive: true,
            draggable: true,
            autoPan: true,
          });
          cornerMarker.bindTooltip(`↔ ${t('aetherMap.dragCornerToResize', 'Arraste o canto para redimensionar')} (${object.name})`, { direction: 'top' });
          cornerMarker.on('drag', () => {
            const p = cornerMarker.getLatLng();
            setCursorCoordRef.current(formatCoord(p.lat, p.lng, coordUnitRef.current, polarOriginRef.current));

            const oppIdx = (i + 2) % 4;
            const opp = objectPolygonPoints(object)[oppIdx];

            const newLat = (opp[0] + p.lat) / 2;
            const newLng = (opp[1] + p.lng) / 2;

            const cosLat = Math.max(0.05, Math.cos(newLat * Math.PI / 180));
            const dNorth = (p.lat - opp[0]) * 111_320;
            const dEast = (p.lng - opp[1]) * 111_320 * cosLat;

            const rad = -((object.rotation_deg || 0) * Math.PI) / 180;
            const cos = Math.cos(rad);
            const sin = Math.sin(rad);
            const localEast = dEast * cos - dNorth * sin;
            const localNorth = dEast * sin + dNorth * cos;

            const newWidth = Math.max(0.2, Math.abs(localEast));
            const newDepth = Math.max(0.2, Math.abs(localNorth));

            const updated: EnvironmentObjectConfig = {
              ...object,
              lat: newLat,
              lng: newLng,
              width_m: newWidth,
              depth_m: newDepth,
            };

            const newPolygonPoints = objectPolygonPoints(updated);
            polygon.setLatLngs(newPolygonPoints);
            marker.setLatLng([newLat, newLng]);
            cornerMarkers.forEach((cm, idx) => {
              if (idx !== i) cm.setLatLng(newPolygonPoints[idx]);
            });
          });
          cornerMarker.on('dragend', () => {
            const p = cornerMarker.getLatLng();
            const oppIdx = (i + 2) % 4;
            const opp = objectPolygonPoints(object)[oppIdx];

            const newLat = (opp[0] + p.lat) / 2;
            const newLng = (opp[1] + p.lng) / 2;

            const cosLat = Math.max(0.05, Math.cos(newLat * Math.PI / 180));
            const dNorth = (p.lat - opp[0]) * 111_320;
            const dEast = (p.lng - opp[1]) * 111_320 * cosLat;

            const rad = -((object.rotation_deg || 0) * Math.PI) / 180;
            const cos = Math.cos(rad);
            const sin = Math.sin(rad);
            const localEast = dEast * cos - dNorth * sin;
            const localNorth = dEast * sin + dNorth * cos;

            const newWidth = Math.max(0.2, Math.abs(localEast));
            const newDepth = Math.max(0.2, Math.abs(localNorth));

            const updated: EnvironmentObjectConfig = {
              ...object,
              lat: newLat,
              lng: newLng,
              width_m: newWidth,
              depth_m: newDepth,
            };

            setMapConfig((cur) => ({
              ...cur,
              environment_objects: cur.environment_objects.map((o) => (o.id === object.id ? updated : o)),
            }));
            setEditingObject((cur) => (cur && cur.id === object.id ? { ...cur, lat: newLat, lng: newLng, width_m: newWidth, depth_m: newDepth } : cur));
            void saveEnvironmentObject(updated).catch((error) => {
              console.error('Falha ao salvar redimensionamento do elemento', error);
              setMapFeedback(t('aetherMap.feedback.saveResizeFailed', 'falha ao salvar redimensionamento'));
            });
            setMapFeedback(t('aetherMap.feedback.elementResized', '{{name}}: redimensionado ({{width}}m × {{depth}}m)', { name: object.name, width: newWidth.toFixed(1), depth: newDepth.toFixed(1) }));
          });
          cornerMarker.addTo(layers.objectLayer);
          cornerMarkers.push(cornerMarker);
        });
      }

      bounds.extend([object.lat, object.lng]);
      hasBounds = true;
    });

    if (hasBounds && bounds.isValid() && lastCenterRef.current.type === 'none') {
      map.fitBounds(bounds, { padding: [40, 40], maxZoom: 23 });
    }
  }, [mapConfig, saveEnvironmentObject, editable, editLayout, editingObject, t]);

  // Feature 2: marcadores de verdade-terra (validação de localização no mapa).
  useEffect(() => {
    const layers = layersRef.current;
    if (!layers) return;
    layers.validationLayer.clearLayers();
    (groundTruthMarkers ?? []).forEach((gt) => {
      if (!hasMeaningfulCoordinates(gt.lat, gt.lng)) return;
      L.marker([gt.lat, gt.lng], {
        icon: L.divIcon({
          className: 'aether-gt-marker',
          html: `<div style="display:flex;flex-direction:column;align-items:center;transform:translateY(-4px);">
            <div style="width:16px;height:16px;border-radius:50%;background:#f59e0b;border:2px solid #fff;box-shadow:0 2px 6px rgba(0,0,0,0.35);"></div>
            ${gt.label ? `<span style="margin-top:2px;font-size:0.58rem;font-weight:700;color:#fff;background:rgba(15,23,42,0.82);padding:1px 5px;border-radius:6px;white-space:nowrap;">${escapeHtml(gt.label)}</span>` : ''}
          </div>`,
          iconSize: [16, 16],
          iconAnchor: [8, 8],
        }),
        interactive: false,
      })
        .bindTooltip(t('aetherMap.groundTruthTooltip', 'Verdade-terra (posição real marcada)'), { direction: 'top', opacity: 0.9 })
        .addTo(layers.validationLayer);
    });
  }, [groundTruthMarkers, t]);

  useEffect(() => {
    const unsubscribe = subscribe((data) => {
      // Throttle map updates to ~2 fps to avoid DOM thrashing
      const now = performance.now();
      if (now - lastMapRenderRef.current < 500) return;
      lastMapRenderRef.current = now;

      if (!mapRef.current || !layersRef.current) {
        return;
      }

      const layers = layersRef.current;
      const hasPacket = data.rssi !== null && data.rssi !== undefined;
      const presenceDetected = Boolean(hasPacket && data.detection?.presence?.detected);
      const location = data.detection?.location;
      const validLocation = Boolean(location && hasMeaningfulCoordinates(location.lat, location.lng));
      const confidence = location?.confidence ?? data.detection?.presence?.confidence ?? 0;
      const receiverCount = (data.devices ?? []).filter((device) => {
        const role = (device.role || 'receiver').toLowerCase();
        return role === 'receiver' || role === '';
      }).length;

      setDeviceCount(receiverCount);
      setStatusText(locationStatus(location, hasPacket));

      // Mantém a lista de dispositivos p/ o modo de edição e define a origem polar
      // (1º receptor com coordenadas) usada no readout de coordenadas.
      latestDevicesRef.current = data.devices ?? [];
      const polarRef = (data.devices ?? []).find((d) => {
        const r = (d.role || 'receiver').toLowerCase();
        return (r === 'receiver' || r === '') && hasMeaningfulCoordinates(d.lat, d.lng);
      });
      if (polarRef) {
        polarOriginRef.current = { lat: polarRef.lat, lng: polarRef.lng };
      }

      layers.espLayer.clearLayers();
      const bounds = L.latLngBounds([]);
      let hasBounds = false;

      const occupancy = data.detection?.occupancy;
      const mlPersons = occupancy?.available && Array.isArray(occupancy.persons) ? occupancy.persons : [];
      const hasMlPersons = mlPersons.length > 0;

      let closestDeviceName: string | null = null;
      let minDistToDevice = Infinity;

      const refLat = validLocation && location ? location.lat : (hasMlPersons ? mlPersons[0].lat : undefined);
      const refLng = validLocation && location ? location.lng : (hasMlPersons ? mlPersons[0].lng : undefined);

      if (refLat !== undefined && refLng !== undefined && data.devices) {
        data.devices.forEach(d => {
          if (hasMeaningfulCoordinates(d.lat, d.lng) && mapRef.current) {
            const dist = mapRef.current.distance([refLat, refLng], [d.lat, d.lng]);
            if (dist < minDistToDevice) {
              minDistToDevice = dist;
              closestDeviceName = d.name;
            }
          }
        });
      }
      setOccupiedRoom(closestDeviceName);

      // No modo de edição, os dispositivos viram alças arrastáveis na editLayer
      // (effect dedicado); aqui não desenhamos os marcadores fixos para não
      // recriá-los a cada tick e atrapalhar o arraste.
      (editLayoutRef.current ? [] : (data.devices ?? [])).forEach((device, index) => {
        if (!hasMeaningfulCoordinates(device.lat, device.lng)) {
          return;
        }

        bounds.extend([device.lat, device.lng]);
        hasBounds = true;

        const isThisRoomOccupied = (presenceDetected || validLocation || hasMlPersons) && closestDeviceName === device.name;

        const role = (device.role || 'receiver').toLowerCase();
        const isReceiver = role === 'receiver' || role === '';

        // Cálculo visual de orientação da antena (ângulo em graus apontado para o centro)
        let angleDeg = (device as any).orientation_deg;
        if (angleDeg === undefined || angleDeg === null) {
          // Apontar visualmente para o centro das âncoras/sala
          const centerLat = bounds.isValid() ? bounds.getCenter().lat : device.lat;
          const centerLng = bounds.isValid() ? bounds.getCenter().lng : device.lng;
          const dLat = centerLat - device.lat;
          const dLng = centerLng - device.lng;
          angleDeg = (Math.atan2(dLng, dLat) * 180 / Math.PI + 360) % 360;
        }

        // 1) Desenhar Setor Visual de Cobertura RF (Cone direcional da antena)
        const coneRadius = 0.000032; // ~3.5m no plano georreferenciado
        const conePoints: [number, number][] = [[device.lat, device.lng]];
        for (let a = -28; a <= 28; a += 7) {
          const rad = ((angleDeg + a) * Math.PI) / 180;
          conePoints.push([
            device.lat + coneRadius * Math.cos(rad),
            device.lng + coneRadius * Math.sin(rad)
          ]);
        }
        const sectorCone = L.polygon(conePoints, {
          color: isThisRoomOccupied ? '#10b981' : '#0ea5e9',
          fillColor: isThisRoomOccupied ? '#10b981' : '#0ea5e9',
          fillOpacity: isThisRoomOccupied ? 0.24 : 0.12,
          weight: 1.5,
          dashArray: '3, 3'
        });
        sectorCone.bindTooltip(`📡 Setor RF Direcional • Ângulo Visada: ${Math.round(angleDeg)}° | Polarização Vertical`, { direction: 'top' });
        sectorCone.addTo(layers.espLayer);

        // 2) Linha de âncora redonda
        const zone = L.circle([device.lat, device.lng], {
          radius: isReceiver ? 3 : 5,
          color: isThisRoomOccupied ? '#10b981' : '#64748b',
          fillColor: isThisRoomOccupied ? '#10b981' : '#1e293b',
          fillOpacity: isThisRoomOccupied ? 0.35 : 0.1,
          weight: 2,
          dashArray: isThisRoomOccupied ? '' : '5, 5'
        });
        zone.bindTooltip(`${isReceiver ? t('aetherMap.anchor', 'Âncora') : t('aetherMap.reference', 'Referência')}: ${device.name}`, { direction: 'top' });
        zone.addTo(layers.espLayer);

        const marker = L.marker([device.lat, device.lng], {
          icon: createDeviceIcon(device, angleDeg),
          riseOnHover: true,
        });

        marker.on('click', () => {
          if (editableRef.current) setEditingDevice(device);
        });

        marker.bindTooltip(
          `${device.name} • ${profileLabel(device) || 'ESP32'} • ${role || 'receiver'} • ${t('aetherMap.floorWord', 'Andar')} ${device.floor} • ID ${device.id.slice(-4)} • #${index + 1}\n📍 Altura: 1.2m | Ângulo de Visada: ${Math.round(angleDeg)}° LoS | Polarização: Vertical ↕`,
          { direction: 'top', opacity: 0.95 }
        );
        marker.addTo(layers.espLayer);
      });

      // 3) Desenho da malha visual de Linha de Visada Direta (LoS TX <-> RX) no mapa
      const visibleDevs = (editLayoutRef.current ? [] : (data.devices ?? [])).filter(d => hasMeaningfulCoordinates(d.lat, d.lng));
      if (visibleDevs.length >= 2) {
        const txDevs = visibleDevs.filter(d => (d.role || '').toLowerCase() === 'router' || (d.hardware_profile || '').toLowerCase() === 'router' || (d.name || '').toLowerCase().includes('tx'));
        const rxDevs = visibleDevs.filter(d => !txDevs.includes(d));
        const sources = txDevs.length > 0 ? txDevs : [visibleDevs[0]];
        const targets = txDevs.length > 0 ? rxDevs : visibleDevs.slice(1);

        sources.forEach(tx => {
          targets.forEach(rx => {
            const losLine = L.polyline([
              [tx.lat, tx.lng],
              [rx.lat, rx.lng]
            ], {
              color: '#10b981',
              weight: 2,
              opacity: 0.65,
              dashArray: '5, 7'
            });
            losLine.bindTooltip('⚡ Feixe CSI LoS (Linha de Visada Direta entre ESP32)', { direction: 'center', opacity: 0.9 });
            losLine.addTo(layers.espLayer);
          });
        });
      }


      let targetCenter: { lat: number, lng: number, type: 'person' | 'devices' } | null = null;
      let boundsToFit: L.LatLngBounds | null = null;

      if ((validLocation && location) || hasMlPersons) {
        const centerLat = validLocation && location ? location.lat : mlPersons[0].lat!;
        const centerLng = validLocation && location ? location.lng : mlPersons[0].lng!;
        targetCenter = { lat: centerLat, lng: centerLng, type: 'person' };
        boundsToFit = L.latLngBounds([centerLat, centerLng], [centerLat, centerLng]);
        if (hasBounds) boundsToFit.extend(bounds); // Ensure devices are also visible if possible
      } else if (hasBounds && bounds.isValid()) {
        const center = bounds.getCenter();
        targetCenter = { lat: center.lat, lng: center.lng, type: 'devices' };
        boundsToFit = bounds;
      }

      if (targetCenter && boundsToFit && mapRef.current && !editLayoutRef.current) {
        const last = lastCenterRef.current;
        const dist = last.type === 'none' ? 999 : mapRef.current.distance([last.lat, last.lng], [targetCenter.lat, targetCenter.lng]);

        const typeChanged = last.type !== targetCenter.type;
        const movedSignificantly = dist > 2; // > 2 meters

        if (typeChanged || movedSignificantly || last.type === 'none') {
          lastCenterRef.current = targetCenter;
          if (targetCenter.type === 'person' && !typeChanged) {
            mapRef.current.panTo([targetCenter.lat, targetCenter.lng], { animate: true, duration: 1 });
          } else {
            const indoorContext = mapConfigRef.current.floorplans.length > 0 || mapConfigRef.current.environment_objects.length > 0;
            mapRef.current.flyToBounds(boundsToFit, { padding: [50, 50], maxZoom: indoorContext ? 23 : 21, duration: 1.5 });
          }
        }
      }

      layers.presenceLayer.clearLayers();
      const presenceStatus = data.detection?.presence?.status ?? (presenceDetected ? 'ACTIVE' : 'EMPTY');
      const holdRemaining = data.detection?.presence?.hold_seconds_remaining;

      if (validLocation && location) {
        const prev = smoothedPresenceLocationRef.current;
        let smoothedLat = location.lat;
        let smoothedLng = location.lng;

        if (prev) {
          const distDegrees = Math.hypot(location.lat - prev.lat, location.lng - prev.lng);
          if (distDegrees <= 0.00018) {
            const alpha = presenceStatus === 'HOLDING_STATIC' ? 0.15 : 0.35;
            smoothedLat = alpha * location.lat + (1 - alpha) * prev.lat;
            smoothedLng = alpha * location.lng + (1 - alpha) * prev.lng;
          }
        }
        smoothedPresenceLocationRef.current = { lat: smoothedLat, lng: smoothedLng };

        const accuracyRadius = Math.max(0.25, accuracyMeters(location));
        L.circle([smoothedLat, smoothedLng], {
          radius: accuracyRadius,
          color: presenceStatus === 'HOLDING_STATIC' ? '#f59e0b' : '#10b981',
          fillColor: presenceStatus === 'HOLDING_STATIC' ? '#f59e0b' : '#10b981',
          fillOpacity: 0.12,
          weight: 2,
          dashArray: '6, 6',
        })
          .bindTooltip(`Raio de precisão estimado: ${accuracyLabel(location)}`, {
            direction: 'top',
            opacity: 0.95,
          })
          .addTo(layers.presenceLayer);

        const marker = L.marker([smoothedLat, smoothedLng], {
          icon: createPresenceIcon(location, confidence, presenceStatus, holdRemaining),
          riseOnHover: true,
        });

        const statusLabelTooltip = presenceStatus === 'HOLDING_STATIC' ? ' • Repouso / Hold' : '';
        marker.bindTooltip(
          `${t('aetherMap.estimatedLocation', 'Localização estimada')}${statusLabelTooltip}${location.current_room ? ` • ${location.current_room}` : ''} • ${methodLabel(location.method)} • ${accuracyLabel(location)} • ${(confidence * 100).toFixed(0)}% ${t('aetherMap.confidenceWord', 'confiança')}`,
          { direction: 'top', opacity: 0.95 }
        );
        marker.addTo(layers.presenceLayer);
      } else {
        smoothedPresenceLocationRef.current = null;
      }

      // ── Pessoas do modelo ML (ONNX), lado a lado com a heurística ──────
      // Marcadores reusados por índice (NÃO recriados a cada tick): o avatar e
      // suas animações, e o HUD de vitais aberto durante hover/toque, sobrevivem
      // aos novos dados do WebSocket — só a posição e os números são atualizados.
      const persons = mlPersons;
      const personMarkers = personMarkersRef.current;
      // Reusa o `now` (performance.now, ms) já capturado no topo do callback.
      const PERSON_GRACE_MS = 2500; // mantém o avatar/HUD por até 2,5 s de ausência
      const clamp = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, v));

      const PERSON_SVG =
        '<svg viewBox="0 0 24 24" width="16" height="16" fill="#fff"><path d="M12 12a4.4 4.4 0 1 0-4.4-4.4A4.4 4.4 0 0 0 12 12Zm0 2c-3.6 0-7.2 1.8-7.2 4.6V20h14.4v-1.4c0-2.8-3.6-4.6-7.2-4.6Z"/></svg>';
      const HEART_SVG =
        '<svg viewBox="0 0 24 24" width="22" height="22" fill="currentColor"><path d="M12 21s-7.4-4.6-9.9-9.2C.4 8.3 2.1 5 5.3 5c2 0 3.3 1.2 4.7 2.8C11.3 6.2 12.7 5 14.7 5 17.9 5 19.6 8.3 22 11.8 19.4 16.4 12 21 12 21Z"/></svg>';
      const LUNGS_SVG =
        '<svg viewBox="0 0 24 24" width="22" height="22" fill="none" stroke="currentColor" stroke-width="2.4"><circle cx="12" cy="12" r="8"/></svg>';

      const seenPersons = new Set<number>();
      persons.forEach((person, idx) => {
        if (person.lat === undefined || person.lng === undefined) return;
        if (!hasMeaningfulCoordinates(person.lat, person.lng)) return;
        seenPersons.add(idx);

        // Cor própria por pessoa (matiz HSL) para distinguir vários avatares.
        const hue = [265, 200, 150, 32, 330][idx % 5];

        // Coração e respiração pulsam na frequência REAL captada: a duração da
        // animação = 60/taxa (s por batida/respiração), limitada a faixas visíveis.
        const beat = person.heart_rate_bpm
          ? `${clamp(60 / person.heart_rate_bpm, 0.4, 1.5).toFixed(2)}s`
          : '0.85s';
        const breath = person.breathing_rate
          ? `${clamp(60 / person.breathing_rate, 2, 6).toFixed(2)}s`
          : '4s';

        const heartVal = person.heart_rate_bpm !== undefined ? person.heart_rate_bpm.toFixed(0) : '--';
        const breathVal = person.breathing_rate !== undefined ? person.breathing_rate.toFixed(1) : '--';
        const confPct = (person.confidence * 100).toFixed(0);
        const noteHtml =
          person.vitals_scope === 'global'
            ? `<div class="vh__note"><span>📡</span><span>${t('aetherMap.globalVitalsNote', 'Estimativa global do ambiente (vitais por sala — 1 antena não separa pessoas).')}</span></div>`
            : '';

        // HUD de vitais — coração batendo e anel de respiração animados na
        // frequência real, mostrados ao passar o mouse ou tocar na pessoa.
        const hudHtml = `<div class="vh" style="--hue:${hue};--beat:${beat};--breath:${breath}">
            <div class="vh__head">
              <span class="vh__dot"></span>
              <span class="vh__title">${t('aetherMap.personWord', 'Pessoa')} ${idx + 1}</span>
              <span class="vh__conf">${confPct}%</span>
            </div>
            <div class="vh__row">
              <span class="vh__icon vh__heart">${HEART_SVG}</span>
              <span><span class="vh__val">${heartVal}</span><span class="vh__unit">bpm</span><div class="vh__lbl">${t('aetherMap.heartWord', 'Coração')}</div></span>
            </div>
            <div class="vh__row">
              <span class="vh__icon vh__lungs">${LUNGS_SVG}</span>
              <span><span class="vh__val">${breathVal}</span><span class="vh__unit">irpm</span><div class="vh__lbl">${t('aetherMap.breathingWord', 'Respiração')}</div></span>
            </div>
            ${noteHtml}
          </div>`;

        const entry = personMarkers.get(idx);
        if (!entry) {
          // Cria o avatar (núcleo + silhueta + badge) UMA vez por slot de pessoa.
          const icon = L.divIcon({
            className: 'ml-person-marker',
            html: `<div class="gp-avatar" style="--hue:${hue}">
              <span class="gp-badge">${idx + 1}</span>
              <div class="gp-core">${PERSON_SVG}</div>
            </div>`,
            iconSize: [40, 40],
            iconAnchor: [20, 20],
          });
          const m = L.marker([person.lat, person.lng], { icon, riseOnHover: true })
            .bindPopup(hudHtml, {
              className: 'vitals-hud-popup',
              closeButton: false,
              offset: [0, -6],
              autoPan: false,
              maxWidth: 240,
            })
            .addTo(layers.mlPersonsLayer);

          // Desktop: abre ao passar o mouse, fecha ao sair. Touch: o toque (click)
          // abre; tocar em outro lugar fecha (autoClose padrão do Leaflet).
          m.on('mouseover', () => m.openPopup());
          m.on('mouseout', () => m.closePopup());
          personMarkers.set(idx, { marker: m, lastSeen: now });
        } else {
          entry.lastSeen = now;
          entry.marker.setLatLng([person.lat, person.lng]);
          if (entry.marker.isPopupOpen()) {
            // HUD aberto: atualiza só os números e as durações NO LUGAR, sem
            // recriar o DOM — assim o batimento/respiração não "saltam".
            const el = entry.marker.getPopup()?.getElement();
            const root = el?.querySelector('.vh') as HTMLElement | null;
            if (root) {
              root.style.setProperty('--beat', beat);
              root.style.setProperty('--breath', breath);
              const conf = el?.querySelector('.vh__conf');
              if (conf) conf.textContent = `${confPct}%`;
              const vals = el?.querySelectorAll('.vh__val');
              if (vals?.[0]) vals[0].textContent = heartVal;
              if (vals?.[1]) vals[1].textContent = breathVal;
            }
          } else {
            // Fechado: pode trocar o conteúdo inteiro (reabre já atualizado).
            entry.marker.setPopupContent(hudHtml);
          }
        }
      });

      // Remove avatares só após o período de carência: piscas de detecção (idx
      // ausente por 1-2 ticks) NÃO destroem o avatar nem fecham o HUD aberto.
      for (const [idx, entry] of personMarkers) {
        if (!seenPersons.has(idx) && now - entry.lastSeen > PERSON_GRACE_MS) {
          layers.mlPersonsLayer.removeLayer(entry.marker);
          personMarkers.delete(idx);
        }
      }

      // Conta os avatares de fato em cena (após carência) para ativar a legenda.
      setMlPersonCount(personMarkers.size);
    });

    return () => unsubscribe();
  }, [subscribe, t]);

  // Modo de edição: alças arrastáveis para reposicionar dispositivos e mover/
  // redimensionar a planta baixa diretamente sobre o mapa/satélite.
  useEffect(() => {
    const map = mapRef.current;
    const layers = layersRef.current;
    if (!map || !layers) return;

    layers.editLayer.clearLayers();
    if (!editLayout) return;

    const unit = coordUnitRef.current;
    const origin = polarOriginRef.current;

    editDevices.forEach((device) => {
      if (!hasMeaningfulCoordinates(device.lat, device.lng)) return;
      const marker = L.marker([device.lat, device.lng], {
        icon: createDeviceIcon(device),
        draggable: true,
        autoPan: true,
      });
      marker.bindTooltip(`✋ ${device.name} — ${t('aetherMap.dragToReposition', 'arraste para reposicionar')}`, { direction: 'top' });
      marker.on('drag', () => {
        const p = marker.getLatLng();
        setCursorCoordRef.current(formatCoord(p.lat, p.lng, coordUnitRef.current, polarOriginRef.current));
      });
      marker.on('dragend', () => {
        const p = marker.getLatLng();
        const updated: ESPDevice = { ...device, lat: p.lat, lng: p.lng };
        setEditDevices((cur) => cur.map((d) => (d.id === device.id ? updated : d)));
        void persistDevice(updated).catch((error) => {
          console.error('Falha ao salvar posição do dispositivo', error);
          setMapFeedback(t('aetherMap.feedback.saveDevicePosFailed', 'falha ao salvar posição do dispositivo'));
        });
        setMapFeedback(`${device.name}: ${formatCoord(p.lat, p.lng, unit, origin)}`);
      });
      marker.addTo(layers.editLayer);
    });

    mapConfig.floorplans.forEach((floorplan) => {
      if (!hasUsableBounds(floorplan.bounds)) return;
      const b = floorplan.bounds;
      const center = { lat: (b.north + b.south) / 2, lng: (b.east + b.west) / 2 };

      const moveHandle = L.marker([center.lat, center.lng], {
        draggable: true,
        icon: L.divIcon({
          className: 'aether-edit-handle',
          html: `<div style="background:#2563eb;color:#fff;border-radius:6px;padding:3px 7px;font-size:11px;font-weight:700;white-space:nowrap;box-shadow:0 2px 6px rgba(0,0,0,.3);">✛ ${t('aetherMap.moveFloorplan', 'mover planta')}</div>`,
          iconSize: [96, 22],
          iconAnchor: [48, 11],
        }),
      });
      moveHandle.on('dragend', () => {
        const p = moveHandle.getLatLng();
        const dLat = p.lat - center.lat;
        const dLng = p.lng - center.lng;
        const updated: FloorplanConfig = {
          ...floorplan,
          bounds: { north: b.north + dLat, south: b.south + dLat, east: b.east + dLng, west: b.west + dLng },
        };
        void saveFloorplan(updated).catch((error) => {
          console.error('Falha ao mover planta', error);
          setMapFeedback(t('aetherMap.feedback.moveFloorplanFailed', 'falha ao mover planta'));
        });
        setMapFeedback(t('aetherMap.feedback.floorplanMoved', 'planta movida'));
      });
      moveHandle.addTo(layers.editLayer);

      const resizeHandle = L.marker([b.north, b.east], {
        draggable: true,
        icon: L.divIcon({
          className: 'aether-edit-handle',
          html: '<div style="background:#f59e0b;width:18px;height:18px;border-radius:50%;border:2px solid #fff;box-shadow:0 2px 6px rgba(0,0,0,.3);"></div>',
          iconSize: [18, 18],
          iconAnchor: [9, 9],
        }),
      });
      resizeHandle.bindTooltip(t('aetherMap.dragResizeNE', 'arraste p/ redimensionar (canto NE)'), { direction: 'right' });
      resizeHandle.on('dragend', () => {
        const p = resizeHandle.getLatLng();
        const north = Math.max(p.lat, b.south + 1e-6);
        const east = Math.max(p.lng, b.west + 1e-6);
        const updated: FloorplanConfig = { ...floorplan, bounds: { ...b, north, east } };
        void saveFloorplan(updated).catch((error) => {
          console.error('Falha ao redimensionar planta', error);
          setMapFeedback(t('aetherMap.feedback.resizeFloorplanFailed', 'falha ao redimensionar planta'));
        });
        setMapFeedback(t('aetherMap.feedback.floorplanResized', 'planta redimensionada'));
      });
      resizeHandle.addTo(layers.editLayer);
    });
  }, [editLayout, editDevices, mapConfig, persistDevice, saveFloorplan, t]);

  const toggleEditLayout = useCallback(() => {
    setEditLayout((current) => {
      const next = !current;
      if (next) {
        setEditDevices(latestDevicesRef.current.filter((d) => hasMeaningfulCoordinates(d.lat, d.lng)));
        setMapFeedback(t('aetherMap.feedback.editModeStarted', 'modo edição: arraste dispositivos e a planta; solte para salvar'));
      } else {
        setMapFeedback(t('aetherMap.feedback.editModeEnded', 'modo edição encerrado'));
      }
      return next;
    });
  }, [t]);

  return (
    <section className="bento-card" style={{ height: '100%', padding: '0', overflow: 'hidden', position: 'relative' }}>
      <div
        className={`map-control-card ${isCardMinimized ? 'minimized' : 'expanded'}`}
        style={{
          position: 'absolute',
          top: '16px',
          left: '16px',
          zIndex: 1000,
        background: 'var(--bg-card)',
        backdropFilter: 'blur(10px)',
        padding: isCardMinimized ? '8px 14px' : '12px 16px',
        borderRadius: 'var(--radius-md)',
        boxShadow: 'var(--shadow-sm)',
        border: '1px solid var(--border)',
        width: 'fit-content',
        maxWidth: 'calc(100% - 32px)',
        transition: 'all 0.25s cubic-bezier(0.4, 0, 0.2, 1)',
        overflow: 'hidden'
      }}>
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: '16px' }}>
          <h2
            style={{ fontSize: '0.85rem', margin: 0, color: 'var(--text-secondary)', display: 'flex', alignItems: 'center', gap: '8px', cursor: 'pointer', userSelect: 'none' }}
            onClick={() => setIsCardMinimized(!isCardMinimized)}
            title={isCardMinimized ? t('aetherMap.expandTitle', 'Clique para expandir o painel') : t('aetherMap.minimizeTitle', 'Clique para minimizar o painel')}
          >
            <Users size={18} style={{ flexShrink: 0 }} />
            <span>{t('aetherMap.title', 'MONITORAMENTO DE PESSOAS')}</span>
          </h2>
          <div style={{ display: 'flex', alignItems: 'center', gap: '8px', flexShrink: 0 }}>
            {isCardMinimized && (
              <span style={{ fontSize: '0.68rem', color: '#10b981', background: 'rgba(16, 185, 129, 0.15)', border: '1px solid rgba(16, 185, 129, 0.3)', padding: '2px 8px', borderRadius: '999px', fontWeight: 600 }}>
                {visibleDeviceCount} ESP32
              </span>
            )}
            <button
              type="button"
              className="btn btn--secondary"
              onClick={() => setIsCardMinimized(!isCardMinimized)}
              style={{ padding: '0', height: '24px', width: '24px', display: 'flex', alignItems: 'center', justifyContent: 'center', borderRadius: '6px', border: '1px solid var(--border)', background: 'var(--bg-surface)', cursor: 'pointer' }}
              title={isCardMinimized ? t('aetherMap.expandBtn', 'Expandir painel de controle do mapa') : t('aetherMap.minimizeBtn', 'Minimizar painel para desobstruir o mapa')}
            >
              {isCardMinimized ? <Maximize2 size={13} /> : <Minimize2 size={13} />}
            </button>
          </div>
        </div>

        {!isCardMinimized && (
          <>
            <p style={{ fontSize: '0.7rem', color: 'var(--text-muted)', margin: '4px 0 0' }}>{t('aetherMap.subtitle', 'Monitoramento Georreferenciado / Planta Baixa')}</p>
            <div style={{ display: 'flex', gap: '6px', flexWrap: 'wrap', marginTop: '10px' }}>
              <span style={{ fontSize: '0.7rem', color: 'var(--text-secondary)', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '4px 8px', borderRadius: '999px' }}>
                {visibleStatusText}
              </span>
              <span style={{ fontSize: '0.7rem', color: 'var(--text-secondary)', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '4px 8px', borderRadius: '999px' }}>
                {visibleDeviceCount} ESP32
              </span>
              <span style={{ fontSize: '0.7rem', color: 'var(--text-secondary)', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '4px 8px', borderRadius: '999px' }}>
                {t('aetherMap.elementsCount', '{{count}} elementos', { count: mapConfig.environment_objects.length })}
              </span>
              <button
                type="button"
                onClick={() => setShowEspVisualGuide(true)}
                style={{
                  fontSize: '0.7rem',
                  fontWeight: 600,
                  color: '#0ea5e9',
                  background: 'rgba(14, 165, 233, 0.14)',
                  border: '1px solid rgba(14, 165, 233, 0.4)',
                  padding: '4px 10px',
                  borderRadius: '999px',
                  cursor: 'pointer',
                  display: 'flex',
                  alignItems: 'center',
                  gap: '5px'
                }}
                title="Abrir Guia Visual Interativo de Altura, Posição e Ângulo dos Dispositivos ESP32"
              >
                📐 Posição & Ângulo ESP32
              </button>
            </div>


            {editable && (
            <div style={{ display: 'flex', flexWrap: 'wrap', gap: '4px', alignItems: 'center', marginTop: '10px' }}>
              <label style={{ display: 'flex', alignItems: 'center', gap: '4px', fontSize: '0.65rem', color: 'var(--text-muted)', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '4px 6px', borderRadius: '6px' }}>
                <Layers size={13} />
                <span>{t('aetherMap.floor', 'Andar:')}</span>
                <input
                  type="number"
                  value={floorplanFloor}
                  onChange={(event) => setFloorplanFloor(parseInt(event.target.value, 10) || 0)}
                  style={{ width: '32px', background: 'transparent', border: 'none', color: 'var(--text-primary)', fontWeight: 'bold', outline: 'none', textAlign: 'center' }}
                  title={t('aetherMap.activeFloor', 'Andar ativo')}
                />
              </label>
              <button
                className="btn btn--secondary"
                type="button"
                onClick={() => floorplanInputRef.current?.click()}
                style={{ fontSize: '0.65rem', padding: '5px 8px', justifyContent: 'center' }}
                title={t('aetherMap.addFloorplanTitle', 'Adicionar planta baixa')}
              >
                <Upload size={13} /> {t('aetherMap.btnFloorplan', 'Planta')}
              </button>
              <button
                className="btn btn--secondary"
                type="button"
                onClick={() => {
                  setIsAddingRoomMode((current) => !current);
                  setIsAddingObjectMode(false);
                  setIsAddingWallMode(false);
                  setIsAddingMode(false);
                  setPendingFloorplan(null);
                }}
                style={{ fontSize: '0.65rem', padding: '5px 8px', justifyContent: 'center', background: isAddingRoomMode ? 'var(--accent-amber)' : 'var(--bg-card)', color: isAddingRoomMode ? 'white' : 'var(--text-secondary)' }}
                title={t('aetherMap.addRoomTitle', 'Adicionar área de cômodo funcional (3.5m x 3.5m)')}
              >
                <Home size={13} /> {t('aetherMap.btnRoom', 'Cômodo')}
              </button>
              <button
                className="btn btn--secondary"
                type="button"
                onClick={() => {
                  setIsAddingWallMode((current) => !current);
                  setIsAddingObjectMode(false);
                  setIsAddingRoomMode(false);
                  setIsAddingMode(false);
                  setPendingFloorplan(null);
                }}
                style={{ fontSize: '0.65rem', padding: '5px 8px', justifyContent: 'center', background: isAddingWallMode ? 'var(--accent-amber)' : 'var(--bg-card)', color: isAddingWallMode ? 'white' : 'var(--text-secondary)' }}
                title={t('aetherMap.addWallTitle', 'Adicionar parede ou divisória de atenuação (3.0m x 0.2m)')}
              >
                <Shield size={13} /> {t('aetherMap.btnWall', 'Parede')}
              </button>
              <button
                className="btn btn--secondary"
                type="button"
                onClick={() => {
                  setIsAddingObjectMode((current) => !current);
                  setIsAddingRoomMode(false);
                  setIsAddingWallMode(false);
                  setIsAddingMode(false);
                  setPendingFloorplan(null);
                }}
                style={{ fontSize: '0.65rem', padding: '5px 8px', justifyContent: 'center', background: isAddingObjectMode ? 'var(--accent-amber)' : 'var(--bg-card)', color: isAddingObjectMode ? 'white' : 'var(--text-secondary)' }}
                title={t('aetherMap.addObjectTitle', 'Adicionar móvel ou elemento')}
              >
                <Box size={13} /> {t('aetherMap.btnObject', 'Objeto')}
              </button>
              <button
                className="btn btn--secondary"
                type="button"
                onClick={() => void loadMapConfig()}
                style={{ fontSize: '0.65rem', padding: '5px 8px', justifyContent: 'center' }}
                title={t('aetherMap.reloadLayersTitle', 'Recarregar camadas')}
              >
                <ImageIcon size={13} /> {t('aetherMap.syncBtn', 'Sync')}
              </button>
            </div>
            )}

            <div style={{ display: 'flex', gap: '6px', flexWrap: 'wrap', alignItems: 'center', marginTop: '8px' }}>
              {editable && (
              <button
                className="btn btn--secondary"
                type="button"
                onClick={toggleEditLayout}
                style={{ fontSize: '0.65rem', padding: '5px 8px', justifyContent: 'center', background: editLayout ? 'var(--accent-amber)' : 'var(--bg-card)', color: editLayout ? 'white' : 'var(--text-secondary)' }}
                title={t('aetherMap.dragElementsTitle', 'Arrastar dispositivos, cômodos, paredes e a planta baixa sobre o mapa/satélite')}
              >
                <Move size={13} /> {editLayout ? t('aetherMap.editing', 'Editando…') : t('aetherMap.editLayout', 'Editar layout')}
              </button>
              )}
              <div style={{ display: 'flex', gap: '3px', alignItems: 'center' }} title={t('aetherMap.coordUnitTitle', 'Unidade de coordenadas')}>
                <Crosshair size={13} style={{ color: 'var(--text-muted)', marginRight: '2px' }} />
                {(['latlng', 'utm', 'polar'] as CoordUnit[]).map((unit) => (
                  <button
                    key={unit}
                    type="button"
                    onClick={() => setCoordUnit(unit)}
                    className="btn btn--secondary"
                    style={{ fontSize: '0.6rem', padding: '4px 6px', background: coordUnit === unit ? 'var(--accent-blue, #2563eb)' : 'var(--bg-card)', color: coordUnit === unit ? 'white' : 'var(--text-secondary)' }}
                  >
                    {COORD_UNIT_LABEL[unit]}
                  </button>
                ))}
              </div>
            </div>
            {cursorCoord && (
              <p style={{ fontSize: '0.62rem', color: 'var(--text-muted)', margin: '6px 0 0', fontFamily: 'monospace' }}>
                📍 {cursorCoord}
              </p>
            )}

            {editable && activeFloorplan && (
              <div style={{ display: 'flex', flexWrap: 'wrap', gap: '8px', alignItems: 'center', marginTop: '8px' }}>
                <label style={{ display: 'flex', alignItems: 'center', gap: '8px', minWidth: 0, fontSize: '0.65rem', color: 'var(--text-muted)', flex: 1 }}>
                  {t('aetherMap.opacity', 'Opacidade')}
                  <input
                    type="range"
                    min="0.1"
                    max="1"
                    step="0.05"
                    value={activeFloorplan.opacity}
                    onChange={(event) => updateFloorplanOpacity(activeFloorplan, parseFloat(event.target.value))}
                    style={{ minWidth: '60px', flex: 1 }}
                  />
                </label>
                <button
                  type="button"
                  onClick={() => {
                    void (async () => {
                      try {
                        await deleteFloorplan(activeFloorplan.id);
                        setMapFeedback(t('aetherMap.feedback.floorplanRemoved', 'planta removida'));
                      } catch (error) {
                        console.error('Falha ao remover planta', error);
                        setMapFeedback(t('aetherMap.feedback.removeFloorplanFailed', 'falha ao remover planta'));
                      }
                    })();
                  }}
                  style={{ width: '26px', height: '26px', borderRadius: '6px', border: '1px solid var(--border)', background: 'var(--bg-surface)', color: 'var(--text-muted)', display: 'flex', alignItems: 'center', justifyContent: 'center', cursor: 'pointer' }}
                  title={t('aetherMap.removeActiveFloorplan', 'Remover planta ativa')}
                >
                  <Trash2 size={13} />
                </button>
              </div>
            )}

            {(pendingFloorplan || mapFeedback || isAddingObjectMode || isAddingRoomMode || isAddingWallMode) && (
              <p style={{ margin: '8px 0 0', fontSize: '0.65rem', color: 'var(--text-muted)' }}>
                {pendingFloorplan?.firstCorner
                  ? t('aetherMap.clickOpposite', 'Clique no canto oposto da planta.')
                  : pendingFloorplan
                    ? t('aetherMap.clickFirst', 'Clique no primeiro canto da planta.')
                    : isAddingRoomMode
                      ? t('aetherMap.clickRoom', 'Clique no mapa para posicionar a área do cômodo (3.5m x 3.5m).')
                      : isAddingWallMode
                        ? t('aetherMap.clickWall', 'Clique no mapa para posicionar a parede de atenuação (3.0m x 0.2m).')
                        : isAddingObjectMode
                          ? t('aetherMap.clickObject', 'Clique no mapa para posicionar o elemento.')
                          : mapFeedback}
              </p>
            )}

            {editable && (
              <input
                ref={floorplanInputRef}
                type="file"
                accept="image/png,image/jpeg,image/svg+xml,image/webp"
                onChange={handleFloorplanFileChange}
                style={{ display: 'none' }}
              />
            )}
          </>
        )}
      </div>

      <div
        className="map-status-badge"
        style={{
          position: 'absolute',
          top: '16px',
          right: '16px',
          zIndex: 1000,
        background: isLive || isReplay ? (visibleOccupiedRoom ? 'var(--accent-emerald)' : 'var(--accent-amber)') : 'var(--text-muted)',
        color: 'white',
        padding: '6px 12px',
        borderRadius: 'var(--radius-sm)',
        fontSize: '0.7rem',
        fontWeight: 'bold',
        display: 'flex',
        alignItems: 'center',
        gap: '6px',
        boxShadow: 'var(--shadow-sm)'
      }}>
        <MapPin size={12} />
        {isLive
          ? (visibleOccupiedRoom ? t('aetherMap.occupied', 'OCUPADO: {{room}}', { room: visibleOccupiedRoom.toUpperCase() }) : t('aetherMap.freeRoom', 'AMBIENTE LIVRE'))
          : isReplay
            ? t('aetherMap.replayCsv', 'REPLAY CSV')
            : t('aetherMap.noCsi', 'SEM CSI')}
      </div>

      <div ref={mapContainerRef} style={{ width: '100%', height: '100%', minHeight: isClientView ? '100%' : '500px', cursor: isAddingMode || isAddingObjectMode || isAddingRoomMode || isAddingWallMode || isFloorplanPlacementActive ? 'crosshair' : 'default' }} />

      {/* Legenda/affordance do mapa-game: só aparece quando o ML está
          desenhando avatares de pessoas — explica o que são os avatares e
          que dá para ver os sinais vitais (passar o mouse ou tocar). Fica
          acima do controle de zoom (bottomleft do Leaflet). */}
      {mlPersonCount > 0 && (
        <div className="map-legend" aria-hidden="true">
          <div className="map-legend__sample">
            <span className="gp-avatar map-legend__avatar" style={{ ['--hue' as string]: '265' }}>
              <span className="gp-badge">1</span>
              <span className="gp-core" />
            </span>
          </div>
          <div className="map-legend__text">
            <strong>{t('aetherMap.legendTitle', 'Avatares = pessoas detectadas pelo ML')}</strong>
            <span>{t('aetherMap.legendBadge', 'Badge P1, P2… é o índice · a cor identifica cada pessoa.')}</span>
            <span className="map-legend__hint">{t('aetherMap.legendHint', 'Passe o mouse ou toque para ver os sinais vitais (batimentos e respiração).')}</span>
          </div>
        </div>
      )}

      {/* Buttons & Modals */}
      {editable && (
      <button
        onClick={() => {
          setIsAddingMode((current) => !current);
          setIsAddingObjectMode(false);
          setIsAddingRoomMode(false);
          setIsAddingWallMode(false);
          setPendingFloorplan(null);
        }}
        style={{
          position: 'absolute', bottom: '24px', left: '50%', transform: 'translateX(-50%)', zIndex: 1000,
          background: isAddingMode ? 'var(--accent-amber)' : 'var(--accent-indigo)',
          color: 'white', border: 'none', borderRadius: '50%', width: '48px', height: '48px',
          display: 'flex', alignItems: 'center', justifyContent: 'center', cursor: 'pointer',
          boxShadow: 'var(--shadow-lg)', transition: 'all 0.2s'
        }}
        title={isAddingMode ? t('common.cancel', 'Cancelar') : t('aetherMap.addDevice', 'Adicionar Dispositivo')}
      >
        {isAddingMode ? <X size={24} /> : <Plus size={24} />}
      </button>
      )}

      {isAddingMode && (
        <div style={{
          position: 'absolute', bottom: '80px', left: '50%', transform: 'translateX(-50%)', zIndex: 1000,
          background: 'var(--bg-card)', padding: '8px 12px', borderRadius: 'var(--radius-sm)',
          fontSize: '0.7rem', color: 'var(--text-primary)', border: '1px solid var(--border)',
          boxShadow: 'var(--shadow-md)', pointerEvents: 'none'
        }}>
          {t('aetherMap.clickToPlace', 'Clique no mapa para posicionar.')}
        </div>
      )}

      {editingDevice && (
        <div style={{
          position: 'fixed', inset: 0, zIndex: 99998, background: 'rgba(0,0,0,0.7)',
          display: 'flex', alignItems: 'center', justifyContent: 'center', backdropFilter: 'blur(6px)', padding: '16px', boxSizing: 'border-box'
        }}>
          <div style={{
            background: 'var(--bg-card)', padding: '20px', borderRadius: 'var(--radius-xl)',
            width: '420px', maxWidth: '100%', border: '1px solid var(--border)', boxShadow: '0 25px 50px -12px rgba(0, 0, 0, 0.85)',
            display: 'flex', flexDirection: 'column', gap: '12px',
            maxHeight: 'calc(100vh - 32px)', overflowY: 'auto'
          }}>
            <h3 style={{ margin: 0, fontSize: '1.05rem', color: 'var(--text-primary)' }}>{t('aetherMap.editDevice', 'Editar Dispositivo')}</h3>

            <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>
              {t('aetherMap.idLabel', 'ID:')}
              <input
                type="text"
                value={editingDevice.id || ''}
                onChange={e => setEditingDevice({ ...editingDevice, id: e.target.value })}
                style={{ width: '100%', marginTop: '4px', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '6px', borderRadius: '4px', color: 'var(--text-primary)' }}
              />
            </label>
            <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>
              {t('aetherMap.labelName', 'Nome:')}
              <input
                type="text"
                value={editingDevice.name || ''}
                onChange={e => setEditingDevice({ ...editingDevice, name: e.target.value })}
                style={{ width: '100%', marginTop: '4px', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '6px', borderRadius: '4px', color: 'var(--text-primary)' }}
              />
            </label>
            <div style={{ display: 'flex', gap: '8px' }}>
              <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)', flex: 1 }}>
                {t('aetherMap.lat', 'Latitude:')}
                <input
                  type="number"
                  step="0.0000001"
                  value={editingDevice.lat || 0}
                  onChange={e => setEditingDevice({ ...editingDevice, lat: parseFloat(e.target.value) })}
                  style={{ width: '100%', marginTop: '4px', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '6px', borderRadius: '4px', color: 'var(--text-primary)' }}
                />
              </label>
              <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)', flex: 1 }}>
                {t('aetherMap.lng', 'Longitude:')}
                <input
                  type="number"
                  step="0.0000001"
                  value={editingDevice.lng || 0}
                  onChange={e => setEditingDevice({ ...editingDevice, lng: parseFloat(e.target.value) })}
                  style={{ width: '100%', marginTop: '4px', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '6px', borderRadius: '4px', color: 'var(--text-primary)' }}
                />
              </label>
            </div>
            <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>
              {t('aetherMap.labelIcon', 'Ícone:')}
              <select
                value={editingDevice.icon || '/esp32-real.svg'}
                onChange={e => setEditingDevice({ ...editingDevice, icon: e.target.value })}
                style={{ width: '100%', marginTop: '4px', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '6px', borderRadius: '4px', color: 'var(--text-primary)' }}
              >
                <option value="/esp32-real.svg">ESP32</option>
                <option value="/router-real.svg">{t('aetherMap.router', 'Roteador')}</option>
              </select>
            </label>
            <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>
              {t('aetherMap.labelRole', 'Papel:')}
              <select
                value={editingDevice.role || 'receiver'}
                onChange={e => setEditingDevice({ ...editingDevice, role: e.target.value })}
                style={{ width: '100%', marginTop: '4px', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '6px', borderRadius: '4px', color: 'var(--text-primary)' }}
              >
                <option value="receiver">{t('aetherMap.roleReceiver', 'Receptor CSI')}</option>
                <option value="router">{t('aetherMap.roleRouter', 'Roteador/Injetor')}</option>
                <option value="injector">{t('aetherMap.roleInjector', 'Injetor dedicado')}</option>
              </select>
            </label>
            <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>
              {t('aetherMap.hwLabel', 'Hardware:')}
              <select
                value={editingDevice.hardware_profile || 'wroom'}
                onChange={e => setEditingDevice({ ...editingDevice, hardware_profile: e.target.value })}
                style={{ width: '100%', marginTop: '4px', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '6px', borderRadius: '4px', color: 'var(--text-primary)' }}
              >
                <option value="esp32c5">ESP32-C5</option>
                <option value="wroom">ESP32-WROOM</option>
                <option value="classic">ESP32 Classic</option>
                <option value="router">{t('aetherMap.router', 'Roteador')}</option>
              </select>
            </label>
            <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>
              {t('aetherMap.labelSize', 'Tamanho (px):')}
              <input
                type="number"
                value={editingDevice.size || 32}
                onChange={e => setEditingDevice({ ...editingDevice, size: parseInt(e.target.value, 10) })}
                style={{ width: '100%', marginTop: '4px', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '6px', borderRadius: '4px', color: 'var(--text-primary)' }}
              />
            </label>
            <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>
              {t('aetherMap.labelFloor', 'Andar:')}
              <input
                type="number"
                value={editingDevice.floor || 1}
                onChange={e => setEditingDevice({ ...editingDevice, floor: parseInt(e.target.value, 10) })}
                style={{ width: '100%', marginTop: '4px', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '6px', borderRadius: '4px', color: 'var(--text-primary)' }}
              />
            </label>

            <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>
              📐 Ângulo de Visada LoS (° Azimute):
              <input
                type="number"
                min={0}
                max={360}
                placeholder="Ex: 45° (automático se vazio)"
                value={(editingDevice as any).orientation_deg || ''}
                onChange={e => setEditingDevice({ ...editingDevice, orientation_deg: e.target.value ? parseInt(e.target.value, 10) : undefined } as any)}
                style={{ width: '100%', marginTop: '4px', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '6px', borderRadius: '4px', color: 'var(--text-primary)' }}
              />
            </label>

            {/* Diagrama Gráfico Visual Interativo de Posição & Ângulo */}
            <div style={{
              background: 'rgba(14, 165, 233, 0.08)',
              border: '1px solid rgba(14, 165, 233, 0.28)',
              borderRadius: '8px',
              padding: '12px',
              display: 'flex',
              flexDirection: 'column',
              gap: '8px'
            }}>
              <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
                <strong style={{ color: '#38bdf8', fontSize: '0.78rem' }}>
                  📐 Esquema Visual de Instalação ESP32
                </strong>
                <span style={{ fontSize: '0.65rem', color: '#10b981', fontWeight: 'bold' }}>Polarização Vertical ↕</span>
              </div>

              {/* SVG Ilustrativo da Posição Física na Parede */}
              <svg viewBox="0 0 240 110" style={{ width: '100%', height: 'auto', background: 'rgba(0,0,0,0.25)', borderRadius: '6px' }}>
                {/* Linha do Chão */}
                <line x1="10" y1="95" x2="230" y2="95" stroke="#475569" strokeWidth="2" />
                <text x="15" y="105" fill="#94a3b8" fontSize="8">Chão (0.0m)</text>

                {/* Parede e ESP32 na altura ideal 1.2m */}
                <rect x="25" y="25" width="8" height="70" fill="#334155" />
                <rect x="29" y="42" width="12" height="20" rx="2" fill="#0ea5e9" stroke="#38bdf8" strokeWidth="1.5" />
                <line x1="35" y1="35" x2="35" y2="42" stroke="#38bdf8" strokeWidth="2" />
                <text x="45" y="48" fill="#38bdf8" fontSize="8" fontWeight="bold">ESP32 (1.0m–1.4m)</text>
                <text x="45" y="58" fill="#e2e8f0" fontSize="7">Antena Vertical ↕</text>

                {/* Ondas CSI / Feixe LoS propagando no tórax da pessoa */}
                <path d="M 45 52 Q 90 40 140 52 T 220 52" fill="none" stroke="#10b981" strokeWidth="1.5" strokeDasharray="4,3" />
                <path d="M 45 52 Q 90 64 140 52 T 220 52" fill="none" stroke="#10b981" strokeWidth="1.5" strokeDasharray="4,3" />

                {/* Silhueta da Pessoa (Altura do tórax interceptando as ondas) */}
                <circle cx="155" cy="35" r="6" fill="#f59e0b" />
                <line x1="155" y1="41" x2="155" y2="70" stroke="#f59e0b" strokeWidth="3.5" />
                <line x1="155" y1="70" x2="148" y2="95" stroke="#f59e0b" strokeWidth="2.5" />
                <line x1="155" y1="70" x2="162" y2="95" stroke="#f59e0b" strokeWidth="2.5" />
                <text x="140" y="25" fill="#f59e0b" fontSize="8" fontWeight="bold">Alvo (Tórax/CSI)</text>
              </svg>

              <div style={{ fontSize: '0.72rem', color: 'var(--text-secondary)', lineHeight: '1.4' }}>
                • <strong>Posição Física:</strong> Monte entre <strong>1,0m e 1,4m do chão</strong> para cruzar o tórax/abdômen.<br />
                • <strong>Orientação/Ângulo:</strong> Aponte a face da antena diretamente para a área central monitorada (Linha de Visada direta LoS).
              </div>
            </div>

            <div style={{ display: 'flex', gap: '8px', marginTop: '12px' }}>
              <button
                onClick={() => setEditingDevice(null)}
                style={{ flex: 1, padding: '8px', background: 'var(--bg-surface)', color: 'var(--text-primary)', border: '1px solid var(--border)', borderRadius: '4px', cursor: 'pointer' }}
              >
                {t('common.cancel', 'Cancelar')}
              </button>
              <button
                onClick={async () => {
                  await apiFetch(`${apiBaseUrl}/api/devices`, {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify(editingDevice)
                  });
                  setEditingDevice(null);
                }}
                style={{ flex: 1, padding: '8px', background: 'var(--accent-indigo)', color: 'white', border: 'none', borderRadius: '4px', cursor: 'pointer' }}
              >
                {t('common.save', 'Salvar')}
              </button>
            </div>
          </div>
        </div>
      )}

      {editingObject && (
        <div style={{
          position: 'absolute', inset: 0, zIndex: 2000, background: 'rgba(0,0,0,0.5)',
          display: 'flex', alignItems: 'center', justifyContent: 'center', backdropFilter: 'blur(4px)'
        }}>
          <div style={{
            background: 'var(--bg-card)', padding: '24px', borderRadius: 'var(--radius-lg)',
            width: '340px', border: '1px solid var(--border)', boxShadow: 'var(--shadow-xl)',
            display: 'flex', flexDirection: 'column', gap: '12px',
            maxHeight: '90vh', overflowY: 'auto'
          }}>
            <h3 style={{ margin: 0, fontSize: '1rem', color: 'var(--text-primary)' }}>{t('aetherMap.editElement', 'Editar Elemento')}</h3>

            <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>
              {t('aetherMap.labelName', 'Nome:')}
              <input
                type="text"
                value={editingObject.name || ''}
                onChange={event => setEditingObject({ ...editingObject, name: event.target.value })}
                style={{ width: '100%', marginTop: '4px', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '6px', borderRadius: '4px', color: 'var(--text-primary)' }}
              />
            </label>
            <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>
              {t('aetherMap.labelType', 'Tipo:')}
              <select
                value={editingObject.type || 'furniture'}
                onChange={event => setEditingObject({ ...editingObject, type: event.target.value })}
                style={{ width: '100%', marginTop: '4px', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '6px', borderRadius: '4px', color: 'var(--text-primary)' }}
              >
                <option value="room">{t('aetherMap.typeRoom', 'Cômodo (define o "cômodo atual")')}</option>
                <option value="wall">{t('aetherMap.typeWall', 'Parede (atenua o sinal na localização)')}</option>
                <option value="furniture">{t('aetherMap.typeFurniture', 'Móvel')}</option>
                <option value="desk">{t('aetherMap.typeDesk', 'Mesa')}</option>
                <option value="chair">{t('aetherMap.typeChair', 'Cadeira')}</option>
                <option value="shelf">{t('aetherMap.typeShelf', 'Armário/Estante')}</option>
                <option value="equipment">{t('aetherMap.typeEquipment', 'Equipamento')}</option>
              </select>
            </label>
            {editingObject.type === 'room' && (
              <div style={{ fontSize: '0.72rem', color: '#0ea5e9', background: 'rgba(14, 165, 233, 0.1)', padding: '6px 8px', borderRadius: '4px', borderLeft: '3px solid #0ea5e9', marginTop: '4px' }}>
                <b>{t('aetherMap.roomAreaTitle', 'Área Funcional de Localização:')}</b> {t('aetherMap.roomAreaDesc', 'Polígono utilizado pelo backend em Rust para geofencing e teste Point-in-Polygon, definindo o cômodo atual via MQTT e no Home Assistant.')}
              </div>
            )}
            {editingObject.type === 'wall' && (
              <div style={{ fontSize: '0.72rem', color: '#64748b', background: 'rgba(100, 116, 139, 0.1)', padding: '6px 8px', borderRadius: '4px', borderLeft: '3px solid #64748b', marginTop: '4px' }}>
                <b>{t('aetherMap.wallObstacleTitle', 'Obstáculo de Atenuação:')}</b> {t('aetherMap.wallObstacleDesc', 'Linha utilizada pelo ray-casting no cálculo de localização. Intercepta a linha de visada adicionando penalidade dB no modelo log-distância.')}
              </div>
            )}
            <div style={{ display: 'flex', gap: '8px' }}>
              <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)', flex: 1 }}>
                {t('aetherMap.lat', 'Latitude:')}
                <input
                  type="number"
                  step="0.0000001"
                  value={editingObject.lat || 0}
                  onChange={event => setEditingObject({ ...editingObject, lat: parseFloat(event.target.value) })}
                  style={{ width: '100%', marginTop: '4px', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '6px', borderRadius: '4px', color: 'var(--text-primary)' }}
                />
              </label>
              <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)', flex: 1 }}>
                {t('aetherMap.lng', 'Longitude:')}
                <input
                  type="number"
                  step="0.0000001"
                  value={editingObject.lng || 0}
                  onChange={event => setEditingObject({ ...editingObject, lng: parseFloat(event.target.value) })}
                  style={{ width: '100%', marginTop: '4px', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '6px', borderRadius: '4px', color: 'var(--text-primary)' }}
                />
              </label>
            </div>
            <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: '8px' }}>
              <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>
                {t('aetherMap.widthM', 'Largura (m):')}
                <input
                  type="number"
                  min="0.1"
                  step="0.1"
                  value={editingObject.width_m || 1}
                  onChange={event => setEditingObject({ ...editingObject, width_m: parseFloat(event.target.value) })}
                  style={{ width: '100%', marginTop: '4px', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '6px', borderRadius: '4px', color: 'var(--text-primary)' }}
                />
              </label>
              <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>
                {t('aetherMap.depthM', 'Prof. (m):')}
                <input
                  type="number"
                  min="0.1"
                  step="0.1"
                  value={editingObject.depth_m || 0.6}
                  onChange={event => setEditingObject({ ...editingObject, depth_m: parseFloat(event.target.value) })}
                  style={{ width: '100%', marginTop: '4px', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '6px', borderRadius: '4px', color: 'var(--text-primary)' }}
                />
              </label>
              <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>
                {t('aetherMap.rotDeg', 'Rot.:')}
                <input
                  type="number"
                  step="5"
                  value={editingObject.rotation_deg || 0}
                  onChange={event => setEditingObject({ ...editingObject, rotation_deg: parseFloat(event.target.value) })}
                  style={{ width: '100%', marginTop: '4px', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '6px', borderRadius: '4px', color: 'var(--text-primary)' }}
                />
              </label>
            </div>
            <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>
              {t('aetherMap.labelFloor', 'Andar:')}
              <input
                type="number"
                value={editingObject.floor ?? floorplanFloor}
                onChange={event => setEditingObject({ ...editingObject, floor: parseInt(event.target.value, 10) })}
                style={{ width: '100%', marginTop: '4px', background: 'var(--bg-surface)', border: '1px solid var(--border)', padding: '6px', borderRadius: '4px', color: 'var(--text-primary)' }}
              />
            </label>
            <label style={{ display: 'flex', alignItems: 'center', gap: '8px', fontSize: '0.8rem', color: 'var(--text-secondary)' }}>
              <input
                type="checkbox"
                checked={editingObject.calibration_relevant ?? true}
                onChange={event => setEditingObject({ ...editingObject, calibration_relevant: event.target.checked })}
              />
              {t('aetherMap.baselineCheckbox', 'Participa do baseline de sala vazia')}
            </label>

            <div style={{ display: 'flex', gap: '8px', marginTop: '12px' }}>
              {editingObject.id && mapConfig.environment_objects.some((object) => object.id === editingObject.id) && (
                <button
                  onClick={() => {
                    void (async () => {
                      try {
                        await deleteEnvironmentObject(editingObject.id as string);
                        setEditingObject(null);
                        setMapFeedback(t('aetherMap.feedback.elementRemoved', 'elemento removido'));
                      } catch (error) {
                        console.error('Falha ao remover elemento', error);
                        setMapFeedback(t('aetherMap.feedback.removeElementFailed', 'falha ao remover elemento'));
                      }
                    })();
                  }}
                  style={{ padding: '8px', background: 'var(--bg-surface)', color: 'var(--accent-rose)', border: '1px solid var(--border)', borderRadius: '4px', cursor: 'pointer' }}
                  title={t('aetherMap.removeElementTitle', 'Remover elemento')}
                >
                  <Trash2 size={15} />
                </button>
              )}
              <button
                onClick={() => setEditingObject(null)}
                style={{ flex: 1, padding: '8px', background: 'var(--bg-surface)', color: 'var(--text-primary)', border: '1px solid var(--border)', borderRadius: '4px', cursor: 'pointer' }}
              >
                {t('common.cancel', 'Cancelar')}
              </button>
              <button
                onClick={() => {
                  const objectToSave: EnvironmentObjectConfig = {
                    id: editingObject.id || buildClientId('obj'),
                    name: editingObject.name || t('aetherMap.defaultElementShortName', 'Elemento'),
                    type: editingObject.type || 'furniture',
                    lat: Number(editingObject.lat ?? 0),
                    lng: Number(editingObject.lng ?? 0),
                    floor: Number(editingObject.floor ?? floorplanFloor),
                    width_m: Number(editingObject.width_m ?? 1),
                    depth_m: Number(editingObject.depth_m ?? 0.6),
                    rotation_deg: Number(editingObject.rotation_deg ?? 0),
                    calibration_relevant: editingObject.calibration_relevant ?? true,
                  };

                  void (async () => {
                    try {
                      await saveEnvironmentObject(objectToSave);
                      setEditingObject(null);
                      setMapFeedback(t('aetherMap.feedback.elementSaved', 'elemento salvo'));
                    } catch (error) {
                      console.error('Falha ao salvar elemento', error);
                      setMapFeedback(t('aetherMap.feedback.saveElementFailed', 'falha ao salvar elemento'));
                    }
                  })();
                }}
                style={{ flex: 1, padding: '8px', background: 'var(--accent-indigo)', color: 'white', border: 'none', borderRadius: '4px', cursor: 'pointer' }}
              >
                {t('common.save', 'Salvar')}
              </button>
            </div>
          </div>
        </div>
      )}

      {showEspVisualGuide && (
        <div style={{
          position: 'fixed', inset: 0, zIndex: 99999, background: 'rgba(0,0,0,0.78)',
          display: 'flex', alignItems: 'center', justifyContent: 'center', backdropFilter: 'blur(8px)', padding: '16px', boxSizing: 'border-box'
        }}>
          <div style={{
            background: 'var(--bg-card)', borderRadius: 'var(--radius-xl)',
            width: '620px', maxWidth: '100%', maxHeight: 'calc(100vh - 32px)', border: '1px solid var(--border)', boxShadow: '0 25px 50px -12px rgba(0, 0, 0, 0.85)',
            display: 'flex', flexDirection: 'column', overflow: 'hidden'
          }}>
            {/* Header Fixo */}
            <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', borderBottom: '1px solid var(--border)', padding: '14px 20px', flexShrink: 0, background: 'var(--bg-surface)' }}>
              <div>
                <h3 style={{ margin: 0, fontSize: '1.1rem', color: 'var(--text-primary)', display: 'flex', alignItems: 'center', gap: '8px' }}>
                  <span>📐 Guia Visual Interativo: Posição & Ângulo ESP32</span>
                </h3>
                <p style={{ margin: '3px 0 0', fontSize: '0.75rem', color: 'var(--text-secondary)' }}>
                  Como posicionar fisicamente e orientar visualmente seus módulos ESP32 no ambiente
                </p>
              </div>
              <button
                onClick={() => setShowEspVisualGuide(false)}
                style={{ background: 'transparent', border: 'none', color: 'var(--text-secondary)', cursor: 'pointer', fontSize: '1.25rem', padding: '4px', lineHeight: 1 }}
                title="Fechar"
              >
                ✕
              </button>
            </div>

            {/* Conteúdo Rolável (Zero Cortes) */}
            <div style={{ padding: '16px 20px', overflowY: 'auto', display: 'flex', flexDirection: 'column', gap: '14px', flex: '1 1 auto' }}>
              {/* Seção 1: Altura e Polarização da Antena (Vista Lateral) */}
              <div style={{ background: 'var(--bg-surface)', padding: '12px 14px', borderRadius: '10px', border: '1px solid var(--border)' }}>
                <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: '8px' }}>
                  <strong style={{ color: '#38bdf8', fontSize: '0.82rem' }}>1. Altura na Parede & Polarização Vertical (Vista Lateral)</strong>
                  <span style={{ fontSize: '0.68rem', background: 'rgba(56,189,248,0.15)', color: '#38bdf8', padding: '2px 8px', borderRadius: '4px', fontWeight: 'bold' }}>↕ 1.0m – 1.4m</span>
                </div>

                <svg viewBox="0 0 400 120" style={{ width: '100%', maxHeight: '130px', height: 'auto', background: 'rgba(0,0,0,0.3)', borderRadius: '8px' }}>
                  {/* Chão e Teto */}
                  <line x1="20" y1="105" x2="380" y2="105" stroke="#475569" strokeWidth="2" />
                  <text x="25" y="116" fill="#94a3b8" fontSize="9">Piso (0.0m)</text>

                  {/* Parede Esquerda + ESP32 TX */}
                  <rect x="40" y="15" width="10" height="90" fill="#334155" />
                  <rect x="46" y="40" width="14" height="24" rx="2" fill="#0ea5e9" stroke="#38bdf8" strokeWidth="1.5" />
                  <text x="66" y="50" fill="#38bdf8" fontSize="9" fontWeight="bold">ESP32 TX (1.2m)</text>
                  <text x="66" y="61" fill="#e2e8f0" fontSize="8">Antena Vertical ↕</text>

                  {/* Parede Direita + ESP32 RX */}
                  <rect x="350" y="15" width="10" height="90" fill="#334155" />
                  <rect x="340" y="40" width="14" height="24" rx="2" fill="#10b981" stroke="#34d399" strokeWidth="1.5" />
                  <text x="270" y="50" fill="#34d399" fontSize="9" fontWeight="bold">ESP32 RX (1.2m)</text>

                  {/* Linha de feixe CSI cruzando o tronco */}
                  <path d="M 60 52 Q 200 37 340 52" fill="none" stroke="#10b981" strokeWidth="2" strokeDasharray="5,4" />
                  <path d="M 60 52 Q 200 67 340 52" fill="none" stroke="#10b981" strokeWidth="2" strokeDasharray="5,4" />

                  {/* Silhueta da Pessoa (Centro da Sala) */}
                  <circle cx="200" cy="30" r="8" fill="#f59e0b" />
                  <line x1="200" y1="38" x2="200" y2="73" stroke="#f59e0b" strokeWidth="4.5" />
                  <line x1="200" y1="73" x2="191" y2="105" stroke="#f59e0b" strokeWidth="3" />
                  <line x1="200" y1="73" x2="209" y2="105" stroke="#f59e0b" strokeWidth="3" />
                  <text x="178" y="20" fill="#f59e0b" fontSize="9" fontWeight="bold">Zona de Corte CSI</text>
                </svg>

                <p style={{ margin: '8px 0 0', fontSize: '0.74rem', color: 'var(--text-secondary)', lineHeight: '1.4' }}>
                  • Monte a placa na <strong>vertical (antena apontando para cima)</strong> entre <strong>1,0m e 1,4m do chão</strong>. Isso alinha a polarização das antenas e garante que as subportadoras CSI cruzem diretamente o tronco da pessoa em pé ou andando.
                </p>
              </div>

              {/* Seção 2: Ângulo de Visada e Cruzamento na Sala (Vista Superior) */}
              <div style={{ background: 'var(--bg-surface)', padding: '12px 14px', borderRadius: '10px', border: '1px solid var(--border)' }}>
                <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: '8px' }}>
                  <strong style={{ color: '#10b981', fontSize: '0.82rem' }}>2. Ângulo de Visada LoS Cruzado (Planta Baixa)</strong>
                  <span style={{ fontSize: '0.68rem', background: 'rgba(16,185,129,0.15)', color: '#10b981', padding: '2px 8px', borderRadius: '4px', fontWeight: 'bold' }}>↻ Orientação Diagonal</span>
                </div>

                <svg viewBox="0 0 400 115" style={{ width: '100%', maxHeight: '120px', height: 'auto', background: 'rgba(0,0,0,0.3)', borderRadius: '8px' }}>
                  {/* Contorno da Sala */}
                  <rect x="50" y="8" width="300" height="98" fill="none" stroke="#475569" strokeWidth="2" strokeDasharray="6,4" />

                  {/* ESP32 TX Canto Superior Esquerdo */}
                  <circle cx="70" cy="25" r="7" fill="#0ea5e9" />
                  <text x="85" y="23" fill="#38bdf8" fontSize="9" fontWeight="bold">ESP32 TX (45° Azimute)</text>

                  {/* ESP32 RX Canto Inferior Direito */}
                  <circle cx="330" cy="88" r="7" fill="#10b981" />
                  <text x="205" y="101" fill="#34d399" fontSize="9" fontWeight="bold">ESP32 RX (225° Azimute)</text>

                  {/* Cone/Setor de Cobertura */}
                  <polygon points="70,25 330,55 330,88" fill="#0ea5e9" fillOpacity="0.15" />

                  {/* Linha LoS Principal */}
                  <line x1="70" y1="25" x2="330" y2="88" stroke="#10b981" strokeWidth="2.5" strokeDasharray="6,4" />

                  {/* Alvo em movimento cortando a linha */}
                  <circle cx="200" cy="56" r="7" fill="#f59e0b" />
                  <text x="180" y="46" fill="#f59e0b" fontSize="9" fontWeight="bold">Caminho da Pessoa</text>
                </svg>

                <p style={{ margin: '8px 0 0', fontSize: '0.74rem', color: 'var(--text-secondary)', lineHeight: '1.4' }}>
                  • Posicione os módulos em <strong>cantos opostos / diagonais</strong> para maximizar a cobertura. No mapa do AetherSense, os <strong>setores cônicos azuis</strong> representam o campo de visão RF e as <strong>linhas tracejadas verdes</strong> mostram os feixes diretos (LoS).
                </p>
              </div>

              {/* Seção 3: Boas Práticas contra Interferência */}
              <div style={{ background: 'rgba(245,158,11,0.08)', border: '1px solid rgba(245,158,11,0.3)', borderRadius: '8px', padding: '12px', fontSize: '0.74rem', color: 'var(--text-secondary)', lineHeight: '1.4' }}>
                <strong style={{ color: '#f59e0b', display: 'block', marginBottom: '4px' }}>⚡ Dicas de Instalação Física</strong>
                • <strong>Afaste de metais:</strong> Mantenha os ESP32 pelo menos <strong>30 cm longe de armários metálicos, pilares de concreto armado ou geladeiras</strong> para evitar reflexão multipéssima estática.<br />
                • <strong>Ajuste no Mapa:</strong> Ao editar um dispositivo no mapa, você pode visualizar e orientar o ângulo do setor cônico para refletir com precisão a instalação real.
              </div>
            </div>

            {/* Footer Fixo */}
            <div style={{ borderTop: '1px solid var(--border)', padding: '12px 20px', display: 'flex', justifyContent: 'flex-end', flexShrink: 0, background: 'var(--bg-surface)' }}>
              <button
                type="button"
                onClick={() => setShowEspVisualGuide(false)}
                style={{ padding: '9px 18px', background: 'var(--accent-indigo)', color: 'white', border: 'none', borderRadius: '6px', fontWeight: 'bold', cursor: 'pointer', fontSize: '0.82rem' }}
              >
                Entendido — Visualizar no Mapa
              </button>
            </div>
          </div>
        </div>
      )}
    </section>
  );
}
