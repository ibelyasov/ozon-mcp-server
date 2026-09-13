use super::{
    parse_description as typed_description, parse_details as typed_details,
    parse_reviews as typed_reviews, parse_search_items as typed_search_items,
};
use serde_json::{Value, json};

fn parse_description(page: &Value) -> Value {
    serde_json::to_value(typed_description(page)).unwrap()
}

fn parse_details(base: &Value, secondary: Option<&Value>) -> Value {
    serde_json::to_value(typed_details(base, secondary)).unwrap()
}

fn parse_reviews(page: &Value, limit: usize) -> Value {
    serde_json::to_value(typed_reviews(page, limit)).unwrap()
}

fn parse_search_items(page: &Value) -> Vec<Value> {
    typed_search_items(page)
        .into_iter()
        .map(|item| serde_json::to_value(item).unwrap())
        .collect()
}

fn parse_search(page: &Value, limit: usize) -> Value {
    let items: Vec<Value> = parse_search_items(page).into_iter().take(limit).collect();
    json!({"count": items.len(), "items": items})
}

fn fixture(name: &str) -> Value {
    let source = match name {
        "details-page" => include_str!("../../tests/fixtures/domain/details-page.json"),
        "details-response" => include_str!("../../tests/fixtures/domain/details-response.json"),
        "reviews-page" => include_str!("../../tests/fixtures/domain/reviews-page.json"),
        "reviews-response" => include_str!("../../tests/fixtures/domain/reviews-response.json"),
        _ => panic!("unknown fixture"),
    };
    serde_json::from_str(source).unwrap()
}

#[test]
fn public_details_and_reviews_match_golden_contracts() {
    assert_eq!(
        parse_details(&fixture("details-page"), None),
        fixture("details-response")
    );
    assert_eq!(
        parse_reviews(&fixture("reviews-page"), 10),
        fixture("reviews-response")
    );
}

#[test]
fn search_skips_malformed_widgets_and_parses_fractional_counts() {
    let page = json!({
        "widgetStates": {
            "tileGridDesktop-bad": "{bad",
            "tileGridDesktop-good": json!({
                "items": [{
                    "sku": "901",
                    "action": { "link": "/product/example-901/?from=search#reviews" },
                    "mainState": [
                        { "type": "priceV2", "priceV2": { "price": [
                            { "textStyle": "PRICE", "text": "1 234,50 ₽" },
                            { "textStyle": "ORIGINAL_PRICE", "text": "1 299,99 ₽" }
                        ]}},
                        { "id": "name", "textDS": { "text": " Test   product " }},
                        { "labelListV2": { "items": [
                            { "type": "icon", "icon": "ic_s_star" },
                            { "type": "text", "text": { "text": "4,8" }},
                            { "type": "text", "text": { "text": "1,2 тыс. отзывов" }}
                        ]}}
                    ]
                }]
            }).to_string()
        }
    });

    assert_eq!(
        parse_search(&page, 12),
        json!({
            "count": 1,
            "items": [{
                "sku": "901", "name": "Test product", "price": 1234.5,
                "oldPrice": 1299.99, "discount": null, "rating": 4.8,
                "reviews": 1200, "brand": null,
                "currency": "RUB", "priceType": "unknown", "priceLabel": null,
                "deliveryLabel": null, "seller": null,
                "url": "https://www.ozon.ru/product/example-901/", "image": null
            }]
        })
    );
}

#[test]
fn details_preserves_false_zero_and_null_fields() {
    let page = json!({
        "seo": { "link": [{ "href": "/product/example-901/?campaign=1" }] },
        "layoutTrackingInfo": "{bad",
        "widgetStates": {
            "webProductHeading-0": { "title": " Example product " },
            "webPrice-0": { "cardPrice": "10,50 ₽", "isAvailable": false },
            "webSingleProductScore-0": { "reviewsCount": "0" },
            "webAspects-0": {"aspects": [{
                "aspectName": "Цвет",
                "variants": [
                    {"sku": "902", "link": "/product/blue-902/", "data": {"value": "Синий"}},
                    {"sku": "903", "link": "https://example.test/product/903/", "data": {"value": "Красный"}}
                ]
            }]},
            "webCurrentSeller-0": {
                "rating": { "title": { "text": "0" }},
                "sellerCell": { "centerBlock": { "title": { "text": "Seller" }}}
            }
        }
    });

    let details = parse_details(&page, None);
    assert_eq!(details["sku"], "901");
    assert_eq!(details["price"], 10.5);
    assert_eq!(details["cardPrice"], 10.5);
    assert_eq!(details["available"], false);
    assert_eq!(details["rating"], json!(null));
    assert_eq!(details["reviews"], 0);
    assert_eq!(details["variants"]["status"], "available");
    assert_eq!(details["variants"]["items"][0]["sku"], "902");
    assert_eq!(details["variants"]["items"][0]["title"], "Цвет: Синий");
    assert_eq!(details["variants"]["items"][1]["url"], json!(null));
    assert_eq!(details["seller"]["rating"].as_f64(), Some(0.0));
    assert_eq!(details["description"], json!({ "text": "", "images": [] }));
}

