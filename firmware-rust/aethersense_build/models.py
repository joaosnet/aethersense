"""Versioned, serialisable device configuration for build.py and firmware NVS."""

from __future__ import annotations

from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
import json
import re
from typing import Any
from uuid import uuid4


DEVICE_PROFILE_SCHEMA = "aethersense.device_profile.v1"
SUPPORTED_TARGETS = {"classic", "esp32c5", "esp32c6"}
PROVISIONED_SECRET_KINDS = {
    "wifi_password",
    "mqtt_password",
    "server_bearer_token",
}


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")


def _text(value: Any, default: str = "") -> str:
    return value.strip() if isinstance(value, str) else default


def _integer(value: Any, default: int) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def _optional_pin(value: Any) -> int | None:
    if value is None or value == "":
        return None
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def _mapping(value: Any) -> dict[str, Any]:
    return value if isinstance(value, dict) else {}


@dataclass
class UsbAssociation:
    vid: str = ""
    pid: str = ""
    serial_number: str = ""
    efuse_mac: str = ""

    @classmethod
    def from_dict(cls, data: Any) -> "UsbAssociation":
        data = _mapping(data)
        return cls(
            vid=_text(data.get("vid")).upper(),
            pid=_text(data.get("pid")).upper(),
            serial_number=_text(data.get("serial_number")),
            efuse_mac=normalise_mac(data.get("efuse_mac")),
        )


@dataclass
class TargetConfig:
    profile: str = "classic"
    board_variant: str = "generic"
    partitions: str = "partitions.csv"
    features: list[str] = field(default_factory=lambda: ["csi"])

    @classmethod
    def from_dict(cls, data: Any) -> "TargetConfig":
        data = _mapping(data)
        profile = _text(data.get("profile"), "classic").lower()
        raw_features = data.get("features", ["csi"])
        features = sorted(
            {_text(item).lower() for item in raw_features if _text(item)}
        ) if isinstance(raw_features, list) else ["csi"]
        return cls(
            profile=profile,
            board_variant=_text(data.get("board_variant"), "generic").lower(),
            partitions=_text(data.get("partitions"), "partitions.csv"),
            features=features,
        )


@dataclass
class WifiConfig:
    ssid: str = ""
    password_ref: str = ""
    band: str = "auto"
    prefer_ht40: bool = False
    captive_portal_fallback: bool = True

    @classmethod
    def from_dict(cls, data: Any) -> "WifiConfig":
        data = _mapping(data)
        return cls(
            ssid=_text(data.get("ssid")),
            password_ref=_text(data.get("password_ref")),
            band=_text(data.get("band"), "auto").lower(),
            prefer_ht40=bool(data.get("prefer_ht40", False)),
            captive_portal_fallback=bool(data.get("captive_portal_fallback", True)),
        )


@dataclass
class UdpConfig:
    host: str = "255.255.255.255"
    port: int = 5005
    send_frequency_hz: int = 10

    @classmethod
    def from_dict(cls, data: Any) -> "UdpConfig":
        data = _mapping(data)
        return cls(
            host=_text(data.get("host"), "255.255.255.255"),
            port=_integer(data.get("port"), 5005),
            send_frequency_hz=_integer(data.get("send_frequency_hz"), 10),
        )


@dataclass
class NetworkConfig:
    wifi: WifiConfig = field(default_factory=WifiConfig)
    udp: UdpConfig = field(default_factory=UdpConfig)

    @classmethod
    def from_dict(cls, data: Any) -> "NetworkConfig":
        data = _mapping(data)
        return cls(
            wifi=WifiConfig.from_dict(data.get("wifi")),
            udp=UdpConfig.from_dict(data.get("udp")),
        )


@dataclass
class CsiConfig:
    enabled: bool = True
    raw_recording_enabled: bool = False
    empty_room_only: bool = False

    @classmethod
    def from_dict(cls, data: Any) -> "CsiConfig":
        data = _mapping(data)
        return cls(
            enabled=bool(data.get("enabled", True)),
            raw_recording_enabled=bool(data.get("raw_recording_enabled", False)),
            empty_room_only=bool(data.get("empty_room_only", False)),
        )


