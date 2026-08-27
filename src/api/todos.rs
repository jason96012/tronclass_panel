//! Tronclass 作業與待辦事項 API 模組（完全重寫版）
//!
//! 專為 Tronclass（含淡江 iClass、輔仁、東海 iLearn 等各校實體架構）設計：
//! 1. 優先請求全域待辦 `/api/todos`（解析 `todo_list`）
//! 2. 請求學生修課清單 `/api/my-courses`（解析 `courses`）
//! 3. 併發讀取各課程作業 `/api/courses/{id}/homework-activities` 與活動 `/api/courses/{id}/activities`
//! 4. 具備高容錯多型資料解析（時間戳、繳交狀態、計分、遲交設定）

use chrono::{DateTime, Datelike, NaiveDateTime, TimeZone, Utc};
use futures::stream::StreamExt;
use miette::Result;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tracing::{debug, instrument};

// ─── 常數 ─────────────────────────────────────────────────────────────────────

/// 學期起點再往前寬限的天數。
///
/// 避免開學前幾天發布、截止日落在正式開學日之前的作業被誤刪。
const SEMESTER_START_GRACE_DAYS: i64 = 14;

/// 同時查詢課程作業／活動的最大並行數。
///
/// 設為 8：足以把 20 次往返壓進 3 波，又不會像爆破那樣打爆學校 WAF。
/// 背景排程器仍會在帳號之間 sleep 1 秒，整體流量依舊平緩。
const COURSE_FETCH_CONCURRENCY: usize = 8;

// ─── 資料模型 ─────────────────────────────────────────────────────────────────

/// 統一標準化的 Tronclass 作業/測驗資料結構
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TronclassTodoItem {
    pub id: u64,
    pub title: String,
    pub course_id: Option<u64>,
    pub course_name: Option<String>,
    pub department_name: Option<String>,
    pub item_type: String,
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    pub is_submitted: bool,
    pub submission_status: String,
    pub allow_late_submission: bool,
    pub score: Option<f64>,
}

impl TronclassTodoItem {
    /// 判斷作業是否已被學生繳交或已被老師評分
    pub fn is_completed_or_submitted(&self) -> bool {
        if self.is_submitted {
            return true;
        }
        let status = self.submission_status.trim().to_ascii_lowercase();
        matches!(
            status.as_str(),
            "submitted" | "graded" | "group_submitted" | "completed" | "pass" | "passed" | "done" | "finished"
        )
    }

    /// 判斷是否為進行中且未截止的待辦作業
    pub fn is_active_pending(&self, now: DateTime<Utc>) -> bool {
        if self.is_completed_or_submitted() {
            return false;
        }

        let Some(end) = self.end_time else {
            return false;
        };

        if end <= now {
            return false;
        }

        if let Some(start) = self.start_time {
            if start > now {
                return false;
            }
        }

        true
    }
}

// ─── API 客戶端重寫實作 ───────────────────────────────────────────────────────

impl super::ApiClient {
    /// 取得使用者的所有作業與待辦事項
    #[instrument(skip(self))]
    pub async fn get_todos(&self) -> Result<Vec<TronclassTodoItem>> {
        let mut results: Vec<TronclassTodoItem> = Vec::new();
        let mut seen_ids = HashSet::new();

        // ─────────────────────────────────────────────────────────────────────
        // 步驟 1: 查詢全域待辦清單 GET /api/todos
        // ─────────────────────────────────────────────────────────────────────
        let todos_url = format!("{}/api/todos", self.base_url);
        debug!(url = %todos_url, "正在查詢 Tronclass 全域待辦清單 /api/todos");

        if let Ok(resp) = self.client.get(&todos_url).send().await {
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED {
                return Err(miette::miette!(crate::api::ApiError::Unauthorized));
            }
            if status.is_success() {
                if let Ok(body_bytes) = resp.bytes().await {
                    if let Ok(json_val) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
                        let items = parse_todos_json(&json_val, None, None);
                        for item in items {
                            if seen_ids.insert(item.id) {
                                results.push(item);
                            }
                        }
                    }
                }
            }
        }

        // ─────────────────────────────────────────────────────────────────────
        // 步驟 2: 查詢學生修課清單 GET /api/my-courses 並獲取各科作業
        // ─────────────────────────────────────────────────────────────────────
        let my_courses_url = format!("{}/api/my-courses", self.base_url);
        debug!(url = %my_courses_url, "正在查詢學生修課清單 /api/my-courses");

