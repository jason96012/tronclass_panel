//! 點名送出紀錄（ROLLCALL_ATTENDANCE_V1／fix66）。
//!
//! 為什麼要落地成資料表：面板原本只有記憶體裡「最近 50 筆」的點名歷史
//! （`MonitorStatus.rollcall_history`），重啟就消失，也撐不起一整個學期的出席統計。
//! 這裡把每一次送出結果（成功與失敗都算）寫進 SQLite，讓面板能回答兩個問題：
//!
//! 1. 這學期每門課點名幾次、送出成功幾次？（出席統計）
//! 2. 這一次點名是由哪個帳號／誰送出的？（送出者）
//!
//! 資料量很小（每人每天數筆），因此單筆直接 INSERT；保留 400 天後自動清理。

use miette::{IntoDiagnostic, Result, WrapErr};
use serde::Serialize;
use sqlx::FromRow;

use super::{truncate_event_text, AccountsDb};

/// 保留天數：超過就清掉。一人一學期大約數百筆，對 SQLite 完全不是負擔。
pub(crate) const ROLLCALL_EVENT_RETENTION_DAYS: i64 = 400;
/// 每寫入幾筆做一次清理（用 rowid 取模判斷，不需要額外維護計數器）。
const ROLLCALL_EVENT_PRUNE_INTERVAL: i64 = 50;
const ROLLCALL_IDENTITY_MAX_BYTES: usize = 256;
const ROLLCALL_COURSE_MAX_BYTES: usize = 512;
const ROLLCALL_TYPE_MAX_BYTES: usize = 64;
const ROLLCALL_RESULT_MAX_BYTES: usize = 256;

/// 寫入一筆點名送出紀錄（monitor 每次送出後呼叫）。
#[derive(Debug, Clone)]
pub struct RollcallEventInput {
    /// 面板帳號 ID（帳號管理頁的那個 ID）。
    pub account_id: String,
    /// 學校帳號（學號）。
    pub username: String,
    /// 學校姓名（iClass profile name；拿不到時 monitor 會退回學校帳號）。
    pub user_name: String,
    pub rollcall_id: u64,
    pub course_name: String,
    pub attendance_type: String,
    pub success: bool,
    pub result: String,
    pub class_total: i64,
    pub class_signed: i64,
    /// ATT_STATS_AUTO_MANUAL_V1：這次是不是「大家都沒按按鈕，系統因為班上
    /// 超過 15% 的人點名成功而自動送出」（`AUTO_SUBMIT_BY_CLASS_PROGRESS_V1`／
    /// fix72）。使用者自己輸入代碼、按「自動讀取」／「系統演算」按鈕都算手動
    /// （有人動手觸發）；只有系統在完全沒人動手的情況下自己送出才算 `true`。
    pub auto_submitted: bool,
}

/// 從資料庫讀出的一筆點名送出紀錄。
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct RollcallEventRow {
    pub id: i64,
    pub timestamp: i64,
    pub account_id: String,
    pub username: String,
    pub user_name: String,
    pub rollcall_id: i64,
    pub course_name: String,
    pub attendance_type: String,
    /// SQLite 沒有 bool：0／1。
    pub success: i64,
    pub result: String,
    #[sqlx(default)]
    pub class_total: i64,
    #[sqlx(default)]
    pub class_signed: i64,
    /// ATT_STATS_AUTO_MANUAL_V1：0／1。舊資料（欄位新增前寫入）一律是 0（手動）。
    #[sqlx(default)]
    pub auto_submitted: i64,
}

impl RollcallEventRow {
    /// 送出者顯示名稱：學校姓名優先，其次學校帳號，最後才是面板帳號 ID。
    pub fn sender_label(&self) -> &str {
        let name = self.user_name.trim();
        if !name.is_empty() {
            return name;
        }
        let username = self.username.trim();
        if !username.is_empty() {
            return username;
        }
        self.account_id.trim()
    }

    pub fn is_success(&self) -> bool {
        self.success != 0
    }

    /// ATT_STATS_AUTO_MANUAL_V1：見 [`RollcallEventInput::auto_submitted`] 說明。
    pub fn is_auto_submitted(&self) -> bool {
        self.auto_submitted != 0
    }
}

