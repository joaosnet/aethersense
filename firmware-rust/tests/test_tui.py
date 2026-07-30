import asyncio
from pathlib import Path

import build as build_tool
from textual.widgets import Button, DataTable, Input, Select, Static, Switch, TabbedContent, Tabs

from aethersense_build.models import new_device_profile
from aethersense_build.secrets import MemorySecretStore, SecretManager
from aethersense_build.store import DeviceProfileStore
from aethersense_build.tui import _pin
from aethersense_build.validation import validate_profile


def test_pin_parser_preserves_explicit_gpio() -> None:
    assert _pin("8") == 8
    assert _pin(" 27 ") == 27
    assert _pin("") is None
    assert _pin("GPIO8") is None


def test_tui_collects_explicit_peripheral_pins_and_opens_invalid_field(
    tmp_path: Path,
) -> None:
    async def scenario() -> None:
        store = DeviceProfileStore(tmp_path / "devices")
        app = build_tool.create_tui_app(
            build_tool.BuildSettings(project_dir=str(Path(build_tool.__file__).parent)),
            build_tool.BuildOptions(action="build"),
            profile_store=store,
            secret_manager=SecretManager(MemorySecretStore()),
            port_provider=lambda: [],
        )
        async with app.run_test(size=(140, 46)) as pilot:
            app.query_one("#device-target-select", Select).value = "esp32c5"
            app.query_one("#board-variant-select", Select).value = "esp32-c5-devkitc-1"
            app.query_one("#led-switch", Switch).value = True
            app.query_one("#led-gpio-input", Input).value = ""
            await pilot.pause()

            assert not app.validate_and_report()
            await pilot.pause()
            assert app.query_one("#main-tabs", TabbedContent).active == "config-tab"
            assert app.query_one("#led-gpio-input", Input).has_focus

            app.query_one("#led-gpio-input", Input).value = "27"
            app.query_one("#ir-rx-switch", Switch).value = True
            app.query_one("#ir-rx-gpio-input", Input).value = "8"
            app.query_one("#ir-tx-switch", Switch).value = True
            app.query_one("#ir-tx-gpio-input", Input).value = "9"
            app.query_one("#sd-switch", Switch).value = True
            app.query_one("#sd-sclk-input", Input).value = "0"
            app.query_one("#sd-mosi-input", Input).value = "1"
            app.query_one("#sd-miso-input", Input).value = "6"
            app.query_one("#sd-cs-input", Input).value = "10"
            await pilot.pause()

            profile = app.collect_profile()
            assert profile.runtime.status_led.data_gpio == 27
            assert profile.ir_rx.gpio == 8
            assert profile.ir_tx.gpio == 9
            assert (
                profile.storage.sclk_gpio,
                profile.storage.mosi_gpio,
                profile.storage.miso_gpio,
                profile.storage.cs_gpio,
            ) == (0, 1, 6, 10)
            assert not [
                issue
                for issue in validate_profile(profile)
                if issue.code == "pin.required"
            ]

    asyncio.run(scenario())


def test_tui_mounts_headless_and_saves_profile(tmp_path: Path) -> None:
    async def scenario() -> None:
        store = DeviceProfileStore(tmp_path / "devices")
        app = build_tool.create_tui_app(
            build_tool.BuildSettings(project_dir=str(Path(build_tool.__file__).parent)),
            build_tool.BuildOptions(action="build"),
            profile_store=store,
            secret_manager=SecretManager(MemorySecretStore()),
            port_provider=lambda: [],
        )
        async with app.run_test(size=(140, 46)) as pilot:
            app.set_advanced_mode(True)
            await pilot.pause()
            assert app.query_one("#device-table", DataTable).row_count >= 1
            app.query_one("#friendly-name-input", Input).value = "C5 bancada"
            await pilot.pause()
            await pilot.click("#save-profile-button")
            await pilot.pause()
            profiles = store.list(include_drafts=True)
            assert profiles[0][0].friendly_name == "C5 bancada"

    asyncio.run(scenario())


