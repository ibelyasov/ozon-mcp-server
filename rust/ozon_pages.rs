use crate::{
    browser::BrowserSession,
    browser_error::BrowserError,
    page_outcome::{PageError, PageOutcome},
    page_source::PageSource,
};
use anyhow::Result;
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const HOME: &str = "https://www.ozon.ru/";

pub struct OzonPages {
    session: BrowserSession,
    ready: bool,
    last_used: Instant,
}

impl OzonPages {
    pub async fn from_env() -> Result<Self> {
        Ok(Self {
            session: BrowserSession::from_env().await?,
            ready: false,
            last_used: Instant::now(),
        })
    }

    /// Observe only the narrow, privacy-filtered header context projection.
    /// This method never opens dialogs, signs in, or changes a saved region.
    pub async fn context_json(&mut self, cancel: &CancellationToken) -> Result<Value> {
        let first = self.context_once(cancel).await;
        let result = if first.as_ref().is_err_and(|error| {
            error
                .downcast_ref::<BrowserError>()
                .is_some_and(BrowserError::should_retry)
        }) && !cancel.is_cancelled()
        {
            self.shutdown().await?;
            self.context_once(cancel).await
        } else {
            first
        };
        if result
            .as_ref()
            .is_err_and(crate::browser_error::requires_reset)
        {
            self.shutdown().await?;
        }
        result
    }

    async fn context_once(&mut self, cancel: &CancellationToken) -> Result<Value> {
        self.ensure_ready(cancel).await?;
        // Composer fragment pages (notably secondary product layouts) can omit
        // the global header. Retry transient rendering first, then observe the
        // same read-only indicators on the public home page.
        for phase in 0..2 {
            for attempt in 0..3 {
                match self
                    .evaluate_outcome(json!({"mode":"context"}), cancel)
                    .await?
                {
                    PageOutcome::Page(success) => {
                        let transient =
                            success
                                .page
                                .context_observation
                                .as_ref()
                                .is_none_or(|context| {
                                    context.signature.is_none()
                                        || context.access_state == "unknown"
                                        || context.account_state == "unknown"
                                });
                        if transient {
                            if attempt < 2 {
                                self.session.run(&["wait", "500"], cancel).await?;
                                continue;
                            }
                            if phase == 0 {
                                self.session.run(&["open", HOME], cancel).await?;
                                self.session.run(&["wait", "2000"], cancel).await?;
                                break;
                            }
                        }
                        return Ok(success.page.into_value());
                    }
                    PageOutcome::Error(failure) if failure.error == PageError::ResponseTooLarge => {
                        return Err(BrowserError::ResponseTooLarge.into());
                    }
                    _ => return Err(BrowserError::CaptchaOrBlocked.into()),
                }
            }
        }
        unreachable!("bounded context observation loop always returns")
    }

    pub async fn fetch_json(&mut self, path: &str, cancel: &CancellationToken) -> Result<Value> {
        let first = self.fetch_once(path, cancel).await;
        let result = if first.as_ref().is_err_and(|error| {
            error
                .downcast_ref::<BrowserError>()
                .is_some_and(BrowserError::should_retry)
        }) && !cancel.is_cancelled()
        {
            // Read-only request: one fresh-session retry for driver command
            // failures only. Reset classification is deliberately broader.
            self.shutdown().await?;
            self.fetch_once(path, cancel).await
        } else {
            first
        };
        if result
            .as_ref()
            .is_err_and(crate::browser_error::requires_reset)
        {
            // A scenario may recover a secondary fetch as a partial result.
            // Restore a known idle state before returning the recoverable error.
            self.shutdown().await?;
        }
        result
    }

