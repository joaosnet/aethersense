from pathlib import Path

import json

from aethersense_build.integrations import (
    LocalBrokerManager,
    get_commissioning_status,
    onboard_device,
    prepare_local_dashboard_profile,
    send_accessory_command,
    sync_ir_code,
    test_mqtt_connection as probe_mqtt,
)
from aethersense_build.models import RelayConfig, new_device_profile
from aethersense_build.secrets import MemorySecretStore, SecretManager


def test_ir_code_sync_is_idempotent_and_authenticated() -> None:
    secrets = SecretManager(MemorySecretStore())
    profile = new_device_profile("IR server", "esp32c5")
    profile.device_id = "esp32-aabbccddeeff"
    profile.server.base_url = "http://aether.local"
    profile.server.admin_bearer_token_ref = secrets.reference(profile.profile_id, "admin")
    secrets.set(profile.server.admin_bearer_token_ref, "admin-secret")
    stored: dict[str, object] = {}
    methods: list[str] = []

    class Response:
        def __init__(self, status: int, document: dict):
            self.status = status
            self.document = document

        def __enter__(self):
            return self

        def __exit__(self, *_args):
            return False

        def read(self):
            return json.dumps(self.document).encode("utf-8")

    def opener(request, timeout):
        assert timeout > 0
        assert request.headers["Authorization"] == "Bearer admin-secret"
        methods.append(request.method)
        if request.method == "GET":
            return Response(200, {"codes": [stored] if stored else []})
        payload = json.loads(request.data.decode("utf-8"))
        stored.clear()
        stored.update(payload)
        return Response(200 if request.method == "PUT" else 201, payload)

    code = {
        "id": "power",
        "device_id": profile.device_id,
        "name": "Ligar",
        "carrier_hz": 38_000,
        "duty_cycle": 0.33,
        "sequence_us": [9000, 4500, 560, 560],
    }
    assert sync_ir_code(profile, secrets, code, opener=opener).ok
    assert sync_ir_code(profile, secrets, code, opener=opener).ok
    assert methods == ["GET", "POST", "GET", "PUT"]


def test_local_broker_runtime_contains_hash_acl_and_no_plaintext_secret(
    tmp_path: Path, monkeypatch
) -> None:
    manager = SecretManager(MemorySecretStore())
    profile = new_device_profile("Sala")
    profile.device_id = "esp32-aabbccddeeff"
    profile.server.admin_bearer_token_ref = manager.reference(profile.profile_id, "admin")
    manager.set(profile.server.admin_bearer_token_ref, "admin-secret")
    broker = LocalBrokerManager(tmp_path / "mosquitto" / "runtime", manager)

    hashed_users = []

    def fake_hash(username: str, password: str) -> None:
        assert len(password) >= 32
        hashed_users.append(username)
        broker._atomic_write(broker.password_file_path, f"{username}:$7$fake-hash\n")

    monkeypatch.setattr(broker, "_update_password_hash", fake_hash)
    username, reference, fragment = broker.prepare_device(profile)
    secret = manager.get(reference)
    first_revision = profile.secret_revisions["mqtt_password"]
    assert username == profile.device_id
    assert fragment.exists()
    assert broker.managed_config_path.exists()
    assert broker.combined_acl_path.exists()
    assert broker.password_file_path.exists()
    disk_text = "\n".join(
        path.read_text(encoding="utf-8")
        for path in broker.runtime_dir.rglob("*")
        if path.is_file()
    )
    assert secret
    assert secret not in disk_text
    assert set(hashed_users) == {profile.device_id, "aethersense-server", "homeassistant"}
    assert f"aethersense/devices/{profile.device_id}/command" in disk_text
    assert f"aethersense/devices/{profile.device_id}/commissioning/#" in disk_text
    compose_env = broker.compose_environment(profile)
    assert compose_env["AETHERSENSE_MOSQUITTO_CONFIG"] == "./mosquitto/runtime/managed.conf"
    assert compose_env["AETHERSENSE_MQTT_USERNAME"] == "aethersense-server"
    assert compose_env["AETHERSENSE_MQTT_PASSWORD"] == manager.get(
        manager.reference("local-broker", "backend-password")
    )
    broker.prepare_device(profile)
    assert profile.secret_revisions["mqtt_password"] == first_revision
    broker.prepare_device(profile, rotate=True)
    assert profile.secret_revisions["mqtt_password"] != first_revision


