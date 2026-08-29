//! 課表資料來源抽象層（TIMETABLE_AUTO_SYNC_V1）。
//!
//! Tronclass **沒有**課表 API：課程是由學校校務系統推進 Tronclass 的，
//! `/api/my-courses` 只有選課清單（學年、課號、起訖日期），沒有星期／節次。
//! 因此「自動課表」必須回到學校自己的校務系統去取。
//!
//! 本模組的結構刻意對齊 `src/auth/providers/`：一個 trait + 一個 registry。
//! 但有一個關鍵差異——
//!
//! ```text
//! auth::providers::get_auth_flow()  →  Box<dyn AuthFlow>     （不支援時 panic!）
//! timetable_source::get_source()    →  Option<Box<dyn ...>>  （不支援時 None）
//! ```
//!
//! 認證流程是「沒有就無法運作」的必要條件，panic 尚可接受；
//! 課表同步是**加值功能**，某間學校沒有實作絕不能讓整個服務崩潰。
//! 所以這裡回傳 `Option`，呼叫端安靜跳過即可。

use std::sync::Arc;

use async_trait::async_trait;
use miette::Result;
use reqwest::{cookie::Jar, Client};

use crate::account::AccountConfig;
use crate::config::ProviderKind;

pub mod tku;

/// 從學校系統爬回來的單筆「課程 × 節次」原始資料。
///
/// 這是 scraper 與資料庫之間的中介型別。刻意不直接產生
/// `db::TimetableEntryInput`，因為：
/// 1. 節次 → 時間的換算屬於 provider 知識（各校節次時間不同）；
/// 2. 保留原始 `periods` 讓連續節次可以被合併成單一列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTimetableRow {
    /// 已正規化的星期（`mon`..`sun`）。
    pub weekday: String,
    /// 該課在這天佔用的節次代碼，已排序（例：`[3, 4]`）。
    pub periods: Vec<i64>,
    pub course_name: String,
    pub teacher: String,
    pub classroom: String,
    pub course_code: String,
}

/// 一間學校的課表抓取實作。
#[async_trait]
pub trait TimetableSource: Send + Sync {
    /// 這個來源的可讀名稱，用於日誌與前端顯示。
    fn source_name(&self) -> &'static str;

    /// 登入學校系統並抓回課表。
    ///
    /// 實作必須自行完成登入；傳入的 `client`/`cookie_jar` 是**專用**於本次
    /// 同步的（呼叫端每次都建立全新的 jar），因此實作不必擔心污染
    /// Tronclass 主 session，也不該假設 jar 裡已有任何 cookie。
    async fn fetch(
        &self,
        client: &Client,
        cookie_jar: &Arc<Jar>,
        account: &AccountConfig,
    ) -> Result<Vec<RawTimetableRow>>;
}

/// 依帳號設定取得對應的課表來源；不支援的學校回傳 `None`。
///
/// 分派邏輯與 `get_auth_flow()` 一致：先看 `provider` 字串，再退回
/// `ProviderKind`，讓使用者把 provider 命名成 `tamkang` 之類也能運作。
pub fn get_timetable_source(account: &AccountConfig) -> Option<Box<dyn TimetableSource>> {
    match account.provider.as_str() {
        "tku" => return Some(tku::create_source()),
        // FJU 的校務系統需要圖形驗證碼 OCR，尚未實作；明確回 None 而非 panic。
        "fju" => return None,
        _ => {}
    }

    match &account.provider_config.kind {
        ProviderKind::TKU => Some(tku::create_source()),
        ProviderKind::FJU | ProviderKind::Tronclass => None,
    }
}

/// 把節次清單壓縮成連續區間，例如 `[3,4,5,7]` → `[(3,5), (7,7)]`。
///
/// 校務系統是以「每節一格」的方式呈現課表，同一堂課會橫跨多格。
/// 資料庫的 `timetable_entries` 則用 `period_start`/`period_end` 表示區間，
/// 因此需要在這裡合併，否則一堂 2 小時的課會變成兩列、上課提醒也會響兩次。
pub fn compress_periods(periods: &[i64]) -> Vec<(i64, i64)> {
    let mut sorted: Vec<i64> = periods.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut ranges: Vec<(i64, i64)> = Vec::new();
    for period in sorted {
        match ranges.last_mut() {
            Some(last) if period == last.1 + 1 => last.1 = period,
            _ => ranges.push((period, period)),
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compress_periods_merges_consecutive_runs() {
        assert_eq!(compress_periods(&[3, 4, 5]), vec![(3, 5)]);
        assert_eq!(compress_periods(&[3, 4, 7]), vec![(3, 4), (7, 7)]);
        assert_eq!(compress_periods(&[1]), vec![(1, 1)]);
    }

    #[test]
    fn compress_periods_sorts_and_dedups() {
        // 校務系統列出的順序不保證遞增，重複格子也可能出現。
        assert_eq!(compress_periods(&[5, 3, 4, 4]), vec![(3, 5)]);
    }

    #[test]
    fn compress_periods_handles_empty() {
        assert!(compress_periods(&[]).is_empty());
    }
}
