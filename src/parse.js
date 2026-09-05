// Pure parsers over Ozon's composer-api JSON. No browser, no network here —
// every function takes a parsed composer-api response object and returns
// plain data. Kept side-effect-free so it can be unit-tested against saved
// samples.

const OZON_ORIGIN = "https://www.ozon.ru";

function isRecord(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function asArray(value) {
  return Array.isArray(value) ? value : [];
}

function text(value) {
  return typeof value === "string" ? value.replace(/\s+/g, " ").trim() || null : null;
}

function textFrom(value) {
  if (typeof value === "string") return text(value);
  if (!isRecord(value)) return null;
  return text(value.text) ?? text(value.content);
}

function optionalString(record, key) {
  return isRecord(record) && typeof record[key] === "string" ? record[key] : null;
}

function booleanOrNull(value) {
  return typeof value === "boolean" ? value : null;
}

function primitiveOrNull(value) {
  if (typeof value === "string") return value.trim() || null;
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function parseJsonValue(value) {
  if (isRecord(value) || Array.isArray(value)) return value;
  if (typeof value !== "string") return null;
  try {
    return JSON.parse(value);
  } catch {
    return null;
  }
}

function parseWidgetState(value) {
  const parsed = parseJsonValue(value);
  return isRecord(parsed) ? parsed : null;
}

function widgetName(key) {
  return typeof key === "string" ? key.split("-")[0] : "";
}

function widget(page, name) {
  const states = isRecord(page?.widgetStates) ? page.widgetStates : {};
  for (const key of Object.keys(states)) {
    if (widgetName(key) !== name) continue;
    const parsed = parseWidgetState(states[key]);
    if (parsed) return parsed;
  }
  return null;
}

function widgets(page, name) {
  const states = isRecord(page?.widgetStates) ? page.widgetStates : {};
  const parsed = [];
  for (const key of Object.keys(states)) {
    if (widgetName(key) !== name) continue;
    const value = parseWidgetState(states[key]);
    if (value) parsed.push(value);
  }
  return parsed;
}

function validGroupedInteger(value) {
  if (/^\d+$/.test(value)) return true;
  const separators = value.match(/[.,]/g) || [];
  if (new Set(separators).size !== 1) return false;
  const groups = value.split(separators[0]);
  return (
    groups.length > 1 &&
    /^\d{1,3}$/.test(groups[0]) &&
    groups.slice(1).every((group) => /^\d{3}$/.test(group))
  );
}

function parseLocalizedNumber(value) {
  if (typeof value !== "string") return null;
  const compact = value.replace(/[\s\u00a0\u202f]/g, "");
  if (!/^[+-]?\d(?:[\d.,]*\d)?$/.test(compact)) return null;

  const signed = compact[0] === "+" || compact[0] === "-";
  const sign = compact[0] === "-" ? -1 : 1;
  const body = signed ? compact.slice(1) : compact;
  const comma = body.lastIndexOf(",");
  const dot = body.lastIndexOf(".");
  let decimalIndex = -1;

  if (comma >= 0 && dot >= 0) {
    decimalIndex = Math.max(comma, dot);
  } else {
    const separator = comma >= 0 ? comma : dot;
    if (separator >= 0 && body.length - separator - 1 <= 2) decimalIndex = separator;
  }

  let normalized;
  if (decimalIndex >= 0) {
    const integerPart = body.slice(0, decimalIndex);
    const fractionPart = body.slice(decimalIndex + 1);
    if (!/^\d+$/.test(fractionPart) || !validGroupedInteger(integerPart)) return null;
    normalized = integerPart.replace(/[.,]/g, "") + "." + fractionPart;
  } else {
    if (!validGroupedInteger(body)) return null;
    normalized = body.replace(/[.,]/g, "");
  }

  const parsed = Number(normalized) * sign;
  return Number.isFinite(parsed) ? parsed : null;
}

function priceToNumber(value) {
  let parsed = typeof value === "number" && Number.isFinite(value) ? value : null;
  if (parsed === null && typeof value === "string") {
    const match = value.match(/[+-]?\d(?:[\d\s\u00a0\u202f.,]*\d)?/u);
    parsed = match ? parseLocalizedNumber(match[0]) : null;
  }
  return parsed !== null && parsed >= 0 ? parsed : null;
}

function scaledCount(number, multiplier = 1) {
  if (number === null || number < 0) return null;
  const scaled = number * multiplier;
  if (!Number.isFinite(scaled) || scaled > Number.MAX_SAFE_INTEGER) return null;
  return Math.round(scaled);
}

function countFromValue(value) {
  if (typeof value === "number" && Number.isFinite(value)) return scaledCount(value);
  if (typeof value !== "string") return null;

  const match = value.match(
    /^\s*([+-]?\d(?:[\d\s\u00a0\u202f.,]*\d)?)(?:\s*)(тыс(?:яч[аи])?\.?|млн\.?|миллион[а-яё]*|k|m)?\s*$/iu
  );
  if (!match) return null;
  const suffix = (match[2] || "").toLowerCase();
  const multiplier = /^(?:тыс|k)/u.test(suffix) ? 1_000 : /^(?:млн|миллион|m)/u.test(suffix) ? 1_000_000 : 1;
  return scaledCount(parseLocalizedNumber(match[1]), multiplier);
}

function parseReviewCount(value) {
  if (typeof value !== "string") return null;
  const matcher = /([+-]?\d(?:[\d\s\u00a0\u202f.,]*\d)?)(?:\s*)(тыс(?:яч[аи])?\.?|млн\.?|миллион[а-яё]*|k|m)?\s*(?:отзыв[а-яё]*|reviews?)/giu;
  for (const match of value.matchAll(matcher)) {
    const suffix = (match[2] || "").toLowerCase();
    const multiplier = /^(?:тыс|k)/u.test(suffix) ? 1_000 : /^(?:млн|миллион|m)/u.test(suffix) ? 1_000_000 : 1;
    const count = scaledCount(parseLocalizedNumber(match[1]), multiplier);
    if (count !== null) return count;
  }
  return null;
}

function ratingFromValue(value) {
  if (typeof value === "number") {
    return Number.isFinite(value) && value >= 0 && value <= 5 ? value : null;
  }
  if (isRecord(value)) return ratingFromValue(value.text) ?? ratingFromValue(value.value);
  if (typeof value !== "string") return null;

  const source = text(value);
  if (!source) return null;
  const direct = parseLocalizedNumber(source);
  if (direct !== null && direct >= 0 && direct <= 5) return direct;

  const explicit = source.match(
    /(?:^|[^\d])([0-5](?:[.,]\d+)?)(?![\d.,])\s*(?:из\s*5|\/\s*5|[★⭐]|звезд[а-яё]*)/iu
  );
  if (explicit) return ratingFromValue(explicit[1]);

  const atStart = source.match(/^([0-5](?:[.,]\d+)?)(?![\d.,])/u);
  if (!atStart) return null;
  const remaining = source.slice(atStart[0].length);
  if (
    /^\s*\d/u.test(remaining) ||
    /^\s*(?:(?:тыс(?:яч[аи])?\.?|млн\.?|миллион[а-яё]*|k|m)\s*)?(?:отзыв[а-яё]*|reviews?)/iu.test(
      remaining
    )
  ) {
    return null;
  }
  return ratingFromValue(atStart[1]);
}

function collectStrings(value, result = [], seen = new WeakSet()) {
  if (typeof value === "string") {
    result.push(value);
    return result;
  }
  if (value === null || typeof value !== "object" || seen.has(value)) return result;
  seen.add(value);
  for (const child of Array.isArray(value) ? value : Object.values(value)) {
    collectStrings(child, result, seen);
  }
  return result;
}

function hasStarIcon(value) {
  return collectStrings(value).some((item) => item.includes("ic_s_star"));
}

function normalizeSku(value) {
  if (typeof value === "number") {
    return Number.isSafeInteger(value) && value >= 0 ? String(value) : null;
  }
  const candidate = typeof value === "string" ? value.trim() : null;
  return candidate && /^\d+$/.test(candidate) ? candidate : null;
}

function cleanUrl(link) {
  if (typeof link !== "string" || !link.trim()) return null;
  try {
    const url = new URL(link.trim(), OZON_ORIGIN);
    if (!/^https?:$/i.test(url.protocol) || !/^(?:www\.)?ozon\.ru$/i.test(url.hostname)) {
      return null;
    }
    url.protocol = "https:";
    url.hostname = "www.ozon.ru";
    url.search = "";
    url.hash = "";
    return url.href;
  } catch {
    return null;
  }
}

function skuFromUrl(url) {
  const clean = cleanUrl(url);
  if (!clean) return null;
  const path = new URL(clean).pathname;
  const product = path.match(/\/product\/(?:[^/]*-)?(\d+)\/?$/iu);
  const terminal = path.match(/-(\d+)\/?$/u);
  return normalizeSku(product?.[1] ?? terminal?.[1]);
}

function imageUrl(value) {
  if (typeof value === "string") return value.trim() || null;
  if (!isRecord(value)) return null;
  return imageUrl(value.src) ?? imageUrl(value.link) ?? imageUrl(value.url);
}

// ── search ────────────────────────────────────────────────────────────────────

function parseSearchItem(item) {
  if (!isRecord(item)) return null;
  const states = asArray(item.mainState).filter(isRecord);

  const priceBlock = states.find((state) => state.type === "priceV2" && isRecord(state.priceV2))
    ?.priceV2;
  const prices = asArray(priceBlock?.price).filter(isRecord);
  const price = priceToNumber(prices.find((entry) => entry.textStyle === "PRICE")?.text);
  const oldPrice = priceToNumber(prices.find((entry) => entry.textStyle === "ORIGINAL_PRICE")?.text);

  const name = textFrom(states.find((state) => state.id === "name")?.textDS);

  let rating = null;
  let reviews = null;
  const ratingList = states.find((state) => hasStarIcon(state.labelListV2))?.labelListV2?.items;
  if (Array.isArray(ratingList)) {
    const labels = ratingList
      .filter((entry) => isRecord(entry) && entry.type === "text")
      .map((entry) => textFrom(entry.text))
      .filter(Boolean);
    rating = ratingFromValue(labels[0]);
    reviews = parseReviewCount(labels[1]) ?? countFromValue(labels[1]);
  }

  // Financial/reward labels also contain text; only accept explicit brand evidence.
  let brand = null;
  for (const state of states) {
    const labels = state.labelListV2;
    if (!isRecord(labels) || labels.testInfo?.automatizationId !== "tile-list-labels") continue;
    const texts = asArray(labels.items)
      .filter((entry) => isRecord(entry) && entry.type === "text")
      .map((entry) => textFrom(entry.text))
      .filter(Boolean);
    const verifiedIndex = texts.findIndex((itemText) => /^бренд проверен$/iu.test(itemText));
    if (verifiedIndex > 0) {
      brand = texts[verifiedIndex - 1];
      break;
    }
  }

  const url = cleanUrl(item.action?.link);
  const sku = normalizeSku(item.sku) ?? normalizeSku(item.id) ?? skuFromUrl(url);
  const tileImage = isRecord(item.tileImage) ? item.tileImage : null;
  const image =
    asArray(tileImage?.items)
      .filter(isRecord)
      .map((entry) => imageUrl(entry.image))
      .find(Boolean) ?? imageUrl(tileImage?.coverImage);

  if (!sku || price === null) return null;
  return {
    sku,
    name,
    price,
    oldPrice: oldPrice !== null && oldPrice > price ? oldPrice : null,
    discount: primitiveOrNull(priceBlock?.discount),
    rating,
    reviews,
    brand,
    url,
    image,
  };
}

function normalizedLimit(value, fallback) {
  if (typeof value !== "number" || !Number.isFinite(value)) return fallback;
  return Math.max(0, Math.floor(value));
}

export function parseSearch(page, limit = 12) {
  const grid = widget(page, "tileGridDesktop");
  const items = asArray(grid?.items)
    .map(parseSearchItem)
    .filter(Boolean)
    .slice(0, normalizedLimit(limit, 12));
  return { count: items.length, items };
}

// ── product details ───────────────────────────────────────────────────────────

function rsText(value) {
  const pieces = [];
  for (const item of Array.isArray(value) ? value : [value]) {
    const piece = typeof item === "string" ? text(item) : textFrom(item);
    if (piece) pieces.push(piece);
  }
  return pieces.join(" ").replace(/\s+/g, " ").trim();
}

function firstRsText(...values) {
  for (const value of values) {
    const parsed = rsText(value);
    if (parsed) return parsed;
  }
  return null;
}

function parseShortCharacteristics(page) {
  const state = widget(page, "webShortCharacteristics");
  const out = {};
  for (const characteristic of asArray(state?.characteristics).filter(isRecord)) {
    const title = firstRsText(characteristic.title?.textRs, characteristic.title?.text, characteristic.title);
    const value = firstRsText(characteristic.values, characteristic.contentRS, characteristic.valueRs);
    if (title && value) {
      // Defining an own property avoids the legacy __proto__ setter on plain objects.
      Object.defineProperty(out, title, { configurable: true, enumerable: true, value, writable: true });
    }
  }
  return out;
}

function parseProductScore(page) {
  const state = widget(page, "webSingleProductScore") || widget(page, "webReviewProductScore");
  if (!state) return { rating: null, reviews: null };

  const ratingCandidates = [state.rating, state.ratingValue, state.text, state.title?.text];
  let rating = null;
  for (const candidate of ratingCandidates) {
    rating = ratingFromValue(candidate);
    if (rating !== null) break;
  }

  const countCandidates = [state.reviews, state.reviewCount, state.reviewsCount, state.totalReviews];
  let reviews = null;
  for (const candidate of countCandidates) {
    reviews = countFromValue(candidate) ?? parseReviewCount(candidate);
    if (reviews !== null) break;
  }
  if (reviews === null) {
    for (const candidate of collectStrings(state)) {
      reviews = parseReviewCount(candidate);
      if (reviews !== null) break;
    }
  }
  return { rating, reviews };
}

function parseSeller(page) {
  const state = widget(page, "webCurrentSeller");
  if (!state) return null;
  const name = textFrom(state.sellerCell?.centerBlock?.title) ?? textFrom(state.title);
  if (!name) return null;
  const rating =
    ratingFromValue(state.rating?.title?.text) ??
    ratingFromValue(state.rating?.title) ??
    ratingFromValue(state.rating);
  const url = cleanUrl(state.sellerCell?.common?.action?.link);
  return { name, rating, url };
}

function decodeHtmlEntities(value) {
  return value.replace(/&(#x[0-9a-f]+|#\d+|nbsp|amp|lt|gt|quot|apos);/giu, (entity, code) => {
    const named = { nbsp: " ", amp: "&", lt: "<", gt: ">", quot: '"', apos: "'" };
    if (!code.startsWith("#")) return named[code.toLowerCase()] || entity;
    const number = code[1].toLowerCase() === "x" ? parseInt(code.slice(2), 16) : Number(code.slice(1));
    return number > 0 && number <= 0x10ffff && (number < 0xd800 || number > 0xdfff)
      ? String.fromCodePoint(number)
      : entity;
  });
}

function descriptionText(html) {
  if (typeof html !== "string") return "";
  return text(
    decodeHtmlEntities(
      html
        .replace(/<(script|style)\b[^>]*>[\s\S]*?<\/\1\s*>/giu, " ")
        .replace(/<!--[\s\S]*?-->/gu, " ")
        .replace(/<\/?[a-z][^>]*>/giu, " ")
    )
  ) || "";
}

function descriptionImages(html) {
  if (typeof html !== "string") return [];
  const images = [];
  const matcher = /<img\b[^>]*\ssrc\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>\x60]+))/giu;
  for (const match of html.matchAll(matcher)) {
    const source = imageUrl(match[1] ?? match[2] ?? match[3]);
    if (source) images.push(source);
  }
  return images;
}

