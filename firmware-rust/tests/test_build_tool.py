import build as build_tool
import io
import json
from pathlib import Path
from types import SimpleNamespace

import pytest

from aethersense_build.environment import build_environment
from aethersense_build.models import new_device_profile
from aethersense_build.provisioning import ProvisioningError, ProvisioningTimeout
from aethersense_build.secrets import MemorySecretStore, SecretManager


def test_canonical_sdkconfig_sizes_rust_main_task_stack() -> None:
    template = (Path(build_tool.__file__).parent / "sdkconfig.example").read_text(
        encoding="utf-8"
    )
    assert "CONFIG_ESP_MAIN_TASK_STACK_SIZE=32768" in template
    assert "CONFIG_FREERTOS_CHECK_STACKOVERFLOW_CANARY=y" in template


def test_host_test_action_overrides_embedded_default_target(monkeypatch) -> None:
    class Result:
        returncode = 0
        stdout = "rustc 1.0\nhost: x86_64-pc-windows-msvc\n"

    monkeypatch.setattr(build_tool.subprocess, "run", lambda *_args, **_kwargs: Result())
    assert build_tool.rust_host_target() == "x86_64-pc-windows-msvc"
    options = build_tool.BuildOptions(action="test", profile="classic")
    preview = build_tool.describe_planned_command(
        options, build_tool.BuildSettings(project_dir="C:\\repo")
    )
    assert "--target x86_64-pc-windows-msvc" in preview


def test_isolated_sandbox_is_under_afw_on_windows(monkeypatch) -> None:
    monkeypatch.setattr(build_tool.os, "name", "nt")
    settings = build_tool.BuildSettings(project_dir="C:\\repo")
    isolated = build_tool.isolated_build_settings(settings, "esp32c5")
    try:
        assert isolated.sandbox_dir.startswith("C:\\afw\\")
        assert isolated.sandbox_profile == "esp32c5"
        relative = Path(isolated.sandbox_dir).relative_to("C:\\afw")
        assert len(relative.parts[0]) == 6
        assert relative.parts[1] == "c5"
        assert "-" in relative.parts[2]
        assert len(isolated.sandbox_dir) <= 32
        assert isolated.target_dir.startswith("C:\\a\\")
        assert len(isolated.target_dir) <= 10
    finally:
        build_tool.remove_directory(isolated.target_dir)


def test_console_sink_redacts_bare_and_assigned_secrets() -> None:
    terminal = io.StringIO()
    log = io.StringIO()
    sink = build_tool.ConsoleLogSink(terminal, log)
    sink.add_secret_values(["senha-super-secreta"])
    sink.line("bare senha-super-secreta")
    sink.line("AETHERSENSE_WIFI_PASSWORD=senha-super-secreta")
    for output in (terminal.getvalue(), log.getvalue()):
        assert "senha-super-secreta" not in output
        assert "<redacted>" in output


def test_console_sink_keeps_log_when_terminal_is_already_closed() -> None:
    class ClosedTerminal:
        def write(self, _message):
            raise OSError(22, "closed")

        def flush(self):
            raise ValueError("closed")

    log = io.StringIO()
    sink = build_tool.ConsoleLogSink(ClosedTerminal(), log)
    sink.line("cleanup completed")
    assert "cleanup completed" in log.getvalue()


def test_cli_parser_supports_device_profile_services_without_secret_flags() -> None:
    parser = build_tool.build_parser()
    args = parser.parse_args(
        [
            "test-integration",
            "--device-profile",
            "profile-uuid",
            "--integration",
            "mqtt",
        ]
    )
    assert args.action == "test-integration"
    assert args.device_profile == "profile-uuid"
    assert args.integration == "mqtt"
    assert "password" not in parser.format_help().lower()


def test_load_device_profile_path_and_service_dispatch(tmp_path: Path, monkeypatch) -> None:
    profile = new_device_profile("CLI")
    source = tmp_path / "profile.json"
    source.write_text(json.dumps(profile.to_dict()), encoding="utf-8")
    loaded = build_tool.load_device_profile_argument(source.as_posix())
    assert loaded.profile_id == profile.profile_id

    class Session:
        bytes = 123

    monkeypatch.setattr(build_tool, "inspect_sd", lambda _source: [Session()])
    output = io.StringIO()
    options = build_tool.BuildOptions(action="inspect-sd", sd_path=str(tmp_path))
    assert (
        build_tool.execute_profile_service_action(
            options,
            build_tool.BuildSettings(project_dir=str(tmp_path)),
            build_tool.ConsoleLogSink(output, io.StringIO()),
        )
        == 0
    )
    assert '"bytes": 123' in output.getvalue()
    assert "[START]" in output.getvalue()
    assert "Action: inspect-sd" in output.getvalue()


