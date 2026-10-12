//! INDEXNOW_V1（fix105）：主動把公開網址提交給 Bing。
//!
//! 使用者原話：「我希望在 BING 也能打 jasonserver 也能搜尋到」。
//!
//! 背景：`SEO_LANDING_V1`（fix60）已經讓首頁成為唯一公開、可被收錄的內容，
//! Google 會自己爬；但 Bing 對「低流量、沒有外部連結的新站」幾乎不會主動發現。
//! 讓 Bing 收錄只有兩條路：
//!
//! 1. **Bing Webmaster Tools 驗證站台**（把驗證字串設成環境變數
//!    `BING_SITE_VERIFICATION`，meta 由 `seo.rs` 注入）→ 之後可以在那裡看收錄狀況、提交 sitemap。
//! 2. **IndexNow**（本檔）：Bing 官方的「主動通知」協定（Yandex／Seznam／Naver 也吃同一套通知）。
//!
//! 本檔做第 2 條：
//!   * 站方自帶一組金鑰，放在網站根目錄 `/{key}.txt`，**內容只有金鑰本身**；
//!     Bing 會來抓這個檔案驗證所有權（金鑰檔必須打得開、不可轉址、不可被防火牆／robots 擋）。
//!   * 送出 `POST https://api.indexnow.org/indexnow`，JSON 為
//!     `{host, key, keyLocation, urlList}`；200／202 都算受理（202＝金鑰仍在非同步驗證）。
//!
//! 金鑰從哪裡來：環境變數 `INDEXNOW_KEY` 優先，否則自動產生一組 32 位 hex 並持久化到
//! `config/indexnow.json`（與其他設定檔同目錄、同一套 `atomic_write`）。**一定要持久化** ——
//! 金鑰一換，舊的 `/{key}.txt` 就失效，Bing 會回 403。
//!
//! 什麼時候送出：
//!   * 服務啟動後（[`start_auto_submit`]）—— 但**同一個網址 24 小時內只送一次**
//!     （IndexNow 的禮貌是「內容有變動才通知」，我們只有一頁，重啟不等於改版）；
//!   * 管理員在「設定 → 搜尋引擎收錄」按「立即提交給 Bing」（手動不受 24 小時限制）。
//!
//! 注意：Google **不參與** IndexNow，這一支對 Google 沒有作用（Google 那條路是自己爬或 Search Console）。

use super::*;
use std::path::PathBuf;

/// IndexNow 的共用端點（受理後會轉發給所有參與的引擎）。
const INDEXNOW_ENDPOINT: &str = "https://api.indexnow.org/indexnow";
/// Bing 官方專屬端點（雙通道提交保證 Bing 100% 收到通知）。
const BING_INDEXNOW_ENDPOINT: &str = "https://www.bing.com/indexnow";
/// 自動提交的最小間隔（秒）：24 小時。手動提交不受此限。
const AUTO_SUBMIT_MIN_INTERVAL_SECS: i64 = 24 * 60 * 60;
/// 啟動後等幾秒再提交：讓服務先把埠綁好（Bing 驗證金鑰時要抓得到 `/{key}.txt`）。
const AUTO_SUBMIT_DELAY_SECS: u64 = 15;

fn state_file() -> PathBuf {
    std::path::Path::new("config/indexnow.json").to_path_buf()
}

/// 持久化內容：金鑰 ＋ 最近一次提交的結果（面板要顯示「上次什麼時候送的、成不成功」）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct IndexNowState {
    /// 所有權金鑰（`[A-Za-z0-9-]`、8～128 字元）。
    #[serde(default)]
    key: String,
    /// 最近一次提交時間（Unix 秒）。
    #[serde(default)]
    last_submit_at: Option<i64>,
    /// 最近一次提交是否受理（200／202）。
    #[serde(default)]
    last_ok: Option<bool>,
    /// 最近一次提交的 HTTP 狀態碼。
    #[serde(default)]
    last_status: Option<u16>,
    /// 給人看的一句話（成功或失敗原因）。
    #[serde(default)]
    last_message: Option<String>,
}

