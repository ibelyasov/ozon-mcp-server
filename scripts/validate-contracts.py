#!/usr/bin/env python3
"""Validate the canonical schemas and contract examples."""

from __future__ import annotations

import copy
import json
import sys
from pathlib import Path
from typing import Any

from jsonschema import Draft202012Validator, FormatChecker


ROOT = Path(__file__).resolve().parents[1] / "contracts"
CHECKER = FormatChecker()


def load(relative: str | Path) -> Any:
    with (ROOT / relative).open(encoding="utf-8") as handle:
        return json.load(handle)


def schema_errors(schema: dict[str, Any], instance: Any) -> list[str]:
    validator = Draft202012Validator(schema, format_checker=CHECKER)
    return [error.message for error in sorted(validator.iter_errors(instance), key=lambda item: list(item.absolute_path))]


def price_list_errors(prices: list[dict[str, Any]], location: str) -> list[str]:
    kinds = [price["type"] for price in prices]
    errors = []
    if len(kinds) != len(set(kinds)):
        errors.append(f"{location}: duplicate price type")
    if "ozon_card" in kinds and kinds[0] != "ozon_card":
        errors.append(f"{location}: ozon_card price must be first")
    return errors


def semantic_errors(schema_name: str, instance: Any) -> list[str]:
    errors: list[str] = []
    if schema_name == "ozon_get_context.output.schema.json":
        expected = {"search", "search_refinements", "search_pagination", "card_prices", "products", "characteristics", "description", "variants", "offers", "review_text", "review_refinements", "review_pagination", "review_images", "product_images", "image_content", "region_verification", "account_observation"}
        names = [entry["name"] for entry in instance["data"]["capabilities"]]
        if len(names) != len(set(names)) or set(names) != expected:
            errors.append("capabilities must contain every canonical name exactly once")
    if schema_name == "ozon_search.input.schema.json":
        price_range = instance.get("start", {}).get("priceRange")
        if price_range and "minMinor" in price_range and "maxMinor" in price_range and price_range["minMinor"] > price_range["maxMinor"]:
            errors.append("priceRange.minMinor exceeds maxMinor")
    if schema_name == "ozon_search.output.schema.json":
        data = instance["data"]
        if data["coverage"]["returned"] != len(data["items"]):
            errors.append("coverage.returned does not equal items length")
        if data["coverage"]["uniqueSeen"] < data["coverage"]["returned"]:
            errors.append("coverage.uniqueSeen is less than returned")
        errors.extend(cursor_errors(data, "data"))
        for index, item in enumerate(data["items"]):
            errors.extend(price_list_errors(item["prices"], f"items[{index}].prices"))
    if schema_name == "ozon_get_reviews.output.schema.json":
        data = instance["data"]
        if data["coverage"]["returned"] != len(data["reviews"]):
            errors.append("coverage.returned does not equal reviews length")
        if data["coverage"]["uniqueSeen"] < data["coverage"]["returned"]:
            errors.append("coverage.uniqueSeen is less than returned")
        errors.extend(cursor_errors(data, "data"))
    if schema_name == "ozon_get_products.output.schema.json":
        for result_index, result in enumerate(instance["data"]["results"]):
            if result["status"] != "ok":
                continue
            product = result["product"]
            errors.extend(price_list_errors(product["prices"], f"results[{result_index}].product.prices"))
            for name in ("characteristics", "description", "variants", "offers", "images"):
                if name in product:
                    section = product[name]
                    errors.extend(cursor_errors(section, f"results[{result_index}].product.{name}"))
                    if section["truncated"] and section["hasNext"] is not True:
                        errors.append(f"results[{result_index}].product.{name}: truncated section lacks known continuation")
                    if name == "offers":
                        for offer_index, offer in enumerate(section["items"]):
                            errors.extend(price_list_errors(offer["prices"], f"offers[{offer_index}].prices"))
    return errors


def cursor_errors(value: dict[str, Any], location: str) -> list[str]:
    if value["hasNext"] is True and value["nextCursor"] is None:
        return [f"{location}: hasNext=true without nextCursor"]
    if value["hasNext"] is False and value["nextCursor"] is not None:
        return [f"{location}: hasNext=false with nextCursor"]
    return []


