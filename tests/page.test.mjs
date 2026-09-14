import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import { webcrypto } from "node:crypto";

const source = readFileSync(new URL("../rust/page.js", import.meta.url), "utf8");
const outcomeFixtures = JSON.parse(
  readFileSync(new URL("fixtures/page-outcomes.json", import.meta.url), "utf8"),
);
const encoder = new TextEncoder();

function jsonValue(value) {
  return JSON.parse(JSON.stringify(value));
}

function exactKeys(value, keys) {
  return value !== null && typeof value === "object" && !Array.isArray(value) &&
    Object.keys(value).length === keys.length && keys.every((key) => key in value);
}

function validPageOutcome(value) {
  if (exactKeys(value, ["error"])) {
    return [
      "CAPTCHA_OR_BLOCKED",
      "FETCH_FAILED",
      "FETCH_TIMEOUT",
      "INVALID_OPTIONS",
      "INVALID_ORIGIN",
      "INVALID_RESPONSE",
      "RESPONSE_TOO_LARGE",
    ].includes(value.error);
  }
  if (exactKeys(value, ["status"])) {
    return Number.isSafeInteger(value.status) && value.status >= 0;
  }
  if (!exactKeys(value, ["page"]) || value.page === null ||
      typeof value.page !== "object" || Array.isArray(value.page)) return false;
  const allowed = new Set(["widgetStates", "seo", "layoutTrackingInfo", "contextObservation", "regionProbe", "navigationProbe"]);
  if (Object.keys(value.page).some((key) => !allowed.has(key)) ||
      value.page.widgetStates === null ||
      typeof value.page.widgetStates !== "object" ||
      Array.isArray(value.page.widgetStates)) return false;
  if ("seo" in value.page &&
      (!exactKeys(value.page.seo, ["title", "link"]) ||
       !(value.page.seo.title === null || typeof value.page.seo.title === "string") ||
       !Array.isArray(value.page.seo.link) ||
       !value.page.seo.link.every((link) =>
         exactKeys(link, ["href"]) && typeof link.href === "string"))) return false;
  if ("contextObservation" in value.page &&
      (!exactKeys(value.page.contextObservation,
        ["regionLabel", "regionVerified", "accountState", "accessState", "signature"]) ||
       !(value.page.contextObservation.regionLabel === null ||
         typeof value.page.contextObservation.regionLabel === "string") ||
       typeof value.page.contextObservation.regionVerified !== "boolean" ||
       !["authenticated", "anonymous", "unknown"].includes(value.page.contextObservation.accountState) ||
       !["available", "unknown"].includes(value.page.contextObservation.accessState) ||
       !(value.page.contextObservation.signature === null ||
         /^[a-f0-9]{64}$/.test(value.page.contextObservation.signature)))) return false;
  if ("regionProbe" in value.page &&
      (!exactKeys(value.page.regionProbe,
        ["addressBookModalAvailable", "selectedRegionLabel"]) ||
       typeof value.page.regionProbe.addressBookModalAvailable !== "boolean" ||
       !(value.page.regionProbe.selectedRegionLabel === null ||
         typeof value.page.regionProbe.selectedRegionLabel === "string"))) return false;
  if ("navigationProbe" in value.page &&
      (!exactKeys(value.page.navigationProbe, ["routeValid", "status"]) ||
       typeof value.page.navigationProbe.routeValid !== "boolean" ||
       !(value.page.navigationProbe.status === null ||
         (Number.isSafeInteger(value.page.navigationProbe.status) &&
          value.page.navigationProbe.status >= 0)))) return false;
  return !("layoutTrackingInfo" in value.page) ||
    exactKeys(value.page.layoutTrackingInfo, ["sku"]);
}

test("shared page outcomes accept all variants and reject malformed unknowns", () => {
  for (const { outcome } of outcomeFixtures.valid) {
    assert.equal(validPageOutcome(outcome), true, JSON.stringify(outcome));
  }
  for (const outcome of outcomeFixtures.invalid) {
    assert.equal(validPageOutcome(outcome), false, JSON.stringify(outcome));
  }
});

