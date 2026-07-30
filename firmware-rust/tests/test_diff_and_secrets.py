from datetime import datetime, timedelta, timezone

from aethersense_build.diffing import (
    compute_build_hash,
    compute_config_hash,
    compute_firmware_source_hash,
    diff_profiles,
    profile_status,
)
from aethersense_build.models import DeviceProfile, new_device_profile
from aethersense_build.redaction import MASK, redact_mapping, redact_text
from aethersense_build.secrets import (
    MemorySecretStore,
    SecretManager,
    synchronise_secret_revisions,
)


def clone(profile: DeviceProfile) -> DeviceProfile:
    return DeviceProfile.from_dict(profile.to_dict())


def test_firmware_source_hash_is_deterministic_and_changes_with_sources(tmp_path) -> None:
    (tmp_path / "src").mkdir()
    source = tmp_path / "src" / "main.rs"
    source.write_text("fn main() {}\n", encoding="utf-8")
    (tmp_path / "Cargo.toml").write_text(
        '[package]\nname = "fixture"\n', encoding="utf-8"
    )
    profile = new_device_profile()

    first_source_hash = compute_firmware_source_hash(tmp_path)
    first_build_hash = compute_build_hash(profile, source_root=tmp_path)
    assert compute_firmware_source_hash(tmp_path) == first_source_hash
    assert compute_build_hash(profile, source_root=tmp_path) == first_build_hash

    source.write_text("fn main() { println!(\"changed\"); }\n", encoding="utf-8")
    assert compute_firmware_source_hash(tmp_path) != first_source_hash
    assert compute_build_hash(profile, source_root=tmp_path) != first_build_hash


def test_hashes_and_diff_separate_build_from_provision_changes() -> None:
    before = new_device_profile()
    after = clone(before)
    after.network.udp.port = 6000
    change = diff_profiles(before, after)
    assert change.requires_provision is True
    assert change.requires_build is False
    assert compute_build_hash(before) == compute_build_hash(after)
    assert compute_config_hash(before) != compute_config_hash(after)

    target = clone(after)
    target.target.profile = "esp32c5"
    target_change = diff_profiles(after, target)
    assert target_change.requires_build is True

    led_color = clone(target)
    led_color.runtime.status_led.colors["online"] = [1, 2, 3]
    color_change = diff_profiles(target, led_color)
    assert color_change.requires_provision is True
    assert color_change.requires_build is False

    led_enabled = clone(target)
    led_enabled.runtime.status_led.enabled = True
    led_enabled.runtime.status_led_enabled = True
    led_enabled.runtime.status_led.data_gpio = 8
    led_enabled.runtime.status_led_gpio = 8
    led_enabled.refresh_features()
    assert diff_profiles(target, led_enabled).requires_build is True


def test_memory_secret_manager_and_central_redaction() -> None:
    manager = SecretManager(MemorySecretStore())
    reference = manager.reference("uuid", "wifi-password")
    manager.set(reference, "muito-secreto")
    assert manager.get(reference) == "muito-secreto"
    assert manager.backend_name == "memory-only"
    assert redact_mapping({"wifi_password": "muito-secreto", "ssid": "Lab"}) == {
        "wifi_password": MASK,
        "ssid": "Lab",
    }
    text = redact_text("AETHERSENSE_WIFI_PASSWORD=muito-secreto token: abc", ("abc",))
    assert "muito-secreto" not in text
    assert "abc" not in text


