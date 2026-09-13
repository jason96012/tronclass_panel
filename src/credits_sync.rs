//! 學分／成績自動同步排程器（CREDITS_AUTO_SYNC_V1）。
//!
//! ## 功能
//! 1. **成績公布推播**（CREDITS_GRADE_PUSH_V1）：每帳號**每月隨機挑一天**
//!    自動同步 emis 學分頁（同步日由 `(帳號, 年月)` 決定性偽隨機決定，
//!    見 `monthly_sync_day`），與前次快照 diff；偵測到新成績時透過 web push
//!    推播該帳號。首次同步（無前次快照）只建立基準、不推播，避免一次
//!    灌爆整份歷年成績。
//! 2. **學校改版告警**（CREDITS_SYNC_ALERT_V1）：同步日當天一輪內所有帳號
//!    皆失敗、連續 `FAIL_ALERT_THRESHOLD` 次（6 小時一輪，即半天後）→
//!    推播管理員；任何一次成功即重置。學校改版時解析會全面失敗，
//!    比單一帳號的失敗更早暴露問題。
//!
//! ## 帳號範圍與設計
//! 跟隨課表自動同步的啟用清單（`timetable_sync_state.enabled`）且
//! provider = tku（`fetch_credit_report` 僅支援淡江 emis）——同一個校務帳號，
//! 沒有理由開了課表同步卻不要成績同步。時段避讓、防重入
//! （`MissedTickBehavior::Delay`）與帳號間隔皆沿用 `timetable_sync` 的模式；
//! 同樣是獨立 `tokio::spawn`，爬蟲逾時不影響其他功能。

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{Datelike, Local};
use tracing::{debug, info, warn};

use crate::account::AccountConfig;
use crate::db::AccountsDb;
use crate::timetable_source::tku_credits::{fetch_credit_report, CreditCourseRow, CreditReport};

/// 檢查間隔：與課表同步相同，6 小時醒來看一次誰該同步。
const TICK_INTERVAL_SECS: u64 = 6 * 60 * 60;
/// 啟動延遲：避開服務啟動高峰（與課表同步相同）。
const STARTUP_DELAY_SECS: u64 = 120;
/// 單次抓取逾時（與手動同步 API 的 CREDITS_SCRAPE_TIMEOUT_SECS 相同）。
const SCRAPE_TIMEOUT_SECS: u64 = 90;
/// 帳號間隔：避免連續登入撞學校系統。
const ACCOUNT_GAP_SECS: u64 = 8;
/// 全輪失敗連續幾輪後告警管理員（3 輪 × 6 小時 = 18 小時）。
const FAIL_ALERT_THRESHOLD: u32 = 3;
/// 推播內文最多列出的新成績筆數。
const MAX_LISTED_GRADES: usize = 5;
/// 告警內文裡最後錯誤的長度上限。
const MAX_ERROR_CHARS: usize = 200;

/// 跨輪失敗狀態（scheduler task 私有，重啟歸零）。
#[derive(Default)]
struct FailState {
    streak: u32,
    alerted: bool,
    last_error: String,
}

/// 一筆新成績（diff 結果；成績字串原樣呈現，例如 "87"、"*43"、"通過"）。
#[derive(Debug, Clone, PartialEq)]
pub struct NewGrade {
    pub name: String,
    pub grade: String,
    pub credits: u32,
}

