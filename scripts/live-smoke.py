#!/usr/bin/env python3
"""Disposable two-client live smoke for the vNext Ozon MCP broker."""

from __future__ import annotations

import argparse
import base64
import hashlib
import ipaddress
import json
import os
import queue
import signal
import socket
import stat
import struct
import subprocess
import threading
import time
import uuid
from concurrent.futures import ThreadPoolExecutor
from urllib.parse import urlsplit
from pathlib import Path
from typing import Any

try:
    import jsonschema
except ImportError:
    jsonschema = None  # type: ignore[assignment]


PROTOCOL_VERSION = "2025-11-25"
EXPECTED_TOOLS = {
    "ozon_get_context",
    "ozon_search",
    "ozon_get_products",
    "ozon_get_reviews",
    "ozon_get_images",
    "ozon_list_research",
    "ozon_get_research",
    "ozon_append_research_note",
}
CALL_TIMEOUT = 90.0
START_TIMEOUT = 20.0


class SmokeFailure(RuntimeError):
    pass


class McpClient:
    def __init__(
        self,
        binary: Path,
        env: dict[str, str],
        stderr_path: Path,
        label: str,
        broker: subprocess.Popen[bytes],
    ):
        self.label = label
        self._broker = broker
        self._stderr = stderr_path.open("wb")
        self.process = subprocess.Popen(
            [str(binary)],
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self._stderr,
            bufsize=0,
        )
        self._messages: queue.Queue[dict[str, Any] | BaseException] = queue.Queue()
        self._pending: dict[int, dict[str, Any]] = {}
        self._next_id = 1
        self._reader = threading.Thread(target=self._read_loop, daemon=True)
        self._reader.start()

    def _read_loop(self) -> None:
        try:
            assert self.process.stdout is not None
            for raw in self.process.stdout:
                if not raw.strip():
                    continue
                self._messages.put(json.loads(raw))
            self._messages.put(EOFError(f"{self.label} stdout closed"))
        except BaseException as error:  # delivered to the waiting main thread
            self._messages.put(error)

    def _send(self, message: dict[str, Any]) -> None:
        if self._broker.poll() is not None:
            raise SmokeFailure(
                f"explicit broker exited with {self._broker.returncode}; refusing frontend auto-start"
            )
        if self.process.poll() is not None:
            raise SmokeFailure(f"{self.label} exited with {self.process.returncode}")
        assert self.process.stdin is not None
        payload = json.dumps(message, ensure_ascii=False, separators=(",", ":")).encode() + b"\n"
        self.process.stdin.write(payload)
        self.process.stdin.flush()

    def notify(self, method: str, params: dict[str, Any] | None = None) -> None:
        message: dict[str, Any] = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            message["params"] = params
        self._send(message)

    def request(self, method: str, params: dict[str, Any] | None = None, timeout: float = CALL_TIMEOUT) -> dict[str, Any]:
        request_id = self._next_id
        self._next_id += 1
        message: dict[str, Any] = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            message["params"] = params
        self._send(message)
        deadline = time.monotonic() + timeout
        while True:
            if request_id in self._pending:
                return self._pending.pop(request_id)
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SmokeFailure(f"{self.label} timed out waiting for {method}")
            try:
                received = self._messages.get(timeout=remaining)
            except queue.Empty as error:
                raise SmokeFailure(f"{self.label} timed out waiting for {method}") from error
            if isinstance(received, BaseException):
                raise SmokeFailure(str(received)) from received
            if self._broker.poll() is not None:
                raise SmokeFailure(
                    f"explicit broker exited with {self._broker.returncode} during {method}"
                )
            response_id = received.get("id")
            if isinstance(response_id, int):
                self._pending[response_id] = received

    def initialize(self) -> None:
        response = self.request(
            "initialize",
            {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "ozon-live-smoke", "version": "1"},
            },
            START_TIMEOUT,
        )
        require_rpc_result(response, f"{self.label} initialize")
        negotiated = response["result"].get("protocolVersion")
        if negotiated != PROTOCOL_VERSION:
            raise SmokeFailure(f"{self.label} negotiated unexpected protocol {negotiated!r}")
        self.notify("notifications/initialized")

    def call(self, name: str, arguments: dict[str, Any]) -> tuple[dict[str, Any], float]:
        started = time.monotonic()
        response = self.request("tools/call", {"name": name, "arguments": arguments})
        elapsed_ms = round((time.monotonic() - started) * 1000, 1)
        return require_rpc_result(response, name), elapsed_ms

    def attach_broker(self, broker: subprocess.Popen[bytes]) -> None:
        if broker.poll() is not None:
            raise SmokeFailure("cannot reconnect frontend to an exited explicit broker")
        self._broker = broker

    def close(self) -> None:
        if self.process.stdin is not None:
            try:
                self.process.stdin.close()
            except OSError:
                pass
        stop_process(self.process, 5.0)
        self._stderr.close()


