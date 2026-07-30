# ruff: noqa: I001
import argparse
import collections
from dataclasses import dataclass, field, replace
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import sys
import threading
import time
from typing import Any

from aethersense_build.diffing import compute_build_hash, compute_config_hash, profile_status
from aethersense_build.devices import DevicePairingError, probe_board, resolve_profile_port
from aethersense_build.environment import build_environment, effective_values
from aethersense_build.integrations import test_mqtt_connection, test_server_connection
from aethersense_build.hardware_validation import (
    AutomaticTestResult,
    HardwareValidationError,
    HardwareValidationService,
    ValidationState,
    wait_for_runtime_snapshot,
)
from aethersense_build.models import (
    DeviceProfile,
    RelayConfig,
    device_id_from_mac,
    new_device_profile,
    utc_now,
)
from aethersense_build.provisioning import (
    ProvisionClient,
    ProvisioningError,
    ProvisioningTimeout,
    activate_provisioned_profile,
)
from aethersense_build.redaction import is_secret_key, redact_text
from aethersense_build.secrets import SecretManager
from aethersense_build.sd_import import import_sd_sessions, inspect_sd
from aethersense_build.store import DeviceProfileStore
from aethersense_build.validation import has_errors, validate_profile


DEFAULT_WINDOWS_PORT = "COM5"
DEFAULT_HARDWARE_PROFILE = "classic"
DEFAULT_FLASH_BAUD = 115200
ESP32C5_SAFE_FLASH_BAUD = 115200
DEFAULT_MONITOR_BAUD = 115200
DEFAULT_SANDBOX_DIR = "C:\\afw"
DEFAULT_TARGET_ROOT = "C:\\a"

# Valores-sentinela do Select de porta da TUI (nao sao portas reais).
PORT_SELECT_AUTO = "__auto__"
PORT_SELECT_MANUAL = "__manual__"

HARDWARE_PROFILES = {
    "classic": {
        "aliases": {"", "classic", "esp32", "wroom", "esp32-wroom", "esp32-wroom-32"},
        "chip": "esp32",
        "mcu": "esp32",
        "target": "xtensa-esp32-espidf",
        "profile": "classic",
        "esp_idf_version": "v5.4.2",
    },
    "esp32c5": {
        "aliases": {"c5", "esp32c5", "esp32-c5"},
        "chip": "esp32c5",
        "mcu": "esp32c5",
        "target": "riscv32imac-esp-espidf",
        "profile": "esp32c5",
        "esp_idf_version": "v5.5.4",
    },
    "esp32c6": {
        "aliases": {"c6", "esp32c6", "esp32-c6", "c61", "esp32c61", "esp32-c61"},
        "chip": "esp32c6",
        "mcu": "esp32c6",
        "target": "riscv32imac-esp-espidf",
        "profile": "esp32c6",
        "esp_idf_version": "v5.4.2",
    },
}

CRITICAL_MONITOR_PATTERNS = (
    ("panicked at", "[ERROR] CRITICAL CRASH DETECTED IN ESP32 OUTPUT!"),
    ("abort() was called", "[ERROR] CRITICAL CRASH DETECTED IN ESP32 OUTPUT!"),
    ("guru meditation error", "[ERROR] CRITICAL CRASH DETECTED IN ESP32 OUTPUT!"),
    (
        "csi not enabled in menuconfig",
        "[ERROR] CSI feature is disabled in ESP-IDF! The build did not apply sdkconfig.defaults correctly.",
    ),
    ("unknown csi bug!", "[ERROR] ESP-IDF internal CSI bug detected! ('unknown csi bug!')"),
)

MONITOR_TIMER_START_PATTERNS = (
    "Flashing has completed!",
    "ets Jul 29 2019",
    "ESP-IDF v",
)

PYTHON_ENV_VARS_TO_REMOVE = (
    "VIRTUAL_ENV",
    "VIRTUAL_ENV_PROMPT",
    "PYTHONHOME",
    "PYTHONPATH",
    "UV_VIRTUAL_ENV",
    "__PYVENV_LAUNCHER__",
    "PIP_REQUIRE_VIRTUALENV",
    "CONDA_PREFIX",
    "CONDA_DEFAULT_ENV",
)

# (substring procurado na saida, mensagem clara e acionavel). Usado para resumir
# a causa provavel no FIM do build, em vez de deixa-la enterrada no build.log.
BUILD_FAILURE_HINTS = (
    (
        "failed to locate python",
        "Causa: o ESP-IDF nao encontrou um 'python' compativel (3.9-3.13). "
        "O build.py tenta garantir um via uv; rode 'uv python install 3.13' e tente de novo.",
    ),
    (
        "python was not found",
        "Causa: 'python' caiu no alias do Microsoft Store (sem python real no PATH). "
        "O build.py garante um via uv; verifique se o uv esta instalado.",
    ),
    (
        "could not install esp-idf",
        "Causa: falha ao instalar/exportar o ESP-IDF (normalmente python ausente ou rede). "
        "Veja as linhas acima; o ESP-IDF gerenciado fica em ~/.espressif.",
    ),
    (
        "error: linker `ldproxy`",
        "Causa: 'ldproxy' ausente. Rode 'cargo install ldproxy' (faz parte do toolchain esp).",
    ),
    (
        "no such command: `+esp`",
        "Causa: toolchain Rust 'esp' ausente. Rode 'cargo install espup && espup install'.",
    ),
    (
        "error[e",
        "Causa: erro de compilacao Rust (ver 'error[E...]' acima).",
    ),
    (
        "the selected serial port",
        "Causa: porta serial errada/ocupada. Feche monitores e confira a porta com --port.",
    ),
    (
        "espflash::timeout",
        "Causa: timeout do espflash (placa nao entrou em bootloader). Tente 'rescue' ou um cabo/fonte melhor (C5 e sensivel).",
    ),
)


def summarize_failure(recent_lines) -> list[str]:
    blob = "".join(recent_lines).lower()
    hints = []
    for needle, message in BUILD_FAILURE_HINTS:
        if needle in blob and message not in hints:
            hints.append(message)
    return hints


@dataclass(frozen=True)
class ActionSpec:
    label: str
    description: str
    requires_sandbox: bool = True
    parses_monitor: bool = False


ACTIONS = {
    "test": ActionSpec(
        "Testes host",
        "Run firmware Rust unit/contract tests on the host.",
        requires_sandbox=False,
    ),
    "build": ActionSpec("Build", "Compile the firmware release image."),
    "flash": ActionSpec("Flash", "Build and flash the selected ESP32 board."),
    "erase": ActionSpec("Erase", "Erase the selected ESP32 flash."),
    "monitor": ActionSpec(
        "Monitor",
        "Open a serial monitor without rebuilding.",
        requires_sandbox=False,
        parses_monitor=True,
    ),
    "run": ActionSpec(
        "Run",
        "Build, flash, and monitor for crashes.",
        parses_monitor=True,
    ),
    "clean": ActionSpec("Clean", "Run cargo clean in the firmware build sandbox."),
    "fullclean": ActionSpec("Full Clean", "Clean cargo and ESP-IDF CMake caches."),
    "rescue": ActionSpec(
        "Rescue",
        "Force bootloader mode, flash with no reset, and monitor.",
        parses_monitor=True,
    ),
    "flashall": ActionSpec(
        "Flash All",
        "Detect connected ESP32 boards and build+flash each automatically.",
        requires_sandbox=False,
    ),
    "provision": ActionSpec(
        "Provisionar",
        "Provision the selected device profile atomically over USB serial.",
        requires_sandbox=False,
    ),
    "test-integration": ActionSpec(
        "Testar integração",
        "Validate MQTT/server integration for a device profile.",
        requires_sandbox=False,
    ),
    "inspect-sd": ActionSpec(
        "Inspecionar SD",
        "Inspect AetherSense sessions on a mounted microSD without changing it.",
        requires_sandbox=False,
    ),
    "import-sd": ActionSpec(
        "Importar SD",
        "Import and deduplicate mounted microSD sessions into resultados/.",
        requires_sandbox=False,
    ),
    "validate-hardware": ActionSpec(
        "Validar hardware",
        "Validate the connected physical board and persist redacted evidence.",
        requires_sandbox=False,
    ),
}

PROFILE_SERVICE_ACTIONS = frozenset(
    {"provision", "test-integration", "inspect-sd", "import-sd", "validate-hardware"}
)


@dataclass(frozen=True)
class BuildSettings:
    project_dir: str
    sandbox_dir: str = DEFAULT_SANDBOX_DIR
    sandbox_profile: str = ""
    target_dir: str = ""

    @property
    def log_path(self) -> str:
        return os.path.join(self.project_dir, "build.log")


@dataclass
class BuildOptions:
    action: str = "run"
    port: str | None = None
    profile: str | None = None
    timeout: int | None = None
    env_overrides: dict[str, str] = field(default_factory=dict)
    device_profile: DeviceProfile | None = None
    device_profile_source: str | None = None
    integration: str = "all"
    sd_path: str | None = None
    results_dir: str | None = None
    allow_unpaired_legacy: bool = False


@dataclass(frozen=True)
class BuildEvent:
    kind: str
    message: str
    data: dict[str, Any] = field(default_factory=dict)


class BuildFailure(Exception):
    def __init__(self, message: str, exit_code: int = 1):
        super().__init__(message)
        self.message = message
        self.exit_code = exit_code