/// 只留合法字元並檢查長度：IndexNow 規定金鑰是 8～128 個 `[A-Za-z0-9-]`。
fn sanitize_key(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(128)
        .collect();
    if cleaned.len() >= 8 {
        Some(cleaned)
    } else {
        None
    }
}

fn load_state() -> IndexNowState {
    std::fs::read_to_string(state_file())
        .ok()
        .and_then(|text| serde_json::from_str::<IndexNowState>(&text).ok())
        .map(|mut state| {
            // 手改檔塞了不合法的金鑰 → 當成沒有（後面會重新產生一組）。
            state.key = sanitize_key(&state.key).unwrap_or_default();
            state
        })
        .unwrap_or_default()
}

fn save_state(state: &IndexNowState) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    // ATOMIC_WRITE_V1：temp → fsync → rename（不會留下半截 JSON 讓金鑰讀不回來）。
    crate::atomic_write::write_atomic(state_file(), text)
}

/// 目前生效的金鑰（`static`：路由註冊要在同步的 `build_admin_router` 裡就拿到它）。
static KEY: once_cell::sync::Lazy<String> = once_cell::sync::Lazy::new(|| {
    let mut state = load_state();
    if let Ok(env_key) = std::env::var("INDEXNOW_KEY") {
        if let Some(cleaned) = sanitize_key(&env_key) {
            state.key = cleaned;
        }
    }
    if state.key.is_empty() {
        // 32 位 hex（uuid v4 去連字號）：符合 IndexNow 建議的格式，也保證每次都不一樣。
        state.key = Uuid::new_v4().simple().to_string();
    }
    if let Err(error) = save_state(&state) {
        tracing::warn!(
            error = %error,
            "IndexNow 金鑰無法寫入 config/indexnow.json（重啟後金鑰會換一組，Bing 需要重新驗證）"
        );
    }
    tracing::info!("IndexNow 金鑰就緒：金鑰檔路徑 /{}.txt", state.key);
    state.key
});

/// 目前的金鑰。
pub(super) fn key() -> &'static str {
    KEY.as_str()
}

/// 金鑰檔的**路徑**：`/{key}.txt`。
///
/// 金鑰只含 `[A-Za-z0-9-]`，所以這是一條普通字面路徑（不是 `{參數}`）——
/// 路由只註冊這一個確切路徑，不會攔到別的根目錄請求。
static KEY_PATH: once_cell::sync::Lazy<String> =
    once_cell::sync::Lazy::new(|| format!("/{}.txt", key()));

pub(super) fn key_path() -> &'static str {
    KEY_PATH.as_str()
}

/// `GET /{key}.txt`：IndexNow 的所有權驗證檔。
///
/// 內容必須**只有金鑰本身**（沒有換行、沒有其他字），回應 200、`text/plain`，
/// 而且不能被轉址或被防火牆／robots.txt 擋住 —— 這幾點任一項不成立，Bing 會回 403。
pub(super) async fn key_file_handler() -> Response {
    let mut response = key().to_string().into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=3600"),
    );
    response
}

/// 共用連線池（與站內其他對外呼叫一樣只建一次，不要每個請求都重新握手）。
static CLIENT: once_cell::sync::Lazy<reqwest::Client> = once_cell::sync::Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5))
        .user_agent("TronclassPanel/indexnow")
        .build()
        .unwrap_or_default()
});

/// 提交結果（面板顯示用）。
#[derive(Debug, Clone, Serialize)]
pub(super) struct IndexNowOutcome {
    /// 受理（HTTP 200／202）為 true。
    pub accepted: bool,
    /// HTTP 狀態碼。
    pub status: u16,
    /// 給人看的一句話。
    pub message: String,
    /// 這次提交的網址。
    pub url: String,
}