/// 每門課的出席統計。
#[derive(Debug, Clone, Serialize, Default)]
pub struct RollcallCourseStat {
    pub course_name: String,
    pub total: i64,
    pub success: i64,
    pub failed: i64,
    /// 送出成功率（0.0–100.0，一位小數）。
    pub success_rate: f64,
    pub last_at: i64,
    /// 最近一次「成功送出」的時間（0＝這門課沒有成功紀錄）。
    pub last_success_at: i64,
    /// 最新一次班級預計點名總人數
    pub latest_class_total: i64,
    /// 最新一次班級實際點到名人數
    pub latest_class_signed: i64,
    /// 最新一次班級全班簽到率
    pub latest_class_rate: f64,
    /// 最新一次點名場次的詳細狀態（避免將歷史成功率誤認成本次）。
    pub latest_session: Option<RollcallLatestSession>,
    /// 這門課未點到名（失敗）的使用者名單。
    pub unattended_users: Vec<RollcallUnattendedUser>,
    /// 這門課各送出者的次數（多帳號時一眼看出是誰送的）。
    pub senders: Vec<RollcallSenderStat>,
    /// ATT_STATS_AUTO_MANUAL_V1：這段期間「系統自動送出」（無人按按鈕，班上
    /// 超過 15% 人點名成功觸發）的場次數。`auto_count + manual_count == total`。
    pub auto_count: i64,
    /// ATT_STATS_AUTO_MANUAL_V1：這段期間「有人動手」（輸入代碼／按自動讀取或
    /// 系統演算按鈕）送出的場次數。
    pub manual_count: i64,
}

/// 單門課最新一次點名場次的摘要資訊。
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct RollcallLatestSession {
    pub timestamp: i64,
    pub attendance_type: String,
    pub total: i64,
    pub success: i64,
    pub failed: i64,
    pub success_rate: f64,
    pub is_all_success: bool,
    /// 班級預計總點名人數（全班學生數）
    pub class_total: i64,
    /// 班級實際點到名人數（全班已簽到數）
    pub class_signed: i64,
    /// 班級全班簽到率（0.0 ~ 100.0%）
    pub class_rate: f64,
    /// 最新這一次未點到名（失敗）的使用者名單。
    pub unattended_users: Vec<RollcallUnattendedUser>,
    /// ATT_STATS_AUTO_MANUAL_V1：這一場（可能含多帳號）是不是系統自動送出。
    /// 只要有帳號是「成功且自動送出」就算 `true`；沒有任何成功紀錄時，
    /// 退而只要有任何一筆自動送出的嘗試就算 `true`（與 `class_total`/
    /// `class_signed` 採「這個群組裡取最大/有意義的那個值」同一個精神）。
    pub auto_submitted: bool,
}

/// 未點到名（簽到失敗）的使用者資訊。
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct RollcallUnattendedUser {
    pub account_id: String,
    pub username: String,
    pub user_name: String,
    /// 顯示用名稱（姓名 → 帳號 → 帳號 ID）。
    pub label: String,
    /// 失敗/未點到次數
    pub failed_count: i64,
    /// 最近一次失敗的原因/結果訊息
    pub last_result: String,
    /// 最近一次失敗的時間
    pub last_failed_at: i64,
}

/// 單一送出者的統計。
#[derive(Debug, Clone, Serialize, Default)]
pub struct RollcallSenderStat {
    pub account_id: String,
    pub username: String,
    pub user_name: String,
    /// 顯示用名稱（姓名 → 帳號 → 帳號 ID）。
    pub label: String,
    pub total: i64,
    pub success: i64,
}

/// 成功率：固定一位小數，避免前後端各自四捨五入造成數字對不上。
pub fn rollcall_success_rate(success: i64, total: i64) -> f64 {
    if total <= 0 {
        return 0.0;
    }
    ((success as f64) * 1000.0 / (total as f64)).round() / 10.0
}

