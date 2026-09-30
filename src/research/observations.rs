//! Typed marketplace observations become journal-owned references in one transaction.
use crate::{
    error::{Code, fail},
    ozon::model::*,
    research::evidence::{self, fact, observed_evidence},
    research::journal::JournalTxn,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub const MAX_SEEN_REVIEWS: usize = 30_000;
pub const REVIEW_SEEN_CHECKPOINT_INTERVAL: usize = 256;
pub const MAX_REVIEW_SEEN_CHECKPOINT_BYTES: usize = 2_500_000;
pub const MAX_REVIEW_NO_PROGRESS_PAGES: usize = 3;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchCriteria {
    pub query: Option<String>,
    pub price_min: Option<u64>,
    pub price_max: Option<u64>,
    refinement: Option<SearchRefinement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SearchRefinement {
    kind: String,
    label: String,
    group: Option<String>,
    selected: Option<bool>,
}

impl SearchCriteria {
    pub fn for_query(query: &str) -> Self {
        Self {
            query: Some(query.to_owned()),
            ..Self::default()
        }
    }

    pub fn summary(&self) -> String {
        let mut summary = format!(
            "Search: {}",
            self.query.as_deref().unwrap_or("observed refinement")
        );
        if self.price_min.is_some() || self.price_max.is_some() {
            summary.push_str(&format!(
                "; RUB minor-unit bounds: {}..{}",
                self.price_min
                    .map_or_else(|| "unbounded".into(), |v| v.to_string()),
                self.price_max
                    .map_or_else(|| "unbounded".into(), |v| v.to_string()),
            ));
        }
        if let Some(refinement) = &self.refinement {
            summary.push_str(&format!("; {}: ", refinement.kind));
            if let Some(group) = &refinement.group {
                summary.push_str(group);
                summary.push_str(" / ");
            }
            summary.push_str(&refinement.label);
            if let Some(selected) = refinement.selected {
                summary.push_str(if selected {
                    " (previously selected)"
                } else {
                    " (previously unselected)"
                });
            }
        }
        summary
    }
}

#[derive(Default)]
pub struct Normalized {
    pub data: Value,
    pub evidence: Vec<Value>,
    pub warnings: Vec<Value>,
    pub product_refs: Vec<String>,
}

pub fn warning(code: &str, message: &str, evidence_refs: &[&str]) -> Value {
    json!({"code":code,"message":message,"evidenceRefs":evidence_refs})
}
pub fn source_warnings(warnings: &[Warning]) -> Vec<Value> {
    warnings
        .iter()
        .map(|w| {
            let code = serde_json::to_value(w).expect("warning enum");
            warning(
                code.as_str().expect("warning text"),
                "The captured source reported a retrieval limitation.",
                &[],
            )
        })
        .collect()
}
pub fn item_error(selector: &Value, error: &anyhow::Error) -> Value {
    json!({"status":"error","requested":selector,"error":error_value(error)})
}
pub fn error_value(error: &anyhow::Error) -> Value {
    let code = crate::error::code(error);
    let code = if matches!(
        code,
        "INVALID_REFERENCE"
            | "CONTEXT_CHANGED"
            | "CONTEXT_UNVERIFIED"
            | "RESEARCH_EXPIRED"
            | "UNSUPPORTED_CAPABILITY"
            | "SOURCE_BLOCKED"
            | "SOURCE_CHANGED"
            | "UPSTREAM_TIMEOUT"
            | "NOT_FOUND"
            | "RESULT_TOO_LARGE"
            | "SERVER_BUSY"
    ) {
        code
    } else {
        "SOURCE_CHANGED"
    };
    json!({"code":code,"message":crate::error::safe_message(error),"retryable":matches!(code,"UPSTREAM_TIMEOUT"|"SERVER_BUSY")})
}

pub fn normalize_context(context: &Value, observed: &str) -> Normalized {
    const NAMES: [&str; 17] = [
        "search",
        "search_refinements",
        "search_pagination",
        "card_prices",
        "products",
        "characteristics",
        "description",
        "variants",
        "offers",
        "review_text",
        "review_refinements",
        "review_pagination",
        "review_images",
        "product_images",
        "image_content",
        "region_verification",
        "account_observation",
    ];
    let capabilities = NAMES
        .into_iter()
        .map(|name| {
            json!({
                "name": name,
                "status": if name=="offers" {
                    "unsupported"
                } else {
                    "supported"
                },
                "reason": if name=="offers" {
                    Some("Independent seller offers are unavailable from this public source.")
                } else {
                    None
                },
                "evidenceRefs": [

                ]
            })
        })
        .collect::<Vec<_>>();
    let warnings = if context["regionVerification"] == "verified" {
        vec![]
    } else {
        vec![warning(
            "REGION_UNVERIFIED",
            "The current region could not be verified.",
            &[],
        )]
    };
    Normalized {
        data: json!({
            "contextId": context[
                "contextId"
            ],
            "observedAt": observed,
            "region": {
                "label": context[
                    "regionLabel"
                ],
                "verification": context[
                    "regionVerification"
                ],
                "evidenceRefs": [

                ]
            },
            "accountState": context[
                "accountState"
            ],
            "accessState": context[
                "accessState"
            ],
            "capabilities": capabilities
        }),
        warnings,
        ..Default::default()
    }
}

fn provenance(raw: &Option<Acquisition>, fallback: &str) -> (String, String) {
    raw.as_ref()
        .map(|a| (a.source_url.clone(), a.observed_at.clone()))
        .unwrap_or_else(|| (fallback.into(), evidence::now()))
}
fn evidence_ref(evidence: &Value) -> &str {
    evidence["evidenceRef"]
        .as_str()
        .expect("generated evidence")
}
fn prices(
    card: Option<u64>,
    regular: Option<u64>,
    displayed: Option<u64>,
    label: Option<&str>,
) -> Vec<Value> {
    let mut result = vec![];
    if let Some(amount) = card {
        result.push(json!({"amountMinor":amount,"currency":"RUB","type":"ozon_card","condition":"Ozon Card payment"}));
    }
    if let Some(amount) = regular {
        result
            .push(json!({"amountMinor":amount,"currency":"RUB","type":"regular","condition":null}));
    }
    if result.is_empty() && displayed.is_some() {
        result.push(
            json!({"amountMinor":displayed,"currency":"RUB","type":"unknown","condition":label}),
        );
    }
    result
}
fn availability(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "available",
        Some(false) => "unavailable",
        None => "unknown",
    }
}
fn completeness(source: &SourceCoverage, has_next: Option<bool>, pending: bool) -> &'static str {
    if pending || has_next == Some(true) || (source.present && !source.complete()) {
        "partial"
    } else if source.complete() && has_next == Some(false) {
        "complete"
    } else {
        "unknown"
    }
}
pub struct SearchBatch<'a> {
    pub range: std::ops::Range<usize>,
    pub context_id: &'a str,
    pub unique_seen: usize,
    pub chain_complete: bool,
    pub include_facets: bool,
    pub next_cursor: Option<&'a str>,
    pub criteria: &'a SearchCriteria,
}

