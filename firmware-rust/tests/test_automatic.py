from aethersense_build.automatic import (
    apply_automatic_board_defaults,
    apply_known_accessory_wiring,
    build_automatic_candidates,
    choose_automatic_candidate,
    prepare_automatic_profile,
    repair_incomplete_profile,
    usable_port_infos,
)
from aethersense_build.models import new_device_profile
from aethersense_build.validation import has_errors, validate_profile


def c5_port(device: str = "COM9") -> dict[str, str]:
    return {
        "device": device,
        "description": "USB-SERIAL CH340",
        "manufacturer": "wch.cn",
        "hwid": "USB VID:PID=1A86:7523",
        "vid": "1A86",
        "pid": "7523",
        "serial_number": "USB-C5",
        "suggested_profile": "esp32c5",
    }


def test_automatic_candidates_ignore_bluetooth_and_reuse_saved_profile() -> None:
    profile = new_device_profile("C5 sala", "esp32c5")
    profile.device_id = "esp32-aabbccddeeff"
    profile.association.efuse_mac = "aa:bb:cc:dd:ee:ff"
    profile.association.vid = "1A86"
    profile.association.pid = "7523"
    profile.association.serial_number = "USB-C5"
    profile.ir_rx.enabled = True
    profile.ir_rx.gpio = 8
    profile.ir_tx.enabled = True
    profile.ir_tx.gpio = 9
    ports = [
        {"device": "COM3", "description": "Bluetooth", "hwid": "BTHENUM"},
        c5_port(),
    ]

    assert [item["device"] for item in usable_port_infos(ports)] == ["COM9"]
    candidates = build_automatic_candidates([(profile, False)], ports)
    assert len(candidates) == 1
    assert candidates[0].profile_id == profile.profile_id
    assert candidates[0].port == "COM9"
    assert choose_automatic_candidate(candidates) == candidates[0]


def test_new_c5_automatic_profile_uses_safe_defaults_without_external_accessories() -> None:
    candidate = build_automatic_candidates([], [c5_port()])[0]
    profile, created = prepare_automatic_profile(candidate)

    assert created
    assert profile.target.profile == "esp32c5"
    assert profile.target.board_variant == "esp32-c5-devkitc-1"
    assert profile.runtime.status_led.enabled
    assert profile.runtime.status_led.data_gpio == 27
    assert profile.csi.enabled
    assert profile.network.wifi.captive_portal_fallback
    assert not profile.ir_rx.enabled
    assert not profile.ir_tx.enabled
    assert not profile.relays
    assert not profile.storage.enabled
    assert not profile.mqtt.enabled
    assert not profile.recording.raw_csi_enabled
    assert not has_errors(validate_profile(profile))


def test_board_defaults_preserve_saved_ir_and_sd_wiring() -> None:
    profile = new_device_profile("C5 bancada", "esp32c5")
    profile.ir_rx.enabled = True
    profile.ir_rx.gpio = 8
    profile.ir_tx.enabled = True
    profile.ir_tx.gpio = 9
    profile.storage.enabled = True
    profile.storage.sclk_gpio = 0
    profile.storage.mosi_gpio = 1
    profile.storage.miso_gpio = 23
    profile.storage.cs_gpio = 24

    apply_automatic_board_defaults(profile, "esp32c5")

    assert profile.ir_rx.gpio == 8
    assert profile.ir_tx.gpio == 9
    assert (
        profile.storage.sclk_gpio,
        profile.storage.mosi_gpio,
        profile.storage.miso_gpio,
        profile.storage.cs_gpio,
    ) == (0, 1, 23, 24)
    assert not has_errors(validate_profile(profile))


def test_repair_incomplete_profile_never_guesses_external_gpio() -> None:
    profile = new_device_profile("Legado incompleto", "esp32c5")
    profile.runtime.status_led.enabled = True
    profile.runtime.status_led_enabled = True
    profile.ir_rx.enabled = True
    profile.ir_tx.enabled = True
    profile.storage.enabled = True

    changes = repair_incomplete_profile(profile)

    assert profile.runtime.status_led.data_gpio == 27
    assert not profile.ir_rx.enabled
    assert profile.ir_rx.gpio is None
    assert not profile.ir_tx.enabled
    assert profile.ir_tx.gpio is None
    assert not profile.storage.enabled
    assert len(changes) == 4
    assert not has_errors(validate_profile(profile))


def test_known_c5_installation_restores_only_its_missing_ir_and_rgb_wiring() -> None:
    profile = new_device_profile("Meu espc5", "esp32c5")
    profile.device_id = "esp32-d0cf13ebf38c"

    changes = apply_known_accessory_wiring(profile)

    assert profile.ir_rx.enabled and profile.ir_rx.gpio == 8
    assert profile.ir_tx.enabled and profile.ir_tx.gpio == 9
    assert profile.runtime.status_led.enabled
    assert profile.runtime.status_led.data_gpio == 27
    assert len(changes) == 3

    other = new_device_profile("Outra placa", "esp32c5")
    other.device_id = "esp32-aabbccddeeff"
    assert apply_known_accessory_wiring(other) == ()
    assert other.ir_rx.gpio is None and other.ir_tx.gpio is None
