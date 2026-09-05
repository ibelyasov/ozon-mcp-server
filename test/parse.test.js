import assert from "node:assert/strict";
import test from "node:test";

import {
  _internal,
  parseDescription,
  parseDetails,
  parseDuty,
  parseReviews,
  parseSearch,
} from "../src/parse.js";

function descriptionPage(...descriptions) {
  return {
    widgetStates: Object.fromEntries(
      descriptions.map((description, index) => [
        `webDescription-${index}`,
        JSON.stringify(description),
      ])
    ),
  };
}

function searchPage(mainState) {
  return {
    widgetStates: {
      "tileGridDesktop-0": JSON.stringify({
        items: [
          {
            sku: "123456",
            action: { link: "/product/example-123456/" },
            mainState: [
              {
                type: "priceV2",
                priceV2: {
                  price: [{ textStyle: "PRICE", text: "1 234 ₽" }],
                },
              },
              ...mainState,
            ],
          },
        ],
      }),
    },
  };
}

function label(texts, automatizationId = "tile-list-labels") {
  return {
    labelListV2: {
      testInfo: { automatizationId },
      items: texts.map((text) => ({ type: "text", text: { text } })),
    },
  };
}

test("uses the brand next to verified evidence when rewards precede it", () => {
  const result = parseSearch(
    searchPage([label(["Ozon Карта", "1 000 бонусов", "Acme", "Бренд проверен"])])
  );

  assert.equal(result.items[0].brand, "Acme");
});

test("does not infer a brand from label text without explicit verified evidence", () => {
  const result = parseSearch(searchPage([label(["Ozon Карта", "1 000 бонусов", "Acme"]) ]));

  assert.equal(result.items[0].brand, null);
});

test("parses richAnnotation HTML and plain annotation text", () => {
  const result = parseDescription(
    descriptionPage(
      { richAnnotation: "<p>Fresh &amp; <b>clean</b></p><img src=\"/one.jpg\">" },
      { richAnnotation: "Plain annotation" }
    )
  );

  assert.deepEqual(result, {
    text: "Fresh & clean Plain annotation",
    images: ["/one.jpg"],
  });
});

test("walks nested richAnnotationJson once and preserves literal angle brackets", () => {
  const result = parseDescription(
    descriptionPage({
      richAnnotationJson: JSON.stringify({
        content: {
          blocks: [
            { type: "text", content: "2 < 3 > 1" },
            {
              children: [
                { type: "text", content: "Nested copy only once" },
                { img: { src: "/nested.jpg" } },
              ],
            },
          ],
        },
      }),
    })
  );

  assert.deepEqual(result, {
    text: "2 < 3 > 1 Nested copy only once",
    images: ["/nested.jpg"],
  });
});

test("falls back to richAnnotation when richAnnotationJson is empty or malformed", () => {
  const result = parseDescription(
    descriptionPage(
      { richAnnotationJson: "", richAnnotation: "<p>Empty JSON fallback</p>" },
      { richAnnotationJson: "{bad", richAnnotation: "<p>Malformed JSON fallback</p>" }
    )
  );

  assert.equal(result.text, "Empty JSON fallback Malformed JSON fallback");
});

test("deduplicates description images and handles a missing description page", () => {
  const result = parseDescription(
    descriptionPage({
      richAnnotationJson: JSON.stringify({
        content: [
          { img: { src: "/same.jpg" } },
          { img: { src: "/same.jpg" } },
          { img: { src: "/other.jpg" } },
        ],
      }),
    })
  );

  assert.deepEqual(result.images, ["/same.jpg", "/other.jpg"]);
  assert.deepEqual(parseDescription(null), { text: "", images: [] });
});

test("parses fractional prices, compact review counts, and skips a malformed grid state", () => {
  const page = {
    widgetStates: {
      "tileGridDesktop-bad": "{not json",
      "tileGridDesktop-good": JSON.stringify({
        items: [
          null,
          {
            sku: "901",
            action: { link: "/product/example-901/?from=search#reviews" },
            tileImage: { coverImage: " https://cdn.example/product.jpg " },
            mainState: [
              null,
              {
                type: "priceV2",
                priceV2: {
                  discount: "-5%",
                  price: [
                    { textStyle: "PRICE", text: "1 234,50 ₽" },
                    { textStyle: "ORIGINAL_PRICE", text: "1 299,99 ₽" },
                  ],
                },
              },
              { id: "name", textDS: { text: "  Test   product " } },
              {
                labelListV2: {
                  items: [
                    { type: "icon", icon: "ic_s_star" },
                    { type: "text", text: { text: "4,8" } },
                    { type: "text", text: { text: "1,2 тыс. отзывов" } },
                  ],
                },
              },
            ],
          },
        ],
      }),
    },
  };

  assert.deepEqual(parseSearch(page), {
    count: 1,
    items: [
      {
        sku: "901",
        name: "Test product",
        price: 1234.5,
        oldPrice: 1299.99,
        discount: "-5%",
        rating: 4.8,
        reviews: 1200,
        brand: null,
        url: "https://www.ozon.ru/product/example-901/",
        image: "https://cdn.example/product.jpg",
      },
    ],
  });
  assert.equal(_internal.priceToNumber("1,234.50 ₽"), 1234.5);
  assert.equal(_internal.priceToNumber(Number.POSITIVE_INFINITY), null);
  assert.equal(_internal.priceToNumber("not a price"), null);
});

