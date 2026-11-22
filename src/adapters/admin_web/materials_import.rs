//! 從 iClass/Tronclass 自動匯入教材到「教材雲端硬碟」。
//!
//! MATERIALS_IMPORT_V1
//! 用途：把老師放在 iClass 各課程的教材（material / web_link / online_video 等）
//! 自動下載到 `materials_root()`（= `TRONCLASS_MATERIALS_DIR` 或 `./materials`），
//! 讓既有的同源教材頁（files.html）直接掃到、瀏覽、下載。
//!
//! 規則：
//! - 只處理「當學期」課程（is_current != Some(false)），避免拉進上學期舊教材。
//! - `material`：取下載真檔（依 upload 的 `key` 組出下載位址，帶 session cookie）。
//! - MATERIALS_MATERIAL_FOLDER_V1（fix101）：一個教材含**多個**檔案時，替它開一個跟教材同名
//!   的資料夾：`<root>/<group>/<course>/<教材標題>/<檔案>`（學校 App 的呈現就是這樣：
//!   例如「E-Commerce 2018 Book」是一個資料夾，裡面是 Chapter 1..12.pptx）。
//!   只有 0～1 個檔案的教材維持攤平在課程資料夾裡，不製造一堆單檔資料夾。
//! - `web_link`：存成 `.url` 連結檔。
//! - `online_video`：能下載就下載，否則存 `.url` 連結檔。
//! - 冪等：目的檔案已存在即跳過；一律寫一份 `tronclass_materials_manifest.json`。
//! - 節流：課程間 sleep ＋ 有界並行（課程 3／教材項 3／同時下載 4），避免打爆學校 WAF。
//! - MATERIALS_IMPORT_FAST_V1（fix59）：匯入速度優化（見下方常數區的說明）。
//! - MATERIALS_IMPORT_FAST_V2（fix59b）：併行任務改成「擁有自己的資料」——
//!   修正部署第一版的編譯錯誤 `implementation of FnOnce/Iterator is not general enough`
//!   （詳見第 2 節上方與下方兩處 `into_iter()`）。
//!
//! 版本自檢：本檔（V2）的課程流與教材項都走 `into_iter()` 取得所有權，教材清單是擁有的
//! `Value`（不再是以借用元素組成的清單）；舊版（V1）兩者都是借用，行數也少 21 行。
//! 精確的比對指令寫在 fix59 報告的「部署注意」一節（不在原始碼裡，避免指令自己命中自己）。

use super::*;
use axum::extract::Query;
// MATERIALS_PENDING_V1（fix48）：教材開放時間的比對需要 DateTime/Utc。
use chrono::{DateTime, Utc};
// IMPORT_STREAM_TO_DISK_V1（fix51）：逐段寫入 .part 需要 AsyncWriteExt。
use tokio::io::AsyncWriteExt;
// MATERIALS_DEBUG_MACRO_V1（fix49）：`super::*` 只帶進 mod.rs 匯入的 error/info/warn，
// `debug!` 必須自己引入——否則編譯直接失敗（cannot find macro `debug` in this scope）。
use tracing::debug;
use serde_json::{json, Value};
// MATERIALS_IMPORT_FAST_V1（fix59）：課程／教材項的有界並行（buffer_unordered）。
use futures::stream::StreamExt;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// MATERIALS_IMPORT_SHARED_V1
// 雲端硬碟是共用的，因此：
// - 匯入寫入**共用** `materials_root()`（管理員與一般帳號共用同一棵樹，所有人可看全部資料夾）。
// - 同一門課的教材只寫一次（冪等：檔存在即跳過），不重複佔用硬碟。
// - 匯入改為背景 job（立即回 202），避免占住 HTTP request 直到 proxy 逾時 → 502。

/// 匯入背景工作的共用狀態。
#[derive(Clone)]
struct ImportJob {
    running: bool,
    started_at: u64,
    last_update_at: u64,
    /// 截至目前完成的步驟敘述（顯示給前端）。
    message: String,
    progress: u64,
    total: u64,
    done: bool,
    ok: bool,
    report: ImportReport,
}

impl Default for ImportJob {
    fn default() -> Self {
        Self {
            running: false,
            started_at: 0,
            last_update_at: 0,
            message: String::new(),
            progress: 0,
            total: 0,
            done: false,
            ok: false,
            report: ImportReport::default(),
        }
    }
}

/// 全域單一匯入 job(同一時間只允許一個匯入)。
static IMPORT_JOB: once_cell::sync::Lazy<RwLock<ImportJob>> =
    once_cell::sync::Lazy::new(|| RwLock::new(ImportJob::default()));

// ─────────────────────────────────────────────────────────────
// MATERIALS_IMPORT_FAST_V1（fix59）：匯入速度優化
//
// 原本的匯入是「一門課接一門課、一個檔案接一個檔案」的嚴格序列，而且每個「拿不到
// 簽章網址」的教材都會 (a) 重新完整登入一次 (b) 依序硬打 5 個備援位址——其中 4 個是
// 學校 S3 化之後早已關閉的檔案埠（8085/8443 的 http 與 https），封包被防火牆丟棄時
// 要等 OS 預設的 SYN 重試（約兩分鐘）才會失敗。三十幾門課跑完常要十幾分鐘。
//
// 這裡做四件事（不改任何對外行為、統計數字與錯誤分類都保持一致）：
//   1. 課程層有界並行；每門課的 activities／homework-activities 也改成同時抓
//   2. 課程內教材項有界並行 ＋ 全域下載號誌（同時最多 4 個檔案在下載）
//   3. 「尚未開放」的教材不再做任何網路嘗試（iClass 當下就是不會給檔案）
//   4. 同一次匯入只重登一次；備援位址先做 3 秒 TCP 探測、整批最多 20 秒
// ─────────────────────────────────────────────────────────────

/// 同時處理幾門課（每門課一開始會同時打 2 個 JSON 清單）。
///
/// 3 這個數字是刻意保守的：作業頁的 `api::todos::COURSE_FETCH_CONCURRENCY` 早就用 8
/// 對同一個 iClass 併發抓各課程資料（線上實際跑得動），這裡只用 3；若學校 WAF 有
/// 意見，只要調這幾個常數即可（單一位置）。
const IMPORT_COURSE_CONCURRENCY: usize = 3;
/// 同一門課內同時處理幾個教材項（大多是「已存在→跳過」；真正下載另有全域號誌）。
const IMPORT_ITEM_CONCURRENCY: usize = 3;
/// 整個匯入同時最多幾個檔案在下載（跨課程共用；學校 WAF 不喜歡一次開太多連線）。
const IMPORT_MAX_CONCURRENT_DOWNLOADS: usize = 4;
/// 課程之間的最小間隔（併行後仍保留，讓請求不要全部在同一瞬間送出）。
const IMPORT_COURSE_STAGGER_MS: u64 = 150;
/// 備援位址動手前的 TCP 探測逾時：連不上就直接跳過，不等 OS 的兩分鐘。
const IMPORT_FALLBACK_PROBE_TIMEOUT_SECS: u64 = 3;
/// 同一個檔案花在「備援位址」上的總時間上限（含探測）。
/// 簽章網址（真正的大檔）不受此限，30 分鐘的長逾時照舊。
const IMPORT_FALLBACK_BUDGET_SECS: u64 = 20;
/// 重登失敗後的冷卻：避免幾十個教材輪流嘗試登入。
const IMPORT_RELOGIN_FAILURE_COOLDOWN_SECS: u64 = 300;

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// MATERIALS_IMPORT_SHARED_V1: 即時更新背景 job 狀態，讓後端（進度 API）
// 能看到「目前已跑到哪、每個錯誤」，利於診斷與後台監控。
fn job_update(message: &str, progress: u64, total: u64, report: &ImportReport) {
    let mut job = IMPORT_JOB.write().unwrap_or_else(|p| p.into_inner());
    job.running = true;
    job.last_update_at = now_ts();
    job.message = message.to_string();
    job.progress = progress;
    job.total = total;
    // 同步最新統計與錯誤，讓 /import/status 即時反映。
    job.report.courses_scanned = report.courses_scanned;
    job.report.materials_found = report.materials_found;
    job.report.files_saved = report.files_saved;
    job.report.files_skipped = report.files_skipped;
    job.report.links_created = report.links_created;
    job.report.failed = report.failed;
    job.report.pending = report.pending;
    job.report.disallowed = report.disallowed;
    job.report.disallowed_files = report.disallowed_files.clone();
    job.report.all_types = report.all_types;
    job.report.errors = report.errors.clone();
}

