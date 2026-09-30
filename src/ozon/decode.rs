use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, HashSet},
    sync::LazyLock,
};
use url::Url;

use crate::{
    ozon::model::{
        Characteristic, Description, Duty, PriceType, ProductDetails, ProductVariant,
        ProductVariants, Review, ReviewAggregationScope, ReviewPage, ReviewRefinement, SearchItem,
        Seller, SourceCoverage, SourceImage, SourceSectionStatus,
    },
    ozon::widgets::WidgetSet,
};

const OZON_ORIGIN: &str = "https://www.ozon.ru";

fn widget_locator(key: &str) -> String {
    format!(
        "/widgetStates/{}",
        key.replace('~', "~0").replace('/', "~1")
    )
}
fn object(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

fn text(value: &Value) -> Option<String> {
    let value = value.as_str()?;
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    (!normalized.is_empty()).then_some(normalized)
}

fn text_from(value: Option<&Value>) -> Option<String> {
    let value = value?;
    text(value)
        .or_else(|| object(value).and_then(|o| text(o.get("text")?)))
        .or_else(|| object(value).and_then(|o| text(o.get("content")?)))
}

fn parse_json_value(value: Option<&Value>) -> Option<Value> {
    let value = value?;
    if value.is_object() || value.is_array() {
        return Some(value.clone());
    }
    serde_json::from_str(value.as_str()?).ok()
}

fn widget_matching(
    page: &Value,
    name: &str,
    predicate: impl FnMut(&Value) -> bool,
) -> Option<Value> {
    WidgetSet::new(page).first_matching(name, predicate)
}

fn valid_grouped_integer(value: &str) -> bool {
    if !value.is_empty() && value.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    let separators: HashSet<char> = value.chars().filter(|c| *c == '.' || *c == ',').collect();
    if separators.len() != 1 {
        return false;
    }
    let separator = *separators.iter().next().unwrap();
    let groups: Vec<_> = value.split(separator).collect();
    groups.len() > 1
        && (1..=3).contains(&groups[0].len())
        && groups[0].chars().all(|c| c.is_ascii_digit())
        && groups[1..]
            .iter()
            .all(|group| group.len() == 3 && group.chars().all(|c| c.is_ascii_digit()))
}

fn parse_localized_number(value: &str) -> Option<f64> {
    let compact: String = value
        .chars()
        .filter(|c| !matches!(c, ' ' | '\u{00a0}' | '\u{202f}') && !c.is_whitespace())
        .collect();
    let re = Regex::new(r"^[+-]?\d(?:[\d.,]*\d)?$").unwrap();
    if !re.is_match(&compact) {
        return None;
    }
    let (sign, body) = match compact.as_bytes().first() {
        Some(b'-') => (-1.0, &compact[1..]),
        Some(b'+') => (1.0, &compact[1..]),
        _ => (1.0, compact.as_str()),
    };
    let comma = body.rfind(',');
    let dot = body.rfind('.');
    let decimal_index = match (comma, dot) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) | (None, Some(a)) if body.len() - a - 1 <= 2 => Some(a),
        _ => None,
    };
    let normalized = if let Some(index) = decimal_index {
        let integer = &body[..index];
        let fraction = &body[index + 1..];
        if fraction.is_empty()
            || !fraction.chars().all(|c| c.is_ascii_digit())
            || !valid_grouped_integer(integer)
        {
            return None;
        }
        format!("{}.{}", integer.replace(['.', ','], ""), fraction)
    } else {
        if !valid_grouped_integer(body) {
            return None;
        }
        body.replace(['.', ','], "")
    };
    let parsed = normalized.parse::<f64>().ok()? * sign;
    parsed.is_finite().then_some(parsed)
}

/// Exact decimal conversion; no binary floating-point rounding or first-number guessing.
pub(crate) fn decimal_minor(source: &str) -> Option<u64> {
    let compact: String = source
        .chars()
        .filter(|c| !matches!(c, ' ' | '\u{00a0}' | '\u{202f}'))
        .collect();
    let mut parts = compact.split(['.', ',']);
    let whole = parts.next()?;
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let fraction = parts.next().unwrap_or("");
    if parts.next().is_some() || fraction.len() > 2 || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let cents = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<u64>().ok()? * 10,
        _ => fraction.parse().ok()?,
    };
    whole
        .parse::<u64>()
        .ok()?
        .checked_mul(100)?
        .checked_add(cents)
        .filter(|n| *n <= 9_007_199_254_740_991)
}
fn price_to_minor(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(n) => decimal_minor(&n.to_string()),
        Value::String(s) => {
            let re = Regex::new(r"(?iu)^\s*([0-9]+(?:[ \x{00a0}\x{202f}][0-9]{3})*(?:[.,][0-9]{1,2})?)\s*(?:₽|руб\.?|рублей)\s*$").unwrap();
            decimal_minor(re.captures(s)?.get(1)?.as_str())
        }
        _ => None,
    }
}

fn scaled_count(number: f64, multiplier: f64) -> Option<u64> {
    let scaled = number * multiplier;
    (number >= 0.0 && scaled.is_finite() && scaled <= 9_007_199_254_740_991.0)
        .then_some(scaled.round() as u64)
}

fn suffix_multiplier(suffix: &str) -> f64 {
    let suffix = suffix.to_lowercase();
    if suffix.starts_with("тыс") || suffix == "k" {
        1_000.0
    } else if suffix.starts_with("млн") || suffix.starts_with("миллион") || suffix == "m"
    {
        1_000_000.0
    } else {
        1.0
    }
}

fn count_from_value(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(number) => scaled_count(number.as_f64()?, 1.0),
        Value::String(source) => {
            let re = Regex::new(r"(?iu)^\s*([+-]?\d(?:[\d\s\u{00a0}\u{202f}.,]*\d)?)(?:\s*)(тыс(?:яч[аи])?\.?|млн\.?|миллион[а-яё]*|k|m)?\s*$").unwrap();
            let captures = re.captures(source)?;
            scaled_count(
                parse_localized_number(captures.get(1)?.as_str())?,
                suffix_multiplier(captures.get(2).map_or("", |m| m.as_str())),
            )
        }
        _ => None,
    }
}