def test_local_dashboard_defaults_are_automatic_secret_safe_and_non_destructive(
    monkeypatch,
) -> None:
    secrets = SecretManager(MemorySecretStore())
    profile = new_device_profile("Sala", "esp32c5")
    profile.device_id = "esp32-aabbccddeeff"
    profile.storage.enabled = True
    monkeypatch.setattr(
        "aethersense_build.integrations.shutil.which", lambda name: f"/{name}"
    )

    result = prepare_local_dashboard_profile(
        profile, secrets, lan_host="192.168.18.149"
    )

    assert result.ok
    assert profile.mqtt.enabled
    assert profile.mqtt.broker_mode == "local"
    assert profile.mqtt.broker_host == "192.168.18.149"
    assert profile.server.base_url == "http://192.168.18.149:8000"
    assert profile.sync.base_url == profile.server.base_url
    assert profile.server.admin_bearer_token_ref
    assert secrets.get(profile.server.admin_bearer_token_ref)
    assert secrets.get(profile.server.admin_bearer_token_ref) not in profile.canonical_json()

    profile.mqtt.broker_host = "broker.example"
    profile.mqtt.broker_mode = "external"
    profile.server.base_url = "https://server.example"
    preserved = prepare_local_dashboard_profile(
        profile, secrets, lan_host="192.168.18.150"
    )
    assert preserved.ok
    assert profile.mqtt.broker_host == "broker.example"
    assert profile.server.base_url == "https://server.example"


class _Reason:
    def __init__(self, failure: bool = False) -> None:
        self.is_failure = failure


class _PublishInfo:
    rc = 0

    def __init__(self, mid: int) -> None:
        self.mid = mid

    def wait_for_publish(self, timeout: float) -> None:
        assert timeout > 0


class _Message:
    def __init__(self, topic: str, payload: bytes) -> None:
        self.topic = topic
        self.payload = payload


class _FakeMqttClient:
    def __init__(self) -> None:
        self.mid = 0
        self.password = ""
        self.username = ""
        self.connected = None

    def username_pw_set(self, username: str, password: str) -> None:
        self.username = username
        self.password = password

    def tls_set(self) -> None:
        self.tls = True

    def connect(self, host: str, port: int, keepalive: int) -> None:
        self.connected = (host, port, keepalive)

    def loop_start(self) -> None:
        self.on_connect(self, None, None, _Reason(False), None)

    def subscribe(self, topic: str, qos: int):
        self.mid += 1
        mid = self.mid
        denied = "/acl-probe/" in topic
        self.on_subscribe(self, None, mid, [_Reason(denied)], None)
        return 0, mid

    def publish(self, topic: str, payload: bytes, qos: int, retain: bool):
        self.mid += 1
        info = _PublishInfo(self.mid)
        denied = "/acl-probe/" in topic
        self.on_publish(self, None, info.mid, _Reason(denied), None)
        if not denied:
            self.on_message(self, None, _Message(topic, payload))
        return info

    def unsubscribe(self, _topic: str) -> None:
        pass

    def disconnect(self) -> None:
        pass

    def loop_stop(self) -> None:
        pass


