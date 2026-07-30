"""Stable USB association and read-only eFuse MAC pairing helpers."""

from __future__ import annotations

from dataclasses import dataclass
import re
import shutil
import subprocess
from typing import Any, Callable, Iterable

from .models import DeviceProfile, device_id_from_mac, normalise_mac


class DevicePairingError(RuntimeError):
    pass


@dataclass(frozen=True)
class BoardIdentity:
    port: str
    mac: str
    device_id: str
    target_profile: str
    chip: str


def _normalise_usb_id(value: Any) -> str:
    text = str(value or "").strip().upper()
    if text.startswith("0X"):
        text = text[2:]
    return text.zfill(4) if text else ""


def parse_board_info(output: str, *, port: str = "") -> BoardIdentity:
    mac_match = re.search(
        r"(?im)^\s*(?:base\s+)?mac(?:\s+address)?\s*[:=]\s*([0-9a-f][0-9a-f:\-.]{10,20})\s*$",
        output,
    )
    mac = normalise_mac(mac_match.group(1)) if mac_match else ""
    if not mac:
        raise DevicePairingError("espflash não informou um Base MAC/eFuse MAC válido")

    lowered = output.lower().replace("_", "-")
    if re.search(r"esp32\s*-?\s*c5", lowered):
        target, chip = "esp32c5", "esp32c5"
    elif re.search(r"esp32\s*-?\s*c6", lowered):
        target, chip = "esp32c6", "esp32c6"
    elif "esp32" in lowered:
        target, chip = "classic", "esp32"
    else:
        raise DevicePairingError("tipo de chip ausente ou não suportado no board-info")
    return BoardIdentity(port, mac, device_id_from_mac(mac), target, chip)


def probe_board(
    port: str,
    *,
    runner: Callable[..., Any] | None = None,
    timeout: float = 20.0,
) -> BoardIdentity:
    if not port:
        raise DevicePairingError("selecione uma porta serial para parear")
    executable = shutil.which("espflash")
    if executable:
        command = [executable, "board-info", "--port", port]
    else:
        cargo = shutil.which("cargo")
        cargo_espflash = shutil.which("cargo-espflash")
        if cargo and cargo_espflash:
            # cargo-espflash is normally installed as a Cargo subcommand and
            # intentionally has no standalone `espflash.exe` alias on Windows.
            command = [cargo, "espflash", "board-info", "--port", port]
        else:
            raise DevicePairingError(
                "espflash não encontrado; instale-o com `cargo install cargo-espflash`"
            )
    run = runner or subprocess.run
    try:
        completed = run(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            errors="replace",
            check=False,
            timeout=timeout,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        raise DevicePairingError(f"falha ao consultar a placa: {exc}") from exc
    if completed.returncode != 0:
        raise DevicePairingError(
            f"espflash board-info falhou (exit {completed.returncode}); confira a porta e o modo boot"
        )
    return parse_board_info(f"{completed.stdout}\n{completed.stderr}", port=port)


def resolve_profile_port(
    profile: DeviceProfile,
    port_infos: Iterable[dict[str, Any]],
) -> str | None:
    """Resolve a current COM/tty name from immutable USB association metadata."""

    infos = [info for info in port_infos if str(info.get("device") or "").strip()]
    serial = profile.association.serial_number.strip().casefold()
    if serial:
        matches = [
            info
            for info in infos
            if str(info.get("serial_number") or "").strip().casefold() == serial
        ]
        if len(matches) == 1:
            return str(matches[0]["device"])
        if len(matches) > 1:
            raise DevicePairingError("serial USB associado aparece em mais de uma porta")

    vid = _normalise_usb_id(profile.association.vid)
    pid = _normalise_usb_id(profile.association.pid)
    if vid and pid:
        matches = [
            info
            for info in infos
            if _normalise_usb_id(info.get("vid")) == vid
            and _normalise_usb_id(info.get("pid")) == pid
        ]
        if len(matches) == 1:
            return str(matches[0]["device"])
        if len(matches) > 1:
            raise DevicePairingError(
                "VID/PID associado é ambíguo; selecione a porta ou use um adaptador com serial USB"
            )
    return None


def pair_profile(
    profile: DeviceProfile,
    identity: BoardIdentity,
    port_infos: Iterable[dict[str, Any]],
) -> DeviceProfile:
    """Apply a probe result while preserving immutable device identity."""

    if profile.device_id and profile.device_id != identity.device_id:
        raise DevicePairingError(
            "este perfil já pertence a outro eFuse MAC; clone/crie um perfil para a nova placa"
        )
    if profile.device_id and profile.target.profile != identity.target_profile:
        raise DevicePairingError(
            f"chip detectado ({identity.chip}) diverge do alvo pareado ({profile.target.profile})"
        )
    profile.target.profile = identity.target_profile
    profile.association.efuse_mac = identity.mac
    profile.device_id = identity.device_id
    matching = [
        info
        for info in port_infos
        if str(info.get("device") or "").casefold() == identity.port.casefold()
    ]
    if matching:
        info = matching[0]
        profile.association.vid = _normalise_usb_id(info.get("vid"))
        profile.association.pid = _normalise_usb_id(info.get("pid"))
        profile.association.serial_number = str(info.get("serial_number") or "").strip()
    return profile
