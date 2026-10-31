//! Tronclass 作業與待辦事項 API 模組（完全重寫版）
//!
//! 專為 Tronclass（含淡江 iClass、輔仁、東海 iLearn 等各校實體架構）設計：
//! 1. 優先請求全域待辦 `/api/todos`（解析 `todo_list`）
//! 2. 請求學生修課清單 `/api/my-courses`（解析 `courses`）
//! 3. 併發讀取各課程作業 `/api/courses/{id}/homework-activities` 與活動 `/api/courses/{id}/activities`
//! 4. 具備高容錯多型資料解析（時間戳、繳交狀態、計分、遲交設定）

use chrono::{DateTime, Datelike, NaiveDateTime, TimeZone, Utc};
use futures::stream::StreamExt;
use miette::{IntoDiagnostic, Result, WrapErr};
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
    /// HOMEWORK_PAGE_CURRENT_ONLY_V1（fix45）：這筆項目是否屬於「當前學期」。
    ///
    /// * `Some(true)`：判定為當學期（有截止時間且不早於本學期起點，或所屬課程可確認為當學期）。
    /// * `Some(false)`：判定為非當學期（所屬課程確定是舊課／已封存，或截止日早於本學期起點）。
    /// * `None`：無從判定（例如 `/api/my-courses` 抓不到、或該校沒有可判讀的學期欄位）。
    ///
    /// 作業頁預設**只顯示 `!= Some(false)`** 的項目（使用者可切換「全部學期」）；
    /// `None` 一律保留，寧可多顯示也不要誤藏使用者的作業。
    ///
    /// `#[serde(default)]`：舊的快照（SQLite）沒有這個欄位，反序列化必須照舊通過。
    #[serde(default)]
    pub semester_current: Option<bool>,
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

        // HOMEWORK_PAGE_CURRENT_ONLY_V1（fix45）：為每一筆項目標上「是否屬於當前學期」。
        //
        // 為什麼要另立旗標：上面三道防線是「依設定決定要不要過濾」（測試用的
        // include_all_semesters、以及為各校差異保留的 strict_current_semester），
        // 而作業頁的預設行為應該是**不論設定如何都只顯示當學期**。把判定結果隨資料
        // 送給前端（`semester_current`），前端就能用它當預設篩選，並讓使用者按需要
        // 切換到「全部學期」——比在伺服器端直接刪資料更透明（前端會顯示隱藏了幾筆）。
        //
        // 判定規則與第三道防線一致：
        //   有截止時間 → 以本學期起點（含 14 天寬限）為界；
        //   沒有截止時間 → 看所屬課程是否可確認為當前學期；
        //   無法判定（課程清單抓不到、該校沒有可判讀的學期欄位）→ None（保留）。
        // 放在三道防線之後、EXAM_SCORE_V1 之前：只為最終會回傳的項目標記。
        label_semester_current(
            &mut results,
            &course_current,
            current_semester_start(Utc::now()),
        );

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
        // 先留空，`get_todos()` 取得課程清單後統一判定（見 HOMEWORK_PAGE_CURRENT_ONLY_V1）。
        semester_current: None,
    })
}

// ─────────────────────────────────────────────────────────────
// HOMEWORK_DETAIL_V1（fix41）：面板內顯示作業詳情（說明＋附件）
//
// 目的：使用者「按一個按鈕就看到作業內容」，但**完全不經過 iClass 頁面代理**
// （代理從 fix22 到 fix40 反覆出問題；見 KB §15.5 起）。這裡改走「伺服器端用面板
// 已保存的 iClass session 直接抓資料、面板自己渲染」，因此：
//   * 不會碰任何瀏覽器 cookie（面板登入狀態零風險）
//   * 不需要 iClass 的 CSRF token（面板伺服器端呼叫自己帶 cookie）
//   * 上游 JSON 的形狀未知的部分一律「多鍵候選 + 深度掃描 + 找不到就優雅降級」
//
// 說明文字可能是老師編輯的 HTML：伺服器只回原文（另附純文字版），前端用
// DOMPurify 消毒後才 innerHTML（沒有 DOMPurify 就退回純文字）——與 bulletins.html
// 的既有做法一致。
// ─────────────────────────────────────────────────────────────

/// `/homework-activities` 逐頁掃描時的每頁筆數（詳情查找用；與清單抓取共用常數語意）。
pub const HOMEWORK_DETAIL_PAGE_SIZE: u64 = 100;
/// 詳情查找最多掃幾頁，避免單一請求打出數十次上游往返。
pub const HOMEWORK_DETAIL_MAX_PAGES: u64 = 6;
/// 深度掃描 JSON 時的節點預算（防止病態大的回應把 CPU 吃光）。
const DETAIL_SCAN_NODE_BUDGET: usize = 6000;
/// 說明文字長度上限（超過截斷；HTML 標籤也算在內）。
const DETAIL_DESCRIPTION_MAX_CHARS: usize = 60_000;
/// 一張作業最多列出幾個附件。
const DETAIL_MAX_ATTACHMENTS: usize = 30;

