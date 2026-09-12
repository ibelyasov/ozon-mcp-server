use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PageOutcome {
    Page(PageSuccess),
    Error(PageFailure),
    Status(PageStatus),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageSuccess {
    pub page: FilteredPage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageFailure {
    pub error: PageError,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageStatus {
    pub status: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FilteredPage {
    pub widget_states: Map<String, Value>,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub seo: Option<Seo>,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub layout_tracking_info: Option<LayoutTrackingInfo>,
}

fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seo {
    pub title: Option<String>,
    pub link: Vec<SeoLink>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeoLink {
    pub href: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutTrackingInfo {
    pub sku: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PageError {
    CaptchaOrBlocked,
    FetchFailed,
    FetchTimeout,
    InvalidOptions,
    InvalidOrigin,
    InvalidResponse,
    ResponseTooLarge,
}

impl FilteredPage {
    pub fn into_value(self) -> Value {
        serde_json::to_value(self).expect("serializing a validated page cannot fail")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Fixture {
        valid: Vec<ValidCase>,
        invalid: Vec<Value>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ValidCase {
        outcome: Value,
        kind: String,
    }

    #[test]
    fn shared_outcome_contract_accepts_all_variants_and_fails_closed() {
        let fixture: Fixture =
            serde_json::from_str(include_str!("../tests/fixtures/page-outcomes.json")).unwrap();

        for case in fixture.valid {
            let outcome: PageOutcome = serde_json::from_value(case.outcome).unwrap();
            let actual = match outcome {
                PageOutcome::Page(_) => "page",
                PageOutcome::Error(_) => "error",
                PageOutcome::Status(_) => "status",
            };
            assert_eq!(actual, case.kind);
        }
        for outcome in fixture.invalid {
            assert!(
                serde_json::from_value::<PageOutcome>(outcome.clone()).is_err(),
                "accepted malformed outcome {outcome}"
            );
        }
    }
}
