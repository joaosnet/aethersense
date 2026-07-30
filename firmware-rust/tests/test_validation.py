from aethersense_build.models import RelayConfig, new_device_profile
from aethersense_build.validation import validate_profile


def issue_codes(profile) -> set[str]:
    return {issue.code for issue in validate_profile(profile)}


def test_valid_explicit_c5_pin_map() -> None:
    profile = new_device_profile(target_profile="esp32c5")
    profile.ir_rx.enabled = True
    profile.ir_rx.gpio = 8
    profile.ir_tx.enabled = True
    profile.ir_tx.gpio = 9
    profile.relays = [RelayConfig(gpio=10)]
    profile.storage.enabled = True
    profile.storage.sclk_gpio = 0
    profile.storage.mosi_gpio = 1
    profile.storage.miso_gpio = 23
    profile.storage.cs_gpio = 24
    assert issue_codes(profile) == set()


def test_c5_blocks_flash_strapping_usb_and_duplicate_pins() -> None:
    profile = new_device_profile(target_profile="esp32c5")
    profile.ir_tx.enabled = True
    profile.ir_tx.gpio = 16
    profile.ir_rx.enabled = True
    profile.ir_rx.gpio = 13
    profile.relays = [RelayConfig(gpio=2), RelayConfig(relay_id="relay_2", gpio=2)]
    codes = issue_codes(profile)
    assert "pin.reserved" in codes


def test_c5_blocks_every_datasheet_strapping_gpio() -> None:
    for gpio in (2, 3, 7, 25, 26, 27, 28):
        profile = new_device_profile(target_profile="esp32c5")
        profile.ir_tx.enabled = True
        profile.ir_tx.gpio = gpio
        assert "pin.reserved" in issue_codes(profile), f"GPIO{gpio} should be reserved"


def test_wroom_rejects_gpio_not_exposed_by_module() -> None:
    for gpio in (20, 24, 28, 29, 30, 31, 37, 38):
        profile = new_device_profile(target_profile="classic")
        profile.ir_tx.enabled = True
        profile.ir_tx.gpio = gpio
        assert "pin.unavailable" in issue_codes(profile), f"GPIO{gpio} is not exposed"


def test_wroom_input_only_pin_cannot_drive_relay() -> None:
    profile = new_device_profile(target_profile="classic")
    profile.relays = [RelayConfig(gpio=34)]
    assert "pin.input_only" in issue_codes(profile)


def test_raw_csi_requires_sd_and_bench_or_empty_room() -> None:
    profile = new_device_profile()
    profile.recording.raw_csi_enabled = True
    profile.recording.privacy_mode = "events_only"
    codes = issue_codes(profile)
    assert "privacy.raw_csi" in codes
    assert "recording.storage" in codes


def test_c6_rejects_new_accessories() -> None:
    profile = new_device_profile(target_profile="esp32c6")
    profile.ir_rx.enabled = True
    profile.ir_rx.gpio = 4
    assert "target.accessories_unsupported" in issue_codes(profile)


def test_c5_devkit_board_owned_ws2812_gpio27_exception_is_narrow() -> None:
    profile = new_device_profile(target_profile="esp32c5")
    profile.target.board_variant = "esp32-c5-devkitc-1"
    profile.runtime.status_led.enabled = True
    profile.runtime.status_led.kind = "ws2812"
    profile.runtime.status_led.data_gpio = 27
    profile.runtime.status_led_enabled = True
    profile.runtime.status_led_gpio = 27
    assert "pin.reserved" not in issue_codes(profile)

    profile.ir_tx.enabled = True
    profile.ir_tx.gpio = 27
    assert "pin.reserved" in issue_codes(profile)

    profile.ir_tx.enabled = False
    profile.target.board_variant = "generic"
    codes = issue_codes(profile)
    assert "led.board_variant_required" in codes
    assert "pin.reserved" not in codes


def test_status_rgb_requires_c5_explicit_pins_and_valid_colors() -> None:
    profile = new_device_profile(target_profile="classic")
    profile.runtime.status_led.enabled = True
    profile.runtime.status_led.kind = "rgb_discrete"
    profile.runtime.status_led.red_gpio = 18
    profile.runtime.status_led.green_gpio = 19
    profile.runtime.status_led.blue_gpio = 21
    profile.runtime.status_led_enabled = True
    profile.runtime.status_led.colors["error"] = [300, 0, 0]
    codes = issue_codes(profile)
    assert "led.target" in codes
    assert "led.color" in codes


def test_sync_v1_requires_exactly_one_mib_blocks() -> None:
    profile = new_device_profile()
    assert "sync.block_bytes" not in issue_codes(profile)

    profile.sync.block_bytes = 1024 * 1024 - 1
    assert "sync.block_bytes" in issue_codes(profile)

    profile.sync.block_bytes = 1024 * 1024 + 1
    assert "sync.block_bytes" in issue_codes(profile)


def test_network_sync_requires_http_url_and_enabled_sd() -> None:
    profile = new_device_profile()
    profile.sync.delete_after_sync = True
    codes = issue_codes(profile)
    assert "sync.url_required" in codes
    assert "sync.storage" in codes

    profile.sync.base_url = "ftp://server/sessions"
    assert "url.invalid" in issue_codes(profile)

    profile.sync.base_url = "https://server.local/api/uploads"
    profile.storage.enabled = True
    assert "sync.url_required" not in issue_codes(profile)
    assert "sync.storage" not in issue_codes(profile)
