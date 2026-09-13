//! 淡江大學（TKU）學分／成績狀態抓取：emis 修業相關資訊＋必修科目＋歷年成績。
//!
//! CREDITS_EMIS_V1
//!
//! 資料來源（皆為 2026-09-13 以真實 session 實測過的頁面結構）：
//!
//! 1. `TMWS080.aspx`（學生修業相關資訊）：畢業審核學年度、畢業應修學分數
//!    （總分＋必修＋系選修）、修業學期數。表格為「項目｜內容｜說明」三欄，
//!    說明欄內有巢狀小表（深度感知解析可正確處理）。
//! 2. `TMWC120.aspx`（必修科目及畢業學分）：頁首有畢業/必修/系選修學分數；
//!    表格為「年級｜名稱｜科號｜專業組別｜群別｜學分(上/下)｜說明」——
//!    科號空的列是通識學門（群別 L~Z 單字母），其餘是必修科目。
//! 3. `TMWS100.aspx`（歷年各科成績）：11 欄表（學年/學期/單位/科號/科目/
//!    學期序/群別/選必修/學分/成績），成績格式有「87」「*43」（不及格）、
//!    「通過」（pass/fail 課）。頁首有「累計實得學分」。
//!
//! 與課表同步共用同一套 SSO 與「學生查詢情境」流程
//! （見 `tku.rs` 的 EMIS_STUDENT_QUERY_CONTEXT_V1）。
//!
//! 隱私：學號只出現在對學校的請求與 report 內（report 存本機 DB、
//! 只回給已登入的管理介面），日誌一律走 `redact_url`。

use std::sync::Arc;

use miette::{IntoDiagnostic, Result, WrapErr};
use reqwest::{cookie::Jar, header, Client, Url};
use serde::Serialize;
use tracing::{debug, warn};

use crate::account::AccountConfig;

use super::tku::{
    decode_html_bytes, extract_tables, is_entry_page, looks_like_login_page,
    parse_table_cells, redact_url, Cell, ACCEPT_LANGUAGE, EMIS_BASE_URL, HTML_ACCEPT,
    STUDENT_MENU_PAGE,
};

/// 修業相關資訊（畢業門檻總覽）。
const STUDY_PAGE: &str = "TMWS080.aspx";
/// 必修科目及畢業學分（必修清單＋通識學門）。
const REQUIRED_PAGE: &str = "TMWC120.aspx";
/// 歷年各科成績（已修課程與成績）。
const TRANSCRIPT_PAGE: &str = "TMWS100.aspx";

// ─── 資料結構（直接序列化成 API 回應）───────────────────────────────────────

/// 一份完整的學分／成績報告。
#[derive(Debug, Clone, Serialize)]
pub struct CreditReport {
    pub student: CreditStudent,
    #[serde(default)]
    pub graduation: GraduationInfo,
    /// 頁面宣告的累計實得學分（歷年成績頁；與自行加總及格學分應一致）。
    #[serde(default)]
    pub earned_total: Option<u32>,
    /// 歷年各科成績（含不及格；依學年/學期排序）。
    #[serde(default)]
    pub courses: Vec<CreditCourseRow>,
    /// 必修科目清單（TMWC120；含科號）。
    #[serde(default)]
    pub required_courses: Vec<RequiredCourseRow>,
    /// 通識學門清單（TMWC120 群別 L~Z；科號為空的列）。
    #[serde(default)]
    pub ge_domains: Vec<GeDomainRow>,
}

/// 學生基本資料。
#[derive(Debug, Clone, Serialize, Default)]
pub struct CreditStudent {
    pub dept: String,
    pub student_no: String,
    pub name: String,
}

/// 畢業門檻（TMWS080/TMWC120）。
#[derive(Debug, Clone, Serialize, Default)]
pub struct GraduationInfo {
    /// 畢業審核學年度（例如 "114"）。
    #[serde(default)]
    pub audit_year: String,
    /// 畢業應修總學分（例如 128）。
    #[serde(default)]
    pub total_credits: Option<u32>,
    /// 必修學分（例如 81；含通識核心課）。
    #[serde(default)]
    pub required_credits: Option<u32>,
    /// 本系選修最低學分（例如 19）。
    #[serde(default)]
    pub dept_elective_min: Option<u32>,
    /// 最低修業學期數。
    #[serde(default)]
    pub min_semesters: Option<u32>,
    /// 至多修業學期數。
    #[serde(default)]
    pub max_semesters: Option<u32>,
    /// 已修業學期數（含本學期）。
    #[serde(default)]
    pub completed_semesters: Option<u32>,
}

