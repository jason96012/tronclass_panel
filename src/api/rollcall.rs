//! 簽到相關的 Payload、Response 資料結構，以及 API 呼叫實作

use miette::{IntoDiagnostic, Result, WrapErr};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, instrument, warn};
use uuid::Uuid;

use super::ApiError;

// ─── 簽到錯誤類型 ─────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum RollcallError {
    #[error("雷達簽到距離不足：需要移動 {distance:.2} 公尺")]
    RadarDistanceTooFar { distance: f64 },

    #[error("數字簽到爆破未找到正確答案（已嘗試 0000~9999）")]
    BruteForceExhausted,

    #[error("QR code 簽到失敗：{reason}")]
    QrCodeFailed { reason: String },
}

// ─── API 回應資料結構 ──────────────────────────────────────────────────────────

/// `/api/radar/rollcalls` 回應的外層結構
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RollcallsResponse {
    /// 簽到列表
    #[serde(default)]
    pub rollcalls: Vec<Rollcall>,
}

/// 單一簽到項目（Default 僅供測試與建構便利；serde 行為不變）
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Rollcall {
    /// 簽到 ID
    pub rollcall_id: u64,

    /// 課程名稱
    #[serde(default)]
    pub course_title: String,

    /// 教師姓名
    #[serde(default)]
    pub created_by_name: String,

    /// 學系名稱
    #[serde(default)]
    pub department_name: String,

    /// 是否已過期
    #[serde(default)]
    pub is_expired: bool,

    /// 是否為數字簽到
    #[serde(default)]
    pub is_number: bool,

    /// 是否為雷達簽到
    #[serde(default)]
    pub is_radar: bool,

    /// 簽到狀態（`"absent"` / `"on_call_fine"` / 其他）
    #[serde(default)]
    pub status: String,

    /// 簽到結果狀態
    #[serde(default)]
    pub rollcall_status: String,

    /// 是否已得分
    #[serde(default)]
    pub scored: bool,

    /// TronClass 原始類型欄位；自主報到常見為 `self_registration`。
    #[serde(default, rename = "type")]
    pub rollcall_type_raw: String,

    /// 部分租戶使用 rollcall_type 表示類型。
    #[serde(default)]
    pub rollcall_type: String,

    /// 部分 payload 可能把類型或顯示名稱放在 name。
    #[serde(default)]
    pub name: String,
}

impl Rollcall {
    /// 是否需要簽到（狀態為 absent 且未過期）
    pub fn needs_attendance(&self) -> bool {
        self.status == "absent" && !self.is_expired
    }

    /// 是否已簽到成功
    pub fn is_attended(&self) -> bool {
        self.status == "on_call_fine"
    }

    /// 是否為自主報到 / self-registration。
    pub fn is_self_registration(&self) -> bool {
        let text = format!(
            "{} {} {} {}",
            self.rollcall_type_raw, self.rollcall_type, self.name, self.rollcall_status
        )
        .to_ascii_lowercase()
        .replace(['-', ' '], "_");
        text.contains("self_registration") || text.contains("自主報到")
    }

    /// 判斷簽到類型
    pub fn attendance_type(&self) -> AttendanceType {
        if self.is_number && !self.is_radar {
            AttendanceType::Number
        } else if self.is_radar {
            AttendanceType::Radar
        } else if self.is_self_registration() {
            AttendanceType::SelfRegistration
        } else {
            AttendanceType::QrCode
        }
    }

    /// 課程與教師的顯示字串
    pub fn display(&self) -> String {
        format!(
            "[{}] {} (by {})",
            self.rollcall_id, self.course_title, self.created_by_name
        )
    }
}

/// 簽到類型枚舉
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttendanceType {
    /// 數字簽到（爆破 0000~9999）
    Number,
    /// 雷達簽到（地理位置）
    Radar,
    /// QR Code 簽到
    QrCode,
    /// 自主報到
    SelfRegistration,
}

impl std::fmt::Display for AttendanceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttendanceType::Number => write!(f, "數字簽到"),
            AttendanceType::Radar => write!(f, "雷達簽到"),
            AttendanceType::QrCode => write!(f, "QR Code 簽到"),
            AttendanceType::SelfRegistration => write!(f, "自主報到"),
        }
    }
}

// ─── 請求 Body 結構 ───────────────────────────────────────────────────────────

/// 數字簽到請求 Body
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NumberRollcallBody {
    /// 裝置唯一識別碼
    pub device_id: Uuid,
    /// 4 位數字驗證碼（"0000" ~ "9999"）
    pub number_code: String,
}

impl NumberRollcallBody {
    pub fn new(number_code: impl Into<String>) -> Self {
        Self {
            device_id: Uuid::new_v4(),
            number_code: number_code.into(),
        }
    }
}

/// 雷達簽到請求 Body
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RadarRollcallBody {
    /// 位置精確度（公尺）
    pub accuracy: u32,
    /// 高度（公尺）
    pub altitude: i32,
    /// 高度精確度（通常為 null）
    pub altitude_accuracy: Option<f64>,
    /// 裝置唯一識別碼
    pub device_id: Uuid,
    /// 方向（通常為 null）
    pub heading: Option<f64>,
    /// 緯度
    pub latitude: f64,
    /// 經度
    pub longitude: f64,
    /// 速度（通常為 null）
    pub speed: Option<f64>,
}

impl RadarRollcallBody {
    pub fn new(latitude: f64, longitude: f64, accuracy: u32, altitude: i32) -> Self {
        Self {
            accuracy,
            altitude,
            altitude_accuracy: None,
            device_id: Uuid::new_v4(),
            heading: None,
            latitude,
            longitude,
            speed: None,
        }
    }
}

/// QR Code 簽到請求 Body
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QrCodeRollcallBody {
    /// QR Code 解析出的 data 字串
    pub data: String,
    /// 裝置唯一識別碼
    pub device_id: Uuid,
}

impl QrCodeRollcallBody {
    pub fn new(data: impl Into<String>) -> Self {
        Self {
            data: data.into(),
            device_id: Uuid::new_v4(),
        }
    }
}

// ─── 回應結果結構 ─────────────────────────────────────────────────────────────

/// 簽到 API 的通用回應
#[derive(Debug, Clone, Deserialize)]
pub struct AttendanceResponse {
    /// 是否成功（部分 API 有此欄位）
    #[serde(default)]
    pub success: Option<bool>,

    /// 錯誤訊息（失敗時）
    #[serde(default)]
    pub message: Option<String>,