/// 匯入參數。
#[derive(Debug, Deserialize)]
pub(super) struct AdminMaterialsImportQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    account_id: Option<String>,
    /// 只匯入指定課程 id（逗號分隔）。留空 = 全部當學期課程。
    #[serde(default)]
    course_ids: Option<String>,
    /// IMPORT_ALL_TYPES_V1（fix64）：`all_types=1` 時不套用副檔名白名單，
    /// 把 iClass 上的教材全部抓下來（前端只在使用者按了「確定」之後才帶這個參數）。
    #[serde(default)]
    all_types: Option<String>,
}

/// IMPORT_ALL_TYPES_V1（fix64）：把 `all_types` query 參數解析成 bool。
///
/// 只接受 1/true/yes/on（大小寫不拘）；其他值一律當成「關閉」——打錯字不該變成
/// 「全部匯入」，那是使用者的明確決定而不是預設值。
fn parse_all_types_flag(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// IMPORT_ALL_TYPES_V1（fix64）：報告裡最多留幾個「被跳過的檔名」樣本。
/// 只給人看與判斷用，不需要全部列出（幾百個檔案的通知沒人讀得完）。
const DISALLOWED_SAMPLE_LIMIT: usize = 20;

/// 匯入結果報告（回傳給前端）。
#[derive(Debug, Serialize, Default, Clone)]
pub(super) struct ImportReport {
    pub ok: bool,
    pub courses_scanned: u64,
    pub materials_found: u64,
    pub files_saved: u64,
    pub files_skipped: u64,
    /// MATERIALS_MATERIAL_FOLDER_V1（fix101）：舊的攤平檔案被移進教材資料夾的數量。
    /// 這些檔案既沒有重新下載、也不算跳過 —— 單獨記一筆，使用者才知道「東西被搬到哪去了」。
    pub files_moved: u64,
    pub links_created: u64,
    pub failed: u64,
    /// MATERIALS_PENDING_V1（fix48）：教材「還沒到開放時間」而略過的數量。
    ///
    /// 這不是錯誤：iClass 對尚未開放的教材一律不給檔案（連簽章網址都拿不到），
    /// 硬記成 failed 只會讓日誌滿是紅色 ERROR。開放後再跑一次匯入就會下載。
    pub pending: u64,
    /// IMPORT_ALL_TYPES_V1（fix64）：因為「副檔名不在允許清單」而先跳過的數量。
    /// 這些檔案不是壞掉、也不算失敗；預設先跳過並回報，讓使用者決定要不要改抓。
    pub disallowed: u64,
    /// 被跳過的檔名樣本（最多 [`DISALLOWED_SAMPLE_LIMIT`] 筆），供前端列出給使用者看。
    pub disallowed_files: Vec<String>,
    /// 這次匯入是否採用了「不限制類型」模式（前端顯示與診斷用）。
    pub all_types: bool,
    pub errors: Vec<String>,
}

/// 查詢匯入進度：`GET /admin/api/files/import/status`。
pub(super) async fn admin_materials_import_status_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
) -> Response {
    let token = session_token(&headers, "");
    let Some(_session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    let job = IMPORT_JOB.read().unwrap_or_else(|p| p.into_inner()).clone();
    Json(json!({
        "running": job.running,
        "done": job.done,
        "ok": job.ok,
        "started_at": job.started_at,
        "last_update_at": job.last_update_at,
        "message": job.message,
        "progress": job.progress,
        "total": job.total,
        "courses_scanned": job.report.courses_scanned,
        "materials_found": job.report.materials_found,
        "files_saved": job.report.files_saved,
        "files_skipped": job.report.files_skipped,
        // MATERIALS_MATERIAL_FOLDER_V1（fix101）：舊檔被搬進教材資料夾的數量。
        "files_moved": job.report.files_moved,
        "links_created": job.report.links_created,
        "failed": job.report.failed,
        "pending": job.report.pending,
        // IMPORT_ALL_TYPES_V1（fix64）：讓前端能問「這些檔案要不要也抓下來」。
        "disallowed": job.report.disallowed,
        "disallowed_files": job.report.disallowed_files,
        "all_types": job.report.all_types,
        "errors": job.report.errors,
    }))
    .into_response()
}

/// 實際匯入教材的主 handler：`GET /admin/api/files/import`（含 query 參數
/// `?all_types=1`／`?course_ids=`／`?account_id=`；IMPORT_ALL_TYPES_V1（fix64）順手
/// 更正原本寫成 POST 的註解，路由表實際掛的是 GET）。
/// 立即回傳 202「已排程」，實際匯入在背景執行。
pub(super) async fn admin_materials_import_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminMaterialsImportQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };

    let requested_account_id = query
        .account_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());

    let target_account_id = if session.is_admin {
        requested_account_id.unwrap_or(&session.account_id)
    } else {
        if let Some(requested) = requested_account_id {
            if requested != session.account_id {
                return unauthorized_response();
            }
        }
        &session.account_id
    };

    let Some(account) = resolve_account(&state, target_account_id).await else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": "找不到指定帳號，請至帳號設定確認" })),
        )
            .into_response();
    };

    if !account.enabled {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "該帳號目前已被停用" })),
        )
            .into_response();
    }

    let Some(client) = crate::session_client::session_http_client(&account) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "ok": false, "error": "該帳號沒有可用的 session cookie，請先登入 iClass" })),
        )
            .into_response();
    };

    // IMPORT_ALL_TYPES_V1（fix64）：使用者確認後前端會帶 all_types=1。
    let include_all_types = parse_all_types_flag(query.all_types.as_deref());

    let only_courses: Option<Vec<u64>> = query
        .course_ids
        .as_deref()
        .map(|s| s.split(',').filter_map(|p| p.trim().parse::<u64>().ok()).collect())
        .filter(|v: &Vec<u64>| !v.is_empty());

    // MATERIALS_IMPORT_SHARED_V2: 共用硬碟 → 一律寫進共用的 materials_root()。
    // 管理員與一般帳號共用同一棵 <root>/<分班>/<課程>/<檔名> 樹；不再替非 admin 額外
    // 開 root/教材，避免出現與「全班可看全部資料夾」不一致的兩棵樹。同一門課只寫一份(冪等)。
    let root = materials_root();

    // 背景執行：立即回 202，避免佔住 HTTP request 直到 proxy 逾時 → 502。
    // 若已有匯入正在跑，直接回 409 以免重複。
    {
        let mut job = IMPORT_JOB.write().unwrap_or_else(|p| p.into_inner());
        if job.running {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "ok": false, "error": "已有匯入工作正在執行，請稍候再試" })),
            )
                .into_response();
        }
        *job = ImportJob {
            running: true,
            started_at: now_ts(),
            last_update_at: now_ts(),
            message: "開始匯入…".to_string(),
            progress: 0,
            total: 0,
            done: false,
            ok: false,
            report: ImportReport::default(),
        };
    }

    // 複製背景所需資料。
    let base_url = account.base_url().to_string();
    let client_clone = client.clone();
    let root_clone = root.clone();
    let only = only_courses.clone();
    // MATERIALS_RELOGIN_RETRY_V1（fix48）：session 過期時要能重登再抓一次，
    // 因此背景工作需要帳號設定與 DB（與作業檔案路徑同一套處置）。
    let account_clone = account.clone();
    let db_clone = state.accounts_db.clone();
    let include_all_types_clone = include_all_types;

    // MATERIALS_IMPORT_SHARED_V1: 背景 job，不佔住 HTTP request。
    tokio::spawn(async move {
        let report = run_import(
            &client_clone,
            &base_url,
            &root_clone,
            only.as_deref(),
            Some(&account_clone),
            Some(&db_clone),
            include_all_types_clone,
        )
        .await;
        let mut job = IMPORT_JOB.write().unwrap_or_else(|p| p.into_inner());
        job.running = false;
        job.done = true;
        job.ok = report.ok;
        job.last_update_at = now_ts();
        job.message = format!(
            "完成：課程 {} 門、教材 {} 項、儲存 {}、跳過 {}（非允許類型 {}）、整理 {}、連結 {}、未開放 {}、失敗 {}",
            report.courses_scanned,
            report.materials_found,
            report.files_saved,
            report.files_skipped,
            report.disallowed,
            report.files_moved,
            report.links_created,
            report.pending,
            report.failed,
        );
        job.progress = 1;
        job.total = 1;
        job.report = report;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "ok": true,
            "accepted": true,
            "message": "已開始匯入，請稍候查看進度",
        })),
    )
        .into_response()
}