/// 歷年成績中的一科。
#[derive(Debug, Clone, Serialize)]
pub struct CreditCourseRow {
    /// 學年（民國，例如 "114"）。
    pub yr: String,
    /// 學期（"1" 上學期 / "2" 下學期）。
    pub sem: String,
    pub dept_code: String,
    pub dept_name: String,
    pub course_code: String,
    /// 科目名稱（去除「◎ 全英語」前綴；取中文名行）。
    pub name: String,
    /// 該科是否全英語授課（emis 科目名稱前綴「◎」；全英語學士班畢業學分採計要件）。
    #[serde(default)]
    pub is_english: bool,
    /// 學期序（"0" 學期課 / "1" 上 / "2" 下；一年課上下各列一筆）。
    pub term_seq: String,
    /// 通識群別代碼（L 文學經典、Z 全球科技革命…；非通識為空）。
    pub group_code: String,
    /// 是否必修（emis「選必修」欄）。
    pub required: bool,
    pub credits: u32,
    /// 原始成績字串（"87"、"*43"、"通過"…）。
    pub grade: String,
    /// 數字成績（「通過」等非數字成績為 None）。
    pub grade_value: Option<f64>,
    /// 是否及格（取得學分）。
    pub passed: bool,
}

/// 必修科目清單中的一科（TMWC120）。
#[derive(Debug, Clone, Serialize)]
pub struct RequiredCourseRow {
    /// 年級（"一"～"四"）。
    pub grade_level: String,
    pub name: String,
    pub course_code: String,
    pub credits: u32,
    /// 開課學期（"上" / "下" / "上下"）。
    pub term: String,
    #[serde(default)]
    pub note: String,
}

/// 通識學門（TMWC120 群別）。
#[derive(Debug, Clone, Serialize)]
pub struct GeDomainRow {
    /// 群別代碼（"L"、"M"、…）。
    pub code: String,
    /// 學門名稱（「文學經典」…）。
    pub name: String,
    /// 排定年級（"一"～"二"；僅供參考）。
    pub grade_level: String,
}

// ─── 抓取入口 ────────────────────────────────────────────────────────────────

/// 以 SSO 登入 emis 並抓取學分／成績報告。
///
/// 呼叫端負責逾時控制（`tokio::time::timeout`）與 cookie jar 的生命週期；
/// 每次同步都應使用全新的 `cookie_jar`（與課表同步相同的隔離策略）。
pub async fn fetch_credit_report(
    client: &Client,
    cookie_jar: &Arc<Jar>,
    account: &AccountConfig,
) -> Result<CreditReport> {
    let student_no = account.username.trim();
    if student_no.is_empty() {
        return Err(miette::miette!("學分查詢：帳號的 username（學號）為空"));
    }

    let base = Url::parse(EMIS_BASE_URL)
        .into_diagnostic()
        .wrap_err("學分查詢：無法組出 emis 網址")?;
    let target = base
        .join(TRANSCRIPT_PAGE)
        .into_diagnostic()
        .wrap_err("學分查詢：無法組出歷年成績頁網址")?;

    // 與課表同步同一套 SSO（NEAI/NAM），把 session 綁到 emis。
    debug!("學分查詢：開始 SSO 登入 emis");
    crate::auth::providers::tku::sso_login_to_service(client, cookie_jar, &target, account)
        .await
        .wrap_err("學分查詢：emis SSO 登入失敗")?;

    // 情境建立：入口 → 學生查詢子選單（tmw0012 → 302 TMW0040）。
    // 沒有這兩步，TMWS080/TMWS100 會被 rewrite 回選單頁（同課表同步的教訓）。
    for (page, referer_page) in [("", EMIS_BASE_URL), (STUDENT_MENU_PAGE, EMIS_BASE_URL)] {
        let url = if page.is_empty() {
            base.clone()
        } else {
            base.join(page)
                .into_diagnostic()
                .wrap_err("學分查詢：無法組出情境步驟網址")?
        };
        let referer = base
            .join(referer_page)
            .into_diagnostic()
            .wrap_err("學分查詢：無法組出 Referer 網址")?;
        if let Err(e) = get_html(client, &url, &referer).await {
            // 情境步驟失敗不直接放棄——查詢頁本身也可能被接受。
            warn!(error = %e, "學分查詢：情境步驟（{}）失敗，仍繼續", if page.is_empty() { "入口" } else { page });
        }
    }

    let study_html = get_html(client, &base.join(STUDY_PAGE).into_diagnostic()?, &base).await?;
    let required_html = get_html(client, &base.join(REQUIRED_PAGE).into_diagnostic()?, &base).await?;
    let transcript_html = get_html(client, &target, &base).await?;

    let mut report = CreditReport {
        student: CreditStudent::default(),
        graduation: GraduationInfo::default(),
        earned_total: None,
        courses: Vec::new(),
        required_courses: Vec::new(),
        ge_domains: Vec::new(),
    };
    parse_study_page(&study_html, &mut report);
    parse_required_page(&required_html, &mut report);
    parse_transcript_page(&transcript_html, &mut report);
    Ok(report)
}