def require_rpc_result(response: dict[str, Any], label: str) -> dict[str, Any]:
    if "error" in response:
        raise SmokeFailure(f"{label} JSON-RPC error: {safe_rpc_error(response['error'])}")
    result = response.get("result")
    if not isinstance(result, dict):
        raise SmokeFailure(f"{label} returned no result object")
    return result


def safe_rpc_error(error: Any) -> str:
    if not isinstance(error, dict):
        return "malformed JSON-RPC error"
    return f"code={error.get('code')}"


def failure_value(result: dict[str, Any]) -> dict[str, Any] | None:
    if result.get("isError") is not True:
        return None
    for block in result.get("content", []):
        if isinstance(block, dict) and block.get("type") == "text":
            try:
                value = json.loads(block.get("text", ""))
            except (TypeError, json.JSONDecodeError):
                continue
            if isinstance(value, dict) and isinstance(value.get("error"), dict):
                return value
    raise SmokeFailure("tool returned an unparseable error payload")


def require_success(
    result: dict[str, Any], name: str, schemas: Path, schema_errors: list[dict[str, Any]]
) -> dict[str, Any]:
    failure = failure_value(result)
    if failure is not None:
        code = failure["error"].get("code", "UNKNOWN")
        raise SmokeFailure(f"{name} failed with {code}")
    value = result.get("structuredContent")
    if not isinstance(value, dict):
        raise SmokeFailure(f"{name} omitted structuredContent")
    schema_path = schemas / f"{name}.output.schema.json"
    schema = json.loads(schema_path.read_text())
    validator = jsonschema.Draft202012Validator(schema, format_checker=jsonschema.FormatChecker())
    errors = sorted(validator.iter_errors(value), key=lambda item: list(item.absolute_path))
    if errors:
        rendered = [
            {
                "path": "/" + "/".join(map(str, item.absolute_path)),
                "validator": item.validator,
            }
            for item in errors[:20]
        ]
        schema_errors.append({"tool": name, "errors": rendered})
        raise SmokeFailure(f"{name} output failed schema validation")
    return value


def validate_failure(value: dict[str, Any], schemas: Path) -> None:
    schema = json.loads((schemas / "tool_failure.schema.json").read_text())
    jsonschema.Draft202012Validator(schema, format_checker=jsonschema.FormatChecker()).validate(value)


def tool_call(
    client: McpClient,
    name: str,
    arguments: dict[str, Any],
    schemas: Path,
    report: dict[str, Any],
) -> tuple[dict[str, Any], dict[str, Any]]:
    result, elapsed_ms = client.call(name, arguments)
    report["timingsMs"].append({"client": client.label, "tool": name, "elapsed": elapsed_ms})
    value = require_success(result, name, schemas, report["schemaErrors"])
    report["calls"].append(project(name, value))
    return result, value


