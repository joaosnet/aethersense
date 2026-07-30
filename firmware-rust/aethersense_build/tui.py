"""Textual multi-device setup wizard and maintenance panel for build.py."""

from __future__ import annotations

from pathlib import Path
import re
import threading
from typing import Any, Callable
from uuid import uuid4

from .automatic import (
    AutomaticDeviceCandidate,
    build_automatic_candidates,
    choose_automatic_candidate,
    finish_automatic_identity,
    prepare_automatic_profile,
    repair_incomplete_profile,
    usable_port_infos,
)
from .batch import apply_batch, plan_batch
from .diffing import compute_build_hash, compute_config_hash, profile_status
from .devices import DevicePairingError, pair_profile, probe_board, resolve_profile_port
from .environment import build_environment, effective_values
from .integrations import (
    IntegrationResult,
    LocalBrokerManager,
    get_commissioning_status,
    onboard_device,
    prepare_local_dashboard_profile,
    send_accessory_command,
    sync_ir_code,
    test_mqtt_connection,
    test_server_connection,
)
from .hardware_validation import (
    AutomaticTestResult,
    HardwareValidationError,
    HardwareValidationService,
    ValidationState,
    wait_for_runtime_snapshot,
)
from .ir_library import IrLibraryStore
from .models import (
    DEFAULT_STATUS_LED_COLORS,
    DeviceProfile,
    RelayConfig,
    StatusLedConfig,
    device_id_from_mac,
    new_device_profile,
    utc_now,
)
from .provisioning import ProvisionClient, activate_provisioned_profile
from .redaction import redact_text
from .sd_import import import_sd_sessions, inspect_sd
from .secrets import SecretManager, synchronise_secret_revisions
from .store import DeviceProfileStore
from .validation import has_errors, validate_profile


AUTO_PORT = "__auto__"
MANUAL_PORT = "__manual__"
NO_AUTOMATIC_DEVICE = "__no_automatic_device__"


ISSUE_FIELDS: dict[str, tuple[str, str, str]] = {
    "target.board_variant": ("devices-tab", "#board-variant-select", "Variante da placa"),
    "runtime.status_led.data_gpio": ("config-tab", "#led-gpio-input", "LED RGB — GPIO data"),
    "runtime.status_led_gpio": ("config-tab", "#led-gpio-input", "LED RGB — GPIO data"),
    "ir_rx.gpio": ("peripherals-tab", "#ir-rx-gpio-input", "IR RX — GPIO"),
    "ir_tx.gpio": ("peripherals-tab", "#ir-tx-gpio-input", "IR TX — GPIO"),
    "storage.sclk_gpio": ("storage-tab", "#sd-sclk-input", "microSD — SCLK GPIO"),
    "storage.mosi_gpio": ("storage-tab", "#sd-mosi-input", "microSD — MOSI GPIO"),
    "storage.miso_gpio": ("storage-tab", "#sd-miso-input", "microSD — MISO GPIO"),
    "storage.cs_gpio": ("storage-tab", "#sd-cs-input", "microSD — CS GPIO"),
}


def _issue_field(path: str) -> tuple[str, str, str] | None:
    destination = ISSUE_FIELDS.get(path)
    if destination is not None:
        return destination
    color = re.fullmatch(r"runtime\.status_led\.colors\.([a-z_]+)", path)
    if color:
        state = color.group(1)
        return "config-tab", f"#led-color-{state}-input", f"LED RGB — cor {state}"
    relay = re.fullmatch(r"relays\[(\d+)]\.(gpio|pulse_limit_ms)", path)
    if relay:
        index, field = relay.groups()
        suffix = "gpio" if field == "gpio" else "pulse"
        label = "GPIO" if field == "gpio" else "pulso máximo"
        return "peripherals-tab", f"#relay-{index}-{suffix}", f"Relé {int(index) + 1} — {label}"
    return None


def _integer(value: str, default: int) -> int:
    try:
        return int(value.strip())
    except (TypeError, ValueError):
        return default


def _pin(value: str) -> int | None:
    value = value.strip()
    if not value:
        return None
    try:
        return int(value)
    except ValueError:
        return None


def _rgb(value: str, default: list[int]) -> list[int]:
    try:
        channels = [int(item.strip()) for item in value.split(",")]
    except ValueError:
        return list(default)
    return channels if len(channels) == 3 else list(default)


