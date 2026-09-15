//! Privacy-preserving source gateway used by the vNext application layer.
//! It exposes marketplace observations only; refs, evidence and persistence
//! belong to the application layer above this module.
use crate::{
    browser_error::BrowserError,
    model::{PriceType, SourceSectionStatus, Warning},
    ozon_pages::OzonPages,
    parse,
    search::{PreparedSearch, SearchArgs},
    widgets::WidgetSet,
};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

pub struct Gateway {
    pages: OzonPages,
    observed: CapabilityObservations,
}

#[derive(Default)]
struct CapabilityObservations {
    context_signature: Option<Value>,
    search: bool,
    search_refinements: bool,
    search_pagination: bool,
    card_prices: bool,
    products: bool,
    characteristics: bool,
    description: bool,
    product_images: bool,
    image_content: bool,
    review_text: bool,
    review_images: bool,
    review_pagination: bool,
    review_refinements: bool,
    variants: bool,
    region_verification: bool,
    account_observation: bool,
}

impl Gateway {
    pub async fn from_env() -> Result<Self> {
        Ok(Self {
            pages: OzonPages::from_env().await?,
            observed: CapabilityObservations::default(),
        })
    }

    pub async fn context(&mut self, cancel: &CancellationToken) -> Result<Value> {
        ensure_not_cancelled(cancel)?;
        let page = self.pages.context_json(cancel).await?;
        ensure_not_cancelled(cancel)?;
        let observation = page.get("contextObservation");
        self.observed
            .update_context_signature(context_capability_signature(observation));
        self.observed.region_verification |= observation
            .and_then(|value| value.get("regionVerified"))
            .and_then(Value::as_bool)
            == Some(true);
        self.observed.account_observation |= observation
            .and_then(|value| value.get("accountState"))
            .and_then(Value::as_str)
            .is_some_and(|state| matches!(state, "authenticated" | "anonymous"));
        Ok(json!({
            "regionLabel": observation.and_then(|value| value.get("regionLabel")).cloned().unwrap_or(Value::Null),
            "regionVerification": observation
                .and_then(|value| value.get("regionVerified"))
                .and_then(Value::as_bool)
                .filter(|verified| *verified)
                .map_or("unverified", |_| "verified"),
            "regionSourceUrl": observation
                .and_then(|value| value.get("regionSourceUrl"))
                .cloned()
                .unwrap_or(Value::Null),
            "accountState": observed_enum(observation, "accountState", &["authenticated", "anonymous"], "unknown"),
            "accessState": observed_enum(observation, "accessState", &["available", "blocked"], "unknown"),
            // The page layer hashes only the observed account-state class and
            // city label; identity and address values never leave the page.
            "signature": observation.and_then(|value| value.get("signature")).cloned().unwrap_or(Value::Null),
            "capabilities": {
                "search": observed_status(self.observed.search),
                "search_refinements": observed_status(self.observed.search_refinements),
                "search_pagination": observed_status(self.observed.search_pagination),
                "card_prices": observed_status(self.observed.card_prices),
                "products": observed_status(self.observed.products),
                "characteristics": observed_status(self.observed.characteristics),
                "description": observed_status(self.observed.description),
                "product_images": observed_status(self.observed.product_images),
                "image_content": observed_status(self.observed.image_content),
                "review_text": observed_status(self.observed.review_text),
                "review_images": observed_status(self.observed.review_images),
                "review_pagination": observed_status(self.observed.review_pagination),
                "review_refinements": observed_status(self.observed.review_refinements),
                "variants": observed_status(self.observed.variants),
                "offers": "unsupported",
                "region_verification": observed_status(self.observed.region_verification),
                "account_observation": observed_status(self.observed.account_observation)
            }
        }))
    }

    pub fn mark_image_content_available(&mut self) {
        self.observed.image_content = true;
    }

    pub async fn search(&mut self, args: SearchArgs, cancel: &CancellationToken) -> Result<Value> {
        ensure_not_cancelled(cancel)?;
        let response = PreparedSearch::prepare(args)?
            .execute(&mut self.pages, cancel)
            .await?;
        ensure_not_cancelled(cancel)?;
        self.observed.search = true;
        self.observed.search_refinements |= matches!(&response.facets, Some(Some(_)));
        self.observed.search_pagination |= response.has_next.is_some();
        self.observed.card_prices |= response
            .items
            .iter()
            .any(|item| matches!(&item.price_type, PriceType::OzonCard));
        Ok(serde_json::to_value(response)?)
    }

