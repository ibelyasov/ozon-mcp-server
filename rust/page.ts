// Runs inside the Ozon tab. No account, address, or order widgets leave the page.
type JsonRecord = Record<string, unknown>;
type PageOptions =
  | { mode: "widgets" }
  | { mode: "context" }
  | { mode: "contextModal" }
  | { mode: "navigation"; target: "home" | "addressBook" }
  | { mode: "fetch"; path: string };
type PageError =
  | "CAPTCHA_OR_BLOCKED"
  | "FETCH_FAILED"
  | "FETCH_TIMEOUT"
  | "INVALID_OPTIONS"
  | "INVALID_ORIGIN"
  | "INVALID_RESPONSE"
  | "RESPONSE_TOO_LARGE";

interface FilteredPage {
  widgetStates: JsonRecord;
  seo?: {
    title: string | null;
    link: Array<{ href: string }>;
  };
  layoutTrackingInfo?: { sku: unknown };
  contextObservation?: {
    regionLabel: string | null;
    regionVerified: boolean;
    accountState: "authenticated" | "anonymous" | "unknown";
    accessState: "available" | "unknown";
    signature: string | null;
  };
  regionProbe?: {
    addressBookModalAvailable: boolean;
    selectedRegionLabel: string | null;
  };
  navigationProbe?: {
    routeValid: boolean;
    status: number | null;
  };
}

type PageResult =
  | { page: FilteredPage }
  | { error: PageError }
  | { status: number };

const publicWidgetNames = new Set([
  "tileGridDesktop",
  "webShortCharacteristics",
  "webSingleProductScore",
  "webReviewProductScore",
  "webCurrentSeller",
  "webDescription",
  "webIconWithText",
  "webProductHeading",
  "webPrice",
  "webGallery",
]);
const searchWidgetNames = new Set([
  "filtersDesktop", "searchResultsSort", "searchResultsFiltersActive",
  "infiniteVirtualPaginator", "categoryBrandList",
]);
const sourceWidgetNames = new Set(["webAspects", "webListReviews"]);
const filterTypes = new Set([
  "categoryFilter", "boolFilter", "checkboxesFilter", "rangeFilter",
  "multipleRangesFilter", "colorFilter",
]);

// Every level is projected explicitly: optional widgets must never export
// tracking, experiments, search history, or executable composer actions.
function scalars(value: JsonRecord, keys: string[]): JsonRecord {
  const result: JsonRecord = {};
  for (const key of keys) {
    const field = value[key];
    if (typeof field === "string" || typeof field === "boolean" ||
        (typeof field === "number" && Number.isFinite(field))) result[key] = field;
  }
  return result;
}

function records(value: unknown): JsonRecord[] {
  return Array.isArray(value) ? value.filter((v) => isRecord(v) && !Array.isArray(v)) : [];
}

function display(value: JsonRecord, result: JsonRecord): void {
  for (const key of ["title", "description"]) {
    if (typeof value[key] === "string") result[key] = value[key];
    else if (isRecord(value[key]) && typeof value[key].text === "string") {
      result[key] = { text: value[key].text };
    }
  }
}

function filterFields(value: JsonRecord, depth = 0): JsonRecord {
  const result = scalars(value, ["isSelected", "isActive", "isRadio", "hasManyValues",
    "minValue", "maxValue", "fromValue", "toValue"]);
  display(value, result);
  if (Array.isArray(value.categories)) result.categories = records(value.categories).map((v) => {
    const item = scalars(v, ["isActive", "level", "urlValue"]);
    display(v, item);
    return item;
  });
  if (Array.isArray(value.sections)) result.sections = records(value.sections).map((section) => ({
    items: records(section.items).map((v) => {
      const item = scalars(v, ["key", "isSelected"]);
      display(v, item);
      return item;
    }),
  }));
  if (Array.isArray(value.colorIcons)) result.colorIcons = records(value.colorIcons).map((v) => {
    const item = scalars(v, ["key", "isSelected"]);
    display(v, item);
    return item;
  });
  if (isRecord(value.openingButtons)) {
    const buttons: JsonRecord = {};
    for (const key of ["showAllButton", "hideAllButton"]) {
      if (isRecord(value.openingButtons[key])) buttons[key] = {};
    }
    result.openingButtons = buttons;
  }
  if (depth === 0) {
    for (const key of ["rangeFilter", "checkboxesFilter"]) {
      if (isRecord(value[key])) result[key] = filterFields(value[key], depth + 1);
    }
  }
  return result;
}

