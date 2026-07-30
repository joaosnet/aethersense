import json
import hashlib

import pytest

from aethersense_build.models import new_device_profile
from aethersense_build.diffing import compute_build_hash, compute_config_hash
from aethersense_build.provisioning import (
    PROVISION_PROTOCOL,
    ProvisionClient,
    ProvisioningError,
    ProvisioningTimeout,
    SerialDeviceStatus,
    activate_provisioned_profile,
    cobs_decode,
    decode_frame,
    encode_frame,
)
from aethersense_build.secrets import MemorySecretStore, SecretManager


def test_frame_round_trip_and_crc_rejection() -> None:
    message = {"type": "chunk", "request_id": "r", "sequence": 0, "data": [0, 1, 2, 0]}
    encoded = encode_frame(message)
    assert encoded.endswith(b"\x00")
    assert decode_frame(encoded) == message
    raw = bytearray(cobs_decode(encoded))
    raw[2] ^= 0x01
    # Re-COBS through a valid frame body while preserving the old CRC.
    from aethersense_build.provisioning import cobs_encode

    with pytest.raises(ProvisioningError, match="CRC mismatch"):
        decode_frame(cobs_encode(bytes(raw)))


class FakeSerial:
    def __init__(self, *_args, **_kwargs):
        self.read_buffer = bytearray()
        self.requests = []
        self.payload = bytearray()
        self.profile = None
        self.reset_input_calls = 0
        self.accessory_results = {}

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        return False

    def flush(self):
        pass

    def reset_input_buffer(self):
        self.reset_input_calls += 1
        self.read_buffer.clear()

    def read(self, size):
        if not self.read_buffer:
            return b""
        value = bytes(self.read_buffer[:size])
        del self.read_buffer[:size]
        return value

    def write(self, frame):
        request = decode_frame(frame)
        self.requests.append(request)
        kind = request["type"]
        if kind == "hello":
            response = {
                "type": "hello",
                "protocol": PROVISION_PROTOCOL,
                "device_id": "esp32-aabbccddeeff",
                "firmware_version": "test",
                "build_hash": self.build_hash,
                "boot_id": "boot-local",
            }
        elif kind == "capabilities":
            response = {
                "type": "capabilities",
                "profile_schema": "aethersense.device_profile.v1",
                "max_chunk_bytes": 128,
                "atomic_slots": 2,
                "features": ["csi", "serial-commissioning-v1"],
            }
        elif kind == "current_config":
            response = {
                "type": "current_config",
                "profile": None,
                "config_hash": (
                    compute_config_hash(self.profile) if self.profile else self.config_hash
                ),
                "generation": 3,
            }
        elif kind == "begin":
            response = {"type": "ack", "request_id": request["request_id"], "sequence": None}
        elif kind == "chunk":
            self.payload.extend(request["data"])
            response = {"type": "ack", "request_id": request["request_id"], "sequence": request["sequence"]}
        elif kind == "validate":
            config_hash = compute_config_hash(self.profile) if self.profile else self.config_hash
            response = {"type": "valid", "request_id": request["request_id"], "config_hash": config_hash}
        elif kind == "commit":
            config_hash = compute_config_hash(self.profile) if self.profile else self.config_hash
            response = {
                "type": "committed",
                "request_id": request["request_id"],
                "config_hash": config_hash,
                "generation": 3,
            }
        elif kind == "readback":
            config_hash = compute_config_hash(self.profile) if self.profile else self.config_hash
            response = {
                "type": "current_config",
                "profile": None,
                "config_hash": config_hash,
                "generation": 3,
            }
        elif kind == "accessory_command":
            command = request["command"]
            request_id = command["request_id"]
            action = command["action"]
            command_params = command.get("params") or {}
            state = {"event": "ir.sent", "code_id": command_params.get("code_id")}
            if action == "ir.learn.start":
                state = {
                    "event": "ir.learned",
                    "code": {
                        "schema": "aethersense.ir_raw.v1",
                        "code_id": command_params.get("code_id") or request_id,
                        "carrier_hz": 38_000,
                        "duty_percent": 33,
                        "sequence_us": [9000, 4500, 560, 560],
                    },
                }
            elif action == "diagnostic.status_led":
                state = {
                    "event": "diagnostic.status_led",
                    "color": command_params["color"],
                    "duration_ms": command_params["duration_ms"],
                    "scheduled": True,
                    "hardware_observation_required": True,
                }
            elif action == "diagnostic.runtime":
                state = {
                    "event": "diagnostic.runtime",
                    "network_ready": True,
                    "csi_streaming": True,
                    "last_udp_send_secs": 12,
                    "dropped_packets": 0,
                    "transmitted_packets": 120,
                    "hardware_observation_required": False,
                }
            elif action == "storage.self_test":
                state = {
                    "event": "storage.self_test",
                    "storage_id": "sd",
                    "bytes_written": len(command_params["payload"].encode("utf-8")),
                    "bytes_read": len(command_params["payload"].encode("utf-8")),
                    "sha256": command_params["expected_sha256"],
                    "temporary_file_removed": True,
                }
            self.accessory_results[request_id] = {
                "schema": "aethersense.command_ack.v1",
                "request_id": request_id,
                "boot_id": "boot-local",
                "status": "completed",
                "state": state,
            }
            response = {
                "type": "accessory_result",
                "ack": {
                    "schema": "aethersense.command_ack.v1",
                    "request_id": request_id,
                    "boot_id": "boot-local",
                    "status": "acknowledged",
                },
            }
        elif kind == "accessory_result":
            response = {
                "type": "accessory_result",
                "ack": self.accessory_results[request["request_id"]],
            }
        else:
            response = {"type": "ack", "request_id": request.get("request_id", ""), "sequence": None}
        self.read_buffer.extend(encode_frame(response))
        return len(frame)