impl AccountsDb {
    /// 寫入一筆點名送出紀錄。
    ///
    /// 統計是輔助資訊，呼叫端即使拿到 `Err` 也只會記一筆 warn，不影響點名流程本身。
    pub async fn record_rollcall_event(&self, input: RollcallEventInput) -> Result<()> {
        let account_id = truncate_event_text(input.account_id, ROLLCALL_IDENTITY_MAX_BYTES);
        let username = truncate_event_text(input.username, ROLLCALL_IDENTITY_MAX_BYTES);
        let user_name = truncate_event_text(input.user_name, ROLLCALL_IDENTITY_MAX_BYTES);
        let course_name = truncate_event_text(input.course_name, ROLLCALL_COURSE_MAX_BYTES);
        let attendance_type = truncate_event_text(input.attendance_type, ROLLCALL_TYPE_MAX_BYTES);
        let result = truncate_event_text(input.result, ROLLCALL_RESULT_MAX_BYTES);
        let timestamp = chrono::Local::now().timestamp();
        let rollcall_id = i64::try_from(input.rollcall_id).unwrap_or(i64::MAX);

        let inserted = sqlx::query(
            "INSERT INTO rollcall_events
                (timestamp, account_id, username, user_name, rollcall_id, course_name, attendance_type, success, result, class_total, class_signed, auto_submitted)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(timestamp)
        .bind(&account_id)
        .bind(&username)
        .bind(&user_name)
        .bind(rollcall_id)
        .bind(&course_name)
        .bind(&attendance_type)
        .bind(if input.success { 1i64 } else { 0i64 })
        .bind(&result)
        .bind(input.class_total)
        .bind(input.class_signed)
        .bind(if input.auto_submitted { 1i64 } else { 0i64 })
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("寫入 rollcall_events 失敗")?;

        // 保留窗清理：每 50 筆做一次（用 rowid 取模，不必額外狀態）。
        if inserted.last_insert_rowid() % ROLLCALL_EVENT_PRUNE_INTERVAL == 0 {
            let cutoff = timestamp - ROLLCALL_EVENT_RETENTION_DAYS * 86_400;
            let _ = sqlx::query("DELETE FROM rollcall_events WHERE timestamp < ?")
                .bind(cutoff)
                .execute(&self.pool)
                .await;
        }
        Ok(())
    }

    /// 讀出點名送出紀錄（新的在前）。`account_id` 為 `None` ＝ 全部帳號。
    pub async fn list_rollcall_events(
        &self,
        account_id: Option<&str>,
        since: i64,
        limit: i64,
    ) -> Result<Vec<RollcallEventRow>> {
        let limit = limit.clamp(1, 5000);
        let rows = if let Some(account_id) = account_id.map(str::trim).filter(|v| !v.is_empty()) {
            sqlx::query_as::<_, RollcallEventRow>(
                "SELECT id, timestamp, account_id, username, user_name, rollcall_id, course_name, attendance_type, success, result, class_total, class_signed, auto_submitted
                 FROM rollcall_events WHERE account_id = ? AND timestamp >= ?
                 ORDER BY timestamp DESC, id DESC LIMIT ?",
            )
            .bind(account_id)
            .bind(since)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as::<_, RollcallEventRow>(
                "SELECT id, timestamp, account_id, username, user_name, rollcall_id, course_name, attendance_type, success, result, class_total, class_signed, auto_submitted
                 FROM rollcall_events WHERE timestamp >= ?
                 ORDER BY timestamp DESC, id DESC LIMIT ?",
            )
            .bind(since)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
        }
        .into_diagnostic()
        .wrap_err("讀取 rollcall_events 失敗")?;
        Ok(rows)
    }
}

/// ATT_STATS_LATEST_SESSION_V1：每門課選取真正最新的一場點名（包含今天最新點名，絕不跳過）。
pub fn select_display_session_rows(
    rows: &[RollcallEventRow],
    _today_start: i64,
) -> Vec<RollcallEventRow> {
    use std::collections::HashMap;

    #[derive(Clone)]
    struct Session {
        max_ts: i64,
        rows: Vec<RollcallEventRow>,
    }

    let mut sessions: HashMap<(String, i64), Session> = HashMap::new();
    for row in rows {
        let key = if row.rollcall_id != 0 {
            (row.course_name.clone(), row.rollcall_id)
        } else {
            (row.course_name.clone(), row.timestamp / 1800)
        };
        let entry = sessions.entry(key).or_insert_with(|| Session {
            max_ts: row.timestamp,
            rows: Vec::new(),
        });
        entry.max_ts = entry.max_ts.max(row.timestamp);
        entry.rows.push(row.clone());
    }

    let mut by_course: HashMap<String, Vec<(i64, (String, i64))>> = HashMap::new();
    for (key, sess) in &sessions {
        by_course
            .entry(key.0.clone())
            .or_default()
            .push((sess.max_ts, key.clone()));
    }

    let mut out = Vec::new();
    for list in by_course.values_mut() {
        list.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.1.cmp(&a.1.1)));
        // 永遠選取每門課最新的一場（包含今天最新場次）
        if let Some(pick) = list.first() {
            if let Some(sess) = sessions.get(&pick.1) {
                out.extend(sess.rows.iter().cloned());
            }
        }
    }
    out.sort_by(|a, b| {
        b.timestamp
            .cmp(&a.timestamp)
            .then_with(|| b.id.cmp(&a.id))
    });
    out
}

