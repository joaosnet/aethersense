"""Commissioning helpers for MQTT/server integration.

No plaintext secret is written to disk or passed on a command line.  The local
broker manager keeps the device password in the OS keyring and gives Mosquitto
only a salted password hash plus the least-privilege ACL.
"""

from __future__ import annotations

from dataclasses import dataclass
import json
import os
from pathlib import Path
import secrets as secure_random
import shutil
import socket
import subprocess
import tempfile
import threading
import time
from typing import Any, Callable
from uuid import uuid4
from urllib.error import HTTPError, URLError
from urllib.parse import quote
from urllib.request import Request, urlopen

from .models import DeviceProfile
from .secrets import SecretManager, synchronise_secret_revisions


@dataclass(frozen=True)
class IntegrationResult:
    ok: bool
    message: str


@dataclass(frozen=True)
class OnboardingResult:
    ok: bool
    message: str
    device_token_ref: str = ""


@dataclass(frozen=True)
class CommissioningStatus:
    ok: bool
    message: str
    device: dict[str, Any] | None = None
    accessories: tuple[dict[str, Any], ...] = ()


def detect_lan_ipv4() -> str | None:
    """Return the host IPv4 selected by the OS for normal LAN traffic."""

    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
            # UDP connect only consults the routing table; it sends no packet.
            probe.connect(("8.8.8.8", 80))
            host = str(probe.getsockname()[0]).strip()
    except OSError:
        return None
    return host if host and not host.startswith("127.") else None


def prepare_local_dashboard_profile(
    profile: DeviceProfile,
    secret_manager: SecretManager,
    *,
    lan_host: str | None = None,
    server_port: int = 8000,
) -> IntegrationResult:
    """Apply local-dashboard defaults without overriding explicit integrations.

    The generated administrative Bearer remains in the OS keyring.  Only its
    opaque reference is retained in the device profile.
    """

    managed_reference = secret_manager.reference(
        profile.profile_id, "server-admin-bearer"
    )
    managed_local = (
        profile.mqtt.broker_mode == "local"
        and bool(profile.mqtt.broker_host)
        and profile.server.admin_bearer_token_ref == managed_reference
    )
    if (profile.mqtt.enabled or profile.server.base_url) and not managed_local:
        return IntegrationResult(
            True,
            "Integrações já configuradas; mantive broker e servidor existentes",
        )
    if not shutil.which("docker"):
        return IntegrationResult(
            False,
            "Docker não está disponível para iniciar o dashboard local automaticamente",
        )
    host = (lan_host or detect_lan_ipv4() or "").strip()
    if not host or host.startswith("127."):
        return IntegrationResult(
            False,
            "Não encontrei um endereço de rede local acessível pela placa",
        )
    if not 1 <= int(server_port) <= 65_535:
        return IntegrationResult(False, "Porta do servidor local inválida")

    admin_reference = profile.server.admin_bearer_token_ref or managed_reference
    if not secret_manager.get(admin_reference):
        secret_manager.set(admin_reference, secure_random.token_urlsafe(32))

    # Per-device MQTT credentials can only be generated after eFuse pairing.
    # The pairing continuation calls this helper again and completes the flag.
    profile.mqtt.enabled = bool(profile.device_id)
    profile.mqtt.broker_mode = "local"
    profile.mqtt.broker_host = host
    profile.mqtt.broker_port = 1883
    profile.mqtt.tls = False
    profile.server.base_url = f"http://{host}:{int(server_port)}"
    profile.server.admin_bearer_token_ref = admin_reference
    if profile.storage.enabled:
        profile.sync.base_url = profile.server.base_url
        profile.sync.network_default = True
    profile.refresh_features()
    return IntegrationResult(
        True,
        "Dashboard local, MQTT e sincronização preparados automaticamente",
    )


