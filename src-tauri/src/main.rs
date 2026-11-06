//! Cacaduu Upload: one small window ("Export folder") and a tray icon. Runs in the background and sends the newest
//! stock file from the export folder to cacaduu.com every 15 minutes, with the shop owner's own login.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cloud;
mod sync;

use cloud::{Cloud, Session, Shop};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use sync::{Attempt, FileInfo, Message, CHECK_EVERY_MS};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;
use tokio::sync::Notify;

const HIDDEN_ARG: &str = "--hidden";
const KEYRING_SERVICE: &str = "Cacaduu Upload";
const KEYRING_USER: &str = "refresh-token";

static STRINGS: &str = include_str!("../strings.json");

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// ---------------------------------------------------------------- settings (app config dir / settings.json)

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    email: Option<String>,
    shop_id: Option<String>,
    folder: Option<PathBuf>,
    autostart: bool,
    last_attempt: Option<Attempt>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { email: None, shop_id: None, folder: None, autostart: true, last_attempt: None }
    }
}

// ---------------------------------------------------------------- the refresh token: OS keychain only

fn keychain() -> Option<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).ok()
}

fn stored_refresh_token() -> Option<String> {
    keychain()?.get_password().ok()
}

/// Keeps the refresh token in Windows Credential Manager / macOS Keychain. Without one: sign in again after a restart.
fn keep_refresh_token(token: Option<&str>) {
    if let Some(entry) = keychain() {
        let _ = match token {
            Some(t) => entry.set_password(t),
            None => entry.delete_credential(),
        };
    }
}

// ---------------------------------------------------------------- the app's state

struct Core {
    settings: Settings,
    settings_file: PathBuf,
    session: Option<Session>,
    lang: &'static str,
    strings: Map<String, Value>,
    phase: &'static str,
    message: Message,
    shops: Vec<Shop>,
    /// my_shops() answered since the last sign-in.
    shops_loaded: bool,
    file: Option<FileInfo>,
    last_check: Option<i64>,
    next_check: Option<i64>,
    checking: bool,
}

struct AppState {
    core: Mutex<Core>,
    cloud: Cloud,
    wake: Notify,
    /// Serialises token refreshes (Supabase rotates the refresh token: two refreshes at once would sign us out).
    auth: tokio::sync::Mutex<()>,
}

type Shared<'a> = State<'a, Arc<AppState>>;

impl Core {
    fn save(&self) {
        if let Some(dir) = self.settings_file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(&self.settings) {
            let _ = std::fs::write(&self.settings_file, text);
        }
    }

    fn t(&self, key: &str) -> String {
        self.strings.get(key).and_then(Value::as_str).unwrap_or(key).to_string()
    }

    fn status_text(&self) -> String {
        let mut text = self.t(&self.message.key);
        for (k, v) in &self.message.values {
            text = text.replace(&format!("{{{k}}}"), v);
        }
        text
    }

    fn signed_in(&self) -> bool {
        self.session.is_some()
    }

    fn view(&self) -> Value {
        json!({
            "phase": self.phase,
            "message": self.message,
            "statusText": self.status_text(),
            "shops": self.shops,
            "file": self.file,
            "lastCheck": self.last_check,
            "nextCheck": self.next_check,
            "checking": self.checking,
            "signedIn": self.signed_in(),
            "email": self.settings.email,
            "shopId": self.settings.shop_id,
            "folder": self.settings.folder.as_ref().map(|f| f.display().to_string()),
            "autostart": self.settings.autostart,
            "canAutostart": cfg!(any(target_os = "windows", target_os = "macos")),
            "strings": self.strings,
            "lang": self.lang,
        })
    }
}

fn publish(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>();
    let (view, tip) = {
        let core = state.core.lock().unwrap();
        (core.view(), format!("Cacaduu Upload – {}", core.status_text()))
    };
    let _ = app.emit("state", view);
    if let Some(tray) = app.tray_by_id("main") {
        let tip: String = tip.chars().take(120).collect();
        let _ = tray.set_tooltip(Some(tip));
    }
}

// ---------------------------------------------------------------- the login

/// A valid access token, refreshed when it ends within 2 minutes; None when signed out (or offline: Err).
async fn access_token(state: &AppState) -> Result<Option<String>, String> {
    let _one_at_a_time = state.auth.lock().await;
    let refresh_token = {
        let core = state.core.lock().unwrap();
        match &core.session {
            Some(s) if s.expires_at - now_ms() > 120_000 => return Ok(Some(s.access_token.clone())),
            Some(s) => Some(s.refresh_token.clone()),
            None => None,
        }
    }
    .or_else(stored_refresh_token);
    let Some(refresh_token) = refresh_token else { return Ok(None) };
    match state.cloud.refresh(&refresh_token).await {
        Ok(session) => {
            keep_refresh_token(Some(&session.refresh_token));
            let token = session.access_token.clone();
            state.core.lock().unwrap().session = Some(session);
            Ok(Some(token))
        }
        Err(e) if (400..500).contains(&e.status) => {
            keep_refresh_token(None);
            state.core.lock().unwrap().session = None;
            Ok(None)
        }
        Err(e) => Err(e.message), // offline: try again at the next check
    }
}