/// 單一帳號在某場點名的整合狀態（場次去重：同一場點名不論逾時或重試幾次，永遠只算 1 筆點名）。
#[derive(Debug, Clone)]
pub struct ConsolidatedSessionOutcome {
    pub course_name: String,
    pub session_id: i64,
    pub account_id: String,
    pub username: String,
    pub user_name: String,
    pub attendance_type: String,
    pub success: bool,
    pub timestamp: i64,
    pub last_success_at: i64,
    pub last_failed_at: i64,
    pub last_failed_result: String,
    pub class_total: i64,
    pub class_signed: i64,
    /// ATT_STATS_AUTO_MANUAL_V1：見下方 `consolidate_sessions` 的取值說明。
    pub auto_submitted: bool,
}

impl ConsolidatedSessionOutcome {
    pub fn sender_label(&self) -> &str {
        let name = self.user_name.trim();
        if !name.is_empty() {
            return name;
        }
        let username = self.username.trim();
        if !username.is_empty() {
            return username;
        }
        self.account_id.trim()
    }
}

/// 將原始事件紀錄依 (course_name, session_id, account_id) 聚合為唯一點名場次結果。
///
/// 判定規則：
/// 1. 若該帳號在該場點名有至少一筆成功紀錄，則最終判定為「成功」，絕不列入未點到名單。
/// 2. 若該帳號在該場點名均為失敗（例如多次逾時重試），只計為「1 次失敗」，絕不因逾時重複計算。
pub fn consolidate_sessions(rows: &[RollcallEventRow]) -> Vec<ConsolidatedSessionOutcome> {
    use std::collections::HashMap;

    let mut map: HashMap<(String, i64, String), Vec<&RollcallEventRow>> = HashMap::new();

    for row in rows {
        let session_id = if row.rollcall_id != 0 {
            row.rollcall_id
        } else {
            row.timestamp / 1800
        };
        let key = (row.course_name.clone(), session_id, row.account_id.clone());
        map.entry(key).or_default().push(row);
    }

    let mut outcomes = Vec::with_capacity(map.len());
    for ((course_name, session_id, account_id), mut evs) in map {
        evs.sort_by_key(|r| (r.timestamp, r.id));
        let has_success = evs.iter().any(|r| r.is_success());
        let last_ev = evs.last().unwrap();
        let last_success_at = evs
            .iter()
            .filter(|r| r.is_success())
            .map(|r| r.timestamp)
            .max()
            .unwrap_or(0);
        let last_failed = evs.iter().filter(|r| !r.is_success()).last();
        let last_failed_at = last_failed.map(|r| r.timestamp).unwrap_or(0);
        let last_failed_result = last_failed.map(|r| r.result.clone()).unwrap_or_default();
        let class_total = evs.iter().map(|r| r.class_total).max().unwrap_or(0);
        let class_signed = evs.iter().map(|r| r.class_signed).max().unwrap_or(0);
        // ATT_STATS_AUTO_MANUAL_V1：同一場點名可能因逾時重試留下好幾筆原始紀錄
        // （例如先被系統自動送出失敗、使用者後來手動再送一次成功）。真正代表
        // 這場「最終算數的那一次」是「最後一筆成功紀錄」；完全沒有成功紀錄時
        // （整場都失敗）才退而使用最後一筆嘗試的旗標。
        let auto_submitted = evs
            .iter()
            .filter(|r| r.is_success())
            .max_by_key(|r| r.timestamp)
            .map(|r| r.is_auto_submitted())
            .unwrap_or_else(|| last_ev.is_auto_submitted());

        outcomes.push(ConsolidatedSessionOutcome {
            course_name,
            session_id,
            account_id,
            username: last_ev.username.clone(),
            user_name: last_ev.user_name.clone(),
            attendance_type: last_ev.attendance_type.clone(),
            success: has_success,
            timestamp: last_ev.timestamp,
            last_success_at,
            last_failed_at,
            last_failed_result,
            class_total,
            class_signed,
            auto_submitted,
        });
    }

    outcomes.sort_by(|a, b| {
        b.timestamp
            .cmp(&a.timestamp)
            .then_with(|| a.account_id.cmp(&b.account_id))
    });
    outcomes
}