pub fn normalize_search(
    raw: &SearchResponse,
    txn: &mut JournalTxn<'_>,
    rid: &str,
    batch: SearchBatch<'_>,
) -> Result<Normalized> {
    let SearchBatch {
        range,
        context_id,
        unique_seen,
        chain_complete,
        include_facets,
        next_cursor,
        criteria,
    } = batch;
    if raw.items.len() > 120 || range.end > raw.items.len() {
        return Err(fail(
            Code::SourceChanged,
            "Search exceeds the captured-page contract",
        ));
    }
    let (source_url, observed) = provenance(&raw.acquisition, &raw.search_url);
    let mut result = Normalized::default();
    let mut rows = vec![];
    for item in raw.items.iter().take(range.end).skip(range.start) {
        let url = item.url.as_deref().and_then(evidence::canonical_source_url);
        let product_ref = txn.put_ref(rid, "product", &json!({"sku":item.sku,"url":url}), None)?;
        let card = if item.price_type == PriceType::OzonCard {
            item.price_minor
        } else {
            None
        };
        let values = prices(card, None, item.price_minor, item.price_label.as_deref());
        let mut facts = vec![
            fact(
                format!("{}/mainState", item.source_locator),
                json!(item.name),
            ),
            fact(
                format!("{}/mainState", item.source_locator),
                json!(item.rating),
            ),
            fact(
                format!("{}/mainState", item.source_locator),
                json!(item.reviews),
            ),
            fact(
                format!(
                    "{}/multiButton/ozonButton/addToCart/actionButton/title",
                    item.source_locator
                ),
                json!(item.delivery_label),
            ),
        ];
        if let Some(image) = &item.image {
            facts.push(fact(
                format!("{}/tileImage", item.source_locator),
                image.clone(),
            ));
        }
        for price in &values {
            facts.push(fact(
                format!("{}/mainState", item.source_locator),
                price["amountMinor"].clone(),
            ));
        }
        let ev = observed_evidence(&source_url, context_id, Some(&item.sku), facts, &observed)?;
        let eid = evidence_ref(&ev).to_owned();
        let mut image_refs = vec![];
        if let Some(image) = &item.image {
            image_refs.push(image_ref(
                txn,
                rid,
                image,
                "product",
                &product_ref,
                &source_url,
                &item.sku,
                &format!("{}/tileImage", item.source_locator),
                &observed,
                &eid,
            )?);
        }
        rows.push(json!({
            "productRef": product_ref,
            "sku": item.sku,
            "title": item.name,
            "url": url,
            "availability": "unknown",
            "prices": values,
            "matchesDisplayedPriceRange": item.matches_price_range,
            "seller": item.seller.as_ref().map(|name|json!({
                "name": name,
                "rating": null,
                "url": null
            })),
            "deliveryLabel": item.delivery_label,
            "rating": item.rating,
            "reviewCount": item.reviews,
            "imageRefs": image_refs,
            "evidenceRefs": [
                eid
            ]
        }));
        result.product_refs.push(product_ref);
        result.evidence.push(ev);
    }
    let refinements = if include_facets {
        search_refinements(raw, txn, rid, criteria)?
    } else {
        vec![]
    };
    let source_complete = chain_complete && raw.source.complete();
    let mut coverage_source = raw.source.clone();
    if !source_complete && coverage_source.present {
        coverage_source.source_truncated = true;
    }
    result.data = json!({
        "items": rows,
        "refinements": refinements,
        "refinementsIncluded": include_facets,
        "refinementsTruncated": false,
        "refinementsSourceTruncated": raw.refinements_source_truncated,
        "nextCursor": next_cursor,
        "hasNext": if next_cursor.is_some(){
            Some(true)
        }else{
            raw.has_next
        },
        "coverage": {
            "returned": range.len(),
            "uniqueSeen": unique_seen,
            "total": raw.total,
            "snapshotGuaranteed": false,
            "completeness": completeness(&coverage_source,raw.has_next,next_cursor.is_some())
        }
    });
    result.evidence.push(observed_evidence(
        &source_url,
        context_id,
        None,
        vec![
            fact("/sourceCoverage/present", raw.source.present),
            fact(
                "/sourceCoverage/malformedRows",
                json!(raw.source.malformed_rows),
            ),
            fact(
                "/sourceCoverage/sourceTruncated",
                raw.source.source_truncated,
            ),
            fact("/sourceCoverage/localTruncated", raw.source.local_truncated),
        ],
        &observed,
    )?);
    result.warnings = source_warnings(&raw.warnings);
    Ok(result)
}

