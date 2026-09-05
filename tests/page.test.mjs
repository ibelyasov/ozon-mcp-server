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
