//! 淡江大學（TKU）課表來源：校務行政資訊查詢系統 emis。
//!
//! TKU_EMIS_TIMETABLE_V1
//!
//! 目標頁面：
//! ```text
//! https://sso.tku.edu.tw/aissinfo/emis/TMWC090_result.aspx?YrSem=<學年期>&stu_no=<學號>
//! ```
//! 即 emis「在學學生 → 選課資料(依上課星期、節次列表)」。
//!
//! 三個關鍵事實（決定了本模組的實作方式）：
//!
//! 1. **與 iClass 同一套 SSO。** 未登入時 emis 會 302 到
//!    `sso.tku.edu.tw/NEAI/logineb.jsp?myurl=<原始網址>`，正是
//!    `auth::providers::tku` 已經在處理的 NEAI/NAM 流程。因此這裡直接重用
//!    `sso_login_to_service()`，不需要第二套登入實作，也不需要 OCR
//!    （淡江的驗證碼 token 由伺服器透過 `ImageValidate?outType=2` 直接給出）。
//!
//! 2. **純 GET，沒有 ASP.NET PostBack。** 結果頁的參數全在 query string，
//!    不需要處理 `__VIEWSTATE` / `__EVENTVALIDATION` 往返。
//!
//! 3. **`stu_no` 就是 SSO 帳號。** 淡江學生的單一登入帳號即學號，所以
//!    `account.username` 可直接當作 `stu_no`，使用者不必額外填任何設定。
//!    （學號屬個資，只會出現在對學校的請求中，不會寫進日誌或前端回應。）

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use chrono::{Datelike, Local};
use miette::{IntoDiagnostic, Result, WrapErr};
use regex::Regex;
use reqwest::{cookie::Jar, header, Client, Url};
use tracing::{debug, warn};

use crate::account::AccountConfig;

use super::{RawTimetableRow, TimetableSource};

pub(super) const EMIS_BASE_URL: &str = "https://sso.tku.edu.tw/aissinfo/emis/";
const RESULT_PAGE: &str = "TMWC090_result.aspx";
/// 查詢頁（選學年期 → 開始查詢）。必須先造訪這頁，結果頁才會被接受。
const QUERY_PAGE: &str = "TMWC090.aspx";
/// 學生查詢子選單的入口（2026-09 學校新增的中介步驟）。
///
/// 主選單上「選課資料(依上課星期、節次列表)」現在連到 `tmw0012.aspx`，
/// 它會 302 到 `TMW0040.aspx`（學生查詢子選單：學生基本/修業/成績/選課…）。
/// 走過這一頁之後，TMWC090 / TMWC090_result 才會被伺服器接受。
pub(super) const STUDENT_MENU_PAGE: &str = "tmw0012.aspx";
pub(super) const HTML_ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
pub(super) const ACCEPT_LANGUAGE: &str = "zh-TW,zh;q=0.9,en;q=0.8";

/// 一份課表最多接受的「課程 × 節次」格數，避免異常頁面撐爆記憶體。
const MAX_GRID_CELLS: usize = 5000;

pub fn create_source() -> Box<dyn TimetableSource> {
    Box::new(TkuTimetableSource)
}

struct TkuTimetableSource;

