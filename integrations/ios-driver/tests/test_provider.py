"""Fail-closed policy tests for the Sirin-owned iPhone provider."""

import importlib.util
import json
from pathlib import Path


SCRIPT = Path(__file__).parents[1] / "scripts" / "unattended_host.py"
SPEC = importlib.util.spec_from_file_location("sirin_ios_driver_host", SCRIPT)
assert SPEC and SPEC.loader
provider = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(provider)


def test_bind_is_exact_loopback():
    assert provider._validate_bind_host("127.0.0.1") == "127.0.0.1"
    for value in ("0.0.0.0", "::", "localhost", "192.168.1.10"):
        try:
            provider._validate_bind_host(value)
        except ValueError:
            pass
        else:
            raise AssertionError(f"unsafe host accepted: {value}")


def test_acceptance_mode_rejects_tap_and_unknown_routes():
    assert provider._action_rejection("/api/tap", {"x": 1, "y": 2}, True)
    assert provider._action_rejection("/api/type", {"text": "secret"}, True)
    assert provider._action_rejection("/api/acceptance-route", {"target": "arbitrary"}, True)
    assert provider._action_rejection("/api/ensure-link", {"unexpected": True}, True)
    assert provider._action_rejection("/api/ensure-link", {}, True) is None
    assert provider._action_rejection("/api/release-link", {"unexpected": True}, True)
    assert provider._action_rejection("/api/release-link", {}, True) is None


def test_acceptance_mode_allows_only_fixed_apps_routes_and_vertical_scroll():
    assert provider._action_rejection("/api/recover-home", {}, True) is None
    assert provider._action_rejection("/api/recover-home", {"x": 1}, True)
    assert provider._action_rejection("/api/open-app", {"name": "Safari"}, True) is None
    assert provider._action_rejection("/api/open-app", {"name": "Settings"}, True)
    assert provider._action_rejection(
        "/api/acceptance-route", {"target": "codex_remote"}, True
    ) is None
    assert provider.SAFE_ROUTES["codex_remote"] == "https://remote.purrtechllc.com/"
    assert provider._action_rejection(
        "/api/swipe", {"x1": 200, "y1": 700, "x2": 210, "y2": 200}, True
    ) is None
    assert provider._action_rejection(
        "/api/swipe", {"x1": 20, "y1": 100, "x2": 500, "y2": 110}, True
    )


def safari_page(url, key="page-1"):
    return {
        "application": {
            "bundleId": provider.SAFARI_BUNDLE_ID,
            "active": True,
        },
        "page": {
            "key": key,
            "type": "WIRTypeWebPage",
            "url": url,
        },
    }


def webapp_page(url, bundle_id="com.apple.webapp", key="page-1"):
    return {
        "application": {
            "bundleId": bundle_id,
            "active": True,
        },
        "page": {
            "key": key,
            "type": "WIRTypeWebPage",
            "url": url,
        },
    }


def test_exact_active_webapp_page_requires_unique_origin_and_foreground_bundle():
    expected = "https://remote.purrtechllc.com/"
    exact = webapp_page("https://remote.purrtechllc.com/session")

    assert provider._exact_active_webapp_page(
        [exact], expected, "com.apple.webapp"
    ) == exact
    assert provider._exact_active_webapp_page(
        [exact], expected, "com.apple.mobilesafari"
    ) is None
    assert provider._exact_active_webapp_page(
        [exact, webapp_page(expected, key="page-2")],
        expected,
        "com.apple.webapp",
    ) is None


def test_launch_route_navigates_only_the_visible_safari_page_and_verifies(monkeypatch):
    calls = []
    current = safari_page("https://remote.purrtechllc.com/")
    observed = safari_page("https://agoramarket.purrtechllc.com/dashboard")
    page_lists = iter([[current], None, [observed]])

    class Process:
        def __init__(self, stdout, returncode=0):
            self.stdout = json.dumps(stdout)
            self.returncode = returncode

    def run(arguments, **_kwargs):
        calls.append(arguments)
        if arguments[1:3] == ["webinspector", "list"]:
            value = next(page_lists)
            return Process(value or [], returncode=1 if value is None else 0)
        return Process(True, returncode=1)

    class Device:
        @staticmethod
        def ios_path():
            return "ios.exe"

    class Client:
        visible_host = "remote.purrtechllc.com"

        @staticmethod
        def active_app():
            return {"bundleId": provider.SAFARI_BUNDLE_ID}

        @classmethod
        def find_first(cls, chain):
            return "address-bar" if cls.visible_host in chain else None

    class Helpers:
        @staticmethod
        def open_app(name, wait_seconds=0):
            calls.append(("open_app", name, wait_seconds))

    monkeypatch.setattr(provider.subprocess, "run", run)
    monkeypatch.setattr(provider.time, "sleep", lambda _seconds: None)
    context = provider.DriverContext(
        True,
        {
            "device": Device(),
            "capture": object(),
            "helpers": Helpers(),
            "WDAClient": lambda timeout: Client(),
            "WDAError": RuntimeError,
        },
    )

    original_run = provider.subprocess.run

    def run_and_update(arguments, **kwargs):
        process = original_run(arguments, **kwargs)
        if arguments[1:3] == ["webinspector", "eval"]:
            Client.visible_host = "agoramarket.purrtechllc.com"
        return process

    monkeypatch.setattr(provider.subprocess, "run", run_and_update)
    result = context.launch_route("safari_store")

    assert result == {
        "ok": True,
        "target": "safari_store",
        "observed_url": "https://agoramarket.purrtechllc.com/dashboard",
        "transport": "webinspector-fixed-page",
        "verification": "foreground_app_and_single_webinspector_page",
        "dispatch_status": "VERIFY_AFTER_DISCONNECT",
        "reconnect_expected": True,
    }
    eval_call = next(call for call in calls if isinstance(call, list) and "eval" in call)
    assert eval_call[3] == "page-1"
    assert (
        'setTimeout(() => window.location.assign("https://agoramarket.purrtechllc.com/"), 250)'
        in eval_call[4]
    )
    assert all("launch" not in call for call in calls if isinstance(call, list))
    assert not any(isinstance(call, tuple) and call[0] == "open_app" for call in calls)


