"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const zlib = require("node:zlib");
const http = require("node:http");

const { checkOnce, newestStockFile, RECHECK_SOON_MS, CHECK_EVERY_MS } = require("../src/sync");

function folderWith(files) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "cacaduu-"));
  for (const [name, content, ageMs] of files) {
    const full = path.join(dir, name);
    fs.writeFileSync(full, content);
    const t = (Date.now() - ageMs) / 1000;
    fs.utimesSync(full, t, t);
  }
  return dir;
}

function fakeCloud(server = {}, result = { status: "updated", items: 3, zeroed: 1, skipped: 0 }) {
  const calls = { checkIn: 0, uploads: [] };
  return {
    calls,
    checkIn: async () => {
      calls.checkIn++;
      return { latest_file_time: null, mapping_status: "confirmed", ...server };
    },
    uploadFile: async (token, shopId, file) => {
      calls.uploads.push({ token, shopId, ...file });
      return result;
    },
  };
}

const base = (folder, cloud, extra = {}) => ({
  folder,
  shopId: "11111111-1111-1111-1111-111111111111",
  token: async () => "access",
  cloud,
  lastAttempt: null,
  saveAttempt: async () => undefined,
  ...extra,
});

test("picks the newest stock file and skips other files", async () => {
  const dir = folderWith([
    ["old.csv", "a", 600_000],
    ["stock.xml", "b", 300_000],
    ["sklad.sk.xlsx", "own", 1_000],
    ["~$stock.xlsx", "lock", 1_000],
    ["notes.pdf", "x", 1_000],
    [".hidden.csv", "x", 1_000],
  ]);
  fs.mkdirSync(path.join(dir, "sub"));
  assert.equal((await newestStockFile(dir)).name, "stock.xml");
});

test("sends a settled new file, gzip-free bytes, the file time and name", async () => {
  const dir = folderWith([["stock.csv", "code;name;qty\n1;A;2\n", 120_000]]);
  const cloud = fakeCloud();
  let saved = null;
  const out = await checkOnce(base(dir, cloud, { saveAttempt: async (a) => (saved = a) }));
  assert.equal(out.sent, true);
  assert.equal(out.message.key, "result_updated");
  assert.deepEqual(out.message.values, { items: 3, zeroed: 1 });
  assert.equal(cloud.calls.uploads.length, 1);
  assert.equal(cloud.calls.uploads[0].name, "stock.csv");
  assert.equal(cloud.calls.uploads[0].data.toString(), "code;name;qty\n1;A;2\n");
  assert.equal(saved.name, "stock.csv");
  assert.equal(out.next, CHECK_EVERY_MS);
});

test("waits for a file written less than a minute ago", async () => {
  const dir = folderWith([["stock.csv", "x", 5_000]]);
  const cloud = fakeCloud();
  const out = await checkOnce(base(dir, cloud));
  assert.equal(out.message.key, "settling");
  assert.equal(out.next, RECHECK_SOON_MS);
  assert.equal(cloud.calls.uploads.length, 0);
});

test("does not send a file Cacaduu already applied", async () => {
  const dir = folderWith([["stock.csv", "x", 120_000]]);
  const cloud = fakeCloud({ latest_file_time: new Date(Date.now() - 60_000).toISOString() });
  const out = await checkOnce(base(dir, cloud));
  assert.equal(out.message.key, "up_to_date");
  assert.equal(cloud.calls.uploads.length, 0);
});

test("does not resend while still waiting for the column approval", async () => {
  const dir = folderWith([["stock.csv", "x", 120_000]]);
  const stat = fs.statSync(path.join(dir, "stock.csv"));
  const cloud = fakeCloud({ mapping_status: "proposed" });
  const lastAttempt = { name: "stock.csv", mtimeMs: stat.mtimeMs, size: stat.size, mappingStatus: "proposed", result: { status: "proposed", rows: 9 } };
  const out = await checkOnce(base(dir, cloud, { lastAttempt }));
  assert.equal(out.message.key, "result_proposed");
  assert.equal(cloud.calls.uploads.length, 0);
  // once the owner approved the columns, the same file goes again
  const approved = fakeCloud({ mapping_status: "confirmed" });
  await checkOnce(base(dir, approved, { lastAttempt }));
  assert.equal(approved.calls.uploads.length, 1);
});

