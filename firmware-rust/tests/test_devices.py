import pytest

from aethersense_build.devices import (
    BoardIdentity,
    DevicePairingError,
    pair_profile,
    parse_board_info,
    probe_board,
    resolve_profile_port,
)
from aethersense_build.models import new_device_profile
from aethersense_build.validation import validate_profile


@pytest.mark.parametrize(
    ("output", "target", "mac"),
    [
        (
            "Chip type: ESP32-C5 (revision v1.0)\nMAC address: 10:20:30:40:50:60\n",
            "esp32c5",
            "10:20:30:40:50:60",
        ),
        (
            "Chip type: ESP32 (revision v3.1)\nBase MAC: AA-BB-CC-DD-EE-FF\n",
            "classic",
            "aa:bb:cc:dd:ee:ff",
        ),
    ],
)
def test_parse_board_info_c5_and_wroom(output: str, target: str, mac: str) -> None:
    identity = parse_board_info(output, port="COM12")
    assert identity.target_profile == target
    assert identity.mac == mac
    assert identity.device_id == f"esp32-{mac.replace(':', '')}"


def test_probe_board_uses_read_only_espflash_command(monkeypatch) -> None:
    monkeypatch.setattr("aethersense_build.devices.shutil.which", lambda _name: "espflash")
    captured = {}

    class Completed:
        returncode = 0
        stdout = "Chip: esp32c5\nBase MAC: 01:23:45:67:89:ab\n"
        stderr = ""

    def runner(command, **kwargs):
        captured["command"] = command
        return Completed()

    identity = probe_board("COM9", runner=runner)
    assert captured["command"] == ["espflash", "board-info", "--port", "COM9"]
    assert identity.device_id == "esp32-0123456789ab"


def test_probe_board_falls_back_to_cargo_espflash_on_windows(monkeypatch) -> None:
    def which(name: str):
        return {
            "espflash": None,
            "cargo": "cargo.exe",
            "cargo-espflash": "cargo-espflash.exe",
        }[name]

    monkeypatch.setattr("aethersense_build.devices.shutil.which", which)
    captured = {}

    class Completed:
        returncode = 0
        stdout = "Chip: esp32c5\nBase MAC: 01:23:45:67:89:ab\n"
        stderr = ""

    def runner(command, **kwargs):
        captured["command"] = command
        return Completed()

    identity = probe_board("COM9", runner=runner)
    assert captured["command"] == [
        "cargo.exe",
        "espflash",
        "board-info",
        "--port",
        "COM9",
    ]
    assert identity.device_id == "esp32-0123456789ab"


def test_pairing_records_usb_association_and_rejects_identity_change() -> None:
    profile = new_device_profile("Sala")
    identity = BoardIdentity(
        "COM7", "aa:bb:cc:dd:ee:ff", "esp32-aabbccddeeff", "esp32c5", "esp32c5"
    )
    infos = [
        {
            "device": "COM7",
            "vid": "303A",
            "pid": "1001",
            "serial_number": "USB-ABC",
        }
    ]
    pair_profile(profile, identity, infos)
    assert profile.target.profile == "esp32c5"
    assert profile.association.serial_number == "USB-ABC"
    assert resolve_profile_port(profile, [{**infos[0], "device": "COM19"}]) == "COM19"

    different = BoardIdentity(
        "COM8", "00:11:22:33:44:55", "esp32-001122334455", "esp32c5", "esp32c5"
    )
    with pytest.raises(DevicePairingError, match="outro eFuse"):
        pair_profile(profile, different, [])


def test_port_resolution_falls_back_to_unique_vid_pid_and_blocks_ambiguity() -> None:
    profile = new_device_profile("Sala")
    profile.association.vid = "303a"
    profile.association.pid = "1001"
    assert (
        resolve_profile_port(
            profile,
            [
                {"device": "COM3", "vid": "FFFF", "pid": "0001"},
                {"device": "COM21", "vid": "303A", "pid": "1001"},
            ],
        )
        == "COM21"
    )
    with pytest.raises(DevicePairingError, match="ambíguo"):
        resolve_profile_port(
            profile,
            [
                {"device": "COM21", "vid": "303A", "pid": "1001"},
                {"device": "COM22", "vid": "303A", "pid": "1001"},
            ],
        )


def test_validation_blocks_device_id_that_diverges_from_efuse() -> None:
    profile = new_device_profile("Sala")
    profile.association.efuse_mac = "aa:bb:cc:dd:ee:ff"
    profile.device_id = "esp32-001122334455"
    assert "identity.device_id_mismatch" in {issue.code for issue in validate_profile(profile)}