function walkDescription(value, texts, images, seen = new WeakSet()) {
  if (Array.isArray(value)) {
    for (const item of value) walkDescription(item, texts, images, seen);
    return;
  }
  if (!isRecord(value) || seen.has(value)) return;
  seen.add(value);

  if (value.type === "text") {
    const content = text(value.content);
    if (content) texts.push(content);
  }
  const image =
    imageUrl(value.img?.src) ??
    imageUrl(value.image?.src) ??
    (value.type === "image" ? imageUrl(value.src) ?? imageUrl(value.attrs?.src) : null);
  if (image) images.push(image);

  for (const child of Object.values(value)) {
    if (child && typeof child === "object") walkDescription(child, texts, images, seen);
  }
}

export function parseDescription(page2) {
  const texts = [];
  const images = [];
  for (const state of widgets(page2, "webDescription")) {
    const textStart = texts.length;
    const imageStart = images.length;
    const annotation = parseJsonValue(state.richAnnotationJson);
    if (isRecord(annotation) || Array.isArray(annotation)) {
      const root = isRecord(annotation) && Object.hasOwn(annotation, "content") ? annotation.content : annotation;
      walkDescription(root, texts, images);
    }

    if (typeof state.richAnnotation === "string") {
      if (texts.length === textStart) {
        const fallbackText = descriptionText(state.richAnnotation);
        if (fallbackText) texts.push(fallbackText);
      }
      if (images.length === imageStart) images.push(...descriptionImages(state.richAnnotation));
    }
  }
  return { text: texts.join(" ").trim(), images: [...new Set(images)] };
}