def pair_errors(case: dict[str, Any]) -> list[str]:
    if not case["inputSchema"].endswith("ozon_get_products.input.schema.json"):
        return []
    inputs = case["input"]["products"]
    results = case["output"]["data"]["results"]
    errors = []
    if len(inputs) != len(results):
        errors.append("product result count differs from selector count")
        return errors
    default_sections = ["characteristics", "offers"] if case["input"].get("view", "compact") == "full" else ["characteristics"]
    requested = set(case["input"].get("include", default_sections))
    section_names = {"characteristics", "description", "variants", "offers", "images"}
    for index, (selector, result) in enumerate(zip(inputs, results)):
        if result["requested"] != selector:
            errors.append(f"product result {index} does not preserve its selector")
        if "cursor" not in selector and result["status"] == "ok":
            returned = set(result["product"]) & section_names
            if returned != requested:
                errors.append(f"product result {index} returned sections {sorted(returned)}, expected {sorted(requested)}")
    return errors


def mutate(base: Any, dotted_path: str, value: Any) -> Any:
    result = copy.deepcopy(base)
    current = result
    parts = dotted_path.split(".")
    for part in parts[:-1]:
        current = current[int(part)] if isinstance(current, list) else current[part]
    last = parts[-1]
    if isinstance(current, list):
        current[int(last)] = value
    else:
        current[last] = value
    return result


def main() -> int:
    failures: list[str] = []
    format_samples = {"date-time": "2026-09-13T09:00:00Z", "uri": "https://www.ozon.ru/product/1/", "json-pointer": "/items/0/title"}
    for format_name, sample in format_samples.items():
        if format_name not in CHECKER.checkers:
            failures.append(f"format checker does not register {format_name}")
        elif not CHECKER.conforms(sample, format_name):
            failures.append(f"format checker does not support valid {format_name}")
    schema_paths = sorted((ROOT / "schemas").glob("*.schema.json"))
    schemas: dict[str, dict[str, Any]] = {}
    for path in schema_paths:
        schema = load(path.relative_to(ROOT))
        try:
            Draft202012Validator.check_schema(schema)
        except Exception as exc:
            failures.append(f"schema {path.name}: {exc}")
        schemas[path.name] = schema
        if schema.get("type") != "object":
            failures.append(f"schema {path.name}: root type is not object")
        for ref in collect_refs(schema):
            if not ref.startswith("#/"):
                failures.append(f"schema {path.name}: non-local ref {ref}")
                continue
            try:
                resolve_local_ref(schema, ref)
            except (KeyError, IndexError, TypeError, ValueError) as exc:
                failures.append(f"schema {path.name}: dangling ref {ref}: {exc}")

    positive_count = 0
    for path in sorted((ROOT / "examples/positive").glob("*.json")):
        case = load(path.relative_to(ROOT))
        if "inputSchema" in case:
            checks = ((case["inputSchema"], case["input"]), (case["outputSchema"], case["output"]))
            failures.extend(f"positive {path.name}: {message}" for message in pair_errors(case))
        else:
            checks = ((case["schema"], case["instance"]),)
        for schema_rel, instance in checks:
            schema_name = Path(schema_rel).name
            for message in schema_errors(schemas[schema_name], instance) + semantic_errors(schema_name, instance):
                failures.append(f"positive {path.name} against {schema_name}: {message}")
        positive_count += 1

    negative_count = 0
    for path in sorted((ROOT / "examples/negative").glob("*.json")):
        case = load(path.relative_to(ROOT))
        if "baseExample" in case:
            base = load(case["baseExample"])[case["target"]]
            instance = mutate(base, case["mutation"]["path"], case["mutation"]["value"])
        else:
            instance = case["instance"]
        schema_name = Path(case["schema"]).name
        structural = schema_errors(schemas[schema_name], instance)
        semantic = [] if structural else semantic_errors(schema_name, instance)
        actual = "schema" if structural else "semantic" if semantic else "valid"
        if actual != case["expectedInvalidAt"]:
            failures.append(f"negative {path.name}: expected {case['expectedInvalidAt']}, got {actual}")
        negative_count += 1

    if failures:
        print(f"FAIL: {len(failures)} contract validation issue(s)")
        for failure in failures:
            print(f"- {failure}")
        return 1
    print(f"PASS: {len(schema_paths)} schemas; {positive_count} positive cases; {negative_count} negative cases")
    print("PASS: schema syntax, resolved local refs, formats, root objects, and example-level contract checks")
    return 0


def collect_refs(value: Any) -> list[str]:
    if isinstance(value, dict):
        return ([value["$ref"]] if "$ref" in value else []) + [ref for child in value.values() for ref in collect_refs(child)]
    if isinstance(value, list):
        return [ref for child in value for ref in collect_refs(child)]
    return []


def resolve_local_ref(schema: Any, ref: str) -> Any:
    current = schema
    for encoded in ref[2:].split("/"):
        token = encoded.replace("~1", "/").replace("~0", "~")
        current = current[int(token)] if isinstance(current, list) else current[token]
    return current


if __name__ == "__main__":
    sys.exit(main())