async fn load_shops(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<Arc<AppState>>().inner().clone();
    let Some(token) = access_token(&state).await? else {
        let mut core = state.core.lock().unwrap();
        core.phase = "signed_out";
        core.message = Message::new("signed_out");
        core.shops.clear();
        core.shops_loaded = false;
        return Ok(());
    };
    let shops = state.cloud.my_shops(&token).await.map_err(|e| e.message)?;
    let mut core = state.core.lock().unwrap();
    // one shop: chosen for the owner; a shop that is gone: choose again
    if shops.len() == 1 && core.settings.shop_id.as_deref() != Some(&shops[0].id) {
        core.settings.shop_id = Some(shops[0].id.clone());
    }
    if let Some(id) = core.settings.shop_id.clone() {
        if !shops.iter().any(|s| s.id == id) {
            core.settings.shop_id = None;
        }
    }
    if shops.is_empty() {
        core.phase = "setup";
        core.message = Message::new("no_shops");
    }
    core.shops = shops;
    core.shops_loaded = true;
    core.save();
    Ok(())
}

// ---------------------------------------------------------------- the check loop

async fn run_check(app: &AppHandle) -> u64 {
    let state = app.state::<Arc<AppState>>().inner().clone();
    state.core.lock().unwrap().checking = true;
    publish(app);

    let needs_shops = !state.core.lock().unwrap().shops_loaded;
    if needs_shops {
        let _ = load_shops(app).await; // e.g. offline at start
    }
    let outcome = match access_token(&state).await {
        Err(e) => sync::Outcome { state: "error", message: Message::new("send_failed").with("error", e), next_ms: CHECK_EVERY_MS, file: None, attempt: None },
        Ok(token) => {
            let (shop, folder, last, no_shops) = {
                let core = state.core.lock().unwrap();
                (
                    core.settings.shop_id.clone(),
                    core.settings.folder.clone(),
                    core.settings.last_attempt.clone(),
                    core.shops_loaded && core.shops.is_empty(),
                )
            };
            if no_shops && token.is_some() {
                sync::Outcome { state: "setup", message: Message::new("no_shops"), next_ms: CHECK_EVERY_MS, file: None, attempt: None }
            } else {
                sync::check_once(&state.cloud, shop.as_deref(), folder.as_deref(), token.as_deref(), last.as_ref(), now_ms()).await
            }
        }
    };

    {
        let mut core = state.core.lock().unwrap();
        core.phase = outcome.state;
        core.message = outcome.message;
        if outcome.file.is_some() {
            core.file = outcome.file;
        }
        if let Some(a) = outcome.attempt {
            core.settings.last_attempt = Some(a);
            core.save();
        }
        core.checking = false;
        core.last_check = Some(now_ms());
        core.next_check = Some(now_ms() + outcome.next_ms as i64);
    }
    publish(app);
    outcome.next_ms
}

fn start_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            let wait = run_check(&app).await;
            let state = app.state::<Arc<AppState>>().inner().clone();
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(wait)) => {}
                _ = state.wake.notified() => {}
            }
        }
    });
}

fn check_soon(state: &AppState) {
    state.wake.notify_one();
}

// ---------------------------------------------------------------- the page's few buttons

#[tauri::command]
fn get_state(state: Shared) -> Value {
    state.core.lock().unwrap().view()
}

#[tauri::command]
async fn sign_in(app: AppHandle, state: Shared<'_>, email: String, password: String) -> Result<Value, String> {
    let email = email.trim().to_string();
    match state.cloud.sign_in(&email, &password).await {
        Ok(session) => {
            keep_refresh_token(Some(&session.refresh_token));
            let mut core = state.core.lock().unwrap();
            core.settings.email = Some(session.email.clone().unwrap_or(email));
            core.session = Some(session);
            core.shops.clear();
            core.shops_loaded = false;
            core.save();
        }
        Err(e) => {
            let core = state.core.lock().unwrap();
            let bad = e.status == 400 && format!("{} {}", e.code, e.message).to_lowercase().contains("invalid");
            return Err(if bad { core.t("bad_login") } else { core.t("login_failed").replace("{error}", &e.message) });
        }
    }
    if let Err(e) = load_shops(&app).await {
        let mut core = state.core.lock().unwrap();
        core.phase = "error";
        core.message = Message::new("send_failed").with("error", e);
    }
    check_soon(&state);
    publish(&app);
    Ok(state.core.lock().unwrap().view())
}

#[tauri::command]
async fn sign_out(app: AppHandle, state: Shared<'_>) -> Result<Value, String> {
    let token = {
        let mut core = state.core.lock().unwrap();
        let token = core.session.take().map(|s| s.access_token);
        core.settings.last_attempt = None;
        core.shops.clear();
        core.shops_loaded = false;
        core.file = None;
        core.phase = "signed_out";
        core.message = Message::new("signed_out");
        core.save();
        token
    };
    keep_refresh_token(None);
    if let Some(t) = token {
        state.cloud.sign_out(&t).await;
    }
    publish(&app);
    Ok(state.core.lock().unwrap().view())
}

