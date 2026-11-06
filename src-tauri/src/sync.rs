//! The folder watcher, with the same rules as the website's PPI window (FolderSync): the newest stock file directly
//! in the export folder, untouched for a minute, newer than what Cacaduu applied last, and not already sent while
//! nothing changed on Cacaduu's side. The folder is only ever read.

use crate::cloud::{Cloud, CloudError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub const CHECK_EVERY_MS: u64 = 15 * 60_000;
/// A file must be untouched this long before it is sent, so a half-written export is never sent.
pub const SETTLE_MS: i64 = 60_000;
pub const RECHECK_SOON_MS: u64 = 60_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub key: String,
    pub values: BTreeMap<String, String>,
}

impl Message {
    pub fn new(key: &str) -> Self {
        Message { key: key.into(), values: BTreeMap::new() }
    }
    pub fn with(mut self, k: &str, v: impl ToString) -> Self {
        self.values.insert(k.into(), v.to_string());
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileInfo {
    pub name: String,
    pub mtime_ms: i64,
}

/// The last file sent for this shop, so an unchanged situation is not sent again every 15 minutes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Attempt {
    pub shop_id: String,
    pub name: String,
    pub mtime_ms: i64,
    pub size: u64,
    pub mapping_status: Option<String>,
    pub result: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// setup | signed_out | watching | other_source | error
    pub state: &'static str,
    pub message: Message,
    pub next_ms: u64,
    pub file: Option<FileInfo>,
    /// A file was sent: remember it.
    pub attempt: Option<Attempt>,
}

fn later(state: &'static str, message: Message, next_ms: u64, file: Option<FileInfo>) -> Outcome {
    Outcome { state, message, next_ms, file, attempt: None }
}

pub struct Found {
    pub name: String,
    pub path: PathBuf,
    pub mtime_ms: i64,
    pub size: u64,
}

fn mtime_ms(meta: &std::fs::Metadata) -> Option<i64> {
    meta.modified().ok()?.duration_since(UNIX_EPOCH).ok().map(|d| d.as_millis() as i64)
}

/// Stock files stock-pull can read (old binary .xls is not one of them); never Cacaduu's own sklad.<lang>.xlsx,
/// Excel's ~$ lock files or hidden files.
pub fn is_stock_file(name: &str) -> bool {
    let lower = name.to_lowercase();
    let ext_ok = [".xml", ".csv", ".txt", ".xlsx"].iter().any(|e| lower.ends_with(e));
    let own = lower.strip_prefix("sklad.").and_then(|r| r.strip_suffix(".xlsx")).is_some_and(|lang| {
        let mut parts = lang.split('-');
        let main = parts.next().unwrap_or("");
        let rest: Vec<&str> = parts.collect();
        (2..=3).contains(&main.len())
            && main.chars().all(|c| c.is_ascii_lowercase())
            && rest.len() <= 1
            && rest.iter().all(|r| (2..=4).contains(&r.len()) && r.chars().all(|c| c.is_ascii_lowercase()))
    });
    ext_ok && !own && !name.starts_with("~$") && !name.starts_with('.')
}

/// The most recently written stock file directly in the folder (subfolders ignored).
pub fn newest_stock_file(folder: &Path) -> std::io::Result<Option<Found>> {
    let mut newest: Option<Found> = None;
    for entry in std::fs::read_dir(folder)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().to_string();
        if !is_stock_file(&name) {
            continue;
        }
        // being written or locked right now: the next check picks it up
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Some(m) = mtime_ms(&meta) else { continue };
        if newest.as_ref().map_or(true, |n| m > n.mtime_ms) {
            newest = Some(Found { name, path: entry.path(), mtime_ms: m, size: meta.len() });
        }
    }
    Ok(newest)
}

/// What a stock-pull result says, as a status line.
pub fn result_message(result: &Value) -> (&'static str, Message) {
    let n = |k: &str| result.get(k).and_then(Value::as_i64).unwrap_or(0);
    match result.get("status").and_then(Value::as_str).unwrap_or("") {
        "updated" => ("watching", Message::new("result_updated").with("items", n("items")).with("zeroed", n("zeroed"))),
        s @ ("proposed" | "waiting_for_approval" | "layout_changed") => ("watching", Message::new(&format!("result_{s}")).with("rows", n("rows"))),
        "error" => ("error", Message::new("result_error").with("error", result.get("error").and_then(Value::as_str).unwrap_or(""))),
        _ => ("watching", Message::new("result_unchanged")),
    }
}

