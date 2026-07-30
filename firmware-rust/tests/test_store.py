import json
from pathlib import Path
import zipfile

import pytest

from aethersense_build.models import new_device_profile
from aethersense_build.store import DeviceProfileStore


def test_atomic_store_drafts_backups_and_immutable_device_id(tmp_path: Path) -> None:
    store = DeviceProfileStore(tmp_path / "devices", backup_limit=2)
    profile = new_device_profile("C5")
    store.save_draft(profile)
    assert store.draft_path(profile.profile_id).exists()
    profile.device_id = "esp32-aabbccddeeff"
    store.save(profile)
    assert not store.draft_path(profile.profile_id).exists()
    for index in range(4):
        profile.friendly_name = f"C5 {index}"
        store.save(profile)
    assert len(list((store.backups_dir / profile.profile_id).glob("*.json"))) == 2
    profile.device_id = "esp32-000000000000"
    with pytest.raises(ValueError, match="immutable"):
        store.save(profile)


def test_export_is_deterministic_and_excludes_local_secret_refs(tmp_path: Path) -> None:
    store = DeviceProfileStore(tmp_path / "devices")
    profile = new_device_profile("Sala")
    profile.network.wifi.password_ref = "device/id/wifi-password"
    profile.mqtt.password_ref = "device/id/mqtt-password"
    profile.server.bearer_token_ref = "device/id/server-bearer"
    profile.server.admin_bearer_token_ref = "device/id/server-admin-bearer"
    profile.secret_revisions = {
        "wifi_password": "1" * 32,
        "mqtt_password": "2" * 32,
        "server_bearer_token": "3" * 32,
    }
    first = store.export_profile(profile, tmp_path / "first.zip")
    second = store.export_profile(profile, tmp_path / "second.zip")
    assert first.read_bytes() == second.read_bytes()
    imported = store.preview_import(first).profile
    assert imported.network.wifi.password_ref == ""
    assert imported.mqtt.password_ref == ""
    assert imported.server.bearer_token_ref == ""
    assert imported.server.admin_bearer_token_ref == ""
    assert imported.secret_revisions == {}
    with zipfile.ZipFile(first) as archive:
        exported_text = archive.read(archive.namelist()[0]).decode("utf-8")
    assert "secret_revisions" not in exported_text

    cloned = store.clone(profile)
    assert cloned.secret_revisions == {}


def test_import_creates_unpaired_copy_by_default(tmp_path: Path) -> None:
    store = DeviceProfileStore(tmp_path / "devices")
    profile = new_device_profile("Original")
    profile.device_id = "esp32-aabbccddeeff"
    source = store.export_profile(profile, tmp_path / "profile.json")
    imported = store.import_profile(source)
    assert imported.profile_id != profile.profile_id
    assert imported.device_id == ""
    assert imported.friendly_name.endswith("(cópia)")
    assert store.draft_path(imported.profile_id).exists()


def test_import_never_trusts_or_reuses_local_secret_revision(tmp_path: Path) -> None:
    store = DeviceProfileStore(tmp_path / "devices")
    profile = new_device_profile("Externo")
    profile.secret_revisions = {"wifi_password": "a" * 32}
    source = tmp_path / "external-local-profile.json"
    source.write_text(json.dumps(profile.to_dict()), encoding="utf-8")

    preview = store.preview_import(source)
    assert preview.profile.secret_revisions == {}
    imported = store.import_profile(source, copy_profile=False)
    assert imported.secret_revisions == {}


def test_catalogue_zip_is_deterministic_and_secret_free(tmp_path: Path) -> None:
    store = DeviceProfileStore(tmp_path / "devices")
    first = new_device_profile("A")
    second = new_device_profile("B")
    second.network.wifi.password_ref = "device/b/wifi-password"
    a = store.export_catalogue_zip([second, first], tmp_path / "a.zip")
    b = store.export_catalogue_zip([first, second], tmp_path / "b.zip")
    assert a.read_bytes() == b.read_bytes()
    with zipfile.ZipFile(a) as archive:
        text = "\n".join(archive.read(name).decode("utf-8") for name in archive.namelist())
    assert "device/b/wifi-password" not in text
    assert "password_ref\": \"\"" in text

    preview = store.preview_catalogue_zip(a)
    assert [item.profile.profile_id for item in preview.profiles] == sorted(
        [first.profile_id, second.profile_id]
    )
    imported = store.import_catalogue_zip(a)
    assert len(imported) == 2
    assert all(profile.device_id == "" for profile in imported)
    assert all(profile.profile_id not in {first.profile_id, second.profile_id} for profile in imported)


def test_catalogue_import_rejects_manifest_path_mismatch(tmp_path: Path) -> None:
    archive_path = tmp_path / "bad.zip"
    with zipfile.ZipFile(archive_path, "w") as archive:
        archive.writestr(
            "catalogue.json",
            json.dumps(
                {
                    "schema": "aethersense.device_catalogue.v1",
                    "profiles": ["missing"],
                }
            ),
        )
        archive.writestr("profiles/other.json", "{}")
    with pytest.raises(ValueError, match="manifesto"):
        DeviceProfileStore(tmp_path / "devices").preview_catalogue_zip(archive_path)


def test_catalogue_import_rejects_windows_drive_and_duplicate_entries(tmp_path: Path) -> None:
    drive_path = tmp_path / "drive.zip"
    with zipfile.ZipFile(drive_path, "w") as archive:
        archive.writestr("C:/escape.json", "{}")
    with pytest.raises(ValueError, match="inseguro"):
        DeviceProfileStore(tmp_path / "devices").preview_catalogue_zip(drive_path)

    duplicate_path = tmp_path / "duplicate.zip"
    manifest = json.dumps(
        {"schema": "aethersense.device_catalogue.v1", "profiles": []}
    )
    with zipfile.ZipFile(duplicate_path, "w") as archive:
        archive.writestr("catalogue.json", manifest)
        archive.writestr("catalogue.json", manifest)
    with pytest.raises(ValueError, match="duplicadas"):
        DeviceProfileStore(tmp_path / "devices").preview_catalogue_zip(duplicate_path)