def test_opaque_secret_revisions_invalidate_config_without_secret_verifiers() -> None:
    manager = SecretManager(MemorySecretStore())
    profile = new_device_profile("Segredos")
    references = {
        "wifi_password": manager.reference(profile.profile_id, "wifi-password"),
        "mqtt_password": manager.reference(profile.profile_id, "mqtt-password"),
        "server_bearer_token": manager.reference(
            profile.profile_id, "server-device-bearer"
        ),
    }
    profile.network.wifi.password_ref = references["wifi_password"]
    profile.mqtt.password_ref = references["mqtt_password"]
    profile.server.bearer_token_ref = references["server_bearer_token"]

    for kind, reference in references.items():
        profile.secret_revisions[kind] = manager.set_with_revision(
            reference, f"initial-{kind}"
        )
    synchronise_secret_revisions(profile, manager, require_available=True)
    baseline = clone(profile)
    baseline_hash = compute_config_hash(baseline)

    same_revision = manager.set_with_revision(
        references["wifi_password"],
        "initial-wifi_password",
        profile.secret_revisions["wifi_password"],
    )
    assert same_revision == profile.secret_revisions["wifi_password"]

    for kind, reference in references.items():
        changed = clone(baseline)
        changed.secret_revisions[kind] = manager.set_with_revision(
            reference,
            f"rotated-{kind}",
            changed.secret_revisions[kind],
        )
        assert changed.secret_revisions[kind] != baseline.secret_revisions[kind]
        assert len(changed.secret_revisions[kind]) == 32
        assert compute_config_hash(changed) != baseline_hash
        change = diff_profiles(baseline, changed)
        assert change.requires_provision
        assert not change.requires_build

    local_text = baseline.canonical_json()
    public_text = baseline.canonical_json(include_local_state=False)
    for kind in references:
        assert f"initial-{kind}" not in local_text
    assert "secret_revisions" not in public_text
    assert "fingerprint" not in local_text
    assert "salt" not in local_text


def test_secret_revision_change_moves_synchronised_profile_to_provisioning() -> None:
    manager = SecretManager(MemorySecretStore())
    profile = new_device_profile("Online")
    profile.association.efuse_mac = "aa:bb:cc:dd:ee:ff"
    profile.device_id = "esp32-aabbccddeeff"
    reference = manager.reference(profile.profile_id, "wifi-password")
    profile.network.wifi.password_ref = reference
    profile.secret_revisions["wifi_password"] = manager.set_with_revision(
        reference, "primeira"
    )
    profile.applied.build_hash = compute_build_hash(profile)
    profile.applied.config_hash = compute_config_hash(profile)
    profile.applied.last_seen_at = datetime.now(timezone.utc).isoformat()
    assert profile_status(profile) == "Sincronizado"

    profile.secret_revisions["wifi_password"] = manager.set_with_revision(
        reference,
        "segunda",
        profile.secret_revisions["wifi_password"],
    )
    assert profile_status(profile) == "Requer provisionamento"


def test_synchronised_status_expires_to_offline() -> None:
    profile = new_device_profile("Online")
    profile.association.efuse_mac = "aa:bb:cc:dd:ee:ff"
    profile.device_id = "esp32-aabbccddeeff"
    profile.applied.build_hash = compute_build_hash(profile)
    profile.applied.config_hash = compute_config_hash(profile)
    seen = datetime(2026, 1, 1, tzinfo=timezone.utc)
    profile.applied.last_seen_at = seen.isoformat().replace("+00:00", "Z")
    assert profile_status(profile, now=seen + timedelta(minutes=1)) == "Sincronizado"
    assert profile_status(profile, now=seen + timedelta(minutes=6)) == "Offline"


def test_hardware_validation_status_is_bound_to_current_hashes() -> None:
    profile = new_device_profile("C5 validada", "esp32c5")
    profile.association.efuse_mac = "aa:bb:cc:dd:ee:ff"
    profile.device_id = "esp32-aabbccddeeff"
    profile.applied.build_hash = compute_build_hash(profile)
    profile.applied.config_hash = compute_config_hash(profile)
    profile.applied.last_seen_at = datetime.now(timezone.utc).isoformat()

    assert (
        profile_status(profile, require_hardware_validation=True)
        == "Requer teste físico"
    )

    profile.applied.hardware_validation_status = "passed"
    profile.applied.hardware_validation_session = "2026-07-17t12-00-00z_c5"
    profile.applied.hardware_validation_build_hash = compute_build_hash(profile)
    profile.applied.hardware_validation_config_hash = compute_config_hash(profile)
    profile.applied.hardware_validated_at = datetime.now(timezone.utc).isoformat()
    assert (
        profile_status(profile, require_hardware_validation=True)
        == "Sincronizado e testado"
    )

    profile.network.udp.port += 1
    assert (
        profile_status(profile, require_hardware_validation=True)
        == "Requer provisionamento"
    )