fn parse_review_count(value: Option<&Value>) -> Option<u64> {
    let source = value?.as_str()?;
    let re = Regex::new(r"(?iu)([+-]?\d(?:[\d\s\u{00a0}\u{202f}.,]*\d)?)(?:\s*)(тыс(?:яч[аи])?\.?|млн\.?|миллион[а-яё]*|k|m)?\s*(?:отзыв[а-яё]*|reviews?)").unwrap();
    re.captures_iter(source).find_map(|captures| {
        scaled_count(
            parse_localized_number(captures.get(1)?.as_str())?,
            suffix_multiplier(captures.get(2).map_or("", |m| m.as_str())),
        )
    })
}

fn rating_from_value(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    if let Some(number) = value.as_f64() {
        return (number.is_finite() && (0.0..=5.0).contains(&number)).then_some(number);
    }
    if let Some(record) = value.as_object() {
        return rating_from_value(record.get("text"))
            .or_else(|| rating_from_value(record.get("value")));
    }
    let source = text(value)?;
    if let Some(number) = parse_localized_number(&source)
        && (0.0..=5.0).contains(&number)
    {
        return Some(number);
    }
    let explicit = Regex::new(
        r"(?iu)(?:^|[^\d])([0-5](?:[.,]\d+)?)(?:\s*)(?:из\s*5|/\s*5|[★⭐]|звезд[а-яё]*)",
    )
    .unwrap();
    if let Some(captures) = explicit.captures(&source) {
        return parse_localized_number(captures.get(1)?.as_str()).filter(|n| *n <= 5.0);
    }
    let at_start = Regex::new(r"^([0-5](?:[.,]\d+)?)").unwrap();
    let captures = at_start.captures(&source)?;
    let matched = captures.get(0)?;
    let remaining = &source[matched.end()..];
    let reviews = Regex::new(r"(?iu)^\s*(?:\d|(?:(?:тыс(?:яч[аи])?\.?|млн\.?|миллион[а-яё]*|k|m)\s*)?(?:отзыв[а-яё]*|reviews?))").unwrap();
    if reviews.is_match(remaining) {
        return None;
    }
    parse_localized_number(captures.get(1)?.as_str()).filter(|n| *n <= 5.0)
}

fn collect_strings(value: &Value, result: &mut Vec<String>) {
    match value {
        Value::String(s) => result.push(s.clone()),
        Value::Array(items) => items.iter().for_each(|item| collect_strings(item, result)),
        Value::Object(record) => record
            .values()
            .for_each(|item| collect_strings(item, result)),
        _ => {}
    }
}

fn normalize_sku(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Number(n) if n.is_u64() => Some(n.as_u64()?.to_string()),
        Value::String(s) => {
            let candidate = s.trim();
            (!candidate.is_empty() && candidate.chars().all(|c| c.is_ascii_digit()))
                .then(|| candidate.to_owned())
        }
        _ => None,
    }
}

fn clean_url(link: Option<&Value>) -> Option<String> {
    let link = link?.as_str()?.trim();
    if link.is_empty() {
        return None;
    }
    let base = Url::parse(OZON_ORIGIN).ok()?;
    let mut url = base.join(link).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(url.host_str().map(|h| h.to_ascii_lowercase()), Some(h) if h == "ozon.ru" || h == "www.ozon.ru")
    {
        return None;
    }
    url.set_scheme("https").ok()?;
    url.set_host(Some("www.ozon.ru")).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    Some(url.to_string())
}

fn sku_from_url(url: Option<&Value>) -> Option<String> {
    let clean = clean_url(url)?;
    let path = Url::parse(&clean).ok()?.path().to_owned();
    let product = Regex::new(r"(?iu)/product/(?:[^/]*-)?(\d+)/?$").unwrap();
    product
        .captures(&path)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_owned())
}

fn image_url(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(source) = value.as_str() {
        let source = source.trim();
        if source.is_empty() || source.len() > 2048 || source.chars().any(char::is_control) {
            return None;
        }
        let mut url = Url::parse(source).ok()?;
        url.set_query(None);
        url.set_fragment(None);
        if !crate::images::is_downloadable_image_url(url.as_str()) {
            return None;
        }
        return Some(url.into());
    }
    let record = value.as_object()?;
    image_url(record.get("src"))
        .or_else(|| image_url(record.get("link")))
        .or_else(|| image_url(record.get("url")))
        .or_else(|| image_url(record.get("photoUrl")))
}

fn has_star_icon(value: Option<&Value>) -> bool {
    let mut strings = Vec::new();
    if let Some(value) = value {
        collect_strings(value, &mut strings);
    }
    strings.iter().any(|item| item.contains("ic_s_star"))
}

