import json
from datetime import datetime, timezone
from pathlib import Path
from types import SimpleNamespace

import pytest

from aethersense_build.devices import BoardIdentity
from aethersense_build.diffing import compute_build_hash, compute_config_hash
from aethersense_build.hardware_validation import (
    AutomaticTestResult,
    HARDWARE_VALIDATION_SCHEMA,
    HardwareValidationError,
    HardwareValidationService,
    ValidationState,
)
from aethersense_build.models import new_device_profile
from aethersense_build.provisioning import AccessoryCommissionResult, SerialDeviceStatus


MAC = "aa:bb:cc:dd:ee:ff"
DEVICE_ID = "esp32-aabbccddeeff"
NOW = datetime(2026, 7, 17, 12, 30, tzinfo=timezone.utc)


def c5_profile():
    profile = new_device_profile("Meu esp c5", "esp32c5")
    profile.device_id = DEVICE_ID
    profile.association.efuse_mac = MAC
    profile.association.vid = "1A86"
    profile.association.pid = "7523"
    profile.association.serial_number = "USB-C5"
    profile.csi.enabled = False
    profile.refresh_features()
    return profile


def usb_port(device="COM9", *, serial="USB-C5"):
    return {
        "device": device,
        "description": "USB-SERIAL CH340",
        "manufacturer": "wch.cn",
        "hwid": "USB VID:PID=1A86:7523",
        "vid": "1A86",
        "pid": "7523",
        "serial_number": serial,
    }


def bluetooth_port():
    return {
        "device": "COM3",
        "description": "Serial padrão por link Bluetooth",
        "manufacturer": "Microsoft",
        "hwid": "BTHENUM\\Dev_001122",
    }


class PortRig:
    def __init__(self, ports):
        self.ports = list(ports)
        self.probed = []
        self.identity_by_port = {}

    def port_provider(self):
        return list(self.ports)

    def board_probe(self, port):
        self.probed.append(port)
        return self.identity_by_port.get(
            port,
            BoardIdentity(port, MAC, DEVICE_ID, "esp32c5", "esp32c5"),
        )


class FakeClient:
    def __init__(self, port, *, features, build_hash=None, config_hash=None, device_id=DEVICE_ID):
        self.port = port
        self.features = tuple(features)
        self.build_hash = build_hash
        self.config_hash = config_hash
        self.device_id = device_id
        self.commands = []
        self.runtime_tick = 20

    def inspect_device(self, profile):
        return SerialDeviceStatus(
            device_id=self.device_id,
            build_hash=self.build_hash if self.build_hash is not None else compute_build_hash(profile),
            config_hash=(
                self.config_hash
                if self.config_hash is not None
                else compute_config_hash(profile)
            ),
            generation=7,
            features=self.features,
        )

    def commission_accessory(self, profile, action, params=None, **_kwargs):
        params = dict(params or {})
        self.commands.append((action, params))
        state = {"event": "ok"}
        if action == "ir.learn.start":
            state = {
                "event": "ir.learned",
                "code": {
                    "schema": "aethersense.ir_raw.v1",
                    "code_id": params["code_id"],
                    "carrier_hz": 38_000,
                    "duty_percent": 33,
                    "sequence_us": [9000, 4500, 560, 560],
                },
            }
        elif action == "storage.self_test":
            state = {
                "event": "storage.self_test",
                "bytes_written": len(params["payload"].encode("utf-8")),
                "bytes_read": len(params["payload"].encode("utf-8")),
                "sha256": params["expected_sha256"],
                "temporary_file_removed": True,
            }
        elif action == "diagnostic.runtime":
            self.runtime_tick += 1
            state = {
                "event": "diagnostic.runtime",
                "network_ready": True,
                "csi_streaming": True,
                "last_udp_send_secs": self.runtime_tick,
                "dropped_packets": 0,
                "transmitted_packets": self.runtime_tick * 10,
                "hardware_observation_required": False,
            }
        return AccessoryCommissionResult(
            request_id=f"request-{len(self.commands)}",
            action=action,
            status="completed",
            state=state,
        )


def service_for(tmp_path, profile, rig, *, features, testers=None, client_options=None, secrets=()):
    clients = []
    options = dict(client_options or {})

    def client_factory(port):
        client = FakeClient(port, features=features, **options)
        clients.append(client)
        return client

    service = HardwareValidationService(
        results_root=tmp_path / "resultados",
        port_provider=rig.port_provider,
        board_probe=rig.board_probe,
        client_factory=client_factory,
        testers=testers,
        now=lambda: NOW,
        secret_values=secrets,
    )
    return service, clients


