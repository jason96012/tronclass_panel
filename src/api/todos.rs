//! Tronclass 待辦事項與作業 API 模組
//!
//! 提供 `/api/todos` 呼叫與作業截止時間、繳交狀態解析。

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use miette::Result;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tracing::{debug, instrument, warn};

// ─── 資料模型 ─────────────────────────────────────────────────────────────────

/// Tronclass 待辦事項/作業結構
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
    /// 判斷作業是否已被繳交或評分
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
        // 若已繳交則不需提醒
        if self.is_completed_or_submitted() {
            return false;
        }

        // 若無截止時間則略過
        let Some(end) = self.end_time else {
            return false;
        };

        // 已過期
        if end <= now {
            return false;
        }

        // 尚未開放
        if let Some(start) = self.start_time {
            if start > now {
                return false;
            }
        }

        true
    }
}

// ─── API 客戶端擴充 ───────────────────────────────────────────────────────────

impl super::ApiClient {
    /// 取得使用者待辦事項（含作業、測驗截止時間）
    ///
    /// 支援多種 Tronclass 伺服器版本策略：
    /// 1. 輪詢 `/api/todos`、`/api/student/todos`、`/api/activities/unfinished` 等全局待辦端點
    /// 2. 若無全局端點，自動透過 `/api/my-courses` 抓取各科作業清單
    /// 3. 對所有 JSON 格式進行高度容錯解析
    #[instrument(skip(self))]
    pub async fn get_todos(&self) -> Result<Vec<TronclassTodoItem>> {
        let mut all_items: Vec<TronclassTodoItem> = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        // 策略 1: 嘗試常見的全局待辦事項 API 端點
        let global_endpoints = [
            "/api/todos",
            "/api/student/todos",
            "/api/activities/unfinished",
            "/api/users/current/todos",
            "/api/users/todos",
        ];

        for endpoint in &global_endpoints {
            let url = format!("{}{}", self.base_url, endpoint);
            debug!(url = %url, "嘗試從待辦端點獲取作業清單");

            if let Ok(resp) = self.client.get(&url).send().await {
                let status = resp.status();
                if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                    return Err(miette::miette!(crate::api::ApiError::Unauthorized));
                }
                if status.is_success() {
                    if let Ok(body_bytes) = resp.bytes().await {
                        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
                            let extracted = extract_todos_from_value(&json, None, None);
                            for item in extracted {
                                if seen_ids.insert(item.id) {
                                    all_items.push(item);
                                }
                            }
                            if !all_items.is_empty() {
                                return Ok(all_items);
                            }
                        }
                    }
                }
            }
        }

        // 策略 2: 若全局待辦端點無回傳，嘗試從課程清單 `/api/my-courses` 或 `/api/courses` 取得各科作業
        let course_endpoints = ["/api/my-courses", "/api/courses", "/api/users/courses"];
        for endpoint in &course_endpoints {
            let url = format!("{}{}", self.base_url, endpoint);
            if let Ok(resp) = self.client.get(&url).send().await {
                let status = resp.status();
                if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                    return Err(miette::miette!(crate::api::ApiError::Unauthorized));
                }
                if status.is_success() {
                    if let Ok(body_bytes) = resp.bytes().await {
                        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
                            let courses = extract_course_list(&json);
                            for (c_id, c_name) in courses.iter().take(20) {
                                for sub_path in ["homeworks", "activities", "todos"] {
                                    let sub_url = format!("{}/api/courses/{}/{}", self.base_url, c_id, sub_path);
                                    if let Ok(c_resp) = self.client.get(&sub_url).send().await {
                                        if c_resp.status().is_success() {
                                            if let Ok(c_bytes) = c_resp.bytes().await {
                                                if let Ok(c_json) = serde_json::from_slice::<serde_json::Value>(&c_bytes) {
                                                    let items = extract_todos_from_value(&c_json, Some(c_name.as_str()), Some(*c_id));
                                                    for item in items {
                                                        if seen_ids.insert(item.id) {
                                                            all_items.push(item);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            if !all_items.is_empty() {
                                return Ok(all_items);
                            }
                        }
                    }
                }
            }
        }

        // 若確認已無作業，正常回傳空清單
        Ok(all_items)
    }
}

// ─── 輔助解析函式 ─────────────────────────────────────────────────────────────

/// 從任意 JSON 結構中遞迴或展開待辦事項
pub fn extract_todos_from_value(
    root: &serde_json::Value,
    fallback_course_name: Option<&str>,
    fallback_course_id: Option<u64>,
) -> Vec<TronclassTodoItem> {
    let mut items = Vec::new();

    if let Some(arr) = root.as_array() {
        for v in arr {
            if let Some(item) = extract_todo_item_from_json(v, fallback_course_name, fallback_course_id) {
                items.push(item);
            }
        }
        return items;
    }

    if let Some(obj) = root.as_object() {
        for key in ["todos", "activities", "items", "results", "todo_list", "assignments", "homeworks"] {
            if let Some(arr) = obj.get(key).and_then(|v| v.as_array()) {
                for v in arr {
                    if let Some(item) = extract_todo_item_from_json(v, fallback_course_name, fallback_course_id) {
                        items.push(item);
                    }
                }
                if !items.is_empty() {
                    return items;
                }
            }
        }

        if let Some(data) = obj.get("data") {
            if let Some(arr) = data.as_array() {
                for v in arr {
                    if let Some(item) = extract_todo_item_from_json(v, fallback_course_name, fallback_course_id) {
                        items.push(item);
                    }
                }
                if !items.is_empty() {
                    return items;
                }
            } else if let Some(data_obj) = data.as_object() {
                for key in ["todos", "activities", "items", "results", "todo_list", "assignments", "homeworks"] {
                    if let Some(arr) = data_obj.get(key).and_then(|v| v.as_array()) {
                        for v in arr {
                            if let Some(item) = extract_todo_item_from_json(v, fallback_course_name, fallback_course_id) {
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

/// 解析單一 JSON 節點為 `TronclassTodoItem`
pub fn extract_todo_item_from_json(
    v: &serde_json::Value,
    fallback_course_name: Option<&str>,
    fallback_course_id: Option<u64>,
) -> Option<TronclassTodoItem> {
    let obj = v.as_object()?;

    // 1. ID 提取
    let id = obj
        .get("id")
        .or_else(|| obj.get("activity_id"))
        .or_else(|| obj.get("todo_id"))
        .or_else(|| obj.get("homework_id"))
        .and_then(parse_flexible_u64)
        .unwrap_or_else(|| rand::random::<u32>() as u64);

    // 2. 標題提取
    let title = obj
        .get("title")
        .or_else(|| obj.get("name"))
        .or_else(|| obj.get("activity_name"))
        .or_else(|| obj.get("homework_title"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "未命名作業".to_string());

    // 3. 課程資訊提取
    let mut course_name = obj
        .get("course_name")
        .or_else(|| obj.get("course_title"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let mut course_id = obj.get("course_id").and_then(parse_flexible_u64);

    if let Some(course_obj) = obj.get("course").and_then(|c| c.as_object()) {
        if course_name.is_none() {
            course_name = course_obj
                .get("name")
                .or_else(|| course_obj.get("title"))
                .or_else(|| course_obj.get("course_name"))
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string());
        }
        if course_id.is_none() {
            course_id = course_obj.get("id").and_then(parse_flexible_u64);
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

    // 6. 時間與截止期限
    let end_time_val = obj
        .get("end_time")
        .or_else(|| obj.get("deadline"))
        .or_else(|| obj.get("end_at"))
        .or_else(|| obj.get("due_date"))
        .or_else(|| obj.get("due_at"))
        .or_else(|| obj.get("end_date"))
        .or_else(|| obj.get("close_time"))
        .or_else(|| obj.get("submit_end_time"));
    let end_time = end_time_val.and_then(parse_flexible_datetime);

    let start_time_val = obj
        .get("start_time")
        .or_else(|| obj.get("start_at"))
        .or_else(|| obj.get("open_time"))
        .or_else(|| obj.get("begin_time"))
        .or_else(|| obj.get("start_date"));
    let start_time = start_time_val.and_then(parse_flexible_datetime);

    // 7. 繳交狀態
    let mut is_submitted = obj
        .get("is_submitted")
        .or_else(|| obj.get("submitted"))
        .or_else(|| obj.get("is_finished"))
        .or_else(|| obj.get("finished"))
        .and_then(parse_flexible_bool)
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
        if let Some(sub_bool) = sub_obj.get("is_submitted").and_then(parse_flexible_bool) {
            is_submitted = is_submitted || sub_bool;
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
        .and_then(parse_flexible_bool)
        .unwrap_or(false);

    // 9. 分數
    let score = obj
        .get("score")
        .or_else(|| obj.get("grade"))
        .or_else(|| obj.get("submission").and_then(|s| s.get("score")))
        .and_then(parse_flexible_f64);

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

/// 從課程列表中解析 `(course_id, course_name)`
fn extract_course_list(root: &serde_json::Value) -> Vec<(u64, String)> {
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
                if let Some(id) = obj.get("id").and_then(parse_flexible_u64) {
                    let name = obj
                        .get("name")
                        .or_else(|| obj.get("course_name"))
                        .or_else(|| obj.get("title"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("未命名課程")
                        .to_string();
                    courses.push((id, name));
                }
            }
        }
    }
    courses
}

fn parse_flexible_u64(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

fn parse_flexible_f64(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn parse_flexible_bool(v: &serde_json::Value) -> Option<bool> {
    match v {
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

/// 支援多種 Tronclass 日期時間格式解析：
/// 1. ISO 8601 UTC ("2026-08-30T23:59:00Z", "2026-08-30T15:59:00+08:00")
/// 2. 本地字串 ("2026-08-30 23:59:00", "2026-08-30 23:59")
/// 3. 秒數或毫秒數 Timestamp
pub fn parse_flexible_datetime(value: &serde_json::Value) -> Option<DateTime<Utc>> {
    match value {
        serde_json::Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }

            // 1. 標準 RFC 3339 / ISO 8601
            if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                return Some(dt.with_timezone(&Utc));
            }

            // 2. 本地時間格式 "YYYY-MM-DD HH:MM:SS" (預設視為 Asia/Taipei +08:00)
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

            // 4. "YYYY-MM-DDTHH:MM:SS" (無時區標記)
            if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
                let offset = chrono::FixedOffset::east_opt(8 * 3600)?;
                return offset
                    .from_local_datetime(&naive)
                    .single()
                    .map(|dt| dt.with_timezone(&Utc));
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
    fn test_parse_flexible_datetime_rfc3339() {
        let val = serde_json::Value::String("2026-08-30T15:00:00Z".to_string());
        let parsed = parse_flexible_datetime(&val).unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-08-30T15:00:00+00:00");
    }

    #[test]
    fn test_parse_flexible_datetime_local_taipei() {
        let val = serde_json::Value::String("2026-08-30 23:00:00".to_string());
        let parsed = parse_flexible_datetime(&val).unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-08-30T15:00:00+00:00");
    }

    #[test]
    fn test_deserialize_raw_todos_complex_shapes() {
        let json_str = r#"{
            "activities": [
                {
                    "id": "1001",
                    "title": "作業一：機器學習基礎",
                    "course": {
                        "id": "501",
                        "name": "人工智慧概論"
                    },
                    "type": "homework",
                    "end_time": "2026-08-30T23:59:59Z",
                    "is_submitted": 0,
                    "submission_status": "not_submitted"
                },
                {
                    "activity_id": 1002,
                    "name": "單元測驗二",
                    "course_title": "網路通訊",
                    "category": "quiz",
                    "deadline": 1788100000,
                    "submission": {
                        "status": "submitted",
                        "score": 98.5
                    }
                }
            ]
        }"#;

        let json_val: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let items = extract_todos_from_value(&json_val, None, None);

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, 1001);
        assert_eq!(items[0].title, "作業一：機器學習基礎");
        assert_eq!(items[0].course_name.as_deref(), Some("人工智慧概論"));
        assert!(!items[0].is_completed_or_submitted());

        assert_eq!(items[1].id, 1002);
        assert_eq!(items[1].title, "單元測驗二");
        assert_eq!(items[1].course_name.as_deref(), Some("網路通訊"));
        assert!(items[1].is_completed_or_submitted());
        assert_eq!(items[1].score, Some(98.5));
    }
}
