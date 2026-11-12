<p align="center">
  <img src="frontend/public/logo/logo_inicial.png" alt="AetherSense" width="520">
</p>

<p align="center">
  <strong>English (USD $)</strong> ·
  <a href="README.pt-BR.md">Português (BR · BRL R$)</a> ·
  <a href="README.es.md">Español (EUR € / USD $)</a> ·
  <a href="README.de.md">Deutsch (EUR €)</a>
</p>

# AetherSense

**AetherSense** is an open-source, camera-free **Wi-Fi Channel State Information (CSI)** spatial sensing and smart-home automation platform. Built on low-cost ESP32 nodes, a high-performance Rust server, a web dashboard, optional ONNX inference, and Home Assistant integration, it delivers **geolocalized presence detection, movement tracking, experimental vital trends, and local IR/relay automation** using your existing Wi-Fi network.

> **Scientific Transparency:** AetherSense originated as an academic undergraduate thesis (*TCC*) prototype for local, camera-free Wi-Fi spatial sensing and smart-home automation. Location and vital-sign estimates are experimental and must not be used for medical diagnosis, life-safety, or mission-critical decisions.

<p align="center">
  <img src="docs/images/aethersense_showcase.gif" alt="AetherSense Live Showcase" width="100%">
</p>

---

## Why AetherSense? Cost & Efficiency Comparison (USD `$`)

