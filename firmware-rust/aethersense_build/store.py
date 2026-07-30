"""Atomic multi-device catalogue with drafts, backups and safe interchange."""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import tempfile
from typing import Any
from uuid import uuid4
import zipfile

from .diffing import ProfileDiff, diff_profiles
from .models import DeviceProfile, utc_now


def default_store_root() -> Path:
    explicit = os.environ.get("AETHERSENSE_DEVICE_STORE")
    if explicit:
        return Path(explicit).expanduser()
    if os.name == "nt":
        base = os.environ.get("LOCALAPPDATA")
        if base:
            return Path(base) / "AetherSense" / "devices"
        return Path.home() / "AppData" / "Local" / "AetherSense" / "devices"
    base = os.environ.get("XDG_CONFIG_HOME")
    if base:
        return Path(base) / "aethersense" / "devices"
    return Path.home() / ".config" / "aethersense" / "devices"


def _json_bytes(data: dict[str, Any], *, pretty: bool = True) -> bytes:
    if pretty:
        text = json.dumps(data, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
    else:
        text = json.dumps(data, ensure_ascii=False, separators=(",", ":"), sort_keys=True)
    return text.encode("utf-8")


def _atomic_write(path: Path, payload: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temp_name = tempfile.mkstemp(prefix=f".{path.name}.", suffix=".tmp", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temp_name, path)
    finally:
        try:
            os.unlink(temp_name)
        except FileNotFoundError:
            pass


@dataclass(frozen=True)
class ImportPreview:
    profile: DeviceProfile
    diff: ProfileDiff
    source: Path


@dataclass(frozen=True)
class CatalogueProfilePreview:
    profile: DeviceProfile
    diff: ProfileDiff


@dataclass(frozen=True)
class CatalogueImportPreview:
    profiles: tuple[CatalogueProfilePreview, ...]
    source: Path


class DeviceProfileStore:
    def __init__(self, root: Path | str | None = None, *, backup_limit: int = 5) -> None:
        self.root = Path(root) if root is not None else default_store_root()
        self.drafts_dir = self.root / "drafts"
        self.backups_dir = self.root / "backups"
        self.backup_limit = max(1, int(backup_limit))

    def profile_path(self, profile_id: str) -> Path:
        return self.root / f"{profile_id}.json"

    def draft_path(self, profile_id: str) -> Path:
        return self.drafts_dir / f"{profile_id}.json"

    def list(self, *, include_drafts: bool = True) -> list[tuple[DeviceProfile, bool]]:
        profiles: dict[str, tuple[DeviceProfile, bool]] = {}
        if self.root.exists():
            for path in sorted(self.root.glob("*.json")):
                try:
                    profile = self._read_path(path)
                except (OSError, ValueError, json.JSONDecodeError):
                    continue
                profiles[profile.profile_id] = (profile, False)
        if include_drafts and self.drafts_dir.exists():
            for path in sorted(self.drafts_dir.glob("*.json")):
                try:
                    profile = self._read_path(path)
                except (OSError, ValueError, json.JSONDecodeError):
                    continue
                profiles.setdefault(profile.profile_id, (profile, True))
        return sorted(
            profiles.values(), key=lambda item: (item[0].friendly_name.casefold(), item[0].profile_id)
        )

    def load(self, profile_id: str, *, include_draft: bool = True) -> DeviceProfile:
        path = self.profile_path(profile_id)
        if path.exists():
            return self._read_path(path)
        draft = self.draft_path(profile_id)
        if include_draft and draft.exists():
            return self._read_path(draft)
        raise FileNotFoundError(f"device profile not found: {profile_id}")

    def save(self, profile: DeviceProfile) -> Path:
        path = self.profile_path(profile.profile_id)
        if path.exists():
            current = self._read_path(path)
            if current.device_id and profile.device_id != current.device_id:
                raise ValueError("device_id is immutable after a board is paired")
            self._backup(path, profile.profile_id)
        profile.updated_at = utc_now()
        _atomic_write(path, _json_bytes(profile.to_dict()))
        try:
            self.draft_path(profile.profile_id).unlink()
        except FileNotFoundError:
            pass
        return path

    def save_draft(self, profile: DeviceProfile) -> Path:
        profile.updated_at = utc_now()
        path = self.draft_path(profile.profile_id)
        _atomic_write(path, _json_bytes(profile.to_dict()))
        return path

    def delete(self, profile_id: str) -> None:
        for path in (self.profile_path(profile_id), self.draft_path(profile_id)):
            try:
                path.unlink()
            except FileNotFoundError:
                pass

    def clone(self, profile: DeviceProfile, *, friendly_name: str | None = None) -> DeviceProfile:
        clone = DeviceProfile.from_dict(profile.to_dict())
        clone.profile_id = str(uuid4())
        clone.device_id = ""
        clone.friendly_name = friendly_name or f"{profile.friendly_name} (cópia)"
        clone.association.vid = ""
        clone.association.pid = ""
        clone.association.serial_number = ""
        clone.association.efuse_mac = ""
        clone.network.wifi.password_ref = ""
        clone.mqtt.password_ref = ""
        clone.server.admin_bearer_token_ref = ""
        clone.server.bearer_token_ref = ""
        clone.secret_revisions = {}
        clone.applied.build_hash = ""
        clone.applied.config_hash = ""
        clone.applied.last_seen_at = ""
        clone.created_at = utc_now()
        clone.updated_at = clone.created_at
        return clone

    def preview_import(
        self, source: Path | str, *, compare_to: DeviceProfile | None = None
    ) -> ImportPreview:
        path = Path(source)
        profile = self._read_import(path)
        # An imported file is a portability artifact, never an authority for
        # local keyring state. Force fresh revisions after credentials are
        # entered/linked on this machine, even when UUID preservation is chosen.
        profile.secret_revisions = {}
        return ImportPreview(profile, diff_profiles(compare_to, profile), path)

    def import_profile(
        self,
        source: Path | str,
        *,
        copy_profile: bool = True,
        compare_to: DeviceProfile | None = None,
    ) -> DeviceProfile:
        preview = self.preview_import(source, compare_to=compare_to)
        profile = self.clone(preview.profile) if copy_profile else preview.profile
        self.save_draft(profile)
        return profile

    def preview_catalogue_zip(self, source: Path | str) -> CatalogueImportPreview:
        path = Path(source)
        if path.suffix.lower() != ".zip":
            raise ValueError("catálogo deve ser um arquivo ZIP")
        existing = {
            profile.profile_id: profile
            for profile, _draft in self.list(include_drafts=True)
        }
        with zipfile.ZipFile(path, "r") as archive:
            names = archive.namelist()
            if len(names) != len(set(names)):
                raise ValueError("catálogo contém entradas ZIP duplicadas")
            for name in names:
                normalized = name.replace("\\", "/")
                pure = PurePosixPath(normalized)
                if (
                    name != normalized
                    or pure.is_absolute()
                    or ".." in pure.parts
                    or any(":" in part for part in pure.parts)
                    or "\x00" in name
                ):
                    raise ValueError("catálogo contém caminho ZIP inseguro")
            if "catalogue.json" not in names:
                raise ValueError("ZIP não contém manifesto catalogue.json")
            if archive.getinfo("catalogue.json").file_size > 1024 * 1024:
                raise ValueError("manifesto do catálogo excede 1 MiB")
            manifest = json.loads(archive.read("catalogue.json").decode("utf-8"))
            if manifest.get("schema") != "aethersense.device_catalogue.v1":
                raise ValueError("schema de catálogo não suportado")
            profile_ids = manifest.get("profiles")
            if not isinstance(profile_ids, list) or not all(
                isinstance(item, str) and item for item in profile_ids
            ):
                raise ValueError("manifesto do catálogo possui lista de perfis inválida")
            if len(profile_ids) > 256 or len(set(profile_ids)) != len(profile_ids):
                raise ValueError("catálogo excede 256 perfis ou contém UUID duplicado")
            expected = {"catalogue.json"} | {
                f"profiles/{profile_id}.json" for profile_id in profile_ids
            }
            if set(names) != expected:
                raise ValueError("entradas do ZIP divergem do manifesto do catálogo")
            previews: list[CatalogueProfilePreview] = []
            for profile_id in profile_ids:
                member = f"profiles/{profile_id}.json"
                if archive.getinfo(member).file_size > 1024 * 1024:
                    raise ValueError(f"perfil {profile_id} excede 1 MiB")
                profile = DeviceProfile.from_dict(
                    json.loads(archive.read(member).decode("utf-8"))
                )
                profile.secret_revisions = {}
                if profile.profile_id != profile_id:
                    raise ValueError(
                        f"UUID interno de {member} diverge do manifesto"
                    )
                previews.append(
                    CatalogueProfilePreview(
                        profile,
                        diff_profiles(existing.get(profile_id), profile),
                    )
                )
        return CatalogueImportPreview(tuple(previews), path)

    def import_catalogue_zip(
        self,
        source: Path | str,
        *,
        copy_profiles: bool = True,
    ) -> tuple[DeviceProfile, ...]:
        preview = self.preview_catalogue_zip(source)
        imported: list[DeviceProfile] = []
        for item in preview.profiles:
            if copy_profiles:
                profile = self.clone(item.profile)
            else:
                profile = item.profile
                if self.profile_path(profile.profile_id).exists() or self.draft_path(
                    profile.profile_id
                ).exists():
                    raise ValueError(
                        f"perfil {profile.profile_id} já existe; importe como cópia"
                    )
            self.save_draft(profile)
            imported.append(profile)
        return tuple(imported)

    def export_profile(self, profile: DeviceProfile, destination: Path | str) -> Path:
        destination = Path(destination)
        public_data = profile.to_dict(include_local_state=False)
        payload = _json_bytes(public_data)
        if destination.suffix.lower() == ".zip":
            destination.parent.mkdir(parents=True, exist_ok=True)
            fd, temp_name = tempfile.mkstemp(
                prefix=f".{destination.name}.", suffix=".tmp", dir=destination.parent
            )
            os.close(fd)
            try:
                info = zipfile.ZipInfo(f"{profile.profile_id}.json")
                info.date_time = (1980, 1, 1, 0, 0, 0)
                info.compress_type = zipfile.ZIP_DEFLATED
                info.external_attr = 0o600 << 16
                with zipfile.ZipFile(temp_name, "w") as archive:
                    archive.writestr(info, payload)
                os.replace(temp_name, destination)
            finally:
                try:
                    os.unlink(temp_name)
                except FileNotFoundError:
                    pass
        else:
            _atomic_write(destination, payload)
        return destination

    def export_catalogue_zip(
        self, profiles: list[DeviceProfile] | tuple[DeviceProfile, ...], destination: Path | str
    ) -> Path:
        """Export a deterministic, secret-free multi-profile archive."""
        destination = Path(destination)
        destination.parent.mkdir(parents=True, exist_ok=True)
        ordered = sorted(profiles, key=lambda item: item.profile_id)
        manifest = {
            "schema": "aethersense.device_catalogue.v1",
            "profiles": [profile.profile_id for profile in ordered],
        }
        entries = [("catalogue.json", _json_bytes(manifest))]
        entries.extend(
            (
                f"profiles/{profile.profile_id}.json",
                _json_bytes(profile.to_dict(include_local_state=False)),
            )
            for profile in ordered
        )
        fd, temp_name = tempfile.mkstemp(
            prefix=f".{destination.name}.", suffix=".tmp", dir=destination.parent
        )
        os.close(fd)
        try:
            with zipfile.ZipFile(temp_name, "w") as archive:
                for name, payload in entries:
                    info = zipfile.ZipInfo(name)
                    info.date_time = (1980, 1, 1, 0, 0, 0)
                    info.compress_type = zipfile.ZIP_DEFLATED
                    info.external_attr = 0o600 << 16
                    archive.writestr(info, payload)
            os.replace(temp_name, destination)
        finally:
            try:
                os.unlink(temp_name)
            except FileNotFoundError:
                pass
        return destination

    def write_firmware_payload(self, profile: DeviceProfile, destination: Path | str) -> Path:
        destination = Path(destination)
        _atomic_write(destination, profile.firmware_json().encode("utf-8"))
        return destination

    def _read_path(self, path: Path) -> DeviceProfile:
        return DeviceProfile.from_dict(json.loads(path.read_text(encoding="utf-8")))

    def _read_import(self, path: Path) -> DeviceProfile:
        if path.suffix.lower() != ".zip":
            return self._read_path(path)
        with zipfile.ZipFile(path, "r") as archive:
            members = sorted(
                item for item in archive.namelist() if item.lower().endswith(".json") and "/" not in item.strip("/")
            )
            if len(members) != 1:
                raise ValueError("profile ZIP must contain exactly one top-level JSON file")
            if archive.getinfo(members[0]).file_size > 1024 * 1024:
                raise ValueError("profile JSON exceeds 1 MiB")
            data = json.loads(archive.read(members[0]).decode("utf-8"))
        return DeviceProfile.from_dict(data)

    def _backup(self, path: Path, profile_id: str) -> None:
        directory = self.backups_dir / profile_id
        directory.mkdir(parents=True, exist_ok=True)
        payload = path.read_bytes()
        digest = hashlib.sha256(payload).hexdigest()[:10]
        stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
        backup = directory / f"{stamp}-{digest}.json"
        _atomic_write(backup, payload)
        backups = sorted(directory.glob("*.json"), reverse=True)
        for stale in backups[self.backup_limit :]:
            stale.unlink()