class LocalBrokerManager:
    BACKEND_USERNAME = "aethersense-server"
    HOME_ASSISTANT_USERNAME = "homeassistant"

    def __init__(self, runtime_dir: Path | str, secret_manager: SecretManager) -> None:
        self.runtime_dir = Path(runtime_dir)
        self.secret_manager = secret_manager

    def prepare_device(
        self, profile: DeviceProfile, *, rotate: bool = False
    ) -> tuple[str, str, Path]:
        if not profile.device_id:
            raise ValueError("pareie a placa antes de gerar a credencial MQTT")
        username = profile.device_id
        reference = self.secret_manager.reference(profile.profile_id, "mqtt-password")
        password = self.secret_manager.get(reference)
        if rotate or not password:
            password = secure_random.token_urlsafe(32)
            profile.secret_revisions["mqtt_password"] = (
                self.secret_manager.set_with_revision(
                    reference,
                    password,
                    profile.secret_revisions.get("mqtt_password", ""),
                )
            )
        profile.mqtt.username = username
        profile.mqtt.password_ref = reference
        profile.mqtt.enabled = True
        profile.mqtt.broker_mode = "local"
        synchronise_secret_revisions(profile, self.secret_manager)

        device_dir = self.runtime_dir / "devices" / profile.device_id
        device_dir.mkdir(parents=True, exist_ok=True)
        acl_path = device_dir / "mosquitto.acl"
        topic = f"aethersense/devices/{profile.device_id}"
        acl = "\n".join(
            [
                f"user {username}",
                f"topic read {topic}/command",
                f"topic write {topic}/ack",
                f"topic write {topic}/event",
                f"topic write {topic}/state",
                f"topic write {topic}/capabilities",
                f"topic write {topic}/availability",
                # A random leaf below this namespace is used only while the
                # TUI commissions the credential.  It provides a QoS-1
                # subscribe/publish roundtrip without broadening operational
                # command/state permissions.
                f"topic readwrite {topic}/commissioning/#",
                "",
            ]
        )
        self._atomic_write(acl_path, acl)
        request = {
            "schema": "aethersense.mqtt_credential_request.v1",
            "device_id": profile.device_id,
            "username": username,
            "password_ref": reference,
            "acl_file": str(acl_path),
            "status": "broker-ready",
        }
        self._atomic_write(
            device_dir / "credential-request.json",
            json.dumps(request, indent=2, sort_keys=True) + "\n",
        )
        self._write_managed_config()
        self._write_combined_acl()
        self._update_password_hash(username, password)
        self._ensure_service_credentials()
        return username, reference, acl_path

    @property
    def managed_config_path(self) -> Path:
        return self.runtime_dir / "managed.conf"

    @property
    def password_file_path(self) -> Path:
        return self.runtime_dir / "passwords"

    @property
    def combined_acl_path(self) -> Path:
        return self.runtime_dir / "acl"

    def compose_environment(self, profile: DeviceProfile) -> dict[str, str]:
        """Return process-only compose values; callers must never persist them."""

        password = self.secret_manager.get(self._backend_password_reference())
        if not password:
            raise RuntimeError("credencial MQTT do servidor ausente; prepare o broker local")
        api_token = self.secret_manager.get(profile.server.admin_bearer_token_ref)
        if not api_token:
            raise RuntimeError(
                "token Bearer administrativo ausente; informe-o na TUI antes de iniciar o stack"
            )
        try:
            config = self.managed_config_path.resolve().relative_to(
                self.runtime_dir.parent.parent.resolve()
            )
            config_value = f"./{config.as_posix()}"
        except ValueError:
            config_value = str(self.managed_config_path.resolve())
        return {
            "AETHERSENSE_MOSQUITTO_CONFIG": config_value,
            "AETHERSENSE_MQTT_USERNAME": self.BACKEND_USERNAME,
            "AETHERSENSE_MQTT_PASSWORD": password,
            "AETHERSENSE_API_TOKEN": api_token,
        }

    def start_or_reload_service(
        self,
        profile: DeviceProfile,
        compose_file: Path | str,
        *,
        runner: Callable[..., Any] | None = None,
        timeout: float = 120.0,
    ) -> IntegrationResult:
        """Start/recreate the managed broker and backend without a .env file.

        Secret values are supplied only through the child process environment;
        neither the command line nor generated files contain plaintext values.
        """

        compose_file = Path(compose_file).resolve()
        if not compose_file.is_file():
            return IntegrationResult(False, f"Compose não encontrado: {compose_file}")
        docker = shutil.which("docker")
        if not docker:
            return IntegrationResult(False, "Docker não encontrado para iniciar o broker local")
        env = os.environ.copy()
        env.update(self.compose_environment(profile))
        command = [
            docker,
            "compose",
            "-f",
            str(compose_file),
            "up",
            "-d",
            "--build",
            "--force-recreate",
            "mosquitto",
            "backend",
            "frontend",
        ]
        run = runner or subprocess.run
        try:
            completed = run(
                command,
                cwd=compose_file.parent,
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                check=False,
                timeout=timeout,
            )
        except (OSError, subprocess.SubprocessError) as exc:
            return IntegrationResult(False, f"Falha ao iniciar broker local: {exc}")
        if completed.returncode != 0:
            return IntegrationResult(
                False,
                f"Docker Compose falhou (exit {completed.returncode}); consulte os logs do Docker",
            )
        return IntegrationResult(True, "Broker MQTT e backend iniciados/recarregados via Docker Compose")

    def _ensure_service_credentials(self) -> None:
        for username, reference in (
            (self.BACKEND_USERNAME, self._backend_password_reference()),
            (self.HOME_ASSISTANT_USERNAME, self._home_assistant_password_reference()),
        ):
            password = self.secret_manager.get(reference)
            if not password:
                password = secure_random.token_urlsafe(32)
                self.secret_manager.set(reference, password)
            self._update_password_hash(username, password)

    def _backend_password_reference(self) -> str:
        return self.secret_manager.reference("local-broker", "backend-password")

    def _home_assistant_password_reference(self) -> str:
        return self.secret_manager.reference("local-broker", "homeassistant-password")

    def _write_managed_config(self) -> None:
        config = "\n".join(
            [
                "persistence true",
                "persistence_location /mosquitto/data/",
                "log_dest stdout",
                "listener 1883 0.0.0.0",
                "allow_anonymous false",
                "password_file /mosquitto/security/passwords",
                "acl_file /mosquitto/security/acl",
                "",
            ]
        )
        self._atomic_write(self.managed_config_path, config)

    def _write_combined_acl(self) -> None:
        fragments: list[str] = []
        devices_dir = self.runtime_dir / "devices"
        if devices_dir.exists():
            for path in sorted(devices_dir.glob("*/mosquitto.acl")):
                fragments.append(path.read_text(encoding="utf-8").strip())
        service_acl = "\n".join(
            [
                "# Gerado pela TUI AetherSense. Não contém credenciais.",
                "user aethersense-server",
                "topic readwrite aethersense/#",
                "topic readwrite homeassistant/#",
                "",
                "user homeassistant",
                "topic readwrite aethersense/#",
                "topic readwrite homeassistant/#",
            ]
        )
        content = "\n\n".join([service_acl, *fragments]).rstrip() + "\n"
        self._atomic_write(self.combined_acl_path, content)

    def _update_password_hash(self, username: str, password: str) -> None:
        """Hash through mosquitto_passwd while keeping plaintext off argv/disk."""

        self.runtime_dir.mkdir(parents=True, exist_ok=True)
        fd, temporary_name = tempfile.mkstemp(
            prefix=".passwords-", dir=self.runtime_dir
        )
        os.close(fd)
        temporary = Path(temporary_name)
        try:
            if self.password_file_path.exists():
                shutil.copyfile(self.password_file_path, temporary)
            else:
                temporary.unlink(missing_ok=True)
            command = self._password_command(temporary, username)
            completed = subprocess.run(
                command,
                input=f"{password}\n{password}\n",
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
                timeout=20,
            )
            if completed.returncode != 0:
                raise RuntimeError(
                    "não foi possível gerar o hash do Mosquitto; "
                    "instale mosquitto_passwd ou mantenha Docker disponível"
                )
            os.replace(temporary, self.password_file_path)
        except (OSError, subprocess.SubprocessError) as exc:
            raise RuntimeError(
                "falha ao preparar o arquivo de senhas do broker local"
            ) from exc
        finally:
            temporary.unlink(missing_ok=True)

    def _password_command(self, password_path: Path, username: str) -> list[str]:
        create = not password_path.exists()
        host_tool = shutil.which("mosquitto_passwd")
        flags = ["-H", "sha512-pbkdf2"]
        if create:
            flags.append("-c")
        if host_tool:
            return [host_tool, *flags, str(password_path), username]

        docker = shutil.which("docker")
        if docker:
            relative = password_path.resolve().relative_to(self.runtime_dir.resolve())
            return [
                docker,
                "run",
                "--rm",
                "-i",
                "-v",
                f"{self.runtime_dir.resolve()}:/work",
                "eclipse-mosquitto:2",
                "mosquitto_passwd",
                *flags,
                f"/work/{relative.as_posix()}",
                username,
            ]
        raise RuntimeError(
            "mosquitto_passwd não encontrado e Docker não está disponível"
        )

    @staticmethod
    def _atomic_write(path: Path, content: str) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        fd, temporary_name = tempfile.mkstemp(
            prefix=f".{path.name}-", dir=path.parent
        )
        temporary = Path(temporary_name)
        try:
            with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as handle:
                handle.write(content)
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(temporary, path)
        finally:
            temporary.unlink(missing_ok=True)