fn parse_search_item(
    item: &Value,
    source_index: usize,
    source_locator: String,
) -> Option<SearchItem> {
    let item = item.as_object()?;
    let states: Vec<&Map<String, Value>> = item
        .get("mainState")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .collect();
    let price_block = states.iter().find_map(|state| {
        (state.get("type").and_then(Value::as_str) == Some("priceV2"))
            .then(|| state.get("priceV2")?.as_object())?
    });
    let prices: Vec<&Map<String, Value>> = price_block
        .and_then(|p| p.get("price"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .collect();
    let price = price_to_minor(
        prices
            .iter()
            .find(|p| p.get("textStyle").and_then(Value::as_str) == Some("PRICE"))
            .and_then(|p| p.get("text")),
    );
    let name = states
        .iter()
        .find(|s| s.get("id").and_then(Value::as_str) == Some("name"))
        .and_then(|s| text_from(s.get("textDS")));

    let price_type = match price_block
        .and_then(|p| p.get("priceStyle"))
        .and_then(|p| p.get("styleType"))
        .and_then(Value::as_str)
    {
        Some("CARD_PRICE") => PriceType::OzonCard,
        _ => PriceType::Unknown,
    };
    let price_label = (price_type == PriceType::OzonCard).then(|| "Цена с Ozon Картой".to_owned());
    let delivery_label = item
        .get("multiButton")
        .and_then(|v| v.pointer("/ozonButton/addToCart/actionButton/title"))
        .and_then(text)
        .filter(|label| label.chars().count() <= 120);
    let mut rating = None;
    let mut reviews = None;
    if let Some(items) = states.iter().find_map(|state| {
        let labels = state.get("labelListV2")?;
        has_star_icon(Some(labels)).then(|| labels.get("items")?.as_array())?
    }) {
        let labels: Vec<String> = items
            .iter()
            .filter_map(|entry| {
                let entry = entry.as_object()?;
                (entry.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| text_from(entry.get("text")))?
            })
            .collect();
        rating = labels
            .first()
            .and_then(|s| rating_from_value(Some(&Value::String(s.clone()))));
        reviews = labels.get(1).and_then(|s| {
            let value = Value::String(s.clone());
            parse_review_count(Some(&value)).or_else(|| count_from_value(Some(&value)))
        });
    }

    let url = clean_url(item.get("action").and_then(|value| value.get("link")));
    let url_value = url.as_ref().map(|value| Value::String(value.clone()));
    let sku = normalize_sku(item.get("sku"))
        .or_else(|| normalize_sku(item.get("id")))
        .or_else(|| sku_from_url(url_value.as_ref()))?;
    let tile_image = item.get("tileImage").and_then(Value::as_object);
    let image = tile_image
        .and_then(|tile| tile.get("items"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| image_url(entry.get("image")))
        .next()
        .or_else(|| tile_image.and_then(|tile| image_url(tile.get("coverImage"))));

    Some(SearchItem {
        source_index,
        source_locator,
        sku,
        name,
        price_minor: price,
        price_type,
        price_label,
        delivery_label,
        seller: None,
        rating,
        reviews,
        url,
        image,
        matches_price_range: None,
    })
}

#[cfg(test)]
pub fn parse_search_items(page: &Value) -> Vec<SearchItem> {
    parse_search_page(page).0
}
pub fn parse_search_page(page: &Value) -> (Vec<SearchItem>, SourceCoverage) {
    let widgets = WidgetSet::new(page);
    let mut source = SourceCoverage {
        malformed_rows: widgets.malformed_count("tileGridDesktop"),
        ..Default::default()
    };
    let mut out = Vec::new();
    for (key, grid) in widgets.all_named("tileGridDesktop") {
        let Some(items) = grid.get("items").and_then(Value::as_array) else {
            source.malformed_rows += 1;
            continue;
        };
        source.present = true;
        source.raw_rows += items.len();
        for (source_index, row) in items.iter().enumerate() {
            if let Some(item) = parse_search_item(
                row,
                source_index,
                format!("{}/items/{source_index}", widget_locator(&key)),
            ) {
                out.push(item);
            } else {
                source.malformed_rows += 1;
            }
        }
    }
    source.parsed_rows = out.len();
    source.local_truncated = out.len() > 120;
    out.truncate(120);
    (out, source)
}

fn rs_text(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let values: Vec<&Value> = value
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_else(|| vec![value]);
    values
        .into_iter()
        .filter_map(|item| text(item).or_else(|| text_from(Some(item))))
        .collect::<Vec<_>>()
        .join(" ")
}

fn first_rs_text(values: &[Option<&Value>]) -> Option<String> {
    values.iter().find_map(|value| {
        let parsed = rs_text(*value);
        (!parsed.is_empty()).then_some(parsed)
    })
}

fn parse_short_characteristics(page: &Value) -> Vec<Characteristic> {
    let mut out = Vec::new();
    let state = WidgetSet::new(page)
        .all_named("webShortCharacteristics")
        .find(|(_, value)| value.get("characteristics").is_some_and(Value::is_array));
    let source_prefix = state
        .as_ref()
        .map(|(key, _)| widget_locator(key))
        .unwrap_or_default();
    let state = state.map(|(_, value)| value);
    for (source_index, characteristic) in state
        .as_ref()
        .and_then(|s| s.get("characteristics"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, value)| value.as_object().map(|value| (index, value)))
    {
        let title_value = characteristic.get("title");
        let title = first_rs_text(&[
            title_value.and_then(|v| v.get("textRs")),
            title_value.and_then(|v| v.get("text")),
            title_value,
        ]);
        let value = first_rs_text(&[
            characteristic.get("values"),
            characteristic.get("contentRS"),
            characteristic.get("valueRs"),
        ]);
        if let (Some(title), Some(value)) = (title, value) {
            out.push(Characteristic {
                source_index,
                source_locator: format!("{source_prefix}/characteristics/{source_index}"),
                label: title,
                value,
            });
        }
    }
    out
}

fn parse_product_score(page: &Value) -> (Option<f64>, Option<u64>) {
    let score_shape = |value: &Value| {
        [
            "rating",
            "ratingValue",
            "text",
            "reviews",
            "reviewCount",
            "reviewsCount",
            "totalReviews",
        ]
        .iter()
        .any(|key| value.get(*key).is_some())
            || value.pointer("/title/text").is_some()
    };
    let Some(state) = widget_matching(page, "webSingleProductScore", score_shape)
        .or_else(|| widget_matching(page, "webReviewProductScore", score_shape))
    else {
        return (None, None);
    };
    let rating = [
        state.get("rating"),
        state.get("ratingValue"),
        state.get("text"),
        state.pointer("/title/text"),
    ]
    .into_iter()
    .find_map(rating_from_value);
    let reviews = [
        state.get("reviews"),
        state.get("reviewCount"),
        state.get("reviewsCount"),
        state.get("totalReviews"),
    ]
    .into_iter()
    .find_map(|v| count_from_value(v).or_else(|| parse_review_count(v)));
    (rating, reviews)
}

fn parse_seller(page: &Value) -> Option<Seller> {
    let state = widget_matching(page, "webCurrentSeller", |value| {
        value.pointer("/sellerCell/centerBlock/title").is_some() || value.get("title").is_some()
    })?;
    let name = text_from(state.pointer("/sellerCell/centerBlock/title"))
        .or_else(|| text_from(state.get("title")))?;
    let rating = rating_from_value(state.pointer("/rating/title/text"))
        .or_else(|| rating_from_value(state.pointer("/rating/title")))
        .or_else(|| rating_from_value(state.get("rating")));
    let url = clean_url(state.pointer("/sellerCell/common/action/link"));
    Some(Seller { name, rating, url })
}

fn parse_variants(page: &Value) -> ProductVariants {
    let state = widget_matching(page, "webAspects", |value| {
        value.get("aspects").is_some_and(Value::is_array)
    });
    let mut items = Vec::new();
    if let Some(aspects) = state
        .as_ref()
        .and_then(|value| value.get("aspects"))
        .and_then(Value::as_array)
    {
        for aspect in aspects {
            let aspect_name = aspect.get("aspectName").and_then(text);
            for variant in aspect
                .get("variants")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(sku) = normalize_sku(variant.get("sku")) else {
                    continue;
                };
                let url = clean_url(variant.get("link")).filter(|url| {
                    sku_from_url(Some(&Value::String(url.clone()))).as_deref() == Some(&sku)
                });
                let data = variant.get("data").and_then(Value::as_object);
                let value = ["title", "text", "name", "value"]
                    .into_iter()
                    .find_map(|key| {
                        data.and_then(|data| data.get(key))
                            .and_then(|value| text_from(Some(value)))
                    });
                let title = match (aspect_name.as_ref(), value) {
                    (Some(name), Some(value)) => Some(format!("{name}: {value}")),
                    (_, Some(value)) => Some(value),
                    _ => None,
                };
                items.push(ProductVariant { sku, title, url });
            }
        }
    }
    let truncated = items.len() > 20;
    items.truncate(20);
    ProductVariants {
        status: if state.is_none() {
            SourceSectionStatus::Unknown
        } else if truncated {
            SourceSectionStatus::Partial
        } else {
            SourceSectionStatus::Available
        },
        items,
        has_next: None,
        next_path: None,
    }
}

fn decode_html_entities(value: &str) -> String {
    let re = Regex::new(r"(?iu)&(#x[0-9a-f]+|#\d+|nbsp|amp|lt|gt|quot|apos);").unwrap();
    re.replace_all(value, |caps: &regex::Captures<'_>| {
        let code = &caps[1];
        match code.to_ascii_lowercase().as_str() {
            "nbsp" => " ".to_owned(),
            "amp" => "&".to_owned(),
            "lt" => "<".to_owned(),
            "gt" => ">".to_owned(),
            "quot" => "\"".to_owned(),
            "apos" => "'".to_owned(),
            _ => {
                let number = if code.to_ascii_lowercase().starts_with("#x") {
                    u32::from_str_radix(&code[2..], 16).ok()
                } else {
                    code[1..].parse().ok()
                };
                number
                    .and_then(char::from_u32)
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| caps[0].to_owned())
            }
        }
    })
    .into_owned()
}