/// 作業附件（面板內可直接下載）。
#[derive(Debug, Clone, Serialize, Default)]
pub struct HomeworkAttachment {
    /// 上游 upload id——面板端只用它去 `/api/uploads/{id}/preview` 取檔。
    /// `None` 表示上游只給了檔名、沒有可用 id（前端顯示但不下載）。
    pub upload_id: Option<u64>,
    pub name: String,
    pub size: Option<u64>,
}

/// 作業詳情＝清單既有的欄位（`TronclassTodoItem`）＋說明與附件。
#[derive(Debug, Clone, Serialize)]
pub struct HomeworkDetail {
    #[serde(flatten)]
    pub item: TronclassTodoItem,
    /// 上游原始的說明 HTML（前端負責消毒）。找不到說明時為空字串。
    pub description_html: String,
    /// 說明是否含 `<img>`——前端據此提示「圖片請到 iClass 看」。
    pub has_images: bool,
    /// 是否為小組作業（上游 `submit_by_group`）。
    ///
    /// HOMEWORK_SUBMIT_V1：小組作業的繳交走 `/api/activities/{id}/groups/{gid}/submission`，
    /// 個人端點送出去會被上游拒絕或記到錯的地方——所以面板先擋下來（前端也一併提示）。
    pub submit_by_group: bool,
    pub attachments: Vec<HomeworkAttachment>,
    /// 這份資料是從哪個端點找到的（診斷用，不影響功能）。
    pub source: String,
}

/// 取出節點上的 id（多鍵候選；與 `parse_single_todo` 的順序一致）。
fn node_id(node: &serde_json::Value) -> Option<u64> {
    let obj = node.as_object()?;
    for key in ["id", "activity_id", "todo_id", "homework_id"] {
        if let Some(id) = obj.get(key).and_then(parse_u64) {
            return Some(id);
        }
    }
    None
}

/// 「這個節點有多像我們要的作業詳情」：有說明、有附件者優先。
/// 同一個 item_id 會同時出現在 `/api/todos`、`/homework-activities`、`/activities`
/// 等回應裡，其中只有部分帶完整說明／附件——取最豐富的那一份。
fn node_richness(node: &serde_json::Value) -> usize {
    let described = if parse_homework_description(node).is_empty() {
        0
    } else {
        1
    };
    described * 1000 + parse_homework_attachments(node).len()
}

fn detail_string_at(node: &serde_json::Value, path: &[&str]) -> Option<String> {
    let mut current = node;
    for key in path {
        current = current.get(*key)?;
    }
    if let Some(text) = current.as_str() {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
        return None;
    }
    // 少數回應把說明包成物件（例如 { "text": "…" }／{ "html": "…" }）。
    if let Some(obj) = current.as_object() {
        for key in ["text", "html", "content", "value", "description"] {
            if let Some(text) = obj.get(key).and_then(|v| v.as_str()) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    None
}

/// 取出作業說明（HTML）。候選鍵依「實測／最常見」排序，全部找不到回空字串。
pub fn parse_homework_description(node: &serde_json::Value) -> String {
    const CANDIDATES: &[&[&str]] = &[
        &["data", "description"],
        &["description"],
        &["data", "content"],
        &["content"],
        &["data", "requirement"],
        &["data", "requirements"],
        &["description_html"],
        &["body_html"],
        &["content_html"],
        &["data", "summary"],
        &["summary"],
    ];
    for path in CANDIDATES {
        if let Some(text) = detail_string_at(node, path) {
            let truncated = crate::text_utils::truncate_chars(&text, DETAIL_DESCRIPTION_MAX_CHARS);
            return truncated.to_string();
        }
    }
    String::new()
}

fn attachment_from_value(value: &serde_json::Value) -> Option<HomeworkAttachment> {
    // 只給檔名的字串形式（例如 attachments: ["a.pdf"]）。
    if let Some(name) = value.as_str() {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        return Some(HomeworkAttachment {
            upload_id: None,
            name: name.to_string(),
            size: None,
        });
    }
    let obj = value.as_object()?;
    let mut upload_id = None;
    for key in ["id", "upload_id", "attachment_id", "file_id", "uploadId"] {
        if let Some(id) = obj.get(key).and_then(parse_u64) {
            upload_id = Some(id);
            break;
        }
    }
    let mut name = String::new();
    for key in [
        "name",
        "file_name",
        "filename",
        "title",
        "display_name",
        "original_name",
    ] {
        if let Some(text) = obj.get(key).and_then(|v| v.as_str()) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                name = trimmed.to_string();
                break;
            }
        }
    }
    let mut size = None;
    for key in ["size", "file_size", "filesize", "bytes"] {
        if let Some(bytes) = obj.get(key).and_then(parse_u64) {
            size = Some(bytes);
            break;
        }
    }
    if name.is_empty() && upload_id.is_none() {
        return None;
    }
    if name.is_empty() {
        name = format!("附件 {}", upload_id.unwrap_or_default());
    }
    Some(HomeworkAttachment {
        upload_id,
        name,
        size,
    })
}

