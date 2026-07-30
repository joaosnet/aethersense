# AetherSense Frontend

Dashboard React + Vite + TypeScript para visualizar CSI em tempo real, presença, processamento de sinais e localização georreferenciada.

## Estado Atual

O frontend está alinhado ao contrato C5 + WROOM do backend Rust. Ele consome snapshots via WebSocket em `/ws` e publica ajustes via API REST em `/api`.

Campos de localização suportados:

- `lat`, `lng`, `floor`;
- `accuracy_m` e `accuracy_dm`;
- `confidence`;
- `nodes_used`;
- `method`;
- `experimental`;
- `resolution_m`.

Campos de dispositivo suportados:

- `id`, `name`, `lat`, `lng`, `floor`;
- `role`: `receiver`, `router` ou `injector`;
- `hardware_profile`: `esp32c5`, `wroom`, `classic` ou `router`;
- `icon` e `size`.

## Mapa e Topologia

O componente `AetherMap` renderiza:

- ESP32-C5 e ESP32-WROOM como âncoras georreferenciadas;
- marcador de pessoa pela estimativa fusionada;
- confiança, precisão e número de nós usados no marcador;
- status `2 nós ativos`, `fallback C5`, `fallback WROOM`, `sem baseline` ou `calibrando`;
- edição de papel/perfil do dispositivo e persistência em `POST /api/devices`.

O componente `HardwareTopology` mostra:

- contagem de receptores CSI;
- quantidade de C5 e WROOM;
- precisão atual;
- confiança;
- resolução da coordenada;
- método de localização;
- botão **Salvar baseline sala vazia**, que chama `POST /api/location/baseline`.

## Verificação

O frontend inclui testes automatizados e pode ser verificado com:

```powershell
npm ci
npm run build
```

Portas seriais, endereços MAC, coordenadas e baselines são dados locais do
usuário e não fazem parte da distribuição pública.

## Como Rodar

```powershell
npm install
npm run dev
```

Em desenvolvimento, o Vite expõe a UI em `http://localhost:3000` e usa proxy para o backend:

- `/api` -> `http://backend:8000`;
- `/ws` -> `ws://backend:8000`.

Build de produção:

```powershell
npm run build
```

Aviso atual: o build pode emitir warning de chunk JavaScript maior que 500 kB por causa de Plotly/Leaflet. Isso não bloqueia a validação, mas pode ser otimizado futuramente com code splitting.