/// 解析指定帳號為 `AccountConfig`（鏡像 homework.rs 的授權解析）。
async fn resolve_account(state: &AdminWebState, account_id: &str) -> Option<crate::account::AccountConfig> {
    let mut raw_account = state.accounts_db.get(account_id).await.ok().flatten();
    if raw_account.is_none() {
        if let Ok(accounts) = state.accounts_db.find_by_username(account_id, None).await {
            raw_account = accounts.into_iter().next();
        }
    }
    let raw_account = raw_account?;
    // VIEWER_NO_SCHOOL_API_V1：檢視者不呼叫學校 API。
    if !raw_account.enabled || raw_account.is_viewer {
        return None;
    }
    let config = crate::config::try_get_config()?;
    let provider = config.providers.get(&raw_account.provider)?.clone();
    Some(crate::account::AccountConfig {
        id: raw_account.id.clone(),
        provider: raw_account.provider.clone(),
        username: raw_account.username.clone(),
        password: raw_account.password.clone(),
        enabled: raw_account.enabled,
        line_user_id: raw_account.line_user_id.clone(),
        discord_user_id: raw_account.discord_user_id.clone(),
        session_cookie: raw_account.session_cookie.clone(),
        is_admin: raw_account.is_admin,
        provider_config: provider.clone(),
        request_timeout_secs: provider.api.request_timeout_secs,
    })
}

/// 執行匯入（純 async,不依賴 HTTP layer,便於日後測試）。
async fn run_import(
    client: &reqwest::Client,
    base_url: &str,
    root: &Path,
    only_courses: Option<&[u64]>,
    account: Option<&crate::account::AccountConfig>,
    accounts_db: Option<&crate::db::AccountsDb>,
    // IMPORT_ALL_TYPES_V1（fix64）：true = 不套用副檔名白名單（使用者已確認）。
    // 注意：函式參數只能用一般註解 `//`，用 doc 註解 `///` 會被 rustc 擋下
    // （documentation comments cannot be applied to function parameters）。
    include_all_types: bool,
) -> ImportReport {
    let mut report = ImportReport {
        ok: true,
        courses_scanned: 0,
        materials_found: 0,
        files_saved: 0,
        files_skipped: 0,
        files_moved: 0,
        links_created: 0,
        failed: 0,
        pending: 0,
        disallowed: 0,
        disallowed_files: Vec::new(),
        all_types: include_all_types,
        errors: Vec::new(),
    };
    // MATERIALS_IMPORT_CTX_V1：把「每個檔案都會用到的連線資訊」收成一包，
    // 避免 process_item／process_upload 的參數一路膨脹。
    let ctx = ImportContext {
        client,
        base_url,
        root,
        account,
        accounts_db,
        downloads: tokio::sync::Semaphore::new(IMPORT_MAX_CONCURRENT_DOWNLOADS),
        relogin: tokio::sync::Mutex::new(ReloginState::default()),
        inflight: std::sync::Mutex::new(HashSet::new()),
        include_all_types,
    };
    // 進度訊息附上「已用時間」，讓使用者看得出還在跑（不是卡住）。
    let started_at = IMPORT_JOB
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .started_at;

    // 1. 課程清單 → 只留當學期。
    let courses_json = match fetch_json(client, &format!("{base_url}/api/my-courses")).await {
        Ok(v) => v,
        Err(e) => {
            report.ok = false;
            report.errors.push(super::public_error("取得課程清單失敗：", &e));
            job_update("取得課程清單失敗", 0, 0, &report);
            error!(%base_url, error = %e, "教材匯入：取得課程清單 /api/my-courses 失敗");
            return report;
        }
    };

    let all_courses = crate::api::todos::parse_course_briefs(&courses_json);
    let courses: Vec<_> = all_courses
        .into_iter()
        .filter(|c| c.is_current != Some(false))
        .filter(|c| only_courses.map_or(true, |ids| ids.contains(&c.id)))
        .collect();

    report.courses_scanned = courses.len() as u64;
    let total_courses = courses.len() as u64;
    info!(
        base_url = %base_url,
        total = total_courses,
        "教材匯入：取得課程清單成功，共 {} 門當學期課程",
        total_courses
    );
    job_update(
        &format!("取得課程清單：共 {} 門，開始逐課抓取…", total_courses),
        0,
        total_courses,
        &report,
    );

    // 2. 每門課抓 activities。
    // MATERIALS_IMPORT_FAST_V1（fix59）：課程層有界並行。
    //
    // 用 next() 一個一個收而不是 collect()：buffer_unordered 是「完成順序」，
    // collect() 會等到全部跑完才回來（等於整個匯入期間都不會更新進度），
    // next() 則是一門課跑完就合併統計並更新進度條。
    //
    // MATERIALS_IMPORT_FAST_V2（fix59b）：這一版把「課程」**擁有**（`into_iter()`）後
    // 才交給任務。原因是一個 rustc 的已知限制：閉包若「參數是借用、又回傳捕獲該參數的
    // async 區塊」，編譯器推不出 higher-ranked 的閉包簽章，會出現
    //   error: implementation of `FnOnce` is not general enough
    //   error: implementation of `Iterator` is not general enough
    // （部署第一版時的實際錯誤，指向 tokio::spawn 那一行）。讓參數完全沒有生命週期
    // 就沒有這個問題——這也是為什麼併行任務最好「自己帶自己的資料」。
    let mut course_stream = Box::pin(
        futures::stream::iter(courses.into_iter().enumerate().map(|(idx, course)| {
            let ctx = &ctx;
            async move {
                if idx > 0 {
                    // 節流：課程間歇一下，別讓所有請求在同一瞬間送出（避免打爆學校 WAF）。
                    tokio::time::sleep(Duration::from_millis(IMPORT_COURSE_STAGGER_MS)).await;
                }
                // LOG_NOISE_V1（fix61）：逐檔／逐位址的訊息一律降到 `debug!`。
                // 一次匯入是「課程 × 教材數 × 候選位址」，33 門課可以產生上千行，
                // 真正有用的只有「課程處理完畢」與最後的總結——這兩行留在 info!。
                debug!(
                    idx = idx + 1,
                    total = total_courses,
                    course = %course.name,
                    "教材匯入：開始處理課程"
                );
                let outcome = import_course(ctx, &course).await;
                // 把課程本身一起帶回來，完成訊息才有名字（不必再回頭查 courses）。
                (idx, course, outcome)
            }
        }))
        .buffer_unordered(IMPORT_COURSE_CONCURRENCY),
    );

    let mut courses_done = 0u64;
    while let Some((idx, course, outcome)) = course_stream.next().await {
        courses_done += 1;
        report.failed += outcome.failed;
        report.errors.extend(outcome.errors);
        outcome.items.merge_into(&mut report);
        let course_name = course.name.as_str();
        info!(
            idx = idx + 1,
            total = total_courses,
            course = %course_name,
            "教材匯入：課程處理完畢"
        );
        job_update(
            &format!(
                "完成 {courses_done}/{total_courses} 門：{course_name}（已存 {}、跳過 {}、非允許類型 {}、未開放 {}、失敗 {}；已用 {}s）",
                report.files_saved,
                report.files_skipped,
                report.disallowed,
                report.pending,
                report.failed,
                now_ts().saturating_sub(started_at),
            ),
            courses_done,
            total_courses,
            &report,
        );
    }

    // 3. 寫 manifest。
    write_manifest(root, &report);
    // IMPORT_INVALIDATE_CACHE_V1（fix61）：匯入是直接寫磁碟、不經過上傳端點，所以
    // 教材頁的快取不會自己失效——以前只能等 TTL（列表 3 秒、用量 60 秒）過去，
    // 匯入剛結束的那一分鐘「已使用容量」是舊的。這裡主動清一次。
    super::files::invalidate_materials_list_cache().await;
    super::files::invalidate_materials_usage_cache().await;
    info!(
        courses = report.courses_scanned,
        materials = report.materials_found,
        saved = report.files_saved,
        skipped = report.files_skipped,
        links = report.links_created,
        failed = report.failed,
        disallowed = report.disallowed,
        "教材匯入：完成（共用硬碟）"
    );
    report
}