    /// 雷達簽到失敗時返回的距離（公尺）
    #[serde(default)]
    pub distance: Option<f64>,

    /// 其他欄位
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl AttendanceResponse {
    /// 是否為雷達距離不足的錯誤
    pub fn is_radar_distance_error(&self) -> bool {
        self.distance.is_some()
    }
}

/// 簽到操作結果
#[derive(Debug, Clone)]
pub enum AttendanceResult {
    /// 簽到成功
    Success,
    /// 雷達距離不足，附帶從原點到教室的距離
    RadarTooFar { distance: f64 },
    /// 暫時性失敗（限流、逾時、伺服器錯誤等，可稍後重試）
    TransientFailure { reason: String },
    /// 其他失敗（含錯誤訊息）
    Failed { reason: String },
}

impl AttendanceResult {
    pub fn is_success(&self) -> bool {
        matches!(self, AttendanceResult::Success)
    }
}

/// 從 `/api/rollcall/{id}/student_rollcalls` 讀到的數字點名碼。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NumberCodeLookup {
    /// 4 位數字點名碼；沒有讀到時為 None。
    pub code: Option<String>,
    /// 來源 payload 內可能出現的狀態欄位。
    pub status: String,
    /// 來源 payload 內可能出現的截止時間。
    pub end_time: String,
    /// 成功讀到 code 的 JSON 路徑描述，用於 debug。
    pub source: String,
}

impl NumberCodeLookup {
    pub fn has_code(&self) -> bool {
        self.code.is_some()
    }
}

// ─── AUTO_SUBMIT_BY_CLASS_PROGRESS_V1（fix72）────────────────────────────────
// 使用者需求：「只要班上超過 15% 的人點名成功，自動送出點名」。
// 門檻 2026-09-17 由 25% 改成 15%（與 auto-rollcall-thu-tronclass 的 15% 到課率保險同一個數字）。
// 這一區只負責「把班上進度讀出來」；要不要據此自動送出、要怎麼送，留給各點名流程決定
// （數字＝自動讀碼送出、雷達＝用最新座標再試一次、QR＝只能靠掃碼所以只催人）。
//
// 讀不到、欄位長得不一樣、或只看到自己那一筆 → 一律回 `None`。
// 這是刻意的：讀不到就維持原本行為，絕不亂猜一個比例去觸發送出。

/// 班上點名進度。（分子／分母都確定時才會被建出來）
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RollcallProgress {
    /// 已點名成功的人數
    pub completed: usize,
    /// 班上人數（這份 payload 裡看得到的學生數）
    pub total: usize,
    /// 分子／分母是從哪讀到的（診斷與日誌用）
    pub source: String,
}

impl RollcallProgress {
    /// 是否「**超過**」門檻（嚴格大於：剛好 15% 不算通過，與使用者說的「超過 15%」一致）。
    pub fn passed(&self, percent: u32) -> bool {
        self.total > 0
            && self.completed.saturating_mul(100) > self.total.saturating_mul(percent as usize)
    }

    /// 顯示用的整數百分比。
    pub fn percent_rounded(&self) -> usize {
        if self.total == 0 {
            0
        } else {
            (self.completed.saturating_mul(100) + self.total / 2) / self.total
        }
    }
}

/// AUTO_SUBMIT_BY_CLASS_PROGRESS_V1（fix72）：班上**超過**這個百分比的人點名成功，
/// 就不必再等使用者輸入——直接自動送出（使用者原話：「只要班上超過 15% 的人點名成功，自動送出點名」）。
/// 放在這裡是因為它跟 [`RollcallProgress::passed`] 是同一個判斷的另一半。
/// 15%：2026-09-17 使用者指定（原本 25%），與 auto-rollcall-thu-tronclass 的到課率保險一致。
pub const AUTO_SUBMIT_PERCENT: u32 = 15;
/// 至少要有這麼多筆「帶狀態的學生」才當成班級名單。
/// 避免把「只有我自己那一筆」的 payload 誤讀成全班（1/1＝100% 會誤觸發）。
const MIN_CLASS_PROGRESS_ROSTER: usize = 4;
/// 可能代表「班上總人數」的欄位名。
const PROGRESS_TOTAL_KEYS: &[&str] = &[
    "total_count", "total_students", "student_count", "students_count", "total_num",
    "student_num", "class_size", "total",
];
/// 可能代表「已點名成功人數」的欄位名。
const PROGRESS_DONE_KEYS: &[&str] = &[
    "signed_count", "finished_count", "completed_count", "attended_count", "present_count",
    "checked_in_count", "on_call_fine_count", "signed_num", "signed_students",
];
/// 每一筆學生紀錄裡「狀態」可能長在這些欄位。
const PROGRESS_STATUS_KEYS: &[&str] = &[
    "status", "attendance_status", "attendanceStatus", "rollcall_status", "rollcallStatus",
    "state", "result", "signed", "checked_in", "checkedIn", "attended", "finished",
    "is_signed", "is_checked_in",
];
/// 學生名單可能放在這些 key 底下。
const PROGRESS_LIST_KEYS: &[&str] = &["student_rollcalls", "students", "records", "list", "items"];

/// 從 JSON 值取整數（接受數字與數字字串）。
fn progress_number(value: &serde_json::Value) -> Option<usize> {
    match value {
        serde_json::Value::Number(number) => number
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .or_else(|| number.as_f64().filter(|f| *f >= 0.0).map(|f| f as usize)),
        serde_json::Value::String(text) => text.trim().parse::<usize>().ok(),
        _ => None,
    }
}

/// 在一個物件裡找第一個命中的整數欄位。
fn progress_number_in(
    map: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<(usize, String)> {
    for key in keys {
        if let Some(value) = map.get(*key) {
            if let Some(number) = progress_number(value) {
                return Some((number, (*key).to_string()));
            }
        }
    }
    None
}

/// 把單筆學生的狀態值轉成「是否成功」。
fn progress_status_success(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(flag) => *flag,
        serde_json::Value::Number(number) => number.as_u64() == Some(1),
        serde_json::Value::String(text) => is_number_success_text(text),
        _ => false,
    }
}