def project(name: str, value: dict[str, Any]) -> dict[str, Any]:
    data = value.get("data", {})
    projection: dict[str, Any] = {
        "tool": name,
        "researchId": value.get("researchId"),
        "contextId": value.get("context", {}).get("contextId"),
        "warningCodes": [warning.get("code") for warning in value.get("warnings", [])],
        "evidenceCount": len(value.get("evidence", [])),
    }
    if name == "ozon_get_context":
        projection.update(
            {
                "accessState": data.get("accessState"),
                "accountState": data.get("accountState"),
                "regionVerification": data.get("region", {}).get("verification"),
                "capabilities": {
                    item.get("name"): item.get("status") for item in data.get("capabilities", [])
                },
            }
        )
    elif name == "ozon_search":
        items = data.get("items", [])
        projection.update(
            {
                "count": len(items),
                "skus": [item.get("sku") for item in items],
                "prices": [
                    [
                        {"amountMinor": price.get("amountMinor"), "type": price.get("type")}
                        for price in item.get("prices", [])
                    ]
                    for item in items
                ],
                "hasNext": data.get("hasNext"),
                "nextCursorPresent": bool(data.get("nextCursor")),
                "coverage": data.get("coverage"),
            }
        )
    elif name == "ozon_get_products":
        results = data.get("results", [])
        projection["results"] = [project_product_result(item) for item in results]
    elif name == "ozon_get_reviews":
        reviews = data.get("reviews", [])
        projection.update(
            {
                "subjectSku": data.get("subjectSku"),
                "reviewCount": len(reviews),
                "ratings": [review.get("rating") for review in reviews],
                "imageRefCount": sum(len(review.get("imageRefs", [])) for review in reviews),
                "hasNext": data.get("hasNext"),
                "coverage": data.get("coverage"),
            }
        )
    elif name == "ozon_get_images":
        projection["results"] = [
            {
                "status": item.get("status"),
                "mimeType": item.get("mimeType"),
                "width": item.get("width"),
                "height": item.get("height"),
                "sha256": item.get("sha256"),
                "contentIndex": item.get("contentIndex"),
                "errorCode": item.get("error", {}).get("code"),
            }
            for item in data.get("results", [])
        ]
    elif name == "ozon_list_research":
        projection["researchIds"] = [item.get("researchId") for item in data.get("researches", [])]
    elif name == "ozon_get_research":
        payload = data.get("payload")
        projection.update(
            {
                "section": data.get("section"),
                "payloadCount": len(payload) if isinstance(payload, list) else 1,
                "nextCursorPresent": bool(data.get("nextCursor")),
            }
        )
    elif name == "ozon_append_research_note":
        projection["noteId"] = data.get("noteId")
    return projection


def project_product_result(item: dict[str, Any]) -> dict[str, Any]:
    product = item.get("product", {})
    sections = {}
    for section in ("characteristics", "offers", "variants", "images", "description"):
        if isinstance(product.get(section), dict):
            value = product[section]
            sections[section] = {
                "status": value.get("status"),
                "count": len(value.get("items", [])) if isinstance(value.get("items"), list) else None,
                "truncated": value.get("truncated"),
            }
    return {
        "status": item.get("status"),
        "errorCode": item.get("error", {}).get("code"),
        "sku": product.get("sku"),
        "priceTypes": [price.get("type") for price in product.get("prices", [])],
        "sections": sections,
    }


def collect_image_refs(value: Any) -> list[str]:
    found: list[str] = []
    if isinstance(value, dict):
        for key, child in value.items():
            if key == "imageRef" and isinstance(child, str):
                found.append(child)
            elif key == "imageRefs" and isinstance(child, list):
                found.extend(item for item in child if isinstance(item, str))
            else:
                found.extend(collect_image_refs(child))
    elif isinstance(value, list):
        for child in value:
            found.extend(collect_image_refs(child))
    return list(dict.fromkeys(found))