/// MATERIALS_IMPORT_CTX_V1（fix48）：匯入過程中每個檔案都會用到的連線資訊。
///
/// 收成一包的理由：`process_item`／`process_upload` 原本已經是 7 個參數，
/// 再加上「重登重試」需要的帳號與 DB 會更難閱讀。
struct ImportContext<'a> {
    client: &'a reqwest::Client,
    base_url: &'a str,
    root: &'a Path,
    /// 供 session 過期時重新登入用；不需要（或測試）時為 `None`。
    account: Option<&'a crate::account::AccountConfig>,
    accounts_db: Option<&'a crate::db::AccountsDb>,
    /// MATERIALS_IMPORT_FAST_V1（fix59）：同時下載數的全域上限（跨課程共用）。
    downloads: tokio::sync::Semaphore,
    /// 同一次匯入的「重登」狀態：成功後所有檔案共用同一份新連線，失敗則進入冷卻。
    relogin: tokio::sync::Mutex<ReloginState>,
    /// 正在被某個任務下載的目標檔（避免兩個併行任務抓同一個檔名）。
    inflight: std::sync::Mutex<HashSet<PathBuf>>,
    /// IMPORT_ALL_TYPES_V1（fix64）：使用者已確認「不允許的副檔名也要抓」。
    include_all_types: bool,
}

/// 同一次匯入的重新登入狀態（MATERIALS_IMPORT_FAST_V1）。
#[derive(Default)]
struct ReloginState {
    /// 重登成功後的新 client（含新的 cookie jar），後續所有檔案共用。
    client: Option<reqwest::Client>,
    /// 最近一次重登失敗的時間；冷卻期內不再嘗試。
    failed_at: Option<Instant>,
    /// 這次匯入總共嘗試登入幾次（給日誌／診斷用）。
    attempts: u32,
}

/// 單一教材項目的處理結果。
///
/// 為什麼要獨立一個型別：併行處理時每個任務各自累計，回到主流程再合併進
/// `ImportReport`——原本所有任務共用一個 `&mut ImportReport`，在併行下根本
/// 不能這樣共享可變參考（而且編譯不過）。
#[derive(Debug, Default, Clone)]
struct ItemOutcome {
    /// 處理了幾個教材（對應原本的 `materials_found`）。
    materials: u64,
    saved: u64,
    skipped: u64,
    /// MATERIALS_MATERIAL_FOLDER_V1（fix101）：把「以前攤平在課程層」的舊檔搬進教材資料夾的數量。
    moved: u64,
    links: u64,
    pending: u64,
    failed: u64,
    /// IMPORT_ALL_TYPES_V1（fix64）：被副檔名白名單擋下的數量與樣本。
    disallowed: u64,
    disallowed_files: Vec<String>,
    errors: Vec<String>,
}

impl ItemOutcome {
    /// 把另一個任務的結果併進來（課程內多個教材項的合併）。
    fn absorb(&mut self, other: ItemOutcome) {
        self.materials += other.materials;
        self.saved += other.saved;
        self.skipped += other.skipped;
        self.moved += other.moved;
        self.links += other.links;
        self.pending += other.pending;
        self.failed += other.failed;
        self.disallowed += other.disallowed;
        for name in other.disallowed_files {
            if self.disallowed_files.len() >= DISALLOWED_SAMPLE_LIMIT {
                break;
            }
            self.disallowed_files.push(name);
        }
        self.errors.extend(other.errors);
    }

    /// 併進整份匯入報告（課程完成時呼叫）。
    fn merge_into(&self, report: &mut ImportReport) {
        report.materials_found += self.materials;
        report.files_saved += self.saved;
        report.files_skipped += self.skipped;
        report.files_moved += self.moved;
        report.links_created += self.links;
        report.pending += self.pending;
        report.failed += self.failed;
        report.disallowed += self.disallowed;
        for name in &self.disallowed_files {
            if report.disallowed_files.len() >= DISALLOWED_SAMPLE_LIMIT {
                break;
            }
            report.disallowed_files.push(name.clone());
        }
        for error in &self.errors {
            report.errors.push(error.clone());
        }
    }
}

/// 單一課程的處理結果（掃描失敗的記錄＋課程內所有教材的合計）。
#[derive(Debug, Default)]
struct CourseOutcome {
    failed: u64,
    errors: Vec<String>,
    items: ItemOutcome,
}

/// 目標檔的「正在下載」註冊表：離開 scope 時自動移除（含所有提前 return）。
struct InflightGuard<'a> {
    set: &'a std::sync::Mutex<HashSet<PathBuf>>,
    path: PathBuf,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut set) = self.set.lock() {
            set.remove(&self.path);
        }
    }
}

/// MATERIALS_IMPORT_FAST_V1（fix59）：取得「重登後可用的 client」——**整次匯入只登入一次**。
///
/// 原本每個「拿不到簽章網址」的檔案都各自呼叫 `iclass_api_after_relogin`（完整 SSO
/// 登入＋寫回 DB）。一堂課若有幾十個尚未開放的教材，那就是幾十次登入，而且回來
/// 還是一樣拿不到檔案。現在改成共用同一份結果，失敗則進入冷卻期。
async fn fresh_client_after_relogin(ctx: &ImportContext<'_>) -> Option<reqwest::Client> {
    let (Some(account), Some(db)) = (ctx.account, ctx.accounts_db) else {
        return None;
    };
    let mut state = ctx.relogin.lock().await;
    if let Some(client) = state.client.clone() {
        return Some(client);
    }
    if let Some(failed_at) = state.failed_at {
        if failed_at.elapsed() < Duration::from_secs(IMPORT_RELOGIN_FAILURE_COOLDOWN_SECS) {
            return None;
        }
    }
    state.attempts += 1;
    match super::homework::iclass_api_after_relogin(account, db).await {
        Ok(fresh) => {
            info!(
                attempts = state.attempts,
                "教材匯入：session 已重建，後續檔案共用這個連線"
            );
            state.client = Some(fresh.client.clone());
            state.client.clone()
        }
        Err(error) => {
            state.failed_at = Some(Instant::now());
            debug!(%error, attempts = state.attempts, "教材匯入：重新登入失敗（冷卻中）");
            None
        }
    }
}

/// 備援位址動手前的 TCP 探測。
///
/// 學校的檔案埠多半已關閉；若封包被防火牆丟棄（DROP），沒有這一步就要等 OS 預設的
/// SYN 重試（約兩分鐘）才會失敗——這正是「匯入要等很久」的主要來源。
/// 解析不出 host/port 時回 `true`（交給原本的下載流程處理）。
async fn tcp_reachable(url: &str, timeout_secs: u64) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return true;
    };
    let (Some(host), Some(port)) = (parsed.host_str(), parsed.port_or_known_default()) else {
        return true;
    };
    let connect = tokio::net::TcpStream::connect((host, port));
    matches!(
        tokio::time::timeout(Duration::from_secs(timeout_secs), connect).await,
        Ok(Ok(_))
    )
}

/// MATERIALS_PENDING_V1（fix48）：教材下載失敗的歸類。
///
/// 為什麼要分類：iClass 對「尚未開放」的教材不會給檔案（`/api/uploads/{id}/preview`
/// 拿不到簽章網址、備援的 `/download/{key}` 回 404）。過去這與真正的故障
/// （連線失敗、硬碟寫不進去）都記成同一種 ERROR，使用者只看到紅字卻查不出原因。
enum MaterialFailureKind {
    /// 還沒到教材開放時間 → 不是錯誤，開放後再匯入即會下載。
    NotOpenYet(String),
    /// 上游完全沒給可下載位址 → 通常是尚未開放，或這筆教材本來就不是檔案。
    NoDownloadUrl(String),
    /// 其他（連線、HTTP 狀態、0 bytes…）→ 沿用原本的 failed／error 記錄。
    Other,
}

/// 依「開放時間」與「有沒有拿到簽章網址」把失敗歸類。
///
/// `has_signed_url`：候選網址裡是否有 `/api/uploads/{id}/preview` 取得的簽章連結
/// （只有備援的 `/download/{key}` 或完全沒有，都代表上游沒有真的給檔案）。
fn classify_material_failure(
    opens_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    has_signed_url: bool,
) -> MaterialFailureKind {
    if let Some(open_at) = opens_at.filter(|open_at| *open_at > now) {
        return MaterialFailureKind::NotOpenYet(format!(
            "教材尚未開放（預計 {} 開放），開放後再匯入即會下載",
            open_at
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
        ));
    }
    if !has_signed_url {
        return MaterialFailureKind::NoDownloadUrl(
            "iClass 沒有提供這個檔案的下載位址（可能尚未開放，或這筆教材不是可下載的檔案）"
                .to_string(),
        );
    }
    MaterialFailureKind::Other
}

