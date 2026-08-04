# AetherSense Rust Firmware Scaffold

This directory hosts the active Rust firmware for the AetherSense ESP32 CSI receivers.

> **Just want to flash your own ESP32?** Follow the step-by-step onboarding guide in
> [FLASHING.md](FLASHING.md) — it covers both the prebuilt-binary path (from the CI
> Release) and the local build, plus Wi-Fi setup via the captive portal (no recompile)
> and the automatic server discovery. This README below is the deeper reference.

The current scope is intentionally narrow:
- preserve the existing `CSI_DATA` CSV contract
- keep Wi-Fi extraction and UDP transmission separated
- use a bounded queue so the extractor can absorb short bursts instead of blocking
- keep the CSI raw conversion and callback bridge behind a feature-gated FFI layer
- choose the CSI capture profile from the hardware profile string before callback registration
- expose an explicit callback binding so the future ESP-IDF install step has a stable entry point
- support the current two-receiver topology: ESP32-C5 as the main receiver and ESP32-WROOM as the second CSI receiver

## Current state

The crate defaults to the `xtensa-esp32-espidf` target for ESP32-WROOM, and the helper script switches to `riscv32imac-esp-espidf` for ESP32-C5/C6 profiles.
The `esp-idf` feature gates the hardware path, and the real callback install is enabled only when the crate is built for an `espidf` target through `esp-idf-sys`.
Each device profile selects the canonical `classic`, `esp32c5`, or `esp32c6`
target; TUI and CLI consume the same profile model.

## Hardware support

The build profiles support ESP32-WROOM and ESP32-C5 receivers. Serial ports,
MAC addresses, Wi-Fi credentials and installation-specific measurements are
intentionally absent from the public repository.

The current firmware tuning uses a queue capacity of 256, assigns `seq` from
transmitted UDP frames, and paces the sender with
`AETHERSENSE_SEND_FREQUENCY_HZ=10`.

## Configuration

### Interactive multi-device setup (recommended)

Run `uv run build.py` without an action in an interactive terminal. The Textual
TUI opens in a simple one-click page: connect the board and choose **Configurar e
testar automaticamente**. Bluetooth COM ports are ignored; the current USB port and its saved
device profile are selected automatically. The tool then pairs a first-use
board, builds, flashes, provisions only the changed data, validates the physical
hardware and opens the monitor only after the serial port is released.
If more than one physical board is connected, the sole required choice is which
friendly device to use.

Existing IR, relay and microSD wiring is reused unchanged. A new board starts
with safe defaults and captive-portal Wi-Fi; the tool never guesses external
wiring or energises a relay. Detailed identity, GPIO, MQTT/server, storage,
import/export and commissioning cards live behind **Opções avançadas**. Those
cards can learn/replay raw IR and test enabled accessories directly over USB;
MQTT, server and onboarding remain optional unless remote control is wanted.
The advanced view does not use a horizontal tab strip: **Seções** opens a
scrollable card menu for board, network/LED, accessories, integrations, SD,
execution, diagnostics and help, including on an 80-column terminal.

The paired `Meu espc5` profile has a known wiring map (IR RX GPIO8, IR TX GPIO9,
on-board RGB GPIO27), so common mode restores it without exposing electrical
parameters. IR/relay USB commissioning starts before Wi-Fi association; local
control remains available while the network is slow or offline.

Profiles use `aethersense.device_profile.v1` and live outside the repository in
`%LOCALAPPDATA%\AetherSense\devices`. Passwords and Bearer tokens are stored in
the OS Credential Manager/keyring; exports never contain them. Explicit CLI
actions remain available for CI and automation.

### Runtime configuration and secrets

Wi-Fi, UDP, MQTT, server, pins, relays, IR, LED RGB and microSD are configured in
the TUI and committed atomically to NVS. They are intentionally excluded from
the Cargo environment and binary, so two devices with the same target/features
share one reproducible build. Passwords and tokens are resolved from keyring
references only during provisioning and never written to profiles or logs.

After a successful Wi-Fi association, the firmware also stores the non-secret
`wifi_channel` hint in NVS. Subsequent boots start a fast scan on that channel;
if the hint is absent or incompatible with the selected 2.4/5 GHz band, it is
ignored and the firmware discovers the strongest matching SSID without pinning
the connection to a BSSID. Cache read/write failures never block startup.

For Docker Desktop on Windows, enter the host IPv4 in the TUI's UDP field when
broadcast cannot reach the container, or run the repository's UDP relay. Apply
the change through provisioning; no rebuild is needed.

### Hardware Profiles

Choose the matching target in the TUI (or in the profile consumed by CLI):
- `classic` or `wroom` - ESP32-WROOM-32D (WiFi 4, 2.4GHz)
- `esp32c5` - ESP32-C5 (WiFi 6, dual-band)
- `esp32c6` or `c61` - ESP32-C6/C61 (WiFi 6, Thread/Matter)

## Run

### Hardware Build (ESP-IDF)

On Windows, prefer the helper script from this directory. It copies each build to
an isolated `C:\afw\<repo>\<profile>\<process>` sandbox, keeps the full log in
`build.log`, and is the validated flow for the boards used in this workspace.

```powershell
# Build only
uv run build.py build --device-profile <UUID>

# Host unit/contract tests (overrides the embedded default Cargo target)
uv run build.py test

# Flash a paired WROOM profile
uv run build.py flash --device-profile <UUID> --port <SERIAL_PORT>

# Build, flash and monitor WROOM
uv run build.py run --device-profile <UUID> --port <SERIAL_PORT>

# Open the serial monitor only
uv run build.py monitor --device-profile <UUID> --port <SERIAL_PORT>

# Recover a board that missed bootloader entry
uv run build.py rescue --device-profile <UUID> --port <SERIAL_PORT> --timeout 20
```

For ESP32-C5:

```powershell
uv run build.py build --device-profile <UUID-C5>
uv run build.py run --device-profile <UUID-C5> --port <SERIAL_PORT> --timeout 120
```

The ESP32-C5 image currently requires the custom 2 MB app partition declared in
`partitions.csv`. The helper includes it in each isolated `C:\afw` sandbox; keep
the partition file committed so C5 builds do not fall back to the default 1 MB
factory slot.

**Note:** The `esp-idf` feature enables the real CSI extraction path via ESP-IDF bindings.

All firmware builds and tests must go through `uv run build.py`; do not invoke
Cargo directly for the embedded target.

## Validation

After flashing, validate the live packet-loss path from the server workspace:

```powershell
cd ..\aethersense-server
$env:AETHERSENSE_REQUIRE_LIVE_CAPTURE='true'
cargo test --test m7_packet_loss -- --nocapture
```

The current firmware tuning passes this test with packet loss below 1%; the latest C5+WROOM run measured `0.00%`.

For full live validation:

```powershell
cd ..\aethersense-server
$env:AETHERSENSE_REQUIRE_LIVE_CAPTURE='true'
cargo test -- --nocapture
```

Latest full-suite result: passed with real UDP packets from both devices.

## Next integration step

Keep the real ESP-IDF CSI callback isolated from the UDP sender worker, and tune sampling frequency/phase stability without regressing the `m7` packet-loss result.