        if let Ok(resp) = self.client.get(&my_courses_url).send().await {
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED {
                return Err(miette::miette!(crate::api::ApiError::Unauthorized));
            }
            if status.is_success() {
                if let Ok(body_bytes) = resp.bytes().await {
                    if let Ok(json_val) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
                        // CURRENT_SEMESTER_ONLY_V1
                        // 只保留當前學期仍在進行的課程，避免作業頁混入上學期／已封存
                        // 課程的舊作業。無法判斷學期的課程一律保留（寧可多顯示）。
                        let all_courses = parse_course_briefs(&json_val);
                        let total_courses = all_courses.len();
                        let courses: Vec<_> = all_courses
                            .into_iter()
                            .filter(|course| course.is_current != Some(false))
                            .collect();
                        if courses.len() < total_courses {
                            debug!(
                                total = total_courses,
                                kept = courses.len(),
                                skipped = total_courses - courses.len(),
                                "已略過非當前學期課程"
                            );
                        }
                        // CONCURRENT_COURSE_FETCH_V1
                        // 原本是循序 for 迴圈，每門課要等 2 個請求依序回來；
                        // 9 門課 = 18 次往返，網路稍慢就要 5～10 秒，作業頁一直轉圈。
                        // 改用 buffer_unordered 同時發送，耗時由「總和」變成「最慢的那批」。
                        let course_list: Vec<CourseBrief> = courses.into_iter().take(25).collect();
                        let mut request_urls = Vec::with_capacity(course_list.len() * 2);
                        for course in &course_list {
                            request_urls.push((
                                format!(
                                    "{}/api/courses/{}/homework-activities",
                                    self.base_url, course.id
                                ),
                                course.id,
                                course.name.clone(),
                            ));
                            request_urls.push((
                                format!("{}/api/courses/{}/activities", self.base_url, course.id),
                                course.id,
                                course.name.clone(),
                            ));
                        }

                        let fetched: Vec<Vec<TronclassTodoItem>> =
                            futures::stream::iter(request_urls.into_iter().map(
                                |(url, course_id, course_name)| async move {
                                    let Ok(resp) = self.client.get(&url).send().await else {
                                        return Vec::new();
                                    };
                                    if !resp.status().is_success() {
                                        return Vec::new();
                                    }
                                    let Ok(bytes) = resp.bytes().await else {
                                        return Vec::new();
                                    };
                                    let Ok(json) =
                                        serde_json::from_slice::<serde_json::Value>(&bytes)
                                    else {
                                        return Vec::new();
                                    };
                                    parse_todos_json(
                                        &json,
                                        Some(course_name.as_str()),
                                        Some(course_id),
                                    )
                                },
                            ))
                            .buffer_unordered(COURSE_FETCH_CONCURRENCY)
                            .collect()
                            .await;

                        // buffer_unordered 的完成順序不固定，這裡統一去重後收攏。
                        for items in fetched {
                            for item in items {
                                if seen_ids.insert(item.id) {
                                    results.push(item);
                                }
                            }
                        }
                    }
                }
            }
        }

        // CURRENT_SEMESTER_ONLY_V1（第二道防線）
        // 有些學校的 /api/my-courses 不提供可判讀的學期欄位，光靠課程過濾擋不掉
        // 舊課。這裡再以「本學期起點」為界剔除上學期殘留的作業。
        //
        // 刻意不用「今天 − N 天」的滾動門檻：那會在學期中後期誤刪本學期稍早的
        // 逾期作業（例如 12 月時把 9 月的作業藏起來）。改用固定的學期起點，
        // 整個學期內的作業都看得到，跨學期才會被濾掉。
        // 沒有截止時間的項目一律保留，避免誤刪長期開放的作業。
        let cutoff = current_semester_start(Utc::now());
        let before = results.len();
        results.retain(|item| match item.end_time {
            Some(end) => end >= cutoff,
            None => true,
        });
        if results.len() < before {
            debug!(
                total = before,
                kept = results.len(),
                skipped = before - results.len(),
                cutoff = %cutoff,
                "已略過上一學期的舊作業"
            );
        }

        Ok(results)
    }
}

// ─── 高容錯 JSON 解析工具 ─────────────────────────────────────────────────────

