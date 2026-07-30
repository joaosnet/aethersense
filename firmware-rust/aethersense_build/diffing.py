"""Canonical hashes, field-level diffs and apply-state decisions."""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
import hashlib
import json
from pathlib import Path
from typing import Any

from .models import DeviceProfile
from .validation import has_errors, validate_profile


@dataclass(frozen=True)
class FieldChange:
    path: str
    before: Any
    after: Any


@dataclass(frozen=True)
class ProfileDiff:
    changes: tuple[FieldChange, ...]
    requires_build: bool
    requires_provision: bool

    @property
    def is_empty(self) -> bool:
        return not self.changes


BUILD_PATH_PREFIXES = (
    "target.profile",
    "target.partitions",
    "target.features",
)
IGNORED_DIFF_PREFIXES = (
    "applied",
    "created_at",
    "updated_at",
)


def _canonical_hash(value: Any) -> str:
    payload = json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=True)
    return hashlib.sha256(payload.encode("utf-8")).hexdigest()


FIRMWARE_SOURCE_FILES = (
    ".cargo/config.toml",
    "Cargo.lock",
    "Cargo.toml",
    "build.rs",
    "espflash.toml",
    "partitions.csv",
    "rust-toolchain.toml",
    "sdkconfig.example",
)


def compute_firmware_source_hash(source_root: str | Path | None = None) -> str:
    """Hash every deterministic input that can change the firmware binary.

    Generated files (``sdkconfig``, ``sdkconfig.defaults``), build artefacts and
    logs are deliberately excluded.  The build tool recreates those from the
    profile and ``sdkconfig.example`` before each build.
    """

    root = (
        Path(source_root).resolve()
        if source_root is not None
        else Path(__file__).resolve().parents[1]
    )
    candidates = [root / relative for relative in FIRMWARE_SOURCE_FILES]
    candidates.extend(sorted((root / "src").rglob("*.rs")))

    digest = hashlib.sha256()
    for path in sorted(
        (candidate for candidate in candidates if candidate.is_file()),
        key=lambda candidate: candidate.relative_to(root).as_posix(),
    ):
        relative = path.relative_to(root).as_posix().encode("utf-8")
        content = path.read_bytes()
        digest.update(len(relative).to_bytes(4, "big"))
        digest.update(relative)
        digest.update(len(content).to_bytes(8, "big"))
        digest.update(content)
    return digest.hexdigest()


def compute_build_hash(
    profile: DeviceProfile, *, source_root: str | Path | None = None
) -> str:
    payload = {
        "target": profile.firmware_dict()["target"],
        "firmware_source_hash": compute_firmware_source_hash(source_root),
    }
    return _canonical_hash(payload)


def compute_config_hash(profile: DeviceProfile) -> str:
    payload = profile.firmware_dict()
    payload = dict(payload)
    payload.pop("target", None)
    return _canonical_hash(payload)


def _flatten(value: Any, prefix: str = "") -> dict[str, Any]:
    result: dict[str, Any] = {}
    if isinstance(value, dict):
        for key in sorted(value):
            path = f"{prefix}.{key}" if prefix else str(key)
            result.update(_flatten(value[key], path))
    elif isinstance(value, list):
        for index, item in enumerate(value):
            result.update(_flatten(item, f"{prefix}[{index}]"))
        if not value:
            result[prefix] = []
    else:
        result[prefix] = value
    return result


def diff_profiles(before: DeviceProfile | None, after: DeviceProfile) -> ProfileDiff:
    before_flat = _flatten(before.to_dict() if before is not None else {})
    after_flat = _flatten(after.to_dict())
    paths = sorted(set(before_flat) | set(after_flat))
    changes = tuple(
        FieldChange(path, before_flat.get(path), after_flat.get(path))
        for path in paths
        if before_flat.get(path) != after_flat.get(path)
        and not path.startswith(IGNORED_DIFF_PREFIXES)
    )
    requires_build = before is None or any(
        change.path.startswith(BUILD_PATH_PREFIXES) for change in changes
    )
    requires_provision = before is None or any(
        not change.path.startswith(BUILD_PATH_PREFIXES) for change in changes
    )
    return ProfileDiff(changes, requires_build, requires_provision)


OFFLINE_AFTER = timedelta(minutes=5)


def profile_status(
    profile: DeviceProfile,
    *,
    draft: bool = False,
    require_hardware_validation: bool = False,
    now: datetime | None = None,
    offline_after: timedelta = OFFLINE_AFTER,
) -> str:
    if has_errors(validate_profile(profile)):
        return "Inválido"
    if draft or not profile.device_id:
        return "Rascunho"
    if profile.applied.build_hash != compute_build_hash(profile):
        return "Requer build"
    if profile.applied.config_hash != compute_config_hash(profile):
        return "Requer provisionamento"
    if not profile.applied.last_seen_at:
        return "Offline"
    try:
        seen = datetime.fromisoformat(profile.applied.last_seen_at.replace("Z", "+00:00"))
        if seen.tzinfo is None:
            seen = seen.replace(tzinfo=timezone.utc)
    except ValueError:
        return "Offline"
    current = now or datetime.now(timezone.utc)
    if current.astimezone(timezone.utc) - seen.astimezone(timezone.utc) > offline_after:
        return "Offline"
    if require_hardware_validation:
        validation_matches = (
            profile.applied.hardware_validation_status == "passed"
            and profile.applied.hardware_validation_build_hash
            == compute_build_hash(profile)
            and profile.applied.hardware_validation_config_hash
            == compute_config_hash(profile)
        )
        if validation_matches:
            return "Sincronizado e testado"
        return "Requer teste físico"
    return "Sincronizado"
