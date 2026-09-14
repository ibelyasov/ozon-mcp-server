//! Bounded, stateless continuation over observed public search pages.
//! Paginator links are promoted from widget-fragment to full-document requests
//! by removing only paginator_token, layout_page_index and layout_container.
//! Observed page numbers and semantic continuation state remain unchanged.
use crate::{
    model::{
        ActiveFilter, ActiveFilterValue, Facet, FacetOption, FacetRange, Facets, NumericValue,
        PriceRangeAssessment, PriceRangeBasis, SearchContext, SearchCoverage, SearchItem,
        SearchResponse, SortOption, Warning,
    },
    page_source::PageSource,
    parse,
    response::{METADATA_BUDGET_UNITS, serialized_utf16_len},
    widgets::WidgetSet,
};
use anyhow::{Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use tokio_util::sync::CancellationToken;

const MAX_URL: usize = 24_000;
const MAX_CURSOR: usize = 48_000;
const MAX_SEEN: usize = 500;
const MAX_CALLS: usize = 100;
const MAX_OFFSET: usize = 10000;

fn search_limit() -> usize {
    12
}

fn sort_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "enum": ["popular", "price", "price_desc", "rating", "new", "discount"]
    })
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchArgs {
    #[schemars(length(min = 1, max = 2000))]
    pub query: Option<String>,
    #[schemars(length(min = 1, max = 24000))]
    pub search_url: Option<String>,
    #[schemars(length(min = 1, max = 48000))]
    pub next_cursor: Option<String>,
    pub include_facets: Option<bool>,
    #[schemars(schema_with = "sort_schema")]
    pub sort: Option<String>,
    #[schemars(range(min = 0, max = 9_007_199_254_740_991_u64))]
    pub price_min: Option<u64>,
    #[schemars(range(min = 0, max = 9_007_199_254_740_991_u64))]
    pub price_max: Option<u64>,
    #[serde(default = "search_limit")]
    #[schemars(range(min = 1, max = 36))]
    pub limit: usize,
}

fn safe_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 100
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Accept public GET search/category routes only; validate before URL normalization.
pub fn normalize_search_url(input: &str) -> Result<String> {
    ensure!(
        input.len() <= MAX_URL && !input.chars().any(|c| c.is_control() || c == '\\'),
        "Invalid search URL"
    );
    ensure!(
        input.starts_with("https://www.ozon.ru/"),
        "Expected https://www.ozon.ru search/category URL"
    );
    let raw_path = input
        .split('?')
        .next()
        .unwrap_or(input)
        .trim_start_matches("https://www.ozon.ru");
    ensure!(
        !raw_path.contains('%')
            && !raw_path.contains("//")
            && !raw_path.split('/').any(|s| s == "." || s == ".."),
        "Invalid search path"
    );
    let mut url = url::Url::parse(input)?;
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("www.ozon.ru")
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none()
            && url.fragment().is_none(),
        "Invalid search URL authority"
    );
    let path = url.path();
    ensure!(
        path == "/search/"
            || (path.starts_with("/category/")
                && path.ends_with('/')
                && path.len() > 10
                && path[10..path.len() - 1].split('/').all(|s| !s.is_empty()
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))),
        "Expected public search/category route"
    );
    let mut keys = HashSet::new();
    let pairs: Vec<_> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    ensure!(pairs.len() <= 100, "Too many search parameters");
    for (key, value) in &pairs {
        ensure!(
            safe_key(key)
                && keys.insert(key.clone())
                && (if key == "text" {
                    value.encode_utf16().count() <= 2000
                } else {
                    value.len() <= 4096
                })
                && !value.chars().any(char::is_control),
            "Invalid or duplicate search parameter"
        );
        ensure!(
            !matches!(
                key.as_str(),
                "url"
                    | "redirect"
                    | "redirect_uri"
                    | "return_url"
                    | "callback"
                    | "action"
                    | "method"
                    | "endpoint"
            ),
            "Unsupported search parameter"
        );
    }
    url.set_query(None);
    for (key, value) in pairs {
        if !matches!(key.as_str(), "__rr" | "at") {
            url.query_pairs_mut().append_pair(&key, &value);
        }
    }
    Ok(url.into())
}