def test_validate_hardware_cli_persists_blocked_physical_evidence(
    tmp_path: Path, monkeypatch
) -> None:
    profile = new_device_profile("Meu esp c5", "esp32c5")
    profile.device_id = "esp32-aabbccddeeff"
    profile.association.efuse_mac = "aa:bb:cc:dd:ee:ff"
    report_path = tmp_path / "resultados" / "firmware-hardware" / "session" / "session.json"
    report_path.parent.mkdir(parents=True)
    report_path.write_text('{"status":"blocked"}\n', encoding="utf-8")

    class Service:
        def __init__(self, **_kwargs):
            self.report_path = report_path

        def start(self, selected):
            assert selected is profile
            return SimpleNamespace(
                preflight=SimpleNamespace(
                    state=build_tool.ValidationState.BLOCKED,
                    reason="nenhuma ESP32 USB foi encontrada",
                ),
                checks={},
                physically_validated=False,
            )

    monkeypatch.setattr(build_tool, "HardwareValidationService", Service)
    monkeypatch.setattr(build_tool, "validate_profile", lambda _profile: [])
    output = io.StringIO()
    result = build_tool.execute_profile_service_action(
        build_tool.BuildOptions(action="validate-hardware", device_profile=profile),
        build_tool.BuildSettings(project_dir=str(tmp_path)),
        build_tool.ConsoleLogSink(output, io.StringIO()),
    )

    assert result == 1
    assert "[BLOCKED] Preflight" in output.getvalue()
    assert str(report_path) in output.getvalue()


def test_cli_provision_logs_start_progress_and_uses_total_timeout(monkeypatch) -> None:
    profile = new_device_profile("CLI", "esp32c5")
    captured = {}

    class Client:
        def __init__(self, port, **kwargs):
            captured["port"] = port
            captured["kwargs"] = kwargs

        def provision(self, selected_profile, _secret_manager):
            assert selected_profile is profile
            captured["kwargs"]["progress"]("Negociando protocolo com o firmware…")
            return SimpleNamespace(
                build_hash="build-hash",
                config_hash="config-hash",
                device_id="esp32-aabbccddeeff",
            )

    monkeypatch.setattr(build_tool, "ProvisionClient", Client)
    def activate(port, selected_profile, **kwargs):
        assert port == "COM9"
        assert selected_profile is profile
        captured["activation_timeout"] = kwargs["timeout"]
        kwargs["progress"]("Novo runtime ativo e confirmado por capabilities/readback.")

    monkeypatch.setattr(build_tool, "activate_provisioned_profile", activate)
    monkeypatch.setattr(build_tool, "validate_profile", lambda _profile: [])
    monkeypatch.setattr(build_tool, "resolve_monitor_port", lambda _root, port: port)
    output = io.StringIO()
    options = build_tool.BuildOptions(
        action="provision",
        port="COM9",
        timeout=7,
        device_profile=profile,
    )

    assert (
        build_tool.execute_profile_service_action(
            options,
            build_tool.BuildSettings(project_dir="C:\\repo"),
            build_tool.ConsoleLogSink(output, io.StringIO()),
        )
        == 0
    )
    assert captured["port"] == "COM9"
    assert captured["kwargs"]["operation_timeout"] == 7.0
    assert captured["activation_timeout"] == 7.0
    assert "[START]" in output.getvalue()
    assert "Action: provision" in output.getvalue()
    assert "[STEP] Negociando protocolo" in output.getvalue()
    assert "Novo runtime ativo" in output.getvalue()


@pytest.mark.parametrize(
    ("failure", "expected"),
    [
        (
            ProvisioningTimeout("provisioning operation timed out"),
            "Provisioning on COM9 timed out",
        ),
        (
            ProvisioningError("device rejected token=must-never-be-logged"),
            "Provisioning on COM9 failed",
        ),
    ],
)
def test_cli_provision_reports_protocol_failures_without_traceback_or_secrets(
    tmp_path: Path,
    monkeypatch,
    failure: Exception,
    expected: str,
) -> None:
    profile = new_device_profile("CLI", "esp32c5")

    class Client:
        def __init__(self, _port, **_kwargs):
            pass

        def provision(self, _profile, _secret_manager):
            raise failure

    terminal = io.StringIO()
    monkeypatch.setattr(build_tool, "ProvisionClient", Client)
    monkeypatch.setattr(build_tool, "validate_profile", lambda _profile: [])
    monkeypatch.setattr(build_tool, "resolve_monitor_port", lambda _root, port: port)
    monkeypatch.setattr(build_tool.sys, "__stdout__", terminal)
    options = build_tool.BuildOptions(
        action="provision",
        port="COM9",
        timeout=1,
        device_profile=profile,
    )

    assert build_tool.run_cli(
        options,
        build_tool.BuildSettings(project_dir=str(tmp_path)),
    ) == 1
    log_text = (tmp_path / "build.log").read_text(encoding="utf-8")
    combined = terminal.getvalue() + log_text
    assert expected in combined
    assert "[START]" in combined
    assert "must-never-be-logged" not in combined
    assert "Traceback" not in combined


def test_build_environment_never_contains_runtime_values_or_secrets() -> None:
    profile = new_device_profile("Seguro", "esp32c5")
    profile.network.wifi.ssid = "ssid-private"
    secrets = SecretManager(MemorySecretStore())
    profile.network.wifi.password_ref = secrets.reference(profile.profile_id, "wifi")
    secrets.set(profile.network.wifi.password_ref, "wifi-private")
    env = build_environment(profile, secrets)
    assert env == {
        "AETHERSENSE_HARDWARE_PROFILE": "esp32c5",
        "AETHERSENSE_BUILD_HASH": build_tool.compute_build_hash(profile),
    }
    assert "private" not in json.dumps(env)
