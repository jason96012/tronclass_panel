//! QR Code 簽到解析與簽到邏輯模組
//!
//! # QR Code 編碼格式
//!
//! Tronclass 的 QR code 內容為一個 URL，格式為：
//! - `/j?p=<encoded>`
//! - `/scanner-jumper?p=<encoded>`
//!
//! `p` 參數是特殊編碼的字串，解碼方式如下：
//!
//! 1. URL decode（處理 `%XX` 轉義）
//! 2. 用 `!`（或 chr(31)）分割成多個 segment
//! 3. 每個 segment 用 `~`（或 chr(30)）分割成 `key` 和 `value`
//! 4. `key` 是 base36 整數，對應欄位名稱（見 `KEY_MAP`）
//! 5. `value` 可能帶有特殊前綴字元：
//!    - `chr(26)`（0x1A）開頭 → bool 值（緊接的字元 `1`/`0` 或 `t`/`f`）
//!    - `chr(16)`（0x10）開頭 → 數字（後面跟十進位數字字串）
//!    - 其他 → 純字串
//!
//! # 欄位 Key 對應表（base36）
//! | base36 | 十進位 | 欄位名稱      |
//! |--------|--------|---------------|
//! | `0`    | 0      | courseId      |
//! | `1`    | 1      | activityId    |
//! | `2`    | 2      | activityType  |
//! | `3`    | 3      | data          |
//! | `4`    | 4      | rollcallId    |
//! | `5`    | 5      | type          |
//! | `6`    | 6      | extra         |
//!
//! # 使用範例
//! ```
//! let url = "https://elearn2.fju.edu.tw/scanner-jumper?p=0~12345!3~mydata!4~67890";
//! let parsed = parse_qr_url(url).unwrap();
//! assert_eq!(parsed.rollcall_id, Some(67890));
//! assert_eq!(parsed.data, Some("mydata".to_string()));
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use miette::{IntoDiagnostic, Result, WrapErr};
use tracing::{debug, info, instrument, warn};

use crate::api::{rollcall::AttendanceResult, ApiClient};

// ─── 特殊控制字元常數 ─────────────────────────────────────────────────────────

/// chr(30) — 替代 `~` 的轉義字元（在 value 中出現的字面 `~`）
const CHAR_TILDE_ESCAPE: char = '\x1E'; // ASCII 30 (RS - Record Separator)

/// chr(31) — 替代 `!` 的轉義字元（在 value 中出現的字面 `!`）
const CHAR_BANG_ESCAPE: char = '\x1F'; // ASCII 31 (US - Unit Separator)

/// chr(26) — bool 值前綴
const CHAR_BOOL_PREFIX: char = '\x1A'; // ASCII 26 (SUB)

/// chr(16) — 數字值前綴
const CHAR_NUM_PREFIX: char = '\x10'; // ASCII 16 (DLE)

/// QR code URL 的 p 參數名稱
const PARAM_NAME: &str = "p";

/// 舊版 QR code 路徑
const PATH_J: &str = "/j";

/// 新版 QR code 路徑
const PATH_SCANNER_JUMPER: &str = "/scanner-jumper";

// ─── Key 對應表 ───────────────────────────────────────────────────────────────

/// base36 key → 欄位名稱的對應表
///
/// key 本身是 base36 整數的字串表示，數值即索引：
/// 0→courseId, 1→activityId, 2→activityType, 3→data, 4→rollcallId, 5→type, 6→extra
const KEY_MAP: &[(&str, &str)] = &[
    ("0", "courseId"),
    ("1", "activityId"),
    ("2", "activityType"),
    ("3", "data"),
    ("4", "rollcallId"),
    ("5", "type"),
    ("6", "extra"),
    // 保留擴充欄位（部分版本可能有更多）
    ("7", "userId"),
    ("8", "sessionId"),
    ("9", "timestamp"),
    ("a", "version"),
    ("b", "checksum"),
];

/// 取得 key 對應的欄位名稱
fn key_name(base36_key: &str) -> Option<&'static str> {
    KEY_MAP
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(base36_key))
        .map(|(_, v)| *v)
}

// ─── 解析結果結構 ─────────────────────────────────────────────────────────────

/// QR Code 解析結果
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QrCodeData {
    /// 課程 ID
    pub course_id: Option<u64>,

    /// 活動 ID
    pub activity_id: Option<u64>,

    /// 活動類型
    pub activity_type: Option<u64>,

    /// 簽到驗證資料（傳給 API 的 `data` 欄位）
    pub data: Option<String>,

    /// 點名 ID（對應 API 的 rollcall_id）
    pub rollcall_id: Option<u64>,

    /// 類型
    pub type_field: Option<u64>,

    /// 其他欄位（原始 key-value）
    pub extra: HashMap<String, QrValue>,
}

