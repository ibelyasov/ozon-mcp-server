//! One bounded captured public search page. The application owns opaque continuation.
//! Paginator links are promoted from widget-fragment to full-document requests
//! by removing only paginator_token, layout_page_index and layout_container.
//! Observed page numbers and semantic continuation state remain unchanged.
use crate::{
    ozon::bridge::PageSource,
    ozon::decode,
    ozon::model::{Facet, FacetOption, Facets, SearchItem, SearchResponse, SortOption, Warning},
    ozon::widgets::WidgetSet,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use tokio_util::sync::CancellationToken;

macro_rules! ensure {
    ($condition:expr, $($message:tt)*) => { if !$condition { return Err(crate::error::fail(crate::error::Code::InvalidArgument, format!($($message)*))); } };
}

const MAX_URL: usize = 24_000;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchArgs {
    pub query: Option<String>,
    pub search_url: Option<String>,
    pub include_facets: Option<bool>,
    pub sort: Option<String>,
    pub price_min: Option<u64>,
    pub price_max: Option<u64>,
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

fn same_continuation_state(current: &str, next: &str) -> bool {
    let without_paging = |value: &str| -> Option<String> {
        let mut url = url::Url::parse(value).ok()?;
        let pairs: Vec<_> = url
            .query_pairs()
            .filter(|(k, _)| !paging(k))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        url.set_query(None);
        for (k, v) in pairs {
            url.query_pairs_mut().append_pair(&k, &v);
        }
        Some(url.into())
    };
    match (without_paging(current), without_paging(next)) {
        (Some(current), Some(next)) => crate::ozon::pages::is_requested_page(&current, &next),
        _ => false,
    }
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
    url: String,
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
        args.query.is_some() != args.search_url.is_some(),
        "Provide exactly one of query or searchUrl"
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
        .ok_or_else(|| {
            crate::error::fail(crate::error::Code::InvalidArgument, "Invalid sorting URL")
        })?;
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
        .ok_or_else(|| {
            crate::error::fail(crate::error::Code::InvalidArgument, "Invalid price URL")
        })?;
    }
    Ok(PreparedSearchRequest {
        url: normalize_search_url(&current)?,
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
        if cancel.is_cancelled() {
            return Err(crate::runtime::browser::error::BrowserError::Cancelled.into());
        }
        let page = source.fetch_json(&self.request.path(), cancel).await?;
        if cancel.is_cancelled() {
            return Err(crate::runtime::browser::error::BrowserError::Cancelled.into());
        }
        let mut result = finish(&page, &self.args, self.request)?;
        if result.source.raw_rows > 120 {
            return Err(crate::error::fail(
                crate::error::Code::ResultTooLarge,
                "Source search page exceeds its bounded snapshot",
            ));
        }
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
    let mut truncated = w["sourceTruncated"].as_bool().unwrap_or(false);
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
            truncated = true;
            continue;
        }
        let key = filter["key"].as_str().unwrap_or("");
        if !safe_key(key) {
            truncated = true;
            continue;
        }
        let body = &filter[kind];
        truncated |= body["sourceTruncated"].as_bool().unwrap_or(false);
        let title_body = if kind == "multipleRangesFilter" {
            &body["rangeFilter"]
        } else {
            body
        };
        let mut facet = Facet {
            title: label(&title_body["title"]),
            selected: None,
            search_url: None,
            options: None,
        };
        let mut options = vec![];
        let mut count = 0;
        if kind == "boolFilter" {
            let selected = body["isSelected"].as_bool().unwrap_or(false);
            facet.selected = Some(selected);
            facet.search_url = refine(base, key, if selected { None } else { Some("true") });
        } else if kind == "categoryFilter" {
            for c in body["categories"].as_array().into_iter().flatten() {
                count += 1;
                if options.len() < 20 {
                    options.push(FacetOption {
                        label: label(&c["title"]),
                        selected: c["isActive"].as_bool().unwrap_or(false),
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
                    truncated = true;
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
                    label: label(item.get("title").unwrap_or(&item["description"])),
                    selected: active,
                    search_url: target,
                });
            }
            truncated |= checks["hasManyValues"].as_bool().unwrap_or(false)
                || checks["openingButtons"].get("showAllButton").is_some();
        }
        if kind != "boolFilter" {
            facet.options = Some(options);
            truncated |= count > 20;
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

pub(crate) fn price_filter_bounds(request_url: &url::Url) -> Option<(u64, u64)> {
    request_url
        .query_pairs()
        .find(|(k, _)| k == "currency_price")
        .and_then(|(_, v)| {
            let (min, max) = v.split_once(';')?;
            // Source price filter encodes thousandths with a known trailing zero.
            let min = decode::decimal_minor(min.strip_suffix('0')?)?;
            let max = decode::decimal_minor(max.strip_suffix('0')?)?;
            (min <= max).then_some((min, max))
        })
}

fn annotate_price_range(products: &mut [SearchItem], request_url: &url::Url) -> Option<bool> {
    let bounds = price_filter_bounds(request_url);
    let mut outside = false;
    for product in products {
        let matches =
            bounds.and_then(|(min, max)| product.price_minor.map(|p| p >= min && p <= max));
        outside |= matches == Some(false);
        product.matches_price_range = matches;
    }
    bounds.map(|_| outside)
}

fn finish(
    page: &Value,
    args: &SearchArgs,
    request: PreparedSearchRequest,
) -> Result<SearchResponse> {
    let (mut products, source) = decode::parse_search_page(page);
    let paginator = WidgetSet::new(page).first_matching("infiniteVirtualPaginator", |value| {
        value.get("nextPage").is_some_and(Value::is_string)
    });
    let observed_next = paginator
        .as_ref()
        .and_then(|p| p["nextPage"].as_str())
        .filter(|s| !s.is_empty());
    let next_url = observed_next
        .and_then(paginator_url)
        .filter(|next| same_continuation_state(&request.url, next));
    let stalled = next_url.as_ref() == Some(&request.url);
    let next_url = next_url.filter(|_| !stalled);
    let has_next = if next_url.is_some() {
        Some(true)
    } else if source.complete()
        && paginator.as_ref().and_then(|p| p["nextPage"].as_str()) == Some("")
    {
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
    let mut warnings = vec![Warning::SearchResultsMayChangeBetweenCalls];
    if stalled {
        warnings.push(Warning::ContinuationStalled);
    }
    if observed_next.is_some() && next_url.is_none() && !stalled {
        warnings.push(Warning::UnsafePaginatorUrlIgnored);
    }
    let u = url::Url::parse(&current)?;
    let price_range_mismatch = annotate_price_range(&mut products, &u);
    if price_range_mismatch == Some(true) {
        warnings.push(Warning::PriceOutsideRequestedRange);
    }
    let mut result = SearchResponse {
        acquisition: page
            .get("acquisition")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        items: products,
        search_url: current,
        next_path: next_url,
        source: source.clone(),
        refinements_source_truncated: false,
        has_next,
        total: None,
        warnings,
        facets: None,
        sort_options: None,
    };
    if args.include_facets.unwrap_or(true) {
        result.facets = facets(page, &facet_base);
        let sort_widget = WidgetSet::new(page).first_matching("searchResultsSort", sort_shape);
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
    }
    result.refinements_source_truncated = result.facets.as_ref().is_some_and(|f| f.truncated)
        || WidgetSet::new(page)
            .first_matching("searchResultsSort", sort_shape)
            .is_some_and(|w| {
                w.pointer("/sortButton/options")
                    .and_then(Value::as_array)
                    .is_some_and(|v| v.len() > 12)
            })
        || WidgetSet::new(page)
            .first_matching("searchResultsFiltersActive", |v| {
                v.get("activeFilters").is_some_and(Value::is_array)
            })
            .is_some_and(|w| {
                w["activeFilters"].as_array().is_some_and(|v| {
                    v.len() > 30
                        || v.iter()
                            .any(|f| f["activeValues"].as_array().is_some_and(|v| v.len() > 20))
                })
            });
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
            serde_json::from_str(include_str!("../../tests/fixtures/domain/search-page.json"))
                .unwrap();
        let expected: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/domain/search-response.json"
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
        assert_eq!(response.items.len(), 1);
        assert_eq!(response.has_next, None);
        assert_eq!(response.facets.unwrap().items.len(), 1);
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
    fn captured_page_preserves_all_rows_independent_of_application_limit() {
        let a = args(json!({"query":"x"}));
        let r = finish(
            &page(&[1, 1, 2, 3], Some("/search/?text=x&page=2")),
            &a,
            prepare(&a).unwrap(),
        )
        .unwrap();
        assert_eq!(r["items"].as_array().unwrap().len(), 4);
        assert_eq!(r["items"][3]["sku"], "3");
        assert_eq!(r["nextPath"], "https://www.ozon.ru/search/?text=x&page=2");
        for retired in [
            json!({"query":"x","nextCursor":"{}"}),
            json!({"query":"x","limit":1}),
        ] {
            assert!(serde_json::from_value::<SearchArgs>(retired).is_err());
        }
    }
    #[test]
    fn malformed_rows_and_missing_grid_cannot_prove_end() {
        let a = args(json!({"query":"x"}));
        let mut p = page(&[1], Some(""));
        p["widgetStates"]["tileGridDesktop-x"]["items"]
            .as_array_mut()
            .unwrap()
            .push(json!({"sku":"bad"}));
        let r = finish(&p, &a, prepare(&a).unwrap()).unwrap();
        assert!(r["hasNext"].is_null());
        assert_eq!(r["source"]["malformedRows"], 1);
        let r = finish(
            &json!({"widgetStates":{"infiniteVirtualPaginator-x":{"nextPage":""}}}),
            &a,
            prepare(&a).unwrap(),
        )
        .unwrap();
        assert!(r["hasNext"].is_null());
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
        assert!(r["nextPath"].is_null());
        assert!(
            r["warnings"]
                .as_array()
                .unwrap()
                .contains(&json!("CONTINUATION_STALLED"))
        );
    }

    #[test]
    fn observed_continuation_cannot_change_query_or_filters() {
        let a = args(json!({"searchUrl":"https://www.ozon.ru/search/?text=x&rating=5"}));
        let result = finish(
            &page(&[1], Some("/search/?text=y&rating=5&page=2")),
            &a,
            prepare(&a).unwrap(),
        )
        .unwrap();
        assert!(result["nextPath"].is_null());
        assert!(result["hasNext"].is_null());
        let result = finish(
            &page(&[1], Some("/search/?text=x&rating=4&page=2")),
            &a,
            prepare(&a).unwrap(),
        )
        .unwrap();
        assert!(result["nextPath"].is_null());
    }
    #[test]
    fn source_option_loss_and_hidden_values_propagate_to_refinements() {
        let options = (0..21)
            .map(|n| json!({"key":n.to_string(),"title":n.to_string()}))
            .collect::<Vec<_>>();
        let p = json!({"widgetStates":{"tileGridDesktop-a":{"items":[]},"filtersDesktop-a":{"sections":[{"filters":[{"type":"checkboxesFilter","key":"brand","checkboxesFilter":{"hasManyValues":true,"sections":[{"items":options}]}}]}]}}});
        let a = args(json!({"query":"x"}));
        let result = finish(&p, &a, prepare(&a).unwrap()).unwrap();
        assert_eq!(
            result["facets"]["items"][0]["options"]
                .as_array()
                .unwrap()
                .len(),
            20
        );
        assert_eq!(result["refinementsSourceTruncated"], true);
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
            source_index: 0,
            source_locator: "/items/0".into(),
            sku: sku.to_owned(),
            name: None,
            price_minor: price,
            price_type: crate::ozon::model::PriceType::Unknown,
            price_label: None,
            delivery_label: None,
            seller: None,
            rating: None,
            reviews: None,
            url: None,
            image: None,
            matches_price_range: None,
        };
        let mut products = vec![
            product("1", Some(9900)),
            product("2", Some(10000)),
            product("3", Some(20000)),
            product("4", Some(20100)),
            product("5", None),
        ];
        let u =
            url::Url::parse("https://www.ozon.ru/search/?currency_price=100.000;200.000").unwrap();
        assert_eq!(annotate_price_range(&mut products, &u), Some(true));
        assert_eq!(
            products
                .iter()
                .map(|p| p.matches_price_range)
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
            assert!(products.iter().all(|p| p.matches_price_range.is_none()));
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
            "priceMax": 150_000
        }));

        let result = finish(&page, &args, prepare(&args).unwrap()).unwrap();

        assert_eq!(result["items"].as_array().unwrap().len(), 3);
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
        assert_eq!(result["hasNext"], true);
        assert!(
            result["warnings"]
                .as_array()
                .unwrap()
                .contains(&json!("PRICE_OUTSIDE_REQUESTED_RANGE"))
        );
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

    #[test]
    fn refinements_keep_radio_replacement_and_option_description_labels() {
        let page = json!({"widgetStates":{"filtersDesktop-a":{"sections":[{"filters":[{
            "type":"checkboxesFilter","key":"brand","checkboxesFilter":{
                "title":"Brand","isRadio":true,"sections":[{"items":[
                    {"key":"1","title":"One","isSelected":true},
                    {"key":"2","description":"Two"}
                ]}]
            }
        }]}]}}});
        let parsed =
            super::facets(&page, "https://www.ozon.ru/search/?text=x&brand=1&page=2").unwrap();
        let option = &parsed.items[0].options.as_ref().unwrap()[1];
        assert_eq!(option.label.as_deref(), Some("Two"));
        let url = url::Url::parse(option.search_url.as_deref().unwrap()).unwrap();
        assert_eq!(
            url.query_pairs().find(|(key, _)| key == "brand").unwrap().1,
            "2"
        );
        assert!(!url.query_pairs().any(|(key, _)| key == "page"));
    }

    #[test]
    fn captured_search_page_roundtrips_unknown_facets_and_price_match() {
        let request = args(json!({"query":"x"}));
        let captured =
            super::finish(&page(&[1], Some("")), &request, prepare(&request).unwrap()).unwrap();
        let value = serde_json::to_value(&captured).unwrap();
        assert_eq!(value["facets"], Value::Null);
        assert_eq!(value["items"][0]["matchesPriceRange"], Value::Null);
        assert_eq!(
            serde_json::from_value::<SearchResponse>(value).unwrap(),
            captured
        );
    }
}
