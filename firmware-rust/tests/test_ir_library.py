import json
from pathlib import Path
import re

import pytest

from aethersense_build.ir_library import (
    DEFAULT_CARRIER_HZ,
    DEFAULT_DUTY_PERCENT,
    IR_LIBRARY_SCHEMA,
    IrLibrary,
    IrLibraryStore,
    compute_signal_hash,
    make_appliance_id,
    make_code_id,
)


PROFILE_UUID = "4f1d9da2-7166-47b7-a156-f08112a01e68"
DEVICE_ID = "esp32-aabbccddeeff"
POWER_SIGNAL = (9_000, 4_500, 560, 560, 560, 1_690)


def test_library_schema_defaults_and_secret_free_persistence(tmp_path: Path) -> None:
    store = IrLibraryStore(tmp_path / "ir-library")
    library = store.get_or_create(PROFILE_UUID, DEVICE_ID)
    appliance = store.create_appliance(
        PROFILE_UUID, "Ar-condicionado", device_id=DEVICE_ID, kind="air_conditioner"
    )
    result = store.save_capture(
        PROFILE_UUID,
        appliance.appliance_id,
        "Ligar",
        POWER_SIGNAL,
        device_id=DEVICE_ID,
    )

    assert library.profile_uuid == PROFILE_UUID
    assert result.code.carrier_hz == DEFAULT_CARRIER_HZ
    assert result.code.duty_percent == DEFAULT_DUTY_PERCENT
    persisted = json.loads(store.library_path(PROFILE_UUID).read_text(encoding="utf-8"))
    assert persisted["schema"] == IR_LIBRARY_SCHEMA
    assert persisted["profile_uuid"] == PROFILE_UUID
    assert persisted["device_id"] == DEVICE_ID
    text = json.dumps(persisted).casefold()
    for forbidden in ("password", "secret", "token", "credential", "bearer"):
        assert forbidden not in text


def test_ids_are_deterministic_ascii_safe_and_server_compatible() -> None:
    first = make_appliance_id(PROFILE_UUID, "Caixa de Som da Sala", "speaker")
    second = make_appliance_id(PROFILE_UUID, "Caixa de Som da Sala", "speaker")
    code = make_code_id(PROFILE_UUID, first, "Volume +")

    assert first == second
    assert first.startswith("appliance-caixa-de-som-da-sala-")
    assert code.startswith("ir-volume-")
    assert re.fullmatch(r"[A-Za-z0-9_.:-]{1,120}", first)
    assert re.fullmatch(r"[A-Za-z0-9_.:-]{1,120}", code)


def test_capture_deduplicates_by_hash_and_recapture_updates_stable_code(tmp_path: Path) -> None:
    store = IrLibraryStore(tmp_path / "ir-library")
    appliance = store.create_appliance(PROFILE_UUID, "TV", kind="television")
    first = store.save_capture(PROFILE_UUID, appliance.appliance_id, "Power", POWER_SIGNAL)
    duplicate = store.save_capture(
        PROFILE_UUID, appliance.appliance_id, "Outro nome", POWER_SIGNAL
    )
    replacement_signal = (8_950, 4_480, 550, 570)
    replacement = store.save_capture(
        PROFILE_UUID, appliance.appliance_id, "Power", replacement_signal
    )

    assert first.created
    assert not first.deduplicated
    assert duplicate.deduplicated
    assert duplicate.code.code_id == first.code.code_id
    assert not replacement.created
    assert not replacement.deduplicated
    assert replacement.code.code_id == first.code.code_id
    loaded = store.load(PROFILE_UUID)
    codes = loaded.get_appliance(appliance.appliance_id).codes
    assert len(codes) == 1
    assert codes[0].sequence_us == replacement_signal
    assert codes[0].signal_hash == compute_signal_hash(replacement_signal)


