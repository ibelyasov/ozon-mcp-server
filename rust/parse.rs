use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::{Map, Number, Value, json};
use std::collections::HashSet;
use url::Url;

const OZON_ORIGIN: &str = "https://www.ozon.ru";

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

fn widget_name(key: &str) -> &str {
    key.split('-').next().unwrap_or("")
}

fn widgets(page: &Value, name: &str) -> Vec<Value> {
    page.get("widgetStates")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|states| states.iter())
        .filter(|(key, _)| widget_name(key) == name)
        .filter_map(|(_, value)| parse_json_value(Some(value)))
        .filter(Value::is_object)
        .collect()
}

fn widget(page: &Value, name: &str) -> Option<Value> {
    widgets(page, name).into_iter().next()
}

fn number_value(number: f64) -> Value {
    Number::from_f64(number)
        .map(Value::Number)
        .unwrap_or(Value::Null)
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

fn price_to_number(value: Option<&Value>) -> Option<f64> {
    let parsed = match value? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let matcher = Regex::new(r"[+-]?\d(?:[\d\s\u{00a0}\u{202f}.,]*\d)?").unwrap();
            matcher
                .find(s)
                .and_then(|m| parse_localized_number(m.as_str()))
        }
        _ => None,
    }?;
    (parsed.is_finite() && parsed >= 0.0).then_some(parsed)
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
    let terminal = Regex::new(r"-(\d+)/?$").unwrap();
    product
        .captures(&path)
        .or_else(|| terminal.captures(&path))
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_owned())
}

fn image_url(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(source) = value.as_str() {
        let source = source.trim();
        return (!source.is_empty()).then(|| source.to_owned());
    }
    let record = value.as_object()?;
    image_url(record.get("src"))
        .or_else(|| image_url(record.get("link")))
        .or_else(|| image_url(record.get("url")))
}

fn has_star_icon(value: Option<&Value>) -> bool {
    let mut strings = Vec::new();
    if let Some(value) = value {
        collect_strings(value, &mut strings);
    }
    strings.iter().any(|item| item.contains("ic_s_star"))
}

fn parse_search_item(item: &Value) -> Option<Value> {
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
    let price = price_to_number(
        prices
            .iter()
            .find(|p| p.get("textStyle").and_then(Value::as_str) == Some("PRICE"))
            .and_then(|p| p.get("text")),
    )?;
    let old_price = price_to_number(
        prices
            .iter()
            .find(|p| p.get("textStyle").and_then(Value::as_str) == Some("ORIGINAL_PRICE"))
            .and_then(|p| p.get("text")),
    )
    .filter(|old| *old > price);
    let name = states
        .iter()
        .find(|s| s.get("id").and_then(Value::as_str) == Some("name"))
        .and_then(|s| text_from(s.get("textDS")));

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

    let mut brand = None;
    let verified = Regex::new(r"(?iu)^бренд проверен$").unwrap();
    for state in &states {
        let Some(labels) = state.get("labelListV2").and_then(Value::as_object) else {
            continue;
        };
        if labels
            .get("testInfo")
            .and_then(|value| value.get("automatizationId"))
            .and_then(Value::as_str)
            != Some("tile-list-labels")
        {
            continue;
        }
        let labels: Vec<String> = labels
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let entry = entry.as_object()?;
                (entry.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| text_from(entry.get("text")))?
            })
            .collect();
        if let Some(index) = labels.iter().position(|label| verified.is_match(label))
            && index > 0
        {
            brand = Some(labels[index - 1].clone());
            break;
        }
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

    Some(json!({
        "sku": sku, "name": name, "price": number_value(price),
        "oldPrice": old_price.map(number_value),
        "discount": price_block.and_then(|p| p.get("discount")).and_then(|v| match v { Value::String(s) if !s.trim().is_empty() => Some(Value::String(s.trim().to_owned())), Value::Number(_) => Some(v.clone()), _ => None }),
        "rating": rating.map(number_value), "reviews": reviews, "brand": brand,
        "url": url, "image": image
    }))
}

