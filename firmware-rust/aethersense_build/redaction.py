"""Central masking for logs, environment tables and exported structures."""

from __future__ import annotations

from collections.abc import Mapping
import re
from typing import Any


SECRET_KEY_RE = re.compile(
    r"(?:password|passwd|pwd|secret|token|authorization|bearer|api[_-]?key|credential)",
    re.IGNORECASE,
)
ASSIGNMENT_RE = re.compile(
    r"(?i)\b([A-Z0-9_.-]*(?:PASSWORD|PASSWD|SECRET|TOKEN|AUTHORIZATION|BEARER|API_KEY)[A-Z0-9_.-]*)"
    r"(\s*[:=]\s*)(?:\"[^\"]*\"|'[^']*'|[^\s,;]+)"
)
MASK = "<redacted>"


def is_secret_key(key: object) -> bool:
    return bool(SECRET_KEY_RE.search(str(key)))


def redact_mapping(value: Any) -> Any:
    if isinstance(value, Mapping):
        return {
            str(key): MASK
            if is_secret_key(key) and item is not None and item != ""
            else redact_mapping(item)
            for key, item in value.items()
        }
    if isinstance(value, list):
        return [redact_mapping(item) for item in value]
    if isinstance(value, tuple):
        return tuple(redact_mapping(item) for item in value)
    return value


def redact_text(text: str, secret_values: tuple[str, ...] = ()) -> str:
    result = ASSIGNMENT_RE.sub(lambda match: f"{match.group(1)}{match.group(2)}{MASK}", text)
    for secret in sorted({item for item in secret_values if item}, key=len, reverse=True):
        result = result.replace(secret, MASK)
    return result


def display_value(key: str, value: object) -> str:
    if is_secret_key(key) and value not in {None, ""}:
        return MASK
    if value is None:
        return ""
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)