DEFAULT_STATUS_LED_COLORS: dict[str, list[int]] = {
    "boot": [255, 160, 60],
    "online": [0, 180, 40],
    "offline": [0, 80, 255],
    "error": [255, 0, 0],
    "ir_rx": [255, 220, 80],
    "ir_tx": [255, 0, 255],
    "provisioning": [0, 220, 220],
    "recording": [120, 40, 255],
}


def _status_colors(value: Any) -> dict[str, list[int]]:
    data = _mapping(value)
    colors: dict[str, list[int]] = {}
    for state, default in DEFAULT_STATUS_LED_COLORS.items():
        raw = data.get(state, default)
        if isinstance(raw, (list, tuple)) and len(raw) == 3:
            colors[state] = [_integer(channel, default[index]) for index, channel in enumerate(raw)]
        else:
            colors[state] = list(default)
    return colors


@dataclass
class StatusLedConfig:
    enabled: bool = False
    kind: str = "ws2812"
    data_gpio: int | None = None
    red_gpio: int | None = None
    green_gpio: int | None = None
    blue_gpio: int | None = None
    brightness_percent: int = 25
    colors: dict[str, list[int]] = field(
        default_factory=lambda: {key: list(value) for key, value in DEFAULT_STATUS_LED_COLORS.items()}
    )

    @classmethod
    def from_dict(cls, data: Any) -> "StatusLedConfig":
        data = _mapping(data)
        gpios = _mapping(data.get("gpios"))
        return cls(
            enabled=bool(data.get("enabled", False)),
            kind=_text(data.get("kind"), "ws2812").lower(),
            data_gpio=_optional_pin(data.get("data_gpio", gpios.get("data"))),
            red_gpio=_optional_pin(data.get("red_gpio", gpios.get("red"))),
            green_gpio=_optional_pin(data.get("green_gpio", gpios.get("green"))),
            blue_gpio=_optional_pin(data.get("blue_gpio", gpios.get("blue"))),
            brightness_percent=_integer(data.get("brightness_percent"), 25),
            colors=_status_colors(data.get("colors")),
        )

    def to_firmware_dict(self) -> dict[str, Any]:
        document = {
            "enabled": self.enabled,
            "kind": self.kind,
            "gpios": {
                "data": self.data_gpio,
                "red": self.red_gpio,
                "green": self.green_gpio,
                "blue": self.blue_gpio,
            },
            "brightness_percent": self.brightness_percent,
            "colors": {key: list(value) for key, value in sorted(self.colors.items())},
        }
        return document


@dataclass
class RuntimeConfig:
    status_led_enabled: bool = False
    status_led_gpio: int | None = None
    status_led: StatusLedConfig = field(default_factory=StatusLedConfig)
    queue_capacity: int = 256

    @classmethod
    def from_dict(cls, data: Any) -> "RuntimeConfig":
        data = _mapping(data)
        status_data = _mapping(data.get("status_led"))
        if not status_data:
            status_data = {
                "enabled": bool(data.get("status_led_enabled", False)),
                "kind": "ws2812",
                "data_gpio": data.get("status_led_gpio"),
            }
        status_led = StatusLedConfig.from_dict(status_data)
        return cls(
            status_led_enabled=status_led.enabled,
            status_led_gpio=status_led.data_gpio if status_led.kind == "ws2812" else None,
            status_led=status_led,
            queue_capacity=_integer(data.get("queue_capacity"), 256),
        )


@dataclass
class MqttConfig:
    enabled: bool = False
    broker_mode: str = "local"
    broker_host: str = ""
    broker_port: int = 1883
    tls: bool = False
    username: str = ""
    password_ref: str = ""
    base_topic: str = "aethersense/devices"

    @classmethod
    def from_dict(cls, data: Any) -> "MqttConfig":
        data = _mapping(data)
        return cls(
            enabled=bool(data.get("enabled", False)),
            broker_mode=_text(data.get("broker_mode"), "local").lower(),
            broker_host=_text(data.get("broker_host")),
            broker_port=_integer(data.get("broker_port"), 1883),
            tls=bool(data.get("tls", False)),
            username=_text(data.get("username")),
            password_ref=_text(data.get("password_ref")),
            base_topic=_text(data.get("base_topic"), "aethersense/devices").rstrip("/"),
        )


