// Pure parsers over Ozon's composer-api JSON. No browser, no network here —
// every function takes a parsed composer-api response object and returns
// plain data. Kept side-effect-free so it can be unit-tested against saved
// samples.

function widgetName(key) {
  return String(key).split("-")[0];
}

function widget(page, name) {
  const ws = page?.widgetStates || {};
  const key = Object.keys(ws).find((k) => widgetName(k) === name);
  if (!key) return null;
  try {
    return JSON.parse(ws[key]);
  } catch {
    return null;
  }
}

function widgets(page, name) {
  const ws = page?.widgetStates || {};
  return Object.keys(ws)
    .filter((k) => widgetName(k) === name)
    .map((k) => {
      try {
        return JSON.parse(ws[k]);
      } catch {
        return null;
      }
    })
    .filter(Boolean);
}

function priceToNumber(text) {
  if (typeof text !== "string") return null;
  const digits = text.replace(/[^\d]/g, "");
  return digits ? parseInt(digits, 10) : null;
}

function cleanUrl(link) {
  if (!link) return null;
  const path = String(link).split("?")[0];
  return path.startsWith("http") ? path : `https://www.ozon.ru${path}`;
}

function skuFromUrl(url) {
  const m = String(url || "").match(/-(\d+)\/?(?:\?|$)/) || String(url || "").match(/(\d{6,})/);
  return m ? m[1] : null;
}

// ── search ────────────────────────────────────────────────────────────────────

function parseSearchItem(it) {
  if (!it) return null;
  const ms = Array.isArray(it.mainState) ? it.mainState : [];

  const priceBlock = ms.find((s) => s.type === "priceV2")?.priceV2;
  const prices = priceBlock?.price || [];
  const price = priceToNumber(prices.find((p) => p.textStyle === "PRICE")?.text);
  const oldPrice = priceToNumber(prices.find((p) => p.textStyle === "ORIGINAL_PRICE")?.text);

  const name = ms.find((s) => s.id === "name")?.textDS?.text || null;

  let rating = null;
  let reviews = null;
  const ratingList = ms.find(
    (s) => s.labelListV2 && JSON.stringify(s.labelListV2).includes("ic_s_star")
  )?.labelListV2?.items;
  if (Array.isArray(ratingList)) {
    const texts = ratingList.filter((x) => x.type === "text").map((x) => x.text?.text);
    if (texts[0]) rating = parseFloat(String(texts[0]).replace(",", "."));
    if (texts[1]) reviews = priceToNumber(texts[1]);
  }

  // Financial/reward labels also contain text; only accept explicit brand evidence.
  let brand = null;
  for (const state of ms) {
    const ll = state.labelListV2;
    if (!ll || ll.testInfo?.automatizationId !== "tile-list-labels") continue;
    const texts = (ll.items || []).filter((x) => x.type === "text")
      .map((x) => x.text?.text?.trim()).filter(Boolean);
    const verifiedIndex = texts.findIndex((text) => /^бренд проверен$/i.test(text));
    if (verifiedIndex > 0) {
      brand = texts[verifiedIndex - 1];
      break;
    }
  }

  const url = cleanUrl(it.action?.link);
  const sku = String(it.sku || it.id || skuFromUrl(url) || "") || null;

  const image =
    it.tileImage?.items?.find((x) => x.image?.link)?.image?.link ||
    it.tileImage?.coverImage ||
    null;

  if (!sku || !price) return null;
  return {
    sku,
    name,
    price,
    oldPrice: oldPrice && oldPrice > price ? oldPrice : null,
    discount: priceBlock?.discount || null,
    rating,
    reviews,
    brand,
    url,
    image,
  };
}

export function parseSearch(page, limit = 12) {
  const grid = widget(page, "tileGridDesktop");
  const raw = grid?.items || [];
  const items = raw.map(parseSearchItem).filter(Boolean).slice(0, limit);
  return { count: items.length, items };
}

// ── product details ───────────────────────────────────────────────────────────

function rsText(arr) {
  return (arr || [])
    .map((v) => v.text || v.content)
    .filter(Boolean)
    .join(" ")
    .replace(/\s+/g, " ")
    .trim();
}

function parseShortCharacteristics(page) {
  const w = widget(page, "webShortCharacteristics");
  const out = {};
  for (const c of w?.characteristics || []) {
    const title = rsText(c.title?.textRs) || (typeof c.title === "string" ? c.title : null);
    const value = rsText(c.values || c.contentRS || c.valueRs);
    if (title && value) out[title] = value;
  }
  return out;
}

function parseProductScore(page) {
  const w = widget(page, "webSingleProductScore") || widget(page, "webReviewProductScore");
  const text = w?.text || JSON.stringify(w || {});
  let rating = null;
  let reviews = null;
  const rm = text.match(/(\d[.,]\d)/);
  if (rm) rating = parseFloat(rm[1].replace(",", "."));
  const cm = text.match(/(\d[\d\s]*)\s*отзыв/);
  if (cm) reviews = priceToNumber(cm[1]);
  return { rating, reviews };
}

function parseSeller(page) {
  const w = widget(page, "webCurrentSeller");
  if (!w) return null;
  const name = w.sellerCell?.centerBlock?.title?.text || w.title?.text || null;
  const rating = parseFloat(String(w.rating?.title?.text || "").replace(",", ".")) || null;
  const url = cleanUrl(w.sellerCell?.common?.action?.link);
  if (!name) return null;
  return { name, rating, url };
}