def test_pending_sync_payload_edit_and_offline_delete_tombstone(tmp_path: Path) -> None:
    store = IrLibraryStore(tmp_path / "ir-library")
    appliance = store.create_appliance(
        PROFILE_UUID, "Robô", device_id=DEVICE_ID, kind="robot_vacuum"
    )
    saved = store.save_capture(
        PROFILE_UUID,
        appliance.appliance_id,
        "Limpar",
        POWER_SIGNAL,
        device_id=DEVICE_ID,
    )
    assert [code.code_id for code in store.pending_codes(PROFILE_UUID)] == [saved.code.code_id]

    payload = saved.code.to_server_payload(DEVICE_ID)
    assert payload == {
        "id": saved.code.code_id,
        "device_id": DEVICE_ID,
        "name": "Limpar",
        "carrier_hz": 38_000,
        "duty_cycle": 0.33,
        "sequence_us": list(POWER_SIGNAL),
    }
    grouped_payload = saved.code.to_server_payload(
        DEVICE_ID,
        appliance_id=appliance.appliance_id,
        appliance_name=appliance.name,
    )
    assert grouped_payload["appliance_id"] == appliance.appliance_id
    assert grouped_payload["appliance_name"] == "Robô"
    expected_hash = saved.code.current_payload_hash(DEVICE_ID)
    synced = store.mark_code_synced(
        PROFILE_UUID, saved.code.code_id, expected_payload_hash=expected_hash
    )
    assert synced.sync_status == "synced"
    assert not store.pending_codes(PROFILE_UUID)

    edited = store.update_code(PROFILE_UUID, saved.code.code_id, name="Iniciar limpeza")
    assert edited.sync_status == "pending"
    assert store.pending_codes(PROFILE_UUID)
    assert store.delete_code(PROFILE_UUID, saved.code.code_id)
    deletions = store.pending_deletions(PROFILE_UUID)
    assert [(item.device_id, item.code_id) for item in deletions] == [
        (DEVICE_ID, saved.code.code_id)
    ]
    assert store.mark_deletion_synced(PROFILE_UUID, saved.code.code_id)
    assert not store.pending_deletions(PROFILE_UUID)


def test_stale_sync_ack_cannot_mark_new_capture_as_synced(tmp_path: Path) -> None:
    store = IrLibraryStore(tmp_path / "ir-library")
    appliance = store.create_appliance(PROFILE_UUID, "Som", device_id=DEVICE_ID)
    saved = store.save_capture(
        PROFILE_UUID, appliance.appliance_id, "Mute", POWER_SIGNAL, device_id=DEVICE_ID
    )
    stale_hash = saved.code.current_payload_hash(DEVICE_ID)
    store.update_code(PROFILE_UUID, saved.code.code_id, name="Silenciar")

    with pytest.raises(ValueError, match="mudou durante"):
        store.mark_code_synced(
            PROFILE_UUID, saved.code.code_id, expected_payload_hash=stale_hash
        )
    assert store.pending_codes(PROFILE_UUID)


def test_atomic_save_keeps_exactly_five_backups_and_no_temp_files(tmp_path: Path) -> None:
    store = IrLibraryStore(tmp_path / "ir-library")
    appliance = store.create_appliance(PROFILE_UUID, "TV")
    saved = store.save_capture(PROFILE_UUID, appliance.appliance_id, "Power", POWER_SIGNAL)
    for index in range(8):
        store.update_code(PROFILE_UUID, saved.code.code_id, name=f"Power {index}")

    backups = list((store.backups_dir / PROFILE_UUID).glob("*.json"))
    assert len(backups) == 5
    assert not list(store.root.rglob("*.tmp"))
    assert store.load(PROFILE_UUID).find_code(saved.code.code_id)[1].name == "Power 7"