test("context exposes the observed city and never the address prompt", async () => {
  class MockElement {}
  const auth = new MockElement();
  const address = {
    getAttribute: () => JSON.stringify({ customCell: { cells: [
      { button: { text: "Москва" } },
      { button: { text: "Укажите адрес" } },
    ] } }),
  };
  const value = await evaluate(
    { mode: "context" },
    {
      HTMLElement: MockElement,
      Element: MockElement,
      getComputedStyle: () => ({ display: "block", visibility: "visible" }),
      crypto: webcrypto,
      document: {
        querySelector(selector) {
          if (selector.startsWith('[id^="state-addressBookBarWeb-"')) return address;
          return selector.startsWith('a[href^="/login"]') ? auth : null;
        },
        querySelectorAll: () => [],
      },
    },
  );
  assert.match(value.page.contextObservation.signature, /^[a-f0-9]{64}$/);
  assert.deepEqual(jsonValue(value), { page: {
    widgetStates: {},
    contextObservation: {
      regionLabel: "Москва",
      regionVerified: true,
      accountState: "anonymous",
      accessState: "available",
      signature: value.page.contextObservation.signature,
    },
  } });
  assert.equal(JSON.stringify(value).includes("address"), false);
  assert.equal(JSON.stringify(value).includes("Укажите адрес"), false);
  const unknown = await evaluate(
    { mode: "context" },
    {
      HTMLElement: MockElement,
      Element: MockElement,
      getComputedStyle: () => ({ display: "block", visibility: "visible" }),
      crypto: webcrypto,
      document: { querySelector: () => null, querySelectorAll: () => [] },
    },
  );
  assert.deepEqual(jsonValue(unknown.page.contextObservation), {
    regionLabel: null,
    regionVerified: false,
    accountState: "unknown",
    accessState: "unknown",
    signature: null,
  });
});

test("context observes the signed-in header without reading profile state", async () => {
  class MockElement {}
  const profile = new MockElement();
  profile.id = "state-profileMenu-1";
  profile.getAttribute = () => JSON.stringify({ accountId: "PRIVATE" });
  const value = await evaluate(
    { mode: "context" },
    {
      HTMLElement: MockElement,
      Element: MockElement,
      getComputedStyle: () => ({ display: "block", visibility: "visible" }),
      crypto: webcrypto,
      document: {
        querySelector(selector) {
          return selector.startsWith('[id^="state-profileMenu-"') ? profile : null;
        },
        querySelectorAll: () => [profile],
      },
    },
  );
  assert.equal(value.page.contextObservation.accountState, "authenticated");
  assert.equal(value.page.contextObservation.accessState, "available");
  assert.match(value.page.contextObservation.signature, /^[a-f0-9]{64}$/);
  assert.doesNotMatch(JSON.stringify(value), /PRIVATE|accountId/);
});

test("authenticated delivery timing and address are not exported as a region", async () => {
  class MockElement {}
  const profile = new MockElement();
  profile.id = "state-profileMenu-1";
  const address = {
    getAttribute: () => JSON.stringify({ customCell: { cells: [
      { button: { text: "Сегодня" } },
      { text: { text: "PRIVATE STREET ADDRESS" } },
    ] } }),
  };
  const value = await evaluate(
    { mode: "context" },
    {
      HTMLElement: MockElement,
      Element: MockElement,
      getComputedStyle: () => ({ display: "block", visibility: "visible" }),
      crypto: webcrypto,
      document: {
        querySelector(selector) {
          if (selector.startsWith('[id^="state-addressBookBarWeb-"')) return address;
          if (selector.startsWith('[id^="state-profileMenu-"')) return profile;
          return null;
        },
        querySelectorAll: () => [profile],
      },
    },
  );

  assert.equal(value.page.contextObservation.accountState, "authenticated");
  assert.equal(value.page.contextObservation.regionLabel, null);
  assert.equal(value.page.contextObservation.regionVerified, false);
  assert.doesNotMatch(JSON.stringify(value), /Сегодня|PRIVATE STREET ADDRESS/);
});

