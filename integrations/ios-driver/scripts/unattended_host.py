"""Sirin-owned, loopback-only iPhone driver for physical-device acceptance."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import logging
import logging.handlers
import math
import os
import re
import struct
import subprocess
import sys
import tempfile
import threading
import time
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import urlparse


ROOT = Path(__file__).resolve().parents[1]
DEVICE_ID = re.compile(r"\b[0-9A-Fa-f]{8}-[0-9A-Fa-f]{16}\b")
SAFE_APPS = {
    "safari": "Safari",
    "com.apple.mobilesafari": "Safari",
    "telegram": "Telegram",
    "ph.telegra.telegraph": "Telegram",
}
SAFE_ROUTES = {
    "safari_store": "https://agoramarket.purrtechllc.com/",
    "telegram_bot": "https://t.me/agora_login_bot",
    "codex_remote": "https://remote.purrtechllc.com/",
}
SAFARI_BUNDLE_ID = "com.apple.mobilesafari"
SPRINGBOARD_BUNDLE_ID = "com.apple.springboard"
CODEX_REMOTE_HOME_LABEL = "Remote Desktop"
SAFARI_PAGE_TYPES = {"WIRTypeWeb", "WIRTypeWebPage", "WIRTypeJavaScript"}
AGORAMARKET_HOST = "agoramarket.purrtechllc.com"
AGORAMARKET_REQUIRED_VERSION = "1.0.4261+4261"
TELEGRAM_BUNDLE_ID = "ph.telegra.Telegraph"
AGORAMARKET_WEBVIEW_EXPRESSION = r"""(() => {
  const safe = (fn) => { try { return fn(); } catch (_) { return null; } };
  const scrolling = document.scrollingElement || document.documentElement;
  const resources = safe(() => performance.getEntriesByType('resource').map((entry) => String(entry.name))) || [];
  const visibleText = safe(() => String(document.body ? document.body.innerText : '')) || '';
  const versions = visibleText.match(/\b\d+\.\d+\.\d+\+\d+\b/g) || [];
  const hit = safe(() => document.elementFromPoint(window.innerWidth / 2, window.innerHeight * 0.72));
  const hitPath = [];
  for (let node = hit, depth = 0; node && depth < 8; node = node.parentElement, depth += 1) {
    hitPath.push({
      tag: String(node.tagName || '').toLowerCase(),
      id: String(node.id || '').slice(0, 80),
      className: String(node.className || '').slice(0, 120),
      clientHeight: Number(node.clientHeight || 0),
      scrollHeight: Number(node.scrollHeight || 0),
      overflowY: safe(() => String(getComputedStyle(node).overflowY))
    });
  }
  return JSON.stringify({
    href: String(location.href),
    title: String(document.title || ''),
    readyState: String(document.readyState || ''),
    metaVersion: safe(() => document.querySelector('meta[name="version"]')?.getAttribute('content')),
    bootstrapVersion: safe(() => localStorage.getItem('agora_web_bootstrap_version')),
    bootstrapScriptPresent: safe(() => Array.from(document.scripts).some((script) => new URL(script.src, location.href).pathname.endsWith('/flutter_bootstrap.js'))),
    bootstrapResourceLoaded: resources.some((name) => { try { return new URL(name, location.href).pathname.endsWith('/flutter_bootstrap.js'); } catch (_) { return false; } }),
    flutterRootPresent: Boolean(document.querySelector('flt-glass-pane, flutter-view, flt-semantics-host')),
    visibleVersionMatches: Array.from(new Set(versions)).slice(0, 8),
    navigationType: safe(() => performance.getEntriesByType('navigation')[0]?.type || null),
    viewport: {
      innerWidth: Number(window.innerWidth || 0),
      innerHeight: Number(window.innerHeight || 0),
      scrollTop: Number(scrolling ? scrolling.scrollTop : 0),
      clientHeight: Number(scrolling ? scrolling.clientHeight : 0),
      scrollHeight: Number(scrolling ? scrolling.scrollHeight : 0)
    },
    hitPath
  });
})()"""
TELEGRAM_TARGET = "telegram_bot"
TELEGRAM_BOT_LABEL = "AgoraLoginBot"
TELEGRAM_OPEN_LABEL = "OPEN"
TELEGRAM_BRANDED_OPEN_PHRASE = "進入 agoramarket 商城"
TELEGRAM_DIAGNOSTIC_Y_MIN = 470.0
TELEGRAM_DIAGNOSTIC_Y_MAX = 620.0
TELEGRAM_DIAGNOSTIC_LIMIT = 12
TELEGRAM_DIAGNOSTIC_TEXT_LIMIT = 120
TELEGRAM_VISUAL_LABELS = {
    "storefront": "進入 AgoraMarket 商城",
    "contact": "聯絡 @sirin555",
    "game": "月舞老虎機",
}
MAX_BODY_BYTES = 8 * 1024


def _redact(value: object) -> str:
    return DEVICE_ID.sub("[REDACTED-DEVICE-ID]", str(value)).replace("\x00", "")


def _validate_bind_host(host: str) -> str:
    if host != "127.0.0.1":
        raise ValueError("Sirin iOS Driver must bind exactly to 127.0.0.1")
    return host


def _full_version_tuple(value: object) -> tuple[int, int, int, int] | None:
    match = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)\+(\d+)", str(value or "").strip())
    return tuple(map(int, match.groups())) if match else None


def _exact_agoramarket_telegram_page(pages: object) -> dict[str, Any] | None:
    """Select one current Telegram WebView for the exact production host."""
    if not isinstance(pages, list):
        return None
    matches = []
    for candidate in pages:
        if not isinstance(candidate, dict):
            continue
        app = candidate.get("application")
        page = candidate.get("page")
        if not isinstance(app, dict) or not isinstance(page, dict):
            continue
        if TELEGRAM_BUNDLE_ID not in {app.get("bundleId"), app.get("host")}:
            continue
        parsed = urlparse(str(page.get("url") or ""))
        if parsed.scheme != "https" or parsed.hostname != AGORAMARKET_HOST:
            continue
        if page.get("type") not in {"WIRTypeWeb", "WIRTypeWebPage", "WIRTypeJavaScript"}:
            continue
        matches.append(candidate)
    return matches[0] if len(matches) == 1 else None


def _safari_pages(pages: object) -> list[dict[str, Any]]:
    if not isinstance(pages, list):
        return []
    matches = []
    for candidate in pages:
        if not isinstance(candidate, dict):
            continue
        app = candidate.get("application")
        page = candidate.get("page")
        if not isinstance(app, dict) or not isinstance(page, dict):
            continue
        if app.get("bundleId") != SAFARI_BUNDLE_ID or app.get("active") is not True:
            continue
        if page.get("type") not in SAFARI_PAGE_TYPES:
            continue
        if not str(page.get("key") or ""):
            continue
        matches.append(candidate)
    return matches


def _exact_safari_route_page(
    pages: object, url: str
) -> dict[str, Any] | None:
    expected = urlparse(url)
    if expected.scheme != "https" or not expected.hostname:
        return None
    matches = []
    for candidate in _safari_pages(pages):
        observed = urlparse(str(candidate["page"].get("url") or ""))
        if (
            observed.scheme == expected.scheme
            and observed.hostname == expected.hostname
            and observed.port == expected.port
        ):
            matches.append(candidate)
    return matches[0] if len(matches) == 1 else None


def _exact_active_webapp_page(
    pages: object, url: str, active_bundle_id: str
) -> dict[str, Any] | None:
    """Select one active inspected page tied to the foreground Home Screen app."""
    expected = urlparse(url)
    if expected.scheme != "https" or not expected.hostname or not active_bundle_id:
        return None
    matches = []
    for candidate in pages if isinstance(pages, list) else []:
        if not isinstance(candidate, dict):
            continue
        app = candidate.get("application")
        page = candidate.get("page")
        if not isinstance(app, dict) or not isinstance(page, dict):
            continue
        inspected_bundle = str(app.get("bundleId") or app.get("host") or "")
        observed = urlparse(str(page.get("url") or ""))
        if (
            app.get("active") is True
            and inspected_bundle == active_bundle_id
            and page.get("type") in SAFARI_PAGE_TYPES
            and str(page.get("key") or "")
            and observed.scheme == expected.scheme
            and observed.hostname == expected.hostname
            and observed.port == expected.port
        ):
            matches.append(candidate)
    return matches[0] if len(matches) == 1 else None


def _agoramarket_runtime_evidence(candidate: dict[str, Any], runtime: object) -> dict[str, Any]:
    if not isinstance(runtime, dict):
        raise ValueError("Web Inspector returned no structured runtime evidence")
    page = candidate["page"]
    app = candidate["application"]
    href = str(runtime.get("href") or "")
    parsed = urlparse(href)
    if parsed.scheme != "https" or parsed.hostname != AGORAMARKET_HOST:
        raise ValueError("runtime origin did not match the fixed AgoraMarket production host")

    meta_version = str(runtime.get("metaVersion") or "")
    bootstrap_version = str(runtime.get("bootstrapVersion") or "")
    parsed_version = _full_version_tuple(meta_version)
    version_bound = bool(
        runtime.get("readyState") == "complete"
        and runtime.get("flutterRootPresent")
        and parsed_version is not None
        and meta_version == bootstrap_version
    )
    minimum = _full_version_tuple(AGORAMARKET_REQUIRED_VERSION)
    version_pass = bool(version_bound and minimum is not None and parsed_version >= minimum)
    return {
        "ok": True,
        "observed_at": datetime.now(timezone.utc).isoformat(),
        "target": "telegram_agoramarket_webview",
        "physical_session_binding": "PASS",
        "application": {
            "bundle_id": app.get("bundleId"),
            "host_bundle_id": app.get("host"),
            "active": app.get("active"),
        },
        "page": {
            "key": page.get("key"),
            "type": page.get("type"),
            "url": page.get("url"),
            "runtime_url": href,
            "title": runtime.get("title"),
        },
        "client_version": {
            "required": AGORAMARKET_REQUIRED_VERSION,
            "observed": meta_version or None,
            "bootstrap_gate": bootstrap_version or None,
            "status": "PASS" if version_pass else "MISSING_PROOF",
            "source": "live Telegram WebView meta plus matching bootstrap gate and running Flutter root",
        },
        "runtime": {
            "ready_state": runtime.get("readyState"),
            "bootstrap_script_present": bool(runtime.get("bootstrapScriptPresent")),
            "bootstrap_resource_loaded": bool(runtime.get("bootstrapResourceLoaded")),
            "flutter_root_present": bool(runtime.get("flutterRootPresent")),
            "visible_version_matches": runtime.get("visibleVersionMatches") or [],
            "navigation_type": runtime.get("navigationType"),
            "viewport": runtime.get("viewport"),
            "hit_path": runtime.get("hitPath") or [],
        },
        "old_cache_update": {
            "status": "MISSING_PROOF",
            "reason": "No known-old cached-build precondition or observed reload count was supplied.",
        },
        "safety": {
            "fixed_expression": True,
            "arbitrary_javascript_supported": False,
            "storage_keys_read": ["agora_web_bootstrap_version"],
            "credentials_read": False,
            "page_mutated": False,
        },
    }


def _action_rejection(path: str, payload: dict[str, Any], acceptance_only: bool) -> str | None:
    always_allowed = {
        "/api/ensure-link",
        "/api/release-link",
        "/api/swipe",
        "/api/home",
        "/api/recover-home",
        "/api/open-app",
        "/api/acceptance-route",
    }
    if path == "/api/tap" and not acceptance_only:
        return None
    if path not in always_allowed:
        return "blocked by Sirin iPhone driver policy"
    if path == "/api/ensure-link" and payload:
        return "link activation accepts no parameters"
    if path == "/api/release-link" and payload:
        return "link release accepts no parameters"
    if path == "/api/open-app":
        if str(payload.get("name", "")).strip().lower() not in SAFE_APPS:
            return "only Safari or Telegram may be opened"
    elif path == "/api/acceptance-route":
        if payload.get("target") not in SAFE_ROUTES:
            return "unknown fixed acceptance route"
    elif path == "/api/swipe":
        try:
            dx = abs(float(payload["x2"]) - float(payload["x1"]))
            dy = abs(float(payload["y2"]) - float(payload["y1"]))
        except (KeyError, TypeError, ValueError):
            return "a valid vertical swipe is required"
        if dy < 80 or dx > dy / 2:
            return "only vertical scrolling is permitted"
    elif path == "/api/recover-home" and payload:
        return "Home recovery accepts no parameters"
    return None


def _exact_rows(rows: list[dict], text: str, kind: str | None = None) -> list[dict]:
    needle = text.strip().lower()
    return [
        row
        for row in rows
        if str(row.get("text", "")).strip().lower() == needle
        and (kind is None or row.get("type") == kind)
    ]


def _same_telegram_row(left: dict, right: dict, max_center_distance: float = 90.0) -> bool:
    """Require two Telegram controls to occupy the same visible list row."""
    try:
        left_y = float(left["y"])
        right_y = float(right["y"])
        left_rect = left.get("rect") or {}
        right_rect = right.get("rect") or {}
        left_top = float(left_rect.get("y", left_y))
        left_bottom = left_top + float(left_rect.get("height", 0))
        right_top = float(right_rect.get("y", right_y))
        right_bottom = right_top + float(right_rect.get("height", 0))
    except (KeyError, TypeError, ValueError):
        return False
    overlaps_vertically = max(left_top, right_top) <= min(left_bottom, right_bottom)
    return overlaps_vertically or abs(left_y - right_y) <= max_center_distance


def _telegram_chat_list_open(rows: list[dict]) -> dict | None:
    bots = _exact_rows(rows, TELEGRAM_BOT_LABEL)
    opens = _exact_rows(rows, TELEGRAM_OPEN_LABEL, "Button")
    if len(bots) != 1 or len(opens) != 1:
        return None
    return opens[0] if _same_telegram_row(bots[0], opens[0]) else None


def _telegram_bot_chat_visible(rows: list[dict]) -> bool:
    bot_rows = _exact_rows(rows, TELEGRAM_BOT_LABEL)
    headers = []
    for row in bot_rows:
        try:
            if float(row.get("y", -1)) <= 130:
                headers.append(row)
        except (TypeError, ValueError):
            continue
    # Telegram currently exposes neither the inline keyboard nor its compose
    # field as separate elements. The exact header plus a second same-name
    # message container is the observed chat-only WDA shape; a chat-list row
    # has no exact bot header in the navigation band.
    return len(headers) == 1 and len(bot_rows) >= 2


def _normalized_telegram_label(value: object) -> str:
    return " ".join(str(value).replace("\ufe0f", "").split()).casefold()


def _telegram_branded_open(rows: list[dict]) -> list[dict]:
    return [
        row
        for row in rows
        if TELEGRAM_BRANDED_OPEN_PHRASE
        in _normalized_telegram_label(row.get("text", ""))
    ]


def _telegram_storefront_buttons(rows: list[dict]) -> list[dict]:
    return _exact_rows(rows, TELEGRAM_OPEN_LABEL, "Button") + _telegram_branded_open(rows)


def _telegram_diagnostic_rows(rows: list[dict]) -> list[dict]:
    """Return bounded, evidence-only candidates near Telegram's inline keyboard."""
    candidates = []
    for row in rows:
        text = _redact(row.get("text", ""))[:TELEGRAM_DIAGNOSTIC_TEXT_LIMIT]
        normalized = _normalized_telegram_label(text)
        try:
            center_x = float(row.get("x", 0))
            center_y = float(row.get("y", -1))
        except (TypeError, ValueError):
            center_x = 0.0
            center_y = -1.0
        expected_band = TELEGRAM_DIAGNOSTIC_Y_MIN <= center_y <= TELEGRAM_DIAGNOSTIC_Y_MAX
        branded_hint = "agora" in normalized or "商城" in normalized
        if not expected_band and not branded_hint:
            continue

        rect = row.get("rect") if isinstance(row.get("rect"), dict) else {}
        bounded_rect = {}
        for key in ("x", "y", "width", "height"):
            try:
                bounded_rect[key] = round(float(rect.get(key, 0)), 1)
            except (TypeError, ValueError):
                bounded_rect[key] = 0.0
        candidates.append(
            {
                "text": text,
                "type": _redact(row.get("type", ""))[:64],
                "x": round(center_x, 1),
                "y": round(center_y, 1),
                "rect": bounded_rect,
            }
        )
        if len(candidates) >= TELEGRAM_DIAGNOSTIC_LIMIT:
            break
    return candidates