fn failed(e: &CloudError) -> Outcome {
    match e.code.as_str() {
        c @ ("api_connected" | "agent_connected" | "sklad_active") => later("other_source", Message::new(c), CHECK_EVERY_MS, None),
        _ if e.status == 401 => later("signed_out", Message::new("signed_out"), CHECK_EVERY_MS, None),
        _ if e.status == 403 => later("error", Message::new("not_owner"), CHECK_EVERY_MS, None),
        _ => later("error", Message::new("send_failed").with("error", &e.message), CHECK_EVERY_MS, None),
    }
}

/// One check: the shop, the folder and a valid access token (None = signed out).
pub async fn check_once(
    cloud: &Cloud,
    shop_id: Option<&str>,
    folder: Option<&Path>,
    token: Option<&str>,
    last_attempt: Option<&Attempt>,
    now_ms: i64,
) -> Outcome {
    let Some(shop_id) = shop_id else { return later("setup", Message::new("pick_shop"), CHECK_EVERY_MS, None) };
    let Some(folder) = folder else { return later("setup", Message::new("pick_folder"), CHECK_EVERY_MS, None) };
    let Some(token) = token else { return later("signed_out", Message::new("signed_out"), CHECK_EVERY_MS, None) };
    if !folder.is_dir() {
        return later("error", Message::new("folder_missing").with("folder", folder.display()), CHECK_EVERY_MS, None);
    }

    let server = match cloud.check_in(token, shop_id).await {
        Ok(s) => s,
        Err(e) => return failed(&e),
    };

    let found = match newest_stock_file(folder) {
        Ok(Some(f)) => f,
        Ok(None) => return later("watching", Message::new("no_file"), CHECK_EVERY_MS, None),
        Err(_) => return later("error", Message::new("folder_missing").with("folder", folder.display()), CHECK_EVERY_MS, None),
    };
    let info = Some(FileInfo { name: found.name.clone(), mtime_ms: found.mtime_ms });
    if now_ms - found.mtime_ms < SETTLE_MS {
        return later("watching", Message::new("settling"), RECHECK_SOON_MS, info);
    }

    let applied = server
        .get("latest_file_time")
        .and_then(Value::as_str)
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.timestamp_millis())
        .unwrap_or(0);
    if found.mtime_ms <= applied {
        return later("watching", Message::new("up_to_date"), CHECK_EVERY_MS, info);
    }

    // Already sent and nothing changed on Cacaduu's side (e.g. still waiting for the owner to approve the columns).
    let mapping = server.get("mapping_status").and_then(Value::as_str).map(str::to_string);
    if let Some(prev) = last_attempt {
        if prev.shop_id == shop_id && prev.name == found.name && prev.mtime_ms == found.mtime_ms && prev.size == found.size && prev.mapping_status == mapping {
            let (state, message) = result_message(&prev.result);
            return later(state, message, CHECK_EVERY_MS, info);
        }
    }

    let data = match std::fs::read(&found.path) {
        Ok(d) => d,
        Err(_) => return later("watching", Message::new("settling"), RECHECK_SOON_MS, info), // locked by the stock software
    };
    let again = std::fs::metadata(&found.path).ok();
    if again.as_ref().and_then(mtime_ms) != Some(found.mtime_ms) || again.map(|m| m.len()) != Some(found.size) {
        return later("watching", Message::new("settling"), RECHECK_SOON_MS, info); // written again while read
    }

    let result = match cloud.upload(token, shop_id, &found.name, found.mtime_ms, &data).await {
        Ok(r) => r,
        Err(e) => return failed(&e),
    };
    // the file is in; if this fails, the status line catches up at the next check
    let mapping_after = match cloud.check_in(token, shop_id).await {
        Ok(after) => after.get("mapping_status").and_then(Value::as_str).map(str::to_string),
        Err(_) => mapping,
    };
    let (state, message) = result_message(&result);
    Outcome {
        state,
        message,
        next_ms: CHECK_EVERY_MS,
        file: info,
        attempt: Some(Attempt { shop_id: shop_id.into(), name: found.name, mtime_ms: found.mtime_ms, size: found.size, mapping_status: mapping_after, result }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::Cloud;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime};

    /// A stand-in for Supabase: answers by path; records every request.
    #[derive(Clone, Default)]
    struct Seen(Arc<Mutex<Vec<(String, String, Vec<u8>)>>>); // (path, authorization, body)

    fn fake_supabase(check_in: &'static str, upload: &'static str) -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let seen = Seen::default();
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split(' ').nth(1).unwrap_or("").to_string();
                let (mut len, mut auth) = (0usize, String::new());
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h == "\r\n" || h.is_empty() {
                        break;
                    }
                    let lower = h.to_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    if lower.starts_with("authorization:") {
                        auth = h["authorization:".len()..].trim().to_string();
                    }
                }
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                let (status, answer) = if path.starts_with("/auth/v1/token?grant_type=password") {
                    if String::from_utf8_lossy(&body).contains("\"right\"") {
                        ("200 OK", r#"{"access_token":"acc","refresh_token":"ref","expires_in":3600,"user":{"email":"a@b.sk"}}"#)
                    } else {
                        ("400 Bad Request", r#"{"code":400,"error_code":"invalid_credentials","msg":"Invalid login credentials"}"#)
                    }
                } else if path == "/rest/v1/rpc/my_shops" {
                    ("200 OK", r#"[{"id":"s1","name":"Shop","slug":"shop"}]"#)
                } else if path == "/rest/v1/rpc/upload_check_in" {
                    if check_in.starts_with('{') && check_in.contains("\"message\"") {
                        ("400 Bad Request", check_in)
                    } else {
                        ("200 OK", check_in)
                    }
                } else if path.starts_with("/functions/v1/stock-pull?") {
                    ("200 OK", upload)
                } else {
                    ("404 Not Found", "{}")
                };
                log.0.lock().unwrap().push((path, auth, body));
                let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}", answer.len());
            }
        });
        (addr, seen)
    }

    fn folder_with(files: &[(&str, &str, u64)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cacaduu-test-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, content, age_ms) in files {
            let p = dir.join(name);
            std::fs::write(&p, content).unwrap();
            let f = std::fs::File::options().write(true).open(&p).unwrap();
            f.set_modified(SystemTime::now() - Duration::from_millis(*age_ms)).unwrap();
        }
        dir
    }

    fn rand_suffix() -> u128 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    }

    fn now() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    const OK: &str = r#"{"latest_file_time":null,"mapping_status":"confirmed"}"#;
    const UPDATED: &str = r#"{"result":{"status":"updated","items":3,"zeroed":1,"skipped":0}}"#;

    #[test]
    fn stock_file_names() {
        for yes in ["stock.csv", "Stock.XML", "a.txt", "sklad.xlsx", "sklad.export.xlsx"] {
            assert!(is_stock_file(yes), "{yes}");
        }
        for no in ["sklad.sk.xlsx", "sklad.zh-hans.xlsx", "~$stock.xlsx", ".hidden.csv", "notes.pdf", "old.xls"] {
            assert!(!is_stock_file(no), "{no}");
        }
    }

    #[test]
    fn newest_file_wins_and_subfolders_are_ignored() {
        let dir = folder_with(&[("old.csv", "a", 600_000), ("stock.xml", "b", 300_000), ("sklad.sk.xlsx", "own", 1_000), ("notes.pdf", "x", 1_000)]);
        std::fs::create_dir(dir.join("sub.csv")).unwrap();
        assert_eq!(newest_stock_file(&dir).unwrap().unwrap().name, "stock.xml");
    }

    #[tokio::test]
    async fn sends_a_settled_new_file_gzipped_with_its_name_and_time() {
        let (addr, seen) = fake_supabase(OK, UPDATED);
        let cloud = Cloud::new(&addr, "key");
        let dir = folder_with(&[("stock.csv", "code;name;qty\n1;A;2\n", 120_000)]);
        let out = check_once(&cloud, Some("s1"), Some(&dir), Some("acc"), None, now()).await;
        assert_eq!(out.message, Message::new("result_updated").with("items", 3).with("zeroed", 1));
        assert_eq!(out.next_ms, CHECK_EVERY_MS);
        let attempt = out.attempt.expect("remembered");
        assert_eq!(attempt.name, "stock.csv");

        let log = seen.0.lock().unwrap();
        let (path, auth, body) = log.iter().find(|r| r.0.starts_with("/functions/v1/stock-pull")).unwrap();
        let q: BTreeMap<String, String> = url::form_urlencoded::parse(path.split_once('?').unwrap().1.as_bytes()).into_owned().collect();
        assert_eq!(q["shop_id"], "s1");
        assert_eq!(q["file_name"], "stock.csv");
        assert_eq!(q["gzip"], "1");
        assert_eq!(chrono::DateTime::parse_from_rfc3339(&q["file_time"]).unwrap().timestamp_millis(), attempt.mtime_ms);
        assert_eq!(auth, "Bearer acc");
        let mut text = String::new();
        flate2::read::GzDecoder::new(&body[..]).read_to_string(&mut text).unwrap();
        assert_eq!(text, "code;name;qty\n1;A;2\n");
    }

    #[tokio::test]
    async fn waits_for_a_file_written_less_than_a_minute_ago() {
        let (addr, seen) = fake_supabase(OK, UPDATED);
        let dir = folder_with(&[("stock.csv", "x", 5_000)]);
        let out = check_once(&Cloud::new(&addr, "k"), Some("s1"), Some(&dir), Some("acc"), None, now()).await;
        assert_eq!(out.message.key, "settling");
        assert_eq!(out.next_ms, RECHECK_SOON_MS);
        assert!(!seen.0.lock().unwrap().iter().any(|r| r.0.contains("stock-pull")));
    }

    #[tokio::test]
    async fn does_not_send_a_file_cacaduu_already_applied() {
        let applied: &'static str = Box::leak(format!(r#"{{"latest_file_time":"{}","mapping_status":"confirmed"}}"#, chrono::Utc::now().to_rfc3339()).into_boxed_str());
        let (addr, seen) = fake_supabase(applied, UPDATED);
        let dir = folder_with(&[("stock.csv", "x", 120_000)]);
        let out = check_once(&Cloud::new(&addr, "k"), Some("s1"), Some(&dir), Some("acc"), None, now()).await;
        assert_eq!(out.message.key, "up_to_date");
        assert!(!seen.0.lock().unwrap().iter().any(|r| r.0.contains("stock-pull")));
    }

    #[tokio::test]
    async fn does_not_resend_while_waiting_for_the_column_approval() {
        let dir = folder_with(&[("stock.csv", "x", 120_000)]);
        let found = newest_stock_file(&dir).unwrap().unwrap();
        let last = Attempt {
            shop_id: "s1".into(),
            name: "stock.csv".into(),
            mtime_ms: found.mtime_ms,
            size: found.size,
            mapping_status: Some("proposed".into()),
            result: serde_json::json!({ "status": "proposed", "rows": 9 }),
        };
        let (addr, seen) = fake_supabase(r#"{"latest_file_time":null,"mapping_status":"proposed"}"#, UPDATED);
        let out = check_once(&Cloud::new(&addr, "k"), Some("s1"), Some(&dir), Some("acc"), Some(&last), now()).await;
        assert_eq!(out.message, Message::new("result_proposed").with("rows", 9));
        assert!(!seen.0.lock().unwrap().iter().any(|r| r.0.contains("stock-pull")));
        // once the owner approved the columns, the same file goes again
        let (addr, seen) = fake_supabase(OK, UPDATED);
        check_once(&Cloud::new(&addr, "k"), Some("s1"), Some(&dir), Some("acc"), Some(&last), now()).await;
        assert!(seen.0.lock().unwrap().iter().any(|r| r.0.contains("stock-pull")));
    }

    #[tokio::test]
    async fn setup_states_and_another_stock_source() {
        let (addr, _) = fake_supabase(r#"{"code":"55000","message":"agent_connected","hint":"…"}"#, UPDATED);
        let cloud = Cloud::new(&addr, "k");
        let dir = folder_with(&[("stock.csv", "x", 120_000)]);
        assert_eq!(check_once(&cloud, None, Some(&dir), Some("a"), None, now()).await.message.key, "pick_shop");
        assert_eq!(check_once(&cloud, Some("s1"), None, Some("a"), None, now()).await.message.key, "pick_folder");
        assert_eq!(check_once(&cloud, Some("s1"), Some(&dir), None, None, now()).await.state, "signed_out");
        assert_eq!(check_once(&cloud, Some("s1"), Some(&dir.join("gone")), Some("a"), None, now()).await.message.key, "folder_missing");
        let out = check_once(&cloud, Some("s1"), Some(&dir), Some("a"), None, now()).await;
        assert_eq!((out.state, out.message.key.as_str()), ("other_source", "agent_connected"));
    }

    #[tokio::test]
    async fn sign_in_and_shops() {
        let (addr, _) = fake_supabase(OK, UPDATED);
        let cloud = Cloud::new(&addr, "k");
        let e = cloud.sign_in("a@b.sk", "wrong").await.unwrap_err();
        assert_eq!((e.status, e.code.as_str()), (400, "invalid_credentials"));
        let s = cloud.sign_in("a@b.sk", "right").await.unwrap();
        assert_eq!((s.access_token.as_str(), s.refresh_token.as_str(), s.email.as_deref()), ("acc", "ref", Some("a@b.sk")));
        let shops = cloud.my_shops("acc").await.unwrap();
        assert_eq!(shops, vec![crate::cloud::Shop { id: "s1".into(), name: "Shop".into() }]);
    }
}