test("authenticated header exposes only an exact address-book navigation capability", async () => {
  class MockElement {}
  const profile = new MockElement();
  profile.id = "state-profileMenu-1";
  const context = async (link) => evaluate(
    { mode: "context" },
    {
      HTMLElement: MockElement,
      Element: MockElement,
      getComputedStyle: () => ({ display: "block", visibility: "visible" }),
      crypto: webcrypto,
      URL,
      document: {
        querySelector(selector) {
          if (selector.startsWith('[id^="state-addressBookBarWeb-"')) return {
            getAttribute: () => JSON.stringify({customCell: {
              action: { link }, cells: [{ button: { text: "Сегодня" } }],
            }}),
          };
          if (selector.startsWith('[id^="state-profileMenu-"')) return profile;
          return null;
        },
        querySelectorAll: () => [profile],
      },
    },
  );

  for (const link of ["/modal/addressbook", "/modal/addressbook?set_sm=1"]) {
    assert.deepEqual(jsonValue((await context(link)).page.regionProbe), {
      addressBookModalAvailable: true,
      selectedRegionLabel: null,
    }, link);
  }
  for (const link of [
    "/modal/addressbook/", "/modal/addressbook?x=1", "/modal/addressbook?set_sm=2",
    "/modal/addressbook?set_sm=1&set_sm=1", "/modal/addressbook?set_sm=1&x=1",
    "/modal/addressbook?%73et_sm=%31", "/modal/addressbook?set_sm=1&&",
    "/modal/addressbook#x",
    "https://evil.example/modal/addressbook", "https://user@www.ozon.ru/modal/addressbook",
  ]) {
    assert.equal((await context(link)).page.regionProbe, undefined, link);
  }
});

test("modal extracts only one selected bounded city prefix without exporting the address", async () => {
  const modal = async (addresses) => evaluate(
    { mode: "contextModal" },
    { document: { querySelector: () => ({
      getAttribute: () => JSON.stringify({ addresses }),
    }) } },
  );
  const selected = { isSelected: true, elements: [{ text: "Москва, PRIVATE STREET ADDRESS" }] };
  const value = await modal([selected]);
  assert.deepEqual(jsonValue(value.page.regionProbe), {
    addressBookModalAvailable: false,
    selectedRegionLabel: "Москва",
  });
  assert.doesNotMatch(JSON.stringify(value), /PRIVATE STREET ADDRESS/);

  for (const addresses of [
    [selected, selected],
    [{ isSelected: false, elements: [{ text: "Москва, PRIVATE" }] }],
    [{ isSelected: true, elements: [{ text: "Москва PRIVATE" }] }],
    [{ isSelected: true, elements: [{ text: "Сегодня, PRIVATE" }] }],
    [{ isSelected: true, elements: [{ text: "Пункт выдачи, PRIVATE" }] }],
    [{ isSelected: true, elements: [{ text: "---, PRIVATE" }] }],
    [{ isSelected: true, elements: [{ text: `${"А".repeat(101)}, PRIVATE` }] }],
  ]) {
    assert.equal((await modal(addresses)).page.regionProbe.selectedRegionLabel, null);
  }
});