def test_launch_route_returns_already_open_without_dispatch(monkeypatch):
    calls = []
    current = safari_page("https://agoramarket.purrtechllc.com/")

    class Process:
        returncode = 0

        def __init__(self, stdout):
            self.stdout = json.dumps(stdout)

    def run(arguments, **_kwargs):
        calls.append(arguments)
        return Process([current])

    class Device:
        @staticmethod
        def ios_path():
            return "ios.exe"

    class Client:
        @staticmethod
        def active_app():
            return {"bundleId": provider.SAFARI_BUNDLE_ID}

        @staticmethod
        def find_first(_chain):
            # Mobile Safari can expose only a clipped URL in native chrome.
            # The sole active Web Inspector page remains unambiguous.
            return None

    class Helpers:
        @staticmethod
        def open_app(name, wait_seconds=0):
            calls.append(("open_app", name, wait_seconds))

    monkeypatch.setattr(provider.subprocess, "run", run)
    context = provider.DriverContext(
        True,
        {
            "device": Device(),
            "capture": object(),
            "helpers": Helpers(),
            "WDAClient": lambda timeout: Client(),
            "WDAError": RuntimeError,
        },
    )

    result = context.launch_route("safari_store")

    assert result["dispatch_status"] == "ALREADY_OPEN"
    assert result["reconnect_expected"] is False
    assert all("eval" not in call for call in calls)
    assert not any(isinstance(call, tuple) and call[0] == "open_app" for call in calls)


def test_launch_route_fails_closed_when_target_is_not_visible(monkeypatch):
    current = safari_page("https://remote.purrtechllc.com/")

    class Process:
        returncode = 0

        def __init__(self, stdout):
            self.stdout = json.dumps(stdout)

    def run(arguments, **_kwargs):
        if arguments[1:3] == ["webinspector", "list"]:
            return Process([current])
        return Process(True)

    class Device:
        @staticmethod
        def ios_path():
            return "ios.exe"

    class Client:
        @staticmethod
        def active_app():
            return {"bundleId": provider.SAFARI_BUNDLE_ID}

        @staticmethod
        def find_first(chain):
            return "address-bar" if "agoramarket.purrtechllc.com" in chain else None

    class Helpers:
        @staticmethod
        def open_app(_name, wait_seconds=0):
            return None

    monkeypatch.setattr(provider.subprocess, "run", run)
    monkeypatch.setattr(provider.time, "sleep", lambda _seconds: None)
    context = provider.DriverContext(
        True,
        {
            "device": Device(),
            "capture": object(),
            "helpers": Helpers(),
            "WDAClient": lambda timeout: Client(),
            "WDAError": RuntimeError,
        },
    )

    try:
        context.launch_route("safari_store")
    except RuntimeError as error:
        assert str(error) == "MISSING_PROOF: fixed Safari navigation was not visible"
    else:
        raise AssertionError("unobserved fixed route must fail closed")


def test_codex_remote_opens_exact_home_icon_and_verifies_active_page(monkeypatch):
    calls = []
    active_apps = iter(
        [
            {"bundleId": provider.SPRINGBOARD_BUNDLE_ID},
            {"bundleId": "com.apple.webapp"},
        ]
    )

    class Process:
        returncode = 0
        stdout = json.dumps(
            [webapp_page("https://remote.purrtechllc.com/", "com.apple.webapp")]
        )

    class Device:
        @staticmethod
        def ios_path():
            return "ios.exe"

    class Client:
        @staticmethod
        def active_app():
            return next(active_apps)

        @staticmethod
        def find_first(chain):
            calls.append(("find_first", chain))
            return "remote-desktop-icon"

        @staticmethod
        def element_click(element_id):
            calls.append(("element_click", element_id))

    class Helpers:
        @staticmethod
        def press_home():
            calls.append(("press_home",))

    monkeypatch.setattr(provider.subprocess, "run", lambda *_a, **_k: Process())
    monkeypatch.setattr(provider.time, "sleep", lambda _seconds: None)
    context = provider.DriverContext(
        True,
        {
            "device": Device(),
            "capture": object(),
            "helpers": Helpers(),
            "WDAClient": lambda timeout: Client(),
            "WDAError": RuntimeError,
        },
    )

    result = context.launch_route("codex_remote")

    assert result == {
        "ok": True,
        "target": "codex_remote",
        "observed_url": "https://remote.purrtechllc.com/",
        "transport": "home-screen-webapp",
        "verification": "exact_home_icon_and_active_webinspector_page",
        "dispatch_status": "HOME_ICON_OPENED",
        "reconnect_expected": True,
    }
    assert calls == [
        ("press_home",),
        (
            "find_first",
            '**/XCUIElementTypeIcon[`name == "Remote Desktop"`]',
        ),
        ("element_click", "remote-desktop-icon"),
    ]