def test_states_are_the_public_machine_contract():
    assert {state.value for state in ValidationState} == {
        "waiting",
        "running",
        "manual_confirmation",
        "passed",
        "failed",
        "blocked",
        "skipped",
    }


def test_preflight_ignores_bluetooth_and_re_resolves_changed_com_port(tmp_path: Path):
    profile = c5_profile()
    profile.runtime.status_led.enabled = True
    profile.runtime.status_led.data_gpio = 27
    profile.runtime.status_led_enabled = True
    profile.runtime.status_led_gpio = 27
    profile.refresh_features()
    rig = PortRig([bluetooth_port(), usb_port("COM9")])
    observed_ports = []

    def rgb_tester(context, _check, _params):
        observed_ports.append(context.port)
        return AutomaticTestResult(
            True,
            {"firmware_ack": True, "colors": ["red", "green", "blue"]},
            source="usb_led_diagnostic",
        )

    service, _clients = service_for(
        tmp_path,
        profile,
        rig,
        features=("status_led", "serial-commissioning-v1"),
        testers={"rgb": rgb_tester},
    )
    session = service.start(profile)

    assert session.preflight.state == ValidationState.PASSED
    assert session.checks["boot_serial"].state == ValidationState.PASSED
    assert "COM3" not in rig.probed
    assert session.hardware["port"] == "COM9"

    rig.ports = [bluetooth_port(), usb_port("COM11")]
    check = service.run_check("rgb")
    assert observed_ports == ["COM11"]
    assert check.state == ValidationState.MANUAL_CONFIRMATION
    assert check.automatic_evidence["passed"] is True
    assert check.manual_confirmation is None

    service.confirm_manual("rgb", True, "vi as três cores na placa")
    assert session.overall_state == ValidationState.PASSED
    assert not session.physically_validated
    assert session.evidence_mode == "simulated"
    assert session.hardware["port"] == "COM11"


def test_hash_mismatch_fails_preflight_and_blocks_accessories(tmp_path: Path):
    profile = c5_profile()
    profile.ir_rx.enabled = True
    profile.ir_rx.gpio = 8
    profile.ir_tx.enabled = True
    profile.ir_tx.gpio = 9
    profile.refresh_features()
    rig = PortRig([usb_port()])
    service, _clients = service_for(
        tmp_path,
        profile,
        rig,
        features=("ir", "serial-commissioning-v1"),
        client_options={"build_hash": "wrong-build"},
    )

    session = service.start(profile)

    assert session.preflight.state == ValidationState.FAILED
    assert session.checks["boot_serial"].state == ValidationState.FAILED
    assert session.checks["ir_rx"].state == ValidationState.BLOCKED
    assert not session.physically_validated
    assert "build_hash" in session.preflight.reason


def test_identity_mismatch_is_a_failed_preflight_not_a_pass(tmp_path: Path):
    profile = c5_profile()
    rig = PortRig([usb_port()])
    rig.identity_by_port["COM9"] = BoardIdentity(
        "COM9", "11:22:33:44:55:66", "esp32-112233445566", "esp32c5", "esp32c5"
    )
    service, _clients = service_for(
        tmp_path,
        profile,
        rig,
        features=("serial-commissioning-v1",),
    )

    session = service.start(profile)

    assert session.preflight.state == ValidationState.FAILED
    assert session.checks["boot_serial"].state == ValidationState.FAILED
    assert "eFuse MAC diferente" in session.preflight.reason


def test_only_bluetooth_ports_leave_physical_validation_blocked(tmp_path: Path):
    profile = c5_profile()
    rig = PortRig([bluetooth_port()])
    service, _clients = service_for(
        tmp_path,
        profile,
        rig,
        features=("serial-commissioning-v1",),
    )

    session = service.start(profile)

    assert session.preflight.state == ValidationState.BLOCKED
    assert rig.probed == []
    assert "cabo de dados" in session.preflight.reason


def test_missing_capability_blocks_each_enabled_resource(tmp_path: Path):
    profile = c5_profile()
    profile.ir_rx.enabled = True
    profile.ir_rx.gpio = 8
    profile.ir_tx.enabled = True
    profile.ir_tx.gpio = 9
    profile.refresh_features()
    rig = PortRig([usb_port()])
    service, _clients = service_for(
        tmp_path,
        profile,
        rig,
        features=("serial-commissioning-v1",),
    )

    session = service.start(profile)

    assert session.preflight.state == ValidationState.PASSED
    assert session.checks["ir_rx"].state == ValidationState.BLOCKED
    assert session.checks["ir_tx"].state == ValidationState.BLOCKED
    assert "ir" in session.checks["ir_rx"].reason