def _telegram_tree_diagnostic_rows(tree: object) -> list[dict]:
    """Extract small or interactive raw WDA nodes in the expected button band."""
    candidates = []

    def visit(node: object) -> None:
        if not isinstance(node, dict):
            return
        visible = str(node.get("isVisible", "1")) in ("1", "true", "True")
        rect = node.get("rect") if isinstance(node.get("rect"), dict) else {}
        try:
            x = float(rect.get("x", 0))
            y = float(rect.get("y", 0))
            width = float(rect.get("width", 0))
            height = float(rect.get("height", 0))
        except (TypeError, ValueError):
            x = y = width = height = 0.0
        text = _redact(node.get("label") or node.get("name") or node.get("value") or "")
        text = text[:TELEGRAM_DIAGNOSTIC_TEXT_LIMIT]
        kind = _redact(node.get("type", ""))[:64]
        center_x = x + width / 2
        center_y = y + height / 2
        expected_band = TELEGRAM_DIAGNOSTIC_Y_MIN <= center_y <= TELEGRAM_DIAGNOSTIC_Y_MAX
        branded_hint = "agora" in _normalized_telegram_label(text) or "商城" in text
        bounded_candidate = width > 0 and height > 0 and width <= 390 and height <= 160
        excluded_container = kind in {"Application", "Window"}
        if visible and not excluded_container and (
            branded_hint or (expected_band and bounded_candidate)
        ):
            interactive = kind in {"Button", "Link", "Cell"}
            candidates.append(
                (
                    0 if branded_hint else 1 if interactive else 2,
                    center_y,
                    center_x,
                    {
                        "text": text,
                        "type": kind,
                        "x": round(center_x, 1),
                        "y": round(center_y, 1),
                        "rect": {
                            "x": round(x, 1),
                            "y": round(y, 1),
                            "width": round(width, 1),
                            "height": round(height, 1),
                        },
                    },
                )
            )
        for child in node.get("children") or []:
            visit(child)

    visit(tree)
    candidates.sort(key=lambda candidate: candidate[:3])
    return [candidate[3] for candidate in candidates[:TELEGRAM_DIAGNOSTIC_LIMIT]]