def test_mqtt_probe_validates_auth_roundtrip_qos1_and_negative_acl() -> None:
    secrets = SecretManager(MemorySecretStore())
    profile = new_device_profile("MQTT")
    profile.device_id = "esp32-aabbccddeeff"
    profile.mqtt.enabled = True
    profile.mqtt.broker_host = "broker.local"
    profile.mqtt.username = profile.device_id
    profile.mqtt.password_ref = secrets.reference(profile.profile_id, "mqtt-password")
    secrets.set(profile.mqtt.password_ref, "mqtt-secret-not-for-logs")
    client = _FakeMqttClient()

    result = probe_mqtt(
        profile,
        secrets,
        client_factory=lambda _client_id: client,
        timeout=0.2,
    )

    assert result.ok, result.message
    assert "QoS 1" in result.message
    assert "mqtt-secret-not-for-logs" not in result.message
    assert client.connected == ("broker.local", 1883, 15)
    assert client.password == "mqtt-secret-not-for-logs"


def test_mqtt_probe_rejects_overbroad_acl() -> None:
    class OverbroadClient(_FakeMqttClient):
        def subscribe(self, topic: str, qos: int):
            self.mid += 1
            mid = self.mid
            self.on_subscribe(self, None, mid, [_Reason(False)], None)
            return 0, mid

        def publish(self, topic: str, payload: bytes, qos: int, retain: bool):
            self.mid += 1
            info = _PublishInfo(self.mid)
            self.on_publish(self, None, info.mid, _Reason(False), None)
            self.on_message(self, None, _Message(topic, payload))
            return info

    secrets = SecretManager(MemorySecretStore())
    profile = new_device_profile("MQTT")
    profile.device_id = "esp32-aabbccddeeff"
    profile.mqtt.enabled = True
    profile.mqtt.broker_host = "broker.local"
    profile.mqtt.username = profile.device_id
    profile.mqtt.password_ref = secrets.reference(profile.profile_id, "mqtt-password")
    secrets.set(profile.mqtt.password_ref, "secret")

    result = probe_mqtt(
        profile,
        secrets,
        client_factory=lambda _client_id: OverbroadClient(),
        timeout=0.2,
    )
    assert not result.ok
    assert "ACL excessiva" in result.message


def test_onboarding_uses_admin_bearer_and_stores_individual_device_token() -> None:
    secrets = SecretManager(MemorySecretStore())
    profile = new_device_profile("Sala")
    profile.device_id = "esp32-aabbccddeeff"
    profile.server.base_url = "http://aethersense.local"
    profile.server.admin_bearer_token_ref = secrets.reference(
        profile.profile_id, "server-admin-bearer"
    )
    secrets.set(profile.server.admin_bearer_token_ref, "admin-only-secret")
    profile.relays.append(RelayConfig(
        relay_id="luz", name="Luz", enabled=True, gpio=18, pulse_limit_ms=5000
    ))
    captured = {}

    class Response:
        status = 200

        def __enter__(self):
            return self

        def __exit__(self, *_args):
            return False

        def read(self):
            return b"{}"

    def opener(request, timeout: float):
        captured["url"] = request.full_url
        captured["authorization"] = request.get_header("Authorization")
        captured["payload"] = json.loads(request.data)
        captured["timeout"] = timeout
        return Response()

    result = onboard_device(profile, secrets, opener=opener)

    assert result.ok, result.message
    assert captured["url"].endswith("/api/accessories/onboard")
    assert captured["authorization"] == "Bearer admin-only-secret"
    assert captured["payload"]["device_id"] == profile.device_id
    assert captured["payload"]["accessories"][0]["kind"] == "relay"
    assert captured["payload"]["device_token"] != "admin-only-secret"
    assert profile.server.bearer_token_ref == result.device_token_ref
    assert secrets.get(result.device_token_ref) == captured["payload"]["device_token"]
    assert captured["payload"]["device_token"] not in result.message
    revision = profile.secret_revisions["server_bearer_token"]
    assert len(revision) == 32
    assert captured["payload"]["device_token"] not in revision


