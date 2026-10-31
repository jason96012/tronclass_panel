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
use serde::{Deserialize, Serialize};
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CreditStudent {
    pub dept: String,
    pub student_no: String,
    pub name: String,
}

/// 畢業門檻（TMWS080/TMWC120）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