function linkedItem(value: JsonRecord, keys: string[]): JsonRecord {
  const result = scalars(value, keys);
  if (isRecord(value.action) && typeof value.action.link === "string") {
    result.action = { link: value.action.link };
  }
  return result;
}

function projectSearchWidget(name: string, raw: unknown): JsonRecord | undefined {
  let value: unknown = raw;
  try { if (typeof raw === "string") value = JSON.parse(raw); } catch { return undefined; }
  if (!isRecord(value) || Array.isArray(value)) return undefined;
  switch (name) {
    case "filtersDesktop":
      if (!Array.isArray(value.sections)) return undefined;
      return { sections: records(value.sections).map((section) => ({
        filters: records(section.filters).flatMap((filter) => {
          const type = filter.type;
          if (typeof type !== "string" || !filterTypes.has(type) ||
              !isRecord(filter[type]) || Array.isArray(filter[type])) return [];
          return [{ ...scalars(filter, ["type", "key"]), [type]: filterFields(filter[type]) }];
        }),
      })) };
    case "searchResultsSort":
      if (!isRecord(value.sortButton) || !Array.isArray(value.sortButton.options)) return undefined;
      return { sortButton: { options: records(value.sortButton.options)
        .map((v) => linkedItem(v, ["name", "isSelected"])) } };
    case "searchResultsFiltersActive":
      if (!Array.isArray(value.activeFilters)) return undefined;
      return { activeFilters: records(value.activeFilters).map((v) => {
        const item = scalars(v, ["key", "name", "ftype"]);
        if (Array.isArray(v.activeValues)) {
          item.activeValues = records(v.activeValues)
            .map((entry) => scalars(entry, ["title", "disableUri"]));
        }
        return item;
      }) };
    case "infiniteVirtualPaginator":
      return scalars(value, ["nextPage", "prevPage", "size", "layoutContainer", "fetchType"]);
    case "categoryBrandList":
      if (!Array.isArray(value.brands)) return undefined;
      return { brands: records(value.brands).map((v) => linkedItem(v, ["text"])) };
    case "webAspects":
      if (!Array.isArray(value.aspects)) return undefined;
      return { aspects: records(value.aspects).map((aspect) => {
        const item = scalars(aspect, ["aspectKey", "aspectName", "type"]);
        if (Array.isArray(aspect.descriptionRs)) {
          item.descriptionRs = records(aspect.descriptionRs)
            .map((description) => scalars(description, ["content", "type"]));
        }
        if (Array.isArray(aspect.variants)) {
          item.variants = records(aspect.variants).map((variant) => {
            const projected = scalars(variant, ["availability", "link", "price", "sku"]);
            if (isRecord(variant.data)) {
              projected.data = scalars(variant.data,
                ["title", "text", "name", "value", "isSelected", "selected"]);
            }
            return projected;
          });
        }
        return item;
      }) };
    case "webListReviews": {
      if (!Array.isArray(value.reviews) && !Array.isArray(value.items)) return undefined;
      const rawReviews = records(Array.isArray(value.reviews) ? value.reviews : value.items);
      const result = scalars(value,
        ["itemId", "requestedPath", "fullRequestUrl", "productScore", "pageType"]);
      if (isRecord(value.paging)) {
        const paging = scalars(value.paging,
          ["page", "perPage", "total", "commonTotal", "nextButton", "prevButton"]);
        if (Array.isArray(value.paging.links)) {
          paging.links = records(value.paging.links)
            .map((link) => scalars(link, ["text", "urlParams"]));
        }
        result.paging = paging;
      }
      if (isRecord(value.filters)) {
        result.filters = scalars(value.filters, ["withMedia", "withPhotos"]);
      }
      if (Array.isArray(value.sortings)) {
        result.sortings = records(value.sortings)
          .map((sorting) => scalars(sorting, ["active", "name", "value"]));
      }
      result.reviews = rawReviews.map((review) => {
        const item = scalars(review, ["uuid", "itemId", "publishedAt", "createdAt",
          "isItemPurchased", "showVariantImage", "isAnonymous"]);
        if (isRecord(review.author)) {
          item.author = scalars(review.author, ["firstName", "lastName", "fio"]);
        }
        if (isRecord(review.content)) {
          const content = scalars(review.content,
            ["score", "comment", "positive", "negative"]);
          if (Array.isArray(review.content.photos)) {
            content.photos = review.content.photos.map((photo) => isRecord(photo)
              ? scalars(photo, ["src", "url", "link", "image", "previewUrl", "originalUrl"])
              : typeof photo === "string" ? photo : {}).filter((photo) =>
                typeof photo === "string" || Object.keys(photo).length > 0);
          }
          item.content = content;
        }
        if (isRecord(review.usefulness)) {
          item.usefulness = scalars(review.usefulness, ["useful", "unuseful"]);
        }
        if (isRecord(review.status)) item.status = scalars(review.status, ["id", "name"]);
        return item;
      });
      if (isRecord(value.products)) {
        result.productsCount = Object.keys(value.products)
          .filter((sku) => /^[0-9]+$/.test(sku)).length;
        const itemIds = new Set(rawReviews.map((review) => String(review.itemId ?? "")));
        result.products = Object.fromEntries(Object.entries(value.products).flatMap(([sku, product]) => {
          if (!/^[0-9]+$/.test(sku) || !isRecord(product) ||
              (!itemIds.has(sku) && !itemIds.has(String(product.itemId ?? "")))) return [];
          const projected = scalars(product, ["itemId", "name", "uri"]);
          if (Array.isArray(product.variants)) {
            projected.variants = records(product.variants)
              .map((variant) => scalars(variant, ["name", "value"]));
          }
          return [[sku, projected]];
        }));
      }
      return result;
    }
    default: return undefined;
  }
}