    async fn fetch_once(&mut self, path: &str, cancel: &CancellationToken) -> Result<Value> {
        let home = url::Url::parse(HOME)?;
        let target = home.join(path)?;
        if target.origin() != home.origin() {
            return Err(BrowserError::InvalidOrigin.into());
        }
        self.ensure_ready(cancel).await?;

        let outcome = self
            .evaluate_outcome(json!({"mode":"fetch", "path":path}), cancel)
            .await?;
        if let PageOutcome::Page(success) = outcome {
            return Ok(success.page.into_value());
        }
        match outcome {
            PageOutcome::Error(failure) if failure.error == PageError::ResponseTooLarge => {
                return Err(BrowserError::ResponseTooLarge.into());
            }
            PageOutcome::Status(status) if !matches!(status.status, 403 | 307) => {
                return Err(BrowserError::HttpStatus(status.status).into());
            }
            _ => {}
        }

        // A direct page can still work when composer or warm-up is blocked.
        self.session.run(&["open", target.as_str()], cancel).await?;
        self.session.run(&["wait", "1500"], cancel).await?;
        let navigation = self
            .session
            .evaluate(
                "({url:location.href,status:performance.getEntriesByType('navigation')[0]?.responseStatus})",
                cancel,
            )
            .await?;
        let status = navigation["status"]
            .as_u64()
            .ok_or(BrowserError::NavigationStatusUnavailable)?;
        if !(200..400).contains(&status) {
            return Err(BrowserError::HttpStatus(status).into());
        }
        if !is_requested_page(
            target.as_str(),
            navigation["url"].as_str().unwrap_or_default(),
        ) {
            return Err(BrowserError::UnexpectedRedirect.into());
        }

        let outcome = self
            .evaluate_outcome(json!({"mode":"widgets"}), cancel)
            .await?;
        match outcome {
            PageOutcome::Page(success) => Ok(success.page.into_value()),
            PageOutcome::Error(failure) if failure.error == PageError::ResponseTooLarge => {
                Err(BrowserError::ResponseTooLarge.into())
            }
            _ => Err(BrowserError::CaptchaOrBlocked.into()),
        }
    }

    async fn evaluate_outcome(
        &mut self,
        options: Value,
        cancel: &CancellationToken,
    ) -> Result<PageOutcome> {
        let script = format!("({})({options})", include_str!("page.js"));
        let response = self.session.evaluate(&script, cancel).await?;
        self.last_used = Instant::now();
        serde_json::from_value(response).map_err(|_| BrowserError::InvalidBridgeResponse.into())
    }

    async fn ensure_ready(&mut self, cancel: &CancellationToken) -> Result<()> {
        if self.ready && self.last_used.elapsed() < Duration::from_secs(590) {
            return Ok(());
        }
        if self.ready {
            self.shutdown().await?;
        }
        self.session.ensure_running(cancel).await?;
        self.session
            .run(&["set", "viewport", "1920", "1080"], cancel)
            .await?;
        self.session.run(&["open", HOME], cancel).await?;
        self.session.run(&["wait", "12000"], cancel).await?;
        self.ready = true;
        self.last_used = Instant::now();
        Ok(())
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        self.ready = false;
        self.session.shutdown().await
    }
}

impl PageSource for OzonPages {
    async fn fetch_json(&mut self, path: &str, cancel: &CancellationToken) -> Result<Value> {
        OzonPages::fetch_json(self, path, cancel).await
    }
}

fn is_requested_page(requested: &str, actual: &str) -> bool {
    let (Ok(target), Ok(final_url)) = (url::Url::parse(requested), url::Url::parse(actual)) else {
        return false;
    };
    if target.origin() != final_url.origin() {
        return false;
    }
    if target.path() == "/search/" || target.path().starts_with("/category/") {
        if has_duplicate_query_key(&target, "sorting")
            || has_duplicate_query_key(&final_url, "sorting")
        {
            return false;
        }
        let predicted_category = target.path() == "/search/"
            && final_url.path().starts_with("/category/")
            && final_url
                .query_pairs()
                .any(|(key, value)| key == "category_was_predicted" && value == "true");
        let allowed_path = if target.path() == "/search/" {
            final_url.path() == "/search/" || predicted_category
        } else {
            target.path() == final_url.path()
        };
        let mut requested_query = semantic_query(&target);
        let actual_query = semantic_query(&final_url);
        if allowed_path && requested_query == actual_query {
            return true;
        }

        // Ozon canonicalizes a single numeric brand facet into the final category
        // path segment. Accept only that exact representation change.
        let Some(brand) = single_numeric_brand(&requested_query) else {
            return false;
        };
        let brand_pair = ("brand".to_owned(), brand.clone());
        requested_query.remove(&brand_pair);
        let category_path_matches = if target.path() == "/search/" {
            predicted_category
        } else {
            target.path() == final_url.path()
                || final_url
                    .path()
                    .trim_end_matches('/')
                    .strip_suffix(&format!("-{brand}"))
                    .and_then(|path| path.rsplit_once('/'))
                    .is_some_and(|(parent, _)| parent == target.path().trim_end_matches('/'))
        };
        return category_path_matches
            && category_path_has_brand(final_url.path(), &brand)
            && requested_query == actual_query;
    }
    let re = regex::Regex::new(r"^/product/(?:[^/]*-)?([0-9]+)/(reviews/)?$").unwrap();
    match (re.captures(target.path()), re.captures(final_url.path())) {
        (Some(a), Some(b)) => {
            a.get(1).map(|m| m.as_str()) == b.get(1).map(|m| m.as_str())
                && a.get(2).map(|m| m.as_str()) == b.get(2).map(|m| m.as_str())
        }
        _ => target.path() == final_url.path(),
    }
}