def test_codex_remote_requires_visible_exact_home_icon(monkeypatch):
    class Device:
        @staticmethod
        def ios_path():
            return "ios.exe"

    class Client:
        @staticmethod
        def active_app():
            return {"bundleId": provider.SPRINGBOARD_BUNDLE_ID}

        @staticmethod
        def find_first(_chain):
            return None

    class Helpers:
        @staticmethod
        def press_home():
            return None

    context = provider.DriverContext(
        True,
        {
            "device": Device(),
            "capture": object(),
            "helpers": Helpers(),
            "WDAClient": lambda timeout: Client(),
            "WDAError": RuntimeError,
        },
    )

    try:
        context.launch_route("codex_remote")
    except RuntimeError as error:
        assert str(error) == (
            "NEEDS_HUMAN_ADD_TO_HOME_SCREEN: Remote Desktop icon is not visible"
        )
    else:
        raise AssertionError("missing fixed Home Screen icon must fail closed")


def test_supervised_mode_adds_tap_but_never_typing():
    assert provider._action_rejection("/api/tap", {"x": 1, "y": 2}, False) is None
    assert provider._action_rejection("/api/type", {"text": "secret"}, False)


def test_agoramarket_webview_selector_requires_one_exact_telegram_production_page():
    exact = {
        "application": {
            "bundleId": "com.apple.WebKit.WebContent",
            "host": "ph.telegra.Telegraph",
            "active": True,
        },
        "page": {
            "key": "page-1",
            "type": "WIRTypeWebPage",
            "url": "https://agoramarket.purrtechllc.com/?tgWebAppVersion=9.1",
        },
    }
    assert provider._exact_agoramarket_telegram_page([exact]) == exact
    assert provider._exact_agoramarket_telegram_page([exact, exact]) is None
    assert provider._exact_agoramarket_telegram_page(
        [{**exact, "page": {**exact["page"], "url": "https://example.com/"}}]
    ) is None
    assert provider._exact_agoramarket_telegram_page(
        [{**exact, "application": {**exact["application"], "host": "com.apple.mobilesafari"}}]
    ) is None


def test_agoramarket_runtime_version_requires_live_bootstrap_gate_and_flutter_root():
    candidate = {
        "application": {
            "bundleId": "com.apple.WebKit.WebContent",
            "host": "ph.telegra.Telegraph",
            "active": True,
        },
        "page": {
            "key": "page-1",
            "type": "WIRTypeWebPage",
            "url": "https://agoramarket.purrtechllc.com/",
        },
    }
    runtime = {
        "href": "https://agoramarket.purrtechllc.com/",
        "title": "AgoraMarket",
        "readyState": "complete",
        "metaVersion": "1.0.4261+4261",
        "bootstrapVersion": "1.0.4261+4261",
        # A completed Flutter bootstrap may no longer remain in DOM or the
        # bounded PerformanceResourceTiming buffer. The running Flutter root
        # plus the exact bootstrap gate are the durable live-session proof.
        "bootstrapScriptPresent": False,
        "bootstrapResourceLoaded": False,
        "flutterRootPresent": True,
        "viewport": {"innerHeight": 844, "scrollHeight": 844},
    }
    evidence = provider._agoramarket_runtime_evidence(candidate, runtime)
    assert evidence["physical_session_binding"] == "PASS"
    assert evidence["client_version"]["status"] == "PASS"
    assert evidence["safety"]["arbitrary_javascript_supported"] is False
    stale = provider._agoramarket_runtime_evidence(
        candidate, {**runtime, "bootstrapVersion": "1.0.4260+4260"}
    )
    assert stale["client_version"]["status"] == "MISSING_PROOF"


def test_fixed_webview_expression_is_read_only_and_secret_bounded():
    expression = provider.AGORAMARKET_WEBVIEW_EXPRESSION
    assert "localStorage.getItem('agora_web_bootstrap_version')" in expression
    for forbidden in (
        "localStorage.clear",
        "localStorage.removeItem",
        "sessionStorage",
        "document.cookie",
        "location.reload",
        "fetch(",
        "XMLHttpRequest",
    ):
        assert forbidden not in expression


def telegram_row(text, x, y, kind="StaticText", width=100, height=30):
    return {
        "text": text,
        "x": x,
        "y": y,
        "type": kind,
        "rect": {
            "x": x - width / 2,
            "y": y - height / 2,
            "width": width,
            "height": height,
        },
    }


def test_telegram_chat_list_open_must_be_unique_and_share_the_bot_row():
    bot = telegram_row("AgoraLoginBot", 150, 200)
    safe_open = telegram_row("OPEN", 350, 225, "Button")
    unrelated_open = telegram_row("OPEN", 350, 500, "Button")

    assert provider._telegram_chat_list_open([bot, safe_open]) == safe_open
    assert provider._telegram_chat_list_open([bot, unrelated_open]) is None
    assert provider._telegram_chat_list_open([bot, safe_open, unrelated_open]) is None


