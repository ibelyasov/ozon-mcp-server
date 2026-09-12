use schemars::JsonSchema;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum NumericValue {
    Unsigned(u64),
    Signed(i64),
    Float(f64),
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum Discount {
    Text(String),
    Number(NumericValue),
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PriceType {
    OzonCard,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Warning {
    SearchResultsMayChangeBetweenCalls,
    ContinuationStalled,
    UnsafePaginatorUrlIgnored,
    ContinuationLimitReached,
    PriceOutsideRequestedRange,
    SearchMetadataTruncated,
    SearchWidgetMissing,
    DescriptionFetchFailed,
    DescriptionTextEmpty,
    DescriptionEmpty,
    ProductWidgetsMissing,
    ReviewsWidgetMissing,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchItem {
    pub sku: String,
    pub name: Option<String>,
    pub price: Option<f64>,
    pub currency: String,
    pub price_type: PriceType,
    pub price_label: Option<String>,
    pub delivery_label: Option<String>,
    pub seller: Option<String>,
    pub old_price: Option<f64>,
    pub discount: Option<Discount>,
    pub rating: Option<f64>,
    pub reviews: Option<u64>,
    pub brand: Option<String>,
    pub url: Option<String>,
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matches_price_range: Option<Option<bool>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchContext {
    pub region: Option<String>,
    pub region_verified: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchCoverage {
    pub page: u64,
    pub fetched_pages_this_call: usize,
    pub parsed_items: usize,
    pub parsed_offset_start: usize,
    pub parsed_offset_end: usize,
    pub returned: usize,
    pub unique_seen: usize,
    pub calls_in_chain: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct FacetRange {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_value: Option<NumericValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_value: Option<NumericValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_value: Option<NumericValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_value: Option<NumericValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FacetOption {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub label: Option<String>,
    pub selected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<Option<NumericValue>>,
    pub search_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Facet {
    #[serde(rename = "type")]
    pub kind: String,
    pub key: String,
    pub title: Option<String>,
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<FacetRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_url: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<FacetOption>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options_truncated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_radio: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_more_values: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Facets {
    pub items: Vec<Facet>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SortOption {
    pub label: Option<String>,
    pub selected: bool,
    pub search_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActiveFilterValue {
    pub label: Option<String>,
    pub search_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActiveFilter {
    pub key: Option<String>,
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub values: Vec<ActiveFilterValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    pub items: Vec<SearchItem>,
    pub count: usize,
    pub query: Option<String>,
    pub sort: String,
    pub search_url: String,
    pub next_cursor: Option<String>,
    pub has_next: Option<bool>,
    pub total: Option<u64>,
    pub context: SearchContext,
    pub coverage: SearchCoverage,
    pub warnings: Vec<Warning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facets: Option<Option<Facets>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_options: Option<Vec<SortOption>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_filters: Option<Vec<ActiveFilter>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema, Default)]
pub struct Description {
    pub text: String,
    pub images: Vec<String>,
}

impl Description {
    pub fn has_text(&self) -> bool {
        !self.text.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Duty {
    pub amount: f64,
    pub total: Option<f64>,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Seller {
    pub name: String,
    pub rating: Option<f64>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProductDetails {
    pub sku: Option<String>,
    pub name: Option<String>,
    pub url: Option<String>,
    pub price: Option<f64>,
    pub price_regular: Option<f64>,
    pub old_price: Option<f64>,
    pub duty: Option<Duty>,
    pub available: Option<bool>,
    pub rating: Option<f64>,
    pub reviews: Option<u64>,
    pub seller: Option<Seller>,
    pub images: Vec<String>,
    pub characteristics: BTreeMap<String, String>,
    pub description: Description,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    pub author: Option<String>,
    pub score: Option<f64>,
    pub comment: Option<String>,
    pub pros: Option<String>,
    pub cons: Option<String>,
    pub date: Option<String>,
    pub useful: Option<u64>,
    pub purchased: Option<bool>,
    pub has_photos: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReviewPage {
    pub rating: Option<f64>,
    pub total_reviews: Option<u64>,
    pub count: usize,
    pub reviews: Vec<Review>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Warning>,
}
