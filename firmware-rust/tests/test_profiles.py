import json
from pathlib import Path

from aethersense_build.models import DEVICE_PROFILE_SCHEMA, DeviceProfile, RelayConfig, new_device_profile
from aethersense_build.diffing import compute_config_hash


FIXTURE = Path(__file__).parent / "fixtures" / "device_profile_v1.json"


def test_rust_golden_profile_round_trips_exactly() -> None:
    expected = json.loads(FIXTURE.read_text(encoding="utf-8"))
    profile = DeviceProfile.from_dict(expected)
    assert profile.to_firmware_dict() == expected
    assert json.loads(profile.firmware_json()) == expected
    assert compute_config_hash(profile) == "849ad61ec3343183ea5275236198dfe241b2f30d44065fb6d77b1e4a31554331"


def test_firmware_payload_never_contains_secret_references() -> None:
    profile = new_device_profile("Lab", "esp32c5")
    profile.network.wifi.password_ref = "device/uuid/wifi-password"
    profile.mqtt.password_ref = "device/uuid/mqtt-password"
    profile.server.bearer_token_ref = "device/uuid/server-bearer"
    profile.server.admin_bearer_token_ref = "device/uuid/server-admin-bearer"
    payload = profile.firmware_json()
    assert "password_ref" not in payload
    assert "bearer_token_ref" not in payload
    assert "admin_bearer_token_ref" not in payload
    assert "device/uuid" not in payload
    assert json.loads(payload)["schema"] == DEVICE_PROFILE_SCHEMA


def test_opaque_revision_has_cross_language_config_hash_contract() -> None:
    profile = DeviceProfile.from_dict(json.loads(FIXTURE.read_text(encoding="utf-8")))
    profile.secret_revisions = {
        "wifi_password": "0123456789abcdef0123456789abcdef"
    }
    payload = profile.to_firmware_dict()
    assert payload["secret_revisions"] == profile.secret_revisions
    assert compute_config_hash(profile) == (
        "0cbc4a32fc0c3ab1b9a3a97ea15caa1c4d2e043f68baa124b8f5eb69cdbb8efa"
    )


def test_feature_set_is_sorted_and_uses_relays_plural() -> None:
    profile = new_device_profile()
    profile.mqtt.enabled = True
    profile.relays = [RelayConfig(enabled=True, gpio=13)]
    profile.storage.enabled = True
    profile.refresh_features()
    assert profile.target.features == ["csi", "mqtt", "relays", "sd"]


def test_bench_and_empty_room_normalize_to_wire_privacy_value() -> None:
    for local_value in ("bench", "empty_room"):
        profile = new_device_profile()
        profile.recording.privacy_mode = local_value
        wire = profile.to_firmware_dict()
        assert wire["csi"]["empty_room_only"] is True
        assert wire["storage"]["recording"]["privacy_mode"] == "empty_room_bench"