@dataclass
class ServerConfig:
    base_url: str = ""
    # Administrative credential is used only by the commissioning TUI.  The
    # per-device credential is provisioned to firmware through
    # ``bearer_token_ref`` after a successful onboarding request.
    admin_bearer_token_ref: str = ""
    bearer_token_ref: str = ""

    @classmethod
    def from_dict(cls, data: Any) -> "ServerConfig":
        data = _mapping(data)
        return cls(
            base_url=_text(data.get("base_url")).rstrip("/"),
            admin_bearer_token_ref=_text(data.get("admin_bearer_token_ref")),
            bearer_token_ref=_text(data.get("bearer_token_ref")),
        )


@dataclass
class IrReceiverConfig:
    enabled: bool = False
    gpio: int | None = None
    idle_threshold_us: int = 12_000

    @classmethod
    def from_dict(cls, data: Any) -> "IrReceiverConfig":
        data = _mapping(data)
        return cls(
            enabled=bool(data.get("enabled", False)),
            gpio=_optional_pin(data.get("gpio")),
            idle_threshold_us=_integer(data.get("idle_threshold_us"), 12_000),
        )


@dataclass
class IrTransmitterConfig:
    enabled: bool = False
    gpio: int | None = None
    carrier_hz: int = 38_000
    duty_percent: int = 33

    @classmethod
    def from_dict(cls, data: Any) -> "IrTransmitterConfig":
        data = _mapping(data)
        return cls(
            enabled=bool(data.get("enabled", False)),
            gpio=_optional_pin(data.get("gpio")),
            carrier_hz=_integer(data.get("carrier_hz"), 38_000),
            duty_percent=_integer(data.get("duty_percent"), 33),
        )


@dataclass
class RelayConfig:
    relay_id: str = "relay_1"
    name: str = "Relay 1"
    enabled: bool = True
    gpio: int | None = None
    active_high: bool = True
    pulse_limit_ms: int = 60_000
    restore_state: bool = True

    @classmethod
    def from_dict(cls, data: Any, index: int = 0) -> "RelayConfig":
        data = _mapping(data)
        number = index + 1
        return cls(
            relay_id=_text(data.get("relay_id"), f"relay_{number}"),
            name=_text(data.get("name"), f"Relay {number}"),
            enabled=bool(data.get("enabled", True)),
            gpio=_optional_pin(data.get("gpio")),
            active_high=bool(data.get("active_high", True)),
            pulse_limit_ms=_integer(data.get("pulse_limit_ms"), 60_000),
            restore_state=bool(data.get("restore_state", True)),
        )


@dataclass
class SdConfig:
    enabled: bool = False
    sclk_gpio: int | None = None
    mosi_gpio: int | None = None
    miso_gpio: int | None = None
    cs_gpio: int | None = None
    mount_point: str = "/sdcard"
    rotate_bytes: int = 4 * 1024 * 1024

    @classmethod
    def from_dict(cls, data: Any) -> "SdConfig":
        data = _mapping(data)
        return cls(
            enabled=bool(data.get("enabled", False)),
            sclk_gpio=_optional_pin(data.get("sclk_gpio")),
            mosi_gpio=_optional_pin(data.get("mosi_gpio")),
            miso_gpio=_optional_pin(data.get("miso_gpio")),
            cs_gpio=_optional_pin(data.get("cs_gpio")),
            mount_point=_text(data.get("mount_point"), "/sdcard"),
            rotate_bytes=_integer(data.get("rotate_bytes"), 4 * 1024 * 1024),
        )


@dataclass
class RecordingConfig:
    events_enabled: bool = True
    raw_csi_enabled: bool = False
    privacy_mode: str = "events_only"

    @classmethod
    def from_dict(cls, data: Any) -> "RecordingConfig":
        data = _mapping(data)
        return cls(
            events_enabled=bool(data.get("events_enabled", True)),
            raw_csi_enabled=bool(data.get("raw_csi_enabled", False)),
            privacy_mode=_text(data.get("privacy_mode"), "events_only").lower(),
        )