const maxBytes = 4 * 1024 * 1024;

function isRecord(value: unknown): value is JsonRecord {
  return typeof value === "object" && value !== null;
}

function widgetName(key: string): string {
  return key.split("-")[0] ?? "";
}

function parseOptions(value: unknown): PageOptions | null {
  if (!isRecord(value)) return null;
  if (value.mode === "widgets") return { mode: "widgets" };
  if (value.mode === "context") return { mode: "context" };
  if (value.mode === "contextModal") return { mode: "contextModal" };
  if (value.mode === "navigation" &&
      (value.target === "home" || value.target === "addressBook")) {
    return { mode: "navigation", target: value.target };
  }
  if (value.mode === "fetch" && typeof value.path === "string") {
    return { mode: "fetch", path: value.path };
  }
  return null;
}

function filterPage(page: unknown): PageResult {
  if (!isRecord(page)) return { error: "INVALID_RESPONSE" };

  const widgetStates: JsonRecord = Object.create(null) as JsonRecord;
  if (isRecord(page.widgetStates)) {
    for (const [key, value] of Object.entries(page.widgetStates)) {
      const name = widgetName(key);
      if (publicWidgetNames.has(name)) widgetStates[key] = value;
      else if (searchWidgetNames.has(name) || sourceWidgetNames.has(name)) {
        const projected = projectSearchWidget(name, value);
        if (projected !== undefined) widgetStates[key] = projected;
      }
    }
  }

  const result: FilteredPage = { widgetStates };
  if (isRecord(page.seo)) {
    result.seo = {
      title: typeof page.seo.title === "string" ? page.seo.title : null,
      link: Array.isArray(page.seo.link)
        ? page.seo.link
            .filter(
              (entry): entry is JsonRecord & { href: string } =>
                isRecord(entry) && typeof entry.href === "string",
            )
            .map((entry) => ({ href: entry.href }))
        : [],
    };
  }

  try {
    const tracking: unknown =
      typeof page.layoutTrackingInfo === "string"
        ? JSON.parse(page.layoutTrackingInfo)
        : page.layoutTrackingInfo;
    if (isRecord(tracking) && /^[0-9]+$/.test(String(tracking.sku))) {
      result.layoutTrackingInfo = { sku: tracking.sku };
    }
  } catch {
    // Invalid tracking metadata is optional and intentionally omitted.
  }

  if (new TextEncoder().encode(JSON.stringify(result)).length > maxBytes) {
    return { error: "RESPONSE_TOO_LARGE" };
  }
  return { page: result };
}

