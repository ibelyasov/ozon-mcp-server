"""Offline checks for product-section truncation semantics."""
from __future__ import annotations

import copy
import importlib.util
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "validate-contracts.py"
SPEC = importlib.util.spec_from_file_location("validate_contracts", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
validator = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(validator)


class ProductSectionTruncationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.output = validator.load("examples/positive/ozon_get_products.json")["output"]
        source = validator.load("schemas/ozon_get_products.output.schema.json")
        self.schema = validator.standalone(source)

    def with_description(self, *, truncated: bool, has_next: bool | None, next_cursor: str | None) -> dict:
        output = copy.deepcopy(self.output)
        output["data"]["results"][0]["product"]["description"] = {
            "status": "partial",
            "text": "captured source text",
            "evidenceRefs": ["ev-description"],
            "truncated": truncated,
            "hasNext": has_next,
            "nextCursor": next_cursor,
        }
        return output

    def errors(self, output: dict) -> list[str]:
        return validator.schema_errors(self.schema, output) + validator.semantic_errors(
            "ozon_get_products.output.schema.json", output
        )

    def test_source_incomplete_truncation_without_cursor_is_valid(self) -> None:
        output = self.with_description(truncated=True, has_next=None, next_cursor=None)
        self.assertEqual(self.errors(output), [])

    def test_local_page_truncation_with_cursor_is_valid(self) -> None:
        output = self.with_description(truncated=True, has_next=True, next_cursor="cursor-description-2")
        self.assertEqual(self.errors(output), [])

    def test_true_without_cursor_is_rejected(self) -> None:
        output = self.with_description(truncated=True, has_next=True, next_cursor=None)
        self.assertTrue(any("hasNext=true without nextCursor" in error for error in self.errors(output)))

    def test_false_with_cursor_is_rejected(self) -> None:
        output = self.with_description(truncated=False, has_next=False, next_cursor="cursor-description-2")
        self.assertTrue(any("hasNext=false with nextCursor" in error for error in self.errors(output)))

    def test_unknown_continuation_with_cursor_is_rejected(self) -> None:
        output = self.with_description(truncated=True, has_next=None, next_cursor="cursor-description-2")
        self.assertTrue(any("hasNext=null with nextCursor" in error for error in self.errors(output)))

    def test_truncated_section_cannot_claim_confirmed_end(self) -> None:
        output = self.with_description(truncated=True, has_next=False, next_cursor=None)
        self.assertTrue(any("truncated section cannot have hasNext=false" in error for error in self.errors(output)))

class ResearchDepthContractsTests(unittest.TestCase):
    def schema(self, name: str) -> dict:
        return validator.standalone(validator.load(f"schemas/{name}.schema.json"))

    def test_opt_in_bounds_and_strict_quantities(self) -> None:
        products = self.schema("ozon_get_products.input")
        self.assertFalse(validator.schema_errors(products, {"products": [{"sku": "123"}], "include": ["fullCharacteristics", "offers", "customsDuty"], "maxSourceReads": 32, "characteristicNames": ["Вес"]}))
        self.assertTrue(validator.schema_errors(products, {"products": [{"sku": "123"}], "maxSourceReads": 33}))
        self.assertTrue(validator.schema_errors(products, {"products": [{"cursor": "c"}], "characteristicNames": ["Вес"]}))
        reviews = self.schema("ozon_get_reviews.input")
        self.assertFalse(validator.schema_errors(reviews, {"start": {"productRef": "p"}, "textLimit": 80, "includeDetails": True, "maxSourceReads": 16}))
        self.assertTrue(validator.schema_errors(reviews, {"start": {"productRef": "p"}, "textLimit": 79}))
        compare = self.schema("ozon_compare_products.input")
        args = {"researchId": "r", "productRefs": ["p1", "p2"], "quantities": [{"productRef": "p1", "amount": 1, "unit": "г", "evidenceRefs": ["e"], "characteristicName": "Вес"}]}
        self.assertFalse(validator.schema_errors(compare, args))
        del args["quantities"][0]["characteristicName"]
        self.assertTrue(validator.schema_errors(compare, args))

    def test_review_fragment_requires_truthful_length(self) -> None:
        output = validator.load("examples/positive/ozon_get_reviews.json")["output"]
        row = output["data"]["reviews"][0]
        row.update(text="short", textTruncated=True, textLength=5)
        self.assertTrue(validator.semantic_errors("ozon_get_reviews.output.schema.json", output))
        row["textLength"] = 6
        self.assertFalse(validator.semantic_errors("ozon_get_reviews.output.schema.json", output))

    def test_expansion_selection_is_bounded(self) -> None:
        schema = self.schema("ozon_get_research.input")
        self.assertFalse(validator.schema_errors(schema, {"researchId": "r", "section": "reviews", "reviewRefs": ["x"], "textOffset": 0, "textLimit": 12000}))
        self.assertTrue(validator.schema_errors(schema, {"researchId": "r", "section": "products", "productRefs": [str(i) for i in range(9)]}))
        self.assertTrue(validator.schema_errors(schema, {"researchId": "r", "section": "reviews", "reviewRefs": ["x"], "cursor": "c"}))


if __name__ == "__main__":
    unittest.main()