    pub async fn product(&mut self, product: &str, cancel: &CancellationToken) -> Result<Value> {
        let path = product_path(product)?;
        ensure_not_cancelled(cancel)?;
        let base = self.pages.fetch_json(&path, cancel).await?;
        ensure_not_cancelled(cancel)?;
        let needs_secondary = needs_product_supplement(&base);
        let mut secondary = None;
        let mut secondary_failed = false;
        if needs_secondary {
            match self
                .pages
                .fetch_json(
                    &format!("{path}?layout_container=pdpPage2column&layout_page_index=2"),
                    cancel,
                )
                .await
            {
                Ok(page) => secondary = Some(page),
                Err(error) if cancel.is_cancelled() => return Err(error),
                Err(_) => secondary_failed = true,
            }
        }
        ensure_not_cancelled(cancel)?;
        let mut details = parse::parse_details(&base, secondary.as_ref());
        if secondary_failed && !parse::parse_description(&base).has_text() {
            push_warning(&mut details.warnings, Warning::DescriptionFetchFailed);
        }
        if !details.description.has_text() {
            push_warning(
                &mut details.warnings,
                if details.description.images.is_empty() {
                    Warning::DescriptionEmpty
                } else {
                    Warning::DescriptionTextEmpty
                },
            );
        }
        self.observed.products |= details.sku.is_some() && details.name.is_some();
        self.observed.characteristics |= !details.characteristics.is_empty();
        self.observed.description |=
            details.description.has_text() || !details.description.images.is_empty();
        self.observed.product_images |= !details.images.is_empty();
        self.observed.variants |= matches!(
            &details.variants.status,
            SourceSectionStatus::Available | SourceSectionStatus::Partial
        );
        let widgets = WidgetSet::new(&base);
        if !widgets.has_matching("webProductHeading", |value| value.get("title").is_some())
            || !widgets.has_matching("webPrice", |value| {
                ["cardPrice", "price", "originalPrice", "isAvailable"]
                    .iter()
                    .any(|key| value.get(*key).is_some())
            })
        {
            push_warning(&mut details.warnings, Warning::ProductWidgetsMissing);
        }
        Ok(serde_json::to_value(details)?)
    }

    pub async fn reviews(
        &mut self,
        path: &str,
        limit: usize,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        ensure!((1..=30).contains(&limit), "limit must be between 1 and 30");
        let path = reviews_path(path)?;
        ensure_not_cancelled(cancel)?;
        let page = self.pages.fetch_json(&path, cancel).await?;
        ensure_not_cancelled(cancel)?;
        // Preserve the complete bounded upstream page. The application layer
        // applies the caller limit and retains any remainder in its opaque
        // cursor before advancing to the upstream next page.
        ensure!(
            !WidgetSet::new(&page).has_matching("webListReviews", |value| {
                ["reviews", "items"].iter().any(|key| {
                    value
                        .get(*key)
                        .and_then(Value::as_array)
                        .is_some_and(|items| items.len() > 30)
                })
            }),
            "RESULT_TOO_LARGE: source review page exceeds its bounded snapshot"
        );
        let mut result = parse::parse_reviews(&page, 30);
        let review_widget = WidgetSet::new(&page).has_matching("webListReviews", |value| {
            value.get("reviews").is_some_and(Value::is_array)
                || value.get("items").is_some_and(Value::is_array)
        });
        if !review_widget {
            push_warning(&mut result.warnings, Warning::ReviewsWidgetMissing);
        }
        self.observed.review_text |= review_widget;
        self.observed.review_images |= result
            .reviews
            .iter()
            .any(|review| !review.photos.is_empty());
        self.observed.review_pagination |= result.has_next.is_some();
        self.observed.review_refinements |= !result.refinements.is_empty();
        Ok(serde_json::to_value(result)?)
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        self.pages.shutdown().await
    }
}

impl CapabilityObservations {
    fn update_context_signature(&mut self, signature: Value) {
        if self
            .context_signature
            .as_ref()
            .is_some_and(|previous| previous != &signature)
        {
            self.image_content = false;
        }
        self.context_signature = Some(signature);
    }
}

fn context_capability_signature(observation: Option<&Value>) -> Value {
    observation
        .and_then(|value| value.get("signature"))
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or_else(|| {
            json!({
                "regionLabel": observation.and_then(|value| value.get("regionLabel")).cloned().unwrap_or(Value::Null),
                "regionVerified": observation.and_then(|value| value.get("regionVerified")).cloned().unwrap_or(Value::Null),
                "accountState": observation.and_then(|value| value.get("accountState")).cloned().unwrap_or(Value::Null)
            })
        })
}

fn observed_status(observed: bool) -> &'static str {
    if observed { "available" } else { "unverified" }
}

