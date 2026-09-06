use anyhow::{Result, bail, ensure};
use percent_encoding::percent_decode_str;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{browser::Browser, parse};

fn search_limit() -> usize {
    12
}
fn reviews_limit() -> usize {
    10
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
    /// Search text. Provide exactly one of query, searchUrl or nextCursor.
    #[schemars(length(min = 1, max = 2000))]
    pub query: Option<String>,
    /// Public Ozon search/category URL copied from a returned facet or sort option.
    /// Mutually exclusive with query and nextCursor; optional sort/price overrides refine it.
    #[schemars(length(min = 1, max = 24000))]
    pub search_url: Option<String>,
    /// Copy a returned nextCursor unchanged. Combine only with limit and includeFacets,
    /// never query, searchUrl, sort or price parameters.
    #[schemars(length(min = 1, max = 48000))]
    pub next_cursor: Option<String>,
    /// Include observed facets, sort options and active filters.
    /// Defaults to true for initial requests and false for nextCursor requests.
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

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DetailsArgs {
    #[schemars(length(min = 1, max = 2000))]
    pub product: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReviewsArgs {
    #[schemars(length(min = 1, max = 2000))]
    pub product: String,
    #[serde(default = "reviews_limit")]
    #[schemars(range(min = 1, max = 30))]
    pub limit: usize,
}

pub fn product_path(product: &str) -> Result<String> {
    ensure!(
        !product.trim().is_empty() && product.encode_utf16().count() <= 2000,
        "Invalid product: expected Ozon product URL, SKU or slug"
    );
    let trimmed = product.trim();
    let path = if trimmed.to_ascii_lowercase().starts_with("http://")
        || trimmed.to_ascii_lowercase().starts_with("https://")
    {
        let url = url::Url::parse(trimmed)?;
        ensure!(
            matches!(url.host_str(), Some("ozon.ru" | "www.ozon.ru"))
                && url.username().is_empty()
                && url.password().is_none()
                && url.port().is_none(),
            "Expected an ozon.ru product URL"
        );
        url.path().to_owned()
    } else {
        trimmed
            .split(['?', '#'])
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    // percent_decode_str leaves malformed escapes unchanged; reject them explicitly.
    let bytes = path.as_bytes();
    for (i, byte) in bytes.iter().enumerate() {
        if *byte == b'%' {
            ensure!(
                bytes.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
                    && bytes.get(i + 2).is_some_and(u8::is_ascii_hexdigit),
                "Invalid URL encoding"
            );
        }
    }
    let decoded = percent_decode_str(&path)
        .decode_utf8()
        .map_err(|_| anyhow::anyhow!("Invalid URL encoding"))?;
    let slug = decoded
        .strip_prefix("/product/")
        .unwrap_or(&decoded)
        .trim_end_matches('/');
    let slug = slug
        .strip_suffix("/reviews")
        .or_else(|| slug.strip_suffix("/questions"))
        .unwrap_or(slug);
    ensure!(
        regex::Regex::new(r"^[\p{L}\p{N}_-]*[0-9]$")?.is_match(slug),
        "Invalid product SKU or slug"
    );
    Ok(format!("/product/{slug}/"))
}

fn check_cancel(cancel: &CancellationToken) -> Result<()> {
    ensure!(!cancel.is_cancelled(), "Request cancelled");
    Ok(())
}

fn has_widget(page: &Value, name: &str) -> bool {
    page.get("widgetStates")
        .and_then(Value::as_object)
        .is_some_and(|widgets| {
            widgets
                .keys()
                .any(|key| key.split('-').next() == Some(name))
        })
}

fn warn(result: &mut Value, warning: &str) {
    if !result["warnings"].is_array() {
        result["warnings"] = json!([]);
    }
    let warnings = result["warnings"]
        .as_array_mut()
        .expect("warnings is an array");
    if !warnings.iter().any(|item| item.as_str() == Some(warning)) {
        warnings.push(json!(warning));
    }
}

fn has_description_text(value: &Value) -> bool {
    value["text"].as_str().is_some_and(|text| !text.is_empty())
}

fn merge_descriptions(base: &Value, secondary: &Value) -> Value {
    let text = if has_description_text(base) {
        base["text"].as_str().unwrap_or_default()
    } else {
        secondary["text"].as_str().unwrap_or_default()
    };
    let mut images = Vec::new();
    for description in [base, secondary] {
        if let Some(items) = description["images"].as_array() {
            for image in items {
                if !images.contains(image) {
                    images.push(image.clone());
                }
            }
        }
    }
    json!({ "text": text, "images": images })
}

pub async fn search(
    browser: &mut Browser,
    args: SearchArgs,
    cancel: &CancellationToken,
) -> Result<Value> {
    let request = crate::search::prepare(&args)?;
    check_cancel(cancel)?;
    let page = browser.fetch_json(&request.path(), cancel).await?;
    check_cancel(cancel)?;
    let mut result = crate::search::finish(&page, &args, request)?;
    if !has_widget(&page, "tileGridDesktop") {
        warn(&mut result, "SEARCH_WIDGET_MISSING");
    }
    Ok(result)
}

pub async fn details(
    browser: &mut Browser,
    args: DetailsArgs,
    cancel: &CancellationToken,
) -> Result<Value> {
    let path = product_path(&args.product)?;
    check_cancel(cancel)?;
    let base = browser.fetch_json(&path, cancel).await?;
    check_cancel(cancel)?;
    let description = parse::parse_description(&base);
    let mut second = None;
    let mut failed = false;
    if !has_description_text(&description) {
        check_cancel(cancel)?;
        match browser
            .fetch_json(
                &format!("{path}?layout_container=pdpPage2column&layout_page_index=2"),
                cancel,
            )
            .await
        {
            Ok(page) => second = Some(page),
            Err(error) => {
                if cancel.is_cancelled() {
                    return Err(error);
                }
                failed = true;
            }
        }
    }
    check_cancel(cancel)?;
    let mut result = parse::parse_details(&base, second.as_ref());
    result["description"] = merge_descriptions(&description, &result["description"]);
    if failed {
        warn(&mut result, "DESCRIPTION_FETCH_FAILED");
    }
    if !has_description_text(&result["description"]) {
        if result["description"]["images"]
            .as_array()
            .is_some_and(|images| !images.is_empty())
        {
            warn(&mut result, "DESCRIPTION_TEXT_EMPTY");
        } else {
            warn(&mut result, "DESCRIPTION_EMPTY");
        }
    }
    if !has_widget(&base, "webProductHeading") || !has_widget(&base, "webPrice") {
        warn(&mut result, "PRODUCT_WIDGETS_MISSING");
    }
    Ok(result)
}

pub async fn reviews(
    browser: &mut Browser,
    args: ReviewsArgs,
    cancel: &CancellationToken,
) -> Result<Value> {
    if !(1..=30).contains(&args.limit) {
        bail!("limit must be between 1 and 30");
    }
    let path = format!("{}reviews/", product_path(&args.product)?);
    check_cancel(cancel)?;
    let page = browser.fetch_json(&path, cancel).await?;
    check_cancel(cancel)?;
    let mut result = parse::parse_reviews(&page, args.limit);
    if !has_widget(&page, "webListReviews") {
        warn(&mut result, "REVIEWS_WIDGET_MISSING");
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_image_only_description_and_retains_images_on_fallback() {
        let base = json!({"text": "", "images": ["base"]});
        let secondary = json!({"text": "Details", "images": ["base", "extra"]});
        assert!(!has_description_text(&base));
        assert_eq!(
            merge_descriptions(&base, &secondary),
            json!({"text": "Details", "images": ["base", "extra"]})
        );
        assert_eq!(merge_descriptions(&base, &Value::Null), base);
    }

    #[test]
    fn accepts_product_identifiers() {
        assert_eq!(
            product_path("https://www.ozon.ru/product/товар-123/reviews/?x=1").unwrap(),
            "/product/товар-123/"
        );
        assert_eq!(product_path("7").unwrap(), "/product/7/");
    }

    #[test]
    fn rejects_foreign_urls_and_encoded_paths() {
        for input in [
            "https://evil.example/product/123/",
            "https://user@ozon.ru/product/123/",
            "123%2F456",
            "../123",
            "123%ZZ",
        ] {
            assert!(product_path(input).is_err(), "accepted {input}");
        }
    }
}