/**
 * Таможенная пошлина для импортных товаров. Ozon показывает её отдельным
 * блоком "webIconWithText" с href вида "/modal/customs-duty?product_id=...".
 * Этот же widget используется и для других "иконка + текст" (доставка,
 * гарантия, акции), поэтому отличаем именно по customs-duty или по слову
 * "пошлин" в тексте.
 */
function dutyAmount(value) {
  const matcher = /([+-]?\d(?:[\d\s\u00a0\u202f.,]*\d)?)\s*(?:₽|руб(?:\.|л[а-яё]*)?)/giu;
  for (const match of value.matchAll(matcher)) {
    const amount = priceToNumber(match[1]);
    if (amount !== null) return amount;
  }
  return null;
}

export function parseDuty(page) {
  for (const state of widgets(page, "webIconWithText")) {
    const strings = collectStrings(state);
    if (!strings.some((item) => /customs-duty|пошлин/iu.test(item))) continue;
    for (const item of strings) {
      if (/customs-duty|пошлин/iu.test(item)) {
        const amount = dutyAmount(item);
        if (amount !== null) return { amount, note: "пошлина не входит в цену" };
      }
    }
    const amount = dutyAmount(strings.join(" "));
    if (amount !== null) return { amount, note: "пошлина не входит в цену" };
  }
  return null;
}