fn has_duplicate_query_key(url: &url::Url, expected: &str) -> bool {
    url.query_pairs()
        .filter(|(key, _)| key == expected)
        .nth(1)
        .is_some()
}

fn single_numeric_brand(
    query: &std::collections::BTreeMap<(String, String), usize>,
) -> Option<String> {
    let mut brands = query.iter().filter(|((key, _), _)| key == "brand");
    let ((_, brand), count) = brands.next()?;
    (brands.next().is_none()
        && *count == 1
        && !brand.is_empty()
        && brand.bytes().all(|byte| byte.is_ascii_digit()))
    .then(|| brand.clone())
}

fn category_path_has_brand(path: &str, brand: &str) -> bool {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .is_some_and(|segment| {
            segment == brand
                || segment
                    .strip_suffix(brand)
                    .is_some_and(|prefix| prefix.ends_with('-'))
        })
}

fn semantic_query(url: &url::Url) -> std::collections::BTreeMap<(String, String), usize> {
    let mut pairs = std::collections::BTreeMap::new();
    for (key, value) in url.query_pairs() {
        if is_navigation_metadata(&key) {
            continue;
        }
        if key == "sorting" && matches!(value.as_ref(), "score" | "popular") {
            continue;
        }
        let value = value.into_owned();
        *pairs.entry((key.into_owned(), value)).or_default() += 1;
    }
    pairs
}