fn observed_enum<'a>(
    observation: Option<&Value>,
    key: &str,
    allowed: &[&'a str],
    fallback: &'a str,
) -> &'a str {
    let Some(value) = observation
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
    else {
        return fallback;
    };
    allowed
        .iter()
        .copied()
        .find(|candidate| *candidate == value)
        .unwrap_or(fallback)
}

fn ensure_not_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(BrowserError::Cancelled.into());
    }
    Ok(())
}

fn product_path(product: &str) -> Result<String> {
    let value = product.trim();
    ensure!(
        !value.is_empty()
            && value.encode_utf16().count() <= 2000
            && !value.chars().any(char::is_control),
        "Invalid product: expected Ozon product URL, SKU or slug"
    );
    let path = if value.starts_with("https://") {
        let url = url::Url::parse(value)?;
        ensure!(
            matches!(url.host_str(), Some("ozon.ru" | "www.ozon.ru"))
                && url.username().is_empty()
                && url.password().is_none()
                && url.port().is_none(),
            "Expected an ozon.ru product URL"
        );
        url.path().to_owned()
    } else {
        value
            .split(['?', '#'])
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    ensure!(
        !path.contains('%') && !path.contains("//"),
        "Invalid product path"
    );
    let slug = path
        .strip_prefix("/product/")
        .unwrap_or(&path)
        .trim_end_matches('/')
        .strip_suffix("/reviews")
        .unwrap_or(
            path.strip_prefix("/product/")
                .unwrap_or(&path)
                .trim_end_matches('/'),
        );
    ensure!(
        regex::Regex::new(r"^[\p{L}\p{N}_-]*[0-9]$")?.is_match(slug),
        "Invalid product SKU or slug"
    );
    Ok(format!("/product/{slug}/"))
}

fn reviews_path(input: &str) -> Result<String> {
    if input.contains("/reviews/") {
        parse::safe_review_path(input).ok_or_else(|| anyhow::anyhow!("Invalid reviews path"))
    } else {
        Ok(format!("{}reviews/", product_path(input)?))
    }
}

fn push_warning(warnings: &mut Vec<Warning>, warning: Warning) {
    if !warnings.contains(&warning) {
        warnings.push(warning);
    }
}

fn needs_product_supplement(base: &Value) -> bool {
    !parse::parse_description(base).has_text() || parse::parse_duty(base).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_description_does_not_skip_customs_supplement() {
        let mut base = json!({"widgetStates":{"webDescription-0":{"richAnnotation":"<p>Real product description</p>"}}});
        assert!(needs_product_supplement(&base));
        let secondary = json!({"widgetStates":{"webIconWithText-duty":{"title":"Таможенная пошлина", "text":"1 737 ₽ при получении"}}});
        assert_eq!(
            parse::parse_details(&base, Some(&secondary))
                .duty
                .unwrap()
                .amount,
            1737.0
        );
        base["widgetStates"]["webIconWithText-duty"] =
            secondary["widgetStates"]["webIconWithText-duty"].clone();
        assert!(!needs_product_supplement(&base));
    }

    #[test]
    fn product_and_review_navigation_stays_on_public_product_routes() {
        assert_eq!(product_path("901").unwrap(), "/product/901/");
        assert_eq!(
            product_path("https://www.ozon.ru/product/example-901/?tracking=discard").unwrap(),
            "/product/example-901/"
        );
        assert_eq!(
            reviews_path("/product/example-901/reviews/?page=2").unwrap(),
            "/product/example-901/reviews/?page=2"
        );
        assert_eq!(
            reviews_path("/product/example-901/reviews/?page=2&page_key=TOKEN_1&reviewsVariantMode=2&sort=score_desc").unwrap(),
            "/product/example-901/reviews/?page=2&page_key=TOKEN_1&reviewsVariantMode=2&sort=score_desc"
        );
        for invalid in [
            "https://example.test/product/901/",
            "https://www.ozon.ru@127.0.0.1/product/901/",
            "/product/901/reviews/?redirect=https://example.test",
            "/product/901/reviews/?page=2#private",
        ] {
            assert!(
                if invalid.contains("/reviews/") && invalid.starts_with('/') {
                    reviews_path(invalid)
                } else {
                    product_path(invalid)
                }
                .is_err(),
                "accepted unsafe navigation {invalid}"
            );
        }
    }

    #[test]
    fn image_content_observation_resets_when_context_signature_changes() {
        let mut observed = CapabilityObservations::default();
        observed.update_context_signature(json!("context-a"));
        observed.image_content = true;
        observed.update_context_signature(json!("context-a"));
        assert!(observed.image_content);
        observed.update_context_signature(json!("context-b"));
        assert!(!observed.image_content);
    }
}
