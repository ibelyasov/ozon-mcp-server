import assert from "node:assert/strict";
import test from "node:test";

import { parseDescription, parseSearch } from "../src/parse.js";

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
