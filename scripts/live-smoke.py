#!/usr/bin/env python3
"""Optional v3 live acceptance using an isolated broker and private Chromium profile.

Never imported by the offline gate. Run manually with explicit executable paths.
No existing profile is accepted; retained artifacts include only safe projections.
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import queue
import signal
import socket
import stat
import subprocess
import tempfile
import threading
import time
import uuid
from concurrent.futures import ThreadPoolExecutor
from typing import Any

PROTOCOL_VERSION = "2025-11-25"
EXPECTED_TOOLS = {
    "ozon_get_context", "ozon_search", "ozon_get_products", "ozon_get_reviews",
    "ozon_get_images", "ozon_list_research", "ozon_get_research", "ozon_append_research_note",
}
# Service deadline is 55 seconds; five seconds allow transport/cleanup delivery.
CALL_TIMEOUT = 60.0
START_TIMEOUT = 20.0
OWNER_FILE = ".ozon-mcp-owner.json"
LIVE_UNAVAILABLE = {"SOURCE_BLOCKED", "UPSTREAM_TIMEOUT", "CONTEXT_UNVERIFIED", "CONTEXT_CHANGED", "NOT_FOUND", "UNSUPPORTED_CAPABILITY"}

class SmokeFailure(RuntimeError):
    pass

class ObservationUnavailable(RuntimeError):
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
        self.deadline = time.monotonic() + 600.0
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
        deadline = min(time.monotonic() + timeout, self.deadline)
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


class Contracts:
    """Reuse the canonical publisher's composition and semantic checks."""
    def __init__(self) -> None:
        path = Path(__file__).with_name("validate-contracts.py")
        spec = importlib.util.spec_from_file_location("ozon_contract_validation", path)
        assert spec is not None and spec.loader is not None
        self.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.module)
        self.schemas = {
            path.name: self.module.standalone(self.module.load(path.relative_to(self.module.ROOT)))
            for path in (self.module.ROOT / "schemas").glob("*.schema.json")
            if path.name != "common.schema.json"
        }

    def validate(self, name: str, value: Any) -> None:
        errors = self.module.schema_errors(self.schemas[name], value)
        if not errors:
            errors = self.module.semantic_errors(name, value)
        if errors:
            # Avoid including observed account data or URLs in failure messages.
            raise SmokeFailure(f"{name}: {len(errors)} contract violation(s)")

    def discovery(self, listed: dict[str, Any]) -> None:
        tools = listed.get("tools", [])
        if len(tools) != len(EXPECTED_TOOLS) or {tool.get("name") for tool in tools} != EXPECTED_TOOLS:
            raise SmokeFailure("tools/list advertised an unexpected tool set")
        for tool in tools:
            for field, kind in (("inputSchema", "input"), ("outputSchema", "output")):
                if tool.get(field) != self.schemas[f"{tool['name']}.{kind}.schema.json"]:
                    raise SmokeFailure(f"{tool['name']} published a different {field}")


def success(result: dict[str, Any], name: str, contracts: Contracts) -> dict[str, Any]:
    failure = failure_value(result)
    if failure is not None:
        contracts.validate("tool_failure.schema.json", failure)
        code = failure["error"]["code"]
        if code in LIVE_UNAVAILABLE:
            raise ObservationUnavailable(f"{name}: live observation unavailable ({code})")
        raise SmokeFailure(f"{name} failed with {code}")
    value = result.get("structuredContent")
    contracts.validate(f"{name}.output.schema.json", value)
    content = result.get("content", [])
    text_blocks = [block for block in content if block.get("type") == "text"]
    if len(text_blocks) != 1 or text_blocks[0].get("text") != "Complete result: use structuredContent.":
        raise SmokeFailure(f"{name} duplicated or omitted the lean structured-result marker")
    return value


