"""Physical, capability-driven validation for a connected AetherSense board.

The service is deliberately independent from Textual and ``build.py``.  It is
therefore shared by the CLI, the TUI and tests while keeping the important
distinction between firmware evidence and something a person actually saw on
the board or controlled appliance.

Only a live USB preflight can unlock checks.  Unit tests may exercise the state
machine through injected probes/clients, but their results are never presented
as physical evidence outside the temporary results directory supplied by the
test.
"""

from __future__ import annotations

from collections.abc import Callable, Iterable, Mapping
from copy import deepcopy
from dataclasses import dataclass, field
from datetime import datetime, timezone
from enum import Enum
import hashlib
import json
import os
from pathlib import Path
import re
import tempfile
import time
from typing import Any, Protocol
from uuid import uuid4

from .automatic import usable_port_infos
from .devices import (
    BoardIdentity,
    DevicePairingError,
    probe_board,
    resolve_profile_port,
)
from .diffing import compute_build_hash, compute_config_hash
from .models import DeviceProfile, device_id_from_mac, normalise_mac
from .provisioning import ProvisionClient, ProvisioningError, ProvisioningTimeout
from .redaction import MASK, is_secret_key, redact_text


HARDWARE_VALIDATION_SCHEMA = "aethersense.hardware_validation.v1"


class ValidationState(str, Enum):
    WAITING = "waiting"
    RUNNING = "running"
    MANUAL_CONFIRMATION = "manual_confirmation"
    PASSED = "passed"
    FAILED = "failed"
    BLOCKED = "blocked"
    SKIPPED = "skipped"


TERMINAL_STATES = {
    ValidationState.PASSED,
    ValidationState.FAILED,
    ValidationState.BLOCKED,
    ValidationState.SKIPPED,
}


class HardwareValidationError(RuntimeError):
    """Base error raised for invalid validation operations."""


class HardwareValidationBlocked(HardwareValidationError):
    """The physical test cannot proceed until an external condition changes."""


class HardwareValidationFailed(HardwareValidationError):
    """The board returned evidence that contradicts the selected profile."""


class ValidationClient(Protocol):
    """Small injectable boundary around the USB provisioning client."""

    def inspect_device(self, profile: DeviceProfile) -> Any: ...

    def commission_accessory(
        self,
        profile: DeviceProfile,
        action: str,
        params: dict[str, Any] | None = None,
        **kwargs: Any,
    ) -> Any: ...


@dataclass(frozen=True)
class ManualDecision:
    confirmed: bool
    recorded_at: str
    note: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {
            "confirmed": self.confirmed,
            "recorded_at": self.recorded_at,
            "note": self.note,
        }


@dataclass
class ValidationCheck:
    check_id: str
    label: str
    enabled: bool
    required_capabilities: tuple[str, ...] = ()
    manual_confirmation_required: bool = False
    authorization_required: bool = False
    manual_prompt: str = ""
    authorization_prompt: str = ""
    state: ValidationState = ValidationState.WAITING
    attempts: int = 0
    reason: str = ""
    automatic_evidence: dict[str, Any] | None = None
    manual_authorization: ManualDecision | None = None
    manual_confirmation: ManualDecision | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.check_id,
            "label": self.label,
            "enabled": self.enabled,
            "required_capabilities": list(self.required_capabilities),
            "manual_confirmation_required": self.manual_confirmation_required,
            "authorization_required": self.authorization_required,
            "state": self.state.value,
            "attempts": self.attempts,
            "reason": self.reason or None,
            "automatic_evidence": self.automatic_evidence,
            "manual_authorization": (
                self.manual_authorization.to_dict()
                if self.manual_authorization is not None
                else None
            ),
            "manual_confirmation": (
                self.manual_confirmation.to_dict()
                if self.manual_confirmation is not None
                else None
            ),
        }


@dataclass
class PreflightResult:
    state: ValidationState = ValidationState.WAITING
    reason: str = ""
    automatic_evidence: dict[str, Any] | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "state": self.state.value,
            "reason": self.reason or None,
            "automatic_evidence": self.automatic_evidence,
        }


@dataclass(frozen=True)
class AutomaticTestResult:
    passed: bool
    evidence: Mapping[str, Any] = field(default_factory=dict)
    message: str = ""
    source: str = "hardware_diagnostic"
    artifact: Any = None


@dataclass(frozen=True)
class ResolvedHardware:
    port: str
    port_info: Mapping[str, Any]
    identity: BoardIdentity


@dataclass(frozen=True)
class ValidationContext:
    profile: DeviceProfile
    port: str
    port_info: Mapping[str, Any]
    identity: BoardIdentity
    serial_status: Any
    capabilities: tuple[str, ...]
    client: ValidationClient


@dataclass
class HardwareValidationSession:
    session_id: str
    started_at: str
    updated_at: str
    profile_id: str
    device_id: str
    target: str
    expected_build_hash: str
    expected_config_hash: str
    evidence_mode: str
    checks: dict[str, ValidationCheck]
    preflight: PreflightResult = field(default_factory=PreflightResult)
    completed_at: str | None = None
    hardware: dict[str, Any] = field(default_factory=dict)

    @property
    def overall_state(self) -> ValidationState:
        states = [self.preflight.state, *(check.state for check in self.checks.values())]
        if any(state == ValidationState.RUNNING for state in states):
            return ValidationState.RUNNING
        if any(state == ValidationState.MANUAL_CONFIRMATION for state in states):
            return ValidationState.MANUAL_CONFIRMATION
        if any(state == ValidationState.FAILED for state in states):
            return ValidationState.FAILED
        if any(state == ValidationState.BLOCKED for state in states):
            return ValidationState.BLOCKED
        if all(state in {ValidationState.PASSED, ValidationState.SKIPPED} for state in states):
            return ValidationState.PASSED
        return ValidationState.WAITING

    @property
    def physically_validated(self) -> bool:
        return (
            self.evidence_mode == "physical"
            and self.overall_state == ValidationState.PASSED
            and all(
                check.state in {ValidationState.PASSED, ValidationState.SKIPPED}
                for check in self.checks.values()
            )
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "schema": HARDWARE_VALIDATION_SCHEMA,
            "session_id": self.session_id,
            "started_at": self.started_at,
            "updated_at": self.updated_at,
            "completed_at": self.completed_at,
            "status": self.overall_state.value,
            "physically_validated": self.physically_validated,
            "evidence_mode": self.evidence_mode,
            "device_profile": {
                "profile_id": self.profile_id,
                "device_id": self.device_id,
                "target": self.target,
                "expected_build_hash": self.expected_build_hash,
                "expected_config_hash": self.expected_config_hash,
            },
            "hardware": self.hardware,
            "preflight": self.preflight.to_dict(),
            "tests": [self.checks[key].to_dict() for key in self.checks],
        }


