//! View-specific presentation of already journaled public envelopes.
use anyhow::{Result, bail};
use serde_json::{Map, Value};

const TOOLS: [&str; 3] = ["ozon_search", "ozon_get_products", "ozon_get_reviews"];

/// Applies a response view after normalization and evidence journaling.
///
/// Compact views reduce repeated transport fields. Evidence references remain
/// resolvable through `ozon_get_research`; the journaled envelope is not changed.
pub fn apply(tool: &str, mut value: Value, view: &str) -> Result<Value> {
    if !TOOLS.contains(&tool) {
        bail!("INVALID_ARGUMENT: unsupported presentation tool");
    }
    if !matches!(view, "compact" | "comparison" | "full") {
        bail!("INVALID_ARGUMENT: view must be compact, comparison, or full");
    }
    let root = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("SOURCE_CHANGED: response envelope is not an object"))?;
    root.insert("view".into(), Value::String(view.into()));
    root.insert(
        "evidenceDetail".into(),
        Value::String(if view == "full" { "inline" } else { "journal" }.into()),
    );
    if tool == "ozon_search" && !root.contains_key("repeatMode") {
        root.insert("repeatMode".into(), Value::String("full".into()));
    }
    if view == "full" {
        return Ok(value);
    }
    let evidence = root
        .get_mut("evidence")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| anyhow::anyhow!("SOURCE_CHANGED: response evidence is not an array"))?;
    evidence.clear();

    match tool {
        "ozon_search" => present_search(root, view)?,
        "ozon_get_products" => present_products(root, view)?,
        "ozon_get_reviews" => present_reviews(root, view)?,
        _ => unreachable!(),
    }
    Ok(value)
}

fn object_at<'a>(
    root: &'a mut Map<String, Value>,
    pointer: &str,
) -> Result<&'a mut Map<String, Value>> {
    let key = pointer
        .strip_prefix('/')
        .filter(|key| !key.is_empty() && !key.contains('/'))
        .ok_or_else(|| anyhow::anyhow!("SOURCE_CHANGED: invalid object path {pointer}"))?;
    root.get_mut(key)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("SOURCE_CHANGED: expected object at {pointer}"))
}

fn present_search(root: &mut Map<String, Value>, view: &str) -> Result<()> {
    let items = object_at(root, "/data")?
        .get_mut("items")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| anyhow::anyhow!("SOURCE_CHANGED: expected array at /data/items"))?;
    for item in items {
        let item = item
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("SOURCE_CHANGED: search item is not an object"))?;
        let changed = item
            .get("novelty")
            .and_then(|value| value.get("changedFields"))
            .and_then(Value::as_array)
            .map(|fields| {
                fields
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<std::collections::HashSet<_>>()
            })
            .unwrap_or_default();
        let delta = item.get("representation").and_then(Value::as_str) == Some("delta");
        if !delta || !changed.contains("imageRefs") {
            item.remove("imageRefs");
        }
        if view == "compact" {
            for key in ["url", "seller", "deliveryLabel"] {
                if !delta || !changed.contains(key) {
                    item.remove(key);
                }
            }
        }
        if let Some(prices) = item.get_mut("prices").and_then(Value::as_array_mut) {
            for price in prices {
                if let Some(price) = price.as_object_mut() {
                    price.remove("evidenceRefs");
                }
            }
        }
    }
    Ok(())
}