def save_images(result: dict[str, Any], value: dict[str, Any], output: Path) -> list[dict[str, Any]]:
    saved = []
    content = result.get("content", [])
    for number, metadata in enumerate(value.get("data", {}).get("results", []), 1):
        if metadata.get("status") != "ok":
            continue
        index = metadata.get("contentIndex")
        if not isinstance(index, int) or index >= len(content):
            raise SmokeFailure("image contentIndex does not identify a returned content block")
        block = content[index]
        if not isinstance(block, dict) or block.get("type") != "image":
            raise SmokeFailure("image contentIndex does not point to an image block")
        encoded = block.get("data")
        if not isinstance(encoded, str):
            raise SmokeFailure("image block omitted base64 data")
        raw = base64.b64decode(encoded, validate=True)
        digest = hashlib.sha256(raw).hexdigest()
        if digest != metadata.get("sha256"):
            raise SmokeFailure("decoded image sha256 does not match metadata")
        mime = metadata.get("mimeType")
        if block.get("mimeType") != mime:
            raise SmokeFailure("image block MIME does not match metadata")
        width, height = image_dimensions(raw, mime)
        if (width, height) != (metadata.get("width"), metadata.get("height")):
            raise SmokeFailure("decoded image dimensions do not match metadata")
        suffix = {"image/jpeg": ".jpg", "image/png": ".png", "image/webp": ".webp"}[mime]
        path = output / f"image-{number}{suffix}"
        path.write_bytes(raw)
        saved.append({"file": path.name, "mimeType": mime, "width": width, "height": height, "sha256": digest})
    return saved


def image_dimensions(data: bytes, mime: str) -> tuple[int, int]:
    if mime == "image/png" and data.startswith(b"\x89PNG\r\n\x1a\n") and len(data) >= 24:
        return struct.unpack(">II", data[16:24])
    if mime == "image/jpeg" and data.startswith(b"\xff\xd8"):
        offset = 2
        while offset + 9 <= len(data):
            if data[offset] != 0xFF:
                offset += 1
                continue
            marker = data[offset + 1]
            offset += 2
            if marker in (0xD8, 0xD9) or 0xD0 <= marker <= 0xD7:
                continue
            if offset + 2 > len(data):
                break
            length = struct.unpack(">H", data[offset : offset + 2])[0]
            if marker in {0xC0, 0xC1, 0xC2, 0xC3, 0xC5, 0xC6, 0xC7, 0xC9, 0xCA, 0xCB, 0xCD, 0xCE, 0xCF}:
                return struct.unpack(">HH", data[offset + 3 : offset + 7])[::-1]
            offset += length
    if mime == "image/webp" and data.startswith(b"RIFF") and data[8:12] == b"WEBP":
        kind = data[12:16]
        if kind == b"VP8X" and len(data) >= 30:
            return (1 + int.from_bytes(data[24:27], "little"), 1 + int.from_bytes(data[27:30], "little"))
        if kind == b"VP8L" and len(data) >= 25:
            bits = int.from_bytes(data[21:25], "little")
            return (1 + (bits & 0x3FFF), 1 + ((bits >> 14) & 0x3FFF))
        if kind == b"VP8 " and len(data) >= 30 and data[23:26] == b"\x9d\x01\x2a":
            return (int.from_bytes(data[26:28], "little") & 0x3FFF, int.from_bytes(data[28:30], "little") & 0x3FFF)
    raise SmokeFailure(f"cannot parse dimensions for returned {mime} image")


def stop_process(process: subprocess.Popen[bytes], timeout: float) -> None:
    if process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=timeout)


def stop_broker(process: subprocess.Popen[bytes], timeout: float) -> None:
    if process.poll() is not None:
        return
    process.send_signal(signal.SIGINT)
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        process.terminate()
        try:
            process.wait(timeout=5.0)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5.0)


def read_private_cdp_marker(data_dir: Path) -> str:
    path = data_dir / "r/browser-owned"
    metadata = path.lstat()
    if not stat.S_ISREG(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode):
        raise SmokeFailure("crashed broker left an invalid browser ownership marker")
    if stat.S_IMODE(metadata.st_mode) != 0o600 or metadata.st_uid != os.geteuid():
        raise SmokeFailure("crashed broker ownership marker is not private")
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    try:
        opened = os.fstat(descriptor)
        if (opened.st_dev, opened.st_ino) != (metadata.st_dev, metadata.st_ino):
            raise SmokeFailure("crashed broker ownership marker changed during inspection")
        raw = os.read(descriptor, 2049)
    finally:
        os.close(descriptor)
    if len(raw) > 2048:
        raise SmokeFailure("crashed broker ownership marker is oversized")
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise SmokeFailure("crashed broker ownership marker is invalid") from error
    endpoint = value.get("cdp") if isinstance(value, dict) else None
    if not isinstance(endpoint, str):
        raise SmokeFailure("crashed broker marker omitted the captured CDP identity")
    parsed = urlsplit(endpoint)
    try:
        address = parsed.hostname and ipaddress.ip_address(parsed.hostname)
    except ValueError as error:
        raise SmokeFailure("captured CDP identity is not a literal IP address") from error
    if (
        parsed.scheme != "ws"
        or address is None
        or not address.is_loopback
        or parsed.port is None
        or not parsed.path.startswith("/devtools/browser/")
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
    ):
        raise SmokeFailure("captured CDP identity is outside the private loopback boundary")
    return endpoint