def test_telegram_diagnostics_are_bounded_redacted_and_region_scoped():
    device_id = "00008101-0012719002C2001E"
    rows = [telegram_row("outside private message", 100, 300)]
    rows += [
        telegram_row(f"candidate {index} {device_id} " + "x" * 160, 120, 480 + index)
        for index in range(20)
    ]

    diagnostics = provider._telegram_diagnostic_rows(rows)

    assert len(diagnostics) == provider.TELEGRAM_DIAGNOSTIC_LIMIT
    assert all(len(row["text"]) <= provider.TELEGRAM_DIAGNOSTIC_TEXT_LIMIT for row in diagnostics)
    assert all("outside private message" not in row["text"] for row in diagnostics)
    assert all(device_id not in row["text"] for row in diagnostics)
    assert all("[REDACTED-DEVICE-ID]" in row["text"] for row in diagnostics)
    assert all(set(row["rect"]) == {"x", "y", "width", "height"} for row in diagnostics)


def test_telegram_diagnostics_include_branded_hint_outside_expected_band():
    branded = telegram_row("Web App - 進入 AgoraMarket 商城", 195, 700, "Link", width=340)
    diagnostics = provider._telegram_diagnostic_rows([branded])

    assert diagnostics == [
        {
            "text": "Web App - 進入 AgoraMarket 商城",
            "type": "Link",
            "x": 195.0,
            "y": 700.0,
            "rect": {"x": 25.0, "y": 685.0, "width": 340.0, "height": 30.0},
        }
    ]


def test_telegram_tree_diagnostics_include_unlabelled_small_nodes_not_containers():
    tree = {
        "type": "Application",
        "rect": {"x": 0, "y": 0, "width": 390, "height": 844},
        "children": [
            {
                "type": "Other",
                "label": "message container",
                "rect": {"x": 3, "y": 354, "width": 332, "height": 277},
            },
            {
                "type": "Other",
                "rect": {"x": 10, "y": 500, "width": 320, "height": 44},
            },
            {
                "type": "Button",
                "rect": {"x": 10, "y": 550, "width": 320, "height": 44},
            },
            {
                "type": "Button",
                "label": "hidden",
                "isVisible": "0",
                "rect": {"x": 10, "y": 600, "width": 320, "height": 44},
            },
            {
                "type": "Link",
                "label": "進入 AgoraMarket 商城",
                "rect": {"x": 10, "y": 700, "width": 320, "height": 44},
            },
        ],
    }

    diagnostics = provider._telegram_tree_diagnostic_rows(tree)

    assert [row["type"] for row in diagnostics] == ["Link", "Button", "Other"]
    assert diagnostics[1]["text"] == ""
    assert diagnostics[2]["text"] == ""
    assert all(row["text"] != "message container" for row in diagnostics)
    assert all(row["text"] != "hidden" for row in diagnostics)


def telegram_visual_matches():
    return {
        "storefront": [
            {"centerX": 553.5, "centerY": 1958, "width": 497, "height": 48}
        ],
        "contact": [
            {"centerX": 554, "centerY": 2089, "width": 330, "height": 44}
        ],
        "game": [
            {"centerX": 555.5, "centerY": 2220, "width": 229, "height": 44}
        ],
    }


def test_telegram_bot_chat_identity_requires_header_and_second_bot_container():
    header = telegram_row("AgoraLoginBot", 195, 79, "Other", width=178, height=60)
    container = telegram_row(
        "AgoraLoginBot", 169, 492.5, "Other", width=332, height=277
    )
    list_row = telegram_row("AgoraLoginBot", 150, 200)

    assert provider._telegram_bot_chat_visible([header, container])
    assert not provider._telegram_bot_chat_visible([header])
    assert not provider._telegram_bot_chat_visible([list_row])


def test_telegram_visual_selector_requires_unique_aligned_three_row_geometry():
    candidate = provider._telegram_visual_entry_candidate(
        telegram_visual_matches(), 1170, 2532, 390, 844
    )

    assert candidate == {
        "pixel_center": {"x": 553.5, "y": 1958.0},
        "logical_point": {"x": 184.5, "y": 652.67},
        "row_gaps_pixels": [131.0, 131.0],
    }

    duplicated = telegram_visual_matches()
    duplicated["storefront"].append(dict(duplicated["storefront"][0]))
    assert provider._telegram_visual_entry_candidate(
        duplicated, 1170, 2532, 390, 844
    ) is None

    misaligned = telegram_visual_matches()
    misaligned["contact"][0]["centerX"] = 900
    assert provider._telegram_visual_entry_candidate(
        misaligned, 1170, 2532, 390, 844
    ) is None


def test_telegram_visual_selector_requires_stable_fresh_frames():
    first = provider._telegram_visual_entry_candidate(
        telegram_visual_matches(), 1170, 2532, 390, 844
    )
    assert first is not None
    second = {
        **first,
        "pixel_center": {"x": first["pixel_center"]["x"], "y": 1962.0},
    }
    moved = {
        **first,
        "pixel_center": {"x": first["pixel_center"]["x"], "y": 2050.0},
    }
    assert provider._telegram_visual_candidates_stable(first, second, 1170, 2532)
    assert not provider._telegram_visual_candidates_stable(first, moved, 1170, 2532)