def test_safe_reboot_reconciles_capabilities_from_newly_active_profile(
    tmp_path: Path, monkeypatch
):
    profile = c5_profile()
    profile.runtime.status_led.enabled = True
    profile.runtime.status_led.data_gpio = 27
    profile.runtime.status_led_enabled = True
    profile.runtime.status_led_gpio = 27
    profile.refresh_features()
    rig = PortRig([usb_port()])
    clients = []

    class RebootClient(FakeClient):
        def __init__(self, port, features, boot_id):
            super().__init__(port, features=features)
            self.boot_id = boot_id

        def inspect_device(self, profile):
            status = super().inspect_device(profile)
            return SerialDeviceStatus(
                device_id=status.device_id,
                build_hash=status.build_hash,
                config_hash=status.config_hash,
                generation=status.generation,
                features=status.features,
                firmware_version="0.1.0",
                boot_id=self.boot_id,
            )

        def commission_accessory(self, profile, action, params=None, **kwargs):
            if action == "diagnostic.reboot":
                self.commands.append((action, dict(params or {})))
                return AccessoryCommissionResult(
                    request_id="reboot-request",
                    action=action,
                    status="completed",
                    state={"scheduled": True},
                )
            return super().commission_accessory(profile, action, params, **kwargs)

    def client_factory(port):
        index = len(clients)
        features = (
            "csi",
            "safe-reboot-diagnostic-v1",
            "serial-commissioning-v1",
            "serial-hardware-validation-v1",
        )
        if index >= 2:
            features = features + ("status_led",)
        client = RebootClient(port, features, "boot-before" if index < 2 else "boot-after")
        clients.append(client)
        return client

    service = HardwareValidationService(
        results_root=tmp_path / "resultados",
        port_provider=rig.port_provider,
        board_probe=rig.board_probe,
        client_factory=client_factory,
        now=lambda: NOW,
    )
    # The injected boundaries keep normal tests simulation-only.  This test
    # exercises the production-only reboot path without claiming real evidence.
    service.evidence_mode = "physical"
    monkeypatch.setattr("aethersense_build.hardware_validation.time.sleep", lambda _seconds: None)

    session = service.start(profile)
    assert session.checks["rgb"].state == ValidationState.BLOCKED

    boot = service.run_check("boot_serial")

    assert boot.state == ValidationState.PASSED
    assert session.hardware["boot_id"] == "boot-after"
    assert "status_led" in session.hardware["capabilities"]
    assert session.checks["rgb"].state == ValidationState.WAITING


def test_enabled_storage_passes_only_with_write_hash_readback_and_cleanup(tmp_path: Path):
    profile = c5_profile()
    profile.storage.enabled = True
    profile.storage.sclk_gpio = 0
    profile.storage.mosi_gpio = 1
    profile.storage.miso_gpio = 23
    profile.storage.cs_gpio = 24
    profile.refresh_features()
    rig = PortRig([usb_port()])
    service, _clients = service_for(
        tmp_path,
        profile,
        rig,
        features=("sd", "serial-commissioning-v1"),
    )
    session = service.start(profile)

    assert session.checks["storage"].state == ValidationState.WAITING
    check = service.run_check("storage")

    assert check.state == ValidationState.PASSED
    assert check.automatic_evidence is not None
    assert check.automatic_evidence["passed"] is True
    assert check.automatic_evidence["details"]["temporary_file_removed"] is True
    # Injected USB boundaries are simulation-only even with a complete readback.
    assert not session.physically_validated


def test_manual_confirmation_is_separate_and_report_is_atomic_and_redacted(tmp_path: Path):
    profile = c5_profile()
    profile.runtime.status_led.enabled = True
    profile.runtime.status_led.data_gpio = 27
    profile.runtime.status_led_enabled = True
    profile.runtime.status_led_gpio = 27
    profile.refresh_features()
    rig = PortRig([usb_port()])

    def rgb_tester(_context, _check, _params):
        return AutomaticTestResult(
            True,
            {"password": "super-secret", "diagnostic_log": "BEARER=super-secret"},
            source="usb_led_diagnostic",
        )

    service, _clients = service_for(
        tmp_path,
        profile,
        rig,
        features=("status_led", "serial-commissioning-v1"),
        testers={"rgb": rgb_tester},
        secrets=("super-secret",),
    )
    session = service.start(profile)
    check = service.run_check("rgb")

    assert check.state == ValidationState.MANUAL_CONFIRMATION
    assert check.automatic_evidence is not None
    assert check.manual_confirmation is None
    service.confirm_manual("rgb", True, "token=super-secret; confirmei no LED")

    assert service.report_path is not None
    document_text = service.report_path.read_text(encoding="utf-8")
    document = json.loads(document_text)
    assert document["schema"] == HARDWARE_VALIDATION_SCHEMA
    assert document["status"] == "passed"
    assert document["evidence_mode"] == "simulated"
    assert document["physically_validated"] is False
    assert "super-secret" not in document_text
    assert "<redacted>" in document_text
    assert not list(service.report_path.parent.glob(".session.*.tmp"))
    rgb = next(item for item in document["tests"] if item["id"] == "rgb")
    assert rgb["authorization_required"] is False
    assert rgb["automatic_evidence"] is not None
    assert rgb["manual_confirmation"]["confirmed"] is True
    assert session.completed_at is not None