def test_full_provision_flow_chunks_secrets_and_confirms_readback() -> None:
    fake = FakeSerial()
    profile = new_device_profile("Lab", "esp32c5")
    fake.build_hash = compute_build_hash(profile)
    fake.config_hash = compute_config_hash(profile)
    fake.profile = profile
    manager = SecretManager(MemorySecretStore())
    profile.network.wifi.password_ref = manager.reference(profile.profile_id, "wifi-password")
    manager.set(profile.network.wifi.password_ref, "wifi-secret")
    result = ProvisionClient("COM1", serial_factory=lambda *_a, **_k: fake).provision(profile, manager)
    assert result.config_hash == compute_config_hash(profile)
    assert result.build_hash == compute_build_hash(profile)
    assert result.generation == 3
    assert result.device_id == "esp32-aabbccddeeff"
    assert profile.device_id == result.device_id
    assert profile.association.efuse_mac == "aa:bb:cc:dd:ee:ff"
    payload = json.loads(fake.payload.decode("utf-8"))
    assert payload["protocol"] == PROVISION_PROTOCOL
    assert payload["build_hash"] == compute_build_hash(profile)
    assert payload["profile"] == profile.to_firmware_dict()
    assert payload["secrets"]["wifi_password"] == "wifi-secret"
    revision = payload["profile"]["secret_revisions"]["wifi_password"]
    assert len(revision) == 32
    assert "wifi-secret" not in revision
    begin = next(item for item in fake.requests if item["type"] == "begin")
    assert begin["sha256"] == hashlib.sha256(bytes(fake.payload)).hexdigest()
    for chunk in (item for item in fake.requests if item["type"] == "chunk"):
        assert chunk["sha256"] == hashlib.sha256(bytes(chunk["data"])).hexdigest()
    assert [item["type"] for item in fake.requests[:4]] == [
        "hello",
        "capabilities",
        "current_config",
        "begin",
    ]


