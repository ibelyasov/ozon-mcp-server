use super::{parse_description, parse_details, parse_reviews, parse_search};
use serde_json::json;

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
            "webCurrentSeller-0": {
                "rating": { "title": { "text": "0" }},
                "sellerCell": { "centerBlock": { "title": { "text": "Seller" }}}
            }
        }
    });

    let details = parse_details(&page, None);
    assert_eq!(details["sku"], "901");
    assert_eq!(details["price"], 10.5);
    assert_eq!(details["available"], false);
    assert_eq!(details["rating"], json!(null));
    assert_eq!(details["reviews"], 0);
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
    assert_eq!(parsed["reviews"][0]["purchased"], false);
    assert_eq!(parsed["reviews"][1]["author"], "Аноним");
    assert_eq!(parsed["reviews"][1]["purchased"], json!(null));
    assert_eq!(parsed["reviews"][1]["hasPhotos"], json!(null));
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
            "text": "Fresh & clean", "images": ["/same.jpg"]
        })
    );
}
