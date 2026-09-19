use crate::{
    browser::{BrowserSession, RunningCommandOutcome},
    browser_error::BrowserError,
    page_outcome::{FilteredPage, PageError, PageOutcome},
    page_source::PageSource,
};
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const HOME: &str = "https://www.ozon.ru/";
const ADDRESS_BOOK: &str = "https://www.ozon.ru/modal/addressbook";

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

    /// Observe a narrow, privacy-filtered context projection. Authenticated
    /// contexts may read the selected saved-address city through an exact,
    /// read-only same-origin modal navigation; no address or action is exported.
    pub async fn context_json(&mut self, cancel: &CancellationToken) -> Result<Value> {
        let first = self.context_with_region_once(cancel).await;
        let result = if first.as_ref().is_err_and(|error| {
            error
                .downcast_ref::<BrowserError>()
                .is_some_and(BrowserError::should_retry)
        }) && !cancel.is_cancelled()
        {
            self.shutdown().await?;
            self.context_with_region_once(cancel).await
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

    async fn context_with_region_once(&mut self, cancel: &CancellationToken) -> Result<Value> {
        let mut before = self.context_header_once(cancel).await?;
        let should_probe = before.context_observation.as_ref().is_some_and(|context| {
            context.account_state == "authenticated" && context.region_label.is_none()
        }) && before
            .region_probe
            .as_ref()
            .is_some_and(|probe| probe.address_book_modal_available);
        if !should_probe {
            before.region_probe = None;
            return Ok(before.into_value());
        }

        let selected_region = self.address_book_region(cancel).await?;
        let after = self.context_header_once(cancel).await?;
        Ok(merge_context_region(before, after, selected_region)?.into_value())
    }

    async fn context_header_once(&mut self, cancel: &CancellationToken) -> Result<FilteredPage> {
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
                        return Ok(success.page);
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

    async fn address_book_region(&mut self, cancel: &CancellationToken) -> Result<Option<String>> {
        let operation = async {
            self.session.run(&["open", ADDRESS_BOOK], cancel).await?;
            self.session.run(&["wait", "1000"], cancel).await?;
            let navigation = navigation_probe(
                self.evaluate_outcome(json!({"mode":"navigation", "target":"addressBook"}), cancel)
                    .await?,
            )?;
            let status = navigation
                .status
                .ok_or(BrowserError::NavigationStatusUnavailable)?;
            if !(200..400).contains(&status) {
                return Err(BrowserError::HttpStatus(status).into());
            }
            if !navigation.route_valid {
                return Err(BrowserError::UnexpectedRedirect.into());
            }
            match self
                .evaluate_outcome(json!({"mode":"contextModal"}), cancel)
                .await?
            {
                PageOutcome::Page(success) => Ok(success
                    .page
                    .region_probe
                    .and_then(|probe| probe.selected_region_label)),
                PageOutcome::Error(failure) if failure.error == PageError::ResponseTooLarge => {
                    Err(BrowserError::ResponseTooLarge.into())
                }
                _ => Err(BrowserError::CaptchaOrBlocked.into()),
            }
        }
        .await;

        let cleanup_deadline = Instant::now() + Duration::from_secs(5);
        let restore = async {
            let completed = |outcome| match outcome {
                RunningCommandOutcome::Completed(value) => Some(value),
                RunningCommandOutcome::AlreadyClosed => None,
            };
            if completed(
                self.session
                    .run_if_running(&["open", HOME], cleanup_remaining(cleanup_deadline)?)
                    .await?,
            )
            .is_none()
            {
                return Ok(false);
            }
            if completed(
                self.session
                    .run_if_running(&["wait", "1000"], cleanup_remaining(cleanup_deadline)?)
                    .await?,
            )
            .is_none()
            {
                return Ok(false);
            }
            let script = page_script(&json!({"mode":"navigation", "target":"home"}));
            let Some(navigation) = completed(
                self.session
                    .evaluate_if_running(&script, cleanup_remaining(cleanup_deadline)?)
                    .await?,
            ) else {
                return Ok(false);
            };
            let navigation = navigation_probe(
                serde_json::from_value(navigation)
                    .map_err(|_| BrowserError::InvalidBridgeResponse)?,
            )?;
            let status = navigation
                .status
                .ok_or(BrowserError::NavigationStatusUnavailable)?;
            if !(200..400).contains(&status) {
                return Err(BrowserError::HttpStatus(status).into());
            }
            if !navigation.route_valid {
                return Err(BrowserError::UnexpectedRedirect.into());
            }
            Ok(true)
        }
        .await;

        match restore {
            Ok(true) => {}
            Ok(false) if operation.is_err() => return operation,
            Ok(false) => return Err(BrowserError::CleanupFailed.into()),
            Err(restore_error) => {
                self.shutdown().await?;
                return Err(restore_error);
            }
        }
        operation
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
        let can_recover_origin = matches!(
            options.get("mode").and_then(Value::as_str),
            Some("context" | "fetch")
        );
        let script = page_script(&options);
        let first = self.evaluate_script(&script, cancel).await?;
        if !matches!(
            first,
            PageOutcome::Error(ref failure) if failure.error == PageError::InvalidOrigin
        ) {
            if matches!(first, PageOutcome::Page(_)) {
                self.last_used = Instant::now();
            }
            return Ok(first);
        }

        // A surviving driver can attach to a replacement browser whose active
        // target is a new-tab page. Recover that warm session once through the
        // fixed trusted home URL; never follow the unexpected page's URL or
        // replay an evaluation bound to a product, modal, or navigation page.
        self.ready = false;
        if !can_recover_origin {
            return Err(BrowserError::InvalidOrigin.into());
        }
        self.session.run(&["open", HOME], cancel).await?;
        self.session.run(&["wait", "2000"], cancel).await?;
        let recovered = self.evaluate_script(&script, cancel).await?;
        match recovered {
            PageOutcome::Page(_) => {
                self.ready = true;
                self.last_used = Instant::now();
                Ok(recovered)
            }
            PageOutcome::Error(ref failure) if failure.error == PageError::InvalidOrigin => {
                Err(BrowserError::InvalidOrigin.into())
            }
            _ => Ok(recovered),
        }
    }

    async fn evaluate_script(
        &mut self,
        script: &str,
        cancel: &CancellationToken,
    ) -> Result<PageOutcome> {
        let response = self.session.evaluate(script, cancel).await?;
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

fn merge_context_region(
    mut before: FilteredPage,
    mut after: FilteredPage,
    selected_region: Option<String>,
) -> Result<FilteredPage> {
    before.region_probe = None;
    after.region_probe = None;
    let before_context = before
        .context_observation
        .as_ref()
        .ok_or_else(|| anyhow!("CONTEXT_CHANGED: context disappeared before region observation"))?;
    let after_context = after
        .context_observation
        .as_mut()
        .ok_or_else(|| anyhow!("CONTEXT_CHANGED: context disappeared after region observation"))?;
    if before_context.account_state != "authenticated"
        || after_context.account_state != before_context.account_state
        || after_context.access_state != before_context.access_state
        || after_context.signature != before_context.signature
    {
        return Err(anyhow!(
            "CONTEXT_CHANGED: account or header changed during region observation"
        ));
    }
    let Some(selected_region) = selected_region else {
        return Ok(after);
    };
    if [&before_context.region_label, &after_context.region_label]
        .into_iter()
        .flatten()
        .any(|region| region != &selected_region)
    {
        return Err(anyhow!(
            "CONTEXT_CHANGED: header region changed during region observation"
        ));
    }
    after_context.region_label = Some(selected_region.clone());
    after_context.region_verified = true;
    after_context.region_source_url = Some(ADDRESS_BOOK.to_owned());
    let digest = Sha256::digest(format!(
        "{}\n{selected_region}",
        after_context.account_state
    ));
    after_context.signature = Some(digest.iter().map(|byte| format!("{byte:02x}")).collect());
    Ok(after)
}

fn page_script(options: &Value) -> String {
    format!("({})({options})", include_str!("page.js"))
}

fn navigation_probe(outcome: PageOutcome) -> Result<crate::page_outcome::NavigationProbe> {
    match outcome {
        PageOutcome::Page(success) => success
            .page
            .navigation_probe
            .ok_or_else(|| BrowserError::InvalidBridgeResponse.into()),
        PageOutcome::Error(failure) if failure.error == PageError::ResponseTooLarge => {
            Err(BrowserError::ResponseTooLarge.into())
        }
        _ => Err(BrowserError::UnexpectedRedirect.into()),
    }
}

fn cleanup_remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| BrowserError::CommandTimeout.into())
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
        let mut actual_query = semantic_query(&final_url);
        if !normalize_observed_brand_prediction(
            &mut requested_query,
            &mut actual_query,
            target.path() == "/search/",
        ) {
            return false;
        }
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

fn normalize_observed_brand_prediction(
    requested: &mut std::collections::BTreeMap<(String, String), usize>,
    actual: &mut std::collections::BTreeMap<(String, String), usize>,
    allow_inferred_brand: bool,
) -> bool {
    let Some(_) = remove_valid_brand_prediction_marker(requested) else {
        return false;
    };
    let Some(actual_prediction) = remove_valid_brand_prediction_marker(actual) else {
        return false;
    };

    if !actual_prediction
        || !allow_inferred_brand
        || requested.keys().any(|(key, _)| key == "brand")
    {
        return true;
    }
    let brands = actual
        .iter()
        .filter(|((key, _), _)| key == "brand")
        .map(|((_, value), count)| (value.clone(), *count))
        .collect::<Vec<_>>();
    match brands.as_slice() {
        [] => true,
        [(brand, 1)] if !brand.is_empty() && brand.bytes().all(|byte| byte.is_ascii_digit()) => {
            actual.remove(&("brand".to_owned(), brand.clone()));
            true
        }
        _ => false,
    }
}

fn remove_valid_brand_prediction_marker(
    query: &mut std::collections::BTreeMap<(String, String), usize>,
) -> Option<bool> {
    let marker = ("brand_was_predicted".to_owned(), "true".to_owned());
    let marker_entries = query
        .iter()
        .filter(|((key, _), _)| key == "brand_was_predicted")
        .collect::<Vec<_>>();
    if marker_entries.is_empty() {
        return Some(false);
    }
    if marker_entries.len() != 1 || query.get(&marker) != Some(&1) {
        return None;
    }
    query.remove(&marker);
    Some(true)
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
    use crate::page_outcome::{ContextObservation, FilteredPage, RegionProbe};
    use std::os::unix::fs::PermissionsExt;

    fn scripted_pages(responses: &[Value]) -> (tempfile::TempDir, OzonPages) {
        let root = tempfile::tempdir().unwrap();
        let queue = root.path().join("responses");
        let log = root.path().join("commands");
        let script = root.path().join("fake-agent-browser");
        std::fs::write(
            &queue,
            responses
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
case " $* " in
  *" eval --stdin "*)
    IFS= read -r response < '{queue}'
    tail -n +2 '{queue}' > '{queue}.next'
    mv '{queue}.next' '{queue}'
    printf '{{"success":true,"data":{{"result":%s}}}}\n' "$response"
    ;;
  *) printf '{{"success":true,"data":{{}}}}\n' ;;
esac
"#,
                log = log.display(),
                queue = queue.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let session = BrowserSession::test_running(script, root.path());
        (
            root,
            OzonPages {
                session,
                ready: true,
                last_used: Instant::now(),
            },
        )
    }

    fn valid_context_outcome() -> Value {
        json!({
            "page": {
                "widgetStates": {},
                "contextObservation": {
                    "regionLabel": null,
                    "regionVerified": false,
                    "accountState": "anonymous",
                    "accessState": "available",
                    "signature": "stable"
                },
                "regionProbe": {
                    "addressBookModalAvailable": false,
                    "selectedRegionLabel": null
                }
            }
        })
    }

    fn command_log(root: &tempfile::TempDir) -> String {
        std::fs::read_to_string(root.path().join("commands")).unwrap()
    }

    #[tokio::test]
    async fn warm_context_revisits_home_once_after_unexpected_origin() {
        let (root, mut pages) =
            scripted_pages(&[json!({"error":"INVALID_ORIGIN"}), valid_context_outcome()]);

        let context = pages.context_json(&CancellationToken::new()).await.unwrap();

        assert_eq!(context["contextObservation"]["accountState"], "anonymous");
        assert_eq!(
            command_log(&root)
                .lines()
                .filter(|line| line.contains("open https://www.ozon.ru/"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn persistent_unexpected_origin_is_typed_and_bounded() {
        let (root, mut pages) = scripted_pages(&[
            json!({"error":"INVALID_ORIGIN"}),
            json!({"error":"INVALID_ORIGIN"}),
        ]);

        let error = pages
            .context_json(&CancellationToken::new())
            .await
            .unwrap_err();

        assert!(matches!(
            error.downcast_ref::<BrowserError>(),
            Some(BrowserError::InvalidOrigin)
        ));
        assert!(!pages.ready);
        let log = command_log(&root);
        assert_eq!(
            log.lines()
                .filter(|line| line.contains(" eval --stdin"))
                .count(),
            2
        );
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("open https://www.ozon.ru/"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn source_fetch_shares_unexpected_origin_recovery() {
        let (root, mut pages) = scripted_pages(&[
            json!({"error":"INVALID_ORIGIN"}),
            json!({"page":{"widgetStates":{"source":"recovered"}}}),
        ]);

        let source = pages
            .fetch_json(
                "api/composer-api.bx/page/json/v2?url=/search/",
                &CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(source["widgetStates"]["source"], "recovered");
        assert_eq!(
            command_log(&root)
                .lines()
                .filter(|line| line.contains("open https://www.ozon.ru/"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn page_bound_modes_never_replay_on_home_after_unexpected_origin() {
        for mode in ["widgets", "contextModal", "navigation"] {
            let (root, mut pages) = scripted_pages(&[
                json!({"error":"INVALID_ORIGIN"}),
                json!({"page":{"widgetStates":{}}}),
            ]);

            let error = pages
                .evaluate_outcome(json!({"mode":mode}), &CancellationToken::new())
                .await
                .unwrap_err();

            assert!(matches!(
                error.downcast_ref::<BrowserError>(),
                Some(BrowserError::InvalidOrigin)
            ));
            assert!(!pages.ready);
            let log = command_log(&root);
            assert_eq!(
                log.lines()
                    .filter(|line| line.contains(" eval --stdin"))
                    .count(),
                1,
                "mode {mode} was evaluated more than once"
            );
            assert!(
                !log.contains("open https://www.ozon.ru/"),
                "mode {mode} navigated away from its bound page"
            );
        }
    }

    #[tokio::test]
    async fn page_error_does_not_renew_warm_readiness() {
        let (_root, mut pages) = scripted_pages(&[json!({"error":"CAPTCHA_OR_BLOCKED"})]);
        let before = Instant::now() - Duration::from_secs(589);
        pages.last_used = before;

        let outcome = pages
            .evaluate_outcome(json!({"mode":"context"}), &CancellationToken::new())
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            PageOutcome::Error(failure) if failure.error == PageError::CaptchaOrBlocked
        ));
        assert_eq!(pages.last_used, before);
    }

    fn context_page(account_state: &str, signature: &str) -> FilteredPage {
        FilteredPage {
            widget_states: serde_json::Map::new(),
            seo: None,
            layout_tracking_info: None,
            context_observation: Some(ContextObservation {
                region_label: None,
                region_verified: false,
                region_source_url: None,
                account_state: account_state.to_owned(),
                access_state: "available".to_owned(),
                signature: Some(signature.to_owned()),
            }),
            region_probe: Some(RegionProbe {
                address_book_modal_available: true,
                selected_region_label: None,
            }),
            navigation_probe: None,
        }
    }

    #[test]
    fn authenticated_modal_region_requires_stable_before_and_after_context() {
        let merged = merge_context_region(
            context_page("authenticated", "same"),
            context_page("authenticated", "same"),
            Some("Москва".to_owned()),
        )
        .unwrap();
        let observation = merged.context_observation.unwrap();
        assert_eq!(observation.region_label.as_deref(), Some("Москва"));
        assert!(observation.region_verified);
        assert!(observation.signature.is_some_and(|value| value.len() == 64));
        assert!(merged.region_probe.is_none());

        assert!(
            merge_context_region(
                context_page("authenticated", "before"),
                context_page("authenticated", "after"),
                Some("Москва".to_owned()),
            )
            .is_err()
        );
        assert!(
            merge_context_region(
                context_page("authenticated", "same"),
                context_page("anonymous", "same"),
                Some("Москва".to_owned()),
            )
            .is_err()
        );
    }

    #[test]
    fn navigation_probe_rejects_url_fields_crossing_the_page_boundary() {
        assert!(
            serde_json::from_value::<PageOutcome>(json!({
                "page": {
                    "widgetStates": {},
                    "navigationProbe": {"routeValid": true, "status": 200, "url": "private"}
                }
            }))
            .is_err()
        );
    }

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
    fn search_brand_prediction_marker_is_final_only_navigation_metadata() {
        let requested = "https://www.ozon.ru/search/?text=GMKtec+M6+Ultra";
        let observed = "https://www.ozon.ru/category/mini-pk-15705/gmktec-100888485/?brand_was_predicted=true&category_was_predicted=true&deny_category_prediction=true&from_global=true&text=GMKtec+M6+Ultra";
        assert!(is_requested_page(requested, observed));
        assert!(is_requested_page(
            "https://www.ozon.ru/search/?text=GMKtec+M6+32",
            "https://www.ozon.ru/search/?brand=100888485&brand_was_predicted=true&deny_category_prediction=true&from_global=true&text=GMKtec+M6+32"
        ));
        assert!(is_requested_page(
            "https://www.ozon.ru/search/?text=GMKtec+M6+32&brand=100888485&brand_was_predicted=true&page=2",
            "https://www.ozon.ru/search/?page=2&brand_was_predicted=true&brand=100888485&text=GMKtec+M6+32&__rr=1"
        ));

        for actual in [
            "https://www.ozon.ru/category/mini-pk-15705/gmktec-100888485/?brand_was_predicted=false&category_was_predicted=true&text=GMKtec+M6+Ultra",
            "https://www.ozon.ru/category/mini-pk-15705/gmktec-100888485/?brand_was_predicted=true&brand_was_predicted=true&category_was_predicted=true&text=GMKtec+M6+Ultra",
            "https://www.ozon.ru/category/mini-pk-15705/gmktec-100888485/?brand_was_predicted=true&category_was_predicted=true&text=GMKtec+M6",
            "https://www.ozon.ru/category/mini-pk-15705/gmktec-100888485/?brand_was_predicted=true&category_was_predicted=true&text=GMKtec+M6+Ultra&color=black",
            "https://www.ozon.ru/category/mini-pk-15705/gmktec-100888485/?brand_was_predicted=true&category_was_predicted=true&text=GMKtec+M6+Ultra&sorting=price",
            "https://evil.example/category/mini-pk-15705/gmktec-100888485/?brand_was_predicted=true&category_was_predicted=true&text=GMKtec+M6+Ultra",
            "https://www.ozon.ru/search/?brand=100888485&text=GMKtec+M6+Ultra",
            "https://www.ozon.ru/search/?brand=not-numeric&brand_was_predicted=true&text=GMKtec+M6+Ultra",
            "https://www.ozon.ru/search/?brand=100888485&brand=42&brand_was_predicted=true&text=GMKtec+M6+Ultra",
        ] {
            assert!(!is_requested_page(requested, actual), "accepted {actual}");
        }

        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=GMKtec+M6+Ultra&brand=42",
            "https://www.ozon.ru/category/mini-pk-15705/gmktec-100888485/?brand_was_predicted=true&category_was_predicted=true&text=GMKtec+M6+Ultra"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/search/?text=GMKtec+M6+Ultra&brand=42",
            "https://www.ozon.ru/search/?text=GMKtec+M6+Ultra&brand=43&brand_was_predicted=true"
        ));
        assert!(is_requested_page(
            "https://www.ozon.ru/search/?text=GMKtec+M6+Ultra&brand=42",
            "https://www.ozon.ru/search/?text=GMKtec+M6+Ultra&brand=42&brand_was_predicted=true"
        ));
        assert!(!is_requested_page(
            "https://www.ozon.ru/category/mini-pk-15705/?text=GMKtec+M6+32",
            "https://www.ozon.ru/category/mini-pk-15705/?text=GMKtec+M6+32&brand=100888485&brand_was_predicted=true"
        ));
        for requested in [
            "https://www.ozon.ru/search/?text=GMKtec+M6+32&brand=100888485&brand_was_predicted=false",
            "https://www.ozon.ru/search/?text=GMKtec+M6+32&brand=100888485&brand_was_predicted=true&brand_was_predicted=true",
        ] {
            assert!(!is_requested_page(
                requested,
                "https://www.ozon.ru/search/?text=GMKtec+M6+32&brand=100888485&brand_was_predicted=true"
            ));
        }
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