#[async_trait]
impl TimetableSource for TkuTimetableSource {
    fn source_name(&self) -> &'static str {
        "淡江大學校務行政資訊查詢系統 (emis)"
    }

    async fn fetch(
        &self,
        client: &Client,
        cookie_jar: &Arc<Jar>,
        account: &AccountConfig,
    ) -> Result<Vec<RawTimetableRow>> {
        let student_no = account.username.trim();
        if student_no.is_empty() {
            return Err(miette::miette!("TKU 課表同步：帳號的 username（學號）為空"));
        }

        // CURRENT_SEMESTER_OVERRIDE_V1：[semester] current 可覆蓋學期代碼
        let yr_sem = yr_sem_with_override(Local::now().date_naive(), crate::config::semester_override());
        let target = build_result_url(&yr_sem, student_no)?;

        // 先走一次 SSO，把 session 綁到 emis 這個 service 上。
        // 注意：cookie_jar 由呼叫端新建且只給本次同步使用，
        // 因此即使 emis 登入失敗，也不會影響 Tronclass 的點名 session。
        debug!(yr_sem = %yr_sem, "TKU 課表同步：開始 SSO 登入 emis");
        crate::auth::providers::tku::sso_login_to_service(client, cookie_jar, &target, account)
            .await
            .wrap_err("TKU 課表同步：emis SSO 登入失敗")?;

        // EMIS_SESSION_WARMUP_V1
        // emis (TAMIS) 是 ASP.NET 舊系統，除了 SSO 的身分驗證外，**另外還有一層
        // 應用程式自己的 session**。若拿著 SSO cookie 直接深連結查詢結果頁，
        // 應用程式會認為「沒有 session」而導向 TMW_overtime.html
        // （訊息：您因為停留太久沒有操作…請回首頁重新登入）——實測就是卡在這裡。
        //
        // 因此登入後要先走一次入口頁「暖身」，讓伺服器建立 ASP.NET session
        // 與相關 cookie，再去取結果頁。
        let entry_url = Url::parse(EMIS_BASE_URL)
            .into_diagnostic()
            .wrap_err("TKU 課表同步：無法組出 emis 入口網址")?;
        // 入口頁的 HTML 與其最終網址，用於後續自動探索課表連結。
        let mut entry_html: Option<(String, Url)> = None;
        debug!(url = %entry_url, "TKU 課表同步：先進入 emis 入口頁建立 session");
        match client
            .get(entry_url.clone())
            .header(header::ACCEPT, HTML_ACCEPT)
            .header(header::ACCEPT_LANGUAGE, ACCEPT_LANGUAGE)
            .send()
            .await
        {
            Ok(entry_resp) => {
                debug!(
                    status = %entry_resp.status(),
                    final_url = %redact_url(entry_resp.url()),
                    "TKU 課表同步：emis 入口頁回應"
                );
                let ct = entry_resp
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let base = entry_resp.url().clone();
                // 讀完 body 才算完成這次往返，cookie 也才確定寫入 jar。
                if let Ok(b) = entry_resp.bytes().await {
                    entry_html = Some((decode_html_bytes(&b, &ct), base));
                }
            }
            Err(e) => {
                // 暖身失敗不直接放棄，後面仍會嘗試結果頁並回報真正的錯誤。
                warn!(error = %e, "TKU 課表同步：emis 入口頁讀取失敗，仍嘗試直接取結果頁");
            }
        }

        // EMIS_STUDENT_QUERY_CONTEXT_V1
        // 2026-09-13 實測（root cause：當時同步失敗）：emis 在主選單與查詢頁之間
        // 新增了「學生查詢」中介頁。只走「入口 → TMWC090 → result」已不夠——
        // 跳過這一步時，TMWC090_result.aspx 會被伺服器 rewrite 回 TMW0040.aspx
        // （約 6KB 的學生查詢子選單，日誌中 html_bytes≈5983 即此頁），
        // 解析 0 筆 → 觸發安全閥保留原課表。
        //
        // 正確流程（與瀏覽器同步實測）：
        //   emis/ → tmw0012.aspx →(302) TMW0040.aspx → TMWC090.aspx → TMWC090_result.aspx
        //
        // 因此在查詢頁之前先 GET tmw0012.aspx（跟隨轉址到 TMW0040.aspx），
        // 建立這次 session 的「學生查詢」情境；同時收集子選單頁上的連結，
        // 之後作為候選網址的後備（TMWC090.aspx 就在這個子選單上）。
        let mut sub_menu_links: Vec<(String, String)> = Vec::new();
        let student_menu_url = entry_url
            .join(STUDENT_MENU_PAGE)
            .into_diagnostic()
            .wrap_err("TKU 課表同步：無法組出 emis 學生查詢選單網址")?;
        debug!(url = %redact_url(&student_menu_url), "TKU 課表同步：先進入學生查詢選單建立情境");
        match client
            .get(student_menu_url.clone())
            .header(header::ACCEPT, HTML_ACCEPT)
            .header(header::ACCEPT_LANGUAGE, ACCEPT_LANGUAGE)
            .header(header::REFERER, entry_url.as_str())
            .send()
            .await
        {
            Ok(r) => {
                debug!(
                    status = %r.status(),
                    final_url = %redact_url(r.url()),
                    "TKU 課表同步：學生查詢選單回應"
                );
                let ct = r
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let base = r.url().clone();
                if let Ok(b) = r.bytes().await {
                    sub_menu_links = extract_page_links(&decode_html_bytes(&b, &ct), &base);
                }
            }
            Err(e) => {
                // 與入口頁同策略：情境步驟失敗不直接放棄，仍嘗試查詢頁與結果頁。
                warn!(error = %e, "TKU 課表同步：學生查詢選單讀取失敗，仍繼續查詢頁流程");
            }
        }

        // EMIS_QUERY_PAGE_FIRST_V1
        // 使用者實測回報的正確流程（截圖佐證，2026-09 起前面還多一步學生查詢選單）：
        //   1. 進 TMWC090.aspx（選學期頁：顯示學號姓名 + 學年期下拉 + 開始查詢）
        //   2. 再送出查詢，才會到 TMWC090_result.aspx?YrSem=..&stu_no=..
        //
        // 換言之，`TMWC090_result.aspx` 這個網址**本來就是對的**（先前推測正確），
        // 只是不能直接深連結——必須先造訪查詢頁，讓伺服器建立這次查詢的情境
        // （ASP.NET 在查詢頁 session 中記錄「是誰要查、查哪個學期」）。
        //
        // 因此在結果頁之前，先 GET 一次查詢頁；同時仍保留選單自動探索作為後備，
        // 以防未來頁面代號變更。
        let query_page = entry_url
            .join(QUERY_PAGE)
            .into_diagnostic()
            .wrap_err("TKU 課表同步：無法組出 emis 查詢頁網址")?;
        debug!(url = %query_page, "TKU 課表同步：先進入查詢頁建立查詢情境");
        match client
            .get(query_page.clone())
            .header(header::ACCEPT, HTML_ACCEPT)
            .header(header::ACCEPT_LANGUAGE, ACCEPT_LANGUAGE)
            .header(header::REFERER, entry_url.as_str())
            .send()
            .await
        {
            Ok(r) => {
                debug!(
                    status = %r.status(),
                    final_url = %redact_url(r.url()),
                    "TKU 課表同步：查詢頁回應"
                );
                let _ = r.bytes().await;
            }
            Err(e) => {
                warn!(error = %e, "TKU 課表同步：查詢頁讀取失敗，仍嘗試直接取結果頁");
            }
        }

        // 候選網址：以「查詢頁 → 結果頁」為主要路徑，選單探索為後備。
        // 順位：結果頁直達 → 學生查詢子選單（TMW0040）上找到的課表連結
        //       → 主選單上找到的課表連結。
        // 子選單的探索放在主選單之前：主選單的「選課資料」連結現指到
        // tmw0012.aspx（情境頁，被導回時 is_entry_page 會換下一個候選），
        // 而 TMW0040 上的連結直接指到 TMWC090.aspx 這種真正的查詢頁。
        let mut candidates: Vec<Url> = vec![target.clone()];
        for (links, label) in [
            (sub_menu_links, "學生查詢子選單"),
            entry_html
                .as_ref()
                .map(|(html, base)| (extract_page_links(html, base), "主選單"))
                .unwrap_or_default(),
        ] {
            if links.is_empty() {
                continue;
            }
            // links 是 by-value（自陣列迭代而來），需明確借用成 slice。
            if let Some(found) = find_timetable_link(&links) {
                debug!(source = label, url = %redact_url(&found), "TKU 課表同步：從選單找到課表連結");
                let mut with_params = found.clone();
                with_params
                    .query_pairs_mut()
                    .append_pair("YrSem", &yr_sem)
                    .append_pair("stu_no", student_no);
                if !candidates.contains(&with_params) {
                    candidates.push(with_params);
                }
                if !candidates.contains(&found) {
                    candidates.push(found);
                }
            } else if label == "主選單" {
                let all: Vec<String> = links
                    .iter()
                    .filter(|(href, _)| href.to_ascii_lowercase().contains(".aspx"))
                    .map(|(href, text)| {
                        let short: String = text.chars().take(24).collect();
                        format!("{href} = {short}")
                    })
                    .take(60)
                    .collect();
                warn!(
                    link_count = links.len(),
                    aspx_links = ?all,
                    "TKU 課表同步：入口頁中找不到課表連結，以下為頁面上的 .aspx 連結清單"
                );
            }
        }

        // 依序嘗試候選網址，第一個「解析得到課程」的就採用。
        // 被導回入口頁（TMW0000）或逾時頁時視為失敗，換下一個候選。
        let mut last_err: Option<String> = None;
        let mut fallback: Option<(String, String, String)> = None; // (html, final_url, path)

        for candidate in &candidates {
            let resp = match client
                .get(candidate.clone())
                .header(header::ACCEPT, HTML_ACCEPT)
                .header(header::ACCEPT_LANGUAGE, ACCEPT_LANGUAGE)
                .header(header::REFERER, entry_url.as_str())
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(format!("讀取失敗：{e}"));
                    continue;
                }
            };

            if !resp.status().is_success() {
                last_err = Some(format!("返回非 2xx 狀態：{}", resp.status()));
                continue;
            }

            let final_url = redact_url(resp.url());
            let resp_url_path = resp.url().path().to_string();
            let content_type = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();

            // BIG5_DECODE_V1（見 decode_html_bytes）
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    last_err = Some(format!("讀取內容失敗：{e}"));
                    continue;
                }
            };
            let html = decode_html_bytes(&bytes, &content_type);

            debug!(
                final_url = %final_url,
                content_type = %content_type,
                html_bytes = html.len(),
                "TKU 課表同步：已取得 emis 回應"
            );

            if looks_like_login_page(&html) {
                return Err(miette::miette!(
                    "TKU 課表同步：仍停留在 SSO 登入頁，請確認帳號密碼是否正確"
                ));
            }

            // OVERTIME_PAGE_DETECT_V1
            if looks_like_overtime_page(&html, &resp_url_path) {
                last_err = Some("emis 回應連線逾時頁（應用程式 session 未建立）".to_string());
                continue;
            }

            // 被導回入口頁代表這個網址不被接受，換下一個候選。
            if is_entry_page(&resp_url_path) {
                last_err = Some(format!("被導回入口頁（{resp_url_path}），此網址不被接受"));
                continue;
            }

            let rows = parse_timetable_html(&html)?;
            if !rows.is_empty() {
                return Ok(rows);
            }
            // 解析為空：先留著，若所有候選都失敗再用它產出診斷。
            if fallback.is_none() {
                fallback = Some((html, final_url, resp_url_path));
            }
        }

        // 所有候選都沒解析到課程。
        if let Some((html, final_url, _)) = fallback {
            log_parse_diagnostics(&html, &extract_tables(&html));
            warn!(
                final_url = %final_url,
                yr_sem = %yr_sem,
                "TKU 課表同步：未從 emis 解析到任何課程"
            );
            return Ok(Vec::new());
        }

        Err(miette::miette!(
            "TKU 課表同步：無法取得課表頁（{}）。\
             已嘗試 {} 個網址，包含從入口頁選單自動探索到的連結。\
             若持續失敗，請查看日誌中的 .aspx 連結清單以確認正確的頁面代號。",
            last_err.unwrap_or_else(|| "原因不明".to_string()),
            candidates.len()
        ))
    }
}

/// 依日期推算淡江的「學年期」代碼，例如 `1151` = 民國 115 學年度第 1 學期。
///
/// 規則：8 月～次年 1 月為第 1 學期，2 月～7 月為第 2 學期。
/// 跨年的 1 月仍屬前一學年度的第 1 學期。
///
/// SEMESTER_AUTO_CALC_SHARED_V1：日期→學年度/學期的推算規則已收進
/// `config::academic_year_for()`（原本這裡跟 `api::todos` 的兩個學期判斷函式
/// 各自重複一份），這裡只做「西元年 → 民國年」的格式轉換。
pub(crate) fn current_yr_sem(today: chrono::NaiveDate) -> String {
    let (western_year, semester) = crate::config::academic_year_for(today.year(), today.month());
    let roc_year = western_year - 1911;
    format!("{roc_year}{semester}")
}