fn description_text(html: &str) -> String {
    let script = Regex::new(r"(?is)<script\b[^>]*>.*?</script\s*>").unwrap();
    let style = Regex::new(r"(?is)<style\b[^>]*>.*?</style\s*>").unwrap();
    let comment = Regex::new(r"(?s)<!--.*?-->").unwrap();
    let tag = Regex::new(r"(?iu)</?[a-z][^>]*>").unwrap();
    let stripped = script.replace_all(html, " ");
    let stripped = style.replace_all(&stripped, " ");
    let stripped = comment.replace_all(&stripped, " ");
    let stripped = tag.replace_all(&stripped, " ");
    text(&Value::String(decode_html_entities(&stripped))).unwrap_or_default()
}

fn description_images(html: &str) -> Vec<String> {
    let re =
        Regex::new(r#"(?iu)<img\b[^>]*\ssrc\s*=\s*(?:\"([^\"]*)\"|'([^']*)'|([^\s\"'=<>`]+))"#)
            .unwrap();
    re.captures_iter(html)
        .filter_map(|caps| caps.get(1).or_else(|| caps.get(2)).or_else(|| caps.get(3)))
        .filter_map(|m| image_url(Some(&Value::String(m.as_str().to_owned()))))
        .collect()
}

fn walk_description(value: &Value, texts: &mut Vec<String>, images: &mut Vec<String>) {
    if let Some(items) = value.as_array() {
        for item in items {
            walk_description(item, texts, images);
        }
        return;
    }
    let Some(record) = value.as_object() else {
        return;
    };
    if record.get("type").and_then(Value::as_str) == Some("text")
        && let Some(content) = record.get("content").and_then(text)
    {
        texts.push(content);
    }
    for key in ["title", "text"] {
        if let Some(content) = record
            .get(key)
            .and_then(Value::as_object)
            .and_then(|value| value.get("content"))
            .and_then(Value::as_array)
        {
            texts.extend(
                content
                    .iter()
                    .filter_map(text)
                    .filter(|value| key != "title" || value.trim().to_lowercase() != "заголовок"),
            );
        }
    }
    let image = image_url(record.get("img").and_then(|value| value.get("src")))
        .or_else(|| image_url(record.get("image").and_then(|value| value.get("src"))))
        .or_else(|| {
            (record.get("type").and_then(Value::as_str) == Some("image"))
                .then(|| {
                    image_url(record.get("src")).or_else(|| {
                        image_url(record.get("attrs").and_then(|value| value.get("src")))
                    })
                })
                .flatten()
        });
    if let Some(image) = image {
        images.push(image);
    }
    for key in ["content", "blocks"] {
        if let Some(child) = record.get(key) {
            walk_description(child, texts, images);
        }
    }
}

pub fn parse_description(page: &Value) -> Description {
    static IMAGE_TAG: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?iu)<img\b[^>]*\ssrc\s*=").expect("image tag regex"));
    let mut texts = Vec::new();
    let mut images = Vec::new();
    let widgets = WidgetSet::new(page);
    let mut source = SourceCoverage {
        malformed_rows: widgets.malformed_count("webDescription"),
        ..Default::default()
    };
    let mut image_locators: Vec<String> = Vec::new();
    let mut image_indexes: Vec<usize> = Vec::new();
    for (key, state) in widgets.all_named("webDescription") {
        source.raw_rows += 1;
        let text_start = texts.len();
        let image_start = images.len();
        let annotation = parse_json_value(state.get("richAnnotationJson"));
        let annotation_valid = annotation.as_ref().is_some_and(|v| {
            v.is_array()
                || v.as_object().is_some_and(|r| {
                    ["content", "blocks", "title", "text", "img", "image", "type"]
                        .iter()
                        .any(|k| r.contains_key(*k))
                })
        });
        let html = state.get("richAnnotation").and_then(Value::as_str);
        source.present |= annotation_valid || html.is_some();
        source.parsed_rows += usize::from(annotation_valid || html.is_some());
        if !annotation_valid && html.is_none() {
            source.malformed_rows += 1;
        }
        if let Some(annotation) = annotation.filter(|_| annotation_valid) {
            let root = annotation.get("content").unwrap_or(&annotation);
            walk_description(root, &mut texts, &mut images);
            for index in image_start..images.len() {
                image_locators.push(format!("{}/richAnnotationJson", widget_locator(&key)));
                image_indexes.push(index - image_start);
            }
        }
        if let Some(html) = html {
            if texts.len() == text_start {
                let fallback = description_text(html);
                if !fallback.is_empty() {
                    texts.push(fallback);
                }
            }
            if images.len() == image_start {
                let extracted = description_images(html);
                let raw_images = IMAGE_TAG.find_iter(html).count();
                source.malformed_rows += raw_images.saturating_sub(extracted.len());
                images.extend(extracted);
                for index in image_start..images.len() {
                    image_locators.push(format!("{}/richAnnotation", widget_locator(&key)));
                    image_indexes.push(index - image_start);
                }
            }
        }
    }
    let mut seen = HashSet::new();
    let mut image_origins: Vec<SourceImage> = images
        .iter()
        .enumerate()
        .map(|(index, url)| SourceImage {
            source_locator: image_locators.get(index).cloned(),
            url: url.clone(),
            section: "description".into(),
            index: image_indexes.get(index).copied().unwrap_or(index),
        })
        .collect();
    image_origins.retain(|i| seen.insert(i.url.clone()));
    source.local_truncated = image_origins.len() > 12;
    Description {
        acquisition: page
            .get("acquisition")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        text: texts.join(" ").trim().to_owned(),
        source,
        image_origins: image_origins.into_iter().take(12).collect(),
    }
}