def test_direct_usb_ir_learn_and_replay_need_no_server_or_mqtt() -> None:
    fake = FakeSerial()
    profile = new_device_profile("IR USB", "esp32c5")
    profile.device_id = "esp32-aabbccddeeff"
    profile.association.efuse_mac = "aa:bb:cc:dd:ee:ff"
    profile.ir_rx.enabled = True
    profile.ir_rx.gpio = 8
    profile.ir_tx.enabled = True
    profile.ir_tx.gpio = 9
    fake.profile = profile
    fake.build_hash = compute_build_hash(profile)
    fake.config_hash = compute_config_hash(profile)
    client = ProvisionClient(
        "COM1",
        serial_settle=0,
        serial_factory=lambda *_a, **_k: fake,
    )

    learned = client.commission_accessory(
        profile,
        "ir.learn.start",
        {"code_id": "power", "timeout_ms": 1_000},
    )
    code = learned.state["code"]
    assert code["sequence_us"] == [9000, 4500, 560, 560]

    sent = client.commission_accessory(
        profile,
        "ir.send",
        {
            "code_id": code["code_id"],
            "carrier_hz": code["carrier_hz"],
            "duty_percent": code["duty_percent"],
            "sequence_us": code["sequence_us"],
            "repeats": 1,
        },
    )
    assert sent.state == {"event": "ir.sent", "code_id": "power"}
    assert [item["type"] for item in fake.requests].count("accessory_command") == 2


def test_accessory_recovers_recorded_ack_when_initial_usb_response_is_lost() -> None:
    class LostInitialAccessoryResponse(FakeSerial):
        def __init__(self):
            super().__init__()
            self.drop_once = True

        def write(self, frame):
            request = decode_frame(frame)
            written = super().write(frame)
            if request["type"] == "accessory_command" and self.drop_once:
                self.drop_once = False
                self.read_buffer.clear()
            return written

    fake = LostInitialAccessoryResponse()
    profile = new_device_profile("IR USB", "esp32c5")
    profile.device_id = "esp32-aabbccddeeff"
    profile.association.efuse_mac = "aa:bb:cc:dd:ee:ff"
    profile.ir_rx.enabled = True
    profile.ir_rx.gpio = 8
    profile.refresh_features()
    fake.profile = profile
    fake.build_hash = compute_build_hash(profile)
    fake.config_hash = compute_config_hash(profile)

    learned = ProvisionClient(
        "COM1",
        timeout=0.01,
        operation_timeout=2,
        serial_settle=0,
        hello_retry=0,
        serial_factory=lambda *_a, **_k: fake,
    ).commission_accessory(
        profile,
        "ir.learn.start",
        {"code_id": "power", "timeout_ms": 500},
    )

    assert learned.state["code"]["sequence_us"] == [9000, 4500, 560, 560]
    assert [item["type"] for item in fake.requests].count("accessory_command") == 1
    assert [item["type"] for item in fake.requests].count("accessory_result") >= 1


def test_live_status_exposes_firmware_and_boot_identity_and_diagnostics_bind_boot_id() -> None:
    fake = FakeSerial()
    profile = new_device_profile("Diagnóstico USB", "esp32c5")
    profile.device_id = "esp32-aabbccddeeff"
    profile.association.efuse_mac = "aa:bb:cc:dd:ee:ff"
    profile.runtime.status_led.enabled = True
    profile.runtime.status_led.data_gpio = 27
    profile.runtime.status_led_enabled = True
    profile.runtime.status_led_gpio = 27
    fake.profile = profile
    fake.build_hash = compute_build_hash(profile)
    fake.config_hash = compute_config_hash(profile)
    client = ProvisionClient("COM1", serial_settle=0, serial_factory=lambda *_a, **_k: fake)

    status = client.inspect_device(profile)
    assert status.firmware_version == "test"
    assert status.boot_id == "boot-local"

    result = client.commission_accessory(
        profile,
        "diagnostic.status_led",
        {"color": "ir_rx", "duration_ms": 500},
    )
    assert result.state["scheduled"] is True
    command = next(
        item["command"]
        for item in reversed(fake.requests)
        if item["type"] == "accessory_command"
    )
    assert command["boot_id"] == "boot-local"


def test_direct_usb_commissioning_requires_applied_pin_config() -> None:
    fake = FakeSerial()
    profile = new_device_profile("IR não aplicado", "esp32c5")
    profile.device_id = "esp32-aabbccddeeff"
    profile.ir_rx.enabled = True
    profile.ir_rx.gpio = 8
    fake.profile = None
    fake.build_hash = compute_build_hash(profile)
    fake.config_hash = "0" * 64

    with pytest.raises(ProvisioningError, match="configuração de pinos"):
        ProvisionClient(
            "COM1",
            serial_settle=0,
            serial_factory=lambda *_a, **_k: fake,
        ).commission_accessory(
            profile,
            "ir.learn.start",
            {"code_id": "power", "timeout_ms": 1_000},
        )
    assert "accessory_command" not in [item["type"] for item in fake.requests]