/// 送出一次 IndexNow 通知。`origin` 例如 `https://dashboard.jasoncloudserver.me`（不含結尾斜線）。
pub(super) async fn submit(origin: &str) -> Result<IndexNowOutcome, String> {
    let origin = origin.trim().trim_end_matches('/').to_string();
    let host = origin
        .split("://")
        .nth(1)
        .unwrap_or_default()
        .trim()
        .to_string();
    if host.is_empty() {
        return Err("無法判斷對外網域（origin 不合法）".to_string());
    }
    if !origin.starts_with("https://") {
        return Err(format!(
            "IndexNow 只接受 HTTPS 網址（目前是 {origin}）——請用公開網域開啟面板後再提交"
        ));
    }
    /* 本機／內網不提交：一是沒有意義，二是 IndexNow 會去抓金鑰檔，不該讓它敲內部位址。 */
    if host.starts_with("localhost") || host.starts_with("127.0.0.1") {
        return Err("本機網址不需要提交給 Bing".to_string());
    }
    let url = format!("{origin}/");
    let payload = json!({
        "host": host,
        "key": key(),
        "keyLocation": format!("{origin}{}", key_path()),
        // 明確 clone：`json!` 對變數的取值方式依版本而異，這裡不要賭（url 後面還要寫進 log）。
        "urlList": [url.clone()],
    });

    let response = CLIENT
        .post(INDEXNOW_ENDPOINT)
        .json(&payload)
        .send()
        .await
        .map_err(|e| super::public_error("連線 IndexNow 失敗：", &e))?;

    // 同步向 Bing 官方專屬端點發送通知（靜默記錄，不阻塞主流程）
    let bing_payload = payload.clone();
    tokio::spawn(async move {
        let _ = CLIENT.post(BING_INDEXNOW_ENDPOINT).json(&bing_payload).send().await;
    });

    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    let accepted = matches!(status, 200 | 202);
    let message = match status {
        200 => "已受理：Bing 會排隊來爬這個網址".to_string(),
        202 => "已受理：金鑰仍在驗證中（Bing 會非同步確認金鑰檔）".to_string(),
        403 => {
            "金鑰驗證失敗：確認金鑰檔打得開、內容只有金鑰、且沒有被防火牆或 robots.txt 擋住".to_string()
        }
        422 => "網址不屬於這個 host：確認提交的對外網域與 Bing 看到的一致".to_string(),
        429 => "送太頻繁（IndexNow 限流）：過一段時間再試".to_string(),
        _ => {
            let detail = body.trim();
            if detail.is_empty() {
                format!("未受理（HTTP {status}）")
            } else {
                format!("未受理（HTTP {status}）：{}", detail.chars().take(200).collect::<String>())
            }
        }
    };

    // 記錄最近一次結果（成功與失敗都記：面板要能顯示「上次失敗的原因」）。
    let mut state = load_state();
    state.key = key().to_string();
    state.last_submit_at = Some(chrono::Local::now().timestamp());
    state.last_ok = Some(accepted);
    state.last_status = Some(status);
    state.last_message = Some(message.clone());
    if let Err(error) = save_state(&state) {
        tracing::warn!(error = %error, "IndexNow 提交結果無法寫入 config/indexnow.json");
    }
    if accepted {
        tracing::info!(status, url = %url, "IndexNow 已提交公開首頁");
    } else {
        tracing::warn!(status, url = %url, "IndexNow 提交未受理");
    }

    Ok(IndexNowOutcome {
        accepted,
        status,
        message,
        url,
    })
}

/// 面板顯示用的狀態。
#[derive(Debug, Clone, Serialize)]
pub(super) struct IndexNowStatus {
    /// 目前的金鑰。
    pub key: String,
    /// 金鑰檔的完整網址（`https://<host>/<key>.txt`）。
    pub key_path: String,
    /// 最近一次提交時間（人看得懂的相對時間 ＋ 絕對時間）。
    pub last_submit: Option<String>,
    /// 最近一次是否受理。
    pub last_ok: Option<bool>,
    /// 最近一次的 HTTP 狀態碼。
    pub last_status: Option<u16>,
    /// 最近一次的說明。
    pub last_message: Option<String>,
}

