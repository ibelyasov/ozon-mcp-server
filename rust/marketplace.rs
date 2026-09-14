use crate::evidence::{canonical_source_url, now, observed_evidence};
use crate::store::Store;
use anyhow::{Result, anyhow};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use uuid::Uuid;

pub const MAX_SEEN_REVIEWS: usize = 30_000;
pub const REVIEW_SEEN_CHECKPOINT_INTERVAL: usize = 256;
pub const MAX_REVIEW_SEEN_CHECKPOINT_BYTES: usize = 2_500_000;
pub const MAX_REVIEW_NO_PROGRESS_PAGES: usize = 3;

pub struct Normalized {
    pub data: Value,
    pub evidence: Vec<Value>,
    pub warnings: Vec<Value>,
    pub product_refs: Vec<String>,
}

pub struct ProductNormalized {
    pub result: Value,
    pub evidence: Vec<Value>,
    pub product_refs: Vec<String>,
    pub warnings: Vec<Value>,
}

pub struct ReviewRequest<'a> {
    pub research_id: &'a str,
    pub context_id: &'a str,
    pub product_ref: &'a str,
    pub sku: &'a str,
    pub source_url: &'a str,
    pub limit: usize,
    pub prior_seen_review_keys: &'a [String],
    pub prior_seen_ref: Option<&'a str>,
    pub prior_seen_depth: usize,
    pub current_path: &'a str,
    pub prior_no_progress_pages: usize,
    pub prior_page_made_progress: bool,
}

struct ProductImageRequest<'a> {
    research_id: &'a str,
    product_ref: &'a str,
    source_url: &'a str,
    sku: &'a str,
    evidence_ref: &'a str,
    observed_at: &'a str,
}

pub fn normalize_context(raw: &Value) -> Normalized {
    let observed = now();
    let context_id = text(raw, "contextId").unwrap_or("context-unknown");
    let source_url = text(raw, "sourceUrl").unwrap_or("https://www.ozon.ru/");
    let region_source =
        text(raw, "regionSourceUrl").filter(|url| *url == "https://www.ozon.ru/modal/addressbook");
    let evidence_id = unique("evidence");
    let mut evidence = observed_evidence(
        Some(source_url),
        context_id,
        None,
        if region_source.is_some() {
            &["/accountState", "/accessState"]
        } else {
            &["/region/label", "/accountState", "/accessState"]
        },
        json!([]),
        &observed,
    );
    evidence["evidenceRef"] = json!(evidence_id);
    let (region_evidence_id, all_evidence) = if let Some(region_source) = region_source {
        let region_id = unique("evidence");
        let mut region_evidence = observed_evidence(
            Some(region_source),
            context_id,
            None,
            &["/region/label"],
            json!([]),
            &observed,
        );
        region_evidence["evidenceRef"] = json!(region_id);
        (region_id, vec![evidence, region_evidence])
    } else {
        (evidence_id.clone(), vec![evidence])
    };
    let region_label = raw.get("regionLabel").cloned().unwrap_or(Value::Null);
    let region_verification = enum_or(
        raw.get("regionVerification"),
        &["verified", "unverified"],
        "unverified",
    );
    let account = enum_or(
        raw.get("accountState"),
        &["authenticated", "anonymous", "unknown"],
        "unknown",
    );
    let access = enum_or(
        raw.get("accessState"),
        &["available", "blocked", "unknown"],
        "unknown",
    );
    let capabilities = capability_registry(raw.get("capabilities"));
    let mut warnings = vec![];
    if region_verification == "unverified" {
        warnings.push(warning(
            "REGION_UNVERIFIED",
            "The current region could not be verified.",
            &[region_evidence_id.as_str()],
        ));
    }
    Normalized {
        data: json!({"contextId":context_id,"observedAt":observed,"region":{"label":region_label,"verification":region_verification,"evidenceRefs":[region_evidence_id]},"accountState":account,"accessState":access,"capabilities":capabilities}),
        evidence: all_evidence,
        warnings,
        product_refs: vec![],
    }
}