def _default_mqtt_client(client_id: str):
    import paho.mqtt.client as mqtt

    kwargs: dict[str, Any] = {"client_id": client_id, "protocol": mqtt.MQTTv5}
    if hasattr(mqtt, "CallbackAPIVersion"):
        kwargs["callback_api_version"] = mqtt.CallbackAPIVersion.VERSION2
    return mqtt.Client(**kwargs)


def _reason_failed(reason: Any) -> bool:
    if reason is None:
        return False
    marker = getattr(reason, "is_failure", None)
    if marker is not None:
        return bool(marker)
    try:
        return int(reason) >= 128
    except (TypeError, ValueError):
        return False


def _wait_for(predicate: Callable[[], bool], event: threading.Event, timeout: float) -> bool:
    deadline = time.monotonic() + timeout
    while not predicate():
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return False
        event.wait(min(remaining, 0.1))
        event.clear()
    return True


def test_mqtt_connection(
    profile: DeviceProfile,
    secret_manager: SecretManager,
    *,
    timeout: float = 5.0,
    client_factory: Callable[[str], Any] | None = None,
) -> IntegrationResult:
    """Validate MQTT v5 auth, QoS-1 pub/sub and least-privilege ACL.

    The password is read directly from the secret backend and supplied only to
    the MQTT client's in-memory authentication API.  It is never interpolated
    into a command, error or log message.
    """

    if not profile.mqtt.enabled:
        return IntegrationResult(False, "MQTT está desabilitado")
    if not profile.mqtt.broker_host:
        return IntegrationResult(False, "Host MQTT não configurado")
    if not profile.device_id:
        return IntegrationResult(False, "Pareie o dispositivo antes de testar MQTT")
    if not profile.mqtt.username:
        return IntegrationResult(False, "Usuário MQTT não configurado")
    password = secret_manager.get(profile.mqtt.password_ref)
    if not password:
        return IntegrationResult(False, "Credencial MQTT não encontrada no keyring")

    nonce = uuid4().hex
    namespace = f"{profile.mqtt.base_topic.rstrip('/')}/{profile.device_id}"
    probe_topic = f"{namespace}/commissioning/{nonce}"
    forbidden_topic = f"aethersense/acl-probe/{nonce}"
    payload = f"aethersense-mqtt-probe:{nonce}".encode("ascii")
    connected = threading.Event()
    callback_event = threading.Event()
    message_received = threading.Event()
    connect_error: list[str] = []
    subacks: dict[int, list[Any]] = {}
    pubacks: dict[int, Any] = {}

    def on_connect(_client, _userdata, _flags, reason_code, _properties=None) -> None:
        if _reason_failed(reason_code):
            connect_error.append("autenticação MQTT recusada")
        connected.set()

    def on_subscribe(_client, _userdata, mid, reason_codes, _properties=None) -> None:
        if not isinstance(reason_codes, (list, tuple)):
            reason_codes = [reason_codes]
        subacks[int(mid)] = list(reason_codes)
        callback_event.set()

    def on_publish(_client, _userdata, mid, reason_code=None, _properties=None) -> None:
        pubacks[int(mid)] = reason_code
        callback_event.set()

    def on_message(_client, _userdata, message) -> None:
        if message.topic == probe_topic and bytes(message.payload) == payload:
            message_received.set()

    factory = client_factory or _default_mqtt_client
    client = None
    try:
        client = factory(f"aethersense-tui-{nonce[:12]}")
        client.on_connect = on_connect
        client.on_subscribe = on_subscribe
        client.on_publish = on_publish
        client.on_message = on_message
        client.username_pw_set(profile.mqtt.username, password)
        if profile.mqtt.tls:
            client.tls_set()
        client.connect(profile.mqtt.broker_host, profile.mqtt.broker_port, keepalive=15)
        client.loop_start()
        if not connected.wait(timeout):
            return IntegrationResult(False, "Timeout aguardando autenticação MQTT")
        if connect_error:
            return IntegrationResult(False, connect_error[0])

        rc, positive_mid = client.subscribe(probe_topic, qos=1)
        if int(rc) != 0:
            return IntegrationResult(False, "Broker recusou a inscrição de comissionamento")
        if not _wait_for(lambda: int(positive_mid) in subacks, callback_event, timeout):
            return IntegrationResult(False, "Timeout aguardando SUBACK MQTT")
        if any(_reason_failed(reason) for reason in subacks[int(positive_mid)]):
            return IntegrationResult(False, "ACL recusou o tópico de comissionamento do dispositivo")

        publish_info = client.publish(probe_topic, payload, qos=1, retain=False)
        if int(getattr(publish_info, "rc", 0)) != 0:
            return IntegrationResult(False, "Falha ao publicar teste MQTT QoS 1")
        publish_info.wait_for_publish(timeout=timeout)
        if not _wait_for(
            lambda: int(publish_info.mid) in pubacks, callback_event, timeout
        ):
            return IntegrationResult(False, "Timeout aguardando PUBACK MQTT QoS 1")
        if _reason_failed(pubacks[int(publish_info.mid)]):
            return IntegrationResult(False, "Broker recusou a publicação MQTT de comissionamento")
        if not message_received.wait(timeout):
            return IntegrationResult(False, "Publish/subscribe MQTT não completou o roundtrip")

        # A credential scoped to one device must not use an unrelated root.
        # Some brokers intentionally return a successful SUBACK for a denied
        # filter and silently suppress matching messages.  In that case prove
        # isolation through a QoS-1 write, whose PUBACK carries Not Authorized.
        rc, forbidden_mid = client.subscribe(forbidden_topic, qos=1)
        if int(rc) != 0:
            acl_rejected = True
        elif not _wait_for(lambda: int(forbidden_mid) in subacks, callback_event, timeout):
            return IntegrationResult(False, "Broker não confirmou a verificação negativa de ACL")
        else:
            acl_rejected = any(
                _reason_failed(reason) for reason in subacks[int(forbidden_mid)]
            )
        if not acl_rejected:
            forbidden_info = client.publish(
                forbidden_topic, payload, qos=1, retain=False
            )
            if int(getattr(forbidden_info, "rc", 0)) != 0:
                acl_rejected = True
            else:
                forbidden_info.wait_for_publish(timeout=timeout)
                if not _wait_for(
                    lambda: int(forbidden_info.mid) in pubacks,
                    callback_event,
                    timeout,
                ):
                    return IntegrationResult(
                        False,
                        "Broker não confirmou a verificação negativa de ACL",
                    )
                acl_rejected = _reason_failed(pubacks[int(forbidden_info.mid)])
        if not acl_rejected:
            return IntegrationResult(
                False,
                "ACL excessiva: a credencial publica fora do namespace do dispositivo",
            )
        return IntegrationResult(
            True,
            "MQTT validado: autenticação, QoS 1, publish/subscribe e isolamento de ACL",
        )
    except Exception as exc:
        # Library/network errors are safe to show; the credential was never
        # included in the exception context or function arguments rendered.
        return IntegrationResult(False, f"Falha no teste MQTT: {exc}")
    finally:
        if client is not None:
            try:
                client.unsubscribe(probe_topic)
                client.unsubscribe(forbidden_topic)
            except Exception:
                pass
            try:
                client.disconnect()
            except Exception:
                pass
            try:
                client.loop_stop()
            except Exception:
                pass


