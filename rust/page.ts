// Runs inside the Ozon tab. No account, address, or order widgets leave the page.
type JsonRecord = Record<string, unknown>;
type PageOptions =
  | { mode: "widgets" }
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
  "webListReviews",
]);
const searchWidgetNames = new Set([
  "filtersDesktop", "searchResultsSort", "searchResultsFiltersActive",
  "infiniteVirtualPaginator", "categoryBrandList",
]);
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
      else if (searchWidgetNames.has(name)) {
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

  if (options.mode === "widgets") {
    const widgetStates: JsonRecord = Object.create(null) as JsonRecord;
    let bytes = 0;
    for (const element of document.querySelectorAll<HTMLElement>(
      '[id^="state-"][data-state]',
    )) {
      const key = element.id.slice(6);
      if (!publicWidgetNames.has(widgetName(key)) && !searchWidgetNames.has(widgetName(key))) continue;
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