#[test]
fn reviews_preserves_variant_specific_unknowns() {
    let page = json!({
        "widgetStates": {
            "webSingleProductScore-0": { "reviewsCount": "5" },
            "webListReviews-0": { "reviews": [
                { "author": { "firstName": "Ada", "lastName": "Lovelace" },
                  "content": { "score": 5, "comment": "Works", "positive": "", "negative": "", "photos": [] },
                  "publishedAt": 1704067200, "usefulness": { "useful": 0 }, "isItemPurchased": false },
                { "isAnonymous": true, "content": {}, "createdAt": "bad", "isItemPurchased": "false" }
            ]}
        }
    });

    let parsed = parse_reviews(&page, 10);
    assert_eq!(parsed["rating"], json!(null));
    assert_eq!(parsed["reviews"][0]["pros"], "");
    assert_eq!(parsed["reviews"][0]["useful"], 0);
    assert_eq!(parsed["reviews"][0]["purchased"], json!(null));
    assert_eq!(parsed["reviews"][1]["author"], "Аноним");
    assert_eq!(parsed["reviews"][1]["purchased"], json!(null));
    assert_eq!(parsed["reviews"][1]["hasPhotos"], json!(null));
}

#[test]
fn reviews_exposes_only_safe_observed_identity_photos_and_continuation() {
    let page = json!({"widgetStates": {"webListReviews-a": {
        "requestedPath": "/product/example-901/reviews/",
        "fullRequestUrl": "/product/example-901/reviews/?reviewsVariantMode=2&sort=usefulness_desc",
        "productsCount": 2,
        "paging": {"page": 1, "total": 3,
          "nextButton": "?page=2&page_key=TOKEN_1&sort=usefulness_desc", "links": [
            {"text": "1", "urlParams": "page=1"},
            {"text": "2", "urlParams": "page=2"}
        ]},
        "sortings": [{"active": true, "name": "Сначала полезные", "value": "usefulness_desc"}],
        "products": {"901": {"itemId": "901", "variants": [{"name": "Цвет", "value": "Blue"}]}},
        "reviews": [{
            "reviewId": "review_1",
            "itemId": "901",
            "isItemPurchased": true,
            "content": {"photos": [
                {"url": "https://ir.ozone.ru/picture.jpg?tracking=discard#fragment"},
                "http://127.0.0.1/private.jpg"
            ]}
        }]
    }}});
    let parsed = parse_reviews(&page, 10);
    assert_eq!(
        parsed["nextPath"],
        "/product/example-901/reviews/?page=2&page_key=TOKEN_1&reviewsVariantMode=2&sort=usefulness_desc"
    );
    assert_eq!(parsed["hasNext"], true);
    assert_eq!(parsed["aggregationScope"], "multiple_variants");
    assert_eq!(parsed["refinements"][0]["kind"], "sort");
    assert_eq!(
        parsed["refinements"][0]["url"],
        "/product/example-901/reviews/?reviewsVariantMode=2&sort=usefulness_desc"
    );
    assert_eq!(parsed["reviews"][0]["reviewId"], "review_1");
    assert_eq!(parsed["reviews"][0]["variantLabel"], "Цвет: Blue");
    assert_eq!(parsed["reviews"][0]["purchased"], true);
    assert_eq!(
        parsed["reviews"][0]["photos"],
        json!(["https://ir.ozone.ru/picture.jpg"])
    );
}