/// 把 Unix 秒轉成「3 小時前（2026-09-21 15:04）」。刻意不用時區資料庫：
/// 用 `now - 相差秒數` 反推，只需要 chrono 的 `Duration` 與 `format`。
fn describe_when(unix_secs: i64) -> String {
    let now = chrono::Local::now();
    let diff = (now.timestamp() - unix_secs).max(0);
    let relative = if diff < 60 {
        "剛剛".to_string()
    } else if diff < 3600 {
        format!("{} 分鐘前", diff / 60)
    } else if diff < 86_400 {
        format!("{} 小時前", diff / 3600)
    } else {
        format!("{} 天前", diff / 86_400)
    };
    let when = now - chrono::Duration::seconds(diff);
    format!("{relative}（{}）", when.format("%Y-%m-%d %H:%M"))
}

pub(super) fn status(origin: &str) -> IndexNowStatus {
    let state = load_state();
    IndexNowStatus {
        key: key().to_string(),
        key_path: format!("{origin}{}", key_path()),
        last_submit: state.last_submit_at.map(describe_when),
        last_ok: state.last_ok,
        last_status: state.last_status,
        last_message: state.last_message,
    }
}

/// 啟動後自動提交一次（同一個網址 24 小時內不重複：IndexNow 的禮貌是「內容有變動才通知」）。
pub(crate) fn start_auto_submit() {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(AUTO_SUBMIT_DELAY_SECS)).await;
        let state = load_state();
        if let Some(last) = state.last_submit_at {
            let fresh = chrono::Local::now().timestamp() - last < AUTO_SUBMIT_MIN_INTERVAL_SECS;
            if state.last_ok == Some(true) && fresh {
                tracing::info!("IndexNow：24 小時內已成功提交過，這次啟動不重複提交");
                return;
            }
        }
        let origin = super::seo::default_public_origin();
        match submit(&origin).await {
            Ok(outcome) => info!(
                status = outcome.status,
                "IndexNow 啟動提交：{}",
                outcome.message
            ),
            Err(error) => warn!("IndexNow 啟動提交失敗：{error}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_key_accepts_valid_and_rejects_short_or_dirty() {
        // 32 位 hex（自動產生的格式）
        assert_eq!(
            sanitize_key("9f8c1d2e3b4a5c6d7e8f90a1b2c3d4e5").as_deref(),
            Some("9f8c1d2e3b4a5c6d7e8f90a1b2c3d4e5")
        );
        // 允許大寫與連字號（IndexNow 的合法字元）
        assert_eq!(sanitize_key("  My-IndexNow-Key-1  ").as_deref(), Some("My-IndexNow-Key-1"));
        // 太短（< 8）→ 不合法
        assert_eq!(sanitize_key("abc123"), None);
        // 不合法的字元會被濾掉，濾完仍要 ≥ 8 才算合法
        assert_eq!(sanitize_key("!!!!!!"), None);
        assert_eq!(sanitize_key("abcdefgh/../x").as_deref(), Some("abcdefghx"));
    }

    #[test]
    fn describe_when_is_relative_then_absolute() {
        let now = chrono::Local::now().timestamp();
        assert!(describe_when(now).starts_with("剛剛"));
        assert!(describe_when(now - 3 * 3600).starts_with("3 小時前"));
        assert!(describe_when(now - 2 * 86_400).starts_with("2 天前"));
        // 未來時間（時鐘被調整）不該出現負數
        assert!(describe_when(now + 600).starts_with("剛剛"));
    }

    #[test]
    fn key_path_looks_like_a_root_text_file() {
        let path = key_path();
        assert!(path.starts_with('/'), "金鑰檔必須在網站根目錄：{path}");
        assert!(path.ends_with(".txt"), "金鑰檔必須是 .txt：{path}");
        assert!(!path.contains('{'), "金鑰只含 [A-Za-z0-9-]，路徑應為字面值：{path}");
        assert!(path.contains(key()), "路徑要包含金鑰本身：{path}");
    }
}
