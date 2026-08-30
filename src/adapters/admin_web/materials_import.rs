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
#[derive(Debug, Serialize)]
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

/// 實際匯入教材的主 handler：`POST /admin/api/files/import`。
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

    let root = materials_root();
    let report = run_import(&client, account.base_url(), &root, only_courses.as_deref()).await;

    Json(json!(report)).into_response()
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

    // 2. 每門課抓 activities。
    for (idx, course) in courses.iter().enumerate() {
        if idx > 0 {
            // 節流：課程間請等待,避免打爆學校 WAF。
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
        // MATERIALS_IMPORT_V1: 依系所決定「分班資料夾」。
        // 結構 = <root>/<分班>/<課程>/<教材>/<檔名>。
        // 共同科(通識/體育/核心課程/三全教育)統一歸到「共同科」。
        let group = group_folder_for(course.department.as_deref(), &course.name);
        let base = format!("{}/api/courses/{}", base_url, course.id);
        let activities_url = format!("{base}/activities?page=1&page_size=300");
        let Ok(json) = fetch_json(client, &activities_url).await else {
            report.failed += 1;
            report.errors.push(format!("[{}] 讀取 activities 失敗", course.name));
            continue;
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
            }
        } else if !hw_items.is_empty() {
            for item in hw_items.iter() {
                process_item(client, base_url, root, &group, &course.name, item, &mut report).await;
            }
        }
    }

    // 3. 寫 manifest。
    write_manifest(root, &report);
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
    let key = upload
        .get("key")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty());
    let Some(key) = key else {
        return false;
    };

    let name = upload
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| upload.get("file_name").and_then(Value::as_str))
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| material_title.to_string());

    let dir = root
        .join(sanitize_component(group))
        .join(sanitize_component(course_name))
        .join(sanitize_component(material_title));
    let filename = sanitize_filename(&name);
    if filename.is_empty() {
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
        return true;
    }

    match download_file(client, base_url, &key).await {
        Ok(bytes) => {
            if bytes.is_empty() {
                report.failed += 1;
                report.errors.push(format!("[{}] {filename} 下載到 0 bytes", course_name));
                return false;
            }
            if let Err(e) = tokio::fs::write(&dest, &bytes).await {
                report.failed += 1;
                report.errors.push(format!("[{}] {filename} 寫入失敗：{e}", course_name));
                return false;
            }
            report.files_saved += 1;
            true
        }
        Err(e) => {
            report.failed += 1;
            report.errors.push(format!("[{}] {filename} 下載失敗：{e}", course_name));
            false
        }
    }
}

/// 依 upload `key` 組出下載候選位址(參考社群:檔案由 :8085 /download/{key} 提供)。
/// 回傳「依序嘗試」的清單;萬一某個位址對特定主機不適用,會自動跳下一個。
fn download_candidates(base_url: &str, key: &str) -> Vec<String> {
    let parsed = reqwest::Url::parse(base_url).ok();
    let scheme = parsed
        .as_ref()
        .map(|u| u.scheme().to_string())
        .unwrap_or_else(|| "https".to_string());
    let host = parsed
        .as_ref()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    if host.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    // 檔案服務常跑在非標準 port。社群解碼出的位址帶 port 且可能為 http，
    // 故每 port 同時試 http 與 https，再依序跳過失敗的位址。
    for port in ["8085", "8443", "8081"] {
        out.push(format!("https://{host}:{port}/download/{key}"));
        out.push(format!("http://{host}:{port}/download/{key}"));
    }
    // 保留同源路徑作為最後備援(有些站把 download 放在主域)。
    out.push(format!("{scheme}://{host}/download/{key}"));
    out
}

/// 帶 cookie 下載真檔 bytes。
async fn download_file(client: &reqwest::Client, base_url: &str, key: &str) -> Result<Vec<u8>, String> {
    let candidates = download_candidates(base_url, key);
    let mut last_err = String::new();
    for url in candidates {
        let mut req = client.get(&url);
        req = req
            .header(reqwest::header::REFERER, base_url)
            .header(reqwest::header::ACCEPT, "application/octet-stream, */*");
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(bytes) = resp.bytes().await {
                    return Ok(bytes.to_vec());
                }
                last_err = "讀取回應失敗".to_string();
            }
            _ => {
                last_err = format!("HTTP 非 2xx（{url}）");
            }
        }
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
    let dir = root
        .join(sanitize_component(group))
        .join(sanitize_component(course_name))
        .join(sanitize_component(title));
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
#[allow(dead_code)]
fn course_material_dir(root: &Path, group: &str, course: &str, title: &str) -> PathBuf {
    root.join(sanitize_component(group))
        .join(sanitize_component(course))
        .join(sanitize_component(title))
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
    fn download_candidates_include_8085() {
        let cands = download_candidates("https://iclass.tku.edu.tw", "abcd");
        assert!(cands.iter().any(|u| u.contains(":8085/download/abcd")));
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