#[tauri::command]
fn choose_shop(app: AppHandle, state: Shared, id: String) -> Value {
    {
        let mut core = state.core.lock().unwrap();
        if core.shops.iter().any(|s| s.id == id) {
            core.settings.shop_id = Some(id);
            core.save();
        }
    }
    check_soon(&state);
    publish(&app);
    let view = state.core.lock().unwrap().view();
    view
}

#[tauri::command]
async fn choose_folder(app: AppHandle, state: Shared<'_>) -> Result<Value, String> {
    let start = state.core.lock().unwrap().settings.folder.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut dialog = app.dialog().file();
    if let Some(dir) = start {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_folder(move |picked| {
        let _ = tx.send(picked);
    });
    if let Ok(Some(picked)) = rx.await {
        if let Ok(path) = picked.into_path() {
            let mut core = state.core.lock().unwrap();
            core.settings.folder = Some(path);
            core.save();
            drop(core);
            check_soon(&state);
        }
    }
    publish(&app);
    Ok(state.core.lock().unwrap().view())
}

#[tauri::command]
fn check_now(app: AppHandle, state: Shared) -> Value {
    check_soon(&state);
    publish(&app);
    let view = state.core.lock().unwrap().view();
    view
}

#[tauri::command]
fn set_autostart(app: AppHandle, state: Shared, enabled: bool) -> Value {
    {
        let mut core = state.core.lock().unwrap();
        core.settings.autostart = enabled;
        core.save();
    }
    apply_autostart(&app, enabled);
    let view = state.core.lock().unwrap().view();
    view
}

#[tauri::command]
fn open_site(app: AppHandle, state: Shared) {
    let lang = state.core.lock().unwrap().lang;
    let _ = app.opener().open_url(format!("{}/{}/sync", cloud::SITE, lang), None::<&str>);
}

// ---------------------------------------------------------------- window, tray, autostart

fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn apply_autostart(app: &AppHandle, enabled: bool) {
    if !cfg!(any(target_os = "windows", target_os = "macos")) {
        return;
    }
    let launcher = app.autolaunch();
    let _ = if enabled { launcher.enable() } else { launcher.disable() };
}

fn create_tray(app: &AppHandle, core: &Core) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", core.t("tray_open"), true, None::<&str>)?;
    let check = MenuItem::with_id(app, "check", core.t("tray_check"), true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", core.t("tray_quit"), true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &check, &PredefinedMenuItem::separator(app)?, &quit])?;
    #[cfg(target_os = "macos")]
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/trayTemplate@2x.png"))?;
    #[cfg(not(target_os = "macos"))]
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?;
    TrayIconBuilder::with_id("main")
        .icon(icon)
        .icon_as_template(cfg!(target_os = "macos"))
        .tooltip("Cacaduu Upload")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_window(app),
            "check" => check_soon(&app.state::<Arc<AppState>>()),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn system_lang() -> &'static str {
    let locale = sys_locale::get_locale().unwrap_or_default().to_lowercase();
    if locale.starts_with("sk") {
        "sk"
    } else {
        "en"
    }
}

fn main() {
    tauri::Builder::default()
        // a second start only brings the running app's window forward
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_window(app)))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec![HIDDEN_ARG])))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![get_state, sign_in, sign_out, choose_shop, choose_folder, check_now, set_autostart, open_site])
        .setup(|app| {
            // menu-bar app on macOS: no Dock icon
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let handle = app.handle().clone();
            let settings_file = app.path().app_config_dir()?.join("settings.json");
            let settings: Settings = std::fs::read_to_string(&settings_file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
            let lang = system_lang();
            let all: Value = serde_json::from_str(STRINGS).expect("strings.json");
            let strings = all.get(lang).and_then(Value::as_object).cloned().unwrap_or_default();
            let has_token = stored_refresh_token().is_some();
            let ready = has_token && settings.folder.is_some() && settings.shop_id.is_some();
            let autostart = settings.autostart;
            let core = Core {
                settings,
                settings_file,
                // a placeholder until the first refresh: the window shows "signed in" without a flash of the form
                session: has_token.then(|| Session { access_token: String::new(), refresh_token: stored_refresh_token().unwrap_or_default(), expires_at: 0, email: None }),
                lang,
                strings,
                phase: "starting",
                message: Message::new("starting"),
                shops: Vec::new(),
                shops_loaded: false,
                file: None,
                last_check: None,
                next_check: None,
                checking: false,
            };
            create_tray(&handle, &core)?;
            app.manage(Arc::new(AppState { core: Mutex::new(core), cloud: Cloud::production(), wake: Notify::new(), auth: tokio::sync::Mutex::new(()) }));
            apply_autostart(&handle, autostart);

            let hidden = std::env::args().any(|a| a == HIDDEN_ARG) && ready;
            if !hidden {
                show_window(&handle);
            }
            start_loop(handle);
            Ok(())
        })
        // closing the window keeps the uploads running in the background
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("Cacaduu Upload failed to start");
}