def test_local_broker_service_passes_secrets_only_in_process_environment(
    tmp_path: Path, monkeypatch
) -> None:
    secrets = SecretManager(MemorySecretStore())
    profile = new_device_profile("Sala")
    profile.device_id = "esp32-aabbccddeeff"
    profile.server.admin_bearer_token_ref = secrets.reference(profile.profile_id, "admin")
    secrets.set(profile.server.admin_bearer_token_ref, "admin-secret")
    broker = LocalBrokerManager(tmp_path / "mosquitto" / "runtime", secrets)
    secrets.set(broker._backend_password_reference(), "backend-secret")
    broker._atomic_write(broker.managed_config_path, "listener 1883\n")
    compose = tmp_path / "docker-compose.yml"
    compose.write_text("services: {}\n", encoding="utf-8")
    monkeypatch.setattr("aethersense_build.integrations.shutil.which", lambda name: f"/{name}")
    captured = {}

    class Completed:
        returncode = 0

    def runner(command, **kwargs):
        captured["command"] = command
        captured["env"] = kwargs["env"]
        return Completed()

    result = broker.start_or_reload_service(profile, compose, runner=runner)
    assert result.ok
    command_text = " ".join(captured["command"])
    assert "backend-secret" not in command_text
    assert "admin-secret" not in command_text
    assert captured["env"]["AETHERSENSE_MQTT_PASSWORD"] == "backend-secret"
    assert captured["env"]["AETHERSENSE_API_TOKEN"] == "admin-secret"
    assert "--build" in captured["command"]
    assert captured["command"][-3:] == ["mosquitto", "backend", "frontend"]


def test_commissioning_status_and_relay_pulse_enforce_profile_limit() -> None:
    secrets = SecretManager(MemorySecretStore())
    profile = new_device_profile("Comissionamento")
    profile.device_id = "esp32-aabbccddeeff"
    profile.association.efuse_mac = "aa:bb:cc:dd:ee:ff"
    profile.server.base_url = "http://server.local"
    profile.server.admin_bearer_token_ref = secrets.reference(profile.profile_id, "admin")
    secrets.set(profile.server.admin_bearer_token_ref, "admin-secret")
    profile.relays.append(
        RelayConfig(
            relay_id="luz", name="Luz", enabled=True, gpio=18, pulse_limit_ms=1000
        )
    )
    requests = []

    class Response:
        status = 200

        def __init__(self, body):
            self.body = body

        def __enter__(self):
            return self

        def __exit__(self, *_args):
            return False

        def read(self):
            return json.dumps(self.body).encode()

    def opener(request, timeout):
        requests.append(request)
        if request.method == "GET" and request.full_url.endswith("/api/accessories"):
            return Response(
                {
                    "devices": [
                        {"device_id": profile.device_id, "available": True}
                    ],
                    "accessories": [
                        {
                            "id": "luz",
                            "device_id": profile.device_id,
                            "kind": "relay",
                            "available": True,
                        }
                    ],
                }
            )
        if request.method == "POST":
            command = json.loads(request.data)
            return Response(
                {
                    "ok": True,
                    "command": {
                        "request_id": command["request_id"],
                        "status": "pending",
                    },
                }
            )
        request_id = request.full_url.rsplit("/", 1)[-1]
        return Response(
            {
                "ok": True,
                "command": {"request_id": request_id, "status": "completed"},
            }
        )

    status = get_commissioning_status(profile, secrets, opener=opener)
    assert status.ok
    denied = send_accessory_command(
        profile,
        secrets,
        "relay.pulse",
        {"relay_id": "luz", "duration_ms": 1001},
        opener=opener,
    )
    assert not denied.ok
    assert len(requests) == 1

    accepted = send_accessory_command(
        profile,
        secrets,
        "relay.pulse",
        {"relay_id": "luz", "duration_ms": 500},
        opener=opener,
    )
    assert accepted.ok
    post_request = next(request for request in requests if request.method == "POST")
    payload = json.loads(post_request.data)
    assert payload["action"] == "relay.pulse"
    assert payload["params"]["duration_ms"] == 500
    assert any("/api/accessories/commands/" in request.full_url for request in requests)
    assert "admin-secret" not in accepted.message