test("accepts only Ozon product URLs and does not invent a SKU from arbitrary digits", () => {
  assert.equal(
    _internal.cleanUrl("http://ozon.ru/product/example-123456/?campaign=1#reviews"),
    "https://www.ozon.ru/product/example-123456/"
  );
  assert.equal(_internal.cleanUrl("https://evil.example/product-123456/"), null);
  assert.equal(_internal.cleanUrl("javascript:alert(1)"), null);
  assert.equal(_internal.skuFromUrl("/product/example-123456/?campaign=1"), "123456");
  assert.equal(_internal.skuFromUrl("https://evil.example/product-123456/"), null);
  assert.equal(_internal.skuFromUrl("/search/?text=123456"), null);
});

test("keeps valid false and zero values in product details and tolerates malformed tracking JSON", () => {
  const basePage = {
    layoutTrackingInfo: "{malformed",
    seo: { link: [{ href: "/product/example-901/?campaign=1" }] },
    widgetStates: {
      "webProductHeading-0": JSON.stringify({ title: "  Example product " }),
      "webPrice-0": JSON.stringify({
        cardPrice: "1 234,50 ₽",
        price: "1 299,99 ₽",
        originalPrice: "1 499,99 ₽",
        isAvailable: false,
      }),
      "webGallery-0": JSON.stringify({
        images: [{ src: "https://cdn.example/one.jpg" }, { image: "https://cdn.example/two.jpg" }],
        coverImage: "https://cdn.example/cover.jpg",
      }),
      "webSingleProductScore-0": JSON.stringify({ text: "4,9 · 1,2 тыс. отзывов" }),
      "webCurrentSeller-0": JSON.stringify({
        rating: { title: { text: "0" } },
        sellerCell: {
          centerBlock: { title: { text: " Seller " } },
          common: { action: { link: "/seller/99/?from=card" } },
        },
      }),
      "webShortCharacteristics-0": JSON.stringify({
        characteristics: [
          null,
          {
            title: { textRs: [{ text: " Material " }] },
            values: [{ content: " Steel " }],
          },
          { title: "__proto__", values: [{ text: "safe" }] },
        ],
      }),
      "webIconWithText-0": JSON.stringify({
        action: { link: "/modal/customs-duty?product_id=901" },
        text: "Пошлина 5 ₽",
      }),
    },
  };

  const details = parseDetails(basePage, null);
  assert.equal(details.sku, "901");
  assert.equal(details.name, "Example product");
  assert.equal(details.url, "https://www.ozon.ru/product/example-901/");
  assert.equal(details.price, 1234.5);
  assert.equal(details.priceRegular, 1299.99);
  assert.equal(details.oldPrice, 1499.99);
  assert.deepEqual(details.duty, {
    amount: 5,
    total: 1239.5,
    note: "пошлина не входит в цену",
  });
  assert.equal(details.available, false);
  assert.equal(details.rating, 4.9);
  assert.equal(details.reviews, 1200);
  assert.deepEqual(details.seller, {
    name: "Seller",
    rating: 0,
    url: "https://www.ozon.ru/seller/99/",
  });
  assert.deepEqual(details.images, [
    "https://cdn.example/cover.jpg",
    "https://cdn.example/one.jpg",
    "https://cdn.example/two.jpg",
  ]);
  assert.equal(details.characteristics.Material, "Steel");
  assert.equal(details.characteristics["__proto__"], "safe");
  assert.deepEqual(details.description, { text: "", images: [] });
  assert.deepEqual(parseDuty({ widgetStates: { "webIconWithText-0": "{bad" } }), null);
});

test("does not infer a rating from a review count and keeps absent review fields unknown", () => {
  const page = {
    widgetStates: {
      "webSingleProductScore-0": { reviewsCount: "5" },
      "webListReviews-0": {
        reviews: [
          null,
          {
            author: { firstName: "Ada", lastName: "Lovelace" },
            content: {
              score: 5,
              comment: "Works",
              positive: "",
              negative: "",
              photos: [],
            },
            publishedAt: 1704067200,
            usefulness: { useful: 0 },
            isItemPurchased: false,
          },
          {
            isAnonymous: true,
            content: { score: Number.NaN },
            createdAt: "not-a-timestamp",
            usefulness: { useful: Number.POSITIVE_INFINITY },
            isItemPurchased: "false",
          },
        ],
      },
    },
  };

  assert.deepEqual(parseReviews(page), {
    rating: null,
    totalReviews: 5,
    count: 2,
    reviews: [
      {
        author: "Ada Lovelace",
        score: 5,
        comment: "Works",
        pros: "",
        cons: "",
        date: "2024-01-01",
        useful: 0,
        purchased: false,
        hasPhotos: false,
      },
      {
        author: "Аноним",
        score: null,
        comment: null,
        pros: null,
        cons: null,
        date: null,
        useful: null,
        purchased: null,
        hasPhotos: null,
      },
    ],
  });
  assert.equal(parseReviews(page, -1).count, 0);
});

test("preserves literal angle brackets in plain descriptions and does not recurse through cycles", () => {
  const cyclic = { type: "text", content: "Only once" };
  cyclic.self = cyclic;
  const result = parseDescription({
    widgetStates: {
      "webDescription-0": {
        richAnnotationJson: { content: cyclic },
        richAnnotation: "<p>Ignored duplicate</p><img src=/fallback.jpg>",
      },
      "webDescription-1": JSON.stringify({
        richAnnotation: "2 < 3 > 1<script>ignore</script><img src=/plain.jpg>",
      }),
    },
  });

  assert.deepEqual(result, {
    text: "Only once 2 < 3 > 1",
    images: ["/fallback.jpg", "/plain.jpg"],
  });
});