pub(crate) fn parse_duty(page: &Value) -> Option<(u64, String)> {
    let amount_re = Regex::new(r"(?iu)^\s*([0-9]+(?:[ \x{00a0}\x{202f}][0-9]{3})*(?:[.,][0-9]{1,2})?)\s*(?:₽|руб\.?|рублей)(?:\s+при получении)?\s*$").unwrap();
    for state in WidgetSet::new(page).all_valid("webIconWithText") {
        let Some(title) = text_from(state.get("title")) else {
            continue;
        };
        if !matches!(
            title.to_lowercase().as_str(),
            "таможенная пошлина" | "пошлина"
        ) {
            continue;
        }
        let Some(label) = text_from(state.get("text")) else {
            continue;
        };
        if let Some(amount) = amount_re
            .captures(&label)
            .and_then(|c| decimal_minor(c.get(1)?.as_str()))
        {
            return Some((amount, format!("{title} {label}")));
        }
    }
    None
}

fn seo_url(page: &Value) -> Option<String> {
    page.pointer("/seo/link")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|link| clean_url(link.get("href")))
}

/// Every observed identity in a fragment must agree; missing identity is never merge authority.
pub(crate) fn validate_product_identity(
    page: &Value,
    requested: &str,
    required: bool,
) -> anyhow::Result<bool> {
    let mut identities = Vec::new();
    for gallery in WidgetSet::new(page).all_valid("webGallery") {
        if let Some(value) = gallery.get("sku") {
            identities.push(normalize_sku(Some(value)));
        }
    }
    if let Some(tracking) = parse_json_value(page.get("layoutTrackingInfo"))
        && let Some(value) = tracking.get("sku")
    {
        identities.push(normalize_sku(Some(value)));
    }
    for link in page
        .pointer("/seo/link")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(value) = link.get("href") {
            identities.push(sku_from_url(Some(value)));
        }
    }
    if !identities
        .iter()
        .all(|sku| sku.as_deref() == Some(requested))
    {
        return Err(crate::error::fail(
            crate::error::Code::SourceChanged,
            "Conflicting product identity",
        ));
    }
    if required && identities.is_empty() {
        return Err(crate::error::fail(
            crate::error::Code::SourceChanged,
            "Product identity unavailable",
        ));
    }
    Ok(!identities.is_empty())
}