/// 從 `student_rollcalls` payload 讀出班上進度。
///
/// 兩種長相都支援（先試統計欄位，再試學生名單）：
/// 1. **統計欄位**：同一層物件裡同時有「總人數」與「已成功人數」欄位。
/// 2. **學生名單**：某個 key 底下是陣列，每筆帶狀態欄位；總數＝有狀態的筆數。
pub fn parse_rollcall_progress(payload: &serde_json::Value) -> Option<RollcallProgress> {
    fn walk(value: &serde_json::Value, depth: usize) -> Option<RollcallProgress> {
        if depth > 4 {
            return None;
        }
        match value {
            serde_json::Value::Object(map) => {
                // (1) 統計欄位
                if let (Some((total, total_key)), Some((completed, done_key))) = (
                    progress_number_in(map, PROGRESS_TOTAL_KEYS),
                    progress_number_in(map, PROGRESS_DONE_KEYS),
                ) {
                    if total >= MIN_CLASS_PROGRESS_ROSTER && completed <= total {
                        return Some(RollcallProgress {
                            completed,
                            total,
                            source: format!("aggregate:{total_key}/{done_key}"),
                        });
                    }
                }
                // (2) 學生名單：先看已知的 key，再看這一層所有陣列
                for key in PROGRESS_LIST_KEYS {
                    if let Some(items) = map.get(*key).and_then(|v| v.as_array()) {
                        if let Some(progress) = from_students(items, key) {
                            return Some(progress);
                        }
                    }
                }
                for (key, child) in map {
                    if let Some(items) = child.as_array() {
                        if let Some(progress) = from_students(items, key) {
                            return Some(progress);
                        }
                    }
                }
                // (3) 往下鑽（例如 {"data": {...}}）
                for child in map.values() {
                    if let Some(progress) = walk(child, depth + 1) {
                        return Some(progress);
                    }
                }
                None
            }
            serde_json::Value::Array(items) => {
                if let Some(progress) = from_students(items, "list[].status") {
                    return Some(progress);
                }
                for child in items {
                    if let Some(progress) = walk(child, depth + 1) {
                        return Some(progress);
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// 把一個陣列當成「班級名單」數：總數＝有狀態欄位的筆數，成功＝狀態看起來成功。
    fn from_students(items: &[serde_json::Value], key: &str) -> Option<RollcallProgress> {
        let mut total = 0usize;
        let mut completed = 0usize;
        for item in items {
            let Some(map) = item.as_object() else { continue };
            let mut status: Option<&serde_json::Value> = None;
            for status_key in PROGRESS_STATUS_KEYS {
                if let Some(value) = map.get(*status_key) {
                    status = Some(value);
                    break;
                }
            }
            let Some(status) = status else { continue };
            total += 1;
            if progress_status_success(status) {
                completed += 1;
            }
        }
        if total >= MIN_CLASS_PROGRESS_ROSTER && completed <= total {
            Some(RollcallProgress {
                completed,
                total,
                source: format!("list:{key}"),
            })
        } else {
            None
        }
    }

    walk(payload, 0)
}

fn coerce_number_code(value: Option<&serde_json::Value>) -> Option<String> {
    let value = value?;
    match value {
        serde_json::Value::Number(number) => number
            .as_u64()
            .filter(|n| *n <= 9999)
            .map(|n| format!("{n:04}")),
        serde_json::Value::String(text) => {
            let text = text.trim();
            if text.len() == 4 && text.chars().all(|c| c.is_ascii_digit()) {
                Some(text.to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn parse_number_code_payload(payload: &serde_json::Value) -> NumberCodeLookup {
    fn absorb_meta(value: &serde_json::Value, status: &mut String, end_time: &mut String) {
        let Some(map) = value.as_object() else {
            return;
        };
        if status.is_empty() {
            if let Some(value) = map.get("status").and_then(|v| v.as_str()) {
                *status = value.to_string();
            }
        }
        if end_time.is_empty() {
            if let Some(value) = map.get("end_time").and_then(|v| v.as_str()) {
                *end_time = value.to_string();
            }
        }
    }

    fn result(
        code: Option<String>,
        source: &str,
        status: &str,
        end_time: &str,
    ) -> NumberCodeLookup {
        NumberCodeLookup {
            code,
            status: status.to_string(),
            end_time: end_time.to_string(),
            source: source.to_string(),
        }
    }

    let mut status = String::new();
    let mut end_time = String::new();

    if let Some(map) = payload.as_object() {
        absorb_meta(payload, &mut status, &mut end_time);

        if let Some(code) = coerce_number_code(map.get("number_code")) {
            return result(Some(code), "number_code", &status, &end_time);
        }

        if let Some(data) = map.get("data") {
            absorb_meta(data, &mut status, &mut end_time);
            if let Some(code) = data
                .as_object()
                .and_then(|data_map| coerce_number_code(data_map.get("number_code")))
            {
                return result(Some(code), "data.number_code", &status, &end_time);
            }
        }

        for container_key in ["student_rollcalls", "data"] {
            if let Some(items) = map.get(container_key).and_then(|v| v.as_array()) {
                for item in items {
                    absorb_meta(item, &mut status, &mut end_time);
                    if let Some(code) = item
                        .as_object()
                        .and_then(|item_map| coerce_number_code(item_map.get("number_code")))
                    {
                        return result(
                            Some(code),
                            &format!("{container_key}[].number_code"),
                            &status,
                            &end_time,
                        );
                    }
                }
            }
        }
    } else if let Some(items) = payload.as_array() {
        for item in items {
            absorb_meta(item, &mut status, &mut end_time);
            if let Some(code) = item
                .as_object()
                .and_then(|item_map| coerce_number_code(item_map.get("number_code")))
            {
                return result(Some(code), "list[].number_code", &status, &end_time);
            }
        }
    }

    result(None, "", &status, &end_time)
}

const NUMBER_RESULT_BOOL_KEYS: &[&str] = &["success", "succeeded", "ok", "result"];
const NUMBER_RESULT_TEXT_KEYS: &[&str] = &[
    "message",
    "msg",
    "error",
    "errors",
    "reason",
    "detail",
    "details",
    "code",
    "status",
    "state",
    "result",
    "attendance_status",
    "attendanceStatus",
    "rollcall_status",
    "rollcallStatus",
];
const RESPONSE_BODY_PREVIEW_CHARS: usize = 300;

fn classify_number_rollcall_response(
    status: StatusCode,
    body_text: &str,
    number_code: &str,
) -> AttendanceResult {
    if status == StatusCode::NO_CONTENT {
        return AttendanceResult::Success;
    }

    let body = body_text.trim();
    if body.is_empty() {
        if status == StatusCode::CREATED {
            return AttendanceResult::Success;
        }

        return AttendanceResult::TransientFailure {
            reason: format!(
                "HTTP {status}: 數字簽到回應為空，無法確認 code {number_code} 是否正確"
            ),
        };
    }

    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
        if number_json_bool_marker(&json, false) {
            return AttendanceResult::Failed {
                reason: format!("code {number_code} 不正確（回應標示 success=false）"),
            };
        }

        if let Some(reason) = find_number_response_text(&json, is_number_failure_text) {
            return AttendanceResult::Failed {
                reason: format!("code {number_code} 不正確：{reason}"),
            };
        }

        if number_json_bool_marker(&json, true)
            || find_number_response_text(&json, is_number_success_text).is_some()
            || number_ack_record_marker(&json)
        {
            return AttendanceResult::Success;
        }

        return unverified_number_rollcall_response(status, body);
    }

    if is_number_failure_text(body) {
        return AttendanceResult::Failed {
            reason: format!("code {number_code} 不正確：{body}"),
        };
    }

    if is_number_success_text(body) {
        return AttendanceResult::Success;
    }

    unverified_number_rollcall_response(status, body)
}

fn unverified_number_rollcall_response(status: StatusCode, body: &str) -> AttendanceResult {
    AttendanceResult::TransientFailure {
        reason: format!(
            "HTTP {status}: 數字簽到回應無法確認成功：{}",
            response_body_preview(body)
        ),
    }
}

/// NUMBER_ID_ACK_V1（fix28）：`answer_number_rollcall` 送出正確數字時，iClass 實測
/// （2026-09-14，rollcall_id=1172118）只回 200 與新建的 student_rollcall 記錄
/// `{"id":68019054,"status":"on_call"}`——沒有 success 布林、沒有成功文字，舊邏輯
/// 落入「無法確認成功」而誤報失敗。頂層物件含「正整數（或數字字串）id」欄位、
/// 且前面的失敗特徵檢查皆未命中時，視為伺服器已立案（Success）；真偽由
/// ROLLCALL_REVERIFY_V1 二次確認把關——重查仍 absent 會降級 Failed 並重試，
/// 因此此判定偏樂觀是安全的。只認頂層，不遞迴，避免巢狀 id 誤判。
fn number_ack_record_marker(value: &serde_json::Value) -> bool {
    let serde_json::Value::Object(map) = value else {
        return false;
    };
    map.iter().any(|(key, value)| {
        key == "id"
            && (value.as_u64().is_some()
                || value
                    .as_str()
                    .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())))
    })
}

fn number_json_bool_marker(value: &serde_json::Value, expected: bool) -> bool {
    match value {
        serde_json::Value::Object(map) => map.iter().any(|(key, value)| {
            (key_matches(key, NUMBER_RESULT_BOOL_KEYS) && value.as_bool() == Some(expected))
                || number_json_bool_marker(value, expected)
        }),
        serde_json::Value::Array(items) => items
            .iter()
            .any(|value| number_json_bool_marker(value, expected)),
        _ => false,
    }
}

fn find_number_response_text(
    value: &serde_json::Value,
    predicate: fn(&str) -> bool,
) -> Option<String> {
    match value {
        serde_json::Value::String(text) if predicate(text) => Some(text.clone()),
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if key_matches(key, NUMBER_RESULT_TEXT_KEYS) {
                    if let Some(text) = find_text_in_json(value, predicate) {
                        return Some(text);
                    }
                }
            }

            map.values()
                .find_map(|value| find_number_response_text(value, predicate))
        }
        serde_json::Value::Array(items) => items
            .iter()
            .find_map(|value| find_number_response_text(value, predicate)),
        _ => None,
    }
}

fn find_text_in_json(value: &serde_json::Value, predicate: fn(&str) -> bool) -> Option<String> {
    match value {
        serde_json::Value::String(text) if predicate(text) => Some(text.clone()),
        serde_json::Value::Array(items) => items
            .iter()
            .find_map(|value| find_text_in_json(value, predicate)),
        serde_json::Value::Object(map) => map
            .values()
            .find_map(|value| find_text_in_json(value, predicate)),
        _ => None,
    }
}

fn key_matches(key: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| key.eq_ignore_ascii_case(candidate))
}

fn is_number_failure_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let normalized = lower.trim();

    matches!(
        normalized,
        "false" | "fail" | "failed" | "failure" | "error" | "incorrect" | "wrong" | "invalid"
    ) || lower.contains("incorrect")
        || lower.contains("wrong")
        || lower.contains("invalid")
        || lower.contains("not match")
        || lower.contains("not_match")
        || lower.contains("number_code")
        || lower.contains("number code")
        || lower.contains("rollcall number")
        || lower.contains("failed")
        || lower.contains("failure")
        || lower.contains("expired")
        || text.contains("錯")
        || text.contains("不正確")
        || text.contains("失敗")
        || text.contains("無效")
        || text.contains("已過期")
        || text.contains("已結束")
}

fn is_number_success_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let normalized = lower.trim();

    matches!(
        normalized,
        "true"
            | "success"
            | "succeeded"
            | "ok"
            | "attended"
            | "on_call"
            | "on_call_fine"
            | "checked_in"
            | "checked-in"
    ) || lower.contains("on_call_fine")
        || text.contains("簽到成功")
        || text.contains("签到成功")
        || text.contains("點名成功")
        || text.contains("点名成功")
        || text.contains("已簽到")
        || text.contains("已签到")
}

fn response_body_preview(body: &str) -> String {
    let trimmed = body.trim();
    let mut chars = trimmed.chars();
    let preview: String = chars.by_ref().take(RESPONSE_BODY_PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

// ─── ApiClient 簽到方法 ───────────────────────────────────────────────────────

impl super::ApiClient {
    /// `GET /api/radar/rollcalls` — 輪詢獲取簽到列表
    #[instrument(skip(self))]
    pub async fn get_rollcalls(&self) -> Result<Vec<Rollcall>> {
        let url = format!("{}/api/radar/rollcalls", self.base_url);
        debug!(url = %url, "GET /api/radar/rollcalls");

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .into_diagnostic()
            .wrap_err("GET /api/radar/rollcalls failed")?;

        let rollcalls_resp: RollcallsResponse = self.handle_response(resp).await?;
        Ok(rollcalls_resp.rollcalls)
    }

    /// `GET /api/rollcall/{id}/student_rollcalls` — 嘗試直接讀取數字點名碼。
    #[instrument(skip(self), fields(rollcall_id = rollcall_id))]
    pub async fn get_number_rollcall_code(&self, rollcall_id: u64) -> Result<NumberCodeLookup> {
        let url = format!(
            "{}/api/rollcall/{}/student_rollcalls",
            self.base_url, rollcall_id
        );
        debug!(url = %url, "GET student_rollcalls for number_code");

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .into_diagnostic()
            .wrap_err_with(|| {
                format!("GET /api/rollcall/{rollcall_id}/student_rollcalls failed")
            })?;

        let status = resp.status();
        match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return Err(miette::miette!(ApiError::Unauthorized));
            }
            _ if !status.is_success() => {
                let body = resp.text().await.unwrap_or_default();
                return Err(miette::miette!(ApiError::ApiStatus {
                    status: status.as_u16(),
                    body,
                }));
            }
            _ => {}
        }

        let body_bytes = resp
            .bytes()
            .await
            .into_diagnostic()
            .wrap_err("Failed to read student_rollcalls response body")?;
        let payload = serde_json::from_slice::<serde_json::Value>(&body_bytes).map_err(|e| {
            let body_str = String::from_utf8_lossy(&body_bytes);
            miette::miette!(ApiError::Json(format!(
                "{e}: body was `{}`",
                // REDIRECT_UTF8_SLICE_V1：位元組切片切在中文字中間會 panic，改用字元截斷。
                crate::text_utils::truncate_chars(&body_str, 200)
            )))
        })?;

        let lookup = parse_number_code_payload(&payload);
        debug!(
            rollcall_id = rollcall_id,
            code = lookup.code.as_deref().unwrap_or(""),
            source = %lookup.source,
            status = %lookup.status,
            "student_rollcalls number_code lookup completed"
        );
        Ok(lookup)
    }

    /// `GET /api/rollcall/{id}/student_rollcalls` — 讀出「班上點名進度」（AUTO_SUBMIT_BY_CLASS_PROGRESS_V1）。
    ///
    /// 與數字碼查詢打同一個端點（學生看得到班上每一筆的狀態），但這裡只在乎「幾分之幾已成功」。
    /// 讀不到可判讀的欄位（或只看到自己那一筆）就回 `Ok(None)`——呼叫端維持原本行為。
    #[instrument(skip(self), fields(rollcall_id = rollcall_id))]
    pub async fn get_rollcall_progress(
        &self,
        rollcall_id: u64,
    ) -> Result<Option<RollcallProgress>> {
        let url = format!(
            "{}/api/rollcall/{}/student_rollcalls",
            self.base_url, rollcall_id
        );
        debug!(url = %url, "GET student_rollcalls for class progress");

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .into_diagnostic()
            .wrap_err_with(|| {
                format!("GET /api/rollcall/{rollcall_id}/student_rollcalls failed")
            })?;

        let status = resp.status();
        match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return Err(miette::miette!(ApiError::Unauthorized));
            }
            _ if !status.is_success() => {
                let body = resp.text().await.unwrap_or_default();
                return Err(miette::miette!(ApiError::ApiStatus {
                    status: status.as_u16(),
                    body,
                }));
            }
            _ => {}
        }

        let body_bytes = resp
            .bytes()
            .await
            .into_diagnostic()
            .wrap_err("Failed to read student_rollcalls response body")?;
        let payload = serde_json::from_slice::<serde_json::Value>(&body_bytes).map_err(|e| {
            let body_str = String::from_utf8_lossy(&body_bytes);
            miette::miette!(ApiError::Json(format!(
                "{e}: body was `{}`",
                crate::text_utils::truncate_chars(&body_str, 200)
            )))
        })?;

        match parse_rollcall_progress(&payload) {
            Some(progress) => {
                debug!(
                    rollcall_id = rollcall_id,
                    completed = progress.completed,
                    total = progress.total,
                    source = %progress.source,
                    "讀到班上點名進度"
                );
                Ok(Some(progress))
            }
            None => {
                debug!(
                    rollcall_id = rollcall_id,
                    "student_rollcalls 沒有可判讀的班級進度（維持原本行為）"
                );
                Ok(None)
            }
        }
    }

    /// `PUT /api/rollcall/{id}/answer`（**空的答案**）— RADAR_EMPTY_ANSWER_V1。
    ///
    /// 抄自 auto-rollcall-thu-tronclass：對雷達點名送一個不帶座標的空物件 `{}`，
    /// 若這台伺服器沒有驗證座標，會直接判到場（參考專案實測 100%）。我們只把它當「第一發」：
    /// 沒被接受就照原本的座標流程走；每個帳號、每場點名只送一次（`radar.rs` 記錄）。
    #[instrument(skip(self), fields(rollcall_id = rollcall_id))]
    pub async fn answer_radar_rollcall_empty(
        &self,
        rollcall_id: u64,
    ) -> Result<AttendanceResult> {
        let url = format!("{}/api/rollcall/{}/answer", self.base_url, rollcall_id);
        debug!(url = %url, "PUT answer (radar rollcall, empty payload)");

        let resp = self
            .client
            .put(&url)
            .json(&serde_json::json!({}))
            .send()
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("PUT /api/rollcall/{rollcall_id}/answer (empty) failed"))?;

        let status = resp.status();
        match status {
            StatusCode::OK | StatusCode::CREATED | StatusCode::NO_CONTENT => {
                debug!("空答案被接受（這台伺服器沒有驗證座標）");
                Ok(AttendanceResult::Success)
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                Err(miette::miette!(ApiError::Unauthorized))
            }
            other => {
                let body_text = resp.text().await.unwrap_or_default();
                debug!(status = %other, body = %body_text, "空答案沒有被接受（改用座標送）");
                Ok(AttendanceResult::Failed {
                    reason: format!("HTTP {other}: {body_text}"),
                })
            }
        }
    }

    /// `PUT /api/rollcall/{id}/answer_number_rollcall` — 數字簽到
    #[instrument(skip(self), fields(rollcall_id = rollcall_id, number_code = %number_code))]
    pub async fn answer_number_rollcall(
        &self,
        rollcall_id: u64,
        number_code: &str,
    ) -> Result<AttendanceResult> {
        let url = format!(
            "{}/api/rollcall/{}/answer_number_rollcall",
            self.base_url, rollcall_id
        );
        let body = NumberRollcallBody::new(number_code);
        debug!(url = %url, code = %number_code, "PUT answer_number_rollcall");

        let resp = self
            .client
            .put(&url)
            .json(&body)
            .send()
            .await
            .into_diagnostic()
            .wrap_err_with(|| {
                format!("PUT /api/rollcall/{rollcall_id}/answer_number_rollcall failed")
            })?;

        let status = resp.status();
        match status {
            StatusCode::OK | StatusCode::CREATED | StatusCode::NO_CONTENT => {
                let body_text = resp.text().await.unwrap_or_default();
                let result = classify_number_rollcall_response(status, &body_text, number_code);
                let body_preview = response_body_preview(&body_text);

                match &result {
                    AttendanceResult::Success => {
                        debug!(
                            code = %number_code,
                            status = %status,
                            body = %body_preview,
                            "數字簽到成功"
                        );
                    }
                    AttendanceResult::Failed { reason } => {
                        debug!(
                            code = %number_code,
                            status = %status,
                            body = %body_preview,
                            reason = %reason,
                            "數字代碼錯誤"
                        );
                    }
                    AttendanceResult::TransientFailure { reason } => {
                        warn!(
                            code = %number_code,
                            status = %status,
                            body = %body_preview,
                            reason = %reason,
                            "數字簽到 2xx 回應無法確認成功"
                        );
                    }
                    AttendanceResult::RadarTooFar { .. } => {}
                }

                Ok(result)
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                Err(miette::miette!(ApiError::Unauthorized))
            }
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
                let body_text = resp.text().await.unwrap_or_default();
                debug!(code = %number_code, status = %status, body = %body_text, "數字代碼錯誤");
                Ok(AttendanceResult::Failed {
                    reason: format!("code {number_code} 不正確"),
                })
            }
            StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS => {
                let body_text = resp.text().await.unwrap_or_default();
                warn!(status = %status, body = %body_text, "數字簽到遇到暫時性 HTTP 失敗");
                Ok(AttendanceResult::TransientFailure {
                    reason: format!("HTTP {status}: {body_text}"),
                })
            }
            other if other.is_server_error() => {
                let body_text = resp.text().await.unwrap_or_default();
                warn!(status = %other, body = %body_text, "數字簽到遇到伺服器錯誤");
                Ok(AttendanceResult::TransientFailure {
                    reason: format!("HTTP {other}: {body_text}"),
                })
            }
            other => {
                let body_text = resp.text().await.unwrap_or_default();
                warn!(status = %other, body = %body_text, "數字簽到收到非預期狀態");
                Ok(AttendanceResult::TransientFailure {
                    reason: format!("HTTP {other}: {body_text}"),
                })
            }
        }
    }

    /// `PUT /api/rollcall/{id}/answer` — 雷達簽到
    #[instrument(skip(self), fields(rollcall_id = rollcall_id, lat = latitude, lon = longitude))]
    pub async fn answer_radar_rollcall(
        &self,
        rollcall_id: u64,
        latitude: f64,
        longitude: f64,
        accuracy: u32,
        altitude: i32,
    ) -> Result<AttendanceResult> {
        let url = format!("{}/api/rollcall/{}/answer", self.base_url, rollcall_id);
        let body = RadarRollcallBody::new(latitude, longitude, accuracy, altitude);
        debug!(url = %url, lat = latitude, lon = longitude, "PUT answer (radar rollcall)");

        let resp = self
            .client
            .put(&url)
            .json(&body)
            .send()
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("PUT /api/rollcall/{rollcall_id}/answer failed"))?;

        let status = resp.status();
        match status {
            StatusCode::OK | StatusCode::CREATED | StatusCode::NO_CONTENT => {
                debug!(lat = latitude, lon = longitude, "雷達簽到成功");
                Ok(AttendanceResult::Success)
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                Err(miette::miette!(ApiError::Unauthorized))
            }
            StatusCode::BAD_REQUEST
            | StatusCode::UNPROCESSABLE_ENTITY
            | StatusCode::NOT_ACCEPTABLE => {
                let body_text = resp.text().await.unwrap_or_default();
                debug!(status = %status, body = %body_text, "雷達簽到失敗，嘗試解析 distance");
                if let Ok(err_resp) = serde_json::from_str::<AttendanceResponse>(&body_text) {
                    if let Some(dist) = err_resp.distance {
                        return Ok(AttendanceResult::RadarTooFar { distance: dist });
                    }
                }
                Ok(AttendanceResult::Failed {
                    reason: format!("HTTP {status}: {body_text}"),
                })
            }
            other => {
                let body_text = resp.text().await.unwrap_or_default();
                warn!(status = %other, body = %body_text, "雷達簽到收到非預期狀態");
                if let Ok(err_resp) = serde_json::from_str::<AttendanceResponse>(&body_text) {
                    if let Some(dist) = err_resp.distance {
                        return Ok(AttendanceResult::RadarTooFar { distance: dist });
                    }
                }
                Ok(AttendanceResult::Failed {
                    reason: format!("HTTP {other}: {body_text}"),
                })
            }
        }
    }

    /// `PUT /api/rollcall/{id}/answer_self_registration_rollcall` — 自主報到
    #[instrument(skip(self), fields(rollcall_id = rollcall_id))]
    pub async fn answer_self_registration_rollcall(
        &self,
        rollcall_id: u64,
    ) -> Result<AttendanceResult> {
        let url = format!(
            "{}/api/rollcall/{}/answer_self_registration_rollcall",
            self.base_url, rollcall_id
        );
        debug!(url = %url, "PUT answer_self_registration_rollcall");

        let resp = self
            .client
            .put(&url)
            .json(&serde_json::json!({}))
            .send()
            .await
            .into_diagnostic()
            .wrap_err_with(|| {
                format!("PUT /api/rollcall/{rollcall_id}/answer_self_registration_rollcall failed")
            })?;

        let status = resp.status();
        match status {
            StatusCode::OK | StatusCode::CREATED | StatusCode::NO_CONTENT => {
                debug!("自主報到成功");
                Ok(AttendanceResult::Success)
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                Err(miette::miette!(ApiError::Unauthorized))
            }
            other
                if other == StatusCode::REQUEST_TIMEOUT
                    || other == StatusCode::TOO_MANY_REQUESTS
                    || other.is_server_error() =>
            {
                let body_text = resp.text().await.unwrap_or_default();
                warn!(status = %other, body = %body_text, "自主報到遇到暫時性 HTTP 失敗");
                Ok(AttendanceResult::TransientFailure {
                    reason: format!("HTTP {other}: {body_text}"),
                })
            }
            other => {
                let body_text = resp.text().await.unwrap_or_default();
                warn!(status = %other, body = %body_text, "自主報到失敗");
                Ok(AttendanceResult::Failed {
                    reason: format!("HTTP {other}: {body_text}"),
                })
            }
        }
    }

    /// `PUT /api/rollcall/{id}/answer_qr_rollcall` — QR Code 簽到
    #[instrument(skip(self, data), fields(rollcall_id = rollcall_id))]
    pub async fn answer_qr_rollcall(
        &self,
        rollcall_id: u64,
        data: impl Into<String>,
    ) -> Result<AttendanceResult> {
        let url = format!(
            "{}/api/rollcall/{}/answer_qr_rollcall",
            self.base_url, rollcall_id
        );
        let body = QrCodeRollcallBody::new(data);
        debug!(url = %url, "PUT answer_qr_rollcall");

        let resp = self
            .client
            .put(&url)
            .json(&body)
            .send()
            .await
            .into_diagnostic()
            .wrap_err_with(|| {
                format!("PUT /api/rollcall/{rollcall_id}/answer_qr_rollcall failed")
            })?;

        let status = resp.status();
        match status {
            StatusCode::OK | StatusCode::CREATED | StatusCode::NO_CONTENT => {
                debug!("QR Code 簽到成功");
                Ok(AttendanceResult::Success)
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                Err(miette::miette!(ApiError::Unauthorized))
            }
            other => {
                let body_text = resp.text().await.unwrap_or_default();
                warn!(status = %other, body = %body_text, "QR Code 簽到失敗");
                Ok(AttendanceResult::Failed {
                    reason: format!("HTTP {other}: {body_text}"),
                })
            }
        }
    }
}