/// 依課程聚合（純函式，不碰資料庫，方便測試）。
///
/// 回傳順序＝最近有點名的課在前面；同一門課內附上各送出者統計與最新一次點名場次摘要。
/// 支援同一場點名去重：同一場點名若有多次逾時或重試紀錄，永遠只算 1 筆點名。
pub fn aggregate_courses(rows: &[RollcallEventRow]) -> Vec<RollcallCourseStat> {
    let outcomes = consolidate_sessions(rows);
    let mut order: Vec<String> = Vec::new();
    let mut courses: Vec<RollcallCourseStat> = Vec::new();

    for out in &outcomes {
        let index = match order.iter().position(|name| name == &out.course_name) {
            Some(index) => index,
            None => {
                order.push(out.course_name.clone());
                courses.push(RollcallCourseStat {
                    course_name: out.course_name.clone(),
                    ..Default::default()
                });
                courses.len() - 1
            }
        };
        let course = &mut courses[index];
        course.total += 1;
        if out.success {
            course.success += 1;
            course.last_success_at = course.last_success_at.max(out.last_success_at);
        } else {
            course.failed += 1;
        }
        course.last_at = course.last_at.max(out.timestamp);
        // ATT_STATS_AUTO_MANUAL_V1：每場（去重後）點名都算進自動或手動其中一邊，
        // 所以 `auto_count + manual_count` 恆等於 `total`。
        if out.auto_submitted {
            course.auto_count += 1;
        } else {
            course.manual_count += 1;
        }
    }

    for course in &mut courses {
        course.success_rate = rollcall_success_rate(course.success, course.total);
        course.senders = senders_for_course_consolidated(&outcomes, &course.course_name);
        course.unattended_users = unattended_for_course_consolidated(&outcomes, &course.course_name);
        course.latest_session = extract_latest_session_consolidated(&outcomes, &course.course_name);
        course.latest_class_total = course.latest_session.as_ref().map(|ls| ls.class_total).unwrap_or(0);
        course.latest_class_signed = course.latest_session.as_ref().map(|ls| ls.class_signed).unwrap_or(0);
        course.latest_class_rate = course.latest_session.as_ref().map(|ls| ls.class_rate).unwrap_or(0.0);
    }
    courses.sort_by(|a, b| {
        b.last_at
            .cmp(&a.last_at)
            .then_with(|| a.course_name.cmp(&b.course_name))
    });
    courses
}

/// 提取某門課程真正最新一場點名的摘要狀態（區分歷史累積與最新一次，去重後每人算 1 筆）。
fn extract_latest_session_consolidated(
    outcomes: &[ConsolidatedSessionOutcome],
    course_name: &str,
) -> Option<RollcallLatestSession> {
    let latest = outcomes
        .iter()
        .filter(|o| o.course_name == course_name)
        .max_by_key(|o| (o.timestamp, o.session_id))?;

    let latest_ts = latest.timestamp;
    let latest_session_id = latest.session_id;
    let att_type = latest.attendance_type.clone();

    let mut total = 0i64;
    let mut success = 0i64;
    let mut unattended_users: Vec<RollcallUnattendedUser> = Vec::new();
    // ATT_STATS_AUTO_MANUAL_V1：這場可能有多個帳號（多帳號同時修同一門課），
    // 優先看「有成功且自動送出」的那幾筆；若這場全部失敗，退而看「有沒有任何
    // 自動送出的嘗試」——跟 class_total/class_signed 用 `.max()` 同一個精神：
    // 在同一個群組裡挑出最有參考價值的那個值。
    let mut any_success_auto = false;
    let mut any_auto = false;

    for out in outcomes
        .iter()
        .filter(|o| o.course_name == course_name && o.session_id == latest_session_id)
    {
        total += 1;
        if out.success {
            success += 1;
            if out.auto_submitted {
                any_success_auto = true;
            }
        } else {
            unattended_users.push(RollcallUnattendedUser {
                account_id: out.account_id.clone(),
                username: out.username.clone(),
                user_name: out.user_name.clone(),
                label: out.sender_label().to_string(),
                failed_count: 1,
                last_result: out.last_failed_result.clone(),
                last_failed_at: out.last_failed_at,
            });
        }
        if out.auto_submitted {
            any_auto = true;
        }
    }
    let auto_submitted = if success > 0 { any_success_auto } else { any_auto };

    unattended_users.sort_by(|a, b| a.label.cmp(&b.label));

    let failed = total - success;
    let rate = rollcall_success_rate(success, total);

    let class_total = outcomes
        .iter()
        .filter(|o| o.course_name == course_name && o.session_id == latest_session_id)
        .map(|o| o.class_total)
        .max()
        .unwrap_or(0);

    let class_signed = outcomes
        .iter()
        .filter(|o| o.course_name == course_name && o.session_id == latest_session_id)
        .map(|o| o.class_signed)
        .max()
        .unwrap_or(0);

    let class_rate = rollcall_success_rate(class_signed, class_total);

    Some(RollcallLatestSession {
        timestamp: latest_ts,
        attendance_type: att_type,
        total,
        success,
        failed,
        success_rate: rate,
        is_all_success: total > 0 && failed == 0,
        class_total,
        class_signed,
        class_rate,
        unattended_users,
        auto_submitted,
    })
}

