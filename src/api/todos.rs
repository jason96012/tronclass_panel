//! Tronclass 作業與待辦事項 API 模組（完全重寫版）
//!
//! 專為 Tronclass（含淡江 iClass、輔仁、東海 iLearn 等各校實體架構）設計：
//! 1. 優先請求全域待辦 `/api/todos`（解析 `todo_list`）
//! 2. 請求學生修課清單 `/api/my-courses`（解析 `courses`）
//! 3. 併發讀取各課程作業 `/api/courses/{id}/homework-activities` 與活動 `/api/courses/{id}/activities`
//! 4. 具備高容錯多型資料解析（時間戳、繳交狀態、計分、遲交設定）

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use miette::Result;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tracing::{debug, instrument, warn};

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
                        let courses = parse_course_list(&json_val);
                        for (course_id, course_name) in courses.into_iter().take(25) {
                            // 2-1: 查詢課程作業 GET /api/courses/{id}/homework-activities
                            let hw_url = format!("{}/api/courses/{}/homework-activities", self.base_url, course_id);
                            if let Ok(c_resp) = self.client.get(&hw_url).send().await {
                                if c_resp.status().is_success() {
                                    if let Ok(c_bytes) = c_resp.bytes().await {
                                        if let Ok(c_json) = serde_json::from_slice::<serde_json::Value>(&c_bytes) {
                                            let items = parse_todos_json(&c_json, Some(course_name.as_str()), Some(course_id));
                                            for item in items {
                                                if seen_ids.insert(item.id) {
                                                    results.push(item);
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            // 2-2: 查詢課程活動 GET /api/courses/{id}/activities
                            let act_url = format!("{}/api/courses/{}/activities", self.base_url, course_id);
                            if let Ok(c_resp) = self.client.get(&act_url).send().await {
                                if c_resp.status().is_success() {
                                    if let Ok(c_bytes) = c_resp.bytes().await {
                                        if let Ok(c_json) = serde_json::from_slice::<serde_json::Value>(&c_bytes) {
                                            let items = parse_todos_json(&c_json, Some(course_name.as_str()), Some(course_id));
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
                    }
                }
            }
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

/// 從 `/api/my-courses` 回應中解析課程清單 `(course_id, course_name)`
pub fn parse_course_list(root: &serde_json::Value) -> Vec<(u64, String)> {
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
                    courses.push((id, name));
                }
            }
        }
    }
    courses
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

            // 5. 純數字時間戳記字串 ("1788100000" / "1788100000000")
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