// ─── 測試 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn test_rollcall_needs_attendance() {
        let mut rc = make_test_rollcall();
        rc.status = "absent".to_string();
        rc.is_expired = false;
        assert!(rc.needs_attendance());

        rc.is_expired = true;
        assert!(!rc.needs_attendance());

        rc.is_expired = false;
        rc.status = "on_call_fine".to_string();
        assert!(!rc.needs_attendance());
    }

    #[test]
    fn test_rollcall_is_attended() {
        let mut rc = make_test_rollcall();
        rc.status = "on_call_fine".to_string();
        assert!(rc.is_attended());

        rc.status = "absent".to_string();
        assert!(!rc.is_attended());
    }

    #[test]
    fn test_attendance_type_number() {
        let mut rc = make_test_rollcall();
        rc.is_number = true;
        rc.is_radar = false;
        assert_eq!(rc.attendance_type(), AttendanceType::Number);
    }

    #[test]
    fn test_attendance_type_radar() {
        let mut rc = make_test_rollcall();
        rc.is_radar = true;
        // is_radar 優先
        rc.is_number = true;
        assert_eq!(rc.attendance_type(), AttendanceType::Radar);
    }

    #[test]
    fn test_attendance_type_qrcode() {
        let mut rc = make_test_rollcall();
        rc.is_number = false;
        rc.is_radar = false;
        assert_eq!(rc.attendance_type(), AttendanceType::QrCode);
    }

    #[test]
    fn test_number_rollcall_body_format() {
        let body = NumberRollcallBody::new("0042");
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["numberCode"], "0042");
        assert!(json["deviceId"].is_string());
        // deviceId 應為合法的 UUID 格式
        let device_id_str = json["deviceId"].as_str().unwrap();
        assert!(Uuid::parse_str(device_id_str).is_ok());
    }

    #[test]
    fn test_radar_rollcall_body_null_fields() {
        let body = RadarRollcallBody::new(24.3, 118.0, 35, 0);
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["latitude"], 24.3);
        assert_eq!(json["longitude"], 118.0);
        assert_eq!(json["accuracy"], 35);
        assert_eq!(json["altitude"], 0);
        assert!(json["altitudeAccuracy"].is_null());
        assert!(json["heading"].is_null());
        assert!(json["speed"].is_null());
    }

    #[test]
    fn test_qrcode_rollcall_body() {
        let body = QrCodeRollcallBody::new("test-data-string");
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["data"], "test-data-string");
        assert!(json["deviceId"].is_string());
    }

    #[test]
    fn test_rollcalls_response_deserialize() {
        let raw = r#"{
            "rollcalls": [
                {
                    "rollcall_id": 12345,
                    "course_title": "計算機網路",
                    "created_by_name": "陳教授",
                    "department_name": "資訊工程系",
                    "is_expired": false,
                    "is_number": false,
                    "is_radar": true,
                    "status": "absent",
                    "rollcall_status": "ongoing",
                    "scored": false
                }
            ]
        }"#;

        let resp: RollcallsResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.rollcalls.len(), 1);
        let rc = &resp.rollcalls[0];
        assert_eq!(rc.rollcall_id, 12345);
        assert_eq!(rc.course_title, "計算機網路");
        assert!(rc.is_radar);
        assert!(!rc.is_number);
        assert!(rc.needs_attendance());
        assert_eq!(rc.attendance_type(), AttendanceType::Radar);
    }

    #[test]
    fn test_attendance_response_radar_distance() {
        let raw = r#"{"distance": 42.5}"#;
        let resp: AttendanceResponse = serde_json::from_str(raw).unwrap();
        assert!(resp.is_radar_distance_error());
        assert_eq!(resp.distance, Some(42.5));
    }

    #[test]
    fn test_number_rollcall_response_success_json() {
        let result =
            classify_number_rollcall_response(StatusCode::OK, r#"{"success":true}"#, "0042");
        assert!(result.is_success());

        let result = classify_number_rollcall_response(
            StatusCode::OK,
            r#"{"rollcall":{"status":"on_call_fine"}}"#,
            "0042",
        );
        assert!(result.is_success());
    }

    #[test]
    fn test_number_rollcall_response_wrong_code_with_200_json() {
        let result = classify_number_rollcall_response(
            StatusCode::OK,
            r#"{"success":false,"message":"number code incorrect"}"#,
            "0042",
        );
        assert_number_failed(result);

        let result = classify_number_rollcall_response(
            StatusCode::OK,
            r#"{"message":"驗證碼錯誤"}"#,
            "0042",
        );
        assert_number_failed(result);
    }

    #[test]
    fn test_number_rollcall_response_unverified_200_is_transient() {
        let result = classify_number_rollcall_response(
            StatusCode::OK,
            r#"{"message":"request accepted"}"#,
            "0042",
        );
        assert_number_transient(result);

        let result = classify_number_rollcall_response(StatusCode::OK, "", "0042");
        assert_number_transient(result);
    }

    #[test]
    fn test_number_rollcall_response_ack_record_is_success() {
        // NUMBER_ID_ACK_V1（fix28）：實測正確數字回 200 + 新建 student_rollcall
        // 記錄 {"id":68019054,"status":"on_call"}，無 success 布林／成功文字。
        // 舊邏輯誤報「數字簽到回應無法確認成功」。
        let result = classify_number_rollcall_response(
            StatusCode::OK,
            r#"{"id":68019054,"status":"on_call"}"#,
            "0042",
        );
        assert!(result.is_success());

        // 純 status:"on_call"（無 id）也算成功（全字匹配；reverify 把關）。
        let result =
            classify_number_rollcall_response(StatusCode::OK, r#"{"status":"on_call"}"#, "0042");
        assert!(result.is_success());

        // 字串形式的數字 id 同樣是立案 ack。
        let result = classify_number_rollcall_response(
            StatusCode::OK,
            r#"{"id":"68019054","status":"on_call"}"#,
            "0042",
        );
        assert!(result.is_success());

        // 失敗特徵仍然優先：帶錯誤訊息的回應不得因含 id 而誤判成功。
        let result = classify_number_rollcall_response(
            StatusCode::OK,
            r#"{"id":1,"error":"number code incorrect"}"#,
            "0042",
        );
        assert_number_failed(result);

        // 巢狀 id（非頂層）不構成 ack，維持保守的 TransientFailure。
        let result = classify_number_rollcall_response(
            StatusCode::OK,
            r#"{"message":"request accepted","rollcall":{"id":1172118}}"#,
            "0042",
        );
        assert_number_transient(result);

        // 非數字 id 不構成 ack。
        let result = classify_number_rollcall_response(
            StatusCode::OK,
            r#"{"id":"abc"}"#,
            "0042",
        );
        assert_number_transient(result);
    }

    #[test]
    fn test_number_rollcall_no_content_success() {
        let result = classify_number_rollcall_response(StatusCode::NO_CONTENT, "", "0042");
        assert!(result.is_success());
    }

    #[test]
    fn test_attendance_result_is_success() {
        assert!(AttendanceResult::Success.is_success());
        assert!(!AttendanceResult::RadarTooFar { distance: 10.0 }.is_success());
        assert!(!AttendanceResult::Failed {
            reason: "err".into()
        }
        .is_success());
        assert!(!AttendanceResult::TransientFailure {
            reason: "HTTP 429 Too Many Requests".into()
        }
        .is_success());
    }

    #[test]
    fn test_attendance_type_display() {
        assert_eq!(AttendanceType::Number.to_string(), "數字簽到");
        assert_eq!(AttendanceType::Radar.to_string(), "雷達簽到");
        assert_eq!(AttendanceType::QrCode.to_string(), "QR Code 簽到");
    }

    // ── 輔助函式 ──────────────────────────────────────────────────────────────

    fn make_test_rollcall() -> Rollcall {
        Rollcall {
            rollcall_id: 1,
            course_title: "測試課程".to_string(),
            created_by_name: "測試教授".to_string(),
            department_name: "測試系".to_string(),
            is_expired: false,
            is_number: false,
            is_radar: false,
            status: "absent".to_string(),
            rollcall_status: "ongoing".to_string(),
            scored: false,
            rollcall_type_raw: String::new(),
            rollcall_type: String::new(),
            name: String::new(),
        }
    }

    fn assert_number_failed(result: AttendanceResult) {
        match result {
            AttendanceResult::Failed { .. } => {}
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    fn assert_number_transient(result: AttendanceResult) {
        match result {
            AttendanceResult::TransientFailure { .. } => {}
            other => panic!("expected TransientFailure, got {other:?}"),
        }
    }

    // ── AUTO_SUBMIT_BY_CLASS_PROGRESS_V1（fix72）────────────────────────────
    #[test]
    fn test_rollcall_progress_strict_threshold() {
        // 剛好 15%（6/40）→ 不算「超過」；15% 多一點點（7/40）才算。
        let exact = RollcallProgress {
            completed: 6,
            total: 40,
            source: "test".to_string(),
        };
        assert!(!exact.passed(15), "剛好 15% 不該觸發（要超過）");
        assert!(exact.passed(14));
        let over = RollcallProgress {
            completed: 7,
            total: 40,
            source: "test".to_string(),
        };
        assert!(over.passed(15));
        assert_eq!(over.percent_rounded(), 18);
        // 門檻常數本身就是 15（頁面與訊息都照它顯示）。
        assert_eq!(AUTO_SUBMIT_PERCENT, 15);
    }

    #[test]
    fn test_parse_rollcall_progress_aggregate_counts() {
        let payload = serde_json::json!({ "total_count": 40, "signed_count": 11 });
        let progress = parse_rollcall_progress(&payload).expect("應該讀到統計欄位");
        assert_eq!(progress.completed, 11);
        assert_eq!(progress.total, 40);
        assert!(progress.passed(25));
        assert!(progress.source.contains("total_count"));
    }

    #[test]
    fn test_parse_rollcall_progress_student_list() {
        let payload = serde_json::json!({
            "student_rollcalls": [
                { "student_id": 1, "status": "on_call_fine" },
                { "student_id": 2, "status": "absent" },
                { "student_id": 3, "status": "on_call_fine" },
                { "student_id": 4, "status": "absent" },
                { "student_id": 5, "status": "absent" },
                { "student_id": 6, "status": "absent" },
                { "student_id": 7, "status": "on_call_fine" },
                { "student_id": 8, "status": "absent" }
            ]
        });
        let progress = parse_rollcall_progress(&payload).expect("應該讀到學生名單");
        assert_eq!(progress.completed, 3);
        assert_eq!(progress.total, 8);
        assert!(!progress.passed(25));
        assert!(progress.passed(30));
    }

    #[test]
    fn test_parse_rollcall_progress_ignores_own_record_only() {
        // 只有自己那一筆時 1/1＝100%，絕對不能當成「全班都完成了」
        let payload = serde_json::json!({ "student_rollcalls": [{ "status": "on_call_fine" }] });
        assert!(parse_rollcall_progress(&payload).is_none());
    }

    #[test]
    fn test_parse_rollcall_progress_unknown_shape_is_none() {
        let payload = serde_json::json!({ "data": { "number_code": "1234" }, "foo": [1, 2, 3] });
        assert!(parse_rollcall_progress(&payload).is_none());
    }
}