pub fn normalize_search(
    raw: &Value,
    store: &mut Store,
    research_id: &str,
    context_id: &str,
    region_verified: bool,
    include_facets: bool,
) -> Result<Normalized> {
    let observed = now();
    if array(raw, "items").len() > 36 {
        return Err(anyhow!(
            "SOURCE_CHANGED: search result exceeds the 36 item source contract"
        ));
    }
    let source_url = text(raw, "searchUrl")
        .and_then(canonical_source_url)
        .ok_or_else(|| anyhow!("SOURCE_CHANGED: search source URL is missing or invalid"))?;
    let mut evidence = vec![];
    let mut product_refs = vec![];
    let mut items = vec![];
    for (index, item) in array(raw, "items").iter().take(36).enumerate() {
        let sku =
            text(item, "sku").ok_or_else(|| anyhow!("SOURCE_CHANGED: search item has no SKU"))?;
        let url = text(item, "url")
            .and_then(canonical_source_url)
            .ok_or_else(|| {
                anyhow!("SOURCE_CHANGED: search item source URL is missing or invalid")
            })?;
        let product_ref =
            store.put_ref(research_id, "product", &json!({"sku":sku,"url":url}), None)?;
        product_refs.push(product_ref.clone());
        let ev_id = unique("evidence");
        let normalized_prices = search_prices(item, &ev_id);
        let mut facts = vec![
            json!({"fieldPath":"/title","value":item.get("name").cloned().unwrap_or(Value::Null)}),
            json!({"fieldPath":"/rating","value":item.get("rating").cloned().unwrap_or(Value::Null)}),
            json!({"fieldPath":"/reviewCount","value":item.get("reviews").cloned().unwrap_or(Value::Null)}),
            json!({"fieldPath":"/deliveryLabel","value":item.get("deliveryLabel").cloned().unwrap_or(Value::Null)}),
        ];
        for price in &normalized_prices {
            let kind = price["type"].as_str().unwrap_or("unknown");
            facts.push(json!({"fieldPath":format!("/prices/{kind}/amountMinor"),"value":price["amountMinor"]}));
            facts.push(
                json!({"fieldPath":format!("/prices/{kind}/condition"),"value":price["condition"]}),
            );
        }
        let mut ev = observed_evidence(
            Some(&source_url),
            context_id,
            Some(sku),
            &[&format!("/items/{index}")],
            Value::Array(facts),
            &observed,
        );
        ev["evidenceRef"] = json!(ev_id);
        evidence.push(ev);
        let mut image_refs = vec![];
        if let Some(image_url) = text(item, "image") {
            image_refs.push(store.put_ref(research_id, "image", &json!({"url":image_url,"sourceKind":"product","sourceRef":product_ref,"sourceUrl":url,"sku":sku,"fieldPath":"/images/0"}), None)?);
        }
        items.push(json!({
            "productRef":product_ref,"sku":sku,"title":item.get("name").cloned().unwrap_or(Value::Null),"url":url,
            "availability":"unknown","prices":normalized_prices,"seller":seller_from_search(item,&ev_id),
            "matchesDisplayedPriceRange":item.get("matchesPriceRange").cloned().unwrap_or(Value::Null),
            "deliveryLabel":item.get("deliveryLabel").cloned().unwrap_or(Value::Null),"rating":item.get("rating").cloned().unwrap_or(Value::Null),
            "reviewCount":item.get("reviews").cloned().unwrap_or(Value::Null),"imageRefs":image_refs,"evidenceRefs":[ev_id]
        }));
    }
    let refinements = if include_facets {
        search_refinements(raw, store, research_id)?
    } else {
        vec![]
    };
    let refinements_truncated = raw
        .pointer("/facets/truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || refinements.len() >= 100;
    let next_cursor = match raw.get("nextCursor").and_then(Value::as_str) {
        Some(cursor) => Some(store.put_ref(
            research_id,
            "search_cursor",
            &json!({"cursor":cursor}),
            Some(1800),
        )?),
        None => None,
    };
    let has_next = raw.get("hasNext").cloned().unwrap_or(Value::Null);
    let total = raw.get("total").cloned().unwrap_or(Value::Null);
    let unique_seen = raw
        .pointer("/coverage/uniqueSeen")
        .and_then(Value::as_u64)
        .unwrap_or(items.len() as u64);
    let completeness = if next_cursor.is_some() {
        "partial"
    } else if has_next == Value::Bool(false) {
        "complete"
    } else {
        "unknown"
    };
    let mut warnings = source_warnings(raw);
    if !region_verified {
        warnings.push(warning(
            "REGION_UNVERIFIED",
            "Price and delivery context may depend on region.",
            &[],
        ));
    }
    let mut data = json!({"items":items,"refinements":refinements,"refinementsIncluded":include_facets,"refinementsTruncated":refinements_truncated,"nextCursor":next_cursor,"hasNext":has_next,"coverage":{"returned":items.len(),"uniqueSeen":unique_seen,"total":total,"snapshotGuaranteed":false,"completeness":completeness}});
    if let Some(assessment) = raw.get("priceRangeAssessment") {
        data["priceRangeAssessment"] = assessment.clone();
    }
    Ok(Normalized {
        data,
        evidence,
        warnings,
        product_refs,
    })
}

pub fn normalize_product(
    raw: &Value,
    requested: &Value,
    store: &mut Store,
    research_id: &str,
    context_id: &str,
    include: &[String],
) -> Result<ProductNormalized> {
    let observed = text(raw, "_continuationObservedAt")
        .map(str::to_owned)
        .unwrap_or_else(now);
    if include.iter().any(|section| section == "variants")
        && array(raw.get("variants").unwrap_or(&Value::Null), "items").len() > 20
        && text(raw.get("variants").unwrap_or(&Value::Null), "nextPath").is_none()
        || include.iter().any(|section| section == "offers")
            && array(raw.get("offers").unwrap_or(&Value::Null), "items").len() > 20
            && text(raw.get("offers").unwrap_or(&Value::Null), "nextPath").is_none()
    {
        return Err(anyhow!(
            "RESULT_TOO_LARGE: marketplace section exceeds its bounded page"
        ));
    }
    let sku = text(raw, "sku").ok_or_else(|| anyhow!("SOURCE_CHANGED: product page has no SKU"))?;
    let url = text(raw, "url")
        .and_then(canonical_source_url)
        .ok_or_else(|| anyhow!("SOURCE_CHANGED: product source URL is missing or invalid"))?;
    let product_ref = match text(raw, "_continuationProductRef") {
        Some(reference) => reference.to_owned(),
        None => store.put_ref(research_id, "product", &json!({"sku":sku,"url":url}), None)?,
    };
    let ev_id = unique("evidence");
    let continuation_section = raw
        .pointer("/_continuation/section")
        .and_then(Value::as_str);
    let continuation_offset = raw
        .pointer("/_continuation/offset")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let mut facts = vec![
        json!({"fieldPath":"/title","value":raw.get("name").cloned().unwrap_or(Value::Null)}),
        json!({"fieldPath":"/prices/ozonCard/amountMinor","value":money(raw.get("cardPrice"))}),
        json!({"fieldPath":"/prices/regular/amountMinor","value":money(raw.get("priceRegular"))}),
        json!({"fieldPath":"/availability","value":availability(raw.get("available"))}),
        json!({"fieldPath":"/rating","value":raw.get("rating").cloned().unwrap_or(Value::Null)}),
        json!({"fieldPath":"/reviewCount","value":raw.get("reviews").cloned().unwrap_or(Value::Null)}),
    ];
    let characteristic_offset = if continuation_section == Some("characteristics") {
        continuation_offset
    } else {
        0
    };
    if include.iter().any(|section| section == "characteristics")
        && let Some(values) = raw.get("characteristics").and_then(Value::as_object)
    {
        for (name, value) in values.iter().skip(characteristic_offset).take(50) {
            facts.push(json!({"fieldPath":format!("/characteristics/{}",pointer_escape(name)),"value":value}));
        }
    }
    if include.iter().any(|section| section == "description")
        && let Some(value) = raw.pointer("/description/text").and_then(Value::as_str)
    {
        let offset = if continuation_section == Some("description") {
            continuation_offset
        } else {
            0
        };
        facts.push(json!({"fieldPath":"/description/text","value":value.chars().skip(offset).take(12000).collect::<String>()}));
    }
    let image_offset = if continuation_section == Some("images") {
        continuation_offset
    } else {
        0
    };
    let image_urls = array(raw, "images")
        .iter()
        .chain(array(raw.get("description").unwrap_or(&Value::Null), "images").iter())
        .filter_map(Value::as_str)
        .skip(image_offset)
        .take(12)
        .collect::<Vec<_>>();
    if include.iter().any(|section| section == "images") && !image_urls.is_empty() {
        facts.push(json!({"fieldPath":"/images","value":image_urls}));
    }
    if include.iter().any(|section| section == "variants") {
        let skus = array(raw.get("variants").unwrap_or(&Value::Null), "items")
            .iter()
            .filter_map(|v| text(v, "sku"))
            .take(20)
            .collect::<Vec<_>>();
        facts.push(json!({"fieldPath":"/variants/items","value":skus}));
    }
    if include.iter().any(|section| section == "offers") {
        let sellers = array(raw.get("offers").unwrap_or(&Value::Null), "items")
            .iter()
            .filter_map(|v| v.get("seller").and_then(|s| text(s, "name")))
            .take(20)
            .collect::<Vec<_>>();
        facts.push(json!({"fieldPath":"/offers/sellers","value":sellers}));
    }
    facts.truncate(100);
    let mut field_paths = vec![
        "/title",
        "/prices",
        "/availability",
        "/rating",
        "/reviewCount",
    ];
    for section in include {
        match section.as_str() {
            "characteristics" => field_paths.push("/characteristics"),
            "description" => field_paths.push("/description"),
            "variants" => field_paths.push("/variants"),
            "offers" => field_paths.push("/offers"),
            "images" => field_paths.push("/images"),
            _ => {}
        }
    }
    let mut ev = observed_evidence(
        Some(&url),
        context_id,
        Some(sku),
        &field_paths,
        Value::Array(facts),
        &observed,
    );
    ev["evidenceRef"] = json!(ev_id);
    let mut product = Map::new();
    product.insert("productRef".into(), json!(product_ref));
    product.insert("sku".into(), json!(sku));
    product.insert(
        "title".into(),
        raw.get("name").cloned().unwrap_or(Value::Null),
    );
    product.insert("url".into(), json!(url));
    product.insert(
        "availability".into(),
        json!(availability(raw.get("available"))),
    );
    product.insert("prices".into(), json!(product_prices(raw, &ev_id)));
    product.insert("seller".into(), seller(raw.get("seller"), &ev_id));
    product.insert(
        "rating".into(),
        raw.get("rating").cloned().unwrap_or(Value::Null),
    );
    product.insert(
        "reviewCount".into(),
        raw.get("reviews").cloned().unwrap_or(Value::Null),
    );
    product.insert(
        "fieldEvidence".into(),
        json!([{"fieldPath":"","evidenceRefs":[ev_id]}]),
    );
    product.insert("evidenceRefs".into(), json!([ev_id]));
    for section in include {
        let value = match section.as_str() {
            "characteristics" => {
                characteristics(raw, store, research_id, &product_ref, &ev_id, &observed)?
            }
            "description" => description(raw, store, research_id, &product_ref, &ev_id, &observed)?,
            "variants" => variants(raw, store, research_id, &ev_id)?,
            "offers" => offers(raw, store, research_id, &ev_id)?,
            "images" => images(
                raw,
                store,
                ProductImageRequest {
                    research_id,
                    product_ref: &product_ref,
                    source_url: &url,
                    sku,
                    evidence_ref: &ev_id,
                    observed_at: &observed,
                },
            )?,
            _ => continue,
        };
        product.insert(section.clone(), value);
    }
    Ok(ProductNormalized {
        result: json!({"status":"ok","requested":requested,"product":Value::Object(product)}),
        evidence: vec![ev],
        product_refs: vec![product_ref],
        warnings: source_warnings(raw),
    })
}

pub fn normalize_reviews(
    raw: &Value,
    store: &mut Store,
    request: ReviewRequest<'_>,
) -> Result<Normalized> {
    let ReviewRequest {
        research_id,
        context_id,
        product_ref,
        sku,
        source_url,
        limit,
        prior_seen_review_keys,
        prior_seen_ref,
        prior_seen_depth,
        current_path,
        prior_no_progress_pages,
        prior_page_made_progress,
    } = request;
    let observed = text(raw, "_continuationObservedAt")
        .map(str::to_owned)
        .unwrap_or_else(now);
    let canonical = canonical_source_url(source_url)
        .ok_or_else(|| anyhow!("SOURCE_CHANGED: review source URL is missing or invalid"))?;
    if array(raw, "reviews").len() > 30 {
        return Err(anyhow!(
            "SOURCE_CHANGED: review result exceeds the bounded source page"
        ));
    }
    let source_reviews = array(raw, "reviews");
    let mut seen_review_keys = prior_seen_review_keys
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if seen_review_keys.len() > MAX_SEEN_REVIEWS {
        return Err(anyhow!(
            "SOURCE_CHANGED: review cursor coverage exceeds {MAX_SEEN_REVIEWS} identities"
        ));
    }
    let mut new_review_keys = vec![];
    let mut selected_reviews = vec![];
    let mut consumed = 0usize;
    for (index, review) in source_reviews.iter().enumerate() {
        consumed = index + 1;
        let identity = review_identity(review)?;
        if seen_review_keys.insert(identity.clone()) {
            new_review_keys.push(identity);
            selected_reviews.push((index, review));
            if selected_reviews.len() == limit.min(30) {
                break;
            }
        }
    }
    if seen_review_keys.len() > MAX_SEEN_REVIEWS {
        return Err(anyhow!(
            "SOURCE_CHANGED: review cursor coverage exceeds {MAX_SEEN_REVIEWS} identities"
        ));
    }
    let remaining = &source_reviews[consumed..];
    let next_path = if remaining.is_empty() {
        text(raw, "nextPath")
    } else {
        None
    };
    let has_continuation = !remaining.is_empty() || next_path.is_some();
    let page_made_progress = prior_page_made_progress || !new_review_keys.is_empty();
    let no_progress_pages = if remaining.is_empty() {
        if page_made_progress {
            0
        } else {
            prior_no_progress_pages.saturating_add(1)
        }
    } else {
        prior_no_progress_pages
    };
    if has_continuation
        && (no_progress_pages >= MAX_REVIEW_NO_PROGRESS_PAGES
            || remaining.is_empty() && !page_made_progress && next_path == Some(current_path))
    {
        return Err(anyhow!(
            "SOURCE_CHANGED: review pagination made no progress; restart reviews from productRef"
        ));
    }
    let mut evidence = vec![];
    let mut reviews = vec![];
    for (index, review) in selected_reviews {
        let review_ref = store.put_ref(research_id,"review",&json!({"reviewId":review.get("reviewId"),"productRef":product_ref,"sku":sku,"sourceUrl":canonical}),None)?;
        let mut image_refs = vec![];
        for (photo_index, url) in array(review, "photos")
            .iter()
            .filter_map(Value::as_str)
            .take(12)
            .enumerate()
        {
            image_refs.push(store.put_ref(research_id,"image",&json!({"url":url,"sourceKind":"review","sourceRef":review_ref,"sourceUrl":canonical,"sku":sku,"fieldPath":format!("/reviews/{index}/imageRefs/{photo_index}")}),None)?);
        }
        let rating = review
            .get("score")
            .and_then(Value::as_f64)
            .and_then(|n| {
                if (1.0..=5.0).contains(&n) && n.fract() == 0.0 {
                    Some(json!(n as u64))
                } else {
                    None
                }
            })
            .unwrap_or(Value::Null);
        let text_value = review_text(review);
        let purchase_verified = if review.get("purchased").and_then(Value::as_bool) == Some(true) {
            Value::Bool(true)
        } else {
            Value::Null
        };
        let ev_id = unique("evidence");
        let published_at = parse_timestamp(review.get("date"));
        let facts = json!([
            {"fieldPath":format!("/reviews/{index}/rating"),"value":rating},
            {"fieldPath":format!("/reviews/{index}/publishedAt"),"value":published_at},
            {"fieldPath":format!("/reviews/{index}/text"),"value":text_value},
            {"fieldPath":format!("/reviews/{index}/variantLabel"),"value":review.get("variantLabel").cloned().unwrap_or(Value::Null)},
            {"fieldPath":format!("/reviews/{index}/purchaseVerified"),"value":purchase_verified},
            {"fieldPath":format!("/reviews/{index}/imageRefs"),"value":array(review,"photos").iter().filter_map(Value::as_str).take(12).collect::<Vec<_>>()}
        ]);
        let field_path = format!("/reviews/{index}");
        let mut ev = observed_evidence(
            Some(&canonical),
            context_id,
            Some(sku),
            &[&field_path],
            facts,
            &observed,
        );
        ev["evidenceRef"] = json!(ev_id);
        evidence.push(ev);
        reviews.push(json!({"reviewRef":review_ref,"rating":rating,"publishedAt":parse_timestamp(review.get("date")),"text":text_value,"variantLabel":review.get("variantLabel").cloned().unwrap_or(Value::Null),"purchaseVerified":purchase_verified,"imageRefs":image_refs,"evidenceRefs":[ev_id]}));
    }
    let mut refinements = vec![];
    for r in array(raw, "refinements").iter().take(100) {
        if let (Some(label), Some(url)) = (text(r, "label"), text(r, "url")) {
            let rr = store.put_ref(
                research_id,
                "review_search",
                &json!({"path":url,"productRef":product_ref,"sku":sku,"sourceUrl":canonical}),
                Some(1800),
            )?;
            refinements.push(json!({"kind":enum_or(r.get("kind"),&["filter","sort"],"filter"),"label":label,"groupLabel":null,"selected":r.get("selected").cloned().unwrap_or(Value::Null),"reviewSearchRef":rr}));
        }
    }
    let seen_ref = if has_continuation && !new_review_keys.is_empty() {
        let (reference, _) = store_review_seen_state(
            store,
            research_id,
            prior_seen_ref,
            prior_seen_depth,
            &new_review_keys,
            &seen_review_keys,
        )?;
        Some(reference)
    } else {
        prior_seen_ref.map(str::to_owned)
    };
    let next_cursor = if !remaining.is_empty() {
        let mut cached = raw.clone();
        cached["reviews"] = json!(remaining);
        cached["_continuationObservedAt"] = json!(observed);
        Some(store.put_ref(
            research_id,
            "review_cursor",
            &json!({"cachedRaw":cached,"capturedAt":observed,"contextId":context_id,"productRef":product_ref,"sku":sku,"sourceUrl":canonical,"seenRef":seen_ref,"noProgressPages":no_progress_pages,"pageMadeProgress":page_made_progress}),
            Some(1800),
        )?)
    } else {
        match next_path {
            Some(path) => Some(store.put_ref(
                research_id,
                "review_cursor",
                &json!({"path":path,"productRef":product_ref,"sku":sku,"sourceUrl":canonical,"seenRef":seen_ref,"noProgressPages":no_progress_pages,"pageMadeProgress":false}),
                Some(1800),
            )?),
            None => None,
        }
    };
    let has_next = if !remaining.is_empty() {
        Value::Bool(true)
    } else {
        raw.get("hasNext").cloned().unwrap_or(Value::Null)
    };
    let completeness = if next_cursor.is_some() {
        "partial"
    } else if has_next == Value::Bool(false) {
        "complete"
    } else {
        "unknown"
    };
    let aggregate_id = unique("evidence");
    let mut aggregate_ev = observed_evidence(
        Some(&canonical),
        context_id,
        Some(sku),
        &["/reviews", "/aggregate", "/refinements"],
        json!([
            {"fieldPath":"/aggregate/rating","value":raw.get("rating").cloned().unwrap_or(Value::Null)},
            {"fieldPath":"/aggregate/count","value":raw.get("totalReviews").cloned().unwrap_or(Value::Null)},
            {"fieldPath":"/aggregationScope","value":raw.get("aggregationScope").cloned().unwrap_or(Value::Null)}
        ]),
        &observed,
    );
    aggregate_ev["evidenceRef"] = json!(aggregate_id);
    evidence.push(aggregate_ev);
    let mut warnings = source_warnings(raw);
    if raw.get("rating").is_none() && raw.get("totalReviews").is_none() {
        warnings.push(warning(
            "AGGREGATE_UNAVAILABLE",
            "Review aggregate was not observed on this page.",
            &[],
        ));
    }
    Ok(Normalized {
        data: json!({"subjectSku":sku,"aggregationScope":enum_or(raw.get("aggregationScope"),&["specific_sku","multiple_variants","unknown"],"unknown"),"aggregate":{"rating":raw.get("rating").cloned().unwrap_or(Value::Null),"count":raw.get("totalReviews").cloned().unwrap_or(Value::Null)},"reviews":reviews,"refinements":refinements,"refinementsIncluded":!array(raw,"refinements").is_empty(),"refinementsTruncated":array(raw,"refinements").len()>100,"coverage":{"returned":reviews.len(),"uniqueSeen":seen_review_keys.len(),"total":raw.get("totalReviews").cloned().unwrap_or(Value::Null),"snapshotGuaranteed":false,"completeness":completeness},"nextCursor":next_cursor,"hasNext":has_next}),
        evidence,
        warnings,
        product_refs: vec![product_ref.into()],
    })
}

fn store_review_seen_state(
    store: &mut Store,
    research_id: &str,
    parent: Option<&str>,
    prior_depth: usize,
    new_keys: &[String],
    all_keys: &BTreeSet<String>,
) -> Result<(String, usize)> {
    if new_keys.is_empty() || new_keys.len() > 30 || all_keys.len() > MAX_SEEN_REVIEWS {
        return Err(anyhow!("SOURCE_CHANGED: invalid review coverage chunk"));
    }
    let (value, depth) = if prior_depth + 1 >= REVIEW_SEEN_CHECKPOINT_INTERVAL {
        let value = json!({"kind":"checkpoint","keys":all_keys,"count":all_keys.len(),"depth":0});
        if serde_json::to_vec(&value)?.len() > MAX_REVIEW_SEEN_CHECKPOINT_BYTES {
            return Err(anyhow!(
                "SOURCE_CHANGED: review coverage checkpoint is too large"
            ));
        }
        (value, 0)
    } else {
        let depth = prior_depth + 1;
        (
            json!({"kind":"delta","parent":parent,"keys":new_keys,"count":all_keys.len(),"depth":depth}),
            depth,
        )
    };
    Ok((
        store.put_ref(research_id, "review_seen", &value, None)?,
        depth,
    ))
}

pub fn item_error(requested: &Value, code: &str, message: &str) -> Value {
    json!({"status":"error","requested":requested,"error":{"code":item_error_code(code),"message":message.chars().take(2000).collect::<String>(),"retryable":matches!(code,"SOURCE_BLOCKED"|"UPSTREAM_TIMEOUT")}})
}
pub fn warning(code: &str, message: &str, refs: &[&str]) -> Value {
    json!({"code":code,"message":message,"evidenceRefs":refs})
}

fn product_prices(raw: &Value, ev: &str) -> Vec<Value> {
    let mut out = vec![];
    if let Some(v) = money(raw.get("cardPrice")) {
        out.push(price(v, "ozon_card", Some("с Ozon Картой"), ev));
    }
    if let Some(v) = money(raw.get("priceRegular")) {
        out.push(price(v, "regular", None, ev));
    }
    out
}
fn search_prices(raw: &Value, ev: &str) -> Vec<Value> {
    let Some(v) = money(raw.get("price")) else {
        return vec![];
    };
    let kind = enum_or(
        raw.get("priceType"),
        &["ozon_card", "regular", "unknown"],
        "unknown",
    );
    vec![price(v, &kind, text(raw, "priceLabel"), ev)]
}
fn price(amount: u64, kind: &str, condition: Option<&str>, ev: &str) -> Value {
    json!({"amountMinor":amount,"currency":"RUB","type":kind,"condition":condition,"evidenceRefs":[ev]})
}
fn money(v: Option<&Value>) -> Option<u64> {
    v.and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| (n * 100.0).round() as u64)
}
fn availability(v: Option<&Value>) -> &'static str {
    match v.and_then(Value::as_bool) {
        Some(true) => "available",
        Some(false) => "unavailable",
        None => "unknown",
    }
}
fn seller(v: Option<&Value>, ev: &str) -> Value {
    let Some(v) = v else { return Value::Null };
    json!({"name":v.get("name").cloned().unwrap_or(Value::Null),"rating":v.get("rating").cloned().unwrap_or(Value::Null),"url":text(v,"url").and_then(canonical_source_url),"evidenceRefs":[ev]})
}
fn seller_from_search(v: &Value, ev: &str) -> Value {
    text(v, "seller")
        .map(|name| json!({"name":name,"rating":null,"url":null,"evidenceRefs":[ev]}))
        .unwrap_or(Value::Null)
}
fn section_base(
    status: &str,
    count: usize,
    cap: usize,
    next: Option<String>,
    has_next: Value,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("status".into(), json!(status));
    m.insert("truncated".into(), json!(count > cap));
    m.insert("nextCursor".into(), json!(next));
    m.insert("hasNext".into(), has_next);
    m
}
fn characteristics(
    raw: &Value,
    store: &mut Store,
    rid: &str,
    pref: &str,
    ev: &str,
    observed: &str,
) -> Result<Value> {
    let pairs = raw.get("characteristics").and_then(Value::as_object);
    let count = pairs.map_or(0, Map::len);
    let offset = raw
        .pointer("/_continuation/offset")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let remaining = count.saturating_sub(offset);
    let more = remaining > 50;
    let next = if more {
        Some(cached_product_cursor(
            store,
            rid,
            raw,
            pref,
            "characteristics",
            offset + 50,
            observed,
        )?)
    } else {
        None
    };
    let mut m = section_base(
        if pairs.is_some() {
            if more { "partial" } else { "available" }
        } else {
            "unknown"
        },
        remaining,
        50,
        next,
        Value::Bool(more),
    );
    m.insert(
        "items".into(),
        json!(
            pairs
                .into_iter()
                .flat_map(|p| p.iter())
                .skip(offset)
                .take(50)
                .map(|(k, v)| json!({"name":k,"value":v.as_str(),"evidenceRefs":[ev]}))
                .collect::<Vec<_>>()
        ),
    );
    Ok(Value::Object(m))
}
fn description(
    raw: &Value,
    store: &mut Store,
    rid: &str,
    pref: &str,
    ev: &str,
    observed: &str,
) -> Result<Value> {
    let text = raw
        .pointer("/description/text")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let offset = raw
        .pointer("/_continuation/offset")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let total = text.map_or(0, |value| value.chars().count());
    let remaining = total.saturating_sub(offset);
    let truncated = remaining > 12000;
    let next = if truncated {
        Some(cached_product_cursor(
            store,
            rid,
            raw,
            pref,
            "description",
            offset + 12000,
            observed,
        )?)
    } else {
        None
    };
    let observed_text = text.is_some_and(|value| !value.trim().is_empty());
    let mut m = section_base(
        if observed_text {
            if truncated { "partial" } else { "available" }
        } else {
            "unknown"
        },
        remaining,
        12000,
        next,
        if observed_text {
            Value::Bool(truncated)
        } else {
            Value::Null
        },
    );
    m.insert(
        "text".into(),
        text.map(|v| v.chars().skip(offset).take(12000).collect::<String>())
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    m.insert("evidenceRefs".into(), json!([ev]));
    Ok(Value::Object(m))
}
fn cached_product_cursor(
    store: &mut Store,
    rid: &str,
    raw: &Value,
    pref: &str,
    section: &str,
    offset: usize,
    observed: &str,
) -> Result<String> {
    let mut cached = raw.clone();
    cached["_continuation"] = json!({"section":section,"offset":offset});
    cached["_continuationProductRef"] = json!(pref);
    cached["_continuationObservedAt"] = json!(observed);
    let context_id = store.research_context(rid)?["contextId"].clone();
    store.put_ref(
        rid,
        "product_cursor",
        &json!({"cachedRaw":cached,"include":[section],"capturedAt":observed,"contextId":context_id}),
        Some(1800),
    )
}
fn variants(raw: &Value, store: &mut Store, rid: &str, ev: &str) -> Result<Value> {
    let source = raw.get("variants").unwrap_or(&Value::Null);
    let values = array(source, "items");
    let mut items = vec![];
    for v in values.iter().take(20) {
        if let Some(sku) = text(v, "sku") {
            let pref =
                store.put_ref(rid, "product", &json!({"sku":sku,"url":v.get("url")}), None)?;
            items.push(json!({"productRef":pref,"sku":sku,"title":v.get("title").cloned().unwrap_or(Value::Null),"evidenceRefs":[ev]}));
        }
    }
    let next = match text(source, "nextPath") {
        Some(path) => Some(store.put_ref(
            rid,
            "product_cursor",
            &json!({"path":path,"include":["variants"]}),
            Some(1800),
        )?),
        None => None,
    };
    let mut m = section_base(
        enum_or(
            source.get("status"),
            &["available", "partial", "unknown", "unsupported"],
            "unknown",
        )
        .as_str(),
        values.len(),
        20,
        next,
        source.get("hasNext").cloned().unwrap_or(Value::Null),
    );
    m.insert("items".into(), json!(items));
    Ok(Value::Object(m))
}
fn offers(raw: &Value, store: &mut Store, rid: &str, ev: &str) -> Result<Value> {
    let source = raw.get("offers").unwrap_or(&Value::Null);
    let values = array(source, "items");
    let items=values.iter().take(20).map(|v|json!({"offerRef":null,"url":text(v,"url").and_then(canonical_source_url),"availability":availability(v.get("available")),"seller":seller(v.get("seller"),ev),"prices":v.get("prices").and_then(Value::as_array).map(|a|a.iter().filter_map(|x|money(Some(x))).take(3).map(|p|price(p,"unknown",None,ev)).collect::<Vec<_>>()).unwrap_or_default(),"delivery":text(v,"deliveryLabel").map(|l|json!({"label":l,"dateFrom":null,"dateTo":null})),"evidenceRefs":[ev]})).collect::<Vec<_>>();
    let next = match text(source, "nextPath") {
        Some(path) => Some(store.put_ref(
            rid,
            "product_cursor",
            &json!({"path":path,"include":["offers"]}),
            Some(1800),
        )?),
        None => None,
    };
    let mut m = section_base(
        enum_or(
            source.get("status"),
            &["available", "partial", "unknown", "unsupported"],
            "unknown",
        )
        .as_str(),
        values.len(),
        20,
        next,
        source.get("hasNext").cloned().unwrap_or(Value::Null),
    );
    m.insert("items".into(), json!(items));
    Ok(Value::Object(m))
}
fn images(raw: &Value, store: &mut Store, request: ProductImageRequest<'_>) -> Result<Value> {
    let gallery = array(raw, "images");
    let description = array(raw.get("description").unwrap_or(&Value::Null), "images");
    let mut seen = std::collections::BTreeSet::new();
    let candidates = gallery
        .iter()
        .filter_map(Value::as_str)
        .map(|v| (v, "/images"))
        .chain(
            description
                .iter()
                .filter_map(Value::as_str)
                .map(|v| (v, "/description/images")),
        )
        .filter(|(v, _)| seen.insert((*v).to_owned()))
        .collect::<Vec<_>>();
    let offset = raw
        .pointer("/_continuation/offset")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let remaining = candidates.len().saturating_sub(offset);
    let more = remaining > 12;
    let next = if more {
        Some(cached_product_cursor(
            store,
            request.research_id,
            raw,
            request.product_ref,
            "images",
            offset + 12,
            request.observed_at,
        )?)
    } else {
        None
    };
    let mut items = vec![];
    for (i, (image, base)) in candidates.iter().skip(offset).take(12).enumerate() {
        let source_index = offset + i;
        let ir=store.put_ref(request.research_id,"image",&json!({"url":image,"sourceKind":"product","sourceRef":request.product_ref,"sourceUrl":request.source_url,"sku":request.sku,"fieldPath":format!("{base}/{source_index}")}),None)?;
        items.push(json!({"imageRef":ir,"altText":null,"evidenceRefs":[request.evidence_ref]}));
    }
    let mut m = section_base(
        if more { "partial" } else { "available" },
        remaining,
        12,
        next,
        Value::Bool(more),
    );
    m.insert("items".into(), json!(items));
    Ok(Value::Object(m))
}
fn search_refinements(raw: &Value, store: &mut Store, rid: &str) -> Result<Vec<Value>> {
    let mut out = vec![];
    for facet in raw
        .pointer("/facets/items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if out.len() < 100
            && let (Some(label), Some(url)) = (text(facet, "title"), text(facet, "searchUrl"))
        {
            let rf = store.put_ref(rid, "search_ref", &json!({"searchUrl":url}), Some(1800))?;
            out.push(json!({"kind":"filter","label":label,"groupLabel":facet.get("title").cloned().unwrap_or(Value::Null),"selected":facet.get("selected").cloned().unwrap_or(Value::Null),"searchRef":rf}));
        }
        for option in array(facet, "options") {
            if out.len() >= 100 {
                break;
            }
            if let (Some(label), Some(url)) = (text(option, "label"), text(option, "searchUrl")) {
                let rf = store.put_ref(rid, "search_ref", &json!({"searchUrl":url}), Some(1800))?;
                out.push(json!({"kind":"filter","label":label,"groupLabel":facet.get("title").cloned().unwrap_or(Value::Null),"selected":option.get("selected").cloned().unwrap_or(Value::Null),"searchRef":rf}));
            }
        }
    }
    for option in array(raw, "sortOptions") {
        if out.len() >= 100 {
            break;
        }
        if let (Some(label), Some(url)) = (text(option, "label"), text(option, "searchUrl")) {
            let rf = store.put_ref(rid, "search_ref", &json!({"searchUrl":url}), Some(1800))?;
            out.push(json!({"kind":"sort","label":label,"groupLabel":"Сортировка","selected":option.get("selected").cloned().unwrap_or(Value::Null),"searchRef":rf}));
        }
    }
    for active in array(raw, "activeFilters") {
        for value in array(active, "values") {
            if out.len() >= 100 {
                break;
            }
            if let (Some(label), Some(url)) = (text(value, "label"), text(value, "searchUrl")) {
                let rf = store.put_ref(rid, "search_ref", &json!({"searchUrl":url}), Some(1800))?;
                out.push(json!({"kind":"filter","label":label,"groupLabel":active.get("name").cloned().unwrap_or(Value::Null),"selected":true,"searchRef":rf}));
            }
        }
    }
    Ok(out)
}
fn review_text(v: &Value) -> Value {
    let parts = ["comment", "pros", "cons"]
        .iter()
        .filter_map(|k| text(v, k))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        Value::Null
    } else {
        json!(parts.join("\n\n"))
    }
}
fn review_identity(review: &Value) -> Result<String> {
    Ok(match text(review, "reviewId").filter(|id| !id.is_empty()) {
        Some(id) => format!("id:{:x}", Sha256::digest(id.as_bytes())),
        None => format!("anonymous:{}", Uuid::new_v4()),
    })
}
fn parse_timestamp(v: Option<&Value>) -> Value {
    v.and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| json!(d.to_rfc3339()))
        .unwrap_or(Value::Null)
}
fn source_warnings(raw: &Value) -> Vec<Value> {
    array(raw, "warnings")
        .iter()
        .take(30)
        .filter_map(Value::as_str)
        .map(|code| {
            let message = if code == "PRICE_OUTSIDE_REQUESTED_RANGE" {
                "Returned displayed search prices do not all match the requested range. Ozon's source filter price basis is unknown; results were preserved."
            } else {
                "The source reported incomplete or unstable data."
            };
            warning(
                code,
                message,
                &[],
            )
        })
        .collect()
}
fn capability_registry(raw: Option<&Value>) -> Vec<Value> {
    const NAMES: [(&str, &str, Option<&str>); 17] = [
        ("search", "search", None),
        ("search_refinements", "searchRefinements", None),
        ("search_pagination", "searchPagination", None),
        ("card_prices", "cardPrices", None),
        ("products", "products", None),
        ("characteristics", "characteristics", None),
        ("description", "description", None),
        ("variants", "variants", None),
        ("offers", "offers", None),
        ("review_text", "reviewText", None),
        ("review_refinements", "reviewRefinements", None),
        ("review_pagination", "reviewPagination", None),
        ("review_images", "reviewImages", None),
        ("product_images", "productImages", None),
        ("image_content", "imageContent", None),
        ("region_verification", "regionVerification", None),
        ("account_observation", "accountObservation", None),
    ];
    NAMES
        .into_iter()
        .map(|(name, key, known)| {
            let v = raw.and_then(|r| r.get(name).or_else(|| r.get(key)));
            let (status, mut reason) = match v {
                Some(Value::String(s)) => (
                    enum_or(
                        Some(&Value::String(s.clone())),
                        &["available", "unsupported", "unverified"],
                        "unverified",
                    ),
                    Value::Null,
                ),
                Some(Value::Object(m)) => (
                    enum_or(
                        m.get("status"),
                        &["available", "unsupported", "unverified"],
                        "unverified",
                    ),
                    m.get("reason").cloned().unwrap_or(Value::Null),
                ),
                None if known.is_some() => (
                    known.unwrap().into(),
                    if known == Some("unverified") {
                        json!("Capability has not yet been exercised in this context.")
                    } else {
                        Value::Null
                    },
                ),
                _ => (
                    "unverified".into(),
                    json!("Capability was not reported by the source."),
                ),
            };
            if reason.is_null() && status != "available" {
                reason = if status == "unsupported" {
                    json!("The source does not expose this capability.")
                } else {
                    json!("The capability has not yet been verified in this context.")
                };
            }
            json!({"name":name,"status":status,"reason":reason})
        })
        .collect()
}
fn item_error_code(code: &str) -> &str {
    match code {
        "INVALID_REFERENCE"
        | "CONTEXT_CHANGED"
        | "CONTEXT_UNVERIFIED"
        | "RESEARCH_EXPIRED"
        | "SOURCE_BLOCKED"
        | "SOURCE_CHANGED"
        | "UPSTREAM_TIMEOUT"
        | "NOT_FOUND"
        | "RESULT_TOO_LARGE"
        | "UNSUPPORTED_CAPABILITY" => code,
        _ => "SOURCE_CHANGED",
    }
}
fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}
fn array<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}
fn enum_or(v: Option<&Value>, allowed: &[&str], fallback: &str) -> String {
    v.and_then(Value::as_str)
        .filter(|s| allowed.contains(s))
        .unwrap_or(fallback)
        .into()
}
fn pointer_escape(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}
fn unique(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{contracts, evidence};
    use tempfile::tempdir;

    fn test_store() -> (tempfile::TempDir, Store, String, Value) {
        let d = tempdir().unwrap();
        std::fs::set_permissions(
            d.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let mut store = Store::open(d.path()).unwrap();
        let context = json!({"contextId":"c","regionLabel":null,"regionVerification":"unverified","accountState":"unknown","accessState":"available"});
        let rid = store.create_research("q", &context).unwrap();
        (d, store, rid, context)
    }

    fn read_seen_chain(store: &Store, seen_ref: Option<&str>) -> Vec<String> {
        let mut current = seen_ref.map(str::to_owned);
        let mut keys = BTreeSet::new();
        while let Some(reference) = current {
            let node = store.get_ref(&reference, "review_seen").unwrap();
            keys.extend(
                node.value["keys"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|key| key.as_str().unwrap().to_owned()),
            );
            current = node.value["parent"].as_str().map(str::to_owned);
        }
        keys.into_iter().collect()
    }

    #[test]
    fn missing_card_price_is_not_inferred() {
        assert!(
            product_prices(&json!({"price":10.0,"cardPrice":null}), "e")
                .iter()
                .all(|v| v["type"] != "ozon_card")
        );
    }

    #[test]
    fn selected_address_city_has_separate_static_source_evidence() {
        let raw = json!({"contextId":"c","regionLabel":"Москва","regionVerification":"verified","accountState":"authenticated","accessState":"available","regionSourceUrl":"https://www.ozon.ru/modal/addressbook"});
        let normalized = normalize_context(&raw);
        assert_eq!(normalized.evidence.len(), 2);
        let region_ref = &normalized.data["region"]["evidenceRefs"][0];
        let region = normalized
            .evidence
            .iter()
            .find(|item| &item["evidenceRef"] == region_ref)
            .unwrap();
        assert_eq!(region["sourceUrl"], "https://www.ozon.ru/modal/addressbook");
        assert_eq!(region["fieldPaths"], json!(["/region/label"]));
        assert_eq!(normalized.evidence[0]["sourceUrl"], "https://www.ozon.ru/");
        assert_eq!(
            normalized.evidence[0]["fieldPaths"],
            json!(["/accountState", "/accessState"])
        );
        let mut invalid = raw;
        invalid["regionSourceUrl"] = json!("https://www.ozon.ru/modal/addressbook?private=value");
        let normalized = normalize_context(&invalid);
        assert_eq!(normalized.evidence.len(), 1);
        assert!(
            !serde_json::to_string(&normalized.evidence)
                .unwrap()
                .contains("private")
        );
    }
    #[test]
    fn search_creates_durable_refs() {
        let (_d, mut s, r, c) = test_store();
        let n=normalize_search(&json!({"searchUrl":"https://www.ozon.ru/search/?text=x","items":[{"sku":"1","name":"x","price":1.5,"priceType":"unknown","currency":"RUB","url":"https://www.ozon.ru/product/1/"}],"hasNext":false}),&mut s,&r,"c",false,false).unwrap();
        let p = n.data["items"][0]["productRef"].as_str().unwrap();
        assert_eq!(s.get_ref(p, "product").unwrap().research_id, r);
        s.record(&r, "search", "q", &n.product_refs, &n.evidence)
            .unwrap();
        let stored = s
            .read(&json!({"researchId":r,"section":"evidence"}))
            .unwrap();
        assert!(!stored["payload"][0]["facts"].as_array().unwrap().is_empty());
        let observed = now();
        contracts::validate_output(
            "ozon_search",
            &evidence::envelope(Some(&r), n.data, &c, n.evidence, n.warnings, &observed),
        )
        .unwrap();
    }

    #[test]
    fn search_price_range_semantics_are_explicit() {
        let (_d, mut store, research_id, context) = test_store();
        let raw = json!({
            "searchUrl": "https://www.ozon.ru/search/?text=x&currency_price=500.000%3B1500.000",
            "items": [
                {"sku":"1","name":"below","price":362.0,"priceType":"unknown","matchesPriceRange":false,"currency":"RUB","url":"https://www.ozon.ru/product/1/"},
                {"sku":"2","name":"inside","price":500.0,"priceType":"unknown","matchesPriceRange":true,"currency":"RUB","url":"https://www.ozon.ru/product/2/"},
                {"sku":"3","name":"unknown","price":null,"priceType":"unknown","matchesPriceRange":null,"currency":"RUB","url":"https://www.ozon.ru/product/3/"}
            ],
            "priceRangeAssessment": {
                "basis": "displayed_search_price",
                "sourceFilterGuaranteesMatch": false
            },
            "nextCursor": "opaque-source-cursor",
            "hasNext": true,
            "coverage": {"uniqueSeen": 3},
            "warnings": ["PRICE_OUTSIDE_REQUESTED_RANGE"]
        });

        let normalized =
            normalize_search(&raw, &mut store, &research_id, "c", false, false).unwrap();

        assert_eq!(
            normalized.data["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["matchesDisplayedPriceRange"].clone())
                .collect::<Vec<_>>(),
            vec![json!(false), json!(true), Value::Null]
        );
        assert_eq!(
            normalized.data["priceRangeAssessment"],
            json!({
                "basis": "displayed_search_price",
                "sourceFilterGuaranteesMatch": false
            })
        );
        assert_eq!(normalized.data["coverage"]["completeness"], "partial");
        assert_eq!(normalized.data["coverage"]["returned"], 3);
        assert_eq!(
            normalized.warnings[0]["message"],
            "Returned displayed search prices do not all match the requested range. Ozon's source filter price basis is unknown; results were preserved."
        );
        let observed = now();
        contracts::validate_output(
            "ozon_search",
            &evidence::envelope(
                Some(&research_id),
                normalized.data,
                &context,
                normalized.evidence,
                normalized.warnings,
                &observed,
            ),
        )
        .unwrap();
    }

    #[test]
    fn product_and_review_conversion_match_public_contracts() {
        let (_d, mut store, rid, context) = test_store();
        let raw = json!({"sku":"123","name":"Headphones","url":"https://www.ozon.ru/product/headphones-123/?tracking=x","price":640.0,"cardPrice":627.0,"priceRegular":640.0,"available":null,"rating":4.9,"reviews":27,"seller":null,"deliveryLabel":null,"images":["https://ir.ozone.ru/a.jpg"],"characteristics":{"Цвет":"черный"},"description":{"text":"Описание","images":[]},"variants":{"status":"unsupported","items":[],"hasNext":null,"nextPath":null},"offers":{"status":"unsupported","items":[],"hasNext":null,"nextPath":null},"warnings":[]});
        let product = normalize_product(
            &raw,
            &json!({"sku":"123"}),
            &mut store,
            &rid,
            "c",
            &["characteristics".into(), "offers".into(), "images".into()],
        )
        .unwrap();
        store
            .record(
                &rid,
                "products",
                "details",
                &product.product_refs,
                &product.evidence,
            )
            .unwrap();
        let observed = now();
        let product_output = evidence::envelope(
            Some(&rid),
            json!({"results":[product.result]}),
            &context,
            product.evidence,
            product.warnings,
            &observed,
        );
        contracts::validate_output("ozon_get_products", &product_output).unwrap();
        assert_eq!(
            product_output["data"]["results"][0]["product"]["prices"][0]["type"],
            "ozon_card"
        );

        let reviews=normalize_reviews(&json!({"rating":4.9,"totalReviews":1,"reviews":[{"reviewId":"r1","score":5.0,"comment":"ok","pros":null,"cons":null,"date":null,"purchased":false,"variantLabel":null,"photos":[]}],"nextPath":null,"hasNext":false,"refinements":[],"aggregationScope":"specific_sku","warnings":[]}),&mut store,ReviewRequest{research_id:&rid,context_id:"c",product_ref:&product.product_refs[0],sku:"123",source_url:"https://www.ozon.ru/product/headphones-123/reviews/",limit:10,prior_seen_review_keys:&[],prior_seen_ref:None,prior_seen_depth:0,current_path:"",prior_no_progress_pages:0,prior_page_made_progress:false}).unwrap();
        store
            .record(
                &rid,
                "reviews",
                "reviews",
                &reviews.product_refs,
                &reviews.evidence,
            )
            .unwrap();
        let stored = store
            .read(&json!({"researchId":rid,"section":"evidence"}))
            .unwrap();
        assert!(
            stored["payload"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| !e["facts"].as_array().unwrap().is_empty())
        );
        let review_output = evidence::envelope(
            Some(&rid),
            reviews.data,
            &context,
            reviews.evidence,
            reviews.warnings,
            &observed,
        );
        contracts::validate_output("ozon_get_reviews", &review_output).unwrap();
        assert!(review_output["data"]["reviews"][0]["purchaseVerified"].is_null());
    }

    #[test]
    fn review_cursor_drains_captured_page_without_skipping() {
        let (_d, mut store, rid, context) = test_store();
        let product_ref = store
            .put_ref(
                &rid,
                "product",
                &json!({"sku":"123","url":"https://www.ozon.ru/product/item-123/"}),
                None,
            )
            .unwrap();
        let reviews = (0..30)
            .map(|index| {
                json!({"reviewId":format!("r{index}"),"score":5.0,"comment":format!("review-{index}"),"pros":null,"cons":null,"date":null,"purchased":null,"variantLabel":null,"photos":[]})
            })
            .collect::<Vec<_>>();
        let raw = json!({"rating":5.0,"totalReviews":30,"reviews":reviews,"nextPath":null,"hasNext":false,"refinements":[],"aggregationScope":"specific_sku","warnings":[]});
        let mut current = raw;
        let mut emitted = vec![];
        let mut captured_at = None;
        let mut prior_seen_review_keys = vec![];
        let mut prior_seen_ref = None;
        let mut prior_seen_depth = 0;
        for limit in [3, 5, 30] {
            let normalized = normalize_reviews(
                &current,
                &mut store,
                ReviewRequest {
                    research_id: &rid,
                    context_id: "c",
                    product_ref: &product_ref,
                    sku: "123",
                    source_url: "https://www.ozon.ru/product/item-123/reviews/",
                    limit,
                    prior_seen_review_keys: &prior_seen_review_keys,
                    prior_seen_ref: prior_seen_ref.as_deref(),
                    prior_seen_depth,
                    current_path: "",
                    prior_no_progress_pages: 0,
                    prior_page_made_progress: false,
                },
            )
            .unwrap();
            let observed = normalized.evidence[0]["observedAt"].clone();
            assert_eq!(captured_at.get_or_insert(observed.clone()), &observed);
            emitted.extend(
                normalized.data["reviews"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|review| review["text"].as_str().unwrap().to_owned()),
            );
            assert_eq!(normalized.data["coverage"]["uniqueSeen"], emitted.len());
            let output = evidence::envelope(
                Some(&rid),
                normalized.data.clone(),
                &context,
                normalized.evidence,
                normalized.warnings,
                observed.as_str().unwrap(),
            );
            contracts::validate_output("ozon_get_reviews", &output).unwrap();
            match normalized.data["nextCursor"].as_str() {
                Some(cursor) => {
                    let stored = store.get_ref(cursor, "review_cursor").unwrap();
                    assert_eq!(stored.value["capturedAt"], observed);
                    assert_eq!(stored.value["contextId"], "c");
                    prior_seen_ref = stored.value["seenRef"].as_str().map(str::to_owned);
                    prior_seen_review_keys = read_seen_chain(&store, prior_seen_ref.as_deref());
                    prior_seen_depth = prior_seen_ref
                        .as_deref()
                        .map(|reference| {
                            store.get_ref(reference, "review_seen").unwrap().value["depth"]
                                .as_u64()
                                .unwrap() as usize
                        })
                        .unwrap_or(0);
                    current = stored.value["cachedRaw"].clone();
                }
                None => break,
            }
        }
        assert_eq!(
            emitted,
            (0..30)
                .map(|index| format!("review-{index}"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn all_duplicate_review_page_advances_without_growing_seen_chain() {
        let (_directory, mut store, research_id, _context) = test_store();
        let first = normalize_reviews(
            &json!({"rating":5.0,"totalReviews":2,"reviews":[
                {"reviewId":"r1","score":5.0,"comment":"one","photos":[]},
                {"reviewId":"r2","score":5.0,"comment":"two","photos":[]}
            ],"nextPath":"/product/item-123/reviews/?page=2","hasNext":true,"refinements":[],"aggregationScope":"specific_sku","warnings":[]}),
            &mut store,
            ReviewRequest {
                research_id: &research_id,
                context_id: "c",
                product_ref: "ref_product",
                sku: "123",
                source_url: "https://www.ozon.ru/product/item-123/reviews/",
                limit: 10,
                prior_seen_review_keys: &[],
                prior_seen_ref: None,
                prior_seen_depth: 0,
                current_path: "",
                prior_no_progress_pages: 0,
                prior_page_made_progress: false,
            },
        )
        .unwrap();
        let cursor = store
            .get_ref(first.data["nextCursor"].as_str().unwrap(), "review_cursor")
            .unwrap();
        let seen_ref = cursor.value["seenRef"].as_str().unwrap().to_owned();
        let seen = read_seen_chain(&store, Some(&seen_ref));

        let duplicate = normalize_reviews(
            &json!({"rating":5.0,"totalReviews":2,"reviews":[
                {"reviewId":"r1","score":5.0,"comment":"one","photos":[]},
                {"reviewId":"r2","score":5.0,"comment":"two","photos":[]}
            ],"nextPath":"/product/item-123/reviews/?page=3","hasNext":true,"refinements":[],"aggregationScope":"specific_sku","warnings":[]}),
            &mut store,
            ReviewRequest {
                research_id: &research_id,
                context_id: "c",
                product_ref: "ref_product",
                sku: "123",
                source_url: "https://www.ozon.ru/product/item-123/reviews/",
                limit: 10,
                prior_seen_review_keys: &seen,
                prior_seen_ref: Some(&seen_ref),
                prior_seen_depth: 1,
                current_path: "/product/item-123/reviews/?page=2",
                prior_no_progress_pages: 0,
                prior_page_made_progress: false,
            },
        )
        .unwrap();
        assert!(duplicate.data["reviews"].as_array().unwrap().is_empty());
        assert_eq!(duplicate.data["coverage"]["uniqueSeen"], 2);
        let cursor = store
            .get_ref(
                duplicate.data["nextCursor"].as_str().unwrap(),
                "review_cursor",
            )
            .unwrap();
        assert_eq!(cursor.value["seenRef"], seen_ref);
    }

    #[test]
    fn identical_idless_review_rows_remain_distinct_across_pages() {
        let (_directory, mut store, research_id, _context) = test_store();
        let row = json!({"reviewId":null,"score":5.0,"comment":"same","photos":[]});
        let first = normalize_reviews(
            &json!({"rating":5.0,"totalReviews":4,"reviews":[row.clone(),row.clone()],"nextPath":"/product/item-123/reviews/?page=2","hasNext":true,"refinements":[],"aggregationScope":"specific_sku","warnings":[]}),
            &mut store,
            ReviewRequest {
                research_id: &research_id,
                context_id: "c",
                product_ref: "ref_product",
                sku: "123",
                source_url: "https://www.ozon.ru/product/item-123/reviews/",
                limit: 10,
                prior_seen_review_keys: &[],
                prior_seen_ref: None,
                prior_seen_depth: 0,
                current_path: "",
                prior_no_progress_pages: 0,
                prior_page_made_progress: false,
            },
        )
        .unwrap();
        assert_eq!(first.data["reviews"].as_array().unwrap().len(), 2);
        assert_eq!(first.data["coverage"]["uniqueSeen"], 2);
        let cursor = store
            .get_ref(first.data["nextCursor"].as_str().unwrap(), "review_cursor")
            .unwrap();
        let seen_ref = cursor.value["seenRef"].as_str().unwrap().to_owned();
        let seen = read_seen_chain(&store, Some(&seen_ref));
        let depth = store.get_ref(&seen_ref, "review_seen").unwrap().value["depth"]
            .as_u64()
            .unwrap() as usize;

        let second = normalize_reviews(
            &json!({"rating":5.0,"totalReviews":4,"reviews":[row.clone(),row],"nextPath":null,"hasNext":false,"refinements":[],"aggregationScope":"specific_sku","warnings":[]}),
            &mut store,
            ReviewRequest {
                research_id: &research_id,
                context_id: "c",
                product_ref: "ref_product",
                sku: "123",
                source_url: "https://www.ozon.ru/product/item-123/reviews/",
                limit: 10,
                prior_seen_review_keys: &seen,
                prior_seen_ref: Some(&seen_ref),
                prior_seen_depth: depth,
                current_path: "/product/item-123/reviews/?page=2",
                prior_no_progress_pages: 0,
                prior_page_made_progress: false,
            },
        )
        .unwrap();
        assert_eq!(second.data["reviews"].as_array().unwrap().len(), 2);
        assert_eq!(second.data["coverage"]["uniqueSeen"], 4);
    }

    #[test]
    fn duplicate_review_cycles_stop_without_hidden_writes_and_progress_resets_bound() {
        let (directory, mut store, research_id, _context) = test_store();
        let duplicate = json!({"reviewId":"r1","score":5.0,"comment":"same","photos":["https://ir.ozone.ru/a.jpg"]});
        let seen = vec![review_identity(&duplicate).unwrap()];
        let refinements = (0..100)
            .map(|index| json!({"label":format!("f{index}"),"url":format!("/product/item-123/reviews/?f={index}")}))
            .collect::<Vec<_>>();
        let bytes = || {
            std::fs::read_dir(directory.path())
                .unwrap()
                .map(|entry| entry.unwrap().metadata().unwrap().len())
                .sum::<u64>()
        };
        let before = bytes();
        let same_path = normalize_reviews(
            &json!({"reviews":[duplicate.clone()],"nextPath":"/product/item-123/reviews/?page=1","hasNext":true,"refinements":refinements,"warnings":[]}),
            &mut store,
            ReviewRequest {
                research_id: &research_id, context_id: "c", product_ref: "ref_product", sku: "123",
                source_url: "https://www.ozon.ru/product/item-123/reviews/", limit: 10,
                prior_seen_review_keys: &seen, prior_seen_ref: None, prior_seen_depth: 0,
                current_path: "/product/item-123/reviews/?page=1", prior_no_progress_pages: 0,
                prior_page_made_progress: false,
            },
        )
        .err()
        .unwrap();
        assert!(same_path.to_string().starts_with("SOURCE_CHANGED:"));
        assert_eq!(bytes(), before);

        let page_a = normalize_reviews(
            &json!({"reviews":[duplicate.clone()],"nextPath":"/product/item-123/reviews/?page=b","hasNext":true,"refinements":[],"warnings":[]}),
            &mut store,
            ReviewRequest {
                research_id: &research_id, context_id: "c", product_ref: "ref_product", sku: "123",
                source_url: "https://www.ozon.ru/product/item-123/reviews/", limit: 10,
                prior_seen_review_keys: &seen, prior_seen_ref: None, prior_seen_depth: 0,
                current_path: "/product/item-123/reviews/?page=a", prior_no_progress_pages: 0,
                prior_page_made_progress: false,
            },
        )
        .unwrap();
        let cursor_a = store
            .get_ref(page_a.data["nextCursor"].as_str().unwrap(), "review_cursor")
            .unwrap();
        assert_eq!(cursor_a.value["noProgressPages"], 1);
        let page_b = normalize_reviews(
            &json!({"reviews":[duplicate.clone()],"nextPath":"/product/item-123/reviews/?page=a","hasNext":true,"refinements":[],"warnings":[]}),
            &mut store,
            ReviewRequest {
                research_id: &research_id, context_id: "c", product_ref: "ref_product", sku: "123",
                source_url: "https://www.ozon.ru/product/item-123/reviews/", limit: 10,
                prior_seen_review_keys: &seen, prior_seen_ref: None, prior_seen_depth: 0,
                current_path: "/product/item-123/reviews/?page=b", prior_no_progress_pages: 1,
                prior_page_made_progress: false,
            },
        )
        .unwrap();
        assert_eq!(page_b.data["coverage"]["returned"], 0);
        let cycle = normalize_reviews(
            &json!({"reviews":[duplicate],"nextPath":"/product/item-123/reviews/?page=b","hasNext":true,"refinements":[],"warnings":[]}),
            &mut store,
            ReviewRequest {
                research_id: &research_id, context_id: "c", product_ref: "ref_product", sku: "123",
                source_url: "https://www.ozon.ru/product/item-123/reviews/", limit: 10,
                prior_seen_review_keys: &seen, prior_seen_ref: None, prior_seen_depth: 0,
                current_path: "/product/item-123/reviews/?page=a", prior_no_progress_pages: 2,
                prior_page_made_progress: false,
            },
        )
        .err()
        .unwrap();
        assert!(cycle.to_string().starts_with("SOURCE_CHANGED:"));

        let progress = normalize_reviews(
            &json!({"reviews":[{"reviewId":"r2","score":5.0,"comment":"new","photos":[]}],"nextPath":"/product/item-123/reviews/?page=c","hasNext":true,"refinements":[],"warnings":[]}),
            &mut store,
            ReviewRequest {
                research_id: &research_id, context_id: "c", product_ref: "ref_product", sku: "123",
                source_url: "https://www.ozon.ru/product/item-123/reviews/", limit: 10,
                prior_seen_review_keys: &seen, prior_seen_ref: None, prior_seen_depth: 0,
                current_path: "/product/item-123/reviews/?page=b", prior_no_progress_pages: 2,
                prior_page_made_progress: false,
            },
        )
        .unwrap();
        let progress_cursor = store
            .get_ref(
                progress.data["nextCursor"].as_str().unwrap(),
                "review_cursor",
            )
            .unwrap();
        assert_eq!(progress_cursor.value["noProgressPages"], 0);
    }

    #[test]
    fn large_small_limit_review_seen_chain_stays_bounded_and_under_quota() {
        let (directory, mut store, research_id, _context) = test_store();
        let mut parent = None;
        let mut last_cursor = None;
        let mut all_keys = BTreeSet::new();
        let mut prior_seen_depth = 0;
        let total = 23_091usize;
        for start in (0..total).step_by(10) {
            let end = (start + 10).min(total);
            let keys = (start..end)
                .map(|index| format!("id:{index:064x}"))
                .collect::<Vec<_>>();
            all_keys.extend(keys.iter().cloned());
            let (next_parent, next_depth) = store_review_seen_state(
                &mut store,
                &research_id,
                parent.as_deref(),
                prior_seen_depth,
                &keys,
                &all_keys,
            )
            .unwrap();
            parent = Some(next_parent);
            prior_seen_depth = next_depth;
            last_cursor = Some(
                store
                .put_ref(
                    &research_id,
                    "review_cursor",
                    &json!({"path":"/product/item-123/reviews/?page=2","seenRef":parent.as_deref()}),
                    Some(1800),
                )
                .unwrap(),
            );
        }
        assert_eq!(read_seen_chain(&store, parent.as_deref()).len(), total);
        let cursor = store
            .get_ref(last_cursor.as_deref().unwrap(), "review_cursor")
            .unwrap();
        assert!(cursor.value.get("seenReviewIds").is_none());
        assert!(serde_json::to_vec(&cursor.value).unwrap().len() < 256);
        let bytes = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum::<u64>();
        eprintln!("review_seen_storage identities={total} bytes={bytes}");
        assert!(bytes < 32 * 1024 * 1024, "seen chain used {bytes} bytes");
    }

    #[test]
    fn context_registry_is_complete_and_contract_valid() {
        let raw = json!({"contextId":"c","regionLabel":null,"regionVerification":"unverified","accountState":"unknown","accessState":"available","capabilities":{"search":"available","variants":"unsupported","review_images":"unverified","image_content":"unverified"}});
        let n = normalize_context(&raw);
        let observed = n.data["observedAt"].as_str().unwrap().to_owned();
        let output = evidence::envelope(None, n.data, &raw, n.evidence, n.warnings, &observed);
        contracts::validate_output("ozon_get_context", &output).unwrap();
        assert_eq!(output["data"]["capabilities"].as_array().unwrap().len(), 17);
    }

    #[test]
    fn characteristics_overflow_has_a_bound_continuation() {
        let (_d, mut store, rid, context) = test_store();
        let mut characteristics = Map::new();
        for index in 0..51 {
            characteristics.insert(format!("field-{index:02}"), json!(format!("value-{index}")));
        }
        let raw = json!({"sku":"123","name":"x","url":"https://www.ozon.ru/product/x-123/","cardPrice":null,"priceRegular":1.0,"available":true,"rating":null,"reviews":null,"seller":null,"images":[],"characteristics":characteristics,"description":{"text":"","images":[]},"variants":{"status":"unsupported","items":[],"hasNext":null,"nextPath":null},"offers":{"status":"unsupported","items":[],"hasNext":null,"nextPath":null},"warnings":[]});
        let first = normalize_product(
            &raw,
            &json!({"sku":"123"}),
            &mut store,
            &rid,
            "c",
            &["characteristics".into()],
        )
        .unwrap();
        let section = &first.result["product"]["characteristics"];
        assert_eq!(section["hasNext"], true);
        let first_observed = first.evidence[0]["observedAt"].clone();
        let observed = first_observed.as_str().unwrap().to_owned();
        let first_output = evidence::envelope(
            Some(&rid),
            json!({"results":[first.result.clone()]}),
            &context,
            first.evidence.clone(),
            first.warnings.clone(),
            &observed,
        );
        contracts::validate_output("ozon_get_products", &first_output).unwrap();
        let cursor = section["nextCursor"].as_str().unwrap();
        let stored = store.get_ref(cursor, "product_cursor").unwrap();
        assert_eq!(stored.value["capturedAt"], first_observed);
        assert_eq!(stored.value["contextId"], "c");
        let second = normalize_product(
            &stored.value["cachedRaw"],
            &json!({"cursor":cursor}),
            &mut store,
            &rid,
            "c",
            &["characteristics".into()],
        )
        .unwrap();
        assert_eq!(
            second.result["product"]["characteristics"]["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            second.result["product"]["productRef"],
            first.result["product"]["productRef"]
        );
        assert_eq!(second.evidence[0]["observedAt"], first_observed);
        let second_output = evidence::envelope(
            Some(&rid),
            json!({"results":[second.result]}),
            &context,
            second.evidence,
            second.warnings,
            &observed,
        );
        contracts::validate_output("ozon_get_products", &second_output).unwrap();
        assert_eq!(second_output["observedAt"], first_observed);
    }
    #[test]
    fn empty_description_is_unverified_instead_of_available() {
        let (_directory, mut store, research_id, _context) = test_store();
        let raw = json!({
            "sku": "123",
            "name": "x",
            "url": "https://www.ozon.ru/product/x-123/",
            "cardPrice": null,
            "priceRegular": 1.0,
            "available": true,
            "rating": null,
            "reviews": null,
            "seller": null,
            "images": [],
            "characteristics": {},
            "description": {"text": "", "images": []},
            "variants": {"status": "unsupported", "items": [], "hasNext": null, "nextPath": null},
            "offers": {"status": "unsupported", "items": [], "hasNext": null, "nextPath": null},
            "warnings": []
        });

        let product = normalize_product(
            &raw,
            &json!({"sku": "123"}),
            &mut store,
            &research_id,
            "c",
            &["description".into()],
        )
        .unwrap();

        assert_eq!(
            product.result["product"]["description"]["status"],
            "unknown"
        );
        assert!(product.result["product"]["description"]["text"].is_null());
        assert!(product.result["product"]["description"]["hasNext"].is_null());
        assert!(product.result["product"]["description"]["nextCursor"].is_null());
    }

    #[test]
    fn observed_review_timestamp_survives_parse_and_normalization() {
        let page = json!({"widgetStates": {
            "webListReviews-0": {"reviews": [{
                "uuid": "review-1",
                "content": {"score": 5},
                "publishedAt": 1704067200
            }]}
        }});
        let raw = serde_json::to_value(crate::parse::parse_reviews(&page, 10)).unwrap();
        let (_directory, mut store, research_id, _context) = test_store();
        let normalized = normalize_reviews(
            &raw,
            &mut store,
            ReviewRequest {
                research_id: &research_id,
                context_id: "c",
                product_ref: "ref_product",
                sku: "123",
                source_url: "https://www.ozon.ru/product/x-123/reviews/",
                limit: 10,
                prior_seen_review_keys: &[],
                prior_seen_ref: None,
                prior_seen_depth: 0,
                current_path: "",
                prior_no_progress_pages: 0,
                prior_page_made_progress: false,
            },
        )
        .unwrap();

        assert_eq!(
            normalized.data["reviews"][0]["publishedAt"],
            "2024-01-01T00:00:00+00:00"
        );
    }
}
