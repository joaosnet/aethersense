"""Domain services used by the AetherSense firmware build tool.

The package intentionally has no dependency on Textual.  The CLI, TUI and tests
therefore share exactly the same profile, validation, persistence and redaction
rules.
"""

from .diffing import ProfileDiff, compute_build_hash, compute_config_hash, diff_profiles
from .models import DEVICE_PROFILE_SCHEMA, DeviceProfile, new_device_profile
from .secrets import MemorySecretStore, SecretManager
from .store import DeviceProfileStore
from .validation import ValidationIssue, validate_profile

__all__ = [
    "DEVICE_PROFILE_SCHEMA",
    "DeviceProfile",
    "DeviceProfileStore",
    "MemorySecretStore",
    "ProfileDiff",
    "SecretManager",
    "ValidationIssue",
    "compute_build_hash",
    "compute_config_hash",
    "diff_profiles",
    "new_device_profile",
    "validate_profile",
]