/// QR Code 欄位的值類型
#[derive(Debug, Clone, PartialEq)]
pub enum QrValue {
    /// 字串值
    String(String),
    /// 數字值
    Number(i64),
    /// 布林值
    Bool(bool),
}

impl QrValue {
    /// 嘗試轉換為字串
    pub fn as_str(&self) -> Option<&str> {
        match self {
            QrValue::String(s) => Some(s),
            _ => None,
        }
    }

    /// 嘗試轉換為數字
    pub fn as_number(&self) -> Option<i64> {
        match self {
            QrValue::Number(n) => Some(*n),
            QrValue::String(s) => s.parse().ok(),
            QrValue::Bool(b) => Some(*b as i64),
        }
    }

    /// 嘗試轉換為 bool
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            QrValue::Bool(b) => Some(*b),
            QrValue::Number(n) => Some(*n != 0),
            QrValue::String(s) => match s.to_lowercase().as_str() {
                "true" | "1" | "yes" => Some(true),
                "false" | "0" | "no" => Some(false),
                _ => None,
            },
        }
    }
}

impl std::fmt::Display for QrValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QrValue::String(s) => write!(f, "{s}"),
            QrValue::Number(n) => write!(f, "{n}"),
            QrValue::Bool(b) => write!(f, "{b}"),
        }
    }
}

impl QrCodeData {
    /// 判斷此 QR code 資料是否包含足夠的簽到資訊
    pub fn is_valid_rollcall(&self) -> bool {
        self.data.is_some() && self.rollcall_id.is_some()
    }

    /// 取得用於顯示的摘要字串
    pub fn summary(&self) -> String {
        format!(
            "QrCodeData {{ rollcall_id: {:?}, course_id: {:?}, data: {:?} }}",
            self.rollcall_id, self.course_id, self.data,
        )
    }
}

// ─── QR Code 解析 ─────────────────────────────────────────────────────────────

/// 解析錯誤類型
#[derive(Debug, thiserror::Error)]
pub enum QrParseError {
    #[error("不是有效的 Tronclass QR code URL：{url}")]
    NotQrUrl { url: String },

    #[error("URL 中缺少 `p` 參數")]
    MissingParam,

    #[error("p 參數解碼失敗：{reason}")]
    DecodeFailed { reason: String },

    #[error("QR code 資料不完整：缺少 data 欄位")]
    MissingData,

    #[error("QR code 資料不完整：缺少 rollcallId 欄位")]
    MissingRollcallId,
}

/// 從完整 QR code URL 解析出結構化資料
///
/// 接受以下格式的 URL：
/// - `https://elearn2.fju.edu.tw/j?p=...`
/// - `https://elearn2.fju.edu.tw/scanner-jumper?p=...`
/// - `/j?p=...`（相對 URL）
/// - 純 `p` 參數值（直接傳入編碼後的字串）
pub fn parse_qr_url(input: &str) -> Result<QrCodeData> {
    debug!(input = %input, "解析 QR code URL");

    // 嘗試判斷是否為完整 URL 或相對 URL
    let p_value = if input.contains("?p=") || input.contains("&p=") {
        // 是 URL，提取 p 參數
        extract_p_param(input)?
    } else if input.starts_with('/') {
        // 相對 URL 沒有 p 參數
        return Err(miette::miette!(QrParseError::MissingParam));
    } else {
        // 假設輸入本身就是 p 參數值
        input.to_string()
    };

    // URL decode p 參數
    let decoded = url_decode(&p_value);
    debug!(decoded = %decoded, "p 參數 URL decode 結果");

    // 解析編碼的 p 參數
    parse_encoded_p(&decoded)
        .into_diagnostic()
        .wrap_err_with(|| format!("解析 QR code p 參數失敗：{decoded}"))
}

/// 從 URL 字串中提取 `p` 參數的值
fn extract_p_param(url: &str) -> Result<String> {
    // 找到 ? 後面的 query string
    let query = if let Some(pos) = url.find('?') {
        &url[pos + 1..]
    } else {
        return Err(miette::miette!(QrParseError::MissingParam));
    };

    // 解析 query string
    for pair in query.split('&') {
        if let Some(eq_pos) = pair.find('=') {
            let key = &pair[..eq_pos];
            let value = &pair[eq_pos + 1..];
            if key == PARAM_NAME {
                return Ok(value.to_string());
            }
        }
    }

    Err(miette::miette!(QrParseError::MissingParam))
}