| Solution | Technology | Est. Price (USD) | Static Presence (Still/Sleeping) | Multi-Room / Through-Wall | Built-in IR / Relay | Camera-Free Privacy | 100% Local (No Cloud) | Links |
| :--- | :--- | :---: | :---: | :---: | :---: | :---: | :---: | :--- |
| **AetherSense (1-Node Wi-Fi CSI)** *(DIY BOM)* | Wi-Fi CSI (`ESP32-WROOM-32` / `ESP32-C5`) | **$5–$16** | Yes (CSCR + baseline) | Partial (1 room/link) | Yes (38 kHz IR + Relay) | Yes | Yes | [Espressif ESP32-C5](https://www.espressif.com/en/products/socs/esp32-c5) · [AliExpress ESP32](https://www.aliexpress.com/w/wholesale-esp32-wroom-32.html) |
| **AetherSense (Multi-Node Kit)** *(DIY BOM)* | Dual-Band Wi-Fi CSI (`C5` 5 GHz + `WROOM` 2.4 GHz) | **$20–$30** | Yes | **Yes** (multi-zone fusion) | Yes (per node) | Yes | Yes | [Releases](https://github.com/joaosnet/aethersense/releases) · [AliExpress C5](https://www.aliexpress.com/w/wholesale-esp32-c5.html) |
| **RuView (WiFi DensePose)** | Wi-Fi CSI (`3–6× ESP32-S3` Mesh) | **~$54+** | Yes | Yes (mesh) | No | Yes | Yes | [GitHub (ruvnet/RuView)](https://github.com/ruvnet/RuView) · [AliExpress ESP32-S3](https://www.aliexpress.com/w/wholesale-esp32-s3.html) |
| **Tuya mmWave Presence (`ZY-M100`)** | 5.8 / 24 GHz mmWave Radar | **$18–$40** | **Yes** | No (line-of-sight / 1 room) | No | Yes | Zigbee: Yes · Wi-Fi: Cloud | [Blakadder](https://blakadder.com/) · [Zigbee2MQTT](https://www.zigbee2mqtt.io/) · [AliExpress](https://www.aliexpress.com/w/wholesale-tuya-zy-m100-mmwave.html) |
| **Aqara Presence Sensor FP2** | 60 GHz mmWave Multi-Zone Radar | **$60–$83** | **Yes** (30 zones / 5 targets) | No (1 room per unit) | No | Yes | Yes (HomeKit / HA) | [Aqara Official](https://www.aqara.com/en/product/presence-sensor-fp2/) · [Amazon US](https://www.amazon.com/s?k=Aqara+FP2) |
| **Generic Wi-Fi PIR Sensor** | Passive Infrared (PIR) | **$4–$16** | **No** (motion only) | No (optical cone) | No | Yes | Usually Cloud | [Amazon US](https://www.amazon.com/s?k=wifi+pir+motion+sensor) |
| **Wi-Fi IP Camera (`Tapo C200`)** | Optical Video + AI | **$25–$35** | Yes (vision) | No (needs light/LOS) | No | **No** (video/audio) | Partial (RTSP / SD) | [TP-Link Official](https://www.tp-link.com/en/home-networking/cloud-camera/tapo-c200/) · [Amazon US](https://www.amazon.com/s?k=TP-Link+Tapo+C200) |

> Prices reflect observed market ranges across AliExpress and Amazon (excluding optional shipping or local import taxes). DIY BOM reflects electronics components before enclosure and assembly. Single-node setups monitor room/zone presence and movement; dual-band multi-node kits improve cross-room zone fusion and spatial tracking.

---

## Where to Buy or Build (USD `$`)

### Option A — Ready-to-Use Pre-Flashed Kit *(Coming Soon)*
- **AetherSense Plug-and-Play Kit**: *Pre-flashed hardware kits with protective enclosure, 5V power supply, and guided local setup — coming soon.* Follow [GitHub Releases](https://github.com/joaosnet/aethersense/releases) and [Discussions](https://github.com/joaosnet/aethersense/discussions) for launch updates.

### Option B — DIY Bill of Materials (Build It Yourself)
- **Primary 5 GHz Wi-Fi 6 Node (`ESP32-C5-DevKitC-1`)**: ~**$15 USD** — [Espressif](https://www.espressif.com/en/products/socs/esp32-c5) · [AliExpress](https://www.aliexpress.com/w/wholesale-esp32-c5-devkit.html)
- **Secondary 2.4 GHz Node (`ESP32-WROOM-32`)**: ~**$3–$6 USD** (AliExpress) / **$6–$10 USD** (Amazon) — [AliExpress](https://www.aliexpress.com/w/wholesale-esp32-wroom-32.html) · [Amazon US](https://www.amazon.com/s?k=ESP32-WROOM-32+DevKit)
- **Optional 38 kHz IR TX/RX Module**: ~**$1–$3 USD** — [AliExpress](https://www.aliexpress.com/w/wholesale-ir-transmitter-receiver-38khz-esp32.html)
- **5V ≥ 1A USB-C Power Supply**: ~**$3–$5 USD**

---

## System Showcase

### 1. Real-Time Operational Dashboard
Displays the interactive floorplan with real-time spatial tracking, presence heatmaps, occupancy estimation, and configurable automation zones:

<p align="center">
  <img src="docs/images/dashboard_cliente.png" alt="AetherSense Operational Client Dashboard" width="100%">
</p>

- **Spatial tracking**: continuous $(x, y)$ estimation with EWMA smoothing and multi-node zone fusion;
- **Occupancy & zones**: room/zone triggers ready for Home Assistant or direct local automation;
- **Built-in accessories**: IR remote learning/blasting and relay switching per node.

### 2. RF Signal Processing (DSP) & Engineering Diagnostics
The engineering view exposes the live RF pipeline for inspecting physical channel behavior:

<p align="center">
  <img src="docs/images/dashboard_debug.png" alt="AetherSense Engineering Debug Dashboard" width="100%">
</p>

- **Subcarrier Amplitude Heatmap** · **Doppler STFT Spectrogram** · **Phase Sanitization** · **Power Delay Profile (PDP) via IFFT**.

### 3. Hardware Topology & Empty-Room Calibration
Live node telemetry (sample rate, RSSI, band, latency) and static-background calibration without collecting local human training datasets:

<p align="center">
  <img src="docs/images/card_hardware_hud.png" alt="AetherSense ESP32 Hardware Telemetry HUD" width="100%">
</p>

<p align="center">
  <img src="docs/images/dashboard_calibracao.png" alt="AetherSense Empty-Room Calibration" width="100%">
</p>

### 4. Physical Foundations (OFDM & CSI)
Passive sensing leverages IEEE 802.11 OFDM subcarriers to detect subtle body-induced multipath perturbations without cameras:

<p align="center">
  <img src="docs/images/infografico_ofdm_csi.png" alt="AetherSense OFDM and CSI Extraction Infographic" width="85%">
</p>

---

## Components

| Directory | Contents |
| :--- | :--- |
| `firmware-rust/` | Active ESP32-WROOM-32, ESP32-C5, and ESP32-C6 firmware |
| `aethersense-server/` | Rust API, UDP CSI ingest, DSP processing, ONNX inference, and MQTT publisher |
| `frontend/` | React + TypeScript web dashboard |
| `ml/` | Public-dataset training, evaluation, and ONNX export pipeline |
| `homeassistant/` | Home Assistant package and add-on definitions |
| `esp_realtime_plot/` | Python tools for live serial CSI inspection |
| `docs/` | Architecture and advanced integration guides |

---

## Quick Start

Clone the repository and create your local config:

```bash
git clone https://github.com/joaosnet/aethersense.git
cd aethersense
cp aethersense-server/aethersense_config.example.json aethersense-server/aethersense_config.json
```

Start the stack with credentials passed via environment variables (never commit secrets):

```bash
export AETHERSENSE_API_TOKEN="change-this-secret"
export AETHERSENSE_MQTT_USERNAME="user"
export AETHERSENSE_MQTT_PASSWORD="password"
docker compose up --build
```

- Dashboard: <http://localhost>
- API & Healthcheck: <http://localhost:8000/api/health>
- CSI UDP ingest: `5005/udp` · Broker discovery: `5151/udp`

### Flashing Firmware (Dev Flow)
```bash
cd firmware-rust
uv run build.py build
uv run build.py flash --port YOUR_PORT
```
See [firmware-rust/FLASHING.md](firmware-rust/FLASHING.md) for board profiles, captive portal setup, and recovery.

---

## Documentation, Privacy & Security

- [Documentation Map](docs/README.md) · [Architecture](docs/arquitetura-firmware-servidor.md) · [Home Assistant](docs/home-assistant-integration.md)
- [Backend](aethersense-server/README.md) · [Frontend](frontend/README.md) · [ML Pipeline](ml/README.md)
- [Security Policy](SECURITY.md)

The public repository excludes credentials, local home coordinates, device identifiers, and raw captures. Please report security vulnerabilities privately via [SECURITY.md](SECURITY.md).
