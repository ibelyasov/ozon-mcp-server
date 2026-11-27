//! Typed Ozon observations. Research references, continuation and persistence belong to the application.
use crate::{
    ozon::bridge::PageSource,
    ozon::decode,
    ozon::model::{
        ProductDetails, ProductReadOptions, ReviewPage, ReviewReadOptions, SearchResponse,
        SourceFailure, SourceFailureCode, SourceSectionStatus, SourceStage, SupplementObservation,
        SupplementSection, Warning,
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
    pub async fn product_with_options(
        &mut self,
        product: &str,
        options: &ProductReadOptions,
        cancel: &CancellationToken,
    ) -> Result<ProductDetails> {
        read_product_with_options(&mut self.pages, product, options, cancel).await
    }
    /// Continue only an explicitly observed, validated offers link.
    pub async fn offers_page(
        &mut self,
        sku: &str,
        path: &str,
        cancel: &CancellationToken,
    ) -> Result<(
        crate::ozon::model::ProductOffers,
        crate::ozon::model::ReadUsage,
    )> {
        let mut usage = crate::ozon::model::ReadUsage::default();
        let result: Result<_> = async {
            let path = validated_offers_path(path, sku).ok_or_else(|| {
                crate::error::fail(
                    crate::error::Code::InvalidArgument,
                    "Invalid offers continuation",
                )
            })?;
            ensure_not_cancelled(cancel)?;
            usage.source_reads += 1;
            let page = self.pages.fetch_json(&path, cancel).await?;
            ensure_not_cancelled(cancel)?;
            decode::validate_product_identity(&page, sku, false)?;
            let mut offers = decode::parse_offers(&page);
            offers.next_path = offers
                .next_path
                .as_deref()
                .and_then(|p| validated_offers_path(p, sku));
            Ok((offers, usage.clone()))
        }
        .await;
        result.map_err(|error| error.context(usage))
    }
    pub async fn reviews_with_options(
        &mut self,
        path: &str,
        options: &ReviewReadOptions,
        cancel: &CancellationToken,
    ) -> Result<ReviewPage> {
        read_reviews_with_options(&mut self.pages, path, options, cancel).await
    }
    pub async fn shutdown(&mut self) -> Result<()> {
        self.pages.shutdown().await
    }
}
async fn read_reviews_with_options(
    source: &mut impl PageSource,
    path: &str,
    options: &ReviewReadOptions,
    cancel: &CancellationToken,
) -> Result<ReviewPage> {
    let mut usage = crate::ozon::model::ReadUsage::default();
    let outcome: Result<ReviewPage> = async {
        let path = reviews_path(path)?;
        ensure_not_cancelled(cancel)?;
        usage.source_reads += 1;
        let page = source.fetch_json(&path, cancel).await?;
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
        let root_sku = sku_from_path(path.split("reviews/").next().unwrap_or_default())
            .ok_or_else(|| {
                crate::error::fail(crate::error::Code::InvalidArgument, "Invalid reviews SKU")
            })?;
        if let Some(observed) = result.source_url.as_deref() {
            let observed_path = url::Url::parse(observed).map_err(|_| {
                crate::error::fail(
                    crate::error::Code::SourceChanged,
                    "Invalid review source identity",
                )
            })?;
            let observed_sku = sku_from_path(
                observed_path
                    .path()
                    .strip_suffix("reviews/")
                    .unwrap_or_default(),
            );
            if observed_sku.as_deref() != Some(root_sku.as_str()) {
                return Err(crate::error::fail(
                    crate::error::Code::SourceChanged,
                    "Conflicting review source identity",
                ));
            }
        }

        if options.include_details || options.include_seller_replies {
            decode::add_review_details(&page, &mut result);
        }
        if options.include_seller_replies {
            let root_sku = sku_from_path(path.split("reviews/").next().unwrap_or_default())
                .ok_or_else(|| {
                    crate::error::fail(crate::error::Code::InvalidArgument, "Invalid reviews SKU")
                })?;
            let limit = options.max_source_reads.max(1);
            for review in &mut result.reviews {
                let Some(uuid) = decode::review_uuid(&page, review.source_index) else {
                    continue;
                };
                if usage.source_reads >= limit {
                    usage.budget_exhausted = true;
                    break;
                }
                usage.source_reads += 1;
                let sku = review
                    .details
                    .as_ref()
                    .and_then(|d| d.item_sku.as_deref())
                    .unwrap_or(&root_sku);
                match source.seller_replies(&uuid, sku, &root_sku, cancel).await {
                    Ok(response) => {
                        ensure_not_cancelled(cancel)?;
                        if let Some((replies, status)) = decode::decode_seller_replies(&response)
                            && let Some(details) = review.details.as_mut()
                        {
                            details.seller_replies = replies;
                            details.replies_acquisition = response
                                .get("acquisition")
                                .and_then(|v| serde_json::from_value(v.clone()).ok());
                            details.replies_status = status;
                        }
                    }
                    Err(error) => {
                        ensure_not_cancelled(cancel)?;
                        if !recoverable_reply(&error) {
                            return Err(error);
                        }
                        if let Some(details) = review.details.as_mut() {
                            details.replies_status = SourceSectionStatus::Partial;
                        }
                    }
                }
            }
        }
        result.source_url = Some(format!("https://www.ozon.ru{path}"));
        result.observed_at = Some(chrono::Utc::now().to_rfc3339());
        if !result.source.present {
            result.warnings.push(Warning::ReviewsWidgetMissing);
        }
        result.read_usage = usage.clone();
        Ok(result)
    }
    .await;
    outcome.map_err(|error| error.context(usage))
}

#[cfg(test)]
async fn read_product(
    source: &mut impl PageSource,
    product: &str,
    cancel: &CancellationToken,
) -> Result<ProductDetails> {
    read_product_with_options(
        source,
        product,
        &ProductReadOptions {
            description: true,
            duty: true,
            max_source_reads: 8,
            ..Default::default()
        },
        cancel,
    )
    .await
}
async fn read_product_with_options(
    source: &mut impl PageSource,
    product: &str,
    options: &ProductReadOptions,
    cancel: &CancellationToken,
) -> Result<ProductDetails> {
    let limit = options.max_source_reads.max(1);
    let mut reads = 0;
    let mut exhausted = false;
    let outcome: Result<ProductDetails> = async {
        let path = product_path(product)?;
        let requested_sku = sku_from_path(&path).ok_or_else(|| {
            crate::error::fail(crate::error::Code::InvalidArgument, "Invalid product SKU")
        })?;
        ensure_not_cancelled(cancel)?;
        reads += 1;
        let base = source.fetch_json(&path, cancel).await?;
        ensure_not_cancelled(cancel)?;
        decode::validate_product_identity(&base, &requested_sku, true)?;
        let mut secondary = None;
        let mut supplement = None;
        let needs_description = options.description && !decode::parse_description(&base).has_text();
        let needs_duty = options.duty && decode::parse_duty(&base).is_none();
        let needs_full = options.full_characteristics
            && !decode::parse_full_characteristics(&base).source.complete();
        if (needs_description || needs_duty || needs_full) && reads < limit {
            reads += 1;
            let supplement_path =
                format!("{path}?layout_container=pdpPage2column&layout_page_index=2");
            let observed_at = chrono::Utc::now().to_rfc3339();
            let mut record = SupplementObservation {
                requested_sections: [
                    needs_description.then_some(SupplementSection::Description),
                    needs_duty.then_some(SupplementSection::Duty),
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
        } else if needs_description || needs_duty || needs_full {
            exhausted = true;
        }
        let mut full = options.full_characteristics.then(|| {
            let base_full = decode::parse_full_characteristics(&base);
            if let Some(page) = secondary.as_ref() {
                choose_full(base_full, decode::parse_full_characteristics(page))
            } else {
                base_full
            }
        });
        if full.as_ref().is_some_and(|f| !f.source.complete()) {
            if reads < limit {
                reads += 1;
                match source.fetch_json(&format!("{path}features/"), cancel).await {
                    Ok(page) => {
                        ensure_not_cancelled(cancel)?;
                        if decode::validate_product_identity(&page, &requested_sku, false)? {
                            let candidate = decode::parse_full_characteristics(&page);
                            full = Some(choose_full(full.take().unwrap(), candidate));
                        }
                    }
                    Err(error) => {
                        ensure_not_cancelled(cancel)?;
                        if !recoverable(&error) {
                            return Err(error);
                        }
                    }
                }
            } else {
                exhausted = true;
            }
        }
        let mut offers = options.offers.then(decode::empty_offers);
        if options.offers {
            if reads < limit {
                reads += 1;
                let offer_path =
                    format!("/modal/otherOffersFromSellers?product_id={requested_sku}&sort=price");
                match source.fetch_json(&offer_path, cancel).await {
                    Ok(page) => {
                        ensure_not_cancelled(cancel)?;
                        decode::validate_product_identity(&page, &requested_sku, false)?;
                        let mut parsed = decode::parse_offers(&page);
                        parsed.next_path = parsed
                            .next_path
                            .as_deref()
                            .and_then(|p| validated_offers_path(p, &requested_sku));
                        offers = Some(parsed);
                    }
                    Err(error) => {
                        ensure_not_cancelled(cancel)?;
                        if !recoverable(&error) {
                            return Err(error);
                        }
                    }
                }
            } else {
                exhausted = true;
            }
        }
        let mut details = decode::parse_details(&base, secondary.as_ref());
        details.supplement = supplement;
        details.full_characteristics = full;
        details.offers = offers;
        details.read_usage = crate::ozon::model::ReadUsage {
            source_reads: reads,
            budget_exhausted: exhausted,
        };
        if needs_description
            && details
                .supplement
                .as_ref()
                .is_some_and(|s| s.error.is_some())
        {
            details.warnings.push(Warning::DescriptionFetchFailed);
            details.description.source.source_truncated = true;
        }
        if options.description && !details.description.has_text() {
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
    .await;
    outcome.map_err(|error| {
        error.context(crate::ozon::model::ReadUsage {
            source_reads: reads,
            budget_exhausted: exhausted,
        })
    })
}
fn choose_full(
    current: crate::ozon::model::FullCharacteristics,
    candidate: crate::ozon::model::FullCharacteristics,
) -> crate::ozon::model::FullCharacteristics {
    if candidate.source.complete()
        || (!current.source.complete() && candidate.source.parsed_rows > current.source.parsed_rows)
    {
        candidate
    } else {
        current
    }
}

fn validated_offers_path(input: &str, sku: &str) -> Option<String> {
    if input.len() > 2000
        || !input.starts_with('/') && !input.starts_with("https://www.ozon.ru/")
        || sku.is_empty()
        || !sku.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let base = url::Url::parse("https://www.ozon.ru/").ok()?;
    let url = base.join(input).ok()?;
    if url.origin() != base.origin()
        || url.path() != "/modal/otherOffersFromSellers"
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    let mut product = None;
    let mut sort = None;
    let mut page = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "product_id" if product.is_none() => product = Some(value.into_owned()),
            "sort" if sort.is_none() => sort = Some(value.into_owned()),
            "page" if page.is_none() => page = Some(value.into_owned()),
            _ => return None,
        }
    }
    if product.as_deref() != Some(sku)
        || sort.as_deref() != Some("price")
        || !page.as_deref().is_some_and(|p| {
            !p.is_empty()
                && p.len() <= 6
                && p.bytes().all(|b| b.is_ascii_digit())
                && p.parse::<u32>().is_ok_and(|n| n > 1)
        })
    {
        return None;
    }
    Some(format!("{}?{}", url.path(), url.query()?))
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
fn recoverable_reply(error: &anyhow::Error) -> bool {
    recoverable(error)
        || matches!(
            error.downcast_ref::<BrowserError>(),
            Some(BrowserError::ResponseTooLarge)
        )
        || error
            .downcast_ref::<crate::error::RuntimeError>()
            .is_some_and(|e| e.code == crate::error::Code::UpstreamTimeout)
}

fn recoverable(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<BrowserError>(),
        Some(
            BrowserError::CaptchaOrBlocked
                | BrowserError::HttpStatus(_)
                | BrowserError::InvalidBridgeResponse
        )
    )
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
    async fn product_options_skip_unrequested_supplement_and_report_budget() {
        let mut pages = Pages(VecDeque::from([Ok(page("901"))]));
        let options = crate::ozon::model::ProductReadOptions {
            full_characteristics: true,
            max_source_reads: 1,
            ..Default::default()
        };
        let result =
            read_product_with_options(&mut pages, "901", &options, &CancellationToken::new())
                .await
                .unwrap();
        assert_eq!(result.read_usage.source_reads, 1);
        assert!(result.read_usage.budget_exhausted);
        assert_eq!(
            result.full_characteristics.unwrap().source.status(),
            SourceSectionStatus::Unknown
        );
        let mut pages = Pages(VecDeque::from([Ok(page("901"))]));
        let result = read_product_with_options(
            &mut pages,
            "901",
            &Default::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.read_usage.source_reads, 1);
        assert!(!result.read_usage.budget_exhausted);
    }
    struct ReviewPages {
        page: Value,
        reply: Option<Result<Value>>,
        calls: usize,
    }
    impl PageSource for ReviewPages {
        async fn fetch_json(&mut self, _: &str, _: &CancellationToken) -> Result<Value> {
            Ok(self.page.clone())
        }
        async fn seller_replies(
            &mut self,
            _: &str,
            _: &str,
            _: &str,
            _: &CancellationToken,
        ) -> Result<Value> {
            self.calls += 1;
            self.reply.take().unwrap()
        }
    }
    fn review_page(path: &str) -> Value {
        json!({"widgetStates":{"webListReviews-a":{"requestedPath":path,"reviews":[{"uuid":"review-1","itemId":"902","content":{"comment":"good"}}]}}})
    }
    #[tokio::test]
    async fn optional_reply_failures_preserve_base_and_identity_is_checked_first() {
        for error in [
            crate::error::fail(crate::error::Code::UpstreamTimeout, "timeout"),
            BrowserError::ResponseTooLarge.into(),
        ] {
            let mut pages = ReviewPages {
                page: review_page("/product/901/reviews/"),
                reply: Some(Err(error)),
                calls: 0,
            };
            let options = ReviewReadOptions {
                include_seller_replies: true,
                max_source_reads: 2,
                ..Default::default()
            };
            let result = read_reviews_with_options(
                &mut pages,
                "/product/901/reviews/",
                &options,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(result.reviews[0].comment.as_deref(), Some("good"));
            assert_eq!(
                result.reviews[0].details.as_ref().unwrap().replies_status,
                SourceSectionStatus::Partial
            );
            assert_eq!(result.read_usage.source_reads, 2);
        }
        let options = ReviewReadOptions {
            include_seller_replies: true,
            max_source_reads: 2,
            ..Default::default()
        };
        let mut pages = ReviewPages {
            page: review_page("/product/902/reviews/"),
            reply: None,
            calls: 0,
        };
        assert!(
            read_reviews_with_options(
                &mut pages,
                "/product/901/reviews/",
                &options,
                &CancellationToken::new()
            )
            .await
            .is_err()
        );
        assert_eq!(pages.calls, 0);
        let mut pages = ReviewPages {
            page: review_page("/product/901/reviews/"),
            reply: Some(Err(BrowserError::Cancelled.into())),
            calls: 0,
        };
        assert!(
            read_reviews_with_options(
                &mut pages,
                "/product/901/reviews/",
                &options,
                &CancellationToken::new()
            )
            .await
            .is_err()
        );
    }
    #[test]
    fn observed_offer_continuation_requires_same_sku_and_safe_route() {
        let good = "/modal/otherOffersFromSellers?product_id=901&sort=price&page=2";
        assert_eq!(validated_offers_path(good, "901").as_deref(), Some(good));
        for bad in [
            "2",
            "/modal/otherOffersFromSellers?product_id=902&sort=price&page=2",
            "/modal/otherOffersFromSellers?product_id=901&product_id=902&sort=price&page=2",
            "https://evil.example/modal/otherOffersFromSellers?product_id=901&sort=price&page=2",
        ] {
            assert!(validated_offers_path(bad, "901").is_none());
        }
    }
    #[tokio::test]
    async fn malformed_characteristics_try_same_sku_fallback() {
        let base = json!({"layoutTrackingInfo":{"sku":"901"},"widgetStates":{"webCharacteristics-a":{"characteristics":[{"short":[{"name":"Weight","values":null}]}]}}});
        let full = json!({"layoutTrackingInfo":{"sku":"901"},"widgetStates":{"webCharacteristics-a":{"characteristics":[{"short":[{"name":"Weight","values":[{"text":"1 kg"}]}]}]}}});
        let mut pages = Pages(VecDeque::from([Ok(base), Ok(full)]));
        let result = read_product_with_options(
            &mut pages,
            "901",
            &ProductReadOptions {
                full_characteristics: true,
                max_source_reads: 2,
                ..Default::default()
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.full_characteristics.unwrap().items[0].value, "1 kg");
        assert_eq!(result.read_usage.source_reads, 2);
    }
    #[test]
    fn conflicting_star_buckets_are_not_reported() {
        let page = json!({"widgetStates":{"webReviewProductScore-a":{"score":[{"title":"5","value":1},{"title":"5","value":2},{"title":"4","value":3},{"title":"4","value":3},{"title":"3","value":0},{"title":"2","value":0}]}}});
        let mut reviews = decode::parse_reviews(&page, 30);
        decode::add_review_details(&page, &mut reviews);
        assert_eq!(
            reviews
                .star_counts
                .unwrap()
                .iter()
                .map(|v| (v.stars, v.count))
                .collect::<Vec<_>>(),
            vec![(2, 0), (3, 0), (4, 3)]
        );
    }
    #[tokio::test]
    async fn full_specs_and_offers_are_bounded_and_preserve_sku() {
        // Shapes follow pinned SZHukovWork/ozon-mcp source, not a live observation.
        let full = json!({"layoutTrackingInfo":{"sku":"901"},"widgetStates":{"webCharacteristics-a":{"characteristics":[{"short":[{"name":"Weight","values":[{"text":"1 kg"}]}],"long":[]}]}}});
        let offers = json!({"widgetStates":{"webSellerList-a":{"sellers":[{"sku":"902","name":"Shop","productLink":"/product/902/","price":{"cardPrice":{"price":100},"price":{"price":120}}}]}}});
        let mut pages = Pages(VecDeque::from([Ok(page("901")), Ok(full), Ok(offers)]));
        let options = ProductReadOptions {
            full_characteristics: true,
            offers: true,
            max_source_reads: 3,
            ..Default::default()
        };
        let result =
            read_product_with_options(&mut pages, "901", &options, &CancellationToken::new())
                .await
                .unwrap();
        assert_eq!(result.read_usage.source_reads, 3);
        assert_eq!(result.full_characteristics.unwrap().items[0].value, "1 kg");
        let offer = &result.offers.unwrap().items[0];
        assert_eq!(offer.sku, "902");
        assert_eq!(offer.card_price_minor, Some(10000));
        assert_eq!(offer.regular_price_minor, Some(12000));
    }
    #[test]
    fn review_details_keep_observed_counts_and_unknown_reply_status() {
        // Synthetic projection shaped after the pinned competitor, without personal data.
        let page = json!({"widgetStates": {
            "webListReviews-a": {"reviews":[{"uuid":"review-1", "itemId":"902", "content":{"comment":"Good"}, "usefulness":{"useful":3,"unuseful":1}}]},
            "webReviewProductScore-a": {"score":[{"title":"5 stars","value":10},{"title":"1 star","value":0}]}
        }});
        let mut result = decode::parse_reviews(&page, 30);
        decode::add_review_details(&page, &mut result);
        let details = result.reviews[0].details.as_ref().unwrap();
        assert_eq!(
            (
                details.helpful,
                details.unhelpful,
                details.item_sku.as_deref()
            ),
            (Some(3), Some(1), Some("902"))
        );
        assert_eq!(details.replies_status, SourceSectionStatus::Unknown);
        assert_eq!(
            result
                .star_counts
                .unwrap()
                .iter()
                .map(|s| (s.stars, s.count))
                .collect::<Vec<_>>(),
            vec![(1, 0), (5, 10)]
        );
    }
    #[tokio::test]
    async fn fatal_error_carries_attempted_read_count() {
        let mut pages = Pages(VecDeque::from([
            Ok(page("901")),
            Err(BrowserError::InvalidOrigin.into()),
        ]));
        let options = ProductReadOptions {
            description: true,
            max_source_reads: 2,
            ..Default::default()
        };
        let error =
            read_product_with_options(&mut pages, "901", &options, &CancellationToken::new())
                .await
                .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<crate::ozon::model::ReadUsage>()
                .unwrap()
                .source_reads,
            2
        );
        assert!(matches!(
            error.downcast_ref::<BrowserError>(),
            Some(BrowserError::InvalidOrigin)
        ));
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
