//! Tronclass 作業與待辦事項 API 模組（完全重寫版）
//!
//! 專為 Tronclass（含淡江 iClass、輔仁、東海 iLearn 等各校實體架構）設計：
//! 1. 優先請求全域待辦 `/api/todos`（解析 `todo_list`）
//! 2. 請求學生修課清單 `/api/my-courses`（解析 `courses`）
//! 3. 併發讀取各課程作業 `/api/courses/{id}/homework-activities` 與活動 `/api/courses/{id}/activities`
//! 4. 具備高容錯多型資料解析（時間戳、繳交狀態、計分、遲交設定）

use chrono::{DateTime, Datelike, NaiveDateTime, TimeZone, Utc};
use futures::stream::StreamExt;
use miette::Result;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use tracing::{debug, instrument};

// ─── 常數 ─────────────────────────────────────────────────────────────────────

/// 學期起點再往前寬限的天數。
///
/// 避免開學前幾天發布、截止日落在正式開學日之前的作業被誤刪。
const SEMESTER_START_GRACE_DAYS: i64 = 14;

/// 同時查詢課程作業／活動的最大並行數。
///
/// 設為 8：足以把 20 次往返壓進 3 波，又不會像爆破那樣打爆學校 WAF。
/// 背景排程器仍會在帳號之間 sleep 1 秒，整體流量依舊平緩。
const COURSE_FETCH_CONCURRENCY: usize = 8;

/// 單次抓取 `/homework-activities` 的分頁大小。
///
/// SUBMISSION_PAGINATION_V1
/// 淡江 iClass 的 `/homework-activities` 是分頁的（預設 page_size = 10），
/// 而且「個別學生的繳交狀態 `submitted`」只在這個端點提供（`/activities`
/// 的 homework 項目 submitted 一律為 null）。因此必須做大分頁抓取，否則
/// 只拿到第 1 頁，絕大多數作業會因缺少繳交旗標而被誤判成「未繳交」。
const HOMEWORK_ACTIVITIES_PAGE_SIZE: u64 = 300;

// ─── 資料模型 ─────────────────────────────────────────────────────────────────

/// 統一標準化的 Tronclass 作業/測驗資料結構
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// 判斷作業是否已被學生繳交或已被老師評分
    pub fn is_completed_or_submitted(&self) -> bool {
        if self.is_submitted {
            return true;
        }
        let status = self.submission_status.trim().to_ascii_lowercase();
        matches!(
            status.as_str(),
            "submitted" | "graded" | "group_submitted" | "completed" | "pass" | "passed" | "done" | "finished"
        )
    }

    /// 判斷是否為進行中且未截止的待辦作業
    pub fn is_active_pending(&self, now: DateTime<Utc>) -> bool {
        if self.is_completed_or_submitted() {
            return false;
        }

        let Some(end) = self.end_time else {
            return false;
        };

        if end <= now {
            return false;
        }

        if let Some(start) = self.start_time {
            if start > now {
                return false;
            }
        }

        true
    }
}

// ─── API 客戶端重寫實作 ───────────────────────────────────────────────────────

impl super::ApiClient {
    /// 取得使用者的所有作業與待辦事項
    #[instrument(skip(self))]
    pub async fn get_todos(&self) -> Result<Vec<TronclassTodoItem>> {
        let mut results: Vec<TronclassTodoItem> = Vec::new();
        let mut seen_ids = HashSet::new();

        // HOMEWORK_INCLUDE_ALL_SEMESTERS_V1
        // 放寬旗標：若 homework.include_all_semesters = true，
        // 則「第一道防線（課程清單過濾）」與「第二道防線（學期起點）」
        // 都會被跳過，保留所有學期的作業與測驗（含上學期、已過期），
        // 供測試確認測驗（/exams）與舊作業是否被正確解析、圖示是否正確。
        // 預設 false 維持原行為。
        let include_all_semesters = crate::config::try_get_config()
            .map(|config| config.homework.include_all_semesters)
            .unwrap_or(false);

        // STRICT_CURRENT_SEMESTER_V1：嚴格模式旗標＋課程學期判定映射。
        // 映射在步驟 2（/api/my-courses）填入，最終過濾階段使用。
        let strict_current_semester = crate::config::try_get_config()
            .map(|config| config.homework.strict_current_semester)
            .unwrap_or(false);
        let mut course_current: HashMap<u64, Option<bool>> = HashMap::new();

        // ─────────────────────────────────────────────────────────────────────
        // 步驟 1: 查詢全域待辦清單 GET /api/todos
        // ─────────────────────────────────────────────────────────────────────
        let todos_url = format!("{}/api/todos", self.base_url);
        debug!(url = %todos_url, "正在查詢 Tronclass 全域待辦清單 /api/todos");

        if let Ok(resp) = self.client.get(&todos_url).send().await {
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED {
                return Err(miette::miette!(crate::api::ApiError::Unauthorized));
            }
            if status.is_success() {
                if let Ok(body_bytes) = resp.bytes().await {
                    if let Ok(json_val) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
                        let items = parse_todos_json(&json_val, None, None);
                        for item in items {
                            if seen_ids.insert(item.id) {
                                results.push(item);
                            }
                        }
                    }
                }
            }
        }

        // ─────────────────────────────────────────────────────────────────────
        // 步驟 2: 查詢學生修課清單 GET /api/my-courses 並獲取各科作業
        // ─────────────────────────────────────────────────────────────────────
        let my_courses_url = format!("{}/api/my-courses", self.base_url);
        debug!(url = %my_courses_url, "正在查詢學生修課清單 /api/my-courses");