def profile_onboarding_document(profile: DeviceProfile) -> dict[str, Any]:
    """Create the server onboarding document from the canonical profile."""

    accessories: list[dict[str, Any]] = []
    for relay in sorted(profile.relays, key=lambda item: item.relay_id):
        if relay.enabled:
            accessories.append(
                {
                    "id": relay.relay_id,
                    "kind": "relay",
                    "name": relay.name,
                    "capabilities": {
                        "pulse": True,
                        "pulse_limit_ms": relay.pulse_limit_ms,
                    },
                }
            )
    if profile.ir_tx.enabled:
        accessories.append(
            {
                "id": "ir-tx",
                "kind": "ir_transmitter",
                "name": "Transmissor IR",
                "capabilities": {
                    "raw_replay": True,
                    "carrier_hz": profile.ir_tx.carrier_hz,
                    "duty_percent": profile.ir_tx.duty_percent,
                },
            }
        )
    if profile.ir_rx.enabled:
        accessories.append(
            {
                "id": "ir-rx",
                "kind": "ir_receiver",
                "name": "Receptor IR",
                "capabilities": {"raw_learning": True},
            }
        )
    if profile.storage.enabled:
        accessories.append(
            {
                "id": "sd",
                "kind": "storage",
                "name": "microSD",
                "capabilities": {
                    "events": profile.recording.events_enabled,
                    "raw_csi": profile.recording.raw_csi_enabled,
                    "network_sync": bool(profile.sync.base_url),
                },
            }
        )
    return {
        "device_id": profile.device_id,
        "name": profile.friendly_name,
        "capabilities": {
            "schema": "aethersense.capabilities.v1",
            "target": profile.target.profile,
            "feature_set": sorted(set(profile.target.features)),
            "command_qos": 1,
            "raw_ir": profile.ir_rx.enabled or profile.ir_tx.enabled,
        },
        "accessories": accessories,
    }


