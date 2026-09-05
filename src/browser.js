import { chromium } from "playwright";
import { homedir } from "node:os";
import { join } from "node:path";
import { BrowserRuntime, browserOptionsFromEnvironment } from "./browser-runtime.js";

const log = (...a) => console.error("[browser]", ...a);
const userDataDir = process.env.OZON_USER_DATA_DIR || join(homedir(), ".ozon-mcp-userdata");
const options = browserOptionsFromEnvironment(process.env, { userDataDir, log });
const runtime = new BrowserRuntime({ chromium, ...options, log });

// `channel: "chromium"` selects full Chromium even in the default headless
// mode. A headed login is explicit (OZON_HEADLESS=false); there is no hidden
// off-screen-window fallback.
export const fetchJson = (path, options) => runtime.fetchJson(path, options);
export const shutdown = () => runtime.shutdown();