def test_telegram_visual_route_taps_only_after_chat_and_fresh_selector_revalidation():
    calls = []
    chat = [
        telegram_row("AgoraLoginBot", 195, 79, "Other", width=178, height=60),
        telegram_row("AgoraLoginBot", 169, 492.5, "Other", width=332, height=277),
        telegram_row("Message", 195, 805, "TextField", width=220, height=40),
    ]
    screens = iter(
        [
            chat,
            list(chat),
            [
                telegram_row("商品", 50, 760, "Button"),
                telegram_row("訂單", 150, 760, "Button"),
                telegram_row("錢包", 250, 760, "Button"),
                telegram_row("我的", 350, 760, "Button"),
            ],
        ]
    )

    class Helpers:
        @staticmethod
        def open_app(name, wait_seconds=0):
            calls.append(("open_app", name, wait_seconds))

        @staticmethod
        def ocr():
            return next(screens)

        @staticmethod
        def tap(x, y):
            calls.append(("tap", x, y))

    context = provider.DriverContext(
        True,
        {
            "device": object(),
            "capture": object(),
            "helpers": Helpers(),
            "WDAClient": lambda timeout: object(),
            "WDAError": RuntimeError,
        },
    )
    context._observe_telegram_visual_entry = lambda: {
        "ok": True,
        "candidate": {"logical_point": {"x": 184.5, "y": 652.67}},
        "evidence": {"selector": "fresh_windows_ocr_branded_triple"},
    }
    original_sleep = provider.time.sleep
    provider.time.sleep = lambda _seconds: None
    try:
        result = context._open_telegram_bot()
    finally:
        provider.time.sleep = original_sleep

    assert result["state"] == "STOREFRONT_ALREADY_OPEN"
    assert result["selector_evidence"] == {
        "selector": "fresh_windows_ocr_branded_triple"
    }
    assert calls == [
        ("open_app", "Telegram", 5),
        ("tap", 184.5, 652.67),
    ]


def test_telegram_route_taps_only_the_allowlisted_bot_open():
    calls = []
    screens = iter(
        [
            [
                telegram_row("Chats", 195, 60),
                telegram_row("AgoraLoginBot", 150, 200),
                telegram_row("OPEN", 350, 225, "Button"),
            ],
            [
                telegram_row("商品", 50, 760, "Button"),
                telegram_row("訂單", 150, 760, "Button"),
                telegram_row("錢包", 250, 760, "Button"),
                telegram_row("我的", 350, 760, "Button"),
            ],
        ]
    )

    class Helpers:
        @staticmethod
        def open_app(name, wait_seconds=0):
            calls.append(("open_app", name, wait_seconds))

        @staticmethod
        def ocr():
            return next(screens)

        @staticmethod
        def tap(x, y):
            calls.append(("tap", x, y))

    context = provider.DriverContext(
        True,
        {
            "device": object(),
            "capture": object(),
            "helpers": Helpers(),
            "WDAClient": lambda timeout: object(),
            "WDAError": RuntimeError,
        },
    )
    original_sleep = provider.time.sleep
    provider.time.sleep = lambda _seconds: None
    try:
        result = context._open_telegram_bot()
    finally:
        provider.time.sleep = original_sleep

    assert result["target"] == "telegram_bot"
    assert result["state"] == "STOREFRONT_ALREADY_OPEN"
    assert result["storefront_entry_opened"] is True
    assert calls == [("open_app", "Telegram", 5), ("tap", 350, 225)]


def test_telegram_route_accepts_the_observed_branded_storefront_button():
    calls = []
    screens = iter(
        [
            [
                telegram_row("AgoraLoginBot", 195, 60),
                telegram_row(
                    "Web App · 🛍️  進入 AgoraMarket 商城 · Open",
                    195,
                    650,
                    "Link",
                    width=340,
                ),
            ],
            [
                telegram_row("商品", 50, 760, "Button"),
                telegram_row("訂單", 150, 760, "Button"),
                telegram_row("錢包", 250, 760, "Button"),
                telegram_row("我的", 350, 760, "Button"),
            ],
        ]
    )

    class Helpers:
        @staticmethod
        def open_app(_name, wait_seconds=0):
            return None

        @staticmethod
        def ocr():
            return next(screens)

        @staticmethod
        def tap(x, y):
            calls.append((x, y))

    context = provider.DriverContext(
        True,
        {
            "device": object(),
            "capture": object(),
            "helpers": Helpers(),
            "WDAClient": lambda timeout: object(),
            "WDAError": RuntimeError,
        },
    )
    original_sleep = provider.time.sleep
    provider.time.sleep = lambda _seconds: None
    try:
        result = context._open_telegram_bot()
    finally:
        provider.time.sleep = original_sleep

    assert result["state"] == "STOREFRONT_ALREADY_OPEN"
    assert calls == [(195, 650)]


def test_telegram_route_never_taps_two_storefront_controls_in_one_action():
    calls = []
    screens = iter(
        [
            [
                telegram_row("AgoraLoginBot", 150, 200),
                telegram_row("OPEN", 350, 225, "Button"),
            ],
            [
                telegram_row("AgoraLoginBot", 195, 60),
                telegram_row("🛍 進入 AgoraMarket 商城", 195, 650, "Button", width=340),
            ],
        ]
    )

    class Helpers:
        @staticmethod
        def open_app(_name, wait_seconds=0):
            return None

        @staticmethod
        def ocr():
            return next(screens)

        @staticmethod
        def tap(x, y):
            calls.append((x, y))

    context = provider.DriverContext(
        True,
        {
            "device": object(),
            "capture": object(),
            "helpers": Helpers(),
            "WDAClient": lambda timeout: object(),
            "WDAError": RuntimeError,
        },
    )
    original_sleep = provider.time.sleep
    provider.time.sleep = lambda _seconds: None
    try:
        result = context._open_telegram_bot()
    finally:
        provider.time.sleep = original_sleep

    assert result["state"] == "MISSING_PROOF_TELEGRAM_OPEN"
    assert calls == [(350, 225)]


