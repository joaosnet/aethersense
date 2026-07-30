from aethersense_build.batch import apply_batch, plan_batch
from aethersense_build.diffing import compute_build_hash
from aethersense_build.models import new_device_profile
import hashlib


def paired(name: str):
    profile = new_device_profile(name)
    compact = hashlib.sha256(name.encode()).hexdigest()[:12]
    profile.association.efuse_mac = ":".join(
        compact[index : index + 2] for index in range(0, 12, 2)
    )
    profile.device_id = f"esp32-{compact}"
    profile.association.serial_number = f"usb-{name.lower()}"
    return profile


def test_batch_plan_blocks_drafts_and_orders_actions() -> None:
    draft = new_device_profile("Draft")
    build = paired("Build")
    provision = paired("Provision")
    provision.applied.build_hash = compute_build_hash(provision)
    plan = plan_batch([(provision, False), (draft, True), (build, False)])
    operations = {item.friendly_name: item.operation for item in plan.items}
    assert operations == {
        "Draft": "blocked",
        "Build": "flash_then_provision",
        "Provision": "provision",
    }
    called = []
    results = apply_batch(plan, lambda item: called.append(item.profile_id) or "ok")
    assert len(results) == 2
    assert len(called) == 2
