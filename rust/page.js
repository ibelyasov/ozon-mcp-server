// Runs inside the Ozon tab. No account, address, or order widgets leave the page.
async (options) => {
  const names = new Set(['tileGridDesktop', 'webShortCharacteristics',
    'webSingleProductScore', 'webReviewProductScore', 'webCurrentSeller',
    'webDescription', 'webIconWithText', 'webProductHeading', 'webPrice',
    'webGallery', 'webListReviews']);
  const maxBytes = 4 * 1024 * 1024;
  if (location.origin !== 'https://www.ozon.ru') return { error: 'INVALID_ORIGIN' };
  const filter = (page) => {
    const widgetStates = Object.create(null);
    for (const [key, value] of Object.entries(page.widgetStates || {})) {
      if (names.has(key.split('-')[0])) widgetStates[key] = value;
    }
    const result = { widgetStates };
    if (page.seo && typeof page.seo === 'object') {
      result.seo = {
        title: typeof page.seo.title === 'string' ? page.seo.title : null,
        link: Array.isArray(page.seo.link) ? page.seo.link.filter(x => x && typeof x.href === 'string').map(x => ({href: x.href})) : [],
      };
    }
    try {
      const tracking = typeof page.layoutTrackingInfo === 'string' ? JSON.parse(page.layoutTrackingInfo) : page.layoutTrackingInfo;
      if (tracking && /^[0-9]+$/.test(String(tracking.sku))) result.layoutTrackingInfo = {sku: tracking.sku};
    } catch {}
    if (new TextEncoder().encode(JSON.stringify(result)).length > maxBytes)
      return { error: 'RESPONSE_TOO_LARGE' };
    return { page: result };
  };
  if (options.mode === 'widgets') {
    const widgetStates = Object.create(null);
    let bytes = 0;
    for (const el of document.querySelectorAll('[id^="state-"][data-state]')) {
      const key = el.id.slice(6);
      if (!names.has(key.split('-')[0])) continue;
      const value = el.getAttribute('data-state');
      bytes += new TextEncoder().encode(value).length;
      if (bytes > maxBytes) return { error: 'RESPONSE_TOO_LARGE' };
      widgetStates[key] = value;
    }
    return Object.keys(widgetStates).length ? filter({ widgetStates }) : { error: 'CAPTCHA_OR_BLOCKED' };
  }
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 35000);
  try {
    const response = await fetch('/api/composer-api.bx/page/json/v2?url=' + encodeURIComponent(options.path), {
      headers: { accept: 'application/json' }, signal: controller.signal,
    });
    if (!response.ok) return { status: response.status };
    if (Number(response.headers.get('content-length')) > maxBytes)
      return { error: 'RESPONSE_TOO_LARGE' };
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let bytes = 0, text = '';
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      bytes += value.length;
      if (bytes > maxBytes) { controller.abort(); return { error: 'RESPONSE_TOO_LARGE' }; }
      text += decoder.decode(value, { stream: true });
    }
    try { return filter(JSON.parse(text + decoder.decode())); }
    catch { return { error: 'INVALID_RESPONSE' }; }
  } catch (error) {
    return { error: error.name === 'AbortError' ? 'FETCH_TIMEOUT' : 'FETCH_FAILED' };
  } finally { clearTimeout(timer); }
}