def test_builtin_ir_capture_and_replay_need_real_automatic_evidence_then_manual_confirmation(
    tmp_path: Path,
):
    profile = c5_profile()
    profile.ir_rx.enabled = True
    profile.ir_rx.gpio = 8
    profile.ir_tx.enabled = True
    profile.ir_tx.gpio = 9
    profile.refresh_features()
    rig = PortRig([usb_port()])
    service, clients = service_for(
        tmp_path,
        profile,
        rig,
        features=("ir", "serial-commissioning-v1"),
    )
    session = service.start(profile)

    rx = service.run_check("ir_rx", {"timeout_ms": 1000})
    assert rx.state == ValidationState.MANUAL_CONFIRMATION
    assert rx.automatic_evidence["details"]["duration_count"] == 4
    assert service.artifact("ir_rx")["sequence_us"] == [9000, 4500, 560, 560]
    service.confirm_manual("ir_rx", True, "LED amarelo observado")

    tx = service.run_check("ir_tx")
    assert tx.state == ValidationState.MANUAL_CONFIRMATION
    assert tx.automatic_evidence["details"]["ack_status"] == "completed"
    service.confirm_manual("ir_tx", True, "LED magenta e aparelho respondeu")

    assert session.overall_state == ValidationState.PASSED
    assert not session.physically_validated
    commands = [command for client in clients for command in client.commands]
    assert [item[0] for item in commands] == ["ir.learn.start", "ir.send"]
    report_text = service.report_path.read_text(encoding="utf-8")
    assert "sequence_us" not in report_text


def test_csi_requires_empty_room_authorization_before_automatic_probe(tmp_path: Path):
    profile = c5_profile()
    profile.csi.enabled = True
    profile.refresh_features()
    rig = PortRig([usb_port()])
    calls = []

    def csi_tester(context, _check, _params):
        calls.append(context.port)
        return {"passed": True, "evidence": {"frames": 30, "watchdog": False}}

    service, _clients = service_for(
        tmp_path,
        profile,
        rig,
        features=("csi", "runtime-diagnostic-v1", "serial-commissioning-v1"),
        testers={"csi": csi_tester},
    )
    session = service.start(profile)
    assert session.checks["csi"].state == ValidationState.MANUAL_CONFIRMATION

    with pytest.raises(HardwareValidationError, match="autorização"):
        service.run_check("csi")
    assert calls == []

    service.authorize("csi", True, "sala vazia confirmada")
    result = service.run_check("csi")
    assert result.state == ValidationState.PASSED
    assert result.manual_authorization.confirmed is True
    assert result.automatic_evidence["passed"] is True
    assert session.overall_state == ValidationState.PASSED
    assert not session.physically_validated
    report = json.loads(service.report_path.read_text(encoding="utf-8"))
    csi_report = next(item for item in report["tests"] if item["id"] == "csi")
    assert csi_report["authorization_required"] is True
    assert csi_report["manual_authorization"]["confirmed"] is True


def test_builtin_csi_probe_uses_only_firmware_counters_after_empty_room_authorization(
    tmp_path: Path,
):
    profile = c5_profile()
    profile.csi.enabled = True
    profile.refresh_features()
    rig = PortRig([usb_port()])
    service, _clients = service_for(
        tmp_path,
        profile,
        rig,
        features=("csi", "runtime-diagnostic-v1", "serial-commissioning-v1"),
    )
    session = service.start(profile)
    service.authorize("csi", True, "sala vazia")

    result = service.run_check("csi", {"observation_seconds": 1.0})

    assert result.state == ValidationState.PASSED
    details = result.automatic_evidence["details"]
    assert details["last_udp_send_after_secs"] > details["last_udp_send_before_secs"]
    assert details["raw_csi_captured_by_host"] is False
    assert session.overall_state == ValidationState.PASSED
    assert not session.physically_validated
