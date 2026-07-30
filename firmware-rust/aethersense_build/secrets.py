"""OS-keyring-backed secrets with an explicit in-memory fallback."""

from __future__ import annotations

from dataclasses import dataclass, field
import hmac
from secrets import token_hex
from typing import Protocol

from .models import PROVISIONED_SECRET_KINDS


SERVICE_NAME = "AetherSense"


class SecretStore(Protocol):
    backend_name: str

    def get(self, reference: str) -> str | None: ...

    def set(self, reference: str, value: str) -> None: ...

    def delete(self, reference: str) -> None: ...


@dataclass
class MemorySecretStore:
    values: dict[str, str] = field(default_factory=dict)
    backend_name: str = "memory-only"

    def get(self, reference: str) -> str | None:
        return self.values.get(reference)

    def set(self, reference: str, value: str) -> None:
        if value:
            self.values[reference] = value
        else:
            self.values.pop(reference, None)

    def delete(self, reference: str) -> None:
        self.values.pop(reference, None)


class KeyringSecretStore:
    backend_name = "os-keyring"

    def __init__(self, keyring_module=None, service_name: str = SERVICE_NAME) -> None:
        if keyring_module is None:
            import keyring as keyring_module  # type: ignore[no-redef]

        self._keyring = keyring_module
        self._service_name = service_name
        # get_keyring() catches configurations with the fail backend before the
        # first user edit. Some third-party backends do not expose priority, so
        # the harmless probe remains the definitive check.
        backend = self._keyring.get_keyring()
        if getattr(backend, "priority", 1) <= 0:
            raise RuntimeError("no usable OS keyring backend")

    def get(self, reference: str) -> str | None:
        return self._keyring.get_password(self._service_name, reference)

    def set(self, reference: str, value: str) -> None:
        if value:
            self._keyring.set_password(self._service_name, reference, value)
        else:
            self.delete(reference)

    def delete(self, reference: str) -> None:
        try:
            self._keyring.delete_password(self._service_name, reference)
        except Exception as exc:  # keyring has backend-specific exception types
            if exc.__class__.__name__ not in {"PasswordDeleteError", "KeyringError"}:
                raise


class SecretManager:
    """Stable reference names plus fail-closed fallback behavior.

    A keyring failure never writes a secret to a profile or plaintext file. The
    value remains in this process only and the TUI exposes `backend_name` so the
    user knows persistence is unavailable.
    """

    def __init__(self, store: SecretStore | None = None) -> None:
        self._fallback = MemorySecretStore()
        if store is not None:
            self._store = store
            return
        try:
            self._store = KeyringSecretStore()
        except Exception:
            self._store = self._fallback

    @property
    def backend_name(self) -> str:
        return self._store.backend_name

    @staticmethod
    def reference(profile_id: str, kind: str) -> str:
        return f"device/{profile_id}/{kind}"

    def get(self, reference: str) -> str | None:
        if not reference:
            return None
        try:
            return self._store.get(reference)
        except Exception:
            return self._fallback.get(reference)

    def set(self, reference: str, value: str) -> None:
        if not reference:
            raise ValueError("secret reference must not be empty")
        try:
            self._store.set(reference, value)
        except Exception:
            self._fallback.set(reference, value)
            self._store = self._fallback

    def set_with_revision(
        self,
        reference: str,
        value: str,
        current_revision: str = "",
    ) -> str:
        """Set a secret and return its opaque 128-bit revision.

        The previous value is compared only in memory. No hash/verifier derived
        from the secret is persisted. Re-entering the same value preserves a
        valid revision; a real change gets a fresh random nonce.
        """

        previous = self.get(reference)
        same_value = previous is not None and hmac.compare_digest(previous, value)
        valid_revision = (
            len(current_revision) == 32
            and all(char in "0123456789abcdef" for char in current_revision)
        )
        if same_value and valid_revision:
            return current_revision
        if not same_value:
            self.set(reference, value)
        return token_hex(16)

    def delete(self, reference: str) -> None:
        if not reference:
            return
        try:
            self._store.delete(reference)
        except Exception:
            self._fallback.delete(reference)
            self._store = self._fallback


def _profile_secret_references(profile) -> dict[str, str]:
    return {
        "wifi_password": profile.network.wifi.password_ref,
        "mqtt_password": profile.mqtt.password_ref,
        "server_bearer_token": profile.server.bearer_token_ref,
    }


def synchronise_secret_revisions(
    profile,
    secret_manager: SecretManager,
    *,
    require_available: bool = False,
) -> dict[str, str]:
    """Ensure referenced secrets have opaque revisions, failing closed on demand."""
    result = {
        key: value
        for key, value in profile.secret_revisions.items()
        if key in PROVISIONED_SECRET_KINDS
    }
    for kind, reference in _profile_secret_references(profile).items():
        if not reference:
            result.pop(kind, None)
            continue
        value = secret_manager.get(reference)
        if value is None:
            if require_available:
                raise ValueError(f"segredo {kind} não está disponível no keyring")
            continue
        if not value:
            result.pop(kind, None)
        elif kind not in result:
            result[kind] = token_hex(16)
    profile.secret_revisions = dict(sorted(result.items()))
    return profile.secret_revisions