/// CURRENT_SEMESTER_OVERRIDE_V1：套用 `[semester] current` 覆蓋後的 YrSem。
///
/// 覆蓋為 `(西元年, 學期 1|2)`；未覆蓋時退回 [`current_yr_sem`] 依日期推算。
/// 使用時機：開學初課表尚未換新學期（抓舊學期），或寒假期間預先抓新學期。
fn yr_sem_with_override(
    today: chrono::NaiveDate,
    override_semester: Option<(i32, u32)>,
) -> String {
    if let Some((western_year, term)) = override_semester {
        return format!("{}{}", western_year - 1911, term);
    }
    current_yr_sem(today)
}

fn build_result_url(yr_sem: &str, student_no: &str) -> Result<Url> {
    let mut url = Url::parse(EMIS_BASE_URL)
        .and_then(|base| base.join(RESULT_PAGE))
        .into_diagnostic()
        .wrap_err("TKU 課表同步：無法組出 emis 網址")?;
    url.query_pairs_mut()
        .append_pair("YrSem", yr_sem)
        .append_pair("stu_no", student_no);
    Ok(url)
}

/// 依 HTTP 標頭與 HTML `<meta charset>` 判斷編碼，將位元組解碼成字串。
///
/// emis 是 Big5 的舊系統，且回應標頭**不帶 charset**。若直接用 `resp.text()`，
/// reqwest 會預設 UTF-8，中文將全部損毀成 U+FFFD 且無法還原。
///
/// 判斷順序：
/// 1. HTTP `Content-Type` 的 `charset=`
/// 2. HTML 前 2 KB 內的 `<meta charset>` / `<meta http-equiv>`
/// 3. 若內容不是合法 UTF-8，退回 Big5（台灣校務系統的常見預設）
pub(super) fn decode_html_bytes(bytes: &[u8], content_type: &str) -> String {
    // 1) HTTP 標頭（charset 可能大小寫混用，逐段轉小寫比對）
    let mut label: Option<String> = None;
    for part in content_type.split(';') {
        let part = part.trim();
        if part.to_ascii_lowercase().starts_with("charset=") {
            let value = part[8..].trim().trim_matches('"').trim_matches('\'');
            if !value.is_empty() {
                label = Some(value.to_string());
            }
            break;
        }
    }

    // 2) HTML <meta>：只掃前 2 KB，charset 宣告一定在 <head> 早期
    if label.is_none() {
        let head = &bytes[..bytes.len().min(2048)];
        // 以 latin1 解讀即可，charset 宣告本身必為 ASCII
        let head_ascii: String = head.iter().map(|&b| b as char).collect::<String>();
        let lower = head_ascii.to_ascii_lowercase();
        if let Some(pos) = lower.find("charset") {
            let rest = &lower[pos + 7..];
            let rest = rest.trim_start_matches([' ', '=', '"', '\'']);
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
                .unwrap_or(rest.len());
            if end > 0 {
                label = Some(rest[..end].to_string());
            }
        }
    }

    // 3) 依標籤取得編碼；未知或未宣告時，用「是否為合法 UTF-8」來決定
    let encoding = label
        .as_deref()
        .and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()))
        .unwrap_or_else(|| {
            if std::str::from_utf8(bytes).is_ok() {
                encoding_rs::UTF_8
            } else {
                // 台灣校務系統的傳統編碼。encoding_rs 的 BIG5 也涵蓋 CP950 擴充字。
                encoding_rs::BIG5
            }
        });

    let (text, actual, had_errors) = encoding.decode(bytes);
    if had_errors {
        warn!(
            declared = ?label,
            used = %actual.name(),
            "TKU 課表同步：頁面解碼出現無法對應的字元，部分文字可能不正確"
        );
    }
    text.into_owned()
}

/// 把網址中的個資（學號、帳號、票證）遮蔽後才可寫入日誌。
///
/// emis 的課表網址帶有 `stu_no=<學號>`，SSO 轉址網址則可能帶 `myurl=`／票證參數。
/// 日誌可能被轉貼求助，因此一律遮蔽。
pub(super) fn redact_url(url: &Url) -> String {
    const SENSITIVE: [&str; 6] = ["stu_no", "account", "id", "user", "ticket", "token"];
    let mut out = url.clone();
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| {
            let key = k.to_string();
            let masked = if SENSITIVE
                .iter()
                .any(|s| key.eq_ignore_ascii_case(s))
            {
                "<redacted>".to_string()
            } else {
                v.to_string()
            };
            (key, masked)
        })
        .collect();
    if pairs.is_empty() {
        out.set_query(None);
    } else {
        let mut qp = out.query_pairs_mut();
        qp.clear();
        for (k, v) in pairs {
            qp.append_pair(&k, &v);
        }
        drop(qp);
    }
    out.to_string()
}

/// 判斷是否為 emis 的入口／選單頁（例如 `TMW0000.aspx`）。
///
/// 實測直接 GET 猜測的查詢網址會被導回這裡，代表該網址不被接受。
pub(super) fn is_entry_page(url_path: &str) -> bool {
    let lower = url_path.to_ascii_lowercase();
    // TMW0040：2026-09 新增的「學生查詢」子選單。學生查詢情境（tmw0012 → TMW0040）
    // 未建立時，TMWC090_result 會被 rewrite 回這頁；候選嘗試時視同「不被接受」。
    lower.contains("tmw0040")
        || lower.ends_with("/emis/")
        || lower.ends_with("/emis")
        || lower.contains("tmw0000")
        || lower.ends_with("/default.aspx")
        || lower.ends_with("/index.aspx")
}

/// 取出 HTML 中所有連結，回傳 `(絕對網址, 連結文字)`。
///
/// 同時處理 `<a href>` 與 ASP.NET 常見的 `window.open('xxx.aspx')` 寫法。
fn extract_page_links(html: &str, base: &Url) -> Vec<(String, String)> {
    static A_RE: OnceLock<Option<Regex>> = OnceLock::new();
    let mut out: Vec<(String, String)> = Vec::new();

    if let Some(re) = cached_regex(
        &A_RE,
        r#"(?is)<\s*a\b[^>]*?href\s*=\s*(?:\x22([^\x22]*)\x22|'([^']*)'|([^\s>]+))[^>]*>(.*?)<\s*/\s*a\s*>"#,
    ) {
        for caps in re.captures_iter(html) {
            let href = caps
                .get(1)
                .or_else(|| caps.get(2))
                .or_else(|| caps.get(3))
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
                continue;
            }
            let text = cell_text(caps.get(4).map(|m| m.as_str()).unwrap_or(""));
            if let Ok(abs) = base.join(&href) {
                out.push((abs.to_string(), text.replace('\n', " ")));
            }
        }
    }

    // ASP.NET 的選單常用 JS 開新視窗，這類連結沒有 <a href>。
    static JS_RE: OnceLock<Option<Regex>> = OnceLock::new();
    if let Some(re) = cached_regex(&JS_RE, r#"(?i)['\x22]([A-Za-z0-9_./-]+\.aspx[^'\x22]*)['\x22]"#) {
        for caps in re.captures_iter(html) {
            if let Some(m) = caps.get(1) {
                if let Ok(abs) = base.join(m.as_str()) {
                    let url = abs.to_string();
                    if !out.iter().any(|(u, _)| u == &url) {
                        out.push((url, String::new()));
                    }
                }
            }
        }
    }

    out
}