def cdp_endpoint_accepts(endpoint: str) -> bool:
    parsed = urlsplit(endpoint)
    assert parsed.hostname is not None and parsed.port is not None
    host = f"[{parsed.hostname}]" if ":" in parsed.hostname else parsed.hostname
    request = (
        f"GET {parsed.path} HTTP/1.1\r\n"
        f"Host: {host}:{parsed.port}\r\n"
        "Connection: Upgrade\r\n"
        "Upgrade: websocket\r\n"
        "Sec-WebSocket-Version: 13\r\n"
        "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    ).encode("ascii")
    try:
        with socket.create_connection((parsed.hostname, parsed.port), timeout=1.0) as connection:
            connection.settimeout(1.0)
            connection.sendall(request)
            response = connection.recv(128)
    except (OSError, TimeoutError):
        return False
    return response.startswith(b"HTTP/1.1 101")


def wait_for_socket(path: Path, broker: subprocess.Popen[bytes]) -> None:
    deadline = time.monotonic() + START_TIMEOUT
    while time.monotonic() < deadline:
        if broker.poll() is not None:
            raise SmokeFailure(f"explicit broker exited during startup with {broker.returncode}")
        try:
            mode = path.lstat().st_mode
        except FileNotFoundError:
            time.sleep(0.05)
            continue
        if stat.S_ISSOCK(mode) and stat.S_IMODE(mode) == 0o600:
            return
        raise SmokeFailure("broker path exists but is not a private 0600 Unix socket")
    raise SmokeFailure("explicit broker did not create its socket before the startup deadline")


def write_report(report: dict[str, Any], output: Path) -> None:
    output.mkdir(mode=0o700, parents=True, exist_ok=True)
    (output / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    lines = [
        "# Ozon MCP vNext live smoke",
        "",
        f"Status: **{report['status']}**",
        "",
        "This is one disposable live run. It does not establish sustained marketplace reliability.",
        "",
        f"- Calls completed: {len(report['calls'])}",
        f"- Schema errors: {len(report['schemaErrors'])}",
        f"- Saved images: {len(report['images'])}",
        f"- Profile fallback pool absent: {report.get('profilePoolAbsent')}",
    ]
    for item in report.get("partials", []):
        lines.append(f"- Partial: {item}")
    for item in report.get("failures", []):
        lines.append(f"- Failure: {item}")
    lines.extend(["", "## Timings", ""])
    lines.extend(
        f"- {item['client']} {item['tool']}: {item['elapsed']} ms" for item in report["timingsMs"]
    )
    (output / "report.md").write_text("\n".join(lines) + "\n")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--data-dir", type=Path, required=True)
    parser.add_argument("--driver", type=Path, required=True)
    parser.add_argument("--chrome", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument(
        "--query",
        action="append",
        dest="queries",
        help="repeat to smoke multiple generic marketplace queries",
    )
    parser.add_argument(
        "--crash-restart",
        action="store_true",
        help="SIGKILL the owned broker once and verify captured-CDP recovery",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if jsonschema is None:
        raise SystemExit(
            "jsonschema is required; run with `uv run --with 'jsonschema[format]==4.25.1' scripts/live-smoke.py ...`"
        )
    binary = args.binary.resolve(strict=True)
    driver = args.driver.resolve(strict=True)
    chrome = args.chrome.resolve(strict=True)
    data_dir = args.data_dir.absolute()
    output = args.output_dir.absolute()
    queries = args.queries or ["беспроводная мышь"]
    if any(not query.strip() or len(query) > 500 for query in queries):
        raise SystemExit("each --query must contain 1..500 non-whitespace characters")
    if data_dir.exists() or data_dir.is_symlink():
        raise SystemExit("--data-dir must be a new disposable path")
    data_dir.mkdir(mode=0o700, parents=False)
    if stat.S_IMODE(data_dir.stat().st_mode) != 0o700:
        raise SystemExit("--data-dir was not created with mode 0700")
    output.mkdir(mode=0o700, parents=True, exist_ok=True)
    schemas = Path(__file__).resolve().parents[1] / "contracts/schemas"
    env = os.environ.copy()
    env.update(
        {
            "OZON_DATA_DIR": str(data_dir),
            "OZON_USER_DATA_DIR": str(data_dir / "browser-profile"),
            "OZON_BROKER_SOCKET": str(data_dir / "broker.sock"),
            "OZON_AGENT_BROWSER_BIN": str(driver),
            "OZON_BROWSER_EXECUTABLE": str(chrome),
            "OZON_HEADLESS": "true",
        }
    )
    report: dict[str, Any] = {
        "status": "FAIL",
        "binarySha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "startedAt": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "calls": [],
        "timingsMs": [],
        "schemaErrors": [],
        "partials": [],
        "failures": [],
        "images": [],
    }
    broker_stderr = (output / "broker.stderr.log").open("wb")
    broker = subprocess.Popen(
        [str(binary), "--broker"],
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=broker_stderr,
    )
    clients: list[McpClient] = []
    try:
        wait_for_socket(data_dir / "broker.sock", broker)
        clients = [
            McpClient(binary, env, output / "client-1.stderr.log", "client-1", broker),
            McpClient(binary, env, output / "client-2.stderr.log", "client-2", broker),
        ]
        for client in clients:
            client.initialize()
            listed = require_rpc_result(client.request("tools/list", {}), f"{client.label} tools/list")
            names = {tool.get("name") for tool in listed.get("tools", [])}
            if names != EXPECTED_TOOLS:
                raise SmokeFailure(f"{client.label} advertised an unexpected tool set")

        with ThreadPoolExecutor(max_workers=2) as executor:
            context_futures = [
                executor.submit(client.call, "ozon_get_context", {}) for client in clients
            ]
            context_results = [future.result() for future in context_futures]
        contexts = []
        for client, (result, elapsed_ms) in zip(clients, context_results, strict=True):
            report["timingsMs"].append(
                {"client": client.label, "tool": "ozon_get_context", "elapsed": elapsed_ms}
            )
            value = require_success(
                result, "ozon_get_context", schemas, report["schemaErrors"]
            )
            report["calls"].append(project("ozon_get_context", value))
            contexts.append(value)
        context_one, context_two = contexts
        context_id = context_one["data"]["contextId"]
        if context_two["data"]["contextId"] != context_id:
            raise SmokeFailure("two frontends observed different broker contexts")

        primary_search: dict[str, Any] | None = None
        for query_number, query in enumerate(queries):
            _, search = tool_call(
                clients[query_number % 2],
                "ozon_search",
                {"start": {"query": query}, "limit": 3, "includeFacets": True},
                schemas,
                report,
            )
            items = search["data"]["items"]
            if not items:
                raise SmokeFailure(f"search query {query_number + 1} returned no candidates")
            if primary_search is None:
                primary_search = search
            cursor = search["data"].get("nextCursor")
            if cursor:
                tool_call(
                    clients[(query_number + 1) % 2],
                    "ozon_search",
                    {
                        "start": {"cursor": cursor},
                        "researchId": search["researchId"],
                        "limit": 2,
                    },
                    schemas,
                    report,
                )
            else:
                report["partials"].append(
                    f"search query {query_number + 1} returned no continuation cursor"
                )

        assert primary_search is not None
        search = primary_search
        research_id = search["researchId"]
        items = search["data"]["items"]

        first = items[0]
        sku = first["sku"]
        product_ref = first["productRef"]
        _, products = tool_call(
            clients[0],
            "ozon_get_products",
            {
                "products": [{"sku": sku}],
                "researchId": research_id,
                "include": ["characteristics", "offers", "variants", "images"],
            },
            schemas,
            report,
        )
        _, reviews = tool_call(
            clients[1],
            "ozon_get_reviews",
            {"start": {"productRef": product_ref}, "researchId": research_id, "limit": 3},
            schemas,
            report,
        )

        review_cursor = reviews["data"].get("nextCursor")
        if review_cursor:
            tool_call(clients[1], "ozon_get_reviews",
                      {"start": {"cursor": review_cursor}, "researchId": research_id, "limit": 3},
                      schemas, report)
        product_images = collect_image_refs(products)
        review_images = collect_image_refs(reviews)
        image_refs = list(dict.fromkeys(product_images[:1] + review_images[:1]))
        report["imageSourceCoverage"] = {"productRequested": bool(product_images), "reviewRequested": bool(review_images)}
        if image_refs:
            image_result, image_value = tool_call(
                clients[0],
                "ozon_get_images",
                {"imageRefs": image_refs, "researchId": research_id},
                schemas,
                report,
            )
            report["images"] = save_images(image_result, image_value, output)
            if not report["images"]:
                report["partials"].append("image references were returned but no image content was available")
        else:
            report["partials"].append("product/review image references were unsupported or unavailable")

        for section in ("summary", "events", "evidence"):
            tool_call(
                clients[0],
                "ozon_get_research",
                {"researchId": research_id, "section": section},
                schemas,
                report,
            )
        _, listed = tool_call(clients[1], "ozon_list_research", {"limit": 10}, schemas, report)
        if research_id not in {item["researchId"] for item in listed["data"]["researches"]}:
            raise SmokeFailure("second frontend did not see the first frontend research")

        operation_id = f"live-smoke-{uuid.uuid4()}"
        note = {
            "researchId": research_id,
            "operationId": operation_id,
            "kind": "assessment",
            "text": "Synthetic live-smoke assessment; not a product recommendation.",
            "productRefs": [product_ref],
        }
        _, appended = tool_call(clients[0], "ozon_append_research_note", note, schemas, report)
        _, repeated = tool_call(clients[1], "ozon_append_research_note", note, schemas, report)
        if appended["data"]["noteId"] != repeated["data"]["noteId"]:
            raise SmokeFailure("idempotent note retry returned a different noteId")
        changed = dict(note)
        changed["text"] = "Changed synthetic payload must conflict."
        conflict_result, elapsed = clients[1].call("ozon_append_research_note", changed)
        report["timingsMs"].append(
            {"client": clients[1].label, "tool": "ozon_append_research_note conflict", "elapsed": elapsed}
        )
        conflict = failure_value(conflict_result)
        if conflict is None:
            raise SmokeFailure("changed operationId payload did not fail")
        validate_failure(conflict, schemas)
        if conflict["error"].get("code") != "CONFLICT":
            raise SmokeFailure("changed operationId payload did not return CONFLICT")
        report["calls"].append({"tool": "ozon_append_research_note", "expectedErrorCode": "CONFLICT"})
        contexts = {entry.get("contextId") for entry in report["calls"] if entry.get("contextId")}
        if contexts != {context_id}:
            raise SmokeFailure("successful browser calls crossed broker contexts")

        # Exercise a real frontend reconnect and broker restart while retaining
        # the disposable data root. Journal reads are local and remain valid if
        # a later browser observation would produce a new context generation.
        old_cdp: str | None = None
        if args.crash_restart:
            broker.kill()
            broker.wait(timeout=5.0)
            old_cdp = read_private_cdp_marker(data_dir)
        else:
            stop_broker(broker, 15.0)
        broker_stderr.close()
        if not args.crash_restart and (data_dir / "r/browser-owned").exists():
            raise SmokeFailure("graceful broker shutdown left a browser ownership marker")
        broker_stderr = (output / "broker-restart.stderr.log").open("wb")
        broker = subprocess.Popen(
            [str(binary), "--broker"],
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=broker_stderr,
        )
        wait_for_socket(data_dir / "broker.sock", broker)
        for client in clients:
            client.attach_broker(broker)
            listed_tools = require_rpc_result(
                client.request("tools/list", {}), f"{client.label} tools/list"
            )
            if {tool.get("name") for tool in listed_tools.get("tools", [])} != EXPECTED_TOOLS:
                raise SmokeFailure(f"{client.label} advertised an unexpected tool set after restart")

        recovered_context_id: str | None = None
        if args.crash_restart:
            _, recovered_context = tool_call(
                clients[0], "ozon_get_context", {}, schemas, report
            )
            recovered_context_id = recovered_context["data"]["contextId"]
            assert old_cdp is not None
            new_cdp = read_private_cdp_marker(data_dir)
            if new_cdp == old_cdp:
                raise SmokeFailure("crash recovery retained the stale captured CDP identity")
            if cdp_endpoint_accepts(old_cdp):
                raise SmokeFailure("old captured CDP endpoint still accepts connections after recovery")

        _, summary_value = tool_call(
            clients[0],
            "ozon_get_research",
            {"researchId": research_id, "section": "summary"},
            schemas,
            report,
        )
        summary = summary_value["data"]["payload"]
        durable_payloads: dict[str, list[dict[str, Any]]] = {}
        for section_number, section in enumerate(("events", "evidence", "notes")):
            payload: list[dict[str, Any]] = []
            cursor: str | None = None
            for page_number in range(100):
                arguments = {"researchId": research_id, "section": section}
                if cursor is not None:
                    arguments["cursor"] = cursor
                _, page = tool_call(
                    clients[(section_number + page_number) % 2],
                    "ozon_get_research",
                    arguments,
                    schemas,
                    report,
                )
                payload.extend(page["data"]["payload"])
                cursor = page["data"].get("nextCursor")
                if cursor is None:
                    break
            else:
                raise SmokeFailure(f"durable {section} pagination exceeded 100 pages")
            durable_payloads[section] = payload
        events = durable_payloads["events"]
        evidence = durable_payloads["evidence"]
        notes = durable_payloads["notes"]
        matching_notes = [item for item in notes if item.get("operationId") == operation_id]
        if len(matching_notes) != 1 or matching_notes[0].get("noteId") != appended["data"]["noteId"]:
            raise SmokeFailure("durable journal did not preserve the idempotent noteId exactly once")
        if sum(event.get("kind") == "note_appended" for event in events) != 1:
            raise SmokeFailure("durable journal did not contain exactly one note_appended event")
        if not evidence or summary.get("evidenceCount") != len(evidence):
            raise SmokeFailure("durable journal evidence was absent or inconsistent after restart")
        if summary.get("eventCount") != len(events) or summary.get("noteCount") != len(notes):
            raise SmokeFailure("durable journal counts changed across broker restart")
        report["restart"] = {
            "mode": "crash" if args.crash_restart else "graceful",
            "reconnectedExistingClients": 2,
            "summaryEventCount": summary.get("eventCount"),
            "summaryEvidenceCount": summary.get("evidenceCount"),
            "noteIdStable": True,
            "noteAppendedEventCount": 1,
            "contextRotated": (
                recovered_context_id != context_id if recovered_context_id is not None else None
            ),
        }

        report["profilePoolAbsent"] = not (data_dir / "browser-profile/.ozon-mcp-profiles").exists()
        if not report["profilePoolAbsent"]:
            raise SmokeFailure("legacy fallback profile pool was created")
        report["status"] = "PARTIAL" if report["partials"] else "PASS"
    except BaseException as error:
        report["failures"].append(str(error)[:1000])
        report["status"] = "FAIL"
    finally:
        for client in clients:
            client.close()
        stop_broker(broker, 15.0)
        broker_stderr.close()
        if (data_dir / "r/browser-owned").exists():
            report["failures"].append("final graceful broker shutdown left a browser ownership marker")
            report["status"] = "FAIL"
        report["finishedAt"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        report.setdefault(
            "profilePoolAbsent", not (data_dir / "browser-profile/.ozon-mcp-profiles").exists()
        )
        write_report(report, output)
    return 0 if report["status"] == "PASS" else 2


if __name__ == "__main__":
    raise SystemExit(main())
