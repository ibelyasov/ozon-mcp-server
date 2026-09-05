import assert from "node:assert/strict";
import test from "node:test";

import {
  BrowserRuntime,
  COMPOSER_API_URL,
  boundedPageFetch,
  pageWidgetResponse,
  isRequestedPage,
  browserOptionsFromEnvironment,
} from "../src/browser-runtime.js";

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

function wait(milliseconds) {
  return new Promise(resolve => setTimeout(resolve, milliseconds));
}

function makePage(results = []) {
  const evaluateStarted = deferred();
  const page = {
    evaluateCalls: [],
    currentUrl: "https://www.ozon.ru/",
    async goto(url) { this.currentUrl = url; },
    url() { return this.currentUrl; },
    async waitForTimeout() {},
    async title() { return "Ozon"; },
    evaluate(_fn, args) {
      if (args?.widgetNames) return Promise.resolve(null);
      this.evaluateCalls.push(args);
      evaluateStarted.resolve();
      const result = results.shift();
      if (result instanceof Error) return Promise.reject(result);
      return Promise.resolve(result);
    },
  };
  return { page, evaluateStarted };
}

function makeContext(page) {
  let connected = true;
  let disconnected;
  const browser = {
    isConnected: () => connected,
    once(event, listener) {
      if (event === "disconnected") disconnected = listener;
    },
  };
  return {
    closeCalls: 0,
    cdpCalls: [],
    cdpDetached: false,
    async newCDPSession() {
      return {
        send: async (method, args) => {
          this.cdpCalls.push({ method, args });
          return { userAgent: "Mozilla/5.0 HeadlessChrome/148.0.0.0 Safari/537.36" };
        },
        detach: async () => { this.cdpDetached = true; },
      };
    },
    browser: () => browser,
    newPage: async () => page,
    async close() {
      this.closeCalls += 1;
      connected = false;
    },
    emitDisconnected() {
      connected = false;
      disconnected?.();
    },
  };
}

function makeRuntime(contexts, options = {}) {
  const launches = [];
  const chromium = {
    async launchPersistentContext(...args) {
      launches.push(args);
      const context = contexts.shift();
      if (!context) throw new Error("unexpected Chromium launch");
      return context;
    },
  };
  return {
    launches,
    runtime: new BrowserRuntime({
      chromium,
      userDataDir: "/synthetic/profile",
      challengeWaitMs: 1,
      navigationTimeoutMs: 10,
      launchTimeoutMs: 10,
      fetchTimeoutMs: 10,
      idleTimeoutMs: 0,
      shutdownTimeoutMs: 10,
      ...options,
    }),
  };
}

test("defaults to new full Chromium headless and deprecates the off-screen switch", () => {
  const messages = [];
  assert.deepEqual(
    browserOptionsFromEnvironment({}, { userDataDir: "/profile", log: message => messages.push(message) }),
    { userDataDir: "/profile", headless: true, targetCity: "" }
  );
  assert.deepEqual(
    browserOptionsFromEnvironment(
      { OZON_HEADLESS: "false", OZON_HIDE_WINDOW: "1", OZON_CITY: "  Казань  " },
      { userDataDir: "/profile", log: message => messages.push(message) }
    ),
    { userDataDir: "/profile", headless: false, targetCity: "Казань" }
  );
  assert.equal(messages.length, 1);
  assert.match(messages[0], /ignored/);
});

test("uses one persistent full-Chromium context and sends a bounded same-origin request", async () => {
  const { page } = makePage([{ status: 200, text: '{"ok":true}' }]);
  const context = makeContext(page);
  const { runtime, launches } = makeRuntime([context]);

  assert.deepEqual(await runtime.fetchJson("/search/?text=lamp", { retries: 0 }), { ok: true });
  assert.equal(launches.length, 1);
  const [profile, launchOptions] = launches[0];
  assert.equal(profile, "/synthetic/profile");
  assert.equal(launchOptions.channel, "chromium");
  assert.equal(launchOptions.chromiumSandbox, true);
  assert.equal(launchOptions.headless, true);
  assert.deepEqual(context.cdpCalls, [
    { method: "Browser.getVersion", args: undefined },
    { method: "Network.setUserAgentOverride", args: {
      userAgent: "Mozilla/5.0 Chrome/148.0.0.0 Safari/537.36",
      acceptLanguage: "ru-RU,ru;q=0.9",
    } },
  ]);
  assert.equal(context.cdpDetached, false);
  assert.ok(!launchOptions.args.includes("--no-sandbox"));
  assert.ok(!launchOptions.args.includes("--disable-setuid-sandbox"));
  assert.deepEqual(page.evaluateCalls[0], {
    requestUrl: COMPOSER_API_URL + encodeURIComponent("/search/?text=lamp"),
    timeoutMs: 10,
    maxBytes: 4 * 1024 * 1024,
  });
  await runtime.shutdown();
});

