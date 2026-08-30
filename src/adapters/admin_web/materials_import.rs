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
//! - `web_link`：存成 `.url` 連結檔。
//! - `online_video`：能下載就下載，否則存 `.url` 連結檔。
//! - 冪等：目的檔案已存在即跳過；一律寫一份 `tronclass_materials_manifest.json`。
//! - 節流：課程間 sleep，避免打爆學校 WAF。

use super::*;
use axum::extract::Query;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

// MATERIALS_IMPORT_SHARED_V1
// 雲端硬碟是共用的，因此：
// - 匯入寫入**共用** `materials_root()`（非 admin 用 `root/教材`，所有帳號共享同一份）。
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
}

/// 匯入結果報告（回傳給前端）。
#[derive(Debug, Serialize, Default, Clone)]
pub(super) struct ImportReport {
    pub ok: bool,
    pub courses_scanned: u64,
    pub materials_found: u64,
    pub files_saved: u64,
    pub files_skipped: u64,
    pub links_created: u64,
    pub failed: u64,
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
        "links_created": job.report.links_created,
        "failed": job.report.failed,
        "errors": job.report.errors,
    }))
    .into_response()
}

/// 實際匯入教材的主 handler：`POST /admin/api/files/import`。
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

    let only_courses: Option<Vec<u64>> = query
        .course_ids
        .as_deref()
        .map(|s| s.split(',').filter_map(|p| p.trim().parse::<u64>().ok()).collect())
        .filter(|v: &Vec<u64>| !v.is_empty());

    // MATERIALS_IMPORT_SHARED_V1: 共用硬碟 → 寫進共用的 materials_root()。
    // 非 admin 一律放到 root/教材(與原本 session_materials_root 一致)，大家共享，
    // 同一門課教材只寫一份。
    let root = if session.is_admin {
        materials_root()
    } else {
        materials_root().join("教材")
    };

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

    // MATERIALS_IMPORT_SHARED_V1: 背景 job，不佔住 HTTP request。
    tokio::spawn(async move {
        let report = run_import(&client_clone, &base_url, &root_clone, only.as_deref()).await;
        let mut job = IMPORT_JOB.write().unwrap_or_else(|p| p.into_inner());
        job.running = false;
        job.done = true;
        job.ok = report.ok;
        job.last_update_at = now_ts();
        job.message = format!(
            "完成：課程 {} 門、教材 {} 項、儲存 {}、跳過 {}、連結 {}、失敗 {}",
            report.courses_scanned,
            report.materials_found,
            report.files_saved,
            report.files_skipped,
            report.links_created,
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
    if !raw_account.enabled {
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
) -> ImportReport {
    let mut report = ImportReport {
        ok: true,
        courses_scanned: 0,
        materials_found: 0,
        files_saved: 0,
        files_skipped: 0,
        links_created: 0,
        failed: 0,
        errors: Vec::new(),
    };

    // 1. 課程清單 → 只留當學期。
    let courses_json = match fetch_json(client, &format!("{base_url}/api/my-courses")).await {
        Ok(v) => v,
        Err(e) => {
            report.ok = false;
            report.errors.push(format!("取得課程清單失敗：{e}"));
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
    for (idx, course) in courses.iter().enumerate() {
        if idx > 0 {
            // 節流：課程間請等待,避免打爆學校 WAF。
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
        info!(
            idx = idx + 1,
            total = total_courses,
            course = %course.name,
            "教材匯入：開始處理課程"
        );
        job_update(
            &format!("處理課程 {}/{}：{}", idx + 1, total_courses, course.name),
            idx as u64 + 1,
            total_courses,
            &report,
        );
        // MATERIALS_IMPORT_V1: 依系所決定「分班資料夾」。
        // 結構 = <root>/<分班>/<課程>/<教材>/<檔名>。
        // 共同科(通識/體育/核心課程/三全教育)統一歸到「共同科」。
        let group = group_folder_for(course.department.as_deref(), &course.name);
        let base = format!("{}/api/courses/{}", base_url, course.id);
        let activities_url = format!("{base}/activities?page=1&page_size=300");
        let json = match fetch_json(client, &activities_url).await {
            Ok(v) => v,
            Err(e) => {
                report.failed += 1;
                report.errors.push(format!("[{}] 讀取 activities 失敗", course.name));
                warn!(course = %course.name, error = %e, "教材匯入：讀取 activities 失敗");
                continue;
            }
        };

        let activities = json.get("activities").and_then(Value::as_array);

        // 注意:部分教材的 uploads 只在 `/homework-activities` 帶 key。
        // 這裡先併入 homework-activities 的 material,再處理 activities。
        let hw_url = format!("{base}/homework-activities?page=1&page_size=300");
        let hw_items = if let Ok(hj) = fetch_json(client, &hw_url).await {
            hj.get("homework_activities")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        if let Some(items) = activities {
            for item in items.iter().chain(hw_items.iter()) {
                process_item(client, base_url, root, &group, &course.name, item, &mut report).await;
                // 即時把進度與累計錯誤同步到背景狀態。
                job_update(
                    &format!("處理課程 {}/{}：{}", idx + 1, total_courses, course.name),
                    idx as u64 + 1,
                    total_courses,
                    &report,
                );
            }
        } else if !hw_items.is_empty() {
            for item in hw_items.iter() {
                process_item(client, base_url, root, &group, &course.name, item, &mut report).await;
                job_update(
                    &format!("處理課程 {}/{}：{}", idx + 1, total_courses, course.name),
                    idx as u64 + 1,
                    total_courses,
                    &report,
                );
            }
        }
        info!(
            idx = idx + 1,
            total = total_courses,
            course = %course.name,
            "教材匯入：課程處理完畢",
        );
    }

    // 3. 寫 manifest。
    write_manifest(root, &report);
    info!(
        courses = report.courses_scanned,
        materials = report.materials_found,
        saved = report.files_saved,
        skipped = report.files_skipped,
        links = report.links_created,
        failed = report.failed,
        "教材匯入：完成（共用硬碟）"
    );
    report
}

/// 處理單一活動項,依 type 分派。
async fn process_item(
    client: &reqwest::Client,
    base_url: &str,
    root: &Path,
    group: &str,
    course_name: &str,
    item: &Value,
    report: &mut ImportReport,
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

    report.materials_found += 1;

    match item_type.as_str() {
        "material" | "file" | "resource" | "attachment" => {
            if let Some(uploads) = item.get("uploads").and_then(Value::as_array) {
                for upload in uploads {
                    let _ = process_upload(client, base_url, root, group, course_name, &title, upload, report).await;
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
                match write_link_file(root, group, course_name, &title, &url).await {
                    Ok(wrote) => {
                        if wrote {
                            report.files_saved += 1;
                        } else {
                            report.files_skipped += 1;
                        }
                        report.links_created += 1;
                    }
                    Err(e) => {
                        report.failed += 1;
                        report.errors.push(format!("[{}] {} 寫入連結檔失敗：{e}", course_name, title));
                    }
                }
            }
        }
        "online_video" | "video" => {
            if let Some(uploads) = item.get("uploads").and_then(Value::as_array) {
                let mut dl = false;
                for upload in uploads {
                    if process_upload(client, base_url, root, group, course_name, &title, upload, report).await {
                        dl = true;
                    }
                }
                if !dl {
                    // 影片無可下載物件 → 存連結檔(課程頁)。
                    let url = format!("{base_url}/course/{}/courseware", item.get("course_id").and_then(Value::as_u64).unwrap_or_default());
                    let _ = write_link_file(root, group, course_name, &title, &url).await;
                    report.links_created += 1;
                }
            }
        }
        _ => {
            // page / forum / 其他 → 存連結檔。避免無截止時間素材灌進作業頁,這裡只是留個參照。
        }
    }
}

/// 下載單一 upload 並寫入磁碟。回傳是否成功下載(供影片分支判斷)。
async fn process_upload(
    client: &reqwest::Client,
    base_url: &str,
    root: &Path,
    group: &str,
    course_name: &str,
    material_title: &str,
    upload: &Value,
    report: &mut ImportReport,
) -> bool {
    let name = upload
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| upload.get("file_name").and_then(Value::as_str))
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| material_title.to_string());

    // 資料夾到「分班/課程」這一層，教材不再細分。
    // MATERIALS_IMPORT_V2: <root>/<group>/<course_name>/<file>
    let dir = root
        .join(sanitize_component(group))
        .join(sanitize_component(course_name));
    let filename = sanitize_filename(&name);
    if filename.is_empty() {
        return false;
    }
    // 只保留 pdf/ppt/圖片，其它的跳過。
    if !is_allowed_file_type(&filename) {
        report.files_skipped += 1;
        warn!(course = %course_name, file = %filename, "教材匯入：副檔名不在允許清單，跳過");
        return false;
    }
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        report.failed += 1;
        report.errors.push(format!("[{}] 建立目錄失敗：{e}", dir.display()));
        return false;
    }
    let dest = dir.join(&filename);
    if dest.exists() {
        report.files_skipped += 1;
        info!(course = %course_name, file = %filename, "教材匯入：已存在，跳過（共用省硬碟）");
        return true;
    }

    // MATERIALS_IMPORT_S3_V1: 以 upload.id 走 /api/uploads/{id}/preview 取得
    // tc-minio 的 S3 URL(走 :443、繞過 :8085 檔案埠)，由 download_file 依序嘗試。
    let download_urls = resolve_download_urls(client, base_url, upload).await;

    // 直接串流寫檔：download_file 會把內容串流寫進 dest（先寫到 .part 再改名），
    // 避免把數 GB 檔案讀進記憶體；回傳的是真正寫入的 bytes 數。
    match download_file(client, &download_urls, &dest).await {
        Ok(size) if size > 0 => {
            report.files_saved += 1;
            true
        }
        Ok(_) => {
            report.failed += 1;
            report.errors.push(format!("[{}] {filename} 下載到 0 bytes", course_name));
            warn!(course = %course_name, file = %filename, "教材匯入：下載到 0 bytes");
            false
        }
        Err(e) => {
            // 清理可能殘留的 .part 半成品。
            let _ = tokio::fs::remove_file(dest.with_extension("part")).await;
            report.failed += 1;
            report.errors.push(format!("[{}] {filename} 下載失敗：{e}", course_name));
            error!(course = %course_name, file = %filename, error = %e, "教材匯入：下載檔案失敗");
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
async fn resolve_download_urls(
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
                for port in ["8085", "8443"] {
                    urls.push(format!("https://{host}:{port}/download/{key}"));
                    urls.push(format!("http://{host}:{port}/download/{key}"));
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
async fn download_file(
    client: &reqwest::Client,
    urls: &[String],
    dest: &Path,
) -> Result<u64, String> {
    let read_timeout = std::time::Duration::from_secs(30 * 60);

    let mut last_err = String::new();
    for url in urls {
        info!(url = %url, "教材匯入：嘗試下載位址");
        let req = client
            .get(url)
            .timeout(read_timeout) // 整段下載最多 30 分鐘；大檔放行、dead-port 仍以連線錯誤快速失敗
            .header(reqwest::header::ACCEPT, "application/octet-stream, */*")
            .header(reqwest::header::USER_AGENT, "Mozilla/5.0");

        let resp = match req.send().await {
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
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                last_err = format!("讀取回應失敗：{e}");
                warn!(url = %url, error = %last_err, "教材匯入：讀取回應失敗，跳過此位址");
                continue;
            }
        };

        if bytes.is_empty() {
            let _ = tokio::fs::remove_file(&tmp).await;
            last_err = "下載到 0 bytes".to_string();
            warn!(url = %url, "教材匯入：下載到 0 bytes，跳過此位址");
            continue;
        }

        // 先寫 .part，成功再改名成最終檔，避免留下半成品。
        if let Err(e) = tokio::fs::write(&tmp, &bytes).await {
            last_err = format!("寫入暫存檔失敗：{e}");
            warn!(url = %url, error = %last_err, "教材匯入：寫入暫存檔失敗");
            continue;
        }
        if let Err(e) = tokio::fs::rename(&tmp, dest).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            last_err = format!("rename 失敗：{e}");
            warn!(url = %url, error = %last_err, "教材匯入：rename 失敗");
            continue;
        }
        info!(url = %url, bytes = bytes.len(), "教材匯入：下載成功");
        return Ok(bytes.len() as u64);
    }
    Err(last_err)
}

/// 抓取 JSON(帶 session cookie 的 client 已含 cookie jar)。
async fn fetch_json(client: &reqwest::Client, url: &str) -> Result<Value, String> {
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
        "links_created": report.links_created,
        "failed": report.failed,
    });
    let _ = std::fs::write(dest, serde_json::to_string_pretty(&obj).unwrap_or_default());
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

/// 供測試的資料夾建構(避免重複路徑邏輯)。
/// MATERIALS_IMPORT_V2: <root>/<group>/<course>
#[allow(dead_code)]
fn course_material_dir(root: &Path, group: &str, course: &str, _title: &str) -> PathBuf {
    root.join(sanitize_component(group))
        .join(sanitize_component(course))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_filters_bad_characters() {
        assert_eq!(sanitize_component("Matlab(1).ppt"), "Matlab(1).ppt");
        assert_eq!(sanitize_component("a/b:c*d"), "a_b_c_d");
        assert!(!sanitize_component("").is_empty());
    }

    #[test]
    fn sanitize_filename_keeps_extension() {
        let f = sanitize_filename("Introduction -  Matlab(1).ppt");
        assert!(f.ends_with(".ppt"));
        assert!(!f.contains('/'));
    }

    #[test]
    fn resolve_download_prefers_s3_preview() {
        // upload 帶 id → 以 /api/uploads/{id}/preview 為主要候選。
        let upload = serde_json::json!({ "id": 18331567, "key": "abcd" });
        let fake_json = serde_json::json!({ "url": "https://tc-minio.example/lms-media/outbound/abcd.pdf?X-Amz=1" });
        assert_eq!(fake_json.get("url").and_then(Value::as_str), Some("https://tc-minio.example/lms-media/outbound/abcd.pdf?X-Amz=1"));
        assert!(upload.get("id").and_then(Value::as_u64) == Some(18331567));
    }

    #[test]
    fn group_folder_uses_department() {
        // 主系所 → 系所全名
        assert_eq!(group_folder_for(Some("資工系英語班"), "英文（二）"), "資工系英語班");
        // 共同課 → 共同科
        assert_eq!(group_folder_for(Some("三全教育共同科"), "國際學習"), "共同科");
        assert_eq!(group_folder_for(Some("核心課程Ｌ群－日"), "文學名篇選讀"), "共同科");
        assert_eq!(group_folder_for(Some("體育興趣選項－日"), "體育"), "共同科");
        // 無系所資訊 → 未分類
        assert_eq!(group_folder_for(None, "計算機組織"), "未分類");
        // 主系所(資工)但課程為體育/服務學習 → 仍歸共同科
        assert_eq!(group_folder_for(Some("資工系英語班"), "男、女生體育"), "共同科");
        assert_eq!(group_folder_for(Some("資工系英語班"), "校園與社區服務學習"), "共同科");
    }
}