def tool_call(client: McpClient, name: str, args: dict[str, Any], contracts: Contracts,
              report: dict[str, Any]) -> tuple[dict[str, Any], dict[str, Any]]:
    contracts.validate(f"{name}.input.schema.json", args)
    result, elapsed = client.call(name, args)
    report["timingsMs"].append({"client": client.label, "tool": name, "elapsed": elapsed})
    value = success(result, name, contracts)
    paired = contracts.module.pair_errors({"inputSchema": f"schemas/{name}.input.schema.json",
                                           "input": args, "output": value})
    if paired:
        raise SmokeFailure(f"{name}: input/output pairing violated the contract")
    report["calls"].append({"tool": name, "contextId": value["context"]["contextId"],
                            "researchId": value["researchId"],
                            "warningCodes": [warning["code"] for warning in value["warnings"]]})
    return result, value


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
    from PIL import Image
    content = result["content"]
    saved = []
    used_indexes: set[int] = set()
    for number, metadata in enumerate(value["data"]["results"], 1):
        if metadata["status"] != "ok":
            continue
        index = metadata["contentIndex"]
        if type(index) is not int or index < 0 or index >= len(content) or index in used_indexes:
            raise SmokeFailure("invalid or reused image contentIndex")
        used_indexes.add(index)
        block = content[index]
        if block.get("type") != "image" or block.get("mimeType") != metadata["mimeType"]:
            raise SmokeFailure("image contentIndex/MIME disagrees with metadata")
        raw = base64.b64decode(block["data"], validate=True)
        if not raw or len(raw) > 1024 * 1024:
            raise SmokeFailure("image payload exceeds the encoded byte budget")
        digest = hashlib.sha256(raw).hexdigest()
        if digest != metadata["sha256"]:
            raise SmokeFailure("image SHA256 disagrees with metadata")
        with Image.open(io.BytesIO(raw)) as image:
            expected = {"image/jpeg": "JPEG", "image/png": "PNG", "image/webp": "WEBP"}[metadata["mimeType"]]
            if image.format != expected or image.size != (metadata["width"], metadata["height"]):
                raise SmokeFailure("decoded image format/dimensions disagree with metadata")
            if not all(0 < edge <= 1536 for edge in image.size):
                raise SmokeFailure("image dimensions exceed the output budget")
            image.load()  # Decode all pixels, not just a potentially forged header.
        suffix = {"image/jpeg": ".jpg", "image/png": ".png", "image/webp": ".webp"}[metadata["mimeType"]]
        path = output / f"image-{number}{suffix}"
        with path.open("xb") as stream:
            stream.write(raw)
        saved.append({"file": path.name, "sourceKind": metadata["sourceKind"],
                      "width": metadata["width"], "height": metadata["height"], "sha256": digest})
    if used_indexes != {index for index, block in enumerate(content) if block.get("type") == "image"}:
        raise SmokeFailure("MCP image block has no successful metadata result")
    return saved


def stop_process(process: subprocess.Popen[bytes], timeout: float) -> None:
    if process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        process.kill()  # Only the subprocess object created by this script.
        process.wait(timeout=timeout)


def stop_broker(process: subprocess.Popen[bytes]) -> bool:
    if process.poll() is not None:
        return process.returncode == 0
    process.send_signal(signal.SIGINT)
    try:
        process.wait(timeout=15.0)
        return process.returncode == 0
    except subprocess.TimeoutExpired:
        stop_process(process, 5.0)
        return False


def wait_for_socket(path: Path, broker: subprocess.Popen[bytes]) -> None:
    deadline = time.monotonic() + START_TIMEOUT
    while time.monotonic() < deadline:
        if broker.poll() is not None:
            raise SmokeFailure(f"owned broker exited during startup ({broker.returncode})")
        try:
            metadata = path.lstat()
            if not stat.S_ISSOCK(metadata.st_mode) or stat.S_IMODE(metadata.st_mode) != 0o600 or metadata.st_uid != os.geteuid():
                raise SmokeFailure("broker socket is not private and owner-held")
            with socket.socket(socket.AF_UNIX) as connection:
                connection.settimeout(0.1)
                connection.connect(str(path))
            return
        except (FileNotFoundError, ConnectionRefusedError, socket.timeout):
            time.sleep(0.05)
    raise SmokeFailure("owned broker startup exceeded its deadline")