test("cleans up a partially initialized context", async () => {
  const { page } = makePage();
  page.goto = async () => { throw new Error("navigation failed"); };
  const context = makeContext(page);
  const { runtime } = makeRuntime([context]);

  await assert.rejects(runtime.fetchJson("/search/?text=lamp", { retries: 0 }), /navigation failed/);
  assert.equal(context.closeCalls, 1);
});

test("aborting an active request closes its ticket without waiting for page.evaluate", async () => {
  const pending = deferred();
  const { page } = makePage([{ status: 200, text: "{}" }, pending.promise]);
  const context = makeContext(page);
  const { runtime } = makeRuntime([context]);
  await runtime.fetchJson("/search/?text=warmup", { retries: 0 });

  const secondEvaluateStarted = deferred();
  const evaluate = page.evaluate.bind(page);
  page.evaluate = function patchedEvaluate(fn, args) {
    if (this.evaluateCalls.length === 1) secondEvaluateStarted.resolve();
    return evaluate(fn, args);
  };
  const controller = new AbortController();
  const request = runtime.fetchJson("/search/?text=cancel", { signal: controller.signal, retries: 0 });
  await secondEvaluateStarted.promise;
  controller.abort(new Error("request cancelled"));

  await assert.rejects(request, /request cancelled/);
  assert.equal(context.closeCalls, 1);
  pending.resolve({ status: 200, text: "{}" });
});

test("an immediate abort after page.evaluate starts observes its later rejection", async () => {
  const { page } = makePage([{ status: 200, text: "{}" }]);
  const context = makeContext(page);
  const { runtime } = makeRuntime([context]);
  await runtime.fetchJson("/search/?text=warmup", { retries: 0 });

  const controller = new AbortController();
  page.evaluate = function immediateAbort() {
    controller.abort(new Error("immediate cancellation"));
    return Promise.reject(new Error("late page failure"));
  };

  await assert.rejects(
    runtime.fetchJson("/search/?text=cancel", { signal: controller.signal, retries: 0 }),
    /immediate cancellation/
  );
  await wait(0);
  assert.equal(context.closeCalls, 1);
});

test("aborting during Chromium launch closes the late-arriving context", async () => {
  const launch = deferred();
  const { runtime, launches } = makeRuntime([launch.promise]);
  const controller = new AbortController();
  const request = runtime.fetchJson("/search/?text=launch", { signal: controller.signal, retries: 0 });
  await wait(0);
  assert.equal(launches.length, 1);

  controller.abort(new Error("launch cancelled"));
  await assert.rejects(request, /launch cancelled/);

  const { page } = makePage();
  const context = makeContext(page);
  launch.resolve(context);
  await runtime.shutdown();
  assert.equal(context.closeCalls, 1);
});

test("idle shutdown never closes a context while a request is active", async () => {
  const pending = deferred();
  const { page, evaluateStarted } = makePage([pending.promise]);
  const context = makeContext(page);
  const { runtime } = makeRuntime([context], { idleTimeoutMs: 10 });

  const request = runtime.fetchJson("/search/?text=slow", { retries: 0 });
  await evaluateStarted.promise;
  await wait(25);
  assert.equal(context.closeCalls, 0);

  pending.resolve({ status: 200, text: "{}" });
  await request;
  await wait(25);
  assert.equal(context.closeCalls, 1);
});

test("retries a 403 with a fresh persistent context", async () => {
  const first = makePage([{ status: 403, text: "" }]);
  const second = makePage([{ status: 200, text: '{"retry":true}' }]);
  const firstContext = makeContext(first.page);
  const secondContext = makeContext(second.page);
  const { runtime, launches } = makeRuntime([firstContext, secondContext]);

  assert.deepEqual(await runtime.fetchJson("/search/?text=retry", { retries: 1 }), { retry: true });
  assert.equal(firstContext.closeCalls, 1);
  assert.equal(launches.length, 2);
  await runtime.shutdown();
});

test("a late disconnect from an old context cannot erase a newer one", async () => {
  const first = makePage([{ status: 200, text: "{}" }]);
  const second = makePage([{ status: 200, text: "{}" }, { status: 200, text: "{}" }]);
  const oldContext = makeContext(first.page);
  const newContext = makeContext(second.page);
  const { runtime, launches } = makeRuntime([oldContext, newContext]);

  await runtime.fetchJson("/search/?text=old", { retries: 0 });
  await runtime.shutdown();
  await runtime.fetchJson("/search/?text=new", { retries: 0 });
  oldContext.emitDisconnected();
  await runtime.fetchJson("/search/?text=still-new", { retries: 0 });

  assert.equal(launches.length, 2);
  await runtime.shutdown();
});

test("rejects oversized responses before parsing", async () => {
  const { page } = makePage([{ status: 200, tooLarge: true }]);
  const context = makeContext(page);
  const { runtime } = makeRuntime([context]);

  await assert.rejects(runtime.fetchJson("/search/?text=large", { retries: 0 }), /response exceeds/);
  await runtime.shutdown();
});