async function ozonPage(rawOptions: unknown): Promise<PageResult> {
  if (location.origin !== "https://www.ozon.ru") {
    return { error: "INVALID_ORIGIN" };
  }
  const options = parseOptions(rawOptions);
  if (options === null) return { error: "INVALID_OPTIONS" };

  if (options.mode === "navigation") {
    const current = new URL(location.href);
    const responseQuery = current.search === "" || /^\?__rr=[0-9]{1,16}$/.test(current.search);
    const expectedPath = options.target === "home" ? "/" : "/modal/addressbook";
    const routeValid = current.origin === "https://www.ozon.ru" &&
      current.protocol === "https:" && current.username === "" && current.password === "" &&
      current.port === "" && current.pathname === expectedPath && responseQuery &&
      current.hash === "";
    const navigation = performance.getEntriesByType("navigation")[0] as
      PerformanceNavigationTiming | undefined;
    const status = navigation !== undefined && Number.isSafeInteger(navigation.responseStatus) &&
      navigation.responseStatus >= 0 ? navigation.responseStatus : null;
    return { page: { widgetStates: {}, navigationProbe: { routeValid, status } } };
  }

  if (options.mode === "contextModal") {
    const raw = document.querySelector('[id^="state-commonAddressBook-"][data-state]')
      ?.getAttribute("data-state");
    let selectedRegionLabel: string | null = null;
    try {
      const value: unknown = raw === undefined || raw === null ? null : JSON.parse(raw);
      if (isRecord(value) && Array.isArray(value.addresses)) {
        const selected = records(value.addresses)
          .filter((address) => address.isSelected === true);
        const selectedAddress = selected.length === 1 ? selected[0] : undefined;
        const label = selectedAddress !== undefined && Array.isArray(selectedAddress.elements) &&
          isRecord(selectedAddress.elements[0]) &&
          typeof selectedAddress.elements[0].text === "string"
          ? selectedAddress.elements[0].text : "";
        const comma = label.indexOf(",");
        const city = comma > 0
          ? label.slice(0, comma).split(/\s+/u).filter(Boolean).join(" ") : "";
        selectedRegionLabel = city.length <= 100 && /\p{L}/u.test(city) &&
          /^[\p{L} -]+$/u.test(city) &&
          !/(?:адрес|улиц|дом|квартир|подъезд|этаж|достав|пункт|выдач|укажите|сегодня|завтра|послезавтра)/iu.test(city)
          ? city : null;
      }
    } catch {
      // A malformed or changed private widget never crosses the page boundary.
    }
    return { page: {
      widgetStates: {},
      regionProbe: { addressBookModalAvailable: false, selectedRegionLabel },
    } };
  }

  if (options.mode === "context") {
    // Keep this read-only and narrow. A location control may contain a street
    // address, so its text is never exported until a city-only signal is proven.
    const visible = (element: Element | null): boolean => {
      if (!(element instanceof HTMLElement)) return false;
      const style = getComputedStyle(element);
      return style.display !== "none" && style.visibility !== "hidden";
    };
    const state = (name: string): JsonRecord | null => {
      const raw = document.querySelector(`[id^="state-${name}-"][data-state]`)
        ?.getAttribute("data-state");
      if (raw === undefined || raw === null) return null;
      try {
        const value: unknown = JSON.parse(raw);
        return isRecord(value) && !Array.isArray(value) ? value : null;
      } catch { return null; }
    };
    const address = state("addressBookBarWeb");
    const modalLink = isRecord(address?.customCell) && isRecord(address.customCell.action) &&
      typeof address.customCell.action.link === "string"
      ? address.customCell.action.link : null;
    let addressBookModalAvailable = false;
    if (modalLink !== null) {
      try {
        const target = new URL(modalLink, location.origin);
        const observedShellFlag = target.search === "?set_sm=1";
        addressBookModalAvailable = target.origin === location.origin &&
          target.protocol === "https:" && target.username === "" && target.password === "" &&
          target.port === "" && target.pathname === "/modal/addressbook" &&
          (target.search === "" || observedShellFlag) && target.hash === "";
      } catch {
        // Changed or malformed navigation is unavailable.
      }
    }
    const cityValue = address?.customCell;
    const city = isRecord(cityValue) && Array.isArray(cityValue.cells) &&
      isRecord(cityValue.cells[0]) && isRecord(cityValue.cells[0].button) &&
      typeof cityValue.cells[0].button.text === "string"
      ? cityValue.cells[0].button.text.split(/\s+/u).filter(Boolean).join(" ") : "";
    // This exact first-cell field was observed as the city on home, product and
    // review pages. Reject address-like or structurally ambiguous text.
    const anonymousRegionLabel = city.length <= 100 && /^[\p{L} -]+$/u.test(city) &&
      !/(?:адрес|улиц|дом|квартир|подъезд|этаж|достав|укажите)/iu.test(city)
      ? city : null;
    const anonymous = document.querySelector('[id^="state-profileMenuAnonymous-"][data-state]') !== null ||
      visible(document.querySelector('a[href^="/login"], button[aria-label="Войти"]'));
    // Presence distinguishes the signed-in header without reading profile
    // state, which may contain identity data.
    const authenticated = !anonymous &&
      document.querySelector('[id^="state-profileMenu-"][data-state]') !== null;
    const publicState = Array.from(document.querySelectorAll<HTMLElement>('[id^="state-"][data-state]'))
      .some((element) => {
        const name = widgetName(element.id.slice(6));
        return publicWidgetNames.has(name) || searchWidgetNames.has(name) ||
          sourceWidgetNames.has(name);
      });
    const accountState = anonymous ? "anonymous" : authenticated ? "authenticated" : "unknown";
    // In the authenticated layout this cell contains delivery timing rather
    // than a city, while the adjacent cell contains a private street address.
    // Only the observed anonymous layout gives this field city semantics.
    const regionLabel = accountState === "anonymous" ? anonymousRegionLabel : null;
    const indicatorText = regionLabel !== null || accountState !== "unknown"
      ? `${accountState}\n${regionLabel ?? ""}` : "";
    let signature: string | null = null;
    if (indicatorText.length > 0) {
      try {
        const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(indicatorText));
        signature = Array.from(new Uint8Array(digest),
          (byte) => byte.toString(16).padStart(2, "0")).join("");
      } catch {
        // Context stays observable but unbound if hashing is unavailable.
      }
    }
    const page: FilteredPage = {
      widgetStates: {},
      contextObservation: {
        regionLabel,
        regionVerified: regionLabel !== null,
        accountState,
        accessState: anonymous || authenticated || publicState ? "available" : "unknown",
        signature,
      },
    };
    if (accountState === "authenticated" && addressBookModalAvailable) {
      page.regionProbe = { addressBookModalAvailable: true, selectedRegionLabel: null };
    }
    return { page };
  }

  if (options.mode === "widgets") {
    const widgetStates: JsonRecord = Object.create(null) as JsonRecord;
    let bytes = 0;
    for (const element of document.querySelectorAll<HTMLElement>(
      '[id^="state-"][data-state]',
    )) {
      const key = element.id.slice(6);
      if (!publicWidgetNames.has(widgetName(key)) && !searchWidgetNames.has(widgetName(key)) &&
          !sourceWidgetNames.has(widgetName(key))) continue;
      const value = element.getAttribute("data-state");
      if (value === null) continue;
      bytes += new TextEncoder().encode(value).length;
      if (bytes > maxBytes) return { error: "RESPONSE_TOO_LARGE" };
      widgetStates[key] = value;
    }
    return Object.keys(widgetStates).length > 0
      ? filterPage({ widgetStates })
      : { error: "CAPTCHA_OR_BLOCKED" };
  }

  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 35_000);
  try {
    const response = await fetch(
      "/api/composer-api.bx/page/json/v2?url=" +
        encodeURIComponent(options.path),
      {
        headers: { accept: "application/json" },
        signal: controller.signal,
      },
    );
    if (!response.ok) return { status: response.status };
    if (Number(response.headers.get("content-length")) > maxBytes) {
      return { error: "RESPONSE_TOO_LARGE" };
    }
    if (response.body === null) return { error: "INVALID_RESPONSE" };

    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let bytes = 0;
    let text = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      bytes += value.length;
      if (bytes > maxBytes) {
        controller.abort();
        return { error: "RESPONSE_TOO_LARGE" };
      }
      text += decoder.decode(value, { stream: true });
    }
    try {
      return filterPage(JSON.parse(text + decoder.decode()) as unknown);
    } catch {
      return { error: "INVALID_RESPONSE" };
    }
  } catch (error: unknown) {
    return {
      error:
        isRecord(error) && error.name === "AbortError"
          ? "FETCH_TIMEOUT"
          : "FETCH_FAILED",
    };
  } finally {
    clearTimeout(timer);
  }
}
