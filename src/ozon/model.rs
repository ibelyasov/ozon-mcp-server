use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceType {
    OzonCard,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchItem {
    pub source_index: usize,
    pub source_locator: String,
    pub sku: String,
    pub name: Option<String>,
    pub price_minor: Option<u64>,
    pub price_type: PriceType,
    pub price_label: Option<String>,
    pub delivery_label: Option<String>,
    pub seller: Option<String>,
    pub rating: Option<f64>,
    pub reviews: Option<u64>,
    pub url: Option<String>,
    pub image: Option<String>,
    pub matches_price_range: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FacetOption {
    pub label: Option<String>,
    pub selected: bool,
    pub search_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Facet {
    pub title: Option<String>,
    pub selected: Option<bool>,
    pub search_url: Option<String>,
    pub options: Option<Vec<FacetOption>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Facets {
    pub items: Vec<Facet>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SortOption {
    pub label: Option<String>,
    pub selected: bool,
    pub search_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchResponse {
    pub acquisition: Option<Acquisition>,
    pub items: Vec<SearchItem>,
    pub search_url: String,
    pub next_path: Option<String>,
    pub source: SourceCoverage,
    pub refinements_source_truncated: bool,
    pub has_next: Option<bool>,
    pub total: Option<u64>,
    pub warnings: Vec<Warning>,
    pub facets: Option<Facets>,
    pub sort_options: Option<Vec<SortOption>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Description {
    pub acquisition: Option<Acquisition>,
    pub text: String,
    pub source: SourceCoverage,
    pub image_origins: Vec<SourceImage>,
}

impl Description {
    pub fn has_text(&self) -> bool {
        !self.text.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Duty {
    pub acquisition: Option<Acquisition>,
    pub amount_minor: u64,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Seller {
    pub name: String,
    pub rating: Option<f64>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProductDetails {
    pub acquisition: Option<Acquisition>,
    pub sku: Option<String>,
    pub name: Option<String>,
    pub url: Option<String>,
    pub displayed_price_minor: Option<u64>,
    /// Explicitly observed `cardPrice`; never substituted by the regular price.
    pub card_price_minor: Option<u64>,
    pub regular_price_minor: Option<u64>,
    pub duty: Option<Duty>,
    pub available: Option<bool>,
    pub rating: Option<f64>,
    pub reviews: Option<u64>,
    pub seller: Option<Seller>,
    pub delivery_label: Option<String>,
    pub image_origins: Vec<SourceImage>,
    pub gallery_source: SourceCoverage,
    pub supplement: Option<SupplementObservation>,
    pub characteristics: Vec<Characteristic>,
    /// `webShortCharacteristics` is a summary widget and carries no proof that
    /// the full product specification was returned.
    pub characteristics_complete: bool,
    pub description: Description,
    pub variants: ProductVariants,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceSectionStatus {
    Available,
    Partial,
    Unknown,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProductVariant {
    pub sku: String,
    pub title: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProductVariants {
    pub status: SourceSectionStatus,
    pub items: Vec<ProductVariant>,
    pub has_next: Option<bool>,
    pub next_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Review {
    pub source_index: usize,
    pub source_locator: String,
    pub photo_origins: Vec<SourceImage>,
    pub review_id: Option<String>,
    pub author: Option<String>,
    pub score: Option<f64>,
    pub comment: Option<String>,
    pub pros: Option<String>,
    pub cons: Option<String>,
    pub date: Option<String>,
    pub purchased: Option<bool>,
    pub variant_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewAggregationScope {
    SpecificSku,
    MultipleVariants,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewRefinement {
    pub label: String,
    pub url: String,
    pub selected: Option<bool>,
    pub kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewPage {
    pub acquisition: Option<Acquisition>,
    pub source: SourceCoverage,
    pub source_url: Option<String>,
    pub observed_at: Option<String>,
    pub rating: Option<f64>,
    pub total_reviews: Option<u64>,
    pub reviews: Vec<Review>,
    pub next_path: Option<String>,
    pub has_next: Option<bool>,
    pub refinements: Vec<ReviewRefinement>,
    pub aggregation_scope: ReviewAggregationScope,
    pub warnings: Vec<Warning>,
}

/// A valid empty array is observed emptiness; an absent or malformed array is unknown.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceCoverage {
    pub present: bool,
    pub raw_rows: usize,
    pub parsed_rows: usize,
    pub malformed_rows: usize,
    pub source_truncated: bool,
    pub local_truncated: bool,
}
impl SourceCoverage {
    pub fn complete(&self) -> bool {
        self.present && self.malformed_rows == 0 && !self.source_truncated && !self.local_truncated
    }
    pub fn status(&self) -> SourceSectionStatus {
        if !self.present {
            SourceSectionStatus::Unknown
        } else if self.complete() {
            SourceSectionStatus::Available
        } else {
            SourceSectionStatus::Partial
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Characteristic {
    pub source_index: usize,
    pub source_locator: String,
    pub label: String,
    pub value: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceImage {
    pub source_locator: Option<String>,
    pub url: String,
    pub section: String,
    pub index: usize,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFailureCode {
    Blocked,
    HttpStatus,
    InvalidResponse,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceFailure {
    pub code: SourceFailureCode,
    pub message: String,
    pub stage: SourceStage,
    pub http_status: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceStage {
    ProductSupplement,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SupplementObservation {
    pub requested_sections: Vec<SupplementSection>,
    pub source_url: String,
    pub observed_at: String,
    pub status: SourceSectionStatus,
    pub error: Option<SourceFailure>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupplementSection {
    Description,
    Duty,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Acquisition {
    pub source_url: String,
    pub observed_at: String,
    pub method: CaptureMethod,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMethod {
    Composer,
    Dom,
}
