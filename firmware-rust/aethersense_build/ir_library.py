"""Persistent, secret-free library for raw infrared captures.

The TUI uses this module as its local source of truth while the AetherSense
server is unavailable.  A library belongs to one device profile and may be
paired to exactly one immutable ``device_id`` after the board is identified.
All mutations are written atomically and retain a bounded set of backups.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import tempfile
import threading
from typing import Any, Iterable, Literal
import unicodedata

from .models import utc_now


IR_LIBRARY_SCHEMA = "aethersense.ir_library.v1"
DEFAULT_CARRIER_HZ = 38_000
DEFAULT_DUTY_PERCENT = 33
MAX_IR_DURATIONS = 2_048
MAX_IR_FRAME_US = 5_000_000

SyncStatus = Literal["pending", "synced"]

_SAFE_ID_RE = re.compile(r"^[A-Za-z0-9_.:-]{1,120}$")
_SAFE_FILE_ID_RE = re.compile(r"^[A-Za-z0-9_.-]{1,120}$")
_SECRET_KEY_PARTS = (
    "password",
    "passwd",
    "secret",
    "token",
    "credential",
    "bearer",
    "api_key",
    "apikey",
)


def default_ir_library_root() -> Path:
    """Return the platform-specific directory used by the local IR library."""

    explicit = os.environ.get("AETHERSENSE_IR_LIBRARY_STORE")
    if explicit:
        return Path(explicit).expanduser()
    if os.name == "nt":
        base = os.environ.get("LOCALAPPDATA")
        if base:
            return Path(base) / "AetherSense" / "ir-library"
        return Path.home() / "AppData" / "Local" / "AetherSense" / "ir-library"
    base = os.environ.get("XDG_CONFIG_HOME")
    if base:
        return Path(base) / "aethersense" / "ir-library"
    return Path.home() / ".config" / "aethersense" / "ir-library"


def _normalise_identity_part(value: str) -> str:
    return unicodedata.normalize("NFKC", value).strip().casefold()


def _slug(value: str, *, fallback: str) -> str:
    ascii_value = (
        unicodedata.normalize("NFKD", value)
        .encode("ascii", "ignore")
        .decode("ascii")
        .lower()
    )
    result = re.sub(r"[^a-z0-9]+", "-", ascii_value).strip("-")
    return (result or fallback)[:36].rstrip("-")


def _safe_identifier(value: str, field_name: str, *, file_safe: bool = False) -> str:
    value = value.strip() if isinstance(value, str) else ""
    matcher = _SAFE_FILE_ID_RE if file_safe else _SAFE_ID_RE
    if not matcher.fullmatch(value) or value in {".", ".."}:
        raise ValueError(f"{field_name} inválido")
    return value


def _optional_device_id(value: str) -> str:
    value = value.strip() if isinstance(value, str) else ""
    return _safe_identifier(value, "device_id") if value else ""


def _friendly_name(value: str, field_name: str = "nome") -> str:
    value = value.strip() if isinstance(value, str) else ""
    if not value or len(value) > 120 or any(ord(character) < 32 for character in value):
        raise ValueError(f"{field_name} inválido")
    return value


def _kind(value: str) -> str:
    value = _normalise_identity_part(value or "custom").replace(" ", "_")
    return _safe_identifier(value, "tipo do aparelho")


def deterministic_identifier(prefix: str, label: str, *scope: str) -> str:
    """Build a stable identifier accepted by firmware, MQTT and the server."""

    prefix = _safe_identifier(prefix, "prefixo").lower().strip("-_.:")
    if not prefix:
        raise ValueError("prefixo inválido")
    canonical = "\x1f".join(_normalise_identity_part(part) for part in (*scope, label))
    digest = hashlib.sha256(canonical.encode("utf-8")).hexdigest()[:12]
    return f"{prefix}-{_slug(label, fallback='item')}-{digest}"


def make_appliance_id(profile_uuid: str, name: str, kind: str = "custom") -> str:
    profile_uuid = _safe_identifier(profile_uuid, "profile_uuid", file_safe=True)
    name = _friendly_name(name, "nome do aparelho")
    kind = _kind(kind)
    return deterministic_identifier("appliance", name, profile_uuid, kind)


def make_code_id(profile_uuid: str, appliance_id: str, name: str) -> str:
    profile_uuid = _safe_identifier(profile_uuid, "profile_uuid", file_safe=True)
    appliance_id = _safe_identifier(appliance_id, "appliance_id")
    name = _friendly_name(name, "nome do botão")
    return deterministic_identifier("ir", name, profile_uuid, appliance_id)


def _validated_carrier(value: Any) -> int:
    if isinstance(value, bool):
        raise ValueError("carrier_hz inválido")
    try:
        carrier = int(value)
    except (TypeError, ValueError) as exc:
        raise ValueError("carrier_hz inválido") from exc
    if not 10_000 <= carrier <= 100_000:
        raise ValueError("carrier_hz deve ficar entre 10000 e 100000")
    return carrier


def _validated_duty(value: Any) -> int:
    if isinstance(value, bool):
        raise ValueError("duty_percent inválido")
    try:
        duty = int(value)
    except (TypeError, ValueError) as exc:
        raise ValueError("duty_percent inválido") from exc
    if not 5 <= duty <= 95:
        raise ValueError("duty_percent deve ficar entre 5 e 95")
    return duty


def _validated_sequence(sequence_us: Iterable[int]) -> tuple[int, ...]:
    if isinstance(sequence_us, (str, bytes, bytearray)):
        raise ValueError("sequence_us deve ser uma lista de durações")
    try:
        raw_values = tuple(sequence_us)
    except TypeError as exc:
        raise ValueError("sequence_us deve ser uma lista de durações") from exc
    if not 2 <= len(raw_values) <= MAX_IR_DURATIONS:
        raise ValueError(f"sequence_us deve conter de 2 a {MAX_IR_DURATIONS} durações")
    values: list[int] = []
    for raw in raw_values:
        if isinstance(raw, bool):
            raise ValueError("sequence_us contém duração inválida")
        try:
            value = int(raw)
        except (TypeError, ValueError) as exc:
            raise ValueError("sequence_us contém duração inválida") from exc
        if value <= 0 or value > MAX_IR_FRAME_US:
            raise ValueError("sequence_us contém duração fora do limite")
        values.append(value)
    if sum(values) > MAX_IR_FRAME_US:
        raise ValueError("a duração total do sinal IR excede 5 segundos")
    return tuple(values)


def compute_signal_hash(
    sequence_us: Iterable[int],
    *,
    carrier_hz: int = DEFAULT_CARRIER_HZ,
    duty_percent: int = DEFAULT_DUTY_PERCENT,
) -> str:
    """Return a canonical SHA-256 used to deduplicate raw captures."""

    sequence = _validated_sequence(sequence_us)
    carrier = _validated_carrier(carrier_hz)
    duty = _validated_duty(duty_percent)
    document = {
        "carrier_hz": carrier,
        "duty_percent": duty,
        "sequence_us": list(sequence),
    }
    payload = json.dumps(document, separators=(",", ":"), sort_keys=True).encode("utf-8")
    return hashlib.sha256(payload).hexdigest()


def _payload_hash(document: dict[str, Any]) -> str:
    payload = json.dumps(
        document, ensure_ascii=False, separators=(",", ":"), sort_keys=True
    ).encode("utf-8")
    return hashlib.sha256(payload).hexdigest()


def _assert_secret_free(value: Any) -> None:
    if isinstance(value, dict):
        for key, nested in value.items():
            normalized = str(key).casefold().replace("-", "_")
            if any(part in normalized for part in _SECRET_KEY_PARTS):
                raise ValueError("a biblioteca IR não pode conter segredos ou credenciais")
            _assert_secret_free(nested)
    elif isinstance(value, list):
        for nested in value:
            _assert_secret_free(nested)


@dataclass
class IrRawCode:
    code_id: str
    name: str
    sequence_us: tuple[int, ...]
    carrier_hz: int = DEFAULT_CARRIER_HZ
    duty_percent: int = DEFAULT_DUTY_PERCENT
    signal_hash: str = ""
    sync_status: SyncStatus = "pending"
    synced_payload_hash: str = ""
    synced_at: str = ""
    created_at: str = field(default_factory=utc_now)
    updated_at: str = field(default_factory=utc_now)

    def __post_init__(self) -> None:
        self.code_id = _safe_identifier(self.code_id, "code_id")
        self.name = _friendly_name(self.name, "nome do botão")
        self.sequence_us = _validated_sequence(self.sequence_us)
        self.carrier_hz = _validated_carrier(self.carrier_hz)
        self.duty_percent = _validated_duty(self.duty_percent)
        expected = compute_signal_hash(
            self.sequence_us,
            carrier_hz=self.carrier_hz,
            duty_percent=self.duty_percent,
        )
        if self.signal_hash and self.signal_hash != expected:
            self.sync_status = "pending"
            self.synced_payload_hash = ""
            self.synced_at = ""
        self.signal_hash = expected
        if self.sync_status not in {"pending", "synced"}:
            self.sync_status = "pending"
        if self.synced_payload_hash and not re.fullmatch(r"[0-9a-f]{64}", self.synced_payload_hash):
            self.sync_status = "pending"
            self.synced_payload_hash = ""
            self.synced_at = ""

    @classmethod
    def from_dict(cls, data: Any) -> "IrRawCode":
        if not isinstance(data, dict):
            raise ValueError("código IR deve ser um objeto")
        return cls(
            code_id=str(data.get("code_id") or ""),
            name=str(data.get("name") or ""),
            sequence_us=tuple(data.get("sequence_us") or ()),
            carrier_hz=data.get("carrier_hz", DEFAULT_CARRIER_HZ),
            duty_percent=data.get("duty_percent", DEFAULT_DUTY_PERCENT),
            signal_hash=str(data.get("signal_hash") or ""),
            sync_status=str(data.get("sync_status") or "pending"),  # type: ignore[arg-type]
            synced_payload_hash=str(data.get("synced_payload_hash") or ""),
            synced_at=str(data.get("synced_at") or ""),
            created_at=str(data.get("created_at") or utc_now()),
            updated_at=str(data.get("updated_at") or utc_now()),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "carrier_hz": self.carrier_hz,
            "code_id": self.code_id,
            "created_at": self.created_at,
            "duty_percent": self.duty_percent,
            "name": self.name,
            "sequence_us": list(self.sequence_us),
            "signal_hash": self.signal_hash,
            "sync_status": self.sync_status,
            "synced_at": self.synced_at,
            "synced_payload_hash": self.synced_payload_hash,
            "updated_at": self.updated_at,
        }

    def to_server_payload(
        self,
        device_id: str,
        *,
        appliance_id: str = "",
        appliance_name: str = "",
    ) -> dict[str, Any]:
        device_id = _safe_identifier(device_id, "device_id")
        payload = {
            "id": self.code_id,
            "device_id": device_id,
            "name": self.name,
            "carrier_hz": self.carrier_hz,
            "duty_cycle": self.duty_percent / 100.0,
            "sequence_us": list(self.sequence_us),
        }
        if appliance_id:
            payload["appliance_id"] = _safe_identifier(
                appliance_id, "appliance_id"
            )
        if appliance_name:
            payload["appliance_name"] = _friendly_name(
                appliance_name, "nome do aparelho"
            )
        return payload

    def to_command_params(self, *, repeats: int = 1) -> dict[str, Any]:
        if isinstance(repeats, bool) or not 1 <= int(repeats) <= 20:
            raise ValueError("repeats deve ficar entre 1 e 20")
        return {
            "code_id": self.code_id,
            "carrier_hz": self.carrier_hz,
            "duty_percent": self.duty_percent,
            "sequence_us": list(self.sequence_us),
            "repeats": int(repeats),
        }

    def current_payload_hash(self, device_id: str) -> str:
        return _payload_hash(self.to_server_payload(device_id))

    def is_synced(self, device_id: str) -> bool:
        if not device_id or self.sync_status != "synced" or not self.synced_payload_hash:
            return False
        return self.synced_payload_hash == self.current_payload_hash(device_id)

    def mark_pending(self) -> None:
        self.sync_status = "pending"
        self.synced_at = ""


@dataclass
class IrAppliance:
    appliance_id: str
    name: str
    kind: str = "custom"
    codes: list[IrRawCode] = field(default_factory=list)
    created_at: str = field(default_factory=utc_now)
    updated_at: str = field(default_factory=utc_now)

    def __post_init__(self) -> None:
        self.appliance_id = _safe_identifier(self.appliance_id, "appliance_id")
        self.name = _friendly_name(self.name, "nome do aparelho")
        self.kind = _kind(self.kind)
        identifiers = [code.code_id for code in self.codes]
        if len(identifiers) != len(set(identifiers)):
            raise ValueError("aparelho contém code_id duplicado")

    @classmethod
    def from_dict(cls, data: Any) -> "IrAppliance":
        if not isinstance(data, dict):
            raise ValueError("aparelho IR deve ser um objeto")
        raw_codes = data.get("codes", [])
        if not isinstance(raw_codes, list):
            raise ValueError("codes deve ser uma lista")
        return cls(
            appliance_id=str(data.get("appliance_id") or ""),
            name=str(data.get("name") or ""),
            kind=str(data.get("kind") or "custom"),
            codes=[IrRawCode.from_dict(item) for item in raw_codes],
            created_at=str(data.get("created_at") or utc_now()),
            updated_at=str(data.get("updated_at") or utc_now()),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "appliance_id": self.appliance_id,
            "codes": [code.to_dict() for code in sorted(self.codes, key=lambda item: item.code_id)],
            "created_at": self.created_at,
            "kind": self.kind,
            "name": self.name,
            "updated_at": self.updated_at,
        }

    def get_code(self, code_id: str) -> IrRawCode:
        code_id = _safe_identifier(code_id, "code_id")
        for code in self.codes:
            if code.code_id == code_id:
                return code
        raise KeyError(f"código IR não encontrado: {code_id}")


@dataclass
class IrCodeDeletion:
    code_id: str
    device_id: str
    deleted_at: str = field(default_factory=utc_now)

    def __post_init__(self) -> None:
        self.code_id = _safe_identifier(self.code_id, "code_id")
        self.device_id = _safe_identifier(self.device_id, "device_id")

    @classmethod
    def from_dict(cls, data: Any) -> "IrCodeDeletion":
        if not isinstance(data, dict):
            raise ValueError("exclusão pendente deve ser um objeto")
        return cls(
            code_id=str(data.get("code_id") or ""),
            device_id=str(data.get("device_id") or ""),
            deleted_at=str(data.get("deleted_at") or utc_now()),
        )

    def to_dict(self) -> dict[str, str]:
        return {
            "code_id": self.code_id,
            "deleted_at": self.deleted_at,
            "device_id": self.device_id,
        }


@dataclass
class IrLibrary:
    profile_uuid: str
    device_id: str = ""
    appliances: list[IrAppliance] = field(default_factory=list)
    pending_deletions: list[IrCodeDeletion] = field(default_factory=list)
    created_at: str = field(default_factory=utc_now)
    updated_at: str = field(default_factory=utc_now)

    def __post_init__(self) -> None:
        self.profile_uuid = _safe_identifier(self.profile_uuid, "profile_uuid", file_safe=True)
        self.device_id = _optional_device_id(self.device_id)
        appliance_ids = [item.appliance_id for item in self.appliances]
        if len(appliance_ids) != len(set(appliance_ids)):
            raise ValueError("biblioteca contém appliance_id duplicado")
        code_ids = [code.code_id for appliance in self.appliances for code in appliance.codes]
        if len(code_ids) != len(set(code_ids)):
            raise ValueError("biblioteca contém code_id duplicado")
        deletion_keys = [(item.device_id, item.code_id) for item in self.pending_deletions]
        if len(deletion_keys) != len(set(deletion_keys)):
            raise ValueError("biblioteca contém exclusão pendente duplicada")
        for code in self.iter_codes():
            if code.sync_status == "synced" and not code.is_synced(self.device_id):
                code.mark_pending()

    @classmethod
    def from_dict(cls, data: Any) -> "IrLibrary":
        if not isinstance(data, dict):
            raise ValueError("biblioteca IR deve ser um objeto JSON")
        _assert_secret_free(data)
        if data.get("schema") != IR_LIBRARY_SCHEMA:
            raise ValueError(f"schema de biblioteca IR não suportado: {data.get('schema')}")
        appliances = data.get("appliances", [])
        deletions = data.get("pending_deletions", [])
        if not isinstance(appliances, list) or not isinstance(deletions, list):
            raise ValueError("listas da biblioteca IR são inválidas")
        return cls(
            profile_uuid=str(data.get("profile_uuid") or ""),
            device_id=str(data.get("device_id") or ""),
            appliances=[IrAppliance.from_dict(item) for item in appliances],
            pending_deletions=[IrCodeDeletion.from_dict(item) for item in deletions],
            created_at=str(data.get("created_at") or utc_now()),
            updated_at=str(data.get("updated_at") or utc_now()),
        )

    def to_dict(self) -> dict[str, Any]:
        document = {
            "appliances": [
                appliance.to_dict()
                for appliance in sorted(self.appliances, key=lambda item: item.appliance_id)
            ],
            "created_at": self.created_at,
            "device_id": self.device_id,
            "pending_deletions": [
                item.to_dict()
                for item in sorted(
                    self.pending_deletions,
                    key=lambda deletion: (deletion.device_id, deletion.code_id),
                )
            ],
            "profile_uuid": self.profile_uuid,
            "schema": IR_LIBRARY_SCHEMA,
            "updated_at": self.updated_at,
        }
        _assert_secret_free(document)
        return document

    def get_appliance(self, appliance_id: str) -> IrAppliance:
        appliance_id = _safe_identifier(appliance_id, "appliance_id")
        for appliance in self.appliances:
            if appliance.appliance_id == appliance_id:
                return appliance
        raise KeyError(f"aparelho IR não encontrado: {appliance_id}")

    def find_code(self, code_id: str) -> tuple[IrAppliance, IrRawCode]:
        code_id = _safe_identifier(code_id, "code_id")
        for appliance in self.appliances:
            for code in appliance.codes:
                if code.code_id == code_id:
                    return appliance, code
        raise KeyError(f"código IR não encontrado: {code_id}")

    def iter_codes(self) -> Iterable[IrRawCode]:
        for appliance in self.appliances:
            yield from appliance.codes

    def pending_codes(self) -> tuple[IrRawCode, ...]:
        return tuple(
            sorted(
                (code for code in self.iter_codes() if not code.is_synced(self.device_id)),
                key=lambda item: item.code_id,
            )
        )

    @property
    def sync_status(self) -> SyncStatus:
        return "pending" if self.pending_codes() or self.pending_deletions else "synced"


@dataclass(frozen=True)
class IrCodeSaveResult:
    code: IrRawCode
    created: bool
    deduplicated: bool


def _json_bytes(data: dict[str, Any]) -> bytes:
    return (json.dumps(data, ensure_ascii=False, indent=2, sort_keys=True) + "\n").encode("utf-8")


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


class IrLibraryStore:
    """Atomic CRUD facade intended for direct use by the Textual TUI."""

    def __init__(self, root: Path | str | None = None, *, backup_limit: int = 5) -> None:
        self.root = Path(root) if root is not None else default_ir_library_root()
        self.backups_dir = self.root / "backups"
        self.backup_limit = max(1, int(backup_limit))
        self._lock = threading.RLock()

    def library_path(self, profile_uuid: str) -> Path:
        profile_uuid = _safe_identifier(profile_uuid, "profile_uuid", file_safe=True)
        return self.root / f"{profile_uuid}.json"

    def list(self) -> tuple[IrLibrary, ...]:
        libraries: list[IrLibrary] = []
        if not self.root.exists():
            return ()
        for path in sorted(self.root.glob("*.json")):
            try:
                libraries.append(self._read_path(path))
            except (OSError, ValueError, json.JSONDecodeError):
                continue
        return tuple(sorted(libraries, key=lambda item: item.profile_uuid))

    def load(self, profile_uuid: str) -> IrLibrary:
        with self._lock:
            path = self.library_path(profile_uuid)
            if not path.exists():
                raise FileNotFoundError(f"biblioteca IR não encontrada: {profile_uuid}")
            library = self._read_path(path)
            if library.profile_uuid != profile_uuid:
                raise ValueError("profile_uuid interno diverge do nome da biblioteca")
            return library

    def get_or_create(self, profile_uuid: str, device_id: str = "") -> IrLibrary:
        profile_uuid = _safe_identifier(profile_uuid, "profile_uuid", file_safe=True)
        device_id = _optional_device_id(device_id)
        with self._lock:
            path = self.library_path(profile_uuid)
            if not path.exists():
                library = IrLibrary(profile_uuid=profile_uuid, device_id=device_id)
                self.save(library)
                return library
            library = self.load(profile_uuid)
            if device_id and library.device_id and device_id != library.device_id:
                raise ValueError("device_id da biblioteca IR é imutável após o pareamento")
            if device_id and not library.device_id:
                library.device_id = device_id
                for code in library.iter_codes():
                    code.mark_pending()
                self.save(library)
            return library

    def get_appliance(self, profile_uuid: str, appliance_id: str) -> IrAppliance:
        """Read one appliance without exposing persistence details to the TUI."""

        return self.load(profile_uuid).get_appliance(appliance_id)

    def list_appliances(self, profile_uuid: str) -> tuple[IrAppliance, ...]:
        return tuple(
            sorted(
                self.load(profile_uuid).appliances,
                key=lambda item: (item.name.casefold(), item.appliance_id),
            )
        )

    def get_code(self, profile_uuid: str, code_id: str) -> IrRawCode:
        return self.load(profile_uuid).find_code(code_id)[1]

    def list_codes(
        self, profile_uuid: str, *, appliance_id: str | None = None
    ) -> tuple[IrRawCode, ...]:
        library = self.load(profile_uuid)
        codes = (
            library.get_appliance(appliance_id).codes
            if appliance_id is not None
            else list(library.iter_codes())
        )
        return tuple(sorted(codes, key=lambda item: (item.name.casefold(), item.code_id)))

    def save(self, library: IrLibrary) -> Path:
        with self._lock:
            # Dataclasses are intentionally mutable so Textual forms can work
            # with them directly.  Re-run all invariants before any backup or
            # disk mutation in case a caller changed a field in memory.
            for appliance in library.appliances:
                for code in appliance.codes:
                    code.__post_init__()
                appliance.__post_init__()
            for deletion in library.pending_deletions:
                deletion.__post_init__()
            library.__post_init__()
            path = self.library_path(library.profile_uuid)
            if path.exists():
                current = self._read_path(path)
                if current.device_id and library.device_id != current.device_id:
                    raise ValueError("device_id da biblioteca IR é imutável após o pareamento")
                self._backup(path, library.profile_uuid)
            library.updated_at = utc_now()
            _atomic_write(path, _json_bytes(library.to_dict()))
            return path

    def delete_library(self, profile_uuid: str) -> None:
        with self._lock:
            path = self.library_path(profile_uuid)
            if path.exists():
                self._backup(path, profile_uuid)
                path.unlink()

    def create_appliance(
        self,
        profile_uuid: str,
        name: str,
        *,
        device_id: str = "",
        kind: str = "custom",
        appliance_id: str | None = None,
    ) -> IrAppliance:
        with self._lock:
            library = self.get_or_create(profile_uuid, device_id)
            name = _friendly_name(name, "nome do aparelho")
            kind = _kind(kind)
            identifier = (
                _safe_identifier(appliance_id, "appliance_id")
                if appliance_id
                else make_appliance_id(profile_uuid, name, kind)
            )
            for existing in library.appliances:
                if existing.appliance_id == identifier:
                    if existing.name == name and existing.kind == kind:
                        return existing
                    raise ValueError("appliance_id já pertence a outro aparelho")
            appliance = IrAppliance(appliance_id=identifier, name=name, kind=kind)
            library.appliances.append(appliance)
            self.save(library)
            return appliance

    def update_appliance(
        self,
        profile_uuid: str,
        appliance_id: str,
        *,
        name: str | None = None,
        kind: str | None = None,
    ) -> IrAppliance:
        with self._lock:
            library = self.load(profile_uuid)
            appliance = library.get_appliance(appliance_id)
            changed = False
            if name is not None:
                new_name = _friendly_name(name, "nome do aparelho")
                changed = changed or new_name != appliance.name
                appliance.name = new_name
            if kind is not None:
                new_kind = _kind(kind)
                changed = changed or new_kind != appliance.kind
                appliance.kind = new_kind
            if changed:
                appliance.updated_at = utc_now()
                self.save(library)
            return appliance

    def delete_appliance(self, profile_uuid: str, appliance_id: str) -> bool:
        with self._lock:
            library = self.load(profile_uuid)
            try:
                appliance = library.get_appliance(appliance_id)
            except KeyError:
                return False
            for code in appliance.codes:
                self._queue_deletion(library, code)
            library.appliances = [
                item for item in library.appliances if item.appliance_id != appliance.appliance_id
            ]
            self.save(library)
            return True

    def save_capture(
        self,
        profile_uuid: str,
        appliance_id: str,
        name: str,
        sequence_us: Iterable[int],
        *,
        device_id: str = "",
        carrier_hz: int = DEFAULT_CARRIER_HZ,
        duty_percent: int = DEFAULT_DUTY_PERCENT,
        code_id: str | None = None,
    ) -> IrCodeSaveResult:
        with self._lock:
            library = self.get_or_create(profile_uuid, device_id)
            appliance = library.get_appliance(appliance_id)
            name = _friendly_name(name, "nome do botão")
            sequence = _validated_sequence(sequence_us)
            carrier = _validated_carrier(carrier_hz)
            duty = _validated_duty(duty_percent)
            signal_hash = compute_signal_hash(
                sequence, carrier_hz=carrier, duty_percent=duty
            )
            for existing in appliance.codes:
                if existing.signal_hash == signal_hash:
                    return IrCodeSaveResult(existing, created=False, deduplicated=True)
            identifier = (
                _safe_identifier(code_id, "code_id")
                if code_id
                else make_code_id(profile_uuid, appliance_id, name)
            )
            try:
                existing_appliance, existing = library.find_code(identifier)
            except KeyError:
                existing_appliance = None
                existing = None
            if existing is not None:
                if existing_appliance is not appliance:
                    raise ValueError("code_id já pertence a outro aparelho")
                existing.name = name
                existing.sequence_us = sequence
                existing.carrier_hz = carrier
                existing.duty_percent = duty
                existing.signal_hash = signal_hash
                existing.updated_at = utc_now()
                existing.mark_pending()
                saved_code = existing
                created = False
            else:
                saved_code = IrRawCode(
                    code_id=identifier,
                    name=name,
                    sequence_us=sequence,
                    carrier_hz=carrier,
                    duty_percent=duty,
                    signal_hash=signal_hash,
                )
                appliance.codes.append(saved_code)
                created = True
            appliance.updated_at = utc_now()
            library.pending_deletions = [
                item for item in library.pending_deletions if item.code_id != saved_code.code_id
            ]
            self.save(library)
            return IrCodeSaveResult(saved_code, created=created, deduplicated=False)

    def update_code(
        self,
        profile_uuid: str,
        code_id: str,
        *,
        name: str | None = None,
        sequence_us: Iterable[int] | None = None,
        carrier_hz: int | None = None,
        duty_percent: int | None = None,
    ) -> IrRawCode:
        with self._lock:
            library = self.load(profile_uuid)
            appliance, code = library.find_code(code_id)
            new_name = _friendly_name(name, "nome do botão") if name is not None else code.name
            new_sequence = (
                _validated_sequence(sequence_us) if sequence_us is not None else code.sequence_us
            )
            new_carrier = (
                _validated_carrier(carrier_hz) if carrier_hz is not None else code.carrier_hz
            )
            new_duty = (
                _validated_duty(duty_percent) if duty_percent is not None else code.duty_percent
            )
            new_hash = compute_signal_hash(
                new_sequence, carrier_hz=new_carrier, duty_percent=new_duty
            )
            if any(
                candidate.code_id != code.code_id and candidate.signal_hash == new_hash
                for candidate in appliance.codes
            ):
                raise ValueError("este sinal IR já está cadastrado no aparelho")
            changed = (
                new_name != code.name
                or new_sequence != code.sequence_us
                or new_carrier != code.carrier_hz
                or new_duty != code.duty_percent
            )
            if changed:
                code.name = new_name
                code.sequence_us = new_sequence
                code.carrier_hz = new_carrier
                code.duty_percent = new_duty
                code.signal_hash = new_hash
                code.updated_at = utc_now()
                code.mark_pending()
                appliance.updated_at = utc_now()
                self.save(library)
            return code

    def delete_code(self, profile_uuid: str, code_id: str) -> bool:
        with self._lock:
            library = self.load(profile_uuid)
            try:
                appliance, code = library.find_code(code_id)
            except KeyError:
                return False
            self._queue_deletion(library, code)
            appliance.codes = [item for item in appliance.codes if item.code_id != code.code_id]
            appliance.updated_at = utc_now()
            self.save(library)
            return True

    def mark_code_synced(
        self,
        profile_uuid: str,
        code_id: str,
        *,
        expected_payload_hash: str | None = None,
    ) -> IrRawCode:
        with self._lock:
            library = self.load(profile_uuid)
            if not library.device_id:
                raise ValueError("device_id é obrigatório antes da sincronização")
            _appliance, code = library.find_code(code_id)
            current_hash = code.current_payload_hash(library.device_id)
            if expected_payload_hash is not None and expected_payload_hash != current_hash:
                raise ValueError("o código IR mudou durante a sincronização")
            code.sync_status = "synced"
            code.synced_payload_hash = current_hash
            code.synced_at = utc_now()
            code.updated_at = code.synced_at
            self.save(library)
            return code

    def mark_code_pending(self, profile_uuid: str, code_id: str) -> IrRawCode:
        with self._lock:
            library = self.load(profile_uuid)
            _appliance, code = library.find_code(code_id)
            code.mark_pending()
            code.updated_at = utc_now()
            self.save(library)
            return code

    def pending_codes(self, profile_uuid: str) -> tuple[IrRawCode, ...]:
        return self.load(profile_uuid).pending_codes()

    def pending_deletions(self, profile_uuid: str) -> tuple[IrCodeDeletion, ...]:
        return tuple(self.load(profile_uuid).pending_deletions)

    def mark_deletion_synced(self, profile_uuid: str, code_id: str) -> bool:
        with self._lock:
            library = self.load(profile_uuid)
            code_id = _safe_identifier(code_id, "code_id")
            original = len(library.pending_deletions)
            library.pending_deletions = [
                item for item in library.pending_deletions if item.code_id != code_id
            ]
            if len(library.pending_deletions) == original:
                return False
            self.save(library)
            return True

    def _queue_deletion(self, library: IrLibrary, code: IrRawCode) -> None:
        if not library.device_id or not code.synced_payload_hash:
            return
        key = (library.device_id, code.code_id)
        if any((item.device_id, item.code_id) == key for item in library.pending_deletions):
            return
        library.pending_deletions.append(
            IrCodeDeletion(code_id=code.code_id, device_id=library.device_id)
        )

    def _read_path(self, path: Path) -> IrLibrary:
        if path.stat().st_size > 16 * 1024 * 1024:
            raise ValueError("biblioteca IR excede 16 MiB")
        return IrLibrary.from_dict(json.loads(path.read_text(encoding="utf-8")))

    def _backup(self, path: Path, profile_uuid: str) -> None:
        directory = self.backups_dir / _safe_identifier(
            profile_uuid, "profile_uuid", file_safe=True
        )
        directory.mkdir(parents=True, exist_ok=True)
        payload = path.read_bytes()
        digest = hashlib.sha256(payload).hexdigest()[:10]
        stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
        backup = directory / f"{stamp}-{digest}.json"
        _atomic_write(backup, payload)
        backups = sorted(directory.glob("*.json"), reverse=True)
        for stale in backups[self.backup_limit :]:
            stale.unlink()
