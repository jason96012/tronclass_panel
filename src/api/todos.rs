//! Tronclass 待辦事項與作業 API 模組
//!
//! 提供 `/api/todos` 呼叫與作業截止時間、繳交狀態解析。

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use miette::{IntoDiagnostic, Result, WrapErr};
use serde::{Deserialize, Deserializer, Serialize};
use tracing::{debug, instrument};

// ─── 資料模型 ─────────────────────────────────────────────────────────────────

/// Tronclass 待辦事項/作業結構
#[derive(Debug, Clone, Serialize)]
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
            "submitted" | "graded" | "group_submitted" | "completed" | "pass" | "passed"
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

#[derive(Debug, Deserialize)]
struct RawTodoItem {
    #[serde(default, deserialize_with = "deserialize_flexible_id")]
    id: u64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_flexible_optional_id")]
    course_id: Option<u64>,
    #[serde(default)]
    course_name: Option<String>,
    #[serde(default)]
    course_title: Option<String>,
    #[serde(default)]
    department_name: Option<String>,
    #[serde(default, rename = "type")]
    item_type: Option<String>,
    #[serde(default)]
    start_time: Option<serde_json::Value>,
    #[serde(default)]
    end_time: Option<serde_json::Value>,
    #[serde(default)]
    deadline: Option<serde_json::Value>,
    #[serde(default)]
    is_submitted: Option<bool>,
    #[serde(default)]
    submitted: Option<bool>,
    #[serde(default)]
    submission_status: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    allow_late_submission: Option<bool>,
    #[serde(default)]
    score: Option<f64>,
}

impl From<RawTodoItem> for TronclassTodoItem {
    fn from(raw: RawTodoItem) -> Self {
        let title = raw
            .title
            .or(raw.name)
            .unwrap_or_else(|| "未命名作業".to_string());
        let course_name = raw.course_name.or(raw.course_title);
        let item_type = raw.item_type.unwrap_or_else(|| "homework".to_string());
        let start_time = raw.start_time.as_ref().and_then(parse_flexible_datetime);
        let end_time = raw
            .end_time
            .as_ref()
            .or(raw.deadline.as_ref())
            .and_then(parse_flexible_datetime);
        let is_submitted = raw.is_submitted.or(raw.submitted).unwrap_or(false);
        let submission_status = raw
            .submission_status
            .or(raw.status)
            .unwrap_or_else(|| "not_submitted".to_string());
        let allow_late_submission = raw.allow_late_submission.unwrap_or(false);

        Self {
            id: raw.id,
            title,
            course_id: raw.course_id,
            course_name,
            department_name: raw.department_name,
            item_type,
            start_time,
            end_time,
            is_submitted,
            submission_status,
            allow_late_submission,
            score: raw.score,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawTodosResponse {
    List(Vec<RawTodoItem>),
    ObjectWrapper {
        #[serde(default)]
        todos: Option<Vec<RawTodoItem>>,
        #[serde(default)]
        data: Option<Vec<RawTodoItem>>,
        #[serde(default)]
        activities: Option<Vec<RawTodoItem>>,
        #[serde(default)]
        items: Option<Vec<RawTodoItem>>,
        #[serde(default)]
        results: Option<Vec<RawTodoItem>>,
        #[serde(default)]
        todo_list: Option<Vec<RawTodoItem>>,
        #[serde(default)]
        assignments: Option<Vec<RawTodoItem>>,
        #[serde(default)]
        homeworks: Option<Vec<RawTodoItem>>,
    },
}

// ─── API 客戶端擴充 ───────────────────────────────────────────────────────────

impl super::ApiClient {
    /// `GET /api/todos` — 取得使用者待辦事項（含作業、測驗截止時間）
    #[instrument(skip(self))]
    pub async fn get_todos(&self) -> Result<Vec<TronclassTodoItem>> {
        let endpoints = [
            "/api/todos",
            "/api/student/todos",
            "/api/activities/unfinished",
        ];

        let mut last_error = None;

        for endpoint in &endpoints {
            let url = format!("{}{}", self.base_url, endpoint);
            debug!(url = %url, "嘗試獲取待辦作業");

            let resp = match self.client.get(&url).send().await {
                Ok(resp) => resp,
                Err(e) => {
                    last_error = Some(miette::miette!("請求 {url} 失敗: {e}"));
                    continue;
                }
            };

            let status = resp.status();
            if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::BAD_REQUEST {
                debug!(endpoint = %endpoint, status = %status, "端點不存在或請求無效，嘗試備用端點");
                continue;
            }

            match self.handle_response::<RawTodosResponse>(resp).await {
                Ok(parsed) => {
                    let raw_list = match parsed {
                        RawTodosResponse::List(items) => items,
                        RawTodosResponse::ObjectWrapper {
                            todos,
                            data,
                            activities,
                            items,
                            results,
                            todo_list,
                            assignments,
                            homeworks,
                        } => todos
                            .or(data)
                            .or(activities)
                            .or(items)
                            .or(results)
                            .or(todo_list)
                            .or(assignments)
                            .or(homeworks)
                            .unwrap_or_default(),
                    };
                    return Ok(raw_list.into_iter().map(Into::into).collect());
                }
                Err(e) => {
                    if crate::api::is_auth_error(&e.to_string()) {
                        return Err(e);
                    }
                    last_error = Some(e);
                }
            }
        }

        if let Some(e) = last_error {
            Err(e).wrap_err("獲取 Tronclass 待辦事項失敗")
        } else {
            Ok(Vec::new())
        }
    }
}

// ─── 輔助解析函式 ─────────────────────────────────────────────────────────────

fn deserialize_flexible_id<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(n) => n.as_u64().ok_or_else(|| {
            serde::de::Error::custom(format!("invalid u64 integer: {n}"))
        }),
        serde_json::Value::String(s) => s
            .trim()
            .parse::<u64>()
            .map_err(|e| serde::de::Error::custom(format!("invalid string id: {e}"))),
        _ => Ok(0),
    }
}

fn deserialize_flexible_optional_id<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(value) = value else {
        return Ok(None);
    };
    match value {
        serde_json::Value::Number(n) => Ok(n.as_u64()),
        serde_json::Value::String(s) => Ok(s.trim().parse::<u64>().ok()),
        _ => Ok(None),
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
                // 假設學校本地時區為 +08:00
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
                // 毫秒判斷 (大於 100 億通常為毫秒)
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
        // 23:00 +08:00 等同於 15:00 UTC
        assert_eq!(parsed.to_rfc3339(), "2026-08-30T15:00:00+00:00");
    }