def _commissioning_profile():
    secrets = SecretManager(MemorySecretStore())
    profile = new_device_profile("Comissionamento")
    profile.device_id = "esp32-aabbccddeeff"
    profile.server.base_url = "http://server.local"
    profile.server.admin_bearer_token_ref = secrets.reference(profile.profile_id, "admin")
    secrets.set(profile.server.admin_bearer_token_ref, "admin-secret")
    return profile, secrets


class _FakeClock:
    def __init__(self) -> None:
        self.value = 0.0

    def __call__(self) -> float:
        return self.value

    def sleep(self, seconds: float) -> None:
        self.value += seconds


def test_commissioning_reports_esp_error_instead_of_http_acceptance() -> None:
    profile, secrets = _commissioning_profile()
    profile.relays = [RelayConfig(relay_id="luz", enabled=True, gpio=18)]
    command_id = ""

    class Response:
        status = 202

        def __init__(self, body):
            self.body = body

        def __enter__(self):
            return self

        def __exit__(self, *_args):
            return False

        def read(self):
            return json.dumps(self.body).encode()

    def opener(request, timeout):
        nonlocal command_id
        if request.method == "POST":
            command_id = json.loads(request.data)["request_id"]
            return Response({"command": {"request_id": command_id, "status": "pending"}})
        return Response(
            {
                "command": {
                    "request_id": command_id,
                    "status": "failed",
                    "error": "relay interlock",
                }
            }
        )

    result = send_accessory_command(
        profile,
        secrets,
        "relay.pulse",
        {"relay_id": "luz", "duration_ms": 100},
        opener=opener,
        sleeper=lambda _seconds: None,
    )
    assert not result.ok
    assert "relay interlock" in result.message
    assert "admin-secret" not in result.message


def test_commissioning_times_out_when_esp_never_acknowledges() -> None:
    profile, secrets = _commissioning_profile()
    profile.storage.enabled = True
    clock = _FakeClock()
    command_id = ""

    class Response:
        status = 202

        def __init__(self, body):
            self.body = body

        def __enter__(self):
            return self

        def __exit__(self, *_args):
            return False

        def read(self):
            return json.dumps(self.body).encode()

    def opener(request, timeout):
        nonlocal command_id
        if request.method == "POST":
            command_id = json.loads(request.data)["request_id"]
        return Response({"command": {"request_id": command_id, "status": "pending"}})

    result = send_accessory_command(
        profile,
        secrets,
        "storage.sync",
        {},
        opener=opener,
        poll_interval=2,
        monotonic=clock,
        sleeper=clock.sleep,
    )
    assert not result.ok
    assert "sem ACK" in result.message
    assert clock.value >= 17


def test_ir_learning_waits_from_started_until_learned() -> None:
    profile, secrets = _commissioning_profile()
    command_id = ""
    polls = 0

    class Response:
        status = 202

        def __init__(self, body):
            self.body = body

        def __enter__(self):
            return self

        def __exit__(self, *_args):
            return False

        def read(self):
            return json.dumps(self.body).encode()

    def opener(request, timeout):
        nonlocal command_id, polls
        if request.method == "POST":
            command_id = json.loads(request.data)["request_id"]
            return Response({"request_id": command_id, "status": "started"})
        polls += 1
        return Response(
            {
                "command": {
                    "request_id": command_id,
                    "status": "pending" if polls == 1 else "learned",
                }
            }
        )

    result = send_accessory_command(
        profile,
        secrets,
        "ir.learn.start",
        {"code_id": "commission-test", "timeout_ms": 1000},
        opener=opener,
        sleeper=lambda _seconds: None,
    )
    assert result.ok
    assert "aprendizado concluído" in result.message
    assert polls == 2