        if let Ok(resp) = self.client.get(&my_courses_url).send().await {
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED {
                return Err(miette::miette!(crate::api::ApiError::Unauthorized));
            }
            if status.is_success() {
                if let Ok(body_bytes) = resp.bytes().await {
                    if let Ok(json_val) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
                        // CURRENT_SEMESTER_ONLY_V1
                        // 只保留當前學期仍在進行的課程，避免作業頁混入上學期／已封存
                        // 課程的舊作業。無法判斷學期的課程一律保留（寧可多顯示）。
                        //
                        // HOMEWORK_INCLUDE_ALL_SEMESTERS_V1
                        // 放寬時**跳過**這個課程過濾，讓所有課程（含非當前學期的舊課）
                        // 的作業與測驗都會被抓取——這是讓「舊作業/測驗」出現的
                        // 關鍵，否則那些含測驗/舊作業的課程會先被整門剔除，
                        // 它們的 /activities、/exams 根本不會被抓。
                        let all_courses = parse_course_briefs(&json_val);
                        let total_courses = all_courses.len();

                        // STRICT_CURRENT_SEMESTER_V1：記下每門課的學期判定，
                        // 供最終嚴格過濾（無截止時間項目）使用。
                        for course in &all_courses {
                            course_current.insert(course.id, course.is_current);
                        }

                        // CURRENT_SEMESTER_ONLY_V1（全域待辦回溯過濾）
                        // 步驟 1 的 /api/todos 不帶課程過濾：舊課的作業（尤其
                        // 是沒有截止時間、躲過第二道學期起點防線的項目）會從
                        // 這裡漏進作業頁。以這裡判定出的「非當前學期課程」id
                        // 集合回頭過濾步驟 1 已收集的項目；course_id 缺漏一律
                        // 保留（寧可多顯示）。放寬旗標開啟時一併跳過。
                        if !include_all_semesters {
                            let excluded_course_ids: HashSet<u64> = all_courses
                                .iter()
                                .filter(|course| course.is_current == Some(false))
                                .map(|course| course.id)
                                .collect();
                            let removed = retain_only_current_course_items(
                                &mut results,
                                &excluded_course_ids,
                            );
                            if removed > 0 {
                                debug!(
                                    removed,
                                    "已自全域待辦移除非當前學期課程的項目"
                                );
                            }
                        }

                        let courses: Vec<_> = if include_all_semesters {
                            all_courses
                        } else {
                            all_courses
                                .into_iter()
                                .filter(|course| course.is_current != Some(false))
                                .collect()
                        };
                        if courses.len() < total_courses {
                            debug!(
                                total = total_courses,
                                kept = courses.len(),
                                skipped = total_courses - courses.len(),
                                "已略過非當前學期課程"
                            );
                        }
                        // CONCURRENT_COURSE_FETCH_V1
                        // 原本是循序 for 迴圈，每門課要等 2 個請求依序回來；
                        // 9 門課 = 18 次往返，網路稍慢就要 5～10 秒，作業頁一直轉圈。
                        // 改用 buffer_unordered 同時發送，耗時由「總和」變成「最慢的那批」。
                        //
                        // EXAM_ENDPOINT_V1
                        // 淡江 iClass 的「測驗/考試」只透過 `/api/courses/{id}/exams` 提供
                        // （type = "exam"，含 QUIZ / Mid-Term / Final 等，皆帶 end_time）。
                        // 「作業/活動」端點只會回 homework/material/web_link/page/...，
                        // 不會有測驗；若漏抓 exams，測驗就永遠進不了待辦、也不會發截止提醒。
                        // 因此每門課多抓一筆 /exams。
                        let course_list: Vec<CourseBrief> = courses.into_iter().take(25).collect();
                        // SUBMISSION_PAGINATION_V1
                        // 第三個布林 = 是否要做分頁抓取。只有 `/homework-activities`
                        // 需要分頁（因為它帶正確的 submitted 旗標且預設只有 10 筆／頁）；
                        // `/activities` 的 homework 雖然也會回傳，但 submitted 一律為
                        // null（無法判斷繳交）——它只提供「項目存在」而非「是否已繳」。
                        let mut request_urls = Vec::with_capacity(course_list.len() * 3);
                        for course in &course_list {
                            request_urls.push((
                                format!(
                                    "{}/api/courses/{}/homework-activities",
                                    self.base_url, course.id
                                ),
                                course.id,
                                course.name.clone(),
                                true,
                            ));
                            request_urls.push((
                                format!("{}/api/courses/{}/activities", self.base_url, course.id),
                                course.id,
                                course.name.clone(),
                                false,
                            ));
                            request_urls.push((
                                format!("{}/api/courses/{}/exams", self.base_url, course.id),
                                course.id,
                                course.name.clone(),
                                false,
                            ));
                        }

                        // SUBMISSION_PAGINATION_V1
                        // paginate = true 的端點（homework-activities）會依 `pages` 遍歷所有頁，
                        // 確保拿到每一個 homework 及其正確的 submitted 旗標。
                        let fetched: Vec<Vec<TronclassTodoItem>> =
                            futures::stream::iter(request_urls.into_iter().map(
                                |(url, course_id, course_name, paginate)| async move {
                                    let mut all_items = Vec::new();
                                    let mut page: u64 = 1;
                                    loop {
                                        let effective_url = format!(
                                            "{url}?page={page}&page_size={HOMEWORK_ACTIVITIES_PAGE_SIZE}"
                                        );
                                        let Ok(resp) = self.client.get(&effective_url).send().await else {
                                            break;
                                        };
                                        if !resp.status().is_success() {
                                            break;
                                        }
                                        let Ok(bytes) = resp.bytes().await else {
                                            break;
                                        };
                                        let Ok(json) =
                                            serde_json::from_slice::<serde_json::Value>(&bytes)
                                        else {
                                            break;
                                        };
                                        all_items.extend(parse_todos_json(
                                            &json,
                                            Some(course_name.as_str()),
                                            Some(course_id),
                                        ));
                                        if !paginate {
                                            break;
                                        }
                                        // 依回傳的 pages 決定是否繼續下一頁（安全上限 200 頁）。
                                        let pages = json
                                            .get("pages")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(1);
                                        if page >= pages.max(1) || page >= 200 {
                                            break;
                                        }
                                        page += 1;
                                    }
                                    all_items
                                },
                            ))
                            .buffer_unordered(COURSE_FETCH_CONCURRENCY)
                            .collect()
                            .await;

                        // SUBMISSION_PAGINATION_V1
                        // buffer_unordered 的完成順序不固定，而且同一份作業會同時出現在
                        // homework-activities（submitted 正確）與 activities（submitted=null）
                        // 兩個端點。合併時**優先採用「已繳交」版本**：因為 activities 的
                        // 版本永遠是 submitted=false（欄位為 null），若任由它覆蓋，就會把
                        // 其實已繳交的作業誤判成未繳交。
                        let mut merged: HashMap<u64, TronclassTodoItem> = HashMap::new();
                        for items in fetched {
                            for item in items {
                                // SUBMISSION_PAGINATION_V1
                                // 用 Entry 的互斥分支，避免把 item 同時移動進 and_modify 閉包與
                                // or_insert（否則觸發 E0382）。occupied 分支只有在「新版本已繳交
                                // 且舊版未繳」或「新版本帶分數而舊版沒有」時才用新版本覆蓋，
                                // 否則保留舊版。（item 只在互斥的其中一個分支被移動，借用檢查器接受。）
                                // HOMEWORK_SCORE_PREFER_V1：`/homework-activities` 的版本帶有
                                // 使用者分數（`score`），`/activities` 與全域待辦的版本一律沒有；
                                // 完成順序不固定，若無分數的版本先進來就會把分數蓋掉，
                                // 因此「帶分數」也作為覆蓋條件之一。
                                match merged.entry(item.id) {
                                    std::collections::hash_map::Entry::Occupied(mut e) => {
                                        let prefer_new = (item.is_submitted && !e.get().is_submitted)
                                            || (item.score.is_some() && e.get().score.is_none());
                                        if prefer_new {
                                            e.insert(item);
                                        }
                                    }
                                    std::collections::hash_map::Entry::Vacant(v) => {
                                        v.insert(item);
                                    }
                                }
                            }
                        }
                        // HOMEWORK_SCORE_PREFER_V1：全域待辦（/api/todos）先進 results，
                        // 同一份作業若也出現在逐課端點，優先採「已繳交／帶分數」的版本，
                        // 避免 homework-activities 的分數被無分數的全域版本卡掉。
                        for (_, item) in merged {
                            if seen_ids.insert(item.id) {
                                results.push(item);
                            } else if let Some(existing) = results.iter_mut().find(|r| r.id == item.id) {
                                let prefer_new = (item.is_submitted && !existing.is_submitted)
                                    || (item.score.is_some() && existing.score.is_none());
                                if prefer_new {
                                    *existing = item;
                                }
                            }
                        }
                    }
                }
            }
        }

        // CURRENT_SEMESTER_ONLY_V1（第二道防線）
        // 有些學校的 /api/my-courses 不提供可判讀的學期欄位，光靠課程過濾擋不掉
        // 舊課。這裡再以「本學期起點」為界剔除上學期殘留的作業。
        //
        // 刻意不用「今天 − N 天」的滾動門檻：那會在學期中後期誤刪本學期稍早的
        // 逾期作業（例如 12 月時把 9 月的作業藏起來）。改用固定的學期起點，
        // 整個學期內的作業都看得到，跨學期才會被濾掉。
        // 沒有截止時間的項目一律保留，避免誤刪長期開放的作業。
        //
        // 放寬旗標（include_all_semesters）已在函式開頭讀取，並同時作用於
        // 第一道（課程清單）與此處第二道（學期起點）防線。
        if include_all_semesters {
            debug!("作業截止追蹤已放寬：包含所有學期（測試模式）");
        } else {
            let cutoff = current_semester_start(Utc::now());
            let before = results.len();
            results.retain(|item| match item.end_time {
                Some(end) => end >= cutoff,
                None => true,
            });
            if results.len() < before {
                debug!(
                    total = before,
                    kept = results.len(),
                    skipped = before - results.len(),
                    cutoff = %cutoff,
                    "已略過上一學期的舊作業"
                );
            }
        }

        // STRICT_CURRENT_SEMESTER_V1（第三道防線）
        // 把「寧可多顯示」反轉為「無法判定為當前學期＝隱藏」：
        // 沒有截止時間、且所屬課程不是確定的當前學期（None 或 Some(false)）
        // 的項目移除；有截止時間者仍以學期起點為界（上方已過濾）。
        // 修課清單抓取失敗（映射為空）時不套用，避免整頁清空；
        // 放寬旗標開啟時一併跳過。
        if strict_current_semester && !include_all_semesters && !course_current.is_empty() {
            let cutoff = current_semester_start(Utc::now());
            let before = results.len();
            results.retain(|item| {
                todo_item_is_confirmably_current(item, &course_current, cutoff)
            });
            if results.len() < before {
                debug!(
                    total = before,
                    kept = results.len(),
                    skipped = before - results.len(),
                    "嚴格模式：已隱藏無法判定為當前學期的作業"
                );
            }
        }

        // EXAM_SCORE_V1（測驗分數）
        // `/api/courses/{id}/exams` 的項目不含分數；分數在另一個端點
        // `/api/courses/{id}/exam-scores?no-intercept=true`，回應格式：
        //   {"exam_scores":[{"activity_id":166930,"score":77.5,"submission_scores":[...]}]}
        // 只對「結果中確實有測驗」的課程補抓一次，activity_id 即測驗項目 id，
        // 逐一比對後把分數寫回 item.score。未繳交的測驗不會出現在 exam_scores
        // 裡（保持 score = None → 前端顯示「未評分」）。失敗只記 log 不影響主流程。
        // 放在三道學期防線之後：只為「最終會顯示」的測驗抓分數，不浪費請求。
        let exam_course_ids: HashSet<u64> = results
            .iter()
            .filter(|item| item.item_type.eq_ignore_ascii_case("exam"))
            .filter_map(|item| item.course_id)
            .collect();
        if !exam_course_ids.is_empty() {
            let fetched_maps: Vec<HashMap<u64, f64>> = futures::stream::iter(
                exam_course_ids.into_iter().map(|course_id| async move {
                    let url = format!(
                        "{}/api/courses/{}/exam-scores?no-intercept=true",
                        self.base_url, course_id
                    );
                    let resp = match self.client.get(&url).send().await {
                        Ok(resp) => resp,
                        Err(err) => {
                            debug!(course_id, error = %err, "測驗分數請求失敗（略過）");
                            return HashMap::new();
                        }
                    };
                    if !resp.status().is_success() {
                        debug!(course_id, status = %resp.status(), "測驗分數端點非 2xx（略過）");
                        return HashMap::new();
                    }
                    let bytes = match resp.bytes().await {
                        Ok(bytes) => bytes,
                        Err(err) => {
                            debug!(course_id, error = %err, "測驗分數回應讀取失敗（略過）");
                            return HashMap::new();
                        }
                    };
                    let json: serde_json::Value = match serde_json::from_slice(&bytes) {
                        Ok(json) => json,
                        Err(err) => {
                            debug!(course_id, error = %err, "測驗分數 JSON 解析失敗（略過）");
                            return HashMap::new();
                        }
                    };
                    parse_exam_scores(&json)
                }),
            )
            .buffer_unordered(COURSE_FETCH_CONCURRENCY)
            .collect()
            .await;
            let exam_scores: HashMap<u64, f64> =
                fetched_maps.into_iter().flatten().collect();
            if exam_scores.is_empty() {
                debug!("測驗分數端點未回傳任何分數");
            } else {
                let mut applied = 0usize;
                for item in results.iter_mut() {
                    if item.item_type.eq_ignore_ascii_case("exam") {
                        if let Some(score) = exam_scores.get(&item.id) {
                            item.score = Some(*score);
                            applied += 1;
                        }
                    }
                }
                debug!(total = exam_scores.len(), applied, "已套用測驗分數");
            }
        }

        Ok(results)
    }
}

