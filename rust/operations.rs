use anyhow::{Result, bail, ensure};
use percent_encoding::percent_decode_str;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    model::{ProductDetails, ReviewPage, SearchResponse, Warning},
    page_source::PageSource,
    parse,
    search::{PreparedSearch, SearchArgs},
    widgets::WidgetSet,
};

fn reviews_limit() -> usize {
    10
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

pub enum Operation {
    Search(SearchArgs),
    Details(DetailsArgs),
    Reviews(ReviewsArgs),
}

pub struct PreparedOperation(PreparedKind);

enum PreparedKind {
    Search(PreparedSearch),
    Details { path: String },
    Reviews { path: String, limit: usize },
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum OperationResult {
    Search(SearchResponse),
    Details(ProductDetails),
    Reviews(ReviewPage),
}

impl Operation {
    pub fn prepare(self) -> Result<PreparedOperation> {
        let kind = match self {
            Self::Search(args) => PreparedKind::Search(PreparedSearch::prepare(args)?),
            Self::Details(args) => PreparedKind::Details {
                path: product_path(&args.product)?,
            },
            Self::Reviews(args) => {
                if !(1..=30).contains(&args.limit) {
                    bail!("limit must be between 1 and 30");
                }
                PreparedKind::Reviews {
                    path: format!("{}reviews/", product_path(&args.product)?),
                    limit: args.limit,
                }
            }
        };
        Ok(PreparedOperation(kind))
    }
}

impl PreparedOperation {
    pub async fn execute(
        self,
        source: &mut impl PageSource,
        cancel: &CancellationToken,
    ) -> Result<OperationResult> {
        match self.0 {
            PreparedKind::Search(search) => Ok(OperationResult::Search(
                search.execute(source, cancel).await?,
            )),
            PreparedKind::Details { path } => {
                check_cancel(cancel)?;
                let base = source.fetch_json(&path, cancel).await?;
                check_cancel(cancel)?;
                let needs_secondary = !parse::parse_description(&base).has_text();
                let mut secondary = None;
                let mut secondary_failed = false;
                if needs_secondary {
                    check_cancel(cancel)?;
                    match source
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
                check_cancel(cancel)?;
                let mut result = parse::parse_details(&base, secondary.as_ref());
                if secondary_failed {
                    push_warning(&mut result.warnings, Warning::DescriptionFetchFailed);
                }
                if !result.description.has_text() {
                    let warning = if result.description.images.is_empty() {
                        Warning::DescriptionEmpty
                    } else {
                        Warning::DescriptionTextEmpty
                    };
                    push_warning(&mut result.warnings, warning);
                }
                let widgets = WidgetSet::new(&base);
                if !widgets.has_matching("webProductHeading", |value| value.get("title").is_some())
                    || !widgets.has_matching("webPrice", |value| {
                        ["cardPrice", "price", "originalPrice", "isAvailable"]
                            .iter()
                            .any(|key| value.get(*key).is_some())
                    })
                {
                    push_warning(&mut result.warnings, Warning::ProductWidgetsMissing);
                }
                Ok(OperationResult::Details(result))
            }
            PreparedKind::Reviews { path, limit } => {
                check_cancel(cancel)?;
                let page = source.fetch_json(&path, cancel).await?;
                check_cancel(cancel)?;
                let mut result = parse::parse_reviews(&page, limit);
                if !WidgetSet::new(&page).has_matching("webListReviews", |value| {
                    value
                        .get("reviews")
                        .is_some_and(serde_json::Value::is_array)
                        || value.get("items").is_some_and(serde_json::Value::is_array)
                }) {
                    push_warning(&mut result.warnings, Warning::ReviewsWidgetMissing);
                }
                Ok(OperationResult::Reviews(result))
            }
        }
    }
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

fn push_warning(warnings: &mut Vec<Warning>, warning: Warning) {
    if !warnings.contains(&warning) {
        warnings.push(warning);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use serde_json::{Value, json};
    use std::collections::VecDeque;

    struct ScriptedSource {
        pages: VecDeque<Result<Value>>,
        paths: Vec<String>,
    }
    impl PageSource for ScriptedSource {
        async fn fetch_json(&mut self, path: &str, _: &CancellationToken) -> Result<Value> {
            self.paths.push(path.to_owned());
            self.pages.pop_front().expect("scripted response")
        }
        async fn shutdown(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn validates_before_page_source_execution() {
        assert!(
            Operation::Reviews(ReviewsArgs {
                product: "1".into(),
                limit: 0
            })
            .prepare()
            .is_err()
        );
        assert!(
            Operation::Search(SearchArgs {
                query: None,
                search_url: None,
                next_cursor: None,
                include_facets: None,
                sort: None,
                price_min: None,
                price_max: None,
                limit: 12
            })
            .prepare()
            .is_err()
        );
    }

    #[tokio::test]
    async fn details_scenario_fetches_secondary_and_parser_merges_description() {
        let mut source = ScriptedSource {
            pages: VecDeque::from([
                Ok(
                    json!({"widgetStates": {"webProductHeading-x": {"title": "Example"}, "webPrice-x": {"price": "10 ₽"}, "webDescription-x": {"richAnnotation": "<img src='base.jpg'>"}}}),
                ),
                Ok(
                    json!({"widgetStates": {"webDescription-x": {"richAnnotation": "Text<img src='extra.jpg'>"}}}),
                ),
            ]),
            paths: Vec::new(),
        };
        let result = Operation::Details(DetailsArgs {
            product: "1".into(),
        })
        .prepare()
        .unwrap()
        .execute(&mut source, &CancellationToken::new())
        .await
        .unwrap();
        let OperationResult::Details(result) = result else {
            panic!()
        };
        assert_eq!(result.description.text, "Text");
        assert_eq!(result.description.images, ["base.jpg", "extra.jpg"]);
        assert_eq!(source.paths.len(), 2);
    }

    #[tokio::test]
    async fn base_description_completes_details_without_another_fetch() {
        let page: Value =
            serde_json::from_str(include_str!("../tests/fixtures/domain/details-page.json"))
                .unwrap();
        let mut source = ScriptedSource {
            pages: VecDeque::from([Ok(page)]),
            paths: Vec::new(),
        };
        let result = Operation::Details(DetailsArgs {
            product: "901".into(),
        })
        .prepare()
        .unwrap()
        .execute(&mut source, &CancellationToken::new())
        .await
        .unwrap();
        let OperationResult::Details(result) = result else {
            panic!()
        };
        assert_eq!(result.description.text, "Useful text");
        assert!(result.warnings.is_empty());
        assert_eq!(source.paths, ["/product/901/"]);
    }

    #[tokio::test]
    async fn cancellation_before_or_after_base_fetch_stops_the_details_scenario() {
        struct CancellingSource {
            calls: usize,
        }
        impl PageSource for CancellingSource {
            async fn fetch_json(&mut self, _: &str, cancel: &CancellationToken) -> Result<Value> {
                self.calls += 1;
                cancel.cancel();
                // No description: without cancellation, a secondary fetch is required.
                Ok(json!({"widgetStates": {}}))
            }
            async fn shutdown(&mut self) -> Result<()> {
                Ok(())
            }
        }
        for cancel_before_fetch in [true, false] {
            let cancel = CancellationToken::new();
            if cancel_before_fetch {
                cancel.cancel();
            }
            let mut source = CancellingSource { calls: 0 };
            let result = Operation::Details(DetailsArgs {
                product: "901".into(),
            })
            .prepare()
            .unwrap()
            .execute(&mut source, &cancel)
            .await;
            assert!(result.is_err());
            assert_eq!(source.calls, usize::from(!cancel_before_fetch));
        }
    }

    #[tokio::test]
    async fn secondary_failure_returns_partial_details_with_warning() {
        let mut source = ScriptedSource {
            pages: VecDeque::from([
                Ok(
                    json!({"widgetStates": {"webProductHeading-x": {"title": "Example"}, "webPrice-x": {"price": "10 ₽"}, "webDescription-x": {"richAnnotation": "<img src='base.jpg'>"}}}),
                ),
                Err(anyhow!("secondary failed")),
            ]),
            paths: Vec::new(),
        };
        let result = Operation::Details(DetailsArgs {
            product: "1".into(),
        })
        .prepare()
        .unwrap()
        .execute(&mut source, &CancellationToken::new())
        .await
        .unwrap();
        let OperationResult::Details(result) = result else {
            panic!()
        };
        assert!(result.warnings.contains(&Warning::DescriptionFetchFailed));
        assert_eq!(result.description.images, ["base.jpg"]);
    }

    #[tokio::test]
    async fn usable_review_widget_after_wrong_shape_does_not_warn() {
        let mut source = ScriptedSource {
            pages: VecDeque::from([Ok(json!({"widgetStates": {
                "webListReviews-a": {},
                "webListReviews-b": {"reviews": []}
            }}))]),
            paths: Vec::new(),
        };
        let result = Operation::Reviews(ReviewsArgs {
            product: "1".into(),
            limit: 10,
        })
        .prepare()
        .unwrap()
        .execute(&mut source, &CancellationToken::new())
        .await
        .unwrap();
        let OperationResult::Reviews(result) = result else {
            panic!()
        };
        assert!(!result.warnings.contains(&Warning::ReviewsWidgetMissing));
        assert_eq!(result.count, 0);
    }

    #[test]
    fn accepts_product_identifiers_and_rejects_unsafe_paths() {
        assert_eq!(
            product_path("https://www.ozon.ru/product/товар-123/reviews/?x=1").unwrap(),
            "/product/товар-123/"
        );
        assert_eq!(product_path("7").unwrap(), "/product/7/");
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