/// 從連結清單中挑出最可能是「選課資料(依上課星期、節次列表)」的那一個。
///
/// 以連結文字的關鍵字評分，避免誤選到其他查詢功能。
fn find_timetable_link(links: &[(String, String)]) -> Option<Url> {
    let mut best: Option<(i32, Url)> = None;

    for (href, text) in links {
        let mut score = 0i32;
        // 依關鍵字評分：越貼近「上課星期、節次」的敘述分數越高。
        if text.contains("上課星期") {
            score += 10;
        }
        if text.contains("節次") {
            score += 8;
        }
        if text.contains("選課資料") {
            score += 6;
        }
        if text.contains("課表") {
            score += 6;
        }
        if text.contains("選課") {
            score += 3;
        }
        if text.contains("課程") {
            score += 2;
        }
        // 明顯不是課表的功能要扣分，避免誤選。
        for bad in ["成績", "學分", "繳費", "宿舍", "兵役", "獎懲", "缺曠", "畢業"] {
            if text.contains(bad) {
                score -= 8;
            }
        }
        // TMWC090 是先前推測的頁面代號，若真的存在就加分。
        if href.to_ascii_lowercase().contains("tmwc090") {
            score += 4;
        }
        if score >= 6 {
            if let Ok(url) = Url::parse(href) {
                if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
                    best = Some((score, url));
                }
            }
        }
    }

    best.map(|(_, u)| u)
}

/// 判斷是否為 TAMIS 的「連線逾時／需重新登入」頁。
///
/// 實測：直接深連結查詢結果頁時，emis 會導向 `TMW_overtime.html`，
/// 內容為「您因為停留太久沒有操作，造成連線逾時，請回首頁重新登入」。
/// 這頁只有約 1.7 KB、單一表格，若不特別辨識就會被誤判成「課表是空的」。
fn looks_like_overtime_page(html: &str, url_path: &str) -> bool {
    if url_path.to_ascii_lowercase().contains("overtime") {
        return true;
    }
    // 內容比對需在**正確解碼之後**才有意義（見 BIG5_DECODE_V1）。
    ["連線逾時", "停留太久", "重新登入", "連線逾時，請回首頁"]
        .iter()
        .filter(|kw| html.contains(**kw))
        .count()
        >= 2
}

pub(super) fn looks_like_login_page(html: &str) -> bool {
    html.contains("/NEAI/logineb.jsp")
        || html.contains("/NEAI/loginrwd.jsp")
        || html.contains("Tamkang University Single Sign On")
}

// ─── HTML 解析 ────────────────────────────────────────────────────────────────

/// 表格中的一格。保留 rowspan/colspan 才能正確還原「一堂課橫跨多節」的版面。
#[derive(Debug, Clone)]
pub(super) struct Cell {
    pub(super) text: String,
    pub(super) rowspan: usize,
    pub(super) colspan: usize,
    /// 格內超連結（`<a>`）的文字。
    ///
    /// emis 課表格的版面是固定多行（班級/學門、課名、教師_教室），
    /// 其中**課名那一行是格內唯一的超連結**（課名會連到課程大綱 PDF）。
    /// 以超連結文字為課名，可避免「共同科」「文學經典學門」這類
    /// 開課單位/學門被誤當成課名（真課名分別是「國際學習」「文學名篇選讀」）。
    link_text: String,
}

/// 從整份 HTML 解析出課表。
///
/// 同時支援兩種可能的版面：
/// - **格狀**：第一欄是節次、其餘欄位對應星期（`依上課星期、節次列表` 的典型呈現）。
/// - **清單**：每列一門課，欄位含「星期」「節次」等標題。
///
/// 兩種都試，取解析出較多課程的結果，避免因版面調整而整個失效。
pub(crate) fn parse_timetable_html(html: &str) -> Result<Vec<RawTimetableRow>> {
    let tables = extract_tables(html);
    let mut best: Vec<RawTimetableRow> = Vec::new();

    for table in &tables {
        let rows = parse_table_cells(table);
        if rows.is_empty() {
            continue;
        }

        let parsed = parse_as_list(&rows).unwrap_or_default();
        if parsed.len() > best.len() {
            best = parsed;
        }

        let parsed = parse_as_grid(&rows).unwrap_or_default();
        if parsed.len() > best.len() {
            best = parsed;
        }
    }

    // PARSE_DIAGNOSTICS_V1
    // 解析不到課程時，把「頁面結構」記進日誌以便定位版面差異。
    // 刻意只輸出結構性資訊（表格數、列數、欄數、標題列文字），
    // **不輸出課程內容或任何可識別個人的欄位**，避免個資落入日誌。
    if best.is_empty() {
        log_parse_diagnostics(html, &tables);
    }

    Ok(best)
}

