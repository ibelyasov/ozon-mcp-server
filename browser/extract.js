// Generated from browser/extract.ts; run npm run build:browser.
(()=>{
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
function scalars(value, keys) {
    const result = {};
    for (const key of keys) {
        const field = value[key];
        if (typeof field === "string" || typeof field === "boolean" ||
            (typeof field === "number" && Number.isFinite(field)))
            result[key] = field;
    }
    return result;
}
function records(value) {
    return Array.isArray(value) ? value.filter((v) => isRecord(v) && !Array.isArray(v)) : [];
}
function display(value, result, includeDescription = false) {
    for (const key of includeDescription ? ["title", "description"] : ["title"]) {
        if (typeof value[key] === "string")
            result[key] = value[key];
        else if (isRecord(value[key]) && typeof value[key].text === "string") {
            result[key] = { text: value[key].text };
        }
    }
}
function filterFields(value, depth = 0) {
    const result = scalars(value, ["isSelected", "isActive", "isRadio", "hasManyValues"]);
    result.sourceTruncated = ["categories", "sections", "colorIcons"].some((key) => Array.isArray(value[key]) && records(value[key]).length !== value[key].length) || records(value.sections).some((section) => Array.isArray(section.items) && records(section.items).length !== section.items.length);
    if (result.sourceTruncated === false)
        delete result.sourceTruncated;
    display(value, result);
    if (Array.isArray(value.categories))
        result.categories = records(value.categories).map((v) => {
            const item = scalars(v, ["isActive", "urlValue"]);
            display(v, item);
            return item;
        });
    if (Array.isArray(value.sections))
        result.sections = records(value.sections).map((section) => ({
            items: records(section.items).map((v) => {
                const item = scalars(v, ["key", "isSelected"]);
                display(v, item, true);
                return item;
            }),
        }));
    if (Array.isArray(value.colorIcons))
        result.colorIcons = records(value.colorIcons).map((v) => {
            const item = scalars(v, ["key", "isSelected"]);
            display(v, item, true);
            return item;
        });
    if (isRecord(value.openingButtons)) {
        const buttons = {};
        for (const key of ["showAllButton", "hideAllButton"]) {
            if (isRecord(value.openingButtons[key]))
                buttons[key] = {};
        }
        result.openingButtons = buttons;
    }
    if (depth === 0) {
        for (const key of ["rangeFilter", "checkboxesFilter"]) {
            if (isRecord(value[key]))
                result[key] = filterFields(value[key], depth + 1);
        }
    }
    return result;
}
function linkedItem(value, keys) {
    const result = scalars(value, keys);
    if (isRecord(value.action) && typeof value.action.link === "string") {
        result.action = { link: value.action.link };
    }
    return result;
}
function projectSearchWidget(name, raw) {
    let value = raw;
    try {
        if (typeof raw === "string")
            value = JSON.parse(raw);
    }
    catch {
        return undefined;
    }
    if (!isRecord(value) || Array.isArray(value))
        return undefined;
    switch (name) {
        case "filtersDesktop":
            if (!Array.isArray(value.sections))
                return undefined;
            return { ...(records(value.sections).length !== value.sections.length || records(value.sections).some((section) => Array.isArray(section.filters) && records(section.filters).length !== section.filters.length) ? { sourceTruncated: true } : {}), sections: records(value.sections).map((section) => ({
                    filters: records(section.filters).flatMap((filter) => {
                        const type = filter.type;
                        if (typeof type !== "string" || !filterTypes.has(type) ||
                            !isRecord(filter[type]) || Array.isArray(filter[type]))
                            return [];
                        return [{ ...scalars(filter, ["type", "key"]), [type]: filterFields(filter[type]) }];
                    }),
                })) };
        case "searchResultsSort":
            if (!isRecord(value.sortButton) || !Array.isArray(value.sortButton.options))
                return undefined;
            return { sortButton: { options: records(value.sortButton.options)
                        .map((v) => linkedItem(v, ["name", "isSelected"])) } };
        case "searchResultsFiltersActive":
            if (!Array.isArray(value.activeFilters))
                return undefined;
            return { activeFilters: records(value.activeFilters).map((v) => {
                    const item = scalars(v, ["key"]);
                    if (Array.isArray(v.activeValues)) {
                        item.activeValues = records(v.activeValues)
                            .map((entry) => scalars(entry, ["title", "disableUri"]));
                    }
                    return item;
                }) };
        case "infiniteVirtualPaginator":
            return scalars(value, ["nextPage", "prevPage", "size", "layoutContainer", "fetchType"]);
        case "categoryBrandList":
            if (!Array.isArray(value.brands))
                return undefined;
            return { brands: records(value.brands).map((v) => linkedItem(v, ["text"])) };
        case "webAspects":
            if (!Array.isArray(value.aspects))
                return undefined;
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
                                projected.data = scalars(variant.data, ["title", "text", "name", "value", "isSelected", "selected"]);
                            }
                            return projected;
                        });
                    }
                    return item;
                }) };
        case "webListReviews": {
            if (!Array.isArray(value.reviews) && !Array.isArray(value.items))
                return undefined;
            const rawReviews = Array.isArray(value.reviews) ? value.reviews : value.items;
            const result = scalars(value, ["itemId", "requestedPath", "fullRequestUrl", "productScore", "pageType"]);
            if (isRecord(value.paging)) {
                const paging = scalars(value.paging, ["page", "perPage", "total", "commonTotal", "nextButton", "prevButton"]);
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
                if (!isRecord(review) || Array.isArray(review))
                    return null;
                const item = scalars(review, ["uuid", "reviewId", "id", "variantLabel", "itemId", "publishedAt", "createdAt",
                    "isItemPurchased", "showVariantImage", "isAnonymous"]);
                if (isRecord(review.author)) {
                    const author = scalars(review.author, ["firstName", "lastName", "fio", "title"]);
                    if (isRecord(review.author.title) && typeof review.author.title.text === "string")
                        author.title = { text: review.author.title.text };
                    item.author = author;
                }
                if (isRecord(review.content)) {
                    const content = scalars(review.content, ["score", "comment", "positive", "negative"]);
                    if (Array.isArray(review.content.photos)) {
                        content.photos = review.content.photos.map((photo) => isRecord(photo)
                            ? scalars(photo, ["src", "url", "link", "photoUrl"])
                            : typeof photo === "string" ? photo : null);
                    }
                    item.content = content;
                }
                for (const key of ["productVariant", "variant"]) {
                    if (isRecord(review[key])) {
                        const variant = scalars(review[key], ["title"]);
                        if (isRecord(review[key].title) && typeof review[key].title.text === "string")
                            variant.title = { text: review[key].title.text };
                        item[key] = variant;
                    }
                }
                if (isRecord(review.status))
                    item.status = scalars(review.status, ["id", "name"]);
                return item;
            });
            if (isRecord(value.products)) {
                result.productsCount = Object.keys(value.products)
                    .filter((sku) => /^[0-9]+$/.test(sku)).length;
                const itemIds = new Set(records(rawReviews).map((review) => String(review.itemId ?? "")));
                result.products = Object.fromEntries(Object.entries(value.products).flatMap(([sku, product]) => {
                    if (!/^[0-9]+$/.test(sku) || !isRecord(product) ||
                        (!itemIds.has(sku) && !itemIds.has(String(product.itemId ?? ""))))
                        return [];
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
function isRecord(value) {
    return typeof value === "object" && value !== null;
}
function widgetName(key) {
    return key.split("-")[0] ?? "";
}
function parseOptions(value) {
    if (!isRecord(value))
        return null;
    if (value.mode === "widgets")
        return { mode: "widgets" };
    if (value.mode === "context")
        return { mode: "context" };
    if (value.mode === "contextModal")
        return { mode: "contextModal" };
    if (value.mode === "navigation" &&
        (value.target === "home" || value.target === "addressBook")) {
        return { mode: "navigation", target: value.target };
    }
    if (value.mode === "fetch" && typeof value.path === "string") {
        return { mode: "fetch", path: value.path };
    }
    return null;
}
function displayField(value) {
    if (typeof value === "string")
        return value;
    if (isRecord(value))
        return scalars(value, ["text", "content"]);
    if (Array.isArray(value))
        return value.map(displayField);
    return null;
}
function imageField(value) {
    return typeof value === "string" ? value : isRecord(value) ? scalars(value, ["src", "url", "link", "image", "photoUrl"]) : null;
}
function annotationField(value) {
    if (Array.isArray(value))
        return value.map(annotationField);
    if (!isRecord(value))
        return typeof value === "string" ? value : null;
    const result = scalars(value, ["type", "content", "src"]);
    for (const key of ["content", "blocks", "title", "text"]) {
        if (Array.isArray(value[key]) || isRecord(value[key]))
            result[key] = annotationField(value[key]);
    }
    for (const key of ["img", "image", "attrs"]) {
        if (isRecord(value[key]))
            result[key] = scalars(value[key], ["src"]);
    }
    return result;
}
function projectPublicWidget(name, raw) {
    let value = raw;
    try {
        if (typeof raw === "string")
            value = JSON.parse(raw);
    }
    catch {
        return null;
    }
    if (!isRecord(value) || Array.isArray(value))
        return null;
    switch (name) {
        case "webPrice": return scalars(value, ["cardPrice", "price", "isAvailable", "deliveryLabel"]);
        case "webProductHeading": return { title: displayField(value.title) };
        case "webIconWithText": return { title: displayField(value.title), text: displayField(value.text) };
        case "webGallery": return { ...scalars(value, ["sku"]),
            ...(value.coverImage !== undefined ? { coverImage: imageField(value.coverImage) } : {}),
            ...(Array.isArray(value.images) ? { images: value.images.map(imageField) } : {}) };
        case "webSingleProductScore":
        case "webReviewProductScore": {
            const result = scalars(value, ["rating", "ratingValue", "text", "reviews", "reviewCount", "reviewsCount", "totalReviews"]);
            if (isRecord(value.title))
                result.title = scalars(value.title, ["text"]);
            return result;
        }
        case "webCurrentSeller": {
            const result = {};
            if (value.title !== undefined)
                result.title = displayField(value.title);
            if (isRecord(value.rating))
                result.rating = { title: displayField(value.rating.title) };
            else if (typeof value.rating === "number" || typeof value.rating === "string")
                result.rating = value.rating;
            if (isRecord(value.sellerCell)) {
                const cell = value.sellerCell;
                result.sellerCell = { centerBlock: isRecord(cell.centerBlock) ? { title: displayField(cell.centerBlock.title) } : {},
                    common: isRecord(cell.common) && isRecord(cell.common.action) ? { action: scalars(cell.common.action, ["link"]) } : {} };
            }
            return result;
        }
        case "webShortCharacteristics": return { characteristics: Array.isArray(value.characteristics) ? value.characteristics.map((row) => {
                if (!isRecord(row))
                    return null;
                const result = {};
                if (row.title !== undefined)
                    result.title = isRecord(row.title) ? { ...scalars(row.title, ["text"]), ...(row.title.textRs !== undefined ? { textRs: displayField(row.title.textRs) } : {}) } : displayField(row.title);
                for (const key of ["values", "contentRS", "valueRs"])
                    if (row[key] !== undefined)
                        result[key] = displayField(row[key]);
                return result;
            }) : null };
        case "webDescription": {
            const result = scalars(value, ["richAnnotation"]);
            if (value.richAnnotationJson !== undefined) {
                let annotation = value.richAnnotationJson;
                try {
                    if (typeof annotation === "string")
                        annotation = JSON.parse(annotation);
                }
                catch {
                    annotation = null;
                }
                result.richAnnotationJson = annotationField(annotation);
            }
            return result;
        }
        case "tileGridDesktop": return { items: Array.isArray(value.items) ? value.items.map((row) => {
                if (!isRecord(row))
                    return null;
                const result = scalars(row, ["sku", "id"]);
                if (isRecord(row.action))
                    result.action = scalars(row.action, ["link"]);
                if (isRecord(row.tileImage))
                    result.tileImage = { ...(Array.isArray(row.tileImage.items) ? { items: row.tileImage.items.map((i) => isRecord(i) ? { image: imageField(i.image) } : null) } : {}), ...(row.tileImage.coverImage !== undefined ? { coverImage: imageField(row.tileImage.coverImage) } : {}) };
                if (Array.isArray(row.mainState))
                    result.mainState = records(row.mainState).map((state) => {
                        const out = scalars(state, ["type", "id"]);
                        if (state.textDS !== undefined)
                            out.textDS = displayField(state.textDS);
                        if (isRecord(state.priceV2)) {
                            const price = state.priceV2;
                            out.priceV2 = { ...(Array.isArray(price.price) ? { price: records(price.price).filter((p) => p.textStyle === "PRICE").map((p) => scalars(p, ["textStyle", "text"])) } : {}), ...(isRecord(price.priceStyle) ? { priceStyle: scalars(price.priceStyle, ["styleType"]) } : {}) };
                        }
                        if (isRecord(state.labelListV2)) {
                            const labels = state.labelListV2;
                            out.labelListV2 = { items: records(labels.items).map((i) => {
                                    const item = scalars(i, ["type"]);
                                    if (i.text !== undefined)
                                        item.text = displayField(i.text);
                                    if (typeof i.icon === "string")
                                        item.icon = i.icon;
                                    else if (isRecord(i.icon))
                                        item.icon = annotationField(i.icon);
                                    return item;
                                }) };
                        }
                        return out;
                    });
                if (isRecord(row.multiButton) && isRecord(row.multiButton.ozonButton) && isRecord(row.multiButton.ozonButton.addToCart) && isRecord(row.multiButton.ozonButton.addToCart.actionButton))
                    result.multiButton = { ozonButton: { addToCart: { actionButton: scalars(row.multiButton.ozonButton.addToCart.actionButton, ["title"]) } } };
                return result;
            }) : null };
        default: return null;
    }
}
function filterPage(page) {
    if (!isRecord(page))
        return { error: "INVALID_RESPONSE" };
    const widgetStates = Object.create(null);
    if (isRecord(page.widgetStates)) {
        for (const [key, value] of Object.entries(page.widgetStates)) {
            const name = widgetName(key);
            if (publicWidgetNames.has(name))
                widgetStates[key] = projectPublicWidget(name, value);
            else if (searchWidgetNames.has(name) || sourceWidgetNames.has(name)) {
                const projected = projectSearchWidget(name, value);
                widgetStates[key] = projected === undefined ? null : projected;
            }
        }
    }
    const result = { widgetStates };
    if (isRecord(page.seo)) {
        result.seo = {
            title: typeof page.seo.title === "string" ? page.seo.title : null,
            link: Array.isArray(page.seo.link)
                ? page.seo.link
                    .filter((entry) => isRecord(entry) && typeof entry.href === "string")
                    .map((entry) => ({ href: entry.href }))
                : [],
        };
    }
    try {
        const tracking = typeof page.layoutTrackingInfo === "string"
            ? JSON.parse(page.layoutTrackingInfo)
            : page.layoutTrackingInfo;
        if (isRecord(tracking) && /^[0-9]+$/.test(String(tracking.sku))) {
            result.layoutTrackingInfo = { sku: tracking.sku };
        }
    }
    catch {
    }
    if (new TextEncoder().encode(JSON.stringify(result)).length > maxBytes) {
        return { error: "RESPONSE_TOO_LARGE" };
    }
    return { page: result };
}
function cityOnlyLabel(city) {
    return city.length <= 100 && /\p{L}/u.test(city) && /^[\p{L} -]+$/u.test(city) &&
        !/(?:^|[^\p{L}])(?:адрес|улица|улицы|дом|дома|квартира|квартиры|подъезд|этаж|доставка|пункт|выдача|укажите|сегодня|завтра|послезавтра)(?:$|[^\p{L}])/iu.test(city)
        ? city : null;
}
async function ozonPage(rawOptions) {
    if (location.origin !== "https://www.ozon.ru") {
        return { error: "INVALID_ORIGIN" };
    }
    const options = parseOptions(rawOptions);
    if (options === null)
        return { error: "INVALID_OPTIONS" };
    if (options.mode === "navigation") {
        const current = new URL(location.href);
        const responseQuery = current.search === "" || /^\?__rr=[0-9]{1,16}$/.test(current.search);
        const expectedPath = options.target === "home" ? "/" : "/modal/addressbook";
        const routeValid = current.origin === "https://www.ozon.ru" &&
            current.protocol === "https:" && current.username === "" && current.password === "" &&
            current.port === "" && current.pathname === expectedPath && responseQuery &&
            current.hash === "";
        const navigation = performance.getEntriesByType("navigation")[0];
        const status = navigation !== undefined && Number.isSafeInteger(navigation.responseStatus) &&
            navigation.responseStatus >= 0 ? navigation.responseStatus : null;
        return { page: { widgetStates: {}, navigationProbe: { routeValid, status } } };
    }
    if (options.mode === "contextModal") {
        const raw = document.querySelector('[id^="state-commonAddressBook-"][data-state]')
            ?.getAttribute("data-state");
        let selectedRegionLabel = null;
        try {
            const value = raw === undefined || raw === null ? null : JSON.parse(raw);
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
                selectedRegionLabel = cityOnlyLabel(city);
            }
        }
        catch {
        }
        return { page: {
                widgetStates: {},
                regionProbe: { addressBookModalAvailable: false, selectedRegionLabel },
            } };
    }
    if (options.mode === "context") {
        const visible = (element) => {
            if (!(element instanceof HTMLElement))
                return false;
            const style = getComputedStyle(element);
            return style.display !== "none" && style.visibility !== "hidden";
        };
        const state = (name) => {
            const raw = document.querySelector(`[id^="state-${name}-"][data-state]`)
                ?.getAttribute("data-state");
            if (raw === undefined || raw === null)
                return null;
            try {
                const value = JSON.parse(raw);
                return isRecord(value) && !Array.isArray(value) ? value : null;
            }
            catch {
                return null;
            }
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
            }
            catch {
            }
        }
        const cityValue = address?.customCell;
        const city = isRecord(cityValue) && Array.isArray(cityValue.cells) &&
            isRecord(cityValue.cells[0]) && isRecord(cityValue.cells[0].button) &&
            typeof cityValue.cells[0].button.text === "string"
            ? cityValue.cells[0].button.text.split(/\s+/u).filter(Boolean).join(" ") : "";
        const anonymousRegionLabel = cityOnlyLabel(city);
        const anonymous = document.querySelector('[id^="state-profileMenuAnonymous-"][data-state]') !== null ||
            visible(document.querySelector('a[href^="/login"], button[aria-label="Войти"]'));
        const authenticated = !anonymous &&
            document.querySelector('[id^="state-profileMenu-"][data-state]') !== null;
        const publicState = Array.from(document.querySelectorAll('[id^="state-"][data-state]'))
            .some((element) => {
            const name = widgetName(element.id.slice(6));
            return publicWidgetNames.has(name) || searchWidgetNames.has(name) ||
                sourceWidgetNames.has(name);
        });
        const accountState = anonymous ? "anonymous" : authenticated ? "authenticated" : "unknown";
        const regionLabel = accountState === "anonymous" ? anonymousRegionLabel : null;
        const indicatorText = regionLabel !== null || accountState !== "unknown"
            ? `${accountState}\n${regionLabel ?? ""}` : "";
        let signature = null;
        if (indicatorText.length > 0) {
            try {
                const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(indicatorText));
                signature = Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, "0")).join("");
            }
            catch {
            }
        }
        const page = {
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
        const widgetStates = Object.create(null);
        let bytes = 0;
        for (const element of document.querySelectorAll('[id^="state-"][data-state]')) {
            const key = element.id.slice(6);
            if (!publicWidgetNames.has(widgetName(key)) && !searchWidgetNames.has(widgetName(key)) &&
                !sourceWidgetNames.has(widgetName(key)))
                continue;
            const value = element.getAttribute("data-state");
            if (value === null)
                continue;
            bytes += new TextEncoder().encode(value).length;
            if (bytes > maxBytes)
                return { error: "RESPONSE_TOO_LARGE" };
            widgetStates[key] = value;
        }
        return Object.keys(widgetStates).length > 0
            ? filterPage({ widgetStates })
            : { error: "CAPTCHA_OR_BLOCKED" };
    }
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 35_000);
    try {
        const response = await fetch("/api/composer-api.bx/page/json/v2?url=" +
            encodeURIComponent(options.path), {
            headers: { accept: "application/json" },
            signal: controller.signal,
        });
        if (!response.ok)
            return { status: response.status };
        if (Number(response.headers.get("content-length")) > maxBytes) {
            return { error: "RESPONSE_TOO_LARGE" };
        }
        if (response.body === null)
            return { error: "INVALID_RESPONSE" };
        const reader = response.body.getReader();
        const decoder = new TextDecoder();
        let bytes = 0;
        let text = "";
        for (;;) {
            const { value, done } = await reader.read();
            if (done)
                break;
            bytes += value.length;
            if (bytes > maxBytes) {
                controller.abort();
                return { error: "RESPONSE_TOO_LARGE" };
            }
            text += decoder.decode(value, { stream: true });
        }
        try {
            return filterPage(JSON.parse(text + decoder.decode()));
        }
        catch {
            return { error: "INVALID_RESPONSE" };
        }
    }
    catch (error) {
        return {
            error: isRecord(error) && error.name === "AbortError"
                ? "FETCH_TIMEOUT"
                : "FETCH_FAILED",
        };
    }
    finally {
        clearTimeout(timer);
    }
}
return ozonPage;
})()