def test_direct_usb_ir_learning_can_be_cancelled_from_tui_worker() -> None:
    fake = FakeSerial()
    profile = new_device_profile("IR cancelar", "esp32c5")
    profile.device_id = "esp32-aabbccddeeff"
    profile.ir_rx.enabled = True
    profile.ir_rx.gpio = 8
    fake.profile = profile
    fake.build_hash = compute_build_hash(profile)
    fake.config_hash = compute_config_hash(profile)

    with pytest.raises(ProvisioningError, match="cancelada pelo usuário"):
        ProvisionClient(
            "COM1",
            serial_settle=0,
            serial_factory=lambda *_a, **_k: fake,
        ).commission_accessory(
            profile,
            "ir.learn.start",
            {"code_id": "cancel-me", "timeout_ms": 1_000},
            cancel_requested=lambda: True,
        )

    actions = [
        item["command"]["action"]
        for item in fake.requests
        if item["type"] == "accessory_command"
    ]
    assert actions == ["ir.learn.start", "ir.learn.cancel"]


def test_direct_usb_waits_for_enabled_accessory_runtime_to_start() -> None:
    clock = FakeClock()

    class RuntimeStartsLate(FakeSerial):
        unavailable_once = True

        def write(self, frame):
            request = decode_frame(frame)
            if request["type"] == "accessory_command" and self.unavailable_once:
                self.unavailable_once = False
                self.requests.append(request)
                command = request["command"]
                self.read_buffer.extend(
                    encode_frame(
                        {
                            "type": "accessory_result",
                            "ack": {
                                "schema": "aethersense.command_ack.v1",
                                "request_id": command["request_id"],
                                "boot_id": "boot-local",
                                "status": "error",
                                "error": "requested accessory is disabled or unavailable",
                            },
                        }
                    )
                )
                return len(frame)
            return super().write(frame)

    fake = RuntimeStartsLate()
    profile = new_device_profile("IR aguardando runtime", "esp32c5")
    profile.device_id = "esp32-aabbccddeeff"
    profile.ir_tx.enabled = True
    profile.ir_tx.gpio = 9
    fake.profile = profile
    fake.build_hash = compute_build_hash(profile)
    fake.config_hash = compute_config_hash(profile)

    result = ProvisionClient(
        "COM1",
        operation_timeout=5,
        serial_settle=0,
        serial_factory=lambda *_a, **_k: fake,
        clock=clock,
        sleeper=clock.advance,
    ).commission_accessory(
        profile,
        "ir.send",
        {
            "code_id": "power",
            "carrier_hz": 38_000,
            "duty_percent": 33,
            "sequence_us": [560, 560],
            "repeats": 1,
        },
    )

    assert result.status == "completed"
    assert clock.now == pytest.approx(0.6)
    assert [item["type"] for item in fake.requests].count("accessory_command") == 2


def test_read_only_preflight_retries_one_lost_current_config_response() -> None:
    class DropFirstCurrentConfig(FakeSerial):
        dropped = False

        def write(self, frame):
            request = decode_frame(frame)
            if request["type"] == "current_config" and not self.dropped:
                self.dropped = True
                self.requests.append(request)
                return len(frame)
            return super().write(frame)

    fake = DropFirstCurrentConfig()
    profile = new_device_profile("Lab retry", "esp32c5")
    fake.build_hash = compute_build_hash(profile)
    fake.config_hash = compute_config_hash(profile)
    fake.profile = profile
    progress: list[str] = []

    result = ProvisionClient(
        "COM1",
        timeout=0.1,
        operation_timeout=5,
        serial_settle=0,
        serial_factory=lambda *_a, **_k: fake,
        progress=progress.append,
    ).provision(profile, SecretManager(MemorySecretStore()))

    assert result.config_hash == compute_config_hash(profile)
    assert [request["type"] for request in fake.requests].count("current_config") == 2
    assert any("tentando novamente" in message for message in progress)