def test_tui_opens_in_simple_mode_and_ignores_bluetooth(tmp_path: Path) -> None:
    async def scenario() -> None:
        store = DeviceProfileStore(tmp_path / "devices")
        app = build_tool.create_tui_app(
            build_tool.BuildSettings(project_dir=str(Path(build_tool.__file__).parent)),
            build_tool.BuildOptions(action="build"),
            profile_store=store,
            secret_manager=SecretManager(MemorySecretStore()),
            port_provider=lambda: [
                {
                    "device": "COM3",
                    "description": "Serial Padrão por link Bluetooth",
                    "hwid": "BTHENUM",
                    "suggested_profile": "unknown",
                }
            ],
        )
        async with app.run_test(size=(80, 24)) as pilot:
            await pilot.pause()
            tabs = app.query_one("#main-tabs", TabbedContent)
            assert tabs.active == "simple-tab"
            assert not app._advanced_mode
            assert not tabs.query_one(Tabs).display
            assert app.query_one("#wizard-controls").styles.display == "none"
            assert app.query_one("#simple-run-button", Button).disabled
            assert "cabo usb" in str(
                app.query_one("#simple-device-summary", Static).content
            ).lower()
            assert str(app.query_one("#simple-run-button", Button).label) == (
                "Configurar e testar automaticamente"
            )
            assert len(app.query(".hardware-test-card")) == 9

    asyncio.run(scenario())


def test_tui_uses_card_section_picker_instead_of_native_tabs(tmp_path: Path) -> None:
    async def scenario() -> None:
        store = DeviceProfileStore(tmp_path / "devices")
        app = build_tool.create_tui_app(
            build_tool.BuildSettings(project_dir=str(Path(build_tool.__file__).parent)),
            build_tool.BuildOptions(action="build"),
            profile_store=store,
            secret_manager=SecretManager(MemorySecretStore()),
            port_provider=lambda: [],
        )
        async with app.run_test(size=(80, 24)) as pilot:
            await pilot.pause()
            tabs = app.query_one("#main-tabs", TabbedContent)
            native_tabs = tabs.query_one(Tabs)
            assert not native_tabs.display

            await pilot.click("#advanced-mode-button")
            await pilot.pause()
            assert tabs.active == "devices-tab"
            assert app.query_one("#wizard-controls").styles.display != "none"
            assert "1/7" in str(app.query_one("#wizard-step-label", Static).content)
            assert not native_tabs.display

            await pilot.click("#wizard-sections-button")
            await pilot.pause()
            cards = list(app.screen.query(".advanced-section-card"))
            assert len(cards) == 8
            dialog = app.screen.query_one("#advanced-sections-dialog")
            assert dialog.region.right <= app.size.width
            assert dialog.region.bottom <= app.size.height

            app.screen.query_one("#advanced-section-storage-tab", Button).focus()
            await pilot.press("enter")
            await pilot.pause()
            assert tabs.active == "storage-tab"
            assert "5/7" in str(app.query_one("#wizard-step-label", Static).content)
            assert str(app.query_one("#wizard-previous-button", Button).label) == "←"
            assert str(app.query_one("#wizard-next-step-button", Button).label) == "→"

            await pilot.resize_terminal(140, 46)
            await pilot.pause()
            assert str(app.query_one("#wizard-step-label", Static).content) == "5/7"
            assert str(app.query_one("#wizard-next-step-button", Button).label) == "→"

            await pilot.resize_terminal(80, 24)
            await pilot.pause()
            assert str(app.query_one("#wizard-step-label", Static).content) == "5/7"

            await pilot.click("#wizard-home-button")
            await pilot.pause()
            assert tabs.active == "simple-tab"
            assert not app._advanced_mode

            await pilot.click("#advanced-mode-button")
            await pilot.pause()
            assert tabs.active == "storage-tab"
            assert not native_tabs.display

            await pilot.click("#wizard-sections-button")
            await pilot.pause()
            await pilot.press("escape")
            await pilot.pause()
            assert tabs.active == "storage-tab"

            await pilot.click("#wizard-sections-button")
            await pilot.pause()
            app.screen.query_one("#advanced-section-help-tab", Button).focus()
            await pilot.press("enter")
            await pilot.pause()
            assert tabs.active == "help-tab"
            assert "Ajuda" in str(app.query_one("#wizard-step-label", Static).content)
            assert app.query_one("#wizard-previous-button", Button).styles.display == "none"
            assert app.query_one("#wizard-sections-button", Button).display

    asyncio.run(scenario())


