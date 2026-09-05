#!/usr/bin/env node
// Ozon MCP server (stdio). Три инструмента: ozon_search, ozon_product_details,
// ozon_product_reviews. ВНИМАНИЕ: stdout — JSON-RPC канал, в него НИКОГДА
// нельзя писать; все логи в stderr.

import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import { search, details, reviews } from "./ozon.js";
import { shutdown } from "./browser.js";
import { createToolRunner } from "./tool-runner.js";

const log = (...a) => console.error("[ozon-mcp]", ...a);
const runTool = createToolRunner();
function tool(label, fn) {
  return (args, extra) => runTool(label, signal => fn(args, { signal }), { signal: extra?.signal });
}

const server = new McpServer({ name: "ozon-mcp-server", version: "0.3.3" });

server.registerTool(
  "ozon_search",
  {
    title: "Search Ozon products",
    description:
      "Search products on the Ozon marketplace (ozon.ru). Returns a list of products with name, " +
      "price (RUB, numeric), old price, discount, rating, review count, brand, image and a clean " +
      "product URL.",
    inputSchema: {
      query: z.string().trim().min(1).max(2000).describe('Search query, e.g. "iphone 15", "плед 150х200"'),
      sort: z
        .enum(["popular", "price", "price_desc", "rating", "new", "discount"])
        .default("popular")
        .describe("Sort order: popular (default), price, price_desc, rating, new, discount"),
      priceMin: z.number().int().nonnegative().optional().describe("Minimum price in RUB"),
      priceMax: z.number().int().nonnegative().optional().describe("Maximum price in RUB"),
      limit: z.number().int().min(1).max(36).default(12).describe("Max number of results"),
    },
    annotations: { readOnlyHint: true, openWorldHint: true, idempotentHint: true },
  },
  tool("ozon_search", search)
);

server.registerTool(
  "ozon_product_details",
  {
    title: "Get Ozon product details",
    description:
      "Get full details for one Ozon product: name, price, availability, rating, seller, images, " +
      "characteristics, description. Accepts SKU, full URL, or slug.",
    inputSchema: {
      product: z
        .string()
        .min(1)
        .describe('Product SKU (e.g. "1185261285"), full ozon.ru product URL, or product slug'),
    },
    annotations: { readOnlyHint: true, openWorldHint: true, idempotentHint: true },
  },
  tool("ozon_product_details", details)
);

server.registerTool(
  "ozon_product_reviews",
  {
    title: "Get Ozon product reviews",
    description:
      "Read customer reviews for an Ozon product: author, score, comment, pros, cons, date, " +
      "usefulness, photos.",
    inputSchema: {
      product: z
        .string()
        .min(1)
        .describe('Product SKU, full ozon.ru product URL, or product slug'),
      limit: z.number().int().min(1).max(30).default(10).describe("Max number of reviews"),
    },
    annotations: { readOnlyHint: true, openWorldHint: true, idempotentHint: true },
  },
  tool("ozon_product_reviews", reviews)
);

let cleaning = false;
async function cleanup(exitCode = 0) {
  if (cleaning) return;
  cleaning = true;
  log("shutting down…");
  const deadline = setTimeout(() => process.exit(1), 5000);
  deadline.unref();
  await shutdown().catch(() => {});
  clearTimeout(deadline);
  process.exit(exitCode);
}
process.on("SIGINT", () => cleanup());
process.on("SIGTERM", () => cleanup());
process.on("uncaughtException", (e) => {
  log("uncaughtException:", e?.name || "Error");
  cleanup(1);
});
process.on("unhandledRejection", () => {
  log("unhandledRejection — stopping server");
  cleanup(1);
});
process.stdin.on("end", () => cleanup());

const transport = new StdioServerTransport();
transport.onclose = () => cleanup();
try {
  await server.connect(transport);
  log("ready on stdio");
} catch {
  log("MCP startup failed");
  await cleanup(1);
}