def test_read_only_preflight_consumes_a_late_duplicate_hello_before_capabilities() -> None:
    class DuplicateHello(FakeSerial):
        duplicated = False

        def write(self, frame):
            request = decode_frame(frame)
            written = super().write(frame)
            if request["type"] == "hello" and not self.duplicated:
                self.duplicated = True
                duplicate = {
                    "type": "hello",
                    "protocol": PROVISION_PROTOCOL,
                    "device_id": "esp32-aabbccddeeff",
                    "firmware_version": "test",
                    "build_hash": self.build_hash,
                    "boot_id": "boot-local",
                }
                self.read_buffer.extend(encode_frame(duplicate))
            return written

    fake = DuplicateHello()
    profile = new_device_profile("Hello lento", "esp32c5")
    profile.device_id = "esp32-aabbccddeeff"
    fake.profile = profile
    fake.build_hash = compute_build_hash(profile)
    fake.config_hash = compute_config_hash(profile)
    progress: list[str] = []

    status = ProvisionClient(
        "COM1",
        serial_settle=0,
        serial_factory=lambda *_a, **_k: fake,
        progress=progress.append,
    ).inspect_device(profile)

    assert status.device_id == profile.device_id
    assert any("hello duplicado" in item for item in progress)


def test_reader_ignores_leading_resync_delimiter() -> None:
    fake = FakeSerial()
    response = {"type": "hello", "protocol": PROVISION_PROTOCOL}
    fake.read_buffer.extend(b"\x00" + encode_frame(response))
    client = ProvisionClient("COM1", serial_factory=lambda *_a, **_k: fake)
    assert decode_frame(client._read_frame(fake)) == response


def test_hello_identity_mismatch_aborts_before_begin() -> None:
    fake = FakeSerial()
    profile = new_device_profile("Outra placa", "esp32c5")
    profile.device_id = "esp32-001122334455"
    profile.association.efuse_mac = "00:11:22:33:44:55"
    fake.build_hash = compute_build_hash(profile)
    manager = SecretManager(MemorySecretStore())
    with pytest.raises(ProvisioningError, match="eFuse identity"):
        ProvisionClient("COM1", serial_factory=lambda *_a, **_k: fake).provision(
            profile, manager
        )
    assert "begin" not in [item["type"] for item in fake.requests]


class FakeClock:
    def __init__(self) -> None:
        self.now = 0.0

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds


def test_reader_absolute_deadline_stops_continuous_serial_noise() -> None:
    clock = FakeClock()

    class ContinuousNoise:
        timeout = None
        reads = 0

        def read(self, _size):
            self.reads += 1
            clock.advance(0.1)
            return b"W"

    serial = ContinuousNoise()
    client = ProvisionClient(
        "COM1",
        timeout=0.5,
        operation_timeout=1.0,
        serial_settle=0,
        serial_factory=lambda *_a, **_k: serial,
        clock=clock,
        sleeper=clock.advance,
    )
    with pytest.raises(ProvisioningTimeout, match="provisioning response timed out"):
        client._read_frame(serial)
    assert serial.reads == 5


def test_hello_is_retried_when_opening_serial_drops_first_request() -> None:
    clock = FakeClock()

    class ResetThenReadySerial(FakeSerial):
        def __init__(self):
            super().__init__()
            self.hello_attempts = 0

        def write(self, frame):
            request = decode_frame(frame)
            if request["type"] == "hello":
                self.hello_attempts += 1
                if self.hello_attempts == 1:
                    self.requests.append(request)
                    return len(frame)
            return super().write(frame)

    fake = ResetThenReadySerial()
    profile = new_device_profile("Lab", "esp32c5")
    fake.build_hash = compute_build_hash(profile)
    fake.config_hash = compute_config_hash(profile)
    fake.profile = profile
    manager = SecretManager(MemorySecretStore())

    result = ProvisionClient(
        "COM1",
        timeout=0.5,
        operation_timeout=5.0,
        serial_settle=0.25,
        hello_retry=0.20,
        serial_factory=lambda *_a, **_k: fake,
        clock=clock,
        sleeper=clock.advance,
    ).provision(profile, manager)

    assert result.device_id == "esp32-aabbccddeeff"
    assert fake.hello_attempts == 2
    assert fake.reset_input_calls == 1
    assert clock.now == pytest.approx(0.45)


