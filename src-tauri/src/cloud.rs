//! Talks to Cacaduu (cacaduu.com) the same way the website's "Priečinok s exportom" page does: the owner's own
//! login, public.my_shops(), public.upload_check_in() and the stock-pull function. Only public settings live here:
//! the project address and its publishable key. No service key, ever.

use flate2::{write::GzEncoder, Compression};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Write;
use std::time::Duration;

pub const SUPABASE_URL: &str = "https://szhotccxyaepjhwsqdqx.supabase.co";
pub const SUPABASE_KEY: &str = "sb_publishable_YbIA8qxoTMefxv70bkZmQg_87Q7M0c7";
pub const SITE: &str = "https://cacaduu.com";

#[derive(Debug, Clone)]
pub struct CloudError {
    /// HTTP status, 0 when no answer came.
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for CloudError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Shop {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix milliseconds.
    pub expires_at: i64,
    pub email: Option<String>,
}

#[derive(Clone)]
pub struct Cloud {
    base: String,
    key: String,
    http: reqwest::Client,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl Cloud {
    pub fn new(base: &str, key: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .user_agent(concat!("CacaduuUpload/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("http client");
        Cloud { base: base.trim_end_matches('/').to_string(), key: key.to_string(), http }
    }

    /// The real Cacaduu, or another address for tests (CACADUU_SUPABASE_URL).
    pub fn production() -> Self {
        let base = std::env::var("CACADUU_SUPABASE_URL").unwrap_or_else(|_| SUPABASE_URL.to_string());
        Cloud::new(&base, SUPABASE_KEY)
    }

    async fn call(&self, path: &str, token: Option<&str>, body: Body) -> Result<Value, CloudError> {
        let mut req = self.http.post(format!("{}{}", self.base, path)).header("apikey", &self.key);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req = match body {
            Body::Json(v) => req.header("Content-Type", "application/json").body(v.to_string()),
            Body::Bytes(b) => req.header("Content-Type", "application/octet-stream").body(b),
        };
        let response = req.send().await.map_err(|e| CloudError {
            status: 0,
            code: "network".into(),
            message: if e.is_timeout() { "no answer".into() } else { "no connection".into() },
        })?;
        let status = response.status().as_u16();
        let data: Value = response.json().await.unwrap_or(Value::Null);
        if !(200..300).contains(&status) {
            let pick = |keys: &[&str]| keys.iter().find_map(|k| data.get(*k).and_then(text)).unwrap_or_else(|| format!("HTTP {status}"));
            return Err(CloudError {
                status,
                code: pick(&["error_code", "error", "code"]),
                message: pick(&["message", "msg", "error_description", "error"]),
            });
        }
        Ok(data)
    }

    /// E-mail + password → a session.
    pub async fn sign_in(&self, email: &str, password: &str) -> Result<Session, CloudError> {
        let data = self.call("/auth/v1/token?grant_type=password", None, Body::Json(json!({ "email": email, "password": password }))).await?;
        session(&data)
    }

    /// A stored refresh token → a new session (Supabase rotates the refresh token every time).
    pub async fn refresh(&self, refresh_token: &str) -> Result<Session, CloudError> {
        let data = self.call("/auth/v1/token?grant_type=refresh_token", None, Body::Json(json!({ "refresh_token": refresh_token }))).await?;
        session(&data)
    }

    pub async fn sign_out(&self, access_token: &str) {
        let _ = self.call("/auth/v1/logout", Some(access_token), Body::Json(json!({}))).await;
    }

    /// The shops of the signed-in owner.
    pub async fn my_shops(&self, access_token: &str) -> Result<Vec<Shop>, CloudError> {
        let rows = self.call("/rest/v1/rpc/my_shops", Some(access_token), Body::Json(json!({}))).await?;
        Ok(rows
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| Some(Shop { id: s.get("id")?.as_str()?.to_string(), name: s.get("name").and_then(text).unwrap_or_default() }))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Tells Cacaduu this PC watches the folder and returns what was applied last ({ latest_file_time, mapping_status,
    /// … }). Refused with api_connected / agent_connected / sklad_active when the stock comes from somewhere else.
    pub async fn check_in(&self, access_token: &str, shop_id: &str) -> Result<Value, CloudError> {
        self.call("/rest/v1/rpc/upload_check_in", Some(access_token), Body::Json(json!({ "p_shop_id": shop_id })))
            .await
            .map_err(|mut e| {
                if matches!(e.message.as_str(), "api_connected" | "agent_connected" | "sklad_active") {
                    e.code = e.message.clone();
                }
                e
            })
    }

    /// Sends one stock file (gzipped) to stock-pull → the outcome ({ status: "updated" | "proposed" | … }).
    pub async fn upload(&self, access_token: &str, shop_id: &str, name: &str, mtime_ms: i64, data: &[u8]) -> Result<Value, CloudError> {
        let file_time = chrono::DateTime::from_timestamp_millis(mtime_ms)
            .unwrap_or_default()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let query: String = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("shop_id", shop_id)
            .append_pair("file_time", &file_time)
            .append_pair("file_name", name)
            .append_pair("gzip", "1")
            .finish();
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(data).map_err(|e| CloudError { status: 0, code: "gzip".into(), message: e.to_string() })?;
        let body = gz.finish().map_err(|e| CloudError { status: 0, code: "gzip".into(), message: e.to_string() })?;
        let answer = self.call(&format!("/functions/v1/stock-pull?{query}"), Some(access_token), Body::Bytes(body)).await?;
        answer.get("result").cloned().filter(|r| r.is_object()).ok_or(CloudError { status: 0, code: "bad_answer".into(), message: "no result".into() })
    }
}

enum Body {
    Json(Value),
    Bytes(Vec<u8>),
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn session(data: &Value) -> Result<Session, CloudError> {
    let get = |k: &str| data.get(k).and_then(|v| v.as_str()).map(str::to_string);
    match (get("access_token"), get("refresh_token")) {
        (Some(access_token), Some(refresh_token)) => Ok(Session {
            access_token,
            refresh_token,
            expires_at: now_ms() + data.get("expires_in").and_then(Value::as_i64).unwrap_or(3600).max(60) * 1000,
            email: data.get("user").and_then(|u| u.get("email")).and_then(|e| e.as_str()).map(str::to_string),
        }),
        _ => Err(CloudError { status: 0, code: "bad_session".into(), message: "sign-in failed".into() }),
    }
}