def test_tui_centers_simple_cards_and_survives_terminal_resize(tmp_path: Path) -> None:
    async def scenario() -> None:
        store = DeviceProfileStore(tmp_path / "devices")
        app = build_tool.create_tui_app(
            build_tool.BuildSettings(project_dir=str(Path(build_tool.__file__).parent)),
            build_tool.BuildOptions(action="build"),
            profile_store=store,
            secret_manager=SecretManager(MemorySecretStore()),
            port_provider=lambda: [],
        )
        async with app.run_test(size=(140, 46)) as pilot:
            await pilot.pause()
            simple_view = app.query_one("#simple-view")
            assert simple_view.region.x > 0
            assert simple_view.region.right <= app.size.width

            await pilot.resize_terminal(80, 24)
            await pilot.pause()
            assert simple_view.region.x >= 0
            assert simple_view.region.right <= app.size.width
            assert not app.query_one("#main-tabs", TabbedContent).query_one(Tabs).display

    asyncio.run(scenario())


def test_tui_multiple_boards_requires_only_one_friendly_choice(tmp_path: Path) -> None:
    async def scenario() -> None:
        store = DeviceProfileStore(tmp_path / "devices")
        ports = [
            {
                "device": "COM9",
                "description": "ESP32-C5 bancada",
                "vid": "303A",
                "pid": "1001",
                "serial_number": "C5-A",
                "suggested_profile": "esp32c5",
            },
            {
                "device": "COM10",
                "description": "ESP32-C5 sala",
                "vid": "303A",
                "pid": "1001",
                "serial_number": "C5-B",
                "suggested_profile": "esp32c5",
            },
        ]
        app = build_tool.create_tui_app(
            build_tool.BuildSettings(project_dir=str(Path(build_tool.__file__).parent)),
            build_tool.BuildOptions(action="run"),
            profile_store=store,
            secret_manager=SecretManager(MemorySecretStore()),
            port_provider=lambda: ports,
        )
        async with app.run_test(size=(100, 32)) as pilot:
            await pilot.pause()
            assert app.query_one("#simple-run-button", Button).disabled
            assert "mais de uma placa" in str(
                app.query_one("#simple-device-summary", Static).content
            ).lower()
            wanted = next(
                key
                for key, candidate in app._automatic_candidates.items()
                if candidate.port == "COM10"
            )
            app.query_one("#simple-device-select", Select).value = wanted
            await pilot.pause()
            paired: list[str] = []
            app.pair_board = lambda port: paired.append(port)

            await pilot.click("#simple-run-button")
            await pilot.pause()

            assert paired == ["COM10"]

    asyncio.run(scenario())


def test_tui_pairs_before_server_onboarding(tmp_path: Path) -> None:
    async def scenario() -> None:
        store = DeviceProfileStore(tmp_path / "devices")
        app = build_tool.create_tui_app(
            build_tool.BuildSettings(project_dir=str(Path(build_tool.__file__).parent)),
            build_tool.BuildOptions(action="run"),
            profile_store=store,
            secret_manager=SecretManager(MemorySecretStore()),
            port_provider=lambda: [
                {
                    "device": "COM9",
                    "description": "ESP32-C5",
                    "serial_number": "C5",
                    "suggested_profile": "esp32c5",
                }
            ],
        )
        async with app.run_test(size=(100, 32)) as pilot:
            await pilot.pause()
            app.query_one("#server-url-input", Input).value = "http://127.0.0.1:3000"
            await pilot.pause()
            calls: list[tuple[str, object]] = []
            app.start_pairing_for = lambda continuation: calls.append(
                ("pair", continuation)
            )
            app.start_onboarding = lambda **kwargs: calls.append(("onboard", kwargs))

            assert app._run_current_profile()

            assert calls == [("pair", "run-current")]

    asyncio.run(scenario())


def test_tui_simple_mode_infers_new_c5_and_pairs_once(tmp_path: Path) -> None:
    async def scenario() -> None:
        store = DeviceProfileStore(tmp_path / "devices")
        app = build_tool.create_tui_app(
            build_tool.BuildSettings(project_dir=str(Path(build_tool.__file__).parent)),
            build_tool.BuildOptions(action="run"),
            profile_store=store,
            secret_manager=SecretManager(MemorySecretStore()),
            port_provider=lambda: [
                {
                    "device": "COM4",
                    "description": "Bluetooth",
                    "hwid": "BTHENUM",
                    "suggested_profile": "unknown",
                },
                {
                    "device": "COM9",
                    "description": "USB-SERIAL CH340",
                    "manufacturer": "wch.cn",
                    "vid": "1A86",
                    "pid": "7523",
                    "serial_number": "USB-C5",
                    "suggested_profile": "esp32c5",
                },
            ],
        )
        async with app.run_test(size=(100, 32)) as pilot:
            await pilot.pause()
            assert app.current_profile.target.profile == "esp32c5"
            assert app.current_profile.runtime.status_led.data_gpio == 27
            assert not app.current_profile.ir_rx.enabled
            assert not app.current_profile.ir_tx.enabled
            paired_ports: list[str] = []
            app.pair_board = lambda port: paired_ports.append(port)

            await pilot.click("#simple-run-button")
            await pilot.pause()

            assert paired_ports == ["COM9"]
            assert app._action_after_pairing == "run-current"
            assert app._hardware_validation_after_apply
            assert not app._monitor_after_apply
            assert "identificando" in str(
                app.query_one("#simple-status-line", Static).content
            ).lower()

    asyncio.run(scenario())


