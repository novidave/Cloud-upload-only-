// The folder watcher, with the same rules as the website's PPI window (FolderSync):
// the newest stock file directly in the export folder, untouched for a minute, newer than what Cacaduu applied
// last, and not already sent while nothing changed on Cacaduu's side. The folder is only ever read.
"use strict";

const fs = require("node:fs/promises");
const path = require("node:path");

const CHECK_EVERY_MS = 15 * 60_000;
/** A file must be untouched this long before it is sent, so a half-written export is never sent. */
const SETTLE_MS = 60_000;
const RECHECK_SOON_MS = 60_000;

/** Stock files stock-pull can read (old binary .xls is not one of them). */
const STOCK_FILE = /\.(xml|csv|txt|xlsx)$/i;
/** Cacaduu's own translation files (sklad.<lang>.xlsx) are never a stock file. */
const OWN_FILE = /^sklad\.[a-z]{2,3}(-[a-z]{2,4})?\.xlsx$/i;

/** The most recently written stock file directly in the folder (subfolders ignored) → { name, full, mtimeMs, size }. */
async function newestStockFile(folder) {
  let newest = null;
  for (const entry of await fs.readdir(folder, { withFileTypes: true })) {
    const name = entry.name;
    if (!entry.isFile() || !STOCK_FILE.test(name) || OWN_FILE.test(name) || name.startsWith("~$") || name.startsWith(".")) continue;
    const full = path.join(folder, name);
    try {
      const stat = await fs.stat(full);
      if (!newest || stat.mtimeMs > newest.mtimeMs) newest = { name, full, mtimeMs: stat.mtimeMs, size: stat.size };
    } catch {
      // being written or locked right now: the next check picks it up
    }
  }
  return newest;
}

/**
 * One check. `deps`:
 *   folder, shopId                  what to watch and for which shop
 *   token()                         → a fresh access token, or null when signed out
 *   cloud                           { checkIn, uploadFile } (src/cloud.js)
 *   lastAttempt / saveAttempt(a)    the last file sent, so an unchanged situation is not sent again
 *   now()                           clock (tests)
 * → { state, message: { key, values }, next (ms until the next check), file?, server? }
 */
async function checkOnce(deps) {
  const now = deps.now ? deps.now() : Date.now();
  const later = (state, key, values = {}, next = CHECK_EVERY_MS, extra = {}) => ({ state, message: { key, values }, next, ...extra });

  if (!deps.shopId) return later("setup", "pick_shop");
  if (!deps.folder) return later("setup", "pick_folder");
  const token = await deps.token();
  if (!token) return later("signed_out", "signed_out");

  try {
    await fs.access(deps.folder);
  } catch {
    return later("error", "folder_missing", { folder: deps.folder });
  }

  let server;
  try {
    server = await deps.cloud.checkIn(token, deps.shopId);
  } catch (e) {
    return failed(e, later);
  }

  const file = await newestStockFile(deps.folder);
  if (!file) return later("watching", "no_file", {}, CHECK_EVERY_MS, { server });
  const info = { name: file.name, mtimeMs: file.mtimeMs };
  if (now - file.mtimeMs < SETTLE_MS) return later("watching", "settling", {}, RECHECK_SOON_MS, { server, file: info });

  const applied = server && server.latest_file_time ? Date.parse(server.latest_file_time) : 0;
  // Cacaduu keeps file times to the millisecond; a file is new only when it is newer than that.
  if (Math.floor(file.mtimeMs) <= applied) return later("watching", "up_to_date", {}, CHECK_EVERY_MS, { server, file: info });

  // Already sent and nothing changed on Cacaduu's side (e.g. still waiting for the owner to approve the columns).
  const previous = deps.lastAttempt;
  if (
    previous &&
    previous.name === file.name &&
    previous.mtimeMs === file.mtimeMs &&
    previous.size === file.size &&
    previous.mappingStatus === (server && server.mapping_status)
  ) {
    return { ...resultMessage(previous.result), next: CHECK_EVERY_MS, server, file: info };
  }

  let data;
  try {
    data = await fs.readFile(file.full);
    const again = await fs.stat(file.full);
    if (again.mtimeMs !== file.mtimeMs || again.size !== file.size) {
      return later("watching", "settling", {}, RECHECK_SOON_MS, { server, file: info }); // written again while read
    }
  } catch {
    return later("watching", "settling", {}, RECHECK_SOON_MS, { server, file: info }); // locked by the stock software
  }

  let result;
  try {
    result = await deps.cloud.uploadFile(token, deps.shopId, { name: file.name, mtimeMs: Math.floor(file.mtimeMs), data });
  } catch (e) {
    return failed(e, later);
  }
  let after = server;
  try {
    after = await deps.cloud.checkIn(token, deps.shopId);
  } catch {
    // the file is in; the status line catches up at the next check
  }
  await deps.saveAttempt({
    name: file.name,
    mtimeMs: file.mtimeMs,
    size: file.size,
    mappingStatus: after && after.mapping_status,
    result,
    at: now,
  });
  return { ...resultMessage(result), next: CHECK_EVERY_MS, server: after, file: info, sent: true };
}

function resultMessage(result) {
  const state = result.status === "error" ? "error" : "watching";
  switch (result.status) {
    case "updated":
      return { state, message: { key: "result_updated", values: { items: result.items ?? 0, zeroed: result.zeroed ?? 0 } } };
    case "unchanged":
      return { state, message: { key: "result_unchanged", values: {} } };
    case "proposed":
    case "waiting_for_approval":
    case "layout_changed":
      return { state, message: { key: `result_${result.status}`, values: { rows: result.rows ?? 0 } } };
    case "error":
      return { state, message: { key: "result_error", values: { error: String(result.error ?? "") } } };
    default:
      return { state, message: { key: "result_unchanged", values: {} } };
  }
}

function failed(e, later) {
  const code = e && e.code;
  if (code === "api_connected" || code === "agent_connected" || code === "sklad_active") return later("other_source", code);
  if (e && e.status === 401) return later("signed_out", "signed_out");
  if (e && e.status === 403) return later("error", "not_owner");
  return later("error", "send_failed", { error: (e && e.message) || String(e) });
}

module.exports = { checkOnce, newestStockFile, CHECK_EVERY_MS, SETTLE_MS, RECHECK_SOON_MS };