/// 解析失敗時輸出頁面結構摘要，協助判斷 emis 的實際版面。
fn log_parse_diagnostics(html: &str, tables: &[String]) {
    // 標題列通常是 th 或第一列；只取短字串（版面標籤），長字串多半是資料。
    let mut summaries: Vec<String> = Vec::new();

    for (idx, table) in tables.iter().enumerate().take(12) {
        let rows = parse_table_cells(table);
        if rows.is_empty() {
            continue;
        }
        let max_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        // 只取第一列、且每格限 12 字元以內的短標籤當作結構線索。
        let header: Vec<String> = rows
            .first()
            .map(|r| {
                r.iter()
                    .take(10)
                    .map(|c| {
                        let t = c.text.replace('\n', "/");
                        if t.chars().count() > 12 {
                            "<long>".to_string()
                        } else {
                            t
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        summaries.push(format!(
            "table[{idx}] rows={} cols={max_cols} header={header:?}",
            rows.len()
        ));
    }

    // 頁面上是否出現常見的「查無資料」訊息。
    let no_data = [
        "查無",
        "無資料",
        "沒有資料",
        "無選課",
        "尚未",
        "錯誤",
        "失敗",
        "重新登入",
        "逾時",
        "無權限",
        "請先登入",
    ]
    .iter()
    .find(|kw| html.contains(**kw))
    .map(|kw| kw.to_string());

    // PAGE_IDENTITY_DIAGNOSTICS_V1
    // 只看結構不足以判斷「我們到底拿到哪一頁」。實測回傳僅 1753 bytes，
    // 遠小於真正的課表頁，代表根本不是課表頁。以下擷取足以辨識頁面身分、
    // 但不含個資的線索。
    let title = html
        .split_once("<title")
        .and_then(|(_, rest)| rest.split_once('>'))
        .and_then(|(_, rest)| rest.split_once("</title>"))
        .map(|(t, _)| t.trim().chars().take(80).collect::<String>());

    // 轉址／框架類頁面的特徵：meta refresh、JS 轉址、frame、form action。
    // 只轉一次小寫（原本寫在迴圈內，每輪都會複製整份 HTML）。
    let lower_html = html.to_ascii_lowercase();
    let mut hints: Vec<String> = Vec::new();
    for (kw, label) in [
        ("http-equiv", "meta-refresh"),
        ("window.location", "js-redirect"),
        ("location.href", "js-redirect"),
        ("location.replace", "js-redirect"),
        ("<frame", "frameset"),
        ("<iframe", "iframe"),
        ("document.forms", "auto-submit-form"),
        ("__viewstate", "aspnet-postback"),
        ("logineb.jsp", "sso-login"),
        ("neai", "sso-neai"),
        ("imagevalidate", "sso-captcha"),
    ] {
        if lower_html.contains(kw) && !hints.iter().any(|h| h == label) {
            hints.push(label.to_string());
        }
    }

    // 頁面可見文字（去標籤）的前 200 字。這頁若是錯誤／提示頁，訊息就在這裡。
    // 課表頁不會走到這個分支（有解析到課程就不會呼叫本函式），
    // 因此不會洩漏課程內容；仍限制長度以策安全。
    let visible: String = {
        let mut s = String::new();
        let mut in_tag = false;
        let mut in_script = false;
        let mut count = 0usize;
        // 以 char_indices 迭代，天然對齊 UTF-8 邊界，不會切到中文字中間。
        for (i, c) in html.char_indices() {
            if count >= 200 {
                break;
            }
            if lower_html[i..].starts_with("<script") || lower_html[i..].starts_with("<style") {
                in_script = true;
            }
            if in_script
                && (lower_html[i..].starts_with("</script") || lower_html[i..].starts_with("</style"))
            {
                in_script = false;
            }
            if c == '<' {
                in_tag = true;
            } else if c == '>' {
                in_tag = false;
            } else if !in_tag && !in_script {
                if c.is_whitespace() {
                    if !s.ends_with(' ') {
                        s.push(' ');
                        count += 1;
                    }
                } else {
                    s.push(c);
                    count += 1;
                }
            }
        }
        s.trim().to_string()
    };

    warn!(
        html_bytes = html.len(),
        table_count = tables.len(),
        no_data_hint = ?no_data,
        page_title = ?title,
        page_hints = ?hints,
        visible_text = %visible,
        structure = ?summaries,
        "TKU 課表解析診斷：未解析到課程，以下為頁面結構摘要（不含課程內容）"
    );
}

// LAZY_REGEX_NO_PANIC_V1
// 這些 pattern 都是編譯期固定的字面值，先前寫成
// `Regex::new(...).expect("valid regex")`，有兩個問題：
//
// 1. **panic 風險**：`\1` 反向參照在 Rust 的 regex crate 不被支援，
//    `.expect()` 會直接 panic。本專案沒有掛 `CatchPanicLayer`，
//    若 panic 發生在「立即同步」的 HTTP handler 上，連線會直接斷掉（前端看到 502）。
// 2. **效能**：原本每次呼叫都重新編譯一次 regex，而解析是在巢狀迴圈內進行。
//
// 改用 `OnceLock` 惰性編譯一次並重複使用；編譯失敗時回傳 `None`，
// 由呼叫端優雅降級成「解析不到課程」，而不是讓行程崩潰。
pub(super) fn cached_regex(slot: &'static OnceLock<Option<Regex>>, pattern: &str) -> Option<&'static Regex> {
    slot.get_or_init(|| match Regex::new(pattern) {
        Ok(re) => Some(re),
        Err(e) => {
            // 這代表程式碼中的 pattern 有誤（例如用了不支援的語法）。
            // 記錄下來讓問題可被發現，但不讓它拖垮整個服務。
            warn!(error = %e, pattern = %pattern, "課表解析 regex 編譯失敗，該解析步驟將被略過");
            None
        }
    })
    .as_ref()
}

/// 取出所有 `<table>` 區塊的內容，並且正確處理巢狀表格
/// （ASP.NET GridView 常見）。回傳每個表格的 innerHTML。
pub(super) fn extract_tables(html: &str) -> Vec<String> {
    static TAG_RE: OnceLock<Option<Regex>> = OnceLock::new();
    let Some(tag_re) = cached_regex(&TAG_RE, r#"(?is)<\s*(/?)\s*table\b[^>]*>"#) else {
        return Vec::new();
    };
    let mut tables = Vec::new();
    let mut stack: Vec<usize> = Vec::new();

    for m in tag_re.find_iter(html) {
        let is_close = tag_re
            .captures(m.as_str())
            .and_then(|c| c.get(1))
            .map(|g| g.as_str() == "/")
            .unwrap_or(false);

        if is_close {
            if let Some(start) = stack.pop() {
                tables.push(html[start..m.start()].to_string());
            }
        } else {
            stack.push(m.end());
        }
    }

    tables
}

/// 把一個表格的 innerHTML 拆成 `Vec<Vec<Cell>>`。
///
/// 只取「直接屬於本表格」的儲存格：巢狀表格的內容會被當成該格的文字，
/// 而不會被誤判成本表格的列。
///
/// NESTED_TABLE_DEPTH_SCAN_V1
/// 舊版用 `<tr...>(.*?)</tr>` 與 `<td...>(.*?)</td>` 兩個非貪婪 regex。
/// 2026-09 實測的課表頁在**標頭列的左上角格**內有一個巢狀小表
/// （對角線標籤「星期／節次」）：
///
/// ```text
/// <tr><td><table><tr><td></td><td>星期</td></tr><tr><td>節次</td></tr></table></td>
///     <td>一</td><td>二</td>…<td>日</td></tr>
/// ```
///
/// 非貪婪的 `<tr>(.*?)</tr>` 會在**巢小表的第一個 `</tr>`** 就結束匹配，
/// 導致同一列後半的星期格（一、二、三…日）落在所有匹配之外——
/// 星期標頭整列消失、`find_weekday_header` 找不到、整張課表解析 0 筆。
///
/// 改為單一 token 掃描，以 `<table>`/`</table>` 計深度，只在深度 0 處理
/// `<tr>`/`<td>`；深度 > 0 的標籤屬於巢狀表格，其內容由外層格的
/// `cell_text` 一併吸收為文字（左上角格因此成為「星期 節次」）。
/// 這同時保留了舊行為：巢狀表格的儲存格不會被誤判成本表格的列。
pub(super) fn parse_table_cells(table_html: &str) -> Vec<Vec<Cell>> {
    static TAG_RE: OnceLock<Option<Regex>> = OnceLock::new();
    // 群組 1 = "/"（閉合標記）、群組 2 = 標籤名（table / tr / td / th）。
    let Some(tag_re) = cached_regex(&TAG_RE, r#"(?is)<\s*(/?)\s*(table|tr|t[dh])\b[^>]*>"#)
    else {
        return Vec::new();
    };

    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let mut cells: Vec<Cell> = Vec::new();
    let mut cell_budget = MAX_GRID_CELLS;
    // 巢狀表格深度：> 0 時的 tr/td 屬於巢狀表格，不屬於本表。
    let mut depth: usize = 0;
    // 尚未閉合的儲存格：(屬性字串, 內容起點)。
    let mut pending: Option<(String, usize)> = None;

    // 收掉未閉合的儲存格；`$end` 為內容結尾（通常是 `</td>` 的起點）。
    // cell_budget 用盡時僅丟棄該格，不再計數。
    macro_rules! close_cell {
        ($end:expr) => {{
            if let Some((attrs, start)) = pending.take() {
                if cell_budget > 0 {
                    cell_budget -= 1;
                    let inner = &table_html[start..$end];
                    cells.push(Cell {
                        text: cell_text(inner),
                        link_text: extract_link_text(inner),
                        rowspan: parse_span(&attrs, "rowspan"),
                        colspan: parse_span(&attrs, "colspan"),
                    });
                }
            }
        }};
    }

    for caps in tag_re.captures_iter(table_html) {
        let is_close = caps.get(1).map(|m| m.as_str()).unwrap_or("") == "/";
        let name = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let tag_start = caps.get(0).map(|m| m.start()).unwrap_or(0);
        let tag_end = caps.get(0).map(|m| m.end()).unwrap_or(0);
        let name_end = caps.get(2).map(|m| m.end()).unwrap_or(tag_end);

        match name {
            "table" => {
                if is_close {
                    depth = depth.saturating_sub(1);
                } else {
                    depth += 1;
                }
            }
            _ if depth > 0 => {}
            "tr" => {
                // <tr> 或 </tr> 都代表上一列結束（含未閉合格的寬容處理）。
                close_cell!(tag_start);
                if !cells.is_empty() {
                    rows.push(std::mem::take(&mut cells));
                }
            }
            "td" | "th" => {
                if is_close {
                    close_cell!(tag_start);
                } else {
                    // 上一格未閉合就直接開新格（畸形 HTML 寬容處理）。
                    close_cell!(tag_start);
                    // 屬性 = 標籤名之後、`>` 之前的字串（與舊版格式一致）。
                    let attrs = table_html[name_end..tag_end.saturating_sub(1)]
                        .trim()
                        .to_string();
                    pending = Some((attrs, tag_end));
                }
            }
            _ => {}
        }
        if cell_budget == 0 {
            break;
        }
    }

    // 檔尾沖刷：最後一格與最後一列可能沒有閉合標籤。
    close_cell!(table_html.len());
    // 上面是最後一次 close_cell，扣減的 cell_budget 之後沒人再讀；
    // 顯式讀一次以避免 unused_assignments 警告（語義不變）。
    let _ = cell_budget;
    if !cells.is_empty() {
        rows.push(cells);
    }

    rows
}

/// 取出一段 HTML 內**最長**的超連結文字。
///
/// 課表格中課名行是唯一的 `<a>`（連到課程大綱），取最長可避免
/// 導覽用短連結（如「詳細」）干擾；沒有任何連結時回傳空字串。
pub(super) fn extract_link_text(html: &str) -> String {
    static A_RE: OnceLock<Option<Regex>> = OnceLock::new();
    let Some(a_re) = cached_regex(
        &A_RE,
        r#"(?is)<\s*a\b[^>]*>(.*?)<\s*/\s*a\s*>"#,
    ) else {
        return String::new();
    };
    let mut best = String::new();
    for caps in a_re.captures_iter(html) {
        let inner = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
        let text = cell_text(inner).trim().to_string();
        if text.chars().count() > best.chars().count() {
            best = text;
        }
    }
    best
}

pub(super) fn parse_span(attrs: &str, name: &str) -> usize {
    // 只會用 "rowspan" / "colspan" 兩種，各自快取一份，避免每格重編譯。
    static ROWSPAN_RE: OnceLock<Option<Regex>> = OnceLock::new();
    static COLSPAN_RE: OnceLock<Option<Regex>> = OnceLock::new();

    let slot = if name == "rowspan" {
        &ROWSPAN_RE
    } else {
        &COLSPAN_RE
    };
    let pattern = format!(r#"(?is)\b{name}\s*=\s*(?:"(\d+)"|'(\d+)'|(\d+))"#);
    let Some(re) = cached_regex(slot, &pattern) else {
        return 1;
    };
    re.captures(attrs)
        .and_then(|caps| {
            caps.get(1)
                .or_else(|| caps.get(2))
                .or_else(|| caps.get(3))
                .and_then(|m| m.as_str().parse::<usize>().ok())
        })
        .filter(|v| *v >= 1 && *v <= 64)
        .unwrap_or(1)
}

/// 把儲存格的 HTML 轉成純文字，保留 `<br>` 造成的換行。
pub(super) fn cell_text(html: &str) -> String {
    static BR_RE: OnceLock<Option<Regex>> = OnceLock::new();
    static TAG_RE: OnceLock<Option<Regex>> = OnceLock::new();

    let (Some(br_re), Some(tag_re)) = (
        cached_regex(&BR_RE, r#"(?is)<\s*br\s*/?\s*>|</\s*(p|div|tr|li)\s*>"#),
        cached_regex(&TAG_RE, r#"(?is)<[^>]*>"#),
    ) else {
        // 極端情況下退回原文，至少不會讓整個同步崩潰。
        return html.split_whitespace().collect::<Vec<_>>().join(" ");
    };

    let with_breaks = br_re.replace_all(html, "\n");
    let stripped = tag_re.replace_all(&with_breaks, " ");
    let decoded = decode_html_entities(&stripped);

    decoded
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn decode_html_entities(value: &str) -> String {
    value
        .replace("&nbsp;", " ")
        .replace("&#160;", " ")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

// ─── 版面一：格狀（節次 × 星期） ──────────────────────────────────────────────

fn parse_as_grid(rows: &[Vec<Cell>]) -> Option<Vec<RawTimetableRow>> {
    // 找出標題列：含有兩個以上可辨識星期的那一列。
    let (header_idx, weekday_by_col) = find_weekday_header(rows)?;
    if weekday_by_col.len() < 2 {
        return None;
    }

    // 課程 → 星期 → 節次集合
    let mut collected: BTreeMap<(String, String, String, String, String), Vec<i64>> =
        BTreeMap::new();

    // rowspan 追蹤：col_index → (剩餘列數, 內容, 超連結文字)
    let mut pending: BTreeMap<usize, (usize, String, String)> = BTreeMap::new();

    for row in rows.iter().skip(header_idx + 1) {
        let mut col = 0usize;
        let mut cursor = 0usize;
        let mut period: Option<i64> = None;
        // 本列每個欄位最終的文字內容（含超連結文字，用於課名）
        let mut row_cells: BTreeMap<usize, (String, String)> = BTreeMap::new();

        // 防呆上限：欄數不可能超過這個數字，避免任何意外的無限迴圈。
        const MAX_COLUMNS: usize = 64;

        while col < MAX_COLUMNS {
            // 先讓上一列 rowspan 延伸下來的內容佔位。
            if let Some((remaining, text, link)) = pending.get_mut(&col) {
                if *remaining > 0 {
                    row_cells.insert(col, (text.clone(), link.clone()));
                    *remaining -= 1;
                    col += 1;
                    continue;
                }
            }

            let Some(cell) = row.get(cursor) else {
                // 這一列的 <td> 已用完，但右側欄位可能還被上一列的 rowspan 覆蓋
                // （例如星期五的課跨兩節時，下一列就不會再有那一格）。
                // 必須把這些延伸格也消化掉，否則該課會漏掉第二節，
                // 而且殘留的 pending 會讓後續每一列的欄位對位全部偏移。
                if pending
                    .iter()
                    .any(|(idx, (remaining, _, _))| *idx > col && *remaining > 0)
                {
                    col += 1;
                    continue;
                }
                break;
            };
            cursor += 1;

            for offset in 0..cell.colspan {
                let target = col + offset;
                row_cells.insert(target, (cell.text.clone(), cell.link_text.clone()));
                if cell.rowspan > 1 {
                    pending.insert(
                        target,
                        (cell.rowspan - 1, cell.text.clone(), cell.link_text.clone()),
                    );
                }
            }
            col += cell.colspan;
        }

        pending.retain(|_, (remaining, _, _)| *remaining > 0);

        // 節次：取第一個不是星期欄的欄位裡的數字。
        for (idx, (text, _link)) in &row_cells {
            if weekday_by_col.contains_key(idx) {
                continue;
            }
            if let Some(value) = first_period_number(text) {
                period = Some(value);
                break;
            }
        }
        let Some(period) = period else {
            continue;
        };

        for (idx, weekday) in &weekday_by_col {
            let Some((text, link)) = row_cells.get(idx) else {
                continue;
            };
            let Some(course) = parse_course_cell(text, link) else {
                continue;
            };
            collected
                .entry((
                    weekday.clone(),
                    course.course_name,
                    course.teacher,
                    course.classroom,
                    course.course_code,
                ))
                .or_default()
                .push(period);
        }
    }

    Some(finalize(collected))
}

fn find_weekday_header(rows: &[Vec<Cell>]) -> Option<(usize, BTreeMap<usize, String>)> {
    // WEEKDAY_HEADER_PREFER_EXPLICIT_V1
    // `normalize_weekday` 現在也接受純數字（"1" = 星期一），這讓標頭偵測更寬容，
    // 但也帶來風險：一整列純數字（例如節次欄）可能被誤判成星期標頭。
    //
    // 因此分兩輪：
    //   第 1 輪 只認「明確的星期字樣」（含「星期/週/禮拜/周」或英文 Mon..Sun）
    //   第 2 輪 才退而求其次接受純數字
    // 真實課表的標頭幾乎都是明確字樣，這樣就能穩定命中正確的那一列。
    fn scan(rows: &[Vec<Cell>], explicit_only: bool) -> Option<(usize, BTreeMap<usize, String>)> {
        for (idx, row) in rows.iter().enumerate().take(10) {
            let mut map = BTreeMap::new();
            let mut col = 0usize;
            for cell in row {
                let text = cell.text.trim();
                let is_explicit = text.contains("星期")
                    || text.contains('週')
                    || text.contains("禮拜")
                    || text.contains('周')
                    || {
                        let l = text.to_ascii_lowercase();
                        ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]
                            .iter()
                            .any(|p| l.starts_with(p))
                    };
                if !explicit_only || is_explicit {
                    if let Some(weekday) = normalize_weekday(text) {
                        for offset in 0..cell.colspan {
                            map.insert(col + offset, weekday.clone());
                        }
                    }
                }
                col += cell.colspan;
            }
            if map.len() >= 2 {
                return Some((idx, map));
            }
        }
        None
    }

    scan(rows, true).or_else(|| scan(rows, false))
}

// ─── 版面二：清單（每列一門課） ───────────────────────────────────────────────

fn parse_as_list(rows: &[Vec<Cell>]) -> Option<Vec<RawTimetableRow>> {
    let header = rows.first()?;
    let mut weekday_col = None;
    let mut period_col = None;
    let mut name_col = None;
    let mut teacher_col = None;
    let mut room_col = None;
    let mut code_col = None;

    for (idx, cell) in header.iter().enumerate() {
        let t = cell.text.replace(char::is_whitespace, "");
        if weekday_col.is_none() && t.contains("星期") && !t.contains("節") {
            weekday_col = Some(idx);
        } else if period_col.is_none() && t.contains("節次") {
            period_col = Some(idx);
        } else if name_col.is_none() && (t.contains("科目") || t.contains("課程")) {
            name_col = Some(idx);
        } else if teacher_col.is_none() && (t.contains("教師") || t.contains("授課")) {
            teacher_col = Some(idx);
        } else if room_col.is_none() && t.contains("教室") {
            room_col = Some(idx);
        } else if code_col.is_none() && (t.contains("課號") || t.contains("代號")) {
            code_col = Some(idx);
        }
    }

    let (weekday_col, period_col, name_col) = (weekday_col?, period_col?, name_col?);

    let mut collected: BTreeMap<(String, String, String, String, String), Vec<i64>> =
        BTreeMap::new();

    for row in rows.iter().skip(1) {
        let get = |idx: Option<usize>| -> String {
            idx.and_then(|i| row.get(i))
                .map(|c| c.text.replace('\n', " ").trim().to_string())
                .unwrap_or_default()
        };

        let Some(weekday) = row.get(weekday_col).and_then(|c| normalize_weekday(&c.text)) else {
            continue;
        };
        let course_name = get(Some(name_col));
        if course_name.is_empty() {
            continue;
        }
        let periods = parse_period_list(row.get(period_col).map(|c| c.text.as_str()).unwrap_or(""));
        if periods.is_empty() {
            continue;
        }

        collected
            .entry((
                weekday,
                course_name,
                get(teacher_col),
                get(room_col),
                get(code_col),
            ))
            .or_default()
            .extend(periods);
    }

    Some(finalize(collected))
}

fn finalize(
    collected: BTreeMap<(String, String, String, String, String), Vec<i64>>,
) -> Vec<RawTimetableRow> {
    collected
        .into_iter()
        .map(
            |((weekday, course_name, teacher, classroom, course_code), mut periods)| {
                periods.sort_unstable();
                periods.dedup();
                RawTimetableRow {
                    weekday,
                    periods,
                    course_name,
                    teacher,
                    classroom,
                    course_code,
                }
            },
        )
        .collect()
}

// ─── 欄位解析輔助 ─────────────────────────────────────────────────────────────

struct ParsedCourse {
    course_name: String,
    teacher: String,
    classroom: String,
    course_code: String,
}

/// 解析格狀版面中一格的內容。典型格式是多行：
/// ```text
/// 資料結構
/// 王小明
/// Q409
/// ```
/// 行數與順序不保證，因此用特徵判斷而非固定索引。
///
/// `link_text` 是格內超連結的文字（見 `Cell::link_text`）。emis 課表格中
/// 課名行是唯一的超連結，因此**有超連結時以它為課名**，其餘行仍照舊
/// 邏輯解析出教師/教室/課號。沒有超連結時（其他學校或舊版面）行為不變。
fn parse_course_cell(text: &str, link_text: &str) -> Option<ParsedCourse> {
    let mut base = parse_course_cell_lines(text)?;
    let link = link_text.trim();
    if !link.is_empty() {
        base.course_name = link.to_string();
    }
    Some(base)
}

/// 舊版 `parse_course_cell` 主體：從多行文字以特徵辨識各欄位。
fn parse_course_cell_lines(text: &str) -> Option<ParsedCourse> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && *l != "-" && *l != "－")
        .collect();
    if lines.is_empty() {
        return None;
    }

    // TKU_CELL_LAYOUT_V1
    // 實測 emis 的課表儲存格是固定三行（截圖佐證）：
    //     資工英語二          ← 班級／系所組
    //     電子商務            ← 課程名稱（超連結）
    //     王大明_E 410        ← 教師_教室（底線相連）
    //
    // 舊版的逐行啟發式在這個版面上是錯的：會把「班級」當成課名、
    // 「課名」當成教師，而且 `教師_教室` 黏在一起完全沒拆開。
    // 這裡改為優先辨識這個實際版面。
    let mut course_name = String::new();
    let mut teacher = String::new();
    let mut classroom = String::new();
    let mut course_code = String::new();
    let mut class_name = String::new();

    // 先抽出「教師_教室」這種以底線相連的行。
    let mut rest: Vec<&str> = Vec::new();
    for line in &lines {
        if teacher.is_empty() && classroom.is_empty() {
            if let Some((t, r)) = split_teacher_classroom(line) {
                teacher = t;
                classroom = r;
                continue;
            }
        }
        rest.push(line);
    }

    for line in rest {
        if course_code.is_empty() && looks_like_course_code(line) {
            course_code = line.to_string();
        } else if classroom.is_empty() && looks_like_classroom(line) {
            classroom = line.to_string();
        } else if class_name.is_empty() && looks_like_class_group(line) {
            // 班級／系所組（例：資工英語二）。記下來但不當課名。
            class_name = line.to_string();
        } else if course_name.is_empty() {
            course_name = line.to_string();
        } else if teacher.is_empty() && looks_like_person_name(line) {
            teacher = line.to_string();
        }
    }

    // 若整格只有班級（沒有其他行），那它其實就是課名，避免整堂課遺失。
    if course_name.is_empty() && !class_name.is_empty() {
        course_name = class_name;
    }

    if course_name.is_empty() {
        return None;
    }
    Some(ParsedCourse {
        course_name,
        teacher,
        classroom,
        course_code,
    })
}

/// 拆解「教師_教室」這種以底線相連的寫法，例如 `王大明_E 410` → (`王大明`, `E 410`)。
///
/// emis 的課表格固定用底線把授課教師與教室接在一起；
/// 教師姓名中間可能有全形空白（如 `陳　安`），教室則像 `E 304`、`B 113`。
fn split_teacher_classroom(line: &str) -> Option<(String, String)> {
    let (left, right) = line.split_once('_')?;
    let teacher = left.trim().replace('\u{3000}', "");
    let classroom = right.trim().to_string();
    if teacher.is_empty() && classroom.is_empty() {
        return None;
    }
    Some((teacher, classroom))
}

/// 判斷是否為「班級／系所組」名稱，例如 `資工英語二`、`資工三A`。
///
/// 這類文字出現在課程名稱**之前**，若誤判成課名會讓所有課程名稱都變成班級。
fn looks_like_class_group(line: &str) -> bool {
    let t = line.trim();
    let n = t.chars().count();
    if !(3..=10).contains(&n) {
        return false;
    }
    // 必須全為中文（可含末尾的英數班別代號，如「資工三A」）
    let mut chars = t.chars();
    let all_cjk_prefix = chars
        .by_ref()
        .take(n.saturating_sub(1))
        .all(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
    if !all_cjk_prefix {
        return false;
    }
    // 以年級字結尾（一二三四五六七八九）或單一英數班別
    let last = t.chars().last().unwrap_or(' ');
    let ends_with_grade = matches!(last, '一' | '二' | '三' | '四' | '五' | '六' | '七' | '八' | '九')
        || last.is_ascii_alphanumeric();
    ends_with_grade
}

/// 淡江課號形如 `TQEXB0S0813`：全大寫英數、含數字、長度 >= 7。
fn looks_like_course_code(line: &str) -> bool {
    let s = line.trim();
    s.len() >= 7
        && s.len() <= 24
        && s.chars().all(|c| c.is_ascii_alphanumeric())
        && s.chars().any(|c| c.is_ascii_digit())
        && s.chars().filter(|c| c.is_ascii_alphabetic()).all(|c| c.is_ascii_uppercase())
        && s.chars().filter(|c| c.is_ascii_alphabetic()).count() >= 3
}

/// 淡江教室代碼形如 `Q409`、`E787`、`B302L`：英文字母開頭 + 數字，且不長。
fn looks_like_classroom(line: &str) -> bool {
    let s = line.trim();
    if s.len() < 2 || s.len() > 12 {
        return false;
    }
    if !s.chars().all(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    let first_alpha = s.chars().next().map(|c| c.is_ascii_alphabetic()) == Some(true);
    let has_digit = s.chars().any(|c| c.is_ascii_digit());
    let alpha_count = s.chars().filter(|c| c.is_ascii_alphabetic()).count();
    first_alpha && has_digit && alpha_count <= 2
}

/// 中文姓名：全為中日文字元，長度 2～4。
fn looks_like_person_name(line: &str) -> bool {
    let s = line.trim();
    let count = s.chars().count();
    (2..=4).contains(&count) && s.chars().all(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

/// 從「第3節」「3」「3-4」等字串取出第一個節次數字。
fn first_period_number(text: &str) -> Option<i64> {
    // CHINESE_NUMERAL_PERIOD_V1
    // 實測 emis 的節次欄用**中文數字**（一、二、三…十、十一），
    // 原本只找阿拉伯數字，導致每一列的節次都抓不到 ⇒ 整張課表報廢。
    if let Some(v) = chinese_numeral_to_i64(text) {
        return Some(v);
    }
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    let Some(re) = cached_regex(&RE, r"\d{1,2}") else {
        return None;
    };
    re.find(text)
        .and_then(|m| m.as_str().parse::<i64>().ok())
        .filter(|v| (1..=20).contains(v))
}

/// 將中文數字轉成整數，支援 1..=20（涵蓋淡江的第 1..14 節）。
///
/// 接受「一」「十」「十一」「二十」等寫法；找不到中文數字時回傳 `None`。
fn chinese_numeral_to_i64(text: &str) -> Option<i64> {
    let t: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if t.is_empty() {
        return None;
    }
    // 去掉「第」「節」等裝飾字
    let t = t.trim_matches(|c| c == '第' || c == '節' || c == '/');
    if t.is_empty() {
        return None;
    }

    let digit = |c: char| -> Option<i64> {
        Some(match c {
            '一' | '壹' => 1,
            '二' | '貳' | '兩' => 2,
            '三' | '參' => 3,
            '四' | '肆' => 4,
            '五' | '伍' => 5,
            '六' | '陸' => 6,
            '七' | '柒' => 7,
            '八' | '捌' => 8,
            '九' | '玖' => 9,
            _ => return None,
        })
    };

    let chars: Vec<char> = t.chars().collect();
    // 找出第一段連續的中文數字，避免被其他文字干擾。
    let mut value: Option<i64> = None;
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c == '十' || c == '拾' {
            // 十 / 十X / X十 / X十Y
            let tens = if i > 0 { digit(chars[i - 1]).unwrap_or(1) } else { 1 };
            let ones = chars.get(i + 1).and_then(|c| digit(*c)).unwrap_or(0);
            value = Some(tens * 10 + ones);
            break;
        }
        if let Some(d) = digit(c) {
            // 若下一個字是「十」，交給上面的分支處理。
            if chars.get(i + 1).map(|n| *n == '十' || *n == '拾').unwrap_or(false) {
                i += 1;
                continue;
            }
            value = Some(d);
            break;
        }
        i += 1;
    }

    value.filter(|v| (1..=20).contains(v))
}

/// 解析「34」「3,4」「3-5」這類節次描述，回傳所有節次。
fn parse_period_list(text: &str) -> Vec<i64> {
    let mut out = Vec::new();
    static RANGE_RE: OnceLock<Option<Regex>> = OnceLock::new();
    let Some(range_re) = cached_regex(&RANGE_RE, r"(\d{1,2})\s*[-~～]\s*(\d{1,2})") else {
        return out;
    };
    let mut consumed = String::from(text);

    for caps in range_re.captures_iter(text) {
        let (Some(a), Some(b)) = (caps.get(1), caps.get(2)) else {
            continue;
        };
        let (Ok(a), Ok(b)) = (a.as_str().parse::<i64>(), b.as_str().parse::<i64>()) else {
            continue;
        };
        if a <= b && (1..=20).contains(&a) && (1..=20).contains(&b) {
            out.extend(a..=b);
        }
        consumed = consumed.replace(caps.get(0).map(|m| m.as_str()).unwrap_or(""), " ");
    }

    // 剩下的：可能是逗號分隔，也可能是「34」這種連寫。
    for token in consumed.split(|c: char| !c.is_ascii_digit()) {
        if token.is_empty() {
            continue;
        }
        if let Ok(value) = token.parse::<i64>() {
            if (1..=20).contains(&value) {
                out.push(value);
                continue;
            }
        }
        // 連寫：逐字拆成個別節次。
        for ch in token.chars() {
            if let Some(d) = ch.to_digit(10) {
                out.push(d as i64);
            }
        }
    }

    out.retain(|v| (1..=20).contains(v));
    out.sort_unstable();
    out.dedup();
    out
}

/// 星期字串正規化，輸出與 `timetable_entries.weekday` 相同的 `mon`..`sun`。
fn normalize_weekday(value: &str) -> Option<String> {
    // WEEKDAY_TOLERANT_V1
    // 校務系統的星期標頭寫法不一，實測常見形式包含：
    //   星期一 / 週一 / 禮拜一 / 一 / （一） / (一) / 星期1 / 1 / Mon / Monday
    // 以及全形數字「１」與全形括號。先做正規化再比對，避免因為版面用了
    // 其中一種寫法就整張課表解析失敗。
    let mut v: String = value
        .chars()
        .filter(|c| !c.is_whitespace())
        // 去掉各種括號與標點裝飾
        .filter(|c| !matches!(c, '（' | '）' | '(' | ')' | '[' | ']' | '【' | '】' | '.' | '、'))
        // 全形數字 → 半形
        .map(|c| match c {
            '０'..='９' => char::from(b'0' + (c as u32 - '０' as u32) as u8),
            _ => c,
        })
        .collect();
    v = v.trim().to_string();

    // 英文星期（大小寫不拘）
    let lower = v.to_ascii_lowercase();
    for (prefix, day) in [
        ("mon", "mon"),
        ("tue", "tue"),
        ("wed", "wed"),
        ("thu", "thu"),
        ("fri", "fri"),
        ("sat", "sat"),
        ("sun", "sun"),
    ] {
        if lower.starts_with(prefix) {
            return Some(day.to_string());
        }
    }

    // 去掉中文前綴
    let key = v
        .strip_prefix("星期")
        .or_else(|| v.strip_prefix("週"))
        .or_else(|| v.strip_prefix("禮拜"))
        .or_else(|| v.strip_prefix("周"))
        .unwrap_or(&v);
    let key: String = key.chars().take(1).collect();

    let normalized = match key.as_str() {
        "一" | "1" => "mon",
        "二" | "2" => "tue",
        "三" | "3" => "wed",
        "四" | "4" => "thu",
        "五" | "5" => "fri",
        "六" | "6" => "sat",
        "日" | "天" | "7" | "0" => "sun",
        _ => return None,
    };
    Some(normalized.to_string())
}

/// 節次 → 起訖時刻（淡水／蘭陽校園）。
///
/// 第 n 節 = `(7+n):10 ~ (8+n):00`，第 1 節 08:10–09:00 … 第 14 節 21:10–22:00。
/// 臺北校園第 11 節之後為 18:20 起、每節 50 分鐘，與此表不同；目前以淡水／蘭陽
/// 為準，若日後需要區分校區，可由 provider 設定覆寫。
pub(crate) fn period_to_times(period: i64) -> Option<(String, String)> {
    if !(1..=14).contains(&period) {
        return None;
    }
    let start_hour = 7 + period;
    let end_hour = 8 + period;
    Some((format!("{start_hour:02}:10"), format!("{end_hour:02}:00")))
}

