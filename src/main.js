// Cacaduu Upload: one small window ("Export folder") and a tray icon. Runs in the background and sends the
// newest stock file from the export folder to cacaduu.com every 15 minutes, with the shop owner's own login.
"use strict";

const { app, BrowserWindow, Tray, Menu, ipcMain, dialog, shell, safeStorage, nativeImage } = require("electron");
const fs = require("node:fs");
const path = require("node:path");
const cloud = require("./cloud");
const { checkOnce, CHECK_EVERY_MS } = require("./sync");
const { strings, fill } = require("./strings");

const HIDDEN_ARG = "--hidden";

if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  app.on("second-instance", () => showWindow());
  app.whenReady().then(start);
}

let win = null;
let tray = null;
let quitting = false;
let timer = null;
let checking = false;
let t = strings("en");

// ---------------------------------------------------------------- settings (userData/settings.json)

const settingsFile = () => path.join(app.getPath("userData"), "settings.json");
let settings = {};

function loadSettings() {
  try {
    settings = JSON.parse(fs.readFileSync(settingsFile(), "utf8")) || {};
  } catch {
    settings = {};
  }
}

function saveSettings(patch) {
  settings = { ...settings, ...patch };
  try {
    fs.mkdirSync(path.dirname(settingsFile()), { recursive: true });
    fs.writeFileSync(settingsFile(), JSON.stringify(settings, null, 2), { mode: 0o600 });
  } catch (e) {
    console.error("settings:", e.message);
  }
}

// ---------------------------------------------------------------- the login (refresh token in the OS keychain only)

let session = null; // { access_token, refresh_token, expires_at, email } — in memory

function keepRefreshToken(token) {
  if (token && safeStorage.isEncryptionAvailable()) {
    saveSettings({ refreshToken: safeStorage.encryptString(token).toString("base64") });
  } else {
    saveSettings({ refreshToken: null }); // no secure storage: sign in again after a restart
  }
}

function storedRefreshToken() {
  if (!settings.refreshToken || !safeStorage.isEncryptionAvailable()) return null;
  try {
    return safeStorage.decryptString(Buffer.from(settings.refreshToken, "base64"));
  } catch {
    return null;
  }
}

/** A valid access token, refreshed when it ends within 2 minutes; null when signed out. */
async function accessToken() {
  if (session && session.expires_at - Date.now() > 120_000) return session.access_token;
  const refreshToken = (session && session.refresh_token) || storedRefreshToken();
  if (!refreshToken) return null;
  try {
    session = { ...(await cloud.refresh(refreshToken)), email: settings.email };
    keepRefreshToken(session.refresh_token);
    return session.access_token;
  } catch (e) {
    if (e.status >= 400 && e.status < 500) {
      session = null;
      keepRefreshToken(null);
      return null;
    }
    throw e; // offline: try again at the next check
  }
}

// ---------------------------------------------------------------- state shown in the window

let state = {
  phase: "starting", // starting | signed_out | setup | watching | other_source | error
  message: { key: "starting", values: {} },
  shops: [],
  file: null,
  lastCheck: null,
  nextCheck: null,
  checking: false,
};

function view() {
  return {
    ...state,
    signedIn: Boolean(session || storedRefreshToken()),
    email: settings.email || null,
    shopId: settings.shopId || null,
    folder: settings.folder || null,
    autostart: settings.autostart !== false,
    canAutostart: process.platform === "win32" || process.platform === "darwin",
    strings: t,
    lang: settings.locale || "en",
    statusText: fill(t[state.message.key] ?? state.message.key, state.message.values),
  };
}

function publish(patch = {}) {
  state = { ...state, ...patch };
  if (win && !win.isDestroyed()) win.webContents.send("state", view());
  if (tray) tray.setToolTip(`Cacaduu Upload – ${fill(t[state.message.key] ?? "", state.message.values)}`.slice(0, 127));
}

// ---------------------------------------------------------------- the check loop

function scheduleCheck(ms) {
  clearTimeout(timer);
  timer = setTimeout(runCheck, ms);
  publish({ nextCheck: Date.now() + ms });
}

async function runCheck() {
  if (checking) return;
  checking = true;
  clearTimeout(timer);
  publish({ checking: true });
  let outcome;
  try {
    if (state.shops.length === 0) await loadShops().catch(() => undefined); // e.g. offline at start
    outcome = await checkOnce({
      folder: settings.folder,
      shopId: settings.shopId,
      token: accessToken,
      cloud,
      lastAttempt: settings.lastAttempt && settings.lastAttempt.shopId === settings.shopId ? settings.lastAttempt : null,
      saveAttempt: async (a) => saveSettings({ lastAttempt: { ...a, shopId: settings.shopId } }),
    });
  } catch (e) {
    outcome = { state: "error", message: { key: "send_failed", values: { error: e.message || String(e) } }, next: CHECK_EVERY_MS };
  }
  checking = false;
  publish({
    phase: outcome.state,
    message: outcome.message,
    file: outcome.file || state.file,
    lastCheck: Date.now(),
    checking: false,
  });
  scheduleCheck(outcome.next);
}

async function loadShops() {
  const token = await accessToken();
  if (!token) return publish({ phase: "signed_out", message: { key: "signed_out", values: {} }, shops: [] });
  const shops = await cloud.myShops(token);
  // one shop: chosen for the owner; a shop that is gone: choose again
  if (shops.length === 1 && settings.shopId !== shops[0].id) saveSettings({ shopId: shops[0].id });
  if (settings.shopId && !shops.some((s) => s.id === settings.shopId)) saveSettings({ shopId: null });
  publish({ shops });
  if (shops.length === 0) publish({ phase: "setup", message: { key: "no_shops", values: {} } });
}