/// Applies opt-in repeat-result delta representation before view projection.
/// New rows remain complete; known prior rows retain identity, provenance,
/// decision-critical price/availability fields, and all observed changed fields.
pub fn apply_repeat_mode(value: &mut Value, mode: &str) -> Result<()> {
    if !matches!(mode, "full" | "delta") {
        bail!("INVALID_ARGUMENT: repeatMode must be full or delta");
    }
    let root = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("SOURCE_CHANGED: response envelope is not an object"))?;
    root.insert("repeatMode".into(), Value::String(mode.into()));
    if mode == "full" {
        return Ok(());
    }
    let items = object_at(root, "/data")?
        .get_mut("items")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| anyhow::anyhow!("SOURCE_CHANGED: expected array at /data/items"))?;
    for item in items {
        let object = item
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("SOURCE_CHANGED: search item is not an object"))?;
        let Some(novelty) = object.get("novelty").and_then(Value::as_object) else {
            continue;
        };
        let status = novelty.get("status").and_then(Value::as_str);
        if !matches!(status, Some("unchanged" | "changed")) {
            continue;
        }
        let baseline = novelty
            .get("previousProductRef")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("SOURCE_CHANGED: delta row has no previousProductRef"))?
            .to_owned();
        let changed = novelty
            .get("changedFields")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("SOURCE_CHANGED: delta row has no changedFields"))?
            .iter()
            .map(|field| {
                field
                    .as_str()
                    .filter(|field| {
                        !field.is_empty() && !field.contains('/') && !field.contains('~')
                    })
                    .ok_or_else(|| anyhow::anyhow!("SOURCE_CHANGED: invalid changed field"))
            })
            .map(|field| field.map(str::to_owned))
            .collect::<Result<Vec<_>>>()?;
        let mut keep = std::collections::HashSet::from([
            "productRef".to_owned(),
            "sku".to_owned(),
            "evidenceRefs".to_owned(),
            "novelty".to_owned(),
            "prices".to_owned(),
            "availability".to_owned(),
            "matchesDisplayedPriceRange".to_owned(),
        ]);
        keep.extend(changed);
        object.retain(|key, _| keep.contains(key.as_str()));
        object.insert("representation".into(), Value::String("delta".into()));
        object.insert("baselineProductRef".into(), Value::String(baseline));
    }
    Ok(())
}

fn present_products(root: &mut Map<String, Value>, view: &str) -> Result<()> {
    let results = object_at(root, "/data")?
        .get_mut("results")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| anyhow::anyhow!("SOURCE_CHANGED: expected array at /data/results"))?;
    for result in results {
        let Some(product) = result.get_mut("product").and_then(Value::as_object_mut) else {
            continue;
        };
        product.remove("fieldEvidence");
        if view == "compact" {
            product.remove("url");
            remove_nested_evidence_refs(product);
        }
    }
    Ok(())
}

fn remove_nested_evidence_refs(product: &mut Map<String, Value>) {
    for (key, value) in product.iter_mut() {
        if key != "evidenceRefs" {
            remove_evidence_refs(value);
        }
    }
}

fn remove_evidence_refs(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("evidenceRefs");
            for value in object.values_mut() {
                remove_evidence_refs(value);
            }
        }
        Value::Array(values) => values.iter_mut().for_each(remove_evidence_refs),
        _ => {}
    }
}

