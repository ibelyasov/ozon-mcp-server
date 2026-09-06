import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

const source = readFileSync(new URL("../rust/page.js", import.meta.url), "utf8");
const encoder = new TextEncoder();

function jsonValue(value) {
  return JSON.parse(JSON.stringify(value));
}

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
    encodeURIComponent,
    setTimeout,
    clearTimeout,
    ...overrides,
  };
  return vm.runInNewContext(`(${source})(${JSON.stringify(options)})`, context);
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