/// 解析特殊編碼的 p 參數字串
///
/// 格式：`key~value!key~value!...`
/// 其中 key 是 base36 整數，value 可能帶特殊前綴字元
fn parse_encoded_p(encoded: &str) -> std::result::Result<QrCodeData, QrParseError> {
    if encoded.is_empty() {
        return Err(QrParseError::DecodeFailed {
            reason: "p 參數為空字串".to_string(),
        });
    }

    let mut data = QrCodeData::default();

    // 以 `!` 或 chr(31) 分割 segments
    let segments: Vec<&str> = split_by_bang(encoded);

    debug!(
        segment_count = segments.len(),
        "分割出 {} 個 segment",
        segments.len()
    );

    for (i, segment) in segments.iter().enumerate() {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }

        // 以 `~` 或 chr(30) 分割 key 和 value
        match split_key_value(segment) {
            Some((raw_key, raw_value)) => {
                let field_name = key_name(raw_key).unwrap_or(raw_key);
                let value = decode_value(raw_value);

                debug!(
                    segment_idx = i,
                    key = raw_key,
                    field = field_name,
                    value = %value,
                    "解析 segment"
                );

                // 根據欄位名稱填入對應欄位
                match field_name {
                    "courseId" => {
                        data.course_id = value.as_number().map(|n| n as u64);
                    }
                    "activityId" => {
                        data.activity_id = value.as_number().map(|n| n as u64);
                    }
                    "activityType" => {
                        data.activity_type = value.as_number().map(|n| n as u64);
                    }
                    "data" => {
                        data.data = Some(value.to_string());
                    }
                    "rollcallId" => {
                        data.rollcall_id = value.as_number().map(|n| n as u64);
                    }
                    "type" => {
                        data.type_field = value.as_number().map(|n| n as u64);
                    }
                    _ => {
                        // 未知欄位存入 extra
                        data.extra.insert(field_name.to_string(), value);
                    }
                }
            }
            None => {
                // segment 沒有分隔符，可能是純值或格式有問題
                warn!(segment = %segment, "QR code segment 缺少分隔符，跳過");
            }
        }
    }

    Ok(data)
}

/// 以 `!` 或 chr(31) 分割字串（不包含 value 中的轉義字元）
///
/// 注意：chr(31) 是 CHAR_BANG_ESCAPE，表示 value 中的字面 `!`，
/// 但在頂層分割時 `!` 是 segment 分隔符，chr(31) 是字面 `!`。
/// 因此頂層只以 ASCII `!` 分割。
fn split_by_bang(s: &str) -> Vec<&str> {
    s.split('!').filter(|seg| !seg.is_empty()).collect()
}

/// 以 `~` 或 chr(30) 分割 key 和 value
///
/// 只以第一個分隔符分割（value 中可能含 `~`）。
/// 優先以 ASCII `~` 分割，若沒有則以 chr(30) 分割。
fn split_key_value(segment: &str) -> Option<(&str, &str)> {
    // 先找 ASCII `~`
    if let Some(pos) = segment.find('~') {
        return Some((&segment[..pos], &segment[pos + 1..]));
    }
    // 再找 chr(30)
    if let Some(pos) = segment.find(CHAR_TILDE_ESCAPE) {
        let byte_len = CHAR_TILDE_ESCAPE.len_utf8();
        return Some((&segment[..pos], &segment[pos + byte_len..]));
    }
    None
}