    #[test]
    fn test_deserialize_raw_todos_json_list() {
        let json_str = r#"[
            {
                "id": 1001,
                "title": "作業一：機器學習基礎",
                "course_id": 501,
                "course_name": "人工智慧概論",
                "type": "homework",
                "end_time": "2026-08-30T23:59:59Z",
                "is_submitted": false,
                "submission_status": "not_submitted"
            },
            {
                "id": 1002,
                "title": "單元測驗二",
                "course_id": 502,
                "course_name": "網路通訊",
                "type": "quiz",
                "end_time": "2026-08-28 12:00:00",
                "is_submitted": true,
                "submission_status": "submitted"
            }
        ]"#;

        let parsed: RawTodosResponse = serde_json::from_str(json_str).unwrap();
        let items: Vec<TronclassTodoItem> = match parsed {
            RawTodosResponse::List(list) => list.into_iter().map(Into::into).collect(),
            _ => panic!("Expected List"),
        };

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, 1001);
        assert_eq!(items[0].title, "作業一：機器學習基礎");
        assert_eq!(items[0].course_name.as_deref(), Some("人工智慧概論"));
        assert!(!items[0].is_completed_or_submitted());

        assert_eq!(items[1].id, 1002);
        assert!(items[1].is_completed_or_submitted());
    }

    #[test]
    fn test_deserialize_raw_todos_json_object() {
        let json_str = r#"{
            "todos": [
                {
                    "id": "2001",
                    "name": "期末專案報告",
                    "course_title": "軟體工程",
                    "type": "homework",
                    "deadline": "2026-09-01T15:00:00Z",
                    "submitted": false,
                    "status": "not_submitted"
                }
            ]
        }"#;

        let parsed: RawTodosResponse = serde_json::from_str(json_str).unwrap();
        let items: Vec<TronclassTodoItem> = match parsed {
            RawTodosResponse::ObjectWrapper { todos, .. } => {
                todos.unwrap().into_iter().map(Into::into).collect()
            }
            _ => panic!("Expected ObjectWrapper"),
        };

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, 2001);
        assert_eq!(items[0].title, "期末專案報告");
        assert_eq!(items[0].course_name.as_deref(), Some("軟體工程"));
        assert!(!items[0].is_completed_or_submitted());
    }
}