// ─── 高容錯 JSON 解析工具 ─────────────────────────────────────────────────────

/// 從任意 Tronclass JSON 節點提取作業陣列
pub fn parse_todos_json(
    root: &serde_json::Value,
    fallback_course_name: Option<&str>,
    fallback_course_id: Option<u64>,
) -> Vec<TronclassTodoItem> {
    let mut items = Vec::new();

    // 1. 若本身即為陣列 [ ... ]
    if let Some(arr) = root.as_array() {
        for val in arr {
            if let Some(item) = parse_single_todo(val, fallback_course_name, fallback_course_id) {
                items.push(item);
            }
        }
        return items;
    }

    // 2. 若為物件，搜尋標準 Key
    // EXAM_ENDPOINT_V1: "exams" 是測驗/考試端點的回應包裹鍵
    if let Some(obj) = root.as_object() {
        for key in [
            "todo_list",
            "homework_activities",
            "activities",
            "exams",
            "todos",
            "assignments",
            "items",
            "results",
        ] {
            if let Some(arr) = obj.get(key).and_then(|v| v.as_array()) {
                for val in arr {
                    if let Some(item) = parse_single_todo(val, fallback_course_name, fallback_course_id) {
                        items.push(item);
                    }
                }
                if !items.is_empty() {
                    return items;
                }
            }
        }

        // 3. 搜尋 data 封裝層
        if let Some(data_val) = obj.get("data") {
            if let Some(arr) = data_val.as_array() {
                for val in arr {
                    if let Some(item) = parse_single_todo(val, fallback_course_name, fallback_course_id) {
                        items.push(item);
                    }
                }
                if !items.is_empty() {
                    return items;
                }
            } else if let Some(data_obj) = data_val.as_object() {
                for key in [
                    "todo_list",
                    "homework_activities",
                    "activities",
                    "exams",
                    "todos",
                    "assignments",
                    "items",
                ] {
                    if let Some(arr) = data_obj.get(key).and_then(|v| v.as_array()) {
                        for val in arr {
                            if let Some(item) = parse_single_todo(val, fallback_course_name, fallback_course_id) {
                                items.push(item);
                            }
                        }
                        if !items.is_empty() {
                            return items;
                        }
                    }
                }
            }
        }
    }

    items
}

/// EXAM_SCORE_V1：解析 `/api/courses/{id}/exam-scores` 回應為
/// `activity_id → score` 對照表。
///
/// 實測格式：
/// ```json
/// {"exam_scores":[{"activity_id":166930,"score":77.5,
///   "submission_scores":[...]}]}
/// ```
/// - `activity_id`：測驗項目 id（等同 `/exams` 項目的 `id`）
/// - `score`：數字型分數；未評分時端點直接省略該筆（不會有 score=null）
/// - 混入未知欄位一律容忍（只取認識的兩個鍵）；格式不符回傳空 map，
///   呼叫端以空 map 視同「沒分數」，不影響主流程。
pub fn parse_exam_scores(root: &serde_json::Value) -> HashMap<u64, f64> {
    let mut map = HashMap::new();
    let Some(entries) = root.get("exam_scores").and_then(|v| v.as_array()) else {
        return map;
    };
    for entry in entries {
        let (Some(activity_id), Some(score)) = (
            entry.get("activity_id").and_then(parse_u64),
            entry.get("score").and_then(parse_f64),
        ) else {
            continue;
        };
        map.insert(activity_id, score);
    }
    map
}