@dataclass
class SyncConfig:
    network_default: bool = True
    base_url: str = ""
    block_bytes: int = 1024 * 1024
    delete_after_sync: bool = False

    @classmethod
    def from_dict(cls, data: Any) -> "SyncConfig":
        data = _mapping(data)
        return cls(
            network_default=bool(data.get("network_default", True)),
            base_url=_text(data.get("base_url")).rstrip("/"),
            block_bytes=_integer(data.get("block_bytes"), 1024 * 1024),
            delete_after_sync=bool(data.get("delete_after_sync", False)),
        )


@dataclass
class AppliedState:
    build_hash: str = ""
    config_hash: str = ""
    last_seen_at: str = ""
    hardware_validation_status: str = ""
    hardware_validation_session: str = ""
    hardware_validated_at: str = ""
    hardware_validation_build_hash: str = ""
    hardware_validation_config_hash: str = ""

    @classmethod
    def from_dict(cls, data: Any) -> "AppliedState":
        data = _mapping(data)
        return cls(
            build_hash=_text(data.get("build_hash")),
            config_hash=_text(data.get("config_hash")),
            last_seen_at=_text(data.get("last_seen_at")),
            hardware_validation_status=_text(
                data.get("hardware_validation_status")
            ),
            hardware_validation_session=_text(
                data.get("hardware_validation_session")
            ),
            hardware_validated_at=_text(data.get("hardware_validated_at")),
            hardware_validation_build_hash=_text(
                data.get("hardware_validation_build_hash")
            ),
            hardware_validation_config_hash=_text(
                data.get("hardware_validation_config_hash")
            ),
        )


