"""USB-serial client for the `aethersense.provision.v1` atomic protocol."""

from __future__ import annotations

from dataclasses import dataclass
import binascii
import hashlib
import json
import math
import re
import time
from typing import Any, Callable
from uuid import uuid4

from .diffing import compute_build_hash, compute_config_hash
from .models import DeviceProfile, normalise_mac
from .secrets import SecretManager, synchronise_secret_revisions


PROVISION_PROTOCOL = "aethersense.provision.v1"
WIRE_VERSION = 1
MAX_PAYLOAD_BYTES = 128 * 1024
HOST_MAX_CHUNK_BYTES = 128
DEFAULT_RESPONSE_TIMEOUT_SECONDS = 5.0
DEFAULT_OPERATION_TIMEOUT_SECONDS = 30.0
DEFAULT_HELLO_ATTEMPT_TIMEOUT_SECONDS = 1.0
DEFAULT_SERIAL_SETTLE_SECONDS = 0.25
DEFAULT_HELLO_RETRY_SECONDS = 0.20


class ProvisioningError(RuntimeError):
    pass


class ProvisioningTimeout(ProvisioningError):
    """A deadline expired while exchanging non-secret provisioning frames."""


def crc32_ieee(data: bytes) -> int:
    return binascii.crc32(data) & 0xFFFF_FFFF


def cobs_encode(data: bytes) -> bytes:
    output = bytearray(b"\x00")
    code_index = 0
    code = 1
    for byte in data:
        if byte == 0:
            output[code_index] = code
            code_index = len(output)
            output.append(0)
            code = 1
        else:
            output.append(byte)
            code += 1
            if code == 0xFF:
                output[code_index] = code
                code_index = len(output)
                output.append(0)
                code = 1
    output[code_index] = code
    output.append(0)
    return bytes(output)


def cobs_decode(frame: bytes) -> bytes:
    if frame.endswith(b"\x00"):
        frame = frame[:-1]
    if not frame:
        raise ProvisioningError("empty provisioning frame")
    output = bytearray()
    index = 0
    while index < len(frame):
        code = frame[index]
        if code == 0:
            raise ProvisioningError("invalid COBS frame")
        index += 1
        end = index + code - 1
        if end > len(frame):
            raise ProvisioningError("invalid COBS frame")
        output.extend(frame[index:end])
        index = end
        if code != 0xFF and index < len(frame):
            output.append(0)
    return bytes(output)


def encode_frame(message: dict[str, Any]) -> bytes:
    payload = json.dumps(
        message, ensure_ascii=False, separators=(",", ":"), sort_keys=True
    ).encode("utf-8")
    if len(payload) > MAX_PAYLOAD_BYTES:
        raise ProvisioningError("provisioning message exceeds 128 KiB")
    body = bytes([WIRE_VERSION]) + payload
    body += crc32_ieee(body).to_bytes(4, "little")
    return cobs_encode(body)


