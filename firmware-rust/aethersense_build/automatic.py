"""Deterministic, UI-independent helpers for the one-click setup flow.

The automatic path deliberately distinguishes hardware that can be discovered
over USB from external wiring that cannot.  It reuses the saved profile for a
connected board and applies only board-owned/safe defaults to a new profile.
IR, relays and microSD remain exactly as stored; guessing their wiring could
drive the wrong pin or energise a load.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Iterable, Mapping

from .devices import DevicePairingError, resolve_profile_port
from .models import DeviceProfile, new_device_profile


SUPPORTED_AUTOMATIC_TARGETS = {"classic", "esp32c5", "esp32c6"}

# Explicitly commissioned wiring, keyed by immutable eFuse-derived identity.
# This is not a generic pin preset: it records the physical installation the
# user already confirmed for this exact board and therefore survives COM moves
# and incomplete legacy drafts.
KNOWN_ACCESSORY_WIRINGS: dict[str, dict[str, int]] = {
    "esp32-d0cf13ebf38c": {"ir_rx_gpio": 8, "ir_tx_gpio": 9, "rgb_gpio": 27},
}


@dataclass(frozen=True)
class AutomaticDeviceCandidate:
    """A human-selectable physical board and its optional saved profile."""

    key: str
    port: str
    target_profile: str
    profile_id: str | None
    friendly_name: str
    label: str
    detail: str
    port_info: Mapping[str, Any]

    @property
    def reuses_saved_profile(self) -> bool:
        return self.profile_id is not None


def is_virtual_bluetooth_port(port_info: Mapping[str, Any]) -> bool:
    """Return whether *port_info* describes a Windows Bluetooth COM port."""

    text = " ".join(
        str(port_info.get(key) or "")
        for key in ("device", "description", "manufacturer", "hwid")
    ).casefold()
    return "bluetooth" in text or "bthenum" in text


def usable_port_infos(
    port_infos: Iterable[Mapping[str, Any]],
) -> list[dict[str, Any]]:
    """Keep real serial devices and remove empty/Bluetooth virtual entries."""

    usable: list[dict[str, Any]] = []
    for raw in port_infos:
        info = dict(raw)
        if not str(info.get("device") or "").strip():
            continue
        if is_virtual_bluetooth_port(info):
            continue
        usable.append(info)
    return sorted(usable, key=lambda item: str(item.get("device") or "").casefold())


def _target_name(target_profile: str) -> str:
    return {
        "esp32c5": "ESP32-C5",
        "classic": "ESP32-WROOM",
        "esp32c6": "ESP32-C6",
    }.get(target_profile, "ESP32")


def _candidate_key(port: str, profile_id: str | None) -> str:
    return f"{profile_id or 'new'}::{port}"


def build_automatic_candidates(
    catalogue: Iterable[tuple[DeviceProfile, bool]],
    port_infos: Iterable[Mapping[str, Any]],
) -> list[AutomaticDeviceCandidate]:
    """Map connected USB boards to saved profiles without using transient COM names.

    A candidate is emitted for every unambiguous saved-profile association.  If
    a physical port has no association, it is represented as a new board.  When
    duplicate profiles point at the same adapter they remain separate choices;
    silently choosing between immutable device identities would be unsafe.
    """

    infos = usable_port_infos(port_infos)
    profiles = list(catalogue)
    matches_by_port: dict[str, list[tuple[DeviceProfile, bool]]] = {
        str(info["device"]): [] for info in infos
    }

    for profile, is_draft in profiles:
        try:
            port = resolve_profile_port(profile, infos)
        except DevicePairingError:
            continue
        if port in matches_by_port:
            matches_by_port[port].append((profile, is_draft))

    candidates: list[AutomaticDeviceCandidate] = []
    for info in infos:
        port = str(info["device"])
        suggested = str(info.get("suggested_profile") or "unknown").strip().lower()
        target = suggested if suggested in SUPPORTED_AUTOMATIC_TARGETS else "unknown"
        matches = sorted(
            matches_by_port[port],
            key=lambda item: (item[1], item[0].friendly_name.casefold(), item[0].profile_id),
        )
        if matches:
            for profile, is_draft in matches:
                candidates.append(
                    AutomaticDeviceCandidate(
                        key=_candidate_key(port, profile.profile_id),
                        port=port,
                        target_profile=profile.target.profile,
                        profile_id=profile.profile_id,
                        friendly_name=profile.friendly_name,
                        label=f"{profile.friendly_name} — conectado",
                        detail=(
                            f"Perfil {'em preparação' if is_draft else 'salvo'} encontrado; "
                            "porta e configuração serão reutilizadas automaticamente."
                        ),
                        port_info=info,
                    )
                )
            continue

        target_name = _target_name(target)
        candidates.append(
            AutomaticDeviceCandidate(
                key=_candidate_key(port, None),
                port=port,
                target_profile=target,
                profile_id=None,
                friendly_name=f"AetherSense {target_name.replace('ESP32-', '')}",
                label=f"Nova placa {target_name} — pronta para configurar",
                detail=(
                    "Primeiro uso: o chip e o identificador permanente serão lidos "
                    "automaticamente antes da gravação."
                ),
                port_info=info,
            )
        )
    return candidates


def choose_automatic_candidate(
    candidates: Iterable[AutomaticDeviceCandidate],
    *,
    current_profile_id: str | None = None,
) -> AutomaticDeviceCandidate | None:
    """Choose only when the intended board is unambiguous."""

    options = list(candidates)
    if len(options) == 1:
        return options[0]
    current = [item for item in options if item.profile_id == current_profile_id]
    if len(current) == 1:
        return current[0]
    saved = [item for item in options if item.reuses_saved_profile]
    if len(saved) == 1:
        return saved[0]
    return None


def apply_automatic_board_defaults(
    profile: DeviceProfile,
    target_profile: str,
) -> DeviceProfile:
    """Apply safe defaults that belong to the selected board itself.

    Existing accessory wiring is preserved.  For this project's C5 DevKitC-1
    hardware, GPIO27 is the board-owned WS2812 and is therefore the sole pin
    enabled automatically.
    """

    target = target_profile if target_profile in SUPPORTED_AUTOMATIC_TARGETS else profile.target.profile
    if target in SUPPORTED_AUTOMATIC_TARGETS:
        profile.target.profile = target
    profile.network.wifi.captive_portal_fallback = True
    profile.csi.enabled = True
    profile.recording.events_enabled = True
    profile.recording.raw_csi_enabled = False
    profile.csi.raw_recording_enabled = False
    profile.recording.privacy_mode = "events_only"
    profile.csi.empty_room_only = False

    if target == "esp32c5":
        profile.target.board_variant = "esp32-c5-devkitc-1"
        profile.runtime.status_led.enabled = True
        profile.runtime.status_led.kind = "ws2812"
        profile.runtime.status_led.data_gpio = 27
        profile.runtime.status_led_enabled = True
        profile.runtime.status_led_gpio = 27

    profile.refresh_features()
    return profile


def apply_known_accessory_wiring(profile: DeviceProfile) -> tuple[str, ...]:
    """Restore only missing pins for a previously commissioned physical board."""

    wiring = KNOWN_ACCESSORY_WIRINGS.get(profile.device_id)
    if wiring is None or profile.target.profile != "esp32c5":
        return ()
    changes: list[str] = []
    if profile.runtime.status_led.data_gpio is None:
        profile.runtime.status_led.enabled = True
        profile.runtime.status_led.kind = "ws2812"
        profile.runtime.status_led.data_gpio = wiring["rgb_gpio"]
        profile.runtime.status_led_enabled = True
        profile.runtime.status_led_gpio = wiring["rgb_gpio"]
        changes.append("restaurei o LED RGB integrado no GPIO27")
    if profile.ir_rx.gpio is None:
        profile.ir_rx.enabled = True
        profile.ir_rx.gpio = wiring["ir_rx_gpio"]
        changes.append("restaurei o receptor IR conhecido no GPIO8")
    if profile.ir_tx.gpio is None:
        profile.ir_tx.enabled = True
        profile.ir_tx.gpio = wiring["ir_tx_gpio"]
        changes.append("restaurei o transmissor IR conhecido no GPIO9")
    profile.refresh_features()
    return tuple(changes)


def prepare_automatic_profile(
    candidate: AutomaticDeviceCandidate,
    *,
    current_profile: DeviceProfile | None = None,
    current_is_draft: bool = False,
) -> tuple[DeviceProfile, bool]:
    """Create/reuse an unpaired profile for a newly connected board."""

    reusable = bool(
        current_profile is not None
        and current_is_draft
        and not current_profile.device_id
    )
    profile = current_profile if reusable else new_device_profile()
    target = (
        candidate.target_profile
        if candidate.target_profile in SUPPORTED_AUTOMATIC_TARGETS
        else profile.target.profile
    )
    if profile.friendly_name.strip() in {"", "Novo dispositivo"}:
        profile.friendly_name = candidate.friendly_name
    apply_automatic_board_defaults(profile, target)
    profile.association.vid = str(candidate.port_info.get("vid") or "").strip().upper()
    profile.association.pid = str(candidate.port_info.get("pid") or "").strip().upper()
    profile.association.serial_number = str(
        candidate.port_info.get("serial_number") or ""
    ).strip()
    return profile, not reusable


def finish_automatic_identity(profile: DeviceProfile) -> DeviceProfile:
    """Complete friendly board defaults after eFuse/chip probing."""

    apply_automatic_board_defaults(profile, profile.target.profile)
    automatic_names = {
        "Novo dispositivo",
        "AetherSense C5",
        "AetherSense WROOM",
        "AetherSense C6",
        "AetherSense ESP32",
    }
    if profile.friendly_name.strip() in automatic_names and profile.device_id:
        target = _target_name(profile.target.profile).replace("ESP32-", "")
        profile.friendly_name = f"AetherSense {target} {profile.device_id[-4:].upper()}"
    profile.refresh_features()
    return profile


def repair_incomplete_profile(profile: DeviceProfile) -> tuple[str, ...]:
    """Conservatively unblock a legacy draft without guessing external wiring.

    Only incomplete optional resources are disabled.  Valid saved GPIO maps are
    never changed, so a previously commissioned IR/relay/SD installation keeps
    working exactly as before.
    """

    changes: list[str] = list(apply_known_accessory_wiring(profile))
    led = profile.runtime.status_led
    if led.enabled and led.data_gpio is None:
        if profile.target.profile == "esp32c5":
            apply_automatic_board_defaults(profile, "esp32c5")
            changes.append("configurei o LED RGB integrado do C5")
        else:
            led.enabled = False
            profile.runtime.status_led_enabled = False
            profile.runtime.status_led_gpio = None
            changes.append("desativei um LED sem ligação salva")

    if profile.ir_rx.enabled and profile.ir_rx.gpio is None:
        profile.ir_rx.enabled = False
        changes.append("desativei o receptor IR sem ligação salva")
    if profile.ir_tx.enabled and profile.ir_tx.gpio is None:
        profile.ir_tx.enabled = False
        changes.append("desativei o transmissor IR sem ligação salva")

    for relay in profile.relays:
        if relay.enabled and relay.gpio is None:
            relay.enabled = False
            changes.append(f"desativei {relay.name} sem ligação salva")

    sd_pins = (
        profile.storage.sclk_gpio,
        profile.storage.mosi_gpio,
        profile.storage.miso_gpio,
        profile.storage.cs_gpio,
    )
    if profile.storage.enabled and any(pin is None for pin in sd_pins):
        profile.storage.enabled = False
        profile.recording.raw_csi_enabled = False
        profile.csi.raw_recording_enabled = False
        profile.recording.privacy_mode = "events_only"
        profile.csi.empty_room_only = False
        changes.append("desativei o microSD sem ligação completa salva")

    if profile.recording.raw_csi_enabled and not profile.storage.enabled:
        profile.recording.raw_csi_enabled = False
        profile.csi.raw_recording_enabled = False
        profile.recording.privacy_mode = "events_only"
        profile.csi.empty_room_only = False
        changes.append("desativei a gravação CSI sem microSD")

    profile.refresh_features()
    return tuple(changes)