test("signed out, no folder, no shop, other stock source", async () => {
  const dir = folderWith([["stock.csv", "x", 120_000]]);
  assert.equal((await checkOnce(base(dir, fakeCloud(), { token: async () => null }))).state, "signed_out");
  assert.equal((await checkOnce(base(null, fakeCloud()))).message.key, "pick_folder");
  assert.equal((await checkOnce(base(dir, fakeCloud(), { shopId: null }))).message.key, "pick_shop");
  assert.equal((await checkOnce(base(path.join(dir, "gone"), fakeCloud()))).message.key, "folder_missing");
  const busy = fakeCloud();
  busy.checkIn = async () => {
    throw Object.assign(new Error("agent_connected"), { code: "agent_connected", status: 400 });
  };
  const out = await checkOnce(base(dir, busy));
  assert.equal(out.state, "other_source");
  assert.equal(out.message.key, "agent_connected");
});

// ------------------------------------------------ src/cloud.js against a local stand-in for Supabase

test("cloud.js: sign-in, shops, check-in and a gzipped upload", async () => {
  const seen = [];
  const server = http.createServer((req, res) => {
    const chunks = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => {
      const body = Buffer.concat(chunks);
      seen.push({ method: req.method, url: req.url, headers: req.headers, body });
      res.setHeader("Content-Type", "application/json");
      if (req.url.startsWith("/auth/v1/token?grant_type=password")) {
        const { password } = JSON.parse(body);
        if (password !== "right") {
          res.statusCode = 400;
          return res.end(JSON.stringify({ code: 400, error_code: "invalid_credentials", msg: "Invalid login credentials" }));
        }
        return res.end(JSON.stringify({ access_token: "acc", refresh_token: "ref", expires_in: 3600, user: { email: "a@b.sk" } }));
      }
      if (req.url === "/rest/v1/rpc/my_shops") return res.end(JSON.stringify([{ id: "s1", name: "Shop", slug: "shop", extra: 1 }]));
      if (req.url === "/rest/v1/rpc/upload_check_in") {
        res.statusCode = 400;
        return res.end(JSON.stringify({ code: "55000", message: "sklad_active", hint: "…" }));
      }
      if (req.url.startsWith("/functions/v1/stock-pull?")) return res.end(JSON.stringify({ result: { status: "proposed", rows: 2 } }));
      res.statusCode = 404;
      res.end("{}");
    });
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  process.env.CACADUU_SUPABASE_URL = `http://127.0.0.1:${server.address().port}`;
  delete require.cache[require.resolve("../src/cloud")];
  const cloud = require("../src/cloud");
  try {
    await assert.rejects(cloud.signIn("a@b.sk", "wrong"), (e) => e.status === 400 && e.code === "invalid_credentials");
    const s = await cloud.signIn("a@b.sk", "right");
    assert.equal(s.access_token, "acc");
    assert.equal(s.email, "a@b.sk");
    assert.deepEqual(await cloud.myShops("acc"), [{ id: "s1", name: "Shop", slug: "shop" }]);
    await assert.rejects(cloud.checkIn("acc", "s1"), (e) => e.code === "sklad_active");
    const result = await cloud.uploadFile("acc", "s1", { name: "stock.csv", mtimeMs: Date.UTC(2026, 10, 6, 10, 0), data: Buffer.from("a;b\n") });
    assert.deepEqual(result, { status: "proposed", rows: 2 });

    const upload = seen.find((r) => r.url.startsWith("/functions/v1/stock-pull"));
    const q = new URL(upload.url, "http://x").searchParams;
    assert.equal(q.get("shop_id"), "s1");
    assert.equal(q.get("file_name"), "stock.csv");
    assert.equal(q.get("file_time"), "2026-11-06T10:00:00.000Z");
    assert.equal(q.get("gzip"), "1");
    assert.equal(upload.headers.authorization, "Bearer acc");
    assert.ok(upload.headers.apikey);
    assert.equal(zlib.gunzipSync(upload.body).toString(), "a;b\n");
  } finally {
    server.close();
  }
});