def test_hello_is_retried_after_ch340_invalid_cobs_fragment() -> None:
    clock = FakeClock()

    class InvalidFrameThenReadySerial(FakeSerial):
        def __init__(self):
            super().__init__()
            self.hello_attempts = 0

        def write(self, frame):
            request = decode_frame(frame)
            if request["type"] == "hello":
                self.hello_attempts += 1
                if self.hello_attempts == 1:
                    self.requests.append(request)
                    self.read_buffer.extend(
                        encode_frame(
                            {
                                "type": "error",
                                "code": "invalid_frame",
                                "message": "invalid COBS frame",
                            }
                        )
                    )
                    return len(frame)
            return super().write(frame)

    fake = InvalidFrameThenReadySerial()
    profile = new_device_profile("Lab", "esp32c5")
    fake.build_hash = compute_build_hash(profile)
    fake.config_hash = compute_config_hash(profile)
    fake.profile = profile

    status = ProvisionClient(
        "COM1",
        timeout=0.5,
        operation_timeout=5.0,
        serial_settle=0,
        hello_retry=0.20,
        serial_factory=lambda *_a, **_k: fake,
        clock=clock,
        sleeper=clock.advance,
    ).inspect_device(profile)

    assert status.device_id == "esp32-aabbccddeeff"
    assert fake.hello_attempts == 2
    assert clock.now == pytest.approx(0.20)


def test_activation_reboots_and_waits_for_configured_runtime_capabilities() -> None:
    clock = FakeClock()
    profile = new_device_profile("IR", "esp32c5")
    profile.device_id = "esp32-aabbccddeeff"
    profile.association.efuse_mac = "aa:bb:cc:dd:ee:ff"
    profile.ir_rx.enabled = True
    profile.ir_rx.gpio = 8
    profile.ir_tx.enabled = True
    profile.ir_tx.gpio = 9
    profile.refresh_features()
    expected_build = compute_build_hash(profile)
    expected_config = compute_config_hash(profile)

    class RebootingClient:
        def __init__(self) -> None:
            self.rebooted = False
            self.actions = []

        def inspect_device(self, _profile):
            return SerialDeviceStatus(
                device_id=profile.device_id,
                build_hash=expected_build,
                config_hash=expected_config,
                generation=2,
                features=("csi", "ir") if self.rebooted else ("csi",),
                firmware_version="test",
                boot_id="boot-new" if self.rebooted else "boot-old",
            )

        def commission_accessory(self, _profile, action, params):
            self.actions.append((action, params))
            self.rebooted = True
            return None

    client = RebootingClient()
    status = activate_provisioned_profile(
        "COM1",
        profile,
        timeout=5,
        client_factory=lambda _port: client,
        clock=clock,
        sleeper=clock.advance,
    )

    assert status.boot_id == "boot-new"
    assert set(profile.target.features).issubset(status.features)
    assert client.actions == [("diagnostic.reboot", {})]


def test_hello_retries_obey_total_operation_deadline() -> None:
    clock = FakeClock()

    class NeverReadySerial(FakeSerial):
        def write(self, frame):
            self.requests.append(decode_frame(frame))
            return len(frame)

    fake = NeverReadySerial()
    profile = new_device_profile("Lab", "esp32c5")
    manager = SecretManager(MemorySecretStore())
    client = ProvisionClient(
        "COM1",
        timeout=0.5,
        operation_timeout=1.0,
        serial_settle=0,
        hello_retry=0.25,
        serial_factory=lambda *_a, **_k: fake,
        clock=clock,
        sleeper=clock.advance,
    )

    with pytest.raises(ProvisioningTimeout, match="provisioning operation timed out"):
        client.provision(profile, manager)
    assert 1 < len(fake.requests) <= 5