@dataclass
class DeviceProfile:
    schema: str = DEVICE_PROFILE_SCHEMA
    profile_id: str = field(default_factory=lambda: str(uuid4()))
    device_id: str = ""
    friendly_name: str = "Novo dispositivo"
    target: TargetConfig = field(default_factory=TargetConfig)
    association: UsbAssociation = field(default_factory=UsbAssociation)
    network: NetworkConfig = field(default_factory=NetworkConfig)
    csi: CsiConfig = field(default_factory=CsiConfig)
    runtime: RuntimeConfig = field(default_factory=RuntimeConfig)
    mqtt: MqttConfig = field(default_factory=MqttConfig)
    server: ServerConfig = field(default_factory=ServerConfig)
    ir_rx: IrReceiverConfig = field(default_factory=IrReceiverConfig)
    ir_tx: IrTransmitterConfig = field(default_factory=IrTransmitterConfig)
    relays: list[RelayConfig] = field(default_factory=list)
    storage: SdConfig = field(default_factory=SdConfig)
    recording: RecordingConfig = field(default_factory=RecordingConfig)
    sync: SyncConfig = field(default_factory=SyncConfig)
    # Local-only opaque revisions bind secret changes to config_hash. Revisions
    # are random nonces, never derived from a password/token, and are removed
    # from every user export.
    secret_revisions: dict[str, str] = field(default_factory=dict)
    applied: AppliedState = field(default_factory=AppliedState)
    created_at: str = field(default_factory=utc_now)
    updated_at: str = field(default_factory=utc_now)

    @classmethod
    def from_dict(cls, raw: Any) -> "DeviceProfile":
        data = migrate_profile_data(raw)
        relays_raw = data.get("relays", [])
        relays = (
            [RelayConfig.from_dict(item, index) for index, item in enumerate(relays_raw)]
            if isinstance(relays_raw, list)
            else []
        )
        return cls(
            schema=DEVICE_PROFILE_SCHEMA,
            profile_id=_text(data.get("profile_id"), str(uuid4())),
            device_id=_text(data.get("device_id")),
            friendly_name=_text(data.get("friendly_name"), "Novo dispositivo"),
            target=TargetConfig.from_dict(data.get("target")),
            association=UsbAssociation.from_dict(data.get("association")),
            network=NetworkConfig.from_dict(data.get("network")),
            csi=CsiConfig.from_dict(data.get("csi")),
            runtime=RuntimeConfig.from_dict(data.get("runtime")),
            mqtt=MqttConfig.from_dict(data.get("mqtt")),
            server=ServerConfig.from_dict(data.get("server")),
            ir_rx=IrReceiverConfig.from_dict(data.get("ir_rx")),
            ir_tx=IrTransmitterConfig.from_dict(data.get("ir_tx")),
            relays=relays,
            storage=SdConfig.from_dict(data.get("storage")),
            recording=RecordingConfig.from_dict(data.get("recording")),
            sync=SyncConfig.from_dict(data.get("sync")),
            secret_revisions={
                str(key): str(value)
                for key, value in _mapping(data.get("secret_revisions")).items()
                if key in PROVISIONED_SECRET_KINDS
                and isinstance(value, str)
                and re.fullmatch(r"[0-9a-f]{32}", value)
            },
            applied=AppliedState.from_dict(data.get("applied")),
            created_at=_text(data.get("created_at"), utc_now()),
            updated_at=_text(data.get("updated_at"), utc_now()),
        )

    def to_dict(self, *, include_local_state: bool = True) -> dict[str, Any]:
        data = asdict(self)
        data["schema"] = DEVICE_PROFILE_SCHEMA
        data["target"]["features"] = sorted(set(data["target"]["features"]))
        data["relays"] = sorted(data["relays"], key=lambda item: item["relay_id"])
        if not include_local_state:
            data.pop("applied", None)
            data.pop("created_at", None)
            data.pop("updated_at", None)
            data.pop("secret_revisions", None)
            data["association"].pop("serial_number", None)
            data["association"].pop("vid", None)
            data["association"].pop("pid", None)
            data["network"]["wifi"]["password_ref"] = ""
            data["mqtt"]["password_ref"] = ""
            data["server"]["admin_bearer_token_ref"] = ""
            data["server"]["bearer_token_ref"] = ""
        return data

    def canonical_json(self, *, include_local_state: bool = True) -> str:
        return json.dumps(
            self.to_dict(include_local_state=include_local_state),
            ensure_ascii=False,
            separators=(",", ":"),
            sort_keys=True,
        )

    def to_firmware_dict(self) -> dict[str, Any]:
        """Secret-free, stable payload consumed by firmware/NVS.

        Local catalogue metadata is excluded. Secret values are provisioned via
        their dedicated encrypted fields and never enter this JSON.
        """

        chip = {
            "classic": "esp32",
            "esp32c5": "esp32c5",
            "esp32c6": "esp32c6",
        }.get(self.target.profile, self.target.profile)
        document = {
            "schema": DEVICE_PROFILE_SCHEMA,
            "identity": {
                "profile_uuid": self.profile_id,
                "device_id": self.device_id or None,
                "name": self.friendly_name,
            },
            "target": {
                "chip": chip,
                "board_variant": self.target.board_variant,
                "feature_set": sorted(set(self.target.features)),
                "partition_table": self.target.partitions,
            },
            "network": {
                "wifi_ssid": self.network.wifi.ssid,
                "udp_host": self.network.udp.host,
                "udp_port": self.network.udp.port,
            },
            "csi": {
                "enabled": self.csi.enabled,
                "send_frequency_hz": self.network.udp.send_frequency_hz,
                "queue_capacity": self.runtime.queue_capacity,
                "wifi_band": self.network.wifi.band,
                "prefer_ht40": self.network.wifi.prefer_ht40,
                "empty_room_only": self.csi.empty_room_only
                or self.recording.privacy_mode in {"bench", "empty_room"},
            },
            "runtime": {
                "status_led_enabled": self.runtime.status_led_enabled,
                "status_led_gpio": self.runtime.status_led_gpio,
                "status_led": self.runtime.status_led.to_firmware_dict(),
            },
            "mqtt": {
                "enabled": self.mqtt.enabled,
                "broker_host": self.mqtt.broker_host,
                "port": self.mqtt.broker_port,
                "username": self.mqtt.username or None,
                "tls": self.mqtt.tls,
                "qos": 1,
                "base_topic": self.mqtt.base_topic,
            },
            "peripherals": {
                "ir_rx": {
                    "enabled": self.ir_rx.enabled,
                    "gpio": self.ir_rx.gpio,
                    "idle_threshold_us": self.ir_rx.idle_threshold_us,
                },
                "ir_tx": {
                    "enabled": self.ir_tx.enabled,
                    "gpio": self.ir_tx.gpio,
                    "carrier_hz": self.ir_tx.carrier_hz,
                    "duty_percent": self.ir_tx.duty_percent,
                },
                "relays": [
                    {
                        "id": relay.relay_id,
                        "name": relay.name,
                        "enabled": relay.enabled,
                        "gpio": relay.gpio,
                        "active_high": relay.active_high,
                        "pulse_limit_ms": relay.pulse_limit_ms,
                        "restore_state": relay.restore_state,
                    }
                    for relay in sorted(self.relays, key=lambda item: item.relay_id)
                ],
            },
            "storage": {
                "sd": {
                    "enabled": self.storage.enabled,
                    "sclk_gpio": self.storage.sclk_gpio,
                    "mosi_gpio": self.storage.mosi_gpio,
                    "miso_gpio": self.storage.miso_gpio,
                    "cs_gpio": self.storage.cs_gpio,
                    "mount_point": self.storage.mount_point,
                    "rotate_bytes": self.storage.rotate_bytes,
                },
                "recording": {
                    "events_enabled": self.recording.events_enabled,
                    "raw_csi_enabled": self.recording.raw_csi_enabled,
                    "privacy_mode": (
                        "empty_room_bench"
                        if self.recording.privacy_mode in {"bench", "empty_room"}
                        else "events_only"
                    ),
                },
            },
            "sync": {
                "enabled": bool(self.sync.base_url),
                "base_url": self.sync.base_url,
                "network_first": self.sync.network_default,
                "block_bytes": self.sync.block_bytes,
                "delete_after_sync": self.sync.delete_after_sync,
            },
        }
        if self.secret_revisions:
            document["secret_revisions"] = dict(
                sorted(self.secret_revisions.items())
            )
        return document

    # Backwards-compatible name used by early Python tests. New integrations
    # should prefer the explicit wire-contract name above.
    def firmware_dict(self) -> dict[str, Any]:
        return self.to_firmware_dict()

    def firmware_json(self) -> str:
        return json.dumps(
            self.to_firmware_dict(), ensure_ascii=False, separators=(",", ":"), sort_keys=True
        )

    def refresh_features(self) -> None:
        features = {"csi"} if self.csi.enabled else set()
        if self.mqtt.enabled:
            features.add("mqtt")
        if self.ir_rx.enabled or self.ir_tx.enabled:
            features.add("ir")
        if any(relay.enabled for relay in self.relays):
            features.add("relays")
        if self.storage.enabled:
            features.add("sd")
        if self.runtime.status_led.enabled:
            features.add("status_led")
        self.target.features = sorted(features)


