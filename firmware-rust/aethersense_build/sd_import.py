"""Inspect and atomically import firmware microSD sessions into resultados/."""

from __future__ import annotations

from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
from uuid import uuid4


SESSION_SCHEMA = "aethersense.firmware_sd_session.v1"
SHA256_RE = re.compile(r"^[0-9a-fA-F]{64}$")


@dataclass(frozen=True)
class SdSessionInfo:
    session_id: str
    source: Path
    files: int
    bytes: int
    sha256: str


@dataclass(frozen=True)
class SdImportReport:
    discovered: tuple[SdSessionInfo, ...]
    imported: tuple[str, ...]
    skipped: tuple[str, ...]


def _sessions_dir(source: Path) -> Path:
    candidates = (
        source / "aethersense" / "sessions",
        source / "sessions",
        source,
    )
    for candidate in candidates:
        if candidate.is_dir() and any(candidate.glob("*/session.json")):
            return candidate
    raise ValueError("nenhuma sessão AetherSense encontrada no cartão/pasta")


def _directory_digest(directory: Path) -> tuple[str, int, int]:
    digest = hashlib.sha256()
    count = 0
    total = 0
    for path in sorted(directory.rglob("*"), key=lambda item: item.as_posix()):
        if path.is_symlink():
            raise ValueError(f"links simbólicos não são aceitos no cartão: {path.name}")
        if not path.is_file():
            continue
        relative = path.relative_to(directory).as_posix().encode("utf-8")
        digest.update(len(relative).to_bytes(4, "little"))
        digest.update(relative)
        with path.open("rb") as handle:
            while block := handle.read(1024 * 1024):
                digest.update(block)
                total += len(block)
        count += 1
    return digest.hexdigest(), count, total


def _file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while block := handle.read(1024 * 1024):
            digest.update(block)
    return digest.hexdigest()


def _validate_manifest_hashes(directory: Path, data: dict, manifest: Path) -> None:
    declared = data.get("files_sha256")
    if not isinstance(declared, dict):
        raise ValueError(f"files_sha256 ausente ou inválido em {manifest}")
    active_incomplete = "finished_at_iso" in data and data.get("finished_at_iso") is None
    actual_payloads = {
        path.name: path
        for path in directory.iterdir()
        if path.is_file()
        and (
            (path.name.startswith("events-") and path.name.endswith(".jsonl"))
            or (path.name.startswith("csi-") and path.name.endswith(".csv"))
        )
    }
    for name, expected in declared.items():
        if (
            not isinstance(name, str)
            or Path(name).name != name
            or "/" in name
            or "\\" in name
            or ".." in name
            or name not in actual_payloads
        ):
            raise ValueError(f"arquivo declarado inseguro/ausente em {manifest}: {name!r}")
        if not isinstance(expected, str) or not SHA256_RE.fullmatch(expected):
            raise ValueError(f"SHA-256 declarado inválido em {manifest}: {name}")
        actual = _file_sha256(actual_payloads[name])
        if actual.lower() != expected.lower():
            raise ValueError(f"SHA-256 diverge para {name} em {manifest}")
    undeclared = set(actual_payloads) - set(declared)
    if undeclared and not active_incomplete:
        raise ValueError(
            f"arquivos de payload não declarados em {manifest}: {', '.join(sorted(undeclared))}"
        )
    if not declared and actual_payloads and not active_incomplete:
        raise ValueError(f"sessão finalizada sem hashes de payload em {manifest}")


def inspect_sd(source: Path | str) -> tuple[SdSessionInfo, ...]:
    source = Path(source).expanduser().resolve()
    sessions_dir = _sessions_dir(source).resolve()
    result: list[SdSessionInfo] = []
    for manifest in sorted(sessions_dir.glob("*/session.json")):
        directory = manifest.parent.resolve()
        try:
            directory.relative_to(sessions_dir)
        except ValueError as exc:
            raise ValueError("sessão fora da raiz do cartão") from exc
        data = json.loads(manifest.read_text(encoding="utf-8"))
        if not isinstance(data, dict) or data.get("schema") != SESSION_SCHEMA:
            raise ValueError(f"schema inválido em {manifest}")
        contains_raw_csi = bool(data.get("raw_csi_enabled")) or any(
            directory.glob("csi-*.csv")
        )
        if contains_raw_csi and data.get("privacy_mode") != "empty_room_bench":
            raise ValueError(
                f"CSI bruto em {manifest} exige privacy_mode=empty_room_bench; "
                "sessões humanas locais não são aceitas"
            )
        _validate_manifest_hashes(directory, data, manifest)
        session_id = directory.name
        declared_id = data.get("session_id")
        if declared_id not in {None, "", session_id}:
            raise ValueError(f"session_id divergente em {manifest}")
        sha256, count, total = _directory_digest(directory)
        result.append(SdSessionInfo(session_id, directory, count, total, sha256))
    return tuple(result)


def import_sd_sessions(
    source: Path | str,
    destination_root: Path | str,
) -> SdImportReport:
    sessions = inspect_sd(source)
    destination_root = Path(destination_root).expanduser().resolve()
    destination_root.mkdir(parents=True, exist_ok=True)
    imported: list[str] = []
    skipped: list[str] = []
    for session in sessions:
        destination = destination_root / session.session_id
        if destination.exists():
            existing_sha, _count, _bytes = _directory_digest(destination)
            if existing_sha == session.sha256:
                skipped.append(session.session_id)
                continue
            raise FileExistsError(
                f"sessão {session.session_id} já existe com conteúdo diferente; nada foi sobrescrito"
            )
        temporary = destination_root / f".{session.session_id}.{uuid4().hex}.importing"
        try:
            shutil.copytree(session.source, temporary, copy_function=shutil.copy2)
            copied_sha, _count, _bytes = _directory_digest(temporary)
            if copied_sha != session.sha256:
                raise OSError(f"SHA-256 divergiu ao copiar {session.session_id}")
            os.replace(temporary, destination)
        finally:
            if temporary.exists():
                shutil.rmtree(temporary)
        imported.append(session.session_id)
    return SdImportReport(sessions, tuple(imported), tuple(skipped))