def _png_dimensions(png: bytes) -> tuple[int, int] | None:
    if len(png) < 24 or png[:8] != b"\x89PNG\r\n\x1a\n" or png[12:16] != b"IHDR":
        return None
    width, height = struct.unpack(">II", png[16:24])
    if width <= 0 or height <= 0:
        return None
    return width, height


def _telegram_visual_entry_candidate(
    matches: dict[str, list[dict]],
    pixel_width: int,
    pixel_height: int,
    logical_width: float,
    logical_height: float,
) -> dict | None:
    if pixel_width <= 0 or pixel_height <= 0 or logical_width <= 0 or logical_height <= 0:
        return None
    if any(len(matches.get(key, [])) != 1 for key in TELEGRAM_VISUAL_LABELS):
        return None

    parsed = {}
    for key in TELEGRAM_VISUAL_LABELS:
        match = matches[key][0]
        try:
            center_x = float(match["centerX"])
            center_y = float(match["centerY"])
            width = float(match["width"])
            height = float(match["height"])
        except (KeyError, TypeError, ValueError):
            return None
        if not all(math.isfinite(value) for value in (center_x, center_y, width, height)):
            return None
        x_ratio = center_x / pixel_width
        y_ratio = center_y / pixel_height
        width_ratio = width / pixel_width
        height_ratio = height / pixel_height
        if not (0.30 <= x_ratio <= 0.70 and 0.68 <= y_ratio <= 0.92):
            return None
        if not (0.08 <= width_ratio <= 0.70 and 0.005 <= height_ratio <= 0.05):
            return None
        parsed[key] = {
            "center_x": center_x,
            "center_y": center_y,
            "width": width,
            "height": height,
        }

    ordered = [parsed["storefront"], parsed["contact"], parsed["game"]]
    if not (ordered[0]["center_y"] < ordered[1]["center_y"] < ordered[2]["center_y"]):
        return None
    gaps = [
        ordered[1]["center_y"] - ordered[0]["center_y"],
        ordered[2]["center_y"] - ordered[1]["center_y"],
    ]
    if any(not (0.035 * pixel_height <= gap <= 0.08 * pixel_height) for gap in gaps):
        return None
    if abs(gaps[0] - gaps[1]) > 0.02 * pixel_height:
        return None
    centers_x = [item["center_x"] for item in ordered]
    if max(centers_x) - min(centers_x) > 0.05 * pixel_width:
        return None

    storefront = parsed["storefront"]
    return {
        "pixel_center": {
            "x": round(storefront["center_x"], 2),
            "y": round(storefront["center_y"], 2),
        },
        "logical_point": {
            "x": round(storefront["center_x"] * logical_width / pixel_width, 2),
            "y": round(storefront["center_y"] * logical_height / pixel_height, 2),
        },
        "row_gaps_pixels": [round(gap, 2) for gap in gaps],
    }


def _telegram_visual_candidates_stable(
    first: dict,
    second: dict,
    pixel_width: int,
    pixel_height: int,
) -> bool:
    try:
        return (
            abs(float(first["pixel_center"]["x"]) - float(second["pixel_center"]["x"]))
            <= 0.01 * pixel_width
            and abs(
                float(first["pixel_center"]["y"])
                - float(second["pixel_center"]["y"])
            )
            <= 0.01 * pixel_height
            and all(
                abs(float(a) - float(b)) <= 0.01 * pixel_height
                for a, b in zip(first["row_gaps_pixels"], second["row_gaps_pixels"])
            )
        )
    except (KeyError, TypeError, ValueError):
        return False