def decode_frame(frame: bytes) -> dict[str, Any]:
    decoded = cobs_decode(frame)
    if len(decoded) < 5:
        raise ProvisioningError("empty provisioning frame")
    body, crc_bytes = decoded[:-4], decoded[-4:]
    expected = int.from_bytes(crc_bytes, "little")
    actual = crc32_ieee(body)
    if expected != actual:
        raise ProvisioningError(f"CRC mismatch: expected {expected:08x}, got {actual:08x}")
    if body[0] != WIRE_VERSION:
        raise ProvisioningError(f"unsupported wire version {body[0]}")
    try:
        result = json.loads(body[1:].decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ProvisioningError(f"invalid provisioning JSON: {exc}") from exc
    if not isinstance(result, dict):
        raise ProvisioningError("provisioning response must be a JSON object")
    return result


def provisioned_config_dict(
    profile: DeviceProfile,
    secret_manager: SecretManager,
) -> dict[str, Any]:
    synchronise_secret_revisions(
        profile,
        secret_manager,
        require_available=True,
    )
    return {
        "protocol": PROVISION_PROTOCOL,
        # Bind this atomic NVS payload to the exact source-aware binary that
        # accepted it.  Target fields alone cannot detect firmware code changes.
        "build_hash": compute_build_hash(profile),
        "profile": profile.to_firmware_dict(),
        "secrets": {
            "wifi_password": secret_manager.get(profile.network.wifi.password_ref) or "",
            "mqtt_password": secret_manager.get(profile.mqtt.password_ref) or "",
            "server_bearer_token": secret_manager.get(profile.server.bearer_token_ref) or "",
        },
    }


def provisioned_config_json(profile: DeviceProfile, secret_manager: SecretManager) -> bytes:
    return json.dumps(
        provisioned_config_dict(profile, secret_manager),
        ensure_ascii=False,
        separators=(",", ":"),
        sort_keys=True,
    ).encode("utf-8")


@dataclass(frozen=True)
class ProvisionResult:
    request_id: str
    config_hash: str
    generation: int
    build_hash: str
    device_id: str | None
    payload_sha256: str


@dataclass(frozen=True)
class SerialDeviceStatus:
    device_id: str
    build_hash: str
    config_hash: str | None
    generation: int | None
    features: tuple[str, ...]
    firmware_version: str = ""
    boot_id: str = ""


@dataclass(frozen=True)
class AccessoryCommissionResult:
    request_id: str
    action: str
    status: str
    state: dict[str, Any] | None


class ProvisionClient:
    def __init__(
        self,
        port: str,
        *,
        baudrate: int = 115_200,
        timeout: float = DEFAULT_RESPONSE_TIMEOUT_SECONDS,
        operation_timeout: float = DEFAULT_OPERATION_TIMEOUT_SECONDS,
        serial_settle: float = DEFAULT_SERIAL_SETTLE_SECONDS,
        hello_retry: float = DEFAULT_HELLO_RETRY_SECONDS,
        serial_factory: Callable[..., Any] | None = None,
        client_name: str = "aethersense-build.py",
        clock: Callable[[], float] = time.monotonic,
        sleeper: Callable[[float], None] = time.sleep,
        progress: Callable[[str], None] | None = None,
    ) -> None:
        for label, value in (
            ("timeout", timeout),
            ("operation_timeout", operation_timeout),
        ):
            if not math.isfinite(value) or value <= 0:
                raise ValueError(f"{label} must be a positive finite number")
        for label, value in (("serial_settle", serial_settle), ("hello_retry", hello_retry)):
            if not math.isfinite(value) or value < 0:
                raise ValueError(f"{label} must be a non-negative finite number")
        self.port = port
        self.baudrate = baudrate
        self.timeout = float(timeout)
        self.operation_timeout = float(operation_timeout)
        self.serial_settle = float(serial_settle)
        self.hello_retry = float(hello_retry)
        self.client_name = client_name
        self.clock = clock
        self.sleeper = sleeper
        self.progress = progress
        if serial_factory is None:
            import serial

            def serial_factory(port, **kwargs):
                # Configure modem-control lines before opening the handle. Opening
                # an ESP USB serial port with pyserial's default DTR/RTS state can
                # reset the board and discard the first hello frame.
                connection = serial.serial_for_url(port, do_not_open=True, **kwargs)
                connection.dtr = False
                connection.rts = False
                connection.open()
                return connection
        self.serial_factory = serial_factory

    def _report(self, message: str) -> None:
        if self.progress is not None:
            self.progress(message)

    def _remaining(self, deadline: float, label: str) -> float:
        remaining = deadline - self.clock()
        if remaining <= 0:
            raise ProvisioningTimeout(f"{label} timed out")
        return remaining

    def _read_frame(
        self,
        connection,
        *,
        deadline: float | None = None,
        timeout_label: str = "provisioning response",
    ) -> bytes:
        deadline = self.clock() + self.timeout if deadline is None else deadline
        frame = bytearray()
        scanned = 0
        scan_limit = (MAX_PAYLOAD_BYTES + 1024) * 4
        while scanned <= scan_limit:
            remaining = self._remaining(deadline, timeout_label)
            # A serial timeout is per read, so bound every read by the absolute
            # deadline as well. Continuous boot/log bytes must not extend it.
            try:
                connection.timeout = min(self.timeout, remaining)
            except (AttributeError, TypeError, ValueError):
                pass
            byte = connection.read(1)
            if not byte:
                raise ProvisioningTimeout(f"{timeout_label} timed out")
            scanned += 1
            if byte == b"\x00":
                if not frame:
                    # Firmware emits a leading delimiter to resynchronise after
                    # boot logs; an empty frame is not a protocol response.
                    continue
                candidate = bytes(frame) + b"\x00"
                frame.clear()
                try:
                    decode_frame(candidate)
                except ProvisioningError:
                    # Ignore a delimited serial-log fragment and continue until
                    # a CRC-valid COBS provisioning frame arrives.
                    continue
                return candidate
            frame.extend(byte)
            if len(frame) > MAX_PAYLOAD_BYTES + 1024:
                frame.clear()
        raise ProvisioningError("no valid provisioning frame found before scan limit")

    def _exchange(
        self,
        connection,
        message: dict[str, Any],
        *,
        operation_deadline: float | None = None,
        response_timeout: float | None = None,
    ) -> dict[str, Any]:
        operation_deadline = (
            self.clock() + self.operation_timeout
            if operation_deadline is None
            else operation_deadline
        )
        operation_remaining = self._remaining(operation_deadline, "provisioning operation")
        response_budget = min(response_timeout or self.timeout, operation_remaining)
        response_deadline = self.clock() + response_budget
        timeout_label = (
            "provisioning operation"
            if operation_remaining <= response_budget
            else "provisioning response"
        )

        encoded = encode_frame(message)
        written = connection.write(encoded)
        if written is not None and written != len(encoded):
            raise ProvisioningError("serial port accepted only part of a provisioning frame")
        # Serial.write honours write_timeout. Avoid flush(), which can wait
        # indefinitely in some Windows USB drivers and defeats the total deadline.
        self._remaining(operation_deadline, "provisioning operation")
        response = decode_frame(
            self._read_frame(
                connection,
                deadline=min(response_deadline, operation_deadline),
                timeout_label=timeout_label,
            )
        )
        if response.get("type") == "error":
            code = response.get("code", "unknown")
            raise ProvisioningError(f"device rejected provisioning ({code}): {response.get('message', '')}")
        return response

    def _hello(self, connection, operation_deadline: float) -> dict[str, Any]:
        request = {
            "type": "hello",
            "protocol": PROVISION_PROTOCOL,
            "client": self.client_name,
        }
        while True:
            remaining = self._remaining(operation_deadline, "provisioning operation")
            try:
                return self._expect(
                    self._exchange(
                        connection,
                        request,
                        operation_deadline=operation_deadline,
                        response_timeout=min(
                            self.timeout,
                            remaining,
                        ),
                    ),
                    "hello",
                )
            except ProvisioningError as exc:
                # Opening CH340/CP210x adapters can leave one truncated byte
                # sequence in the ESP console RX path even with DTR/RTS held
                # inactive. The firmware correctly rejects that fragment as
                # an invalid COBS frame. Treat only this synchronisation error
                # like a delayed hello; all other protocol errors remain fatal.
                if not isinstance(exc, ProvisioningTimeout) and "invalid_frame" not in str(exc):
                    raise
                remaining = self._remaining(operation_deadline, "provisioning operation")
                if self.hello_retry <= 0:
                    raise
                self.sleeper(min(self.hello_retry, remaining))
                self._report("Aguardando firmware de provisionamento responder ao hello…")

    @staticmethod
    def _expect(response: dict[str, Any], expected: str) -> dict[str, Any]:
        actual = response.get("type")
        if actual != expected:
            raise ProvisioningError(f"expected {expected} response, got {actual}")
        return response

    def _exchange_idempotent(
        self,
        connection,
        message: dict[str, Any],
        expected: str,
        label: str,
        operation_deadline: float,
    ) -> dict[str, Any]:
        def expect_after_stale_hello(response: dict[str, Any]) -> dict[str, Any]:
            # A slow board can answer the first hello just after the host has
            # retried it. The second, equally valid hello may then be queued in
            # front of capabilities/current_config. Consume only that known
            # idempotent duplicate; every other unexpected type remains fatal.
            stale_count = 0
            while response.get("type") == "hello" and expected != "hello":
                stale_count += 1
                if stale_count > 2:
                    raise ProvisioningError("too many stale hello responses on serial link")
                self._report(f"Ignorando hello duplicado antes de {label}…")
                response = decode_frame(
                    self._read_frame(
                        connection,
                        deadline=min(
                            operation_deadline,
                            self.clock() + self.timeout,
                        ),
                        timeout_label=f"resposta de {label}",
                    )
                )
            return self._expect(response, expected)

        try:
            return expect_after_stale_hello(
                self._exchange(
                    connection,
                    message,
                    operation_deadline=operation_deadline,
                )
            )
        except ProvisioningTimeout:
            self._report(f"Resposta de {label} atrasou; tentando novamente…")
            return expect_after_stale_hello(
                self._exchange(
                    connection,
                    message,
                    operation_deadline=operation_deadline,
                )
            )

    def _prepare_connection(self, connection, operation_deadline: float) -> None:
        self._remaining(operation_deadline, "operação serial")
        if self.serial_settle:
            self.sleeper(
                min(
                    self.serial_settle,
                    self._remaining(operation_deadline, "operação serial"),
                )
            )
        if hasattr(connection, "reset_input_buffer"):
            connection.reset_input_buffer()

    def _validate_identity(
        self,
        profile: DeviceProfile,
        hello: dict[str, Any],
        *,
        require_build: bool,
    ) -> str:
        if hello.get("protocol") != PROVISION_PROTOCOL:
            raise ProvisioningError("firmware usa um protocolo serial incompatível")
        device_id = str(hello.get("device_id") or "").strip().lower()
        if not device_id.startswith("esp32-") or len(device_id) != 18:
            raise ProvisioningError("firmware não informou uma identidade eFuse válida")
        if profile.device_id and profile.device_id != device_id:
            raise ProvisioningError("a porta selecionada pertence a outro dispositivo")
        if require_build and hello.get("build_hash") != compute_build_hash(profile):
            raise ProvisioningError(
                "o firmware da placa não corresponde ao perfil; use Executar no ESP uma vez"
            )
        return device_id

    def inspect_device(self, profile: DeviceProfile) -> SerialDeviceStatus:
        """Read live identity/capabilities over USB without MQTT or a server."""

        operation_deadline = self.clock() + self.operation_timeout
        io_timeout = min(self.timeout, self.operation_timeout)
        with self.serial_factory(
            self.port,
            baudrate=self.baudrate,
            timeout=io_timeout,
            write_timeout=io_timeout,
        ) as connection:
            self._prepare_connection(connection, operation_deadline)
            hello = self._hello(connection, operation_deadline)
            device_id = self._validate_identity(profile, hello, require_build=False)
            capabilities = self._exchange_idempotent(
                connection,
                {"type": "capabilities"},
                "capabilities",
                "capacidades",
                operation_deadline,
            )
            current = self._exchange_idempotent(
                connection,
                {"type": "current_config"},
                "current_config",
                "configuração atual",
                operation_deadline,
            )
        return SerialDeviceStatus(
            device_id=device_id,
            build_hash=str(hello.get("build_hash") or ""),
            config_hash=current.get("config_hash"),
            generation=(
                int(current["generation"])
                if current.get("generation") is not None
                else None
            ),
            features=tuple(str(item) for item in capabilities.get("features", [])),
            firmware_version=str(hello.get("firmware_version") or ""),
            boot_id=str(hello.get("boot_id") or ""),
        )

    def commission_accessory(
        self,
        profile: DeviceProfile,
        action: str,
        params: dict[str, Any] | None = None,
        *,
        cancel_requested: Callable[[], bool] | None = None,
    ) -> AccessoryCommissionResult:
        """Execute a physical accessory test directly over the board's USB port."""

        allowed = {
            "relay.set",
            "relay.pulse",
            "ir.learn.start",
            "ir.learn.cancel",
            "ir.send",
            "storage.record.start",
            "storage.record.stop",
            "storage.sync",
            "storage.self_test",
            "diagnostic.status_led",
            "diagnostic.runtime",
            "diagnostic.reboot",
        }
        if action not in allowed:
            raise ProvisioningError(f"ação local não permitida: {action}")
        command_params = dict(params or {})
        if action == "ir.learn.start" and not profile.ir_rx.enabled:
            raise ProvisioningError("receptor IR está desabilitado no perfil")
        if action == "ir.send" and not profile.ir_tx.enabled:
            raise ProvisioningError("transmissor IR está desabilitado no perfil")
        if action.startswith("storage.") and not profile.storage.enabled:
            raise ProvisioningError("microSD está desabilitado no perfil")
        if action == "diagnostic.status_led":
            if not profile.runtime.status_led.enabled:
                raise ProvisioningError("LED RGB de estado está desabilitado no perfil")
            color = str(command_params.get("color") or "")
            if color not in {
                "boot",
                "wifi_connecting",
                "wifi_connected",
                "streaming",
                "provisioning",
                "recording",
                "offline",
                "ir_rx",
                "ir_tx",
                "error",
            }:
                raise ProvisioningError("cor de diagnóstico do LED RGB inválida")
            try:
                duration_ms = int(command_params.get("duration_ms") or 0)
            except (TypeError, ValueError) as exc:
                raise ProvisioningError("duração de diagnóstico do LED RGB inválida") from exc
            if not 100 <= duration_ms <= 5_000:
                raise ProvisioningError("duração do LED RGB deve ficar entre 100 e 5000 ms")
            command_params["duration_ms"] = duration_ms
        if action in {"diagnostic.runtime", "diagnostic.reboot"}:
            command_params.clear()
        if action == "storage.self_test":
            payload = str(command_params.get("payload") or "")
            expected_sha256 = str(command_params.get("expected_sha256") or "")
            if not 16 <= len(payload.encode("utf-8")) <= 4_096:
                raise ProvisioningError("payload do autoteste do microSD deve ter de 16 a 4096 bytes")
            if not re.fullmatch(r"[0-9a-fA-F]{64}", expected_sha256):
                raise ProvisioningError("SHA-256 esperado do autoteste do microSD é inválido")
        if action == "ir.send":
            sequence = command_params.get("sequence_us")
            if not isinstance(sequence, list) or not 2 <= len(sequence) <= 2048:
                raise ProvisioningError(
                    "capture primeiro um sinal IR ou informe uma sequência válida"
                )
            command_params.setdefault("carrier_hz", 38_000)
            command_params.setdefault("duty_percent", 33)
            command_params.setdefault("repeats", 1)

        operation_deadline = self.clock() + self.operation_timeout
        io_timeout = min(self.timeout, self.operation_timeout)
        request_id = str(uuid4())
        learn_timeout_ms = int(command_params.get("timeout_ms") or 15_000)
        result_deadline = operation_deadline
        accepted = False

        with self.serial_factory(
            self.port,
            baudrate=self.baudrate,
            timeout=io_timeout,
            write_timeout=io_timeout,
        ) as connection:
            self._prepare_connection(connection, operation_deadline)
            hello = self._hello(connection, operation_deadline)
            self._validate_identity(profile, hello, require_build=True)
            capabilities = self._exchange_idempotent(
                connection,
                {"type": "capabilities"},
                "capabilities",
                "capacidades",
                operation_deadline,
            )
            if "serial-commissioning-v1" not in capabilities.get("features", []):
                raise ProvisioningError(
                    "firmware ainda não suporta testes USB; use Executar no ESP uma vez"
                )
            current = self._exchange_idempotent(
                connection,
                {"type": "current_config"},
                "current_config",
                "configuração atual",
                operation_deadline,
            )
            if current.get("config_hash") != compute_config_hash(profile):
                raise ProvisioningError(
                    "a configuração de pinos ainda não foi aplicada; use Aplicar configuração"
                )

            envelope = {
                "schema": "aethersense.command.v1",
                "request_id": request_id,
                "boot_id": str(hello.get("boot_id") or "") or None,
                "issued_at_ms": int(time.time() * 1000),
                "ttl_ms": max(15_000, learn_timeout_ms + 5_000),
                "action": action,
            }
            if action not in {
                "ir.learn.cancel",
                "diagnostic.runtime",
                "diagnostic.reboot",
            }:
                envelope["params"] = command_params
            command_request = {"type": "accessory_command", "command": envelope}
            try:
                response = self._expect(
                    self._exchange(
                        connection,
                        command_request,
                        operation_deadline=operation_deadline,
                    ),
                    "accessory_result",
                )
            except ProvisioningTimeout:
                # The firmware records the acknowledged result before writing
                # it to USB. If CH340/UART traffic drops that first response,
                # recover it by request_id instead of dispatching the physical
                # action twice (which would be unsafe for relays and IR TX).
                self._report("Resposta inicial do acessório atrasou; recuperando pelo request_id…")
                try:
                    response = self._expect(
                        self._exchange(
                            connection,
                            {"type": "accessory_result", "request_id": request_id},
                            operation_deadline=operation_deadline,
                        ),
                        "accessory_result",
                    )
                except ProvisioningError as exc:
                    # If the board explicitly says the request is unknown, the
                    # command itself never reached the dispatcher and one
                    # retry with the same immutable request_id is safe.
                    if "unknown or expired" not in str(exc):
                        raise
                    response = self._expect(
                        self._exchange(
                            connection,
                            command_request,
                            operation_deadline=operation_deadline,
                        ),
                        "accessory_result",
                    )

            while True:
                ack = response.get("ack") or {}
                if ack.get("request_id") != request_id:
                    raise ProvisioningError("firmware respondeu com request_id divergente")
                status = str(ack.get("status") or "")
                if status == "acknowledged" and not accepted:
                    accepted = True
                    if action == "ir.learn.start":
                        result_deadline = min(
                            operation_deadline,
                            self.clock() + (learn_timeout_ms / 1000.0) + 2.0,
                        )
                if status in {"completed", "ok"}:
                    state = ack.get("state")
                    return AccessoryCommissionResult(
                        request_id=request_id,
                        action=action,
                        status=status,
                        state=state if isinstance(state, dict) else None,
                    )
                if status == "error":
                    error = str(ack.get("error") or "o acessório recusou o comando")
                    if (
                        "disabled or unavailable" in error
                        and self.clock() + 0.5 < operation_deadline
                    ):
                        self._report("Aguardando o runtime do acessório iniciar…")
                        self.sleeper(0.5)
                        response = self._expect(
                            self._exchange(
                                connection,
                                {"type": "accessory_command", "command": envelope},
                                operation_deadline=operation_deadline,
                            ),
                            "accessory_result",
                        )
                        continue
                    raise ProvisioningError(error)
                if action == "ir.learn.start" and cancel_requested is not None and cancel_requested():
                    cancel_id = str(uuid4())
                    cancel = dict(envelope)
                    cancel.update(
                        {
                            "request_id": cancel_id,
                            "action": "ir.learn.cancel",
                        }
                    )
                    cancel.pop("params", None)
                    self._exchange(
                        connection,
                        {"type": "accessory_command", "command": cancel},
                        operation_deadline=operation_deadline,
                    )
                    raise ProvisioningError("captura IR cancelada pelo usuário")
                if self.clock() >= result_deadline:
                    if action == "ir.learn.start" and self.clock() < operation_deadline:
                        cancel_id = str(uuid4())
                        cancel = dict(envelope)
                        cancel.update(
                            {
                                "request_id": cancel_id,
                                "action": "ir.learn.cancel",
                            }
                        )
                        cancel.pop("params", None)
                        try:
                            self._exchange(
                                connection,
                                {"type": "accessory_command", "command": cancel},
                                operation_deadline=operation_deadline,
                            )
                        except ProvisioningError:
                            pass
                    if action == "ir.learn.start":
                        gpio = profile.ir_rx.gpio
                        pin = f"GPIO{gpio}" if gpio is not None else "GPIO configurado"
                        raise ProvisioningTimeout(
                            f"nenhum sinal IR válido chegou ao {pin}; confira no módulo "
                            f"OUT/S → {pin}, VCC/+ → 3V3 e GND/- → GND (a ordem física "
                            "dos três pinos varia entre receptores)"
                        )
                    raise ProvisioningTimeout(f"o comando {action} não terminou no prazo")
                self.sleeper(min(0.1, self._remaining(result_deadline, "teste do acessório")))
                response = self._expect(
                    self._exchange(
                        connection,
                        {"type": "accessory_result", "request_id": request_id},
                        operation_deadline=operation_deadline,
                    ),
                    "accessory_result",
                )

    def provision(
        self,
        profile: DeviceProfile,
        secret_manager: SecretManager,
    ) -> ProvisionResult:
        operation_deadline = self.clock() + self.operation_timeout
        request_id = str(uuid4())
        expected_build_hash = compute_build_hash(profile)
        began = False
        io_timeout = min(self.timeout, self.operation_timeout)
        self._report(f"Abrindo porta serial {self.port} para provisionamento…")
        with self.serial_factory(
            self.port,
            baudrate=self.baudrate,
            timeout=io_timeout,
            write_timeout=io_timeout,
        ) as connection:
            self._remaining(operation_deadline, "provisioning operation")
            if self.serial_settle:
                self.sleeper(
                    min(
                        self.serial_settle,
                        self._remaining(operation_deadline, "provisioning operation"),
                    )
                )
            if hasattr(connection, "reset_input_buffer"):
                connection.reset_input_buffer()

            def exchange(message: dict[str, Any]) -> dict[str, Any]:
                return self._exchange(
                    connection,
                    message,
                    operation_deadline=operation_deadline,
                )

            def exchange_idempotent(
                message: dict[str, Any], expected: str, label: str
            ) -> dict[str, Any]:
                try:
                    return self._expect(exchange(message), expected)
                except ProvisioningTimeout:
                    # Read-only queries are safe to repeat. This also consumes a
                    # valid late response from the first request when USB/UART
                    # logging briefly delays delivery on Windows.
                    self._report(f"Resposta de {label} atrasou; tentando novamente…")
                    return self._expect(exchange(message), expected)

            self._report("Negociando protocolo com o firmware…")
            hello = self._hello(connection, operation_deadline)
            if hello.get("protocol") != PROVISION_PROTOCOL:
                raise ProvisioningError("device reported an incompatible provisioning protocol")
            hello_device_id = str(hello.get("device_id") or "").strip().lower()
            if not hello_device_id.startswith("esp32-") or len(hello_device_id) != 18:
                raise ProvisioningError("device did not report a valid eFuse-derived device_id")
            hello_mac = normalise_mac(hello_device_id.removeprefix("esp32-"))
            if not hello_mac:
                raise ProvisioningError("device reported an invalid eFuse-derived device_id")
            if profile.device_id and profile.device_id != hello_device_id:
                raise ProvisioningError(
                    "profile device_id differs from the eFuse identity reported by the board"
                )
            profile.device_id = hello_device_id
            if profile.association.efuse_mac and normalise_mac(
                profile.association.efuse_mac
            ) != hello_mac:
                raise ProvisioningError(
                    "profile eFuse MAC differs from the identity reported by the board"
                )
            profile.association.efuse_mac = hello_mac
            if hello.get("build_hash") != expected_build_hash:
                raise ProvisioningError(
                    "firmware build hash differs from the selected profile; flash the required build first"
                )
            # Identity is part of the canonical configuration hash. Build the
            # payload only after the read-only hello has bound this profile to
            # the board's eFuse MAC.
            try:
                payload = provisioned_config_json(profile, secret_manager)
            except ValueError as exc:
                raise ProvisioningError(str(exc)) from exc
            if not payload or len(payload) > MAX_PAYLOAD_BYTES:
                raise ProvisioningError("provisioned config exceeds 128 KiB")
            payload_sha256 = hashlib.sha256(payload).hexdigest()
            expected_config_hash = compute_config_hash(profile)
            self._report("Consultando capacidades do firmware…")
            capabilities = exchange_idempotent(
                {"type": "capabilities"}, "capabilities", "capacidades"
            )
            if capabilities.get("profile_schema") != "aethersense.device_profile.v1":
                raise ProvisioningError("device does not support device_profile.v1")
            chunk_bytes = int(capabilities.get("max_chunk_bytes") or HOST_MAX_CHUNK_BYTES)
            # A chunk is encoded as a JSON integer array, so its transient wire
            # representation is roughly four times larger. Keep the host below
            # the C5's measured safe peak even when older firmware advertises
            # the former 256-byte limit.
            chunk_bytes = max(64, min(chunk_bytes, HOST_MAX_CHUNK_BYTES))
            exchange_idempotent(
                {"type": "current_config"}, "current_config", "configuração atual"
            )
            try:
                self._expect(
                    exchange(
                        {
                            "type": "begin",
                            "request_id": request_id,
                            "total_len": len(payload),
                            "crc32": crc32_ieee(payload),
                            "sha256": payload_sha256,
                        },
                    ),
                    "ack",
                )
                began = True
                total_chunks = (len(payload) + chunk_bytes - 1) // chunk_bytes
                self._report(f"Enviando configuração em {total_chunks} bloco(s)…")
                for sequence, offset in enumerate(range(0, len(payload), chunk_bytes)):
                    self._report(
                        f"Enviando bloco {sequence + 1}/{total_chunks}…"
                    )
                    chunk = payload[offset : offset + chunk_bytes]
                    chunk_sha256 = hashlib.sha256(chunk).hexdigest()
                    response = self._expect(
                        exchange(
                            {
                                "type": "chunk",
                                "request_id": request_id,
                                "sequence": sequence,
                                "data": list(chunk),
                                "sha256": chunk_sha256,
                            },
                        ),
                        "ack",
                    )
                    if response.get("sequence") != sequence:
                        raise ProvisioningError("device acknowledged an unexpected chunk sequence")
                    if response.get("sha256") not in {None, chunk_sha256}:
                        raise ProvisioningError("device acknowledged a different chunk SHA-256")
                valid = self._expect(
                    exchange({"type": "validate", "request_id": request_id}),
                    "valid",
                )
                self._report("Configuração validada; confirmando gravação atômica…")
                committed = self._expect(
                    exchange({"type": "commit", "request_id": request_id}),
                    "committed",
                )
                config_hash = str(committed.get("config_hash") or "")
                if not config_hash or config_hash != valid.get("config_hash"):
                    raise ProvisioningError("validate/commit config hashes differ")
                if config_hash != expected_config_hash:
                    raise ProvisioningError("device config hash differs from the canonical selected profile")
                for response in (valid, committed):
                    if response.get("payload_sha256") not in {None, payload_sha256}:
                        raise ProvisioningError("device reported a different payload SHA-256")
                readback = self._expect(
                    exchange({"type": "readback", "config_hash": config_hash}),
                    "current_config",
                )
                if readback.get("config_hash") != config_hash:
                    raise ProvisioningError("readback did not confirm the committed config hash")
                if readback.get("payload_sha256") not in {None, payload_sha256}:
                    raise ProvisioningError("readback did not confirm the payload SHA-256")
            except Exception:
                if began:
                    try:
                        exchange({"type": "abort", "request_id": request_id})
                    except Exception:
                        pass
                raise
        self._report("Provisionamento confirmado por readback.")
        return ProvisionResult(
            request_id=request_id,
            config_hash=config_hash,
            generation=int(committed.get("generation") or 0),
            build_hash=str(hello.get("build_hash") or ""),
            device_id=hello.get("device_id"),
            payload_sha256=payload_sha256,
        )


def activate_provisioned_profile(
    port: str,
    profile: DeviceProfile,
    *,
    timeout: float = 30.0,
    client_factory: Callable[[str], ProvisionClient] = ProvisionClient,
    clock: Callable[[], float] = time.monotonic,
    sleeper: Callable[[float], None] = time.sleep,
    progress: Callable[[str], None] | None = None,
) -> SerialDeviceStatus:
    """Reboot after an atomic commit and prove the new runtime is active.

    NVS readback proves persistence, but GPIO/RMT/SPI managers are constructed
    during boot. Testing immediately after a commit can therefore exercise the
    previous runtime even though ``config_hash`` is already current. This
    helper closes that gap for CLI, TUI and batch flows.
    """

    if not math.isfinite(timeout) or timeout <= 0:
        raise ValueError("timeout must be a positive finite number")
    expected_build = compute_build_hash(profile)
    expected_config = compute_config_hash(profile)
    desired_features = {str(item).strip() for item in profile.target.features if str(item).strip()}
    before = client_factory(port).inspect_device(profile)
    if before.build_hash != expected_build or before.config_hash != expected_config:
        raise ProvisioningError(
            "cannot activate profile because the committed build/config hashes do not match"
        )
    if progress is not None:
        progress("Reiniciando a placa para ativar GPIO/RMT/SPI provisionados…")
    client_factory(port).commission_accessory(profile, "diagnostic.reboot", {})

    deadline = clock() + timeout
    last_error: Exception | None = None
    while clock() < deadline:
        sleeper(min(0.5, max(0.0, deadline - clock())))
        try:
            status = client_factory(port).inspect_device(profile)
        except (OSError, ProvisioningError) as exc:
            last_error = exc
            continue
        if status.boot_id == before.boot_id:
            continue
        if status.build_hash != expected_build or status.config_hash != expected_config:
            last_error = ProvisioningError("reboot readback returned divergent hashes")
            continue
        missing = desired_features.difference(status.features)
        if missing:
            last_error = ProvisioningError(
                "runtime did not activate configured capabilities: " + ", ".join(sorted(missing))
            )
            continue
        if progress is not None:
            progress("Novo runtime ativo e confirmado por capabilities/readback.")
        return status
    detail = f": {last_error}" if last_error is not None else ""
    raise ProvisioningTimeout(f"board did not activate the provisioned runtime after reboot{detail}")