class ConsoleLogSink:
    def __init__(self, terminal, log_file):
        self.terminal = terminal
        self.log_file = log_file
        self.lock = threading.Lock()
        self._secret_values = {
            value
            for key, value in os.environ.items()
            if value and is_secret_key(key)
        }

    def add_secret_values(self, values) -> None:
        self._secret_values.update(str(value) for value in values if value)

    def _safe_message(self, message: str) -> str:
        return redact_text(message, tuple(self._secret_values)).encode(
            "ascii", errors="replace"
        ).decode("ascii")

    def write(self, message: str) -> None:
        with self.lock:
            safe = self._safe_message(message)
            for stream in (self.terminal, self.log_file):
                try:
                    stream.write(safe)
                except (OSError, ValueError):
                    # A launcher may close stdout while cleanup is still
                    # running.  Cleanup and the remaining log destination must
                    # continue independently.
                    continue
            self.flush()

    def line(self, message: str = "") -> None:
        self.write(f"{message}\n")

    def flush(self) -> None:
        for stream in (self.terminal, self.log_file):
            try:
                stream.flush()
            except (OSError, ValueError):
                continue

    def event(self, event: BuildEvent) -> None:
        if event.message:
            self.line(event.message)


class ProcessController:
    def __init__(self):
        self.process: subprocess.Popen[str] | None = None
        self.cancel_requested = False
        self.lock = threading.Lock()

    def attach(self, process: subprocess.Popen[str]) -> None:
        with self.lock:
            self.cancel_requested = False
            self.process = process

    def detach(self, process: subprocess.Popen[str]) -> None:
        with self.lock:
            if self.process is process:
                self.process = None

    def cancel(self) -> None:
        with self.lock:
            self.cancel_requested = True
            process = self.process
        if process is not None:
            stop_process_tree(process)


def running_on_required_python() -> bool:
    return sys.version_info >= (3, 11)


def ensure_required_python_runtime() -> int | None:
    if running_on_required_python():
        return None
    print("[ERROR] build.py requires Python 3.11 or newer (run it with uv).", file=sys.stderr)
    return 1


def fail(message: str, exit_code: int = 1) -> None:
    raise BuildFailure(message, exit_code)


def emit(sink, message: str = "") -> None:
    sink.line(message)


def emit_progress(sink, progress: float | None, message: str = "", stage: str = "") -> None:
    sink.event(
        BuildEvent(
            kind="progress",
            message=message,
            data={"progress": progress, "stage": stage},
        )
    )


def command_text(command: list[str]) -> str:
    return " ".join(command)


def get_profile_names() -> list[str]:
    return sorted(HARDWARE_PROFILES.keys())


def remove_directory(path, sink=None):
    if not os.path.exists(path):
        return

    if sink is not None:
        emit(sink, f"       Deleting {path}...")

    last_error = None
    for attempt in range(5):
        try:
            shutil.rmtree(path)
            return
        except FileNotFoundError:
            return
        except OSError as err:
            last_error = err
            if attempt < 4:
                time.sleep(1)
                continue
            if sink is not None:
                emit(sink, f"[WARN] Unable to remove {path}: {err}")
            return

    if last_error is not None and sink is not None:
        emit(sink, f"[WARN] Cleanup finished with a residual error for {path}: {last_error}")