def onboard_device(
    profile: DeviceProfile,
    secret_manager: SecretManager,
    *,
    timeout: float = 5.0,
    opener: Callable[..., Any] | None = None,
    rotate_device_token: bool = False,
) -> OnboardingResult:
    """Register/update one device and atomically retain its individual token."""

    if not profile.device_id:
        return OnboardingResult(False, "Pareie o dispositivo antes do onboarding")
    if not profile.server.base_url:
        return OnboardingResult(False, "URL do servidor não configurada")
    admin_token = secret_manager.get(profile.server.admin_bearer_token_ref)
    if not admin_token:
        return OnboardingResult(False, "Bearer administrativo não encontrado no keyring")

    reference = profile.server.bearer_token_ref or secret_manager.reference(
        profile.profile_id, "server-device-bearer"
    )
    previous = secret_manager.get(reference)
    device_token = (
        secure_random.token_urlsafe(32)
        if rotate_device_token or not previous
        else previous
    )
    document = profile_onboarding_document(profile)
    document["device_token"] = device_token
    request = Request(
        f"{profile.server.base_url.rstrip('/')}/api/accessories/onboard",
        data=json.dumps(document, separators=(",", ":"), sort_keys=True).encode("utf-8"),
        headers={
            "Accept": "application/json",
            "Content-Type": "application/json",
            "Authorization": f"Bearer {admin_token}",
        },
        method="POST",
    )
    open_request = opener or urlopen
    try:
        with open_request(request, timeout=timeout) as response:  # noqa: S310 - configured LAN URL
            status = int(response.status)
            response.read()
    except HTTPError as exc:
        return OnboardingResult(False, f"Onboarding recusado pelo servidor (HTTP {exc.code})")
    except (URLError, OSError) as exc:
        return OnboardingResult(False, f"Servidor inacessível durante onboarding: {exc}")
    if not 200 <= status < 300:
        return OnboardingResult(False, f"Onboarding respondeu HTTP {status}")

    # Store/update the generated token only after the server has committed it.
    profile.secret_revisions["server_bearer_token"] = (
        secret_manager.set_with_revision(
            reference,
            device_token,
            profile.secret_revisions.get("server_bearer_token", ""),
        )
    )
    profile.server.bearer_token_ref = reference
    synchronise_secret_revisions(profile, secret_manager)
    return OnboardingResult(
        True,
        "Dispositivo registrado; token individual armazenado no keyring",
        reference,
    )