/// 提取某門課程期間內所有未點到名（失敗）的使用者清單（同一場點名多次逾時只算 1 次，點到者不列入）。
fn unattended_for_course_consolidated(
    outcomes: &[ConsolidatedSessionOutcome],
    course_name: &str,
) -> Vec<RollcallUnattendedUser> {
    let mut list: Vec<RollcallUnattendedUser> = Vec::new();
    for out in outcomes.iter().filter(|o| o.course_name == course_name && !o.success) {
        match list.iter_mut().find(|u| u.account_id == out.account_id) {
            Some(entry) => {
                entry.failed_count += 1;
                if out.last_failed_at >= entry.last_failed_at {
                    entry.last_failed_at = out.last_failed_at;
                    entry.last_result = out.last_failed_result.clone();
                }
            }
            None => {
                list.push(RollcallUnattendedUser {
                    account_id: out.account_id.clone(),
                    username: out.username.clone(),
                    user_name: out.user_name.clone(),
                    label: out.sender_label().to_string(),
                    failed_count: 1,
                    last_result: out.last_failed_result.clone(),
                    last_failed_at: out.last_failed_at,
                });
            }
        }
    }
    list.sort_by(|a, b| {
        b.failed_count
            .cmp(&a.failed_count)
            .then_with(|| a.label.cmp(&b.label))
    });
    list
}

/// 全部送出者的統計（跨課程，去重後每場點名每人算 1 筆）。
pub fn aggregate_senders(rows: &[RollcallEventRow]) -> Vec<RollcallSenderStat> {
    let outcomes = consolidate_sessions(rows);
    let mut senders: Vec<RollcallSenderStat> = Vec::new();
    for out in &outcomes {
        push_sender_consolidated(&mut senders, out);
    }
    sort_senders(&mut senders);
    senders
}

fn senders_for_course_consolidated(
    outcomes: &[ConsolidatedSessionOutcome],
    course_name: &str,
) -> Vec<RollcallSenderStat> {
    let mut senders: Vec<RollcallSenderStat> = Vec::new();
    for out in outcomes.iter().filter(|o| o.course_name == course_name) {
        push_sender_consolidated(&mut senders, out);
    }
    sort_senders(&mut senders);
    senders
}

fn push_sender_consolidated(senders: &mut Vec<RollcallSenderStat>, out: &ConsolidatedSessionOutcome) {
    match senders.iter_mut().find(|s| s.account_id == out.account_id) {
        Some(entry) => {
            entry.total += 1;
            if out.success {
                entry.success += 1;
            }
        }
        None => senders.push(RollcallSenderStat {
            account_id: out.account_id.clone(),
            username: out.username.clone(),
            user_name: out.user_name.clone(),
            label: out.sender_label().to_string(),
            total: 1,
            success: if out.success { 1 } else { 0 },
        }),
    }
}