/// 讀出教材項目的「開放時間」（iClass 的 start_time 等欄位）。
///
/// 解析沿用 `api::todos::parse_datetime`（與作業頁同一個解析器，支援 RFC 3339、
/// `YYYY-MM-DD HH:MM[:SS]` 等實際會遇到的格式）。欄位缺漏或格式不合法 → `None`，
/// 呼叫端就把失敗當成一般錯誤處理（寧可誤報，也不要漏報）。
fn item_start_time(item: &Value) -> Option<DateTime<Utc>> {
    for key in [
        "start_time",
        "start_at",
        "open_time",
        "begin_time",
        "start_date",
    ] {
        if let Some(dt) = item.get(key).and_then(crate::api::todos::parse_datetime) {
            return Some(dt);
        }
    }
    None
}

/// 處理單一課程：同時抓兩個清單，再把教材項有界並行處理。
///
/// MATERIALS_IMPORT_FAST_V1（fix59）：從 `run_import` 的迴圈抽出來，才能讓多門課
/// 同時進行；課程內則用 `buffer_unordered` 併行處理教材項。
async fn import_course(
    ctx: &ImportContext<'_>,
    course: &crate::api::todos::CourseBrief,
) -> CourseOutcome {
    let mut outcome = CourseOutcome::default();

    // MATERIALS_IMPORT_V1: 依系所決定「分班資料夾」。
    // 結構 = <root>/<分班>/<課程>/<教材>/<檔名>。
    // 共同科(通識/體育/核心課程/三全教育)統一歸到「共同科」。
    let group = group_folder_for(course.department.as_deref(), &course.name);
    let base = format!("{}/api/courses/{}", ctx.base_url, course.id);
    let activities_url = format!("{base}/activities?page=1&page_size=300");
    // 注意:部分教材的 uploads 只在 `/homework-activities` 帶 key。
    // 這裡先併入 homework-activities 的 material,再處理 activities。
    let hw_url = format!("{base}/homework-activities?page=1&page_size=300");

    // 兩個清單同時抓（原本是一個 await 完才換另一個，等於每門課多花一個 RTT）。
    let (activities_result, hw_result) = tokio::join!(
        fetch_json(ctx.client, &activities_url),
        fetch_json(ctx.client, &hw_url)
    );

    let activities_json = match activities_result {
        Ok(value) => value,
        Err(error) => {
            // 與原本行為一致：讀不到 activities 就跳過這門課（計一次失敗）。
            outcome.failed += 1;
            outcome.errors.push(format!("[{}] 讀取 activities 失敗", course.name));
            warn!(course = %course.name, error = %error, "教材匯入：讀取 activities 失敗");
            return outcome;
        }
    };

    let hw_items = hw_result
        .ok()
        .and_then(|value| {
            value
                .get("homework_activities")
                .and_then(Value::as_array)
                .cloned()
        })
        .unwrap_or_default();

    // MATERIALS_IMPORT_FAST_V2（fix59b）：教材項同樣改成**擁有**的 `Value` 清單。
    // 這裡的 clone 是刻意的：一堂課的清單只有幾百 KB（相對於動輒數百 MB 的教材下載
    // 可以忽略），換來的是閉包參數完全沒有生命週期——見上面課程流那段對
    // 「FnOnce is not general enough」的說明。
    // 順序：activities 在前、homework-activities 在後（與原本 chain 的順序一致）。
    let items: Vec<Value> = match activities_json.get("activities").and_then(Value::as_array) {
        Some(list) => list.iter().cloned().chain(hw_items).collect(),
        None => hw_items,
    };

    // 有界並行處理這門課的教材項（已經存在的檔案會立刻跳過，真正下載另有全域號誌）。
    let group_name: &str = group.as_str();
    let course_name: &str = course.name.as_str();
    let mut item_stream = Box::pin(
        futures::stream::iter(items.into_iter().map(|item| {
            let ctx = &ctx;
            async move {
                let mut item_outcome = ItemOutcome::default();
                process_item(ctx, group_name, course_name, &item, &mut item_outcome).await;
                item_outcome
            }
        }))
        .buffer_unordered(IMPORT_ITEM_CONCURRENCY),
    );
    while let Some(item_outcome) = item_stream.next().await {
        outcome.items.absorb(item_outcome);
    }
    outcome
}

/// 處理單一活動項,依 type 分派。
async fn process_item(
    ctx: &ImportContext<'_>,
    group: &str,
    course_name: &str,
    item: &Value,
    outcome: &mut ItemOutcome,
) {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
    if item_type.is_empty() {
        return;
    }
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "未命名教材".to_string());

    outcome.materials += 1;

    // MATERIALS_PENDING_V1：記下這筆教材的開放時間，供失敗時判斷「是不是還沒開放」。
    let opens_at = item_start_time(item);

    match item_type.as_str() {
        "material" | "file" | "resource" | "attachment" => {
            if let Some(uploads) = item.get("uploads").and_then(Value::as_array) {
                /* MATERIALS_MATERIAL_FOLDER_V1：多檔案教材給它自己的資料夾（單檔維持攤平）。 */
                let folder = material_folder_for(ctx, &title, uploads);
                for upload in uploads {
                    let _ = process_upload(
                        ctx,
                        group,
                        course_name,
                        &title,
                        folder.as_deref(),
                        upload,
                        opens_at,
                        outcome,
                    )
                    .await;
                }
            }
        }
        "web_link" | "link" | "external_link" => {
            let url = item
                .pointer("/data/link")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|s| !s.is_empty());
            if let Some(url) = url {
                match write_link_file(ctx.root, group, course_name, &title, &url).await {
                    Ok(wrote) => {
                        if wrote {
                            outcome.saved += 1;
                        } else {
                            outcome.skipped += 1;
                        }
                        outcome.links += 1;
                    }
                    Err(e) => {
                        outcome.failed += 1;
                        outcome.errors.push(format!("[{}] {} 寫入連結檔失敗：{e}", course_name, title));
                    }
                }
            }
        }
        "online_video" | "video" => {
            if let Some(uploads) = item.get("uploads").and_then(Value::as_array) {
                let mut dl = false;
                let folder = material_folder_for(ctx, &title, uploads);
                for upload in uploads {
                    if process_upload(
                        ctx,
                        group,
                        course_name,
                        &title,
                        folder.as_deref(),
                        upload,
                        opens_at,
                        outcome,
                    )
                    .await
                    {
                        dl = true;
                    }
                }
                if !dl {
                    // 影片無可下載物件 → 存連結檔(課程頁)。
                    let url = format!("{}/course/{}/courseware", ctx.base_url, item.get("course_id").and_then(Value::as_u64).unwrap_or_default());
                    let _ = write_link_file(ctx.root, group, course_name, &title, &url).await;
                    outcome.links += 1;
                }
            }
        }
        _ => {
            // page / forum / 其他 → 存連結檔。避免無截止時間素材灌進作業頁,這裡只是留個參照。
        }
    }
}

/// 取 upload 的檔名（沒有 `name`／`file_name` 就用教材標題）。
fn upload_file_name(upload: &Value, material_title: &str) -> String {
    upload
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| upload.get("file_name").and_then(Value::as_str))
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| material_title.to_string())
}

/// MATERIALS_MATERIAL_FOLDER_V1（fix101）：這個教材要不要有自己的資料夾？
///
/// 判斷方式是「這個教材**真的會進教材庫**幾個檔案」：只算通過副檔名白名單的（使用者選了
/// 「全部匯入」時全部都算）—— 一個教材就算掛了 5 個 upload，其中 4 個是被跳過的影片，
/// 那它只會有一個檔案，也就沒有必要開資料夾。
fn material_folder_for(
    ctx: &ImportContext<'_>,
    title: &str,
    uploads: &[Value],
) -> Option<String> {
    let names: Vec<String> = uploads
        .iter()
        .map(|upload| upload_file_name(upload, title))
        .collect();
    if needs_material_folder(ctx.include_all_types, &names) {
        Some(title.to_string())
    } else {
        None
    }
}