/// 解碼 value 字串
///
/// 處理：
/// - chr(26) 前綴 → bool 值
/// - chr(16) 前綴 → 數字值
/// - chr(30) → 字面 `~`（字串中的轉義）
/// - chr(31) → 字面 `!`（字串中的轉義）
/// - 純字串
fn decode_value(raw: &str) -> QrValue {
    if raw.is_empty() {
        return QrValue::String(String::new());
    }

    // UNWRAP_SWEEP_V1：上面已檢查非空，但仍以 let-else 收斂，避免日後改動順序時 panic。
    let Some(first_char) = raw.chars().next() else {
        return QrValue::String(String::new());
    };

    match first_char {
        // bool 前綴
        CHAR_BOOL_PREFIX => {
            let rest = &raw[CHAR_BOOL_PREFIX.len_utf8()..];
            let bool_val = match rest.chars().next() {
                Some('1') | Some('t') | Some('T') | Some('y') | Some('Y') => true,
                Some('0') | Some('f') | Some('F') | Some('n') | Some('N') => false,
                // 無法識別，預設為 false
                _ => {
                    debug!(rest = %rest, "bool 前綴後跟未知字元，預設 false");
                    false
                }
            };
            QrValue::Bool(bool_val)
        }

        // 數字前綴
        CHAR_NUM_PREFIX => {
            let rest = &raw[CHAR_NUM_PREFIX.len_utf8()..];
            match rest.parse::<i64>() {
                Ok(n) => QrValue::Number(n),
                Err(_) => {
                    // 嘗試去除前導零再解析
                    match rest.trim_start_matches('0').parse::<i64>() {
                        Ok(n) => QrValue::Number(n),
                        Err(_) => {
                            warn!(rest = %rest, "數字前綴後無法解析數字，當作字串");
                            QrValue::String(unescape_value(rest))
                        }
                    }
                }
            }
        }

        // 純字串（含轉義字元替換）
        _ => {
            // 嘗試直接解析為數字（部分欄位直接就是數字字串）
            if raw.chars().all(|c| c.is_ascii_digit() || c == '-') {
                if let Ok(n) = raw.parse::<i64>() {
                    return QrValue::Number(n);
                }
            }

            QrValue::String(unescape_value(raw))
        }
    }
}

/// 還原 value 字串中的轉義字元
///
/// - chr(30) → `~`
/// - chr(31) → `!`
fn unescape_value(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            CHAR_TILDE_ESCAPE => result.push('~'),
            CHAR_BANG_ESCAPE => result.push('!'),
            other => result.push(other),
        }
    }
    result
}

/// 簡單的 URL decode（處理 `%XX` 格式的百分比編碼）
///
/// 不依賴外部 crate，只處理常見的 ASCII 轉義。
/// 對於複雜的多字節 UTF-8，使用逐字節解碼。
pub fn url_decode(input: &str) -> String {
    let mut result = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            // 嘗試解析 %XX
            let hi = bytes[i + 1];
            let lo = bytes[i + 2];

            if let (Some(h), Some(l)) = (hex_digit(hi), hex_digit(lo)) {
                result.push((h << 4) | l);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            // + 代表空格（application/x-www-form-urlencoded）
            result.push(b' ');
            i += 1;
            continue;
        }

        result.push(bytes[i]);
        i += 1;
    }

    // 嘗試解析為 UTF-8，失敗則 lossy 轉換
    String::from_utf8(result).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// 將 hex 字元轉換為數值（0-15）
fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ─── 簽到執行邏輯 ─────────────────────────────────────────────────────────────

/// QR Code 簽到的結果
#[derive(Debug, Clone)]
pub enum QrCodeResult {
    /// 簽到成功
    Success {
        /// 使用的 data 字串
        data: String,
    },
    /// 簽到失敗
    Failed { reason: String },
    /// QR code 解析失敗
    ParseError { reason: String },
    /// 發生不可恢復的錯誤（session 過期等）
    Error(String),
}

impl QrCodeResult {
    pub fn is_success(&self) -> bool {
        matches!(self, QrCodeResult::Success { .. })
    }
}

impl std::fmt::Display for QrCodeResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QrCodeResult::Success { data } => write!(f, "QR Code 簽到成功（data: {data}）"),
            QrCodeResult::Failed { reason } => write!(f, "QR Code 簽到失敗：{reason}"),
            QrCodeResult::ParseError { reason } => write!(f, "QR Code 解析失敗：{reason}"),
            QrCodeResult::Error(e) => write!(f, "QR Code 簽到錯誤：{e}"),
        }
    }
}

