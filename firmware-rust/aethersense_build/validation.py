"""Board-aware validation shared by the TUI, CLI and provisioning client."""

from __future__ import annotations

from dataclasses import dataclass
import ipaddress
from typing import Literal
from urllib.parse import urlparse

from .models import DeviceProfile, SUPPORTED_TARGETS, device_id_from_mac, normalise_mac


Direction = Literal["input", "output", "bidirectional"]


@dataclass(frozen=True)
class ValidationIssue:
    path: str
    message: str
    code: str
    severity: Literal["error", "warning"] = "error"

    def __str__(self) -> str:
        return f"{self.path}: {self.message}"


@dataclass(frozen=True)
class BoardPinSpec:
    available: frozenset[int]
    input_only: frozenset[int]
    blocked: dict[int, str]
    rmt_tx_channels: int
    rmt_rx_channels: int
    rmt_total_channels: int | None = None
    supports_accessories: bool = True


# Conservative board-level rules. Pins required for flash, boot strapping,
# firmware download or the default USB console are deliberately unavailable in
# v1: the plan has no unsafe override switch.
BOARD_SPECS: dict[str, BoardPinSpec] = {
    "classic": BoardPinSpec(
        # GPIOs actually broken out by the ESP32-WROOM-32 module. GPIO20,
        # GPIO24, GPIO28-31 and GPIO37-38 do not exist on its 38-pin footprint.
        available=frozenset(
            {
                0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11,
                12, 13, 14, 15, 16, 17, 18, 19,
                21, 22, 23, 25, 26, 27,
                32, 33, 34, 35, 36, 39,
            }
        ),
        input_only=frozenset(range(34, 40)),
        blocked={
            0: "boot strapping",
            1: "UART0 TX / provisioning console",
            2: "boot strapping",
            3: "UART0 RX / provisioning console",
            5: "boot strapping",
            6: "SPI flash",
            7: "SPI flash",
            8: "SPI flash",
            9: "SPI flash",
            10: "SPI flash",
            11: "SPI flash",
            12: "boot strapping",
            15: "boot strapping",
        },
        rmt_tx_channels=8,
        rmt_rx_channels=8,
        rmt_total_channels=8,
    ),
    "esp32c5": BoardPinSpec(
        available=frozenset(range(29)),
        input_only=frozenset(),
        blocked={
            2: "boot/JTAG strapping",
            3: "MTDI/SDIO strapping",
            4: "JTAG reservado no perfil v1",
            5: "JTAG reservado no perfil v1",
            7: "boot strapping",
            11: "UART0 TX / provisioning console",
            12: "UART0 RX / provisioning console",
            13: "USB Serial/JTAG D-",
            14: "USB Serial/JTAG D+",
            15: "SPI0/1 flash/PSRAM",
            16: "SPI0/1 flash",
            17: "SPI0/1 flash",
            18: "SPI0/1 flash",
            19: "SPI0/1 flash",
            20: "SPI0/1 flash",
            21: "SPI0/1 flash",
            22: "SPI0/1 flash",
            25: "SDIO strapping",
            26: "boot strapping",
            27: "boot strapping",
            28: "boot strapping/download mode",
        },
        rmt_tx_channels=2,
        rmt_rx_channels=2,
    ),
    "esp32c6": BoardPinSpec(
        available=frozenset(range(31)),
        input_only=frozenset(),
        blocked={},
        rmt_tx_channels=0,
        rmt_rx_channels=0,
        supports_accessories=False,
    ),
}


def _issue(
    issues: list[ValidationIssue],
    path: str,
    message: str,
    code: str,
    severity: Literal["error", "warning"] = "error",
) -> None:
    issues.append(ValidationIssue(path, message, code, severity))


def _valid_url(value: str) -> bool:
    parsed = urlparse(value)
    return parsed.scheme in {"http", "https"} and bool(parsed.netloc)