/// 從任意 Tronclass JSON 節點提取作業陣列
pub fn parse_todos_json(
    root: &serde_json::Value,
    fallback_course_name: Option<&str>,
    fallback_course_id: Option<u64>,
) -> Vec<TronclassTodoItem> {
    let mut items = Vec::new();

    // 1. 若本身即為陣列 [ ... ]
    if let Some(arr) = root.as_array() {
        for val in arr {
            if let Some(item) = parse_single_todo(val, fallback_course_name, fallback_course_id) {
                items.push(item);
            }
        }
        return items;
    }

    // 2. 若為物件，搜尋標準 Key
    if let Some(obj) = root.as_object() {
        for key in [
            "todo_list",
            "homework_activities",
            "activities",
            "todos",
            "assignments",
            "items",
            "results",
        ] {
            if let Some(arr) = obj.get(key).and_then(|v| v.as_array()) {
                for val in arr {
                    if let Some(item) = parse_single_todo(val, fallback_course_name, fallback_course_id) {
                        items.push(item);
                    }
                }
                if !items.is_empty() {
                    return items;
                }
            }
        }

        // 3. 搜尋 data 封裝層
        if let Some(data_val) = obj.get("data") {
            if let Some(arr) = data_val.as_array() {
                for val in arr {
                    if let Some(item) = parse_single_todo(val, fallback_course_name, fallback_course_id) {
                        items.push(item);
                    }
                }
                if !items.is_empty() {
                    return items;
                }
            } else if let Some(data_obj) = data_val.as_object() {
                for key in [
                    "todo_list",
                    "homework_activities",
                    "activities",
                    "todos",
                    "assignments",
                    "items",
                ] {
                    if let Some(arr) = data_obj.get(key).and_then(|v| v.as_array()) {
                        for val in arr {
                            if let Some(item) = parse_single_todo(val, fallback_course_name, fallback_course_id) {
                                items.push(item);
                            }
                        }
                        if !items.is_empty() {
                            return items;
                        }
                    }
                }
            }
        }
    }

    items
}