def ownership_record(profile: Path, executable: Path) -> dict[str, Any] | None:
    """Inspect the private record; Rust alone verifies native process identity."""
    path = profile / OWNER_FILE
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        return None
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.geteuid() or stat.S_IMODE(metadata.st_mode) != 0o600:
        raise SmokeFailure("browser ownership record is not an owner-only regular file")
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    try:
        opened = os.fstat(descriptor)
        if (opened.st_dev, opened.st_ino) != (metadata.st_dev, metadata.st_ino):
            raise SmokeFailure("browser ownership record changed during inspection")
        raw = os.read(descriptor, 16385)
    finally:
        os.close(descriptor)
    if len(raw) > 16384:
        raise SmokeFailure("browser ownership record exceeds its byte budget")
    record = json.loads(raw)
    identity = profile.stat()
    if record.get("version") != 3 or record.get("executable") != str(executable) or record.get("profile") != {
        "path": str(profile), "dev": identity.st_dev, "ino": identity.st_ino,
    }:
        raise SmokeFailure("browser ownership record disagrees with the disposable launch")
    return record


def journal_pages(clients: list[McpClient], research_id: str, section: str,
                  contracts: Contracts, report: dict[str, Any]) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    cursor = None
    seen: set[str] = set()
    for number in range(64):
        args: dict[str, Any] = {"researchId": research_id, "section": section, "limit": 2}
        if cursor:
            args["cursor"] = cursor
        _, value = tool_call(clients[number % 2], "ozon_get_research", args, contracts, report)
        rows.extend(value["data"]["payload"])
        cursor = value["data"]["nextCursor"]
        if cursor is None:
            return rows
        if cursor in seen:
            raise SmokeFailure(f"journal {section} repeated a cursor")
        seen.add(cursor)
    raise SmokeFailure(f"journal {section} exceeds the 64-page smoke budget")


def write_report(report: dict[str, Any], output: Path) -> None:
    (output / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    lines = ["# Ozon MCP v3 optional live acceptance", "", f"Status: **{report['status']}**", "",
             "One disposable run; static support does not establish live availability or catalog completeness.",
             f"Calls completed: {len(report['calls'])}", f"Fully decoded images: {len(report['images'])}",
             f"Whole run: {report['wallSeconds']} seconds", "", "## Coverage", ""]
    lines += [f"- {name}: {'observed' if observed else 'unobserved'}" for name, observed in report["coverage"].items()]
    lines += [f"- Partial: {item}" for item in report["partials"]]
    lines += [f"- Failure: {item}" for item in report["failures"]]
    lines += ["", "## Timings", ""]
    lines += [f"- {item['client']} {item['tool']}: {item['elapsed']} ms" for item in report["timingsMs"]]
    (output / "report.md").write_text("\n".join(lines) + "\n")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True, help="explicit compiled v3 MCP executable")
    parser.add_argument("--browser-executable", type=Path, required=True, help="explicit Chromium executable")
    parser.add_argument("--data-dir", type=Path, help="new private disposable root; default: fresh /tmp directory")
    parser.add_argument("--output-dir", type=Path, required=True, help="new artifact directory; retained after cleanup")
    parser.add_argument("--query", action="append", dest="queries", help="repeat generic marketplace queries")
    parser.add_argument("--crash-restart", action="store_true", help="kill only the owned broker and test verified Chrome recovery")
    parser.add_argument("--total-timeout", type=float, default=600.0, help="whole observation budget in seconds (default: 600)")
    return parser.parse_args()


def executable(path: Path, label: str) -> Path:
    resolved = path.resolve(strict=True)
    if not resolved.is_file() or not os.access(resolved, os.X_OK):
        raise SmokeFailure(f"{label} must be an executable file")
    return resolved