#[test]
fn reviews_skip_wrong_shape_but_preserve_explicit_empty_list() {
    let page = json!({"widgetStates": {
        "webListReviews-a": {},
        "webListReviews-b": {"reviews": [{"isAnonymous": true, "content": {}}]}
    }});
    assert_eq!(parse_reviews(&page, 10)["count"], 1);

    let empty = json!({"widgetStates": {
        "webListReviews-a": {"reviews": []},
        "webListReviews-b": {"reviews": [{"isAnonymous": true, "content": {}}]}
    }});
    assert_eq!(parse_reviews(&empty, 10)["count"], 0);
}

#[test]
fn description_falls_back_from_malformed_json_and_deduplicates_images() {
    let page = json!({ "widgetStates": {
        "webDescription-0": {
            "richAnnotationJson": "{bad",
            "richAnnotation": "<p>Fresh &amp; <b>clean</b></p><img src=\"/same.jpg\"><img src=\"/same.jpg\">"
        }
    }});
    assert_eq!(
        parse_description(&page),
        json!({
            "text": "Fresh & clean", "images": ["https://www.ozon.ru/same.jpg"]
        })
    );
}

#[test]
fn search_preserves_missing_prices_and_all_grid_source_order() {
    let page = json!({"widgetStates": {
        "tileGridDesktop-z": {"items": [{"sku": "2"}, {"sku": "1", "mainState": [
            {"type": "priceV2", "priceV2": {"price": [
                {"textStyle": "PRICE", "text": "unavailable"},
                {"textStyle": "ORIGINAL_PRICE", "text": "500 ₽"}
            ]}}
        ]}]},
        "tileGridDesktop-a": {"items": [{"sku": "2"}, {"sku": "invalid"}]}
    }});
    let items = parse_search_items(&page);
    assert_eq!(
        items
            .iter()
            .map(|v| v["sku"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["2", "1", "2"]
    );
    for item in &items {
        assert!(item["price"].is_null());
        assert!(item["oldPrice"].is_null());
        assert!(item["deliveryLabel"].is_null());
        assert!(item["seller"].is_null());
    }
    assert_eq!(parse_search(&page, 2)["count"], 2);
}

#[test]
fn search_price_delivery_and_seller_context_is_narrow() {
    let mut item = json!({"sku": "123", "mainState": [
        {"type": "priceV2", "priceV2": {
            "price": [{"textStyle": "PRICE", "text": "100 ₽"}],
            "priceStyle": {"styleType": "CARD_PRICE"}
        }},
        {"labelListV2": {"testInfo": {"automatizationId": "tile-list-rating"}, "items": [
            {"type": "icon", "icon": {"icon": {"icon": "ic_s_ozon_circle_filled_compact"}}},
            {"type": "text", "text": {"text": "Ozon"}}
        ]}}
    ], "multiButton": {"ozonButton": {"addToCart": {"actionButton": {
        "title": "  Завтра  ", "common": {"action": {"id": "not-output"}}
    }}}}});
    let parse = |item| {
        parse_search_items(&json!({"widgetStates": {"tileGridDesktop-0": {"items": [item]}}}))
            .remove(0)
    };
    let parsed = parse(item.clone());
    assert_eq!(parsed["price"], 100.0);
    assert_eq!(parsed["currency"], "RUB");
    assert_eq!(parsed["priceType"], "ozon_card");
    assert_eq!(parsed["priceLabel"], "Цена с Ozon Картой");
    assert_eq!(parsed["deliveryLabel"], "Завтра");
    // The Ozon badge does not establish the merchant identity.
    assert!(parsed["seller"].is_null());
    assert!(!parsed.to_string().contains("not-output"));
    item["mainState"][0]["priceV2"]["priceStyle"]["styleType"] = json!("SALE_PRICE");
    item["mainState"][1]["labelListV2"]["items"][0]["icon"]["icon"]["icon"] = json!("unrecognized");
    item["multiButton"]["ozonButton"]["addToCart"]["actionButton"]["title"] =
        json!("x".repeat(121));
    let parsed = parse(item.clone());
    assert_eq!(parsed["priceType"], "unknown");
    assert!(parsed["priceLabel"].is_null());
    assert!(parsed["deliveryLabel"].is_null());
    assert!(parsed["seller"].is_null());
    item["multiButton"]["ozonButton"]["addToCart"]["actionButton"]["title"] =
        json!({"text": "Завтра"});
    assert!(parse(item)["deliveryLabel"].is_null());
}