pub fn parse_details(base_page: &Value, page2: Option<&Value>) -> ProductDetails {
    let heading = widget_matching(base_page, "webProductHeading", |value| {
        value.get("title").is_some()
    });
    let price = widget_matching(base_page, "webPrice", |_| true);
    let gallery_named = WidgetSet::new(base_page)
        .all_named("webGallery")
        .find(|(_, value)| {
            value.get("coverImage").is_some() || value.get("images").is_some_and(Value::is_array)
        });
    let gallery_prefix = gallery_named
        .as_ref()
        .map(|(key, _)| widget_locator(key))
        .unwrap_or_default();
    let gallery = gallery_named.map(|(_, value)| value);
    let tracking = parse_json_value(base_page.get("layoutTrackingInfo"));
    let url = seo_url(base_page);
    let sku = WidgetSet::new(base_page)
        .all_valid("webGallery")
        .find_map(|g| normalize_sku(g.get("sku")))
        .or_else(|| normalize_sku(tracking.as_ref().and_then(|v| v.get("sku"))))
        .or_else(|| {
            url.as_ref()
                .and_then(|u| sku_from_url(Some(&Value::String(u.clone()))))
        });
    let (rating, reviews) = parse_product_score(base_page);
    let mut image_origins = Vec::new();
    let mut gallery_source = SourceCoverage {
        present: gallery.as_ref().is_some_and(|g| {
            g.get("images").is_some_and(Value::is_array) || g.get("coverImage").is_some()
        }),
        malformed_rows: WidgetSet::new(base_page).malformed_count("webGallery")
            + WidgetSet::new(base_page)
                .all_valid("webGallery")
                .filter(|value| {
                    value.get("coverImage").is_none()
                        && value.get("images").is_some_and(|v| !v.is_array())
                })
                .count(),
        ..Default::default()
    };
    if let Some(gallery) = gallery.as_ref() {
        if let Some(cover) = gallery.get("coverImage") {
            gallery_source.raw_rows += 1;
            if let Some(url) = image_url(Some(cover)) {
                image_origins.push(SourceImage {
                    source_locator: Some(format!("{gallery_prefix}/coverImage")),
                    url,
                    section: "gallery/coverImage".into(),
                    index: 0,
                });
            } else {
                gallery_source.malformed_rows += 1;
            }
        }
        if let Some(raw) = gallery.get("images").and_then(Value::as_array) {
            gallery_source.raw_rows += raw.len();
            for (index, image) in raw.iter().enumerate() {
                if let Some(url) = image_url(
                    image
                        .get("src")
                        .or_else(|| image.get("image"))
                        .or(Some(image)),
                ) {
                    image_origins.push(SourceImage {
                        source_locator: Some(format!("{gallery_prefix}/images/{index}")),
                        url,
                        section: "gallery/images".into(),
                        index,
                    });
                } else {
                    gallery_source.malformed_rows += 1;
                }
            }
        } else if gallery.get("images").is_some() {
            gallery_source.malformed_rows += 1;
        }
    }
    gallery_source.parsed_rows = image_origins.len();
    let mut seen = HashSet::new();
    image_origins.retain(|i| seen.insert(i.url.clone()));
    gallery_source.local_truncated = image_origins.len() > 12;
    image_origins.truncate(12);
    let explicit_card_price = price_to_minor(price.as_ref().and_then(|v| v.get("cardPrice")));
    let card_price =
        explicit_card_price.or_else(|| price_to_minor(price.as_ref().and_then(|v| v.get("price"))));
    let duty_source = if parse_duty(base_page).is_some() {
        Some(base_page)
    } else {
        page2
    };
    let duty = duty_source.and_then(|page| {
        parse_duty(page).map(|(amount, note)| Duty {
            acquisition: page
                .get("acquisition")
                .and_then(|v| serde_json::from_value(v.clone()).ok()),
            amount_minor: amount,
            note,
        })
    });
    let name = text_from(heading.as_ref().and_then(|v| v.get("title")))
        .or_else(|| base_page.pointer("/seo/title").and_then(text));
    let final_url = url.or_else(|| {
        sku.as_ref()
            .map(|sku| format!("https://www.ozon.ru/product/{sku}/"))
    });
    let mut description = parse_description(base_page);
    if let Some(page2) = page2 {
        let mut secondary = parse_description(page2);
        for image in &mut secondary.image_origins {
            image.section = "description_supplement".into();
        }
        description.source.present |= secondary.source.present;
        description.source.malformed_rows += secondary.source.malformed_rows;
        description.source.local_truncated |= secondary.source.local_truncated;
        if !description.has_text() {
            description.acquisition = secondary.acquisition;
            description.text = secondary.text;
        }
        for image in secondary.image_origins {
            if !description
                .image_origins
                .iter()
                .any(|origin| origin.url == image.url)
            {
                description.image_origins.push(image);
            }
        }
        description.source.local_truncated |= description.image_origins.len() > 12;
        description.image_origins.truncate(12);
    }
    image_origins.extend(description.image_origins.iter().cloned());
    ProductDetails {
        acquisition: base_page
            .get("acquisition")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        sku,
        name,
        url: final_url,
        displayed_price_minor: card_price,
        card_price_minor: explicit_card_price,
        regular_price_minor: price_to_minor(price.as_ref().and_then(|v| v.get("price"))),
        duty,
        available: price
            .as_ref()
            .and_then(|v| v.get("isAvailable"))
            .and_then(Value::as_bool),
        rating,
        reviews,
        seller: parse_seller(base_page),
        delivery_label: price
            .as_ref()
            .and_then(|value| value.get("deliveryLabel"))
            .and_then(text),
        image_origins,
        gallery_source,
        supplement: None,
        characteristics: parse_short_characteristics(base_page),
        characteristics_complete: false,
        description,
        variants: parse_variants(base_page),
        warnings: Vec::new(),
    }
}

fn unix_to_date(value: Option<&Value>) -> Option<String> {
    let seconds = match value? {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) if Regex::new(r"^\d+(?:\.\d+)?$").unwrap().is_match(s.trim()) => {
            s.trim().parse().ok()?
        }
        _ => return None,
    };
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    DateTime::<Utc>::from_timestamp_millis((seconds * 1000.0) as i64)
        .map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