def _validate_pin(
    issues: list[ValidationIssue],
    spec: BoardPinSpec,
    used: dict[int, str],
    *,
    pin: int | None,
    path: str,
    direction: Direction,
    allowed_reserved: frozenset[int] = frozenset(),
) -> None:
    if pin is None:
        _issue(issues, path, "GPIO explícito é obrigatório quando o recurso está ativo", "pin.required")
        return
    if isinstance(pin, bool) or not isinstance(pin, int):
        _issue(issues, path, "GPIO deve ser um número inteiro", "pin.type")
        return
    if pin not in spec.available:
        _issue(issues, path, f"GPIO{pin} não existe neste alvo", "pin.unavailable")
        return
    if pin in spec.blocked and pin not in allowed_reserved:
        _issue(
            issues,
            path,
            f"GPIO{pin} está reservado para {spec.blocked[pin]}",
            "pin.reserved",
        )
        return
    if direction in {"output", "bidirectional"} and pin in spec.input_only:
        _issue(issues, path, f"GPIO{pin} aceita somente entrada", "pin.input_only")
        return
    if pin in used:
        _issue(
            issues,
            path,
            f"GPIO{pin} já está atribuído a {used[pin]}",
            "pin.duplicate",
        )
        return
    used[pin] = path


def validate_profile(profile: DeviceProfile) -> list[ValidationIssue]:
    issues: list[ValidationIssue] = []
    if profile.schema != "aethersense.device_profile.v1":
        _issue(issues, "schema", "schema de perfil não suportado", "schema.unsupported")
    if not profile.friendly_name.strip():
        _issue(issues, "friendly_name", "nome amigável é obrigatório", "name.required")
    if profile.association.efuse_mac:
        normalised_mac = normalise_mac(profile.association.efuse_mac)
        if not normalised_mac:
            _issue(issues, "association.efuse_mac", "eFuse MAC inválido", "identity.mac")
        elif profile.device_id and profile.device_id != device_id_from_mac(normalised_mac):
            _issue(
                issues,
                "device_id",
                "device_id diverge do identificador derivado do eFuse MAC",
                "identity.device_id_mismatch",
            )
    elif profile.device_id:
        _issue(
            issues,
            "association.efuse_mac",
            "eFuse MAC é obrigatório para validar o device_id pareado",
            "identity.mac_required",
        )
    if profile.target.profile not in SUPPORTED_TARGETS:
        _issue(issues, "target.profile", "alvo desconhecido", "target.unsupported")
        return issues
    if profile.target.partitions != "partitions.csv":
        _issue(
            issues,
            "target.partitions",
            "v1 aceita somente a tabela canônica partitions.csv",
            "target.partitions",
        )

    allowed_variants = {
        "classic": {"generic"},
        "esp32c5": {"generic", "esp32-c5-devkitc-1"},
        "esp32c6": {"generic"},
    }
    if profile.target.board_variant not in allowed_variants[profile.target.profile]:
        _issue(
            issues,
            "target.board_variant",
            "variante de placa desconhecida para o alvo selecionado",
            "target.board_variant",
        )

    spec = BOARD_SPECS[profile.target.profile]
    accessories_enabled = any(
        (
            profile.ir_rx.enabled,
            profile.ir_tx.enabled,
            profile.storage.enabled,
            any(relay.enabled for relay in profile.relays),
        )
    )
    if accessories_enabled and not spec.supports_accessories:
        _issue(
            issues,
            "target.profile",
            "os novos periféricos permanecem desabilitados para ESP32-C6 no v1",
            "target.accessories_unsupported",
        )

    if profile.network.wifi.band not in {"auto", "2.4ghz", "5ghz"}:
        _issue(issues, "network.wifi.band", "banda deve ser auto, 2.4ghz ou 5ghz", "wifi.band")
    if profile.target.profile == "classic" and profile.network.wifi.band == "5ghz":
        _issue(issues, "network.wifi.band", "ESP32-WROOM não suporta Wi-Fi 5 GHz", "wifi.band_unsupported")
    if not 1 <= profile.network.udp.port <= 65535:
        _issue(issues, "network.udp.port", "porta deve estar entre 1 e 65535", "network.port")
    if not 1 <= profile.network.udp.send_frequency_hz <= 100:
        _issue(
            issues,
            "network.udp.send_frequency_hz",
            "frequência deve estar entre 1 e 100 Hz",
            "csi.frequency",
        )
    try:
        ipaddress.ip_address(profile.network.udp.host)
    except ValueError:
        if not profile.network.udp.host or any(char.isspace() for char in profile.network.udp.host):
            _issue(issues, "network.udp.host", "host UDP inválido", "network.host")

    if profile.mqtt.enabled:
        if not profile.mqtt.broker_host:
            _issue(issues, "mqtt.broker_host", "broker é obrigatório quando MQTT está ativo", "mqtt.host")
        if not 1 <= profile.mqtt.broker_port <= 65535:
            _issue(issues, "mqtt.broker_port", "porta MQTT inválida", "mqtt.port")
        if not profile.device_id:
            _issue(
                issues,
                "device_id",
                "pareie a placa antes de ativar MQTT por dispositivo",
                "mqtt.device_id",
            )
    if profile.server.base_url and not _valid_url(profile.server.base_url):
        _issue(issues, "server.base_url", "URL do servidor deve usar http ou https", "url.invalid")
    if profile.sync.base_url and not _valid_url(profile.sync.base_url):
        _issue(issues, "sync.base_url", "URL de sincronização deve usar http ou https", "url.invalid")
    if profile.sync.block_bytes != 1024 * 1024:
        _issue(
            issues,
            "sync.block_bytes",
            "schema v1 exige blocos retomáveis de exatamente 1 MiB (1048576 bytes)",
            "sync.block_bytes",
        )
    sync_configured = bool(profile.sync.base_url or profile.sync.delete_after_sync)
    if sync_configured and not profile.sync.base_url:
        _issue(
            issues,
            "sync.base_url",
            "URL http(s) é obrigatória quando a sincronização de rede está configurada",
            "sync.url_required",
        )
    if sync_configured and not profile.storage.enabled:
        _issue(
            issues,
            "storage.enabled",
            "habilite o microSD antes de configurar sincronização de sessões",
            "sync.storage",
        )
    if profile.runtime.queue_capacity < 32 or profile.runtime.queue_capacity > 4096:
        _issue(issues, "runtime.queue_capacity", "capacidade deve ficar entre 32 e 4096", "queue.capacity")

    used: dict[int, str] = {}
    status_led = profile.runtime.status_led
    if profile.runtime.status_led_enabled != status_led.enabled:
        _issue(
            issues,
            "runtime.status_led",
            "campos legacy e status_led.enabled devem estar sincronizados",
            "led.legacy_mismatch",
        )
    if status_led.enabled:
        if profile.target.profile != "esp32c5":
            _issue(
                issues,
                "runtime.status_led.enabled",
                "LED RGB de status v1 é suportado somente no ESP32-C5",
                "led.target",
            )
        if status_led.kind != "ws2812":
            _issue(
                issues,
                "runtime.status_led.kind",
                "v1 suporta somente LED endereçável WS2812",
                "led.kind_unsupported_v1",
            )
        if not 1 <= status_led.brightness_percent <= 100:
            _issue(
                issues,
                "runtime.status_led.brightness_percent",
                "brilho deve ficar entre 1% e 100%",
                "led.brightness",
            )
        for state, color in status_led.colors.items():
            if len(color) != 3 or any(
                isinstance(channel, bool)
                or not isinstance(channel, int)
                or not 0 <= channel <= 255
                for channel in color
            ):
                _issue(
                    issues,
                    f"runtime.status_led.colors.{state}",
                    "cor deve conter três canais inteiros RGB entre 0 e 255",
                    "led.color",
                )
        valid_colors = [
            tuple(color)
            for color in status_led.colors.values()
            if len(color) == 3
            and all(isinstance(channel, int) and not isinstance(channel, bool) for channel in color)
        ]
        if len(set(valid_colors)) != len(valid_colors):
            _issue(
                issues,
                "runtime.status_led.colors",
                "cada estado do LED deve usar uma cor RGB distinta",
                "led.color_duplicate",
            )
        if status_led.kind == "ws2812":
            c5_status_led_gpio27 = (
                profile.target.profile == "esp32c5"
                and status_led.data_gpio == 27
            )
            board_owned_gpio27 = (
                c5_status_led_gpio27
                and profile.target.board_variant == "esp32-c5-devkitc-1"
            )
            if c5_status_led_gpio27 and not board_owned_gpio27:
                _issue(
                    issues,
                    "target.board_variant",
                    "GPIO27 é o LED RGB integrado do ESP32-C5-DevKitC-1; "
                    "confirme essa variante de placa para habilitá-lo com segurança",
                    "led.board_variant_required",
                )
            _validate_pin(
                issues,
                spec,
                used,
                pin=status_led.data_gpio,
                path="runtime.status_led.data_gpio",
                direction="output",
                # GPIO27 is electrically valid for the board-owned RGB LED. A
                # generic C5 profile remains invalid through the explicit board
                # variant issue above until the user confirms the physical board.
                allowed_reserved=frozenset({27}) if c5_status_led_gpio27 else frozenset(),
            )
            if profile.runtime.status_led_gpio != status_led.data_gpio:
                _issue(
                    issues,
                    "runtime.status_led_gpio",
                    "GPIO legacy deve refletir o data_gpio do WS2812",
                    "led.legacy_gpio",
                )
    if profile.ir_rx.enabled:
        _validate_pin(
            issues, spec, used, pin=profile.ir_rx.gpio, path="ir_rx.gpio", direction="input"
        )
        if not 1_000 <= profile.ir_rx.idle_threshold_us <= 100_000:
            _issue(issues, "ir_rx.idle_threshold_us", "limite deve ficar entre 1000 e 100000 µs", "ir.idle")
    if profile.ir_tx.enabled:
        _validate_pin(
            issues, spec, used, pin=profile.ir_tx.gpio, path="ir_tx.gpio", direction="output"
        )
        if not 20_000 <= profile.ir_tx.carrier_hz <= 60_000:
            _issue(issues, "ir_tx.carrier_hz", "carrier deve ficar entre 20 e 60 kHz", "ir.carrier")
        if not 10 <= profile.ir_tx.duty_percent <= 50:
            _issue(issues, "ir_tx.duty_percent", "duty deve ficar entre 10% e 50%", "ir.duty")

    if len(profile.relays) > 8:
        _issue(issues, "relays", "no máximo oito relés são permitidos", "relay.limit")
    relay_ids: set[str] = set()
    for index, relay in enumerate(profile.relays):
        path = f"relays[{index}]"
        if relay.relay_id in relay_ids:
            _issue(issues, f"{path}.relay_id", "ID de relé duplicado", "relay.id_duplicate")
        relay_ids.add(relay.relay_id)
        if relay.enabled:
            _validate_pin(
                issues,
                spec,
                used,
                pin=relay.gpio,
                path=f"{path}.gpio",
                direction="output",
            )
        if not 100 <= relay.pulse_limit_ms <= 600_000:
            _issue(issues, f"{path}.pulse_limit_ms", "limite deve ficar entre 100 e 600000 ms", "relay.pulse")

    if profile.storage.enabled:
        for field_name, direction in (
            ("sclk_gpio", "output"),
            ("mosi_gpio", "output"),
            ("miso_gpio", "input"),
            ("cs_gpio", "output"),
        ):
            _validate_pin(
                issues,
                spec,
                used,
                pin=getattr(profile.storage, field_name),
                path=f"storage.{field_name}",
                direction=direction,
            )
        if profile.storage.rotate_bytes < 64 * 1024:
            _issue(issues, "storage.rotate_bytes", "rotação mínima é 64 KiB", "sd.rotate")

    tx_channels = int(status_led.enabled and status_led.kind == "ws2812") + int(profile.ir_tx.enabled)
    rx_channels = int(profile.ir_rx.enabled)
    if tx_channels > spec.rmt_tx_channels:
        _issue(issues, "ir_tx", "canais RMT TX esgotados pelo LED/IR", "rmt.tx_capacity")
    if rx_channels > spec.rmt_rx_channels:
        _issue(issues, "ir_rx", "canais RMT RX esgotados", "rmt.rx_capacity")
    if spec.rmt_total_channels is not None and tx_channels + rx_channels > spec.rmt_total_channels:
        _issue(issues, "runtime", "canais RMT totais esgotados", "rmt.total_capacity")

    if profile.recording.privacy_mode not in {"events_only", "bench", "empty_room"}:
        _issue(issues, "recording.privacy_mode", "modo de privacidade inválido", "privacy.mode")
    if profile.recording.raw_csi_enabled and profile.recording.privacy_mode not in {
        "bench",
        "empty_room",
    }:
        _issue(
            issues,
            "recording.raw_csi_enabled",
            "CSI bruto local exige sessão explícita de bancada ou sala vazia",
            "privacy.raw_csi",
        )
    if profile.recording.raw_csi_enabled and not profile.storage.enabled:
        _issue(
            issues,
            "recording.raw_csi_enabled",
            "gravação local exige microSD ativo",
            "recording.storage",
        )
    return issues


def has_errors(issues: list[ValidationIssue]) -> bool:
    return any(issue.severity == "error" for issue in issues)