/// 解析單一節點為標準 `TronclassTodoItem`
pub fn parse_single_todo(
    val: &serde_json::Value,
    fallback_course_name: Option<&str>,
    fallback_course_id: Option<u64>,
) -> Option<TronclassTodoItem> {
    let obj = val.as_object()?;

    // 1. 唯一標識 ID
    let id = obj
        .get("id")
        .or_else(|| obj.get("activity_id"))
        .or_else(|| obj.get("todo_id"))
        .or_else(|| obj.get("homework_id"))
        .and_then(parse_u64)
        .unwrap_or_else(|| rand::random::<u32>() as u64);

    // 2. 標題
    let title = obj
        .get("title")
        .or_else(|| obj.get("name"))
        .or_else(|| obj.get("activity_name"))
        .or_else(|| obj.get("homework_title"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "未命名作業".to_string());

    // 3. 課程資訊
    let mut course_name = obj
        .get("course_name")
        .or_else(|| obj.get("course_title"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let mut course_id = obj.get("course_id").and_then(parse_u64);

    if let Some(c_obj) = obj.get("course").and_then(|c| c.as_object()) {
        if course_name.is_none() {
            course_name = c_obj
                .get("name")
                .or_else(|| c_obj.get("title"))
                .or_else(|| c_obj.get("course_name"))
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string());
        }
        if course_id.is_none() {
            course_id = c_obj.get("id").and_then(parse_u64);
        }
    }

    let course_name = course_name.or_else(|| fallback_course_name.map(str::to_string));
    let course_id = course_id.or(fallback_course_id);

    // 4. 系所
    let department_name = obj
        .get("department_name")
        .or_else(|| obj.get("department"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string());

    // 5. 類型 (homework, quiz, discussion, etc.)
    let item_type = obj
        .get("type")
        .or_else(|| obj.get("activity_type"))
        .or_else(|| obj.get("item_type"))
        .or_else(|| obj.get("category"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_else(|| "homework".to_string());

    // 6. 截止時間與開放時間
    let end_time = obj
        .get("end_time")
        .or_else(|| obj.get("deadline"))
        .or_else(|| obj.get("end_at"))
        .or_else(|| obj.get("due_at"))
        .or_else(|| obj.get("due_date"))
        .or_else(|| obj.get("end_date"))
        .or_else(|| obj.get("close_time"))
        .or_else(|| obj.get("submit_end_time"))
        .and_then(parse_datetime);

    let start_time = obj
        .get("start_time")
        .or_else(|| obj.get("start_at"))
        .or_else(|| obj.get("open_time"))
        .or_else(|| obj.get("begin_time"))
        .or_else(|| obj.get("start_date"))
        .and_then(parse_datetime);

    // 7. 繳交狀態判定
    let mut is_submitted = obj
        .get("is_submitted")
        .or_else(|| obj.get("submitted"))
        .or_else(|| obj.get("is_finished"))
        .or_else(|| obj.get("finished"))
        .and_then(parse_bool)
        .unwrap_or(false);

    let mut submission_status = obj
        .get("submission_status")
        .or_else(|| obj.get("status"))
        .or_else(|| obj.get("state"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    if let Some(sub_obj) = obj.get("submission").and_then(|s| s.as_object()) {
        if let Some(st) = sub_obj.get("status").and_then(|v| v.as_str()) {
            submission_status = st.trim().to_string();
        }
        if let Some(sub_b) = sub_obj.get("is_submitted").and_then(parse_bool) {
            is_submitted = is_submitted || sub_b;
        }
    }

    let status_lower = submission_status.to_ascii_lowercase();
    if matches!(
        status_lower.as_str(),
        "submitted" | "graded" | "group_submitted" | "completed" | "pass" | "passed" | "done" | "finished"
    ) {
        is_submitted = true;
    }

    if submission_status.is_empty() {
        submission_status = if is_submitted {
            "submitted".to_string()
        } else {
            "not_submitted".to_string()
        };
    }

    // 8. 遲交設定
    let allow_late_submission = obj
        .get("allow_late_submission")
        .or_else(|| obj.get("allow_late"))
        .or_else(|| obj.get("is_allow_late"))
        .and_then(parse_bool)
        .unwrap_or(false);

    // 9. 成績分數
    let score = obj
        .get("score")
        .or_else(|| obj.get("grade"))
        .or_else(|| obj.get("submission").and_then(|s| s.get("score")))
        .and_then(parse_f64);

    Some(TronclassTodoItem {
        id,
        title,
        course_id,
        course_name,
        department_name,
        item_type,
        start_time,
        end_time,
        is_submitted,
        submission_status,
        allow_late_submission,
        score,
    })
}

/// 從 `/api/my-courses` 解析出的單一課程。
#[derive(Debug, Clone)]
pub struct CourseBrief {
    pub id: u64,
    pub name: String,
    /// 課程是否仍在進行中。無法判斷時為 `None`（視為保留）。
    pub is_current: Option<bool>,
}

/// 從 `/api/my-courses` 回應中解析課程清單 `(course_id, course_name)`
///
/// 保留給既有呼叫端與測試使用；新程式碼請改用 [`parse_course_briefs`]。
pub fn parse_course_list(root: &serde_json::Value) -> Vec<(u64, String)> {
    parse_course_briefs(root)
        .into_iter()
        .map(|course| (course.id, course.name))
        .collect()
}

/// 解析課程清單，並盡可能判斷每門課是否屬於「當前學期」。
///
/// Tronclass 各校欄位差異極大，因此採多層推斷，任何一層命中即採用；
/// 全部無法判斷時回傳 `None`，呼叫端會保守地保留該課程。
pub fn parse_course_briefs(root: &serde_json::Value) -> Vec<CourseBrief> {
    let mut courses = Vec::new();

    let arr = if let Some(a) = root.as_array() {
        Some(a)
    } else if let Some(obj) = root.as_object() {
        obj.get("courses")
            .or_else(|| obj.get("data"))
            .or_else(|| obj.get("items"))
            .and_then(|v| v.as_array())
    } else {
        None
    };

    if let Some(arr) = arr {
        for v in arr {
            if let Some(obj) = v.as_object() {
                if let Some(id) = obj.get("id").and_then(parse_u64) {
                    let name = obj
                        .get("name")
                        .or_else(|| obj.get("course_name"))
                        .or_else(|| obj.get("title"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("未命名課程")
                        .trim()
                        .to_string();
                    courses.push(CourseBrief {
                        id,
                        name,
                        is_current: detect_course_is_current(obj),
                    });
                }
            }
        }
    }
    courses
}

/// 判斷單一課程物件是否為當前學期課程。
///
/// 回傳 `None` 表示無從判斷 — 呼叫端應保留該課程，寧可多顯示也不要漏掉作業。
fn detect_course_is_current(obj: &serde_json::Map<String, serde_json::Value>) -> Option<bool> {
    let now = Utc::now();

    // 第 1 層：明確的封存 / 結束旗標。
    for key in ["is_archived", "archived", "is_ended", "ended", "is_closed"] {
        if let Some(true) = obj.get(key).and_then(parse_bool) {
            return Some(false);
        }
    }
    for key in ["is_current", "current", "is_active", "active", "in_progress"] {
        if let Some(flag) = obj.get(key).and_then(parse_bool) {
            return Some(flag);
        }
    }

    // 第 2 層：課程狀態字串。
    if let Some(status) = obj
        .get("status")
        .or_else(|| obj.get("course_status"))
        .or_else(|| obj.get("state"))
        .and_then(|v| v.as_str())
    {
        match status.trim().to_ascii_lowercase().as_str() {
            "archived" | "ended" | "finished" | "closed" | "completed" | "expired" => {
                return Some(false)
            }
            "ongoing" | "active" | "in_progress" | "published" | "current" => return Some(true),
            _ => {}
        }
    }

    // 第 3 層：課程結束日期已過。
    for key in ["end_date", "end_time", "course_end_date", "end_at", "close_date"] {
        if let Some(end) = obj.get(key).and_then(parse_datetime) {
            return Some(end > now);
        }
    }

    // 第 4 層：course_code 前綴的學期碼。
    // 實測 TKU 格式為 "1151TEIDB2A0050 1A"，前 4 碼 = 民國學年(3) + 學期(1)。
    // 這是目前最可靠的來源：academic_year 可能是巢狀物件、semester 常為 null。
    if let Some(code) = obj.get("course_code").and_then(|v| v.as_str()) {
        let digits: String = code
            .trim()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if digits.len() >= 4 {
            if let Some(flag) = semester_text_is_current(&digits[..4], now) {
                return Some(flag);
            }
        }
    }

    // 第 5 層：學年／學期欄位。這些在不同租戶可能是字串、數字，
    // 也可能是 {"name": "115"} 這種巢狀物件（TKU 實測即為後者）。
    for key in [
        "academic_year",
        "semester",
        "term",
        "school_year",
        "academic_term",
        "semester_name",
    ] {
        if let Some(text) = obj.get(key).and_then(extract_scalar_text) {
            if let Some(flag) = semester_text_is_current(&text, now) {
                return Some(flag);
            }
        }
    }

    // 第 6 層：課程起始日仍在未來 → 尚未開課，不算當前。
    if let Some(start) = obj.get("start_date").or_else(|| obj.get("start_time")) {
        if let Some(start) = parse_datetime(start) {
            if start > now {
                return Some(false);
            }
        }
    }

    None
}

/// 推算「當前學期起點」（台北時間），作為過濾舊作業的下界。
///
/// 台灣學制：上學期約 8 月開始、下學期約 2 月開始。
/// 回傳值會再往前寬限 [`SEMESTER_START_GRACE_DAYS`] 天。
fn current_semester_start(now: DateTime<Utc>) -> DateTime<Utc> {
    let taipei = match chrono::FixedOffset::east_opt(8 * 3600) {
        Some(tz) => tz,
        None => return now - chrono::Duration::days(180),
    };
    let local = now.with_timezone(&taipei);
    let (year, month) = (local.year(), local.month());

    // 8 月～隔年 1 月 → 上學期（起點為當年 8 月，1 月時為前一年 8 月）
    // 2 月～7 月       → 下學期（起點為當年 2 月）
    let (start_year, start_month) = if month >= 8 {
        (year, 8)
    } else if month <= 1 {
        (year - 1, 8)
    } else {
        (year, 2)
    };

    chrono::NaiveDate::from_ymd_opt(start_year, start_month, 1)
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .and_then(|naive| taipei.from_local_datetime(&naive).single())
        .map(|dt| dt.with_timezone(&Utc) - chrono::Duration::days(SEMESTER_START_GRACE_DAYS))
        .unwrap_or_else(|| now - chrono::Duration::days(180))
}

/// 從可能是字串／數字／巢狀物件的欄位中取出可判讀的文字。
///
/// TKU 的 `academic_year` 形如 `{"code":null,"id":26,"name":"115","sort":0}`，
/// 因此需要往下取 `name` / `real_name` / `code`。
fn extract_scalar_text(val: &serde_json::Value) -> Option<String> {
    match val {
        serde_json::Value::String(s) => {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        }
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Object(obj) => {
            for key in ["name", "real_name", "code", "value", "title"] {
                if let Some(text) = obj.get(key).and_then(extract_scalar_text) {
                    return Some(text);
                }
            }
            None
        }
        _ => None,
    }
}

/// 比對學期字串（如 `"114-1"`、`"2025 Fall"`、`"1141"`）是否為當前學期。
///
/// 同時支援民國年（台灣學制）與西元年；無法判讀時回傳 `None`。
fn semester_text_is_current(text: &str, now: DateTime<Utc>) -> Option<bool> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    // 以台北時間為準推算當前學年度：8 月起算新學年，上學期為 1、下學期為 2。
    let taipei = chrono::FixedOffset::east_opt(8 * 3600)?;
    let local = now.with_timezone(&taipei);
    let year = local.year();
    let month = local.month();

    let (western_year, term) = if month >= 8 {
        (year, 1)
    } else if month <= 1 {
        (year - 1, 1)
    } else {
        (year - 1, 2)
    };
    let roc_year = western_year - 1911;

    let digits: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }

    // 形如 "1141" / "114-1" / "114學年度第1學期"
    if let Some(found_year) = digits.get(..3).and_then(|s| s.parse::<i32>().ok()) {
        if (100..200).contains(&found_year) {
            let found_term = digits.get(3..4).and_then(|s| s.parse::<i32>().ok());
            return Some(found_year == roc_year && found_term.map_or(true, |t| t == term));
        }
    }

    // 形如 "2025-1" / "2025 Fall"
    if let Some(found_year) = digits.get(..4).and_then(|s| s.parse::<i32>().ok()) {
        if (2000..2100).contains(&found_year) {
            let lower = text.to_ascii_lowercase();
            let found_term = if lower.contains("fall") || lower.contains("上") {
                Some(1)
            } else if lower.contains("spring") || lower.contains("下") {
                Some(2)
            } else {
                digits.get(4..5).and_then(|s| s.parse::<i32>().ok())
            };
            return Some(found_year == western_year && found_term.map_or(true, |t| t == term));
        }
    }

    None
}

// ─── 基礎型別彈性轉換器 ───────────────────────────────────────────────────────

fn parse_u64(val: &serde_json::Value) -> Option<u64> {
    match val {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

fn parse_f64(val: &serde_json::Value) -> Option<f64> {
    match val {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn parse_bool(val: &serde_json::Value) -> Option<bool> {
    match val {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::Number(n) => Some(n.as_i64()? != 0),
        serde_json::Value::String(s) => {
            let s = s.trim().to_ascii_lowercase();
            match s.as_str() {
                "true" | "1" | "yes" | "t" | "submitted" | "done" => Some(true),
                "false" | "0" | "no" | "f" => Some(false),
                _ => None,
            }
        }
        _ => None,
    }
}

/// 支援 ISO 8601、本地時間字串 (Asia/Taipei UTC+8) 及 Unix Timestamp
pub fn parse_datetime(val: &serde_json::Value) -> Option<DateTime<Utc>> {
    match val {
        serde_json::Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }

            // 1. 標準 RFC 3339 / ISO 8601 UTC
            if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                return Some(dt.with_timezone(&Utc));
            }

            // 2. 本地時間格式 "YYYY-MM-DD HH:MM:SS" (預設 Asia/Taipei +08:00)
            if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
                let offset = chrono::FixedOffset::east_opt(8 * 3600)?;
                return offset
                    .from_local_datetime(&naive)
                    .single()
                    .map(|dt| dt.with_timezone(&Utc));
            }

            // 3. 本地時間格式 "YYYY-MM-DD HH:MM"
            if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M") {
                let offset = chrono::FixedOffset::east_opt(8 * 3600)?;
                return offset
                    .from_local_datetime(&naive)
                    .single()
                    .map(|dt| dt.with_timezone(&Utc));
            }

            // 4. "YYYY-MM-DDTHH:MM:SS" (無時區後綴)
            if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
                let offset = chrono::FixedOffset::east_opt(8 * 3600)?;
                return offset
                    .from_local_datetime(&naive)
                    .single()
                    .map(|dt| dt.with_timezone(&Utc));
            }

            // 5. 純日期格式 "YYYY-MM-DD"（Tronclass 的 start_date / end_date 用這種）
            //    視為當日 23:59:59（+08:00），符合「截止日當天仍可繳交」的語意。
            if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                let offset = chrono::FixedOffset::east_opt(8 * 3600)?;
                return date
                    .and_hms_opt(23, 59, 59)
                    .and_then(|naive| offset.from_local_datetime(&naive).single())
                    .map(|dt| dt.with_timezone(&Utc));
            }

            // 6. 純數字時間戳記字串 ("1788100000" / "1788100000000")
            if let Ok(i) = s.parse::<i64>() {
                return if i > 10_000_000_000 {
                    DateTime::from_timestamp_millis(i)
                } else {
                    DateTime::from_timestamp(i, 0)
                };
            }

            None
        }
        serde_json::Value::Number(num) => {
            if let Some(i) = num.as_i64() {
                if i > 10_000_000_000 {
                    DateTime::from_timestamp_millis(i)
                } else {
                    DateTime::from_timestamp(i, 0)
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

// ─── 單元測試 ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_real_empty_todo_list() {
        let raw = r#"{"todo_list": []}"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let items = parse_todos_json(&val, None, None);
        assert_eq!(items.len(), 0);
    }

    #[test]
    fn test_parse_real_empty_homework_activities() {
        let raw = r#"{
            "end": 0,
            "homework_activities": [],
            "page": 0,
            "page_size": 10,
            "pages": 0,
            "start": 0,
            "total": 0
        }"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let items = parse_todos_json(&val, Some("計算機組織"), Some(263858));
        assert_eq!(items.len(), 0);
    }

    // ── 當前學期過濾 ─────────────────────────────────────────────────────────

    #[test]
    fn test_archived_course_is_not_current() {
        let raw = r#"{"courses":[
            {"id": 1, "name": "上學期課", "is_archived": true},
            {"id": 2, "name": "本學期課", "is_archived": false}
        ]}"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let courses = parse_course_briefs(&val);
        assert_eq!(courses[0].is_current, Some(false));
        assert_eq!(courses[1].is_current, Some(true));
    }

    #[test]
    fn test_course_status_string_detection() {
        let raw = r#"{"courses":[
            {"id": 1, "name": "已結束", "status": "archived"},
            {"id": 2, "name": "進行中", "status": "ongoing"},
            {"id": 3, "name": "未知狀態", "status": "whatever"}
        ]}"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let courses = parse_course_briefs(&val);
        assert_eq!(courses[0].is_current, Some(false));
        assert_eq!(courses[1].is_current, Some(true));
        // 無法判斷時必須回 None，讓呼叫端保留該課程
        assert_eq!(courses[2].is_current, None);
    }

    #[test]
    fn test_course_end_date_in_past_is_not_current() {
        let raw = r#"{"courses":[
            {"id": 1, "name": "去年結束", "end_date": "2020-06-30T00:00:00Z"}
        ]}"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let courses = parse_course_briefs(&val);
        assert_eq!(courses[0].is_current, Some(false));
    }

    #[test]
    fn test_course_without_any_hint_is_kept() {
        let raw = r#"{"courses":[{"id": 1, "name": "沒有任何學期資訊"}]}"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let courses = parse_course_briefs(&val);
        // 無從判斷 → None → 呼叫端保留
        assert_eq!(courses[0].is_current, None);
        assert_ne!(courses[0].is_current, Some(false));
    }

    /// 以 TKU `/api/my-courses` 的真實回應驗證整條判斷鏈。
    #[test]
    fn test_real_tku_course_payload_is_current() {
        let raw = r#"{"courses":[{
            "academic_year": {"code": null, "id": 26, "name": "115", "sort": 0},
            "course_code": "1151TEIDB2E0334 0A   ",
            "end_date": "2027-01-31",
            "start_date": "2026-08-01",
            "id": 263858,
            "name": "計算機組織",
            "semester": {"code": null, "id": 136, "name": null, "real_name": null, "sort": 0}
        }]}"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let courses = parse_course_briefs(&val);
        assert_eq!(courses.len(), 1);
        assert_eq!(courses[0].id, 263858);
        assert_eq!(courses[0].name, "計算機組織");
        // 結束日 2027-01-31 仍在未來 → 當前學期課程，必須保留
        assert_eq!(courses[0].is_current, Some(true));
    }

    /// 舊學期課程：結束日已過，必須被濾掉。
    #[test]
    fn test_real_tku_past_semester_course_is_filtered() {
        let raw = r#"{"courses":[{
            "academic_year": {"code": null, "id": 24, "name": "113", "sort": 0},
            "course_code": "1132TEIDB2E0334 0A   ",
            "end_date": "2025-01-31",
            "start_date": "2024-08-01",
            "id": 111111,
            "name": "上上學期的課"
        }]}"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let courses = parse_course_briefs(&val);
        assert_eq!(courses[0].is_current, Some(false));
    }

    /// `end_date` 缺席時，應退回 course_code 前綴判斷。
    #[test]
    fn test_course_code_prefix_fallback() {
        let raw = r#"{"courses":[
            {"id": 1, "name": "本學期", "course_code": "1151ABC0001 0A"},
            {"id": 2, "name": "舊學期", "course_code": "1132ABC0001 0A"}
        ]}"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let courses = parse_course_briefs(&val);
        assert_eq!(courses[0].is_current, Some(true));
        assert_eq!(courses[1].is_current, Some(false));
    }

    /// 巢狀 `academic_year` 物件必須能取出 name 來判斷。
    #[test]
    fn test_nested_academic_year_object() {
        assert_eq!(
            extract_scalar_text(&serde_json::json!({"code": null, "name": "115", "sort": 0})),
            Some("115".to_string())
        );
        // 全 null 的 semester 物件應回 None
        assert_eq!(
            extract_scalar_text(&serde_json::json!({"code": null, "name": null})),
            None
        );
    }

    /// Tronclass 的 `YYYY-MM-DD` 純日期格式必須解析成當日 23:59:59 (+08:00)。
    #[test]
    fn test_parse_date_only_format() {
        let parsed = parse_datetime(&serde_json::json!("2027-01-31"))
            .expect("date-only 格式必須可解析");
        let taipei = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let local = parsed.with_timezone(&taipei);
        assert_eq!(local.format("%Y-%m-%d %H:%M:%S").to_string(), "2027-01-31 23:59:59");
    }

    /// 學期起點門檻必須在整個學期內保持不變，
    /// 不能像滾動天數那樣在學期中後期誤刪本學期稍早的作業。
    #[test]
    fn test_semester_start_is_stable_within_semester() {
        let aug = DateTime::parse_from_rfc3339("2026-08-27T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let dec = DateTime::parse_from_rfc3339("2026-12-15T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // 8 月與 12 月同屬上學期 → 門檻必須相同
        assert_eq!(current_semester_start(aug), current_semester_start(dec));

        // 12 月時，9 月截止的本學期作業必須仍在門檻之後（不被濾掉）
        let sept_homework = DateTime::parse_from_rfc3339("2026-09-15T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(sept_homework >= current_semester_start(dec));

        // 上學期（6 月）的作業則必須被濾掉
        let june_homework = DateTime::parse_from_rfc3339("2026-06-15T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(june_homework < current_semester_start(dec));
    }

    #[test]
    fn test_roc_semester_text_matching() {
        // 2026-08-27 → 西元 2026 上學期 → 民國 115 學年度第 1 學期
        let now = DateTime::parse_from_rfc3339("2026-08-27T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(semester_text_is_current("115-1", now), Some(true));
        assert_eq!(semester_text_is_current("114-2", now), Some(false));
        assert_eq!(semester_text_is_current("113-1", now), Some(false));
    }

    #[test]
    fn test_parse_course_list_backward_compatible() {
        let raw = r#"{"courses":[{"id": 263858, "name": "計算機組織"}]}"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let courses = parse_course_list(&val);
        assert_eq!(courses.len(), 1);
        assert_eq!(courses[0], (263858, "計算機組織".to_string()));
    }

    #[test]
    fn test_parse_populated_homework() {
        let raw = r#"{
            "homework_activities": [
                {
                    "id": 105234,
                    "title": "第一次作業：組合語言設計",
                    "course_id": 263858,
                    "deadline": 1788100000,
                    "submitted": false,
                    "allow_late": true,
                    "score": null
                }
            ]
        }"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let items = parse_todos_json(&val, Some("計算機組織"), Some(263858));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, 105234);
        assert_eq!(items[0].title, "第一次作業：組合語言設計");
        assert_eq!(items[0].course_name.as_deref(), Some("計算機組織"));
        assert!(!items[0].is_completed_or_submitted());
        assert!(items[0].allow_late_submission);
    }
}