def test_telegram_route_never_taps_start():
    class Helpers:
        taps = []

        @staticmethod
        def open_app(_name, wait_seconds=0):
            return None

        @staticmethod
        def ocr():
            return [
                telegram_row("AgoraLoginBot", 150, 200),
                telegram_row("Start", 300, 700, "Button"),
            ]

        @classmethod
        def tap(cls, x, y):
            cls.taps.append((x, y))

    context = provider.DriverContext(
        True,
        {
            "device": object(),
            "capture": object(),
            "helpers": Helpers(),
            "WDAClient": lambda timeout: object(),
            "WDAError": RuntimeError,
        },
    )
    original_sleep = provider.time.sleep
    provider.time.sleep = lambda _seconds: None
    try:
        result = context._open_telegram_bot()
    finally:
        provider.time.sleep = original_sleep

    assert result["state"] == "NEEDS_HUMAN_BOT_START"
    assert Helpers.taps == []


def test_telegram_route_stops_after_open_when_confirmation_appears():
    calls = []
    screens = iter(
        [
            [
                telegram_row("AgoraLoginBot", 150, 200),
                telegram_row("OPEN", 350, 225, "Button"),
            ],
            [
                telegram_row("Cancel", 120, 600, "Button"),
                telegram_row("Open", 280, 600, "Button"),
            ],
        ]
    )

    class Helpers:
        @staticmethod
        def open_app(_name, wait_seconds=0):
            return None

        @staticmethod
        def ocr():
            return next(screens)

        @staticmethod
        def tap(x, y):
            calls.append((x, y))

    context = provider.DriverContext(
        True,
        {
            "device": object(),
            "capture": object(),
            "helpers": Helpers(),
            "WDAClient": lambda timeout: object(),
            "WDAError": RuntimeError,
        },
    )
    original_sleep = provider.time.sleep
    provider.time.sleep = lambda _seconds: None
    try:
        result = context._open_telegram_bot()
    finally:
        provider.time.sleep = original_sleep

    assert result["state"] == "NEEDS_HUMAN_TELEGRAM_LAUNCH_CONFIRMATION"
    assert calls == [(350, 225)]


def test_device_ids_are_redacted():
    assert provider._redact("00008101-0012719002C2001E") == "[REDACTED-DEVICE-ID]"


def test_provider_instance_id_is_stable_for_one_process():
    class Device:
        @staticmethod
        def list_devices():
            return ["device"]

    class Client:
        @staticmethod
        def link_state():
            return "down"

    context = provider.DriverContext(
        True,
        {
            "device": Device(),
            "capture": object(),
            "helpers": object(),
            "WDAClient": lambda timeout: Client(),
            "WDAError": RuntimeError,
        },
    )
    first = context.status()
    second = context.status()
    assert first["provider"] == "sirin-ios-driver"
    assert first["instance_id"] == second["instance_id"]
    assert first["instance_id"].startswith(f"{provider.os.getpid()}-")
    assert first["last_link_result"] == "PASSIVE_LINK_NOT_STARTED"


def test_passive_status_does_not_probe_wda_when_usb_is_absent():
    calls = []

    class Device:
        @staticmethod
        def list_devices():
            calls.append("usb")
            return []

        @staticmethod
        def forward_listener_scope():
            return "inactive"

        @staticmethod
        def lan_block_rule_active():
            return False

    class Client:
        @staticmethod
        def link_state():
            calls.append("wda")
            return "down"

    context = provider.DriverContext(
        True,
        {
            "device": Device(),
            "capture": object(),
            "helpers": object(),
            "WDAClient": lambda timeout: Client(),
            "WDAError": RuntimeError,
        },
    )
    status = context.status()
    assert calls == ["usb"]
    assert status["attach_mode"] == "passive"
    assert status["last_link_result"] == "NEEDS_HUMAN_USB"
    assert status["lan_exposed"] is False
    assert status["lan_protection"] == "forwards_inactive_activation_blocked"


def test_device_probe_failure_is_not_misreported_as_missing_usb():
    class Device:
        @staticmethod
        def list_devices():
            raise RuntimeError("maintenance failure")

        @staticmethod
        def forward_listener_scope():
            return "inactive"

        @staticmethod
        def lan_block_rule_active():
            return True

    context = provider.DriverContext(
        True,
        {
            "device": Device(),
            "capture": object(),
            "helpers": object(),
            "WDAClient": lambda timeout: object(),
            "WDAError": RuntimeError,
        },
    )
    status = context.status()
    phone_status, phone = context.phone(status)
    assert status["device_probe_status"] == "failed"
    assert status["last_link_result"] == "DRIVER_DEVICE_PROBE_FAILED"
    assert phone_status == 503
    assert phone["error"] == "DRIVER_DEVICE_PROBE_FAILED"