// ---------------------------------------------------------------- window and tray

function iconPath(name) {
  return path.join(__dirname, "..", "build", name);
}

function createWindow(show) {
  win = new BrowserWindow({
    width: 460,
    height: 600,
    minWidth: 380,
    minHeight: 480,
    show,
    title: "Cacaduu Upload",
    icon: iconPath("icon.png"),
    autoHideMenuBar: true,
    backgroundColor: "#f7f7f5",
    webPreferences: {
      preload: path.join(__dirname, "preload.js"),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  });
  win.removeMenu();
  win.loadFile(path.join(__dirname, "page", "index.html"));
  // closing the window keeps the uploads running in the background
  win.on("close", (event) => {
    if (quitting) return;
    event.preventDefault();
    win.hide();
    if (process.platform === "darwin" && app.dock) app.dock.hide();
  });
  // links open in the normal browser, never inside the app
  win.webContents.setWindowOpenHandler(({ url }) => {
    if (/^https:\/\//.test(url)) shell.openExternal(url);
    return { action: "deny" };
  });
  win.webContents.on("will-navigate", (event) => event.preventDefault());
}

function showWindow() {
  if (!win || win.isDestroyed()) createWindow(true);
  if (process.platform === "darwin" && app.dock) app.dock.show();
  win.show();
  win.focus();
}

function createTray() {
  const image = nativeImage.createFromPath(iconPath(process.platform === "darwin" ? "trayTemplate.png" : "tray.png"));
  if (process.platform === "darwin") image.setTemplateImage(true);
  tray = new Tray(image);
  tray.setToolTip("Cacaduu Upload");
  tray.setContextMenu(
    Menu.buildFromTemplate([
      { label: t.tray_open, click: showWindow },
      { label: t.tray_check, click: () => runCheck() },
      { type: "separator" },
      {
        label: t.tray_quit,
        click: () => {
          quitting = true;
          app.quit();
        },
      },
    ]),
  );
  tray.on("click", showWindow);
}

function applyAutostart() {
  if (!(process.platform === "win32" || process.platform === "darwin")) return;
  const enabled = settings.autostart !== false;
  app.setLoginItemSettings({ openAtLogin: enabled, args: [HIDDEN_ARG] });
}

function startedHidden() {
  if (process.argv.includes(HIDDEN_ARG)) return true;
  if (process.platform === "darwin") {
    try {
      return app.getLoginItemSettings().wasOpenedAtLogin === true;
    } catch {
      return false;
    }
  }
  return false;
}

// ---------------------------------------------------------------- IPC (the page's few buttons)

function registerIpc() {
  ipcMain.handle("state", () => view());

  ipcMain.handle("sign-in", async (_e, { email, password }) => {
    try {
      session = await cloud.signIn(String(email || "").trim(), String(password || ""));
    } catch (e) {
      const bad = e.status === 400 && /invalid/i.test(`${e.code} ${e.message}`);
      return { ok: false, error: bad ? t.bad_login : fill(t.login_failed, { error: e.message }) };
    }
    saveSettings({ email: session.email || String(email).trim() });
    keepRefreshToken(session.refresh_token);
    try {
      await loadShops();
    } catch (e) {
      publish({ phase: "error", message: { key: "send_failed", values: { error: e.message } } });
    }
    runCheck();
    return { ok: true };
  });

  ipcMain.handle("sign-out", async () => {
    const token = session && session.access_token;
    session = null;
    keepRefreshToken(null);
    saveSettings({ lastAttempt: null });
    if (token) await cloud.signOut(token);
    publish({ phase: "signed_out", message: { key: "signed_out", values: {} }, shops: [], file: null });
    return view();
  });

  ipcMain.handle("choose-shop", (_e, shopId) => {
    if (state.shops.some((s) => s.id === shopId)) saveSettings({ shopId });
    runCheck();
    return view();
  });

  ipcMain.handle("choose-folder", async () => {
    const picked = await dialog.showOpenDialog(win, {
      properties: ["openDirectory"],
      defaultPath: settings.folder || (process.platform === "win32" ? "C:\\" : app.getPath("home")),
    });
    if (!picked.canceled && picked.filePaths[0]) {
      saveSettings({ folder: picked.filePaths[0] });
      runCheck();
    }
    return view();
  });

  ipcMain.handle("check-now", () => {
    runCheck();
    return view();
  });

  ipcMain.handle("set-autostart", (_e, enabled) => {
    saveSettings({ autostart: Boolean(enabled) });
    applyAutostart();
    return view();
  });

  ipcMain.handle("open-site", () => shell.openExternal(`${cloud.CLOUD.site}/${settings.locale === "sk" ? "sk" : "en"}/sync`));
}

// ---------------------------------------------------------------- start

async function start() {
  if (process.platform === "win32") app.setAppUserModelId("com.cacaduu.upload");
  loadSettings();
  const locale = app.getPreferredSystemLanguages()[0] || app.getLocale();
  t = strings(locale);
  saveSettings({ locale: String(locale).toLowerCase().startsWith("sk") ? "sk" : "en" });
  applyAutostart();
  registerIpc();
  createTray();

  const hidden = startedHidden() && Boolean(settings.folder && settings.shopId && storedRefreshToken());
  createWindow(!hidden);
  if (hidden && process.platform === "darwin" && app.dock) app.dock.hide();

  try {
    await loadShops();
  } catch (e) {
    publish({ phase: "error", message: { key: "send_failed", values: { error: e.message } } });
  }
  runCheck();
}

app.on("before-quit", () => {
  quitting = true;
});
app.on("activate", showWindow); // macOS: clicking the app again
app.on("window-all-closed", () => {
  // stays in the tray; only "Quit" ends it
});