fn author_name(author: Option<&Value>) -> Option<String> {
    let author = author?.as_object()?;
    text_from(author.get("title"))
        .or_else(|| text(author.get("fio")?))
        .or_else(|| {
            let full = [
                author.get("firstName").and_then(text),
                author.get("lastName").and_then(text),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
            (!full.is_empty()).then_some(full)
        })
}

pub fn parse_reviews(page: &Value, limit: usize) -> ReviewPage {
    let state_named = WidgetSet::new(page)
        .all_named("webListReviews")
        .find(|(_, value)| {
            value.get("reviews").is_some_and(Value::is_array)
                || value.get("items").is_some_and(Value::is_array)
        });
    let source_prefix = state_named
        .as_ref()
        .map(|(key, value)| {
            format!(
                "{}/{}",
                widget_locator(key),
                if value.get("reviews").is_some_and(Value::is_array) {
                    "reviews"
                } else {
                    "items"
                }
            )
        })
        .unwrap_or_default();
    let state = state_named.map(|(_, value)| value);
    let raw = state.as_ref().and_then(|v| {
        v.get("reviews")
            .and_then(Value::as_array)
            .or_else(|| v.get("items").and_then(Value::as_array))
    });
    let (mut rating, total) = parse_product_score(page);
    if rating.is_none() {
        rating = state
            .as_ref()
            .and_then(|value| rating_from_value(value.get("productScore")));
    }
    let mut source = SourceCoverage {
        present: raw.is_some(),
        raw_rows: raw.map_or(0, Vec::len),
        malformed_rows: WidgetSet::new(page).malformed_count("webListReviews")
            + WidgetSet::new(page)
                .all_valid("webListReviews")
                .filter(|v| {
                    !v.get("reviews").is_some_and(Value::is_array)
                        && !v.get("items").is_some_and(Value::is_array)
                })
                .count()
            + raw.map_or(0, |items| items.iter().filter(|r| !r.is_object()).count()),
        local_truncated: raw.is_some_and(|r| r.iter().filter(|v| v.is_object()).count() > limit),
        ..Default::default()
    };
    for review in raw.into_iter().flatten() {
        if let Some(photos) = review.pointer("/content/photos").and_then(Value::as_array) {
            source.local_truncated |= photos.len() > 12;
            source.malformed_rows += photos
                .iter()
                .filter(|p| image_url(Some(p)).is_none())
                .count();
        } else if review.pointer("/content/photos").is_some() {
            source.malformed_rows += 1;
        }
    }
    let reviews: Vec<Review> = raw
        .into_iter()
        .flatten()
        .enumerate()
        .filter(|(_, v)| v.is_object())
        .take(limit)
        .map(|(source_index, review)| {
            let content = review.get("content").filter(|v| v.is_object());
            let author = author_name(review.get("author")).or_else(|| {
                (review.get("isAnonymous").and_then(Value::as_bool) == Some(true))
                    .then(|| "Аноним".to_owned())
            });
            let optional_string = |key| {
                content
                    .and_then(|c| c.get(key))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            };
            let source_locator = format!("{source_prefix}/{source_index}");
            let photo_origins: Vec<SourceImage> = content
                .and_then(|c| c.get("photos"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
                .filter_map(|(index, photo)| {
                    image_url(Some(photo)).map(|url| SourceImage {
                        url,
                        section: format!("reviews/{source_index}/photos"),
                        index,
                        source_locator: Some(format!("{source_locator}/content/photos/{index}")),
                    })
                })
                .take(12)
                .collect();
            let review_id = review
                .get("reviewId")
                .or_else(|| review.get("id"))
                .or_else(|| review.get("uuid"))
                .and_then(|value| match value {
                    Value::String(value) => Some(value.trim().to_owned()),
                    Value::Number(value) if value.is_u64() => Some(value.to_string()),
                    _ => None,
                })
                .filter(|value| {
                    !value.is_empty()
                        && value.len() <= 128
                        && value
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                });
            let item_id = normalize_sku(review.get("itemId"));
            let variant_label = item_id.as_ref().and_then(|item_id| {
                state
                    .as_ref()
                    .and_then(|value| value.get("products"))
                    .and_then(Value::as_object)
                    .and_then(|products| {
                        products.get(item_id).or_else(|| {
                            products.values().find(|product| {
                                normalize_sku(product.get("itemId")).as_ref() == Some(item_id)
                            })
                        })
                    })
                    .and_then(|product| product.get("variants"))
                    .and_then(Value::as_array)
                    .map(|variants| {
                        variants
                            .iter()
                            .filter_map(|variant| {
                                let name =
                                    variant.get("name").and_then(|value| text_from(Some(value)));
                                let value = variant
                                    .get("value")
                                    .and_then(|value| text_from(Some(value)));
                                match (name, value) {
                                    (Some(name), Some(value)) => Some(format!("{name}: {value}")),
                                    (_, value) => value,
                                }
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .filter(|label| !label.is_empty())
            });
            Review {
                source_index,
                source_locator,
                photo_origins,
                review_id,
                author,
                score: rating_from_value(content.and_then(|c| c.get("score"))),
                comment: optional_string("comment"),
                pros: optional_string("positive"),
                cons: optional_string("negative"),
                date: unix_to_date(
                    review
                        .get("publishedAt")
                        .or_else(|| review.get("createdAt")),
                ),
                // Captured runtime evidence showed false on purchased reviews;
                // only an affirmative upstream value is treated as evidence.
                purchased: (review.get("isItemPurchased").and_then(Value::as_bool) == Some(true))
                    .then_some(true),
                variant_label: variant_label.or_else(|| {
                    review
                        .pointer("/productVariant/title")
                        .or_else(|| review.pointer("/variant/title"))
                        .or_else(|| review.get("variantLabel"))
                        .and_then(|value| text_from(Some(value)))
                }),
            }
        })
        .collect();
    let requested_path = state
        .as_ref()
        .and_then(|value| {
            value
                .get("fullRequestUrl")
                .or_else(|| value.get("requestedPath"))
        })
        .and_then(Value::as_str)
        .and_then(safe_review_path);
    let next_button = state
        .as_ref()
        .and_then(|value| value.pointer("/paging/nextButton"))
        .and_then(Value::as_str);
    let next_path = next_button
        .filter(|value| !value.is_empty())
        .and_then(|params| review_path_with_params(requested_path.as_deref()?, params))
        .filter(|next| same_review_selection(requested_path.as_deref().unwrap_or_default(), next));
    source.parsed_rows = reviews.len();
    let has_next = if next_path.is_some() {
        Some(true)
    } else {
        (next_button == Some("") && source.complete()).then_some(false)
    };
    let refinements = parse_review_refinements(state.as_ref(), requested_path.as_deref());
    let aggregation_scope = match state
        .as_ref()
        .and_then(|value| value.get("productsCount"))
        .and_then(Value::as_u64)
    {
        Some(count) if count > 1 => ReviewAggregationScope::MultipleVariants,
        Some(1) => ReviewAggregationScope::SpecificSku,
        _ => ReviewAggregationScope::Unknown,
    };
    ReviewPage {
        acquisition: page
            .get("acquisition")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        source,
        source_url: requested_path
            .as_ref()
            .map(|path| format!("{OZON_ORIGIN}{path}")),
        observed_at: None,
        rating,
        total_reviews: total,
        reviews,
        next_path,
        has_next,
        refinements,
        aggregation_scope,
        warnings: Vec::new(),
    }
}

fn same_review_selection(base: &str, next: &str) -> bool {
    let selected = |value: &str| -> Option<BTreeMap<String, String>> {
        let url = Url::parse(OZON_ORIGIN).ok()?.join(value).ok()?;
        Some(
            url.query_pairs()
                .filter(|(k, _)| !matches!(k.as_ref(), "page" | "page_key"))
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
        )
    };
    selected(base).is_some_and(|selection| Some(selection) == selected(next))
}

fn review_path_with_params(base: &str, params: &str) -> Option<String> {
    let origin = Url::parse(OZON_ORIGIN).ok()?;
    let mut target = origin.join(base).ok()?;
    let additions = origin
        .join(&format!("/?{}", params.trim_start_matches('?')))
        .ok()?;
    let mut pairs: BTreeMap<String, String> = target
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let mut seen = HashSet::new();
    for (key, value) in additions.query_pairs() {
        if !seen.insert(key.to_string()) {
            return None;
        }
        pairs.insert(key.into_owned(), value.into_owned());
    }
    target.set_query(None);
    for (key, value) in pairs {
        target.query_pairs_mut().append_pair(&key, &value);
    }
    let path = target.as_str().strip_prefix(OZON_ORIGIN)?.to_owned();
    safe_review_path(&path)
}

fn parse_review_refinements(
    state: Option<&Value>,
    requested_path: Option<&str>,
) -> Vec<ReviewRefinement> {
    let Some(base) = requested_path else {
        return Vec::new();
    };
    state
        .and_then(|value| value.get("sortings"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|sorting| {
            let label = sorting
                .get("name")
                .and_then(text)?
                .chars()
                .take(500)
                .collect();
            let value = sorting.get("value").and_then(Value::as_str)?;
            let mut fresh = Url::parse(OZON_ORIGIN).ok()?.join(base).ok()?;
            let keep: Vec<_> = fresh
                .query_pairs()
                .filter(|(k, _)| !matches!(k.as_ref(), "page" | "page_key"))
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            fresh.set_query(None);
            for (k, v) in keep {
                fresh.query_pairs_mut().append_pair(&k, &v);
            }
            let url = review_path_with_params(fresh.as_str(), &format!("sort={value}"))?;
            Some(ReviewRefinement {
                label,
                url,
                selected: sorting.get("active").and_then(Value::as_bool),
                kind: Some("sort".to_owned()),
            })
        })
        .take(100)
        .collect()
}

pub(crate) fn safe_review_path(value: &str) -> Option<String> {
    let raw_path = value.split('?').next()?;
    if raw_path.contains('%')
        || raw_path.contains('\\')
        || raw_path.split('/').any(|p| p == "." || p == "..")
    {
        return None;
    }
    if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        return None;
    }
    let base = Url::parse(OZON_ORIGIN).ok()?;
    let url = base.join(value).ok()?;
    if url.scheme() != "https"
        || !matches!(url.host_str(), Some("ozon.ru" | "www.ozon.ru"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
        || !Regex::new(r"^/product/(?:[^/]*-)?[0-9]+/reviews/$")
            .ok()?
            .is_match(url.path())
    {
        return None;
    }
    let mut result = url.path().to_owned();
    let mut seen = HashSet::new();
    let pairs: Vec<_> = url.query_pairs().collect();
    for (key, value) in &pairs {
        if !seen.insert(key.to_string()) || !safe_review_parameter(key, value) {
            return None;
        }
    }
    if !pairs.is_empty() {
        result.push('?');
        result.push_str(url.query()?);
    }
    Some(result)
}

fn safe_review_parameter(key: &str, value: &str) -> bool {
    match key {
        "page" => value
            .parse::<u64>()
            .is_ok_and(|page| (1..=10_000).contains(&page)),
        "page_key" => {
            !value.is_empty()
                && value.len() <= 512
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        }
        "sort" => matches!(value, "usefulness_desc" | "score_desc" | "score_asc"),
        "reviewsVariantMode" => value.parse::<u8>().is_ok_and(|mode| mode <= 10),
        "rating" => value
            .parse::<u8>()
            .is_ok_and(|rating| (1..=5).contains(&rating)),
        _ => false,
    }
}

#[cfg(test)]
#[path = "tests/decode.rs"]
mod tests;