fn observed_url(value: &str) -> Option<String> {
    let absolute = if value.starts_with('/') && !value.starts_with("//") {
        format!("https://www.ozon.ru{value}")
    } else {
        value.into()
    };
    normalize_search_url(&absolute).ok()
}
fn paginator_url(value: &str) -> Option<String> {
    let observed = observed_url(value)?;
    let mut u = url::Url::parse(&observed).ok()?;
    let pairs: Vec<_> = u
        .query_pairs()
        .filter(|(k, _)| {
            !matches!(
                k.as_ref(),
                "paginator_token" | "layout_page_index" | "layout_container"
            )
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    u.set_query(None);
    for (k, v) in pairs {
        u.query_pairs_mut().append_pair(&k, &v);
    }
    normalize_search_url(u.as_str()).ok()
}

fn paging(key: &str) -> bool {
    matches!(
        key,
        "page"
            | "paginator_token"
            | "search_page_state"
            | "layout_page_index"
            | "layout_container"
            | "start_page_id"
    )
}
fn refine(base: &str, key: &str, value: Option<&str>) -> Option<String> {
    if !safe_key(key) {
        return None;
    }
    let mut u = url::Url::parse(base).ok()?;
    let pairs: Vec<_> = u
        .query_pairs()
        .filter(|(k, _)| !paging(k) && k != key)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    u.set_query(None);
    for (k, v) in pairs {
        u.query_pairs_mut().append_pair(&k, &v);
    }
    if let Some(v) = value {
        u.query_pairs_mut().append_pair(key, v);
    }
    normalize_search_url(u.as_str()).ok()
}
fn reset(base: &str) -> Option<String> {
    refine(base, "page", None)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedSearchRequest {
    version: u8,
    url: String,
    offset: usize,
    seen: Vec<String>,
    calls: usize,
}
impl PreparedSearchRequest {
    fn path(&self) -> String {
        self.url
            .trim_start_matches("https://www.ozon.ru")
            .to_owned()
    }
}
fn prepare_request(args: &SearchArgs) -> Result<PreparedSearchRequest> {
    ensure!(
        (1..=36).contains(&args.limit),
        "limit must be between 1 and 36"
    );
    if let Some(cursor) = &args.next_cursor {
        ensure!(
            args.query.is_none()
                && args.search_url.is_none()
                && args.sort.is_none()
                && args.price_min.is_none()
                && args.price_max.is_none(),
            "nextCursor cannot be combined with query, searchUrl, sort or price"
        );
        ensure!(cursor.len() <= MAX_CURSOR, "Cursor too large");
        let mut request: PreparedSearchRequest = serde_json::from_str(cursor)?;
        ensure!(
            request.version == 1
                && request.offset <= MAX_OFFSET
                && request.calls < MAX_CALLS
                && request.seen.len() < MAX_SEEN,
            "Invalid or exhausted cursor"
        );
        ensure!(
            request
                .seen
                .iter()
                .all(|s| !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_digit())),
            "Invalid cursor SKUs"
        );
        ensure!(
            request.seen.iter().collect::<HashSet<_>>().len() == request.seen.len(),
            "Duplicate cursor SKUs"
        );
        request.url = normalize_search_url(&request.url)?;
        return Ok(request);
    }
    ensure!(
        args.query.is_some() != args.search_url.is_some(),
        "Provide exactly one of query, searchUrl or nextCursor"
    );
    let mut current = if let Some(q) = &args.query {
        ensure!(
            !q.trim().is_empty() && q.encode_utf16().count() <= 2000,
            "query must contain 1–2000 characters"
        );
        let mut u = url::Url::parse("https://www.ozon.ru/search/")?;
        u.query_pairs_mut()
            .append_pair("text", q.trim())
            .append_pair("from_global", "true");
        u.to_string()
    } else {
        normalize_search_url(args.search_url.as_deref().unwrap())?
    };
    if let Some(sort) = &args.sort {
        ensure!(
            matches!(
                sort.as_str(),
                "popular" | "price" | "price_desc" | "rating" | "new" | "discount"
            ),
            "Invalid sort"
        );
        current = refine(
            &current,
            "sorting",
            if sort == "popular" { None } else { Some(sort) },
        )
        .ok_or_else(|| anyhow::anyhow!("Invalid sorting URL"))?;
    }
    for p in [args.price_min, args.price_max].into_iter().flatten() {
        ensure!(
            p <= 9_007_199_254_740_991,
            "Prices must be nonnegative safe integers"
        );
    }
    if let (Some(a), Some(b)) = (args.price_min, args.price_max) {
        ensure!(a <= b, "priceMin must not exceed priceMax");
    }
    if args.price_min.is_some() || args.price_max.is_some() {
        let min = args.price_min.unwrap_or(0);
        let max = args.price_max.unwrap_or(min.max(9_999_999_900));
        current = refine(
            &current,
            "currency_price",
            Some(&format!(
                "{}.{:02}0;{}.{:02}0",
                min / 100,
                min % 100,
                max / 100,
                max % 100
            )),
        )
        .ok_or_else(|| anyhow::anyhow!("Invalid price URL"))?;
    }
    Ok(PreparedSearchRequest {
        version: 1,
        url: normalize_search_url(&current)?,
        offset: 0,
        seen: vec![],
        calls: 0,
    })
}

pub struct PreparedSearch {
    args: SearchArgs,
    request: PreparedSearchRequest,
}

impl PreparedSearch {
    pub fn prepare(args: SearchArgs) -> Result<Self> {
        let request = prepare_request(&args)?;
        Ok(Self { args, request })
    }

    pub async fn execute(
        self,
        source: &mut impl PageSource,
        cancel: &CancellationToken,
    ) -> Result<SearchResponse> {
        ensure!(!cancel.is_cancelled(), "Request cancelled");
        let page = source.fetch_json(&self.request.path(), cancel).await?;
        ensure!(!cancel.is_cancelled(), "Request cancelled");
        let mut result = finish(&page, &self.args, self.request)?;
        if !WidgetSet::new(&page).has_matching("tileGridDesktop", |value| {
            value.get("items").is_some_and(Value::is_array)
        }) {
            result.warnings.push(Warning::SearchWidgetMissing);
        }
        Ok(result)
    }
}

fn label(v: &Value) -> Option<String> {
    v.as_str()
        .or_else(|| v["text"].as_str())
        .map(|s| s.chars().take(500).collect())
}
fn items(v: &Value) -> Vec<&Value> {
    v["sections"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|s| s["items"].as_array().into_iter().flatten())
        .collect()
}
// A selected brand may be encoded in the final category path segment.
fn refinement_base(base: &str, key: &str, selected: &[String]) -> String {
    if key != "brand" {
        return base.to_owned();
    }
    let Ok(mut u) = url::Url::parse(base) else {
        return base.to_owned();
    };
    let path = u.path().trim_end_matches('/');
    let Some((parent, segment)) = path.rsplit_once('/') else {
        return base.to_owned();
    };
    if parent.starts_with("/category/")
        && selected
            .iter()
            .any(|id| segment.ends_with(&format!("-{id}")))
    {
        let path = format!("{parent}/");
        u.set_path(&path);
    }
    u.into()
}
fn disable_link(page: &Value, key: &str, title: &Value) -> Option<String> {
    let w = WidgetSet::new(page).first_matching("searchResultsFiltersActive", |value| {
        value.get("activeFilters").is_some_and(Value::is_array)
    })?;
    let target = label(title);
    w["activeFilters"]
        .as_array()?
        .iter()
        .filter(|f| f["key"].as_str() == Some(key))
        .flat_map(|f| f["activeValues"].as_array().into_iter().flatten())
        .find(|v| label(&v["title"]) == target)?["disableUri"]
        .as_str()
        .and_then(observed_url)
        .and_then(|u| reset(&u))
}

fn numeric(value: &Value) -> Option<NumericValue> {
    let number = value.as_number()?;
    if let Some(value) = number.as_u64() {
        Some(NumericValue::Unsigned(value))
    } else if let Some(value) = number.as_i64() {
        Some(NumericValue::Signed(value))
    } else {
        number.as_f64().map(NumericValue::Float)
    }
}

fn facets(page: &Value, base: &str) -> Option<Facets> {
    let w = WidgetSet::new(page).first_matching("filtersDesktop", |value| {
        value.get("sections").is_some_and(Value::is_array)
    })?;
    let filters: Vec<_> = w["sections"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|s| s["filters"].as_array().into_iter().flatten())
        .collect();
    let mut out = vec![];
    let mut bytes = 0;
    let mut truncated = false;
    for filter in filters {
        let kind = filter["type"].as_str().unwrap_or("");
        if !matches!(
            kind,
            "categoryFilter"
                | "boolFilter"
                | "checkboxesFilter"
                | "multipleRangesFilter"
                | "rangeFilter"
                | "colorFilter"
        ) {
            continue;
        }
        let key = filter["key"].as_str().unwrap_or("");
        if !safe_key(key) {
            continue;
        }
        let body = &filter[kind];
        let title_body = if kind == "multipleRangesFilter" {
            &body["rangeFilter"]
        } else {
            body
        };
        let mut facet = Facet {
            kind: kind.to_owned(),
            key: key.to_owned(),
            title: label(&title_body["title"]),
            description: label(&title_body["description"]),
            range: None,
            selected: None,
            search_url: None,
            options: None,
            options_truncated: None,
            is_radio: None,
            has_more_values: None,
        };
        if matches!(kind, "multipleRangesFilter" | "rangeFilter") {
            facet.range = Some(FacetRange {
                min_value: numeric(&title_body["minValue"]),
                max_value: numeric(&title_body["maxValue"]),
                from_value: numeric(&title_body["fromValue"]),
                to_value: numeric(&title_body["toValue"]),
            });
        }
        let mut options = vec![];
        let mut count = 0;
        if kind == "boolFilter" {
            let selected = body["isSelected"].as_bool().unwrap_or(false);
            facet.selected = Some(selected);
            facet.search_url = Some(refine(
                base,
                key,
                if selected { None } else { Some("true") },
            ));
        } else if kind == "categoryFilter" {
            for c in body["categories"].as_array().into_iter().flatten() {
                count += 1;
                if options.len() < 20 {
                    options.push(FacetOption {
                        value: None,
                        label: label(&c["title"]),
                        selected: c["isActive"].as_bool().unwrap_or(false),
                        level: Some(numeric(&c["level"])),
                        search_url: c["urlValue"]
                            .as_str()
                            .and_then(observed_url)
                            .and_then(|u| reset(&u)),
                    });
                }
            }
        } else {
            let checks = if kind == "multipleRangesFilter" {
                &body["checkboxesFilter"]
            } else {
                body
            };
            let mut all = items(checks);
            all.extend(checks["colorIcons"].as_array().into_iter().flatten());
            let radio = checks["isRadio"].as_bool().unwrap_or(false);
            let mut selected: Vec<String> = all
                .iter()
                .filter(|i| i["isSelected"].as_bool() == Some(true))
                .filter_map(|i| i["key"].as_str().map(str::to_owned))
                .collect();
            // Preserve selections hidden by collapsed widget sections.
            if let Ok(u) = url::Url::parse(base) {
                for (_, v) in u.query_pairs().filter(|(k, _)| k == key) {
                    for v in v.split(',') {
                        if !selected.iter().any(|s| s == v) {
                            selected.push(v.into());
                        }
                    }
                }
            }
            let option_base = refinement_base(base, key, &selected);
            for item in all {
                let Some(value) = item["key"].as_str() else {
                    continue;
                };
                count += 1;
                if options.len() >= 20 {
                    continue;
                }
                let active = item["isSelected"].as_bool().unwrap_or(false);
                let mut values = if radio { vec![] } else { selected.clone() };
                if active {
                    values.retain(|v| v != value);
                } else {
                    values.push(value.into());
                }
                let joined = values.join(",");
                let target = if active {
                    disable_link(page, key, item.get("title").unwrap_or(&item["description"]))
                } else {
                    None
                }
                .or_else(|| {
                    refine(
                        &option_base,
                        key,
                        if joined.is_empty() {
                            None
                        } else {
                            Some(&joined)
                        },
                    )
                });
                options.push(FacetOption {
                    value: Some(value.to_owned()),
                    label: label(item.get("title").unwrap_or(&item["description"])),
                    selected: active,
                    level: None,
                    search_url: target,
                });
            }
            facet.is_radio = Some(radio);
            facet.has_more_values = Some(
                checks["hasManyValues"].as_bool().unwrap_or(false)
                    || checks["openingButtons"].get("showAllButton").is_some(),
            );
        }
        if kind != "boolFilter" {
            facet.options = Some(options);
            facet.options_truncated = Some(count > 20);
        }
        let size = serde_json::to_string(&facet).unwrap_or_default().len();
        if out.len() >= 40 || bytes + size > 24000 {
            truncated = true;
            break;
        }
        bytes += size;
        out.push(facet);
    }
    Some(Facets {
        items: out,
        truncated,
    })
}

fn annotate_price_range(products: &mut [SearchItem], request_url: &url::Url) -> Option<bool> {
    let bounds = request_url
        .query_pairs()
        .find(|(k, _)| k == "currency_price")
        .and_then(|(_, value)| {
            let (min, max) = value.split_once(';')?;
            let min = min.parse::<f64>().ok()?;
            let max = max.parse::<f64>().ok()?;
            (min.is_finite() && max.is_finite() && min >= 0.0 && max >= min).then_some((min, max))
        });
    let mut outside = false;
    for product in products {
        let matches = bounds.and_then(|(min, max)| {
            product
                .price
                .filter(|p| p.is_finite())
                .map(|p| p >= min && p <= max)
        });
        outside |= matches == Some(false);
        product.matches_price_range = Some(matches);
    }
    bounds.map(|_| outside)
}

fn trim_metadata(result: &mut SearchResponse) -> Result<()> {
    if serialized_utf16_len(result)? <= METADATA_BUDGET_UNITS {
        return Ok(());
    }
    let has_metadata = result
        .facets
        .as_ref()
        .and_then(Option::as_ref)
        .is_some_and(|facets| !facets.items.is_empty())
        || result
            .active_filters
            .as_ref()
            .is_some_and(|filters| !filters.is_empty())
        || result
            .sort_options
            .as_ref()
            .is_some_and(|options| !options.is_empty());
    if has_metadata {
        result.facets = Some(Some(Facets {
            items: Vec::new(),
            truncated: true,
        }));
        result.active_filters = Some(Vec::new());
        result.sort_options = Some(Vec::new());
        result.warnings.push(Warning::SearchMetadataTruncated);
    }
    Ok(())
}

fn finish(
    page: &Value,
    args: &SearchArgs,
    mut request: PreparedSearchRequest,
) -> Result<SearchResponse> {
    let all = parse::parse_search_items(page);
    let start = request.offset;
    let mut seen: HashSet<_> = request.seen.iter().cloned().collect();
    let mut products = vec![];
    let mut offset = start.min(all.len());
    while offset < all.len() && products.len() < args.limit && seen.len() < MAX_SEEN {
        let item = &all[offset];
        offset += 1;
        let sku = item.sku.clone();
        if !sku.is_empty()
            && sku.len() <= 32
            && sku.bytes().all(|b| b.is_ascii_digit())
            && seen.insert(sku.clone())
        {
            request.seen.push(sku);
            products.push(item.clone());
        }
    }
    let paginator = WidgetSet::new(page).first_matching("infiniteVirtualPaginator", |value| {
        value.get("nextPage").is_some_and(Value::is_string)
    });
    let observed_next = paginator
        .as_ref()
        .and_then(|p| p["nextPage"].as_str())
        .filter(|s| !s.is_empty());
    let next_url = observed_next.and_then(paginator_url);
    let stalled = next_url.as_ref() == Some(&request.url);
    let next_url = next_url.filter(|_| !stalled);
    let has_next = if offset < all.len() || next_url.is_some() {
        Some(true)
    } else if paginator.as_ref().and_then(|p| p["nextPage"].as_str()) == Some("") {
        Some(false)
    } else {
        None
    };
    // Sort links carry canonical category/brand selections, but reset paging.
    // Use them only as refinement bases, never as continuation locations.
    let sort_shape = |value: &Value| {
        value
            .pointer("/sortButton/options")
            .is_some_and(Value::is_array)
    };
    let facet_base = WidgetSet::new(page)
        .first_matching("searchResultsSort", sort_shape)
        .and_then(|w| {
            w["sortButton"]["options"]
                .as_array()
                .and_then(|a| a.iter().find(|o| o["isSelected"].as_bool() == Some(true)))
                .and_then(|o| o["action"]["link"].as_str())
                .and_then(observed_url)
        })
        .unwrap_or_else(|| request.url.clone());
    let current = request.url.clone();
    let page_num = url::Url::parse(&current)?
        .query_pairs()
        .find(|(k, _)| k == "page")
        .and_then(|(_, v)| v.parse::<u64>().ok())
        .unwrap_or(1);
    request.calls += 1;
    let mut warnings = vec![Warning::SearchResultsMayChangeBetweenCalls];
    if stalled {
        warnings.push(Warning::ContinuationStalled);
    }
    if observed_next.is_some() && next_url.is_none() && !stalled {
        warnings.push(Warning::UnsafePaginatorUrlIgnored);
    }
    let capped = seen.len() >= MAX_SEEN || request.calls >= MAX_CALLS || offset > MAX_OFFSET;
    let next_cursor = if has_next == Some(true) && !capped {
        if offset < all.len() {
            request.offset = offset;
        } else {
            request.url = next_url.unwrap();
            request.offset = 0;
        }
        let encoded = serde_json::to_string(&request)?;
        if encoded.len() <= MAX_CURSOR {
            Some(encoded)
        } else {
            warnings.push(Warning::ContinuationLimitReached);
            None
        }
    } else {
        if capped && has_next == Some(true) {
            warnings.push(Warning::ContinuationLimitReached);
        }
        None
    };
    let u = url::Url::parse(&current)?;
    let query = u
        .query_pairs()
        .find(|(k, _)| k == "text")
        .map(|(_, v)| v.into_owned());
    let sort = u
        .query_pairs()
        .find(|(k, _)| k == "sorting")
        .map(|(_, v)| {
            if v == "score" {
                "popular".into()
            } else {
                v.into_owned()
            }
        })
        .unwrap_or_else(|| "popular".into());
    let price_range_mismatch = annotate_price_range(&mut products, &u);
    if price_range_mismatch == Some(true) {
        warnings.push(Warning::PriceOutsideRequestedRange);
    }
    let count = products.len();
    let mut result = SearchResponse {
        items: products,
        count,
        query,
        sort,
        search_url: current,
        next_cursor,
        has_next,
        total: None,
        context: SearchContext {
            region: None,
            region_verified: false,
        },
        coverage: SearchCoverage {
            page: page_num,
            fetched_pages_this_call: 1,
            parsed_items: all.len(),
            parsed_offset_start: start,
            parsed_offset_end: offset,
            returned: count,
            unique_seen: seen.len(),
            calls_in_chain: request.calls,
        },
        warnings,
        price_range_assessment: price_range_mismatch.map(|_| PriceRangeAssessment {
            basis: PriceRangeBasis::DisplayedSearchPrice,
            source_filter_guarantees_match: false,
        }),
        facets: None,
        sort_options: None,
        active_filters: None,
    };
    if args.include_facets.unwrap_or(args.next_cursor.is_none()) {
        result.facets = Some(facets(page, &facet_base));
        let sort_widget = WidgetSet::new(page)
            .first_matching("sort", sort_shape)
            .or_else(|| WidgetSet::new(page).first_matching("searchResultsSort", sort_shape));
        let sort_widget = sort_widget.or_else(|| {
            WidgetSet::new(page).first_valid_where(|value| value.get("sortButton").is_some())
        });
        result.sort_options = Some(
            sort_widget
                .as_ref()
                .map(|w| {
                    w["sortButton"]["options"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .take(12)
                        .map(|o| SortOption {
                            label: label(&o["name"]),
                            selected: o["isSelected"].as_bool().unwrap_or(false),
                            search_url: o["action"]["link"]
                                .as_str()
                                .and_then(observed_url)
                                .and_then(|u| reset(&u)),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        );
        result.active_filters = Some(
            WidgetSet::new(page)
                .first_matching("searchResultsFiltersActive", |value| {
                    value.get("activeFilters").is_some_and(Value::is_array)
                })
                .map(|w| {
                    w["activeFilters"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .take(30)
                        .map(|f| ActiveFilter {
                            key: label(&f["key"]),
                            name: label(&f["name"]),
                            kind: label(&f["ftype"]),
                            values: f["activeValues"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .take(20)
                                .map(|v| ActiveFilterValue {
                                    label: label(&v["title"]),
                                    search_url: v["disableUri"]
                                        .as_str()
                                        .and_then(observed_url)
                                        .and_then(|u| reset(&u)),
                                })
                                .collect(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        );
    }
    // Metadata is optional; trim it before allowing a response to grow large.
    trim_metadata(&mut result)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    struct OnePage(Value);

    impl PageSource for OnePage {
        async fn fetch_json(&mut self, _: &str, _: &CancellationToken) -> Result<Value> {
            Ok(self.0.clone())
        }
    }

    fn finish(page: &Value, args: &SearchArgs, request: PreparedSearchRequest) -> Result<Value> {
        Ok(serde_json::to_value(super::finish(page, args, request)?)?)
    }

    fn prepare(args: &SearchArgs) -> Result<PreparedSearchRequest> {
        super::prepare_request(args)
    }

    fn facets(page: &Value, base: &str) -> Value {
        serde_json::to_value(super::facets(page, base)).unwrap()
    }
    fn args(v: Value) -> SearchArgs {
        serde_json::from_value(v).unwrap()
    }
    fn page(ids: &[u64], next: Option<&str>) -> Value {
        let mut p = json!({"widgetStates":{"tileGridDesktop-x":{"items":ids.iter().map(|id|json!({"sku":id,"action":{"link":format!("/product/test-{id}/")},"mainState":[]})).collect::<Vec<_>>()}}});
        if let Some(next) = next {
            p["widgetStates"]["infiniteVirtualPaginator-x"] = json!({"nextPage":next});
        }
        p
    }

    #[test]
    fn public_search_matches_golden_contract() {
        let page: Value =
            serde_json::from_str(include_str!("../tests/fixtures/domain/search-page.json"))
                .unwrap();
        let expected: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/domain/search-response.json"
        ))
        .unwrap();
        let args = args(json!({"query": "x"}));
        assert_eq!(
            finish(&page, &args, prepare(&args).unwrap()).unwrap(),
            expected
        );
    }

    #[tokio::test]
    async fn scenario_uses_valid_widgets_after_malformed_instances() {
        let page = json!({"widgetStates": {
            "tileGridDesktop-a": {},
            "tileGridDesktop-b": {"items": [{"sku": "1", "mainState": []}]},
            "infiniteVirtualPaginator-a": {},
            "infiniteVirtualPaginator-b": {"nextPage": ""},
            "filtersDesktop-a": {},
            "filtersDesktop-b": {"sections": [{"filters": [{
                "type": "boolFilter", "key": "is_promo",
                "boolFilter": {"title": "Promo"}
            }]}]}
        }});
        let response = PreparedSearch::prepare(args(json!({"query": "x"})))
            .unwrap()
            .execute(&mut OnePage(page), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(response.count, 1);
        assert_eq!(response.has_next, Some(false));
        assert_eq!(response.facets.unwrap().unwrap().items.len(), 1);
        assert!(!response.warnings.contains(&Warning::SearchWidgetMissing));
    }

    #[tokio::test]
    async fn malformed_only_grid_is_reported_missing() {
        let page = json!({"widgetStates": {"tileGridDesktop-a": {}}});
        let response = PreparedSearch::prepare(args(json!({"query": "x"})))
            .unwrap()
            .execute(&mut OnePage(page), &CancellationToken::new())
            .await
            .unwrap();
        assert!(response.warnings.contains(&Warning::SearchWidgetMissing));
    }

    #[test]
    fn metadata_trim_only_reports_fields_actually_removed() {
        let args = args(json!({"query": "x", "includeFacets": false}));
        let mut without_metadata =
            super::finish(&page(&[1], None), &args, prepare(&args).unwrap()).unwrap();
        without_metadata.search_url = "😀".repeat(METADATA_BUDGET_UNITS / 2);
        trim_metadata(&mut without_metadata).unwrap();
        assert!(without_metadata.facets.is_none());
        assert!(
            !without_metadata
                .warnings
                .contains(&Warning::SearchMetadataTruncated)
        );

        let mut with_metadata = without_metadata;
        with_metadata.sort_options = Some(vec![SortOption {
            label: Some("observed".into()),
            selected: true,
            search_url: None,
        }]);
        trim_metadata(&mut with_metadata).unwrap();
        assert_eq!(with_metadata.sort_options, Some(Vec::new()));
        assert!(
            with_metadata
                .warnings
                .contains(&Warning::SearchMetadataTruncated)
        );
    }
    #[test]
    fn urls_are_scoped_before_normalization() {
        for s in [
            "http://www.ozon.ru/search/",
            "https://ozon.ru/search/",
            "https://www.ozon.ru/product/x-1/",
            "https://www.ozon.ru/search/../search/",
            "https://www.ozon.ru/category/%2e%2e/search/",
            "https://www.ozon.ru/search/?text=a&text=b",
            "https://www.ozon.ru/search/#x",
            "https://www.ozon.ru/search/?action=buy",
            "https://www.ozon.ru//search/",
        ] {
            assert!(normalize_search_url(s).is_err(), "{s}");
        }
        assert_eq!(
            normalize_search_url(
                "https://www.ozon.ru/category/mice-123/brand-2/?__rr=1&search_page_state=abc&page=2"
            )
            .unwrap(),
            "https://www.ozon.ru/category/mice-123/brand-2/?search_page_state=abc&page=2"
        );
    }
    #[test]
    fn price_bounds_preserve_kopecks() {
        let prepared = prepare(&args(
            json!({"query":"x","priceMin":19999,"priceMax":20501}),
        ))
        .unwrap();
        let url = url::Url::parse(&prepared.url).unwrap();
        assert_eq!(
            url.query_pairs()
                .find(|(key, _)| key == "currency_price")
                .unwrap()
                .1,
            "199.990;205.010"
        );
    }
    #[test]
    fn input_modes_and_cursor_bounds() {
        for v in [
            json!({}),
            json!({"query":"x","searchUrl":"https://www.ozon.ru/search/"}),
            json!({"query":"x","nextCursor":"{}"}),
            json!({"query":"x","sort":"bad"}),
        ] {
            assert!(prepare(&args(v)).is_err());
        }
        assert!(serde_json::from_value::<SearchArgs>(json!({"query":"x","unknown":1})).is_err());
        for r in [
            json!({"version":2,"url":"https://www.ozon.ru/search/","offset":0,"seen":[],"calls":0}),
            json!({"version":1,"url":"https://www.ozon.ru/search/","offset":10001,"seen":[],"calls":0}),
            json!({"version":1,"url":"https://www.ozon.ru/search/","offset":0,"seen":["abc"],"calls":0}),
        ] {
            assert!(prepare(&args(json!({"nextCursor":r.to_string()}))).is_err());
        }
    }
    #[test]
    fn overflow_deduplicates_and_advances_only_consumed_cards() {
        let a = args(json!({"query":"x","limit":2}));
        let p = page(&[1, 1, 2, 3], Some("/search/?text=x&page=2"));
        let first = finish(&p, &a, prepare(&a).unwrap()).unwrap();
        assert_eq!(first["count"], 2);
        assert_eq!(first["coverage"]["parsedOffsetEnd"], 3);
        let a = args(json!({"nextCursor":first["nextCursor"],"limit":2}));
        let second = finish(&p, &a, prepare(&a).unwrap()).unwrap();
        assert_eq!(second["count"], 1);
        assert_eq!(second["items"][0]["sku"], "3");
        let a = args(json!({"nextCursor":second["nextCursor"],"limit":2}));
        let req = prepare(&a).unwrap();
        assert!(req.url.ends_with("page=2"));
        assert_eq!(req.offset, 0);
        let third = finish(&page(&[2, 4], Some("")), &a, req).unwrap();
        assert_eq!(third["count"], 1);
        assert_eq!(third["hasNext"], false);
    }
    #[test]
    fn continuation_caps_are_explicit() {
        let a = args(json!({"query":"x","limit":2}));
        let mut req = prepare(&a).unwrap();
        req.seen = (1..500).map(|n| n.to_string()).collect();
        let r = finish(&page(&[500, 501], Some("/search/?page=2")), &a, req).unwrap();
        assert_eq!(r["count"], 1);
        assert_eq!(r["hasNext"], true);
        assert!(r["nextCursor"].is_null());
        assert!(
            r["warnings"]
                .as_array()
                .unwrap()
                .contains(&json!("CONTINUATION_LIMIT_REACHED"))
        );
    }

    #[test]
    fn canonical_brand_refinements_and_stalled_paginator() {
        let p = json!({"widgetStates":{"filtersDesktop-x":{"sections":[{"filters":[{"type":"checkboxesFilter","key":"brand","checkboxesFilter":{"sections":[{"items":[{"key":"1","title":{"text":"One"},"isSelected":true},{"key":"2","title":{"text":"Two"}}]}]}}]}]},"searchResultsFiltersActive-x":{"activeFilters":[{"key":"brand","activeValues":[{"title":"One","disableUri":"/category/mice-3/?text=x"}]}]}}});
        let f = facets(&p, "https://www.ozon.ru/category/mice-3/one-1/?text=x");
        assert_eq!(
            f["items"][0]["options"][0]["searchUrl"],
            "https://www.ozon.ru/category/mice-3/?text=x"
        );
        let add = f["items"][0]["options"][1]["searchUrl"].as_str().unwrap();
        assert!(!add.contains("one-1"));
        assert!(add.contains("brand=1%2C2"));
        let a = args(json!({"searchUrl":"https://www.ozon.ru/search/?text=x"}));
        let r = finish(
            &page(&[1], Some("/search/?text=x")),
            &a,
            prepare(&a).unwrap(),
        )
        .unwrap();
        assert!(r["nextCursor"].is_null());
        assert!(
            r["warnings"]
                .as_array()
                .unwrap()
                .contains(&json!("CONTINUATION_STALLED"))
        );
    }

    #[test]
    fn paginator_transport_is_removed_but_semantic_state_retained() {
        let result=paginator_url("/category/mice-1/?page=2&search_page_state=abc&start_page_id=def&brand=123&sorting=price&paginator_token=fragment&layout_page_index=2&layout_container=grid").unwrap();
        assert_eq!(
            result,
            "https://www.ozon.ru/category/mice-1/?page=2&search_page_state=abc&start_page_id=def&brand=123&sorting=price"
        );
        assert!(
            normalize_search_url("https://www.ozon.ru/search/?page=2&paginator_token=observed")
                .unwrap()
                .contains("paginator_token")
        );
    }

    #[test]
    fn maximum_unicode_query_roundtrips_through_cursor() {
        for query in ["я".repeat(2000), "界".repeat(2000), "😀".repeat(1000)] {
            let a = args(json!({"query":query,"limit":1}));
            let req = prepare(&a).unwrap();
            assert_eq!(
                url::Url::parse(&req.url)
                    .unwrap()
                    .query_pairs()
                    .find(|(k, _)| k == "text")
                    .unwrap()
                    .1,
                query
            );
            let response = finish(&page(&[1, 2], None), &a, req).unwrap();
            let next = args(json!({"nextCursor":response["nextCursor"],"limit":1}));
            let req = prepare(&next).unwrap();
            assert_eq!(req.offset, 1);
            assert_eq!(
                url::Url::parse(&req.url)
                    .unwrap()
                    .query_pairs()
                    .find(|(k, _)| k == "text")
                    .unwrap()
                    .1,
                query
            );
        }
    }

    #[test]
    fn only_explicit_empty_paginator_proves_exhaustion() {
        let a = args(json!({"query":"x"}));
        for paginator in [
            json!({}),
            json!({"nextPage":null}),
            json!({"nextPage":42}),
            json!({"nextPage":[]}),
        ] {
            let mut p = page(&[1], None);
            p["widgetStates"]["infiniteVirtualPaginator-x"] = paginator;
            let r = finish(&p, &a, prepare(&a).unwrap()).unwrap();
            assert!(r["hasNext"].is_null());
        }
        let r = finish(&page(&[1], Some("")), &a, prepare(&a).unwrap()).unwrap();
        assert_eq!(r["hasNext"], false);
    }

    #[test]
    fn displayed_prices_are_checked_without_filtering_or_reordering() {
        let product = |sku: &str, price| SearchItem {
            sku: sku.to_owned(),
            name: None,
            price,
            currency: "RUB".to_owned(),
            price_type: crate::model::PriceType::Unknown,
            price_label: None,
            delivery_label: None,
            seller: None,
            old_price: None,
            discount: None,
            rating: None,
            reviews: None,
            brand: None,
            url: None,
            image: None,
            matches_price_range: None,
        };
        let mut products = vec![
            product("1", Some(99.0)),
            product("2", Some(100.0)),
            product("3", Some(200.0)),
            product("4", Some(201.0)),
            product("5", None),
        ];
        let u =
            url::Url::parse("https://www.ozon.ru/search/?currency_price=100.000;200.000").unwrap();
        assert_eq!(annotate_price_range(&mut products, &u), Some(true));
        assert_eq!(
            products
                .iter()
                .map(|p| p.matches_price_range.flatten())
                .collect::<Vec<_>>(),
            vec![Some(false), Some(true), Some(true), Some(false), None]
        );
        assert_eq!(products[0].sku, "1");
        assert_eq!(products.len(), 5);
        for url in [
            "https://www.ozon.ru/search/",
            "https://www.ozon.ru/search/?currency_price=broken",
            "https://www.ozon.ru/search/?currency_price=NaN;200",
        ] {
            assert_eq!(
                annotate_price_range(&mut products, &url::Url::parse(url).unwrap()),
                None
            );
            assert!(products.iter().all(|p| p.matches_price_range == Some(None)));
        }
    }

    #[test]
    fn source_price_refinement_does_not_claim_strict_displayed_price_filtering() {
        let priced_item = |sku: &str, price: &str| {
            json!({
                "sku": sku,
                "action": {"link": format!("/product/test-{sku}/")},
                "mainState": [{
                    "type": "priceV2",
                    "priceV2": {"price": [{"textStyle": "PRICE", "text": price}]}
                }]
            })
        };
        let page = json!({"widgetStates": {
            "tileGridDesktop-x": {"items": [
                priced_item("5705143151", "362 ₽"),
                priced_item("1746233283", "479 ₽"),
                priced_item("5165920559", "491 ₽")
            ]},
            "infiniteVirtualPaginator-x": {"nextPage": "/search/?text=x&currency_price=500.000%3B1500.000&page=2"}
        }});
        let args = args(json!({
            "query": "x",
            "priceMin": 50_000,
            "priceMax": 150_000,
            "limit": 3
        }));

        let result = finish(&page, &args, prepare(&args).unwrap()).unwrap();

        assert_eq!(result["count"], 3);
        assert_eq!(
            result["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| (&item["sku"], &item["matchesPriceRange"]))
                .collect::<Vec<_>>(),
            vec![
                (&json!("5705143151"), &json!(false)),
                (&json!("1746233283"), &json!(false)),
                (&json!("5165920559"), &json!(false)),
            ]
        );
        assert_eq!(
            result["priceRangeAssessment"],
            json!({
                "basis": "displayed_search_price",
                "sourceFilterGuaranteesMatch": false
            })
        );
        assert_eq!(result["coverage"]["returned"], 3);
        assert_eq!(result["hasNext"], true);
        assert!(
            result["warnings"]
                .as_array()
                .unwrap()
                .contains(&json!("PRICE_OUTSIDE_REQUESTED_RANGE"))
        );
    }

    #[test]
    fn native_score_sort_has_public_popular_alias() {
        let a = args(json!({"searchUrl":"https://www.ozon.ru/search/?sorting=score"}));
        let r = finish(&page(&[1], None), &a, prepare(&a).unwrap()).unwrap();
        assert_eq!(r["sort"], "popular");
        assert_eq!(r["searchUrl"], "https://www.ozon.ru/search/?sorting=score");
    }

    #[test]
    fn missing_metadata_is_unknown() {
        let a = args(json!({"query":"x"}));
        let result = finish(&page(&[1], None), &a, prepare(&a).unwrap()).unwrap();
        assert!(result["hasNext"].is_null());
        assert!(result["total"].is_null());
        assert!(result["facets"].is_null());
    }
    #[test]
    fn observed_facets_preserve_selection_and_reset_paging() {
        let p = json!({"widgetStates":{"filtersDesktop-x":{"sections":[{"filters":[{"type":"checkboxesFilter","key":"brand","checkboxesFilter":{"title":"Brand","sections":[{"items":[{"key":"1","title":{"text":"One"},"isSelected":true},{"key":"2","title":{"text":"Two"}}]}]}},{"type":"boolFilter","key":"is_promo","boolFilter":{"title":"Promo"}}]}]}}});
        let f = facets(
            &p,
            "https://www.ozon.ru/search/?text=x&brand=1&page=2&search_page_state=secret",
        );
        let u = f["items"][0]["options"][1]["searchUrl"].as_str().unwrap();
        assert!(u.contains("brand=1%2C2"));
        assert!(!u.contains("page"));
        assert!(
            f["items"][1]["searchUrl"]
                .as_str()
                .unwrap()
                .contains("is_promo=true")
        );
    }
}