function seoUrl(page) {
  for (const link of asArray(page?.seo?.link).filter(isRecord)) {
    const url = cleanUrl(link.href);
    if (url) return url;
  }
  return null;
}

export function parseDetails(basePage, page2) {
  const heading = widget(basePage, "webProductHeading");
  const price = widget(basePage, "webPrice");
  const gallery = widget(basePage, "webGallery");
  const tracking = parseJsonValue(basePage?.layoutTrackingInfo);
  const url = seoUrl(basePage);
  const sku = normalizeSku(gallery?.sku) ?? normalizeSku(tracking?.sku) ?? skuFromUrl(url);

  const { rating, reviews } = parseProductScore(basePage);
  const images = [];
  const addImage = (value) => {
    const source = imageUrl(value);
    if (source) images.push(source);
  };
  addImage(gallery?.coverImage);
  for (const image of asArray(gallery?.images)) {
    addImage(isRecord(image) ? image.src ?? image.image : image);
  }

  const priceCard = priceToNumber(price?.cardPrice) ?? priceToNumber(price?.price);
  const duty = parseDuty(basePage);

  return {
    sku,
    name: textFrom(heading?.title) ?? text(basePage?.seo?.title),
    url: url || (sku ? "https://www.ozon.ru/product/" + sku + "/" : null),
    price: priceCard,
    priceRegular: priceToNumber(price?.price),
    oldPrice: priceToNumber(price?.originalPrice),
    duty: duty
      ? {
          amount: duty.amount,
          total: priceCard !== null ? priceCard + duty.amount : null,
          note: duty.note,
        }
      : null,
    available: booleanOrNull(price?.isAvailable),
    rating,
    reviews,
    seller: parseSeller(basePage),
    images: [...new Set(images)].slice(0, 10),
    characteristics: parseShortCharacteristics(basePage),
    description: parseDescription(page2),
  };
}

