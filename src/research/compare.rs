//! Deterministic comparison of already validated, historical snapshots.
use anyhow::Result;
use serde_json::{Value, json};
use std::{cmp::Ordering, sync::OnceLock};

fn key(s: &str) -> String {
    s.trim().to_lowercase()
}

fn characteristics(candidate: &Value) -> Vec<&Value> {
    // Full observations may overlap the short section. Preserve source order.
    ["fullCharacteristics", "characteristics"]
        .iter()
        .flat_map(|section| {
            candidate[*section]["items"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .collect()
}

fn quantity_unit(unit: &str) -> Option<(&'static str, f64)> {
    match key(unit).as_str() {
        "g" | "г" => Some(("mass", 1.0)),
        "kg" | "кг" => Some(("mass", 1000.0)),
        "ml" | "мл" => Some(("volume", 1.0)),
        "l" | "л" => Some(("volume", 1000.0)),
        "pcs" | "pc" | "шт" | "шт." => Some(("pieces", 1.0)),
        _ => None,
    }
}

fn observed_quantity(value: &str) -> Option<(f64, &'static str)> {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let regex = PATTERN.get_or_init(|| {
        regex::Regex::new(
            r"(?i)^\s*([0-9]+(?:[.,][0-9]+)?)\s*(kg|кг|g|г|ml|мл|l|л|pcs|pc|шт\.?)\s*$",
        )
        .unwrap()
    });
    let caps = regex.captures(value)?;
    let amount = caps[1].replace(',', ".").parse::<f64>().ok()?;
    let (dimension, factor) = quantity_unit(&caps[2])?;
    let normalized = amount * factor;
    (normalized.is_finite() && normalized > 0.0).then_some((normalized, dimension))
}

fn unit_price(candidate: &Value, args: &Value, price: &Value) -> Value {
    let unknown = || json!({"status":"unknown","amountMinor":null,"unit":null,"basis":"unverified","characteristicName":null});
    let Some(quantity) = args["quantities"].as_array().and_then(|quantities| {
        let matches: Vec<_> = quantities
            .iter()
            .filter(|q| q["productRef"] == candidate["productRef"])
            .collect();
        (matches.len() == 1).then(|| matches[0])
    }) else {
        return unknown();
    };
    let Some(amount) = quantity["amount"]
        .as_f64()
        .filter(|n| n.is_finite() && *n > 0.0)
    else {
        return unknown();
    };
    let Some(name) = quantity["characteristicName"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
    else {
        return unknown();
    };
    let Some(unit) = quantity["unit"].as_str() else {
        return unknown();
    };
    let Some((dimension, factor)) = quantity_unit(unit) else {
        return unknown();
    };
    let normalized = amount * factor;
    if !normalized.is_finite() {
        return unknown();
    }
    let Some(refs) = quantity["evidenceRefs"].as_array() else {
        return unknown();
    };
    let chars = characteristics(candidate);
    let named: Vec<_> = chars
        .iter()
        .filter(|item| item["name"].as_str() == Some(name))
        .collect();
    let verified = !named.is_empty()
        && named.iter().all(|item| {
            item["value"]
                .as_str()
                .and_then(observed_quantity)
                .is_some_and(|(value, dim)| {
                    dim == dimension && (value - normalized).abs() <= value.max(normalized) * 1e-12
                })
        })
        && named.iter().any(|item| {
            item["evidenceRefs"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|id| id.is_string() && refs.contains(id)))
        });
    let Some(price) = price.as_f64().filter(|p| p.is_finite() && *p >= 0.0) else {
        return unknown();
    };
    let result = price / amount;
    if !verified || !result.is_finite() {
        return unknown();
    }
    json!({"status":"available","amountMinor":result,"unit":unit,"basis":"observed_characteristic","characteristicName":name})
}

fn selected_price<'a>(candidate: &'a Value, kind: &str) -> Option<&'a Value> {
    let source_kind = if kind == "displayed" { "unknown" } else { kind };
    let prices: Vec<_> = candidate["prices"]
        .as_array()?
        .iter()
        .filter(|p| p["type"] == source_kind)
        .collect();
    // Ambiguous observations do not authorize choosing a price.
    if prices.len() != 1 {
        return None;
    }
    let price = prices[0];
    price["amountMinor"].as_u64()?;
    price["currency"].as_str().filter(|s| !s.is_empty())?;
    Some(price)
}