def _admin_headers(profile: DeviceProfile, secret_manager: SecretManager) -> dict[str, str]:
    token = secret_manager.get(profile.server.admin_bearer_token_ref)
    if not token:
        raise ValueError("Bearer administrativo não encontrado no keyring")
    return {
        "Accept": "application/json",
        "Content-Type": "application/json",
        "Authorization": f"Bearer {token}",
    }


def get_commissioning_status(
    profile: DeviceProfile,
    secret_manager: SecretManager,
    *,
    timeout: float = 5.0,
    opener: Callable[..., Any] | None = None,
) -> CommissioningStatus:
    if not profile.device_id or not profile.server.base_url:
        return CommissioningStatus(False, "Pareie a placa e configure o servidor")
    try:
        headers = _admin_headers(profile, secret_manager)
    except ValueError as exc:
        return CommissioningStatus(False, str(exc))
    request = Request(
        f"{profile.server.base_url.rstrip('/')}/api/accessories",
        headers=headers,
        method="GET",
    )
    try:
        with (opener or urlopen)(request, timeout=timeout) as response:  # noqa: S310
            data = json.loads(response.read().decode("utf-8"))
    except HTTPError as exc:
        return CommissioningStatus(False, f"Status respondeu HTTP {exc.code}")
    except (URLError, OSError, ValueError, json.JSONDecodeError) as exc:
        return CommissioningStatus(False, f"Falha ao ler status/capabilities: {exc}")
    devices = data.get("devices", []) if isinstance(data, dict) else []
    device = next(
        (
            item
            for item in devices
            if isinstance(item, dict) and item.get("device_id") == profile.device_id
        ),
        None,
    )
    accessories = tuple(
        item
        for item in data.get("accessories", [])
        if isinstance(item, dict) and item.get("device_id") == profile.device_id
    )
    if device is None:
        return CommissioningStatus(False, "Dispositivo não encontrado no servidor")
    available = bool(device.get("available"))
    kinds = ", ".join(sorted({str(item.get("kind")) for item in accessories})) or "nenhum"
    return CommissioningStatus(
        available,
        f"{'Online' if available else 'Offline'}; acessórios/capabilities: {kinds}",
        device,
        accessories,
    )


def _get_ir_code(
    profile: DeviceProfile,
    secret_manager: SecretManager,
    code_id: str,
    *,
    timeout: float,
    opener: Callable[..., Any],
) -> dict[str, Any]:
    request = Request(
        f"{profile.server.base_url.rstrip('/')}/api/ir/codes",
        headers=_admin_headers(profile, secret_manager),
        method="GET",
    )
    with opener(request, timeout=timeout) as response:  # noqa: S310
        data = json.loads(response.read().decode("utf-8"))
    code = next(
        (
            item
            for item in data.get("codes", [])
            if isinstance(item, dict)
            and item.get("id") == code_id
            and item.get("device_id") == profile.device_id
        ),
        None,
    )
    if code is None:
        raise ValueError("código IR escolhido não existe para este dispositivo")
    return code