fn is_navigation_metadata(key: &str) -> bool {
    matches!(
        key,
        "__rr" | "category_was_predicted" | "deny_category_prediction" | "from_global" | "at"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_must_match_product_or_search() {
        assert!(is_requested_page(
            "https://www.ozon.ru/product/123/",
            "https://www.ozon.ru/product/item-123/"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/product/123/reviews/",
            "https://www.ozon.ru/product/123/"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse",
            "https://www.ozon.ru/search/?text=phone"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/product/123/",
            "https://evil.example/product/123/"
        ));
    }

    #[test]
    fn search_redirect_preserves_all_semantic_query_pairs() {
        let requested = "https://www.ozon.ru/search/?text=mouse&brand=logitech&delivery=tomorrow&page=2&search_page_state=abc";
        assert!(is_requested_page(
            requested,
            "https://www.ozon.ru/search/?search_page_state=abc&page=2&delivery=tomorrow&brand=logitech&text=mouse"
        ));
        for actual in [
            "https://www.ozon.ru/search/?text=mouse&delivery=tomorrow&page=2&search_page_state=abc",
            "https://www.ozon.ru/search/?text=mouse&brand=other&delivery=tomorrow&page=2&search_page_state=abc",
            "https://www.ozon.ru/search/?text=mouse&brand=logitech&page=2&search_page_state=abc",
            "https://www.ozon.ru/search/?text=mouse&brand=logitech&delivery=tomorrow&page=3&search_page_state=abc",
            "https://www.ozon.ru/search/?text=mouse&brand=logitech&delivery=tomorrow&page=2&search_page_state=changed",
            "https://www.ozon.ru/search/?text=mouse&brand=logitech&delivery=tomorrow&page=2&search_page_state=abc&color=black",
        ] {
            assert!(!is_requested_page(requested, actual), "accepted {actual}");
        }
    }

    #[test]
    fn search_redirect_compares_duplicate_query_pairs_as_a_multiset() {
        assert!(is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=a&brand=b",
            "https://www.ozon.ru/search/?brand=b&text=mouse&brand=a"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=a&brand=b",
            "https://www.ozon.ru/search/?text=mouse&brand=b&brand=b"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=a&brand=a",
            "https://www.ozon.ru/search/?text=mouse&brand=a"
        ));
    }

    #[test]
    fn search_redirect_allows_only_known_navigation_metadata() {
        assert!(is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=logitech",
            "https://www.ozon.ru/search/?brand=logitech&text=mouse&__rr=1&deny_category_prediction=true&from_global=true&at=analytics-token"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=logitech",
            "https://www.ozon.ru/search/?text=mouse&brand=logitech&tracking_token=123"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=logitech",
            "https://www.ozon.ru/search/?text=mouse&brand=logitech&utm_source=unknown"
        ));
    }

    #[test]
    fn search_redirect_normalizes_only_default_sort() {
        assert!(is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&sorting=popular",
            "https://www.ozon.ru/search/?text=mouse&sorting=score"
        ));
        assert!(is_requested_page(
            "https://www.ozon.ru/search/?text=mouse",
            "https://www.ozon.ru/search/?text=mouse&sorting=score"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse",
            "https://www.ozon.ru/search/?text=mouse&sorting=price"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&sorting=rating",
            "https://www.ozon.ru/search/?text=mouse"
        ));
        for actual in [
            "https://www.ozon.ru/search/?text=mouse&sorting=price&sorting=score",
            "https://www.ozon.ru/search/?text=mouse&sorting=score&sorting=price",
            "https://www.ozon.ru/search/?text=mouse&sorting=rating&sorting=popular",
            "https://www.ozon.ru/search/?text=mouse&sorting=popular&sorting=rating",
        ] {
            assert!(!is_requested_page(
                "https://www.ozon.ru/search/?text=mouse&sorting=price",
                actual
            ));
        }
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&sorting=price&sorting=score",
            "https://www.ozon.ru/search/?text=mouse&sorting=price"
        ));
    }

    #[test]
    fn search_prediction_requires_marker_and_matching_semantics() {
        assert!(is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=logitech",
            "https://www.ozon.ru/category/mice-15871/?brand=logitech&text=mouse&category_was_predicted=true"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=logitech",
            "https://www.ozon.ru/category/mice-15871/?brand=logitech&text=mouse"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=logitech",
            "https://www.ozon.ru/category/mice-15871/?brand=other&text=mouse&category_was_predicted=true"
        ));
        assert!(is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=26303256",
            "https://www.ozon.ru/category/mice-15871/logitech-26303256/?text=mouse&category_was_predicted=true"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=26303256&brand=other",
            "https://www.ozon.ru/category/mice-15871/logitech-26303256/?text=mouse&category_was_predicted=true"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=mouse&brand=26303256",
            "https://www.ozon.ru/category/mice-15871/other-42/?text=mouse&category_was_predicted=true"
        ));
    }

    #[test]
    fn category_redirect_requires_exact_path_and_semantics() {
        assert!(is_requested_page(
            "https://www.ozon.ru/category/mice-15871/?brand=logitech&page=2",
            "https://www.ozon.ru/category/mice-15871/?page=2&brand=logitech&__rr=1"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/category/mice-15871/?brand=logitech&page=2",
            "https://www.ozon.ru/category/keyboards-15872/?brand=logitech&page=2"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/category/mice-15871/?brand=logitech&page=2",
            "https://www.ozon.ru/category/mice-15871/?brand=logitech&page=3"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/category/mice-15871/?brand=logitech&page=2",
            "https://www.ozon.ru/search/?brand=logitech&page=2"
        ));
        assert!(is_requested_page(
            "https://www.ozon.ru/category/mice-15871/?brand=26303256&page=2",
            "https://www.ozon.ru/category/mice-15871/logitech-26303256/?page=2"
        ));
        assert!(is_requested_page(
            "https://www.ozon.ru/category/mice-15871/?brand=26303256&sorting=score",
            "https://www.ozon.ru/category/mice-15871/logitech-26303256/"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/category/mice-15871/?brand=26303256&sorting=price",
            "https://www.ozon.ru/category/mice-15871/logitech-26303256/"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/category/mice-15871/?brand=26303256&sorting=rating",
            "https://www.ozon.ru/category/mice-15871/logitech-26303256/"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/category/mice-15871/?brand=26303256&page=2",
            "https://www.ozon.ru/category/keyboards-15872/logitech-26303256/?page=2"
        ));
    }
}
