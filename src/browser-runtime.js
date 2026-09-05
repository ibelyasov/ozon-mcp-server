// Browser lifecycle and bounded same-origin composer-api requests.
//
// This module deliberately does not import Playwright. Keeping the driver
// injected makes the lifecycle testable without starting Chromium or touching
// an Ozon account/profile.

export const HOME_URL = "https://www.ozon.ru/";
export const COMPOSER_API_URL = "https://www.ozon.ru/api/composer-api.bx/page/json/v2?url=";

const DEAD_BROWSER =
  /Target page, context or browser has been closed|Session closed|Connection closed|browser has been closed/i;

const DEFAULT_LAUNCH_ARGS = [
  "--disable-blink-features=AutomationControlled",
  "--mute-audio",
  "--no-first-run",
  "--no-default-browser-check",
  "--disable-extensions",
  "--disable-background-networking",
];

const noop = () => {};

function abortReason(signal) {
  if (signal?.reason instanceof Error) return signal.reason;
  return new DOMException("The operation was aborted", "AbortError");
}

function throwIfAborted(signal) {
  if (signal?.aborted) throw abortReason(signal);
}

function isAbort(signal) {
  return Boolean(signal?.aborted);
}

function isUsable(ticket) {
  if (!ticket?.ready || ticket.closed || !ticket.context) return false;
  const browser = ticket.context.browser?.();
  return browser?.isConnected?.() !== false;
}

function positiveInteger(value, name) {
  if (!Number.isSafeInteger(value) || value < 1) {
    throw new TypeError(`${name} must be a positive integer`);
  }
  return value;
}

