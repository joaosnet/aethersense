"""Deterministic planning and orchestration primitives for catalogue-wide apply."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Callable, Generic, TypeVar

from .diffing import profile_status
from .models import DeviceProfile


@dataclass(frozen=True)
class BatchPlanItem:
    profile_id: str
    friendly_name: str
    status: str
    operation: str
    blocked_reason: str = ""


@dataclass(frozen=True)
class BatchPlan:
    items: tuple[BatchPlanItem, ...]

    @property
    def actionable(self) -> tuple[BatchPlanItem, ...]:
        return tuple(item for item in self.items if item.operation not in {"none", "blocked"})

    @property
    def blocked(self) -> tuple[BatchPlanItem, ...]:
        return tuple(item for item in self.items if item.operation == "blocked")


def plan_batch(profiles: list[tuple[DeviceProfile, bool]]) -> BatchPlan:
    items: list[BatchPlanItem] = []
    for profile, draft in sorted(profiles, key=lambda item: item[0].profile_id):
        status = profile_status(profile, draft=draft)
        if status == "Inválido":
            operation, reason = "blocked", "configuração inválida"
        elif status == "Rascunho":
            operation, reason = "blocked", "dispositivo ainda não pareado"
        elif not profile.association.serial_number and not (
            profile.association.vid and profile.association.pid
        ):
            operation, reason = "blocked", "associação USB (serial ou VID/PID) ausente"
        elif status == "Requer build":
            operation, reason = "flash_then_provision", ""
        elif status == "Requer provisionamento":
            operation, reason = "provision", ""
        else:
            operation, reason = "none", ""
        items.append(
            BatchPlanItem(
                profile.profile_id,
                profile.friendly_name,
                status,
                operation,
                reason,
            )
        )
    return BatchPlan(tuple(items))


T = TypeVar("T")


def apply_batch(
    plan: BatchPlan,
    executor: Callable[[BatchPlanItem], T],
) -> tuple[tuple[BatchPlanItem, T], ...]:
    """Execute only actionable entries in stable order.

    Hardware/UI code supplies the executor; blocked and already-synchronised
    entries are never passed through accidentally.
    """
    return tuple((item, executor(item)) for item in plan.actionable)