/// 啟動學分／成績自動同步排程器（冪等；由 main 呼叫一次）。
/// 簽名與 `timetable_sync::start_timetable_sync_scheduler` 相同：直接收
/// `AccountsDb`（內部為 sqlx pool，move 進 task 即可，不需要外層 Arc）。
pub fn start_credits_sync_scheduler(
    accounts_db: AccountsDb,
    app_config: Arc<crate::config::AppConfig>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("學分／成績自動同步排程器已啟動（每 6 小時檢查，每帳號每月隨機一天同步）");
        tokio::time::sleep(Duration::from_secs(STARTUP_DELAY_SECS)).await;

        let mut ticker = tokio::time::interval(Duration::from_secs(TICK_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut fail = FailState::default();
        loop {
            ticker.tick().await;
            let (attempted, succeeded) = run_once(&accounts_db, &app_config, &mut fail).await;
            if attempted > 0 && succeeded == 0 {
                fail.streak += 1;
                warn!(streak = fail.streak, "學分同步：本輪全部帳號失敗");
                if fail.streak >= FAIL_ALERT_THRESHOLD && !fail.alerted {
                    fail.alerted = true;
                    alert_admins(&accounts_db, &fail).await;
                }
            } else if succeeded > 0 {
                if fail.alerted {
                    info!("學分同步：已恢復正常，重置告警狀態");
                }
                fail.streak = 0;
                fail.alerted = false;
                fail.last_error.clear();
            }
        }
    })
}

/// 一輪檢查：回傳 (嘗試帳號數, 成功帳號數)。失敗明細寫入 `fail.last_error`。
async fn run_once(
    accounts_db: &AccountsDb,
    app_config: &crate::config::AppConfig,
    fail: &mut FailState,
) -> (usize, usize) {
    let states = match accounts_db.list_enabled_timetable_sync_states().await {
        Ok(s) => s,
        Err(e) => {
            debug!(error = %e, "學分同步：讀取啟用清單失敗，略過本輪");
            return (0, 0);
        }
    };
    if states.is_empty() {
        return (0, 0);
    }
    let accounts = match accounts_db.resolve(app_config).await {
        Ok(a) => a,
        Err(e) => {
            debug!(error = %e, "學分同步：讀取帳號設定失敗，略過本輪");
            return (0, 0);
        }
    };
    if crate::timetable_sync::in_blackout_window(Local::now()) {
        debug!("學分同步：學校系統維護／資料更新時段，略過本輪");
        return (0, 0);
    }

    let now = Local::now();
    let (mut attempted, mut succeeded) = (0usize, 0usize);

    for state in states {
        let Some(account) = accounts.iter().find(|a| a.id == state.account_id) else {
            continue;
        };
        if !account.enabled || account.provider != "tku" {
            continue;
        }

        // 每月隨機一天：只在當月同步日執行；同日失敗後由下一個 tick（6 小時後）重試。
        if monthly_sync_day(&account.id, now.year(), now.month()) != now.day() {
            continue;
        }

        // 本月已同步（自動或手動，快照 fetched_at 落在本月）→ 等下個月。
        // 舊快照同時作為 diff 基準。
        let old = match accounts_db.get_credit_report(&account.id).await {
            Ok(Some((json, fetched_at))) => {
                if same_month(fetched_at, &now) {
                    continue;
                }
                match serde_json::from_str::<CreditReport>(&json) {
                    Ok(r) => Some(r),
                    Err(e) => {
                        debug!(
                            account = %account.id,
                            error = %e,
                            "學分同步：舊快照解析失敗，視為首次建立基準"
                        );
                        None
                    }
                }
            }
            Ok(None) => None,
            Err(e) => {
                warn!(account = %account.id, error = %e, "學分同步：讀取既有快照失敗，仍嘗試同步");
                None
            }
        };

        attempted += 1;
        match sync_one(accounts_db, account, old.as_ref()).await {
            Ok(Some(new_grades)) => {
                succeeded += 1;
                push_new_grades(&account.id, &new_grades).await;
            }
            Ok(None) => {
                succeeded += 1;
            }
            Err(e) => {
                fail.last_error = e.clone();
                warn!(account = %account.id, error = %e, "學分自動同步失敗");
            }
        }

        tokio::time::sleep(Duration::from_secs(ACCOUNT_GAP_SECS)).await;
    }

    (attempted, succeeded)
}