def isolated_build_settings(settings: BuildSettings, profile_name: str) -> BuildSettings:
    """Return a per-process sandbox so concurrent boards/build.py runs cannot clobber.

    The path is deterministic up to the final process/thread nonce and always
    remains below C:\\afw on Windows.
    """
    if os.name != "nt":
        return settings
    project_identity = str(Path(settings.project_dir).resolve()).casefold()
    repo_key = hashlib.sha256(project_identity.encode("utf-8")).hexdigest()[:6]
    short_profile = {"classic": "w", "esp32c5": "c5", "esp32c6": "c6"}.get(
        profile_name, profile_name[:2]
    )

    def base36(number: int) -> str:
        alphabet = "0123456789abcdefghijklmnopqrstuvwxyz"
        if number == 0:
            return "0"
        encoded = ""
        while number:
            number, remainder = divmod(number, 36)
            encoded = alphabet[remainder] + encoded
        return encoded

    nonce_source = f"{threading.get_ident()}|{time.time_ns()}"
    nonce = hashlib.sha256(nonce_source.encode("utf-8")).hexdigest()[:6]
    process_nonce = f"{base36(os.getpid())}-{nonce}"
    # Keep the required repo/profile/process hierarchy, but abbreviate every
    # component: esp-idf-sys has a strict Windows output-directory budget.
    sandbox = Path(DEFAULT_SANDBOX_DIR) / repo_key / short_profile / process_nonce
    root = Path(DEFAULT_SANDBOX_DIR).resolve()
    resolved = sandbox.resolve()
    try:
        resolved.relative_to(root)
    except ValueError as exc:
        raise BuildFailure(f"[ERROR] Unsafe sandbox path: {resolved}") from exc

    # esp-idf-sys applies a second and substantially tighter Windows path
    # budget to Cargo's output root. Keep source sandboxes per-process, while
    # sharing a deterministic repo/profile Cargo cache (Cargo supplies its own
    # target lock) under a <= 10 character path.
    target_root = Path(DEFAULT_TARGET_ROOT).resolve()
    target_root.mkdir(parents=True, exist_ok=True)
    target_token = hashlib.sha256(
        f"{project_identity}|{profile_name}".encode("utf-8")
    ).hexdigest()[:5]
    target_dir = (target_root / target_token).resolve()
    target_dir.mkdir(exist_ok=True)
    if len(str(target_dir)) > 10:
        raise BuildFailure(f"[ERROR] Cargo target path exceeds ESP-IDF limit: {target_dir}")
    target_marker = target_dir / ".aethersense-target.json"
    identity = {
        "schema": "aethersense.build_target.v1",
        "project_dir": str(Path(settings.project_dir).resolve()),
        "profile": profile_name,
    }
    if target_marker.exists():
        try:
            previous_identity = json.loads(target_marker.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise BuildFailure(f"[ERROR] Invalid Cargo target marker: {target_marker}") from exc
        if previous_identity != identity:
            raise BuildFailure(
                f"[ERROR] Short Cargo target hash collision at {target_dir}; remove it and retry."
            )
    else:
        target_marker.write_text(
            json.dumps(identity, sort_keys=True) + "\n",
            encoding="utf-8",
        )
    return replace(
        settings,
        sandbox_dir=str(resolved),
        sandbox_profile=profile_name,
        target_dir=str(target_dir),
    )


def build_cargo_build_command(profile):
    return [
        "cargo",
        "+esp",
        "build",
        "--target",
        profile["target"],
        "--release",
        "--features",
        "esp-idf",
    ]


def rust_host_target() -> str:
    result = subprocess.run(
        ["rustc", "-vV"], capture_output=True, text=True, errors="replace", check=False
    )
    if result.returncode == 0:
        for line in result.stdout.splitlines():
            if line.startswith("host:"):
                target = line.split(":", 1)[1].strip()
                if target:
                    return target
    if os.name == "nt":
        return "x86_64-pc-windows-msvc"
    fail("[ERROR] Could not determine the host Rust target from 'rustc -vV'.")


def build_espflash_command(
    *,
    profile,
    monitor=False,
    no_reset=False,
    port=None,
    monitor_baud=DEFAULT_MONITOR_BAUD,
):
    command = ["cargo", "+esp", "espflash", "flash"]
    if monitor:
        # In espflash 4.x, --non-interactive belongs to the flattened monitor
        # options. Supplying it for a flash-only command emits the misleading
        # "Monitor options were provided" warning even though flashing is
        # otherwise successful. It is only needed when the monitor is active.
        command.extend(["--monitor", "--non-interactive"])

    command.extend(
        [
            "--target",
            profile["target"],
            "--release",
            "--features",
            "esp-idf",
        ]
    )

    command.extend(["--chip", resolve_espflash_chip(profile)])
    baud = flash_baud_for_profile(profile)
    if baud:
        command.extend(["--baud", str(baud)])

    if monitor:
        command.extend(["--monitor-baud", str(monitor_baud)])
    if no_reset:
        command.extend(["--before", "no-reset"])
    if port:
        command.extend(["--port", port])

    return command


def build_espflash_erase_command(*, profile, port=None):
    command = [
        "cargo",
        "+esp",
        "espflash",
        "erase-flash",
        "--non-interactive",
        "--chip",
        resolve_espflash_chip(profile),
    ]
    baud = flash_baud_for_profile(profile)
    if baud:
        command.extend(["--baud", str(baud)])
    if port:
        command.extend(["--port", port])

    return command


def build_espflash_monitor_command(*, profile, port=None, monitor_baud=DEFAULT_MONITOR_BAUD):
    command = [
        "cargo",
        "espflash",
        "monitor",
        "--non-interactive",
        "--chip",
        resolve_espflash_chip(profile),
        "--monitor-baud",
        str(monitor_baud),
    ]
    baud = flash_baud_for_profile(profile)
    if baud:
        command.extend(["--baud", str(baud)])
    if port:
        command.extend(["--port", port])

    return command


def describe_planned_command(options: BuildOptions, settings: BuildSettings) -> str:
    profile = resolve_hardware_profile(options.profile, sink=None)
    port = normalize_serial_port(options.port) if options.port else resolve_monitor_port(
        settings.project_dir,
        None,
        validate=False,
    )

    if options.action == "build":
        return command_text(build_cargo_build_command(profile))
    if options.action == "test":
        return f"cargo test --target {rust_host_target()} --features esp-idf -- --nocapture"
    if options.action in {"clean", "fullclean"}:
        return "cargo +esp clean"
    if options.action == "flash":
        return command_text(
            build_espflash_command(profile=profile, port=port, monitor=False, no_reset=False)
        )
    if options.action == "erase":
        return command_text(build_espflash_erase_command(profile=profile, port=port))
    if options.action == "run":
        return command_text(
            build_espflash_command(profile=profile, port=port, monitor=True, no_reset=False)
        )
    if options.action == "rescue":
        return command_text(
            build_espflash_command(profile=profile, port=port, monitor=True, no_reset=True)
        )
    if options.action == "monitor":
        if profile["profile"] == "esp32c5":
            return command_text(build_espflash_monitor_command(profile=profile, port=port))
        return f"serial monitor {port or '<auto>'} @ {DEFAULT_MONITOR_BAUD}"
    return options.action


def resolve_hardware_profile(requested=None, sink=None):
    if requested is None:
        explicit_mcu = os.environ.get("AETHERSENSE_MCU", "").strip().lower()
        explicit_profile = os.environ.get("AETHERSENSE_HARDWARE_PROFILE", "").strip().lower()
        requested = explicit_mcu or explicit_profile or DEFAULT_HARDWARE_PROFILE
    else:
        requested = requested.strip().lower()

    for profile in HARDWARE_PROFILES.values():
        if requested in profile["aliases"]:
            return profile

    if sink is not None:
        emit(
            sink,
            f"[WARN] Unknown hardware profile '{requested}', falling back to {DEFAULT_HARDWARE_PROFILE}.",
        )
    return HARDWARE_PROFILES[DEFAULT_HARDWARE_PROFILE]


def normalize_serial_port(port):
    port = str(port).strip()
    if os.name == "nt" and port.isdigit():
        return f"COM{port}"
    return port


def available_serial_ports():
    return [info["device"] for info in available_serial_port_infos()]


def is_bluetooth_port(port_info: dict[str, Any]) -> bool:
    text = " ".join(
        str(port_info.get(key) or "") for key in ("description", "manufacturer", "hwid")
    ).lower()
    return "bthenum" in text or "bluetooth" in text


def infer_serial_profile(port_info: dict[str, Any]) -> str:
    text = " ".join(
        str(port_info.get(key) or "")
        for key in ("device", "description", "manufacturer", "hwid")
    ).lower()

    # Pista explicita no nome do dispositivo tem prioridade.
    if "esp32-c6" in text or "esp32c6" in text:
        return "esp32c6"
    if "esp32-c5" in text or "esp32c5" in text:
        return "esp32c5"
    # Pelo chip da ponte USB-serial (setup deste projeto: WROOM=CP210x, C5=CH340).
    # A validacao via 'espflash board-info' antes do flash recusa se o chip divergir,
    # entao um palpite errado aqui aborta com seguranca em vez de gravar a placa errada.
    if "cp210" in text or "silicon labs" in text or "usb to uart" in text:
        return "classic"
    if "ch340" in text or "wch" in text:
        return "esp32c5"
    return "unknown"


def available_serial_port_infos():
    try:
        import serial.tools.list_ports

        infos = []
        for port in serial.tools.list_ports.comports():
            info = {
                "device": port.device or "",
                "description": port.description or "",
                "manufacturer": port.manufacturer or "",
                "hwid": port.hwid or "",
                "vid": "" if port.vid is None else f"{port.vid:04X}",
                "pid": "" if port.pid is None else f"{port.pid:04X}",
                "serial_number": port.serial_number or "",
                "location": port.location or "",
            }
            info["suggested_profile"] = infer_serial_profile(info)
            infos.append(info)
        return infos
    except ImportError:
        return []


def ensure_serial_port_available(port):
    available_ports = available_serial_ports()
    if not available_ports:
        return

    if os.name == "nt":
        requested = port.upper()
        available_normalized = [available.upper() for available in available_ports]
    else:
        requested = port
        available_normalized = available_ports

    if requested not in available_normalized:
        fail(
            f"[ERROR] Serial port {port} was not found. Available ports: {', '.join(available_ports)}"
        )


def resolve_monitor_port(project_dir, explicit_port=None, validate=True):
    if explicit_port:
        port = normalize_serial_port(explicit_port)
        if validate:
            ensure_serial_port_available(port)
        return port

    env_port = os.environ.get("ESPFLASH_PORT")
    if env_port:
        port = normalize_serial_port(env_port)
        if validate:
            ensure_serial_port_available(port)
        return port

    if os.name == "nt":
        settings_path = os.path.join(os.path.dirname(project_dir), ".vscode", "settings.json")
        if os.path.exists(settings_path):
            try:
                with open(settings_path, encoding="utf-8") as settings_file:
                    settings = json.load(settings_file)
                port = settings.get("idf.portWin")
                if port:
                    return normalize_serial_port(port)
            except (OSError, json.JSONDecodeError):
                pass

    available_ports = available_serial_ports()
    if len(available_ports) == 1:
        return available_ports[0]

    if os.name == "nt":
        return DEFAULT_WINDOWS_PORT

    return None


def resolve_espflash_chip(profile=None):
    explicit_chip = os.environ.get("AETHERSENSE_ESPFLASH_CHIP")
    if explicit_chip and explicit_chip.strip():
        explicit_chip = explicit_chip.strip()
        expected_chip = (profile or resolve_hardware_profile())["chip"]
        if explicit_chip != expected_chip:
            fail(
                f"[ERROR] AETHERSENSE_ESPFLASH_CHIP={explicit_chip} conflicts with profile chip {expected_chip}."
            )
        return explicit_chip

    return (profile or resolve_hardware_profile())["chip"]


def flash_baud_for_profile(profile):
    if profile["profile"] == "esp32c5":
        return ESP32C5_SAFE_FLASH_BAUD
    return DEFAULT_FLASH_BAUD


def detect_local_udp_host():
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
            probe.connect(("8.8.8.8", 80))
            host = probe.getsockname()[0]
    except OSError:
        return None

    if host and not host.startswith("127."):
        return host
    return None


def apply_profile_environment(env, profile, sink):
    runtime_only = (
        "AETHERSENSE_DEVICE_PROFILE_JSON",
        "AETHERSENSE_WIFI_SSID",
        "AETHERSENSE_WIFI_PASSWORD",
        "AETHERSENSE_WIFI_BAND",
        "AETHERSENSE_WIFI_PREFER_HT40",
        "AETHERSENSE_WIFI_MAX_RETRY",
        "AETHERSENSE_UDP_HOST",
        "AETHERSENSE_UDP_PORT",
        "AETHERSENSE_SEND_FREQUENCY_HZ",
        "AETHERSENSE_QUEUE_CAPACITY",
        "AETHERSENSE_RGB_LED_ENABLED",
        "AETHERSENSE_RGB_LED_GPIO",
        "AETHERSENSE_MQTT_PASSWORD",
        "AETHERSENSE_SERVER_BEARER_TOKEN",
        "AETHERSENSE_HEALTH_CHECK_SECS",
        "AETHERSENSE_REBOOT_AFTER_SECS",
        "AETHERSENSE_STALL_REBOOT_SECS",
        "AETHERSENSE_WIFI_AUTORECONNECT",
        "AETHERSENSE_CAPTIVE_PORTAL_TIMEOUT_SECS",
    )
    removed = [key for key in runtime_only if env.pop(key, None)]
    if removed:
        emit(
            sink,
            "[INFO] Runtime profile values were excluded from the build; "
            "they are applied atomically through NVS provisioning.",
        )
    env["MCU"] = profile["mcu"]
    env["AETHERSENSE_MCU"] = profile["mcu"]
    env["AETHERSENSE_HARDWARE_PROFILE"] = profile["profile"]

    expected_idf = profile.get("esp_idf_version")
    if profile["profile"] == "esp32c5":
        current_idf = env.get("ESP_IDF_VERSION", "").strip()
        if expected_idf and current_idf and current_idf != expected_idf:
            emit(
                sink,
                f"[WARN] Ignoring external ESP_IDF_VERSION={current_idf}; "
                f"the canonical ESP32-C5 profile requires {expected_idf}.",
            )
        if expected_idf:
            env["ESP_IDF_VERSION"] = expected_idf
    elif expected_idf and not env.get("ESP_IDF_VERSION"):
        env["ESP_IDF_VERSION"] = expected_idf
    elif expected_idf and env.get("ESP_IDF_VERSION", "").strip() != expected_idf:
        emit(
            sink,
            "[WARN] Overriding ESP_IDF_VERSION="
            f"{env.get('ESP_IDF_VERSION', '').strip()} with supported {expected_idf} "
            f"for {profile['profile']}.",
        )
        env["ESP_IDF_VERSION"] = expected_idf

    if profile["profile"] == "esp32c5":
        env["ESPFLASH_BAUD"] = str(ESP32C5_SAFE_FLASH_BAUD)
        emit(sink, f"[INFO] ESP32-C5 flash baud forced to {ESP32C5_SAFE_FLASH_BAUD}.")


def emit_effective_firmware_env(env, profile, sink) -> None:
    keys = [
        "AETHERSENSE_HARDWARE_PROFILE",
        "AETHERSENSE_BUILD_HASH",
        "ESP_IDF_VERSION",
    ]
    values = []
    for key in keys:
        value = env.get(key, "").strip()
        if value:
            values.append(f"{key}={value}")

    flash_baud = flash_baud_for_profile(profile)
    if flash_baud:
        values.append(f"ESPFLASH_BAUD={flash_baud}")

    emit(sink, "[INFO] Effective firmware env: " + ", ".join(values))


def _python_runtime_ok(exe) -> bool:
    """Retorna se o executável serve ao bootstrap interno do ESP-IDF.

    Esse runtime auxiliar é independente do Python >=3.11 que executa build.py.
    A verificação também rejeita naturalmente o alias inoperante da Microsoft
    Store, pois ele termina com código diferente de zero.
    """
    probe = (
        "import sys;"
        "print(sys.version_info[0], sys.version_info[1],"
        " int(sys._is_gil_enabled() if hasattr(sys, '_is_gil_enabled') else True))"
    )
    try:
        res = subprocess.run(
            [exe, "-c", probe],
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return False
    if res.returncode != 0:
        return False
    parts = (res.stdout or "").split()
    if len(parts) < 3:
        return False
    try:
        major, minor, gil = int(parts[0]), int(parts[1]), int(parts[2])
    except ValueError:
        return False
    return major == 3 and 9 <= minor <= 13 and gil == 1


def _usable_python_on_path(env) -> str | None:
    names = ("python.exe", "python3.exe") if os.name == "nt" else ("python3", "python")
    seen = set()
    for raw_dir in env.get("PATH", "").split(os.pathsep):
        directory = raw_dir.strip().strip('"')
        key = directory.lower()
        if not directory or key in seen:
            continue
        seen.add(key)
        for name in names:
            candidate = os.path.join(directory, name)
            if os.path.isfile(candidate) and _python_runtime_ok(candidate):
                return candidate
    return None


def _uv_python_find(uv_exe, request) -> str | None:
    try:
        res = subprocess.run(
            [uv_exe, "python", "find", request],
            capture_output=True,
            text=True,
            timeout=120,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if res.returncode != 0 or not res.stdout:
        return None
    path = res.stdout.strip().splitlines()[0].strip() if res.stdout.strip() else ""
    return path or None


def _find_or_install_espidf_python(uv_exe, sink) -> str | None:
    # Prefere 3.13 (os venvs do ESP-IDF nesta maquina sao idf5.x_py3.13_env).
    for request in ("3.13", "3.12", "3.11", "3.10", "3.9"):
        exe = _uv_python_find(uv_exe, request)
        if exe and _python_runtime_ok(exe):
            return exe
    emit(sink, "[INFO] Instalando CPython 3.13 via uv para o ESP-IDF (uma vez so)...")
    subprocess.run(
        [uv_exe, "python", "install", "3.13"],
        capture_output=True,
        text=True,
        check=False,
    )
    exe = _uv_python_find(uv_exe, "3.13")
    if exe and _python_runtime_ok(exe):
        return exe
    return None


def ensure_espidf_bootstrap_python(env, sink) -> None:
    """Garante um `python` compativel com o ESP-IDF (3.9-3.13, com GIL) no PATH do
    cargo. O esp-idf-sys chama `python` puro ao (re)instalar/exportar o IDF, mas o
    runtime atual ou um alias da Microsoft Store pode não ser aceito. Usa o uv para
    localizar (ou baixar) um CPython compatível e prefixa seu diretório no PATH."""
    existing = _usable_python_on_path(env)
    if existing:
        emit(sink, f"[INFO] Python do ESP-IDF: {existing} (ja no PATH).")
        return

    uv_exe = shutil.which("uv")
    if not uv_exe:
        emit(
            sink,
            "[WARN] Nenhum python 3.9-3.13 no PATH e uv ausente; se o ESP-IDF "
            "precisar (re)instalar, o build falhara com 'python not found'.",
        )
        return

    exe = _find_or_install_espidf_python(uv_exe, sink)
    if not exe:
        emit(
            sink,
            "[WARN] Nao foi possivel obter um python 3.9-3.13 via uv; se o ESP-IDF "
            "precisar (re)instalar, o build pode falhar com 'python not found'.",
        )
        return

    python_dir = os.path.dirname(os.path.abspath(exe))
    env["PATH"] = python_dir + os.pathsep + env.get("PATH", "")
    emit(sink, f"[INFO] Python do ESP-IDF garantido no PATH: {exe}")


def ensure_sdkconfig_files(settings: BuildSettings, sink) -> None:
    """Regenera sdkconfig/defaults from the canonical, credential-free template.

    Device configuration now comes from the selected DeviceProfile/NVS, so stale
    local sdkconfig files must not silently change otherwise reproducible builds.

    Substitui o caminho RELATIVO da partition table por um ABSOLUTO: o esp-idf-sys
    resolve CONFIG_PARTITION_TABLE_CUSTOM_FILENAME relativo ao seu projeto sintetico
    (target/.../out), entao um caminho relativo nao acharia partitions.csv."""
    example = os.path.join(settings.project_dir, "sdkconfig.example")
    if not os.path.exists(example):
        return
    try:
        template = Path(example).read_text(encoding="utf-8")
    except OSError as exc:
        emit(sink, f"[WARN] Nao foi possivel ler sdkconfig.example: {exc}")
        return

    config_root = settings.sandbox_dir if os.name == "nt" else settings.project_dir
    partitions_abs = os.path.join(config_root, "partitions.csv").replace("\\", "/")
    content = template.replace('"partitions.csv"', f'"{partitions_abs}"')

    for name in ("sdkconfig", "sdkconfig.defaults"):
        target = os.path.join(settings.project_dir, name)
        try:
            existing = Path(target).read_text(encoding="utf-8") if os.path.exists(target) else None
            if existing != content:
                temporary = Path(f"{target}.{os.getpid()}.tmp")
                temporary.write_text(content, encoding="utf-8")
                os.replace(temporary, target)
                emit(sink, f"[INFO] {name} regenerado a partir de sdkconfig.example.")
        except OSError as exc:
            emit(sink, f"[WARN] Nao foi possivel regenerar {name}: {exc}")


def prepare_espidf_environment(env, settings: BuildSettings, sink=None) -> None:
    env["ESP_IDF_TOOLS_INSTALL_DIR"] = "global"
    env["IDF_PYTHON_CHECK_IN_VENV"] = "0"
    env["ESP_IDF_SDKCONFIG_DEFAULTS"] = os.path.join(
        settings.sandbox_dir if os.name == "nt" else settings.project_dir,
        "sdkconfig.defaults",
    )

    for var in PYTHON_ENV_VARS_TO_REMOVE:
        env.pop(var, None)

    if "PATH" in env:
        current_python_dir = os.path.dirname(os.path.abspath(sys.executable)).lower()
        filtered_paths = []
        for raw_path in env["PATH"].split(os.pathsep):
            normalized = os.path.abspath(raw_path).lower()
            if normalized == current_python_dir:
                continue
            if "\\.venv\\" in normalized or normalized.endswith("\\.venv\\scripts"):
                continue
            if "\\uv\\" in normalized or "\\uv\\cache\\" in normalized:
                continue
            if "\\.uv\\" in normalized:
                continue
            filtered_paths.append(raw_path)
        env["PATH"] = os.pathsep.join(filtered_paths)

    # Após filtrar o venv atual e os diretórios internos do uv, recolocamos um
    # Python aceito pelo esp-idf-sys para ele conseguir (re)instalar o IDF.
    ensure_espidf_bootstrap_python(env, sink)


def validate_connected_chip(project_dir, profile, port, env, sink):
    if not port:
        emit(sink, "[WARN] No serial port resolved; skipping chip validation.")
        return

    command = [
        "cargo",
        "espflash",
        "board-info",
        "--non-interactive",
        "--chip",
        resolve_espflash_chip(profile),
        "--port",
        port,
    ]
    baud = flash_baud_for_profile(profile)
    if baud:
        command.extend(["--baud", str(baud)])

    emit(sink, f"[INFO] Validating {port} is {profile['chip']} before flashing...")
    result = subprocess.run(
        command,
        cwd=project_dir,
        env=env,
        capture_output=True,
        text=True,
        errors="replace",
        check=False,
    )
    output = (result.stdout or "") + (result.stderr or "")
    if output.strip():
        emit(sink, output.rstrip())

    expected = f"Chip type:         {profile['chip']}"
    if result.returncode != 0 or expected not in output:
        fail(f"[ERROR] Refusing to flash: {port} did not validate as {profile['chip']}.")


def monitor_failure_message(text: str) -> str | None:
    lower_text = text.lower()
    for needle, message in CRITICAL_MONITOR_PATTERNS:
        if needle in lower_text:
            return message
    return None


def extract_log_key_value(text: str, key: str) -> str | None:
    needle = f"{key}="
    index = text.find(needle)
    if index < 0:
        return None

    start = index + len(needle)
    end = start
    while end < len(text) and not text[end].isspace() and text[end] not in {",", ";"}:
        end += 1

    value = text[start:end].strip().strip('"').strip("'")
    return value or None


def runtime_config_diagnostics(text: str, env: dict[str, str]) -> list[str]:
    if "AetherSense config:" not in text:
        return []

    diagnostics = []
    runtime_udp_host = extract_log_key_value(text, "udp_host")
    expected_udp_host = env.get("AETHERSENSE_UDP_HOST", "").strip()
    if runtime_udp_host and expected_udp_host and runtime_udp_host != expected_udp_host:
        diagnostics.append(
            "[WARN] Firmware monitorado esta usando "
            f"AETHERSENSE_UDP_HOST={runtime_udp_host}, mas esta sessao do build.py "
            f"preparou AETHERSENSE_UDP_HOST={expected_udp_host}. "
            "O dashboard/backend nesse host nao recebera esse binario; rode flash/run "
            "novamente com o IP pretendido."
        )

    return diagnostics


def monitor_serial_output(
    port,
    sink,
    baud=DEFAULT_MONITOR_BAUD,
    timeout_sec=None,
    controller: ProcessController | None = None,
    env: dict[str, str] | None = None,
):
    try:
        import serial
    except ImportError:
        emit(
            sink,
            "[ERROR] 'pyserial' is required for monitor mode. Please run this script with 'uv run'.",
        )
        return 1

    emit_progress(
        sink,
        60,
        f"[STEP] Opening serial monitor on {port} at {baud} baud...",
        "monitor",
    )
    try:
        ser = serial.serial_for_url(port, baudrate=baud, timeout=0.1, do_not_open=True)
        ser.dtr = False
        ser.rts = False
        ser.open()
        with ser:
            try:
                ser.dtr = False
                ser.rts = False
            except serial.SerialException:
                pass

            deadline = None
            if timeout_sec is not None and timeout_sec > 0:
                deadline = time.time() + timeout_sec

            buffer = ""
            emitted_diagnostics = set()
            while True:
                if controller is not None and controller.cancel_requested:
                    emit(sink, "\n[INFO] Monitor cancelled.")
                    return 130

                if deadline is not None and time.time() >= deadline:
                    emit(sink, f"\n[INFO] Monitor timeout of {timeout_sec}s reached.")
                    return 0

                try:
                    chunk = ser.read(ser.in_waiting or 1)
                except serial.SerialException as exc:
                    emit(sink, f"\n[ERROR] Serial read failed: {exc}")
                    return 1

                if not chunk:
                    continue

                text = chunk.decode("utf-8", errors="replace")
                sink.write(text)

                buffer = (buffer + text)[-4096:]
                if env is not None:
                    for diagnostic in runtime_config_diagnostics(buffer, env):
                        if diagnostic not in emitted_diagnostics:
                            emitted_diagnostics.add(diagnostic)
                            emit(sink, f"\n{diagnostic}")

                failure_message = monitor_failure_message(buffer)
                if failure_message:
                    emit(sink, f"\n{failure_message}")
                    return 1
    except KeyboardInterrupt:
        emit(sink, "\n[INFO] Stopped by user.")
        return 0
    except serial.SerialException as exc:
        emit(sink, f"\n[ERROR] Could not open serial port {port}: {exc}")
        return 1


def stop_process_tree(process):
    if process.poll() is not None:
        return

    if os.name == "nt":
        subprocess.run(
            ["taskkill", "/PID", str(process.pid), "/T", "/F"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
    else:
        process.kill()


def run_command(
    cmd,
    env,
    working_dir,
    sink,
    parse_monitor=False,
    timeout_sec=None,
    timeout_starts_immediately=False,
    controller: ProcessController | None = None,
):
    emit_progress(sink, 60, "[STEP] Command started.", "command")
    emit(sink, f"[CMD] {command_text(cmd)}")
    process = None
    try:
        process = subprocess.Popen(
            cmd,
            env=env,
            cwd=working_dir,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1,
            encoding="utf-8",
            errors="replace",
        )
        if controller is not None:
            controller.attach(process)

        error_detected = False
        timer = None
        timer_started = False
        timed_out = False
        streamed_lines = 0
        streamed_progress = 60
        emitted_diagnostics = set()
        recent_output = collections.deque(maxlen=600)

        def kill_process():
            nonlocal timed_out
            timed_out = True
            emit(sink, f"\n[INFO] Timeout of {timeout_sec}s reached. Stopping monitor.")
            stop_process_tree(process)

        def fail_with_error(message):
            nonlocal error_detected
            emit(sink, message)
            error_detected = True
            time.sleep(1)
            stop_process_tree(process)

        if timeout_sec is not None and timeout_sec > 0 and timeout_starts_immediately:
            timer = threading.Timer(timeout_sec, kill_process)
            timer.start()
            timer_started = True

        try:
            assert process.stdout is not None
            for line in iter(process.stdout.readline, ""):
                if controller is not None and controller.cancel_requested:
                    continue

                sink.write(line)
                recent_output.append(line)

                for diagnostic in runtime_config_diagnostics(line, env):
                    if diagnostic not in emitted_diagnostics:
                        emitted_diagnostics.add(diagnostic)
                        emit(sink, f"\n{diagnostic}")

                streamed_lines += 1
                if streamed_lines % 25 == 0 and streamed_progress < 90:
                    streamed_progress = min(90, streamed_progress + 1)
                    emit_progress(sink, streamed_progress, stage="command-output")

                if (
                    timeout_sec is not None
                    and timeout_sec > 0
                    and not timer_started
                    and any(marker in line for marker in MONITOR_TIMER_START_PATTERNS)
                ):
                    timer = threading.Timer(timeout_sec, kill_process)
                    timer.start()
                    timer_started = True

                if parse_monitor:
                    failure_message = monitor_failure_message(line)
                    if failure_message:
                        fail_with_error(f"\n{failure_message}")
                        break

            process.wait()
        finally:
            if timer is not None:
                timer.cancel()
            if controller is not None and process is not None:
                controller.detach(process)

        if controller is not None and controller.cancel_requested:
            emit(sink, "\n[INFO] Command cancelled.")
            return 130

        if (error_detected or process.returncode not in (0, None)) and not timed_out:
            hints = summarize_failure(recent_output)
            if hints:
                emit(sink, "\n[DIAGNOSTICO] Causa provavel da falha:")
                for hint in hints:
                    emit(sink, f"  - {hint}")
            emit(sink, "[INFO] Log completo em firmware-rust/build.log")
            fail(f"\n[ERROR] Command failed with exit code {process.returncode}")

        emit_progress(sink, 92, "[STEP] Command finished.", "command-finished")
        return 0

    except KeyboardInterrupt:
        emit(sink, "\n[INFO] Stopped by user.")
        if process is not None:
            stop_process_tree(process)
        return 0
    except FileNotFoundError:
        fail(f"\n[ERROR] Command not found: {cmd[0]}")


def reset_to_bootloader(port, sink, baud=DEFAULT_MONITOR_BAUD):
    try:
        import serial
    except ImportError:
        fail("[ERROR] 'pyserial' is required for rescue mode. Please run this script with 'uv run'.")

    emit(sink, f"[INFO] Forcing {port} into bootloader mode using DTR/RTS...")
    ser = serial.serial_for_url(port, baudrate=baud, do_not_open=True)
    ser.dtr = False
    ser.rts = False
    ser.open()
    try:
        ser.setDTR(False)
        ser.setRTS(True)
        ser.setDTR(ser.dtr)
        time.sleep(0.1)
        ser.setDTR(True)
        ser.setRTS(False)
        ser.setDTR(ser.dtr)
        time.sleep(0.5)
        ser.setDTR(False)
        ser.setDTR(ser.dtr)
        time.sleep(1.0)
        emit(sink, "[INFO] Board should be in bootloader mode now.")
    finally:
        ser.close()
        time.sleep(0.5)


def sync_windows_sandbox(settings: BuildSettings, sink) -> str:
    if os.name != "nt":
        return settings.project_dir

    sandbox = Path(settings.sandbox_dir).resolve()
    sandbox.relative_to(Path(DEFAULT_SANDBOX_DIR).resolve())
    if sandbox == Path(DEFAULT_SANDBOX_DIR).resolve():
        fail("[ERROR] Refusing to mirror into the sandbox root.")

    emit_progress(
        sink,
        28,
        f"[STEP] Syncing project to {settings.sandbox_dir} to bypass path limits...",
        "sandbox",
    )
    robo_cmd = [
        "robocopy",
        settings.project_dir,
        settings.sandbox_dir,
        "/MIR",
        "/XD",
        "target",
        ".git",
        ".aethersense_build",
        ".ae",
        ".embuild",
        "/R:2",
        "/W:5",
        "/NFL",
        "/NDL",
        "/NJH",
        "/NJS",
    ]
    robo = subprocess.run(robo_cmd, capture_output=True, text=True, errors="replace", check=False)
    if robo.returncode >= 8:
        fail(
            f"[ERROR] Sync failed with exit code {robo.returncode}:\n{robo.stdout}\n{robo.stderr}",
            robo.returncode,
        )

    metadata = {
        "schema": "aethersense.build_sandbox.v1",
        "project_dir": str(Path(settings.project_dir).resolve()),
        "profile": settings.sandbox_profile,
        "pid": os.getpid(),
        "thread_id": threading.get_ident(),
        "created_at": datetime.datetime.now(tz=datetime.timezone.utc).isoformat(),
    }
    (sandbox / ".aethersense-sandbox.json").write_text(
        json.dumps(metadata, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    emit_progress(sink, 38, "[STEP] Sandbox sync finished.", "sandbox")
    return settings.sandbox_dir


def cleanup_windows_sandbox(
    settings: BuildSettings, sink, *, remove_target: bool = False
) -> None:
    if os.name != "nt":
        return
    sandbox = Path(settings.sandbox_dir).resolve()
    try:
        sandbox.relative_to(Path(DEFAULT_SANDBOX_DIR).resolve())
    except ValueError:
        fail(f"[ERROR] Refusing to clean unsafe sandbox path: {sandbox}")
    if sandbox == Path(DEFAULT_SANDBOX_DIR).resolve():
        fail("[ERROR] Refusing to clean the sandbox root.")
    emit_progress(sink, 95, f"[STEP] Cleaning up {settings.sandbox_dir}...", "cleanup")
    time.sleep(1)
    remove_directory(settings.sandbox_dir, sink)
    if remove_target and settings.target_dir:
        target = Path(settings.target_dir).resolve()
        target_root = Path(DEFAULT_TARGET_ROOT).resolve()
        try:
            target.relative_to(target_root)
        except ValueError:
            fail(f"[ERROR] Refusing to clean unsafe Cargo target path: {target}")
        if target == target_root:
            fail("[ERROR] Refusing to clean the Cargo target root.")
        remove_directory(target, sink)
    emit_progress(sink, 98, "[STEP] C:\\ drive is clean.", "cleanup")


def execute_monitor_action(
    options: BuildOptions,
    settings: BuildSettings,
    profile,
    env,
    sink,
    controller: ProcessController | None,
) -> int:
    monitor_port = resolve_monitor_port(settings.project_dir, options.port)
    if not monitor_port:
        fail(
            "[ERROR] Could not determine the serial port for monitor mode. Pass --port COM5 or set ESPFLASH_PORT / idf.portWin."
        )

    if profile["profile"] == "esp32c5":
        cmd = build_espflash_monitor_command(profile=profile, port=monitor_port)
        emit_progress(
            sink,
            50,
            "[INFO] Opening reset-aware ESP32-C5 monitor on "
            f"{monitor_port} at {DEFAULT_MONITOR_BAUD} baud...",
            "monitor",
        )
        return run_command(
            cmd,
            env,
            settings.project_dir,
            sink,
            parse_monitor=True,
            timeout_sec=options.timeout,
            timeout_starts_immediately=True,
            controller=controller,
        )

    return monitor_serial_output(
        monitor_port,
        sink,
        timeout_sec=options.timeout,
        controller=controller,
        env=env,
    )


def prepare_action_command(options: BuildOptions, settings: BuildSettings, profile, env, sink):
    if options.action == "test":
        emit(sink, "[INFO] Running host-side firmware tests (no ESP-IDF target/sandbox)...")
        return [
            "cargo",
            "test",
            "--target",
            rust_host_target(),
            "--features",
            "esp-idf",
            "--",
            "--nocapture",
        ], False

    if options.action in {"clean", "fullclean"}:
        emit(sink, "[INFO] Cleaning project cargo cache...")
        return ["cargo", "+esp", "clean"], False

    if options.action == "build":
        emit(
            sink,
            "[INFO] Building firmware for "
            f"{profile['profile']} ({profile['target']}, MCU={profile['mcu']})...",
        )
        return build_cargo_build_command(profile), False

    if options.action == "flash":
        flash_port = resolve_monitor_port(settings.project_dir, options.port)
        emit_progress(sink, 42, "[STEP] Validating connected chip before flashing...", "validate")
        validate_connected_chip(settings.project_dir, profile, flash_port, env, sink)
        emit(
            sink,
            "[INFO] Flashing firmware for "
            f"{profile['profile']} ({profile['target']}, chip={profile['chip']})...",
        )
        return (
            build_espflash_command(profile=profile, port=flash_port),
            False,
        )

    if options.action == "erase":
        erase_port = resolve_monitor_port(settings.project_dir, options.port)
        emit_progress(sink, 42, "[STEP] Validating connected chip before erasing...", "validate")
        validate_connected_chip(settings.project_dir, profile, erase_port, env, sink)
        emit(sink, f"[INFO] Erasing flash for {profile['profile']} (chip={profile['chip']})...")
        return (
            build_espflash_erase_command(
                profile=profile,
                port=erase_port,
            ),
            False,
        )

    if options.action == "run":
        run_port = resolve_monitor_port(settings.project_dir, options.port)
        emit_progress(sink, 42, "[STEP] Validating connected chip before run...", "validate")
        validate_connected_chip(settings.project_dir, profile, run_port, env, sink)
        emit(
            sink,
            "[INFO] Building, flashing and monitoring "
            f"{profile['profile']} ({profile['target']}, chip={profile['chip']})...",
        )
        return (
            build_espflash_command(
                profile=profile,
                monitor=True,
                port=run_port,
            ),
            True,
        )

    if options.action == "rescue":
        rescue_port = resolve_monitor_port(settings.project_dir, options.port)
        if not rescue_port:
            fail("[ERROR] --port is required for rescue mode.")
        emit_progress(sink, 42, "[STEP] Validating connected chip before rescue...", "validate")
        validate_connected_chip(settings.project_dir, profile, rescue_port, env, sink)
        emit_progress(sink, 48, "[STEP] Forcing bootloader mode...", "rescue")
        reset_to_bootloader(rescue_port, sink)
        emit(sink, "[INFO] Rescuing board and monitoring...")
        return (
            build_espflash_command(
                profile=profile,
                monitor=True,
                no_reset=True,
                port=rescue_port,
            ),
            True,
        )

    fail(f"[ERROR] Unknown action: {options.action}")


def execute_action(
    options: BuildOptions,
    settings: BuildSettings,
    sink,
    controller: ProcessController | None = None,
) -> int:
    if options.action not in ACTIONS:
        fail(f"[ERROR] Unknown action: {options.action}")

    if options.action == "flashall":
        return execute_flash_all(options, settings, sink, controller)

    emit(
        sink,
        f"\n{'=' * 60}\n[START] {datetime.datetime.now(tz=datetime.timezone.utc).astimezone().strftime('%Y-%m-%d %H:%M:%S')} - Action: {options.action}\n{'=' * 60}",
    )
    emit_progress(sink, 5, "[STEP] Build request received.", "start")

    emit_progress(sink, 12, "[STEP] Resolving hardware profile.", "profile")
    profile = resolve_hardware_profile(options.profile, sink=sink)
    if options.device_profile is not None and options.action in {
        "flash",
        "erase",
        "run",
        "rescue",
    }:
        if not options.port:
            try:
                options.port = resolve_profile_port(
                    options.device_profile, available_serial_port_infos()
                )
            except DevicePairingError as exc:
                raise BuildFailure(f"[ERROR] {exc}") from exc
        if not options.port:
            raise BuildFailure(
                "[ERROR] Paired device is not present; refusing a destructive action"
            )
        try:
            identity = probe_board(options.port)
        except DevicePairingError as exc:
            raise BuildFailure(f"[ERROR] Identity preflight failed: {exc}") from exc
        expected_mac = options.device_profile.association.efuse_mac.lower()
        if identity.device_id != options.device_profile.device_id or (
            expected_mac and identity.mac != expected_mac
        ):
            raise BuildFailure(
                "[ERROR] Connected board eFuse MAC differs from the paired profile; "
                "flash/erase/rescue was refused"
            )
        if identity.target_profile != options.device_profile.target.profile:
            raise BuildFailure(
                "[ERROR] Connected board chip differs from the paired profile target"
            )
    if ACTIONS[options.action].requires_sandbox:
        settings = isolated_build_settings(settings, profile["profile"])
    emit_progress(sink, 18, "[STEP] Preparing command environment.", "environment")
    env = os.environ.copy()
    for key, value in options.env_overrides.items():
        value = str(value).strip()
        if value:
            env[key] = value
    if hasattr(sink, "add_secret_values"):
        sink.add_secret_values(
            value for key, value in env.items() if value and is_secret_key(key)
        )
    apply_profile_environment(env, profile, sink)
    if settings.target_dir:
        env["CARGO_TARGET_DIR"] = settings.target_dir
    emit_effective_firmware_env(env, profile, sink)

    if options.action == "monitor":
        emit_progress(sink, 35, "[STEP] Starting monitor mode.", "monitor")
        return execute_monitor_action(options, settings, profile, env, sink, controller)

    if options.action == "test":
        cmd, parse_monitor = prepare_action_command(options, settings, profile, env, sink)
        return run_command(
            cmd,
            env,
            settings.project_dir,
            sink,
            parse_monitor=parse_monitor,
            timeout_sec=options.timeout,
            controller=controller,
        )

    working_dir = settings.project_dir
    try:
        ensure_sdkconfig_files(settings, sink)
        emit_progress(sink, 22, "[STEP] Preparing ESP-IDF environment.", "espidf")
        prepare_espidf_environment(env, settings, sink)
        # ESP-IDF is treated as immutable. ESP32-C5 v5.5.4 supports the revision
        # used by this project; build.py must never patch the global installation.

        if ACTIONS[options.action].requires_sandbox:
            working_dir = sync_windows_sandbox(settings, sink)

        cmd, parse_monitor = prepare_action_command(options, settings, profile, env, sink)
        emit_progress(sink, 55, "[STEP] Command prepared.", "command")
        exit_code = run_command(
            cmd,
            env,
            working_dir,
            sink,
            parse_monitor=parse_monitor,
            timeout_sec=options.timeout,
            controller=controller,
        )

        if options.action == "fullclean":
            emit_progress(
                sink,
                93,
                "[STEP] Performing FULL CLEAN (removing ESP-IDF CMake cache)...",
                "fullclean",
            )
            for directory in [
                os.path.join(settings.sandbox_dir, ".embuild"),
                os.path.join(settings.project_dir, ".embuild"),
            ]:
                remove_directory(directory, sink)

        emit_progress(sink, 94, "[STEP] Action finished.", "finished")
        return exit_code
    finally:
        if ACTIONS[options.action].requires_sandbox:
            cleanup_windows_sandbox(
                settings,
                sink,
                remove_target=options.action in {"clean", "fullclean"},
            )


def execute_flash_all(
    options: BuildOptions,
    settings: BuildSettings,
    sink,
    controller: ProcessController | None = None,
) -> int:
    """Detecta os ESP32 conectados e faz build+flash de cada um, com o perfil certo,
    em sequencia (o sandbox C:\\fw e compartilhado). Ignora portas Bluetooth."""
    emit(
        sink,
        f"\n{'=' * 60}\n[START] {datetime.datetime.now(tz=datetime.timezone.utc).astimezone().strftime('%Y-%m-%d %H:%M:%S')} - Action: flashall\n{'=' * 60}",
    )

    infos = [info for info in available_serial_port_infos() if not is_bluetooth_port(info)]
    if not infos:
        fail(
            "[ERROR] Nenhuma porta serial de ESP32 detectada. Conecte os dispositivos "
            "por USB (e instale o pyserial via 'uv run')."
        )

    targets = []
    for info in infos:
        port = info["device"]
        profile = info.get("suggested_profile") or "unknown"
        if profile == "unknown":
            emit(
                sink,
                f"[WARN] {port} ({info.get('description') or '?'}) — perfil nao reconhecido; "
                "pulando. Grave manualmente com --port/--profile.",
            )
            continue
        targets.append((port, profile, info))

    if not targets:
        fail("[ERROR] Nenhum ESP32 reconhecido entre as portas conectadas.")

    emit(
        sink,
        "[INFO] Dispositivos detectados: "
        + ", ".join(f"{port}->{profile}" for port, profile, _ in targets),
    )

    results = []
    for port, profile, info in targets:
        emit(
            sink,
            f"\n{'#' * 60}\n[ALVO] {profile} em {port} ({info.get('description') or '?'})\n{'#' * 60}",
        )
        device_options = BuildOptions(
            action="flash",
            port=port,
            profile=profile,
            timeout=options.timeout,
            env_overrides=dict(options.env_overrides),
        )
        try:
            code = execute_action(device_options, settings, sink, controller)
        except BuildFailure as exc:
            sink.line(exc.message)
            code = exc.exit_code or 1
        results.append((port, profile, code))

    emit(sink, f"\n{'=' * 60}\n[RESUMO] Flash de {len(results)} dispositivo(s):")
    any_failed = False
    for port, profile, code in results:
        status = "OK" if code == 0 else f"FALHOU (exit {code})"
        if code != 0:
            any_failed = True
        emit(sink, f"  - {profile} @ {port}: {status}")
    emit(sink, "=" * 60)
    return 1 if any_failed else 0


def build_tui_config_path() -> Path:
    if os.name == "nt":
        root = os.environ.get("LOCALAPPDATA")
        if root:
            return Path(root) / "AetherSense" / "build_tui.json"
        return Path.home() / "AppData" / "Local" / "AetherSense" / "build_tui.json"

    root = os.environ.get("XDG_CONFIG_HOME")
    if root:
        return Path(root) / "aethersense" / "build_tui.json"
    return Path.home() / ".config" / "aethersense" / "build_tui.json"


def load_tui_preferences() -> dict[str, Any]:
    path = build_tui_config_path()
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}


def saved_tui_theme_name(available_theme_names) -> str | None:
    saved_theme = load_tui_preferences().get("theme")
    if isinstance(saved_theme, str) and saved_theme in set(available_theme_names):
        return saved_theme
    return None


def save_tui_preferences(preferences: dict[str, Any]) -> None:
    path = build_tui_config_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(preferences, indent=2, sort_keys=True), encoding="utf-8")


def create_tui_app(
    settings: BuildSettings,
    initial_options: BuildOptions | None = None,
    *,
    profile_store: DeviceProfileStore | None = None,
    secret_manager: SecretManager | None = None,
    port_provider=None,
):
    """Create the multi-device TUI while keeping the build executor importable.

    Dependency injection is intentional: Textual headless tests use a temporary
    catalogue and a deterministic port provider without touching real hardware.
    """
    from aethersense_build.tui import create_app

    return create_app(
        sys.modules[__name__],
        settings,
        initial_options,
        profile_store=profile_store,
        secret_manager=secret_manager,
        port_provider=port_provider,
    )


def run_tui(settings: BuildSettings, initial_options: BuildOptions | None = None) -> int:
    app = create_tui_app(settings, initial_options)
    app.run()
    return app.return_code or 0


def load_device_profile_argument(
    value: str,
    *,
    store: DeviceProfileStore | None = None,
) -> DeviceProfile:
    """Load a local catalogue UUID or a JSON/single-profile ZIP without importing it."""

    store = store or DeviceProfileStore()
    candidate = Path(value).expanduser()
    if candidate.exists():
        return store.preview_import(candidate).profile
    try:
        return store.load(value)
    except FileNotFoundError as exc:
        raise BuildFailure(
            f"[ERROR] Device profile was not found as UUID or path: {value}"
        ) from exc


def _persist_cli_profile(options: BuildOptions) -> None:
    if options.device_profile is None or not options.device_profile_source:
        return
    source = Path(options.device_profile_source).expanduser()
    if source.exists():
        # Explicit interchange files are inputs, never mutated implicitly.
        return
    DeviceProfileStore().save(options.device_profile)


def _cli_manual_confirmation(prompt: str) -> bool:
    """Ask for an observation only in an interactive terminal.

    A pipe, CI job or test can collect firmware evidence, but must never turn
    that evidence into a human observation implicitly.
    """

    if not sys.stdin.isatty():
        return False
    try:
        answer = input(f"\n{prompt} [s/N]: ").strip().casefold()
    except (EOFError, KeyboardInterrupt):
        return False
    return answer in {"s", "sim", "y", "yes"}


def _integration_hardware_tester(
    profile: DeviceProfile,
    secret_manager: SecretManager,
):
    def tester(context, _check, _params):
        runtime_result, runtime_state = wait_for_runtime_snapshot(context)
        runtime_ok = bool(runtime_state.get("network_ready"))
        checks = []
        if profile.mqtt.enabled:
            checks.append(("mqtt", test_mqtt_connection(profile, secret_manager)))
        if profile.server.base_url:
            checks.append(("server", test_server_connection(profile, secret_manager)))
        evidence = {
            label: {"ok": result.ok, "message": result.message}
            for label, result in checks
        }
        evidence["firmware_runtime"] = {
            "network_ready": runtime_ok,
            "last_udp_send_secs": int(runtime_state.get("last_udp_send_secs") or 0),
            "dropped_packets": int(runtime_state.get("dropped_packets") or 0),
            "transmitted_packets": int(runtime_state.get("transmitted_packets") or 0),
            "raw_csi_captured_by_host": False,
        }
        integrations_ok = all(result.ok for _label, result in checks)
        return AutomaticTestResult(
            runtime_ok and integrations_ok,
            evidence=evidence,
            message=(
                ""
                if runtime_ok and integrations_ok
                else "a placa não confirmou rede/UDP ou uma integração falhou"
            ),
            source="usb_runtime_counters+mqtt+server_readback",
        )

    return tester


def execute_profile_service_action(
    options: BuildOptions,
    settings: BuildSettings,
    sink,
) -> int:
    emit(
        sink,
        f"\n{'=' * 60}\n[START] "
        f"{datetime.datetime.now(tz=datetime.timezone.utc).astimezone().strftime('%Y-%m-%d %H:%M:%S')} "
        f"- Action: {options.action}\n{'=' * 60}",
    )
    if options.action in {"inspect-sd", "import-sd"}:
        if not options.sd_path:
            raise BuildFailure(f"[ERROR] {options.action} requires --sd-path")
        source = Path(options.sd_path).expanduser().resolve()
        if options.action == "inspect-sd":
            sessions = inspect_sd(source)
            emit(
                sink,
                json.dumps(
                    {
                        "sessions": len(sessions),
                        "bytes": sum(session.bytes for session in sessions),
                        "source": str(source),
                    },
                    ensure_ascii=False,
                    sort_keys=True,
                ),
            )
            return 0
        destination = (
            Path(options.results_dir).expanduser().resolve()
            if options.results_dir
            else Path(settings.project_dir).parent / "resultados" / "firmware-sd"
        )
        report = import_sd_sessions(source, destination)
        emit(
            sink,
            json.dumps(
                {
                    "imported": len(report.imported),
                    "skipped": len(report.skipped),
                    "destination": str(destination),
                },
                ensure_ascii=False,
                sort_keys=True,
            ),
        )
        return 0

    profile = options.device_profile
    if profile is None:
        raise BuildFailure(f"[ERROR] {options.action} requires --device-profile <UUID|path>")
    issues = validate_profile(profile)
    if has_errors(issues):
        details = "; ".join(str(issue) for issue in issues[:5])
        raise BuildFailure(f"[ERROR] Invalid device profile: {details}")
    secret_manager = SecretManager()

    if options.action == "validate-hardware":
        secret_values = tuple(
            value
            for reference in (
                profile.network.wifi.password_ref,
                profile.mqtt.password_ref,
                profile.server.admin_bearer_token_ref,
                profile.server.bearer_token_ref,
            )
            if reference
            if (value := secret_manager.get(reference))
        )
        if hasattr(sink, "add_secret_values"):
            sink.add_secret_values(secret_values)
        service = HardwareValidationService(
            testers={
                "network_integration": _integration_hardware_tester(
                    profile, secret_manager
                )
            },
            secret_values=secret_values,
        )
        session = service.start(profile)
        emit(
            sink,
            f"[{'OK' if session.preflight.state == ValidationState.PASSED else 'BLOCKED'}] "
            f"Preflight físico: {session.preflight.reason or 'placa e hashes confirmados'}",
        )
        if session.preflight.state == ValidationState.PASSED:
            for check in session.checks.values():
                if check.state == ValidationState.SKIPPED:
                    continue
                if check.authorization_required:
                    allowed = _cli_manual_confirmation(check.authorization_prompt)
                    service.authorize(
                        check.check_id,
                        allowed,
                        "confirmação interativa da CLI" if allowed else "não confirmada",
                    )
                    if not allowed:
                        emit(sink, f"[BLOCKED] {check.label}: autorização manual necessária")
                        continue
                if check.state != ValidationState.WAITING:
                    emit(sink, f"[{check.state.value.upper()}] {check.label}: {check.reason}")
                    continue
                emit(sink, f"[STEP] Testando na placa: {check.label}")
                try:
                    result = service.run_check(check.check_id)
                except HardwareValidationError as exc:
                    emit(sink, f"[ERROR] {check.label}: {exc}")
                    continue
                if result.state == ValidationState.MANUAL_CONFIRMATION:
                    confirmed = _cli_manual_confirmation(result.manual_prompt or result.reason)
                    service.confirm_manual(
                        check.check_id,
                        confirmed,
                        "confirmação interativa da CLI" if confirmed else "efeito não confirmado",
                    )
                    result = session.checks[check.check_id]
                emit(
                    sink,
                    f"[{result.state.value.upper()}] {result.label}: "
                    f"{result.reason or 'evidência confirmada'}",
                )
        if session.physically_validated:
            profile.applied.hardware_validation_status = "passed"
            profile.applied.hardware_validation_session = session.session_id
            profile.applied.hardware_validated_at = session.completed_at or utc_now()
            profile.applied.hardware_validation_build_hash = session.expected_build_hash
            profile.applied.hardware_validation_config_hash = session.expected_config_hash
            _persist_cli_profile(options)
        report = str(service.report_path) if service.report_path else "indisponível"
        emit(sink, f"[EVIDENCE] {report}")
        emit(
            sink,
            "[OK] Placa fisicamente validada."
            if session.physically_validated
            else "[BLOCKED] A placa ainda não possui todas as provas físicas obrigatórias.",
        )
        return 0 if session.physically_validated else 1

    if options.action == "provision":
        secret_values = tuple(
            value
            for reference in (
                profile.network.wifi.password_ref,
                profile.mqtt.password_ref,
                profile.server.bearer_token_ref,
            )
            if reference
            if (value := secret_manager.get(reference))
        )
        if hasattr(sink, "add_secret_values"):
            sink.add_secret_values(secret_values)
        port = options.port
        if not port:
            try:
                port = resolve_profile_port(profile, available_serial_port_infos())
            except DevicePairingError as exc:
                raise BuildFailure(f"[ERROR] {exc}") from exc
        port = resolve_monitor_port(settings.project_dir, port)
        if not port:
            raise BuildFailure("[ERROR] Unable to resolve the associated serial port")
        client_kwargs = {
            "progress": lambda message: emit(sink, f"[STEP] {message}"),
        }
        if options.timeout is not None and options.timeout > 0:
            client_kwargs["operation_timeout"] = float(options.timeout)
        try:
            result = ProvisionClient(port, **client_kwargs).provision(
                profile, secret_manager
            )
            activate_provisioned_profile(
                port,
                profile,
                timeout=float(options.timeout or 30),
                client_factory=lambda active_port: ProvisionClient(
                    active_port,
                    **client_kwargs,
                ),
                progress=lambda message: emit(sink, f"[STEP] {message}"),
            )
        except ProvisioningTimeout as exc:
            detail = redact_text(str(exc), secret_values)
            raise BuildFailure(
                f"[ERROR] Provisioning on {port} timed out: {detail}. "
                "Check the USB connection and confirm that the firmware is running, "
                "then retry."
            ) from exc
        except (ProvisioningError, OSError, ValueError) as exc:
            detail = redact_text(str(exc), secret_values)
            raise BuildFailure(
                f"[ERROR] Provisioning on {port} failed: {detail}"
            ) from exc
        profile.applied.build_hash = result.build_hash
        profile.applied.config_hash = result.config_hash
        profile.applied.last_seen_at = utc_now()
        _persist_cli_profile(options)
        emit(
            sink,
            f"[OK] Provisioned {result.device_id} on {port}; "
            f"config_hash={result.config_hash}",
        )
        return 0

    if options.action == "test-integration":
        checks = []
        if options.integration in {"all", "mqtt"} and profile.mqtt.enabled:
            checks.append(("MQTT", test_mqtt_connection(profile, secret_manager)))
        if options.integration in {"all", "server"} and profile.server.base_url:
            checks.append(("Servidor", test_server_connection(profile, secret_manager)))
        if not checks:
            raise BuildFailure("[ERROR] No configured integration matches --integration")
        for label, result in checks:
            emit(sink, f"[{'OK' if result.ok else 'ERROR'}] {label}: {result.message}")
        return 0 if all(result.ok for _label, result in checks) else 1
    raise BuildFailure(f"[ERROR] Unsupported profile service action: {options.action}")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="AetherSense Firmware Build System (Sandboxed)")
    parser.add_argument("action", nargs="?", choices=sorted(ACTIONS.keys()))
    parser.add_argument("--port", type=str, help="Serial port (e.g. COM5)")
    parser.add_argument(
        "--profile",
        choices=sorted(HARDWARE_PROFILES.keys()),
        help="Hardware profile to build/flash/monitor (classic, esp32c5, esp32c6).",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        help="Timeout in seconds for monitor or the complete provisioning operation",
        default=None,
    )
    parser.add_argument(
        "--device-profile",
        help="Device profile UUID from the local catalogue, or a JSON/single-profile ZIP path.",
    )
    parser.add_argument(
        "--integration",
        choices=("all", "mqtt", "server"),
        default="all",
        help="Integration to validate with test-integration.",
    )
    parser.add_argument("--sd-path", help="Mounted microSD root for inspect-sd/import-sd.")
    parser.add_argument(
        "--results-dir",
        help="Override import-sd destination (default: resultados/firmware-sd).",
    )
    parser.add_argument(
        "--allow-unpaired-legacy",
        action="store_true",
        help="Explicitly allow legacy destructive CLI actions without eFuse identity checks.",
    )
    parser.add_argument("--tui", action="store_true", help="Open the Textual build TUI.")
    parser.add_argument(
        "--no-tui",
        action="store_true",
        help="Force CLI mode. With no action, print help instead of opening the TUI.",
    )
    return parser


def should_open_tui(args) -> bool:
    if args.no_tui:
        return False
    if args.tui:
        return True
    if args.action:
        return False
    if os.environ.get("CI"):
        return False
    return sys.stdin.isatty() and sys.stdout.isatty()


def run_cli(options: BuildOptions, settings: BuildSettings) -> int:
    with open(settings.log_path, "a", encoding="utf-8") as log_file:
        sink = ConsoleLogSink(sys.__stdout__, log_file)
        try:
            if options.action in PROFILE_SERVICE_ACTIONS:
                return execute_profile_service_action(options, settings, sink)
            if (
                options.action in {"flash", "erase", "run", "rescue", "flashall"}
                and options.device_profile is None
            ):
                if not options.allow_unpaired_legacy:
                    raise BuildFailure(
                        "[ERROR] Destructive CLI action requires --device-profile; "
                        "use --allow-unpaired-legacy only for an intentional legacy recovery"
                    )
                sink.line(
                    "[WARN] Legacy unpaired destructive mode explicitly enabled; "
                    "eFuse identity cannot be enforced."
                )
            if options.device_profile is not None and options.action in {
                "flash",
                "erase",
                "run",
                "rescue",
            }:
                issues = validate_profile(options.device_profile)
                if has_errors(issues):
                    raise BuildFailure(
                        "[ERROR] Invalid device profile blocks this action: "
                        + "; ".join(str(issue) for issue in issues[:5])
                    )
            if options.action == "flashall":
                return execute_flash_all(options, settings, sink)
            return execute_action(options, settings, sink)
        except BuildFailure as exc:
            sink.line(exc.message)
            return exc.exit_code


def main() -> int:
    runtime_exit = ensure_required_python_runtime()
    if runtime_exit is not None:
        return runtime_exit

    parser = build_parser()
    args = parser.parse_args()
    settings = BuildSettings(project_dir=os.path.abspath(os.path.dirname(__file__)))

    device_profile = None
    if args.device_profile:
        try:
            device_profile = load_device_profile_argument(args.device_profile)
        except (BuildFailure, ValueError, OSError, json.JSONDecodeError) as exc:
            parser.error(str(exc))
        if args.profile and args.profile != device_profile.target.profile:
            parser.error(
                f"--profile {args.profile} diverges do alvo {device_profile.target.profile} "
                "do --device-profile"
            )

    resolved_port = args.port
    if (
        device_profile is not None
        and not resolved_port
        and args.action in {"flash", "erase", "run", "rescue", "monitor", "provision"}
    ):
        try:
            resolved_port = resolve_profile_port(
                device_profile, available_serial_port_infos()
            )
        except DevicePairingError as exc:
            parser.error(str(exc))

    initial_options = BuildOptions(
        action=args.action or "run",
        port=resolved_port,
        profile=device_profile.target.profile if device_profile else args.profile,
        timeout=args.timeout,
        env_overrides=(build_environment(device_profile) if device_profile else {}),
        device_profile=device_profile,
        device_profile_source=args.device_profile,
        integration=args.integration,
        sd_path=args.sd_path,
        results_dir=args.results_dir,
        allow_unpaired_legacy=args.allow_unpaired_legacy,
    )

    if should_open_tui(args):
        return run_tui(settings, initial_options)

    if not args.action:
        parser.print_help()
        return 2

    return run_cli(initial_options, settings)


if __name__ == "__main__":
    sys.exit(main())
