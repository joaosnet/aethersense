import json
import hashlib
from pathlib import Path

import pytest

from aethersense_build.sd_import import SESSION_SCHEMA, import_sd_sessions, inspect_sd


def make_session(root: Path, session_id: str, payload: str = "data") -> Path:
    session = root / "aethersense" / "sessions" / session_id
    session.mkdir(parents=True)
    (session / "events-0001.jsonl").write_text(payload, encoding="utf-8")
    digest = hashlib.sha256(payload.encode()).hexdigest()
    (session / "session.json").write_text(
        json.dumps(
            {
                "schema": SESSION_SCHEMA,
                "session_id": session_id,
                "finished_at_iso": "2026-07-15T10:30:00Z",
                "files_sha256": {"events-0001.jsonl": digest},
            }
        ),
        encoding="utf-8",
    )
    return session


def test_inspect_import_and_deduplicate_sd_session(tmp_path: Path) -> None:
    card = tmp_path / "card"
    make_session(card, "2026-07-15T10-00-00Z-c5")
    sessions = inspect_sd(card)
    assert len(sessions) == 1
    assert sessions[0].files == 2
    destination = tmp_path / "resultados" / "firmware-sd"
    first = import_sd_sessions(card, destination)
    assert first.imported == ("2026-07-15T10-00-00Z-c5",)
    second = import_sd_sessions(card, destination)
    assert second.skipped == ("2026-07-15T10-00-00Z-c5",)


def test_import_never_overwrites_conflicting_session(tmp_path: Path) -> None:
    card = tmp_path / "card"
    session_id = "2026-07-15T10-00-00Z-c5"
    make_session(card, session_id, "source")
    destination = tmp_path / "resultados" / "firmware-sd"
    make_session(destination.parent.parent, session_id, "unrelated-layout")
    target = destination / session_id
    target.mkdir(parents=True)
    (target / "session.json").write_text(
        json.dumps(
            {
                "schema": SESSION_SCHEMA,
                "session_id": session_id,
                "finished_at_iso": "2026-07-15T10:30:00Z",
                "files_sha256": {
                    "events-0001.jsonl": hashlib.sha256(b"different").hexdigest()
                },
            }
        ),
        encoding="utf-8",
    )
    (target / "events-0001.jsonl").write_text("different", encoding="utf-8")
    with pytest.raises(FileExistsError, match="nada foi sobrescrito"):
        import_sd_sessions(card, destination)


def test_raw_csi_import_requires_empty_room_or_bench_manifest(tmp_path: Path) -> None:
    card = tmp_path / "card"
    session = make_session(card, "2026-07-15T10-00-00Z-c5")
    (session / "csi-0001.csv").write_text("timestamp,csi\n1,2\n", encoding="utf-8")
    with pytest.raises(ValueError, match="privacy_mode=empty_room_bench"):
        inspect_sd(card)

    manifest = json.loads((session / "session.json").read_text(encoding="utf-8"))
    manifest.update({"raw_csi_enabled": True, "privacy_mode": "empty_room_bench"})
    manifest["files_sha256"]["csi-0001.csv"] = hashlib.sha256(
        (session / "csi-0001.csv").read_bytes()
    ).hexdigest()
    (session / "session.json").write_text(json.dumps(manifest), encoding="utf-8")
    assert len(inspect_sd(card)) == 1


def test_sd_manifest_hashes_reject_corruption_missing_and_unlisted_payload(tmp_path: Path) -> None:
    card = tmp_path / "card"
    session = make_session(card, "2026-07-15T10-00-00Z-c5")
    (session / "events-0001.jsonl").write_text("corrupted", encoding="utf-8")
    with pytest.raises(ValueError, match="SHA-256 diverge"):
        inspect_sd(card)

    session = make_session(tmp_path / "missing", "session-missing")
    (session / "events-0001.jsonl").unlink()
    with pytest.raises(ValueError, match="inseguro/ausente"):
        inspect_sd(tmp_path / "missing")

    session = make_session(tmp_path / "unlisted", "session-unlisted")
    (session / "events-0002.jsonl").write_text("extra", encoding="utf-8")
    with pytest.raises(ValueError, match="não declarados"):
        inspect_sd(tmp_path / "unlisted")


def test_explicit_active_session_may_have_uncheckpointed_payload(tmp_path: Path) -> None:
    card = tmp_path / "card"
    session = make_session(card, "active")
    manifest_path = session / "session.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    manifest["finished_at_iso"] = None
    manifest["files_sha256"] = {}
    manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
    assert inspect_sd(card)[0].session_id == "active"