test("navigation validation returns only status and a bounded exact-route verdict", async () => {
  const navigation = async (target, href, status = 200) => evaluate(
    { mode: "navigation", target },
    {
      location: { origin: "https://www.ozon.ru", href },
      performance: { getEntriesByType: () => [{ responseStatus: status }] },
    },
  );
  for (const [target, href] of [
    ["home", "https://www.ozon.ru/"],
    ["home", "https://www.ozon.ru/?__rr=7"],
    ["addressBook", "https://www.ozon.ru/modal/addressbook?__rr=1234567890123456"],
  ]) {
    const result = jsonValue(await navigation(target, href));
    assert.deepEqual(result, { page: {
      widgetStates: {},
      navigationProbe: { routeValid: true, status: 200 },
    } });
    assert.equal(JSON.stringify(result).includes("__rr"), false);
    assert.equal(JSON.stringify(result).includes(href), false);
  }
  for (const href of [
    "https://www.ozon.ru/modal/addressbook?__rr=",
    "https://www.ozon.ru/modal/addressbook?__rr=12345678901234567",
    "https://www.ozon.ru/modal/addressbook?__rr=a",
    "https://www.ozon.ru/modal/addressbook?set_sm=1",
    "https://www.ozon.ru/modal/addressbook?%5f_rr=1",
    "https://www.ozon.ru/modal/addressbook?__rr=1&__rr=2",
    "https://www.ozon.ru/modal/addressbook?__rr=1&x=1",
    "https://www.ozon.ru/modal/addressbook/?__rr=1",
    "https://www.ozon.ru/search/?__rr=1",
    "https://www.ozon.ru/modal/addressbook?__rr=1#x",
    "https://evil.example/modal/addressbook?__rr=1",
    "https://user@www.ozon.ru/modal/addressbook?__rr=1",
    "https://www.ozon.ru:444/modal/addressbook?__rr=1",
  ]) {
    const probe = (await navigation("addressBook", href)).page.navigationProbe;
    assert.equal(probe.routeValid, false, href);
    assert.equal(JSON.stringify(probe).includes(href), false, href);
  }
  assert.deepEqual(jsonValue((await navigation(
    "addressBook", "https://www.ozon.ru/modal/addressbook", Number.NaN,
  )).page.navigationProbe), { routeValid: true, status: null });
});

function response(body, { status = 200, contentLength } = {}) {
  const chunks = Array.isArray(body) ? body : [encoder.encode(body)];
  let index = 0;
  return {
    ok: status >= 200 && status < 300,
    status,
    headers: {
      get(name) {
        return name === "content-length" && contentLength !== undefined
          ? String(contentLength)
          : null;
      },
    },
    body: {
      getReader() {
        return {
          async read() {
            return index < chunks.length
              ? { value: chunks[index++], done: false }
              : { value: undefined, done: true };
          },
        };
      },
    },
  };
}

async function evaluate(options, overrides = {}) {
  const context = {
    location: { origin: "https://www.ozon.ru" },
    document: { querySelectorAll: () => [] },
    fetch: async () => {
      throw new Error("unexpected fetch");
    },
    AbortController,
    TextDecoder,
    TextEncoder,
    URL,
    encodeURIComponent,
    setTimeout,
    clearTimeout,
    ...overrides,
  };
  const outcome = await vm.runInNewContext(`(${source})(${JSON.stringify(options)})`, context);
  assert.equal(validPageOutcome(jsonValue(outcome)), true, "page producer violated the shared outcome contract");
  return outcome;
}

test("generated artifact is the callable expression Rust evaluates", async () => {
  assert.equal(
    source.trimEnd().endsWith(";") ||
      source.includes("export ") ||
      source.includes('"use strict"'),
    false,
  );
  assert.equal(source.endsWith("\n"), true);
  const value = await evaluate({ mode: "widgets" });
  assert.deepEqual(jsonValue(value), { error: "CAPTCHA_OR_BLOCKED" });
});

test("fetch retains only public widgets and safe metadata", async () => {
  const body = JSON.stringify({
    widgetStates: {
      "webPrice-1": { price: "100" },
      "webDescription-2": "public",
      "accountWidget-1": { email: "private@example.test" },
    },
    seo: {
      title: "Product",
      link: [{ href: "/product/1", private: "discard" }, null, { rel: "x" }],
      private: "discard",
    },
    layoutTrackingInfo: JSON.stringify({ sku: "12345", private: "discard" }),
    user: { address: "discard" },
  });
  let requestedUrl;
  const value = await evaluate(
    { mode: "fetch", path: "/product/a b" },
    {
      fetch: async (url) => {
        requestedUrl = url;
        return response(body);
      },
    },
  );

  assert.equal(
    requestedUrl,
    "/api/composer-api.bx/page/json/v2?url=%2Fproduct%2Fa%20b",
  );
  assert.deepEqual(jsonValue(value), {
    page: {
      widgetStates: {
        "webPrice-1": { price: "100" },
        "webDescription-2": "public",
      },
      seo: { title: "Product", link: [{ href: "/product/1" }] },
      layoutTrackingInfo: { sku: "12345" },
    },
  });
});

