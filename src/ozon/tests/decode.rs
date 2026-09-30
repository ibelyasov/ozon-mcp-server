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
        "details-page" => include_str!("../../../tests/fixtures/domain/details-page.json"),
        "details-response" => include_str!("../../../tests/fixtures/domain/details-response.json"),
        "reviews-page" => include_str!("../../../tests/fixtures/domain/reviews-page.json"),
        "reviews-response" => include_str!("../../../tests/fixtures/domain/reviews-response.json"),
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
                "sourceIndex":0,"sourceLocator":"/widgetStates/tileGridDesktop-good/items/0",
                "sku": "901", "name": "Test product", "priceMinor": 123450,
                "rating": 4.8, "reviews": 1200, "matchesPriceRange": null,
                "priceType": "unknown", "priceLabel": null,
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
    assert_eq!(details["displayedPriceMinor"], 1050);
    assert_eq!(details["cardPriceMinor"], 1050);
    assert_eq!(details["available"], false);
    assert_eq!(details["rating"], json!(null));
    assert_eq!(details["reviews"], 0);
    assert_eq!(details["variants"]["status"], "available");
    assert_eq!(details["variants"]["items"][0]["sku"], "902");
    assert_eq!(details["variants"]["items"][0]["title"], "Цвет: Синий");
    assert_eq!(details["variants"]["items"][1]["url"], json!(null));
    assert_eq!(details["seller"]["rating"].as_f64(), Some(0.0));
    assert_eq!(
        details["description"],
        json!({ "acquisition": null, "text": "", "source": {"present":false,"rawRows":0,"parsedRows":0,"malformedRows":0,"sourceTruncated":false,"localTruncated":false}, "imageOrigins":[] })
    );
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
    assert_eq!(parsed["reviews"][0]["purchased"], json!(null));
    assert_eq!(parsed["reviews"][1]["author"], "Аноним");
    assert_eq!(parsed["reviews"][1]["purchased"], json!(null));
}

#[test]
fn reviews_uses_the_review_widget_product_score_for_the_aggregate() {
    let page = json!({
        "widgetStates": {
            "webListReviews-0": {
                "productScore": 4.8,
                "reviews": [{"content": {"score": 5}}]
            }
        }
    });

    let parsed = parse_reviews(&page, 10);
    assert_eq!(parsed["rating"], 4.8);
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
        parsed["reviews"][0]["photoOrigins"][0]["url"],
        "https://ir.ozone.ru/picture.jpg"
    );
}

