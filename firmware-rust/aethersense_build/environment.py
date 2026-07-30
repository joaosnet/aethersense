"""Translate a DeviceProfile into build inputs without duplicating UI rules."""

from __future__ import annotations

from dataclasses import dataclass
import os

from .diffing import compute_build_hash
from .models import DeviceProfile
from .redaction import display_value
from .secrets import SecretManager


@dataclass(frozen=True)
class EffectiveValue:
    field: str
    value: str
    origin: str


def _bool(value: bool) -> str:
    return "true" if value else "false"


def build_environment(
    profile: DeviceProfile,
    secrets: SecretManager | None = None,
) -> dict[str, str]:
    # Runtime network/peripheral configuration and every secret are written to
    # atomic NVS by the provisioning protocol.  The build receives only the
    # compile-time target identity and its reproducible hash.
    del secrets
    return {
        "AETHERSENSE_HARDWARE_PROFILE": profile.target.profile,
        "AETHERSENSE_BUILD_HASH": compute_build_hash(profile),
    }


def effective_values(
    profile: DeviceProfile,
    secrets: SecretManager | None = None,
    process_env: dict[str, str] | None = None,
) -> list[EffectiveValue]:
    process_env = os.environ if process_env is None else process_env
    profile_env = build_environment(profile, secrets)
    keys = ("AETHERSENSE_HARDWARE_PROFILE", "AETHERSENSE_BUILD_HASH")
    rows: list[EffectiveValue] = []
    for key in keys:
        if key in profile_env:
            value, origin = profile_env[key], "Perfil do dispositivo"
        elif key in process_env:
            value, origin = process_env[key], "Ambiente do processo"
        else:
            value, origin = "", "Default do firmware"
        rows.append(EffectiveValue(key, display_value(key, value), origin))
    runtime_rows = (
        ("Wi-Fi SSID", profile.network.wifi.ssid or "—", "Perfil → NVS"),
        (
            "Wi-Fi senha",
            "<configurada>" if secrets and secrets.get(profile.network.wifi.password_ref) else "—",
            "Keyring → provisionamento NVS",
        ),
        ("UDP host", profile.network.udp.host, "Perfil → NVS"),
        ("UDP porta", str(profile.network.udp.port), "Perfil → NVS"),
        (
            "MQTT senha",
            "<configurada>" if secrets and secrets.get(profile.mqtt.password_ref) else "—",
            "Keyring → provisionamento NVS",
        ),
        (
            "Servidor Bearer do dispositivo",
            "<configurado>" if secrets and secrets.get(profile.server.bearer_token_ref) else "—",
            "Keyring → provisionamento NVS",
        ),
    )
    rows.extend(EffectiveValue(*row) for row in runtime_rows)
    return rows