def test_snapshot_reuses_one_status_probe_for_phone_information():
    calls = []

    class Device:
        @staticmethod
        def list_devices():
            calls.append("usb")
            return ["device"]

        @staticmethod
        def forward_listener_scope():
            return "loopback"

        @staticmethod
        def lan_block_rule_active():
            return False

    class Client:
        @staticmethod
        def link_state():
            calls.append("link")
            return "up"

        @staticmethod
        def active_app():
            return {"bundleId": "com.example.app"}

        @staticmethod
        def battery():
            return {"level": 1.0}

        @staticmethod
        def is_locked():
            return False

        @staticmethod
        def ensure_session():
            return "session"

    context = provider.DriverContext(
        True,
        {
            "device": Device(),
            "capture": object(),
            "helpers": object(),
            "WDAClient": lambda timeout: Client(),
            "WDAError": RuntimeError,
        },
    )
    snapshot = context.snapshot()
    assert calls == ["usb", "link"]
    assert snapshot["phone_status"] == 200
    assert snapshot["phone"]["info_readable"] is True


def test_lan_listener_is_reported_from_observed_listener_and_block_state():
    class Device:
        blocked = False

        @staticmethod
        def list_devices():
            return []

        @staticmethod
        def forward_listener_scope():
            return "lan"

        @classmethod
        def lan_block_rule_active(cls):
            return cls.blocked

    class Client:
        @staticmethod
        def link_state():
            return "down"

    context = provider.DriverContext(
        True,
        {
            "device": Device(),
            "capture": object(),
            "helpers": object(),
            "WDAClient": lambda timeout: Client(),
            "WDAError": RuntimeError,
        },
    )
    unsafe = context.status()
    assert unsafe["lan_exposed"] is True
    assert unsafe["lan_protection"] == "unblocked_lan_listener"

    Device.blocked = True
    context._lan_cache_at = 0.0
    blocked = context.status()
    assert blocked["lan_exposed"] is False
    assert blocked["lan_protection"] == "firewall_blocked_lan_listener"


def test_phone_reports_bounded_last_link_result_when_wda_is_down():
    class Device:
        @staticmethod
        def list_devices():
            return ["device"]

    class Client:
        @staticmethod
        def link_state():
            return "down"

    context = provider.DriverContext(
        True,
        {
            "device": Device(),
            "capture": object(),
            "helpers": object(),
            "WDAClient": lambda timeout: Client(),
            "WDAError": RuntimeError,
        },
    )
    context._finish_link_result("WDA_START_TIMEOUT")
    status, phone = context.phone()
    assert status == 503
    assert phone == {"info_readable": False, "error": "WDA_START_TIMEOUT"}


def test_missing_wda_is_an_explicit_human_install_boundary():
    class Device:
        @staticmethod
        def ios_path():
            return "ios.exe"

        @staticmethod
        def list_devices():
            return ["device"]

        @staticmethod
        def lockdown_ready():
            return True

        @staticmethod
        def lan_block_rule_active():
            return True

        @staticmethod
        def tunnel_running():
            return True

        @staticmethod
        def ddi_mounted():
            return True

        @staticmethod
        def detect_wda_bundle():
            return None

    class Client:
        @staticmethod
        def link_state():
            return "down"

    context = provider.DriverContext(
        True,
        {
            "device": Device(),
            "capture": object(),
            "helpers": object(),
            "WDAClient": lambda timeout: Client(),
            "WDAError": RuntimeError,
        },
    )
    assert context.ensure_link(wait_seconds=0.0) == "NEEDS_HUMAN_WDA_INSTALL"
    assert context.status()["last_link_result"] == "NEEDS_HUMAN_WDA_INSTALL"


def test_link_activation_refuses_before_phone_or_network_changes_without_lan_block():
    calls = []

    class Device:
        @staticmethod
        def ios_path():
            return "ios.exe"

        @staticmethod
        def list_devices():
            return ["device"]

        @staticmethod
        def lockdown_ready():
            return True

        @staticmethod
        def lan_block_rule_active():
            return False

        @staticmethod
        def tunnel_running():
            calls.append("tunnel")
            return False

        @staticmethod
        def ddi_mounted():
            calls.append("ddi")
            return False

    class Client:
        @staticmethod
        def link_state():
            return "down"

    context = provider.DriverContext(
        True,
        {
            "device": Device(),
            "capture": object(),
            "helpers": object(),
            "WDAClient": lambda timeout: Client(),
            "WDAError": RuntimeError,
        },
    )
    assert context.ensure_link(wait_seconds=0.0) == (
        "SECURITY_BLOCK_LAN_PROTECTION_MISSING"
    )
    assert calls == []


def test_link_activation_never_auto_mounts_developer_image():
    calls = []

    class Device:
        @staticmethod
        def ios_path():
            return "ios.exe"

        @staticmethod
        def list_devices():
            return ["device"]

        @staticmethod
        def lockdown_ready():
            return True

        @staticmethod
        def lan_block_rule_active():
            return True

        @staticmethod
        def tunnel_running():
            return True

        @staticmethod
        def ddi_mounted():
            return False

        @staticmethod
        def mount_ddi():
            calls.append("mount")
            return True, "mounted"

    class Client:
        @staticmethod
        def link_state():
            return "down"

    context = provider.DriverContext(
        True,
        {
            "device": Device(),
            "capture": object(),
            "helpers": object(),
            "WDAClient": lambda timeout: Client(),
            "WDAError": RuntimeError,
        },
    )
    assert context.ensure_link(wait_seconds=0.0) == "NEEDS_HUMAN_DDI"
    assert calls == []