#[test]
fn reviews_skip_wrong_shape_but_preserve_explicit_empty_list() {
    let page = json!({"widgetStates": {
        "webListReviews-a": {},
        "webListReviews-b": {"reviews": [{"isAnonymous": true, "content": {}}]}
    }});
    assert_eq!(
        parse_reviews(&page, 10)["reviews"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let empty = json!({"widgetStates": {
        "webListReviews-a": {"reviews": []},
        "webListReviews-b": {"reviews": [{"isAnonymous": true, "content": {}}]}
    }});
    assert!(
        parse_reviews(&empty, 10)["reviews"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn description_falls_back_from_malformed_json_and_deduplicates_images() {
    let page = json!({ "widgetStates": {
        "webDescription-0": {
            "richAnnotationJson": "{bad",
            "richAnnotation": "<p>Fresh &amp; <b>clean</b></p><img src=\"https://ir.ozone.ru/same.jpg\"><img src=\"https://ir.ozone.ru/same.jpg\">"
        }
    }});
    let parsed = typed_description(&page);
    assert_eq!(parsed.text, "Fresh & clean");
    assert_eq!(
        parsed
            .image_origins
            .iter()
            .map(|i| i.url.as_str())
            .collect::<Vec<_>>(),
        vec!["https://ir.ozone.ru/same.jpg"]
    );
    assert_eq!(parsed.image_origins[0].index, 0);
}

#[test]
fn description_extracts_observed_rich_title_and_text_content_arrays() {
    let page = json!({"widgetStates": {"webDescription-0": {
        "richAnnotationJson": {"content": [{"blocks": [{
            "title": {"content": ["Комфорт, скорость, яркость"]},
            "text": {"content": ["Яркая беспроводная мышка.", "", "Бесшумные клавиши."]}
        }]}]}
    }}});

    assert_eq!(
        parse_description(&page)["text"],
        "Комфорт, скорость, яркость Яркая беспроводная мышка. Бесшумные клавиши."
    );
}

#[test]
fn description_rejects_standalone_editor_placeholder_title() {
    let page = json!({"widgetStates": {"webDescription-0": {
        "richAnnotationJson": {"content": [{"blocks": [{
            "title": {"content": ["Заголовок"]}
        }]}]}
    }}});

    assert_eq!(parse_description(&page)["text"], "");
}

#[test]
fn short_characteristics_never_claim_complete_source_coverage() {
    let page = json!({"widgetStates": {"webShortCharacteristics-0": {
        "characteristics": [{
            "title": {"text": "Тип памяти"},
            "values": [{"text": "DDR4"}]
        }]
    }}});

    let parsed = parse_details(&page, None);
    assert_eq!(parsed["characteristics"][0]["value"], "DDR4");
    assert_eq!(parsed["characteristicsComplete"], false);
}

#[test]
fn customs_duty_can_be_observed_in_a_secondary_product_fragment() {
    let base = json!({"widgetStates": {
        "webPrice-0": {"cardPrice": "50 000 ₽"}
    }});
    let secondary = json!({"widgetStates": {
        "webIconWithText-customs-duty": {
            "title": "Таможенная пошлина",
            "text": "1 737 ₽ при получении",
            "trackingInfo": {"unrelatedPrice": "9 999 ₽", "private": "discard"}
        }
    }});

    assert_eq!(
        parse_details(&base, Some(&secondary))["duty"],
        json!({
            "acquisition": null,
            "amountMinor": 173700,
            "note": "Таможенная пошлина 1 737 ₽ при получении"
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
        assert!(item["priceMinor"].is_null());
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
    assert_eq!(parsed["priceMinor"], 10000);
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

#[test]
fn characteristics_keep_duplicate_labels_and_source_order() {
    let page = json!({"widgetStates":{"webShortCharacteristics-a":{"characteristics":[
      {"title":"Интерфейс","values":["USB"]},{"title":"Интерфейс","values":["Bluetooth"]}]}}});
    let parsed = typed_details(&page, None);
    assert_eq!(
        parsed
            .characteristics
            .iter()
            .map(|c| (&*c.label, &*c.value))
            .collect::<Vec<_>>(),
        vec![("Интерфейс", "USB"), ("Интерфейс", "Bluetooth")]
    );
}
#[test]
fn prices_and_duty_require_exact_unambiguous_rub_amounts() {
    for bad in [
        "$12",
        "2 × 1 500 ₽",
        "от 12 ₽",
        "12 ₽ или 15 ₽",
        "1.234 ₽",
        "-5 ₽",
    ] {
        let parsed = typed_details(&json!({"widgetStates":{"webPrice-a":{"price":bad}}}), None);
        assert_eq!(parsed.displayed_price_minor, None, "{bad}");
    }
    for (source, expected) in [("1 234,50 ₽", 123450), ("0 ₽", 0), ("12.01 руб.", 1201)] {
        let parsed = typed_details(
            &json!({"widgetStates":{"webPrice-a":{"price":source}}}),
            None,
        );
        assert_eq!(parsed.displayed_price_minor, Some(expected));
        assert_eq!(parsed.card_price_minor, None);
    }
    assert!(super::parse_duty(&json!({"widgetStates":{"webIconWithText-a":{"title":"Таможенная пошлина","text":"Цена 10 000 ₽; пошлина 500 ₽"}}})).is_none());
}
#[test]
fn missing_malformed_and_truncated_sources_do_not_claim_exhaustion() {
    let unknown = typed_reviews(&json!({"widgetStates":{}}), 30);
    assert!(!unknown.source.present);
    assert_eq!(unknown.has_next, None);
    let bad = typed_reviews(
        &json!({"widgetStates":{"webListReviews-a":{"reviews":[null,{"content":{}}],"paging":{"nextButton":""}}}}),
        30,
    );
    assert_eq!(bad.source.malformed_rows, 1);
    assert_eq!(bad.has_next, None);
    let truncated = typed_reviews(
        &json!({"widgetStates":{"webListReviews-a":{"reviews":[{"content":{}},{"content":{}}],"paging":{"nextButton":""}}}}),
        1,
    );
    assert!(truncated.source.local_truncated);
    assert_eq!(truncated.has_next, None);
    let empty = typed_reviews(
        &json!({"widgetStates":{"webListReviews-a":{"reviews":[],"paging":{"nextButton":""}}}}),
        30,
    );
    assert!(empty.source.complete());
    assert_eq!(empty.has_next, Some(false));
    assert!(
        !typed_details(&json!({"widgetStates":{}}), None)
            .gallery_source
            .present
    );
}
#[test]
fn gallery_and_description_images_retain_source_indexes_before_dedup() {
    let page = json!({"widgetStates":{"webGallery-a":{"images":["https://ir.ozone.ru/a.jpg","https://ir.ozone.ru/a.jpg","https://ir.ozone.ru/b.jpg"]},"webDescription-a":{"richAnnotation":"<img src='https://ir.ozone.ru/c.jpg'>"}}});
    let details = typed_details(&page, None);
    assert_eq!(
        details
            .image_origins
            .iter()
            .map(|i| (&*i.section, i.index))
            .collect::<Vec<_>>(),
        vec![
            ("gallery/images", 0),
            ("gallery/images", 2),
            ("description", 0)
        ]
    );
    assert!(
        typed_details(
            &json!({"widgetStates":{"webGallery-a":{"images":["https://www.ozon.ru/a.jpg"]}}}),
            None
        )
        .gallery_source
        .malformed_rows
            > 0
    );
}
#[test]
fn review_refinement_discards_page_authority() {
    let result = typed_reviews(
        &json!({"widgetStates":{"webListReviews-a":{"reviews":[],"requestedPath":"/product/901/reviews/?page=3&page_key=OLD&rating=1&sort=usefulness_desc","sortings":[{"name":"Negative","value":"score_asc"}]}}}),
        30,
    );
    assert_eq!(
        result.refinements[0].url,
        "/product/901/reviews/?rating=1&sort=score_asc"
    );
}
#[test]
fn browser_projection_and_rust_decoder_share_one_fixture() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = r#"const fs=require('fs'),vm=require('vm'); const raw=JSON.parse(fs.readFileSync(process.argv[2],'utf8')); const source=fs.readFileSync(process.argv[1],'utf8'); const ctx={location:{origin:'https://www.ozon.ru'},document:{querySelectorAll:()=>[]},fetch:async()=>new Response(JSON.stringify(raw.page)),Response,AbortController,TextEncoder,TextDecoder,URL,encodeURIComponent,setTimeout,clearTimeout}; vm.runInNewContext('('+source+')({mode:"fetch",path:"/product/901/reviews/"})',ctx).then(v=>process.stdout.write(JSON.stringify(v)));"#;
    let output = std::process::Command::new("node")
        .arg("-e")
        .arg(script)
        .arg(root.join("browser/extract.js"))
        .arg(root.join("tests/fixtures/domain/source-projection.json"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let outcome: crate::ozon::outcome::PageOutcome =
        serde_json::from_slice(&output.stdout).unwrap();
    let crate::ozon::outcome::PageOutcome::Page(page) = outcome else {
        panic!("page projection failed")
    };
    let json = page.page.into_value();
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/domain/source-projection.json"
    ))
    .unwrap();
    assert_eq!(json, fixture["projected"]);
    assert!(!json.to_string().contains("PRIVATE_MARKER"));
    let reviews = typed_reviews(&json, 30);
    assert_eq!(
        serde_json::to_value(&reviews.reviews).unwrap(),
        fixture["reviews"]
    );
    assert_eq!(
        reviews
            .reviews
            .iter()
            .map(|r| r.review_id.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("uuid_1"), Some("review_2"), Some("id_3")]
    );
    assert_eq!(reviews.reviews[0].author.as_deref(), Some("Ada"));
    assert_eq!(reviews.reviews[1].author.as_deref(), Some("Public Name"));
    assert_eq!(reviews.reviews[2].variant_label.as_deref(), Some("Синий"));
    assert_eq!(
        reviews.reviews[2].photo_origins[0].url,
        "https://ir.ozone.ru/c.jpg"
    );
    assert_eq!(reviews.source.malformed_rows, 1);
    assert_eq!(reviews.has_next, None);
    let details = typed_details(&json, None);
    assert_eq!(
        json!({"imageOrigins": details.image_origins,
            "source": details.gallery_source, "displayedPriceMinor": details.displayed_price_minor}),
        fixture["gallery"]
    );
}

#[test]
fn source_rows_and_photos_keep_original_positions_after_malformed_values() {
    let search = json!({"widgetStates":{"tileGridDesktop-a":{"items":[null,{"sku":"901"}]}}});
    let item = typed_search_items(&search).remove(0);
    assert_eq!(item.source_index, 1);
    assert_eq!(
        item.source_locator,
        "/widgetStates/tileGridDesktop-a/items/1"
    );
    let page = json!({"widgetStates":{"webShortCharacteristics-a":{"characteristics":[null,{"title":"Type","values":["USB"]}]},"webListReviews-a":{"reviews":[null,{"content":{"photos":[null,"https://ir.ozone.ru/a.jpg"]}}]}}});
    let details = typed_details(&page, None);
    assert_eq!(details.characteristics[0].source_index, 1);
    let reviews = typed_reviews(&page, 30);
    assert_eq!(reviews.reviews[0].source_index, 1);
    assert_eq!(reviews.reviews[0].photo_origins[0].index, 1);
    assert_eq!(
        reviews.reviews[0].photo_origins[0]
            .source_locator
            .as_deref(),
        Some("/widgetStates/webListReviews-a/reviews/1/content/photos/1")
    );
}

#[test]
fn gallery_identity_and_malformed_instances_do_not_hide_valid_images() {
    let page = json!({"widgetStates":{
        "webGallery-a":{"sku":"901"},
        "webGallery-b":{"images":"malformed"},
        "webGallery-c":{"images":[null,"https://ir.ozone.ru/a.jpg"]}
    }});
    let details = typed_details(&page, None);
    assert_eq!(details.sku.as_deref(), Some("901"));
    assert_eq!(details.image_origins[0].url, "https://ir.ozone.ru/a.jpg");
    assert!(details.gallery_source.present);
    assert_eq!(details.gallery_source.malformed_rows, 2);
    assert!(!details.gallery_source.complete());
    assert_eq!(details.image_origins[0].index, 1);
    assert_eq!(
        details.image_origins[0].source_locator.as_deref(),
        Some("/widgetStates/webGallery-c/images/1")
    );
}

#[test]
fn captured_product_and_review_pages_roundtrip_empty_warnings_strictly() {
    use crate::ozon::model::{ProductDetails, ReviewPage};
    let product = typed_details(&fixture("details-page"), None);
    assert!(product.warnings.is_empty());
    let mut captured_product = serde_json::to_value(&product).unwrap();
    assert_eq!(captured_product["warnings"], json!([]));
    assert_eq!(
        serde_json::from_value::<ProductDetails>(captured_product.clone()).unwrap(),
        product
    );
    captured_product.as_object_mut().unwrap().remove("warnings");
    assert!(serde_json::from_value::<ProductDetails>(captured_product).is_err());

    let reviews = typed_reviews(&fixture("reviews-page"), 30);
    assert!(reviews.warnings.is_empty());
    let mut captured_reviews = serde_json::to_value(&reviews).unwrap();
    assert_eq!(captured_reviews["warnings"], json!([]));
    assert_eq!(
        serde_json::from_value::<ReviewPage>(captured_reviews.clone()).unwrap(),
        reviews
    );
    captured_reviews.as_object_mut().unwrap().remove("warnings");
    assert!(serde_json::from_value::<ReviewPage>(captured_reviews).is_err());
}

#[test]
fn valid_empty_current_price_widget_stays_unknown_after_original_price_projection_is_removed() {
    let page = json!({"widgetStates":{
        "webPrice-a":{},"webPrice-b":{"price":"999 ₽"}
    }});
    let product = typed_details(&page, None);
    assert_eq!(product.displayed_price_minor, None);
    assert_eq!(product.card_price_minor, None);
    assert_eq!(product.regular_price_minor, None);
}

#[test]
fn review_vote_changes_do_not_change_captured_identity_payload() {
    let mut page = json!({"widgetStates":{"webListReviews-a":{"reviews":[{
        "author":{"firstName":"Ada"},"content":{"comment":"Works"},"usefulness":{"useful":0}
    }]}}});
    let before = typed_reviews(&page, 30);
    page["widgetStates"]["webListReviews-a"]["reviews"][0]["usefulness"]["useful"] = json!(999);
    let after = typed_reviews(&page, 30);
    assert_eq!(before.reviews, after.reviews);
    assert_eq!(after.reviews[0].author.as_deref(), Some("Ada"));
    assert_eq!(after.reviews[0].source_index, 0);
}