/// 對單一帳號同步一次：抓取 → 與舊快照 diff → 寫入新快照。
/// 回傳 `Ok(Some(new_grades))`＝有新成績；`Ok(None)`＝無變化或首次建立基準。
async fn sync_one(
    accounts_db: &AccountsDb,
    account: &AccountConfig,
    old: Option<&CreditReport>,
) -> Result<Option<Vec<NewGrade>>, String> {
    // 全新 cookie jar＋獨立 client：與手動同步、課表同步相同的 session 隔離。
    let cookie_jar = Arc::new(reqwest::cookie::Jar::default());
    let client = crate::api::build_http_client(Arc::clone(&cookie_jar), SCRAPE_TIMEOUT_SECS)
        .map_err(|e| format!("建立學分查詢 HTTP client 失敗：{e}"))?;

    let fetch = fetch_credit_report(&client, &cookie_jar, account);
    let report = match tokio::time::timeout(Duration::from_secs(SCRAPE_TIMEOUT_SECS), fetch).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(format!("{e}")),
        Err(_) => return Err(format!("學分查詢逾時（{SCRAPE_TIMEOUT_SECS}s）")),
    };

    // 合理性檢查：三頁全空代表流程或版面有問題，不寫入快照（與手動同步相同）。
    if report.courses.is_empty() && report.required_courses.is_empty() {
        return Err("未從校務系統解析到任何學分資料（疑似改版）".to_string());
    }

    let new_grades = match old {
        Some(old_report) => diff_new_grades(old_report, &report),
        None => Vec::new(), // 首次同步：只建立基準，不推播。
    };
    let has_new = !new_grades.is_empty();

    let report_json = serde_json::to_string(&report)
        .map_err(|e| format!("序列化學分報告失敗：{e}"))?;
    let fetched_at = Local::now().timestamp();
    accounts_db
        .upsert_credit_report(&account.id, &report_json, fetched_at)
        .await
        .map_err(|e| format!("儲存學分報告失敗：{e}"))?;

    info!(
        account = %account.id,
        courses = report.courses.len(),
        new_grades = new_grades.len(),
        "學分自動同步完成：{}（{} 科，{} 筆新成績）",
        report.student.name,
        report.courses.len(),
        new_grades.len()
    );

    Ok(if has_new { Some(new_grades) } else { None })
}

/// FNV-1a：跨版本／平台確定的簡單雜湊。只用來決定每月同步日，無安全需求。
fn fnv1a(input: &str) -> u64 {
    let mut hash: u64 = 0xcbf_29ce_4842_2235;
    for b in input.bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 帳號在指定年月的同步日（1..=28；避開 29–31 與 2 月尾巴）。
/// 以 `(帳號, 年月)` 決定性偽隨機：重啟不變、免儲存、跨月自然輪替。
fn monthly_sync_day(account_id: &str, year: i32, month: u32) -> u32 {
    let key = format!("{account_id}|{year:04}-{month:02}");
    (fnv1a(&key) % 28) as u32 + 1
}

/// 時間戳是否落在指定的本地年月（判斷「本月是否已同步」）。
fn same_month(ts: i64, now: &chrono::DateTime<Local>) -> bool {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| {
            let local = dt.with_timezone(&Local);
            local.year() == now.year() && local.month() == now.month()
        })
        .unwrap_or(false)
}

/// 成績列的唯一鍵：(學年, 學期, 科號, 學期序)。年課上下學期各算一筆。
fn course_key(c: &CreditCourseRow) -> String {
    format!("{}|{}|{}|{}", c.yr, c.sem, c.course_code, c.term_seq)
}

/// 新舊報告 diff：新報告中出現、舊報告沒有的成績列。
/// 同鍵但成績改變（補考改分等）不算新成績，避免重複打擾。
pub fn diff_new_grades(old: &CreditReport, new: &CreditReport) -> Vec<NewGrade> {
    let old_keys: HashSet<String> = old.courses.iter().map(course_key).collect();
    new.courses
        .iter()
        .filter(|c| !old_keys.contains(&course_key(c)))
        .map(|c| NewGrade {
            name: c.name.clone(),
            grade: c.grade.clone(),
            credits: c.credits,
        })
        .collect()
}