/// 解析單一節點為標準 `TronclassTodoItem`
pub fn parse_single_todo(
    val: &serde_json::Value,
    fallback_course_name: Option<&str>,
    fallback_course_id: Option<u64>,
) -> Option<TronclassTodoItem> {
    let obj = val.as_object()?;

    // 1. 唯一標識 ID
    //
    // STABLE_FALLBACK_ID_V1
    // 這裡以前在缺少 id 欄位時用 rand::random()，每次輪詢都會產生不同 ID，
    // 導致 homework_reminder_sent 的主鍵永不重複 → 同一份作業被反覆通知。
    // 改用「課程 + 標題 + 截止時間」的穩定雜湊，讓同一份作業每次都得到相同 ID。
    let id = obj
        .get("id")
        .or_else(|| obj.get("activity_id"))
        .or_else(|| obj.get("todo_id"))
        .or_else(|| obj.get("homework_id"))
        .and_then(parse_u64)
        .unwrap_or_else(|| stable_fallback_id(obj));

    // 2. 標題
    let title = obj
        .get("title")
        .or_else(|| obj.get("name"))
        .or_else(|| obj.get("activity_name"))
        .or_else(|| obj.get("homework_title"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "未命名作業".to_string());

    // 3. 課程資訊
    let mut course_name = obj
        .get("course_name")
        .or_else(|| obj.get("course_title"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let mut course_id = obj.get("course_id").and_then(parse_u64);

    if let Some(c_obj) = obj.get("course").and_then(|c| c.as_object()) {
        if course_name.is_none() {
            course_name = c_obj
                .get("name")
                .or_else(|| c_obj.get("title"))
                .or_else(|| c_obj.get("course_name"))
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string());
        }
        if course_id.is_none() {
            course_id = c_obj.get("id").and_then(parse_u64);
        }
    }

    let course_name = course_name.or_else(|| fallback_course_name.map(str::to_string));
    let course_id = course_id.or(fallback_course_id);

    // 4. 系所
    let department_name = obj
        .get("department_name")
        .or_else(|| obj.get("department"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string());

    // 5. 類型 (homework, quiz, discussion, etc.)
    let item_type = obj
        .get("type")
        .or_else(|| obj.get("activity_type"))
        .or_else(|| obj.get("item_type"))
        .or_else(|| obj.get("category"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_else(|| "homework".to_string());

    // 6. 截止時間與開放時間
    let end_time = obj
        .get("end_time")
        .or_else(|| obj.get("deadline"))
        .or_else(|| obj.get("end_at"))
        .or_else(|| obj.get("due_at"))
        .or_else(|| obj.get("due_date"))
        .or_else(|| obj.get("end_date"))
        .or_else(|| obj.get("close_time"))
        .or_else(|| obj.get("submit_end_time"))
        .and_then(parse_datetime);

    let start_time = obj
        .get("start_time")
        .or_else(|| obj.get("start_at"))
        .or_else(|| obj.get("open_time"))
        .or_else(|| obj.get("begin_time"))
        .or_else(|| obj.get("start_date"))
        .and_then(parse_datetime);

    // 7. 繳交狀態判定
    let mut is_submitted = obj
        .get("is_submitted")
        .or_else(|| obj.get("submitted"))
        .or_else(|| obj.get("is_finished"))
        .or_else(|| obj.get("finished"))
        .and_then(parse_bool)
        .unwrap_or(false);

    let mut submission_status = obj
        .get("submission_status")
        .or_else(|| obj.get("status"))
        .or_else(|| obj.get("state"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    if let Some(sub_obj) = obj.get("submission").and_then(|s| s.as_object()) {
        if let Some(st) = sub_obj.get("status").and_then(|v| v.as_str()) {
            submission_status = st.trim().to_string();
        }
        if let Some(sub_b) = sub_obj.get("is_submitted").and_then(parse_bool) {
            is_submitted = is_submitted || sub_b;
        }
    }

    // EXAM_ENDPOINT_V1
    // 測驗（/exams）的項目不含 is_submitted / status；它的繳交紀錄統整在
    // `exam_submissions` 這個陣列裡：非空 = 該生已繳交（含 id），空 = 未繳交。
    // 收到幾筆已繳的測驗若判成未繳交，會反覆觸發 24h/6h/1h 提醒造成誤報，
    // 所以這裡把「非空 exam_submissions」視為已繳交。
    if let Some(exam_submissions) = obj.get("exam_submissions") {
        if matches!(exam_submissions, serde_json::Value::Array(arr) if !arr.is_empty()) {
            is_submitted = true;
            if submission_status.trim().is_empty() {
                submission_status = "submitted".to_string();
            }
        }
    }

    let status_lower = submission_status.to_ascii_lowercase();
    if matches!(
        status_lower.as_str(),
        "submitted" | "graded" | "group_submitted" | "completed" | "pass" | "passed" | "done" | "finished"
    ) {
        is_submitted = true;
    }

    if submission_status.is_empty() {
        submission_status = if is_submitted {
            "submitted".to_string()
        } else {
            "not_submitted".to_string()
        };
    }

    // 8. 遲交設定
    let allow_late_submission = obj
        .get("allow_late_submission")
        .or_else(|| obj.get("allow_late"))
        .or_else(|| obj.get("is_allow_late"))
        .and_then(parse_bool)
        .unwrap_or(false);

    // 9. 成績分數
    let score = obj
        .get("score")
        .or_else(|| obj.get("grade"))
        .or_else(|| obj.get("submission").and_then(|s| s.get("score")))
        .and_then(parse_f64);

    Some(TronclassTodoItem {
        id,
        title,
        course_id,
        course_name,
        department_name,
        item_type,
        start_time,
        end_time,
        is_submitted,
        submission_status,
        allow_late_submission,
        score,
    })
}

/// 從 `/api/my-courses` 解析出的單一課程。
#[derive(Debug, Clone)]
pub struct CourseBrief {
    pub id: u64,
    pub name: String,
    /// 課程是否仍在進行中。無法判斷時為 `None`（視為保留）。
    pub is_current: Option<bool>,
    /// 系所／班級名稱，來自 `/api/my-courses` 的 `department.name`。
    /// 用於教材匯入時的「分班資料夾」。無法取得時為 `None`。
    pub department: Option<String>,
}

/// 從 `/api/my-courses` 回應中解析課程清單 `(course_id, course_name)`
///
/// 保留給既有呼叫端與測試使用；新程式碼請改用 [`parse_course_briefs`]。
pub fn parse_course_list(root: &serde_json::Value) -> Vec<(u64, String)> {
    parse_course_briefs(root)
        .into_iter()
        .map(|course| (course.id, course.name))
        .collect()
}

/// 解析課程清單，並盡可能判斷每門課是否屬於「當前學期」。
///
/// Tronclass 各校欄位差異極大，因此採多層推斷，任何一層命中即採用；
/// 全部無法判斷時回傳 `None`，呼叫端會保守地保留該課程。
pub fn parse_course_briefs(root: &serde_json::Value) -> Vec<CourseBrief> {
    let mut courses = Vec::new();

    let arr = if let Some(a) = root.as_array() {
        Some(a)
    } else if let Some(obj) = root.as_object() {
        obj.get("courses")
            .or_else(|| obj.get("data"))
            .or_else(|| obj.get("items"))
            .and_then(|v| v.as_array())
    } else {
        None
    };

    if let Some(arr) = arr {
        for v in arr {
            if let Some(obj) = v.as_object() {
                if let Some(id) = obj.get("id").and_then(parse_u64) {
                    let name = obj
                        .get("name")
                        .or_else(|| obj.get("course_name"))
                        .or_else(|| obj.get("title"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("未命名課程")
                        .trim()
                        .to_string();
                    let department = obj
                        .get("department")
                        .and_then(|v| v.as_object())
                        .and_then(|d| d.get("name").and_then(extract_scalar_text))
                        .filter(|s| !s.trim().is_empty());
                    courses.push(CourseBrief {
                        id,
                        name,
                        is_current: detect_course_is_current(obj),
                        department,
                    });
                }
            }
        }
    }
    courses
}

/// 將課程清單過濾為「當前學期」課程並轉成 `(id, name)`（公告等子系統共用）。
///
/// 與 `get_todos` 的第一道防線同一條規則：`is_current == Some(false)` 剔除，
/// 無法判定（`None`）一律保留（寧可多顯示）。
pub fn current_course_pairs(briefs: Vec<CourseBrief>) -> Vec<(u64, String)> {
    briefs
        .into_iter()
        .filter(|course| course.is_current != Some(false))
        .map(|course| (course.id, course.name))
        .collect()
}

/// 以「已知非當前學期」的課程 id 集合過濾全域待辦項目，回傳移除數量。
///
/// `course_id` 缺漏（`None`）或不在集合內的項目一律保留——只刪「確定屬於
/// 舊課」的項目，寧可多顯示也不誤刪。
fn retain_only_current_course_items(
    items: &mut Vec<TronclassTodoItem>,
    excluded_course_ids: &HashSet<u64>,
) -> usize {
    let before = items.len();
    items.retain(|item| match item.course_id {
        Some(id) => !excluded_course_ids.contains(&id),
        None => true,
    });
    before - items.len()
}

/// STRICT_CURRENT_SEMESTER_V1：判定作業項目是否「可確認為當前學期」。
///
/// 有截止時間 → 以學期起點為界（`end >= cutoff` 顯示）；
/// 沒有截止時間 → 必須隸屬於「確定為當前學期」（`is_current == Some(true)`）
/// 的課程才顯示。`course_id` 缺漏、課程未知或無法判定（`None`）皆視為無法確認。
fn todo_item_is_confirmably_current(
    item: &TronclassTodoItem,
    course_current: &HashMap<u64, Option<bool>>,
    cutoff: DateTime<Utc>,
) -> bool {
    match item.end_time {
        Some(end) => end >= cutoff,
        None => matches!(
            item.course_id.and_then(|id| course_current.get(&id)),
            Some(Some(true))
        ),
    }
}

/// 判斷單一課程物件是否為當前學期課程。
///
/// 回傳 `None` 表示無從判斷 — 呼叫端應保留該課程，寧可多顯示也不要漏掉作業。
fn detect_course_is_current(obj: &serde_json::Map<String, serde_json::Value>) -> Option<bool> {
    let now = Utc::now();

    // 第 1 層：明確的封存 / 結束旗標。
    for key in ["is_archived", "archived", "is_ended", "ended", "is_closed"] {
        if let Some(true) = obj.get(key).and_then(parse_bool) {
            return Some(false);
        }
    }
    for key in ["is_current", "current", "is_active", "active", "in_progress"] {
        if let Some(flag) = obj.get(key).and_then(parse_bool) {
            return Some(flag);
        }
    }

    // 第 2 層：課程狀態字串。
    if let Some(status) = obj
        .get("status")
        .or_else(|| obj.get("course_status"))
        .or_else(|| obj.get("state"))
        .and_then(|v| v.as_str())
    {
        match status.trim().to_ascii_lowercase().as_str() {
            "archived" | "ended" | "finished" | "closed" | "completed" | "expired" => {
                return Some(false)
            }
            "ongoing" | "active" | "in_progress" | "published" | "current" => return Some(true),
            _ => {}
        }
    }

    // 第 3 層：課程結束日期已過。
    for key in ["end_date", "end_time", "course_end_date", "end_at", "close_date"] {
        if let Some(end) = obj.get(key).and_then(parse_datetime) {
            return Some(end > now);
        }
    }

    // 第 4 層：course_code 前綴的學期碼。
    // 實測 TKU 格式為 "1151TEIDB2A0050 1A"，前 4 碼 = 民國學年(3) + 學期(1)。
    // 這是目前最可靠的來源：academic_year 可能是巢狀物件、semester 常為 null。
    if let Some(code) = obj.get("course_code").and_then(|v| v.as_str()) {
        let digits: String = code
            .trim()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if digits.len() >= 4 {
            if let Some(flag) = semester_text_is_current(&digits[..4], now) {
                return Some(flag);
            }
        }
    }

    // 第 5 層：學年／學期欄位。這些在不同租戶可能是字串、數字，
    // 也可能是 {"name": "115"} 這種巢狀物件（TKU 實測即為後者）。
    for key in [
        "academic_year",
        "semester",
        "term",
        "school_year",
        "academic_term",
        "semester_name",
    ] {
        if let Some(text) = obj.get(key).and_then(extract_scalar_text) {
            if let Some(flag) = semester_text_is_current(&text, now) {
                return Some(flag);
            }
        }
    }

    // 第 6 層：課程起始日仍在未來 → 尚未開課，不算當前。
    if let Some(start) = obj.get("start_date").or_else(|| obj.get("start_time")) {
        if let Some(start) = parse_datetime(start) {
            if start > now {
                return Some(false);
            }
        }
    }

    None
}

/// 推算「當前學期起點」（台北時間），作為過濾舊作業的下界。
///
/// 台灣學制：上學期約 8 月開始、下學期約 2 月開始。
/// 回傳值會再往前寬限 [`SEMESTER_START_GRACE_DAYS`] 天。
///
/// CURRENT_SEMESTER_OVERRIDE_V1：設定 `[semester] current` 時以覆蓋為準
/// （見 [`current_semester_start_with`]）。
fn current_semester_start(now: DateTime<Utc>) -> DateTime<Utc> {
    current_semester_start_with(now, crate::config::semester_override())
}

/// [`current_semester_start`] 的可注入版本（測試用）。
///
/// `override_semester = Some((西元年, 學期 1|2))` 時直接以該學期起點
/// （上學期 8/1、下學期 2/1）為界，不依時鐘推算。
fn current_semester_start_with(
    now: DateTime<Utc>,
    override_semester: Option<(i32, u32)>,
) -> DateTime<Utc> {
    let taipei = match chrono::FixedOffset::east_opt(8 * 3600) {
        Some(tz) => tz,
        None => return now - chrono::Duration::days(180),
    };

    // CURRENT_SEMESTER_OVERRIDE_V1：覆蓋時以該學期起點為界。
    let (start_year, start_month) = match override_semester {
        Some((western_year, term)) => (western_year, if term == 1 { 8 } else { 2 }),
        None => {
            let local = now.with_timezone(&taipei);
            let (year, month) = (local.year(), local.month());

            // 8 月～隔年 1 月 → 上學期（起點為當年 8 月，1 月時為前一年 8 月）
            // 2 月～7 月       → 下學期（起點為當年 2 月）
            if month >= 8 {
                (year, 8)
            } else if month <= 1 {
                (year - 1, 8)
            } else {
                (year, 2)
            }
        }
    };

    chrono::NaiveDate::from_ymd_opt(start_year, start_month, 1)
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .and_then(|naive| taipei.from_local_datetime(&naive).single())
        .map(|dt| dt.with_timezone(&Utc) - chrono::Duration::days(SEMESTER_START_GRACE_DAYS))
        .unwrap_or_else(|| now - chrono::Duration::days(180))
}

/// 為缺少 id 欄位的作業產生穩定的替代 ID。
///
/// STABLE_FALLBACK_ID_V1
/// 以「課程 + 標題 + 截止時間」做雜湊：只要是同一份作業，每次輪詢都會得到
/// 相同的 ID，因此 `homework_reminder_sent` 的去重才能正常運作。
///
/// 最高位設為 1，避免與上游真實 ID 撞號。
fn stable_fallback_id(obj: &serde_json::Map<String, serde_json::Value>) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();

    for key in ["course_id", "course_name", "course_title"] {
        if let Some(text) = obj.get(key).and_then(extract_scalar_text) {
            text.hash(&mut hasher);
        }
    }
    for key in ["title", "name", "activity_name", "homework_title"] {
        if let Some(text) = obj.get(key).and_then(|v| v.as_str()) {
            text.trim().hash(&mut hasher);
            break;
        }
    }
    for key in ["end_time", "deadline", "end_at", "due_at", "due_date"] {
        if let Some(text) = obj.get(key).and_then(extract_scalar_text) {
            text.hash(&mut hasher);
            break;
        }
    }

    hasher.finish() | (1u64 << 63)
}

/// 從可能是字串／數字／巢狀物件的欄位中取出可判讀的文字。
///
/// TKU 的 `academic_year` 形如 `{"code":null,"id":26,"name":"115","sort":0}`，
/// 因此需要往下取 `name` / `real_name` / `code`。
fn extract_scalar_text(val: &serde_json::Value) -> Option<String> {
    match val {
        serde_json::Value::String(s) => {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        }
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Object(obj) => {
            for key in ["name", "real_name", "code", "value", "title"] {
                if let Some(text) = obj.get(key).and_then(extract_scalar_text) {
                    return Some(text);
                }
            }
            None
        }
        _ => None,
    }
}

/// 比對學期字串（如 `"114-1"`、`"2025 Fall"`、`"1141"`）是否為當前學期。
///
/// 同時支援民國年（台灣學制）與西元年；無法判讀時回傳 `None`。
///
/// CURRENT_SEMESTER_OVERRIDE_V1：設定 `[semester] current` 時以覆蓋的
/// 學期為比對基準（見 [`semester_text_is_current_with`]）。
fn semester_text_is_current(text: &str, now: DateTime<Utc>) -> Option<bool> {
    semester_text_is_current_with(text, now, crate::config::semester_override())
}

/// [`semester_text_is_current`] 的可注入版本（測試用）。
///
/// `override_semester = Some((西元年, 學期 1|2))` 時以該學期為比對基準，
/// 不依時鐘推算（跨學期交界學校資料未更新時，舊學期課程仍視為當前）。
fn semester_text_is_current_with(
    text: &str,
    now: DateTime<Utc>,
    override_semester: Option<(i32, u32)>,
) -> Option<bool> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    // 以台北時間為準推算當前學年度：8 月起算新學年，上學期為 1、下學期為 2。
    // CURRENT_SEMESTER_OVERRIDE_V1：有覆蓋時直接採用覆蓋值；
    // 學期轉 i32 與下方 parse 出的學期碼同型，供後續比較。
    let (western_year, term) = match override_semester {
        Some((western_year, term)) => (western_year, term as i32),
        None => {
            let taipei = chrono::FixedOffset::east_opt(8 * 3600)?;
            let local = now.with_timezone(&taipei);
            let year = local.year();
            let month = local.month();

            if month >= 8 {
                (year, 1)
            } else if month <= 1 {
                (year - 1, 1)
            } else {
                (year - 1, 2)
            }
        }
    };
    let roc_year = western_year - 1911;

    let digits: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }

    // 形如 "1141" / "114-1" / "114學年度第1學期"
    if let Some(found_year) = digits.get(..3).and_then(|s| s.parse::<i32>().ok()) {
        if (100..200).contains(&found_year) {
            let found_term = digits.get(3..4).and_then(|s| s.parse::<i32>().ok());
            return Some(found_year == roc_year && found_term.map_or(true, |t| t == term));
        }
    }

    // 形如 "2025-1" / "2025 Fall"
    if let Some(found_year) = digits.get(..4).and_then(|s| s.parse::<i32>().ok()) {
        if (2000..2100).contains(&found_year) {
            let lower = text.to_ascii_lowercase();
            let found_term = if lower.contains("fall") || lower.contains("上") {
                Some(1)
            } else if lower.contains("spring") || lower.contains("下") {
                Some(2)
            } else {
                digits.get(4..5).and_then(|s| s.parse::<i32>().ok())
            };
            return Some(found_year == western_year && found_term.map_or(true, |t| t == term));
        }
    }

    None
}

// ─── 基礎型別彈性轉換器 ───────────────────────────────────────────────────────

fn parse_u64(val: &serde_json::Value) -> Option<u64> {
    match val {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

fn parse_f64(val: &serde_json::Value) -> Option<f64> {
    match val {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn parse_bool(val: &serde_json::Value) -> Option<bool> {
    match val {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::Number(n) => Some(n.as_i64()? != 0),
        serde_json::Value::String(s) => {
            let s = s.trim().to_ascii_lowercase();
            match s.as_str() {
                "true" | "1" | "yes" | "t" | "submitted" | "done" => Some(true),
                "false" | "0" | "no" | "f" => Some(false),
                _ => None,
            }
        }
        _ => None,
    }
}

/// 支援 ISO 8601、本地時間字串 (Asia/Taipei UTC+8) 及 Unix Timestamp
pub fn parse_datetime(val: &serde_json::Value) -> Option<DateTime<Utc>> {
    match val {
        serde_json::Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }

            // 1. 標準 RFC 3339 / ISO 8601 UTC
            if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                return Some(dt.with_timezone(&Utc));
            }

            // 2. 本地時間格式 "YYYY-MM-DD HH:MM:SS" (預設 Asia/Taipei +08:00)
            if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
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

            // 4. "YYYY-MM-DDTHH:MM:SS" (無時區後綴)
            if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
                let offset = chrono::FixedOffset::east_opt(8 * 3600)?;
                return offset
                    .from_local_datetime(&naive)
                    .single()
                    .map(|dt| dt.with_timezone(&Utc));
            }

            // 5. 純日期格式 "YYYY-MM-DD"（Tronclass 的 start_date / end_date 用這種）
            //    視為當日 23:59:59（+08:00），符合「截止日當天仍可繳交」的語意。
            if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                let offset = chrono::FixedOffset::east_opt(8 * 3600)?;
                return date
                    .and_hms_opt(23, 59, 59)
                    .and_then(|naive| offset.from_local_datetime(&naive).single())
                    .map(|dt| dt.with_timezone(&Utc));
            }

            // 6. 純數字時間戳記字串 ("1788100000" / "1788100000000")
            if let Ok(i) = s.parse::<i64>() {
                return if i > 10_000_000_000 {
                    DateTime::from_timestamp_millis(i)
                } else {
                    DateTime::from_timestamp(i, 0)
                };
            }

            None
        }
        serde_json::Value::Number(num) => {
            if let Some(i) = num.as_i64() {
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
    fn test_parse_real_empty_todo_list() {
        let raw = r#"{"todo_list": []}"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let items = parse_todos_json(&val, None, None);
        assert_eq!(items.len(), 0);
    }

    #[test]
    fn test_parse_real_empty_homework_activities() {
        let raw = r#"{
            "end": 0,
            "homework_activities": [],
            "page": 0,
            "page_size": 10,
            "pages": 0,
            "start": 0,
            "total": 0
        }"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let items = parse_todos_json(&val, Some("計算機組織"), Some(263858));
        assert_eq!(items.len(), 0);
    }

    // ── 當前學期過濾 ─────────────────────────────────────────────────────────

    #[test]
    fn test_archived_course_is_not_current() {
        let raw = r#"{"courses":[
            {"id": 1, "name": "上學期課", "is_archived": true},
            {"id": 2, "name": "本學期課", "is_archived": false}
        ]}"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let courses = parse_course_briefs(&val);
        assert_eq!(courses[0].is_current, Some(false));
        assert_eq!(courses[1].is_current, Some(true));
    }

    #[test]
    fn test_course_status_string_detection() {
        let raw = r#"{"courses":[
            {"id": 1, "name": "已結束", "status": "archived"},
            {"id": 2, "name": "進行中", "status": "ongoing"},
            {"id": 3, "name": "未知狀態", "status": "whatever"}
        ]}"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let courses = parse_course_briefs(&val);
        assert_eq!(courses[0].is_current, Some(false));
        assert_eq!(courses[1].is_current, Some(true));
        // 無法判斷時必須回 None，讓呼叫端保留該課程
        assert_eq!(courses[2].is_current, None);
    }

    #[test]
    fn test_course_end_date_in_past_is_not_current() {
        let raw = r#"{"courses":[
            {"id": 1, "name": "去年結束", "end_date": "2020-06-30T00:00:00Z"}
        ]}"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let courses = parse_course_briefs(&val);
        assert_eq!(courses[0].is_current, Some(false));
    }

    #[test]
    fn test_course_without_any_hint_is_kept() {
        let raw = r#"{"courses":[{"id": 1, "name": "沒有任何學期資訊"}]}"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let courses = parse_course_briefs(&val);
        // 無從判斷 → None → 呼叫端保留
        assert_eq!(courses[0].is_current, None);
        assert_ne!(courses[0].is_current, Some(false));
    }

    /// 以 TKU `/api/my-courses` 的真實回應驗證整條判斷鏈。
    #[test]
    fn test_real_tku_course_payload_is_current() {
        let raw = r#"{"courses":[{
            "academic_year": {"code": null, "id": 26, "name": "115", "sort": 0},
            "course_code": "1151TEIDB2E0334 0A   ",
            "end_date": "2027-01-31",
            "start_date": "2026-08-01",
            "id": 263858,
            "name": "計算機組織",
            "semester": {"code": null, "id": 136, "name": null, "real_name": null, "sort": 0}
        }]}"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let courses = parse_course_briefs(&val);
        assert_eq!(courses.len(), 1);
        assert_eq!(courses[0].id, 263858);
        assert_eq!(courses[0].name, "計算機組織");
        // 結束日 2027-01-31 仍在未來 → 當前學期課程，必須保留
        assert_eq!(courses[0].is_current, Some(true));
    }

    /// 舊學期課程：結束日已過，必須被濾掉。
    #[test]
    fn test_real_tku_past_semester_course_is_filtered() {
        let raw = r#"{"courses":[{
            "academic_year": {"code": null, "id": 24, "name": "113", "sort": 0},
            "course_code": "1132TEIDB2E0334 0A   ",
            "end_date": "2025-01-31",
            "start_date": "2024-08-01",
            "id": 111111,
            "name": "上上學期的課"
        }]}"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let courses = parse_course_briefs(&val);
        assert_eq!(courses[0].is_current, Some(false));
    }

    /// `end_date` 缺席時，應退回 course_code 前綴判斷。
    #[test]
    fn test_course_code_prefix_fallback() {
        let raw = r#"{"courses":[
            {"id": 1, "name": "本學期", "course_code": "1151ABC0001 0A"},
            {"id": 2, "name": "舊學期", "course_code": "1132ABC0001 0A"}
        ]}"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let courses = parse_course_briefs(&val);
        assert_eq!(courses[0].is_current, Some(true));
        assert_eq!(courses[1].is_current, Some(false));
    }

    /// 巢狀 `academic_year` 物件必須能取出 name 來判斷。
    #[test]
    fn test_nested_academic_year_object() {
        assert_eq!(
            extract_scalar_text(&serde_json::json!({"code": null, "name": "115", "sort": 0})),
            Some("115".to_string())
        );
        // 全 null 的 semester 物件應回 None
        assert_eq!(
            extract_scalar_text(&serde_json::json!({"code": null, "name": null})),
            None
        );
    }

    /// Tronclass 的 `YYYY-MM-DD` 純日期格式必須解析成當日 23:59:59 (+08:00)。
    #[test]
    fn test_parse_date_only_format() {
        let parsed = parse_datetime(&serde_json::json!("2027-01-31"))
            .expect("date-only 格式必須可解析");
        let taipei = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let local = parsed.with_timezone(&taipei);
        assert_eq!(local.format("%Y-%m-%d %H:%M:%S").to_string(), "2027-01-31 23:59:59");
    }

    /// 學期起點門檻必須在整個學期內保持不變，
    /// 不能像滾動天數那樣在學期中後期誤刪本學期稍早的作業。
    #[test]
    fn test_semester_start_is_stable_within_semester() {
        let aug = DateTime::parse_from_rfc3339("2026-08-27T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let dec = DateTime::parse_from_rfc3339("2026-12-15T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // 8 月與 12 月同屬上學期 → 門檻必須相同
        assert_eq!(current_semester_start(aug), current_semester_start(dec));

        // 12 月時，9 月截止的本學期作業必須仍在門檻之後（不被濾掉）
        let sept_homework = DateTime::parse_from_rfc3339("2026-09-15T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(sept_homework >= current_semester_start(dec));

        // 上學期（6 月）的作業則必須被濾掉
        let june_homework = DateTime::parse_from_rfc3339("2026-06-15T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(june_homework < current_semester_start(dec));
    }

    /// 缺少 id 欄位時的替代 ID 必須穩定，否則提醒去重會失效並重複轟炸。
    #[test]
    fn test_stable_fallback_id_is_deterministic() {
        let raw = r#"{"homework_activities":[{
            "title": "第一次作業",
            "course_id": 263858,
            "deadline": "2026-09-01"
        }]}"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();

        let first = parse_todos_json(&val, Some("計算機組織"), Some(263858));
        let second = parse_todos_json(&val, Some("計算機組織"), Some(263858));

        assert_eq!(first.len(), 1);
        assert_eq!(
            first[0].id, second[0].id,
            "同一份作業重複解析必須得到相同 ID，否則去重失效"
        );
        assert!(
            first[0].id >> 63 == 1,
            "替代 ID 最高位應為 1，避免與上游真實 ID 撞號"
        );
    }

    /// 不同作業必須得到不同的替代 ID。
    #[test]
    fn test_stable_fallback_id_differs_per_item() {
        let raw = r#"{"homework_activities":[
            {"title": "作業一", "course_id": 1, "deadline": "2026-09-01"},
            {"title": "作業二", "course_id": 1, "deadline": "2026-09-01"}
        ]}"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let items = parse_todos_json(&val, Some("課"), Some(1));
        assert_eq!(items.len(), 2);
        assert_ne!(items[0].id, items[1].id, "不同作業不可共用同一個替代 ID");
    }

    #[test]
    fn test_roc_semester_text_matching() {
        // 2026-08-27 → 西元 2026 上學期 → 民國 115 學年度第 1 學期
        let now = DateTime::parse_from_rfc3339("2026-08-27T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(semester_text_is_current("115-1", now), Some(true));
        assert_eq!(semester_text_is_current("114-2", now), Some(false));
        assert_eq!(semester_text_is_current("113-1", now), Some(false));
    }

    // ── CURRENT_SEMESTER_OVERRIDE_V1 ─────────────────────────────────────────

    #[test]
    fn test_semester_start_with_override() {
        // 套用相同覆蓋時，起點與查詢時刻無關
        let a = DateTime::parse_from_rfc3339("2026-09-03T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let b = DateTime::parse_from_rfc3339("2027-01-15T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let fall = current_semester_start_with(a, Some((2026, 1)));
        assert_eq!(fall, current_semester_start_with(b, Some((2026, 1))));

        // 115-2（西元 2025 下學期）→ 起點 2025-02-01 台北 0 點，再寬限 14 天
        let taipei = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let expected = taipei
            .from_local_datetime(
                &chrono::NaiveDate::from_ymd_opt(2025, 2, 1)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap(),
            )
            .single()
            .unwrap()
            .with_timezone(&Utc)
            - chrono::Duration::days(SEMESTER_START_GRACE_DAYS);
        assert_eq!(current_semester_start_with(a, Some((2025, 2))), expected);

        // 2026-09 與 2027-01 的時鐘推算都落在 115-1（起點 2026-08-01），與覆蓋一致
        assert_eq!(fall, current_semester_start_with(a, None));
        assert_eq!(fall, current_semester_start_with(b, None));
    }

    #[test]
    fn test_semester_text_is_current_with_override() {
        let now = DateTime::parse_from_rfc3339("2026-09-03T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // 覆蓋為 114-2（西元 2025 下學期）：跨學期殘留的舊課要視為當前
        let over = Some((2025, 2));
        assert_eq!(
            semester_text_is_current_with("114-2", now, over),
            Some(true)
        );
        assert_eq!(
            semester_text_is_current_with("1141", now, over),
            Some(false)
        );
        assert_eq!(
            semester_text_is_current_with("115-1", now, over),
            Some(false)
        );
        // 西元年格式
        assert_eq!(
            semester_text_is_current_with("2025 Spring", now, over),
            Some(true)
        );
        // 未覆蓋＝依時鐘（2026-09 → 115-1）
        assert_eq!(
            semester_text_is_current_with("115-1", now, None),
            Some(true)
        );
    }

    #[test]
    fn test_current_course_pairs_and_global_todo_retro_filter() {
        // CURRENT_SEMESTER_ONLY_V1：Some(false) 剔除、None／Some(true) 保留
        let briefs = vec![
            CourseBrief { id: 1, name: "當前課".to_string(), is_current: Some(true), department: None },
            CourseBrief { id: 2, name: "舊課".to_string(), is_current: Some(false), department: None },
            CourseBrief { id: 3, name: "無法判定課".to_string(), is_current: None, department: None },
        ];
        let pairs = current_course_pairs(briefs);
        assert_eq!(pairs.len(), 2, "只剔除確定為舊課者：{pairs:?}");
        assert_eq!(pairs[0], (1, "當前課".to_string()));
        assert_eq!(pairs[1], (3, "無法判定課".to_string()));

        // 全域待辦回溯過濾：course_id 對應舊課 → 移除；None／未知課 → 保留
        let mk = |id: Option<u64>| TronclassTodoItem {
            id: id.unwrap_or(999),
            title: "作業".to_string(),
            course_id: id,
            course_name: None,
            department_name: None,
            item_type: "homework".to_string(),
            start_time: None,
            end_time: None,
            is_submitted: false,
            submission_status: String::new(),
            allow_late_submission: false,
            score: None,
        };
        let mut items = vec![mk(Some(1)), mk(Some(2)), mk(None), mk(Some(77))];
        let excluded: HashSet<u64> = [2u64].into_iter().collect();
        let removed = retain_only_current_course_items(&mut items, &excluded);
        assert_eq!(removed, 1);
        assert_eq!(items.len(), 3);
        assert!(items.iter().all(|it| it.course_id != Some(2)));
        // 空排除集合＝不移除任何項目
        let mut untouched = vec![mk(Some(2))];
        assert_eq!(retain_only_current_course_items(&mut untouched, &HashSet::new()), 0);
        assert_eq!(untouched.len(), 1);
    }

    #[test]
    fn test_strict_current_semester_gate() {
        let mk = |course_id: Option<u64>, end: Option<DateTime<Utc>>| TronclassTodoItem {
            id: 1,
            title: "作業".to_string(),
            course_id,
            course_name: None,
            department_name: None,
            item_type: "homework".to_string(),
            start_time: None,
            end_time: end,
            is_submitted: false,
            submission_status: String::new(),
            allow_late_submission: false,
            score: None,
        };
        let cutoff = DateTime::parse_from_rfc3339("2026-08-15T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut map = HashMap::new();
        map.insert(1u64, Some(true)); // 確定當前學期
        map.insert(2u64, Some(false)); // 確定舊課
        map.insert(3u64, None); // 無法判定
        let future = Some(cutoff + chrono::Duration::days(30));
        let past = Some(cutoff - chrono::Duration::days(30));
        // 有截止時間：以學期起點為界，與課程判定無關
        assert!(todo_item_is_confirmably_current(&mk(Some(2), future), &map, cutoff));
        assert!(!todo_item_is_confirmably_current(&mk(Some(1), past), &map, cutoff));
        // 無截止時間：只有「確定當前學期」課程的項目顯示
        assert!(todo_item_is_confirmably_current(&mk(Some(1), None), &map, cutoff));
        assert!(!todo_item_is_confirmably_current(&mk(Some(2), None), &map, cutoff));
        assert!(!todo_item_is_confirmably_current(&mk(Some(3), None), &map, cutoff));
        // course_id 缺漏或未知課程 → 無法確認 → 隱藏
        assert!(!todo_item_is_confirmably_current(&mk(None, None), &map, cutoff));
        assert!(!todo_item_is_confirmably_current(&mk(Some(99), None), &map, cutoff));
    }

    #[test]
    fn test_parse_course_list_backward_compatible() {
        let raw = r#"{"courses":[{"id": 263858, "name": "計算機組織"}]}"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let courses = parse_course_list(&val);
        assert_eq!(courses.len(), 1);
        assert_eq!(courses[0], (263858, "計算機組織".to_string()));
    }

    #[test]
    fn test_parse_populated_homework() {
        let raw = r#"{
            "homework_activities": [
                {
                    "id": 105234,
                    "title": "第一次作業：組合語言設計",
                    "course_id": 263858,
                    "deadline": 1788100000,
                    "submitted": false,
                    "allow_late": true,
                    "score": null
                }
            ]
        }"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let items = parse_todos_json(&val, Some("計算機組織"), Some(263858));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, 105234);
        assert_eq!(items[0].title, "第一次作業：組合語言設計");
        assert_eq!(items[0].course_name.as_deref(), Some("計算機組織"));
        assert!(!items[0].is_completed_or_submitted());
        assert!(items[0].allow_late_submission);
    }

    // ── 分數（EXAM_SCORE_V1 / HOMEWORK_SCORE_PREFER_V1）────────────────────

    #[test]
    fn test_parse_exam_scores_real_payload() {
        // 淡江 iClass 實測格式（course 254576 微積分，2026-09-11）
        let raw = r#"{
            "exam_scores": [
                {"activity_id": 165979, "score": 100.0, "submission_scores": []},
                {"activity_id": 166331, "score": 77.5, "submission_scores": []},
                {"activity_id": 166930, "score": 94.0}
            ]
        }"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let map = parse_exam_scores(&val);
        assert_eq!(map.len(), 3);
        assert_eq!(map.get(&165979), Some(&100.0));
        assert_eq!(map.get(&166331), Some(&77.5));
        assert_eq!(map.get(&166930), Some(&94.0));
        assert!(!map.contains_key(&166931)); // 未繳交的測驗不會出現
    }

    #[test]
    fn test_parse_exam_scores_tolerates_malformed_entries() {
        // 缺 activity_id / 分數非數字 / 缺 score 鍵 → 整筆略過，不影響其他筆
        let raw = r#"{
            "exam_scores": [
                {"score": 88.0},
                {"activity_id": 111, "score": "not-a-number"},
                {"activity_id": 222},
                {"activity_id": 333, "score": 60.0},
                "not-an-object"
            ]
        }"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let map = parse_exam_scores(&val);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get(&333), Some(&60.0));
    }

    #[test]
    fn test_parse_exam_scores_wrong_or_missing_container() {
        for raw in [
            r#"{"scores": [{"activity_id": 1, "score": 2.0}]}"#,
            r#"{"exam_scores": {}}"#,
            r#"{}"#,
            r#"[]"#,
        ] {
            let val: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert!(parse_exam_scores(&val).is_empty(), "應容忍格式：{raw}");
        }
    }

    #[test]
    fn test_parse_exam_scores_string_score() {
        // 部分學校實體會把分數序列化成字串；parse_f64 需能吃下
        let raw = r#"{"exam_scores": [{"activity_id": 7, "score": "86.0"}]}"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let map = parse_exam_scores(&val);
        assert_eq!(map.get(&7), Some(&86.0));
    }

    #[test]
    fn test_parse_homework_score_string_from_homework_activities() {
        // homework-activities 的 score 是字串（"90"）；確認既有管線能解析成 f64
        let raw = r#"{
            "homework_activities": [
                {
                    "id": 99001,
                    "title": "作業一",
                    "course_id": 254574,
                    "submitted": true,
                    "score": "90"
                }
            ]
        }"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let items = parse_todos_json(&val, Some("中國語文能力"), Some(254574));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].score, Some(90.0));
        assert!(items[0].is_completed_or_submitted());
    }

    #[test]
    fn test_parse_exam_item_has_no_score_until_merged() {
        // /exams 端點項目本身不含分數（score=None），
        // 分數由 get_todos 的 exam-scores 階段補上（見 EXAM_SCORE_V1）。
        let raw = r#"{
            "exams": [
                {
                    "id": 165979,
                    "title": "Quiz 1",
                    "type": "exam",
                    "course_id": 254576,
                    "end_time": 1788100000
                }
            ]
        }"#;
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let items = parse_todos_json(&val, Some("微積分"), Some(254576));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item_type, "exam");
        assert_eq!(items[0].score, None);
        assert_eq!(items[0].course_id, Some(254576));

        // 模擬 get_todos 合併：exam-scores 對照表寫回 item.score
        let score_raw = r#"{"exam_scores": [{"activity_id": 165979, "score": 100.0}]}"#;
        let score_val: serde_json::Value = serde_json::from_str(score_raw).unwrap();
        let mut map = parse_exam_scores(&score_val);
        if let Some(score) = map.remove(&165979) {
            let mut merged = items;
            for item in merged.iter_mut() {
                if item.item_type == "exam" && item.id == 165979 {
                    item.score = Some(score);
                }
            }
            assert_eq!(merged[0].score, Some(100.0));
        } else {
            panic!("exam-scores 應包含 165979");
        }
    }
}
