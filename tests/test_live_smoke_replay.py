"""Offline checks for live-smoke captured-page replay equality."""
from __future__ import annotations

import copy
import importlib.util
from pathlib import Path
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "live-smoke.py"
SPEC = importlib.util.spec_from_file_location("live_smoke", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
live_smoke = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(live_smoke)


class ReplayDataMatchesTests(unittest.TestCase):
    def test_search_allows_only_known_references_to_be_reminted(self) -> None:
        captured = {
            "items": [{
                "productRef": "product-old", "sku": "123", "title": "Mouse",
                "price": {"amountMinor": 1200}, "evidenceRefs": ["evidence-old"],
                "imageRefs": ["image-old"],
            }, {"productRef": "product-2-old", "sku": "456", "title": "Keyboard",
                "evidenceRefs": [], "imageRefs": []}],
            "refinements": [{"label": "Brand", "searchRef": "search-old"}],
            "nextCursor": "cursor-old", "refinementsNextCursor": "facet-cursor-old",
            "coverage": {"returned": 1, "uniqueSeen": 4},
        }
        replay = copy.deepcopy(captured)
        replay["items"][0].update(productRef="product-new", evidenceRefs=["evidence-new"], imageRefs=["image-new"])
        replay["refinements"][0]["searchRef"] = "search-new"
        replay["nextCursor"] = "cursor-new"
        replay["refinementsNextCursor"] = "facet-cursor-new"

        self.assertTrue(live_smoke.replay_data_matches("search", captured, replay))
        self.assertEqual(captured["items"][0]["productRef"], "product-old")

        missing_evidence = copy.deepcopy(replay)
        missing_evidence["items"][0]["evidenceRefs"] = []
        self.assertFalse(live_smoke.replay_data_matches("search", captured, missing_evidence))
        missing_cursor = copy.deepcopy(replay)
        missing_cursor["nextCursor"] = None
        self.assertFalse(live_smoke.replay_data_matches("search", captured, missing_cursor))

        for path, value in (
            (("items", 0, "title"), "Changed title"),
            (("items", 0, "sku"), "999"),
            (("items", 0, "price", "amountMinor"), 1300),
            (("coverage", "uniqueSeen"), 5),
        ):
            changed = copy.deepcopy(replay)
            target = changed
            for key in path[:-1]:
                target = target[key]
            target[path[-1]] = value
            with self.subTest(path=path):
                self.assertFalse(live_smoke.replay_data_matches("search", captured, changed))

        reordered = copy.deepcopy(replay)
        reordered["items"].reverse()
        self.assertFalse(live_smoke.replay_data_matches("search", captured, reordered))

    def test_reviews_preserve_text_dates_coverage_and_order(self) -> None:
        captured = {
            "reviews": [
                {"reviewRef": "review-old", "text": "First review", "publishedAt": "2026-01-01", "rating": 5,
                 "evidenceRefs": ["evidence-old"], "imageRefs": ["image-old"]},
                {"text": "Second review", "publishedAt": "2026-01-02", "rating": 4,
                 "evidenceRefs": ["evidence-2-old"], "imageRefs": []},
            ],
            "refinements": [{"label": "Recent", "reviewSearchRef": "reviews-old"}],
            "aggregate": {"rating": 4.5, "count": 12},
            "nextCursor": "cursor-old", "refinementsNextCursor": "facet-cursor-old",
            "coverage": {"returned": 2, "uniqueSeen": 7},
        }
        replay = copy.deepcopy(captured)
        replay["reviews"][0].update(reviewRef="review-new", evidenceRefs=["evidence-new"], imageRefs=["image-new"])
        replay["reviews"][1]["evidenceRefs"] = ["evidence-2-new"]
        replay["refinements"][0]["reviewSearchRef"] = "reviews-new"
        replay["nextCursor"] = "cursor-new"
        replay["refinementsNextCursor"] = "facet-cursor-new"

        self.assertTrue(live_smoke.replay_data_matches("reviews", captured, replay))
        missing_images = copy.deepcopy(replay)
        missing_images["reviews"][0]["imageRefs"] = []
        self.assertFalse(live_smoke.replay_data_matches("reviews", captured, missing_images))
        for change in ("text", "publishedAt", "count", "coverage", "order"):
            changed = copy.deepcopy(replay)
            if change == "text":
                changed["reviews"][0]["text"] = "Edited review"
            elif change == "publishedAt":
                changed["reviews"][0]["publishedAt"] = "2026-01-03"
            elif change == "count":
                changed["aggregate"]["count"] += 1
            elif change == "coverage":
                changed["coverage"]["uniqueSeen"] += 1
            else:
                changed["reviews"].reverse()
            with self.subTest(change=change):
                self.assertFalse(live_smoke.replay_data_matches("reviews", captured, changed))

    def test_unknown_kind_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            live_smoke.replay_data_matches("products", {}, {})


if __name__ == "__main__":
    unittest.main()