def test_direct_dataclass_edits_are_revalidated_before_save(tmp_path: Path) -> None:
    store = IrLibraryStore(tmp_path / "ir-library")
    appliance = store.create_appliance(PROFILE_UUID, "TV", device_id=DEVICE_ID)
    code = store.save_capture(
        PROFILE_UUID, appliance.appliance_id, "Power", POWER_SIGNAL, device_id=DEVICE_ID
    ).code
    store.mark_code_synced(PROFILE_UUID, code.code_id)

    library = store.load(PROFILE_UUID)
    edited = library.find_code(code.code_id)[1]
    edited.name = "Power principal"
    edited.sequence_us = (1,)
    with pytest.raises(ValueError, match="2 a 2048"):
        store.save(library)

    edited.sequence_us = POWER_SIGNAL
    store.save(library)
    reloaded = store.load(PROFILE_UUID).find_code(code.code_id)[1]
    assert reloaded.name == "Power principal"
    assert reloaded.sync_status == "pending"


def test_crud_and_immutable_pairing(tmp_path: Path) -> None:
    store = IrLibraryStore(tmp_path / "ir-library")
    appliance = store.create_appliance(PROFILE_UUID, "Aparelho 1")
    renamed = store.update_appliance(
        PROFILE_UUID, appliance.appliance_id, name="Caixa de som", kind="speaker"
    )
    assert renamed.name == "Caixa de som"
    saved = store.save_capture(
        PROFILE_UUID, appliance.appliance_id, "Botão 1", POWER_SIGNAL
    )
    assert store.get_appliance(PROFILE_UUID, appliance.appliance_id).name == "Caixa de som"
    assert store.list_appliances(PROFILE_UUID)[0].appliance_id == appliance.appliance_id
    assert store.get_code(PROFILE_UUID, saved.code.code_id).name == "Botão 1"
    assert store.list_codes(PROFILE_UUID, appliance_id=appliance.appliance_id)[0].code_id == saved.code.code_id
    paired = store.get_or_create(PROFILE_UUID, DEVICE_ID)
    assert paired.device_id == DEVICE_ID
    assert paired.pending_codes()
    with pytest.raises(ValueError, match="imutável"):
        store.get_or_create(PROFILE_UUID, "esp32-001122334455")
    assert store.delete_appliance(PROFILE_UUID, appliance.appliance_id)
    assert not store.load(PROFILE_UUID).appliances
    assert not store.delete_code(PROFILE_UUID, saved.code.code_id)


def test_rejects_path_traversal_invalid_raw_signal_and_secret_fields(tmp_path: Path) -> None:
    store = IrLibraryStore(tmp_path / "ir-library")
    with pytest.raises(ValueError, match="profile_uuid"):
        store.get_or_create("../escape")
    appliance = store.create_appliance(PROFILE_UUID, "TV")
    with pytest.raises(ValueError, match="2 a 2048"):
        store.save_capture(PROFILE_UUID, appliance.appliance_id, "Inválido", [560])

    malicious = {
        "schema": IR_LIBRARY_SCHEMA,
        "profile_uuid": PROFILE_UUID,
        "device_id": "",
        "appliances": [],
        "pending_deletions": [],
        "mqtt_password": "não deve entrar",
    }
    with pytest.raises(ValueError, match="segredos"):
        IrLibrary.from_dict(malicious)


def test_command_params_use_raw_capture_without_exposing_library_metadata(tmp_path: Path) -> None:
    store = IrLibraryStore(tmp_path / "ir-library")
    appliance = store.create_appliance(PROFILE_UUID, "TV")
    code = store.save_capture(
        PROFILE_UUID, appliance.appliance_id, "Power", POWER_SIGNAL
    ).code
    assert code.to_command_params(repeats=2) == {
        "code_id": code.code_id,
        "carrier_hz": 38_000,
        "duty_percent": 33,
        "sequence_us": list(POWER_SIGNAL),
        "repeats": 2,
    }
    with pytest.raises(ValueError, match="repeats"):
        code.to_command_params(repeats=21)