def normalise_mac(value: Any) -> str:
    compact = re.sub(r"[^0-9a-fA-F]", "", _text(value))
    if len(compact) != 12:
        return ""
    return ":".join(compact[index : index + 2] for index in range(0, 12, 2)).lower()


def device_id_from_mac(value: Any) -> str:
    mac = normalise_mac(value)
    return f"esp32-{mac.replace(':', '')}" if mac else ""


def new_device_profile(
    friendly_name: str = "Novo dispositivo", target_profile: str = "classic"
) -> DeviceProfile:
    target = target_profile.lower().strip()
    if target not in SUPPORTED_TARGETS:
        target = "classic"
    return DeviceProfile(friendly_name=friendly_name.strip() or "Novo dispositivo", target=TargetConfig(profile=target))


def migrate_profile_data(raw: Any) -> dict[str, Any]:
    if not isinstance(raw, dict):
        raise ValueError("device profile must be a JSON object")
    schema = raw.get("schema")
    if schema == DEVICE_PROFILE_SCHEMA and "identity" in raw:
        identity = _mapping(raw.get("identity"))
        target = _mapping(raw.get("target"))
        network = _mapping(raw.get("network"))
        csi = _mapping(raw.get("csi"))
        runtime = _mapping(raw.get("runtime"))
        mqtt = _mapping(raw.get("mqtt"))
        peripherals = _mapping(raw.get("peripherals"))
        storage = _mapping(raw.get("storage"))
        sd = _mapping(storage.get("sd"))
        recording = _mapping(storage.get("recording"))
        sync = _mapping(raw.get("sync"))
        chip = _text(target.get("chip"), "esp32")
        profile_name = {"esp32": "classic", "esp32c5": "esp32c5", "esp32c6": "esp32c6"}.get(
            chip, chip
        )
        privacy = _text(recording.get("privacy_mode"), "events_only")
        return {
            "schema": DEVICE_PROFILE_SCHEMA,
            "profile_id": _text(identity.get("profile_uuid"), str(uuid4())),
            "device_id": _text(identity.get("device_id")),
            "friendly_name": _text(identity.get("name"), "Novo dispositivo"),
            "target": {
                "profile": profile_name,
                "board_variant": _text(target.get("board_variant"), "generic"),
                "partitions": _text(target.get("partition_table"), "partitions.csv"),
                "features": target.get("feature_set", ["csi"]),
            },
            "network": {
                "wifi": {
                    "ssid": _text(network.get("wifi_ssid")),
                    "band": _text(csi.get("wifi_band"), "auto"),
                    "prefer_ht40": bool(csi.get("prefer_ht40", False)),
                },
                "udp": {
                    "host": _text(network.get("udp_host"), "255.255.255.255"),
                    "port": _integer(network.get("udp_port"), 5005),
                    "send_frequency_hz": _integer(csi.get("send_frequency_hz"), 10),
                },
            },
            "csi": {
                "enabled": bool(csi.get("enabled", True)),
                "raw_recording_enabled": bool(recording.get("raw_csi_enabled", False)),
                "empty_room_only": bool(csi.get("empty_room_only", False)),
            },
            "runtime": {
                "status_led_enabled": bool(runtime.get("status_led_enabled", False)),
                "status_led_gpio": runtime.get("status_led_gpio"),
                "status_led": runtime.get("status_led"),
                "queue_capacity": _integer(csi.get("queue_capacity"), 256),
            },
            "mqtt": {
                "enabled": bool(mqtt.get("enabled", False)),
                "broker_mode": "external",
                "broker_host": _text(mqtt.get("broker_host")),
                "broker_port": _integer(mqtt.get("port"), 1883),
                "tls": bool(mqtt.get("tls", False)),
                "username": _text(mqtt.get("username")),
                "base_topic": _text(mqtt.get("base_topic"), "aethersense/devices"),
            },
            "ir_rx": _mapping(peripherals.get("ir_rx")),
            "ir_tx": _mapping(peripherals.get("ir_tx")),
            "relays": [
                {
                    "relay_id": item.get("id"),
                    "name": item.get("name"),
                    "enabled": item.get("enabled", True),
                    "gpio": item.get("gpio"),
                    "active_high": item.get("active_high", True),
                    "pulse_limit_ms": item.get("pulse_limit_ms", 60_000),
                    "restore_state": item.get("restore_state", True),
                }
                for item in peripherals.get("relays", [])
                if isinstance(item, dict)
            ],
            "storage": sd,
            "recording": {
                "events_enabled": bool(recording.get("events_enabled", True)),
                "raw_csi_enabled": bool(recording.get("raw_csi_enabled", False)),
                "privacy_mode": "empty_room" if privacy == "empty_room_bench" else "events_only",
            },
            "sync": {
                "network_default": bool(sync.get("network_first", True)),
                "base_url": _text(sync.get("base_url")),
                "block_bytes": _integer(sync.get("block_bytes"), 1024 * 1024),
                "delete_after_sync": bool(sync.get("delete_after_sync", False)),
            },
        }
    if schema == DEVICE_PROFILE_SCHEMA:
        return dict(raw)
    if schema not in {None, "", "aethersense.device_profile.v0"}:
        raise ValueError(f"unsupported device profile schema: {schema}")

    # Early development builds stored the common fields flat.  Keeping this
    # migration here makes imports predictable without growing UI-only rules.
    migrated = dict(raw)
    target_name = migrated.pop("hardware_profile", migrated.pop("profile", "classic"))
    if "target" not in migrated:
        migrated["target"] = {"profile": target_name, "features": ["csi"]}
    if "network" not in migrated:
        migrated["network"] = {
            "wifi": {
                "ssid": migrated.pop("wifi_ssid", ""),
                "band": migrated.pop("wifi_band", "auto"),
            },
            "udp": {
                "host": migrated.pop("udp_host", "255.255.255.255"),
                "port": migrated.pop("udp_port", 5005),
                "send_frequency_hz": migrated.pop("send_frequency_hz", 10),
            },
        }
    migrated["schema"] = DEVICE_PROFILE_SCHEMA
    return migrated