def test_tui_simple_mode_selects_connected_saved_profile_and_flashes(tmp_path: Path) -> None:
    async def scenario() -> None:
        store = DeviceProfileStore(tmp_path / "devices")
        disconnected = new_device_profile("A desconectado", "classic")
        disconnected.device_id = "esp32-001122334455"
        disconnected.association.efuse_mac = "00:11:22:33:44:55"
        disconnected.association.serial_number = "OTHER"
        store.save(disconnected)

        connected = new_device_profile("Z C5 conectada", "esp32c5")
        connected.device_id = "esp32-aabbccddeeff"
        connected.association.efuse_mac = "aa:bb:cc:dd:ee:ff"
        connected.association.vid = "303A"
        connected.association.pid = "1001"
        connected.association.serial_number = "USB-C5"
        connected.ir_rx.enabled = True
        connected.ir_rx.gpio = 8
        connected.ir_tx.enabled = True
        connected.ir_tx.gpio = 9
        store.save(connected)

        app = build_tool.create_tui_app(
            build_tool.BuildSettings(project_dir=str(Path(build_tool.__file__).parent)),
            build_tool.BuildOptions(action="run"),
            profile_store=store,
            secret_manager=SecretManager(MemorySecretStore()),
            port_provider=lambda: [
                {
                    "device": "COM9",
                    "description": "ESP32-C5",
                    "manufacturer": "Espressif",
                    "vid": "303A",
                    "pid": "1001",
                    "serial_number": "USB-C5",
                    "suggested_profile": "esp32c5",
                }
            ],
        )
        async with app.run_test(size=(110, 34)) as pilot:
            await pilot.pause()
            assert app.current_profile.profile_id == connected.profile_id
            assert "Z C5 conectada" in str(
                app.query_one("#simple-plan-summary", Static).content
            )
            started = []
            app.run_build = lambda options: started.append(options)

            await pilot.click("#simple-run-button")
            await pilot.pause()

            assert [options.action for options in started] == ["flash"]
            assert started[0].port == "COM9"
            assert app._provision_after_build
            assert app._hardware_validation_after_apply
            assert not app._monitor_after_apply
            assert app.current_profile.ir_rx.gpio == 8
            assert app.current_profile.ir_tx.gpio == 9

    asyncio.run(scenario())