test("source widgets project variants and reviews without account or tracking data", async () => {
  const body = JSON.stringify({ widgetStates: {
    "webAspects-1": { aspects: [{ aspectKey: "color", aspectName: "Цвет", type: "COLOR",
      variants: [{ sku: "902", availability: "AVAILABLE", link: "/product/blue-902/",
        price: 100, data: { value: "Blue", accountId: "PRIVATE" }, trackingInfo: { key: "PRIVATE" } }]
    }], cellTrackingInfo: { secret: "PRIVATE" } },
    "webListReviews-1": {
      itemId: "901", requestedPath: "/product/901/reviews/", productsCount: 999,
      productScore: 4.8,
      paging: { page: 1, total: 2, links: [{ text: "2", urlParams: "page=2", secret: "PRIVATE" }] },
      sortings: [{ active: true, name: "Useful", value: "usefulness_desc", action: "PRIVATE" }],
      user: { guid: "PRIVATE" }, actions: { vote: "PRIVATE" },
      products: { "901": { itemId: "901", name: "Mouse", uri: "/product/901/",
        variants: [{ name: "Color", value: "Blue", tracking: "PRIVATE" }], trackingInfo: "PRIVATE" },
        "999": { itemId: "999", name: "Unreturned review product" } },
      reviews: [{ uuid: "review-1", itemId: "901", publishedAt: 1, isItemPurchased: true,
        author: { firstName: "Ada", guid: "PRIVATE" },
        content: { score: 5, comment: "Good", photos: [{ url: "https://ir.ozone.ru/a.jpg", token: "PRIVATE" }] },
        editUrl: "PRIVATE", sharing: { url: "PRIVATE" } }]
    }
  } });
  const value = jsonValue(await evaluate(
    { mode: "fetch", path: "/product/901/" },
    { fetch: async () => response(body) },
  ));
  assert.equal(JSON.stringify(value).includes("PRIVATE"), false);
  assert.deepEqual(value.page.widgetStates["webAspects-1"].aspects[0].variants[0], {
    availability: "AVAILABLE", link: "/product/blue-902/", price: 100, sku: "902",
    data: { value: "Blue" },
  });
  const reviews = value.page.widgetStates["webListReviews-1"];
  assert.equal(reviews.productScore, 4.8);
  assert.equal(reviews.productsCount, 2);
  assert.deepEqual(Object.keys(reviews.products), ["901"]);
  assert.deepEqual(reviews.reviews[0].author, { firstName: "Ada" });
  assert.equal(reviews.reviews[0].publishedAt, 1);
  assert.deepEqual(reviews.reviews[0].content.photos, [{ url: "https://ir.ozone.ru/a.jpg" }]);
});

test("DOM fallback filters private widgets", async () => {
  const elements = [
    { id: "state-webPrice-1", getAttribute: () => '{"price":"100"}' },
    { id: "state-orders-1", getAttribute: () => '{"address":"private"}' },
  ];
  const value = await evaluate(
    { mode: "widgets" },
    { document: { querySelectorAll: () => elements } },
  );
  assert.deepEqual(jsonValue(value), {
    page: { widgetStates: { "webPrice-1": '{"price":"100"}' } },
  });
});