fn present_reviews(root: &mut Map<String, Value>, view: &str) -> Result<()> {
    if view != "compact" {
        return Ok(());
    }
    let reviews = object_at(root, "/data")?
        .get_mut("reviews")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| anyhow::anyhow!("SOURCE_CHANGED: expected array at /data/reviews"))?;
    for review in reviews {
        let review = review
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("SOURCE_CHANGED: review is not an object"))?;
        for identity in ["author", "authorName", "authorDisplayName", "userName"] {
            review.remove(identity);
        }
        if review
            .get("imageRefs")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        {
            review.remove("imageRefs");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn example(tool: &str) -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("contracts/examples/positive")
            .join(format!("{tool}.json"));
        serde_json::from_slice::<Value>(&std::fs::read(path).unwrap()).unwrap()["output"].clone()
    }

    fn validates(tool: &str, value: &Value) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("contracts/schemas")
            .join(format!("{tool}.output.schema.json"));
        let schema: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let validator = jsonschema::options()
            .should_validate_formats(true)
            .build(&schema)
            .unwrap();
        if let Err(error) = validator.validate(value) {
            panic!("{tool} output failed at {}: {error}", error.instance_path());
        }
    }

    fn is_valid(tool: &str, value: &Value) -> bool {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("contracts/schemas")
            .join(format!("{tool}.output.schema.json"));
        let schema: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        jsonschema::options()
            .should_validate_formats(true)
            .build(&schema)
            .unwrap()
            .is_valid(value)
    }

    #[test]
    fn full_adds_metadata_without_changing_the_journaled_envelope() {
        for tool in TOOLS {
            let original = example(tool);
            validates(tool, &original);
            let mut expected = original.clone();
            expected["view"] = json!("full");
            expected["evidenceDetail"] = json!("inline");
            if tool == "ozon_search" {
                expected["repeatMode"] = json!("full");
            }
            let full = apply(tool, original, "full").unwrap();
            assert_eq!(full, expected);
            validates(tool, &full);
        }
    }

    #[test]
    fn compact_search_keeps_decision_fields_and_safety_fields() {
        let original = example("ozon_search");
        let compact = apply("ozon_search", original.clone(), "compact").unwrap();
        assert_eq!(compact["context"], original["context"]);
        assert_eq!(compact["observedAt"], original["observedAt"]);
        assert_eq!(compact["warnings"], original["warnings"]);
        assert_eq!(compact["data"]["coverage"], original["data"]["coverage"]);
        let item = &compact["data"]["items"][0];
        for key in [
            "productRef",
            "sku",
            "title",
            "availability",
            "prices",
            "rating",
            "reviewCount",
            "matchesDisplayedPriceRange",
            "evidenceRefs",
        ] {
            assert!(item.get(key).is_some(), "missing {key}");
        }
        for key in ["url", "seller", "deliveryLabel", "imageRefs"] {
            assert!(item.get(key).is_none(), "retained {key}");
        }
        assert!(item["prices"][0].get("evidenceRefs").is_none());
        assert_eq!(compact["evidence"], json!([]));
        validates("ozon_search", &compact);
    }

    #[test]
    fn compact_products_preserves_requested_sections_but_deduplicates_evidence() {
        let original = example("ozon_get_products");
        let compact = apply("ozon_get_products", original.clone(), "compact").unwrap();
        assert_eq!(compact["context"], original["context"]);
        assert_eq!(compact["observedAt"], original["observedAt"]);
        assert_eq!(compact["warnings"], original["warnings"]);
        let before = &original["data"]["results"][0]["product"];
        let after = &compact["data"]["results"][0]["product"];
        for section in [
            "characteristics",
            "description",
            "variants",
            "offers",
            "images",
        ] {
            if before.get(section).is_some() {
                let mut expected = before[section].clone();
                remove_evidence_refs(&mut expected);
                assert_eq!(
                    after[section], expected,
                    "changed requested section {section}"
                );
            }
        }
        assert!(after.get("url").is_none());
        assert!(after.get("fieldEvidence").is_none());
        assert_eq!(after["evidenceRefs"], before["evidenceRefs"]);
        validates("ozon_get_products", &compact);
    }

    #[test]
    fn compact_reviews_preserves_text_rating_date_and_navigation() {
        let mut original = example("ozon_get_reviews");
        original["data"]["reviews"][0]["authorName"] = json!("Покупатель");
        let compact = apply("ozon_get_reviews", original.clone(), "compact").unwrap();
        let before = &original["data"]["reviews"][0];
        let after = &compact["data"]["reviews"][0];
        for key in ["text", "rating", "publishedAt", "evidenceRefs"] {
            assert_eq!(after[key], before[key], "changed review {key}");
        }
        for key in [
            "aggregationScope",
            "aggregate",
            "coverage",
            "nextCursor",
            "hasNext",
            "refinements",
        ] {
            assert_eq!(compact["data"][key], original["data"][key], "changed {key}");
        }
        assert!(after.get("authorName").is_none());
        assert!(after.get("imageRefs").is_none());
        validates("ozon_get_reviews", &compact);
    }

    #[test]
    fn compact_is_measurably_smaller_and_invalid_inputs_fail_closed() {
        for tool in TOOLS {
            let original = example(tool);
            let full = apply(tool, original.clone(), "full").unwrap();
            let compact = apply(tool, original, "compact").unwrap();
            assert!(
                serde_json::to_vec(&compact).unwrap().len()
                    < serde_json::to_vec(&full).unwrap().len()
            );
        }
        assert!(apply("ozon_search", json!([]), "compact").is_err());
        assert!(apply("ozon_search", json!({}), "tiny").is_err());
        assert!(apply("unknown", json!({}), "compact").is_err());
    }

    #[test]
    fn delta_keeps_all_rows_and_decision_fields_plus_changed_values() {
        let mut value = example("ozon_search");
        let original_count = value["data"]["items"].as_array().unwrap().len();
        let item = value["data"]["items"][0].as_object_mut().unwrap();
        item.insert(
            "novelty".into(),
            json!({
                "status":"changed",
                "previousProductRef":"product-old",
                "changedFields":["prices", "seller", "deliveryLabel"]
            }),
        );
        item.insert(
            "seller".into(),
            json!({"name":"Seller","rating":4.8,"url":null,"evidenceRefs":["ev-search"]}),
        );
        item.insert("deliveryLabel".into(), json!("tomorrow"));
        apply_repeat_mode(&mut value, "delta").unwrap();
        let compact = apply("ozon_search", value, "compact").unwrap();
        assert_eq!(
            compact["data"]["items"].as_array().unwrap().len(),
            original_count
        );
        let item = &compact["data"]["items"][0];
        assert_eq!(item["representation"], "delta");
        assert_eq!(item["baselineProductRef"], "product-old");
        assert_eq!(item["availability"], "unknown");
        assert!(item.get("prices").is_some());
        assert_eq!(item["seller"]["name"], "Seller");
        assert_eq!(item["deliveryLabel"], "tomorrow");
    }

    #[test]
    fn unchanged_delta_keeps_unknown_price_and_rejects_unknown_baseline() {
        let mut value = example("ozon_search");
        value["data"]["items"][0]["novelty"] = json!({
            "status":"unchanged", "previousProductRef":"product-old", "changedFields":[]
        });
        value["data"]["items"][0]["prices"][0]["type"] = json!("unknown");
        apply_repeat_mode(&mut value, "delta").unwrap();
        assert_eq!(value["data"]["items"][0]["prices"][0]["type"], "unknown");

        value["data"]["items"][0]["novelty"]["previousProductRef"] = Value::Null;
        assert!(apply_repeat_mode(&mut value, "delta").is_err());
    }

    #[test]
    fn delta_search_validates_in_all_views() {
        for view in ["compact", "comparison", "full"] {
            let mut value = example("ozon_search");
            value["data"]["items"][0]["novelty"] = json!({
                "status":"unchanged", "previousProductRef":"product-old", "changedFields":[]
            });
            apply_repeat_mode(&mut value, "delta").unwrap();
            let presented = apply("ozon_search", value, view).unwrap();
            validates("ozon_search", &presented);
        }
    }

    #[test]
    fn schemas_reject_cross_view_metadata_and_missing_required_fields() {
        let mut search = apply("ozon_search", example("ozon_search"), "compact").unwrap();
        search["evidenceDetail"] = json!("inline");
        assert!(!is_valid("ozon_search", &search));

        let mut products = apply(
            "ozon_get_products",
            example("ozon_get_products"),
            "comparison",
        )
        .unwrap();
        products["data"]["results"][0]["product"]
            .as_object_mut()
            .unwrap()
            .remove("url");
        assert!(!is_valid("ozon_get_products", &products));

        let mut reviews =
            apply("ozon_get_reviews", example("ozon_get_reviews"), "compact").unwrap();
        reviews["evidence"] = json!([{"unexpected":"inline"}]);
        assert!(!is_valid("ozon_get_reviews", &reviews));
    }

    #[test]
    #[ignore = "offline benchmark requires OZON_BENCHMARK_INPUT"]
    fn benchmark_saved_observations() {
        let path = std::env::var("OZON_BENCHMARK_INPUT")
            .expect("set OZON_BENCHMARK_INPUT to an exported observations JSON file");
        let export: Value =
            serde_json::from_slice(&std::fs::read(path).expect("read benchmark input"))
                .expect("parse benchmark input");
        let observations = export
            .get("observations")
            .and_then(Value::as_object)
            .expect("benchmark input must contain an observations object");
        let mut totals = std::collections::BTreeMap::<(&str, &str), (usize, usize)>::new();
        for envelope in observations.values() {
            let tool = if envelope.pointer("/data/items").is_some() {
                "ozon_search"
            } else if envelope.pointer("/data/results").is_some() {
                "ozon_get_products"
            } else if envelope.pointer("/data/reviews").is_some() {
                "ozon_get_reviews"
            } else {
                continue;
            };
            for view in ["compact", "comparison", "full"] {
                let presented = apply(tool, envelope.clone(), view).expect("apply benchmark view");
                let chars = serde_json::to_string(&presented)
                    .expect("serialize benchmark output")
                    .chars()
                    .count();
                let entry = totals.entry((tool, view)).or_default();
                entry.0 += 1;
                entry.1 += chars;
            }
        }
        assert!(
            !totals.is_empty(),
            "no recognized observations in benchmark input"
        );
        for ((tool, view), (count, chars)) in totals {
            println!("tool={tool} count={count} view={view} chars={chars}");
        }
    }
}