def test_tui_primary_run_pairs_once_then_starts_run(tmp_path: Path) -> None:
    async def scenario() -> None:
        store = DeviceProfileStore(tmp_path / "devices")
        port_info = {
            "device": "COM9",
            "description": "ESP32-C5",
            "manufacturer": "Espressif",
            "vid": "303A",
            "pid": "1001",
            "serial_number": "USB-C5",
            "suggested_profile": "esp32c5",
        }
        app = build_tool.create_tui_app(
            build_tool.BuildSettings(project_dir=str(Path(build_tool.__file__).parent)),
            build_tool.BuildOptions(action="run"),
            profile_store=store,
            secret_manager=SecretManager(MemorySecretStore()),
            port_provider=lambda: [
                {
                    "device": "COM3",
                    "description": "Bluetooth",
                    "suggested_profile": "unknown",
                },
                port_info,
                {
                    "device": "COM4",
                    "description": "Bluetooth",
                    "suggested_profile": "unknown",
                },
            ],
        )
        async with app.run_test(size=(120, 38)) as pilot:
            app.set_advanced_mode(True)
            await pilot.pause()
            app.query_one("#main-tabs", TabbedContent).active = "build-tab"
            app.query_one("#device-target-select", Select).value = "esp32c5"
            await pilot.pause()

            paired_ports: list[str] = []
            app.pair_board = lambda port: paired_ports.append(port)
            await pilot.click("#run-button")
            await pilot.pause()
            assert paired_ports == ["COM9"]
            assert app._action_after_pairing == "run-current"
            assert not app.running

            app._action_after_pairing = None
            app.current_profile.device_id = "esp32-aabbccddeeff"
            app.current_profile.association.vid = "303A"
            app.current_profile.association.pid = "1001"
            app.current_profile.association.serial_number = "USB-C5"
            app.query_one("#efuse-mac-input", Input).value = "aa:bb:cc:dd:ee:ff"
            started = []
            app.run_build = lambda options: started.append(options)
            await pilot.click("#run-button")
            await pilot.pause()
            assert [options.action for options in started] == ["flash"]
            assert started[0].port == "COM9"
            assert app.running
            assert app._provision_after_build
            assert app._hardware_validation_after_apply
            assert not app._monitor_after_apply

            assert str(app.query_one("#run-button", Button).label) == "Executar no ESP"
            assert app.query_one("#monitor-button", Button)
            assert app.query_one("#advanced-run-button", Button)
            assert len(app.query(".action-card")) == 3
            assert len(app.query(".commission-card")) == 3
            assert str(app.query_one("#commission-ir-learn-button", Button).label) == "Aprender sinal"
            assert "última captura" in str(
                app.query_one("#commission-ir-send-button", Button).label
            ).lower()
            assert not app.query("#provision-button")
            assert app.query_one("#action-select", Select).value == "build"

    asyncio.run(scenario())


def test_ir_cards_prefer_paired_usb_without_server_or_mqtt(tmp_path: Path) -> None:
    async def scenario() -> None:
        store = DeviceProfileStore(tmp_path / "devices")
        profile = new_device_profile("IR direto", "esp32c5")
        profile.device_id = "esp32-aabbccddeeff"
        profile.association.efuse_mac = "aa:bb:cc:dd:ee:ff"
        profile.association.vid = "303A"
        profile.association.pid = "1001"
        profile.association.serial_number = "USB-C5"
        profile.ir_rx.enabled = True
        profile.ir_rx.gpio = 8
        profile.ir_tx.enabled = True
        profile.ir_tx.gpio = 9
        store.save(profile)
        app = build_tool.create_tui_app(
            build_tool.BuildSettings(project_dir=str(Path(build_tool.__file__).parent)),
            build_tool.BuildOptions(action="build"),
            profile_store=store,
            secret_manager=SecretManager(MemorySecretStore()),
            port_provider=lambda: [
                {
                    "device": "COM9",
                    "description": "ESP32-C5",
                    "manufacturer": "Espressif",
                    "vid": "303A",
                    "pid": "1001",
                    "serial_number": "USB-C5",
                    "suggested_profile": "esp32c5",
                }
            ],
        )
        async with app.run_test(size=(120, 40)) as pilot:
            app.set_advanced_mode(True)
            await pilot.pause()
            calls = []

            def capture(kind, params=None, target="integration", port=None):
                calls.append((kind, params or {}, target, port))

            app.commission_operation = capture
            app.query_one("#main-tabs", TabbedContent).active = "integrations-tab"
            await pilot.pause()
            app.query_one("#commission-ir-learn-button", Button).focus()
            await pilot.press("enter")
            await pilot.pause()
            assert calls[-1][0] == "ir.learn.start"
            assert calls[-1][3] == "COM9"
            assert not app.current_profile.mqtt.enabled
            assert not app.current_profile.server.base_url

            app._last_ir_code = {
                "code_id": "power",
                "carrier_hz": 38_000,
                "duty_percent": 33,
                "sequence_us": [9000, 4500, 560, 560],
            }
            app.query_one("#commission-ir-send-button", Button).focus()
            await pilot.press("enter")
            await pilot.pause()
            assert calls[-1] == (
                "ir.send",
                {
                    "code_id": "power",
                    "carrier_hz": 38_000,
                    "duty_percent": 33,
                    "sequence_us": [9000, 4500, 560, 560],
                    "repeats": 1,
                },
                "integration",
                "COM9",
            )

    asyncio.run(scenario())