HardwareTester = Callable[
    [ValidationContext, ValidationCheck, Mapping[str, Any]], AutomaticTestResult | Mapping[str, Any] | bool
]


def _utc_now() -> datetime:
    return datetime.now(timezone.utc)


def _as_utc(value: datetime) -> datetime:
    return value.replace(tzinfo=timezone.utc) if value.tzinfo is None else value.astimezone(timezone.utc)


def _iso(value: datetime) -> str:
    return _as_utc(value).isoformat(timespec="seconds").replace("+00:00", "Z")


def _status_value(status: Any, name: str, default: Any = None) -> Any:
    if isinstance(status, Mapping):
        return status.get(name, default)
    return getattr(status, name, default)


def _default_port_provider() -> list[dict[str, Any]]:
    try:
        import serial.tools.list_ports
    except ImportError:
        return []
    infos: list[dict[str, Any]] = []
    for port in serial.tools.list_ports.comports():
        infos.append(
            {
                "device": port.device or "",
                "description": port.description or "",
                "manufacturer": port.manufacturer or "",
                "hwid": port.hwid or "",
                "vid": "" if port.vid is None else f"{port.vid:04X}",
                "pid": "" if port.pid is None else f"{port.pid:04X}",
                "serial_number": port.serial_number or "",
                "location": port.location or "",
            }
        )
    return infos


def _normalise_test_result(value: AutomaticTestResult | Mapping[str, Any] | bool) -> AutomaticTestResult:
    if isinstance(value, AutomaticTestResult):
        return value
    if isinstance(value, bool):
        return AutomaticTestResult(value)
    if isinstance(value, Mapping):
        evidence = value.get("evidence", {})
        return AutomaticTestResult(
            passed=bool(value.get("passed", False)),
            evidence=evidence if isinstance(evidence, Mapping) else {"value": evidence},
            message=str(value.get("message") or ""),
            source=str(value.get("source") or "hardware_diagnostic"),
            artifact=value.get("artifact"),
        )
    raise TypeError("hardware tester must return AutomaticTestResult, mapping or bool")


def wait_for_runtime_snapshot(
    context: ValidationContext,
    *,
    timeout: float = 15.0,
    require_ready: bool = False,
    sleeper: Callable[[float], None] = time.sleep,
    monotonic: Callable[[], float] = time.monotonic,
) -> tuple[Any, Mapping[str, Any]]:
    """Wait for Wi-Fi/CSI counters to be attached after boot without reading CSI."""

    deadline = monotonic() + max(0.5, float(timeout))
    last_error = "network/CSI runtime diagnostics are not ready"
    while monotonic() < deadline:
        try:
            result = context.client.commission_accessory(
                context.profile,
                "diagnostic.runtime",
                {},
            )
            state = _status_value(result, "state", {}) or {}
            if isinstance(state, Mapping):
                if not require_ready or (
                    bool(state.get("network_ready"))
                    and bool(state.get("csi_streaming"))
                ):
                    return result, state
                last_error = "Wi-Fi/IP e o fluxo CSI ainda estão inicializando"
                sleeper(min(0.5, max(0.0, deadline - monotonic())))
                continue
            last_error = "o firmware devolveu um diagnóstico de runtime inválido"
        except Exception as exc:
            last_error = str(exc)
            retryable = any(
                marker in last_error.casefold()
                for marker in ("not ready", "unavailable", "ainda não", "ainda nao")
            )
            if not retryable:
                raise
        sleeper(min(0.5, max(0.0, deadline - monotonic())))
    raise HardwareValidationBlocked(
        f"o runtime de rede/CSI não ficou pronto no prazo: {last_error}"
    )