test("page-side fetch turns its timeout into a bounded result", async () => {
  const originalFetch = globalThis.fetch;
  let aborted = false;
  globalThis.fetch = (_url, { signal }) => new Promise(() => {
    signal.addEventListener("abort", () => {
      aborted = true;
    }, { once: true });
  });
  try {
    assert.deepEqual(
      await boundedPageFetch({ requestUrl: "https://example.invalid", timeoutMs: 1, maxBytes: 8 }),
      { timedOut: true }
    );
    assert.equal(aborted, true);
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test("page-side fetch rejects a streamed body that crosses the byte limit", async () => {
  const originalFetch = globalThis.fetch;
  let cancelCalls = 0;
  globalThis.fetch = async () => ({
    status: 200,
    headers: { get: () => null },
    body: {
      getReader: () => ({
        read: async () => ({ done: false, value: new Uint8Array([1, 2, 3]) }),
        cancel: async () => { cancelCalls += 1; },
      }),
    },
  });
  try {
    assert.deepEqual(
      await boundedPageFetch({ requestUrl: "https://example.invalid", timeoutMs: 10, maxBytes: 2 }),
      { status: 200, tooLarge: true }
    );
    assert.equal(cancelCalls, 1);
  } finally {
    globalThis.fetch = originalFetch;
  }
});


test("page fallback exports only public widgets and enforces its byte limit", () => {
  const previous = globalThis.document;
  globalThis.document = { querySelectorAll: () => [
    { id: "state-addressBookBarWeb-1", getAttribute: () => '{"private":"address"}' },
    { id: "state-tileGridDesktop-2", getAttribute: () => '{"items":[]}' },
  ] };
  try {
    const args = { widgetNames: ["tileGridDesktop"], maxBytes: 1024 };
    const result = pageWidgetResponse(args);
    assert.deepEqual(JSON.parse(result.text), { widgetStates: { "tileGridDesktop-2": '{"items":[]}' } });
    assert.equal(pageWidgetResponse({ ...args, maxBytes: 5 }).tooLarge, true);
  } finally { globalThis.document = previous; }
});

test("composer 403 falls back to the requested page without relaunching", async () => {
  const { page } = makePage([{ status: 403, text: "blocked" }]);
  const evaluate = page.evaluate.bind(page);
  const destinations = [];
  page.goto = async url => { destinations.push(url); page.currentUrl = url; };
  page.evaluate = (fn, args) => args?.widgetNames
    ? Promise.resolve({ status: 200, text: '{"widgetStates":{"tileGridDesktop-1":"{}"}}' })
    : evaluate(fn, args);
  const context = makeContext(page);
  const { runtime, launches } = makeRuntime([context]);
  try {
    const result = await runtime.fetchJson("/search/?text=lamp");
    assert.ok(result.widgetStates["tileGridDesktop-1"]);
    assert.equal(launches.length, 1);
    assert.equal(destinations.at(-1), "https://www.ozon.ru/search/?text=lamp");
  } finally { await runtime.shutdown(); }
});

test("headed mode leaves the browser's native user agent unchanged", async () => {
  const { page } = makePage([{ status: 200, text: '{}' }]);
  const context = makeContext(page);
  const { runtime } = makeRuntime([context], { headless: false });
  try {
    await runtime.fetchJson("/search/?text=lamp");
    assert.deepEqual(context.cdpCalls, []);
  } finally { await runtime.shutdown(); }
});


test("page fallback rejects redirects to recommendations or another product", () => {
  const origin = "https://www.ozon.ru";
  assert.equal(isRequestedPage(origin + "/search/?text=mouse", origin + "/"), false);
  assert.equal(isRequestedPage(origin + "/search/?text=mouse", origin + "/category/mice-123/?category_was_predicted=true&text=mouse"), true);
  assert.equal(isRequestedPage(origin + "/search/?text=mouse", origin + "/category/mice-123/?category_was_predicted=true&text=lamp"), false);
  assert.equal(isRequestedPage(origin + "/search/?text=mouse", origin + "/search/?text=lamp"), false);
  assert.equal(isRequestedPage(origin + "/product/123/", origin + "/product/mouse-123/"), true);
  assert.equal(isRequestedPage(origin + "/product/123/reviews/", origin + "/product/mouse-123/"), false);
  assert.equal(isRequestedPage(origin + "/product/123/", origin + "/product/124/"), false);
});

test("page fallback cannot turn a 404 with widgets into a success", async () => {
  const { page } = makePage([{ status: 403, text: "blocked" }]);
  page.goto = async url => { page.currentUrl = url; return { status: () => url.includes("search") ? 404 : 200 }; };
  const context = makeContext(page);
  const { runtime } = makeRuntime([context]);
  try { await assert.rejects(runtime.fetchJson("/search/?text=lamp", { retries: 0 }), /HTTP 404/); }
  finally { await runtime.shutdown(); }
});