test("rejects oversized header, streamed fetch, DOM, and filtered output", async () => {
  const tooLarge = 4 * 1024 * 1024 + 1;
  const header = await evaluate(
    { mode: "fetch", path: "/x" },
    { fetch: async () => response("{}", { contentLength: tooLarge }) },
  );
  assert.deepEqual(jsonValue(header), { error: "RESPONSE_TOO_LARGE" });

  const streamed = await evaluate(
    { mode: "fetch", path: "/x" },
    { fetch: async () => response([new Uint8Array(tooLarge)]) },
  );
  assert.deepEqual(jsonValue(streamed), { error: "RESPONSE_TOO_LARGE" });

  const dom = await evaluate(
    { mode: "widgets" },
    {
      document: {
        querySelectorAll: () => [
          {
            id: "state-webDescription-1",
            getAttribute: () => "x".repeat(tooLarge),
          },
        ],
      },
    },
  );
  assert.deepEqual(jsonValue(dom), { error: "RESPONSE_TOO_LARGE" });

  const filtered = await evaluate(
    { mode: "widgets" },
    {
      document: {
        querySelectorAll: () => [
          {
            id: "state-webDescription-1",
            getAttribute: () => "x".repeat(4 * 1024 * 1024),
          },
        ],
      },
    },
  );
  assert.deepEqual(jsonValue(filtered), { error: "RESPONSE_TOO_LARGE" });
});

test("reports HTTP, invalid responses, fetch failures, and invalid options", async () => {
  const http = await evaluate(
    { mode: "fetch", path: "/x" },
    { fetch: async () => response("", { status: 403 }) },
  );
  assert.deepEqual(jsonValue(http), { status: 403 });

  const invalid = await evaluate(
    { mode: "fetch", path: "/x" },
    { fetch: async () => response("not json") },
  );
  assert.deepEqual(jsonValue(invalid), { error: "INVALID_RESPONSE" });

  const nullPage = await evaluate(
    { mode: "fetch", path: "/x" },
    { fetch: async () => response("null") },
  );
  assert.deepEqual(jsonValue(nullPage), { error: "INVALID_RESPONSE" });

  const failed = await evaluate(
    { mode: "fetch", path: "/x" },
    { fetch: async () => { throw new Error("offline"); } },
  );
  assert.deepEqual(jsonValue(failed), { error: "FETCH_FAILED" });

  assert.deepEqual(jsonValue(await evaluate({ mode: "fetch" })), {
    error: "INVALID_OPTIONS",
  });
});

test("distinguishes timeout aborts and rejects another origin", async () => {
  const timeout = await evaluate(
    { mode: "fetch", path: "/x" },
    {
      setTimeout(callback) {
        queueMicrotask(callback);
        return 1;
      },
      clearTimeout() {},
      fetch: async (_url, init) =>
        new Promise((_resolve, reject) => {
          init.signal.addEventListener("abort", () =>
            reject(new DOMException("aborted", "AbortError")),
          );
        }),
    },
  );
  assert.deepEqual(jsonValue(timeout), { error: "FETCH_TIMEOUT" });

  const wrongOrigin = await evaluate(
    { mode: "widgets" },
    { location: { origin: "https://example.test" } },
  );
  assert.deepEqual(jsonValue(wrongOrigin), { error: "INVALID_ORIGIN" });
});

const searchWidgets = {
  "filtersDesktop-1": { sections: [{ filters: [
    { type: "categoryFilter", key: "category", categoryFilter: { title: "Category", categories: [{ title: "Mice", level: 1, isActive: true, urlValue: "/category/mice/" }] } },
    { type: "checkboxesFilter", key: "brand", checkboxesFilter: { title: "Brand", sections: [{ items: [{ key: "42", title: { text: "Brand A" }, isSelected: true }] }], openingButtons: { showAllButton: { action: { link: "execute-secret" } } }, hasManyValues: true } },
    { type: "multipleRangesFilter", key: "currency_price", multipleRangesFilter: { rangeFilter: { title: "Price", description: { text: "Card price" }, minValue: 10, maxValue: 100, fromValue: 20, toValue: 80 }, checkboxesFilter: { sections: [{ items: [{ key: "10;100", title: { text: "All" } }] }] } } },
    { type: "boolFilter", key: "sale", boolFilter: { title: "Sale", isSelected: false } },
    { type: "colorFilter", key: "color", colorFilter: { title: "Color", colorIcons: [{ key: "1", description: "Black", isSelected: true }] } },
  ] }] },
  "searchResultsSort-1": { sortButton: { options: [{ name: "Popular", isSelected: true, action: { link: "/search/?sorting=score" } }] } },
  "searchResultsFiltersActive-1": { activeFilters: [{ key: "brand", name: "Brand", ftype: "RESPONSE_FILTER_TYPE_MULTI", activeValues: [{ title: "Brand A", disableUri: "/search/" }] }] },
  "infiniteVirtualPaginator-1": { nextPage: "/search/?page=2", prevPage: "", size: 10, layoutContainer: "default", fetchType: "virtualScroll" },
  "categoryBrandList-1": { brands: [{ text: "Brand A", action: { link: "/category/mice/brand-a/" } }] },
};