def sync_ir_code(
    profile: DeviceProfile,
    secret_manager: SecretManager,
    code: dict[str, Any],
    *,
    timeout: float = 5.0,
    opener: Callable[..., Any] | None = None,
) -> IntegrationResult:
    """Idempotently create/update one locally learned IR code on the server.

    The raw sequence is sent only to the configured authenticated server.  It
    is never logged or added to the device profile; offline callers simply
    retain the code as pending in ``IrLibraryStore``.
    """

    if not profile.device_id or not profile.server.base_url:
        return IntegrationResult(False, "Servidor indisponível; código mantido pendente localmente")
    code_id = str(code.get("id") or "").strip()
    if not code_id or str(code.get("device_id") or "") != profile.device_id:
        return IntegrationResult(False, "Código IR não pertence ao dispositivo pareado")
    try:
        headers = _admin_headers(profile, secret_manager)
    except ValueError as exc:
        return IntegrationResult(False, str(exc))
    open_request = opener or urlopen
    collection_url = f"{profile.server.base_url.rstrip('/')}/api/ir/codes"
    try:
        request = Request(collection_url, headers=headers, method="GET")
        with open_request(request, timeout=timeout) as response:  # noqa: S310
            listing = json.loads(response.read().decode("utf-8"))
        exists = any(
            isinstance(item, dict)
            and item.get("id") == code_id
            and item.get("device_id") == profile.device_id
            for item in listing.get("codes", [])
        )
        payload = json.dumps(code, separators=(",", ":"), sort_keys=True).encode("utf-8")
        target = f"{collection_url}/{quote(code_id, safe='')}" if exists else collection_url
        request = Request(
            target,
            data=payload,
            headers=headers,
            method="PUT" if exists else "POST",
        )
        with open_request(request, timeout=timeout) as response:  # noqa: S310
            status = int(response.status)
            response.read()
        if not 200 <= status < 300:
            return IntegrationResult(False, f"Sincronização IR respondeu HTTP {status}")
    except HTTPError as exc:
        return IntegrationResult(False, f"Servidor recusou o código IR (HTTP {exc.code})")
    except (URLError, OSError, TimeoutError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        return IntegrationResult(False, f"Servidor IR indisponível: {exc}")
    return IntegrationResult(True, "Código IR sincronizado com servidor e Home Assistant")


def send_accessory_command(
    profile: DeviceProfile,
    secret_manager: SecretManager,
    action: str,
    params: dict[str, Any],
    *,
    timeout: float = 5.0,
    opener: Callable[..., Any] | None = None,
    poll_interval: float = 0.25,
    monotonic: Callable[[], float] | None = None,
    sleeper: Callable[[float], None] | None = None,
) -> IntegrationResult:
    """Send a user-approved command and wait for the ESP acknowledgement.

    An HTTP 202 only proves that the server accepted the request.  Commissioning
    succeeds exclusively after the command readback reaches a terminal success
    state.  The injectable clock/sleeper keep timeout tests deterministic and
    hardware-free.
    """

    if not profile.device_id or not profile.server.base_url:
        return IntegrationResult(False, "Pareie a placa e configure o servidor")
    allowed = {
        "relay.pulse",
        "ir.learn.start",
        "ir.learn.cancel",
        "ir.send",
        "storage.sync",
    }
    if action not in allowed:
        return IntegrationResult(False, "Ação de comissionamento não permitida pela TUI")
    if action == "relay.pulse":
        relay_id = str(params.get("relay_id") or "")
        relay = next(
            (item for item in profile.relays if item.enabled and item.relay_id == relay_id),
            None,
        )
        duration = int(params.get("duration_ms") or 0)
        if relay is None:
            return IntegrationResult(False, "Relé escolhido não está habilitado no perfil")
        if not 1 <= duration <= min(relay.pulse_limit_ms, 300_000):
            return IntegrationResult(
                False,
                f"Pulso deve ficar entre 1 e {min(relay.pulse_limit_ms, 300_000)} ms",
            )
    if action == "storage.sync" and not profile.storage.enabled:
        return IntegrationResult(False, "microSD está desabilitado no perfil")

    open_request = opener or urlopen
    try:
        headers = _admin_headers(profile, secret_manager)
    except ValueError as exc:
        return IntegrationResult(False, str(exc))
    command_params = dict(params)
    if action == "ir.send":
        code_id = str(command_params.get("code_id") or "")
        if not code_id:
            return IntegrationResult(False, "Escolha um código IR antes de transmitir")
        try:
            code = _get_ir_code(
                profile,
                secret_manager,
                code_id,
                timeout=timeout,
                opener=open_request,
            )
        except (HTTPError, URLError, OSError, ValueError, json.JSONDecodeError) as exc:
            return IntegrationResult(False, f"Falha ao carregar código IR escolhido: {exc}")
        command_params = {
            "code_id": code_id,
            "carrier_hz": int(code.get("carrier_hz") or 38_000),
            "duty_percent": round(float(code.get("duty_cycle") or 0.33) * 100),
            "sequence_us": code.get("sequence_us", []),
            "repeats": 1,
        }
    ttl_ms = 15_000
    if action == "ir.learn.start":
        try:
            learn_timeout_ms = int(command_params.get("timeout_ms") or 15_000)
        except (TypeError, ValueError):
            return IntegrationResult(False, "timeout_ms do aprendizado IR é inválido")
        if not 1_000 <= learn_timeout_ms <= 60_000:
            return IntegrationResult(False, "Aprendizado IR deve durar de 1 a 60 segundos")
        ttl_ms = learn_timeout_ms + 5_000

    document = {
        "request_id": str(uuid4()),
        "device_id": profile.device_id,
        "action": action,
        "params": command_params,
        "ttl_ms": ttl_ms,
    }
    request = Request(
        f"{profile.server.base_url.rstrip('/')}/api/accessories/commands",
        data=json.dumps(document, separators=(",", ":"), sort_keys=True).encode("utf-8"),
        headers=headers,
        method="POST",
    )
    started_at = (monotonic or time.monotonic)()
    try:
        with open_request(request, timeout=timeout) as response:  # noqa: S310
            status = int(response.status)
            raw_response = response.read()
    except HTTPError as exc:
        return IntegrationResult(False, f"Comando {action} recusado (HTTP {exc.code})")
    except (URLError, OSError, TimeoutError) as exc:
        return IntegrationResult(False, f"Falha ao enviar {action}: {exc}")
    if not 200 <= status < 300:
        return IntegrationResult(False, f"Comando {action} respondeu HTTP {status}")

    try:
        response_document = json.loads(raw_response.decode("utf-8")) if raw_response else {}
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        return IntegrationResult(False, f"Resposta inválida ao enviar {action}: {exc}")
    if not isinstance(response_document, dict):
        return IntegrationResult(False, f"Resposta inválida ao enviar {action}")
    initial_command = response_document.get("command")
    if not isinstance(initial_command, dict):
        initial_command = {}
    request_id = str(
        initial_command.get("request_id")
        or response_document.get("request_id")
        or document["request_id"]
    )
    if request_id != document["request_id"]:
        return IntegrationResult(False, "Servidor respondeu com request_id divergente")

    def terminal_result(command: dict[str, Any]) -> IntegrationResult | None:
        state = str(command.get("status") or "").strip().lower()
        if state in {"completed", "complete", "ok", "learned"}:
            detail = "aprendizado concluído" if action == "ir.learn.start" else "ACK recebido"
            return IntegrationResult(True, f"Comando {action}: {detail} ({request_id})")
        if state in {"failed", "error", "expired", "timeout", "cancelled", "canceled"}:
            error = str(command.get("error") or state)
            return IntegrationResult(False, f"Comando {action} falhou: {error} ({request_id})")
        return None

    immediate = terminal_result(initial_command)
    if immediate is not None:
        return immediate

    clock = monotonic or time.monotonic
    wait = sleeper or time.sleep
    deadline = started_at + (ttl_ms / 1000.0) + 2.0
    poll_url = (
        f"{profile.server.base_url.rstrip('/')}/api/accessories/commands/"
        f"{quote(request_id, safe='')}"
    )
    interval = max(0.05, min(float(poll_interval), 2.0))
    while clock() < deadline:
        wait(min(interval, max(0.0, deadline - clock())))
        poll_request = Request(poll_url, headers=headers, method="GET")
        try:
            with open_request(poll_request, timeout=timeout) as response:  # noqa: S310
                poll_status = int(response.status)
                poll_raw = response.read()
        except HTTPError as exc:
            return IntegrationResult(False, f"Consulta do comando {request_id} falhou (HTTP {exc.code})")
        except (URLError, OSError, TimeoutError) as exc:
            return IntegrationResult(False, f"Falha ao consultar comando {request_id}: {exc}")
        if not 200 <= poll_status < 300:
            return IntegrationResult(False, f"Consulta do comando respondeu HTTP {poll_status}")
        try:
            poll_document = json.loads(poll_raw.decode("utf-8"))
            command = poll_document.get("command") if isinstance(poll_document, dict) else None
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            return IntegrationResult(False, f"Readback inválido do comando {request_id}: {exc}")
        if not isinstance(command, dict):
            return IntegrationResult(False, f"Readback do comando {request_id} não contém estado")
        returned_id = str(command.get("request_id") or request_id)
        if returned_id != request_id:
            return IntegrationResult(False, "Readback retornou request_id divergente")
        terminal = terminal_result(command)
        if terminal is not None:
            return terminal

    return IntegrationResult(
        False,
        f"Comando {action} sem ACK do ESP após {ttl_ms + 2_000} ms ({request_id})",
    )


def test_server_connection(
    profile: DeviceProfile,
    secret_manager: SecretManager,
    *,
    timeout: float = 3.0,
) -> IntegrationResult:
    if not profile.server.base_url:
        return IntegrationResult(False, "URL do servidor não configurada")
    headers = {"Accept": "application/json"}
    token = secret_manager.get(profile.server.admin_bearer_token_ref)
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = Request(f"{profile.server.base_url}/api/health", headers=headers, method="GET")
    try:
        with urlopen(request, timeout=timeout) as response:  # noqa: S310 - user-configured LAN URL
            status = response.status
    except HTTPError as exc:
        return IntegrationResult(False, f"Servidor respondeu HTTP {exc.code}")
    except (URLError, OSError) as exc:
        return IntegrationResult(False, f"Servidor inacessível: {exc}")
    return IntegrationResult(200 <= status < 300, f"Servidor respondeu HTTP {status}")