fn collect_attachments_from(value: &serde_json::Value, out: &mut Vec<HomeworkAttachment>) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                if out.len() >= DETAIL_MAX_ATTACHMENTS {
                    return;
                }
                if let Some(parsed) = attachment_from_value(item) {
                    out.push(parsed);
                }
            }
        }
        // 單一物件（只有一個附件時上游不一定包成陣列）。
        _ => {
            if out.len() < DETAIL_MAX_ATTACHMENTS {
                if let Some(parsed) = attachment_from_value(value) {
                    out.push(parsed);
                }
            }
        }
    }
}

/// 取出作業附件清單。多鍵候選＋`data.` 內外都找，並自動去重。
pub fn parse_homework_attachments(node: &serde_json::Value) -> Vec<HomeworkAttachment> {
    const CANDIDATES: &[&[&str]] = &[
        &["data", "attachments"],
        &["attachments"],
        &["data", "uploads"],
        &["uploads"],
        &["data", "files"],
        &["files"],
        &["data", "materials"],
        &["materials"],
        &["data", "attachment"],
        &["attachment"],
        &["data", "attachment_list"],
        &["attachment_list"],
    ];
    let mut out: Vec<HomeworkAttachment> = Vec::new();
    for path in CANDIDATES {
        let mut current = node;
        let mut found = true;
        for key in *path {
            match current.get(*key) {
                Some(next) => current = next,
                None => {
                    found = false;
                    break;
                }
            }
        }
        if found {
            collect_attachments_from(current, &mut out);
        }
    }
    // 去重：同一個 upload id 或同名者只留一份。
    let mut seen: Vec<String> = Vec::new();
    out.retain(|attachment| {
        let key = match attachment.upload_id {
            Some(id) => format!("id:{id}"),
            None => format!("name:{}", attachment.name.to_ascii_lowercase()),
        };
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
    out.truncate(DETAIL_MAX_ATTACHMENTS);
    out
}

/// 在任意 JSON 形狀中找出「這一份作業」最完整的那個節點。
///
/// 採廣度優先（先淺後深）、限制節點預算；同 id 的多份取「有說明／附件」者。
/// 完全找不到回 `None`（呼叫端據此優雅降級，而不是報錯）。
pub fn find_activity_node(root: &serde_json::Value, item_id: u64) -> Option<&serde_json::Value> {
    let mut queue: Vec<(&serde_json::Value, usize)> = vec![(root, 0)];
    let mut visited = 0usize;
    let mut best: Option<(&serde_json::Value, usize)> = None;
    while let Some((node, depth)) = queue.pop() {
        visited += 1;
        if visited > DETAIL_SCAN_NODE_BUDGET {
            break;
        }
        if depth <= 6 {
            if node_id(node) == Some(item_id) {
                let richness = node_richness(node);
                // 取最豐富的那一份；同分時保留較淺層（BFS 先到者）。
                if best.map(|(_, score)| richness > score).unwrap_or(true) {
                    best = Some((node, richness));
                }
            }
            match node {
                serde_json::Value::Object(map) => {
                    for value in map.values() {
                        queue.push((value, depth + 1));
                    }
                }
                serde_json::Value::Array(items) => {
                    for value in items {
                        queue.push((value, depth + 1));
                    }
                }
                _ => {}
            }
        }
    }
    best.map(|(node, _)| node)
}

/// HOMEWORK_DETAIL_V1：由上游節點組出詳情；基礎欄位沿用 `parse_single_todo`
/// （與清單一致的解析規則），再補上說明／附件。
pub fn build_homework_detail(
    node: &serde_json::Value,
    fallback_course_name: Option<&str>,
    fallback_course_id: Option<u64>,
    source: &str,
) -> Option<HomeworkDetail> {
    let item = parse_single_todo(node, fallback_course_name, fallback_course_id)?;
    let description_html = parse_homework_description(node);
    let lower = description_html.to_ascii_lowercase();
    let submit_by_group = node
        .get("submit_by_group")
        .and_then(|value| value.as_bool())
        .or_else(|| {
            node.get("data")
                .and_then(|data| data.get("submit_by_group"))
                .and_then(|value| value.as_bool())
        })
        .unwrap_or(false);
    Some(HomeworkDetail {
        item,
        has_images: lower.contains("<img"),
        submit_by_group,
        description_html,
        attachments: parse_homework_attachments(node),
        source: source.to_string(),
    })
}

/// HOMEWORK_DETAIL_V1：iClass 的作業深連結（直接開在 iClass 網域，不經面板代理）。
///
/// 與 `tc_go_handler` 使用同一個路徑格式（fix22 實測），變更時兩處必須同步：
/// `tc_proxy.rs` 的 `format!("/course/{course_id}/learning-activity/full-screen#{item_id}")`。
/// 註：這個連結需要瀏覽器自己有 iClass 登入；面板無法代為登入。
pub fn iclass_activity_deep_link(base_url: &str, course_id: u64, item_id: u64) -> String {
    format!(
        "{}/course/{course_id}/learning-activity/full-screen#{item_id}",
        base_url.trim_end_matches('/')
    )
}

/// HOMEWORK_DETAIL_V1：把一個候選節點併進 `best`（只留資訊量最多的那一份）。
///
/// COMPILE_FIX_V1（fix42 revB）：原本用閉包捕獲 `best`，但閉包的可變借用會活到
/// 作用域結束，後面 `best.is_some()`／`best.as_ref()` 的讀取就會撞借用檢查
/// （rustc E0502）。改成顯式傳 `&mut` 的自由函式，借用範圍只到呼叫那一行。
fn consider_homework_detail(
    best: &mut Option<HomeworkDetail>,
    node: &serde_json::Value,
    course_id: Option<u64>,
    item_id: u64,
    source: &str,
    course_name: Option<&str>,
) {
    let Some(detail) = build_homework_detail(node, course_name, course_id, source) else {
        return;
    };
    if detail.item.id != item_id {
        return;
    }
    // 評分方式：有說明 > 沒說明（權重 1000），其次附件越多越好。
    let new_score = if detail.description_html.is_empty() { 0 } else { 1000 }
        + detail.attachments.len();
    let better = match best.as_ref() {
        Some(current) => {
            let old_score = if current.description_html.is_empty() { 0 } else { 1000 }
                + current.attachments.len();
            new_score > old_score
        }
        None => true,
    };
    if better {
        *best = Some(detail);
    }
}

impl super::ApiClient {
    /// HOMEWORK_DETAIL_V1：抓單一作業的說明與附件。
    ///
    /// 查找順序（每一種都「拿到 JSON 且找到該 id」才算命中，否則換下一種）：
    ///   1. `GET /api/courses/{cid}/homework-activities/{id}`（單一資源；未實測存在性）
    ///   2. `GET /api/homework-activities/{id}`
    ///   3. `GET /api/courses/{cid}/homework-activities?page=N&page_size=100`（已實測端點，逐頁掃）
    ///   4. `GET /api/courses/{cid}/activities?page=N&page_size=100`（備援）
    ///   5. `GET /api/todos`（最後手段：說明／附件可能沒有，但至少標題時間齊全）
    ///
    /// 回 `Ok(None)` 代表「上游沒有這份作業的資料」——呼叫端顯示提示，不視為錯誤。
    #[instrument(skip(self))]
    pub async fn get_homework_detail(
        &self,
        course_id: Option<u64>,
        item_id: u64,
    ) -> Result<Option<HomeworkDetail>> {
        let mut best: Option<HomeworkDetail> = None;
        // ① 單一資源端點（不確定是否存在的兩條路徑；HTML／非 JSON 一律視為未命中）。
        if let Some(course_id) = course_id {
            let mut singles = vec![
                format!(
                    "{}/api/courses/{course_id}/homework-activities/{item_id}",
                    self.base_url
                ),
                format!("{}/api/homework-activities/{item_id}", self.base_url),
            ];
            singles.dedup();
            for url in singles {
                if let Some(json) = self.get_json_value(&url).await {
                    if let Some(node) = find_activity_node(&json, item_id) {
                        consider_homework_detail(&mut best, node, Some(course_id), item_id, "activity-single", None);
                        if best.is_some() {
                            return Ok(best);
                        }
                    }
                }
            }
        }

        // ② 課程層級的列表端點（逐頁掃描）。
        //
        // 分頁規則：每一頁都整份 BFS 找 item_id；**一旦命中就停止翻頁**——同一份作業
        // 只會落在其中一頁，後面的頁面是「別的作業」，繼續掃只是浪費上游往返。
        // 兩個端點（homework-activities／activities）都試，是因為兩者的 submitted
        // 與說明欄位完整度不同（fix23 SUBMISSION_PAGINATION_V1 的同一個已知現象）。
        if let Some(course_id) = course_id {
            for endpoint in ["homework-activities", "activities"] {
                let mut page = 1u64;
                loop {
                    let url = format!(
                        "{}/api/courses/{course_id}/{endpoint}?page={page}&page_size={HOMEWORK_DETAIL_PAGE_SIZE}",
                        self.base_url
                    );
                    let Some(json) = self.get_json_value(&url).await else {
                        break;
                    };
                    if let Some(node) = find_activity_node(&json, item_id) {
                        consider_homework_detail(&mut best, node, Some(course_id), item_id, endpoint, None);
                        break;
                    }
                    let pages = json.get("pages").and_then(|v| v.as_u64()).unwrap_or(1);
                    if page >= pages.max(1) || page >= HOMEWORK_DETAIL_MAX_PAGES {
                        break;
                    }
                    page += 1;
                }
                // 已經拿到說明或附件就不必再試其他端點（省上游往返）。
                if best.as_ref().is_some_and(|detail| {
                    !detail.description_html.is_empty() || !detail.attachments.is_empty()
                }) {
                    return Ok(best);
                }
            }
        }

        // ③ 全域待辦（最後手段）。
        let todos_url = format!("{}/api/todos", self.base_url);
        if let Some(json) = self.get_json_value(&todos_url).await {
            if let Some(node) = find_activity_node(&json, item_id) {
                consider_homework_detail(&mut best, node, course_id, item_id, "todos", None);
            }
        }

        Ok(best)
    }

    /// HOMEWORK_DETAIL_V1：抓一個端點並要求「真的是 JSON 物件」。
    ///
    /// iClass 對未知路徑可能回 SPA 的 HTML 殼（HTTP 200）——那種情況必須視為
    /// 未命中，否則後續解析會拿到整包 HTML。非 2xx 一律 `None`（不當錯誤）。
    async fn get_json_value(&self, url: &str) -> Option<serde_json::Value> {
        let resp = self.client.get(url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let bytes = resp.bytes().await.ok()?;
        let first = bytes.iter().copied().find(|b| !b.is_ascii_whitespace());
        if first != Some(b'{') && first != Some(b'[') {
            return None;
        }
        serde_json::from_slice::<serde_json::Value>(&bytes).ok()
    }
}

// ─── HOMEWORK_SUBMIT_V1（fix42）：面板內直接繳交作業 ─────────────────────────
//
// 2026-09-15 以真實 iClass（淡江）session 逐步實測確認的流程：
//   1. `POST /api/uploads`（JSON：name／size／parent_type／parent_id／is_scorm／is_wmpkg）
//      → 201，回一組 upload 佔位（`{id, name, upload_url, storage_type, transcoder, key}`）。
//   2. `PUT {upload_url}`（multipart，欄位名固定 `file`）→ 200，位元組進物件儲存。
//      淡江站台 `storage_type=S3`、`transcoder=MEDIA` 走的就是這一條；只有
//      `S3 + WRPC` 才需要再補打 `/internal-api/upload/callback/{id}`（iClass 前端
//      `isUploadCallback` 的判斷式），本模組不需要。
//   3. `POST /api/course/activities/{activity_id}/submissions`
//      body `{"comment": "…", "uploads": [upload_id…], "slides": [], "is_draft": false}`
//      → 建立一次繳交；再呼叫一次就是「重新繳交」，上游會保留新版本。
//      （同一條路徑的 `PUT` 是更新既有版本：OPTIONS 實測 Allow: PUT, OPTIONS, POST。）
//   4. `GET /api/course/activities/{activity_id}/students/{user_id}/submission`
//      讀回目前版本（沒繳交過時上游回 404，實測）。
//
// 為什麼由伺服器端代打：與點名（`api/rollcall.rs` 的
// `PUT /api/rollcall/{id}/answer_number_rollcall`）同一條路線——用已保存的登入
// session 直接寫入上游，瀏覽器完全不必碰 iClass 的 cookie，也不會再遇到
// 「跳轉到 iClass 後被登出」那類問題。

/// 繳交說明（`comment`）在面板端的上限。超過就整筆拒絕，不默默截斷。
pub const HOMEWORK_SUBMIT_MAX_COMMENT_CHARS: usize = 2000;

/// `POST /api/uploads` 的回應（只留面板會用到的欄位）。
#[derive(Debug, Clone, Deserialize)]
pub struct UploadTicket {
    pub id: u64,
    #[serde(default)]
    pub name: String,
    /// 上游給的簽章上傳網址（含 token，有時效性）。
    #[serde(default)]
    pub upload_url: String,
    #[serde(default)]
    pub storage_type: Option<String>,
    #[serde(default)]
    pub transcoder: Option<String>,
}

/// 繳交版本裡的一個檔案（回給前端顯示用）。
#[derive(Debug, Clone, Serialize)]
pub struct HomeworkSubmissionUpload {
    pub upload_id: Option<u64>,
    pub name: String,
    pub size: Option<u64>,
}

/// 目前繳交版本的摘要。刻意只挑「顯示用」欄位——上游回應裡還有
/// `created_by`（含系級、學號）、`submission_correct` 等，沒有必要整包送進瀏覽器。
#[derive(Debug, Clone, Default, Serialize)]
pub struct HomeworkSubmissionSummary {
    /// 上游的 `marked_submitted`（true = 真的送出了，false = 只存了草稿）。
    pub submitted: bool,
    pub submission_id: Option<u64>,
    pub submitted_at: Option<String>,
    pub created_at: Option<String>,
    pub marked_submitted: bool,
    pub is_draft: bool,
    pub is_resubmitted: bool,
    pub is_redo: bool,
    pub submit_by_instructor: bool,
    pub comment: String,
    #[serde(skip_serializing_if = "is_false_flag")]
    pub comment_truncated: bool,
    pub uploads: Vec<HomeworkSubmissionUpload>,
}

/// `serde(skip_serializing_if)` 用：false 時不輸出這個欄位。
fn is_false_flag(value: &bool) -> bool {
    !*value
}

fn value_str_at(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn value_bool_at(value: &serde_json::Value, key: &str) -> bool {
    value.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// 把上游的 submission JSON 收斂成面板要用的摘要（純函式，可單元測試）。
pub fn summarize_submission(value: &serde_json::Value) -> HomeworkSubmissionSummary {
    let uploads = value
        .get("uploads")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .map(|item| HomeworkSubmissionUpload {
                    upload_id: item.get("id").and_then(|v| v.as_u64()),
                    name: item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    size: item.get("size").and_then(|v| v.as_u64()),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let raw_comment = value
        .get("comment")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let comment = crate::text_utils::truncate_chars(raw_comment, HOMEWORK_SUBMIT_MAX_COMMENT_CHARS)
        .to_string();

    let marked_submitted = value_bool_at(value, "marked_submitted");
    HomeworkSubmissionSummary {
        submitted: marked_submitted,
        submission_id: value.get("id").and_then(|v| v.as_u64()),
        submitted_at: value_str_at(value, "submitted_at"),
        created_at: value_str_at(value, "created_at"),
        marked_submitted,
        is_draft: value_bool_at(value, "is_draft"),
        is_resubmitted: value_bool_at(value, "is_resubmitted"),
        is_redo: value_bool_at(value, "is_redo"),
        submit_by_instructor: value_bool_at(value, "submit_by_instructor"),
        comment_truncated: raw_comment.chars().count() > comment.chars().count(),
        comment,
        uploads,
    }
}

/// 上傳網址只允許 http/https，且不得指向本機或內網（SSRF 防線）。
///
/// `upload_url` 是上游回應給的，正常一定是學校自己的物件儲存
/// （實測站台為學校自有的物件儲存主機 `https://tc-media.<校網>/upload/file/{id}?…`）；
/// 仍然驗一次，避免上游回應被替換時，面板變成「帶著請求去戳內網」的跳板。
pub fn is_safe_upload_url(raw: &str) -> bool {
    let Ok(parsed) = url::Url::parse(raw) else {
        return false;
    };
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    // UPLOAD_URL_IPV6_BRACKET_V1：url::Url::host_str() 對 IPv6 會保留方括號（"[::1]"），
    // 不去掉就 parse 不成 IpAddr，會落入「一般網域」而放行 → 內網 IPv6 可繞過。
    let host = host
        .trim_end_matches('.')
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if host.is_empty() || host == "localhost" || host.ends_with(".localhost") {
        return false;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => {
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                // 255.255.255.255（廣播）
                || v4.octets() == [255, 255, 255, 255]
                // 0.0.0.0/8
                || v4.octets()[0] == 0)
        }
        Ok(std::net::IpAddr::V6(v6)) => {
            let segments = v6.segments();
            // IPv4 映射位址（::ffff:127.0.0.1）依 IPv4 規則判斷。
            let mapped_internal = v6.to_ipv4_mapped().is_some_and(|v4| {
                v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified()
            });
            !(v6.is_loopback()
                || v6.is_unspecified()
                || mapped_internal
                // fc00::/7 unique-local
                || (segments[0] & 0xfe00) == 0xfc00
                // fe80::/10 link-local
                || (segments[0] & 0xffc0) == 0xfe80)
        }
        // 不是 IP 字面值 → 是一般網域，交由 DNS／TLS 決定。
        Err(_) => true,
    }
}

impl super::ApiClient {
    /// HOMEWORK_SUBMIT_V1：向 iClass 建立 upload 佔位（位元組還沒上傳）。
    ///
    /// `name` 必須先過面板的檔名檢查（`safe_file_name`）；`size` 只用於上游預留，
    /// 真正的大小以實際 PUT 的內容為準。
    #[instrument(skip(self), fields(size = size))]
    pub async fn create_upload_ticket(&self, name: &str, size: u64) -> Result<UploadTicket> {
        let url = format!("{}/api/uploads", self.base_url);
        let body = serde_json::json!({
            "name": name,
            "size": size,
            // 上游前端在「作業繳交」情境帶的是空 parent，實測可行。
            "parent_type": serde_json::Value::Null,
            "parent_id": 0,
            "is_scorm": false,
            "is_wmpkg": false,
        });
        debug!(url = %url, "POST /api/uploads");
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .into_diagnostic()
            .wrap_err("POST /api/uploads failed")?;
        self.handle_response(resp).await
    }

    /// HOMEWORK_SUBMIT_V1：把位元組 PUT 到上游給的簽章網址（multipart，欄位名 `file`）。
    ///
    /// 淡江的儲存後端是 S3 但 `transcoder` 不是 `WRPC`，因此走的就是 iClass 前端
    /// `upload2Local()` 的通用分支：multipart PUT，不需要 callback。
    #[instrument(skip(self, bytes), fields(size = bytes.len()))]
    pub async fn put_upload_content(
        &self,
        ticket: &UploadTicket,
        name: &str,
        content_type: Option<&str>,
        bytes: Vec<u8>,
    ) -> Result<()> {
        if !is_safe_upload_url(&ticket.upload_url) {
            return Err(miette::miette!(
                "上游回傳的上傳網址不安全（非 http(s) 或指向內網），已中止繳交"
            ));
        }

        // COMPILE_FIX_V2（fix42 revC）：`Part::mime_str(self, mime)` 會**取走 self**
        // （reqwest 0.12.28：`pub fn mime_str(self, mime: &str) -> crate::Result<Part>`），
        // 所以不能寫成 `if let Ok(p) = part.mime_str(..) { part = p }`——解析失敗那條路徑
        // `part` 已被取走、又沒有重新賦值，後面再用 `part` 就是 rustc E0382。
        // 改成「先把 Content-Type 準備好，再當最後一步鏈上去」：`Part::headers(self, ..)`
        // 同樣取走 self，但它是鏈式呼叫的最後一步，不會再有後續使用。
        // （`mime` 為 None 時 reqwest 只會輸出我們放進 map 的那一個 content-type，
        //  不會重複——見 reqwest 原始碼 compute_headers 的組裝順序。）
        let mut part_headers = reqwest::header::HeaderMap::new();
        if let Some(content_type) = content_type.map(str::trim).filter(|v| !v.is_empty()) {
            // 上游只把 mime 當提示；不合法就整段不帶，不讓整筆繳交失敗。
            if let Ok(value) = reqwest::header::HeaderValue::from_str(content_type) {
                part_headers.insert(reqwest::header::CONTENT_TYPE, value);
            }
        }
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(name.to_string())
            .headers(part_headers);
        let form = reqwest::multipart::Form::new().part("file", part);

        let resp = self
            .client
            .put(&ticket.upload_url)
            .multipart(form)
            .send()
            .await
            .into_diagnostic()
            .wrap_err("PUT upload_url failed")?;
        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(miette::miette!(crate::api::ApiError::Unauthorized));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(miette::miette!(crate::api::ApiError::ApiStatus {
                status: status.as_u16(),
                body: crate::text_utils::truncate_chars(&body, 300).to_string(),
            }));
        }
        Ok(())
    }

    /// HOMEWORK_SUBMIT_V1：建立繳交（再次呼叫＝重新繳交，上游保留新版本）。
    ///
    /// 上游的驗證錯誤（逾期、已關閉、檔案不符規定…）會以非 2xx 回應，這裡原樣
    /// 轉成 `ApiStatus`，讓前端能顯示學校真正的理由。
    #[instrument(skip(self, upload_ids, comment), fields(upload_count = upload_ids.len()))]
    pub async fn create_homework_submission(
        &self,
        activity_id: u64,
        upload_ids: &[u64],
        comment: &str,
        is_draft: bool,
    ) -> Result<serde_json::Value> {
        let url = format!(
            "{}/api/course/activities/{activity_id}/submissions",
            self.base_url
        );
        let body = serde_json::json!({
            "comment": comment,
            "uploads": upload_ids,
            "slides": [],
            "is_draft": is_draft,
        });
        debug!(url = %url, uploads = upload_ids.len(), is_draft, "POST submissions");

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .into_diagnostic()
            .wrap_err("POST /api/course/activities/{id}/submissions failed")?;

        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(miette::miette!(crate::api::ApiError::Unauthorized));
        }
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(miette::miette!(crate::api::ApiError::ApiStatus {
                status: status.as_u16(),
                body: crate::text_utils::truncate_chars(&text, 500).to_string(),
            }));
        }
        if text.trim().is_empty() {
            // 上游若回 204／空 body，呼叫端仍可視為成功（前端之後會重讀狀態）。
            return Ok(serde_json::json!({}));
        }
        serde_json::from_str::<serde_json::Value>(&text).map_err(|_| {
            miette::miette!(crate::api::ApiError::Json(
                crate::text_utils::truncate_chars(&text, 300).to_string()
            ))
        })
    }

    /// HOMEWORK_SUBMIT_V1：讀回「我在這份作業的目前繳交版本」。
    ///
    /// 沒繳交過時上游回 404（實測）→ `Ok(None)`；若回 HTML（session 失效被導到
    /// 登入頁）則視為認證錯誤，讓呼叫端走重登重試。
    #[instrument(skip(self))]
    pub async fn get_my_homework_submission(
        &self,
        activity_id: u64,
        user_id: u64,
    ) -> Result<Option<serde_json::Value>> {
        let url = format!(
            "{}/api/course/activities/{activity_id}/students/{user_id}/submission",
            self.base_url
        );
        debug!(url = %url, "GET student submission");
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .into_diagnostic()
            .wrap_err("GET student submission failed")?;

        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(miette::miette!(crate::api::ApiError::Unauthorized));
        }
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(miette::miette!(crate::api::ApiError::ApiStatus {
                status: status.as_u16(),
                body: crate::text_utils::truncate_chars(&text, 300).to_string(),
            }));
        }
        if text.trim_start().starts_with('<') {
            // SPA 的 HTML 殼（未登入）——不是「沒繳交」，要讓上層重登。
            return Err(miette::miette!(crate::api::ApiError::Unauthorized));
        }
        if text.trim().is_empty() {
            return Ok(None);
        }
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(value) => Ok(Some(value)),
            Err(_) => Ok(None),
        }
    }
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

/// HOMEWORK_PAGE_CURRENT_ONLY_V1（fix45）：為整批項目標上「是否屬於當前學期」。
///
/// 作業頁預設只顯示「當學期」，但判斷所需的課程學期資訊只在伺服器端（`/api/my-courses`
/// 的解析結果）；這個函式把判定結果寫進 `TronclassTodoItem::semester_current`，
/// 隨清單一起送給前端，前端就能用它當預設篩選、並讓使用者切換到「全部學期」。
///
/// 規則（與第三道防線 `todo_item_is_confirmably_current` 相同）：
/// * 有截止時間 → `end >= cutoff`（cutoff ＝ 本學期起點，含 14 天寬限）。
/// * 沒有截止時間 → 所屬課程是否可確認為當前學期；課程未知／無法判定 → `None`。
fn label_semester_current(
    items: &mut [TronclassTodoItem],
    course_current: &HashMap<u64, Option<bool>>,
    cutoff: DateTime<Utc>,
) {
    for item in items {
        item.semester_current = match item.end_time {
            Some(end) => Some(end >= cutoff),
            None => match item.course_id.and_then(|id| course_current.get(&id)) {
                Some(flag) => *flag,
                None => None,
            },
        };
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
            // SEMESTER_AUTO_CALC_SHARED_V1：日期→學年度/學期的推算規則收在
            // config::academic_year_for()（原本這裡跟下面 semester_text_is_current_with、
            // tku.rs::current_yr_sem 各自重複一份），這裡只做「學年度/學期 →
            // 學期起點日曆日期」的轉換：第 1 學期起點＝該學年度 8 月，
            // 第 2 學期起點＝（學年度 + 1）年的 2 月（下學期開學月份落在隔年）。
            let local = now.with_timezone(&taipei);
            let (academic_year, term) = crate::config::academic_year_for(local.year(), local.month());
            if term == 1 {
                (academic_year, 8)
            } else {
                (academic_year + 1, 2)
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
            // SEMESTER_AUTO_CALC_SHARED_V1：見 config::academic_year_for() 說明。
            let taipei = chrono::FixedOffset::east_opt(8 * 3600)?;
            let local = now.with_timezone(&taipei);
            let (western_year, term) =
                crate::config::academic_year_for(local.year(), local.month());
            (western_year, term as i32)
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


