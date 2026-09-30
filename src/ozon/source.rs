//! Typed Ozon observations. Research references, continuation and persistence belong to the application.
use crate::{
    ozon::bridge::PageSource,
    ozon::decode,
    ozon::model::{
        ProductDetails, ReviewPage, SearchResponse, SourceFailure, SourceFailureCode,
        SourceSectionStatus, SourceStage, SupplementObservation, SupplementSection, Warning,
    },
    ozon::outcome::ContextObservation,
    ozon::pages::Pages,
    ozon::search::{PreparedSearch, SearchArgs},
    ozon::widgets::WidgetSet,
    runtime::browser::error::BrowserError,
    runtime::config::Config,
};
use anyhow::Result;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

macro_rules! ensure {
    ($condition:expr, $($message:tt)*) => { if !$condition { return Err(crate::error::fail(crate::error::Code::InvalidArgument, format!($($message)*))); } };
}

pub struct OzonSource {
    pages: Pages,
}
impl OzonSource {
    pub async fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            pages: Pages::new(config).await?,
        })
    }
    pub async fn context(&mut self, cancel: &CancellationToken) -> Result<ContextObservation> {
        ensure_not_cancelled(cancel)?;
        let page = self.pages.context_json(cancel).await?;
        ensure_not_cancelled(cancel)?;
        serde_json::from_value(
            page.get("contextObservation")
                .cloned()
                .unwrap_or(Value::Null),
        )
        .map_err(|_| BrowserError::InvalidBridgeResponse.into())
    }
    pub async fn search(
        &mut self,
        args: SearchArgs,
        cancel: &CancellationToken,
    ) -> Result<SearchResponse> {
        PreparedSearch::prepare(args)?
            .execute(&mut self.pages, cancel)
            .await
    }
    pub async fn product(
        &mut self,
        product: &str,
        cancel: &CancellationToken,
    ) -> Result<ProductDetails> {
        read_product(&mut self.pages, product, cancel).await
    }
    pub async fn reviews(&mut self, path: &str, cancel: &CancellationToken) -> Result<ReviewPage> {
        let path = reviews_path(path)?;
        ensure_not_cancelled(cancel)?;
        let page = self.pages.fetch_json(&path, cancel).await?;
        ensure_not_cancelled(cancel)?;
        // The bounded page remains intact; the application retains unreturned rows before advancing.
        if WidgetSet::new(&page).has_matching("webListReviews", |v| {
            ["reviews", "items"].iter().any(|k| {
                v.get(*k)
                    .and_then(Value::as_array)
                    .is_some_and(|v| v.len() > 30)
            })
        }) {
            return Err(crate::error::fail(
                crate::error::Code::ResultTooLarge,
                "Source review page exceeds its bounded snapshot",
            ));
        }
        let mut result = decode::parse_reviews(&page, 30);
        result.source_url = Some(format!("https://www.ozon.ru{path}"));
        result.observed_at = Some(chrono::Utc::now().to_rfc3339());
        if !result.source.present {
            result.warnings.push(Warning::ReviewsWidgetMissing);
        }
        Ok(result)
    }
    pub async fn shutdown(&mut self) -> Result<()> {
        self.pages.shutdown().await
    }
}
async fn read_product(
    source: &mut impl PageSource,
    product: &str,
    cancel: &CancellationToken,
) -> Result<ProductDetails> {
    let path = product_path(product)?;
    let requested_sku = sku_from_path(&path).ok_or_else(|| {
        crate::error::fail(crate::error::Code::InvalidArgument, "Invalid product SKU")
    })?;
    ensure_not_cancelled(cancel)?;
    let base = source.fetch_json(&path, cancel).await?;
    ensure_not_cancelled(cancel)?;
    decode::validate_product_identity(&base, &requested_sku, true)?;
    let mut secondary = None;
    let mut supplement = None;
    let needs_description = !decode::parse_description(&base).has_text();
    if needs_product_supplement(&base) {
        let supplement_path = format!("{path}?layout_container=pdpPage2column&layout_page_index=2");
        let observed_at = chrono::Utc::now().to_rfc3339();
        let mut record = SupplementObservation {
            requested_sections: [
                needs_description.then_some(SupplementSection::Description),
                decode::parse_duty(&base)
                    .is_none()
                    .then_some(SupplementSection::Duty),
            ]
            .into_iter()
            .flatten()
            .collect(),
            source_url: format!("https://www.ozon.ru{supplement_path}"),
            observed_at,
            status: SourceSectionStatus::Unknown,
            error: None,
        };
        match source.fetch_json(&supplement_path, cancel).await {
            Ok(page) => {
                ensure_not_cancelled(cancel)?;
                if decode::validate_product_identity(&page, &requested_sku, false)? {
                    record.status = SourceSectionStatus::Available;
                    secondary = Some(page);
                } else {
                    record.status = SourceSectionStatus::Partial;
                    record.error =
                        Some(supplement_failure(SourceFailureCode::InvalidResponse, None));
                }
            }
            Err(error) => {
                ensure_not_cancelled(cancel)?;
                let failure = match error.downcast_ref::<BrowserError>() {
                    Some(BrowserError::CaptchaOrBlocked) => {
                        supplement_failure(SourceFailureCode::Blocked, None)
                    }
                    Some(BrowserError::HttpStatus(status)) => {
                        supplement_failure(SourceFailureCode::HttpStatus, Some(*status))
                    }
                    Some(BrowserError::InvalidBridgeResponse) => {
                        supplement_failure(SourceFailureCode::InvalidResponse, None)
                    }
                    // Deadlines, origin/route changes and uncertain ownership remain operation errors.
                    _ => return Err(error),
                };
                record.status = SourceSectionStatus::Partial;
                record.error = Some(failure);
            }
        }
        supplement = Some(record);
    }
    let mut details = decode::parse_details(&base, secondary.as_ref());
    details.supplement = supplement;
    if needs_description
        && details
            .supplement
            .as_ref()
            .is_some_and(|s| s.error.is_some())
    {
        details.warnings.push(Warning::DescriptionFetchFailed);
        details.description.source.source_truncated = true;
    }
    if !details.description.has_text() {
        details
            .warnings
            .push(if details.description.image_origins.is_empty() {
                Warning::DescriptionEmpty
            } else {
                Warning::DescriptionTextEmpty
            });
    }
    if details.name.is_none() {
        details.warnings.push(Warning::ProductWidgetsMissing);
    }
    Ok(details)
}
fn supplement_failure(code: SourceFailureCode, http_status: Option<u64>) -> SourceFailure {
    let message = match &code {
        SourceFailureCode::Blocked => "Ozon did not provide the supplemental product section",
        SourceFailureCode::HttpStatus => "Ozon returned an unsuccessful supplemental status",
        SourceFailureCode::InvalidResponse => {
            "Supplemental product identity or projection is unavailable"
        }
    };
    SourceFailure {
        code,
        message: message.into(),
        stage: SourceStage::ProductSupplement,
        http_status,
    }
}
fn ensure_not_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(BrowserError::Cancelled.into())
    } else {
        Ok(())
    }
}
fn sku_from_path(path: &str) -> Option<String> {
    regex::Regex::new(r"^/product/(?:[^/]*-)?([0-9]+)/$")
        .ok()?
        .captures(path)?
        .get(1)
        .map(|m| m.as_str().to_owned())
}
fn product_path(product: &str) -> Result<String> {
    let value = product.trim();
    ensure!(
        !value.is_empty()
            && value.encode_utf16().count() <= 2000
            && !value.chars().any(char::is_control)
            && !value.contains('\\'),
        "Invalid product"
    );
    let path = if value.starts_with("https://") {
        let url = url::Url::parse(value)?;
        ensure!(
            matches!(url.host_str(), Some("ozon.ru" | "www.ozon.ru"))
                && url.username().is_empty()
                && url.password().is_none()
                && url.port().is_none(),
            "Expected public Ozon product URL"
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
        .trim_end_matches('/');
    let path = format!("/product/{slug}/");
    ensure!(
        sku_from_path(&path).is_some(),
        "Invalid product SKU or slug"
    );
    Ok(path)
}
fn reviews_path(input: &str) -> Result<String> {
    if input.contains("/reviews/") {
        decode::safe_review_path(input).ok_or_else(|| {
            crate::error::fail(crate::error::Code::InvalidArgument, "Invalid reviews path")
        })
    } else {
        Ok(format!("{}reviews/", product_path(input)?))
    }
}
fn needs_product_supplement(base: &Value) -> bool {
    !decode::parse_description(base).has_text() || decode::parse_duty(base).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;
    struct Pages(VecDeque<Result<Value>>);
    impl PageSource for Pages {
        async fn fetch_json(&mut self, _: &str, _: &CancellationToken) -> Result<Value> {
            self.0.pop_front().unwrap()
        }
    }
    fn page(sku: &str) -> Value {
        json!({"layoutTrackingInfo":{"sku":sku},"widgetStates":{"webProductHeading-a":{"title":"Product"}}})
    }
    #[tokio::test]
    async fn identity_is_checked_before_merging_each_fragment() {
        let mut pages = Pages(VecDeque::from([Ok(page("901")), Ok(page("902"))]));
        assert!(
            read_product(&mut pages, "901", &CancellationToken::new())
                .await
                .is_err()
        );
        let mut pages = Pages(VecDeque::from([Ok(page("902"))]));
        assert!(
            read_product(&mut pages, "901", &CancellationToken::new())
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn optional_missing_identity_and_blocked_keep_explicit_partial_provenance() {
        for next in [
            Ok(json!({"widgetStates":{}})),
            Err(BrowserError::CaptchaOrBlocked.into()),
        ] {
            let mut pages = Pages(VecDeque::from([Ok(page("901")), next]));
            let result = read_product(&mut pages, "901", &CancellationToken::new())
                .await
                .unwrap();
            let supplement = result.supplement.unwrap();
            assert!(supplement.error.is_some());
            assert_eq!(supplement.status, SourceSectionStatus::Partial);
            assert!(supplement.source_url.contains("layout_page_index=2"));
        }
    }
    #[tokio::test]
    async fn supplementary_deadline_origin_and_ownership_are_never_swallowed() {
        for error in [
            BrowserError::CommandTimeout,
            BrowserError::InvalidOrigin,
            BrowserError::CleanupFailed,
            BrowserError::Cancelled,
        ] {
            let mut pages = Pages(VecDeque::from([Ok(page("901")), Err(error.into())]));
            assert!(
                read_product(&mut pages, "901", &CancellationToken::new())
                    .await
                    .is_err()
            );
        }
    }
    #[test]
    fn public_routes_remain_strict() {
        assert_eq!(product_path("901").unwrap(), "/product/901/");
        assert_eq!(
            reviews_path("/product/901/reviews/?page=2&sort=score_desc").unwrap(),
            "/product/901/reviews/?page=2&sort=score_desc"
        );
        for invalid in [
            "https://example.test/product/901/",
            "https://www.ozon.ru@127.0.0.1/product/901/",
            "/product/901/reviews/?redirect=evil",
        ] {
            assert!(product_path(invalid).is_err());
        }
    }
}