/// GET 一個 emis 頁面並做身分／情境檢查，回傳解碼後的 HTML。
async fn get_html(client: &Client, url: &Url, referer: &Url) -> Result<String> {
    let resp = client
        .get(url.clone())
        .header(header::ACCEPT, HTML_ACCEPT)
        .header(header::ACCEPT_LANGUAGE, ACCEPT_LANGUAGE)
        .header(header::REFERER, referer.as_str())
        .send()
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("學分查詢：請求失敗（{}）", redact_url(url)))?;
    let status = resp.status();
    let final_url = resp.url().clone();
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = resp
        .bytes()
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("學分查詢：讀取內容失敗（{}）", redact_url(url)))?;
    let html = decode_html_bytes(&bytes, &ct);
    debug!(
        status = %status,
        final_url = %redact_url(&final_url),
        html_bytes = html.len(),
        "學分查詢：已取得頁面"
    );
    if !status.is_success() {
        return Err(miette::miette!(
            "學分查詢：emis 回返回非 2xx 狀態（{status}，{}）",
            redact_url(&final_url)
        ));
    }
    if looks_like_login_page(&html) {
        return Err(miette::miette!(
            "學分查詢：仍停留在 SSO 登入頁，請確認帳號密碼是否正確"
        ));
    }
    if is_entry_page(final_url.path()) {
        return Err(miette::miette!(
            "學分查詢：被導回選單頁（學生查詢情境未建立），請稍後再試"
        ));
    }
    Ok(html)
}

// ─── TMWS080：修業相關資訊 ──────────────────────────────────────────────────