function withPrivateMarkers(value) {
  if (Array.isArray(value)) return value.map(withPrivateMarkers);
  if (value && typeof value === "object") return {
    ...Object.fromEntries(Object.entries(value).map(([k, v]) => [k, withPrivateMarkers(v)])),
    trackingInfo: { secret: "PRIVATE_MARKER" },
    cellTrackingInfo: { filterValue: "PRIVATE_MARKER" },
    abFeatures: "PRIVATE_MARKER",
    searchBar: { history: "PRIVATE_MARKER" },
    account: "PRIVATE_MARKER",
  };
  return value;
}

for (const mode of ["fetch", "widgets"]) {
  test(`${mode} projects search metadata at every nesting level`, async () => {
    const marked = withPrivateMarkers(searchWidgets);
    const states = Object.fromEntries(Object.entries(marked).map(([k, v], index) =>
      [k, mode === "widgets" || index % 2 ? JSON.stringify(v) : v]));
    const value = jsonValue(await evaluate(
      mode === "fetch" ? { mode, path: "/search/" } : { mode },
      mode === "fetch" ? { fetch: async () => response(JSON.stringify({ widgetStates: states })) }
        : { document: { querySelectorAll: () => Object.entries(states).map(([key, state]) => ({ id: `state-${key}`, getAttribute: () => state })) } },
    ));
    const expected = structuredClone(searchWidgets);
    expected["filtersDesktop-1"].sections[0].filters[1].checkboxesFilter.openingButtons.showAllButton = {};
    assert.deepEqual(value, { page: { widgetStates: expected } });
    assert.equal(JSON.stringify(value).includes("PRIVATE_MARKER"), false);
    assert.equal(JSON.stringify(value).includes("execute-secret"), false);
  });

  test(`${mode} safely ignores malformed optional metadata`, async () => {
    const states = {
      "webPrice-1": '{"price":"100"}',
      "filtersDesktop-1": '{invalid',
      "searchResultsSort-1": { sortButton: { options: null } },
      "categoryBrandList-1": [],
      "searchResultsFiltersActive-1": null,
      "infiniteVirtualPaginator-1": { nextPage: { private: "PRIVATE_MARKER" } },
      "filtersDesktop-2": { sections: [null, [], { filters: [null, { type: "account", account: { title: "PRIVATE_MARKER" } }, { type: "boolFilter", boolFilter: [] }] }] },
    };
    const value = jsonValue(await evaluate(
      mode === "fetch" ? { mode, path: "/search/" } : { mode },
      mode === "fetch" ? { fetch: async () => response(JSON.stringify({ widgetStates: states })) }
        : { document: { querySelectorAll: () => Object.entries(states).map(([key, state]) => ({ id: `state-${key}`, getAttribute: () => typeof state === "string" ? state : JSON.stringify(state) })) } },
    ));
    assert.deepEqual(value, { page: { widgetStates: {
      "webPrice-1": '{"price":"100"}',
      "infiniteVirtualPaginator-1": {},
      "filtersDesktop-2": { sections: [{ filters: [] }] },
    } } });
  });
}
