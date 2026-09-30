use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PageOutcome {
    Page(Box<PageSuccess>),
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acquisition: Option<crate::ozon::model::Acquisition>,
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
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub context_observation: Option<ContextObservation>,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub region_probe: Option<RegionProbe>,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub navigation_probe: Option<NavigationProbe>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ContextObservation {
    #[serde(deserialize_with = "deserialize_city")]
    pub region_label: Option<String>,
    pub region_verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region_source_url: Option<String>,
    pub account_state: AccountState,
    pub access_state: AccessState,
    #[serde(deserialize_with = "deserialize_signature")]
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountState {
    Authenticated,
    Anonymous,
    Unknown,
}
impl AccountState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Authenticated => "authenticated",
            Self::Anonymous => "anonymous",
            Self::Unknown => "unknown",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessState {
    Available,
    Blocked,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RegionProbe {
    pub address_book_modal_available: bool,
    pub selected_region_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NavigationProbe {
    pub route_valid: bool,
    pub status: Option<u64>,
}

fn deserialize_city<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let value = Option::<String>::deserialize(deserializer)?;
    if let Some(city) = value.as_ref() {
        let valid = !city.is_empty()
            && city.encode_utf16().count() <= 100
            && regex::Regex::new(r"^[\p{L} -]+$").unwrap().is_match(city);
        let private=regex::Regex::new(r"(?iu)\b(?:адрес|улица|улицы|дом|дома|квартира|квартиры|подъезд|этаж|доставка|пункт|выдача|укажите|сегодня|завтра|послезавтра)\b").unwrap();
        if !valid || private.is_match(city) {
            return Err(serde::de::Error::custom("invalid city-only context label"));
        }
    }
    Ok(value)
}
fn deserialize_signature<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let value = Option::<String>::deserialize(deserializer)?;
    if value.as_ref().is_some_and(|v| {
        v.len() != 64
            || !v
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
    }) {
        return Err(serde::de::Error::custom("invalid context signature"));
    }
    Ok(value)
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
    fn typed_context_rejects_unknown_states_private_labels_and_invalid_signatures() {
        let valid = serde_json::json!({"regionLabel":"Домодедово","regionVerified":true,"accountState":"anonymous","accessState":"available","signature":"a".repeat(64)});
        assert!(serde_json::from_value::<ContextObservation>(valid.clone()).is_ok());
        for (key, value) in [
            ("accountState", serde_json::json!("admin")),
            ("accessState", serde_json::json!("authenticated")),
            ("regionLabel", serde_json::json!("Москва, улица Примерная")),
            ("signature", serde_json::json!("bad")),
        ] {
            let mut bad = valid.clone();
            bad[key] = value;
            assert!(serde_json::from_value::<ContextObservation>(bad).is_err());
        }
    }

    #[test]
    fn shared_outcome_contract_accepts_all_variants_and_fails_closed() {
        let fixture: Fixture =
            serde_json::from_str(include_str!("../../tests/fixtures/page-outcomes.json")).unwrap();

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