// This function is serialized by Playwright and runs in the already-warmed
// Ozon page. Keep it closure-free so it can also be unit-tested with a fake
// browser Fetch implementation.
export async function boundedPageFetch({ requestUrl, timeoutMs, maxBytes }) {
  const controller = new AbortController();
  let timer;
  const timedOut = new Promise((_, reject) => {
    timer = setTimeout(() => {
      controller.abort();
      const error = new Error("Ozon page fetch timed out");
      error.name = "AbortError";
      reject(error);
    }, timeoutMs);
  });
  const withinDeadline = promise => Promise.race([promise, timedOut]);
  try {
    const response = await withinDeadline(fetch(requestUrl, {
      headers: { accept: "application/json" },
      signal: controller.signal,
    }));
    const advertisedLength = Number(response.headers.get("content-length"));
    if (Number.isFinite(advertisedLength) && advertisedLength > maxBytes) {
      controller.abort();
      return { status: response.status, tooLarge: true };
    }

    const reader = response.body?.getReader();
    if (!reader) return { status: response.status, text: "" };
    const decoder = new TextDecoder();
    let bytes = 0;
    let text = "";
    for (;;) {
      const { done, value } = await withinDeadline(reader.read());
      if (done) break;
      bytes += value.byteLength;
      if (bytes > maxBytes) {
        controller.abort();
        void Promise.resolve(reader.cancel()).catch(() => {});
        return { status: response.status, tooLarge: true };
      }
      text += decoder.decode(value, { stream: true });
    }
    return { status: response.status, text: text + decoder.decode() };
  } catch (error) {
    if (error?.name === "AbortError") return { timedOut: true };
    throw error;
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Parse user-facing environment values without coupling tests to process.env.
 * `OZON_HEADLESS=false` is the only supported way to request a visible login
 * window. The old off-screen-window switch is intentionally ignored.
 */
export function browserOptionsFromEnvironment(env = {}, { userDataDir, log = noop } = {}) {
  if (!userDataDir) throw new TypeError("userDataDir is required");

  const rawHeadless = String(env.OZON_HEADLESS ?? "").trim().toLowerCase();
  if (env.OZON_HIDE_WINDOW !== undefined) {
    log("OZON_HIDE_WINDOW is ignored; use OZON_HEADLESS=false for a visible login window.");
  }

  return {
    userDataDir,
    headless: rawHeadless !== "false",
    targetCity: String(env.OZON_CITY ?? "").trim(),
  };
}

/**
 * Owns one persistent Chromium profile. Context tickets are identity-checked
 * before global state is changed so an old close/disconnect can never erase a
 * newer context.
 */
export class BrowserRuntime {
  constructor({
    chromium,
    userDataDir,
    headless = true,
    targetCity = "",
    homeUrl = HOME_URL,
    apiUrl = COMPOSER_API_URL,
    challengeWaitMs = 12_000,
    navigationTimeoutMs = 90_000,
    launchTimeoutMs = 30_000,
    fetchTimeoutMs = 45_000,
    maxResponseBytes = 4 * 1024 * 1024,
    idleTimeoutMs = 10 * 60 * 1000,
    shutdownTimeoutMs = 5_000,
    log = noop,
  } = {}) {
    if (!chromium?.launchPersistentContext) {
      throw new TypeError("A Playwright chromium driver is required");
    }
    if (!userDataDir) throw new TypeError("userDataDir is required");

    this.chromium = chromium;
    this.userDataDir = userDataDir;
    this.headless = headless;
    this.targetCity = targetCity;
    this.homeUrl = homeUrl;
    this.apiUrl = apiUrl;
    this.challengeWaitMs = positiveInteger(challengeWaitMs, "challengeWaitMs");
    this.navigationTimeoutMs = positiveInteger(navigationTimeoutMs, "navigationTimeoutMs");
    this.launchTimeoutMs = positiveInteger(launchTimeoutMs, "launchTimeoutMs");
    this.fetchTimeoutMs = positiveInteger(fetchTimeoutMs, "fetchTimeoutMs");
    this.maxResponseBytes = positiveInteger(maxResponseBytes, "maxResponseBytes");
    this.idleTimeoutMs = Number.isSafeInteger(idleTimeoutMs) && idleTimeoutMs > 0 ? idleTimeoutMs : 0;
    this.shutdownTimeoutMs = Number.isSafeInteger(shutdownTimeoutMs) && shutdownTimeoutMs > 0
      ? shutdownTimeoutMs
      : 0;
    this.log = log;

    this.current = null;
    this.initializing = null;
    this.retiring = new Set();
    this.activeRequests = 0;
    this.idleTimer = null;
    this.nextTicketId = 1;
  }

  async fetchJson(path, { signal, retries = 1 } = {}) {
    if (typeof path !== "string" || !path.startsWith("/")) {
      throw new TypeError("path must be an Ozon site path beginning with '/'");
    }
    if (!Number.isSafeInteger(retries) || retries < 0 || retries > 3) {
      throw new TypeError("retries must be an integer from 0 to 3");
    }

    this.activeRequests += 1;
    this._clearIdleTimer();
    try {
      for (let attempt = 0; ; attempt += 1) {
        throwIfAborted(signal);
        let ticket;
        try {
          ticket = await this._ensureContext(signal);
          const response = await this._fetchFromPage(ticket, path, signal);
          throwIfAborted(signal);

          if (response.timedOut) {
            throw new Error(`Ozon request timed out after ${this.fetchTimeoutMs}ms`);
          }
          if (response.tooLarge) {
            throw new Error(`Ozon response exceeds ${this.maxResponseBytes} byte limit`);
          }
          if (!Number.isInteger(response.status)) {
            throw new Error("Ozon returned an invalid HTTP response");
          }
          if (response.status === 200) return JSON.parse(response.text);

          if ((response.status === 403 || response.status === 307) && attempt < retries) {
            await this._withAbort(this._closeTicket(ticket), signal);
            continue;
          }
          throw new Error(`Ozon returned HTTP ${response.status}`);
        } catch (error) {
          if (isAbort(signal)) throw abortReason(signal);
          if (DEAD_BROWSER.test(String(error?.message)) && attempt < retries) {
            await this._withAbort(this._closeTicket(ticket || this.current), signal);
            continue;
          }
          throw error;
        }
      }
    } finally {
      this.activeRequests -= 1;
      this._scheduleIdleShutdown();
    }
  }

  async shutdown() {
    this._clearIdleTimer();
    const tickets = new Set([this.current, this.initializing?.ticket]);
    const closing = [
      ...this.retiring,
      ...[...tickets].filter(Boolean).map(ticket => this._closeTicket(ticket)),
    ];
    if (closing.length === 0) return;

    const settled = Promise.all(closing).catch(noop);
    if (this.shutdownTimeoutMs === 0) {
      await settled;
      return;
    }
    await this._settlesWithin(settled, this.shutdownTimeoutMs);
  }

  async _ensureContext(signal) {
    throwIfAborted(signal);
    if (isUsable(this.current)) return this.current;
    if (this.current) this._closeTicket(this.current);

    let pending = this.initializing;
    if (!pending) {
      await this._withAbort(this._waitForRetiringTickets(), signal);
      if (isUsable(this.current)) return this.current;
      pending = this.initializing || this._startInitialization();
    }
    return this._withAbort(pending.promise, signal, () => this._closeTicket(pending.ticket));
  }

  _startInitialization() {
    const ticket = {
      id: this.nextTicketId++,
      context: null,
      page: null,
      ready: false,
      closed: false,
      launchPromise: null,
      closePromise: null,
    };
    const record = { ticket, promise: null };
    record.promise = this._initialize(ticket).then(
      () => {
        if (ticket.closed) throw new Error("Browser session closed during startup");
        ticket.ready = true;
        this.current = ticket;
        return ticket;
      },
      async error => {
        await this._closeTicket(ticket);
        throw error;
      }
    );
    this.initializing = record;
    record.promise.then(
      () => this._clearInitializing(record),
      () => this._clearInitializing(record)
    );
    return record;
  }

  _clearInitializing(record) {
    if (this.initializing === record) this.initializing = null;
  }

  async _initialize(ticket) {
    this.log(`starting full Chromium (${this.headless ? "headless" : "headed"})…`);
    ticket.launchPromise = Promise.resolve(this.chromium.launchPersistentContext(this.userDataDir, {
      channel: "chromium",
      chromiumSandbox: true,
      headless: this.headless,
      args: DEFAULT_LAUNCH_ARGS,
      viewport: { width: 1920, height: 1080 },
      locale: "ru-RU",
      timeout: this.launchTimeoutMs,
    }));
    const context = await ticket.launchPromise;
    ticket.context = context;
    if (ticket.closed) {
      await this._closeTicket(ticket);
      throw new Error("Browser session closed during startup");
    }

    const browser = context.browser?.();
    browser?.once?.("disconnected", () => this._onDisconnected(ticket));

    ticket.page = await context.newPage();
    if (ticket.closed) {
      await this._closeTicket(ticket);
      throw new Error("Browser session closed during startup");
    }

    await ticket.page.goto(this.homeUrl, {
      waitUntil: "domcontentloaded",
      timeout: this.navigationTimeoutMs,
    });
    await ticket.page.waitForTimeout(this.challengeWaitMs);
    const title = await ticket.page.title();
    if (/antibot|ограничен|доступ|нет соединения/i.test(title)) {
      throw new Error(`Ozon challenge was not passed (title: ${title.slice(0, 80)})`);
    }
    this.log(`Ozon challenge passed: ${title.slice(0, 40)}`);

    if (this.targetCity) await this._trySetCity(ticket.page, this.targetCity);
  }

  _onDisconnected(ticket) {
    ticket.closed = true;
    if (this.current === ticket) this.current = null;
    if (this.initializing?.ticket === ticket) this.initializing = null;
  }

  async _trySetCity(page, city) {
    try {
      this.log("trying to set the configured Ozon region…");
      const locationButton = page.locator(
        '[data-widget*="locationSelector" i], [data-widget*="region" i], ' +
          'button[aria-label*="ород"], button:has-text("Изменить")'
      );
      await locationButton.first().click({ timeout: 5_000 });

      const input = page.locator(
        '[role="dialog"] input[type="text"], [role="dialog"] input[placeholder*="ород" i]'
      );
      await input.first().waitFor({ state: "visible", timeout: 5_000 });
      await input.first().fill(city);
      await page.waitForTimeout(1_500);
      const suggestion = page.locator(
        '[role="dialog"] [role="option"], [role="dialog"] li, [role="dialog"] [data-suggest]'
      );
      await suggestion.first().click({ timeout: 5_000 });
      await page.waitForTimeout(2_500);
      this.log("configured Ozon region was applied");
    } catch (error) {
      this.log(
        `could not set Ozon region automatically (${String(error?.message || error).slice(0, 80)}); ` +
          "the persistent profile keeps its existing region, so verify it manually if it matters."
      );
    }
  }

  async _fetchFromPage(ticket, path, signal) {
    if (!isUsable(ticket)) throw new Error("Browser session is not available");
    throwIfAborted(signal);
    const url = this.apiUrl + encodeURIComponent(path);
    return this._withAbort(
      ticket.page.evaluate(
        boundedPageFetch,
        { requestUrl: url, timeoutMs: this.fetchTimeoutMs, maxBytes: this.maxResponseBytes }
      ),
      signal,
      () => this._closeTicket(ticket)
    );
  }

  _withAbort(promise, signal, onAbort) {
    if (!signal) return promise;
    if (signal.aborted) {
      // page.evaluate() may already be running when the signal flips. Keep
      // its later rejection observed even though cancellation wins this call.
      void Promise.resolve(promise).catch(noop);
      void onAbort?.();
      return Promise.reject(abortReason(signal));
    }

    let removeAbortListener = noop;
    const aborted = new Promise((_, reject) => {
      const abort = () => {
        void onAbort?.();
        reject(abortReason(signal));
      };
      signal.addEventListener("abort", abort, { once: true });
      removeAbortListener = () => signal.removeEventListener("abort", abort);
    });
    return Promise.race([promise, aborted]).finally(removeAbortListener);
  }

  _closeTicket(ticket) {
    if (!ticket) return Promise.resolve();
    if (ticket.closePromise) return ticket.closePromise;

    ticket.closed = true;
    if (this.current === ticket) this.current = null;
    if (this.initializing?.ticket === ticket) this.initializing = null;

    ticket.closePromise = (async () => {
      let context = ticket.context;
      if (!context && ticket.launchPromise) {
        try {
          context = await ticket.launchPromise;
          ticket.context = context;
        } catch {
          return;
        }
      }
      try {
        await context?.close();
      } catch {
        // A disconnected browser is already closed from our perspective.
      }
    })();
    this.retiring.add(ticket.closePromise);
    ticket.closePromise.then(
      () => this.retiring.delete(ticket.closePromise),
      () => this.retiring.delete(ticket.closePromise)
    );
    return ticket.closePromise;
  }

  async _waitForRetiringTickets() {
    const settled = Promise.all([...this.retiring]).then(noop);
    if (this.shutdownTimeoutMs === 0) {
      await settled;
      return;
    }
    if (await this._settlesWithin(settled, this.shutdownTimeoutMs)) {
      return;
    }
    throw new Error(`Previous browser shutdown exceeded ${this.shutdownTimeoutMs}ms`);
  }

  _settlesWithin(promise, timeoutMs) {
    return new Promise(resolve => {
      let finished = false;
      const finish = value => {
        if (finished) return;
        finished = true;
        clearTimeout(timer);
        resolve(value);
      };
      const timer = setTimeout(() => finish(false), timeoutMs);
      timer.unref?.();
      Promise.resolve(promise).then(
        () => finish(true),
        () => finish(true)
      );
    });
  }

  _clearIdleTimer() {
    if (this.idleTimer) clearTimeout(this.idleTimer);
    this.idleTimer = null;
  }

  _scheduleIdleShutdown() {
    this._clearIdleTimer();
    if (this.activeRequests !== 0 || !this.current || this.idleTimeoutMs === 0) return;
    this.idleTimer = setTimeout(() => {
      this.idleTimer = null;
      if (this.activeRequests === 0) {
        this.log("browser idle timeout reached; closing the persistent context");
        void this.shutdown();
      }
    }, this.idleTimeoutMs);
    this.idleTimer.unref?.();
  }
}
