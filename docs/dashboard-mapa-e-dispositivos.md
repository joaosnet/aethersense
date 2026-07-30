# Dashboard, Mapa e Gerenciamento de Dispositivos

O AetherSense possui um dashboard web moderno construído com React e Leaflet, projetado para visualizar o monitoramento de pessoas em tempo real através de uma planta baixa (mapa indoor).

## Módulo de Mapa (AetherMap)

O componente central do dashboard é o `AetherMap`, que renderiza:
- **Dispositivos de Hardware:** Representações visuais dos nós ESP32 receptores CSI e dos roteadores/referências. O fluxo atual diferencia `role` e `hardware_profile`, permitindo marcar ESP32-C5, ESP32-WROOM, ESP32 Classic, roteador ou injetor.
- **Heatmap CSI:** Um mapa de calor animado gerado a partir da amplitude dos pacotes CSI em tempo real, ilustrando a dispersão do sinal de RF.
- **Monitoramento de Pessoas (Por Cômodo):** Quando a presença humana é detectada pelas variações no Channel State Information, o sistema rastreia não apenas a ocupação global, mas também calcula a proximidade do usuário em relação a cada hardware (Cômodo/Zona). A zona ativa pisca em verde e o painel informa visualmente qual ambiente está ocupado.
- **Localização Estimada:** Um marcador de presença que exibe confiança, precisão e número de nós usados. O contrato atual suporta `accuracy_m`, `accuracy_dm`, `confidence`, `nodes_used`, `method`, `experimental` e `resolution_m`.

## Topologia Atual C5 + WROOM

O dashboard foi atualizado para a arquitetura de dois receptores CSI: o
ESP32-C5 (`hardware_profile=esp32c5`, `role=receiver`) e o ESP32-WROOM
(`hardware_profile=wroom` ou `classic`, `role=receiver`). Portas, endereços
MAC, coordenadas e métricas de instalações particulares não são distribuídos
no repositório público.

O painel de topologia agora mostra:

- contagem de receptores CSI registrados;
- quantos são C5 e quantos são WROOM;
- status `2 nós ativos`, `fallback C5`, `fallback WROOM`, `sem baseline` ou `calibrando`;
- precisão atual (`accuracy_m`/`accuracy_dm`);
- confiança e resolução da coordenada;
- método da estimativa, como `fusion_c5_wroom_baseline_quantized`.

Também há um botão **Salvar baseline sala vazia**, que chama `POST /api/location/baseline`. Use esse botão apenas com o ambiente vazio, pois o baseline é o perfil estático que reduz falsos positivos e melhora a estimativa por nó.

### Rastreamento Inteligente de Câmera

Para facilitar a operação contínua sem necessidade de ajustes manuais, a câmera do mapa possui comportamento inteligente:
1. **Foco na Presença:** Se uma pessoa for detectada, o mapa acompanhará os movimentos dela suavemente.
2. **Foco nos Dispositivos (Fallback):** Se o ambiente estiver vazio, a câmera automaticamente calculará a "Bounding Box" (caixa delimitadora) de todos os dispositivos cadastrados e ajustará o zoom para que toda a topologia de hardware caiba na tela perfeitamente.

---

## Gerenciamento de Dispositivos pela UI

Em vez de configurar os dispositivos editando arquivos JSON ou banco de dados no backend, o AetherSense disponibiliza uma interface gráfica direta sobre o mapa para adicionar e editar seu hardware.

### Como Adicionar um Novo Dispositivo
1. Clique no botão **`+`** localizado no canto inferior central do mapa. O cursor mudará para uma mira e uma mensagem "Clique no mapa para posicionar" aparecerá.
2. Clique na posição exata da planta baixa onde o seu ESP32 ou Roteador está fisicamente instalado.
3. Um modal será aberto sobre o mapa, permitindo que você preencha:
   - **ID:** Identificador único do aparelho (ex: `esp32_sala`).
   - **Nome:** O nome do cômodo ou ambiente (ex: `Sala de Estar`, `Roteador Central`).
   - **Coordenadas (Lat/Lng):** Preenchidas automaticamente pelo seu clique, mas ajustáveis manualmente para maior precisão decimal.
   - **Ícone:** Escolha entre representações realistas de um `ESP32` ou um `Roteador`.
   - **Papel:** Escolha `Receptor CSI`, `Roteador/Injetor` ou `Injetor dedicado`.
   - **Hardware:** Escolha `ESP32-C5`, `ESP32-WROOM`, `ESP32 Classic` ou `Roteador`.
   - **Tamanho:** Ajuste o tamanho em pixels (ex: `32` ou `40`) para que o hardware fique proporcional à escala da sua planta baixa.
   - **Andar:** Andar em que o dispositivo está instalado (0 = Térreo).
4. Clique em **Salvar**. O dispositivo aparecerá instantaneamente no mapa.

### Como Editar um Dispositivo Existente
1. Com o mapa no modo padrão, clique diretamente sobre o ícone do dispositivo que deseja modificar.
2. O modal de edição abrirá, trazendo todos os dados atuais daquele hardware.
3. Modifique parâmetros (como ajustar as coordenadas finamente ou trocar de ESP32 para Roteador) e salve.

## Persistência no Backend Rust

Todas as adições e edições do mapa são enviadas via API REST (`POST /api/devices`) para o servidor Rust. O frontend também usa `POST /api/location/baseline` para salvar a calibração de sala vazia dos receptores ativos.

O servidor intercepta essas mudanças e as salva de forma durável no arquivo **`aethersense_config.json`** na raiz do projeto backend. Isso garante que:
- Você não precise reconfigurar a planta baixa a cada reinício do container ou do servidor.
- Os testes de localização e algoritmos possam usar a posição atualizada da sua sala para calibrar cálculos empíricos.
- O backend consiga aplicar pesos e fallback corretamente conforme `role` e `hardware_profile` de cada nó.

## Interpretação

O mapa apresenta estimativas condicionadas à calibração de sala vazia e à
topologia cadastrada. Essas coordenadas devem ser interpretadas como
experimentais, acompanhadas dos campos de confiança e precisão, e não como
medidas universais de desempenho.