/// MATERIALS_MATERIAL_FOLDER_V1：要不要替這個教材開資料夾？（純函式，方便測試）
///
/// 只有一個檔案的教材不開資料夾 —— 目錄列表才不會被一堆單檔資料夾洗掉；
/// 兩個以上才開（學校 App 的呈現就是這樣）。**被白名單跳過的檔案不算**：
/// 一個教材掛了 5 個 upload、其中 4 個是被略過的影片，實際只會進 1 個檔案。
fn needs_material_folder(include_all_types: bool, names: &[String]) -> bool {
    names
        .iter()
        .filter(|name| include_all_types || is_allowed_file_type(name))
        .count()
        >= 2
}

/// 下載單一 upload 並寫入磁碟。回傳是否成功下載(供影片分支判斷)。
async fn process_upload(
    ctx: &ImportContext<'_>,
    group: &str,
    course_name: &str,
    material_title: &str,
    material_folder: Option<&str>,
    upload: &Value,
    opens_at: Option<DateTime<Utc>>,
    outcome: &mut ItemOutcome,
) -> bool {
    let name = upload_file_name(upload, material_title);

    // 資料夾到「分班/課程」這一層；MATERIALS_MATERIAL_FOLDER_V1 再加一層教材資料夾。
    // MATERIALS_IMPORT_V2: <root>/<group>/<course_name>/<file>
    // MATERIALS_MATERIAL_FOLDER_V1: <root>/<group>/<course_name>/<material>/<file>
    let course_dir = ctx
        .root
        .join(sanitize_component(group))
        .join(sanitize_component(course_name));
    let dir = match material_folder {
        Some(folder) => course_dir.join(sanitize_component(folder)),
        None => course_dir.clone(),
    };
    let filename = sanitize_filename(&name);
    if filename.is_empty() {
        return false;
    }
    // 只保留 pdf/ppt/圖片，其它的先跳過。
    //
    // IMPORT_ALL_TYPES_V1（fix64）：使用者要求「遇到不是許可的檔名要讓他選」。
    // 因此預設行為不變（跳過、計入 skipped），但多記一筆 disallowed ＋ 檔名樣本，
    // 讓前端在完成後問「這些要不要也抓？」；使用者按確定後前端會帶 all_types=1
    // 再跑一次，這次 ctx.include_all_types 為 true，就直接抓下來。
    if !is_allowed_file_type(&filename) {
        if !ctx.include_all_types {
            outcome.skipped += 1;
            outcome.disallowed += 1;
            if outcome.disallowed_files.len() < DISALLOWED_SAMPLE_LIMIT {
                outcome
                    .disallowed_files
                    .push(format!("{course_name}／{filename}"));
            }
            warn!(
                course = %course_name,
                file = %filename,
                "教材匯入：副檔名不在允許清單，先跳過（可由使用者決定是否仍要匯入）"
            );
            return false;
        }
        debug!(
            course = %course_name,
            file = %filename,
            "教材匯入：副檔名不在允許清單，但使用者選擇全部匯入 → 照抓"
        );
    }
    // 先判斷「要不要做」再建目錄：原本連已存在、要跳過的檔案都先建一次目錄
    // （每個檔案一次 syscall，幾百個檔案就多幾百次）。
    let dest = dir.join(&filename);
    if dest.exists() {
        outcome.skipped += 1;
        debug!(course = %course_name, file = %filename, "教材匯入：已存在，跳過（共用省硬碟）");
        return true;
    }
    /* MATERIALS_MATERIAL_FOLDER_V1：這次要放進教材資料夾，而課程層已經有同名舊檔
       （fix100 以前是攤平的）→ 直接搬進來：不重抓、不留兩份、也不會出現「同一個檔案兩份」。
       搬不動（極少數跨裝置或權限問題）就走原本的下載流程。 */
    if material_folder.is_some() {
        let legacy = course_dir.join(&filename);
        if legacy.exists() {
            if tokio::fs::create_dir_all(&dir).await.is_ok() {
                match tokio::fs::rename(&legacy, &dest).await {
                    Ok(()) => {
                        outcome.moved += 1;
                        debug!(
                            course = %course_name,
                            file = %filename,
                            folder = ?material_folder,
                            "教材匯入：舊的攤平檔案已移入教材資料夾"
                        );
                        return true;
                    }
                    Err(e) => {
                        debug!(
                            course = %course_name,
                            file = %filename,
                            error = %e,
                            "教材匯入：舊檔搬移失敗，改用重新下載"
                        );
                    }
                }
            }
        }
    }
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        outcome.failed += 1;
        outcome.errors.push(format!("[{}] 建立目錄失敗：{e}", dir.display()));
        return false;
    }
    // MATERIALS_IMPORT_FAST_V1（fix59）：併行處理時，同一個目標檔可能被兩個任務同時
    // 看上（同名的教材、或影片與教材指向同一個檔）→ 第二個直接讓給第一個做，
    // 不要重複抓同一份檔案。
    {
        let mut inflight = ctx
            .inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !inflight.insert(dest.clone()) {
            outcome.skipped += 1;
            debug!(
                course = %course_name,
                file = %filename,
                "教材匯入：同一個檔名正在由其他任務下載，跳過"
            );
            return true;
        }
    }
    let _inflight_guard = InflightGuard {
        set: &ctx.inflight,
        path: dest.clone(),
    };

    // MATERIALS_IMPORT_S3_V1: 以 upload.id 走 /api/uploads/{id}/preview 取得
    // tc-minio 的 S3 URL(走 :443、繞過 :8085 檔案埠)，由 download_file 依序嘗試。
    let mut download_urls = resolve_download_urls(ctx.client, ctx.base_url, upload).await;

    let mut has_signed_url = download_urls.iter().any(|url| !url.contains("/download/"));

    if !has_signed_url {
        // MATERIALS_IMPORT_FAST_V1（fix59）：已經知道「還沒到開放時間」的教材，iClass
        // 當下不會給檔案（連簽章網址都沒有）。原本這裡還是會重登一次、再依序硬打 5 個
        // 備援位址——一個檔案就要數十秒。現在直接記成 pending，不做任何網路嘗試。
        if let MaterialFailureKind::NotOpenYet(note) =
            classify_material_failure(opens_at, Utc::now(), false)
        {
            outcome.pending += 1;
            outcome.errors.push(format!("[{course_name}] {filename}：{note}"));
            debug!(
                course = %course_name,
                file = %filename,
                note = %note,
                "教材匯入：尚未開放，略過（開放後重跑即會下載）"
            );
            return false;
        }

        // MATERIALS_RELOGIN_RETRY_V1（fix48）：只剩 `/download/{key}` 備援位址（或完全
        // 沒有）時，多半是 session 過期——preview 端點回了登入頁，所以解析不到簽章網址。
        // MATERIALS_IMPORT_FAST_V1（fix59）：整次匯入只重登「一次」，而且成功後的連線
        // 所有檔案共用（原本每個檔案都各自完整登入一次）。
        if let Some(fresh) = fresh_client_after_relogin(ctx).await {
            let retried = resolve_download_urls(&fresh, ctx.base_url, upload).await;
            if retried.iter().any(|url| !url.contains("/download/")) || download_urls.is_empty() {
                download_urls = retried;
                has_signed_url = download_urls.iter().any(|url| !url.contains("/download/"));
                if has_signed_url {
                    debug!(
                        course = %course_name,
                        file = %filename,
                        "教材匯入：session 已重建，取得簽章下載位址"
                    );
                }
            }
        }
    }

    // 全域下載上限（MATERIALS_IMPORT_FAST_V1）：跨課程共用一個號誌，同時最多
    // IMPORT_MAX_CONCURRENT_DOWNLOADS 個檔案在下載——併行之餘仍不對學校開太多連線。
    let _download_permit = ctx.downloads.acquire().await.ok();

    // 直接串流寫檔：download_file 會把內容串流寫進 dest（先寫到 .part 再改名），
    // 避免把數 GB 檔案讀進記憶體；回傳的是真正寫入的 bytes 數。
    match download_file(ctx.client, &download_urls, &dest).await {
        Ok(size) if size > 0 => {
            outcome.saved += 1;
            true
        }
        Ok(_) => {
            outcome.failed += 1;
            outcome.errors.push(format!("[{}] {filename} 下載到 0 bytes", course_name));
            warn!(course = %course_name, file = %filename, "教材匯入：下載到 0 bytes");
            false
        }
        Err(e) => {
            // 清理可能殘留的 .part 半成品。
            let _ = tokio::fs::remove_file(dest.with_extension("part")).await;
            // MATERIALS_PENDING_V1：先分辨「尚未開放」與真正的故障，再決定記錄方式。
            match classify_material_failure(opens_at, Utc::now(), has_signed_url) {
                MaterialFailureKind::NotOpenYet(note) => {
                    outcome.pending += 1;
                    outcome.errors.push(format!("[{course_name}] {filename}：{note}"));
                    debug!(
                        course = %course_name,
                        file = %filename,
                        note = %note,
                        "教材匯入：尚未開放，略過（開放後重跑即會下載）"
                    );
                }
                MaterialFailureKind::NoDownloadUrl(note) => {
                    outcome.failed += 1;
                    outcome.errors.push(format!("[{course_name}] {filename}：{note}"));
                    warn!(
                        course = %course_name,
                        file = %filename,
                        note = %note,
                        error = %e,
                        "教材匯入：上游沒有提供下載位址"
                    );
                }
                MaterialFailureKind::Other => {
                    outcome.failed += 1;
                    outcome.errors.push(format!("[{course_name}] {filename} 下載失敗：{e}"));
                    error!(
                        course = %course_name,
                        file = %filename,
                        error = %e,
                        "教材匯入：下載檔案失敗"
                    );
                }
            }
            false
        }
    }
}

