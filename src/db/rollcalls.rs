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
    /// 這門課各送出者的次數（多帳號時一眼看出是誰送的）。
    pub senders: Vec<RollcallSenderStat>,
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
                (timestamp, account_id, username, user_name, rollcall_id, course_name, attendance_type, success, result)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
                "SELECT id, timestamp, account_id, username, user_name, rollcall_id, course_name, attendance_type, success, result
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
                "SELECT id, timestamp, account_id, username, user_name, rollcall_id, course_name, attendance_type, success, result
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

/// ATT_STATS_ONE_SESSION_V1：出席統計每門課只顯示一場點名。
///
/// 先依 `rollcall_id` 分場（沒有 id 則依當地日），每門課取最新一場；
/// 若最新一場落在「今天」而且還有更早的場次，改取上一場。
/// 進行中的今天點名不會把上週已完成的成功率蓋掉。
pub fn select_display_session_rows(
    rows: &[RollcallEventRow],
    today_start: i64,
) -> Vec<RollcallEventRow> {
    use std::collections::HashMap;

    fn day_start(ts: i64, today_start: i64) -> i64 {
        if ts >= today_start {
            today_start
        } else {
            let days_ago = (today_start.saturating_sub(ts).saturating_sub(1)) / 86_400 + 1;
            today_start - days_ago * 86_400
        }
    }

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
            (row.course_name.clone(), day_start(row.timestamp, today_start))
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
        // 有「今天以前」的場次就用最近的那一場；否則才顯示今天。
        let pick = list
            .iter()
            .find(|item| item.0 < today_start)
            .unwrap_or(&list[0]);
        if let Some(sess) = sessions.get(&pick.1) {
            out.extend(sess.rows.iter().cloned());
        }
    }
    out.sort_by(|a, b| {
        b.timestamp
            .cmp(&a.timestamp)
            .then_with(|| b.id.cmp(&a.id))
    });
    out
}

/// 依課程聚合（純函式，不碰資料庫，方便測試）。
///
/// 回傳順序＝最近有點名的課在前面；同一門課內附上各送出者統計。
pub fn aggregate_courses(rows: &[RollcallEventRow]) -> Vec<RollcallCourseStat> {
    let mut order: Vec<String> = Vec::new();
    let mut courses: Vec<RollcallCourseStat> = Vec::new();

    for row in rows {
        let index = match order.iter().position(|name| name == &row.course_name) {
            Some(index) => index,
            None => {
                order.push(row.course_name.clone());
                courses.push(RollcallCourseStat {
                    course_name: row.course_name.clone(),
                    ..Default::default()
                });
                courses.len() - 1
            }
        };
        let course = &mut courses[index];
        course.total += 1;
        if row.is_success() {
            course.success += 1;
            course.last_success_at = course.last_success_at.max(row.timestamp);
        } else {
            course.failed += 1;
        }
        course.last_at = course.last_at.max(row.timestamp);
    }

    for course in &mut courses {
        course.success_rate = rollcall_success_rate(course.success, course.total);
        course.senders = senders_for_course(rows, &course.course_name);
    }
    courses.sort_by(|a, b| {
        b.last_at
            .cmp(&a.last_at)
            .then_with(|| a.course_name.cmp(&b.course_name))
    });
    courses
}

/// 全部送出者的統計（跨課程）。
pub fn aggregate_senders(rows: &[RollcallEventRow]) -> Vec<RollcallSenderStat> {
    let mut senders: Vec<RollcallSenderStat> = Vec::new();
    for row in rows {
        push_sender(&mut senders, row);
    }
    sort_senders(&mut senders);
    senders
}

fn senders_for_course(rows: &[RollcallEventRow], course_name: &str) -> Vec<RollcallSenderStat> {
    let mut senders: Vec<RollcallSenderStat> = Vec::new();
    for row in rows.iter().filter(|row| row.course_name == course_name) {
        push_sender(&mut senders, row);
    }
    sort_senders(&mut senders);
    senders
}

fn push_sender(senders: &mut Vec<RollcallSenderStat>, row: &RollcallEventRow) {
    match senders.iter_mut().find(|s| s.account_id == row.account_id) {
        Some(entry) => {
            entry.total += 1;
            if row.is_success() {
                entry.success += 1;
            }
        }
        None => senders.push(RollcallSenderStat {
            account_id: row.account_id.clone(),
            username: row.username.clone(),
            user_name: row.user_name.clone(),
            label: row.sender_label().to_string(),
            total: 1,
            success: if row.is_success() { 1 } else { 0 },
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
        }
    }

    #[test]
    fn success_rate_is_one_decimal() {
        assert_eq!(rollcall_success_rate(0, 0), 0.0);
        assert_eq!(rollcall_success_rate(1, 3), 33.3);
        assert_eq!(rollcall_success_rate(3, 3), 100.0);
    }

    #[test]
    fn aggregate_counts_and_senders() {
        let rows = vec![
            row("資料結構", true, 300, "a", "王小明"),
            row("資料結構", false, 200, "b", "李小華"),
            row("資料結構", true, 100, "a", "王小明"),
            row("微積分", true, 150, "a", "王小明"),
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

        let senders = aggregate_senders(&rows);
        assert_eq!(senders.len(), 2);
        assert_eq!(senders[0].label, "王小明");
        assert_eq!(senders[0].total, 3);
        assert_eq!(senders[0].success, 3);
    }

    fn row_id(course: &str, success: bool, timestamp: i64, account: &str, name: &str, rid: i64) -> RollcallEventRow {
        let mut r = row(course, success, timestamp, account, name);
        r.rollcall_id = rid;
        r
    }

    #[test]
    fn select_skips_today_when_previous_exists() {
        let today = 1_700_000_000;
        let rows = vec![
            row_id("文學名篇", true, today + 100, "a", "甲", 20),
            row_id("文學名篇", false, today + 90, "b", "乙", 20),
            row_id("文學名篇", true, today - 86_400, "a", "甲", 10),
            row_id("文學名篇", true, today - 86_400 + 5, "c", "丙", 10),
        ];
        let out = select_display_session_rows(&rows, today);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|r| r.rollcall_id == 10));
    }

    #[test]
    fn select_skips_all_today_sessions() {
        let today = 1_700_000_000;
        let rows = vec![
            row_id("文學名篇", true, today + 200, "a", "甲", 21),
            row_id("文學名篇", true, today + 50, "a", "甲", 20),
            row_id("文學名篇", true, today - 86_400, "a", "甲", 10),
        ];
        let out = select_display_session_rows(&rows, today);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rollcall_id, 10);
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