def test_tui_secret_change_rotates_only_opaque_local_revision(tmp_path: Path) -> None:
    async def scenario() -> None:
        store = DeviceProfileStore(tmp_path / "devices")
        manager = SecretManager(MemorySecretStore())
        app = build_tool.create_tui_app(
            build_tool.BuildSettings(project_dir=str(Path(build_tool.__file__).parent)),
            build_tool.BuildOptions(action="build"),
            profile_store=store,
            secret_manager=manager,
            port_provider=lambda: [],
        )
        async with app.run_test(size=(120, 36)) as pilot:
            app.set_advanced_mode(True)
            await pilot.pause()
            password = app.query_one("#wifi-password-input", Input)
            password.value = "segredo-um"
            await pilot.click("#save-profile-button")
            await pilot.pause()
            first = app.current_profile.secret_revisions["wifi_password"]

            password.value = "segredo-um"
            await pilot.click("#save-profile-button")
            await pilot.pause()
            assert app.current_profile.secret_revisions["wifi_password"] == first

            password.value = "segredo-dois"
            await pilot.click("#save-profile-button")
            await pilot.pause()
            second = app.current_profile.secret_revisions["wifi_password"]
            assert second != first
            assert len(second) == 32
            public = app.current_profile.canonical_json(include_local_state=False)
            local = app.current_profile.canonical_json()
            assert "segredo-um" not in local
            assert "segredo-dois" not in local
            assert "secret_revisions" not in public

    asyncio.run(scenario())


def test_tui_keyboard_shortcut_and_small_terminal(tmp_path: Path) -> None:
    async def scenario() -> None:
        store = DeviceProfileStore(tmp_path / "devices")
        app = build_tool.create_tui_app(
            build_tool.BuildSettings(project_dir=str(Path(build_tool.__file__).parent)),
            build_tool.BuildOptions(action="build"),
            profile_store=store,
            secret_manager=SecretManager(MemorySecretStore()),
            port_provider=lambda: [],
        )
        async with app.run_test(size=(80, 24)) as pilot:
            assert app.query_one("#main-tabs", TabbedContent).active == "simple-tab"
            await pilot.click("#advanced-mode-button")
            await pilot.pause()
            name = app.query_one("#friendly-name-input", Input)
            name.value = "Terminal pequeno"
            name.focus()
            await pilot.press("tab", "shift+tab", "ctrl+s")
            await pilot.pause()
            assert store.list(include_drafts=True)[0][0].friendly_name == "Terminal pequeno"
            assert app.query_one("#main-tabs", TabbedContent).active == "devices-tab"
            assert not app.wizard_issues(0), [str(issue) for issue in app.wizard_issues(0)]
            await pilot.press("f6")
            await pilot.pause()
            assert app.query_one("#main-tabs", TabbedContent).active == "config-tab"
            assert "2/7" in str(app.query_one("#wizard-step-label", Static).content)
            await pilot.press("shift+f6")
            await pilot.pause()
            assert app.query_one("#main-tabs", TabbedContent).active == "devices-tab"
            assert app.query_one("#pair-board-button", Button)
            assert app.query_one("#run-button", Button)

    asyncio.run(scenario())


def test_tui_import_modal_cancel_then_confirm_with_keyboard(tmp_path: Path) -> None:
    async def scenario() -> None:
        store = DeviceProfileStore(tmp_path / "devices")
        source_profile = new_device_profile("Importado")
        source = store.export_profile(source_profile, tmp_path / "source.json")
        app = build_tool.create_tui_app(
            build_tool.BuildSettings(project_dir=str(Path(build_tool.__file__).parent)),
            build_tool.BuildOptions(action="build"),
            profile_store=store,
            secret_manager=SecretManager(MemorySecretStore()),
            port_provider=lambda: [],
        )
        async with app.run_test(size=(100, 30)) as pilot:
            app.set_advanced_mode(True)
            await pilot.pause()
            initial_count = len(store.list(include_drafts=True))
            app.query_one("#profile-path-input", Input).value = str(source)
            import_button = app.query_one("#import-profile-button", Button)
            import_button.focus()
            await pilot.press("enter")
            await pilot.pause()
            assert app.screen.query_one("#import-dialog")
            app.screen.query_one("#import-cancel-button", Button).focus()
            await pilot.press("enter")
            await pilot.pause()
            assert len(store.list(include_drafts=True)) == initial_count
            assert "cancelada" in str(app.query_one("#profile-message", Static).content).lower()

            import_button.focus()
            await pilot.press("enter")
            await pilot.pause()
            app.screen.query_one("#import-confirm-button", Button).focus()
            await pilot.press("enter")
            await pilot.pause()
            assert len(store.list(include_drafts=True)) == initial_count + 1
            assert store.list(include_drafts=True)[0][0].device_id == ""

    asyncio.run(scenario())