pub fn parse_search(page: &Value, limit: usize) -> Value {
    let items: Vec<Value> = widget(page, "tileGridDesktop")
        .and_then(|grid| grid.get("items").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(parse_search_item)
        .take(limit)
        .collect();
    json!({ "count": items.len(), "items": items })
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

fn parse_short_characteristics(page: &Value) -> Value {
    let mut out = Map::new();
    let state = widget(page, "webShortCharacteristics");
    for characteristic in state
        .as_ref()
        .and_then(|s| s.get("characteristics"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
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
            out.insert(title, Value::String(value));
        }
    }
    Value::Object(out)
}

fn parse_product_score(page: &Value) -> (Option<f64>, Option<u64>) {
    let Some(state) =
        widget(page, "webSingleProductScore").or_else(|| widget(page, "webReviewProductScore"))
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
    let mut reviews = [
        state.get("reviews"),
        state.get("reviewCount"),
        state.get("reviewsCount"),
        state.get("totalReviews"),
    ]
    .into_iter()
    .find_map(|v| count_from_value(v).or_else(|| parse_review_count(v)));
    if reviews.is_none() {
        let mut strings = Vec::new();
        collect_strings(&state, &mut strings);
        reviews = strings
            .iter()
            .find_map(|s| parse_review_count(Some(&Value::String(s.clone()))));
    }
    (rating, reviews)
}

fn parse_seller(page: &Value) -> Value {
    let Some(state) = widget(page, "webCurrentSeller") else {
        return Value::Null;
    };
    let Some(name) = text_from(state.pointer("/sellerCell/centerBlock/title"))
        .or_else(|| text_from(state.get("title")))
    else {
        return Value::Null;
    };
    let rating = rating_from_value(state.pointer("/rating/title/text"))
        .or_else(|| rating_from_value(state.pointer("/rating/title")))
        .or_else(|| rating_from_value(state.get("rating")));
    let url = clean_url(state.pointer("/sellerCell/common/action/link"));
    json!({ "name": name, "rating": rating.map(number_value), "url": url })
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
    for child in record.values().filter(|v| v.is_object() || v.is_array()) {
        walk_description(child, texts, images);
    }
}

pub fn parse_description(page: &Value) -> Value {
    let mut texts = Vec::new();
    let mut images = Vec::new();
    for state in widgets(page, "webDescription") {
        let text_start = texts.len();
        let image_start = images.len();
        if let Some(annotation) = parse_json_value(state.get("richAnnotationJson"))
            && (annotation.is_object() || annotation.is_array())
        {
            let root = annotation.get("content").unwrap_or(&annotation);
            walk_description(root, &mut texts, &mut images);
        }
        if let Some(html) = state.get("richAnnotation").and_then(Value::as_str) {
            if texts.len() == text_start {
                let fallback = description_text(html);
                if !fallback.is_empty() {
                    texts.push(fallback);
                }
            }
            if images.len() == image_start {
                images.extend(description_images(html));
            }
        }
    }
    let mut seen = HashSet::new();
    images.retain(|image| seen.insert(image.clone()));
    json!({ "text": texts.join(" ").trim(), "images": images })
}

fn parse_duty(page: &Value) -> Option<(f64, &'static str)> {
    let amount_re =
        Regex::new(r"(?iu)([+-]?\d(?:[\d\s\u{00a0}\u{202f}.,]*\d)?)\s*(?:₽|руб(?:\.|л[а-яё]*)?)")
            .unwrap();
    let marker = Regex::new(r"(?iu)customs-duty|пошлин").unwrap();
    for state in widgets(page, "webIconWithText") {
        let mut strings = Vec::new();
        collect_strings(&state, &mut strings);
        if !strings.iter().any(|s| marker.is_match(s)) {
            continue;
        }
        let joined = strings.join(" ");
        for source in strings
            .iter()
            .filter(|s| marker.is_match(s))
            .map(String::as_str)
            .chain(std::iter::once(joined.as_str()))
        {
            if let Some(amount) = amount_re
                .captures(source)
                .and_then(|c| c.get(1))
                .and_then(|m| parse_localized_number(m.as_str()))
                .filter(|amount| *amount >= 0.0)
            {
                return Some((amount, "пошлина не входит в цену"));
            }
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

pub fn parse_details(base_page: &Value, page2: Option<&Value>) -> Value {
    let heading = widget(base_page, "webProductHeading");
    let price = widget(base_page, "webPrice");
    let gallery = widget(base_page, "webGallery");
    let tracking = parse_json_value(base_page.get("layoutTrackingInfo"));
    let url = seo_url(base_page);
    let sku = normalize_sku(gallery.as_ref().and_then(|v| v.get("sku")))
        .or_else(|| normalize_sku(tracking.as_ref().and_then(|v| v.get("sku"))))
        .or_else(|| {
            url.as_ref()
                .and_then(|u| sku_from_url(Some(&Value::String(u.clone()))))
        });
    let (rating, reviews) = parse_product_score(base_page);
    let mut images = Vec::new();
    if let Some(image) = image_url(gallery.as_ref().and_then(|v| v.get("coverImage"))) {
        images.push(image);
    }
    for image in gallery
        .as_ref()
        .and_then(|v| v.get("images"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(source) = image_url(
            image
                .get("src")
                .or_else(|| image.get("image"))
                .or(Some(image)),
        ) {
            images.push(source);
        }
    }
    let mut seen = HashSet::new();
    images.retain(|image| seen.insert(image.clone()));
    images.truncate(10);
    let card_price = price_to_number(price.as_ref().and_then(|v| v.get("cardPrice")))
        .or_else(|| price_to_number(price.as_ref().and_then(|v| v.get("price"))));
    let duty = parse_duty(base_page).map(|(amount, note)| json!({ "amount": number_value(amount), "total": card_price.map(|p| number_value(p + amount)), "note": note }));
    let name = text_from(heading.as_ref().and_then(|v| v.get("title")))
        .or_else(|| base_page.pointer("/seo/title").and_then(text));
    let final_url = url.or_else(|| {
        sku.as_ref()
            .map(|sku| format!("https://www.ozon.ru/product/{sku}/"))
    });
    json!({
        "sku": sku, "name": name, "url": final_url, "price": card_price.map(number_value),
        "priceRegular": price_to_number(price.as_ref().and_then(|v| v.get("price"))).map(number_value),
        "oldPrice": price_to_number(price.as_ref().and_then(|v| v.get("originalPrice"))).map(number_value),
        "duty": duty, "available": price.as_ref().and_then(|v| v.get("isAvailable")).and_then(Value::as_bool),
        "rating": rating.map(number_value), "reviews": reviews, "seller": parse_seller(base_page), "images": images,
        "characteristics": parse_short_characteristics(base_page), "description": parse_description(page2.unwrap_or(&Value::Null))
    })
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
        .map(|date| date.format("%Y-%m-%d").to_string())
}

fn author_name(author: Option<&Value>) -> Option<String> {
    let author = author?.as_object()?;
    text_from(author.get("title")).or_else(|| {
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

pub fn parse_reviews(page: &Value, limit: usize) -> Value {
    let state = widget(page, "webListReviews");
    let raw = state.as_ref().and_then(|v| {
        v.get("reviews")
            .and_then(Value::as_array)
            .or_else(|| v.get("items").and_then(Value::as_array))
    });
    let (rating, total) = parse_product_score(page);
    let reviews: Vec<Value> = raw.into_iter().flatten().filter(|v| v.is_object()).take(limit).map(|review| {
        let content = review.get("content").filter(|v| v.is_object());
        let author = author_name(review.get("author")).or_else(|| (review.get("isAnonymous").and_then(Value::as_bool) == Some(true)).then(|| "Аноним".to_owned()));
        let optional_string = |key| content.and_then(|c| c.get(key)).and_then(Value::as_str).map(str::to_owned);
        json!({
            "author": author, "score": rating_from_value(content.and_then(|c| c.get("score"))).map(number_value),
            "comment": optional_string("comment"), "pros": optional_string("positive"), "cons": optional_string("negative"),
            "date": unix_to_date(review.get("publishedAt").or_else(|| review.get("createdAt"))),
            "useful": count_from_value(review.pointer("/usefulness/useful")),
            "purchased": review.get("isItemPurchased").and_then(Value::as_bool),
            "hasPhotos": content.and_then(|c| c.get("photos")).and_then(Value::as_array).map(|photos| !photos.is_empty())
        })
    }).collect();
    json!({ "rating": rating.map(number_value), "totalReviews": total, "count": reviews.len(), "reviews": reviews })
}

#[cfg(test)]
#[path = "tests/parse.rs"]
mod tests;