/// 推播新成績給帳號（web push；訂閱數由 send_digest 回報）。
async fn push_new_grades(account_id: &str, grades: &[NewGrade]) {
    let n = grades.len();
    let mut lines: Vec<String> = grades
        .iter()
        .take(MAX_LISTED_GRADES)
        .map(|g| format!("• {}：{}（{} 學分）", g.name, g.grade, g.credits))
        .collect();
    if n > MAX_LISTED_GRADES {
        lines.push(format!("…等共 {n} 筆"));
    }
    let title = format!("🎓 成績公布：新增 {n} 筆成績");
    let body = lines.join("\n");
    match crate::web_push_notifications::send_digest(account_id, &title, &body).await {
        Ok(sent) => info!(account = %account_id, sent = sent, "成績公布推播完成"),
        Err(e) => warn!(account = %account_id, error = %e, "成績公布推播失敗"),
    }
}

/// 全輪連續失敗告警：推送所有管理員（與 monitor_watchdog 同一來源）。
async fn alert_admins(accounts_db: &AccountsDb, fail: &FailState) {
    let admins: Vec<String> = match accounts_db.list().await {
        Ok(accounts) => accounts
            .into_iter()
            .filter(|a| a.is_admin)
            .map(|a| a.id)
            .collect(),
        Err(e) => {
            warn!(error = %e, "學分同步告警：讀取帳號清單失敗");
            Vec::new()
        }
    };
    if admins.is_empty() {
        return;
    }
    let last_error: String = fail.last_error.chars().take(MAX_ERROR_CHARS).collect();
    let title = "⚠️ 學分同步連續失敗（學校系統可能改版）".to_string();
    let body = format!(
        "本月同步日的學分同步已連續失敗 {} 次（每次間隔 6 小時），最後錯誤：{}。請到 /admin/credits 手動同步查看詳細訊息；若確認學校系統改版，請更新解析器（下次自動同步為下個月）。",
        fail.streak, last_error
    );
    crate::web_push_notifications::notify_admin_alert(admins, title, body, "credits-sync-fail".to_string());
    warn!(streak = fail.streak, "學分同步：已發送管理員告警");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn course(yr: &str, sem: &str, code: &str, seq: &str, name: &str, grade: &str, credits: u32) -> CreditCourseRow {
        CreditCourseRow {
            yr: yr.into(),
            sem: sem.into(),
            dept_code: String::new(),
            dept_name: String::new(),
            course_code: code.into(),
            name: name.into(),
            is_english: false,
            term_seq: seq.into(),
            group_code: String::new(),
            required: true,
            credits,
            grade: grade.into(),
            grade_value: None,
            passed: true,
        }
    }

    fn report(courses: Vec<CreditCourseRow>) -> CreditReport {
        CreditReport {
            student: Default::default(),
            graduation: Default::default(),
            earned_total: None,
            courses,
            required_courses: Vec::new(),
            ge_domains: Vec::new(),
        }
    }

    #[test]
    fn fnv1a_known_vector() {
        // FNV-1a 64-bit reference vector；鎖定實作避免未來被誤改。
        assert_eq!(fnv1a("a"), 0xaf63dc4c8601ec8c);
        assert_eq!(fnv1a(""), 0xcbf29ce484222325);
    }

    #[test]
    fn monthly_sync_day_within_1_to_28() {
        for year in [2026i32, 2027] {
            for month in 1u32..=12 {
                for id in ["tku-main", "backup", "x"] {
                    let d = monthly_sync_day(id, year, month);
                    assert!((1..=28).contains(&d), "{id} {year}-{month} 得到 {d}");
                }
            }
        }
    }

    #[test]
    fn monthly_sync_day_is_deterministic_and_rotates() {
        // 同輸入必須同輸出（重啟不變）。
        assert_eq!(
            monthly_sync_day("tku-main", 2026, 9),
            monthly_sync_day("tku-main", 2026, 9)
        );
        // 24 個月內應出現不只一個同步日（28^-23 的巧合在實務上不可能；
        // 若真的全同，代表雜湊使用方式有誤，測試會抓出來）。
        let days: std::collections::HashSet<u32> = (0..24)
            .map(|i| monthly_sync_day("tku-main", 2026 + i / 12, i % 12 + 1))
            .collect();
        assert!(days.len() >= 2, "24 個月的同步日全相同：{days:?}");
    }

    #[test]
    fn same_month_checks_local_calendar_month() {
        let now = Local::now();
        let this_month_start = now
            .with_day(1)
            .and_then(|d| d.with_hour(0))
            .and_then(|d| d.with_minute(0))
            .and_then(|d| d.with_second(0))
            .expect("時間重置");
        assert!(same_month(this_month_start.timestamp(), &now));
        let last_month_end = this_month_start.timestamp() - 60;
        assert!(!same_month(last_month_end, &now));
        assert!(!same_month(0, &now)); // 1970-01（極舊資料）
    }

    #[test]
    fn diff_same_report_is_empty() {
        let r = report(vec![course("114", "1", "A1376", "0", "中國語文能力表達", "87", 2)]);
        assert!(diff_new_grades(&r, &r).is_empty());
    }

    #[test]
    fn diff_detects_new_course() {
        let old = report(vec![course("114", "1", "A1376", "0", "中國語文能力表達", "87", 2)]);
        let new = report(vec![
            course("114", "1", "A1376", "0", "中國語文能力表達", "87", 2),
            course("115", "1", "T3160", "0", "資料結構", "76", 3),
        ]);
        let grades = diff_new_grades(&old, &new);
        assert_eq!(grades.len(), 1);
        assert_eq!(grades[0].name, "資料結構");
        assert_eq!(grades[0].grade, "76");
        assert_eq!(grades[0].credits, 3);
    }

    #[test]
    fn diff_year_course_terms_are_separate_keys() {
        // 年課（T0466）上下學期各一筆：只新增下學期那一筆時，只推播一筆。
        let old = report(vec![course("114", "1", "T0466", "1", "英文（一）", "70", 2)]);
        let new = report(vec![
            course("114", "1", "T0466", "1", "英文（一）", "70", 2),
            course("114", "2", "T0466", "2", "英文（一）", "73", 2),
        ]);
        let grades = diff_new_grades(&old, &new);
        assert_eq!(grades.len(), 1);
        assert_eq!(grades[0].grade, "73");
    }

    #[test]
    fn diff_grade_change_same_key_is_not_new() {
        // 同鍵成績改變（例：重複抓取、成績修正）不推播。
        let old = report(vec![course("114", "1", "A1376", "0", "中國語文能力表達", "87", 2)]);
        let new = report(vec![course("114", "1", "A1376", "0", "中國語文能力表達", "90", 2)]);
        assert!(diff_new_grades(&old, &new).is_empty());
    }

    #[test]
    fn diff_failed_grade_still_notified() {
        // 不及格（*43）也是新成績：學生需要知道。
        let old = report(vec![]);
        let new = report(vec![course("114", "1", "T3154", "1", "社會議題探索暨實踐", "*43", 1)]);
        let grades = diff_new_grades(&old, &new);
        assert_eq!(grades.len(), 1);
        assert_eq!(grades[0].grade, "*43");
    }

    #[test]
    fn snapshot_roundtrip_with_legacy_json() {
        // 舊快照（fix7 之前無 is_english 欄位）必須能反序列化：serde(default) 生效。
        let json = r#"{"student":{"dept":"資工英語一","name":"高浚祐","student_no":"414850098"},
            "graduation":{"audit_year":114,"total_credits":128},
            "courses":[{"yr":"114","sem":"1","dept_code":"","dept_name":"資工英語一",
            "course_code":"A1376","name":"中國語文能力表達","term_seq":"0","group_code":"",
            "required":true,"credits":2,"grade":"87","grade_value":87,"passed":true}],
            "required_courses":[],"ge_domains":[]}"#;
        let parsed: CreditReport = serde_json::from_str(json).expect("舊快照應可解析");
        assert_eq!(parsed.courses.len(), 1);
        assert!(!parsed.courses[0].is_english); // serde(default) → false
        assert_eq!(parsed.student.name, "高浚祐");
    }
}