fn numeric_order(a: &Value, b: &Value, field: &str, ascending: bool) -> Ordering {
    match (a[field].as_f64(), b[field].as_f64()) {
        (Some(a), Some(b)) => {
            if ascending {
                a.total_cmp(&b)
            } else {
                b.total_cmp(&a)
            }
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

pub fn compare(args: &Value, candidates: &[Value]) -> Result<Value> {
    let kind = args["priceKind"].as_str().unwrap_or("ozon_card");
    let selected_names = args["characteristicNames"].as_array();
    let requirements = args["requirements"].as_array();
    let mut price_basis: Option<(Value, Value)> = None;
    let mut coherent = true;
    let mut rows = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let chars = characteristics(candidate);
        let price = selected_price(candidate, kind);
        if let Some(price) = price {
            let basis = (price["currency"].clone(), price["condition"].clone());
            if let Some(existing) = &price_basis {
                coherent &= *existing == basis;
            } else {
                price_basis = Some(basis);
            }
        }
        let mut selected = Vec::new();
        let mut evidence = candidate["evidenceRefs"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for item in &chars {
            let Some(name) = item["name"].as_str() else {
                continue;
            };
            let Some(value) = item["value"].as_str() else {
                continue;
            };
            let selected_name = selected_names
                .is_some_and(|names| names.iter().any(|n| n.as_str().is_some_and(|n| n == name)));
            let required_name = requirements.is_some_and(|items| {
                items
                    .iter()
                    .any(|r| r["name"].as_str().is_some_and(|n| n == name))
            });
            let quantity_ref = args["quantities"].as_array().is_some_and(|items| {
                items
                    .iter()
                    .filter(|q| q["productRef"] == candidate["productRef"])
                    .any(|q| {
                        q["characteristicName"].as_str() == Some(name)
                            || q["evidenceRefs"]
                                .as_array()
                                .zip(item["evidenceRefs"].as_array())
                                .is_some_and(|(requested, observed)| {
                                    requested.iter().any(|r| observed.contains(r))
                                })
                    })
            });
            if selected_name {
                let entry = json!({"name":name,"value":value});
                if !selected.contains(&entry) {
                    selected.push(entry);
                }
            }
            if (selected_name || required_name || quantity_ref)
                && let Some(refs) = item["evidenceRefs"].as_array()
            {
                for id in refs {
                    if !evidence.contains(id) {
                        evidence.push(id.clone());
                    }
                }
            }
        }
        anyhow::ensure!(
            evidence.len() <= 100,
            "Too many evidence references for comparison row"
        );
        let checks: Vec<_> = requirements
            .into_iter()
            .flatten()
            .map(|req| {
                let name = req["name"].as_str().unwrap_or("");
                let expected = req["equals"].as_str().unwrap_or("");
                let mut values: Vec<&str> = chars
                    .iter()
                    .filter(|item| item["name"].as_str().is_some_and(|n| n == name))
                    .filter_map(|item| item["value"].as_str())
                    .collect();
                values.dedup_by(|a, b| a == b);
                let unique = values
                    .first()
                    .copied()
                    .filter(|v| values.iter().all(|other| other == v));
                let status = match unique {
                    Some(v) if v == expected => "matches",
                    Some(_) => "does_not_match",
                    None => "unknown",
                };
                json!({"name":name,"expected":expected,"actual":unique,"status":status})
            })
            .collect();
        let price_minor = price.map_or(Value::Null, |p| p["amountMinor"].clone());
        rows.push(json!({"productRef":candidate["productRef"],"sku":candidate["sku"],"title":candidate["title"],"observedAt":candidate["observedAt"],"priceMinor":price_minor,"currency":price.map_or(Value::Null, |p| p["currency"].clone()),"priceKind":kind,"rating":candidate["rating"],"reviewCount":candidate["reviewCount"],"characteristics":selected,"requirements":checks,"unitPrice":unit_price(candidate,args,&price_minor),"evidenceRefs":evidence}));
    }
    let sort = args["sortBy"].as_str().unwrap_or("input");
    match sort {
        "price" if coherent => rows.sort_by(|a, b| numeric_order(a, b, "priceMinor", true)),
        "rating" => rows.sort_by(|a, b| numeric_order(a, b, "rating", false)),
        "reviewCount" => rows.sort_by(|a, b| numeric_order(a, b, "reviewCount", false)),
        _ => {}
    }
    let warnings = if sort == "price" && !coherent {
        vec![
            json!({"code":"INCOMPARABLE_PRICES","message":"Prices with different currencies or conditions were not sorted","evidenceRefs":[]}),
        ]
    } else {
        vec![]
    };
    Ok(json!({"historical":true,"rows":rows,"warnings":warnings}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(reference: &str, price: Value, chars: Value) -> Value {
        json!({"productRef":reference,"sku":reference,"title":reference,"observedAt":"2026-01-01T00:00:00Z","contextId":"context","prices":price,"rating":null,"reviewCount":null,"evidenceRefs":["base"],"characteristics":{"items":chars}})
    }
    fn price(amount: u64, currency: &str, kind: &str, condition: Value) -> Value {
        json!({"amountMinor":amount,"currency":currency,"type":kind,"condition":condition})
    }
    #[test]
    fn requirements_are_explicit_and_unknown_remains_visible() {
        let c = candidate(
            "a",
            json!([]),
            json!([{"name":"Цвет","value":"Белый","evidenceRefs":["e"]},{"name":"Объем","value":"500 мл","evidenceRefs":["q"]}]),
        );
        let result = compare(&json!({"characteristicNames":["Цвет"],"requirements":[{"name":"Цвет","equals":"Белый"},{"name":"Цвет","equals":"черный"},{"name":"Материал","equals":"сталь"}]}), &[c]).unwrap();
        assert_eq!(
            result["rows"][0]["characteristics"],
            json!([{"name":"Цвет","value":"Белый"}])
        );
        assert_eq!(
            result["rows"][0]["requirements"],
            json!([
            {"name":"Цвет","expected":"Белый","actual":"Белый","status":"matches"},
            {"name":"Цвет","expected":"черный","actual":"Белый","status":"does_not_match"},
            {"name":"Материал","expected":"сталь","actual":null,"status":"unknown"}])
        );
    }
    #[test]
    fn quantities_need_exact_observed_value_and_matching_characteristic_evidence() {
        let c = candidate(
            "a",
            json!([price(12000, "RUB", "ozon_card", json!("Ozon Card payment"))]),
            json!([{"name":"Масса","value":"500 г","evidenceRefs":["mass"]}]),
        );
        let run = |amount, unit, refs| {
            compare(&json!({"quantities":[{"productRef":"a","amount":amount,"unit":unit,"characteristicName":"Масса","evidenceRefs":refs}]}), std::slice::from_ref(&c)).unwrap()["rows"][0]["unitPrice"].clone()
        };
        assert_eq!(
            run(0.5, "kg", json!(["mass"])),
            json!({"status":"available","amountMinor":24000.0,"unit":"kg","basis":"observed_characteristic","characteristicName":"Масса"})
        );
        for result in [
            run(1.0, "kg", json!(["mass"])),
            run(500.0, "ml", json!(["mass"])),
            run(500.0, "g", json!(["base"])),
        ] {
            assert_eq!(
                result,
                json!({"status":"unknown","amountMinor":null,"unit":null,"basis":"unverified","characteristicName":null})
            );
        }
        let wrong_name = compare(&json!({"quantities":[{"productRef":"a","amount":500,"unit":"g","characteristicName":"Белок","evidenceRefs":["mass"]}]}), &[c]).unwrap();
        assert_eq!(wrong_name["rows"][0]["unitPrice"]["status"], "unknown");
        assert!(observed_quantity("2 x 500 г").is_none());
    }
    #[test]
    fn explicit_prices_sort_stably_and_never_mix_conditions_or_currencies() {
        let a = candidate(
            "a",
            json!([price(100, "RUB", "regular", Value::Null)]),
            json!([]),
        );
        let b = candidate(
            "b",
            json!([price(50, "RUB", "ozon_card", json!("Ozon Card payment"))]),
            json!([]),
        );
        let c = candidate(
            "c",
            json!([price(25, "RUB", "regular", Value::Null)]),
            json!([]),
        );
        let out = compare(
            &json!({"priceKind":"regular","sortBy":"price"}),
            &[a.clone(), b.clone(), c],
        )
        .unwrap();
        assert_eq!(
            out["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["productRef"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["c", "a", "b"]
        );
        assert_eq!(out["rows"][2]["priceMinor"], Value::Null);
        let foreign = candidate(
            "d",
            json!([price(1, "USD", "regular", Value::Null)]),
            json!([]),
        );
        let out = compare(
            &json!({"priceKind":"regular","sortBy":"price"}),
            &[a, b, foreign],
        )
        .unwrap();
        assert_eq!(out["rows"][0]["productRef"], "a");
        assert_eq!(out["warnings"].as_array().unwrap().len(), 1);
        assert_eq!(
            compare(
                &json!({"priceKind":"displayed"}),
                &[candidate(
                    "x",
                    json!([price(1, "RUB", "regular", Value::Null)]),
                    json!([])
                )]
            )
            .unwrap()["rows"][0]["priceMinor"],
            Value::Null
        );
    }
    #[test]
    fn ambiguous_characteristic_and_price_are_unknown() {
        let c = candidate(
            "a",
            json!([
                price(1, "RUB", "regular", Value::Null),
                price(2, "RUB", "regular", Value::Null)
            ]),
            json!([{"name":"Color","value":"white"},{"name":"Color","value":"black"}]),
        );
        let result = compare(
            &json!({"priceKind":"regular","requirements":[{"name":"color","equals":"white"}]}),
            &[c],
        )
        .unwrap();
        assert_eq!(result["rows"][0]["priceMinor"], Value::Null);
        assert_eq!(result["rows"][0]["requirements"][0]["status"], "unknown");
    }
    #[test]
    fn exact_names_values_and_conflicting_quantities() {
        let mut c = candidate(
            "a",
            json!([price(1000, "RUB", "ozon_card", json!("Ozon Card payment"))]),
            json!([{"name":"Масса","value":"500 г","evidenceRefs":["mass"]},{"name":"Model","value":"ABC","evidenceRefs":["model"]}]),
        );
        c["fullCharacteristics"] =
            json!({"items":[{"name":"Масса","value":"1 кг","evidenceRefs":["full"]}]});
        let out = compare(&json!({"characteristicNames":["model"],"requirements":[{"name":"Model","equals":"abc"},{"name":"model","equals":"ABC"}],"quantities":[{"productRef":"a","amount":500,"unit":"g","characteristicName":"Масса","evidenceRefs":["mass"]}]}), &[c]).unwrap();
        assert_eq!(out["rows"][0]["characteristics"], json!([]));
        assert_eq!(
            out["rows"][0]["requirements"][0]["status"],
            "does_not_match"
        );
        assert_eq!(out["rows"][0]["requirements"][1]["status"], "unknown");
        assert_eq!(out["rows"][0]["unitPrice"]["status"], "unknown");
        assert!(
            out["rows"][0]["evidenceRefs"]
                .as_array()
                .unwrap()
                .contains(&json!("full"))
        );
    }
}