function descriptionText(html) {
  return html.replace(/<(script|style)\b[^>]*>[\s\S]*?<\/\1>/gi, " ")
    .replace(/<[^>]*>/g, " ")
    .replace(/&(#x[0-9a-f]+|#\d+|nbsp|amp|lt|gt|quot|apos);/gi, (entity, code) => {
      const named = {nbsp: " ", amp: "&", lt: "<", gt: ">", quot: '"', apos: "'"};
      if (!code.startsWith("#")) return named[code.toLowerCase()] || entity;
      const n = code[1].toLowerCase() === "x" ? parseInt(code.slice(2), 16) : Number(code.slice(1));
      return n > 0 && n <= 0x10ffff ? String.fromCodePoint(n) : entity;
    }).replace(/\s+/g, " ").trim();
}

export function parseDescription(page2) {
  const texts = [];
  const images = [];
  const walk = (n) => {
    if (!n) return;
    if (Array.isArray(n)) return n.forEach(walk);
    if (typeof n !== "object") return;
    if (n.type === "text" && typeof n.content === "string") texts.push(n.content.replace(/\s+/g, " ").trim());
    if (typeof n.img?.src === "string") images.push(n.img.src);
    for (const value of Object.values(n)) if (value && typeof value === "object") walk(value);
  };
  for (const w of widgets(page2, "webDescription")) {
    let ra = w.richAnnotationJson;
    if (typeof ra === "string") {
      try { ra = JSON.parse(ra); } catch { ra = null; }
    }
    const before = texts.filter(Boolean).length + images.length;
    if (ra && typeof ra === "object") walk(ra.content || ra);
    if (texts.filter(Boolean).length + images.length === before && typeof w.richAnnotation === "string") {
      const html = w.richAnnotation;
      texts.push(descriptionText(html));
      for (const m of html.matchAll(/<img\b[^>]*\bsrc\s*=\s*["']([^"']+)["']/gi)) images.push(m[1]);
    }
  }
  return { text: texts.filter(Boolean).join(" ").trim(), images: [...new Set(images)] };
}

/**
 * Таможенная пошлина для импортных товаров. Ozon показывает её отдельным
 * блоком "webIconWithText" с href вида "/modal/customs-duty?product_id=...".
 * Этот же widget используется и для других "иконка + текст" (доставка,
 * гарантия, акции), поэтому отличаем именно по customs-duty или по слову
 * "пошлин" в тексте.
 */
export function parseDuty(page) {
  if (!page) return null;
  const all = widgets(page, "webIconWithText");
  for (const w of all) {
    const blob = JSON.stringify(w);
    if (!/customs-duty|пошлин/i.test(blob)) continue;
    // Сумма пошлины — берём первое число с ₽ или "руб" в любом тексте widget'а.
    const m = blob.match(/(\d[\d\s  ]*\d)\s*(?:₽|руб)/i);
    if (!m) continue;
    const amount = priceToNumber(m[1]);
    if (!amount) continue;
    return { amount, note: "пошлина не входит в цену" };
  }
  return null;
}

export function parseDetails(basePage, page2) {
  const heading = widget(basePage, "webProductHeading");
  const price = widget(basePage, "webPrice");
  const gallery = widget(basePage, "webGallery");

  const sku =
    String(
      gallery?.sku ||
        (basePage?.layoutTrackingInfo &&
          JSON.parse(basePage.layoutTrackingInfo || "{}").sku) ||
        ""
    ) ||
    skuFromUrl(basePage?.seo?.link?.[0]?.href) ||
    null;

  const url =
    cleanUrl(basePage?.seo?.link?.[0]?.href) ||
    (sku ? `https://www.ozon.ru/product/${sku}/` : null);

  const { rating, reviews } = parseProductScore(basePage);

  const images = [];
  if (gallery?.coverImage) images.push(gallery.coverImage);
  for (const im of gallery?.images || []) {
    const src = im?.src || im?.image || im;
    if (typeof src === "string") images.push(src);
  }

  const priceCard = priceToNumber(price?.cardPrice) ?? priceToNumber(price?.price);
  const duty = parseDuty(basePage);

  return {
    sku,
    name: heading?.title || basePage?.seo?.title || null,
    url,
    price: priceCard,
    priceRegular: priceToNumber(price?.price),
    oldPrice: priceToNumber(price?.originalPrice),
    duty: duty
      ? {
          amount: duty.amount,
          total: priceCard ? priceCard + duty.amount : null,
          note: duty.note,
        }
      : null,
    available: price?.isAvailable ?? null,
    rating,
    reviews,
    seller: parseSeller(basePage),
    images: [...new Set(images)].slice(0, 10),
    characteristics: parseShortCharacteristics(basePage),
    description: parseDescription(page2),
  };
}

// ── reviews ───────────────────────────────────────────────────────────────────

function unixToDate(ts) {
  if (!ts) return null;
  const d = new Date(ts * 1000);
  if (isNaN(d.getTime())) return null;
  return d.toISOString().slice(0, 10);
}

export function parseReviews(page, limit = 10) {
  const w = widget(page, "webListReviews");
  const raw = w?.reviews || w?.items || [];
  const { rating, reviews: total } = parseProductScore(page);

  const reviews = raw.slice(0, limit).map((r) => {
    const c = r.content || {};
    const author =
      r.author?.title ||
      [r.author?.firstName, r.author?.lastName].filter(Boolean).join(" ") ||
      (r.isAnonymous ? "Аноним" : null);
    return {
      author: author || null,
      score: typeof c.score === "number" ? c.score : null,
      comment: c.comment || "",
      pros: c.positive || "",
      cons: c.negative || "",
      date: unixToDate(r.publishedAt || r.createdAt),
      useful: r.usefulness?.useful ?? null,
      purchased: r.isItemPurchased ?? null,
      hasPhotos: Array.isArray(c.photos) && c.photos.length > 0,
    };
  });

  return { rating, totalReviews: total, count: reviews.length, reviews };
}

export const _internal = { priceToNumber, cleanUrl, skuFromUrl, widget, parseDuty };
