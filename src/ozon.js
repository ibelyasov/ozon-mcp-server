import { fetchJson } from './browser.js';
import { createOperations, productPath } from './operations.js';
export const { search, details, reviews } = createOperations(fetchJson);
export const _internal = { productPath };