/// MATERIALS_IMPORT_S3_V1: 解析單一 upload 的可能下載候選 URL 清單。
///
/// 優先走**官方 S3 預覽通道**(繞過 :8085 檔案埠)：
///   1. `GET {base}/api/uploads/{upload_id}/preview` → JSON 內含 `url`
///      （tc-minio.tku.edu.tw 的簽章連結，走 :443、伺服器也抓得到）。
///   2. 若無 upload_id，退回以 `key` 拼的舊位址（較不可靠）。
///   3. 回傳依序嘗試的清單，由 download_file 逐個試。
/// HOMEWORK_DETAIL_V1（fix41）：改為 `pub(super)`——作業附件下載走同一條
/// 「已知且實測過」的取檔路徑（單一來源，避免兩份解析漂移）。
pub(super) async fn resolve_download_urls(
    client: &reqwest::Client,
    base_url: &str,
    upload: &Value,
) -> Vec<String> {
    let mut urls = Vec::new();

    // 1. 用 upload.id 打 /preview 取得 S3 URL。
    if let Some(uid) = upload.get("id").and_then(Value::as_u64) {
        let preview_url = format!("{base_url}/api/uploads/{uid}/preview");
        if let Ok(json) = fetch_json(client, &preview_url).await {
            if let Some(u) = json.get("url").and_then(Value::as_str) {
                if !u.trim().is_empty() {
                    urls.push(u.to_string());
                }
            }
            // 部分回應用其他鍵。
            for k in ["src", "download_url", "file", "content_url"] {
                if let Some(u) = json.get(k).and_then(Value::as_str) {
                    if !u.trim().is_empty() {
                        urls.push(u.to_string());
                    }
                }
            }
        }
    }

    // 2. 備援：以 key 拼 8085/主域(較不可靠，僅當 S3 未取得時)。
    if let Some(key) = upload
        .get("key")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
    {
        if let Ok(parsed) = reqwest::Url::parse(base_url) {
            let host = parsed.host_str().unwrap_or_default().to_string();
            if !host.is_empty() {
                // MATERIALS_IMPORT_FAST_V1（fix59）：原本列出 5 個候選（8085／8443 的
                // https＋http，再加 443）。學校 S3 化之後那些檔案埠早已關閉，每多一個
                // 候選就是一次白等的連線；只留 3 個 https 候選，而且動手前還會先做
                // 3 秒 TCP 探測（見 download_file）。
                for port in ["8085", "8443"] {
                    urls.push(format!("https://{host}:{port}/download/{key}"));
                }
                urls.push(format!("https://{host}/download/{key}"));
            }
        }
    }

    urls
}

/// 帶 cookie 下載真檔 bytes。
/// MATERIALS_IMPORT_S3_V1: 依序嘗試候選 URL，把回應寫入 dest 硬碟檔。
///
/// 與舊版不同處：
/// - **下載逾時(長)**：per-request 逾時從 10 秒提高到 **30 分鐘**——
///   上次被 10 秒掐死的大檔(如 MATLAB 安裝檔 >1GB)這次能跑完。
///   新的 dead-port(8085/8443)仍會因「連線失敗」在~10s 快速跳過，不會被長逾時拖住。
/// - **先寫 .part 再改名**：避免中斷時留下半個檔；成功才 rename 成最終檔名。
/// - 回傳真正寫入的 bytes 數（>0 才算成功）。
///
/// MATERIALS_IMPORT_FAST_V1（fix59）：備援的 `/download/{key}` 位址先用 3 秒 TCP 探測
/// （連不上就跳過，不必等 OS 的兩分鐘 SYN 重試），而且所有備援位址共用 20 秒的總預算
/// ——就算上游全掛，單一檔案也不會在備援位址上耗掉數分鐘。簽章網址（真正的教材內容）
/// 不受這兩個限制，30 分鐘的長逾時照舊。
async fn download_file(
    client: &reqwest::Client,
    urls: &[String],
    dest: &Path,
) -> Result<u64, String> {
    let read_timeout = Duration::from_secs(30 * 60);

    let mut last_err = String::new();
    let mut fallback_deadline: Option<Instant> = None;
    for url in urls {
        // 只有 `/download/{key}` 這條備援路徑需要保護；S3 簽章網址直接放行。
        if url.contains("/download/") {
            let deadline = *fallback_deadline.get_or_insert_with(|| {
                Instant::now() + Duration::from_secs(IMPORT_FALLBACK_BUDGET_SECS)
            });
            if Instant::now() >= deadline {
                last_err = format!(
                    "備援位址跳過（已用完 {IMPORT_FALLBACK_BUDGET_SECS} 秒預算）：{url}"
                );
                debug!(%url, "教材匯入：備援位址預算用盡，跳過");
                continue;
            }
            if !tcp_reachable(url, IMPORT_FALLBACK_PROBE_TIMEOUT_SECS).await {
                last_err = format!("連不上（{url}）");
                debug!(
                    url = %url,
                    "教材匯入：備援位址 TCP 探測失敗（學校檔案埠多半已關閉），跳過"
                );
                continue;
            }
        }
        // SSRF_FETCH_GUARD_V1：上游回傳的網址若指向本機或 metadata 位址，直接跳過（重新導向由 client 政策再檢查）。
        if let Ok(parsed) = reqwest::Url::parse(url) {
            if crate::api::is_blocked_fetch_url(&parsed)
                || crate::api::resolves_to_blocked_address(&parsed).await
            {
                last_err = format!("位址不允許（{url}）");
                debug!(url = %url, "教材匯入：位址指向本機或 link-local，已跳過");
                continue;
            }
        }
        debug!(url = %url, "教材匯入：嘗試下載位址");
        let req = client
            .get(url)
            .timeout(read_timeout) // 整段下載最多 30 分鐘；大檔放行、dead-port 仍以連線錯誤快速失敗
            .header(reqwest::header::ACCEPT, "application/octet-stream, */*")
            .header(reqwest::header::USER_AGENT, "Mozilla/5.0");

        let mut resp = match req.send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                last_err = format!("HTTP {}（{url}）", r.status());
                warn!(%url, status = r.status().as_u16(), "教材匯入：非 2xx，跳過此位址");
                continue;
            }
            Err(e) => {
                last_err = format!("連線失敗（{url}）：{e}");
                warn!(%url, error = %e, "教材匯入：連線失敗，跳過此位址");
                continue;
            }
        };

        let tmp = dest.with_extension("part");
        // IMPORT_STREAM_TO_DISK_V1（fix51）：邊收邊寫，不再整份先收進記憶體。
        //
        // 原本 resp.bytes() 會把整份教材累積成一個 Bytes（一堂課的影片就是幾百 MB
        // 的記憶體），而且「先全部讀完、再一次性寫入」讓網路與磁碟完全串起來跑。
        // 改成逐段寫入 .part 之後，下載與寫入重疊進行，記憶體固定在一段的量。
        // 寫入失敗或連線中斷都只影響這一個候選位址（與原本 bytes() 失敗時一致）。
        let mut out = match tokio::fs::File::create(&tmp).await {
            Ok(file) => file,
            Err(e) => {
                last_err = super::public_error("建立暫存檔失敗：", &e);
                warn!(url = %url, error = %last_err, "教材匯入：建立暫存檔失敗，跳過此位址");
                continue;
            }
        };
        let mut written: u64 = 0;
        let mut stream_err: Option<String> = None;
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    if chunk.is_empty() {
                        continue;
                    }
                    // IMPORT_REJECT_HTML_JSON_V1：第一片若是 HTML 登入頁／JSON 錯誤，
                    // 不要當成 zip／exe 落地。文字類檔名（.html／.json／.txt…）放行。
                    if written == 0
                        && !is_texty_file_name(
                            dest.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                        )
                        && looks_like_html_or_json_prefix(&chunk)
                    {
                        stream_err = Some(
                            "上游回的不是檔案（HTML／JSON 登入頁或錯誤訊息）".to_string(),
                        );
                        break;
                    }
                    if let Err(e) = out.write_all(&chunk).await {
                        stream_err = Some(super::public_error("寫入暫存檔失敗：", &e));
                        break;
                    }
                    written = written.saturating_add(chunk.len() as u64);
                }
                Ok(None) => break,
                Err(e) => {
                    stream_err = Some(super::public_error("讀取回應失敗：", &e));
                    break;
                }
            }
        }
        // 先關檔再改名，免得半截資料被當成完成品。
        drop(out);
        if let Some(err) = stream_err {
            let _ = tokio::fs::remove_file(&tmp).await;
            last_err = err;
            warn!(url = %url, error = %last_err, "教材匯入：下載中斷，跳過此位址");
            continue;
        }
        if written == 0 {
            let _ = tokio::fs::remove_file(&tmp).await;
            last_err = "下載到 0 bytes".to_string();
            warn!(url = %url, "教材匯入：下載到 0 bytes，跳過此位址");
            continue;
        }
        if let Err(e) = tokio::fs::rename(&tmp, dest).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            last_err = super::public_error("rename 失敗：", &e);
            warn!(url = %url, error = %last_err, "教材匯入：rename 失敗");
            continue;
        }
        debug!(url = %url, bytes = written, "教材匯入：下載成功");
        return Ok(written);
    }
    Err(last_err)
}