fn parse_study_page(html: &str, report: &mut CreditReport) {
    // 頁首 span：系級：…</span> 學號：…</span> 姓名：…</span>
    report.student.dept = match_html_text(html, r#"(?s)系級[：:]\s*([^<]+?)(?:\s*</|\s*$)"#);
    report.student.student_no = match_html_text(html, r#"(?s)學號[：:]\s*([^<]+?)(?:\s*</|\s*$)"#);
    report.student.name = match_html_text(html, r#"(?s)姓名[：:]\s*([^<]+?)(?:\s*</|\s*$)"#);

    let Some(rows) = table_rows(html, "畢業審核學年度") else {
        warn!("學分查詢：TMWS080 找不到修業資訊表格");
        return;
    };
    for row in rows {
        // 列格式：[項目, 內容, 說明(可能吸收巢狀小表文字)]
        let key = row.first().map(|c| cell_plain(c)).unwrap_or_default();
        let value = row.get(1).map(|c| cell_plain(c)).unwrap_or_default();
        if key.contains("畢業審核學年度") {
            report.graduation.audit_year = first_number(&value).unwrap_or_default();
        } else if key.contains("畢業應修學分數") {
            // 例：「128(含必修81學分、本系選修19學分)」
            report.graduation.total_credits = parse_u32_prefix(&value);
            if let Some(rest) = value.split_once('(').map(|(_, r)| r) {
                if let Some(v) = number_after(rest, "必修") {
                    report.graduation.required_credits = Some(v);
                }
                if let Some(v) = number_after(rest, "選修") {
                    report.graduation.dept_elective_min = Some(v);
                }
            }
        } else if key.contains("最低修業學期數") {
            report.graduation.min_semesters = parse_u32_prefix(&value);
        } else if key.contains("至多修業學期數") {
            report.graduation.max_semesters = parse_u32_prefix(&value);
        } else if key.contains("已修業學期數") {
            report.graduation.completed_semesters = parse_u32_prefix(&value);
        }
    }
}

// ─── TMWC120：必修科目及畢業學分 ────────────────────────────────────────────

fn parse_required_page(html: &str, report: &mut CreditReport) {
    // 頁首粗體行：「畢業學分數：128　必修學分數：81　本系選修課最低學分：19」
    if report.graduation.audit_year.is_empty() {
        report.graduation.audit_year =
            first_number(&match_html_text(html, r#"(?s)畢業審核學年度[：:]\s*(\d+)"#))
                .unwrap_or_default();
    }
    if report.graduation.total_credits.is_none() {
        report.graduation.total_credits = number_after(html, "畢業學分數");
    }
    if report.graduation.required_credits.is_none() {
        report.graduation.required_credits = number_after(html, "必修學分數");
    }
    if report.graduation.dept_elective_min.is_none() {
        report.graduation.dept_elective_min = number_after(html, "本系選修課最低學分")
            .or_else(|| number_after(html, "選修課最低學分"))
            .or_else(|| number_after(html, "本系選修最低學分"));
    }

    let Some(rows) = table_rows(html, "必修科目名稱") else {
        warn!("學分查詢：TMWC120 找不到必修科目表格");
        return;
    };
    for row in rows {
        let Some(first) = row.first() else { continue };
        let grade_level = cell_plain(first);
        // 年級欄：一～六（中文數字單字）。
        if !matches!(grade_level.as_str(), "一" | "二" | "三" | "四" | "五" | "六") {
            continue;
        }
        if row.len() < 7 {
            continue;
        }
        let name = cell_plain(&row[1]);
        let course_code = cell_plain(&row[2]);
        let group = cell_plain(&row[4]);
        let up = cell_plain(&row[5]);
        let down = cell_plain(&row[6]);
        let note = row.get(7).map(|c| cell_plain(c)).unwrap_or_default();

        // 學分：上/下兩欄各是「該學期學分」；年課兩欄都有時為全年總和
        // （例：英文（一）上 2 下 2 = 全年 4 學分，與成績單兩筆各 2 一致）。
        let up_credits = parse_u32_prefix(&up);
        let down_credits = parse_u32_prefix(&down);
        let credits = up_credits.unwrap_or(0) + down_credits.unwrap_or(0);
        let term = match (up_credits, down_credits) {
            (Some(_), Some(_)) => "上下".to_string(),
            (Some(_), None) => "上".to_string(),
            (None, Some(_)) => "下".to_string(),
            (None, None) => String::new(),
        };

        if !course_code.is_empty() {
            report.required_courses.push(RequiredCourseRow {
                grade_level,
                name,
                course_code,
                credits,
                term,
                note,
            });
        } else if !group.is_empty() {
            // 科號空、群別有值：通識學門列。
            report.ge_domains.push(GeDomainRow {
                code: group,
                name,
                grade_level,
            });
        }
    }
}

// ─── TMWS100：歷年各科成績 ──────────────────────────────────────────────────

fn parse_transcript_page(html: &str, report: &mut CreditReport) {
    // 「累計實得學分：45」（頁首粗體）。
    if let Some(v) = number_after(html, "累計實得學分") {
        report.earned_total = Some(v);
    }

    let Some(rows) = table_rows(html, "科目名稱") else {
        warn!("學分查詢：TMWS100 找不到歷年成績表格");
        return;
    };
    for row in rows {
        // 11 欄：學年/學期/單位代碼/單位簡稱/科號/科目名稱/學期序/群別/選必修/學分/成績
        if row.len() < 11 {
            continue;
        }
        let yr = cell_plain(&row[0]);
        let sem = cell_plain(&row[1]);
        // 只收資料列：學年是 3 位數字。
        if yr.len() != 3 || !yr.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let dept_code = cell_plain(&row[2]);
        let dept_name = cell_plain(&row[3]);
        let course_code = cell_plain(&row[4]);
        // 科目名稱多行（中文名 + <br> 英文名）；取第一行。「◎」前綴＝全英語授課，
        // 全英語學士班只採計全英語授課課程的畢業學分，須保留此資訊。
        let first_line = row[5].text.lines().next().unwrap_or_default().trim();
        let is_english = first_line.starts_with('◎');
        let name = first_line.trim_start_matches('◎').trim().to_string();
        let term_seq = cell_plain(&row[6]);
        let group_code = cell_plain(&row[7]);
        let required = row[8].text.contains("必修");
        let credits = parse_u32_prefix(&cell_plain(&row[9])).unwrap_or(0);
        let grade_raw = row[10].text.trim().to_string();
        let (grade, grade_value, passed) = parse_grade(&grade_raw);

        report.courses.push(CreditCourseRow {
            yr,
            sem,
            dept_code,
            dept_name,
            course_code,
            name,
            is_english,
            term_seq,
            group_code,
            required,
            credits,
            grade,
            grade_value,
            passed,
        });
    }
    report.courses.sort_by(|a, b| {
        (a.yr.parse::<u32>().unwrap_or(0), a.sem.parse::<u32>().unwrap_or(0))
            .cmp(&(b.yr.parse::<u32>().unwrap_or(0), b.sem.parse::<u32>().unwrap_or(0)))
    });
}

// ─── 共用小工具 ─────────────────────────────────────────────────────────────

/// 取出「包含指定表頭文字」的表格並解析成列。
///
/// emis 的頁面慣例：資料表外還包一層版面表格，`extract_tables` 會把
/// 巢狀表先彈出來（LIFO），因此對每個表都試解析，找到含指定表頭者。
fn table_rows<'a>(html: &'a str, header_needle: &str) -> Option<Vec<Vec<Cell>>> {
    for table in extract_tables(html) {
        let rows = parse_table_cells(&table);
        if rows
            .iter()
            .any(|r| r.iter().any(|c| c.text.contains(header_needle)))
        {
            return Some(rows);
        }
    }
    None
}

/// 儲存格文字（單行化：內部換行與多餘空白壓成單一空格；`&nbsp;` 已轉空格）。
fn cell_plain(cell: &Cell) -> String {
    cell.text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 在 HTML 原文上做 regex，回傳第一個擷取群組（trim 後；無匹配為空字串）。
fn match_html_text(html: &str, pattern: &'static str) -> String {
    if let Ok(re) = regex::Regex::new(pattern) {
        if let Some(caps) = re.captures(html) {
            if let Some(m) = caps.get(1) {
                return m.as_str().trim().to_string();
            }
        }
    }
    String::new()
}

/// 取字串中第一個整數（例：「128(含…」→ 128）。
fn parse_u32_prefix(value: &str) -> Option<u32> {
    let digits: String = value
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// 「關鍵字＋冒號＋數字」的寬鬆抓法（中間可夾雜全形空白）。
///
/// 單位注意（NUMBER_AFTER_BYTES_V1）：`find` 回傳 **byte** 位置、`str::len()`
/// 也是 bytes，`&text[start..]` 是 byte slicing——三個單位一致才正確。
/// 先前版本誤用 `chars().count()`/`chars().skip(idx)`（字元數），keyword 前面
/// 有中文時偏移量會差 3 倍，抓到頁面後段的隨機數字（實際案例：必修 81 抓成
/// 19、累計實得 42 抓成 1）。
fn number_after(text: &str, keyword: &str) -> Option<u32> {
    let start = text.find(keyword)? + keyword.len();
    let rest = &text[start..];
    let digits: String = rest
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// 「字串中第一個數字」（回傳字串形式；例：「114」）。
fn first_number(text: &str) -> Option<String> {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_digit() {
            out.push(c);
        } else if !out.is_empty() {
            break;
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 解析成績欄。
///
/// emis 的成績格式（實測）：`87`（及格）、`*43`（星號＝不及格）、
/// `通過`（pass/fail 課，及格）、`不通過`。其他非數字文字（缺考等）
/// 一律視為未取得學分。
pub(super) fn parse_grade(raw: &str) -> (String, Option<f64>, bool) {
    let t = raw.trim();
    if t.is_empty() {
        return (String::new(), None, false);
    }
    if t == "通過" {
        return (t.to_string(), None, true);
    }
    if t == "不通過" {
        return (t.to_string(), None, false);
    }
    let (failed_marker, digits) = match t.strip_prefix('*') {
        Some(rest) => (true, rest),
        None => (false, t),
    };
    if let Ok(value) = digits.trim().parse::<f64>() {
        let passed = !failed_marker && value >= 60.0;
        return (t.to_string(), Some(value), passed);
    }
    // 非數字成績（「缺考」「休」等）：保留原文，不給學分。
    (t.to_string(), None, false)
}

// ─── 測試 ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grade_parsing_matches_emis_formats() {
        // 數字成績
        assert_eq!(parse_grade("87"), ("87".into(), Some(87.0), true));
        assert_eq!(parse_grade("  91  "), ("91".into(), Some(91.0), true));
        assert_eq!(parse_grade("60"), ("60".into(), Some(60.0), true));
        assert_eq!(parse_grade("59"), ("59".into(), Some(59.0), false));
        // 星號＝不及格（即使 ≥60）
        assert_eq!(parse_grade("*43"), ("*43".into(), Some(43.0), false));
        assert_eq!(parse_grade("*75"), ("*75".into(), Some(75.0), false));
        // pass/fail 課
        assert_eq!(parse_grade("通過"), ("通過".into(), None, true));
        assert_eq!(parse_grade("不通過"), ("不通過".into(), None, false));
        // 其他文字不給學分
        let (g, v, p) = parse_grade("缺考");
        assert_eq!((g.as_str(), v, p), ("缺考", None, false));
    }

    #[test]
    fn number_helpers() {
        assert_eq!(parse_u32_prefix("128(含必修81學分、本系選修19學分)"), Some(128));
        assert_eq!(number_after("必修學分數：81", "必修學分數"), Some(81));
        assert_eq!(number_after("畢業學分數：128　必修學分數：81", "必修學分數"), Some(81));
        assert_eq!(number_after("累計實得學分：45&nbsp;", "累計實得學分"), Some(45));
        assert_eq!(first_number("114"), Some("114".to_string()));
        assert_eq!(parse_u32_prefix("無數字"), None);
        // NUMBER_AFTER_BYTES_V1：keyword 前有中文（byte≠char 偏移）的案例。
        // 舊版（chars().skip 混 byte index）會把必修抓成 19、選修抓成 None。
        let rest = "含必修81學分、本系選修19學分)";
        assert_eq!(number_after(rest, "必修"), Some(81));
        assert_eq!(number_after(rest, "選修"), Some(19));
        // 頁首有長中文前綴（累計實得學分出現在頁面深處）也不可偏移。
        let page = "淡江大學歷年各科成績查詢系統學生資料頁首導覽列印本　累計實得學分：42&nbsp;&nbsp;( 實得學分不等於實得畢業學分)";
        assert_eq!(number_after(page, "累計實得學分"), Some(42));
    }

    #[test]
    fn parses_study_page_layout() {
        // TMWS080 縮樣：頁首 span + 「項目|內容|說明」表（說明欄含巢狀小表）。
        let html = r#"<span id="Label1">系級：資工英語二</span>
        <span id="Label2">學號：414850098</span>
        <span id="Label3">姓名：高浚祐   </span>
        <table id="Table1" rules="all" border="1">
        <tr><td align="center">項目</td><td align="center">內容</td><td align="center">說明/注意事項</td></tr>
        <tr><td>畢業審核學年度</td><td>114</td><td>畢業審核條件比照左列學年度入學新生。</td></tr>
        <tr><td>畢業應修學分數</td><td>128(含必修81學分、本系選修19學分)</td><td><a href="TMWC120.aspx">查看</a></td></tr>
        <tr><td>最低修業學期數</td><td>8</td><td><table><tr><td>(1)</td><td>須滿足最低修業學期數方可畢業。</td></tr></table></td></tr>
        <tr><td>已修業學期數<br />(含本學期)</td><td>3</td><td><table><tr><td>(1)</td><td>修業學期：已註冊且沒休(退)學之在學學期。</td></tr></table></td></tr>
        <tr><td>至多修業學期數</td><td>12</td><td>修業期限屆滿。</td></tr>
        </table>"#;
        let mut report = CreditReport {
            student: CreditStudent::default(),
            graduation: GraduationInfo::default(),
            earned_total: None,
            courses: Vec::new(),
            required_courses: Vec::new(),
            ge_domains: Vec::new(),
        };
        parse_study_page(html, &mut report);
        assert_eq!(report.student.dept, "資工英語二");
        assert_eq!(report.student.student_no, "414850098");
        assert_eq!(report.student.name, "高浚祐");
        assert_eq!(report.graduation.audit_year, "114");
        assert_eq!(report.graduation.total_credits, Some(128));
        assert_eq!(report.graduation.required_credits, Some(81));
        assert_eq!(report.graduation.dept_elective_min, Some(19));
        assert_eq!(report.graduation.min_semesters, Some(8));
        assert_eq!(report.graduation.completed_semesters, Some(3));
        assert_eq!(report.graduation.max_semesters, Some(12));
    }

    #[test]
    fn parses_required_page_layout() {
        // TMWC120 縮樣：表頭（年級 rowspan=2、學分 colspan=2）＋必修列＋通識學門列。
        let html = r#"畢業審核學年度：114　專業科目組別： 　畢業學分數：128　必修學分數：81　本系選修課最低學分：19</b></font><br /><br />
        <table><tr><td>必修科目：</td></tr><tr><td align="center">
        <table border="1" width="100%">
        <tr><td rowspan="2">年級</td><td rowspan="2">必修科目名稱</td><td rowspan="2">科號</td><td rowspan="2">專業組別</td><td rowspan="2">群別</td><td colspan="2" align="center">學分</td><td rowspan="2">說明</td></tr>
        <tr><td align="center">上</td><td align="center">下</td></tr>
        <tr><td align="center">一</td><td>中國語文能力表達</td><td align="center">A1376</td><td align="center">&nbsp;</td><td align="center">&nbsp;</td><td align="center">2</td><td align="center">&nbsp;</td><td>&nbsp;</td></tr>
        <tr><td align="center">一</td><td>機率統計</td><td align="center">E3764</td><td align="center">&nbsp;</td><td align="center">&nbsp;</td><td align="center">&nbsp;</td><td align="center">3</td><td>&nbsp;</td></tr>
        <tr><td align="center">一</td><td>英文（一）</td><td align="center">T0466</td><td align="center">&nbsp;</td><td align="center">&nbsp;</td><td align="center">2</td><td align="center">2</td><td>&nbsp;</td></tr>
        <tr><td align="center">一</td><td>文學經典</td><td align="center">&nbsp;</td><td align="center">&nbsp;</td><td align="center">L</td><td align="center">&nbsp;</td><td align="center">&nbsp;</td><td>請詳備註1、2</td></tr>
        <tr><td align="center">二</td><td>外國語文</td><td align="center">&nbsp;</td><td align="center">&nbsp;</td><td align="center">Q</td><td align="center">&nbsp;</td><td align="center">&nbsp;</td><td>請詳備註1、2</td></tr>
        </table></td></tr></table>"#;
        let mut report = CreditReport {
            student: CreditStudent::default(),
            graduation: GraduationInfo::default(),
            earned_total: None,
            courses: Vec::new(),
            required_courses: Vec::new(),
            ge_domains: Vec::new(),
        };
        parse_required_page(html, &mut report);
        assert_eq!(report.graduation.total_credits, Some(128));
        assert_eq!(report.graduation.required_credits, Some(81));
        assert_eq!(report.graduation.dept_elective_min, Some(19));

        assert_eq!(report.required_courses.len(), 3);
        let chinese = &report.required_courses[0];
        assert_eq!(chinese.name, "中國語文能力表達");
        assert_eq!(chinese.course_code, "A1376");
        assert_eq!(chinese.credits, 2);
        assert_eq!(chinese.term, "上");
        let prob = &report.required_courses[1];
        assert_eq!(prob.course_code, "E3764");
        assert_eq!(prob.term, "下");
        // 年課：上/下各 2 → 全年 4 學分（與成績單兩筆各 2 一致）。
        let english = &report.required_courses[2];
        assert_eq!(english.course_code, "T0466");
        assert_eq!(english.credits, 4);
        assert_eq!(english.term, "上下");

        assert_eq!(report.ge_domains.len(), 2);
        assert_eq!(report.ge_domains[0].code, "L");
        assert_eq!(report.ge_domains[0].name, "文學經典");
        assert_eq!(report.ge_domains[1].code, "Q");
        assert_eq!(report.ge_domains[1].name, "外國語文");
    }

    #[test]
    fn parses_transcript_page_layout() {
        // TMWS100 縮樣：11 欄資料列（及格/不及格/通過各一）＋累計實得學分。
        let html = r#"<b>系級：資工英語一　　學號：414850098　　姓名：高浚祐 累計實得學分：45&nbsp;&nbsp;( 實得學分不等於實得畢業學分)</b>
        <table><tr><td>歷年各科成績：</td></tr><tr><td align="center">
        <table border="1" width="100%">
        <tr><td align="center">學<br>年</td><td align="center">學<br>期</td><td align="center">開課<br>單位<br>代碼</td><td align="center">開課單位簡稱</td><td align="center">科號</td><td align="center">科目名稱<br />(科目名稱前方有◎者代表該科為全英語授課)</td><td align="center">學期<br>序</td><td align="center">群<br>別</td><td align="center">選<br>必<br>修</td><td align="center">學<br>分</td><td align="center">成績</td></tr>
        <tr><td align="center">114</td><td align="center">1</td><td align="center">TEIDB</td><td align="left">資工英語一</td><td align="center">A1376</td><td align="left"  >◎ 中國語文能力表達<br/>ABILITY OF EXPRESSING IN SPOKEN AND WRITTEN CHINESE</td><td align="center">0</td><td align="center">&nbsp;</td><td align="center">必修<br/>Required</td><td align="center">2</td><td align="center">   87</td></tr>
        <tr><td align="center">114</td><td align="center">1</td><td align="center">TGPNB</td><td align="left">三全共同科一</td><td align="center">T3154</td><td align="left"  >◎ 社會議題探索暨實踐<br/>EXPLORING AND IMPLEMENTING SOCIAL ISSUES</td><td align="center">1</td><td align="center">&nbsp;</td><td align="center">必修<br/>Required</td><td align="center">1</td><td align="center">*43</td></tr>
        <tr><td align="center">114</td><td align="center">1</td><td align="center">TEIDB</td><td align="left">資工英語一</td><td align="center">T0863</td><td align="left"  >◎ 大學學習<br/>LEARNING IN UNIVERSITY</td><td align="center">0</td><td align="center">N</td><td align="center">必修<br/>Required</td><td align="center">1</td><td align="center">通過</td></tr>
        <tr><td align="center">114</td><td align="center">2</td><td align="center">TNUZB</td><td align="left">全球科技學門</td><td align="center">S0926</td><td align="left"  >◎ 智慧綠生活<br/>PLANNING OF INTELLIGENT LIVING STYLE</td><td align="center">0</td><td align="center">Z</td><td align="center">必修<br/>Required</td><td align="center">2</td><td align="center">90</td></tr>
        <tr><td align="center">114</td><td align="center">2</td><td align="center">TGUPB</td><td align="left">體育</td><td align="center">T9873</td><td align="left"  >籃球<br/>BASKETBALL</td><td align="center">0</td><td align="center">&nbsp;</td><td align="center">必修<br/>Required</td><td align="center">1</td><td align="center">96</td></tr>
        </table></td></tr>
        <tr><td>共4筆</td></tr></table>"#;
        let mut report = CreditReport {
            student: CreditStudent::default(),
            graduation: GraduationInfo::default(),
            earned_total: None,
            courses: Vec::new(),
            required_courses: Vec::new(),
            ge_domains: Vec::new(),
        };
        parse_transcript_page(html, &mut report);
        assert_eq!(report.earned_total, Some(45));
        assert_eq!(report.courses.len(), 5, "只收資料列（表頭與共N筆不收）");
        // ◎ 前綴＝全英語授課（保留供全英語學士班採計規則）。前 4 筆有 ◎，籃球無。
        assert!(report.courses[0].is_english);
        assert!(report.courses[3].is_english);
        assert!(!report.courses[4].is_english);
        assert_eq!(report.courses[4].name, "籃球");

        let c0 = &report.courses[0];
        assert_eq!(c0.name, "中國語文能力表達");
        assert_eq!(c0.course_code, "A1376");
        assert!(c0.required);
        assert_eq!(c0.credits, 2);
        assert_eq!(c0.grade_value, Some(87.0));
        assert!(c0.passed);

        let c1 = &report.courses[1];
        assert_eq!(c1.grade, "*43");
        assert_eq!(c1.grade_value, Some(43.0));
        assert!(!c1.passed);

        let c2 = &report.courses[2];
        assert_eq!(c2.name, "大學學習");
        assert_eq!(c2.group_code, "N");
        assert_eq!(c2.grade, "通過");
        assert!(c2.passed);
        assert_eq!(c2.grade_value, None);

        let c3 = &report.courses[3];
        assert_eq!(c3.group_code, "Z");
        assert_eq!(c3.sem, "2");
    }

    #[test]
    fn report_serializes_for_api() {
        let report = CreditReport {
            student: CreditStudent {
                dept: "資工英語二".into(),
                student_no: "414850098".into(),
                name: "高浚祐".into(),
            },
            graduation: GraduationInfo {
                audit_year: "114".into(),
                total_credits: Some(128),
                required_credits: Some(81),
                dept_elective_min: Some(19),
                min_semesters: Some(8),
                max_semesters: Some(12),
                completed_semesters: Some(3),
            },
            earned_total: Some(45),
            courses: vec![CreditCourseRow {
                yr: "114".into(),
                sem: "1".into(),
                dept_code: "TEIDB".into(),
                dept_name: "資工英語一".into(),
                course_code: "A1376".into(),
                name: "中國語文能力表達".into(),
                term_seq: "0".into(),
                group_code: String::new(),
                required: true,
                credits: 2,
                grade: "87".into(),
                grade_value: Some(87.0),
                passed: true,
            }],
            required_courses: Vec::new(),
            ge_domains: vec![GeDomainRow {
                code: "L".into(),
                name: "文學經典".into(),
                grade_level: "一".into(),
            }],
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["student"]["student_no"], "414850098");
        assert_eq!(json["graduation"]["total_credits"], 128);
        assert_eq!(json["courses"][0]["name"], "中國語文能力表達");
        assert_eq!(json["ge_domains"][0]["code"], "L");
    }
}