/// 對指定的 rollcall 執行 QR Code 簽到
///
/// # 流程
/// 1. 解析傳入的 QR code URL 或 p 參數字串
/// 2. 提取 `data` 欄位
/// 3. 呼叫 API `PUT /api/rollcall/{id}/answer_qr_rollcall`
///
/// # 參數
/// - `api`：已認證的 API 客戶端
/// - `rollcall_id`：要簽到的 rollcall ID
/// - `qr_input`：QR code 的完整 URL，或 p 參數值，或解析後的 `data` 字串
/// - `is_raw_data`：若為 true，`qr_input` 直接作為 data 傳給 API（跳過解析步驟）
#[instrument(skip(api), fields(rollcall_id = rollcall_id))]
pub async fn attempt_qrcode_rollcall(
    api: Arc<ApiClient>,
    rollcall_id: u64,
    qr_input: &str,
    is_raw_data: bool,
) -> QrCodeResult {
    // ── 取得 data 字串 ────────────────────────────────────────────────────────
    let data = if is_raw_data {
        debug!("使用原始 data 字串（跳過 QR code 解析）");
        qr_input.to_string()
    } else {
        match parse_qr_url(qr_input) {
            Ok(parsed) => {
                debug!(parsed = %parsed.summary(), "QR code 解析成功");

                if let Some(d) = parsed.data {
                    d
                } else {
                    warn!("QR code 解析結果缺少 data 欄位");
                    return QrCodeResult::ParseError {
                        reason: "QR code 中沒有 data 欄位".to_string(),
                    };
                }
            }
            Err(e) => {
                warn!(error = %e, "QR code 解析失敗");
                return QrCodeResult::ParseError {
                    reason: e.to_string(),
                };
            }
        }
    };

    info!(
        rollcall_id = rollcall_id,
        data_len = data.len(),
        "執行 QR Code 簽到"
    );

    // ── 呼叫 API ──────────────────────────────────────────────────────────────
    match api.answer_qr_rollcall(rollcall_id, data.clone()).await {
        Ok(AttendanceResult::Success) => {
            info!(rollcall_id = rollcall_id, "✅ QR Code 簽到成功");
            QrCodeResult::Success { data }
        }
        Ok(AttendanceResult::Failed { reason }) => {
            warn!(reason = %reason, "QR Code 簽到失敗");
            QrCodeResult::Failed { reason }
        }
        Ok(AttendanceResult::TransientFailure { reason }) => {
            warn!(reason = %reason, "QR Code 簽到暫時性失敗");
            QrCodeResult::Failed { reason }
        }
        Ok(AttendanceResult::RadarTooFar { distance }) => {
            // QR Code 簽到不應收到此回應
            warn!(distance = distance, "QR Code 簽到收到非預期的 RadarTooFar");
            QrCodeResult::Failed {
                reason: format!("非預期的回應（RadarTooFar: {distance:.2}m）"),
            }
        }
        Err(e) => {
            let err_str = e.to_string();
            warn!(error = %err_str, "QR Code 簽到 API 錯誤");
            if err_str.contains("Unauthorized")
                || err_str.contains("401")
                || err_str.contains("403")
                || err_str.contains("session")
                || err_str.contains("Session")
            {
                QrCodeResult::Error(err_str)
            } else {
                QrCodeResult::Failed { reason: err_str }
            }
        }
    }
}

// ─── 輔助：從 Line Bot 收到的訊息提取 QR code ─────────────────────────────────

/// 嘗試從使用者訊息中提取 Tronclass QR code URL
///
/// 支援：
/// - 完整 URL（含 `elearn2.fju.edu.tw`）
/// - 純 p 參數字串（看起來像 `0~xxx!3~yyy` 的格式）
/// - `/j?p=xxx` 或 `/scanner-jumper?p=xxx` 的相對 URL
pub fn extract_qr_from_message(message: &str) -> Option<String> {
    let message = message.trim();

    // 判斷是否包含 Tronclass 網域
    if message.contains("elearn2.fju.edu.tw")
        || message.contains("/j?p=")
        || message.contains("/scanner-jumper?p=")
    {
        return Some(message.to_string());
    }

    // 嘗試找到 URL
    for word in message.split_whitespace() {
        if word.starts_with("http://") || word.starts_with("https://") {
            if word.contains("elearn")
                || word.contains("tronclass")
                || word.contains("scanner")
                || word.contains("/j?")
            {
                return Some(word.to_string());
            }
        }
    }

    // 判斷是否為 p 參數格式（含 ~ 和 ! 的字串）
    if message.contains('~') && (message.contains('!') || message.len() > 10) {
        // 粗略判斷：看起來像 `key~value!key~value` 的格式
        let looks_like_p = message.split('!').take(2).all(|seg| seg.contains('~'));
        if looks_like_p {
            return Some(message.to_string());
        }
    }

    None
}

/// 判斷一個 URL 是否為 Tronclass QR code URL
pub fn is_tronclass_qr_url(url: &str) -> bool {
    (url.contains(PATH_J) || url.contains(PATH_SCANNER_JUMPER))
        && (url.contains("?p=") || url.contains("&p="))
}

// ─── 測試 ─────────────────────────────────────────────────────────────────────