fn sort_senders(senders: &mut [RollcallSenderStat]) {
    senders.sort_by(|a, b| {
        b.total
            .cmp(&a.total)
            .then_with(|| a.label.cmp(&b.label))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(course: &str, success: bool, timestamp: i64, account: &str, name: &str) -> RollcallEventRow {
        RollcallEventRow {
            id: timestamp,
            timestamp,
            account_id: account.to_string(),
            username: format!("{account}-user"),
            user_name: name.to_string(),
            rollcall_id: 1,
            course_name: course.to_string(),
            attendance_type: "QR Code 簽到".to_string(),
            success: if success { 1 } else { 0 },
            result: String::new(),
            class_total: 0,
            class_signed: 0,
            auto_submitted: 0,
        }
    }

    /// ATT_STATS_AUTO_MANUAL_V1：跟 `row_id` 同一個精神，多帶一個 auto 旗標。
    fn row_id_auto(
        course: &str,
        success: bool,
        timestamp: i64,
        account: &str,
        name: &str,
        rid: i64,
        auto: bool,
    ) -> RollcallEventRow {
        let mut r = row_id(course, success, timestamp, account, name, rid);
        r.auto_submitted = if auto { 1 } else { 0 };
        r
    }

    #[test]
    fn success_rate_is_one_decimal() {
        assert_eq!(rollcall_success_rate(0, 0), 0.0);
        assert_eq!(rollcall_success_rate(1, 3), 33.3);
        assert_eq!(rollcall_success_rate(3, 3), 100.0);
    }

    #[test]
    fn aggregate_counts_and_senders_and_latest_session() {
        let rows = vec![
            row_id("資料結構", true, 300, "a", "王小明", 3),
            row_id("資料結構", false, 200, "b", "李小華", 2),
            row_id("資料結構", true, 100, "a", "王小明", 1),
            row_id("微積分", true, 150, "a", "王小明", 10),
        ];
        let courses = aggregate_courses(&rows);
        assert_eq!(courses.len(), 2);
        // 最近有點名的課排前面。
        assert_eq!(courses[0].course_name, "資料結構");
        assert_eq!(courses[0].total, 3);
        assert_eq!(courses[0].success, 2);
        assert_eq!(courses[0].failed, 1);
        assert_eq!(courses[0].success_rate, 66.7);
        assert_eq!(courses[0].last_at, 300);
        assert_eq!(courses[0].last_success_at, 300);
        assert_eq!(courses[0].senders.len(), 2);
        assert_eq!(courses[0].senders[0].label, "王小明");
        assert_eq!(courses[0].senders[0].total, 2);

        // 未點到名者名單
        assert_eq!(courses[0].unattended_users.len(), 1);
        assert_eq!(courses[0].unattended_users[0].label, "李小華");
        assert_eq!(courses[0].unattended_users[0].failed_count, 1);

        // 最新場次摘要
        assert!(courses[0].latest_session.is_some());
        let ls = courses[0].latest_session.as_ref().unwrap();
        assert_eq!(ls.timestamp, 300);

        let senders = aggregate_senders(&rows);
        assert_eq!(senders.len(), 2);
        assert_eq!(senders[0].label, "王小明");
        assert_eq!(senders[0].total, 3);
        assert_eq!(senders[0].success, 3);
    }

    #[test]
    fn dedupe_same_session_multiple_timeouts_and_retries() {
        let rows = vec![
            // 學生 A 在場次 99 逾時了 2 次 -> 只算 1 筆點名、1 次失敗
            row_id("體育", false, 100, "a", "學生A", 99),
            row_id("體育", false, 120, "a", "學生A", 99),
            // 學生 B 在場次 99 先逾時 1 次，隨後成功 -> 只算 1 筆點名、1 次成功、不列入未點到名單
            row_id("體育", false, 100, "b", "學生B", 99),
            row_id("體育", true, 110, "b", "學生B", 99),
            // 學生 C 在場次 99 直接成功 -> 1 筆成功
            row_id("體育", true, 105, "c", "學生C", 99),
        ];
        let courses = aggregate_courses(&rows);
        assert_eq!(courses.len(), 1);
        assert_eq!(courses[0].total, 3);
        assert_eq!(courses[0].success, 2);
        assert_eq!(courses[0].failed, 1);
        assert_eq!(courses[0].success_rate, 66.7);

        // 未點到名單只有學生 A (1次)，學生 B 成功了絕不出現在未點到名單
        assert_eq!(courses[0].unattended_users.len(), 1);
        assert_eq!(courses[0].unattended_users[0].label, "學生A");
        assert_eq!(courses[0].unattended_users[0].failed_count, 1);
    }

    /// ATT_STATS_AUTO_MANUAL_V1：自動／手動次數分別累計，且兩者加總等於 total；
    /// 「最新一場」的 auto_submitted 以該場「有成功且自動送出」的那筆為準。
    #[test]
    fn counts_auto_and_manual_submissions_separately() {
        let rows = vec![
            // 場次 1：手動輸入代碼成功
            row_id_auto("體育", true, 100, "a", "甲", 1, false),
            // 場次 2：系統因班上超過 15% 自動送出成功
            row_id_auto("體育", true, 200, "b", "乙", 2, true),
            // 場次 3：手動失敗（沒有成功紀錄，退回看最後一筆嘗試的旗標＝手動）
            row_id_auto("體育", false, 300, "c", "丙", 3, false),
        ];
        let courses = aggregate_courses(&rows);
        assert_eq!(courses.len(), 1);
        assert_eq!(courses[0].total, 3);
        assert_eq!(courses[0].auto_count, 1);
        assert_eq!(courses[0].manual_count, 2);
        assert_eq!(courses[0].auto_count + courses[0].manual_count, courses[0].total);

        // 最新一場（場次 3）全部失敗、唯一一筆嘗試是手動 -> 最新場次標記為手動
        let ls = courses[0].latest_session.as_ref().unwrap();
        assert_eq!(ls.timestamp, 300);
        assert!(!ls.auto_submitted);
    }

    /// 同一場點名先被系統自動送出失敗、使用者後來手動再送一次成功：
    /// 代表「最終算數」的是最後那筆成功紀錄（手動），不是先前失敗的自動嘗試。
    #[test]
    fn latest_success_wins_over_earlier_failed_auto_attempt() {
        let rows = vec![
            row_id_auto("體育", false, 100, "a", "甲", 1, true),
            row_id_auto("體育", true, 110, "a", "甲", 1, false),
        ];
        let courses = aggregate_courses(&rows);
        assert_eq!(courses.len(), 1);
        // 去重後只算 1 筆點名（同一場次同一帳號）
        assert_eq!(courses[0].total, 1);
        assert_eq!(courses[0].auto_count, 0);
        assert_eq!(courses[0].manual_count, 1);
        let ls = courses[0].latest_session.as_ref().unwrap();
        assert!(!ls.auto_submitted);
    }

    #[test]
    fn distinguishes_same_title_different_teacher_courses() {
        let rows = vec![
            // 體育 (王老師)
            row_id("體育 (王老師)", true, 100, "a", "甲", 1),
            // 體育 (李老師)
            row_id("體育 (李老師)", true, 120, "b", "乙", 2),
        ];
        let courses = aggregate_courses(&rows);
        assert_eq!(courses.len(), 2);
        assert_eq!(courses[0].course_name, "體育 (李老師)");
        assert_eq!(courses[1].course_name, "體育 (王老師)");
    }

    fn row_id(course: &str, success: bool, timestamp: i64, account: &str, name: &str, rid: i64) -> RollcallEventRow {
        let mut r = row(course, success, timestamp, account, name);
        r.rollcall_id = rid;
        r
    }

    #[test]
    fn select_keeps_latest_session_including_today() {
        let today = 1_700_000_000;
        let rows = vec![
            row_id("文學名篇", true, today + 100, "a", "甲", 20),
            row_id("文學名篇", false, today + 90, "b", "乙", 20),
            row_id("文學名篇", true, today - 86_400, "a", "甲", 10),
            row_id("文學名篇", true, today - 86_400 + 5, "c", "丙", 10),
        ];
        let out = select_display_session_rows(&rows, today);
        assert_eq!(out.len(), 2);
        // 永遠取真正最新這場（rollcall_id = 20）
        assert!(out.iter().all(|r| r.rollcall_id == 20));
    }

    #[test]
    fn select_keeps_latest_when_multiple_sessions_today() {
        let today = 1_700_000_000;
        let rows = vec![
            row_id("文學名篇", true, today + 200, "a", "甲", 21),
            row_id("文學名篇", true, today + 50, "a", "甲", 20),
            row_id("文學名篇", true, today - 86_400, "a", "甲", 10),
        ];
        let out = select_display_session_rows(&rows, today);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rollcall_id, 21);
    }

    #[test]
    fn select_keeps_today_if_only_session() {
        let today = 1_700_000_000;
        let rows = vec![row_id("生活與律", true, today + 10, "a", "甲", 5)];
        let out = select_display_session_rows(&rows, today);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rollcall_id, 5);
    }

    #[test]
    fn select_keeps_latest_not_today() {
        let today = 1_700_000_000;
        let rows = vec![
            row_id("資料結構", true, today - 86_400 * 10, "a", "甲", 1),
            row_id("資料結構", true, today - 86_400 * 5, "a", "甲", 2),
        ];
        let out = select_display_session_rows(&rows, today);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rollcall_id, 2);
    }

    #[test]
    fn sender_label_falls_back_to_username() {
        let mut r = row("課", true, 1, "a", "");
        assert_eq!(r.sender_label(), "a-user");
        r.username = String::new();
        assert_eq!(r.sender_label(), "a");
    }
}