fn search_refinements(
    raw: &SearchResponse,
    txn: &mut JournalTxn<'_>,
    rid: &str,
    criteria: &SearchCriteria,
) -> Result<Vec<Value>> {
    let mut result = vec![];
    if let Some(facets) = &raw.facets {
        for facet in &facets.items {
            if let Some(options) = &facet.options {
                for option in options {
                    if let (Some(label), Some(url)) = (&option.label, &option.search_url) {
                        push_search_refinement(
                            &mut result,
                            txn,
                            rid,
                            SearchRefinement {
                                kind: "filter".into(),
                                label: label.clone(),
                                group: facet.title.clone(),
                                selected: Some(option.selected),
                            },
                            url,
                            criteria,
                        )?;
                    }
                }
            } else if let (Some(label), Some(url)) = (&facet.title, &facet.search_url) {
                push_search_refinement(
                    &mut result,
                    txn,
                    rid,
                    SearchRefinement {
                        kind: "filter".into(),
                        label: label.clone(),
                        group: None,
                        selected: facet.selected,
                    },
                    url,
                    criteria,
                )?;
            }
        }
    }
    if let Some(options) = &raw.sort_options {
        for option in options {
            if let (Some(label), Some(url)) = (&option.label, &option.search_url) {
                push_search_refinement(
                    &mut result,
                    txn,
                    rid,
                    SearchRefinement {
                        kind: "sort".into(),
                        label: label.clone(),
                        group: None,
                        selected: Some(option.selected),
                    },
                    url,
                    criteria,
                )?;
            }
        }
    }
    Ok(result)
}
fn push_search_refinement(
    result: &mut Vec<Value>,
    txn: &mut JournalTxn<'_>,
    rid: &str,
    refinement: SearchRefinement,
    url: &str,
    criteria: &SearchCriteria,
) -> Result<()> {
    if refinement.label.is_empty() {
        return Ok(());
    }
    let mut criteria = criteria.clone();
    let bounds = url::Url::parse(url)
        .ok()
        .and_then(|url| crate::ozon::search::price_filter_bounds(&url));
    criteria.price_min = bounds.map(|(min, _)| min);
    criteria.price_max = bounds.map(|(_, max)| max);
    criteria.refinement = Some(refinement.clone());
    let search_ref = txn.put_ref(
        rid,
        "search_ref",
        &json!({"searchUrl":url,"criteria":criteria}),
        Some(1800),
    )?;
    result.push(json!({"kind":refinement.kind,"label":refinement.label,"groupLabel":refinement.group,"selected":refinement.selected,"searchRef":search_ref}));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn image_ref(
    txn: &mut JournalTxn<'_>,
    rid: &str,
    url: &str,
    kind: &str,
    source_ref: &str,
    source_url: &str,
    sku: &str,
    field_path: &str,
    observed: &str,
    eid: &str,
) -> Result<String> {
    txn.put_ref(
        rid,
        "image",
        &json!({
            "url": url,
            "sourceKind": kind,
            "sourceRef": source_ref,
            "sourceUrl": evidence::canonical_source_url(source_url),
            "sku": sku,
            "fieldPath": field_path,
            "observedAt": observed,
            "evidenceRef": eid
        }),
        None,
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProductContinuation {
    pub raw: ProductDetails,
    pub product_ref: String,
    pub section: String,
    pub offset: usize,
    pub observed_at: String,
    pub source_url: String,
}

pub fn normalize_product(
    raw: &ProductDetails,
    requested: &Value,
    txn: &mut JournalTxn<'_>,
    rid: &str,
    context_id: &str,
    include: &[String],
    continuation: Option<&ProductContinuation>,
) -> Result<Normalized> {
    let sku = raw
        .sku
        .as_deref()
        .ok_or_else(|| fail(Code::SourceChanged, "Product observation has no SKU"))?;
    let fallback = raw
        .url
        .clone()
        .unwrap_or_else(|| format!("https://www.ozon.ru/product/{sku}/"));
    let (source_url, observed) = continuation
        .map(|c| (c.source_url.clone(), c.observed_at.clone()))
        .unwrap_or_else(|| provenance(&raw.acquisition, &fallback));
    let product_ref = match continuation {
        Some(c) => c.product_ref.clone(),
        None => txn.put_ref(
            rid,
            "product",
            &json!({"sku":sku,"url":raw.url.as_deref().and_then(evidence::canonical_source_url)}),
            None,
        )?,
    };
    let ev = observed_evidence(
        &source_url,
        context_id,
        Some(sku),
        vec![
            fact("/title", json!(raw.name)),
            fact("/availability", json!(raw.available)),
            fact("/displayedPriceMinor", json!(raw.displayed_price_minor)),
            fact("/cardPriceMinor", json!(raw.card_price_minor)),
            fact("/regularPriceMinor", json!(raw.regular_price_minor)),
            fact("/deliveryLabel", json!(raw.delivery_label)),
            fact("/rating", json!(raw.rating)),
            fact("/reviewCount", json!(raw.reviews)),
        ],
        &observed,
    )?;
    let eid = evidence_ref(&ev).to_owned();
    let mut product = json!({
        "productRef": product_ref,
        "sku": sku,
        "title": raw.name,
        "url": raw.url.as_deref().and_then(evidence::canonical_source_url),
        "availability": availability(raw.available),
        "prices": prices(raw.card_price_minor,raw.regular_price_minor,raw.displayed_price_minor,None),
        "customsDuty": {
            "status": if raw.duty.is_some(){
                "available"
            }else{
                "unknown"
            },
            "amountMinor": raw.duty.as_ref().map(|d|d.amount_minor),
            "currency": "RUB",
            "label": raw.duty.as_ref().map(|d|d.note.as_str())
        },
        "seller": raw.seller.as_ref().map(|s|json!({
            "name": s.name,
            "rating": s.rating,
            "url": s.url.as_deref().and_then(evidence::canonical_source_url)
        })),
        "deliveryLabel": raw.delivery_label,
        "rating": raw.rating,
        "reviewCount": raw.reviews,
        "evidenceRefs": [
            eid
        ]
    });
    let mut result = Normalized {
        evidence: vec![ev],
        product_refs: vec![product_ref.clone()],
        warnings: source_warnings(&raw.warnings),
        ..Default::default()
    };
    if let Some(duty) = &raw.duty {
        let (url, time) = duty
            .acquisition
            .as_ref()
            .map(|a| (a.source_url.clone(), a.observed_at.clone()))
            .unwrap_or_else(|| (source_url.clone(), observed.clone()));
        let ev = observed_evidence(
            &url,
            context_id,
            Some(sku),
            vec![
                fact("/customsDuty/amountMinor", json!(duty.amount_minor)),
                fact("/customsDuty/label", duty.note.clone()),
            ],
            &time,
        )?;
        product["evidenceRefs"]
            .as_array_mut()
            .expect("product evidence")
            .push(json!(evidence_ref(&ev)));
        result.evidence.push(ev);
    }
    if let Some(supplement) = &raw.supplement {
        let ev = observed_evidence(
            &supplement.source_url,
            context_id,
            Some(sku),
            vec![
                fact(
                    "/supplement/status",
                    serde_json::to_value(&supplement.status)?,
                ),
                fact(
                    "/supplement/error/code",
                    supplement
                        .error
                        .as_ref()
                        .map(|e| serde_json::to_value(&e.code))
                        .transpose()?
                        .unwrap_or(Value::Null),
                ),
            ],
            &supplement.observed_at,
        )?;
        product["evidenceRefs"]
            .as_array_mut()
            .expect("product evidence")
            .push(json!(evidence_ref(&ev)));
        result.evidence.push(ev);
    }
    for section in include {
        let offset = continuation
            .filter(|c| c.section == *section)
            .map_or(0, |c| c.offset);
        let (mut data, next_offset) = match section.as_str() {
            "characteristics" => {
                let end = (offset + 50).min(raw.characteristics.len());
                let mut items = vec![];
                for item in raw.characteristics.iter().take(end).skip(offset) {
                    let ev = observed_evidence(
                        &source_url,
                        context_id,
                        Some(sku),
                        vec![
                            fact(format!("{}/title", item.source_locator), item.label.clone()),
                            fact(item.source_locator.clone(), item.value.clone()),
                        ],
                        &observed,
                    )?;
                    let eid = evidence_ref(&ev).to_owned();
                    result.evidence.push(ev);
                    items.push(json!({"name":item.label,"value":item.value,"evidenceRefs":[eid]}));
                }
                let status = if !raw.characteristics_complete {
                    if raw.characteristics.is_empty() {
                        "unknown"
                    } else {
                        "partial"
                    }
                } else if end < raw.characteristics.len() {
                    "partial"
                } else {
                    "available"
                };
                (
                    json!({
                        "status": status,
                        "items": items,
                        "truncated": end<raw.characteristics.len(),
                        "hasNext": if end<raw.characteristics.len(){
                            Some(true)
                        }else if raw.characteristics_complete{
                            Some(false)
                        }else{
                            None
                        },
                        "nextCursor": null
                    }),
                    (end < raw.characteristics.len()).then_some(end),
                )
            }
            "description" => {
                let has_text = raw.description.has_text();
                let chars = raw.description.text.chars().collect::<Vec<_>>();
                let end = (offset + 12000).min(chars.len());
                let text = chars[offset.min(end)..end].iter().collect::<String>();
                let (description_url, description_time) =
                    provenance(&raw.description.acquisition, &source_url);
                let ev = observed_evidence(
                    &description_url,
                    context_id,
                    Some(sku),
                    vec![fact(
                        format!("/description/text/{offset}"),
                        json!(has_text.then_some(&text)),
                    )],
                    &description_time,
                )?;
                let eid = evidence_ref(&ev).to_owned();
                result.evidence.push(ev);
                let status = if !has_text {
                    "unknown"
                } else if end < chars.len() {
                    "partial"
                } else {
                    status(raw.description.source.status())
                };
                (
                    json!({
                        "status": status,
                        "text": has_text.then_some(text),
                        "evidenceRefs": [
                            eid
                        ],
                        "truncated": end<chars.len()||!raw.description.source.complete(),
                        "hasNext": if !has_text{
                            None
                        }else if end<chars.len(){
                            Some(true)
                        }else if raw.description.source.complete(){
                            Some(false)
                        }else{
                            None
                        },
                        "nextCursor": null
                    }),
                    (end < chars.len()).then_some(end),
                )
            }
            "variants" => {
                let end = (offset + 20).min(raw.variants.items.len());
                let mut items = vec![];
                for (index, item) in raw.variants.items.iter().enumerate().take(end).skip(offset) {
                    let reference=txn.put_ref(rid,"product",&json!({"sku":item.sku,"url":item.url.as_deref().and_then(evidence::canonical_source_url)}),None)?;
                    let ev = observed_evidence(
                        &source_url,
                        context_id,
                        Some(sku),
                        vec![
                            fact(format!("/variants/items/{index}/sku"), item.sku.clone()),
                            fact(format!("/variants/items/{index}/title"), json!(item.title)),
                        ],
                        &observed,
                    )?;
                    let eid = evidence_ref(&ev).to_owned();
                    result.evidence.push(ev);
                    items.push(json!({"productRef":reference,"sku":item.sku,"title":item.title,"evidenceRefs":[eid]}));
                }
                (
                    json!({
                        "status": if end<raw.variants.items.len(){
                            "partial"
                        }else{
                            status(raw.variants.status.clone())
                        },
                        "items": items,
                        "truncated": end<raw.variants.items.len(),
                        "hasNext": if end<raw.variants.items.len(){
                            Some(true)
                        }else{
                            raw.variants.has_next
                        },
                        "nextCursor": null
                    }),
                    (end < raw.variants.items.len()).then_some(end),
                )
            }
            "images" => {
                let origins = product_image_origins(raw);
                let end = (offset + 12).min(origins.len());
                let mut items = vec![];
                for origin in origins.iter().take(end).skip(offset) {
                    let supplement = origin.section == "description_supplement";
                    let (url, time) = if supplement {
                        raw.supplement
                            .as_ref()
                            .map(|s| (s.source_url.as_str(), s.observed_at.as_str()))
                            .unwrap_or((source_url.as_str(), observed.as_str()))
                    } else {
                        (source_url.as_str(), observed.as_str())
                    };
                    let path = origin.source_locator.clone().unwrap_or_else(|| {
                        match origin.section.as_str() {
                            "description" | "description_supplement" => {
                                format!("/description/images/{}", origin.index)
                            }
                            section => format!("/{section}/{}", origin.index),
                        }
                    });
                    let ev = observed_evidence(
                        url,
                        context_id,
                        Some(sku),
                        vec![fact(path.clone(), origin.url.clone())],
                        time,
                    )?;
                    let eid = evidence_ref(&ev).to_owned();
                    result.evidence.push(ev);
                    let image_ref = image_ref(
                        txn,
                        rid,
                        &origin.url,
                        "product",
                        &product_ref,
                        url,
                        sku,
                        &path,
                        time,
                        &eid,
                    )?;
                    items.push(
                        json!({"imageRef":image_ref,"altText":raw.name,"evidenceRefs":[eid]}),
                    );
                }
                let incomplete =
                    !raw.gallery_source.complete() || !raw.description.source.complete();
                let known = raw.gallery_source.present || raw.description.source.present;
                (
                    json!({
                        "status": if end<origins.len()||incomplete {
                            if known{
                                "partial"
                            }else{
                                "unknown"
                            }
                        }else{
                            "available"
                        },
                        "items": items,
                        "truncated": end<origins.len()||incomplete,
                        "hasNext": if end<origins.len(){
                            Some(true)
                        }else if !incomplete{
                            Some(false)
                        }else{
                            None
                        },
                        "nextCursor": null
                    }),
                    (end < origins.len()).then_some(end),
                )
            }
            _ => return Err(fail(Code::InvalidArgument, "Unknown product section")),
        };
        if let Some(offset) = next_offset {
            let cursor = ProductContinuation {
                raw: raw.clone(),
                product_ref: product_ref.clone(),
                section: section.clone(),
                offset,
                observed_at: observed.clone(),
                source_url: source_url.clone(),
            };
            data["nextCursor"] = json!(txn.put_ref(
                rid,
                "product_cursor",
                &serde_json::to_value(cursor)?,
                Some(1800)
            )?);
        }
        product[section] = data;
    }
    result.data = json!({"status":"ok","requested":requested,"product":product});
    Ok(result)
}
fn product_image_origins(raw: &ProductDetails) -> Vec<SourceImage> {
    let mut seen = BTreeSet::new();
    raw.image_origins
        .iter()
        .filter(|origin| seen.insert(origin.url.clone()))
        .cloned()
        .collect()
}
fn status(value: SourceSectionStatus) -> &'static str {
    match value {
        SourceSectionStatus::Available => "available",
        SourceSectionStatus::Partial => "partial",
        SourceSectionStatus::Unknown => "unknown",
        SourceSectionStatus::Unsupported => "unsupported",
    }
}

/// Stable source IDs deduplicate globally. ID-less rows have a content fingerprint
/// plus their captured page and source position; equal text alone is not identity.
pub fn review_identity(review: &Review, source_url: &str, _index: usize) -> Result<String> {
    if let Some(id) = review.review_id.as_deref().filter(|s| !s.is_empty()) {
        return Ok(format!("id:{id}"));
    }
    let mut hash = Sha256::new();
    hash.update(source_url.as_bytes());
    hash.update(review.source_index.to_le_bytes());
    hash.update(review.source_locator.as_bytes());
    hash.update(serde_json::to_vec(review)?);
    Ok(format!("content:{:x}", hash.finalize()))
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewSeen {
    pub keys: Vec<String>,
    pub parent: Option<String>,
    pub depth: usize,
    pub total: usize,
}

pub fn load_review_seen(
    txn: &JournalTxn<'_>,
    rid: &str,
    context_id: &str,
    reference: Option<&str>,
) -> Result<BTreeSet<String>> {
    let mut result = BTreeSet::new();
    let mut current = reference.map(str::to_owned);
    let mut visited = BTreeSet::new();
    while let Some(reference) = current {
        if !visited.insert(reference.clone()) || visited.len() > REVIEW_SEEN_CHECKPOINT_INTERVAL + 1
        {
            return Err(fail(
                Code::InvalidReference,
                "Review identity chain is invalid",
            ));
        }
        let stored = txn.get_ref(&reference, "review_seen")?;
        if stored.research_id != rid {
            return Err(fail(
                Code::InvalidReference,
                "Foreign review identity chain",
            ));
        }
        if stored.context_id != context_id {
            return Err(fail(
                Code::ContextChanged,
                "Review identity chain context changed",
            ));
        }
        let seen: ReviewSeen = serde_json::from_value(stored.value)
            .map_err(|_| fail(Code::SourceChanged, "Invalid review identity checkpoint"))?;
        result.extend(seen.keys);
        if result.len() > MAX_SEEN_REVIEWS {
            return Err(fail(
                Code::ResultTooLarge,
                "Review identity chain limit reached",
            ));
        }
        current = seen.parent;
    }
    Ok(result)
}
pub fn save_review_seen(
    txn: &mut JournalTxn<'_>,
    rid: &str,
    prior: Option<&str>,
    prior_depth: usize,
    all: &BTreeSet<String>,
    new_keys: Vec<String>,
) -> Result<(Option<String>, usize)> {
    if new_keys.is_empty() {
        return Ok((prior.map(str::to_owned), prior_depth));
    }
    if all.len() > MAX_SEEN_REVIEWS {
        return Err(fail(
            Code::ResultTooLarge,
            "Review identity chain limit reached",
        ));
    }
    let checkpoint = prior_depth >= REVIEW_SEEN_CHECKPOINT_INTERVAL;
    let seen = ReviewSeen {
        keys: if checkpoint {
            all.iter().cloned().collect()
        } else {
            new_keys
        },
        parent: if checkpoint {
            None
        } else {
            prior.map(str::to_owned)
        },
        depth: if checkpoint { 0 } else { prior_depth + 1 },
        total: all.len(),
    };
    let value = serde_json::to_value(&seen)?;
    if serde_json::to_vec(&value)?.len() > MAX_REVIEW_SEEN_CHECKPOINT_BYTES {
        return Err(fail(
            Code::ResultTooLarge,
            "Review identity checkpoint exceeds its budget",
        ));
    }
    let reference = txn.put_ref(rid, "review_seen", &value, None)?;
    Ok((Some(reference), seen.depth))
}

pub struct ReviewBatch<'a> {
    pub indices: &'a [usize],
    pub context_id: &'a str,
    pub product_ref: &'a str,
    pub sku: &'a str,
    pub source_url: &'a str,
    pub unique_seen: usize,
    pub chain_complete: bool,
    pub include_facets: bool,
    pub next_cursor: Option<&'a str>,
}

pub fn normalize_reviews(
    raw: &ReviewPage,
    txn: &mut JournalTxn<'_>,
    rid: &str,
    batch: ReviewBatch<'_>,
) -> Result<Normalized> {
    let ReviewBatch {
        indices,
        context_id,
        product_ref,
        sku,
        source_url,
        unique_seen,
        chain_complete,
        include_facets,
        next_cursor,
    } = batch;
    if raw.reviews.len() > 30 {
        return Err(fail(
            Code::SourceChanged,
            "Review capture exceeds source limit",
        ));
    }
    let (source_url, observed) = provenance(&raw.acquisition, source_url);
    let mut result = Normalized {
        product_refs: vec![product_ref.to_owned()],
        warnings: source_warnings(&raw.warnings),
        ..Default::default()
    };
    let mut rows = vec![];
    for &index in indices {
        let item = raw
            .reviews
            .get(index)
            .ok_or_else(|| fail(Code::SourceChanged, "Review source index is invalid"))?;
        let identity = review_identity(item, &source_url, index)?;
        let review_ref = txn.put_ref(
            rid,
            "review",
            &json!({"sku":sku,"productRef":product_ref,"identity":identity}),
            None,
        )?;
        let text = review_text(item);
        let mut facts = vec![
            fact(
                format!("{}/content/comment", item.source_locator),
                json!(item.comment),
            ),
            fact(
                format!("{}/content/positive", item.source_locator),
                json!(item.pros),
            ),
            fact(
                format!("{}/content/negative", item.source_locator),
                json!(item.cons),
            ),
            fact(
                format!("{}/content/score", item.source_locator),
                json!(item.score),
            ),
            fact(item.source_locator.clone(), json!(item.date)),
            fact(item.source_locator.clone(), json!(item.variant_label)),
            fact(
                format!("{}/isItemPurchased", item.source_locator),
                json!(item.purchased),
            ),
        ];
        for origin in &item.photo_origins {
            let path = origin.source_locator.clone().unwrap_or_else(|| {
                format!("{}/content/photos/{}", item.source_locator, origin.index)
            });
            facts.push(fact(path, origin.url.clone()));
        }
        let ev = observed_evidence(&source_url, context_id, Some(sku), facts, &observed)?;
        let eid = evidence_ref(&ev).to_owned();
        let mut image_refs = vec![];
        for origin in item.photo_origins.iter().take(12) {
            let path = origin.source_locator.clone().unwrap_or_else(|| {
                format!("{}/content/photos/{}", item.source_locator, origin.index)
            });
            image_refs.push(image_ref(
                txn,
                rid,
                &origin.url,
                "review",
                &review_ref,
                &source_url,
                sku,
                &path,
                &observed,
                &eid,
            )?);
        }
        rows.push(json!({
            "reviewRef": review_ref,
            "rating": item.score.filter(|n|n.fract()==0.0&&*n>=1.0&&*n<=5.0).map(|n|n as u64),
            "publishedAt": item.date.as_deref().and_then(normalize_timestamp),
            "text": text,
            "variantLabel": item.variant_label,
            "purchaseVerified": item.purchased,
            "imageRefs": image_refs,
            "evidenceRefs": [
                eid
            ]
        }));
        result.evidence.push(ev);
    }
    result.evidence.push(observed_evidence(
        &source_url,
        context_id,
        Some(sku),
        vec![
            fact("/aggregate/rating", json!(raw.rating)),
            fact("/aggregate/count", json!(raw.total_reviews)),
            fact(
                "/aggregationScope",
                serde_json::to_value(&raw.aggregation_scope)?,
            ),
            fact("/sourceCoverage/present", raw.source.present),
            fact(
                "/sourceCoverage/malformedRows",
                json!(raw.source.malformed_rows),
            ),
            fact(
                "/sourceCoverage/sourceTruncated",
                raw.source.source_truncated,
            ),
            fact("/sourceCoverage/localTruncated", raw.source.local_truncated),
        ],
        &observed,
    )?);
    let mut refinements = vec![];
    if include_facets {
        for refinement in raw.refinements.iter().take(100) {
            let reference = txn.put_ref(
                rid,
                "review_search",
                &json!({"path":refinement.url,"sku":sku,"productRef":product_ref}),
                Some(1800),
            )?;
            refinements.push(json!({
                "kind": if refinement.kind.as_deref()==Some("sort"){
                    "sort"
                }else{
                    "filter"
                },
                "label": refinement.label,
                "groupLabel": null,
                "selected": refinement.selected,
                "reviewSearchRef": reference
            }));
        }
    }
    let mut coverage = raw.source.clone();
    if !chain_complete && coverage.present {
        coverage.source_truncated = true;
    }
    result.data = json!({
        "subjectSku": sku,
        "aggregationScope": raw.aggregation_scope,
        "aggregate": {
            "rating": raw.rating,
            "count": raw.total_reviews
        },
        "reviews": rows,
        "refinements": refinements,
        "refinementsIncluded": include_facets,
        "refinementsTruncated": include_facets&&raw.refinements.len()>100,
        "coverage": {
            "returned": indices.len(),
            "uniqueSeen": unique_seen,
            "total": raw.total_reviews,
            "snapshotGuaranteed": false,
            "completeness": completeness(&coverage,raw.has_next,next_cursor.is_some())
        },
        "nextCursor": next_cursor,
        "hasNext": if next_cursor.is_some(){
            Some(true)
        }else{
            raw.has_next
        }
    });
    Ok(result)
}
fn review_text(review: &Review) -> Option<String> {
    let mut segments = vec![];
    if let Some(text) = review.pros.as_deref().filter(|s| !s.is_empty()) {
        segments.push(format!("Достоинства: {text}"));
    }
    if let Some(text) = review.cons.as_deref().filter(|s| !s.is_empty()) {
        segments.push(format!("Недостатки: {text}"));
    }
    if let Some(text) = review.comment.as_deref().filter(|s| !s.is_empty()) {
        segments.push(text.to_owned());
    }
    (!segments.is_empty()).then(|| segments.join("\n"))
}
fn normalize_timestamp(input: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(input)
        .ok()
        .map(|t| t.to_rfc3339())
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(input, "%Y-%m-%d")
                .ok()
                .map(|d| format!("{d}T00:00:00Z"))
        })
}