def _windows_ocr_label_matches(png: bytes) -> tuple[dict[str, list[dict]], dict]:
    script_candidates = [
        ROOT.parents[1] / "scripts" / "ocr_windows_find_text.ps1",
        Path(os.environ.get("LOCALAPPDATA", ""))
        / "Sirin"
        / "scripts"
        / "ocr_windows_find_text.ps1",
    ]
    script = next((candidate for candidate in script_candidates if candidate.is_file()), None)
    powershell = Path(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")
    if sys.platform != "win32" or script is None or not powershell.is_file():
        return {}, {"state": "WINDOWS_OCR_UNAVAILABLE"}

    results = {}
    try:
        with tempfile.TemporaryDirectory(prefix="sirin-ios-ocr-") as temp_dir:
            image_path = Path(temp_dir) / "fresh-telegram.png"
            image_path.write_bytes(png)
            process = subprocess.run(
                [
                    str(powershell),
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    str(script),
                    "-ImagePath",
                    str(image_path),
                    "-Needle",
                    TELEGRAM_VISUAL_LABELS["storefront"],
                    "-MaxResults",
                    "2",
                    "-NeedlesBase64",
                    base64.b64encode(
                        json.dumps(
                            list(TELEGRAM_VISUAL_LABELS.values()),
                            ensure_ascii=False,
                        ).encode("utf-8")
                    ).decode("ascii"),
                ],
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=6,
                creationflags=subprocess.CREATE_NO_WINDOW,
            )
            if process.returncode:
                return {}, {"state": "WINDOWS_OCR_FAILED"}
            line = next(
                (line for line in reversed(process.stdout.splitlines()) if line.strip()),
                "",
            ).lstrip("\ufeff")
            payload = json.loads(line)
            engine_language = str(payload.get("engineLanguage", ""))
            if not payload.get("ok") or not engine_language.startswith("zh-Hant"):
                return {}, {"state": "WINDOWS_OCR_LANGUAGE_UNAVAILABLE"}
            matches_by_needle = payload.get("matchesByNeedle")
            if not isinstance(matches_by_needle, dict):
                return {}, {"state": "WINDOWS_OCR_INVALID_RESULT"}
            for key, needle in TELEGRAM_VISUAL_LABELS.items():
                matches = matches_by_needle.get(needle)
                if not isinstance(matches, list):
                    return {}, {"state": "WINDOWS_OCR_INVALID_RESULT"}
                results[key] = matches
    except (OSError, subprocess.SubprocessError, json.JSONDecodeError):
        return {}, {"state": "WINDOWS_OCR_FAILED"}
    return results, {
        "state": "WINDOWS_OCR_READY",
        "engine": "windows-ocr",
        "engine_language": engine_language,
        "match_counts": {key: len(value) for key, value in results.items()},
    }


def _telegram_launch_confirmation(rows: list[dict]) -> bool:
    buttons = {
        str(row.get("text", "")).strip().lower()
        for row in rows
        if row.get("type") == "Button"
    }
    cancel = {"cancel", "取消"}
    approve = {"open", "continue", "launch", "開啟", "繼續"}
    return bool(buttons & cancel) and bool(buttons & approve)


def _telegram_storefront_visible(rows: list[dict]) -> bool:
    labels = {str(row.get("text", "")).strip().lower() for row in rows}
    zh_tabs = {"商品", "訂單", "錢包", "我的"}
    en_tabs = {"products", "orders", "wallet", "profile"}
    return len(labels & zh_tabs) >= 3 or len(labels & en_tabs) >= 3


class DriverContext:
    def __init__(
        self,
        acceptance_only: bool,
        modules: dict[str, Any],
        attach_mode: str = "passive",
    ) -> None:
        self.acceptance_only = acceptance_only
        if attach_mode not in {"passive", "warm"}:
            raise ValueError("attach_mode must be passive or warm")
        self.attach_mode = attach_mode
        self.device = modules["device"]
        self.capture = modules["capture"]
        self.helpers = modules["helpers"]
        self.WDAClient = modules["WDAClient"]
        self.WDAError = modules["WDAError"]
        self.client = self.WDAClient(timeout=5)
        self.action_lock = threading.Lock()
        self.start_lock = threading.Lock()
        self.starting = False
        self.last_link_result = "NOT_ATTEMPTED"
        self.instance_id = f"{os.getpid()}-{time.time_ns()}"
        self._lan_cache_lock = threading.Lock()
        self._lan_cache_at = 0.0
        self._lan_cache: tuple[bool | None, str] = (None, "unknown")

    def _finish_link_result(self, result: str) -> str:
        # Keep only bounded state-machine labels here. Raw tool output can
        # contain device identifiers and must remain in the redacted log.
        self.last_link_result = result
        return result

    def ensure_link(
        self,
        wait_seconds: float = 45.0,
        recovery_grace_seconds: float = 0.0,
    ) -> str:
        with self.start_lock:
            self.starting = True
            try:
                state = self.client.link_state()
                if state == "up":
                    return self._finish_link_result("HEALTHY")
                if state == "wedged" and recovery_grace_seconds <= 0:
                    return self._finish_link_result("NEEDS_HUMAN_WDA_WEDGED")
                if not self.device.ios_path():
                    return self._finish_link_result("MISSING_GO_IOS")
                if not self.device.list_devices():
                    return self._finish_link_result("NEEDS_HUMAN_USB")
                if not self.device.lockdown_ready():
                    return self._finish_link_result("NEEDS_HUMAN_WAKE_UNLOCK")
                # The bundled go-ios forwarder may listen beyond loopback and
                # has no bind-address switch.  Refuse before tunnel, DDI or WDA
                # work unless the existing inbound block is already active.
                # Sirin observes this prerequisite; it never creates or edits
                # the firewall rule on behalf of a client.
                if not self.device.lan_block_rule_active():
                    return self._finish_link_result(
                        "SECURITY_BLOCK_LAN_PROTECTION_MISSING"
                    )
                if not self.device.tunnel_running():
                    self.device.start_tunnel()
                if not self.device.ddi_mounted():
                    # Mounting a personalized Developer Disk Image changes
                    # device state and may contact Apple TSS after an iOS
                    # update. Acceptance never performs that hidden network or
                    # device mutation; the operator prepares it explicitly.
                    return self._finish_link_result("NEEDS_HUMAN_DDI")
                bundle = self.device.detect_wda_bundle()
                if not bundle:
                    return self._finish_link_result("NEEDS_HUMAN_WDA_INSTALL")
                if recovery_grace_seconds > 0:
                    # A Home recovery may release an existing runner's AX wait.
                    # Re-open only the local forwards and give that runner time
                    # to answer before launching another test bundle; launching
                    # on top of a live-but-stuck runner produces XCTest 103.
                    self.device.start_forwards()
                    grace_deadline = time.monotonic() + recovery_grace_seconds
                    while time.monotonic() < grace_deadline:
                        if self.client.is_up():
                            return self._finish_link_result("RECOVERED_EXISTING_WDA")
                        time.sleep(0.25)
                self.device.start_wda(bundle)
                self.device.start_forwards()
                deadline = time.monotonic() + max(5.0, wait_seconds)
                while time.monotonic() < deadline:
                    if self.client.is_up():
                        return self._finish_link_result("RECOVERED")
                    time.sleep(0.25)
                return self._finish_link_result("WDA_START_TIMEOUT")
            except Exception as error:
                logging.warning("link start failed: %s", _redact(error))
                return self._finish_link_result("DRIVER_START_FAILED")
            finally:
                self.starting = False
                with self._lan_cache_lock:
                    self._lan_cache_at = 0.0

    def _lan_safety(self, max_age_seconds: float = 2.0) -> tuple[bool | None, str]:
        now = time.monotonic()
        with self._lan_cache_lock:
            if now - self._lan_cache_at <= max_age_seconds:
                return self._lan_cache
            try:
                scope = self.device.forward_listener_scope()
                if scope == "unknown":
                    result = (None, "listener_check_unavailable")
                elif scope == "lan":
                    if self.device.lan_block_rule_active():
                        result = (False, "firewall_blocked_lan_listener")
                    else:
                        result = (True, "unblocked_lan_listener")
                elif scope == "loopback":
                    result = (False, "loopback_listener")
                else:
                    if self.device.lan_block_rule_active():
                        result = (False, "forwards_inactive_firewall_ready")
                    else:
                        result = (False, "forwards_inactive_activation_blocked")
            except Exception as error:
                logging.warning("LAN listener check failed: %s", _redact(error))
                result = (None, "listener_check_failed")
            self._lan_cache_at = now
            self._lan_cache = result
            return result

    def healthz(self) -> dict[str, Any]:
        return {
            "ok": True,
            "provider": "sirin-ios-driver",
            "instance_id": self.instance_id,
            "acceptance_only": self.acceptance_only,
            "attach_mode": self.attach_mode,
            "bind": "127.0.0.1",
        }

    def status(self) -> dict[str, Any]:
        device_probe_status = "ok"
        try:
            device_detected = bool(self.device.list_devices())
        except Exception:
            device_detected = False
            device_probe_status = "failed"
        state = self.client.link_state() if device_detected else "down"
        last_link_result = self.last_link_result
        if device_probe_status == "failed":
            last_link_result = "DRIVER_DEVICE_PROBE_FAILED"
        elif not device_detected:
            last_link_result = "NEEDS_HUMAN_USB"
        elif (
            self.attach_mode == "passive"
            and state != "up"
            and last_link_result == "NOT_ATTEMPTED"
        ):
            last_link_result = "PASSIVE_LINK_NOT_STARTED"
        lan_exposed, lan_protection = self._lan_safety()
        return {
            "provider": "sirin-ios-driver",
            "instance_id": self.instance_id,
            "device_detected": device_detected,
            "device_probe_status": device_probe_status,
            "window": None,
            "input": state == "up",
            "link": state,
            "starting": self.starting,
            "last_link_result": last_link_result,
            "acceptance_only": self.acceptance_only,
            "attach_mode": self.attach_mode,
            "link_activation": "background" if self.attach_mode == "warm" else "on_demand",
            "lan_exposed": lan_exposed,
            "lan_protection": lan_protection,
        }

    def phone(self, current_status: dict[str, Any] | None = None) -> tuple[int, dict[str, Any]]:
        current_status = current_status or self.status()
        if not current_status["device_detected"]:
            return 503, {
                "info_readable": False,
                "error": current_status.get("last_link_result") or "MISSING_PROOF",
            }
        if current_status["link"] != "up":
            error = str(current_status.get("last_link_result") or self.last_link_result)
            if error in {
                "NOT_ATTEMPTED",
                "HEALTHY",
                "RECOVERED",
                "RECOVERED_EXISTING_WDA",
            }:
                error = "MISSING_WDA_LINK"
            return 503, {"info_readable": False, "error": error}
        try:
            active = self.client.active_app()
            bundle = active.get("bundleId") if isinstance(active, dict) else None
            return 200, {
                "info_readable": True,
                "battery": self.client.battery(),
                "locked": self.client.is_locked(),
                "app": bundle,
                "page": None,
                "session": self.client.ensure_session(),
            }
        except self.WDAError as error:
            return 503, {"info_readable": False, "error": _redact(error)}

    def snapshot(self) -> dict[str, Any]:
        current_status = self.status()
        phone_status, phone = self.phone(current_status)
        return {
            "status": current_status,
            "phone_status": phone_status,
            "phone": phone,
        }

    def release_link(self) -> dict[str, Any]:
        """Release only Sirin-owned transient phone-link processes.

        This does not unmount DDI, change trust, touch firewall rules, or alter
        phone/network settings.  PID ownership is re-verified by ``stop_all``
        before each process is terminated.
        """
        names = ("forward8100", "forward9100", "runwda", "tunnel", "syslog")
        report = self.device.stop_all_verified(names)
        listener_scope = self.device.forward_listener_scope()
        release_ok = not report["failed"] and listener_scope == "inactive"
        self.client.session_id = None
        self._finish_link_result(
            "PASSIVE_LINK_NOT_STARTED" if release_ok else "RELEASE_INCOMPLETE"
        )
        with self._lan_cache_lock:
            self._lan_cache_at = 0.0
        return {
            "ok": release_ok,
            "status": "released" if release_ok else "release_incomplete",
            "stopped": report["stopped"],
            "failed": report["failed"],
            "stale_pid_files_removed": report["stale_pid_files_removed"],
            "forward_listener_scope": listener_scope,
            "persistent_changes": [],
        }

    def home(self) -> dict[str, Any]:
        """Put SpringBoard in front without bypassing Sirin's action lease.

        A wedged WDA cannot execute its own Home command, and starting another
        runner on top of the stuck one fails with XCTest error 103.  The
        go-ios SpringBoard launch is therefore used only as the implementation
        of an explicit Sirin ``ios_home`` action when WDA is unavailable.  The
        provider never invokes this fallback from its background health loop.
        """
        if self.client.link_state() == "up":
            self.helpers.press_home()
            return {"ok": True, "transport": "wda"}
        return self.recover_home()

    def _request_link_recovery(self) -> None:
        threading.Thread(
            target=self.ensure_link,
            kwargs={"wait_seconds": 45.0, "recovery_grace_seconds": 30.0},
            daemon=True,
            name="sirin-ios-recover-wda",
        ).start()

    def recover_home(self) -> dict[str, Any]:
        """Recover a down or wedged WDA through one fixed USB Home action."""
        prior_link = self.client.link_state()
        if prior_link == "up":
            raise RuntimeError("RECOVERY_NOT_NEEDED: WDA is healthy; use the leased Home action")
        if prior_link not in {"down", "wedged"}:
            raise RuntimeError("RECOVERY_BLOCKED: WDA state is not safely recoverable")
        if not self.device.foreground_springboard():
            raise RuntimeError("Home recovery failed while WebDriverAgent was unavailable")
        self._request_link_recovery()
        return {
            "ok": True,
            "transport": "go-ios-springboard-recovery",
            "prior_link": prior_link,
            "wda_recovery_requested": True,
        }

    def launch_route(self, target: str) -> dict[str, Any]:
        if target == "telegram_bot":
            return self._open_telegram_bot()
        if target == "codex_remote":
            return self._open_codex_remote_home()
        url = SAFE_ROUTES[target]
        expected = urlparse(url)
        if expected.scheme != "https" or not expected.hostname:
            raise RuntimeError("fixed Safari route is invalid")
        executable = self.device.ios_path()
        if not executable:
            raise RuntimeError("go-ios is unavailable")

        def run_json(arguments: list[str], timeout: int) -> object:
            process = subprocess.run(
                [executable, *arguments],
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=timeout,
                creationflags=subprocess.CREATE_NO_WINDOW if sys.platform == "win32" else 0,
            )
            if process.returncode:
                raise RuntimeError("fixed Safari Web Inspector action failed")
            try:
                return json.loads(process.stdout)
            except json.JSONDecodeError as error:
                raise RuntimeError("fixed Safari Web Inspector returned invalid JSON") from error

        active = self.client.active_app()
        if not isinstance(active, dict) or active.get("bundleId") != SAFARI_BUNDLE_ID:
            self.helpers.open_app("Safari", wait_seconds=3)
            active = self.client.active_app()
        if not isinstance(active, dict) or active.get("bundleId") != SAFARI_BUNDLE_ID:
            raise RuntimeError("MISSING_PROOF: Safari is not the foreground application")
        pages = run_json(["webinspector", "list", "--timeout=3"], 5)
        candidates = _safari_pages(pages)
        if len(candidates) != 1:
            raise RuntimeError("MISSING_PROOF: exactly one Safari page was not inspectable")
        current = candidates[0]
        already_open = _exact_safari_route_page([current], url)
        if already_open is not None:
            return {
                "ok": True,
                "target": target,
                "observed_url": already_open["page"].get("url"),
                "transport": "webinspector-fixed-page",
                "verification": "foreground_app_and_single_webinspector_page",
                "dispatch_status": "ALREADY_OPEN",
                "reconnect_expected": False,
            }
        page_key = str(current["page"].get("key") or "")
        expression = (
            "(() => { setTimeout(() => window.location.assign("
            + json.dumps(url)
            + "), 250); return true; })()"
        )
        dispatch_status = "CONFIRMED"
        try:
            run_json(
                ["webinspector", "eval", page_key, expression, "--timeout=6"],
                8,
            )
        except (RuntimeError, subprocess.TimeoutExpired):
            # A successful location change tears down the inspected page and
            # go-ios can therefore exit non-zero after dispatch. Never trust
            # that exit; only the fresh foreground-app + single-page listing
            # below may turn it into success.
            dispatch_status = "VERIFY_AFTER_DISCONNECT"

        for _ in range(2):
            time.sleep(1)
            active = self.client.active_app()
            if not isinstance(active, dict) or active.get("bundleId") != SAFARI_BUNDLE_ID:
                continue
            try:
                pages = run_json(["webinspector", "list", "--timeout=2"], 4)
            except (RuntimeError, subprocess.TimeoutExpired):
                # Web Inspector can need one reconnect cycle after location
                # replaced the inspected page. The bounded loop remains the
                # only retry and still requires both independent proofs.
                continue
            if len(_safari_pages(pages)) != 1:
                continue
            observed = _exact_safari_route_page(pages, url)
            if observed is not None:
                return {
                    "ok": True,
                    "target": target,
                    "observed_url": observed["page"].get("url"),
                    "transport": "webinspector-fixed-page",
                    "verification": "foreground_app_and_single_webinspector_page",
                    "dispatch_status": dispatch_status,
                    "reconnect_expected": True,
                }
        raise RuntimeError("MISSING_PROOF: fixed Safari navigation was not visible")

    def _open_codex_remote_home(self) -> dict[str, Any]:
        """Open only the installed Remote Desktop Home Screen PWA."""
        url = SAFE_ROUTES["codex_remote"]
        executable = self.device.ios_path()
        if not executable:
            raise RuntimeError("go-ios is unavailable")

        def inspected_pages() -> object:
            process = subprocess.run(
                [executable, "webinspector", "list", "--timeout=3"],
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=5,
                creationflags=subprocess.CREATE_NO_WINDOW if sys.platform == "win32" else 0,
            )
            if process.returncode:
                raise RuntimeError("fixed Home Screen PWA inspection failed")
            try:
                return json.loads(process.stdout)
            except json.JSONDecodeError as error:
                raise RuntimeError("fixed Home Screen PWA inspection returned invalid JSON") from error

        self.helpers.press_home()
        active = self.client.active_app()
        if not isinstance(active, dict) or active.get("bundleId") != SPRINGBOARD_BUNDLE_ID:
            raise RuntimeError("MISSING_PROOF: SpringBoard is not the foreground application")
        icon = self.client.find_first(
            f'**/XCUIElementTypeIcon[`name == "{CODEX_REMOTE_HOME_LABEL}"`]'
        )
        if not icon:
            raise RuntimeError(
                "NEEDS_HUMAN_ADD_TO_HOME_SCREEN: Remote Desktop icon is not visible"
            )
        self.client.element_click(icon)

        for _ in range(5):
            time.sleep(1)
            active = self.client.active_app()
            if not isinstance(active, dict):
                continue
            active_bundle_id = str(active.get("bundleId") or "")
            if not active_bundle_id or active_bundle_id == SPRINGBOARD_BUNDLE_ID:
                continue
            try:
                observed = _exact_active_webapp_page(
                    inspected_pages(), url, active_bundle_id
                )
            except (RuntimeError, subprocess.TimeoutExpired):
                continue
            if observed is not None:
                return {
                    "ok": True,
                    "target": "codex_remote",
                    "observed_url": observed["page"].get("url"),
                    "transport": "home-screen-webapp",
                    "verification": "exact_home_icon_and_active_webinspector_page",
                    "dispatch_status": "HOME_ICON_OPENED",
                    "reconnect_expected": True,
                }
        raise RuntimeError("MISSING_PROOF: fixed Home Screen PWA was not visible")

    def agoramarket_webview_evidence(self) -> dict[str, Any]:
        """Read fixed, non-secret evidence from the foreground Telegram Mini App."""
        phone_status, phone = self.phone()
        if phone_status != 200 or phone.get("locked") is not False:
            raise RuntimeError("MISSING_PROOF: the iPhone must be unlocked and readable")
        if phone.get("app") != TELEGRAM_BUNDLE_ID:
            raise RuntimeError("MISSING_PROOF: Telegram is not the foreground application")
        executable = self.device.ios_path()
        if not executable:
            raise RuntimeError("go-ios is unavailable")

        def run_json(arguments: list[str], timeout: int) -> object:
            process = subprocess.run(
                [executable, *arguments],
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=timeout,
                creationflags=subprocess.CREATE_NO_WINDOW if sys.platform == "win32" else 0,
            )
            if process.returncode:
                raise RuntimeError("fixed Web Inspector probe failed")
            try:
                return json.loads(process.stdout)
            except json.JSONDecodeError as error:
                raise RuntimeError("fixed Web Inspector probe returned invalid JSON") from error

        pages = run_json(["webinspector", "list", "--timeout=8"], 15)
        candidate = _exact_agoramarket_telegram_page(pages)
        if candidate is None:
            raise RuntimeError("MISSING_PROOF: exactly one Telegram AgoraMarket WebView was not inspectable")
        page_key = str(candidate["page"].get("key") or "")
        if not page_key:
            raise RuntimeError("MISSING_PROOF: the Telegram AgoraMarket WebView had no page key")
        evaluated = run_json(
            ["webinspector", "eval", page_key, AGORAMARKET_WEBVIEW_EXPRESSION, "--timeout=12"],
            20,
        )
        if isinstance(evaluated, str):
            try:
                evaluated = json.loads(evaluated)
            except json.JSONDecodeError as error:
                raise RuntimeError("fixed Web Inspector runtime evidence was invalid") from error
        return _agoramarket_runtime_evidence(candidate, evaluated)

    def _observe_telegram_visual_entry(self) -> dict[str, Any]:
        try:
            screen = self.helpers.screen_info()
            logical_width = float(screen["width"])
            logical_height = float(screen["height"])
        except (AttributeError, KeyError, TypeError, ValueError):
            return {"ok": False, "diagnostic": {"state": "SCREEN_GEOMETRY_UNAVAILABLE"}}

        observations = []
        for _ in range(2):
            png = self.capture.screenshot_png(max_age=0.0)
            dimensions = _png_dimensions(png)
            if dimensions is None:
                return {"ok": False, "diagnostic": {"state": "INVALID_FRESH_SCREENSHOT"}}
            pixel_width, pixel_height = dimensions
            matches, diagnostic = _windows_ocr_label_matches(png)
            candidate = _telegram_visual_entry_candidate(
                matches,
                pixel_width,
                pixel_height,
                logical_width,
                logical_height,
            )
            if candidate is None:
                return {
                    "ok": False,
                    "diagnostic": {
                        **diagnostic,
                        "state": "NO_UNIQUE_TELEGRAM_BRANDED_TRIPLE",
                    },
                }
            observations.append(
                {
                    "candidate": candidate,
                    "pixel_width": pixel_width,
                    "pixel_height": pixel_height,
                    "frame_sha256": hashlib.sha256(png).hexdigest(),
                    "ocr": diagnostic,
                }
            )

        first, second = observations
        if (
            first["pixel_width"] != second["pixel_width"]
            or first["pixel_height"] != second["pixel_height"]
            or not _telegram_visual_candidates_stable(
                first["candidate"],
                second["candidate"],
                second["pixel_width"],
                second["pixel_height"],
            )
        ):
            return {"ok": False, "diagnostic": {"state": "TELEGRAM_VISUAL_ENTRY_UNSTABLE"}}

        return {
            "ok": True,
            "candidate": second["candidate"],
            "evidence": {
                "selector": "fresh_windows_ocr_branded_triple",
                "fresh_frames_verified": 2,
                "frame_sha256": second["frame_sha256"],
                "pixel_dimensions": {
                    "width": second["pixel_width"],
                    "height": second["pixel_height"],
                },
                "logical_point": second["candidate"]["logical_point"],
                "labels_verified": list(TELEGRAM_VISUAL_LABELS.values()),
                "ocr_engine": second["ocr"].get("engine"),
                "ocr_language": second["ocr"].get("engine_language"),
            },
        }

    def _open_telegram_bot(self) -> dict[str, Any]:
        target = TELEGRAM_TARGET
        self.helpers.open_app("Telegram", wait_seconds=5)
        time.sleep(1)
        rows = self.helpers.ocr()

        def has(text: str, kind: str | None = None) -> bool:
            return bool(_exact_rows(rows, text, kind))

        def human_state(
            state: str,
            detail: str,
            include_diagnostics: bool = False,
        ) -> dict[str, Any]:
            result = {"ok": False, "target": target, "state": state, "detail": detail}
            if include_diagnostics:
                diagnostics = _telegram_diagnostic_rows(rows)
                diagnostic_source = "visible_text_rows"
                tree_reader = getattr(self.helpers, "ui_tree", None)
                if callable(tree_reader):
                    try:
                        tree_diagnostics = _telegram_tree_diagnostic_rows(tree_reader())
                    except Exception:
                        tree_diagnostics = []
                    if tree_diagnostics:
                        diagnostics = tree_diagnostics
                        diagnostic_source = "wda_tree"
                result["diagnostic_rows"] = diagnostics
                result["diagnostic_source"] = diagnostic_source
                result["diagnostic_note"] = (
                    "Untrusted accessibility evidence only; Sirin did not select or tap it."
                )
            return result

        if _telegram_storefront_visible(rows):
            return {
                "ok": True,
                "target": target,
                "state": "STOREFRONT_ALREADY_OPEN",
                "storefront_entry_opened": True,
                "reconnect_expected": False,
            }

        if has("Start", "Button"):
            return human_state(
                "NEEDS_HUMAN_BOT_START",
                "Press Start on the iPhone; Sirin will not send /start.",
            )

        if _telegram_launch_confirmation(rows):
            return human_state(
                "NEEDS_HUMAN_TELEGRAM_LAUNCH_CONFIRMATION",
                "Approve the Telegram launch confirmation on the iPhone.",
            )

        entry_open_tapped = False
        visual_selector_evidence = None
        branded_opens = _telegram_branded_open(rows)
        if len(branded_opens) > 1:
            return human_state(
                "NEEDS_HUMAN_TELEGRAM_ENTRY_AMBIGUOUS",
                "Multiple branded AgoraMarket entry controls are visible; Sirin will not guess.",
                include_diagnostics=True,
            )
        if len(branded_opens) == 1:
            self.helpers.tap(branded_opens[0]["x"], branded_opens[0]["y"])
            entry_open_tapped = True
            time.sleep(2)
            rows = self.helpers.ocr()
        else:
            chat_list_open = _telegram_chat_list_open(rows)
        if not entry_open_tapped and chat_list_open is not None:
            self.helpers.tap(chat_list_open["x"], chat_list_open["y"])
            entry_open_tapped = True
            time.sleep(2)
            rows = self.helpers.ocr()
        elif not entry_open_tapped and _telegram_bot_chat_visible(rows):
            visual_observation = self._observe_telegram_visual_entry()
            if not visual_observation.get("ok"):
                result = human_state(
                    "NEEDS_HUMAN_TELEGRAM_OPEN",
                    "The branded Telegram entry was not uniquely stable in two fresh frames; Sirin will not guess.",
                    include_diagnostics=True,
                )
                result["visual_diagnostic"] = visual_observation.get("diagnostic", {})
                return result
            rows = self.helpers.ocr()
            if not _telegram_bot_chat_visible(rows):
                return human_state(
                    "NEEDS_HUMAN_TELEGRAM_ENTRY_CHANGED",
                    "Telegram changed after visual observation; Sirin discarded the candidate.",
                    include_diagnostics=True,
                )
            if _telegram_launch_confirmation(rows):
                return human_state(
                    "NEEDS_HUMAN_TELEGRAM_LAUNCH_CONFIRMATION",
                    "Approve the Telegram launch confirmation on the iPhone.",
                )
            if has("Start", "Button"):
                return human_state(
                    "NEEDS_HUMAN_BOT_START",
                    "Press Start on the iPhone; Sirin will not send /start.",
                )
            logical_point = visual_observation["candidate"]["logical_point"]
            self.helpers.tap(logical_point["x"], logical_point["y"])
            entry_open_tapped = True
            visual_selector_evidence = visual_observation["evidence"]
            time.sleep(2)
            rows = self.helpers.ocr()
        elif not entry_open_tapped and has(TELEGRAM_BOT_LABEL):
            matches = _exact_rows(rows, TELEGRAM_BOT_LABEL)
            if len(matches) != 1:
                return human_state(
                    "NEEDS_HUMAN_TELEGRAM_ENTRY_AMBIGUOUS",
                    "Multiple AgoraLoginBot entries are visible; Sirin will not guess.",
                    include_diagnostics=True,
                )
            self.helpers.tap(matches[0]["x"], matches[0]["y"])
            time.sleep(1)
            rows = self.helpers.ocr()

        if _telegram_launch_confirmation(rows):
            return human_state(
                "NEEDS_HUMAN_TELEGRAM_LAUNCH_CONFIRMATION",
                "Approve the Telegram launch confirmation on the iPhone.",
            )
        if has("Start", "Button"):
            return human_state(
                "NEEDS_HUMAN_BOT_START",
                "Press Start on the iPhone; Sirin will not send /start.",
            )

        if entry_open_tapped:
            if _telegram_storefront_visible(rows):
                result = {
                    "ok": True,
                    "target": target,
                    "state": "STOREFRONT_ALREADY_OPEN",
                    "storefront_entry_opened": True,
                    "reconnect_expected": False,
                }
                if visual_selector_evidence is not None:
                    result["selector_evidence"] = visual_selector_evidence
                return result
            if _telegram_storefront_buttons(rows):
                result = human_state(
                    "MISSING_PROOF_TELEGRAM_OPEN",
                    "A fixed storefront control remained visible after one tap; Sirin will not tap again in the same action.",
                )
                if visual_selector_evidence is not None:
                    result["selector_evidence"] = visual_selector_evidence
                return result
            result = {
                "ok": True,
                "target": target,
                "state": "STOREFRONT_ENTRY_OPENED",
                "storefront_entry_opened": True,
                "reconnect_expected": False,
            }
            if visual_selector_evidence is not None:
                result["selector_evidence"] = visual_selector_evidence
            return result

        if not _telegram_storefront_visible(rows) and not has(TELEGRAM_BOT_LABEL):
            for _ in range(3):
                if has("Navigation/Search"):
                    break
                self.helpers.swipe(10, 400, 300, 400, 0.3)
                time.sleep(0.5)
                rows = self.helpers.ocr()
            if not has("Navigation/Search"):
                raise RuntimeError("Telegram chat list is not reachable")
            self.helpers.tap_text("Navigation/Search", exact=True)
            time.sleep(0.5)
            rows = self.helpers.ocr()
            fields = [row for row in rows if row.get("type") == "SearchField"]
            if fields:
                self.helpers.set_field_text(fields[0], "@agora_login_bot")
            else:
                self.helpers.type_text("@agora_login_bot")
            time.sleep(1)
            matches = self.helpers.find_text(TELEGRAM_BOT_LABEL, exact=True)
            if len(matches) != 1:
                raise RuntimeError("expected exactly one AgoraLoginBot search result")
            self.helpers.tap(matches[0]["x"], matches[0]["y"])
            time.sleep(1)
            rows = self.helpers.ocr()

        if _telegram_storefront_visible(rows):
            return {
                "ok": True,
                "target": target,
                "state": "STOREFRONT_ALREADY_OPEN",
                "storefront_entry_opened": True,
                "reconnect_expected": False,
            }
        if has("Start", "Button"):
            return human_state(
                "NEEDS_HUMAN_BOT_START",
                "Press Start on the iPhone; Sirin will not send /start.",
            )
        if _telegram_launch_confirmation(rows):
            return human_state(
                "NEEDS_HUMAN_TELEGRAM_LAUNCH_CONFIRMATION",
                "Approve the Telegram launch confirmation on the iPhone.",
            )

        opens = _telegram_storefront_buttons(rows)
        if len(opens) != 1:
            state = (
                "NEEDS_HUMAN_TELEGRAM_ENTRY_AMBIGUOUS"
                if len(opens) > 1
                else "NEEDS_HUMAN_TELEGRAM_OPEN"
            )
            return human_state(
                state,
                "Sirin requires exactly one visible fixed AgoraMarket storefront control and will not guess.",
                include_diagnostics=True,
            )

        self.helpers.tap(opens[0]["x"], opens[0]["y"])
        time.sleep(2)
        rows = self.helpers.ocr()
        if _telegram_launch_confirmation(rows):
            return human_state(
                "NEEDS_HUMAN_TELEGRAM_LAUNCH_CONFIRMATION",
                "Approve the Telegram launch confirmation on the iPhone.",
            )
        if has("Start", "Button"):
            return human_state(
                "NEEDS_HUMAN_BOT_START",
                "Press Start on the iPhone; Sirin will not send /start.",
            )
        if _telegram_storefront_buttons(rows):
            return human_state(
                "MISSING_PROOF_TELEGRAM_OPEN",
                "A fixed storefront control remained visible after one tap; Sirin will not tap it again.",
            )
        return {
            "ok": True,
            "target": target,
            "state": "STOREFRONT_ENTRY_OPENED",
            "storefront_entry_opened": True,
            "reconnect_expected": False,
        }


def _handler(context: DriverContext, port: int):
    class Handler(BaseHTTPRequestHandler):
        server_version = "SirinIosDriver/1"
        protocol_version = "HTTP/1.1"

        def _allowed(self) -> bool:
            host = self.headers.get("Host", "")
            if host not in {f"127.0.0.1:{port}", f"localhost:{port}"}:
                return False
            origin = self.headers.get("Origin")
            if origin and origin not in {f"http://127.0.0.1:{port}", f"http://localhost:{port}"}:
                return False
            return self.headers.get("Sec-Fetch-Site", "none") in {"same-origin", "none"}

        def _send(self, status: int, body: bytes, content_type: str) -> None:
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(body)

        def _json(self, value: dict[str, Any], status: int = 200) -> None:
            self._send(status, json.dumps(value, separators=(",", ":")).encode(), "application/json")

        def do_GET(self) -> None:  # noqa: N802
            if not self._allowed():
                self.close_connection = True
                self._json({"error": "forbidden"}, 403)
                return
            path = self.path.split("?", 1)[0]
            try:
                if path == "/api/status":
                    self._json(context.status())
                elif path == "/api/healthz":
                    self._json(context.healthz())
                elif path == "/api/snapshot":
                    self._json(context.snapshot())
                elif path == "/api/phone":
                    status, value = context.phone()
                    self._json(value, status)
                elif path == "/api/screenshot":
                    data = context.capture.screenshot_png(max_age=0.4)
                    self._send(200, data, "image/png")
                elif path == "/api/agoramarket-webview-evidence":
                    self._json(context.agoramarket_webview_evidence())
                else:
                    self._json({"error": "not found"}, 404)
            except Exception as error:
                self._json({"error": _redact(error)}, 502)

        def do_POST(self) -> None:  # noqa: N802
            if not self._allowed() or self.headers.get("Transfer-Encoding"):
                self.close_connection = True
                self._json({"error": "forbidden"}, 403)
                return
            if not self.headers.get("Content-Type", "").startswith("application/json"):
                self.close_connection = True
                self._json({"error": "JSON required"}, 415)
                return
            try:
                length = int(self.headers.get("Content-Length", "0"))
            except ValueError:
                length = -1
            if length < 0 or length > MAX_BODY_BYTES:
                self.close_connection = True
                self._json({"error": "invalid body length"}, 413)
                return
            try:
                payload = json.loads(self.rfile.read(length) or b"{}")
            except (UnicodeDecodeError, json.JSONDecodeError):
                self._json({"error": "invalid JSON"}, 400)
                return
            path = self.path.split("?", 1)[0]
            rejection = _action_rejection(path, payload, context.acceptance_only)
            if rejection:
                self._json({"ok": False, "error": rejection}, 403)
                return
            try:
                with context.action_lock:
                    if path == "/api/ensure-link":
                        link_result = context.ensure_link()
                        result = {
                            "ok": link_result
                            in {"HEALTHY", "RECOVERED", "RECOVERED_EXISTING_WDA"},
                            "link_result": link_result,
                        }
                    elif path == "/api/release-link":
                        result = context.release_link()
                    elif path == "/api/swipe":
                        context.client.swipe(
                            float(payload["x1"]),
                            float(payload["y1"]),
                            float(payload["x2"]),
                            float(payload["y2"]),
                            min(max(float(payload.get("seconds", 0.3)), 0.05), 3.0),
                        )
                        result = {"ok": True}
                    elif path == "/api/home":
                        result = context.home()
                    elif path == "/api/recover-home":
                        result = context.recover_home()
                    elif path == "/api/open-app":
                        context.helpers.open_app(SAFE_APPS[str(payload["name"]).strip().lower()])
                        result = {"ok": True}
                    elif path == "/api/acceptance-route":
                        result = context.launch_route(str(payload["target"]))
                    elif path == "/api/tap":
                        context.client.tap(float(payload["x"]), float(payload["y"]))
                        result = {"ok": True}
                    else:
                        raise RuntimeError("unsupported action")
                self._json(result)
            except Exception as error:
                self._json({"ok": False, "error": _redact(error)}, 502)

        def log_message(self, format: str, *args: object) -> None:
            logging.info("http %s", _redact(format % args))

    return Handler


def _configure_logging(runtime_root: Path) -> None:
    log_dir = runtime_root / "logs"
    log_dir.mkdir(parents=True, exist_ok=True)
    handler = logging.handlers.RotatingFileHandler(
        log_dir / "sirin-ios-driver.log",
        maxBytes=10 * 1024 * 1024,
        backupCount=9,
        encoding="utf-8",
    )
    handler.setFormatter(logging.Formatter("%(asctime)s %(levelname)s %(message)s"))
    logging.basicConfig(level=logging.INFO, handlers=[handler], force=True)


def _parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=8770)
    parser.add_argument("--runtime-root", required=True)
    parser.add_argument("--health-poll-seconds", type=int, default=60)
    parser.add_argument("--attach-mode", choices=("passive", "warm"), default="passive")
    parser.add_argument("--supervised-control", action="store_true")
    parser.add_argument("--validate-only", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = _parse_args()
    if not 1024 <= args.port <= 65535:
        raise SystemExit("port must be 1024..65535")
    if not 10 <= args.health_poll_seconds <= 3600:
        raise SystemExit("health-poll-seconds must be 10..3600")
    runtime_root = Path(args.runtime_root).expanduser().resolve()
    runtime_root.mkdir(parents=True, exist_ok=True)
    os.environ["SIRIN_IOS_DRIVER_RUNTIME_ROOT"] = str(runtime_root)
    os.environ["SIRIN_IOS_DRIVER_ACCEPTANCE_ONLY"] = "0" if args.supervised_control else "1"
    os.environ["SIRIN_IOS_DRIVER_PORT"] = str(args.port)
    sys.path.insert(0, str(ROOT / "src"))

    from phone_harness import capture, device, helpers
    from phone_harness.wda_client import WDAClient, WDAError

    modules = {
        "capture": capture,
        "device": device,
        "helpers": helpers,
        "WDAClient": WDAClient,
        "WDAError": WDAError,
    }
    if args.validate_only:
        print(
            json.dumps(
                {
                    "valid": True,
                    "provider": "sirin-ios-driver",
                    "source_root": str(ROOT),
                    "runtime_root": str(runtime_root),
                    "go_ios": device.ios_path() is not None,
                    "acceptance_only": not args.supervised_control,
                    "attach_mode": args.attach_mode,
                }
            )
        )
        return 0 if device.ios_path() else 1

    _configure_logging(runtime_root)
    context = DriverContext(not args.supervised_control, modules, attach_mode=args.attach_mode)

    def health_loop() -> None:
        while True:
            result = context.ensure_link()
            logging.info("health=%s", result)
            time.sleep(args.health_poll_seconds)

    if args.attach_mode == "warm":
        threading.Thread(target=health_loop, daemon=True, name="sirin-ios-health").start()
    server = ThreadingHTTPServer((_validate_bind_host("127.0.0.1"), args.port), _handler(context, args.port))
    server.daemon_threads = True
    logging.info(
        "Sirin iOS Driver listening on 127.0.0.1:%s acceptance_only=%s attach_mode=%s",
        args.port,
        context.acceptance_only,
        context.attach_mode,
    )
    server.serve_forever()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