def run_observations(clients: list[McpClient], contracts: Contracts, report: dict[str, Any],
                     queries: list[str], output: Path) -> tuple[str, str, str, dict[str, Any]]:
    with ThreadPoolExecutor(max_workers=2) as executor:
        pending = [executor.submit(client.call, "ozon_get_context", {}) for client in clients]
        replies = [future.result() for future in pending]
    contexts = []
    for client, (result, elapsed) in zip(clients, replies, strict=True):
        report["timingsMs"].append({"client": client.label, "tool": "ozon_get_context", "elapsed": elapsed})
        value = success(result, "ozon_get_context", contracts)
        report["calls"].append({"tool": "ozon_get_context", "contextId": value["data"]["contextId"]})
        contexts.append(value)
    context_id = contexts[0]["data"]["contextId"]
    if contexts[1]["data"]["contextId"] != context_id:
        raise SmokeFailure("two frontends observed different shared contexts")
    report["coverage"]["shared_context"] = True
    for name in ("region_verification", "account_observation"):
        report["coverage"][name] = all(
            value["data"]["region"]["verification"] == "verified" if name == "region_verification"
            else value["data"]["accountState"] != "unknown" for value in contexts
        )
    primary = None
    for number, query in enumerate(queries):
        _, page = tool_call(clients[number % 2], "ozon_search",
                            {"start": {"query": query, "priceRange": {"maxMinor": 10000000}},
                             "limit": 1, "includeFacets": True, "refinementLimit": 2}, contracts, report)
        if not page["data"]["items"]:
            report["partials"].append(f"query {number + 1}: no candidates observed")
            continue
        primary = primary or page
        report["coverage"]["search"] = True
        if page["data"]["refinementsTruncated"]:
            report["partials"].append(f"query {number + 1}: bounded refinements were truncated")
        cursor = page["data"]["nextCursor"]
        if cursor:
            args = {"start": {"cursor": cursor}, "researchId": page["researchId"], "limit": 1}
            _, continuation = tool_call(clients[(number + 1) % 2], "ozon_search", args, contracts, report)
            _, replay = tool_call(clients[number % 2], "ozon_search", args, contracts, report)
            if replay["data"] != continuation["data"]:
                raise SmokeFailure("captured search cursor replay changed its data")
            if continuation["data"]["coverage"]["uniqueSeen"] < page["data"]["coverage"]["uniqueSeen"]:
                raise SmokeFailure("search cumulative uniqueSeen decreased")
            report["coverage"]["search_pagination_replay"] = True
        refinements = page["data"]["refinements"]
        if refinements:
            tool_call(clients[1], "ozon_search", {"start": {"searchRef": refinements[0]["searchRef"]},
                      "researchId": page["researchId"], "limit": 1}, contracts, report)
            report["coverage"]["search_refinements"] = True
    if primary is None:
        raise ObservationUnavailable("no query produced a product candidate for downstream acceptance")
    research_id = primary["researchId"]
    first = primary["data"]["items"][0]
    selectors = [{"sku": first["sku"]}, {"productRef": first["productRef"]}, {"url": first["url"]}]
    _, products = tool_call(clients[0], "ozon_get_products", {"products": selectors,
                           "researchId": research_id, "include": ["characteristics", "description", "variants", "images"]}, contracts, report)
    good = [item["product"] for item in products["data"]["results"] if item["status"] == "ok"]
    if len(good) != len(selectors):
        report["partials"].append("one or more product selectors did not yield a product")
    for product in good:
        if product["sku"] != first["sku"]:
            raise SmokeFailure("product selector resolved to the wrong SKU")
        for section in ("characteristics", "description", "variants", "images"):
            value = product[section]
            report["coverage"][f"product_{section}"] |= value["status"] == "available"
            if value["status"] != "available":
                report["partials"].append(f"product section {section}: {value['status']}")
            cursor = value["nextCursor"]
            if cursor:
                tool_call(clients[1], "ozon_get_products", {"products": [{"cursor": cursor}],
                          "researchId": research_id}, contracts, report)
                report["coverage"]["product_section_pagination"] = True
    _, reviews = tool_call(clients[1], "ozon_get_reviews", {"start": {"productRef": first["productRef"]},
                          "researchId": research_id, "limit": 1, "includeFacets": True}, contracts, report)
    report["coverage"]["reviews"] = bool(reviews["data"]["reviews"])
    cursor = reviews["data"]["nextCursor"]
    if cursor:
        args = {"start": {"cursor": cursor}, "researchId": research_id, "limit": 1}
        _, page = tool_call(clients[0], "ozon_get_reviews", args, contracts, report)
        _, replay = tool_call(clients[1], "ozon_get_reviews", args, contracts, report)
        if page["data"] != replay["data"]:
            raise SmokeFailure("captured review cursor replay changed its data")
        if page["data"]["coverage"]["uniqueSeen"] < reviews["data"]["coverage"]["uniqueSeen"]:
            raise SmokeFailure("review cumulative uniqueSeen decreased")
        report["coverage"]["review_pagination_replay"] = True
    if reviews["data"]["refinements"]:
        tool_call(clients[0], "ozon_get_reviews", {"start": {"reviewSearchRef": reviews["data"]["refinements"][0]["reviewSearchRef"]},
                  "researchId": research_id, "limit": 1}, contracts, report)
        report["coverage"]["review_refinements"] = True
    image_refs = list(dict.fromkeys(collect_image_refs(products)[:1] + collect_image_refs(reviews)[:1]))
    if image_refs:
        result, value = tool_call(clients[0], "ozon_get_images", {"imageRefs": image_refs,
                                  "researchId": research_id}, contracts, report)
        report["images"] = save_images(result, value, output)
        for item in report["images"]:
            report["coverage"][f"{item['sourceKind']}_image_content"] = True
        if any(item["status"] == "error" for item in value["data"]["results"]):
            report["partials"].append("one or more observed image references could not be downloaded")
    _, listed = tool_call(clients[1], "ozon_list_research", {"limit": 50}, contracts, report)
    if research_id not in {item["researchId"] for item in listed["data"]["researches"]}:
        raise SmokeFailure("second frontend cannot see the first frontend research")
    tool_call(clients[1], "ozon_get_research", {"researchId": research_id, "section": "candidates",
              "productRefs": [first["productRef"]]}, contracts, report)
    note = {"researchId": research_id, "operationId": f"live-smoke-{uuid.uuid4()}", "kind": "assessment",
            "text": "Synthetic live-smoke assessment; not a product recommendation.", "productRefs": [first["productRef"]]}
    _, appended = tool_call(clients[0], "ozon_append_research_note", note, contracts, report)
    _, repeated = tool_call(clients[1], "ozon_append_research_note", note, contracts, report)
    note_id = appended["data"]["noteId"]
    if repeated["data"]["noteId"] != note_id:
        raise SmokeFailure("idempotent note retry changed noteId")
    changed = dict(note, text="Changed synthetic payload must conflict.")
    result, elapsed = clients[1].call("ozon_append_research_note", changed)
    report["timingsMs"].append({"client": clients[1].label, "tool": "note conflict", "elapsed": elapsed})
    failure = failure_value(result)
    contracts.validate("tool_failure.schema.json", failure)
    if failure["error"]["code"] != "CONFLICT":
        raise SmokeFailure("changed operationId payload did not return CONFLICT")
    _, selected = tool_call(clients[1], "ozon_get_research", {"researchId": research_id, "section": "notes",
                           "noteIds": [note_id]}, contracts, report)
    if len(selected["data"]["payload"]) != 1 or selected["data"]["payload"][0]["noteId"] != note_id:
        raise SmokeFailure("exact note selection did not preserve the requested note")
    _, before = tool_call(clients[0], "ozon_get_research", {"researchId": research_id}, contracts, report)
    report["coverage"]["cross_client_journal_notes"] = True
    live_contexts = {call["contextId"] for call in report["calls"]}
    if live_contexts != {context_id}:
        raise SmokeFailure("successful calls crossed shared contexts before restart")
    return research_id, note_id, note["operationId"], before["data"]["payload"]