def create_app(
    api: Any,
    settings: Any,
    initial_options: Any | None = None,
    *,
    profile_store: DeviceProfileStore | None = None,
    secret_manager: SecretManager | None = None,
    port_provider: Callable[[], list[dict[str, Any]]] | None = None,
):
    from textual import work
    from textual.app import App, ComposeResult
    from textual.binding import Binding
    from textual.containers import Horizontal, Vertical, VerticalScroll
    from textual.message import Message
    from textual.screen import ModalScreen
    from textual.widgets import (
        Button,
        DataTable,
        Footer,
        Header,
        Input,
        Label,
        LoadingIndicator,
        Markdown,
        ProgressBar,
        RichLog,
        Select,
        Static,
        Switch,
        TabbedContent,
        TabPane,
    )

    store = profile_store or DeviceProfileStore()
    secrets = secret_manager or SecretManager()
    ports = port_provider or api.available_serial_port_infos

    class BuildLog(Message):
        def __init__(self, text: str) -> None:
            self.text = text
            super().__init__()

    class BuildProgress(Message):
        def __init__(self, value: float | None, text: str) -> None:
            self.value = value
            self.text = text
            super().__init__()

    class BuildDone(Message):
        def __init__(self, exit_code: int, action: str) -> None:
            self.exit_code = exit_code
            self.action = action
            super().__init__()

    class IntegrationDone(Message):
        def __init__(self, text: str, ok: bool) -> None:
            self.text = text
            self.ok = ok
            super().__init__()

    class ProvisionDone(Message):
        def __init__(self, result=None, error: str = "") -> None:
            self.result = result
            self.error = error
            super().__init__()

    class OnboardingDone(Message):
        def __init__(self, result=None, error: str = "", continue_apply: bool = False) -> None:
            self.result = result
            self.error = error
            self.continue_apply = continue_apply
            super().__init__()

    class PairingDone(Message):
        def __init__(
            self,
            identity=None,
            error: str = "",
            profile: DeviceProfile | None = None,
            is_draft: bool = False,
        ) -> None:
            self.identity = identity
            self.error = error
            self.profile = profile
            self.is_draft = is_draft
            super().__init__()

    class BatchDone(Message):
        def __init__(self, results) -> None:
            self.results = results
            super().__init__()

    class CommissionDone(Message):
        def __init__(
            self,
            text: str,
            ok: bool,
            target: str = "integration",
            *,
            action: str = "",
            data: dict[str, Any] | None = None,
            source: str = "",
        ) -> None:
            self.text = text
            self.ok = ok
            self.target = target
            self.action = action
            self.data = data
            self.source = source
            super().__init__()

    class SdDone(Message):
        def __init__(self, text: str, ok: bool) -> None:
            self.text = text
            self.ok = ok
            super().__init__()

    class HardwareValidationDone(Message):
        def __init__(self, operation: str, check_id: str = "", error: str = "") -> None:
            self.operation = operation
            self.check_id = check_id
            self.error = error
            super().__init__()

    class ImportConfirmScreen(ModalScreen[dict[str, bool] | None]):
        CSS = """
        ImportConfirmScreen { align: center middle; background: $background 70%; }
        #import-dialog { width: 82; height: auto; max-height: 90%; padding: 1 2;
                         border: heavy $accent; background: $surface; }
        #import-diff { height: auto; max-height: 18; padding: 1; border: solid $secondary; }
        #import-dialog-buttons { height: auto; margin-top: 1; }
        #import-dialog-buttons Button { margin-right: 1; }
        """

        def __init__(self, preview) -> None:
            self.preview = preview
            super().__init__()

        def compose(self) -> ComposeResult:
            changes = self.preview.diff.changes
            lines = [
                f"{change.path}: {change.before!r} → {change.after!r}"
                for change in changes[:14]
            ]
            if len(changes) > 14:
                lines.append(f"… e mais {len(changes) - 14} alteração(ões)")
            with Vertical(id="import-dialog"):
                yield Label("Confirmar importação", classes="section-title")
                yield Static(
                    f"Fonte: {self.preview.source}\n"
                    f"Perfil: {self.preview.profile.friendly_name}\n"
                    f"{len(changes)} campo(s) diferente(s).",
                    classes="hint",
                )
                yield Static("\n".join(lines) or "Sem diferenças de conteúdo.", id="import-diff")
                yield Switch(value=True, id="import-copy-switch")
                yield Label("Criar cópia não pareada (padrão seguro)", classes="hint")
                with Horizontal(id="import-dialog-buttons"):
                    yield Button("Cancelar", id="import-cancel-button")
                    yield Button("Confirmar", id="import-confirm-button", variant="success")

        def on_button_pressed(self, event: Button.Pressed) -> None:
            if event.button.id == "import-cancel-button":
                self.dismiss(None)
            elif event.button.id == "import-confirm-button":
                self.dismiss(
                    {
                        "confirmed": True,
                        "copy": self.query_one("#import-copy-switch", Switch).value,
                    }
                )

    class BatchConfirmScreen(ModalScreen[bool]):
        CSS = """
        BatchConfirmScreen { align: center middle; background: $background 70%; }
        #batch-dialog { width: 78; height: auto; max-height: 90%; padding: 1 2;
                        border: heavy $warning; background: $surface; }
        #batch-summary { height: auto; max-height: 18; padding: 1; border: solid $secondary; }
        #batch-dialog-buttons { height: auto; margin-top: 1; }
        #batch-dialog-buttons Button { margin-right: 1; }
        """

        def __init__(self, plan) -> None:
            self.plan = plan
            super().__init__()

        def compose(self) -> ComposeResult:
            lines = [
                f"{item.friendly_name}: {item.operation}"
                + (f" — BLOQUEADO: {item.blocked_reason}" if item.blocked_reason else "")
                for item in self.plan.items
            ]
            with Vertical(id="batch-dialog"):
                yield Label("Confirmar aplicação em lote", classes="section-title")
                yield Static(
                    f"{len(self.plan.actionable)} dispositivo(s) serão processados sequencialmente. "
                    "Perfis inválidos/bloqueados serão ignorados.",
                    classes="hint",
                )
                yield Static("\n".join(lines), id="batch-summary")
                with Horizontal(id="batch-dialog-buttons"):
                    yield Button("Cancelar", id="batch-cancel-button")
                    yield Button("Aplicar lote", id="batch-confirm-button", variant="error")

        def on_button_pressed(self, event: Button.Pressed) -> None:
            if event.button.id == "batch-cancel-button":
                self.dismiss(False)
            elif event.button.id == "batch-confirm-button":
                self.dismiss(True)

    class CatalogueImportConfirmScreen(ModalScreen[dict[str, bool] | None]):
        CSS = """
        CatalogueImportConfirmScreen { align: center middle; background: $background 70%; }
        #catalogue-dialog { width: 82; height: auto; max-height: 90%; padding: 1 2;
                            border: heavy $accent; background: $surface; }
        #catalogue-diff { height: auto; max-height: 18; padding: 1; border: solid $secondary; }
        #catalogue-buttons { height: auto; margin-top: 1; }
        #catalogue-buttons Button { margin-right: 1; }
        """

        def __init__(self, preview) -> None:
            self.preview = preview
            super().__init__()

        def compose(self) -> ComposeResult:
            lines = [
                f"{item.profile.friendly_name} ({item.profile.profile_id}): "
                f"{len(item.diff.changes)} alteração(ões)"
                for item in self.preview.profiles
            ]
            with Vertical(id="catalogue-dialog"):
                yield Label("Confirmar importação do catálogo", classes="section-title")
                yield Static(
                    f"Fonte: {self.preview.source}\n{len(lines)} perfil(is) validados pelo manifesto.",
                    classes="hint",
                )
                yield Static("\n".join(lines), id="catalogue-diff")
                yield Switch(value=True, id="catalogue-copy-switch")
                yield Label("Criar cópias não pareadas (padrão seguro)", classes="hint")
                with Horizontal(id="catalogue-buttons"):
                    yield Button("Cancelar", id="catalogue-cancel-button")
                    yield Button("Importar", id="catalogue-confirm-button", variant="success")

        def on_button_pressed(self, event: Button.Pressed) -> None:
            if event.button.id == "catalogue-cancel-button":
                self.dismiss(None)
            elif event.button.id == "catalogue-confirm-button":
                self.dismiss(
                    {
                        "confirmed": True,
                        "copy": self.query_one("#catalogue-copy-switch", Switch).value,
                    }
                )

    class RelayPulseConfirmScreen(ModalScreen[bool]):
        CSS = """
        RelayPulseConfirmScreen { align: center middle; background: $background 70%; }
        #relay-confirm-dialog { width: 68; height: auto; padding: 1 2;
                                border: heavy $error; background: $surface; }
        #relay-confirm-buttons { height: auto; margin-top: 1; }
        #relay-confirm-buttons Button { margin-right: 1; }
        """

        def __init__(self, relay_name: str, duration_ms: int) -> None:
            self.relay_name = relay_name
            self.duration_ms = duration_ms
            super().__init__()

        def compose(self) -> ComposeResult:
            with Vertical(id="relay-confirm-dialog"):
                yield Label("Confirmar pulso físico do relé", classes="section-title")
                yield Static(
                    f"Relé: {self.relay_name}\nDuração: {self.duration_ms} ms\n\n"
                    "A carga será energizada. Confirme somente após revisar isolamento e segurança elétrica.",
                    classes="hint",
                )
                with Horizontal(id="relay-confirm-buttons"):
                    yield Button("Cancelar", id="relay-pulse-cancel")
                    yield Button("Confirmar pulso", id="relay-pulse-confirm", variant="error")

        def on_button_pressed(self, event: Button.Pressed) -> None:
            self.dismiss(event.button.id == "relay-pulse-confirm")

    class HardwareConfirmationScreen(ModalScreen[bool]):
        """One plain-language decision for a physical observation or safety gate."""

        CSS = """
        HardwareConfirmationScreen { align: center middle; background: $background 70%; }
        #hardware-confirm-dialog { width: 70; max-width: 94%; height: auto; padding: 1 2;
                                   border: heavy $accent; background: $surface; }
        #hardware-confirm-buttons { height: auto; margin-top: 1; }
        #hardware-confirm-buttons Button { margin-right: 1; }
        """

        def __init__(self, title: str, prompt: str, *, confirm_label: str = "Confirmei") -> None:
            self.title = title
            self.prompt = prompt
            self.confirm_label = confirm_label
            super().__init__()

        def compose(self) -> ComposeResult:
            with Vertical(id="hardware-confirm-dialog"):
                yield Label(self.title, classes="section-title")
                yield Static(self.prompt, classes="hint")
                with Horizontal(id="hardware-confirm-buttons"):
                    yield Button("Não / cancelar", id="hardware-confirm-no")
                    yield Button(
                        self.confirm_label,
                        id="hardware-confirm-yes",
                        variant="success",
                    )

        def on_button_pressed(self, event: Button.Pressed) -> None:
            self.dismiss(event.button.id == "hardware-confirm-yes")

    class AdvancedSectionsScreen(ModalScreen[str | None]):
        """Card-based advanced navigation that remains usable on narrow terminals."""

        BINDINGS = [("escape", "cancel", "Fechar")]
        SECTIONS = (
            (
                "devices-tab",
                "1. Placa e perfil",
                "Nome, pareamento, modelo da placa e catálogo de dispositivos",
            ),
            (
                "config-tab",
                "2. Rede e LED",
                "Wi-Fi, UDP, CSI e cores do LED RGB integrado",
            ),
            (
                "peripherals-tab",
                "3. Acessórios e pinos",
                "Receptor IR, transmissor IR, relés e GPIOs",
            ),
            (
                "integrations-tab",
                "4. Integrações",
                "MQTT, servidor AetherSense e Home Assistant",
            ),
            (
                "storage-tab",
                "5. Cartão SD",
                "Gravação, importação e sincronização de sessões",
            ),
            (
                "build-tab",
                "6. Executar",
                "Validar, compilar, gravar, provisionar e monitorar",
            ),
            (
                "diagnostics-tab",
                "7. Diagnóstico",
                "Portas, hashes, configuração efetiva e problemas",
            ),
            (
                "help-tab",
                "Ajuda e segurança",
                "Guia rápido, comandos preservados e limites elétricos",
            ),
        )
        CSS = """
        AdvancedSectionsScreen {
            align: center middle;
            background: $background 70%;
        }
        #advanced-sections-dialog {
            width: 76;
            max-width: 96%;
            height: 90%;
            max-height: 38;
            padding: 1 2;
            border: heavy $primary;
            background: $surface;
        }
        #advanced-sections-title {
            height: 2;
            text-style: bold;
            color: $accent;
            content-align: center middle;
        }
        #advanced-sections-list {
            height: 1fr;
            margin-top: 1;
        }
        .advanced-section-card {
            width: 100%;
            height: 4;
            margin-bottom: 1;
            text-align: left;
        }
        #advanced-sections-cancel {
            width: 100%;
            margin-top: 1;
        }
        """

        def __init__(self, current_tab: str) -> None:
            self.current_tab = current_tab
            super().__init__()

        def compose(self) -> ComposeResult:
            with Vertical(id="advanced-sections-dialog"):
                yield Label("Escolha o que deseja configurar", id="advanced-sections-title")
                yield Static(
                    "Cada card abre apenas uma área. Escape fecha este menu sem trocar de tela.",
                    classes="hint",
                )
                with VerticalScroll(id="advanced-sections-list"):
                    for tab_id, title, description in self.SECTIONS:
                        yield Button(
                            f"{title}\n{description}",
                            id=f"advanced-section-{tab_id}",
                            classes="advanced-section-card",
                            variant="primary" if tab_id == self.current_tab else "default",
                        )
                yield Button("Fechar", id="advanced-sections-cancel")

        def action_cancel(self) -> None:
            self.dismiss(None)

        def on_button_pressed(self, event: Button.Pressed) -> None:
            button_id = event.button.id or ""
            if button_id == "advanced-sections-cancel":
                self.dismiss(None)
                return
            prefix = "advanced-section-"
            if button_id.startswith(prefix):
                self.dismiss(button_id.removeprefix(prefix))

    class TuiSink:
        def __init__(self, app: "FirmwareBuildApp", log_file) -> None:
            self.app = app
            self.log_file = log_file
            self.lock = threading.Lock()
            self._secret_values: set[str] = set()

        def add_secret_values(self, values) -> None:
            self._secret_values.update(str(value) for value in values if value)

        def write(self, message: str) -> None:
            safe = redact_text(
                message,
                tuple(self._secret_values.union(self.app.secret_values())),
            )
            with self.lock:
                self.log_file.write(safe)
                self.log_file.flush()
            self.app.post_message(BuildLog(safe))

        def line(self, message: str = "") -> None:
            self.write(f"{message}\n")

        def event(self, event) -> None:
            if event.message:
                self.line(event.message)
            if event.kind == "progress":
                self.app.post_message(BuildProgress(event.data.get("progress"), event.message))

    class FirmwareBuildApp(App[int]):
        TITLE = "AetherSense Firmware"
        SUB_TITLE = "Configuração, build e comissionamento"
        BINDINGS = [
            ("ctrl+s", "save_profile", "Salvar perfil"),
            ("f5", "run_selected", "Configurar e testar"),
            ("ctrl+r", "refresh_ports", "Atualizar portas"),
            Binding("ctrl+l", "clear_log", "Limpar log", show=False),
            Binding("ctrl+t", "choose_theme", "Tema", show=False),
            Binding("ctrl+c", "cancel_or_quit", "Cancelar/Sair", show=False),
            Binding("alt+left", "wizard_previous", "Etapa anterior", show=False),
            Binding("alt+right", "wizard_next", "Próxima etapa", show=False),
            Binding("shift+f6", "wizard_previous", "Etapa anterior", show=False),
            Binding("f6", "wizard_next", "Próxima etapa", show=False),
        ]
        WIZARD_STEPS = (
            ("devices-tab", "1/7 — Identidade e placa"),
            ("config-tab", "2/7 — Rede, CSI e LED"),
            ("peripherals-tab", "3/7 — Pinos e recursos"),
            ("integrations-tab", "4/7 — Integrações"),
            ("storage-tab", "5/7 — Armazenamento"),
            ("build-tab", "6/7 — Aplicar e executar"),
            ("diagnostics-tab", "7/7 — Revisão e diagnóstico"),
        )
        CSS = """
        Screen { background: $background; }
        #main-tabs > ContentTabs { display: none; }
        #main-tabs, #main-tabs ContentSwitcher, #main-tabs TabPane { height: 1fr; }
        #simple-tab { height: 1fr; align-horizontal: center; }
        #simple-view {
            width: 86; max-width: 100%; height: 1fr; padding: 1 2;
        }
        #simple-title { text-style: bold; color: $accent; height: 2; content-align: center middle; }
        #simple-subtitle { color: $text-muted; height: auto; margin-bottom: 1; text-align: center; }
        .simple-card {
            width: 100%; height: auto; min-height: 5; margin-bottom: 1; padding: 1;
            border: round $secondary; background: $surface;
        }
        .simple-card-primary { border: heavy $primary; }
        .simple-card-title { text-style: bold; color: $accent; height: 1; }
        #simple-device-select { width: 100%; margin: 1 0; }
        #simple-device-summary, #simple-plan-summary, #simple-status-line {
            height: auto; color: $text-muted;
        }
        #simple-run-button { width: 100%; margin-top: 1; height: 3; }
        #simple-secondary-actions { width: 100%; height: auto; margin-bottom: 1; }
        #simple-secondary-actions Button { width: 1fr; margin-right: 1; }
        #simple-progress { height: 2; }
        #simple-hardware-tests { height: auto; }
        .hardware-test-card {
            width: 100%; height: 3; margin-top: 1; text-align: left;
            border: round $secondary;
        }
        #simple-live-log { height: 1fr; min-height: 6; border: solid $secondary; }
        .pane { height: 1fr; padding: 1; }
        .column { width: 1fr; min-width: 35; height: 1fr; padding: 1; border: solid $primary; }
        .wide { width: 2fr; }
        .section-title { text-style: bold; color: $accent; height: 1; margin-top: 1; }
        .hint { color: $text-muted; height: auto; margin-bottom: 1; }
        .field { margin-bottom: 1; }
        .button-row { height: auto; margin: 1 0; }
        .button-row Button { margin-right: 1; min-width: 13; }
        #primary-action-cards { height: auto; }
        .action-card {
            width: 100%; height: 10; margin-bottom: 1; padding: 1;
            border: round $secondary; background: $surface;
        }
        .action-card-primary { border: heavy $primary; }
        .action-card-title { text-style: bold; color: $accent; height: 1; }
        .action-card-description { color: $text-muted; height: auto; }
        .action-card Button { width: 100%; margin-top: 1; }
        .commission-card {
            width: 100%; height: auto; min-height: 8; margin-bottom: 1; padding: 1;
            border: round $secondary; background: $surface;
        }
        .commission-card-primary { border: heavy $primary; }
        .commission-card-title { text-style: bold; color: $accent; height: 1; }
        #device-table { height: 1fr; min-height: 8; }
        #profile-message, #validation-summary, #hash-summary, #integration-result,
        #storage-result { height: auto; min-height: 3; padding: 1; border: solid $secondary; }
        #command-preview { height: 5; padding: 1; border: solid $accent; }
        #run-live-log { height: 1fr; min-height: 10; border: solid $primary; }
        #build-progress { height: 3; }
        #loading-line { height: 3; }
        #ports-table { height: 1fr; min-height: 8; }
        #env-table { height: 1fr; min-height: 10; }
        #issues-table { height: 1fr; min-height: 8; }
        .relay-row { height: auto; margin-bottom: 1; }
        .relay-row Switch { width: 8; }
        .relay-row Input { width: 1fr; margin-right: 1; }
        #wizard-controls {
            height: 5;
            padding: 0 1;
            border: round $secondary;
            background: $surface;
        }
        #wizard-controls Button { min-width: 0; margin: 0; padding: 0 1; }
        #wizard-step-label { width: 1fr; min-width: 4; content-align: center middle; }
        """

        def __init__(self) -> None:
            self.settings = settings
            self.options = initial_options or api.BuildOptions()
            self.profile_store = store
            self.secret_manager = secrets
            self.controller = api.ProcessController()
            self.running = False
            self._provision_after_build = False
            self._action_after_pairing: str | None = None
            self._loading_form = True
            self._port_infos = ports()
            self._pending_import_preview = None
            self._test_after_apply = False
            self._monitor_after_apply = False
            self._hardware_validation_after_apply = False
            self._hardware_validation_service: HardwareValidationService | None = None
            self._hardware_validation_active_check = ""
            self._hardware_prompt_in_progress = False
            self.ir_library_store = IrLibraryStore()
            self._validation_issues = []
            self._last_ir_code: dict[str, Any] | None = None
            self._ir_cancel_event = threading.Event()
            self._ir_learning_active = False
            self._automatic_candidates: dict[str, AutomaticDeviceCandidate] = {}
            self._automatic_profile_created = False
            self._automatic_profile_ids: set[str] = set()
            self._automatic_setup_active = False
            self._advanced_mode = False
            self._last_advanced_tab = "devices-tab"
            catalogue = self.profile_store.list(include_drafts=True)
            if catalogue:
                self.current_profile, self.current_is_draft = catalogue[0]
            else:
                target = self.options.profile or api.DEFAULT_HARDWARE_PROFILE
                self.current_profile = new_device_profile(target_profile=target)
                self.current_is_draft = True
                self.profile_store.save_draft(self.current_profile)
            super().__init__()
            saved = api.saved_tui_theme_name(self.available_themes.keys())
            if saved:
                self.theme = saved

        def compose(self) -> ComposeResult:
            profile = self.current_profile
            yield Header()
            with TabbedContent(initial="simple-tab", id="main-tabs"):
                with TabPane("Início", id="simple-tab"):
                    with VerticalScroll(id="simple-view"):
                        yield Label("AetherSense em um clique", id="simple-title")
                        yield Static(
                            "Conecte a placa pelo cabo USB. O build.py encontra o dispositivo, "
                            "reutiliza sua configuração e executa apenas o que estiver pendente.",
                            id="simple-subtitle",
                        )
                        with Vertical(classes="simple-card simple-card-primary"):
                            yield Label(
                                "Configurar e testar automaticamente",
                                classes="simple-card-title",
                            )
                            yield Button(
                                "Configurar e testar automaticamente",
                                id="simple-run-button",
                                variant="success",
                            )
                        with Horizontal(id="simple-secondary-actions"):
                            yield Button("Procurar placa novamente", id="simple-refresh-button")
                            yield Button("Opções avançadas", id="advanced-mode-button")
                        with Vertical(classes="simple-card simple-card-primary"):
                            yield Label("Placa encontrada", classes="simple-card-title")
                            yield Select(
                                [("Procurando uma placa ESP32…", NO_AUTOMATIC_DEVICE)],
                                value=NO_AUTOMATIC_DEVICE,
                                allow_blank=False,
                                id="simple-device-select",
                            )
                            yield Static(
                                "Procurando uma placa conectada por USB…",
                                id="simple-device-summary",
                            )
                        with Vertical(classes="simple-card"):
                            yield Label("O que será feito", classes="simple-card-title")
                            yield Static(
                                "Detecção → build/gravação apenas se necessário → configuração → "
                                "testes na placa → monitor.",
                                id="simple-plan-summary",
                            )
                        with Vertical(
                            id="simple-hardware-tests",
                            classes="simple-card",
                        ):
                            yield Label("Validação física", classes="simple-card-title")
                            yield Static(
                                "Cada card exige resposta real da placa. Quando necessário, "
                                "você só confirma o que viu.",
                                classes="hint",
                            )
                            for check_id, label in (
                                ("boot_serial", "Boot e USB"),
                                ("rgb", "LED RGB"),
                                ("ir_rx", "Receptor IR"),
                                ("ir_tx", "Transmissor IR"),
                                ("storage", "Cartão microSD"),
                                ("relays", "Relés"),
                                ("network_integration", "Rede e integrações"),
                                ("csi", "CSI (somente sala vazia)"),
                                ("concurrency", "Teste simultâneo de 30 minutos"),
                            ):
                                yield Button(
                                    f"○ {label} — Aguardando",
                                    id=f"hardware-card-{check_id.replace('_', '-')}",
                                    classes="hardware-test-card",
                                    disabled=True,
                                )
                        yield ProgressBar(id="simple-progress")
                        yield Static("Aguardando a placa.", id="simple-status-line")
                        yield RichLog(
                            highlight=True,
                            markup=False,
                            wrap=True,
                            max_lines=400,
                            id="simple-live-log",
                        )

                with TabPane("Dispositivos", id="devices-tab"):
                    with Horizontal(classes="pane"):
                        with Vertical(classes="column wide"):
                            yield Label("Catálogo de dispositivos", classes="section-title")
                            yield Static(
                                "Assistente: 1 Dispositivo → 2 Rede → 3 Pinos → 4 Integrações → "
                                "5 Armazenamento → 6 Validar/Build",
                                id="wizard-progress",
                                classes="hint",
                            )
                            yield DataTable(id="device-table")
                            with Horizontal(classes="button-row"):
                                yield Button("Novo", id="new-profile-button", variant="primary")
                                yield Button("Clonar", id="clone-profile-button")
                                yield Button("Salvar", id="save-profile-button", variant="success")
                                yield Button("Validar", id="validate-profile-button")
                                yield Button("Excluir", id="delete-profile-button", variant="error")
                        with VerticalScroll(classes="column"):
                            yield Label("Identidade", classes="section-title")
                            yield Label("Nome amigável")
                            yield Input(value=profile.friendly_name, id="friendly-name-input", classes="field")
                            yield Label("Device ID (derivado do eFuse MAC)")
                            yield Static(profile.device_id or "Ainda não pareado", id="device-id-static")
                            yield Label("eFuse MAC")
                            yield Input(
                                value=profile.association.efuse_mac,
                                placeholder="aa:bb:cc:dd:ee:ff",
                                id="efuse-mac-input",
                                classes="field",
                            )
                            yield Button("Parear placa", id="pair-board-button", variant="primary")
                            yield Label("Alvo")
                            yield Select(
                                [(name, name) for name in api.get_profile_names()],
                                value=profile.target.profile,
                                allow_blank=False,
                                id="device-target-select",
                            )
                            yield Label("Placa física / variante")
                            yield Select(
                                [
                                    ("Genérica — GPIO27 permanece reservado", "generic"),
                                    (
                                        "ESP32-C5-DevKitC-1 — RGB integrado no GPIO27",
                                        "esp32-c5-devkitc-1",
                                    ),
                                ],
                                value=profile.target.board_variant,
                                allow_blank=False,
                                id="board-variant-select",
                            )
                            yield Static(
                                "Para usar o RGB que já vem soldado na C5-DevKitC-1, "
                                "selecione essa variante; não é necessário ligar outro LED.",
                                classes="hint",
                            )
                            yield Label("Importar/exportar (JSON ou ZIP)", classes="section-title")
                            yield Input(placeholder="C:\\caminho\\perfil.json", id="profile-path-input")
                            with Horizontal(classes="button-row"):
                                yield Button("Importar cópia", id="import-profile-button")
                                yield Button("Exportar", id="export-profile-button")
                            with Horizontal(classes="button-row"):
                                yield Button("Exportar catálogo ZIP", id="export-catalogue-button")
                                yield Button("Planejar lote", id="batch-plan-button")
                                yield Button("Aplicar lote", id="batch-apply-button", variant="warning")
                            yield Static("", id="profile-message")

                with TabPane("Configuração", id="config-tab"):
                    with Horizontal(classes="pane"):
                        with VerticalScroll(classes="column"):
                            yield Label("Wi-Fi", classes="section-title")
                            yield Label("SSID")
                            yield Input(value=profile.network.wifi.ssid, id="wifi-ssid-input", classes="field")
                            yield Label("Senha (Credential Manager/keyring)")
                            yield Input(password=True, id="wifi-password-input", classes="field")
                            yield Label("Banda")
                            yield Select(
                                [("Automática", "auto"), ("2,4 GHz", "2.4ghz"), ("5 GHz", "5ghz")],
                                value=profile.network.wifi.band,
                                allow_blank=False,
                                id="wifi-band-select",
                            )
                            yield Switch(value=profile.network.wifi.prefer_ht40, id="ht40-switch")
                            yield Label("Preferir HT40", classes="hint")
                            yield Switch(value=profile.network.wifi.captive_portal_fallback, id="captive-switch")
                            yield Label("Fallback por captive portal", classes="hint")
                        with VerticalScroll(classes="column"):
                            yield Label("UDP e CSI", classes="section-title")
                            yield Label("Host UDP")
                            yield Input(value=profile.network.udp.host, id="udp-host-input", classes="field")
                            yield Label("Porta UDP")
                            yield Input(value=str(profile.network.udp.port), id="udp-port-input", classes="field")
                            yield Label("Frequência de envio (Hz)")
                            yield Input(
                                value=str(profile.network.udp.send_frequency_hz),
                                id="send-frequency-input",
                                classes="field",
                            )
                            yield Switch(value=profile.csi.enabled, id="csi-switch")
                            yield Label("CSI ativo", classes="hint")
                            yield Label("Capacidade da fila")
                            yield Input(value=str(profile.runtime.queue_capacity), id="queue-input", classes="field")
                            yield Label("LED de status", classes="section-title")
                            yield Switch(value=profile.runtime.status_led_enabled, id="led-switch")
                            yield Static(
                                "Ative somente se o LED estiver presente. No C5-DevKitC-1 integrado, informe GPIO27.",
                                classes="hint",
                            )
                            yield Label("Tipo")
                            yield Static(
                                "Endereçável / WS2812 (único driver RGB no v1)",
                                id="led-kind-static",
                                classes="hint",
                            )
                            yield Label("GPIO data (WS2812)")
                            yield Input(
                                value="" if profile.runtime.status_led.data_gpio is None else str(profile.runtime.status_led.data_gpio),
                                id="led-gpio-input",
                                classes="field",
                            )
                            yield Label("Brilho (%)")
                            yield Input(
                                value=str(profile.runtime.status_led.brightness_percent),
                                id="led-brightness-input",
                                classes="field",
                            )
                            yield Label("Cores RGB por estado (R,G,B)", classes="section-title")
                            for state, label in (
                                ("boot", "Boot"),
                                ("online", "Online"),
                                ("offline", "Offline"),
                                ("error", "Erro"),
                                ("ir_rx", "IR recebido"),
                                ("ir_tx", "IR transmitido"),
                                ("provisioning", "Provisionamento"),
                                ("recording", "Gravação SD"),
                            ):
                                yield Label(label)
                                yield Input(
                                    value=",".join(str(channel) for channel in profile.runtime.status_led.colors[state]),
                                    id=f"led-color-{state}-input",
                                    classes="field",
                                )
                            yield Static(
                                "C5-DevKitC-1: o RGB integrado usa GPIO27 (strapping) pelo circuito oficial. "
                                "Selecione a variante explicitamente; GPIO27 continua proibido para IR/relé/SD.",
                                classes="hint",
                            )
                            yield Static(
                                "Legenda: boot=âmbar; offline/conectando=azul; online=verde; "
                                "erro=vermelho; IR RX=amarelo; IR TX=magenta; "
                                "provisionamento=ciano; gravação=roxo.",
                                classes="hint",
                            )

                with TabPane("Pinos e recursos", id="peripherals-tab"):
                    with Horizontal(classes="pane"):
                        with VerticalScroll(classes="column"):
                            yield Label("Infravermelho bruto", classes="section-title")
                            yield Static(
                                "Ative RX/TX somente após ligar o circuito e informe os GPIOs usados na fiação.",
                                classes="hint",
                            )
                            yield Switch(value=profile.ir_rx.enabled, id="ir-rx-switch")
                            yield Label("Receptor IR ativo", classes="hint")
                            yield Label("GPIO RX")
                            yield Input(
                                value="" if profile.ir_rx.gpio is None else str(profile.ir_rx.gpio),
                                id="ir-rx-gpio-input",
                                classes="field",
                            )
                            yield Label("Idle threshold (µs)")
                            yield Input(
                                value=str(profile.ir_rx.idle_threshold_us),
                                id="ir-idle-input",
                                classes="field",
                            )
                            yield Switch(value=profile.ir_tx.enabled, id="ir-tx-switch")
                            yield Label("Transmissor IR ativo", classes="hint")
                            yield Label("GPIO TX")
                            yield Input(
                                value="" if profile.ir_tx.gpio is None else str(profile.ir_tx.gpio),
                                id="ir-tx-gpio-input",
                                classes="field",
                            )
                            yield Label("Carrier (Hz)")
                            yield Input(value=str(profile.ir_tx.carrier_hz), id="ir-carrier-input", classes="field")
                            yield Label("Duty (%)")
                            yield Input(value=str(profile.ir_tx.duty_percent), id="ir-duty-input", classes="field")
                            yield Static(
                                "O LED IR exige transistor/MOSFET; não ligue carga de potência diretamente ao GPIO.",
                                classes="hint",
                            )
                        with VerticalScroll(classes="column wide"):
                            yield Label("Relés (máximo 8)", classes="section-title")
                            yield Static(
                                "Todos iniciam inativos; restore ocorre após 2 s somente com NVS/hash válidos.",
                                classes="hint",
                            )
                            for index in range(8):
                                relay = profile.relays[index] if index < len(profile.relays) else RelayConfig(
                                    relay_id=f"relay_{index + 1}", name=f"Relé {index + 1}", enabled=False
                                )
                                with Horizontal(classes="relay-row"):
                                    yield Switch(value=relay.enabled, id=f"relay-{index}-switch")
                                    yield Input(value=relay.name, placeholder="Nome", id=f"relay-{index}-name")
                                    yield Input(
                                        value="" if relay.gpio is None else str(relay.gpio),
                                        placeholder="GPIO",
                                        id=f"relay-{index}-gpio",
                                    )
                                    yield Select(
                                        [("Ativo alto", "high"), ("Ativo baixo", "low")],
                                        value="high" if relay.active_high else "low",
                                        allow_blank=False,
                                        id=f"relay-{index}-polarity",
                                    )
                                    yield Input(
                                        value=str(relay.pulse_limit_ms),
                                        placeholder="Pulso máx ms",
                                        id=f"relay-{index}-pulse",
                                    )
                                    yield Switch(value=relay.restore_state, id=f"relay-{index}-restore")

                with TabPane("Integrações", id="integrations-tab"):
                    with Horizontal(classes="pane"):
                        with VerticalScroll(classes="column"):
                            yield Label("MQTT", classes="section-title")
                            yield Switch(value=profile.mqtt.enabled, id="mqtt-switch")
                            yield Label("MQTT ativo", classes="hint")
                            yield Label("Modo do broker")
                            yield Select(
                                [("Local gerenciado", "local"), ("Home Assistant", "ha"), ("Externo", "external")],
                                value=profile.mqtt.broker_mode,
                                allow_blank=False,
                                id="broker-mode-select",
                            )
                            yield Label("Host")
                            yield Input(value=profile.mqtt.broker_host, id="mqtt-host-input", classes="field")
                            yield Label("Porta")
                            yield Input(value=str(profile.mqtt.broker_port), id="mqtt-port-input", classes="field")
                            yield Switch(value=profile.mqtt.tls, id="mqtt-tls-switch")
                            yield Label("TLS", classes="hint")
                            yield Label("Usuário por dispositivo")
                            yield Input(value=profile.mqtt.username, id="mqtt-user-input", classes="field")
                            yield Label("Senha")
                            yield Input(password=True, id="mqtt-password-input", classes="field")
                            with Horizontal(classes="button-row"):
                                yield Button("Gerar credencial/ACL", id="local-broker-button")
                                yield Button("Iniciar/recarregar", id="local-broker-start-button")
                        with VerticalScroll(classes="column"):
                            yield Label("Servidor AetherSense", classes="section-title")
                            yield Label("URL base")
                            yield Input(value=profile.server.base_url, id="server-url-input", classes="field")
                            yield Label("Bearer administrativo (somente TUI)")
                            yield Input(password=True, id="server-token-input", classes="field")
                            yield Static(
                                "O onboarding gera outro token, exclusivo do dispositivo, e guarda apenas sua referência.",
                                classes="hint",
                            )
                            yield Label("Sincronização pela rede")
                            yield Input(value=profile.sync.base_url, id="sync-url-input", classes="field")
                            with Horizontal(classes="button-row"):
                                yield Button("Testar MQTT", id="test-mqtt-button")
                                yield Button("Testar servidor", id="test-server-button")
                                yield Button("Onboarding", id="onboard-button", variant="primary")
                                yield Button("Rotacionar tokens", id="rotate-token-button", variant="warning")
                            yield Static("", id="integration-result")
                            yield Static(f"Segredos: {self.secret_manager.backend_name}", classes="hint")
                            yield Label("Comissionamento físico", classes="section-title")
                            with Vertical(classes="commission-card"):
                                yield Label("Placa conectada por USB", classes="commission-card-title")
                                yield Static(
                                    "Lê o firmware, a configuração aplicada e os recursos disponíveis sem MQTT.",
                                    classes="hint",
                                )
                                yield Button("Verificar placa agora", id="commission-status-button")
                            with Vertical(classes="commission-card commission-card-primary"):
                                yield Label("Receptor e transmissor IR", classes="commission-card-title")
                                yield Static(
                                    "Aprenda e repita um sinal diretamente por USB. No perfil Meu espc5: "
                                    "OUT/S do receptor → GPIO8, VCC/+ → 3V3 e GND/- → GND. "
                                    "Confira os rótulos do módulo: a ordem dos três pinos varia. "
                                    "Não exige servidor, broker ou token.",
                                    classes="hint",
                                )
                                yield Input(
                                    placeholder="ID remoto da biblioteca (opcional)",
                                    id="commission-ir-code-input",
                                )
                                with Horizontal(classes="button-row"):
                                    yield Button("Aprender sinal", id="commission-ir-learn-button", variant="primary")
                                    yield Button("Cancelar", id="commission-ir-cancel-button")
                                    yield Button("Repetir última captura", id="commission-ir-send-button", variant="warning")
                            with Vertical(classes="commission-card"):
                                yield Label("Teste seguro de relé", classes="commission-card-title")
                                yield Input(placeholder="relay_id", id="commission-relay-id-input")
                                yield Input(value="500", placeholder="duração ms", id="commission-relay-duration-input")
                                yield Button("Testar pulso do relé…", id="commission-relay-button", variant="error")
                            yield Static("", id="commission-result")
                            yield Static(
                                "Nenhum IR/relé/SD é acionado automaticamente por Aplicar e testar. "
                                "Para o uso remoto posterior, MQTT e servidor continuam opcionais.",
                                classes="hint",
                            )

                with TabPane("Armazenamento", id="storage-tab"):
                    with Horizontal(classes="pane"):
                        with VerticalScroll(classes="column"):
                            yield Label("microSD — FAT32/SDSPI", classes="section-title")
                            yield Switch(value=profile.storage.enabled, id="sd-switch")
                            yield Static(
                                "Se não houver cartão conectado, deixe desativado. SDSPI ativo exige os quatro GPIOs.",
                                classes="hint",
                            )
                            for label, field_name in (
                                ("SCLK GPIO", "sclk_gpio"),
                                ("MOSI GPIO", "mosi_gpio"),
                                ("MISO GPIO", "miso_gpio"),
                                ("CS GPIO", "cs_gpio"),
                            ):
                                value = getattr(profile.storage, field_name)
                                yield Label(label)
                                yield Input(
                                    value="" if value is None else str(value),
                                    id=f"sd-{field_name.replace('_gpio', '')}-input",
                                    classes="field",
                                )
                            yield Label("Rotacionar arquivo após bytes")
                            yield Input(value=str(profile.storage.rotate_bytes), id="sd-rotate-input", classes="field")
                            yield Static("A TUI nunca formata o cartão automaticamente.", classes="hint")
                        with VerticalScroll(classes="column"):
                            yield Label("Gravação", classes="section-title")
                            yield Switch(value=profile.recording.events_enabled, id="record-events-switch")
                            yield Label("Gravar eventos", classes="hint")
                            yield Switch(value=profile.recording.raw_csi_enabled, id="record-csi-switch")
                            yield Label("Gravar CSI bruto", classes="hint")
                            yield Label("Modo de privacidade")
                            yield Select(
                                [("Somente eventos", "events_only"), ("Bancada", "bench"), ("Sala vazia", "empty_room")],
                                value=profile.recording.privacy_mode,
                                allow_blank=False,
                                id="privacy-select",
                            )
                            yield Switch(value=profile.sync.network_default, id="network-sync-switch")
                            yield Label("Rede é o transporte padrão", classes="hint")
                            yield Label("Bloco de upload (bytes)")
                            yield Input(value=str(profile.sync.block_bytes), id="sync-block-input", classes="field")
                            yield Switch(value=profile.sync.delete_after_sync, id="delete-synced-switch")
                            yield Label("Apagar somente após confirmação", classes="hint")
                            yield Label("Cartão/pasta no computador")
                            yield Input(placeholder="D:\\ ou /media/sd", id="sd-path-input", classes="field")
                            with Horizontal(classes="button-row"):
                                yield Button("Inspecionar SD", id="inspect-sd-button")
                                yield Button("Importar SD", id="import-sd-button")
                            with Horizontal(classes="button-row"):
                                yield Button("Testar SD no ESP", id="commission-sd-status-button")
                                yield Button("Sincronizar pela rede", id="commission-sd-sync-button", variant="warning")
                            yield Static("", id="storage-result")

                with TabPane("Executar", id="build-tab"):
                    with Horizontal(classes="pane"):
                        with VerticalScroll(classes="column"):
                            yield Label("Fluxo principal", classes="section-title")
                            yield Static(
                                "Aplicar alterações escolhe automaticamente build/flash ou "
                                "provisionamento. Executar no ESP faz build + flash + monitor.",
                                classes="hint",
                            )
                            with Vertical(id="primary-action-cards"):
                                with Vertical(classes="action-card action-card-primary"):
                                    yield Label("Executar no ESP", classes="action-card-title")
                                    yield Static(
                                        "Aplica o perfil salvo (flash + NVS) e abre o monitor.",
                                        classes="action-card-description",
                                    )
                                    yield Button(
                                        "Executar no ESP", id="run-button", variant="success"
                                    )
                                with Vertical(classes="action-card"):
                                    yield Label("Aplicar configuração", classes="action-card-title")
                                    yield Static(
                                        "Executa somente build/flash e provisionamento pendentes.",
                                        classes="action-card-description",
                                    )
                                    yield Button(
                                        "Aplicar alterações",
                                        id="apply-button",
                                        variant="primary",
                                    )
                                with Vertical(classes="action-card"):
                                    yield Label("Monitor serial", classes="action-card-title")
                                    yield Static(
                                        "Acompanha a placa conectada sem recompilar ou regravar.",
                                        classes="action-card-description",
                                    )
                                    yield Button("Somente monitorar", id="monitor-button")
                            yield Button("Cancelar ação atual", id="cancel-button", variant="warning")
                            yield Label("Ações avançadas", classes="section-title")
                            yield Select(
                                [
                                    (spec.label, name)
                                    for name, spec in api.ACTIONS.items()
                                    if name not in api.PROFILE_SERVICE_ACTIONS
                                    and name not in {"run", "monitor"}
                                ],
                                value=(
                                    self.options.action
                                    if self.options.action
                                    not in api.PROFILE_SERVICE_ACTIONS | {"run", "monitor"}
                                    else "build"
                                ),
                                allow_blank=False,
                                id="action-select",
                            )
                            yield Static(f"Alvo: {profile.target.profile}", id="build-target-static")
                            yield Label("Porta serial")
                            yield Select(
                                self.port_select_options(), value=AUTO_PORT, allow_blank=False, id="port-select"
                            )
                            yield Input(placeholder="COM5 / /dev/ttyUSB0", id="port-input", classes="field")
                            yield Label("Timeout (s)")
                            yield Input(
                                value="" if self.options.timeout is None else str(self.options.timeout),
                                id="timeout-input",
                                classes="field",
                            )
                            yield Label("Comando avançado planejado")
                            yield Static("", id="command-preview")
                            with Horizontal(classes="button-row"):
                                yield Button("Executar ação avançada", id="advanced-run-button")
                            yield Static("", id="validation-summary")
                        with Vertical(classes="column wide"):
                            yield ProgressBar(id="build-progress")
                            with Horizontal(id="loading-line"):
                                yield LoadingIndicator(id="loading-indicator")
                                yield Static("Ocioso", id="status-line")
                            yield RichLog(highlight=True, markup=False, wrap=True, max_lines=1000, id="run-live-log")

                with TabPane("Diagnóstico", id="diagnostics-tab"):
                    with Horizontal(classes="pane"):
                        with Vertical(classes="column"):
                            yield Label("Portas seriais", classes="section-title")
                            yield DataTable(id="ports-table")
                            yield Button("Atualizar portas", id="refresh-ports-button")
                            yield Static("", id="hash-summary")
                        with Vertical(classes="column wide"):
                            yield Label("Campo / valor efetivo / origem", classes="section-title")
                            yield DataTable(id="env-table")
                            yield Label("Validação", classes="section-title")
                            yield Static(
                                "Selecione um erro e pressione Enter para abrir o campo. Desative recursos sem hardware conectado.",
                                classes="hint",
                            )
                            yield DataTable(id="issues-table")

                with TabPane("Ajuda", id="help-tab"):
                    yield Markdown(self.help_markdown())
            with Horizontal(id="wizard-controls"):
                yield Button("Início", id="wizard-home-button")
                yield Button("Seções", id="wizard-sections-button", variant="primary")
                yield Button("← Anterior", id="wizard-previous-button")
                yield Static("1/7 — Identidade e placa", id="wizard-step-label")
                yield Button("Validar", id="wizard-validate-button")
                yield Button("Próxima →", id="wizard-next-step-button", variant="primary")
                yield Button(
                    "Aplicar",
                    id="wizard-finish-button",
                    variant="success",
                )
            yield Footer(compact=True, show_command_palette=False)

        def on_mount(self) -> None:
            self.query_one("#loading-indicator", LoadingIndicator).styles.display = "none"
            self.refresh_catalogue()
            self.populate_form(self.current_profile)
            self.refresh_ports()
            self.refresh_diagnostics()
            self.update_command_preview()
            self.set_advanced_mode(False)
            self.call_after_refresh(self.finish_initial_loading)

        def finish_initial_loading(self) -> None:
            self._loading_form = False

        def activate_content_page(self, tab_id: str) -> None:
            """Change pages without an old pane's focused widget switching it back."""

            self.set_focus(None)
            self.query_one("#main-tabs", TabbedContent).active = tab_id
            if tab_id in {item[0] for item in self.WIZARD_STEPS} | {"help-tab"}:
                self._last_advanced_tab = tab_id
                self.update_wizard_controls(tab_id)
                self.call_after_refresh(
                    self.query_one("#wizard-sections-button", Button).focus
                )

        def set_advanced_mode(self, enabled: bool) -> None:
            """Switch between one-click and advanced content without a tab strip."""

            tabs = self.query_one("#main-tabs", TabbedContent)
            if enabled:
                self._advanced_mode = True
                target = self._last_advanced_tab
                valid_targets = {item[0] for item in self.WIZARD_STEPS} | {"help-tab"}
                if target not in valid_targets:
                    target = "devices-tab"
                self.activate_content_page(target)
            else:
                active = str(tabs.active)
                if active in {item[0] for item in self.WIZARD_STEPS} | {"help-tab"}:
                    self._last_advanced_tab = active
                self._advanced_mode = False
                self.set_focus(None)
                tabs.active = "simple-tab"
                self.query_one("#wizard-controls").styles.display = "none"
                self.call_after_refresh(self.query_one("#simple-run-button", Button).focus)

        def help_markdown(self) -> str:
            actions = "\n".join(f"- `{name}` — {spec.description}" for name, spec in api.ACTIONS.items())
            return f"""# AetherSense build.py

O assistente salva múltiplos dispositivos em `{self.profile_store.root}`. Senhas e tokens
ficam no backend `{self.secret_manager.backend_name}` e nunca entram no perfil, exportação
ou log. Perfis usam o schema `aethersense.device_profile.v1`.

## Fluxo

1. Cadastre/pareie o dispositivo e selecione C5 ou WROOM.
2. Abra "Seções" e escolha o card de rede, acessórios, integrações ou microSD.
3. Valide; erros de pino impedem flash/erase/run/rescue.
4. Salve e execute somente o passo indicado pelo status.

CSI bruto local só é aceito para bancada ou sala vazia. O microSD não é formatado
automaticamente. Relés de rede elétrica exigem módulo certificado e isolado.

## Ações preservadas

{actions}
"""

        def wizard_index(self, tab_id: str | None = None) -> int:
            active = tab_id or str(self.query_one("#main-tabs", TabbedContent).active)
            for index, (candidate, _label) in enumerate(self.WIZARD_STEPS):
                if candidate == active:
                    return index
            return 0

        def update_wizard_controls(self, tab_id: str | None = None) -> None:
            controls = self.query_one("#wizard-controls")
            active = tab_id or str(self.query_one("#main-tabs", TabbedContent).active)
            if active == "simple-tab" or not self._advanced_mode:
                controls.styles.display = "none"
                return
            controls.styles.display = "block"
            previous = self.query_one("#wizard-previous-button", Button)
            validate = self.query_one("#wizard-validate-button", Button)
            next_button = self.query_one("#wizard-next-step-button", Button)
            finish = self.query_one("#wizard-finish-button", Button)
            previous.label = "←"
            previous.tooltip = "Etapa anterior"
            next_button.label = "→"
            next_button.tooltip = "Próxima etapa"
            if active == "help-tab":
                self.query_one("#wizard-step-label", Static).update("Ajuda")
                for button in (previous, validate, next_button, finish):
                    button.styles.display = "none"
                return
            if active not in {item[0] for item in self.WIZARD_STEPS}:
                controls.styles.display = "none"
                return
            index = self.wizard_index(active)
            _tab, label = self.WIZARD_STEPS[index]
            self.query_one("#wizard-step-label", Static).update(
                f"{index + 1}/{len(self.WIZARD_STEPS)}"
            )
            self.query_one("#wizard-progress", Static).update(
                f"Assistente de primeiro uso: {label}. F6 avança; Shift+F6 volta."
            )
            previous.styles.display = "block"
            previous.disabled = index == 0
            validate.styles.display = "block"
            next_button.styles.display = (
                "none" if index == len(self.WIZARD_STEPS) - 1 else "block"
            )
            finish.styles.display = (
                "block" if index == len(self.WIZARD_STEPS) - 1 else "none"
            )

        def show_advanced_sections(self) -> None:
            current = str(self.query_one("#main-tabs", TabbedContent).active)
            self.push_screen(
                AdvancedSectionsScreen(current),
                self.complete_advanced_section_choice,
            )

        def complete_advanced_section_choice(self, tab_id: str | None) -> None:
            if tab_id is None:
                return
            valid_tabs = {item[0] for item in self.WIZARD_STEPS} | {"help-tab"}
            if tab_id not in valid_tabs:
                self.notify("Seção avançada desconhecida.", severity="error")
                return
            self.activate_content_page(tab_id)

        def wizard_issues(self, index: int) -> list:
            profile = self.collect_profile()
            prefixes = {
                0: ("schema", "friendly_name", "device_id", "association", "target"),
                1: ("network", "csi", "runtime"),
                2: ("ir_rx", "ir_tx", "relays"),
                3: ("mqtt", "server", "sync"),
                4: ("storage", "recording", "sync"),
                5: ("target", "device_id", "association"),
                6: ("",),
            }[index]
            return [
                issue
                for issue in validate_profile(profile)
                if any(issue.path.startswith(prefix) for prefix in prefixes)
            ]

        def validate_wizard_step(self, index: int | None = None) -> bool:
            current = self.wizard_index() if index is None else index
            issues = self.wizard_issues(current)
            errors = [issue for issue in issues if issue.severity == "error"]
            if errors:
                self.query_one("#profile-message", Static).update(
                    "Etapa inválida: " + "; ".join(str(issue) for issue in errors[:3])
                )
                self.notify(
                    f"Corrija {len(errors)} problema(s) nesta etapa.", severity="error"
                )
                self.open_validation_issue(errors[0])
                return False
            self.autosave_draft()
            self.notify("Etapa válida; rascunho atualizado.", severity="information")
            return True

        def go_to_wizard_step(self, index: int) -> None:
            if not self._advanced_mode:
                self.set_advanced_mode(True)
            index = max(0, min(index, len(self.WIZARD_STEPS) - 1))
            tab_id, _label = self.WIZARD_STEPS[index]
            self.activate_content_page(tab_id)

        def action_wizard_previous(self) -> None:
            self.go_to_wizard_step(self.wizard_index() - 1)

        def action_wizard_next(self) -> None:
            index = self.wizard_index()
            if self.validate_wizard_step(index):
                self.go_to_wizard_step(index + 1)

        def finish_wizard(self) -> None:
            if not self.validate_and_report():
                self.notify("Revise os erros antes de aplicar.", severity="error")
                return
            self.save_current(notify=False)
            self._test_after_apply = True
            if self.current_profile.server.base_url:
                self.start_onboarding(continue_apply=True)
            else:
                self.continue_apply_steps()

        def port_select_options(self) -> list[tuple[str, str]]:
            options: list[tuple[str, str]] = [("Automática", AUTO_PORT)]
            for info in self._port_infos:
                device = info.get("device", "")
                if device:
                    description = info.get("description") or info.get("manufacturer") or "serial"
                    options.append((f"{device} — {description}", device))
            options.append(("Manual…", MANUAL_PORT))
            return options

        def secret_values(self) -> tuple[str, ...]:
            values = []
            for selector in ("#wifi-password-input", "#mqtt-password-input", "#server-token-input"):
                try:
                    value = self.query_one(selector, Input).value
                except Exception:
                    value = ""
                if value:
                    values.append(value)
            return tuple(values)

        def collect_profile(self) -> DeviceProfile:
            p = self.current_profile
            p.friendly_name = self.query_one("#friendly-name-input", Input).value.strip() or "Novo dispositivo"
            p.association.efuse_mac = self.query_one("#efuse-mac-input", Input).value.strip().lower()
            derived = device_id_from_mac(p.association.efuse_mac)
            if derived and not p.device_id:
                p.device_id = derived
            p.target.profile = str(self.query_one("#device-target-select", Select).value)
            p.target.board_variant = str(
                self.query_one("#board-variant-select", Select).value
            )
            p.network.wifi.ssid = self.query_one("#wifi-ssid-input", Input).value.strip()
            p.network.wifi.band = str(self.query_one("#wifi-band-select", Select).value)
            p.network.wifi.prefer_ht40 = self.query_one("#ht40-switch", Switch).value
            p.network.wifi.captive_portal_fallback = self.query_one("#captive-switch", Switch).value
            p.network.udp.host = self.query_one("#udp-host-input", Input).value.strip()
            p.network.udp.port = _integer(self.query_one("#udp-port-input", Input).value, 5005)
            p.network.udp.send_frequency_hz = _integer(
                self.query_one("#send-frequency-input", Input).value, 10
            )
            p.csi.enabled = self.query_one("#csi-switch", Switch).value
            p.runtime.queue_capacity = _integer(self.query_one("#queue-input", Input).value, 256)
            led_enabled = self.query_one("#led-switch", Switch).value
            led_kind = "ws2812"
            led_data_gpio = _pin(self.query_one("#led-gpio-input", Input).value)
            p.runtime.status_led = StatusLedConfig(
                enabled=led_enabled,
                kind=led_kind,
                data_gpio=led_data_gpio,
                red_gpio=None,
                green_gpio=None,
                blue_gpio=None,
                brightness_percent=_integer(
                    self.query_one("#led-brightness-input", Input).value, 25
                ),
                colors={
                    state: _rgb(
                        self.query_one(f"#led-color-{state}-input", Input).value,
                        default,
                    )
                    for state, default in DEFAULT_STATUS_LED_COLORS.items()
                },
            )
            p.runtime.status_led_enabled = led_enabled
            p.runtime.status_led_gpio = led_data_gpio if led_kind == "ws2812" else None
            p.ir_rx.enabled = self.query_one("#ir-rx-switch", Switch).value
            p.ir_rx.gpio = _pin(self.query_one("#ir-rx-gpio-input", Input).value)
            p.ir_rx.idle_threshold_us = _integer(self.query_one("#ir-idle-input", Input).value, 12_000)
            p.ir_tx.enabled = self.query_one("#ir-tx-switch", Switch).value
            p.ir_tx.gpio = _pin(self.query_one("#ir-tx-gpio-input", Input).value)
            p.ir_tx.carrier_hz = _integer(self.query_one("#ir-carrier-input", Input).value, 38_000)
            p.ir_tx.duty_percent = _integer(self.query_one("#ir-duty-input", Input).value, 33)
            p.relays = [
                RelayConfig(
                    relay_id=f"relay_{index + 1}",
                    name=self.query_one(f"#relay-{index}-name", Input).value.strip() or f"Relé {index + 1}",
                    enabled=self.query_one(f"#relay-{index}-switch", Switch).value,
                    gpio=_pin(self.query_one(f"#relay-{index}-gpio", Input).value),
                    active_high=str(self.query_one(f"#relay-{index}-polarity", Select).value) == "high",
                    pulse_limit_ms=_integer(self.query_one(f"#relay-{index}-pulse", Input).value, 60_000),
                    restore_state=self.query_one(f"#relay-{index}-restore", Switch).value,
                )
                for index in range(8)
            ]
            p.mqtt.enabled = self.query_one("#mqtt-switch", Switch).value
            p.mqtt.broker_mode = str(self.query_one("#broker-mode-select", Select).value)
            p.mqtt.broker_host = self.query_one("#mqtt-host-input", Input).value.strip()
            p.mqtt.broker_port = _integer(self.query_one("#mqtt-port-input", Input).value, 1883)
            p.mqtt.tls = self.query_one("#mqtt-tls-switch", Switch).value
            p.mqtt.username = self.query_one("#mqtt-user-input", Input).value.strip()
            p.server.base_url = self.query_one("#server-url-input", Input).value.strip().rstrip("/")
            p.sync.base_url = self.query_one("#sync-url-input", Input).value.strip().rstrip("/")
            p.storage.enabled = self.query_one("#sd-switch", Switch).value
            p.storage.sclk_gpio = _pin(self.query_one("#sd-sclk-input", Input).value)
            p.storage.mosi_gpio = _pin(self.query_one("#sd-mosi-input", Input).value)
            p.storage.miso_gpio = _pin(self.query_one("#sd-miso-input", Input).value)
            p.storage.cs_gpio = _pin(self.query_one("#sd-cs-input", Input).value)
            p.storage.rotate_bytes = _integer(self.query_one("#sd-rotate-input", Input).value, 4 * 1024 * 1024)
            p.recording.events_enabled = self.query_one("#record-events-switch", Switch).value
            p.recording.raw_csi_enabled = self.query_one("#record-csi-switch", Switch).value
            p.csi.raw_recording_enabled = p.recording.raw_csi_enabled
            p.recording.privacy_mode = str(self.query_one("#privacy-select", Select).value)
            p.csi.empty_room_only = p.recording.privacy_mode in {"bench", "empty_room"}
            p.sync.network_default = self.query_one("#network-sync-switch", Switch).value
            p.sync.block_bytes = _integer(self.query_one("#sync-block-input", Input).value, 1024 * 1024)
            p.sync.delete_after_sync = self.query_one("#delete-synced-switch", Switch).value
            p.refresh_features()
            return p

        def populate_form(self, profile: DeviceProfile) -> None:
            previous_loading_state = self._loading_form
            self._loading_form = True
            self.current_profile = profile
            values = {
                "#friendly-name-input": profile.friendly_name,
                "#efuse-mac-input": profile.association.efuse_mac,
                "#wifi-ssid-input": profile.network.wifi.ssid,
                "#wifi-password-input": self.secret_manager.get(profile.network.wifi.password_ref) or "",
                "#udp-host-input": profile.network.udp.host,
                "#udp-port-input": str(profile.network.udp.port),
                "#send-frequency-input": str(profile.network.udp.send_frequency_hz),
                "#queue-input": str(profile.runtime.queue_capacity),
                "#led-gpio-input": "" if profile.runtime.status_led.data_gpio is None else str(profile.runtime.status_led.data_gpio),
                "#led-brightness-input": str(profile.runtime.status_led.brightness_percent),
                "#ir-rx-gpio-input": "" if profile.ir_rx.gpio is None else str(profile.ir_rx.gpio),
                "#ir-idle-input": str(profile.ir_rx.idle_threshold_us),
                "#ir-tx-gpio-input": "" if profile.ir_tx.gpio is None else str(profile.ir_tx.gpio),
                "#ir-carrier-input": str(profile.ir_tx.carrier_hz),
                "#ir-duty-input": str(profile.ir_tx.duty_percent),
                "#mqtt-host-input": profile.mqtt.broker_host,
                "#mqtt-port-input": str(profile.mqtt.broker_port),
                "#mqtt-user-input": profile.mqtt.username,
                "#mqtt-password-input": self.secret_manager.get(profile.mqtt.password_ref) or "",
                "#server-url-input": profile.server.base_url,
                "#server-token-input": self.secret_manager.get(
                    profile.server.admin_bearer_token_ref
                )
                or "",
                "#sync-url-input": profile.sync.base_url,
                "#sd-sclk-input": "" if profile.storage.sclk_gpio is None else str(profile.storage.sclk_gpio),
                "#sd-mosi-input": "" if profile.storage.mosi_gpio is None else str(profile.storage.mosi_gpio),
                "#sd-miso-input": "" if profile.storage.miso_gpio is None else str(profile.storage.miso_gpio),
                "#sd-cs-input": "" if profile.storage.cs_gpio is None else str(profile.storage.cs_gpio),
                "#sd-rotate-input": str(profile.storage.rotate_bytes),
                "#sync-block-input": str(profile.sync.block_bytes),
            }
            for selector, value in values.items():
                self.query_one(selector, Input).value = value
            selects = {
                "#device-target-select": profile.target.profile,
                "#wifi-band-select": profile.network.wifi.band,
                "#broker-mode-select": profile.mqtt.broker_mode,
                "#privacy-select": profile.recording.privacy_mode,
                "#board-variant-select": profile.target.board_variant,
            }
            for selector, value in selects.items():
                self.query_one(selector, Select).value = value
            switches = {
                "#ht40-switch": profile.network.wifi.prefer_ht40,
                "#captive-switch": profile.network.wifi.captive_portal_fallback,
                "#csi-switch": profile.csi.enabled,
                "#led-switch": profile.runtime.status_led_enabled,
                "#ir-rx-switch": profile.ir_rx.enabled,
                "#ir-tx-switch": profile.ir_tx.enabled,
                "#mqtt-switch": profile.mqtt.enabled,
                "#mqtt-tls-switch": profile.mqtt.tls,
                "#sd-switch": profile.storage.enabled,
                "#record-events-switch": profile.recording.events_enabled,
                "#record-csi-switch": profile.recording.raw_csi_enabled,
                "#network-sync-switch": profile.sync.network_default,
                "#delete-synced-switch": profile.sync.delete_after_sync,
            }
            for selector, value in switches.items():
                self.query_one(selector, Switch).value = value
            for state, color in profile.runtime.status_led.colors.items():
                self.query_one(f"#led-color-{state}-input", Input).value = ",".join(
                    str(channel) for channel in color
                )
            for index in range(8):
                relay = profile.relays[index] if index < len(profile.relays) else RelayConfig(
                    relay_id=f"relay_{index + 1}", name=f"Relé {index + 1}", enabled=False
                )
                self.query_one(f"#relay-{index}-switch", Switch).value = relay.enabled
                self.query_one(f"#relay-{index}-name", Input).value = relay.name
                self.query_one(f"#relay-{index}-gpio", Input).value = "" if relay.gpio is None else str(relay.gpio)
                self.query_one(f"#relay-{index}-polarity", Select).value = "high" if relay.active_high else "low"
                self.query_one(f"#relay-{index}-pulse", Input).value = str(relay.pulse_limit_ms)
                self.query_one(f"#relay-{index}-restore", Switch).value = relay.restore_state
            self.query_one("#device-id-static", Static).update(profile.device_id or "Ainda não pareado")
            self.query_one("#build-target-static", Static).update(f"Alvo: {profile.target.profile}")
            self._loading_form = previous_loading_state
            self.refresh_diagnostics()
            self.update_command_preview()

        def persist_secrets(self, profile: DeviceProfile) -> None:
            bindings = (
                (
                    "#wifi-password-input",
                    "wifi-password",
                    "wifi_password",
                    profile.network.wifi,
                    "password_ref",
                ),
                (
                    "#mqtt-password-input",
                    "mqtt-password",
                    "mqtt_password",
                    profile.mqtt,
                    "password_ref",
                ),
                (
                    "#server-token-input",
                    "server-admin-bearer",
                    None,
                    profile.server,
                    "admin_bearer_token_ref",
                ),
            )
            for selector, kind, revision_kind, owner, attribute in bindings:
                value = self.query_one(selector, Input).value
                reference = getattr(owner, attribute) or self.secret_manager.reference(profile.profile_id, kind)
                if value:
                    if revision_kind is None:
                        self.secret_manager.set(reference, value)
                    else:
                        profile.secret_revisions[revision_kind] = (
                            self.secret_manager.set_with_revision(
                                reference,
                                value,
                                profile.secret_revisions.get(revision_kind, ""),
                            )
                        )
                    setattr(owner, attribute, reference)
            synchronise_secret_revisions(profile, self.secret_manager)

        def save_current(self, *, notify: bool = True) -> bool:
            profile = self.collect_profile()
            issues = validate_profile(profile)
            self.persist_secrets(profile)
            if has_errors(issues):
                self.profile_store.save_draft(profile)
                self.current_is_draft = True
                if notify:
                    self.notify(f"Perfil salvo como rascunho: {len(issues)} problema(s).", severity="error")
            else:
                self.profile_store.save(profile)
                self.current_is_draft = False
                if notify:
                    self.notify("Perfil salvo.", severity="information")
            self.refresh_catalogue()
            self.refresh_diagnostics()
            return not has_errors(issues)

        def complete_profile_import(self, result: dict[str, bool] | None) -> None:
            preview = self._pending_import_preview
            self._pending_import_preview = None
            if not result or not result.get("confirmed") or preview is None:
                self.query_one("#profile-message", Static).update("Importação cancelada; nada foi alterado.")
                return
            try:
                self.current_profile = self.profile_store.import_profile(
                    preview.source,
                    copy_profile=bool(result.get("copy", True)),
                )
                self.current_is_draft = True
                self.query_one("#profile-message", Static).update(
                    "Importado como cópia." if result.get("copy", True) else "Perfil importado preservando UUID."
                )
                self.refresh_catalogue()
                self.populate_form(self.current_profile)
            except Exception as exc:
                self.notify(f"Falha ao importar: {exc}", severity="error")

        def complete_catalogue_import(self, result: dict[str, bool] | None) -> None:
            preview = getattr(self, "_pending_catalogue_preview", None)
            self._pending_catalogue_preview = None
            if not result or not result.get("confirmed") or preview is None:
                self.query_one("#profile-message", Static).update(
                    "Importação do catálogo cancelada; nada foi alterado."
                )
                return
            try:
                imported = self.profile_store.import_catalogue_zip(
                    preview.source,
                    copy_profiles=bool(result.get("copy", True)),
                )
                if imported:
                    self.current_profile = imported[0]
                    self.current_is_draft = True
                    self.populate_form(self.current_profile)
                self.refresh_catalogue()
                self.query_one("#profile-message", Static).update(
                    f"{len(imported)} perfil(is) importado(s) "
                    + ("como cópias seguras." if result.get("copy", True) else "preservando UUIDs.")
                )
            except Exception as exc:
                self.notify(f"Falha ao importar catálogo: {exc}", severity="error")

        def autosave_draft(self) -> None:
            if self._loading_form:
                return
            try:
                profile = self.collect_profile()
                self.profile_store.save_draft(profile)
                self.current_is_draft = True
                self.refresh_status_only()
            except Exception:
                # A partially typed integer can be represented by the field while
                # the previous parsed value remains in the draft.
                return

        def refresh_catalogue(self) -> None:
            table = self.query_one("#device-table", DataTable)
            table.clear(columns=True)
            table.cursor_type = "row"
            table.zebra_stripes = True
            table.add_columns("Nome", "Device ID", "Alvo", "Status")
            for profile, is_draft in self.profile_store.list(include_drafts=True):
                table.add_row(
                    profile.friendly_name,
                    profile.device_id or "—",
                    profile.target.profile,
                    profile_status(
                        profile,
                        draft=is_draft,
                        require_hardware_validation=True,
                    ),
                    key=f"device:{profile.profile_id}",
                )

        def activate_automatic_candidate(
            self, candidate: AutomaticDeviceCandidate
        ) -> None:
            """Load the connected profile or create a safe first-use draft."""

            if candidate.profile_id:
                profile = self.profile_store.load(candidate.profile_id)
                is_draft = not self.profile_store.profile_path(candidate.profile_id).exists()
                self._automatic_profile_created = (
                    candidate.profile_id in self._automatic_profile_ids
                )
            else:
                profile, created = prepare_automatic_profile(
                    candidate,
                    current_profile=self.current_profile,
                    current_is_draft=self.current_is_draft,
                )
                is_draft = True
                self._automatic_profile_created = True
                self._automatic_profile_ids.add(profile.profile_id)
                self.profile_store.save_draft(profile)

            changed_profile = profile.profile_id != self.current_profile.profile_id
            self.current_profile = profile
            self.current_is_draft = is_draft
            if changed_profile or candidate.profile_id is None:
                self.populate_form(profile)

            port_select = self.query_one("#port-select", Select)
            values = {value for _label, value in self.port_select_options()}
            if candidate.port in values:
                with self.prevent(Select.Changed):
                    port_select.value = candidate.port

            self.query_one("#simple-device-summary", Static).update(candidate.detail)
            self.refresh_simple_plan(candidate)

        def refresh_simple_plan(
            self, candidate: AutomaticDeviceCandidate | None = None
        ) -> None:
            if candidate is None:
                key = str(self.query_one("#simple-device-select", Select).value)
                candidate = self._automatic_candidates.get(key)
            if candidate is None:
                return
            profile = self.current_profile
            status = profile_status(
                profile,
                draft=self.current_is_draft,
                require_hardware_validation=True,
            )
            accessories = []
            if profile.ir_rx.enabled or profile.ir_tx.enabled:
                accessories.append("infravermelho")
            if any(relay.enabled for relay in profile.relays):
                accessories.append("relés")
            if profile.storage.enabled:
                accessories.append("microSD")
            accessory_text = (
                ", ".join(accessories) if accessories else "nenhum acessório externo ativado"
            )
            wifi_text = (
                "Wi-Fi salvo"
                if profile.network.wifi.ssid
                else "Wi-Fi pelo portal de configuração no primeiro boot"
            )
            plans = {
                "Rascunho": "identificar a placa, compilar, gravar, configurar e abrir o monitor",
                "Inválido": "revisar uma configuração antiga que está incompleta",
                "Requer build": "compilar, gravar, configurar e abrir o monitor",
                "Requer provisionamento": "enviar a configuração e abrir o monitor",
                "Sincronizado": "abrir o monitor; firmware e configuração já estão atualizados",
                "Offline": "reconectar e abrir o monitor",
            }
            plan = plans.get(status, "preparar e executar a placa")
            self.query_one("#simple-plan-summary", Static).update(
                f"Perfil: {profile.friendly_name}. {wifi_text}. Recursos: {accessory_text}.\n"
                f"Ao continuar, o build.py vai {plan}."
            )

        def refresh_simple_setup(self) -> None:
            """Refresh the no-jargon board choice shown on the start page."""

            candidates = build_automatic_candidates(
                self.profile_store.list(include_drafts=True), self._port_infos
            )
            self._automatic_candidates = {item.key: item for item in candidates}
            selector = self.query_one("#simple-device-select", Select)
            previous = str(selector.value)
            preferred = choose_automatic_candidate(
                candidates, current_profile_id=self.current_profile.profile_id
            )

            if not candidates:
                with self.prevent(Select.Changed):
                    selector.set_options(
                        [("Nenhuma placa conectada", NO_AUTOMATIC_DEVICE)]
                    )
                    selector.value = NO_AUTOMATIC_DEVICE
                selector.disabled = True
                self.query_one("#simple-run-button", Button).disabled = True
                self.query_one("#simple-device-summary", Static).update(
                    "Conecte sua placa ESP32 pelo cabo USB. Portas Bluetooth são "
                    "ignoradas automaticamente."
                )
                self.query_one("#simple-plan-summary", Static).update(
                    "Assim que a placa aparecer, o perfil correto e as etapas pendentes "
                    "serão escolhidos sem preencher formulários."
                )
                self.query_one("#simple-status-line", Static).update(
                    "Aguardando uma placa USB."
                )
                return

            options: list[tuple[str, str]] = []
            if preferred is None:
                options.append(("Escolha qual placa deseja usar", NO_AUTOMATIC_DEVICE))
            options.extend((item.label, item.key) for item in candidates)
            with self.prevent(Select.Changed):
                selector.set_options(options)
            selector.disabled = False
            valid_keys = set(self._automatic_candidates)
            if previous in valid_keys:
                selected = self._automatic_candidates[previous]
            elif preferred is not None:
                selected = preferred
            else:
                selected = None
            with self.prevent(Select.Changed):
                selector.value = selected.key if selected else NO_AUTOMATIC_DEVICE
            self.query_one("#simple-run-button", Button).disabled = selected is None
            if selected is None:
                self.query_one("#simple-device-summary", Static).update(
                    "Encontramos mais de uma placa. Escolha apenas pelo nome acima; "
                    "o restante continuará automático."
                )
                self.query_one("#simple-status-line", Static).update(
                    "Escolha a placa que deseja executar."
                )
            else:
                self.activate_automatic_candidate(selected)
                self.query_one("#simple-status-line", Static).update(
                    "Pronto. Pressione “Configurar e testar automaticamente”."
                )

        def refresh_status_only(self) -> None:
            status = profile_status(
                self.current_profile,
                draft=self.current_is_draft,
                require_hardware_validation=True,
            )
            self.query_one("#profile-message", Static).update(f"Status: {status}")

        def refresh_ports(self) -> None:
            self._port_infos = ports()
            seen_at = utc_now()
            for connected_profile, is_draft in self.profile_store.list(include_drafts=True):
                try:
                    connected = resolve_profile_port(connected_profile, self._port_infos)
                except DevicePairingError:
                    connected = None
                if connected:
                    connected_profile.applied.last_seen_at = seen_at
                    if is_draft:
                        self.profile_store.save_draft(connected_profile)
                    else:
                        self.profile_store.save(connected_profile)
                    if connected_profile.profile_id == self.current_profile.profile_id:
                        self.current_profile.applied.last_seen_at = seen_at
            table = self.query_one("#ports-table", DataTable)
            table.clear(columns=True)
            table.cursor_type = "row"
            table.add_columns("Porta", "Dispositivo", "Fabricante", "Perfil", "Serial")
            for info in self._port_infos:
                table.add_row(
                    info.get("device", ""),
                    info.get("description", ""),
                    info.get("manufacturer", ""),
                    info.get("suggested_profile", "unknown"),
                    info.get("serial_number", ""),
                    key=f"port:{info.get('device', '')}",
                )
            if not self._port_infos:
                table.add_row("—", "Nenhuma porta detectada", "", "", "")
            select = self.query_one("#port-select", Select)
            current = select.value
            options = self.port_select_options()
            with self.prevent(Select.Changed):
                select.set_options(options)
                select.value = (
                    current
                    if current in {value for _label, value in options}
                    else AUTO_PORT
                )
            self.refresh_catalogue()
            self.refresh_simple_setup()

        def open_validation_issue(self, issue) -> bool:
            destination = _issue_field(issue.path)
            if destination is None:
                return False
            tab_id, selector, label = destination
            if not self._advanced_mode:
                self.set_advanced_mode(True)
            self.activate_content_page(tab_id)
            try:
                widget = self.query_one(selector)
            except Exception:
                return False
            self.call_after_refresh(widget.focus)
            self.query_one("#profile-message", Static).update(
                f"Corrija {label}: {issue.message}"
            )
            return True

        def refresh_diagnostics(self) -> None:
            profile = self.current_profile
            issues = validate_profile(profile)
            self._validation_issues = issues
            env_table = self.query_one("#env-table", DataTable)
            env_table.clear(columns=True)
            env_table.add_columns("Campo", "Valor efetivo", "Origem")
            for row in effective_values(profile, self.secret_manager):
                env_table.add_row(row.field, row.value, row.origin)
            issues_table = self.query_one("#issues-table", DataTable)
            issues_table.clear(columns=True)
            issues_table.cursor_type = "row"
            issues_table.add_columns("Nível", "Campo", "Problema")
            if not issues:
                issues_table.add_row("OK", "—", "Configuração válida")
            for index, issue in enumerate(issues):
                destination = _issue_field(issue.path)
                field = destination[2] if destination is not None else issue.path
                issues_table.add_row(
                    issue.severity.upper(),
                    field,
                    issue.message,
                    key=f"issue:{index}",
                )
            build_hash = compute_build_hash(profile)
            config_hash = compute_config_hash(profile)
            self.query_one("#hash-summary", Static).update(
                f"build_hash {build_hash[:12]}…\nconfig_hash {config_hash[:12]}…\n"
                f"Segredos: {self.secret_manager.backend_name}"
            )
            summary = (
                "Configuração válida"
                if not has_errors(issues)
                else f"{len(issues)} problema(s); desative recursos sem hardware ou informe os GPIOs"
            )
            self.query_one("#validation-summary", Static).update(summary)
            self.refresh_status_only()

        def collect_options(self, *, action: str | None = None):
            profile = self.collect_profile()
            port_value = str(self.query_one("#port-select", Select).value)
            if port_value == AUTO_PORT:
                port = resolve_profile_port(profile, self._port_infos)
            elif port_value == MANUAL_PORT:
                port = self.query_one("#port-input", Input).value.strip() or None
            else:
                port = port_value
            timeout_text = self.query_one("#timeout-input", Input).value.strip()
            timeout = _integer(timeout_text, 0) or None
            return api.BuildOptions(
                action=action or str(self.query_one("#action-select", Select).value),
                profile=profile.target.profile,
                port=port,
                timeout=timeout,
                env_overrides=build_environment(profile, self.secret_manager),
                device_profile=profile,
                device_profile_source=profile.profile_id,
            )

        def selected_pairing_port(self) -> str:
            value = str(self.query_one("#port-select", Select).value)
            if value == MANUAL_PORT:
                port = self.query_one("#port-input", Input).value.strip()
                if port:
                    return port
            elif value != AUTO_PORT:
                return value
            usable_infos = usable_port_infos(self._port_infos)
            associated = resolve_profile_port(self.current_profile, usable_infos)
            if associated:
                return associated
            compatible = [
                str(info.get("device") or "")
                for info in usable_infos
                if info.get("suggested_profile") == self.current_profile.target.profile
                and str(info.get("device") or "")
            ]
            if len(compatible) == 1:
                return compatible[0]
            detected = [str(info.get("device") or "") for info in usable_infos]
            detected = [port for port in detected if port]
            if len(detected) == 1:
                return detected[0]
            raise DevicePairingError(
                "selecione uma porta para parear; a detecção automática está ambígua"
            )

        def optional_commission_port(self) -> str | None:
            try:
                return self.selected_pairing_port()
            except DevicePairingError:
                return None

        def update_command_preview(self) -> None:
            if not self.is_mounted:
                return
            try:
                options = self.collect_options()
                preview = api.describe_planned_command(options, self.settings)
            except Exception as exc:
                preview = str(exc)
            self.query_one("#command-preview", Static).update(preview)

        def validate_and_report(self) -> bool:
            profile = self.collect_profile()
            issues = validate_profile(profile)
            self.refresh_diagnostics()
            if issues:
                first = "; ".join(str(issue) for issue in issues[:3])
                self.query_one("#profile-message", Static).update(first)
                errors = [issue for issue in issues if issue.severity == "error"]
                if errors:
                    self.open_validation_issue(errors[0])
            else:
                self.query_one("#profile-message", Static).update("Configuração válida.")
            return not has_errors(issues)

        def continue_apply_steps(self) -> None:
            """Run only the build/provision operation implied by canonical hashes."""

            status = profile_status(self.collect_profile(), draft=self.current_is_draft)
            if status == "Rascunho":
                self._test_after_apply = False
                self.start_pairing_for("apply")
            elif status == "Inválido":
                self._test_after_apply = False
                self._monitor_after_apply = False
                self._hardware_validation_after_apply = False
                self.validate_and_report()
            elif status == "Requer build":
                self._provision_after_build = True
                self.start_build_action("flash")
            elif status == "Requer provisionamento":
                self.start_provisioning()
            else:
                if self._hardware_validation_after_apply:
                    self.start_hardware_validation()
                elif self._monitor_after_apply:
                    self._monitor_after_apply = False
                    self.start_build_action("monitor")
                else:
                    self.notify("Dispositivo já está sincronizado; nenhuma etapa necessária.")
                    self.start_post_apply_tests()

        def start_post_apply_tests(self) -> None:
            if not self._test_after_apply:
                return
            self._test_after_apply = False
            if self.current_profile.mqtt.enabled or self.current_profile.server.base_url:
                self.test_integration("all")
            else:
                self.notify(
                    "Aplicação concluída; não há integração MQTT/servidor configurada para testar.",
                    severity="information",
                )

        def action_save_profile(self) -> None:
            self.save_current()

        def action_refresh_ports(self) -> None:
            self.refresh_ports()

        def action_clear_log(self) -> None:
            self.query_one("#run-live-log", RichLog).clear()
            self.query_one("#simple-live-log", RichLog).clear()

        def action_choose_theme(self) -> None:
            self.search_themes()

        def watch_theme(self, old_theme: str, new_theme: str) -> None:
            if not self.is_mounted or new_theme not in self.available_themes:
                return
            preferences = api.load_tui_preferences()
            preferences["theme"] = new_theme
            api.save_tui_preferences(preferences)

        def action_cancel_or_quit(self) -> None:
            if self.running:
                self.controller.cancel()
                self.query_one("#status-line", Static).update("Cancelando…")
                self.query_one("#simple-status-line", Static).update("Cancelando…")
            else:
                self.exit(0)

        def fail_automatic_setup(self, message: str) -> None:
            if not self._automatic_setup_active:
                return
            self._automatic_setup_active = False
            self.query_one("#simple-run-button", Button).disabled = not bool(
                self._automatic_candidates
            )
            self.query_one("#simple-status-line", Static).update(message)

        def _network_hardware_tester(self, context, _check, _params):
            runtime_result, runtime_state = wait_for_runtime_snapshot(context)
            runtime_ok = bool(runtime_state.get("network_ready"))
            checks: list[tuple[str, IntegrationResult]] = []
            if self.current_profile.mqtt.enabled:
                checks.append(
                    (
                        "mqtt",
                        test_mqtt_connection(self.current_profile, self.secret_manager),
                    )
                )
            if self.current_profile.server.base_url:
                checks.append(
                    (
                        "server",
                        test_server_connection(self.current_profile, self.secret_manager),
                    )
                )
            evidence = {
                label: {"ok": result.ok, "message": result.message}
                for label, result in checks
            }
            evidence["firmware_runtime"] = {
                "network_ready": runtime_ok,
                "last_udp_send_secs": int(runtime_state.get("last_udp_send_secs") or 0),
                "dropped_packets": int(runtime_state.get("dropped_packets") or 0),
                "transmitted_packets": int(runtime_state.get("transmitted_packets") or 0),
                "raw_csi_captured_by_host": False,
            }
            integrations_ok = all(result.ok for _label, result in checks)
            return AutomaticTestResult(
                runtime_ok and integrations_ok,
                evidence=evidence,
                message=(
                    ""
                    if runtime_ok and integrations_ok
                    else "a placa não confirmou rede/UDP ou uma integração falhou"
                ),
                source="usb_runtime_counters+mqtt+server_readback",
            )

        def start_hardware_validation(self) -> None:
            """Begin the mandatory live-board phase after hashes are synchronized."""

            if self.running:
                return
            self._hardware_validation_after_apply = False
            self._hardware_validation_active_check = ""
            self._hardware_prompt_in_progress = False
            self.query_one("#simple-status-line", Static).update(
                "Conferindo identidade, firmware e configuração na placa conectada…"
            )
            self.query_one("#simple-live-log", RichLog).write(
                "Iniciando validação física obrigatória pela USB."
            )
            self._hardware_validation_service = HardwareValidationService(
                testers={"network_integration": self._network_hardware_tester},
                secret_values=self.secret_values(),
            )
            self.hardware_validation_operation("start")

        @work(thread=True, exclusive=True, group="hardware-validation")
        def hardware_validation_operation(self, operation: str, check_id: str = "") -> None:
            try:
                service = self._hardware_validation_service
                if service is None:
                    raise HardwareValidationError("serviço de validação não foi iniciado")
                if operation == "start":
                    service.start(self.current_profile)
                elif operation == "run":
                    params: dict[str, Any] = {}
                    if check_id == "ir_rx":
                        params = {
                            "name": "Botão aprendido automaticamente",
                            "timeout_ms": 15_000,
                        }
                    service.run_check(check_id, params)
                else:
                    raise HardwareValidationError(f"operação desconhecida: {operation}")
            except Exception as exc:
                self.post_message(HardwareValidationDone(operation, check_id, str(exc)))
                return
            self.post_message(HardwareValidationDone(operation, check_id))

        def refresh_hardware_validation_cards(self) -> None:
            service = self._hardware_validation_service
            session = service.session if service is not None else None
            states = {
                ValidationState.WAITING: ("○", "Aguardando"),
                ValidationState.RUNNING: ("◌", "Executando"),
                ValidationState.MANUAL_CONFIRMATION: ("?", "Confirmação manual"),
                ValidationState.PASSED: ("✓", "Passou"),
                ValidationState.FAILED: ("✗", "Falhou"),
                ValidationState.BLOCKED: ("!", "Bloqueado"),
                ValidationState.SKIPPED: ("–", "Ignorado"),
            }
            labels = {
                "boot_serial": "Boot e USB",
                "rgb": "LED RGB",
                "ir_rx": "Receptor IR",
                "ir_tx": "Transmissor IR",
                "storage": "Cartão microSD",
                "relays": "Relés",
                "network_integration": "Rede e integrações",
                "csi": "CSI (somente sala vazia)",
                "concurrency": "Teste simultâneo de 30 minutos",
            }
            for card_id, label in labels.items():
                button = self.query_one(
                    f"#hardware-card-{card_id.replace('_', '-')}", Button
                )
                if session is None:
                    state = ValidationState.WAITING
                    reason = ""
                elif card_id == "relays":
                    relay_checks = [
                        check
                        for key, check in session.checks.items()
                        if key.startswith("relay:")
                    ]
                    if not relay_checks:
                        state, reason = ValidationState.SKIPPED, "nenhum relé habilitado"
                    else:
                        priority = (
                            ValidationState.FAILED,
                            ValidationState.BLOCKED,
                            ValidationState.MANUAL_CONFIRMATION,
                            ValidationState.RUNNING,
                            ValidationState.WAITING,
                            ValidationState.PASSED,
                        )
                        state = next(
                            candidate
                            for candidate in priority
                            if any(item.state == candidate for item in relay_checks)
                        )
                        reason = next(
                            (item.reason for item in relay_checks if item.reason), ""
                        )
                else:
                    check = session.checks.get(card_id)
                    state = check.state if check is not None else ValidationState.SKIPPED
                    reason = check.reason if check is not None else "não se aplica"
                icon, state_label = states[state]
                suffix = f" — {reason}" if reason and state in {
                    ValidationState.FAILED,
                    ValidationState.BLOCKED,
                } else ""
                button.label = f"{icon} {label} — {state_label}{suffix}"
                button.disabled = state not in {
                    ValidationState.FAILED,
                    ValidationState.BLOCKED,
                    ValidationState.WAITING,
                }

        def _next_hardware_validation_step(self) -> None:
            service = self._hardware_validation_service
            if service is None or service.session is None or self._hardware_prompt_in_progress:
                return
            session = service.session
            if session.preflight.state != ValidationState.PASSED:
                self.finish_hardware_validation()
                return
            for check in session.checks.values():
                if check.state in {
                    ValidationState.PASSED,
                    ValidationState.SKIPPED,
                    ValidationState.FAILED,
                    ValidationState.BLOCKED,
                }:
                    continue
                if check.state == ValidationState.MANUAL_CONFIRMATION:
                    if check.authorization_required and check.automatic_evidence is None:
                        self._hardware_prompt_in_progress = True
                        self.push_screen(
                            HardwareConfirmationScreen(
                                f"Antes de testar: {check.label}",
                                check.authorization_prompt,
                                confirm_label="Está seguro / sala vazia",
                            ),
                            lambda confirmed, check_id=check.check_id: self._complete_hardware_authorization(
                                check_id, confirmed
                            ),
                        )
                    else:
                        self._hardware_prompt_in_progress = True
                        self.push_screen(
                            HardwareConfirmationScreen(
                                f"Confirmação física: {check.label}",
                                check.manual_prompt or check.reason,
                                confirm_label="Confirmei na placa/aparelho",
                            ),
                            lambda confirmed, check_id=check.check_id: self._complete_hardware_confirmation(
                                check_id, confirmed
                            ),
                        )
                    return
                if check.state == ValidationState.WAITING:
                    if check.check_id == "ir_rx" and check.attempts == 0:
                        self._hardware_prompt_in_progress = True
                        self.push_screen(
                            HardwareConfirmationScreen(
                                "Aprender controle remoto",
                                "Aponte o controle para o receptor IR. Ao iniciar, pressione "
                                "uma tecla até o LED da placa piscar em amarelo.",
                                confirm_label="Começar captura",
                            ),
                            lambda ready: self._start_waiting_hardware_check(
                                "ir_rx", ready
                            ),
                        )
                        return
                    self._hardware_validation_active_check = check.check_id
                    self.query_one("#simple-status-line", Static).update(
                        f"Testando na placa: {check.label}…"
                    )
                    self.hardware_validation_operation("run", check.check_id)
                    return
            self.finish_hardware_validation()

        def _start_waiting_hardware_check(self, check_id: str, ready: bool) -> None:
            self._hardware_prompt_in_progress = False
            if not ready:
                self.query_one("#simple-status-line", Static).update(
                    "Captura pausada. Clique no card do receptor IR quando estiver pronto."
                )
                self.refresh_hardware_validation_cards()
                return
            self._hardware_validation_active_check = check_id
            self.hardware_validation_operation("run", check_id)

        def _complete_hardware_authorization(self, check_id: str, confirmed: bool) -> None:
            self._hardware_prompt_in_progress = False
            service = self._hardware_validation_service
            if service is None:
                return
            service.authorize(
                check_id,
                confirmed,
                "confirmação registrada pela TUI" if confirmed else "não autorizado",
            )
            self.refresh_hardware_validation_cards()
            self.call_after_refresh(self._next_hardware_validation_step)

        def _complete_hardware_confirmation(self, check_id: str, confirmed: bool) -> None:
            self._hardware_prompt_in_progress = False
            service = self._hardware_validation_service
            if service is None:
                return
            service.confirm_manual(
                check_id,
                confirmed,
                "efeito físico confirmado pela TUI" if confirmed else "efeito não observado",
            )
            if confirmed and check_id == "ir_rx":
                self._save_learned_ir_code(service.artifact("ir_rx"))
            self.refresh_hardware_validation_cards()
            self.call_after_refresh(self._next_hardware_validation_step)

        def _save_learned_ir_code(self, code: Any) -> None:
            if not isinstance(code, dict) or not isinstance(code.get("sequence_us"), list):
                return
            try:
                library = self.ir_library_store.get_or_create(
                    self.current_profile.profile_id,
                    self.current_profile.device_id,
                )
                appliance = (
                    library.appliances[0]
                    if library.appliances
                    else self.ir_library_store.create_appliance(
                        self.current_profile.profile_id,
                        "Aparelho 1",
                        device_id=self.current_profile.device_id,
                    )
                )
                saved = self.ir_library_store.save_capture(
                    self.current_profile.profile_id,
                    appliance.appliance_id,
                    str(code.get("name") or "Botão 1"),
                    code["sequence_us"],
                    device_id=self.current_profile.device_id,
                    carrier_hz=int(code.get("carrier_hz") or 38_000),
                    duty_percent=int(code.get("duty_percent") or 33),
                    code_id=str(code.get("code_id") or "") or None,
                )
                self._last_ir_code = {
                    "code_id": saved.code.code_id,
                    "carrier_hz": saved.code.carrier_hz,
                    "duty_percent": saved.code.duty_percent,
                    "sequence_us": list(saved.code.sequence_us),
                }
                sync_result = sync_ir_code(
                    self.current_profile,
                    self.secret_manager,
                    saved.code.to_server_payload(
                        self.current_profile.device_id,
                        appliance_id=appliance.appliance_id,
                        appliance_name=appliance.name,
                    ),
                )
                if sync_result.ok:
                    expected_hash = saved.code.current_payload_hash(
                        self.current_profile.device_id
                    )
                    self.ir_library_store.mark_code_synced(
                        self.current_profile.profile_id,
                        saved.code.code_id,
                        expected_payload_hash=expected_hash,
                    )
                else:
                    self.query_one("#simple-live-log", RichLog).write(
                        sync_result.message
                    )
            except (OSError, ValueError, KeyError) as exc:
                self.query_one("#simple-live-log", RichLog).write(
                    f"Código IR capturado, mas a biblioteca local falhou: {exc}"
                )

        def finish_hardware_validation(self) -> None:
            service = self._hardware_validation_service
            session = service.session if service is not None else None
            self._hardware_validation_active_check = ""
            self._hardware_validation_after_apply = False
            self._automatic_setup_active = False
            self.query_one("#simple-run-button", Button).disabled = False
            self.refresh_hardware_validation_cards()
            if session is None:
                return
            if session.physically_validated:
                self.current_profile.applied.hardware_validation_status = "passed"
                self.current_profile.applied.hardware_validation_session = session.session_id
                self.current_profile.applied.hardware_validated_at = (
                    session.completed_at or utc_now()
                )
                self.current_profile.applied.hardware_validation_build_hash = (
                    session.expected_build_hash
                )
                self.current_profile.applied.hardware_validation_config_hash = (
                    session.expected_config_hash
                )
                self.profile_store.save(self.current_profile)
                self.current_is_draft = False
                self.query_one("#simple-status-line", Static).update(
                    "Sincronizado e testado na placa. Abrindo o monitor…"
                )
                self.notify("Todos os recursos habilitados passaram na placa.")
                self.start_build_action("monitor")
            else:
                report = str(service.report_path or "")
                self.query_one("#simple-status-line", Static).update(
                    "A validação física ficou incompleta. Corrija apenas os cards marcados "
                    "e clique neles para repetir."
                )
                self.query_one("#simple-live-log", RichLog).write(
                    f"Evidência da validação: {report}"
                )
                self.notify(
                    "Nenhum mock/build foi aceito como prova: há testes físicos pendentes.",
                    severity="warning",
                )

        def on_hardware_validation_done(self, event: HardwareValidationDone) -> None:
            self._hardware_validation_active_check = ""
            if event.error:
                self.query_one("#simple-live-log", RichLog).write(
                    f"Validação física falhou: {event.error}"
                )
                self.query_one("#simple-status-line", Static).update(
                    "Não foi possível concluir o teste na placa."
                )
                self.finish_hardware_validation()
                return
            self.refresh_hardware_validation_cards()
            self.call_after_refresh(self._next_hardware_validation_step)

        def action_automatic_setup(self) -> None:
            """Resolve board/profile and delegate to the existing atomic executor."""

            if self.running:
                self.notify("Já existe uma ação em andamento.", severity="warning")
                return
            self.refresh_ports()
            key = str(self.query_one("#simple-device-select", Select).value)
            candidate = self._automatic_candidates.get(key)
            if candidate is None:
                self.query_one("#simple-status-line", Static).update(
                    "Conecte ou escolha uma placa antes de continuar."
                )
                self.notify("Nenhuma placa USB selecionada.", severity="warning")
                return
            self.query_one("#simple-live-log", RichLog).clear()
            self.activate_automatic_candidate(candidate)
            automatic_changes = list(repair_incomplete_profile(self.current_profile))
            if (
                self.secret_manager.backend_name == "os-keyring"
                and not self.current_profile.mqtt.enabled
                and not self.current_profile.server.base_url
            ):
                dashboard = prepare_local_dashboard_profile(
                    self.current_profile,
                    self.secret_manager,
                )
                if dashboard.ok:
                    automatic_changes.append(dashboard.message)
                else:
                    self.query_one("#simple-live-log", RichLog).write(
                        dashboard.message
                    )
            if automatic_changes:
                self.profile_store.save_draft(self.current_profile)
                self.current_is_draft = True
                self.populate_form(self.current_profile)
                self.query_one("#simple-live-log", RichLog).write(
                    "Ajustes seguros automáticos: " + "; ".join(automatic_changes)
                )
                self.refresh_simple_plan(candidate)
            self._automatic_setup_active = True
            self.query_one("#simple-status-line", Static).update(
                "Preparando automaticamente a placa…"
            )
            self.query_one("#simple-run-button", Button).disabled = True
            if not self._run_current_profile():
                self.fail_automatic_setup(
                    "A configuração salva precisa de atenção. Abra as opções avançadas "
                    "para revisar o item indicado."
                )

        def _run_current_profile(self) -> bool:
            """Apply atomically, validate the real board, then open the monitor."""

            if self.running:
                self.notify("Já existe uma ação em andamento.", severity="warning")
                return False
            if not self.save_current(notify=False):
                self._monitor_after_apply = False
                self.notify("Corrija a configuração antes de executar.", severity="error")
                return False
            self._monitor_after_apply = False
            self._hardware_validation_after_apply = True
            # A server cannot onboard a device before its immutable eFuse identity
            # is known.  Pair first, then resume this exact flow.
            if not self.current_profile.device_id:
                self.start_pairing_for("run-current")
                return True
            if self.current_profile.server.base_url:
                self.start_onboarding(continue_apply=True)
            else:
                self.continue_apply_steps()
            return True

        def action_run_selected(self) -> None:
            if str(self.query_one("#main-tabs", TabbedContent).active) == "simple-tab":
                self.action_automatic_setup()
            else:
                self._run_current_profile()

        def start_pairing_for(self, continuation: str) -> None:
            if self._action_after_pairing is not None:
                self.notify("O pareamento da placa já está em andamento.", severity="warning")
                return
            try:
                port = self.selected_pairing_port()
            except Exception as exc:
                message = f"Não foi possível parear automaticamente: {exc}"
                self.notify(message, severity="error")
                self.query_one("#status-line", Static).update("Selecione a porta da placa")
                self.query_one("#profile-message", Static).update(message)
                self.query_one("#run-live-log", RichLog).write(f"[ERROR] {message}")
                self.query_one("#simple-status-line", Static).update(
                    "Não consegui identificar a placa. Reconecte o cabo USB e tente novamente."
                )
                self.query_one("#simple-run-button", Button).disabled = False
                self.fail_automatic_setup(
                    "Não consegui identificar a placa. Reconecte o cabo USB e tente novamente."
                )
                return
            self._action_after_pairing = continuation
            self.query_one("#status-line", Static).update(
                f"Lendo eFuse MAC em {port} antes de executar…"
            )
            self.query_one("#simple-status-line", Static).update(
                "Identificando sua placa com segurança…"
            )
            self.notify(
                f"Primeiro uso: pareando automaticamente a placa em {port}…",
                severity="information",
            )
            self.pair_board(port)

        def start_build_action(self, action: str) -> None:
            if self.running:
                self.notify("Já existe uma ação em andamento.", severity="warning")
                return
            try:
                options = self.collect_options(action=action)
            except Exception as exc:
                self.notify(f"Não foi possível preparar a ação: {exc}", severity="error")
                self.fail_automatic_setup(
                    "Não consegui preparar esta etapa. Tente novamente ou abra as opções avançadas."
                )
                return
            destructive = {"flash", "erase", "run", "rescue", "flashall"}
            if options.action in destructive and not self.validate_and_report():
                self.notify("Corrija a configuração antes desta ação.", severity="error")
                self.fail_automatic_setup(
                    "A configuração salva precisa de atenção. Abra as opções avançadas para revisar."
                )
                return
            if (
                options.action in {"flash", "erase", "run", "rescue"}
                and not self.current_profile.device_id
            ):
                self.start_pairing_for(options.action)
                return
            saved_ok = self.save_current(notify=False)
            if options.action in destructive and not saved_ok:
                self.notify("Corrija a configuração antes desta ação.", severity="error")
                self.fail_automatic_setup(
                    "A configuração salva precisa de atenção. Abra as opções avançadas para revisar."
                )
                return
            self.running = True
            self.query_one("#status-line", Static).update(
                f"Iniciando {api.ACTIONS[options.action].label}…"
            )
            self.query_one("#simple-status-line", Static).update(
                f"{api.ACTIONS[options.action].label} em andamento…"
            )
            self.run_build(options)

        @work(thread=True, exclusive=True, group="build")
        def run_build(self, options) -> None:
            self.call_from_thread(self.query_one("#run-live-log", RichLog).clear)
            self.call_from_thread(self.query_one("#simple-live-log", RichLog).clear)
            self.call_from_thread(self.query_one("#loading-indicator", LoadingIndicator).styles.__setattr__, "display", "block")
            self.controller = api.ProcessController()
            exit_code = 0
            with open(self.settings.log_path, "a", encoding="utf-8") as log_file:
                sink = TuiSink(self, log_file)
                try:
                    exit_code = api.execute_action(options, self.settings, sink, self.controller)
                except api.BuildFailure as exc:
                    exit_code = exc.exit_code
                    sink.line(exc.message)
                except Exception as exc:
                    exit_code = 1
                    sink.line(f"[ERROR] Unexpected failure: {exc}")
            self.post_message(BuildDone(exit_code, options.action))

        def on_build_log(self, event: BuildLog) -> None:
            text = event.text.rstrip("\r\n")
            if text:
                self.query_one("#run-live-log", RichLog).write(text, scroll_end=True)
                self.query_one("#simple-live-log", RichLog).write(text, scroll_end=True)

        def on_build_progress(self, event: BuildProgress) -> None:
            if event.value is not None:
                self.query_one("#build-progress", ProgressBar).update(total=100, progress=float(event.value))
                self.query_one("#simple-progress", ProgressBar).update(
                    total=100, progress=float(event.value)
                )
            if event.text:
                self.query_one("#status-line", Static).update(event.text)
                self.query_one("#simple-status-line", Static).update(event.text)

        def on_build_done(self, event: BuildDone) -> None:
            self.running = False
            self.query_one("#loading-indicator", LoadingIndicator).styles.display = "none"
            if event.exit_code == 0:
                self.query_one("#build-progress", ProgressBar).update(total=100, progress=100)
                self.query_one("#simple-progress", ProgressBar).update(total=100, progress=100)
                self.query_one("#status-line", Static).update("Concluído")
                self.query_one("#simple-status-line", Static).update(
                    "Etapa concluída. Preparando a próxima…"
                )
                self.notify("Ação concluída.", severity="information")
                if self._provision_after_build:
                    self._provision_after_build = False
                    self.start_provisioning()
                else:
                    self._automatic_setup_active = False
                    self.query_one("#simple-run-button", Button).disabled = False
                    self.query_one("#simple-status-line", Static).update(
                        "Pronto. A placa está executando e o monitor foi encerrado."
                    )
            else:
                self._provision_after_build = False
                self._test_after_apply = False
                self._monitor_after_apply = False
                self._hardware_validation_after_apply = False
                self.query_one("#status-line", Static).update(f"Falhou: {event.exit_code}")
                self._automatic_setup_active = False
                self.query_one("#simple-run-button", Button).disabled = False
                self.query_one("#simple-status-line", Static).update(
                    "Não foi possível concluir. Você pode tentar novamente; "
                    "o log abaixo mostra o motivo."
                )
                self.notify(f"Ação falhou (exit {event.exit_code}).", severity="error")
            self.refresh_catalogue()
            self.refresh_diagnostics()

        def start_provisioning(self) -> None:
            if not self.save_current(notify=False):
                self.notify("Corrija a configuração antes de provisionar.", severity="error")
                self.fail_automatic_setup(
                    "A configuração não pôde ser enviada. Abra as opções avançadas para revisar."
                )
                return
            try:
                options = self.collect_options()
                port = api.resolve_monitor_port(self.settings.project_dir, options.port)
            except Exception as exc:
                self.notify(str(exc), severity="error")
                self.fail_automatic_setup(
                    "Não encontrei a placa para enviar a configuração. Reconecte o cabo e tente novamente."
                )
                return
            if not port:
                self.notify("Selecione uma porta serial.", severity="error")
                self.fail_automatic_setup(
                    "Não encontrei a placa para enviar a configuração. Reconecte o cabo e tente novamente."
                )
                return
            self.query_one("#status-line", Static).update("Provisionando…")
            self.query_one("#simple-status-line", Static).update(
                "Enviando a configuração para a placa…"
            )
            self.provision_device(port)

        @work(thread=True, exclusive=True, group="integration")
        def test_integration(self, kind: str) -> None:
            profile = self.current_profile
            if kind == "mqtt":
                result = test_mqtt_connection(profile, self.secret_manager)
            elif kind == "server":
                result = test_server_connection(profile, self.secret_manager)
            else:
                results = []
                if profile.mqtt.enabled:
                    results.append(test_mqtt_connection(profile, self.secret_manager))
                if profile.server.base_url:
                    results.append(test_server_connection(profile, self.secret_manager))
                result = IntegrationResult(
                    bool(results) and all(item.ok for item in results),
                    " | ".join(item.message for item in results)
                    or "Nenhuma integração configurada para testar",
                )
            self.post_message(IntegrationDone(result.message, result.ok))

        @work(thread=True, exclusive=True, group="integration")
        def start_local_broker_service(self) -> None:
            try:
                profile = self.current_profile
                runtime = Path(self.settings.project_dir).parent / "mosquitto" / "runtime"
                manager = LocalBrokerManager(runtime, self.secret_manager)
                manager.prepare_device(profile)
                result = manager.start_or_reload_service(
                    profile,
                    Path(self.settings.project_dir).parent / "docker-compose.yml",
                )
                if result.ok and profile.server.base_url:
                    result = test_server_connection(profile, self.secret_manager, timeout=15.0)
            except Exception as exc:
                result = IntegrationResult(False, str(exc))
            self.post_message(IntegrationDone(result.message, result.ok))

        def start_onboarding(
            self, *, continue_apply: bool = False, rotate_token: bool = False
        ) -> None:
            if not self.save_current(notify=False):
                self.notify("Corrija a configuração antes do onboarding.", severity="error")
                return
            if not self.current_profile.server.base_url:
                self.notify("Configure a URL do servidor AetherSense.", severity="error")
                return
            self.query_one("#integration-result", Static).update(
                "Preparando broker/servidor e registrando dispositivo…"
            )
            self.onboard_integration(continue_apply, rotate_token)

        @work(thread=True, exclusive=True, group="integration")
        def onboard_integration(self, continue_apply: bool, rotate_token: bool) -> None:
            try:
                profile = self.current_profile
                if profile.mqtt.enabled and profile.mqtt.broker_mode == "local":
                    runtime = Path(self.settings.project_dir).parent / "mosquitto" / "runtime"
                    manager = LocalBrokerManager(runtime, self.secret_manager)
                    manager.prepare_device(profile, rotate=rotate_token)
                    service = manager.start_or_reload_service(
                        profile,
                        Path(self.settings.project_dir).parent / "docker-compose.yml",
                    )
                    if not service.ok:
                        self.post_message(
                            OnboardingDone(error=service.message, continue_apply=continue_apply)
                        )
                        return
                    health = test_server_connection(profile, self.secret_manager, timeout=15.0)
                    if not health.ok:
                        self.post_message(
                            OnboardingDone(error=health.message, continue_apply=continue_apply)
                        )
                        return
                result = onboard_device(
                    profile,
                    self.secret_manager,
                    timeout=15.0,
                    rotate_device_token=rotate_token,
                )
                if not result.ok:
                    self.post_message(
                        OnboardingDone(error=result.message, continue_apply=continue_apply)
                    )
                    return
            except Exception as exc:
                self.post_message(OnboardingDone(error=str(exc), continue_apply=continue_apply))
            else:
                self.post_message(
                    OnboardingDone(result=result, continue_apply=continue_apply)
                )

        @work(thread=True, exclusive=True, group="provision")
        def provision_device(self, port: str) -> None:
            try:
                result = ProvisionClient(port).provision(self.current_profile, self.secret_manager)
                activate_provisioned_profile(
                    port,
                    self.current_profile,
                    client_factory=lambda active_port: ProvisionClient(active_port),
                )
            except Exception as exc:
                self.post_message(ProvisionDone(error=str(exc)))
            else:
                self.post_message(ProvisionDone(result=result))

        @work(thread=True, exclusive=True, group="pairing")
        def pair_board(self, port: str) -> None:
            try:
                identity = probe_board(port)
                profile = self.current_profile
                is_draft = self.current_is_draft
                if self._automatic_setup_active and not profile.device_id:
                    identity_matches = [
                        item
                        for item in self.profile_store.list(include_drafts=True)
                        if item[0].device_id == identity.device_id
                    ]
                    if len(identity_matches) > 1:
                        raise DevicePairingError(
                            "mais de um perfil salvo pertence a esta placa; remova a duplicata"
                        )
                    if identity_matches:
                        profile, is_draft = identity_matches[0]
                pair_profile(profile, identity, self._port_infos)
            except Exception as exc:
                self.post_message(PairingDone(error=str(exc)))
            else:
                self.post_message(
                    PairingDone(
                        identity=identity,
                        profile=profile,
                        is_draft=is_draft,
                    )
                )

        def complete_batch_apply(self, confirmed: bool) -> None:
            plan = getattr(self, "_pending_batch_plan", None)
            self._pending_batch_plan = None
            if not confirmed or plan is None:
                self.query_one("#profile-message", Static).update(
                    "Aplicação em lote cancelada; nenhum dispositivo foi alterado."
                )
                return
            self.query_one("#profile-message", Static).update(
                "Aplicando lote sequencialmente…"
            )
            self.run_batch(plan)

        @work(thread=True, exclusive=True, group="build")
        def run_batch(self, plan) -> None:
            self.running = True
            fresh_ports = ports()
            with open(self.settings.log_path, "a", encoding="utf-8") as log_file:
                sink = TuiSink(self, log_file)

                def execute_item(item):
                    try:
                        profile = self.profile_store.load(item.profile_id)
                        completed_steps: list[str] = []
                        if profile.server.base_url:
                            if profile.mqtt.enabled and profile.mqtt.broker_mode == "local":
                                runtime = (
                                    Path(self.settings.project_dir).parent
                                    / "mosquitto"
                                    / "runtime"
                                )
                                manager = LocalBrokerManager(runtime, self.secret_manager)
                                manager.prepare_device(profile)
                                service = manager.start_or_reload_service(
                                    profile,
                                    Path(self.settings.project_dir).parent
                                    / "docker-compose.yml",
                                )
                                if not service.ok:
                                    raise RuntimeError(service.message)
                                completed_steps.append("broker/ACL")
                            onboarded = onboard_device(profile, self.secret_manager)
                            if not onboarded.ok:
                                raise RuntimeError(onboarded.message)
                            profile.refresh_features()
                            self.profile_store.save(profile)
                            completed_steps.append("onboarding")
                        port = resolve_profile_port(profile, fresh_ports)
                        if not port:
                            raise DevicePairingError(
                                "porta atual não encontrada pela associação USB"
                            )
                        if item.operation == "flash_then_provision":
                            options = api.BuildOptions(
                                action="flash",
                                profile=profile.target.profile,
                                port=port,
                                timeout=self.options.timeout,
                                env_overrides=build_environment(
                                    profile, self.secret_manager
                                ),
                                device_profile=profile,
                                device_profile_source=profile.profile_id,
                            )
                            exit_code = api.execute_action(
                                options,
                                self.settings,
                                sink,
                                api.ProcessController(),
                            )
                            if exit_code != 0:
                                raise RuntimeError(f"flash falhou (exit {exit_code})")
                            completed_steps.append("flash")
                        provisioned = ProvisionClient(port).provision(
                            profile, self.secret_manager
                        )
                        activate_provisioned_profile(
                            port,
                            profile,
                            client_factory=lambda active_port: ProvisionClient(active_port),
                        )
                        profile.applied.build_hash = provisioned.build_hash
                        profile.applied.config_hash = provisioned.config_hash
                        profile.applied.last_seen_at = utc_now()
                        self.profile_store.save(profile)
                        completed_steps.extend(("provision", "readback"))
                        return True, f"{port}: {', '.join(completed_steps)} confirmados"
                    except Exception as exc:
                        return False, str(exc)

                results = apply_batch(plan, execute_item)
            self.post_message(BatchDone(results))

        @work(thread=True, exclusive=True, group="sd")
        def sd_operation(self, kind: str, source: Path) -> None:
            try:
                if kind == "inspect":
                    sessions = inspect_sd(source)
                    total_bytes = sum(session.bytes for session in sessions)
                    text = f"{len(sessions)} sessão(ões), {total_bytes} bytes; nenhuma alteração realizada."
                else:
                    destination = Path(self.settings.project_dir).parent / "resultados" / "firmware-sd"
                    report = import_sd_sessions(source, destination)
                    text = (
                        f"Importadas: {len(report.imported)}; já existentes/idênticas: "
                        f"{len(report.skipped)}; destino {destination}"
                    )
            except Exception as exc:
                self.post_message(SdDone(str(exc), False))
            else:
                self.post_message(SdDone(text, True))

        @work(thread=True, exclusive=True, group="commission")
        def commission_operation(
            self,
            kind: str,
            params: dict[str, Any] | None = None,
            target: str = "integration",
            port: str | None = None,
        ) -> None:
            payload: dict[str, Any] | None = None
            source = "server"
            try:
                profile = self.current_profile
                direct_params = dict(params or {})
                direct_supported = kind in {
                    "relay.pulse",
                    "ir.learn.start",
                    "ir.learn.cancel",
                    "ir.send",
                    "storage.sync",
                }
                # A remote library ID does not contain the raw timings needed by
                # the ESP. Prefer USB only when the TUI has a captured sequence.
                if kind == "ir.send" and not direct_params.get("sequence_us"):
                    direct_supported = False

                if port and kind in {"status", "sd-status"}:
                    live = ProvisionClient(port, operation_timeout=15).inspect_device(profile)
                    expected_config = compute_config_hash(profile)
                    synchronized = live.config_hash == expected_config
                    if kind == "sd-status":
                        enabled = profile.storage.enabled and synchronized
                        result = IntegrationResult(
                            enabled,
                            f"USB {port}: microSD {'configurado' if enabled else 'não aplicado/indisponível'}; "
                            f"geração NVS={live.generation or '—'}",
                        )
                    else:
                        result = IntegrationResult(
                            synchronized,
                            f"USB {port}: {live.device_id}; "
                            f"IR RX={'GPIO ' + str(profile.ir_rx.gpio) if profile.ir_rx.enabled else 'desativado'}; "
                            f"IR TX={'GPIO ' + str(profile.ir_tx.gpio) if profile.ir_tx.enabled else 'desativado'}; "
                            f"configuração {'sincronizada' if synchronized else 'precisa ser aplicada'}",
                        )
                    payload = {
                        "device_id": live.device_id,
                        "config_hash": live.config_hash,
                        "generation": live.generation,
                        "features": list(live.features),
                    }
                    source = "usb"
                elif port and direct_supported:
                    direct = ProvisionClient(port, operation_timeout=45).commission_accessory(
                        profile,
                        kind,
                        direct_params,
                        cancel_requested=(
                            self._ir_cancel_event.is_set
                            if kind == "ir.learn.start"
                            else None
                        ),
                    )
                    payload = direct.state
                    source = "usb"
                    if kind == "ir.learn.start":
                        code = (direct.state or {}).get("code", {})
                        count = len(code.get("sequence_us", [])) if isinstance(code, dict) else 0
                        result = IntegrationResult(
                            count >= 2,
                            f"Sinal IR aprendido por USB ({count} durações). "
                            "Use Repetir última captura para testar o transmissor.",
                        )
                    elif kind == "ir.send":
                        result = IntegrationResult(True, "Sinal IR emitido e confirmado pelo RMT via USB.")
                    elif kind == "relay.pulse":
                        result = IntegrationResult(True, "Pulso do relé concluído via USB.")
                    elif kind == "storage.sync":
                        result = IntegrationResult(True, "Sincronização do microSD solicitada via USB.")
                    else:
                        result = IntegrationResult(True, f"Comando {kind} concluído via USB.")
                elif kind in {"status", "sd-status"}:
                    status = get_commissioning_status(profile, self.secret_manager)
                    if kind == "sd-status" and status.device is not None:
                        storage = [
                            item
                            for item in status.accessories
                            if item.get("kind") == "storage"
                        ]
                        if not storage:
                            status = type(status)(
                                False,
                                "Capability de armazenamento não publicada pelo ESP",
                                status.device,
                                status.accessories,
                            )
                        else:
                            accessory = storage[0]
                            status = type(status)(
                                status.ok and bool(accessory.get("available")),
                                f"SD {'online' if accessory.get('available') else 'indisponível'}; "
                                f"capabilities={accessory.get('capabilities', {})}; state={accessory.get('state', {})}",
                                status.device,
                                status.accessories,
                            )
                    result = IntegrationResult(status.ok, status.message)
                else:
                    result = send_accessory_command(
                        profile,
                        self.secret_manager,
                        kind,
                        params or {},
                    )
            except Exception as exc:
                result = IntegrationResult(False, f"Falha no comissionamento: {exc}")
            self.post_message(
                CommissionDone(
                    result.message,
                    result.ok,
                    target,
                    action=kind,
                    data=payload,
                    source=source,
                )
            )

        def complete_relay_pulse(self, confirmed: bool) -> None:
            pending = getattr(self, "_pending_relay_pulse", None)
            port = getattr(self, "_pending_relay_port", None)
            self._pending_relay_pulse = None
            self._pending_relay_port = None
            if not confirmed or pending is None:
                self.query_one("#commission-result", Static).update(
                    "Pulso cancelado; o relé não foi acionado."
                )
                return
            relay_id, duration = pending
            self.query_one("#commission-result", Static).update(
                "Enviando pulso confirmado…"
            )
            self.commission_operation(
                "relay.pulse",
                {"relay_id": relay_id, "duration_ms": duration},
                port=port,
            )

        def on_integration_done(self, event: IntegrationDone) -> None:
            self.query_one("#integration-result", Static).update(event.text)
            self.notify(event.text, severity="information" if event.ok else "error")

        def on_onboarding_done(self, event: OnboardingDone) -> None:
            if event.error:
                self._test_after_apply = False
                self._monitor_after_apply = False
                self._automatic_setup_active = False
                self.query_one("#integration-result", Static).update(event.error)
                self.notify(f"Onboarding falhou: {event.error}", severity="error")
                self.query_one("#simple-run-button", Button).disabled = False
                self.query_one("#simple-status-line", Static).update(
                    "A placa está pronta, mas a integração com o servidor falhou. "
                    "Tente novamente ou revise as opções avançadas."
                )
                return
            self.current_profile.refresh_features()
            self.profile_store.save(self.current_profile)
            self.current_is_draft = False
            reference = event.result.device_token_ref
            message = (
                f"{event.result.message}\nReferência do token do dispositivo: {reference}"
            )
            self.query_one("#integration-result", Static).update(message)
            self.populate_form(self.current_profile)
            self.refresh_catalogue()
            self.notify(event.result.message, severity="information")
            if event.continue_apply:
                self.continue_apply_steps()

        def on_pairing_done(self, event: PairingDone) -> None:
            continuation = self._action_after_pairing
            self._action_after_pairing = None
            if event.error:
                self.query_one("#status-line", Static).update("Pareamento falhou")
                message = f"Pareamento falhou: {event.error}"
                self.notify(message, severity="error")
                self.query_one("#profile-message", Static).update(message)
                self.query_one("#run-live-log", RichLog).write(f"[ERROR] {message}")
                self._automatic_setup_active = False
                self.query_one("#simple-run-button", Button).disabled = False
                self.query_one("#simple-status-line", Static).update(
                    "Não consegui identificar a placa. Confira o cabo USB e tente novamente."
                )
                return
            if event.profile is not None:
                self.current_profile = event.profile
                self.current_is_draft = event.is_draft
            if self.current_profile.profile_id in self._automatic_profile_ids:
                finish_automatic_identity(self.current_profile)
            if (
                not self.current_profile.mqtt.enabled
                and self.current_profile.server.admin_bearer_token_ref
                == self.secret_manager.reference(
                    self.current_profile.profile_id, "server-admin-bearer"
                )
            ):
                prepare_local_dashboard_profile(
                    self.current_profile,
                    self.secret_manager,
                    lan_host=self.current_profile.mqtt.broker_host or None,
                )
            self.profile_store.save_draft(self.current_profile)
            self.current_is_draft = True
            self.populate_form(self.current_profile)
            self.refresh_catalogue()
            self.notify(
                f"Pareado: {event.identity.chip}, {event.identity.mac}",
                severity="information",
            )
            self.query_one("#simple-device-summary", Static).update(
                f"{self.current_profile.friendly_name} identificada e associada com segurança."
            )
            self.query_one("#simple-status-line", Static).update(
                "Placa identificada. Preparando o firmware…"
            )
            self.refresh_simple_plan()
            if continuation == "apply":
                self.call_after_refresh(self.continue_apply_steps)
            elif continuation == "run-current":
                self.call_after_refresh(self._run_current_profile)
            elif continuation:
                self.call_after_refresh(self.start_build_action, continuation)

        def on_batch_done(self, event: BatchDone) -> None:
            self.running = False
            lines = []
            failures = 0
            for item, (ok, message) in event.results:
                lines.append(f"{'OK' if ok else 'FALHA'} — {item.friendly_name}: {message}")
                failures += 0 if ok else 1
            self.query_one("#profile-message", Static).update(
                "Resumo da aplicação em lote:\n" + "\n".join(lines)
            )
            self.refresh_catalogue()
            self.refresh_diagnostics()
            self.notify(
                f"Lote concluído: {len(event.results) - failures} OK, {failures} falha(s).",
                severity="error" if failures else "information",
            )

        def on_provision_done(self, event: ProvisionDone) -> None:
            if event.error:
                self._test_after_apply = False
                self.query_one("#status-line", Static).update("Provisionamento falhou")
                self.notify(f"Provisionamento falhou: {event.error}", severity="error")
                self._automatic_setup_active = False
                self.query_one("#simple-run-button", Button).disabled = False
                self.query_one("#simple-status-line", Static).update(
                    "A gravação terminou, mas a configuração não foi confirmada. Tente novamente."
                )
                return
            result = event.result
            if result.device_id and not self.current_profile.device_id:
                self.current_profile.device_id = result.device_id
            self.current_profile.applied.build_hash = result.build_hash
            self.current_profile.applied.config_hash = result.config_hash
            self.current_profile.applied.last_seen_at = utc_now()
            self.profile_store.save(self.current_profile)
            self.current_is_draft = False
            self.query_one("#status-line", Static).update("Provisionado e confirmado por readback")
            self.query_one("#simple-status-line", Static).update(
                "Configuração confirmada pela placa. Iniciando testes físicos…"
            )
            self.notify("Configuração provisionada atomicamente.", severity="information")
            self.refresh_catalogue()
            self.refresh_diagnostics()
            if self._hardware_validation_after_apply:
                self.start_hardware_validation()
            elif self._monitor_after_apply:
                self._monitor_after_apply = False
                self.start_build_action("monitor")
            else:
                self._automatic_setup_active = False
                self.query_one("#simple-run-button", Button).disabled = False
                self.start_post_apply_tests()

        def on_sd_done(self, event: SdDone) -> None:
            self.query_one("#storage-result", Static).update(event.text)
            self.notify(event.text, severity="information" if event.ok else "error")

        def on_commission_done(self, event: CommissionDone) -> None:
            if event.action == "ir.learn.start":
                self._ir_learning_active = False
                self._ir_cancel_event.clear()
            if event.ok and event.action == "ir.learn.start" and event.data:
                code = event.data.get("code")
                if isinstance(code, dict) and isinstance(code.get("sequence_us"), list):
                    self._last_ir_code = code
                    self._save_learned_ir_code(code)
                    self.query_one("#commission-ir-code-input", Input).value = str(
                        code.get("code_id") or "última-captura"
                    )
            selector = "#storage-result" if event.target == "storage" else "#commission-result"
            self.query_one(selector, Static).update(event.text)
            self.notify(event.text, severity="information" if event.ok else "error")

        def on_input_changed(self, event: Input.Changed) -> None:
            if self._loading_form:
                return
            self.autosave_draft()
            self.update_command_preview()

        def on_switch_changed(self, event: Switch.Changed) -> None:
            if self._loading_form:
                return
            self.autosave_draft()
            self.refresh_diagnostics()

        def on_select_changed(self, event: Select.Changed) -> None:
            if self._loading_form:
                return
            # Textual may deliver a queued Changed event after an automatic
            # refresh has already selected a newer value.  Never let that stale
            # event disable the one-click action or autosave the wrong port.
            if event.value != event.select.value:
                return
            if event.select.id == "simple-device-select":
                candidate = self._automatic_candidates.get(str(event.value))
                self.query_one("#simple-run-button", Button).disabled = candidate is None
                if candidate is not None:
                    self.activate_automatic_candidate(candidate)
                    if not self._automatic_setup_active:
                        self.query_one("#simple-status-line", Static).update(
                            "Pronto. Pressione “Configurar e testar automaticamente”."
                        )
                return
            if event.select.id == "port-select":
                value = str(event.value)
                self.query_one("#port-input", Input).display = value == MANUAL_PORT
                if value not in {AUTO_PORT, MANUAL_PORT}:
                    for info in self._port_infos:
                        if info.get("device") == value:
                            self.current_profile.association.vid = str(info.get("vid") or "")
                            self.current_profile.association.pid = str(info.get("pid") or "")
                            self.current_profile.association.serial_number = str(info.get("serial_number") or "")
                            suggested = info.get("suggested_profile")
                            if suggested in api.HARDWARE_PROFILES:
                                self.query_one("#device-target-select", Select).value = suggested
                            break
            elif event.select.id == "device-target-select":
                self.query_one("#build-target-static", Static).update(f"Alvo: {event.value}")
            self.autosave_draft()
            self.refresh_diagnostics()
            self.update_command_preview()

        def on_tabbed_content_tab_activated(self, event) -> None:
            pane = getattr(event, "pane", None)
            tab_id = getattr(pane, "id", None)
            # The native tab strip is intentionally hidden.  A queued activation
            # for the former "Início" tab must not undo an explicit click on
            # "Opções avançadas"; returning home is handled by its own button.
            if tab_id == "simple-tab":
                return
            if tab_id in {item[0] for item in self.WIZARD_STEPS} | {"help-tab"}:
                self._last_advanced_tab = tab_id
                self.update_wizard_controls(tab_id)

        def on_data_table_row_selected(self, event: DataTable.RowSelected) -> None:
            key = str(event.row_key.value)
            if event.data_table.id == "device-table" and key.startswith("device:"):
                profile_id = key.split(":", 1)[1]
                self.current_profile = self.profile_store.load(profile_id)
                self.current_is_draft = not self.profile_store.profile_path(profile_id).exists()
                self.populate_form(self.current_profile)
            elif event.data_table.id == "ports-table" and key.startswith("port:"):
                port = key.split(":", 1)[1]
                if port:
                    self.query_one("#port-select", Select).value = port
            elif event.data_table.id == "issues-table" and key.startswith("issue:"):
                try:
                    issue_index = int(key.split(":", 1)[1])
                    issue = self._validation_issues[issue_index]
                except (ValueError, IndexError):
                    return
                if not self.open_validation_issue(issue):
                    self.notify(
                        f"Revise {issue.path}: {issue.message}", severity="warning"
                    )

        def on_button_pressed(self, event: Button.Pressed) -> None:
            button_id = event.button.id
            if button_id == "simple-run-button":
                self.action_automatic_setup()
            elif button_id and button_id.startswith("hardware-card-"):
                service = self._hardware_validation_service
                session = service.session if service is not None else None
                if session is None:
                    self.notify("Inicie a configuração automática primeiro.", severity="warning")
                    return
                card_id = button_id.removeprefix("hardware-card-").replace("-", "_")
                if card_id == "relays":
                    candidates = [
                        check
                        for key, check in session.checks.items()
                        if key.startswith("relay:")
                        and check.state in {
                            ValidationState.FAILED,
                            ValidationState.BLOCKED,
                            ValidationState.WAITING,
                        }
                    ]
                    check = candidates[0] if candidates else None
                else:
                    check = session.checks.get(card_id)
                if check is None:
                    return
                try:
                    if check.state in {ValidationState.FAILED, ValidationState.BLOCKED}:
                        service.retry_check(check.check_id)
                except HardwareValidationError as exc:
                    self.notify(str(exc), severity="error")
                    return
                self._automatic_setup_active = True
                self.query_one("#simple-run-button", Button).disabled = True
                self.refresh_hardware_validation_cards()
                self.call_after_refresh(self._next_hardware_validation_step)
            elif button_id == "simple-refresh-button":
                self.refresh_ports()
                self.notify("Busca por placas atualizada.", severity="information")
            elif button_id == "advanced-mode-button":
                self.set_advanced_mode(True)
            elif button_id == "wizard-home-button":
                self.set_advanced_mode(False)
            elif button_id == "wizard-sections-button":
                self.show_advanced_sections()
            elif button_id == "new-profile-button":
                self.current_profile = new_device_profile()
                self.current_is_draft = True
                self.profile_store.save_draft(self.current_profile)
                self.refresh_catalogue()
                self.populate_form(self.current_profile)
            elif button_id == "clone-profile-button":
                self.current_profile = self.profile_store.clone(self.collect_profile())
                self.current_is_draft = True
                self.profile_store.save_draft(self.current_profile)
                self.refresh_catalogue()
                self.populate_form(self.current_profile)
            elif button_id == "pair-board-button":
                try:
                    port = self.selected_pairing_port()
                except Exception as exc:
                    self.notify(str(exc), severity="error")
                else:
                    self.notify(f"Lendo eFuse MAC em {port}…", severity="information")
                    self.pair_board(port)
            elif button_id == "save-profile-button":
                self.save_current()
            elif button_id == "validate-profile-button":
                ok = self.validate_and_report()
                self.notify("Configuração válida." if ok else "Configuração inválida.", severity="information" if ok else "error")
            elif button_id == "delete-profile-button":
                self.profile_store.delete(self.current_profile.profile_id)
                catalogue = self.profile_store.list(include_drafts=True)
                self.current_profile = catalogue[0][0] if catalogue else new_device_profile()
                self.current_is_draft = True if not catalogue else catalogue[0][1]
                if not catalogue:
                    self.profile_store.save_draft(self.current_profile)
                self.refresh_catalogue()
                self.populate_form(self.current_profile)
            elif button_id == "import-profile-button":
                path = self.query_one("#profile-path-input", Input).value.strip()
                try:
                    if Path(path).suffix.lower() == ".zip":
                        try:
                            catalogue_preview = self.profile_store.preview_catalogue_zip(path)
                        except ValueError as catalogue_error:
                            if "catalogue.json" not in str(catalogue_error):
                                raise
                        else:
                            self._pending_catalogue_preview = catalogue_preview
                            self.push_screen(
                                CatalogueImportConfirmScreen(catalogue_preview),
                                self.complete_catalogue_import,
                            )
                            return
                    preview = self.profile_store.preview_import(
                        path, compare_to=self.current_profile
                    )
                    self._pending_import_preview = preview
                    self.push_screen(
                        ImportConfirmScreen(preview), self.complete_profile_import
                    )
                except Exception as exc:
                    self.notify(f"Falha ao importar: {exc}", severity="error")
            elif button_id == "export-profile-button":
                path = self.query_one("#profile-path-input", Input).value.strip()
                try:
                    destination = Path(path)
                    if not destination.suffix:
                        destination = destination / f"{self.current_profile.profile_id}.json"
                    self.profile_store.export_profile(self.collect_profile(), destination)
                    self.query_one("#profile-message", Static).update(f"Exportado sem segredos: {destination}")
                except Exception as exc:
                    self.notify(f"Falha ao exportar: {exc}", severity="error")
            elif button_id == "export-catalogue-button":
                self.save_current(notify=False)
                path = self.query_one("#profile-path-input", Input).value.strip()
                try:
                    destination = (
                        Path(path)
                        if path
                        else self.profile_store.root.parent / "aethersense-devices.zip"
                    )
                    if destination.suffix.lower() != ".zip":
                        destination = (
                            destination / "aethersense-devices.zip"
                            if not destination.suffix
                            else destination.with_suffix(".zip")
                        )
                    profiles = [
                        profile
                        for profile, _draft in self.profile_store.list(include_drafts=True)
                    ]
                    self.profile_store.export_catalogue_zip(profiles, destination)
                    self.query_one("#profile-message", Static).update(
                        f"Catálogo determinístico ({len(profiles)} perfis) exportado sem segredos: {destination}"
                    )
                except Exception as exc:
                    self.notify(f"Falha ao exportar catálogo: {exc}", severity="error")
            elif button_id == "batch-plan-button":
                self.save_current(notify=False)
                plan = plan_batch(self.profile_store.list(include_drafts=True))
                lines = [
                    f"{item.friendly_name}: {item.operation}"
                    + (f" — {item.blocked_reason}" if item.blocked_reason else "")
                    for item in plan.items
                ]
                self.query_one("#profile-message", Static).update(
                    "Plano em lote (ordem estável):\n" + "\n".join(lines)
                )
            elif button_id == "batch-apply-button":
                self.save_current(notify=False)
                plan = plan_batch(self.profile_store.list(include_drafts=True))
                if not plan.actionable:
                    self.query_one("#profile-message", Static).update(
                        "Nenhum perfil pode ser aplicado. Revise os itens bloqueados no planejamento."
                    )
                else:
                    self._pending_batch_plan = plan
                    self.push_screen(
                        BatchConfirmScreen(plan), self.complete_batch_apply
                    )
            elif button_id == "wizard-previous-button":
                self.action_wizard_previous()
            elif button_id == "wizard-next-step-button":
                self.action_wizard_next()
            elif button_id == "wizard-validate-button":
                self.validate_wizard_step()
            elif button_id == "wizard-finish-button":
                self.finish_wizard()
            elif button_id == "refresh-ports-button":
                self.refresh_ports()
            elif button_id == "run-button":
                self.action_run_selected()
            elif button_id == "monitor-button":
                self.start_build_action("monitor")
            elif button_id == "advanced-run-button":
                self.start_build_action(
                    str(self.query_one("#action-select", Select).value)
                )
            elif button_id == "cancel-button":
                self.action_cancel_or_quit()
            elif button_id == "apply-button":
                self.save_current(notify=False)
                if self.current_profile.server.base_url:
                    self.start_onboarding(continue_apply=True)
                else:
                    self.continue_apply_steps()
            elif button_id == "local-broker-button":
                try:
                    runtime = Path(self.settings.project_dir).parent / "mosquitto" / "runtime"
                    manager = LocalBrokerManager(runtime, self.secret_manager)
                    username, reference, acl = manager.prepare_device(self.collect_profile())
                    self.query_one("#mqtt-user-input", Input).value = username
                    self.query_one("#integration-result", Static).update(
                        f"ACL: {acl}\nCredencial armazenada como {reference}; senha não exibida."
                    )
                    self.save_current(notify=False)
                except Exception as exc:
                    self.notify(str(exc), severity="error")
            elif button_id == "local-broker-start-button":
                self.save_current(notify=False)
                self.query_one("#integration-result", Static).update(
                    "Iniciando/recarregando broker local…"
                )
                self.start_local_broker_service()
            elif button_id == "test-mqtt-button":
                self.save_current(notify=False)
                self.test_integration("mqtt")
            elif button_id == "test-server-button":
                self.save_current(notify=False)
                self.test_integration("server")
            elif button_id == "onboard-button":
                self.start_onboarding()
            elif button_id == "rotate-token-button":
                self.start_onboarding(rotate_token=True)
            elif button_id == "commission-status-button":
                self.save_current(notify=False)
                self.commission_operation("status", port=self.optional_commission_port())
            elif button_id == "commission-ir-learn-button":
                self.save_current(notify=False)
                temporary_id = f"commission-{uuid4().hex[:12]}"
                port = self.optional_commission_port()
                self._ir_cancel_event.clear()
                self._ir_learning_active = bool(port)
                self.query_one("#commission-result", Static).update(
                    "Aguardando sinal IR por USB… aponte o controle e pressione uma tecla."
                    if port
                    else "Aguardando aprendizado pela integração remota…"
                )
                self.commission_operation(
                    "ir.learn.start",
                    {
                        "code_id": temporary_id,
                        "name": "Captura temporária de comissionamento",
                        "timeout_ms": 15_000,
                    },
                    port=port,
                )
            elif button_id == "commission-ir-cancel-button":
                if self._ir_learning_active:
                    self._ir_cancel_event.set()
                    self.query_one("#commission-result", Static).update("Cancelando captura IR…")
                else:
                    self.commission_operation(
                        "ir.learn.cancel",
                        {},
                        port=self.optional_commission_port(),
                    )
            elif button_id == "commission-ir-send-button":
                code_id = self.query_one("#commission-ir-code-input", Input).value.strip()
                port = self.optional_commission_port()
                if self._last_ir_code is not None and port:
                    code = self._last_ir_code
                    self.commission_operation(
                        "ir.send",
                        {
                            "code_id": str(code.get("code_id") or code_id or "ultima-captura"),
                            "carrier_hz": int(code.get("carrier_hz") or 38_000),
                            "duty_percent": int(code.get("duty_percent") or 33),
                            "sequence_us": list(code.get("sequence_us") or []),
                            "repeats": 1,
                        },
                        port=port,
                    )
                elif not code_id:
                    self.notify("Aprenda um sinal IR primeiro.", severity="error")
                else:
                    self.commission_operation("ir.send", {"code_id": code_id})
            elif button_id == "commission-relay-button":
                relay_id = self.query_one("#commission-relay-id-input", Input).value.strip()
                duration = _integer(
                    self.query_one("#commission-relay-duration-input", Input).value,
                    0,
                )
                relay = next(
                    (
                        item
                        for item in self.current_profile.relays
                        if item.enabled and item.relay_id == relay_id
                    ),
                    None,
                )
                if relay is None or not 1 <= duration <= relay.pulse_limit_ms:
                    self.notify(
                        "Escolha um relé habilitado e duração dentro do limite do perfil.",
                        severity="error",
                    )
                else:
                    self._pending_relay_pulse = (relay_id, duration)
                    self._pending_relay_port = self.optional_commission_port()
                    self.push_screen(
                        RelayPulseConfirmScreen(relay.name, duration),
                        self.complete_relay_pulse,
                    )
            elif button_id == "commission-sd-status-button":
                self.commission_operation(
                    "sd-status",
                    target="storage",
                    port=self.optional_commission_port(),
                )
            elif button_id == "commission-sd-sync-button":
                self.commission_operation(
                    "storage.sync",
                    {"storage_id": "sd"},
                    target="storage",
                    port=self.optional_commission_port(),
                )
            elif button_id in {"inspect-sd-button", "import-sd-button"}:
                path = Path(self.query_one("#sd-path-input", Input).value.strip())
                if not path.exists():
                    self.query_one("#storage-result", Static).update("Caminho não encontrado.")
                else:
                    self.query_one("#storage-result", Static).update("Processando cartão…")
                    self.sd_operation("import" if button_id == "import-sd-button" else "inspect", path)

    return FirmwareBuildApp()
