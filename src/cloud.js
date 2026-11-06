// Talks to Cacaduu (cacaduu.com) the same way the website's "Priečinok s exportom" page does:
// the owner's own login, public.my_shops(), public.upload_check_in() and the stock-pull function.
// Only public settings live here: the project address and its publishable key. No service key, ever.
"use strict";

const zlib = require("node:zlib");

const CLOUD = {
  url: process.env.CACADUU_SUPABASE_URL || "https://szhotccxyaepjhwsqdqx.supabase.co",
  key: process.env.CACADUU_SUPABASE_KEY || "sb_publishable_YbIA8qxoTMefxv70bkZmQg_87Q7M0c7",
  site: "https://cacaduu.com",
};

class CloudError extends Error {
  constructor(message, status, code) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

async function call(path, { method = "POST", token, json, body, headers = {} } = {}) {
  let response;
  try {
    response = await fetch(`${CLOUD.url}${path}`, {
      method,
      headers: {
        apikey: CLOUD.key,
        ...(token ? { Authorization: `Bearer ${token}` } : {}),
        ...(json !== undefined ? { "Content-Type": "application/json" } : {}),
        ...headers,
      },
      body: json !== undefined ? JSON.stringify(json) : body,
      signal: AbortSignal.timeout(120_000),
    });
  } catch (e) {
    throw new CloudError(e && e.name === "TimeoutError" ? "no answer" : "no connection", 0, "network");
  }
  const data = await response.json().catch(() => null);
  if (!response.ok) {
    const code = (data && (data.error_code || data.error || data.code)) || `HTTP ${response.status}`;
    const message = (data && (data.message || data.msg || data.error_description || data.error)) || `HTTP ${response.status}`;
    throw new CloudError(String(message), response.status, String(code));
  }
  return data;
}

/** E-mail + password → { access_token, refresh_token, expires_at (ms) }. */
async function signIn(email, password) {
  return session(await call("/auth/v1/token?grant_type=password", { json: { email, password } }));
}

/** A stored refresh token → a new session (Supabase rotates the refresh token every time). */
async function refresh(refreshToken) {
  return session(await call("/auth/v1/token?grant_type=refresh_token", { json: { refresh_token: refreshToken } }));
}

function session(data) {
  if (!data || !data.access_token || !data.refresh_token) throw new CloudError("sign-in failed", 0, "bad_session");
  return {
    access_token: data.access_token,
    refresh_token: data.refresh_token,
    expires_at: Date.now() + Math.max(60, Number(data.expires_in) || 3600) * 1000,
    email: data.user && data.user.email,
  };
}

async function signOut(accessToken) {
  await call("/auth/v1/logout", { token: accessToken }).catch(() => undefined);
}

/** The shops of the signed-in owner: [{ id, name, slug }]. */
async function myShops(accessToken) {
  const rows = await call("/rest/v1/rpc/my_shops", { token: accessToken, json: {} });
  return (Array.isArray(rows) ? rows : []).map((s) => ({ id: s.id, name: s.name, slug: s.slug }));
}

/**
 * Tells Cacaduu this PC is watching the folder ("Okno PPI … naposledy aktívne") and returns what was applied last:
 * { latest_file_time, mapping_status, ... }. Refused with api_connected / agent_connected / sklad_active when the
 * shop's stock comes from somewhere else.
 */
async function checkIn(accessToken, shopId) {
  try {
    return await call("/rest/v1/rpc/upload_check_in", { token: accessToken, json: { p_shop_id: shopId } });
  } catch (e) {
    if (e instanceof CloudError && /^(api_connected|agent_connected|sklad_active)$/.test(e.message)) e.code = e.message;
    throw e;
  }
}

/** Sends one stock file (gzipped) to stock-pull → the outcome ({ status: "updated" | "proposed" | … }). */
async function uploadFile(accessToken, shopId, { name, mtimeMs, data }) {
  const params = new URLSearchParams({
    shop_id: shopId,
    file_time: new Date(mtimeMs).toISOString(),
    file_name: name,
    gzip: "1",
  });
  const answer = await call(`/functions/v1/stock-pull?${params}`, {
    token: accessToken,
    body: zlib.gzipSync(data),
    headers: { "Content-Type": "application/octet-stream" },
  });
  if (!answer || !answer.result) throw new CloudError("no result", 0, "bad_answer");
  return answer.result;
}

module.exports = { CLOUD, CloudError, signIn, refresh, signOut, myShops, checkIn, uploadFile };