// ── reviews ───────────────────────────────────────────────────────────────────

function unixToDate(value) {
  const seconds =
    typeof value === "number" && Number.isFinite(value)
      ? value
      : typeof value === "string" && /^\d+(?:\.\d+)?$/.test(value.trim())
        ? Number(value)
        : null;
  if (seconds === null || seconds < 0) return null;
  const date = new Date(seconds * 1000);
  return Number.isNaN(date.getTime()) ? null : date.toISOString().slice(0, 10);
}

function authorName(author) {
  if (!isRecord(author)) return null;
  const fullName = [text(author.firstName), text(author.lastName)].filter(Boolean).join(" ");
  return text(author.title) ?? (fullName || null);
}

export function parseReviews(page, limit = 10) {
  const state = widget(page, "webListReviews");
  const raw = Array.isArray(state?.reviews) ? state.reviews : asArray(state?.items);
  const { rating, reviews: total } = parseProductScore(page);
  const reviews = raw
    .filter(isRecord)
    .slice(0, normalizedLimit(limit, 10))
    .map((review) => {
      const content = isRecord(review.content) ? review.content : null;
      const author = authorName(review.author) ?? (review.isAnonymous === true ? "Аноним" : null);
      return {
        author,
        score: ratingFromValue(content?.score),
        comment: optionalString(content, "comment"),
        pros: optionalString(content, "positive"),
        cons: optionalString(content, "negative"),
        date: unixToDate(review.publishedAt ?? review.createdAt),
        useful: countFromValue(review.usefulness?.useful),
        purchased: booleanOrNull(review.isItemPurchased),
        hasPhotos: Array.isArray(content?.photos) ? content.photos.length > 0 : null,
      };
    });

  return { rating, totalReviews: total, count: reviews.length, reviews };
}

export const _internal = { priceToNumber, cleanUrl, skuFromUrl, widget, parseDuty };