class HardwareValidationService:
    """Orchestrate and persist validation against one physical USB board."""

    def __init__(
        self,
        *,
        results_root: Path | str | None = None,
        port_provider: Callable[[], Iterable[Mapping[str, Any]]] | None = None,
        board_probe: Callable[[str], BoardIdentity] | None = None,
        client_factory: Callable[[str], ValidationClient] | None = None,
        testers: Mapping[str, HardwareTester] | None = None,
        now: Callable[[], datetime] = _utc_now,
        secret_values: Iterable[str] = (),
    ) -> None:
        repository_root = Path(__file__).resolve().parents[2]
        self.results_root = Path(results_root) if results_root is not None else repository_root / "resultados"
        # Replacing a core USB boundary is invaluable for deterministic tests,
        # but such a session must never claim that a real device was validated.
        # Production uses the default boundaries; custom *testers* still run on
        # that proven physical connection and do not change this mode.
        self.evidence_mode = (
            "physical"
            if port_provider is None and board_probe is None and client_factory is None
            else "simulated"
        )
        self.port_provider = port_provider or _default_port_provider
        self.board_probe = board_probe or probe_board
        # Physical accessory workers may still be mounting FAT or negotiating
        # Wi-Fi while the serial server is already answering hello.  Keep a
        # bounded but realistic total budget for one commissioning command.
        self.client_factory = client_factory or (
            lambda port: ProvisionClient(port, operation_timeout=60.0)
        )
        self.testers = dict(testers or {})
        self.now = now
        self.secret_values = tuple(str(value) for value in secret_values if str(value))
        self.profile: DeviceProfile | None = None
        self.session: HardwareValidationSession | None = None
        self.context: ValidationContext | None = None
        self.report_path: Path | None = None
        self._artifacts: dict[str, Any] = {}

    def start(self, profile: DeviceProfile) -> HardwareValidationSession:
        """Create a session and run the mandatory live USB preflight."""

        self.profile = deepcopy(profile)
        self._artifacts.clear()
        instant = _as_utc(self.now())
        session_id, report_path = self._allocate_report_path(instant, profile.profile_id)
        self.report_path = report_path
        self.session = HardwareValidationSession(
            session_id=session_id,
            started_at=_iso(instant),
            updated_at=_iso(instant),
            profile_id=profile.profile_id,
            device_id=profile.device_id,
            target=profile.target.profile,
            expected_build_hash=compute_build_hash(profile),
            expected_config_hash=compute_config_hash(profile),
            evidence_mode=self.evidence_mode,
            checks=self._build_checks(profile),
        )
        self._run_preflight(reset_checks=True)
        return self.session

    def rerun_preflight(self) -> HardwareValidationSession:
        self._require_started()
        self._run_preflight(reset_checks=True)
        return self.session  # type: ignore[return-value]

    def resolve_connected_board(self, profile: DeviceProfile) -> ResolvedHardware:
        """Resolve a stable profile association and prove it with the eFuse MAC."""

        expected_mac = normalise_mac(profile.association.efuse_mac)
        expected_device_id = str(profile.device_id or "").strip().lower()
        if not expected_mac or not expected_device_id:
            raise HardwareValidationBlocked(
                "o perfil ainda não está pareado por eFuse MAC; pareie a placa antes de validar"
            )
        if device_id_from_mac(expected_mac) != expected_device_id:
            raise HardwareValidationFailed(
                "o device_id do perfil não corresponde ao eFuse MAC salvo"
            )
        try:
            raw_infos = list(self.port_provider())
        except Exception as exc:
            raise HardwareValidationBlocked(f"não foi possível enumerar as portas seriais: {exc}") from exc
        infos = usable_port_infos(raw_infos)
        if not infos:
            raise HardwareValidationBlocked(
                "nenhum ESP32 USB foi encontrado; confira cabo de dados, driver, energia e modo boot/rescue"
            )

        try:
            associated = resolve_profile_port(profile, infos)
        except DevicePairingError as exc:
            raise HardwareValidationBlocked(str(exc)) from exc
        ordered = sorted(
            infos,
            key=lambda item: (
                0 if str(item.get("device") or "").casefold() == str(associated or "").casefold() else 1,
                str(item.get("device") or "").casefold(),
            ),
        )
        matches: list[ResolvedHardware] = []
        mismatches: list[str] = []
        probe_errors: list[str] = []
        for info in ordered:
            port = str(info.get("device") or "")
            try:
                identity = self.board_probe(port)
            except Exception as exc:
                probe_errors.append(f"{port}: {exc}")
                continue
            if identity.device_id != expected_device_id or normalise_mac(identity.mac) != expected_mac:
                mismatches.append(port)
                continue
            if identity.target_profile != profile.target.profile:
                raise HardwareValidationFailed(
                    f"o chip físico em {port} diverge do alvo {profile.target.profile}"
                )
            matches.append(ResolvedHardware(port=port, port_info=dict(info), identity=identity))

        if len(matches) == 1:
            return matches[0]
        if len(matches) > 1:
            raise HardwareValidationBlocked(
                "a mesma identidade física apareceu em mais de uma porta; desconecte adaptadores duplicados"
            )
        if mismatches:
            raise HardwareValidationFailed(
                "a placa conectada possui eFuse MAC diferente do perfil selecionado"
            )
        detail = f" ({'; '.join(probe_errors[:2])})" if probe_errors else ""
        raise HardwareValidationBlocked(
            "a placa pareada não respondeu; confira cabo, driver e modo boot/rescue" + detail
        )

    def authorize(self, check_id: str, confirmed: bool, note: str = "") -> ValidationCheck:
        """Record a safety/privacy decision required *before* a physical action."""

        check = self._get_check(check_id)
        if not check.authorization_required:
            raise HardwareValidationError(f"{check_id} não exige autorização prévia")
        if check.automatic_evidence is not None:
            raise HardwareValidationError("a autorização deve ser registrada antes do teste automático")
        decision = ManualDecision(bool(confirmed), _iso(self.now()), str(note))
        check.manual_authorization = decision
        if confirmed:
            check.state = ValidationState.WAITING
            check.reason = ""
        else:
            check.state = ValidationState.BLOCKED
            check.reason = "autorização física recusada"
        self._persist()
        return check

    def run_check(self, check_id: str, params: Mapping[str, Any] | None = None) -> ValidationCheck:
        """Run automatic diagnostics; never infer a manual observation from ACKs."""

        check = self._get_check(check_id)
        if not check.enabled:
            return check
        if check.authorization_required and not (
            check.manual_authorization and check.manual_authorization.confirmed
        ):
            raise HardwareValidationError("registre a autorização manual antes deste teste")
        if check.state != ValidationState.WAITING:
            raise HardwareValidationError(
                f"o teste {check_id} não pode iniciar no estado {check.state.value}; use retry_check"
            )
        if not self._refresh_live_preflight(check):
            return check
        missing = self._missing_capabilities(check)
        if missing:
            check.state = ValidationState.BLOCKED
            check.reason = "firmware não publicou capabilities obrigatórias: " + ", ".join(missing)
            self._persist()
            return check

        check.state = ValidationState.RUNNING
        check.reason = ""
        check.attempts += 1
        check.automatic_evidence = None
        check.manual_confirmation = None
        self._persist()
        try:
            tester = self._tester_for(check)
            if tester is None:
                raise HardwareValidationBlocked(
                    "o firmware/host ainda não oferece prova automática suficiente para este teste"
                )
            assert self.context is not None
            result = _normalise_test_result(tester(self.context, check, dict(params or {})))
            check.automatic_evidence = {
                "passed": result.passed,
                "source": result.source,
                "observed_at": _iso(self.now()),
                "details": dict(result.evidence),
            }
            if result.artifact is not None:
                self._artifacts[check_id] = result.artifact
            if not result.passed:
                check.state = ValidationState.FAILED
                check.reason = result.message or "o diagnóstico automático falhou"
            elif check.manual_confirmation_required:
                check.state = ValidationState.MANUAL_CONFIRMATION
                check.reason = check.manual_prompt or "confirme o efeito físico observado"
            else:
                check.state = ValidationState.PASSED
                check.reason = result.message
        except HardwareValidationBlocked as exc:
            check.state = ValidationState.BLOCKED
            check.reason = str(exc)
        except Exception as exc:
            check.state = ValidationState.FAILED
            check.reason = str(exc)
        self._persist()
        return check

    def confirm_manual(self, check_id: str, confirmed: bool, note: str = "") -> ValidationCheck:
        """Complete a check only after both automatic and human evidence exist."""

        check = self._get_check(check_id)
        evidence = check.automatic_evidence or {}
        if check.state != ValidationState.MANUAL_CONFIRMATION or not evidence.get("passed"):
            raise HardwareValidationError(
                "a confirmação manual só é aceita após uma prova automática bem-sucedida"
            )
        check.manual_confirmation = ManualDecision(bool(confirmed), _iso(self.now()), str(note))
        if confirmed:
            check.state = ValidationState.PASSED
            check.reason = ""
        else:
            check.state = ValidationState.FAILED
            check.reason = "o efeito físico não foi confirmado"
        self._persist()
        return check

    def retry_check(self, check_id: str) -> ValidationCheck:
        """Reset only one failed/blocked check, preserving all other evidence."""

        check = self._get_check(check_id)
        if check_id == "boot_serial":
            self.rerun_preflight()
            return self._get_check(check_id)
        if not check.enabled:
            return check
        check.automatic_evidence = None
        check.manual_confirmation = None
        self._artifacts.pop(check_id, None)
        if check.authorization_required:
            check.manual_authorization = None
            check.state = ValidationState.MANUAL_CONFIRMATION
            check.reason = check.authorization_prompt
        else:
            missing = self._missing_capabilities(check)
            check.state = ValidationState.BLOCKED if missing else ValidationState.WAITING
            check.reason = (
                "firmware não publicou capabilities obrigatórias: " + ", ".join(missing)
                if missing
                else ""
            )
        self._persist()
        return check

    def artifact(self, check_id: str) -> Any:
        """Return an in-memory test artifact (for example an IR raw code)."""

        return self._artifacts.get(check_id)

    def _run_preflight(self, *, reset_checks: bool) -> None:
        assert self.session is not None
        self.session.preflight = PreflightResult(state=ValidationState.RUNNING)
        if reset_checks:
            self._reset_check_states()
        self._persist()
        try:
            self.context = self._perform_live_preflight()
        except HardwareValidationFailed as exc:
            self.context = None
            self._apply_preflight_failure(ValidationState.FAILED, str(exc))
        except Exception as exc:
            self.context = None
            self._apply_preflight_failure(ValidationState.BLOCKED, str(exc))
        else:
            self._apply_preflight_success()
        self._persist()

    def _perform_live_preflight(self) -> ValidationContext:
        assert self.profile is not None and self.session is not None
        resolved = self.resolve_connected_board(self.profile)
        client = self.client_factory(resolved.port)
        try:
            status = client.inspect_device(self.profile)
        except ProvisioningTimeout as exc:
            raise HardwareValidationBlocked(f"o firmware não respondeu ao handshake USB: {exc}") from exc
        except ProvisioningError as exc:
            message = str(exc)
            if "outro dispositivo" in message or "identidade" in message or "device_id" in message:
                raise HardwareValidationFailed(message) from exc
            raise HardwareValidationBlocked(message) from exc
        except Exception as exc:
            raise HardwareValidationBlocked(f"falha no handshake USB: {exc}") from exc

        observed_device = str(_status_value(status, "device_id", "") or "").strip().lower()
        observed_build = str(_status_value(status, "build_hash", "") or "")
        observed_config_raw = _status_value(status, "config_hash")
        observed_config = str(observed_config_raw or "")
        capabilities = tuple(
            sorted(
                {
                    str(item).strip().lower()
                    for item in (_status_value(status, "features", ()) or ())
                    if str(item).strip()
                }
            )
        )
        if observed_device != self.profile.device_id:
            raise HardwareValidationFailed("o handshake USB informou um eFuse/device_id diferente")
        if observed_build != self.session.expected_build_hash:
            raise HardwareValidationFailed(
                "o build_hash da placa diverge do perfil; grave o firmware correto antes de validar"
            )
        if observed_config != self.session.expected_config_hash:
            raise HardwareValidationFailed(
                "o config_hash da placa diverge do perfil; aplique a configuração antes de validar"
            )
        if not capabilities:
            raise HardwareValidationFailed("o firmware não publicou capabilities")
        if self.evidence_mode == "physical":
            if "serial-hardware-validation-v1" not in capabilities:
                raise HardwareValidationFailed(
                    "o firmware conectado ainda não suporta validação física v1; grave o build atual"
                )
            if not str(_status_value(status, "boot_id", "") or ""):
                raise HardwareValidationFailed("o firmware não informou o boot_id atual")
            if not str(_status_value(status, "firmware_version", "") or ""):
                raise HardwareValidationFailed("o firmware não informou sua versão")
        return ValidationContext(
            profile=self.profile,
            port=resolved.port,
            port_info=resolved.port_info,
            identity=resolved.identity,
            serial_status=status,
            capabilities=capabilities,
            client=client,
        )

    def _refresh_live_preflight(self, active_check: ValidationCheck) -> bool:
        assert self.session is not None
        try:
            context = self._perform_live_preflight()
        except HardwareValidationFailed as exc:
            self.context = None
            self._apply_preflight_failure(ValidationState.FAILED, str(exc))
            active_check.state = ValidationState.BLOCKED
            active_check.reason = "preflight físico falhou: " + str(exc)
            self._persist()
            return False
        except Exception as exc:
            self.context = None
            self._apply_preflight_failure(ValidationState.BLOCKED, str(exc))
            active_check.state = ValidationState.BLOCKED
            active_check.reason = "preflight físico bloqueado: " + str(exc)
            self._persist()
            return False
        self.context = context
        self._record_preflight_evidence(context)
        return True

    def _apply_preflight_success(self) -> None:
        assert self.context is not None and self.session is not None
        self._record_preflight_evidence(self.context)
        for check in self.session.checks.values():
            if not check.enabled:
                check.state = ValidationState.SKIPPED
                check.reason = "recurso desabilitado no perfil"
                continue
            if check.check_id == "boot_serial":
                if self.evidence_mode == "physical":
                    missing = self._missing_capabilities(check)
                    check.state = ValidationState.BLOCKED if missing else ValidationState.WAITING
                    check.reason = (
                        "firmware não publicou capabilities obrigatórias: " + ", ".join(missing)
                        if missing
                        else ""
                    )
                else:
                    check.state = ValidationState.PASSED
                    check.reason = ""
                    check.automatic_evidence = dict(
                        self.session.preflight.automatic_evidence or {}
                    )
                continue
            missing = self._missing_capabilities(check)
            if missing:
                check.state = ValidationState.BLOCKED
                check.reason = "firmware não publicou capabilities obrigatórias: " + ", ".join(missing)
            elif check.authorization_required:
                check.state = ValidationState.MANUAL_CONFIRMATION
                check.reason = check.authorization_prompt
            else:
                check.state = ValidationState.WAITING
                check.reason = ""

    def _record_preflight_evidence(self, context: ValidationContext) -> None:
        assert self.session is not None
        status = context.serial_status
        evidence = {
            "passed": True,
            "source": "espflash_board_info+usb_serial",
            "observed_at": _iso(self.now()),
            "port": context.port,
            "efuse_mac": context.identity.mac,
            "device_id": context.identity.device_id,
            "chip": context.identity.chip,
            "target": context.identity.target_profile,
            "build_hash": str(_status_value(status, "build_hash", "") or ""),
            "config_hash": str(_status_value(status, "config_hash", "") or ""),
            "firmware_version": str(_status_value(status, "firmware_version", "") or ""),
            "boot_id": str(_status_value(status, "boot_id", "") or ""),
            "generation": _status_value(status, "generation"),
            "capabilities": list(context.capabilities),
        }
        self.session.preflight = PreflightResult(
            state=ValidationState.PASSED,
            automatic_evidence=evidence,
        )
        self.session.hardware = {
            "port": context.port,
            "efuse_mac": context.identity.mac,
            "device_id": context.identity.device_id,
            "chip": context.identity.chip,
            "firmware_version": str(_status_value(status, "firmware_version", "") or ""),
            "boot_id": str(_status_value(status, "boot_id", "") or ""),
            "capabilities": list(context.capabilities),
        }

    def _apply_preflight_failure(self, state: ValidationState, reason: str) -> None:
        assert self.session is not None
        self.session.preflight = PreflightResult(state=state, reason=reason)
        for check in self.session.checks.values():
            if not check.enabled:
                check.state = ValidationState.SKIPPED
                check.reason = "recurso desabilitado no perfil"
            elif check.check_id == "boot_serial":
                check.state = state
                check.reason = reason
            else:
                check.state = ValidationState.BLOCKED
                check.reason = "preflight físico obrigatório não passou"

    def _reset_check_states(self) -> None:
        assert self.session is not None
        for check in self.session.checks.values():
            check.automatic_evidence = None
            check.manual_authorization = None
            check.manual_confirmation = None
            check.reason = ""
            check.state = ValidationState.WAITING if check.enabled else ValidationState.SKIPPED
        self.session.completed_at = None

    def _missing_capabilities(self, check: ValidationCheck) -> tuple[str, ...]:
        available = set(self.context.capabilities if self.context is not None else ())
        return tuple(item for item in check.required_capabilities if item not in available)

    def _build_checks(self, profile: DeviceProfile) -> dict[str, ValidationCheck]:
        checks: dict[str, ValidationCheck] = {}

        def add(
            check_id: str,
            label: str,
            enabled: bool,
            capabilities: tuple[str, ...] = (),
            *,
            manual: bool = False,
            authorization: bool = False,
            manual_prompt: str = "",
            authorization_prompt: str = "",
        ) -> None:
            checks[check_id] = ValidationCheck(
                check_id=check_id,
                label=label,
                enabled=enabled,
                required_capabilities=capabilities,
                manual_confirmation_required=manual,
                authorization_required=authorization,
                manual_prompt=manual_prompt,
                authorization_prompt=authorization_prompt,
                state=ValidationState.WAITING if enabled else ValidationState.SKIPPED,
                reason="" if enabled else "recurso desabilitado no perfil",
            )

        add(
            "boot_serial",
            "Boot, identidade e serial",
            True,
            ("safe-reboot-diagnostic-v1",),
        )
        add(
            "rgb",
            "LED RGB de estado",
            profile.runtime.status_led.enabled,
            ("status_led", "serial-commissioning-v1"),
            manual=True,
            manual_prompt="confirme que as cores apareceram no LED da placa",
        )
        add(
            "ir_rx",
            "Receptor infravermelho",
            profile.ir_rx.enabled,
            ("ir", "serial-commissioning-v1"),
            manual=True,
            manual_prompt="confirme o pulso amarelo do LED ao receber o controle",
        )
        add(
            "ir_tx",
            "Transmissor infravermelho",
            profile.ir_tx.enabled,
            ("ir", "serial-commissioning-v1"),
            manual=True,
            manual_prompt="confirme o pulso magenta e que o aparelho respondeu",
        )
        add(
            "storage",
            "Cartão microSD",
            profile.storage.enabled,
            ("sd", "serial-commissioning-v1"),
        )
        for relay in profile.relays:
            if not relay.enabled:
                continue
            add(
                f"relay:{relay.relay_id}",
                f"Relé — {relay.name}",
                True,
                ("relays", "serial-commissioning-v1"),
                manual=True,
                authorization=True,
                authorization_prompt=(
                    "confirme que a carga está segura, isolada e não energiza rede elétrica sem módulo certificado"
                ),
                manual_prompt="confirme fisicamente que o relé comutou e voltou ao estado seguro",
            )
        integration_enabled = bool(
            profile.network.wifi.ssid or profile.mqtt.enabled or profile.server.base_url
        )
        integration_caps = ("mqtt",) if profile.mqtt.enabled else ()
        add(
            "network_integration",
            "Rede, MQTT e servidor",
            integration_enabled,
            tuple(dict.fromkeys((*integration_caps, "runtime-diagnostic-v1"))),
        )
        add(
            "csi",
            "CSI na placa",
            profile.csi.enabled,
            ("csi", "runtime-diagnostic-v1"),
            authorization=True,
            authorization_prompt="confirme que o teste ocorrerá em bancada ou sala vazia",
        )
        add(
            "concurrency",
            "CSI + SD + MQTT por 30 minutos",
            profile.csi.enabled and profile.storage.enabled and profile.mqtt.enabled,
            ("csi", "sd", "mqtt"),
        )
        return checks

    def _tester_for(self, check: ValidationCheck) -> HardwareTester | None:
        custom = self.testers.get(check.check_id)
        if custom is None and check.check_id.startswith("relay:"):
            custom = self.testers.get("relay")
        if custom is not None:
            return custom
        if check.check_id == "boot_serial":
            return self._test_boot_serial
        if check.check_id == "ir_rx":
            return self._test_ir_rx
        if check.check_id == "ir_tx":
            return self._test_ir_tx
        if check.check_id == "rgb":
            return self._test_rgb
        if check.check_id == "storage":
            return self._test_storage
        if check.check_id == "network_integration":
            return self._test_network_runtime
        if check.check_id == "csi":
            return self._test_csi_runtime
        if check.check_id.startswith("relay:"):
            return self._test_relay
        return None

    def _test_boot_serial(
        self,
        context: ValidationContext,
        _check: ValidationCheck,
        _params: Mapping[str, Any],
    ) -> AutomaticTestResult:
        before_boot_id = str(_status_value(context.serial_status, "boot_id", "") or "")
        result = context.client.commission_accessory(
            context.profile,
            "diagnostic.reboot",
            {},
        )
        state = _status_value(result, "state", {}) or {}
        if not isinstance(state, Mapping) or not state.get("scheduled"):
            return AutomaticTestResult(False, message="o firmware não confirmou o reboot seguro")
        # The ACK is emitted before restart. Give the USB bridge time to drop,
        # then repeatedly rediscover it by immutable identity instead of COM.
        time.sleep(1.0)
        deadline = time.monotonic() + 20.0
        last_error = "a placa não voltou após reiniciar"
        after_context: ValidationContext | None = None
        while time.monotonic() < deadline:
            try:
                after_context = self._perform_live_preflight()
                break
            except Exception as exc:
                last_error = str(exc)
                time.sleep(0.5)
        if after_context is None:
            raise HardwareValidationBlocked(last_error)
        after_boot_id = str(_status_value(after_context.serial_status, "boot_id", "") or "")
        if not before_boot_id or not after_boot_id or before_boot_id == after_boot_id:
            return AutomaticTestResult(
                False,
                evidence={"boot_id_changed": False},
                message="a placa respondeu, mas o boot_id não mudou após o reboot",
                source="usb_serial+safe_reboot+readback",
            )
        self.context = after_context
        self._record_preflight_evidence(after_context)
        self._reconcile_capability_blocks_after_reboot(exclude={"boot_serial"})
        return AutomaticTestResult(
            True,
            evidence={
                "request_id": str(_status_value(result, "request_id", "") or ""),
                "ack_status": str(_status_value(result, "status", "") or ""),
                "boot_id_changed": True,
                "port_after_reboot": after_context.port,
                "build_hash": str(_status_value(after_context.serial_status, "build_hash", "") or ""),
                "config_hash": str(_status_value(after_context.serial_status, "config_hash", "") or ""),
            },
            source="usb_serial+safe_reboot+readback",
        )

    def _reconcile_capability_blocks_after_reboot(self, *, exclude: set[str]) -> None:
        """Apply capabilities advertised by the newly booted active NVS profile.

        Provisioning commits the profile atomically, but managers and the
        serial capability list are constructed during boot.  Checks that were
        blocked by the pre-reboot snapshot must therefore be reconsidered
        without discarding evidence already collected by other cards.
        """

        assert self.session is not None
        for candidate in self.session.checks.values():
            if candidate.check_id in exclude or not candidate.enabled:
                continue
            if not (
                candidate.state == ValidationState.BLOCKED
                and candidate.reason.startswith("firmware não publicou capabilities obrigatórias:")
            ):
                continue
            missing = self._missing_capabilities(candidate)
            if missing:
                candidate.reason = (
                    "firmware não publicou capabilities obrigatórias: " + ", ".join(missing)
                )
            elif candidate.authorization_required:
                candidate.state = ValidationState.MANUAL_CONFIRMATION
                candidate.reason = candidate.authorization_prompt
            else:
                candidate.state = ValidationState.WAITING
                candidate.reason = ""

    def _test_rgb(
        self,
        context: ValidationContext,
        _check: ValidationCheck,
        params: Mapping[str, Any],
    ) -> AutomaticTestResult:
        duration_ms = int(params.get("duration_ms") or 700)
        if not 250 <= duration_ms <= 5_000:
            return AutomaticTestResult(False, message="duração do teste RGB fora do limite seguro")
        # Canonical firmware states deliberately cover the three primary
        # channels without exposing RGB tuples to common-mode users.
        sequence = (
            ("error", "vermelho"),
            ("streaming", "verde"),
            ("wifi_connecting", "azul"),
        )
        requests: list[dict[str, str]] = []
        for index, (color, label) in enumerate(sequence):
            result = context.client.commission_accessory(
                context.profile,
                "diagnostic.status_led",
                {"color": color, "duration_ms": duration_ms},
            )
            requests.append(
                {
                    "color": label,
                    "request_id": str(_status_value(result, "request_id", "") or ""),
                    "ack_status": str(_status_value(result, "status", "") or ""),
                }
            )
            if index + 1 < len(sequence):
                time.sleep(duration_ms / 1_000.0 + 0.1)
        return AutomaticTestResult(
            True,
            evidence={"colors": requests, "duration_ms": duration_ms},
            source="usb_serial+status_led_driver",
        )

    def _test_storage(
        self,
        context: ValidationContext,
        _check: ValidationCheck,
        _params: Mapping[str, Any],
    ) -> AutomaticTestResult:
        payload = f"aethersense-hardware-validation:{uuid4().hex}"
        expected = hashlib.sha256(payload.encode("utf-8")).hexdigest()
        result = self._commission_when_accessory_ready(
            context,
            context.profile,
            "storage.self_test",
            {
                "storage_id": "sd",
                "payload": payload,
                "expected_sha256": expected,
            },
        )
        state = _status_value(result, "state", {}) or {}
        observed = str(state.get("sha256") or "") if isinstance(state, Mapping) else ""
        removed = bool(state.get("temporary_file_removed")) if isinstance(state, Mapping) else False
        if observed.lower() != expected or not removed:
            return AutomaticTestResult(
                False,
                evidence={"expected_sha256": expected, "observed_sha256": observed},
                message="o microSD não confirmou leitura, hash e remoção do arquivo temporário",
                source="usb_serial+fat32+sdspi",
            )
        return AutomaticTestResult(
            True,
            evidence={
                "request_id": str(_status_value(result, "request_id", "") or ""),
                "ack_status": str(_status_value(result, "status", "") or ""),
                "bytes_written": int(state.get("bytes_written") or 0),
                "bytes_read": int(state.get("bytes_read") or 0),
                "sha256": observed,
                "temporary_file_removed": removed,
            },
            source="usb_serial+fat32+sdspi",
        )

    def _runtime_snapshot(self, context: ValidationContext) -> tuple[Any, Mapping[str, Any]]:
        return wait_for_runtime_snapshot(context, timeout=30.0, require_ready=True)

    def _commission_when_accessory_ready(
        self,
        context: ValidationContext,
        profile: DeviceProfile,
        action: str,
        params: Mapping[str, Any],
        *,
        startup_timeout: float = 30.0,
    ) -> Any:
        """Retry only the transient gap between USB boot and worker attach."""

        deadline = time.monotonic() + startup_timeout
        while True:
            try:
                return context.client.commission_accessory(profile, action, params)
            except (ProvisioningError, ProvisioningTimeout) as exc:
                message = str(exc).casefold()
                transient = any(
                    marker in message
                    for marker in (
                        "disabled or unavailable",
                        "ainda não está disponível",
                        "ainda nao esta disponivel",
                        "not ready",
                    )
                )
                if not transient or time.monotonic() >= deadline:
                    raise
                time.sleep(min(0.5, max(0.0, deadline - time.monotonic())))

    def _test_network_runtime(
        self,
        context: ValidationContext,
        _check: ValidationCheck,
        _params: Mapping[str, Any],
    ) -> AutomaticTestResult:
        result, state = self._runtime_snapshot(context)
        ready = bool(state.get("network_ready"))
        return AutomaticTestResult(
            ready,
            evidence={
                "request_id": str(_status_value(result, "request_id", "") or ""),
                "ack_status": str(_status_value(result, "status", "") or ""),
                "network_ready": ready,
                "last_udp_send_secs": int(state.get("last_udp_send_secs") or 0),
                "dropped_packets": int(state.get("dropped_packets") or 0),
                "transmitted_packets": int(state.get("transmitted_packets") or 0),
            },
            message="o firmware ainda não confirmou Wi-Fi/IP e envio UDP" if not ready else "",
            source="usb_serial+firmware_runtime_counters",
        )

    def _test_csi_runtime(
        self,
        context: ValidationContext,
        _check: ValidationCheck,
        params: Mapping[str, Any],
    ) -> AutomaticTestResult:
        _first_result, first = self._runtime_snapshot(context)
        delay = float(params.get("observation_seconds") or 1.25)
        if not 1.0 <= delay <= 10.0:
            return AutomaticTestResult(False, message="janela de observação CSI inválida")
        time.sleep(delay)
        second_result, second = self._runtime_snapshot(context)
        before = int(first.get("last_udp_send_secs") or 0)
        after = int(second.get("last_udp_send_secs") or 0)
        progressing = bool(second.get("csi_streaming")) and after > before
        return AutomaticTestResult(
            progressing,
            evidence={
                "request_id": str(_status_value(second_result, "request_id", "") or ""),
                "ack_status": str(_status_value(second_result, "status", "") or ""),
                "observation_seconds": delay,
                "last_udp_send_before_secs": before,
                "last_udp_send_after_secs": after,
                "dropped_packets": int(second.get("dropped_packets") or 0),
                "transmitted_packets": int(second.get("transmitted_packets") or 0),
                "raw_csi_captured_by_host": False,
                "empty_room_authorized": True,
            },
            message="o contador interno de envio CSI não avançou" if not progressing else "",
            source="usb_serial+firmware_runtime_counters",
        )

    def _test_ir_rx(
        self,
        context: ValidationContext,
        _check: ValidationCheck,
        params: Mapping[str, Any],
    ) -> AutomaticTestResult:
        timeout_ms = int(params.get("timeout_ms") or 15_000)
        code_id = str(params.get("code_id") or f"hardware-test-{uuid4().hex[:12]}")
        result = self._commission_when_accessory_ready(
            context,
            context.profile,
            "ir.learn.start",
            {
                "code_id": code_id,
                "name": str(params.get("name") or "Teste físico automático"),
                "timeout_ms": timeout_ms,
                "idle_threshold_us": context.profile.ir_rx.idle_threshold_us,
            },
        )
        state = _status_value(result, "state", {}) or {}
        code = state.get("code") if isinstance(state, Mapping) else None
        if not isinstance(code, Mapping):
            return AutomaticTestResult(False, message="o firmware não devolveu o sinal IR capturado")
        sequence = code.get("sequence_us")
        if not isinstance(sequence, list) or not 2 <= len(sequence) <= 2048:
            return AutomaticTestResult(False, message="a captura IR retornou uma sequência inválida")
        if any(not isinstance(item, int) or isinstance(item, bool) or item <= 0 for item in sequence):
            return AutomaticTestResult(False, message="a captura IR contém durações inválidas")
        total_us = sum(sequence)
        if total_us > 5_000_000:
            return AutomaticTestResult(False, message="a captura IR excedeu cinco segundos")
        digest = hashlib.sha256(
            json.dumps(sequence, separators=(",", ":")).encode("utf-8")
        ).hexdigest()
        artifact = dict(code)
        return AutomaticTestResult(
            True,
            evidence={
                "request_id": str(_status_value(result, "request_id", "") or ""),
                "ack_status": str(_status_value(result, "status", "") or ""),
                "code_id": str(code.get("code_id") or code_id),
                "duration_count": len(sequence),
                "total_duration_us": total_us,
                "sequence_sha256": digest,
            },
            source="usb_serial+rmt_rx",
            artifact=artifact,
        )

    def _test_ir_tx(
        self,
        context: ValidationContext,
        _check: ValidationCheck,
        params: Mapping[str, Any],
    ) -> AutomaticTestResult:
        code = params.get("code") or self._artifacts.get("ir_rx")
        if not isinstance(code, Mapping):
            raise HardwareValidationBlocked("capture primeiro um sinal real no teste do receptor IR")
        sequence = code.get("sequence_us")
        if not isinstance(sequence, list) or not 2 <= len(sequence) <= 2048:
            raise HardwareValidationBlocked("a captura disponível para retransmissão é inválida")
        payload = {
            "code_id": str(code.get("code_id") or f"hardware-test-{uuid4().hex[:12]}"),
            "sequence_us": list(sequence),
            "carrier_hz": int(code.get("carrier_hz") or context.profile.ir_tx.carrier_hz),
            "duty_percent": int(code.get("duty_percent") or context.profile.ir_tx.duty_percent),
            "repeats": int(params.get("repeats") or 1),
        }
        result = self._commission_when_accessory_ready(
            context, context.profile, "ir.send", payload
        )
        digest = hashlib.sha256(
            json.dumps(sequence, separators=(",", ":")).encode("utf-8")
        ).hexdigest()
        return AutomaticTestResult(
            True,
            evidence={
                "request_id": str(_status_value(result, "request_id", "") or ""),
                "ack_status": str(_status_value(result, "status", "") or ""),
                "code_id": payload["code_id"],
                "sequence_sha256": digest,
                "repeats": payload["repeats"],
            },
            source="usb_serial+rmt_tx",
        )

    def _test_relay(
        self,
        context: ValidationContext,
        check: ValidationCheck,
        params: Mapping[str, Any],
    ) -> AutomaticTestResult:
        relay_id = check.check_id.split(":", 1)[1]
        relay = next(
            (item for item in context.profile.relays if item.relay_id == relay_id and item.enabled),
            None,
        )
        if relay is None:
            return AutomaticTestResult(False, message="o relé não está habilitado no perfil")
        duration_ms = int(params.get("duration_ms") or min(250, relay.pulse_limit_ms))
        if duration_ms <= 0 or duration_ms > relay.pulse_limit_ms:
            return AutomaticTestResult(False, message="o pulso solicitado excede o limite seguro do relé")
        result = self._commission_when_accessory_ready(
            context,
            context.profile,
            "relay.pulse",
            {"relay_id": relay_id, "duration_ms": duration_ms},
        )
        return AutomaticTestResult(
            True,
            evidence={
                "request_id": str(_status_value(result, "request_id", "") or ""),
                "ack_status": str(_status_value(result, "status", "") or ""),
                "relay_id": relay_id,
                "duration_ms": duration_ms,
            },
            source="usb_serial+relay_driver",
        )

    def _get_check(self, check_id: str) -> ValidationCheck:
        self._require_started()
        assert self.session is not None
        try:
            return self.session.checks[check_id]
        except KeyError as exc:
            raise HardwareValidationError(f"teste desconhecido: {check_id}") from exc

    def _require_started(self) -> None:
        if self.session is None or self.profile is None or self.report_path is None:
            raise HardwareValidationError("inicie uma sessão de validação primeiro")

    def _allocate_report_path(self, instant: datetime, profile_id: str) -> tuple[str, Path]:
        category = self.results_root / "firmware-hardware"
        timestamp = instant.strftime("%Y-%m-%dt%H-%M-%Sz").lower()
        suffix = re.sub(r"[^a-z0-9]+", "", profile_id.casefold())[:8] or "device"
        base = f"{timestamp}_{suffix}"
        session_id = base
        counter = 2
        while (category / session_id).exists():
            session_id = f"{base}-{counter}"
            counter += 1
        return session_id, category / session_id / "session.json"

    def _persist(self) -> None:
        self._require_started()
        assert self.session is not None and self.report_path is not None
        now_text = _iso(self.now())
        self.session.updated_at = now_text
        states = [self.session.preflight.state, *(check.state for check in self.session.checks.values())]
        if states and all(state in TERMINAL_STATES for state in states):
            self.session.completed_at = self.session.completed_at or now_text
        else:
            self.session.completed_at = None
        payload = self._redacted_report(self.session.to_dict())
        encoded = (json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n").encode("utf-8")
        path = self.report_path
        path.parent.mkdir(parents=True, exist_ok=True)
        descriptor, temporary_name = tempfile.mkstemp(
            prefix=".session.", suffix=".tmp", dir=path.parent
        )
        try:
            with os.fdopen(descriptor, "wb") as handle:
                handle.write(encoded)
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(temporary_name, path)
        finally:
            try:
                os.unlink(temporary_name)
            except FileNotFoundError:
                pass

    def _redacted_report(self, report: Mapping[str, Any]) -> dict[str, Any]:
        evidence_authorization_keys = {
            "authorization_required",
            "authorization_prompt",
            "manual_authorization",
        }

        def visit(value: Any) -> Any:
            if isinstance(value, Mapping):
                output: dict[str, Any] = {}
                for key, item in value.items():
                    text_key = str(key)
                    if (
                        is_secret_key(text_key)
                        and text_key not in evidence_authorization_keys
                        and item is not None
                        and item != ""
                    ):
                        output[text_key] = MASK
                    else:
                        output[text_key] = visit(item)
                return output
            if isinstance(value, list):
                return [visit(item) for item in value]
            if isinstance(value, tuple):
                return [visit(item) for item in value]
            if isinstance(value, str):
                return redact_text(value, self.secret_values)
            return value

        return visit(report)