def main() -> int:
    args = parse_args()
    try:
        import PIL.Image  # Full raster decoding is required, including optional live images.
        contracts = Contracts()
    except ImportError as error:
        raise SystemExit("Requires jsonschema[format]==4.25.1 and Pillow; use uv run --no-project --with 'jsonschema[format]==4.25.1' --with 'pillow==12.3.0' python scripts/live-smoke.py ...") from error
    binary = executable(args.binary, "--binary")
    chrome = executable(args.browser_executable, "--browser-executable")
    queries = args.queries or ["беспроводная мышь"]
    if any(not query.strip() or len(query) > 500 for query in queries):
        raise SystemExit("each --query must contain 1..500 non-whitespace characters")
    if not 60 <= args.total_timeout <= 3600:
        raise SystemExit("--total-timeout must be between 60 and 3600 seconds")
    output = args.output_dir.absolute()
    if output.exists() or output.is_symlink():
        raise SystemExit("--output-dir must be a new artifact directory")
    # Umask applies to every log, image and disposable journal created below.
    os.umask(0o077)
    if args.data_dir is None:
        root = Path(tempfile.mkdtemp(prefix="ozon-smoke-", dir="/tmp")).resolve()
    else:
        root = args.data_dir.absolute()
        if root.exists() or root.is_symlink():
            raise SystemExit("--data-dir must be a new disposable path")
        root.mkdir(mode=0o700, parents=False)
        root = root.resolve()
    profile = root / "browser-profile"
    output.mkdir(mode=0o700, parents=True)
    env = {key: value for key, value in os.environ.items() if not key.startswith("OZON_")}
    env.update(OZON_DATA_DIR=str(root), OZON_USER_DATA_DIR=str(profile),
               OZON_BROWSER_EXECUTABLE=str(chrome), OZON_HEADLESS="true", OZON_IMAGE_DOH_FALLBACK="off")
    started = time.monotonic()
    deadline = started + args.total_timeout
    coverage_names = ["shared_context", "region_verification", "account_observation", "search",
                      "search_pagination_replay", "search_refinements", "product_characteristics", "product_description",
                      "product_variants", "product_images", "product_section_pagination", "reviews",
                      "review_pagination_replay", "review_refinements", "product_image_content", "review_image_content",
                      "cross_client_journal_notes", "restart_continuity", "crash_browser_recovery"]
    report: dict[str, Any] = {"status": "FAIL", "binarySha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "dataDir": str(root), "startedAt": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "calls": [], "timingsMs": [], "partials": [], "failures": [], "images": [],
        "coverage": {name: False for name in coverage_names}, "offers": "unsupported"}
    clients: list[McpClient] = []
    broker = None
    handles = []

    def start_broker(label: str) -> subprocess.Popen[bytes]:
        handle = (output / f"{label}.stderr.log").open("xb")
        handles.append(handle)
        process = subprocess.Popen([str(binary), "--broker"], env=env, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.DEVNULL, stderr=handle)
        try:
            wait_for_socket(root / "broker.sock", process)
        except BaseException:
            stop_broker(process)
            raise
        return process

    try:
        broker = start_broker("broker")
        for number in range(2):
            client = McpClient(binary, env, output / f"client-{number + 1}.stderr.log", f"client-{number + 1}", broker)
            client.deadline = deadline
            clients.append(client)
            client.initialize()
            contracts.discovery(require_rpc_result(client.request("tools/list", {}), "tools/list"))
        research_id, note_id, operation_id, before = run_observations(clients, contracts, report, queries, output)
        old_record = ownership_record(profile, chrome)
        if args.crash_restart:
            if old_record is None or old_record.get("browser_endpoint") is None:
                raise ObservationUnavailable("crash acceptance could not observe a recorded owned browser endpoint")
            broker.kill()  # This Popen is the explicitly started disposable broker only.
            broker.wait(timeout=5.0)
        elif not stop_broker(broker) or ownership_record(profile, chrome) is not None:
            raise SmokeFailure("graceful broker shutdown did not release its owned Chrome")
        broker = start_broker("broker-restart")
        for client in clients:
            client.attach_broker(broker)
            contracts.discovery(require_rpc_result(client.request("tools/list", {}), "tools/list after restart"))
        if args.crash_restart:
            # BrowserSession::new does native identity verification and Browser.close.
            # No Python CDP implementation, private driver IPC, or recovered PID signals.
            try:
                tool_call(clients[0], "ozon_get_context", {}, contracts, report)
            except ObservationUnavailable as error:
                report["partials"].append(str(error))
            new_record = ownership_record(profile, chrome)
            if new_record is not None and new_record.get("generation") == old_record["generation"]:
                raise SmokeFailure("crash recovery retained the old browser ownership generation")
            if new_record is not None and new_record.get("browser_endpoint") == old_record["browser_endpoint"]:
                raise SmokeFailure("crash recovery retained the old browser endpoint")
            report["coverage"]["crash_browser_recovery"] = True
        _, summary_value = tool_call(clients[0], "ozon_get_research", {"researchId": research_id}, contracts, report)
        summary = summary_value["data"]["payload"]
        rows = {section: journal_pages(clients, research_id, section, contracts, report)
                for section in ("events", "evidence", "notes")}
        notes = [note for note in rows["notes"] if note["operationId"] == operation_id]
        if len(notes) != 1 or notes[0]["noteId"] != note_id:
            raise SmokeFailure("durable journal did not retain exactly one idempotent note")
        if sum(event["kind"] == "note_appended" for event in rows["events"]) != 1:
            raise SmokeFailure("durable journal did not retain exactly one note_appended event")
        for field, section in (("eventCount", "events"), ("evidenceCount", "evidence"), ("noteCount", "notes")):
            if summary[field] != len(rows[section]) or summary[field] != before[field]:
                raise SmokeFailure(f"durable {section} count changed across restart")
        if not rows["evidence"]:
            raise SmokeFailure("durable journal evidence is absent after live observations")
        _, selected = tool_call(clients[1], "ozon_get_research", {"researchId": research_id, "section": "notes", "noteIds": [note_id]}, contracts, report)
        if [note["noteId"] for note in selected["data"]["payload"]] != [note_id]:
            raise SmokeFailure("exact durable note selection changed after restart")
        report["coverage"]["restart_continuity"] = True
        report["restart"] = {"mode": "crash" if args.crash_restart else "graceful", "reconnectedClients": 2,
                             "noteIdStable": True, "countsStable": True}
    except ObservationUnavailable as error:
        report["partials"].append(str(error))
    except Exception as error:
        report["failures"].append(str(error) if isinstance(error, SmokeFailure) else type(error).__name__)
    finally:
        for client in clients:
            try:
                client.close()
            except Exception:
                report["failures"].append("owned frontend cleanup failed")
        try:
            if broker is not None and not stop_broker(broker):
                report["failures"].append("owned broker did not shut down gracefully")
            if ownership_record(profile, chrome) is not None:
                # Recovery delegates recorded-identity checking/Browser.close to Rust.
                broker = start_broker("broker-cleanup")
                cleanup = McpClient(binary, env, output / "cleanup-client.stderr.log", "cleanup", broker)
                try:
                    cleanup.initialize()
                    result, _ = cleanup.call("ozon_get_context", {})
                    try:
                        success(result, "ozon_get_context", contracts)
                    except ObservationUnavailable:
                        pass
                finally:
                    cleanup.close()
                    stop_broker(broker)
                if ownership_record(profile, chrome) is not None:
                    raise SmokeFailure("owned Chrome cleanup remains unknown; preserved private root for inspection")
        except Exception as error:
            report["failures"].append(str(error) if isinstance(error, SmokeFailure) else "owned Chrome cleanup failed")
        for handle in handles:
            handle.close()
        optional = {"crash_browser_recovery"} if not args.crash_restart else set()
        report["partials"].extend(f"{name}: not observed in this bounded run" for name, observed in report["coverage"].items()
                                  if not observed and name not in optional)
        report["partials"] = list(dict.fromkeys(report["partials"]))
        observed_tools = {call["tool"] for call in report["calls"]}
        report["toolsObserved"] = {name: name in observed_tools for name in sorted(EXPECTED_TOOLS)}
        report["status"] = "FAIL" if report["failures"] else "PARTIAL" if report["partials"] else "PASS"
        report["finishedAt"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        report["wallSeconds"] = round(time.monotonic() - started, 3)
        write_report(report, output)
    return {"PASS": 0, "PARTIAL": 2, "FAIL": 1}[report["status"]]


if __name__ == "__main__":
    raise SystemExit(main())