/// 抓取 JSON(帶 session cookie 的 client 已含 cookie jar)。
async fn fetch_json(client: &reqwest::Client, url: &str) -> Result<Value, String> {
    if let Ok(parsed) = reqwest::Url::parse(url) {
        if crate::api::is_blocked_fetch_url(&parsed)
            || crate::api::resolves_to_blocked_address(&parsed).await
        {
            return Err(format!("位址不允許（{url}）"));
        }
    }
    let resp = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    // 部分端點回 HTML/登入頁。
    if bytes.iter().copied().find(|b| !b.is_ascii_whitespace()) == Some(b'<') {
        return Err("回傳似乎是 HTML(可能 session 失效)".to_string());
    }
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

/// 寫 `.url` 連結檔(Windows InternetShortcut 格式,瀏覽器/檔案管理器可識別)。
/// 回傳 true=新寫入,false=已存在(跳過)。
async fn write_link_file(root: &Path, group: &str, course_name: &str, title: &str, url: &str) -> Result<bool, String> {
    // MATERIALS_IMPORT_V2: 連結檔同樣放「分班/課程」這層：<root>/<group>/<course_name>/<title>.url
    let dir = root
        .join(sanitize_component(group))
        .join(sanitize_component(course_name));
    tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
    let file_path = dir.join(format!("{}.url", sanitize_filename(title)));
    if file_path.exists() {
        return Ok(false);
    }
    let content = format!("[InternetShortcut]\nURL={url}\n");
    tokio::fs::write(&file_path, content).await.map_err(|e| e.to_string())?;
    Ok(true)
}

/// 寫一份匯入清單 manifest(供使用者對照)。
fn write_manifest(root: &Path, report: &ImportReport) {
    let dest = root.join("tronclass_materials_manifest.json");
    let obj = json!({
        "imported_at": chrono::Utc::now().to_rfc3339(),
        "courses_scanned": report.courses_scanned,
        "materials_found": report.materials_found,
        "files_saved": report.files_saved,
        "files_skipped": report.files_skipped,
        // MATERIALS_MATERIAL_FOLDER_V1：搬進教材資料夾的舊檔數量（對照用）。
        "files_moved": report.files_moved,
        "links_created": report.links_created,
        "failed": report.failed,
        // MATERIALS_PENDING_V1（fix48）：尚未開放而略過的數量（與 failed 分開記）。
        "pending": report.pending,
        // IMPORT_ALL_TYPES_V1（fix64）：被副檔名白名單先擋下的數量與樣本。
        // 目的：下次（或使用者決定「全部匯入」後）可以從 manifest 看得出來還少哪些。
        "disallowed": report.disallowed,
        "disallowed_files": report.disallowed_files,
    });
    // ATOMIC_WRITE_V1
    if let Err(e) = crate::atomic_write::write_atomic(&dest, serde_json::to_string_pretty(&obj).unwrap_or_default()) {
        tracing::warn!("無法寫入匯入 manifest：{e}");
    }
}

/// 移除路徑分隔/非法字元,確保只用單一段。
fn sanitize_component(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "未命名".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 檔名消毒(保留副檔名)。
fn sanitize_filename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "file".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 是否有「真實可下載」的副檔名判斷：PDF / 簡報(PPT) / Word / 圖片。
/// MATERIALS_IMPORT_FILTER_V2: 使用者要求「pdf、ppt、word、照片檔」都能匯入。
/// 影片、exe、zip 等其它的跳過(不失敗、計入 skipped)。
fn is_allowed_file_type(filename: &str) -> bool {
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.trim().to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        // 文件
        "pdf"
        | "doc"
        | "docx"
        | "rtf"
        | "odt"
        // 簡報
        | "ppt"
        | "pptx"
        | "pps"
        | "ppsx"
        // 圖片
        | "jpg"
        | "jpeg"
        | "png"
        | "gif"
        | "bmp"
        | "webp"
        | "svg"
        | "tif"
        | "tiff"
        | "heic"
    )
}

/// 依系所決定「分班資料夾」名稱(資料夾第一層)。
///
/// MATERIALS_IMPORT_V1
/// - `department` 為 None(無系所資訊) → 用課程名稱本身當分班(避免空白)。
/// - 非主要系所的共同課(通識/體育/核心課程/三全教育)統一歸到「共同科」。
/// - 其餘用系所全名。
fn group_folder_for(department: Option<&str>, course_name: &str) -> String {
    let Some(dept) = department.map(str::trim).filter(|s| !s.is_empty()) else {
        return "未分類".to_string();
    };
    // 共同課關鍵字 → 共同科(依系所)
    let dept_is_general = [
        "核心課程",
        "體育興趣",
        "體育",
        "三全教育",
        "通識",
        "共同科",
        "軍訓",
        "全民國防",
    ]
    .iter()
    .any(|kw| dept.contains(kw));
    // 即使系所是主系所(如資工),課程名仍含通識/服務/體育等 → 歸共同科。
    // 例:「男、女生體育」「校園與社區服務學習」系所雖為資工,實為共同課。
    let name_is_general = [
        "體育",
        "服務學習",
        "通識",
        "全民國防",
        "軍事訓練",
        "軍訓",
        "大學學習",
        "探索永續",
        "公民",
        "國際學習",
        "核心課程",
    ]
    .iter()
    .any(|kw| course_name.contains(kw));
    if dept_is_general || name_is_general {
        return "共同科".to_string();
    }
    // 主系所 → 用系所全名;若系所名過於籠統(如只有「日」),退回課程名。
    if dept == "日" || dept == "夜" || dept.is_empty() {
        course_name.to_string()
    } else {
        dept.to_string()
    }
}

/// 教材的目的資料夾(避免重複路徑邏輯)。
///
/// MATERIALS_IMPORT_V2: `<root>/<group>/<course>`
/// MATERIALS_MATERIAL_FOLDER_V1（fix101）：`material` 有值時多一層教材資料夾
///   → `<root>/<group>/<course>/<教材標題>`（一個教材含多個檔案時使用）。
#[allow(dead_code)]
fn material_dir(root: &Path, group: &str, course: &str, material: Option<&str>) -> PathBuf {
    let base = root
        .join(sanitize_component(group))
        .join(sanitize_component(course));
    match material {
        Some(title) => base.join(sanitize_component(title)),
        None => base,
    }
}