def test_release_link_stops_only_transient_sirin_phone_processes():
    calls = []

    class Device:
        @staticmethod
        def stop_all_verified(names):
            calls.append(names)
            return {
                "stopped": ["forward8100", "forward9100", "runwda", "tunnel"],
                "failed": [],
                "stale_pid_files_removed": [],
            }

        @staticmethod
        def forward_listener_scope():
            return "inactive"

    class Client:
        session_id = "session"

    context = provider.DriverContext(
        True,
        {
            "device": Device(),
            "capture": object(),
            "helpers": object(),
            "WDAClient": lambda timeout: Client(),
            "WDAError": RuntimeError,
        },
    )
    result = context.release_link()
    assert result["ok"] is True
    assert result["persistent_changes"] == []
    assert context.client.session_id is None
    assert context.last_link_result == "PASSIVE_LINK_NOT_STARTED"
    assert calls == [
        ("forward8100", "forward9100", "runwda", "tunnel", "syslog")
    ]


def test_release_link_fails_closed_when_process_or_listener_remains():
    class Device:
        @staticmethod
        def stop_all_verified(names):
            return {
                "stopped": [],
                "failed": [{"name": "forward8100", "reason": "termination_not_proven"}],
                "stale_pid_files_removed": [],
            }

        @staticmethod
        def forward_listener_scope():
            return "lan"

    class Client:
        session_id = "session"

    context = provider.DriverContext(
        True,
        {
            "device": Device(),
            "capture": object(),
            "helpers": object(),
            "WDAClient": lambda timeout: Client(),
            "WDAError": RuntimeError,
        },
    )
    result = context.release_link()
    assert result["ok"] is False
    assert result["status"] == "release_incomplete"
    assert result["forward_listener_scope"] == "lan"
    assert context.last_link_result == "RELEASE_INCOMPLETE"


def test_home_uses_wda_while_link_is_healthy():
    calls = []

    class Device:
        @staticmethod
        def foreground_springboard():
            calls.append("usb-home")
            return True

    class Client:
        @staticmethod
        def link_state():
            return "up"

    class Helpers:
        @staticmethod
        def press_home():
            calls.append("wda-home")

    context = provider.DriverContext(
        True,
        {
            "device": Device(),
            "capture": object(),
            "helpers": Helpers(),
            "WDAClient": lambda timeout: Client(),
            "WDAError": RuntimeError,
        },
    )
    assert context.home() == {"ok": True, "transport": "wda"}
    assert calls == ["wda-home"]


def test_home_uses_explicit_usb_recovery_only_when_wda_is_down():
    calls = []

    class Device:
        @staticmethod
        def foreground_springboard():
            calls.append("usb-home")
            return True

    class Client:
        @staticmethod
        def link_state():
            return "down"

    class Helpers:
        @staticmethod
        def press_home():
            calls.append("wda-home")

    context = provider.DriverContext(
        True,
        {
            "device": Device(),
            "capture": object(),
            "helpers": Helpers(),
            "WDAClient": lambda timeout: Client(),
            "WDAError": RuntimeError,
        },
    )
    context._request_link_recovery = lambda: calls.append("restart-wda")
    assert context.home() == {
        "ok": True,
        "transport": "go-ios-springboard-recovery",
        "prior_link": "down",
        "wda_recovery_requested": True,
    }
    assert calls == ["usb-home", "restart-wda"]


def test_recover_home_rejects_healthy_wda_without_phone_action():
    calls = []

    class Device:
        @staticmethod
        def foreground_springboard():
            calls.append("usb-home")
            return True

    class Client:
        @staticmethod
        def link_state():
            return "up"

    context = provider.DriverContext(
        True,
        {
            "device": Device(),
            "capture": object(),
            "helpers": object(),
            "WDAClient": lambda timeout: Client(),
            "WDAError": RuntimeError,
        },
    )
    try:
        context.recover_home()
    except RuntimeError as error:
        assert "RECOVERY_NOT_NEEDED" in str(error)
    else:
        raise AssertionError("healthy WDA recovery must fail closed")
    assert calls == []


def test_recovery_grace_reuses_existing_wda_before_starting_a_new_runner():
    calls = []

    class Device:
        @staticmethod
        def ios_path():
            return "ios.exe"

        @staticmethod
        def list_devices():
            return ["device"]

        @staticmethod
        def lockdown_ready():
            return True

        @staticmethod
        def lan_block_rule_active():
            return True

        @staticmethod
        def tunnel_running():
            return True

        @staticmethod
        def ddi_mounted():
            return True

        @staticmethod
        def detect_wda_bundle():
            return "com.example.WebDriverAgentRunner.xctrunner"

        @staticmethod
        def start_forwards():
            calls.append("start-forwards")

        @staticmethod
        def start_wda(_bundle):
            calls.append("start-wda")

    class Client:
        checks = 0

        @staticmethod
        def link_state():
            return "down"

        @classmethod
        def is_up(cls):
            cls.checks += 1
            return cls.checks >= 2

    context = provider.DriverContext(
        True,
        {
            "device": Device(),
            "capture": object(),
            "helpers": object(),
            "WDAClient": lambda timeout: Client(),
            "WDAError": RuntimeError,
        },
    )
    assert (
        context.ensure_link(wait_seconds=1.0, recovery_grace_seconds=1.0)
        == "RECOVERED_EXISTING_WDA"
    )
    assert calls == ["start-forwards"]
    assert context.status()["last_link_result"] == "RECOVERED_EXISTING_WDA"
