import { parseSearch, parseDetails, parseReviews, parseDescription } from './parse.js';
const SORT_MAP = { popular: '', price: 'price', price_desc: 'price_desc', rating: 'rating', new: 'new', discount: 'discount' };

export function productPath(product) {
  if (typeof product !== 'string' || !product.trim() || product.length > 2000) throw new Error('Invalid product: expected Ozon product URL, SKU or slug');
  let p = product.trim();
  if (/^https?:\/\//i.test(p)) {
    const url = new URL(p);
    if (!['ozon.ru', 'www.ozon.ru'].includes(url.hostname) || url.username || url.password || url.port) throw new Error('Expected an ozon.ru product URL');
    p = url.pathname;
  } else {
    p = p.split(/[?#]/, 1)[0];
  }
  try { p = decodeURIComponent(p); } catch { throw new Error('Invalid URL encoding'); }
  if (p.startsWith('/product/')) p = p.slice('/product/'.length);
  p = p.replace(/\/+$/, '').replace(/\/(reviews|questions)$/, '');
  // Disallow arbitrary site paths, encoded separators and dot traversal.
  if (!/^[\p{L}\p{N}_-]+$/u.test(p) || !/\d+$/.test(p)) throw new Error('Invalid product SKU or slug');
  return `/product/${p}/`;
}
function limitValue(value, fallback, max) {
  const result = value ?? fallback;
  if (!Number.isInteger(result) || result < 1 || result > max) throw new Error(`limit must be between 1 and ${max}`);
  return result;
}
function hasWidget(page, name) { return Object.keys(page?.widgetStates || {}).some(key => key.split('-')[0] === name); }
function warn(result, warning) { result.warnings = [...new Set([...(result.warnings || []), warning])]; }

export function createOperations(fetchJson) {
  async function search({ query, sort = 'popular', priceMin, priceMax, limit = 12 }, { signal } = {}) {
    if (typeof query !== 'string' || !query.trim() || query.length > 2000) throw new Error('query must contain 1–2000 characters');
    if (!Object.hasOwn(SORT_MAP, sort)) throw new Error('Invalid sort');
    limit = limitValue(limit, 12, 36);
    for (const price of [priceMin, priceMax]) if (price != null && (!Number.isSafeInteger(price) || price < 0)) throw new Error('Prices must be nonnegative safe integers');
    if (priceMin != null && priceMax != null && priceMin > priceMax) throw new Error('priceMin must not exceed priceMax');
    const params = new URLSearchParams({ text: query.trim(), from_global: 'true' });
    if (SORT_MAP[sort]) params.set('sorting', SORT_MAP[sort]);
    if (priceMin != null || priceMax != null) {
      const max = priceMax ?? Math.max(priceMin ?? 0, 99999999);
      params.set('currency_price', `${priceMin ?? 0}.000;${max}.000`);
    }
    const page = await fetchJson(`/search/?${params}`, { signal });
    const parsed = parseSearch(page, limit);
    const result = { ...parsed, query: query.trim(), sort };
    if (!hasWidget(page, 'tileGridDesktop')) warn(result, 'SEARCH_WIDGET_MISSING');
    return result;
  }
  async function details({ product }, { signal } = {}) {
    const path = productPath(product);
    const basePage = await fetchJson(path, { signal });
    signal?.throwIfAborted();
    // Do not issue the extra request when the base response already has content.
    const baseDescription = parseDescription(basePage);
    let page2 = null, failed = false;
    if (!baseDescription.text && !baseDescription.images.length) {
      try {
        page2 = await fetchJson(`${path}?layout_container=pdpPage2column&layout_page_index=2`, { signal });
      } catch {
        signal?.throwIfAborted();
        failed = true;
      }
    }
    signal?.throwIfAborted();
    const result = parseDetails(basePage, page2);
    if (baseDescription.text || baseDescription.images.length) result.description = baseDescription;
    if (failed) warn(result, 'DESCRIPTION_FETCH_FAILED');
    else if (!result.description.text && !result.description.images.length) warn(result, 'DESCRIPTION_EMPTY');
    if (!hasWidget(basePage, 'webProductHeading') || !hasWidget(basePage, 'webPrice')) warn(result, 'PRODUCT_WIDGETS_MISSING');
    return result;
  }
  async function reviews({ product, limit = 10 }, { signal } = {}) {
    limit = limitValue(limit, 10, 30);
    const page = await fetchJson(`${productPath(product)}reviews/`, { signal });
    const result = parseReviews(page, limit);
    if (!hasWidget(page, 'webListReviews')) warn(result, 'REVIEWS_WIDGET_MISSING');
    return result;
  }
  return { search, details, reviews };
}
