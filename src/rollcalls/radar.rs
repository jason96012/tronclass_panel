//! 雷達簽到模組
//!
//! 實現雷達（地理位置）簽到邏輯：
//! 0. RADAR_REFERENCE_PARITY_V1：先送一次「不帶座標的空答案」（抄 auto-rollcall-thu-tronclass）
//! 1. 再依序嘗試 `config.radar.default_coords` 中的預設座標（輸入頁送來的座標最優先）
//! 2. 若全部失敗，使用 API 回傳的 `distance` 值與圓交叉點/三邊測量算法計算候選座標
//! 3. 依序嘗試計算出的候選座標
//! 4. 最後用棋盤格從推估點一圈圈往外掃（同專案的「無界棋盤格」，我們加上半徑上限與節流）
//!
//! # 圓交叉點算法說明
//!
//! 當我們嘗試座標 P1 失敗，得到距離 d1（P1 到教室的距離）；
//! 再嘗試座標 P2 失敗，得到距離 d2（P2 到教室的距離）。
//!
//! 教室位置 C 同時位於：
//! - 以 P1 為圓心、d1 為半徑的圓上
//! - 以 P2 為圓心、d2 為半徑的圓上
//!
//! 兩圓交叉點即為 C 的候選位置（0、1 或 2 個交叉點）。
//! 若有兩個交叉點，主流程會保留兩者作為候選，而不是額外套用地理偏好。
//!
//! 詳細方程式可至 [Feature] 雷达签到 查看
//! https://github.com/KrsMt-0113/XMU-Rollcall-Bot/issues/9

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use miette::Result;
use tracing::{debug, info, instrument, warn};

// RADAR_INPUT_PAGE_V1（fix71）：雷達點名輸入頁送來的座標通道。
use crate::adapters::requests::{RadarPointInput, RadarPointReceiver};

use crate::api::{is_auth_error, rollcall::AttendanceResult, ApiClient};
use crate::config::RadarConfig;

// RADAR_RELAY_V1：雷達的實際送出也要跟著「每位間隔」走（與數字／QR 同一套游標）。
use super::group_submit::{wait_group_submit_turn, GroupSubmitKind};

/// RADAR_RELAY_V1：雷達接力等待的上限（秒）。
///
/// 與 RELAY_DEADLINE_BUDGET_V1 同一個精神：就算帳號很多、排到很後面，也不會等超過這個
/// 秒數才送出——寧可早點把座標送到學校，也不要等太久讓點名視窗關了。
const RADAR_RELAY_MAX_WAIT_SECS: u64 = 30;
const RADAR_RELAY_MAX_WAIT: Duration = Duration::from_secs(RADAR_RELAY_MAX_WAIT_SECS);

/// RADAR_RELAY_V1：真正打學校 API 之前先排隊（面板「多帳號點名送出節奏」的雷達欄位）。
///
/// 同一場點名**第一位立即送出**，之後每位間隔 `base + U(0, jitter)` 毫秒；同一個帳號在
/// 同一場點名的後續嘗試（重試、棋盤格往下一格）**沿用原本的排定時間**，所以不會每格都再等
/// 一輪，總等待時間有限（見 [`RADAR_RELAY_MAX_WAIT`]）。等待本身不影響成功與否，只是把送出的
/// 時間錯開，避免多帳號同時打學校。
async fn await_radar_submit_turn(provider: &str, rollcall_id: u64, account_id: &str) {
    let _ = wait_group_submit_turn(
        provider,
        rollcall_id,
        GroupSubmitKind::Radar,
        account_id,
        Some(RADAR_RELAY_MAX_WAIT),
    )
    .await;
}

// ─── 地理座標型別 ──────────────────────────────────────────────────────────────

/// 地理座標（緯度、經度），使用十進位度數（decimal degrees）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coordinate {
    /// 緯度（-90.0 ~ 90.0）
    pub latitude: f64,
    /// 經度（-180.0 ~ 180.0）
    pub longitude: f64,
}

impl Coordinate {
    pub fn new(latitude: f64, longitude: f64) -> Self {
        Self {
            latitude,
            longitude,
        }
    }

    /// 計算兩座標之間的距離（公尺），使用 Haversine 公式
    ///
    /// Haversine 公式適用於球面距離計算，對於幾公里內的距離誤差很小。
    pub fn distance_meters(&self, other: &Coordinate) -> f64 {
        haversine_distance_meters(
            self.latitude,
            self.longitude,
            other.latitude,
            other.longitude,
        )
    }

    /// 轉換為平面座標（公尺），以指定原點為基準
    ///
    /// 使用等角投影（equirectangular projection），近距離內足夠精確。
    pub fn to_cartesian_meters(&self, origin: &Coordinate) -> (f64, f64) {
        let earth_radius = 6_371_000.0_f64; // 公尺
        let lat_ref = origin.latitude.to_radians();

        let x = (self.longitude - origin.longitude).to_radians() * earth_radius * lat_ref.cos();
        let y = (self.latitude - origin.latitude).to_radians() * earth_radius;

        (x, y)
    }

    /// 從平面座標（公尺）轉回地理座標，以指定原點為基準
    pub fn from_cartesian_meters(x: f64, y: f64, origin: &Coordinate) -> Self {
        let earth_radius = 6_371_000.0_f64;
        let lat_ref = origin.latitude.to_radians();

        let delta_lon = x / (earth_radius * lat_ref.cos());
        let delta_lat = y / earth_radius;

        Self {
            latitude: origin.latitude + delta_lat.to_degrees(),
            longitude: origin.longitude + delta_lon.to_degrees(),
        }
    }
}

impl std::fmt::Display for Coordinate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "({:.6}, {:.6})", self.latitude, self.longitude)
    }
}

// ─── 雷達簽到結果 ─────────────────────────────────────────────────────────────

/// 雷達簽到的完整結果
#[derive(Debug, Clone)]
pub enum RadarResult {
    /// 簽到成功，附帶成功的座標
    Success { coord: Coordinate },

    /// RADAR_EMPTY_ANSWER_V1：空答案就被接受（這一輪完全沒有用到座標）
    SuccessEmpty,

    /// 所有嘗試都失敗（含最後一次失敗的距離）
    Failed {
        last_distance: Option<f64>,
        tried_coords: Vec<Coordinate>,
    },

    /// 發生不可恢復的錯誤（例如 session 過期）
    Error(String),
}

impl RadarResult {
    pub fn is_success(&self) -> bool {
        matches!(
            self,
            RadarResult::Success { .. } | RadarResult::SuccessEmpty
        )
    }
}

impl std::fmt::Display for RadarResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RadarResult::Success { coord } => write!(f, "雷達簽到成功，座標：{coord}"),
            RadarResult::SuccessEmpty => write!(f, "雷達簽到成功（空答案，未帶座標）"),
            RadarResult::Failed {
                last_distance,
                tried_coords,
            } => {
                write!(f, "雷達簽到失敗，嘗試了 {} 個座標", tried_coords.len())?;
                if let Some(d) = last_distance {
                    write!(f, "，最後距離：{d:.2} 公尺")?;
                }
                Ok(())
            }
            RadarResult::Error(e) => write!(f, "雷達簽到錯誤：{e}"),
        }
    }
}

// ─── 圓交叉點算法 ─────────────────────────────────────────────────────────────

/// 兩圓相交的結果
#[derive(Debug, Clone)]
pub enum CircleIntersection {
    /// 兩個交叉點
    Two(Coordinate, Coordinate),
    /// 一個交叉點（兩圓內切或外切）
    One(Coordinate),
    /// 無交叉點（兩圓不相交或一圓在另一圓內部）
    None,
    /// 無限多個交叉點（兩圓完全重疊）
    Infinite,
}

/// 計算兩個地理圓的交叉點
///
/// # 參數
/// - `center1`：第一個圓的圓心（地理座標）
/// - `radius1`：第一個圓的半徑（公尺）
/// - `center2`：第二個圓的圓心（地理座標）
/// - `radius2`：第二個圓的半徑（公尺）
///
/// # 算法說明
/// 1. 以 `center1` 為原點，將地理座標轉換為平面直角座標（公尺）
/// 2. 在平面上計算兩圓交叉點（標準代數方法）
/// 3. 將結果轉換回地理座標
///
/// # 數學推導
/// 設圓1：x² + y² = r1²（圓心在原點）
/// 設圓2：(x - cx)² + (y - cy)² = r2²
///
/// 展開圓2：x² - 2cx·x + cx² + y² - 2cy·y + cy² = r2²
/// 代入 x² + y² = r1²：
///   r1² - 2cx·x + cx² - 2cy·y + cy² = r2²
///   2cx·x + 2cy·y = r1² + cx² + cy² - r2²
///
/// 令 d² = cx² + cy²（兩圓心距離的平方）：
///   2cx·x + 2cy·y = r1² + d² - r2²  →  (★)
///
/// 設 a = (r1² - r2² + d²) / (2d)，這是圓1圓心沿兩圓心連線到「根軸」的距離
/// 垂直距離 h = √(r1² - a²)
///
/// 最終交叉點坐標：
///   x = a * (cx/d) ± h * (cy/d)
///   y = a * (cy/d) ∓ h * (cx/d)
pub fn circle_intersection(
    center1: &Coordinate,
    radius1: f64,
    center2: &Coordinate,
    radius2: f64,
) -> CircleIntersection {
    // 以 center1 為原點，轉換為平面座標
    let (cx, cy) = center2.to_cartesian_meters(center1);

    // 兩圓心距離
    let d = (cx * cx + cy * cy).sqrt();

    debug!(
        "圓交叉點計算：d={d:.2}m, r1={radius1:.2}m, r2={radius2:.2}m, center2=({cx:.2},{cy:.2})"
    );

    // 浮點比較容差
    const EPS: f64 = 1e-6;

    // 兩圓完全重疊
    if d < EPS && (radius1 - radius2).abs() < EPS {
        debug!("兩圓完全重疊，無限多交叉點");
        return CircleIntersection::Infinite;
    }

    // 兩圓不相交（距離過遠或一圓在另一圓內部）
    if d > radius1 + radius2 + EPS {
        debug!("兩圓不相交（d={d:.2} > r1+r2={:.2}）", radius1 + radius2);
        return CircleIntersection::None;
    }

    if d < (radius1 - radius2).abs() - EPS {
        debug!(
            "一圓在另一圓內部（d={d:.2} < |r1-r2|={:.2}）",
            (radius1 - radius2).abs()
        );
        return CircleIntersection::None;
    }

    // 計算 a：圓1圓心沿連線方向到根軸的有符號距離
    let a = (radius1 * radius1 - radius2 * radius2 + d * d) / (2.0 * d);

    // 計算 h²：交叉點到根軸的距離的平方
    let h_sq = radius1 * radius1 - a * a;

    // 方向向量（單位向量）
    let ux = cx / d;
    let uy = cy / d;

    // 根軸上的中間點（平面座標）
    let mx = a * ux;
    let my = a * uy;

    // 一個交叉點（兩圓內切或外切）
    if h_sq <= EPS {
        let coord = Coordinate::from_cartesian_meters(mx, my, center1);
        debug!("兩圓相切，一個交叉點：{coord}");
        return CircleIntersection::One(coord);
    }

    let h = h_sq.sqrt();

    // 兩個交叉點
    let p1x = mx + h * uy;
    let p1y = my - h * ux;
    let p2x = mx - h * uy;
    let p2y = my + h * ux;

    let coord1 = Coordinate::from_cartesian_meters(p1x, p1y, center1);
    let coord2 = Coordinate::from_cartesian_meters(p2x, p2y, center1);

    debug!("兩圓有兩個交叉點：{coord1} 和 {coord2}");

    CircleIntersection::Two(coord1, coord2)
}

/// 從多個（圓心, 半徑）對中，用最小二乘法估算最可能的教室位置
///
/// 當只有兩個量測時，若只有單一交點則返回該點；
/// 若出現兩個交點，因資訊不足無法判定唯一位置，返回 `None`。
/// 有三個以上量測時，使用迭代最小二乘法求解非線性方程。
pub fn estimate_location_from_distances(measurements: &[(Coordinate, f64)]) -> Option<Coordinate> {
    if measurements.is_empty() {
        return None;
    }

    if measurements.len() == 1 {
        // 只有一個量測，無法確定方向，返回圓心本身（最壞情況）
        warn!("只有一個量測點，無法確定教室位置");
        return Some(measurements[0].0);
    }

    if measurements.len() == 2 {
        let (c1, r1) = &measurements[0];
        let (c2, r2) = &measurements[1];

        return match circle_intersection(c1, *r1, c2, *r2) {
            CircleIntersection::Two(p1, p2) => {
                debug!("從兩個量測點得到兩個交叉點：{p1} 和 {p2}，無法唯一判定");
                None
            }
            CircleIntersection::One(p) => {
                debug!("兩圓相切，交叉點：{p}");
                Some(p)
            }
            CircleIntersection::None => {
                warn!("兩圓無交叉點，嘗試使用中點");
                // 退化處理：取兩圓心的加權中點
                let mid = weighted_midpoint(c1, *r1, c2, *r2);
                Some(mid)
            }
            CircleIntersection::Infinite => Some(measurements[0].0),
        };
    }

    // 三個以上：使用線性最小二乘法（以第一個量測為基準，消去非線性項）
    trilateration_least_squares(measurements)
}

/// 三邊測量最小二乘法（三個以上量測點）
///
/// 算法：將非線性方程線性化
/// 以量測點 0 為基準，對其他每個量測點 i 列方程：
///   ri² - r0² = (xi² + yi²) - 2(xi - x0)x - 2(yi - y0)y - (x0² + y0²)
/// 整理後得到線性方程組 Ax = b，用最小二乘法求解。
fn trilateration_least_squares(measurements: &[(Coordinate, f64)]) -> Option<Coordinate> {
    let origin = &measurements[0].0;
    let (x0, y0) = (0.0_f64, 0.0_f64); // 第一個點作為原點
    let r0 = measurements[0].1;

    let n = measurements.len() - 1;
    let mut a_mat = vec![vec![0.0_f64; 2]; n];
    let mut b_vec = vec![0.0_f64; n];

    for (i, (coord, ri)) in measurements.iter().skip(1).enumerate() {
        let (xi, yi) = coord.to_cartesian_meters(origin);
        a_mat[i][0] = 2.0 * (xi - x0);
        a_mat[i][1] = 2.0 * (yi - y0);
        b_vec[i] = ri * ri - r0 * r0 - xi * xi - yi * yi + x0 * x0 + y0 * y0;
        // 注意：消去 x² + y² 項（以第一點為原點，x0=y0=0）
        b_vec[i] = xi * xi + yi * yi - ri * ri + r0 * r0;
    }

    // 用正規方程 (AᵀA)x = Aᵀb 求解（最小二乘法）
    let (x_est, y_est) = solve_least_squares_2d(&a_mat, &b_vec)?;

    let coord = Coordinate::from_cartesian_meters(x_est, y_est, origin);
    debug!("三邊測量估算位置：{coord}");
    Some(coord)
}

/// 解 2D 最小二乘線性方程組（直接解析解）
fn solve_least_squares_2d(a: &[Vec<f64>], b: &[f64]) -> Option<(f64, f64)> {
    // 計算 AᵀA 和 Aᵀb
    let mut ata = [[0.0_f64; 2]; 2];
    let mut atb = [0.0_f64; 2];

    for (row, bi) in a.iter().zip(b.iter()) {
        ata[0][0] += row[0] * row[0];
        ata[0][1] += row[0] * row[1];
        ata[1][0] += row[1] * row[0];
        ata[1][1] += row[1] * row[1];
        atb[0] += row[0] * bi;
        atb[1] += row[1] * bi;
    }

    // 2×2 矩陣求逆：det = ad - bc
    let det = ata[0][0] * ata[1][1] - ata[0][1] * ata[1][0];

    if det.abs() < 1e-10 {
        warn!("最小二乘法矩陣接近奇異（det={det:.2e}），無法求解");
        return None;
    }

    let inv_det = 1.0 / det;
    let x = inv_det * (ata[1][1] * atb[0] - ata[0][1] * atb[1]);
    let y = inv_det * (ata[0][0] * atb[1] - ata[1][0] * atb[0]);

    Some((x, y))
}

/// 計算兩個圓心的加權中點（當兩圓不相交時的退化處理）
///
/// 加權方式：半徑較小的圓心權重較高（較精確）
fn weighted_midpoint(c1: &Coordinate, r1: f64, c2: &Coordinate, r2: f64) -> Coordinate {
    // 反比加權：r 越小，權重越大
    let w1 = if r1 > 0.0 { 1.0 / r1 } else { 1.0 };
    let w2 = if r2 > 0.0 { 1.0 / r2 } else { 1.0 };
    let total = w1 + w2;

    Coordinate {
        latitude: (c1.latitude * w1 + c2.latitude * w2) / total,
        longitude: (c1.longitude * w1 + c2.longitude * w2) / total,
    }
}

// ─── Haversine 公式 ───────────────────────────────────────────────────────────

/// 用 Haversine 公式計算兩個地理座標之間的距離（公尺）
///
/// # 精度
/// 對於幾公里內的距離，誤差通常小於 0.5%。
/// 地球半徑取 6,371,000 公尺（WGS-84 平均半徑）。
pub fn haversine_distance_meters(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const EARTH_RADIUS: f64 = 6_371_000.0; // 公尺

    let phi1 = lat1.to_radians();
    let phi2 = lat2.to_radians();
    let dphi = (lat2 - lat1).to_radians();
    let dlambda = (lon2 - lon1).to_radians();

    let a = (dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlambda / 2.0).sin().powi(2);

    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());

    EARTH_RADIUS * c
}

// ─── 主雷達簽到函式 ───────────────────────────────────────────────────────────

/// 對指定的 rollcall 執行雷達簽到
///
/// # 策略
/// 0. RADAR_INPUT_PAGE_V1（fix71）：**輸入頁送來的座標優先**——使用者按下「送出座標」就代表
///    他知道自己（或教室）在哪，比任何推算都可靠，所以先試它（同一組不重複試）。
/// 1. 依序嘗試 `config.default_coords` 中的預設座標
/// 2. 若均失敗，收集各次失敗回傳的 `distance`，用圓交叉點算法計算候選座標
/// 3. 嘗試計算出的候選座標（最多嘗試 3 個）
/// 4. RADAR_REFERENCE_PARITY_V1：先試空答案（Phase -1）；座標都失敗再用棋盤格往外掃（Phase 3）
///
/// 每一段開始前都會再看一次輸入頁有沒有新座標（使用者可能在流程進行中才開頁面送出）。
///
/// # 返回
/// `RadarResult`，包含是否成功及成功的座標。
#[instrument(skip(api, config, manual_rx), fields(rollcall_id = rollcall_id))]
pub async fn attempt_radar_rollcall(
    api: Arc<ApiClient>,
    rollcall_id: u64,
    provider: &str,
    account_id: &str,
    config: &RadarConfig,
    manual_rx: Option<RadarPointReceiver>,
) -> RadarResult {
    let accuracy = config.accuracy;
    let altitude = config.altitude;

    // RADAR_SCHOOL_BLOCK_V1（fix77）：學校端正在擋我們（429／403）→ 停手，這一輪完全不碰學校。
    // 停手時間到會自動失效（見 [`school_block`]），不需要人工恢復。
    if let Some(block) = school_block(provider, rollcall_id) {
        warn!(
            status = block.status,
            stop_secs = block.stop_secs,
            "學校端擋下中，這一輪停手（不送出、不掃棋盤格）"
        );
        return RadarResult::Error(block.message());
    }

    let mut tried_coords: Vec<Coordinate> = Vec::new();
    // 量測資料：(座標, 距離) 用於後續圓交叉點計算
    let mut measurements: Vec<(Coordinate, f64)> = Vec::new();
    // 輸入頁送來、已經試過的座標（同一組不重複試，避免使用者連按時多打學校）。
    let mut manual_tried: Vec<Coordinate> = Vec::new();
    // RADAR_DEDUCE_TRACE_V1：這一輪的推估過程（頁面要畫，所以逐階段留下來）。
    let mut candidates_acc: Vec<Coordinate> = Vec::new();
    let mut estimate_acc: Option<Coordinate> = None;
    let mut grid_cells: Vec<Coordinate> = Vec::new();
    let mut grid_scanned = 0usize;

    // ── Phase -1: 空答案（RADAR_EMPTY_ANSWER_V1，抄 auto-rollcall-thu-tronclass）──
    // 伺服器若沒驗證座標，`{}` 就會直接被判到場；沒被接受就照下面的座標流程走。
    // 每個帳號、每場點名只送一次，避免每一輪重複花請求。
    if RADAR_EMPTY_ANSWER_ENABLED && empty_answer_allowed(provider, rollcall_id, account_id) {
        mark_empty_answer_tried(provider, rollcall_id, account_id);
        // RADAR_RELAY_V1：空答案也是一次真的送出，一樣跟著「每位間隔」排隊。
        await_radar_submit_turn(provider, rollcall_id, account_id).await;
        match api.answer_radar_rollcall_empty(rollcall_id).await {
            Ok(AttendanceResult::Success) => {
                info!(
                    rollcall_id = rollcall_id,
                    account = %account_id,
                    "✅ 雷達點名空答案直接被接受（未帶座標）"
                );
                return RadarResult::SuccessEmpty;
            }
            Ok(other) => {
                // RADAR_SCHOOL_BLOCK_V1（fix77）：空答案若被學校端擋下（429／403），一樣停手。
                if let Some((status, detail)) = other.school_block() {
                    warn!(status = status, "空答案被學校端擋下 → 這一場停手");
                    return RadarResult::Error(note_school_block(
                        provider,
                        rollcall_id,
                        account_id,
                        status,
                        &detail,
                    ));
                }
                debug!(
                    rollcall_id = rollcall_id,
                    result = ?other,
                    "空答案沒有被接受，改走原本的座標流程"
                );
            }
            Err(e) => {
                let err_str = e.to_string();
                if is_auth_error(&err_str) {
                    warn!(error = %e, "空答案遇到認證錯誤");
                    return RadarResult::Error(err_str);
                }
                debug!(error = %e, "空答案請求失敗，改走原本的座標流程");
            }
        }
    }

    // ── Phase 0: 輸入頁送來的座標（優先）──────────────────────────────────────
    if let Some(outcome) = drain_and_try_manual_point(
        &api,
        provider,
        rollcall_id,
        account_id,
        accuracy,
        altitude,
        manual_rx.as_ref(),
        &mut manual_tried,
        &mut tried_coords,
        &mut measurements,
    )
    .await
    {
        return outcome;
    }

    // ── Phase 1: 嘗試預設座標 ─────────────────────────────────────────────────
    info!(
        rollcall_id = rollcall_id,
        count = config.default_coords.len(),
        "Phase 1：嘗試 {} 個預設座標",
        config.default_coords.len()
    );

    for (i, &[lat, lon]) in config.default_coords.iter().enumerate() {
        // 每一輪開始前再看一次輸入頁：使用者可能在這段時間才送出座標。
        if let Some(outcome) = drain_and_try_manual_point(
            &api,
            provider,
            rollcall_id,
            account_id,
            accuracy,
            altitude,
            manual_rx.as_ref(),
            &mut manual_tried,
            &mut tried_coords,
            &mut measurements,
        )
        .await
        {
            return outcome;
        }

        let coord = Coordinate::new(lat, lon);
        tried_coords.push(coord);

        debug!(
            idx = i + 1,
            lat = lat,
            lon = lon,
            "嘗試預設座標 {}/{}",
            i + 1,
            config.default_coords.len()
        );

        match try_single_radar_coord(&api, provider, rollcall_id, account_id, coord, accuracy, altitude).await {
            Ok(SingleRadarResult::Success) => {
                info!(
                    lat = lat,
                    lon = lon,
                    "✅ 雷達簽到成功（預設座標 {}）",
                    i + 1
                );
                return RadarResult::Success { coord };
            }
            Ok(SingleRadarResult::TooFar { distance }) => {
                info!(
                    lat = lat,
                    lon = lon,
                    distance = distance,
                    "❌ 預設座標 {} 失敗，距離 {distance:.2}m",
                    i + 1
                );
                measurements.push((coord, distance));
            }
            Ok(SingleRadarResult::OtherFailure { reason }) => {
                warn!(
                    lat = lat,
                    lon = lon,
                    reason = %reason,
                    "⚠️ 預設座標 {} 失敗（非距離錯誤）",
                    i + 1
                );
                // 非距離錯誤不加入量測資料（距離未知）
            }
            // RADAR_SCHOOL_BLOCK_V1（fix77）：學校端擋下 → 這一場停手，不再打學校。
            Ok(SingleRadarResult::Blocked { message, .. }) => {
                warn!(reason = %message, "預設座標 {} 被學校端擋下，這一場停手", i + 1);
                return RadarResult::Error(message);
            }
            Err(e) => {
                let err_str = e.to_string();
                if is_auth_error(&err_str) {
                    warn!(error = %e, "雷達簽到遇到認證錯誤");
                    return RadarResult::Error(err_str);
                }
                warn!(error = %e, "預設座標 {} 請求錯誤，跳過", i + 1);
            }
        }
    }

    // RADAR_DEDUCE_TRACE_V1：Phase 1 結束——先把「試過哪些座標、學校各回報多遠」記下來。
    if !measurements.is_empty() {
        store_probe(
            provider,
            rollcall_id,
            build_probe(account_id, &measurements, &candidates_acc, estimate_acc, None, None),
        );
    }

    // ── Phase 2: 使用圓交叉點算法計算候選座標 ─────────────────────────────────
    if let Some(outcome) = drain_and_try_manual_point(
        &api,
        provider,
        rollcall_id,
        account_id,
        accuracy,
        altitude,
        manual_rx.as_ref(),
        &mut manual_tried,
        &mut tried_coords,
        &mut measurements,
    )
    .await
    {
        return outcome;
    }

    // ── Phase 2.5: 距離探測（RADAR_DISTANCE_PROBE_V1，fix77／S9）────────────────
    // 只有 1 筆量測時圓交會解不出來（兩圓才有交點），棋盤格又得掃上千格。
    // 這裡以那一筆量測為圓心、往三個方位各送一個探測點：每個探測點回報一個新距離，
    // 手上就有 4 筆量測 → 下面的 Phase 2 用圓交會／最小平方直接解出教室。
    // 限制：整場點名每個帳號只做一輪、探測點之間隔 1.2 秒、已試過的座標不重複送。
    if measurements.len() == 1
        && school_block(provider, rollcall_id).is_none()
        && distance_probe_allowed(provider, rollcall_id, account_id)
    {
        mark_distance_probed(provider, rollcall_id, account_id);
        let anchor = measurements[0].0;
        let anchor_distance = measurements[0].1;
        info!(
            rollcall_id = rollcall_id,
            anchor_distance = anchor_distance,
            points = RADAR_PROBE_BEARINGS.len(),
            distance_m = RADAR_PROBE_METERS,
            "Phase 2.5：只有 1 筆量測，先做 {} 個距離探測點（每個 {} 公尺）",
            RADAR_PROBE_BEARINGS.len(),
            RADAR_PROBE_METERS
        );
        for (i, bearing) in RADAR_PROBE_BEARINGS.iter().enumerate() {
            // 每個探測點前都再看一次停手狀態：第一個探測點就可能撞到 429。
            if school_block(provider, rollcall_id).is_some() {
                warn!("距離探測中途偵測到學校端擋下，停止探測");
                break;
            }
            let point = bearing_point(&anchor, RADAR_PROBE_METERS, *bearing);
            if tried_coords
                .iter()
                .any(|c| c.distance_meters(&point) < RADAR_PROBE_DEDUP_METERS)
            {
                debug!(bearing = bearing, "探測點與試過的座標重複，跳過");
                continue;
            }
            // 同一帳號連續探測要留間隔（第一位已經過 relay 排隊，這裡是後面幾個）。
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(RADAR_PROBE_GAP_MS)).await;
            }
            tried_coords.push(point);
            debug!(
                bearing = bearing,
                lat = point.latitude,
                lon = point.longitude,
                "距離探測點 {}",
                i + 1
            );
            match try_single_radar_coord(
                &api, provider, rollcall_id, account_id, point, accuracy, altitude,
            )
            .await
            {
                Ok(SingleRadarResult::Success) => {
                    info!(
                        lat = point.latitude,
                        lon = point.longitude,
                        "✅ 雷達簽到成功（距離探測點 {}）",
                        i + 1
                    );
                    store_probe(
                        provider,
                        rollcall_id,
                        build_probe(
                            account_id,
                            &measurements,
                            &candidates_acc,
                            estimate_acc,
                            None,
                            Some(point),
                        ),
                    );
                    return RadarResult::Success { coord: point };
                }
                Ok(SingleRadarResult::TooFar { distance }) => {
                    info!(
                        bearing = bearing,
                        distance = distance,
                        "距離探測點 {} 回報距離 {distance:.2} 公尺",
                        i + 1
                    );
                    measurements.push((point, distance));
                }
                Ok(SingleRadarResult::OtherFailure { reason }) => {
                    warn!(reason = %reason, "距離探測點 {} 失敗（非距離錯誤）", i + 1);
                }
                Ok(SingleRadarResult::Blocked { message, .. }) => {
                    warn!(reason = %message, "距離探測點 {} 被學校端擋下，這一場停手", i + 1);
                    return RadarResult::Error(message);
                }
                Err(e) => {
                    let err_str = e.to_string();
                    if is_auth_error(&err_str) {
                        warn!(error = %e, "距離探測遇到認證錯誤");
                        return RadarResult::Error(err_str);
                    }
                    warn!(error = %e, "距離探測點 {} 請求錯誤，跳過", i + 1);
                }
            }
        }
        store_probe(
            provider,
            rollcall_id,
            build_probe(
                account_id,
                &measurements,
                &candidates_acc,
                estimate_acc,
                None,
                None,
            ),
        );
        info!(
            measurements = measurements.len(),
            "距離探測結束：目前有 {} 筆量測",
            measurements.len()
        );
    }

    if measurements.len() >= 2 {
        info!(
            measurements = measurements.len(),
            "Phase 2：使用 {} 個距離量測點計算候選座標",
            measurements.len()
        );

        let candidates = compute_radar_candidates(&measurements);
        // RADAR_DEDUCE_TRACE_V1：候選座標＝兩圓交會；推估點＝多個距離的最小平方。
        candidates_acc = candidates.clone();
        estimate_acc = estimate_location_from_distances(&measurements);
        store_probe(
            provider,
            rollcall_id,
            build_probe(
                account_id,
                &measurements,
                &candidates_acc,
                estimate_acc,
                None,
                None,
            ),
        );

        if candidates.is_empty() {
            warn!("圓交叉點算法未產生任何候選座標");
        } else {
            info!(
                count = candidates.len(),
                "計算出 {} 個候選座標",
                candidates.len()
            );

            for (i, &candidate) in candidates.iter().enumerate() {
                // 跳過已嘗試過的座標（容差 10 公尺）
                if tried_coords
                    .iter()
                    .any(|c| c.distance_meters(&candidate) < 10.0)
                {
                    debug!("候選座標 {} 與已嘗試座標重複，跳過", i + 1);
                    continue;
                }

                tried_coords.push(candidate);

                debug!(
                    idx = i + 1,
                    lat = candidate.latitude,
                    lon = candidate.longitude,
                    "嘗試計算出的候選座標 {}/{}",
                    i + 1,
                    candidates.len()
                );

                match try_single_radar_coord(&api, provider, rollcall_id, account_id, candidate, accuracy, altitude).await
                {
                    Ok(SingleRadarResult::Success) => {
                        info!(
                            lat = candidate.latitude,
                            lon = candidate.longitude,
                            "✅ 雷達簽到成功（計算座標 {}）",
                            i + 1
                        );
                        store_probe(
                            provider,
                            rollcall_id,
                            build_probe(
                                account_id,
                                &measurements,
                                &candidates_acc,
                                estimate_acc,
                                None,
                                Some(candidate),
                            ),
                        );
                        return RadarResult::Success { coord: candidate };
                    }
                    Ok(SingleRadarResult::TooFar { distance }) => {
                        info!(
                            lat = candidate.latitude,
                            lon = candidate.longitude,
                            distance = distance,
                            "❌ 計算座標 {} 失敗，距離 {distance:.2}m",
                            i + 1
                        );
                        measurements.push((candidate, distance));
                    }
                    Ok(SingleRadarResult::OtherFailure { reason }) => {
                        warn!(reason = %reason, "計算座標 {} 失敗（非距離錯誤）", i + 1);
                    }
                    Ok(SingleRadarResult::Blocked { message, .. }) => {
                        warn!(reason = %message, "計算座標 {} 被學校端擋下，這一場停手", i + 1);
                        return RadarResult::Error(message);
                    }
                    Err(e) => {
                        let err_str = e.to_string();
                        if is_auth_error(&err_str) {
                            return RadarResult::Error(err_str);
                        }
                        warn!(error = %e, "計算座標 {} 請求錯誤，跳過", i + 1);
                    }
                }
            }
        }
        store_probe(
            provider,
            rollcall_id,
            build_probe(
                account_id,
                &measurements,
                &candidates_acc,
                estimate_acc,
                None,
                None,
            ),
        );
    } else if measurements.len() == 1 {
        warn!("只有一個距離量測點，無法使用圓交叉點算法（至少需要 2 個）");
    } else {
        warn!("沒有任何距離量測資料，跳過圓交叉點計算");
    }

    // ── Phase 3: 最後棋盤格（RADAR_GRID_FALLBACK_V1，抄 auto-rollcall-thu-tronclass）──
    // 只有「手上有距離量測」時才有可信的中心點（推估點）；沒量測就別亂掃學校的伺服器。
    // 同一場點名每 RADAR_GRID_MIN_INTERVAL_SECS 秒只放行一個帳號掃，跨回合續掃。
    // 中心點：優先用距離量測推估出來的位置，推不出來才用最後一個量測座標。
    // （先算好再進 if let：借用的生命週期不會跨進迴圈裡，後面才能 push 新的量測值。）
    let grid_center = match estimate_location_from_distances(&measurements) {
        Some(estimate) => Some(estimate),
        None => measurements.last().map(|(coord, _)| *coord),
    };
    if let Some(center) = grid_center {
        if grid_scan_allowed(provider, rollcall_id) && rollcall_still_open(&api, rollcall_id).await {
            let mut next = grid_cursor_next(provider, rollcall_id, account_id, &center);
            let mut scanned = 0usize;
            // RADAR_DEDUCE_TRACE_V1：先記下棋盤格中心（＝推估點），頁面才知道掃描是從哪裡開始。
            let grid_now = grid_probe(center, &grid_cells, grid_scanned);
            store_probe(
                provider,
                rollcall_id,
                build_probe(
                    account_id,
                    &measurements,
                    &candidates_acc,
                    estimate_acc,
                    Some(&grid_now),
                    None,
                ),
            );
            while scanned < RADAR_GRID_ATTEMPTS_PER_ROUND {
                let Some(candidate) = grid_candidate(next, &center, RADAR_GRID_STEP_METERS) else {
                    info!(
                        rollcall_id = rollcall_id,
                        "棋盤格已掃到上限半徑，這一場不再往外掃"
                    );
                    break;
                };
                next += 1;
                scanned += 1;
                if tried_coords
                    .iter()
                    .any(|c| c.distance_meters(&candidate) < RADAR_GRID_DEDUP_METERS)
                {
                    continue;
                }
                tried_coords.push(candidate);
                grid_cells.push(candidate);
                grid_scanned += 1;
                debug!(
                    rollcall_id = rollcall_id,
                    index = next - 1,
                    lat = candidate.latitude,
                    lon = candidate.longitude,
                    "棋盤格第 {} 格",
                    next - 1
                );
                match try_single_radar_coord(&api, provider, rollcall_id, account_id, candidate, accuracy, altitude).await
                {
                    Ok(SingleRadarResult::Success) => {
                        info!(
                            lat = candidate.latitude,
                            lon = candidate.longitude,
                            index = next - 1,
                            "✅ 雷達簽到成功（棋盤格第 {} 格）",
                            next - 1
                        );
                        let grid_now = grid_probe(center, &grid_cells, grid_scanned);
                        store_probe(
                            provider,
                            rollcall_id,
                            build_probe(
                                account_id,
                                &measurements,
                                &candidates_acc,
                                estimate_acc,
                                Some(&grid_now),
                                Some(candidate),
                            ),
                        );
                        grid_cursor_store(provider, rollcall_id, account_id, &center, next);
                        return RadarResult::Success { coord: candidate };
                    }
                    Ok(SingleRadarResult::TooFar { distance }) => {
                        measurements.push((candidate, distance));
                    }
                    Ok(SingleRadarResult::OtherFailure { reason }) => {
                        warn!(reason = %reason, "棋盤格第 {} 格失敗（非距離錯誤）", next - 1);
                    }
                    Ok(SingleRadarResult::Blocked { message, .. }) => {
                        warn!(reason = %message, "棋盤格第 {} 格被學校端擋下，這一場停手", next - 1);
                        return RadarResult::Error(message);
                    }
                    Err(e) => {
                        let err_str = e.to_string();
                        if is_auth_error(&err_str) {
                            return RadarResult::Error(err_str);
                        }
                        warn!(error = %e, "棋盤格第 {} 格請求錯誤，跳過", next - 1);
                    }
                }
            }
            grid_cursor_store(provider, rollcall_id, account_id, &center, next);
            let grid_now = grid_probe(center, &grid_cells, grid_scanned);
            store_probe(
                provider,
                rollcall_id,
                build_probe(
                    account_id,
                    &measurements,
                    &candidates_acc,
                    estimate_acc,
                    Some(&grid_now),
                    None,
                ),
            );
            if scanned > 0 {
                info!(
                    rollcall_id = rollcall_id,
                    scanned = scanned,
                    next = next,
                    "棋盤格這一輪掃完，下一輪從第 {} 格繼續",
                    next
                );
            }
        }
    }

    // 最後再看一次輸入頁：這一輪才送出的座標不要白費（下一輪監控也會再取一次）。
    if let Some(outcome) = drain_and_try_manual_point(
        &api,
        provider,
        rollcall_id,
        account_id,
        accuracy,
        altitude,
        manual_rx.as_ref(),
        &mut manual_tried,
        &mut tried_coords,
        &mut measurements,
    )
    .await
    {
        return outcome;
    }

    // ── 全部嘗試失敗 ──────────────────────────────────────────────────────────
    let last_distance = measurements.last().map(|(_, d)| *d);
    warn!(
        tried = tried_coords.len(),
        last_distance = ?last_distance,
        "⛔ 雷達簽到全部嘗試失敗，共嘗試 {} 個座標",
        tried_coords.len()
    );

    // 棋盤格只有在「真的掃過」時才留紀錄（沒掃過就不要讓頁面畫一個空棋盤）。
    let grid_final = grid_center
        .filter(|_| grid_scanned > 0)
        .map(|center| grid_probe(center, &grid_cells, grid_scanned));
    store_probe(
        provider,
        rollcall_id,
        build_probe(
            account_id,
            &measurements,
            &candidates_acc,
            estimate_acc,
            grid_final.as_ref(),
            None,
        ),
    );

    RadarResult::Failed {
        last_distance,
        tried_coords,
    }
}

// ─── 單次座標嘗試 ─────────────────────────────────────────────────────────────

/// 單次雷達座標嘗試的內部結果
#[derive(Debug)]
enum SingleRadarResult {
    Success,
    TooFar { distance: f64 },
    OtherFailure { reason: String },
    /// RADAR_SCHOOL_BLOCK_V1（fix77）：學校端擋下（429／403）——這一場停手。
    /// `message` 是已經記好停手狀態、也推播過管理員之後給日誌用的一句話。
    Blocked { status: u16, message: String },
}

/// RADAR_INPUT_PAGE_V1（fix71）：同一組「輸入頁座標」的重複判定距離（公尺）。
/// 使用者連按送出時，10 公尺內視為同一組，不重複打學校。
const MANUAL_POINT_DEDUP_METERS: f64 = 10.0;

/// 輸入頁是否已經按下「系統演算」（通道裡有座標還沒被取走）。
/// 自動送出要等班上超過 15%；使用者手動按按鈕則立刻送，不消耗這筆座標。
pub async fn radar_channel_has_point(rx: Option<&RadarPointReceiver>) -> bool {
    let Some(rx) = rx else {
        return false;
    };
    let guard = rx.lock().await;
    !guard.is_empty()
}

/// 從輸入頁的通道取出「最新一組」座標（非阻塞；一次清空佇列，只留最後一筆）。
///
/// 為什麼只留最後一筆：使用者可能連按兩次（第一組打錯、第二組修正），
/// 舊的那組已經沒有意義，只會多打一次學校。
async fn take_manual_point(rx: Option<&RadarPointReceiver>) -> Option<RadarPointInput> {
    let Some(rx) = rx else {
        return None;
    };
    let mut guard = rx.lock().await;
    let mut latest: Option<RadarPointInput> = None;
    while let Ok(point) = guard.try_recv() {
        latest = Some(point);
    }
    latest
}

/// 取出輸入頁座標並嘗試；回 `Some`＝流程該結束（成功或致命錯誤）。
#[allow(clippy::too_many_arguments)]
async fn drain_and_try_manual_point(
    api: &ApiClient,
    provider: &str,
    rollcall_id: u64,
    account_id: &str,
    accuracy: u32,
    altitude: i32,
    manual_rx: Option<&RadarPointReceiver>,
    manual_tried: &mut Vec<Coordinate>,
    tried_coords: &mut Vec<Coordinate>,
    measurements: &mut Vec<(Coordinate, f64)>,
) -> Option<RadarResult> {
    let point = take_manual_point(manual_rx).await?;
    if !point.is_valid() {
        warn!(
            rollcall_id = rollcall_id,
            lat = point.latitude,
            lon = point.longitude,
            "輸入頁送來的座標不合法，忽略"
        );
        return None;
    }
    let coord = Coordinate::new(point.latitude, point.longitude);
    if manual_tried
        .iter()
        .any(|c| c.distance_meters(&coord) < MANUAL_POINT_DEDUP_METERS)
    {
        debug!(rollcall_id = rollcall_id, "輸入頁座標與剛試過的重複，跳過");
        return None;
    }
    manual_tried.push(coord);
    info!(
        rollcall_id = rollcall_id,
        lat = point.latitude,
        lon = point.longitude,
        source = point.source_label(),
        "使用雷達點名頁送來的座標（{}）",
        point.source_label()
    );
    try_manual_coord(
        api,
        provider,
        rollcall_id,
        account_id,
        coord,
        accuracy,
        altitude,
        tried_coords,
        measurements,
    )
    .await
}

/// 嘗試輸入頁送來的座標：成功回 `Some(Success)`；距離不足回 `None`（並記下量測值）。
#[allow(clippy::too_many_arguments)]
async fn try_manual_coord(
    api: &ApiClient,
    provider: &str,
    rollcall_id: u64,
    account_id: &str,
    coord: Coordinate,
    accuracy: u32,
    altitude: i32,
    tried_coords: &mut Vec<Coordinate>,
    measurements: &mut Vec<(Coordinate, f64)>,
) -> Option<RadarResult> {
    tried_coords.push(coord);
    match try_single_radar_coord(api, provider, rollcall_id, account_id, coord, accuracy, altitude).await {
        Ok(SingleRadarResult::Success) => {
            info!(
                lat = coord.latitude,
                lon = coord.longitude,
                "✅ 雷達簽到成功（輸入頁座標）"
            );
            Some(RadarResult::Success { coord })
        }
        Ok(SingleRadarResult::TooFar { distance }) => {
            info!(
                lat = coord.latitude,
                lon = coord.longitude,
                distance = distance,
                "❌ 輸入頁座標失敗，距離 {distance:.2}m"
            );
            measurements.push((coord, distance));
            None
        }
        Ok(SingleRadarResult::OtherFailure { reason }) => {
            warn!(
                lat = coord.latitude,
                lon = coord.longitude,
                reason = %reason,
                "⚠️ 輸入頁座標失敗（非距離錯誤）"
            );
            None
        }
        Ok(SingleRadarResult::Blocked { message, .. }) => {
            warn!(
                lat = coord.latitude,
                lon = coord.longitude,
                reason = %message,
                "⚠️ 輸入頁座標被學校端擋下，這一場停手"
            );
            Some(RadarResult::Error(message))
        }
        Err(e) => {
            let err_str = e.to_string();
            if is_auth_error(&err_str) {
                warn!(error = %e, "雷達簽到遇到認證錯誤（輸入頁座標）");
                return Some(RadarResult::Error(err_str));
            }
            warn!(error = %e, "輸入頁座標請求錯誤，跳過");
            None
        }
    }
}

/// 嘗試用單一座標進行雷達簽到
///
/// RADAR_RELAY_V1：所有座標送出都經過這裡，所以在這裡排隊——雷達就跟數字／QR 一樣，
/// 「同一場點名第一位立即送出、之後每位間隔 base + U(0, jitter) 毫秒」。
async fn try_single_radar_coord(
    api: &ApiClient,
    provider: &str,
    rollcall_id: u64,
    account_id: &str,
    coord: Coordinate,
    accuracy: u32,
    altitude: i32,
) -> Result<SingleRadarResult> {
    await_radar_submit_turn(provider, rollcall_id, account_id).await;
    let result = api
        .answer_radar_rollcall(
            rollcall_id,
            coord.latitude,
            coord.longitude,
            accuracy,
            altitude,
        )
        .await?;

    match result {
        AttendanceResult::Success => Ok(SingleRadarResult::Success),
        AttendanceResult::RadarTooFar { distance } => Ok(SingleRadarResult::TooFar { distance }),
        // RADAR_SCHOOL_BLOCK_V1（fix77）：學校端擋下（429／403）→ 記下來（停手＋推播）。
        AttendanceResult::TransientFailure { reason } => Ok(classify_single_failure(
            provider,
            rollcall_id,
            account_id,
            reason,
        )),
        AttendanceResult::Failed { reason } => Ok(classify_single_failure(
            provider,
            rollcall_id,
            account_id,
            reason,
        )),
    }
}

// ─── 候選座標計算 ─────────────────────────────────────────────────────────────

/// 從距離量測列表計算可能的教室座標候選
///
/// 策略：
/// - 使用前兩個量測點做圓交叉點計算
/// - 若有更多量測點，用三邊測量法估算
/// - 返回去重後的候選列表（按可信度排序）
pub fn compute_radar_candidates(measurements: &[(Coordinate, f64)]) -> Vec<Coordinate> {
    if measurements.len() < 2 {
        return Vec::new();
    }

    let mut candidates: Vec<Coordinate> = Vec::new();

    // 嘗試所有成對組合的圓交叉點
    for i in 0..measurements.len() {
        for j in (i + 1)..measurements.len() {
            let (c1, r1) = &measurements[i];
            let (c2, r2) = &measurements[j];

            match circle_intersection(c1, *r1, c2, *r2) {
                CircleIntersection::Two(p1, p2) => {
                    candidates.push(p1);
                    candidates.push(p2);
                }
                CircleIntersection::One(p) => {
                    candidates.push(p);
                }
                CircleIntersection::None => {
                    // 兩圓不相交，嘗試中點作為折衷
                    let mid = weighted_midpoint(c1, *r1, c2, *r2);
                    candidates.push(mid);
                }
                CircleIntersection::Infinite => {}
            }
        }
    }

    // 若有三個以上量測點，也加入三邊測量法結果
    if measurements.len() >= 3 {
        if let Some(est) = trilateration_least_squares(measurements) {
            candidates.insert(0, est); // 優先嘗試最小二乘法結果
        }
    }

    // 去重（容差 50 公尺視為同一點）
    dedup_coordinates(candidates, 50.0)
}

/// 對座標列表去重（距離小於 threshold_meters 的視為同一點）
fn dedup_coordinates(coords: Vec<Coordinate>, threshold_meters: f64) -> Vec<Coordinate> {
    let mut result: Vec<Coordinate> = Vec::new();

    for coord in coords {
        let is_dup = result
            .iter()
            .any(|existing| existing.distance_meters(&coord) < threshold_meters);

        if !is_dup {
            result.push(coord);
        }
    }

    result
}

// ─── RADAR_REFERENCE_PARITY_V1（抄 auto-rollcall-thu-tronclass）───────────────
//
// ① 空答案（RADAR_EMPTY_ANSWER_V1）：先送一個不帶座標的 `{}`。伺服器沒驗證座標就
//    直接判到場；失敗（含被補洞後回 400／422）就照原本的座標流程走。
//    每個帳號、每場點名只送一次，記在 `RADAR_EMPTY_ANSWER_TRIED`。
// ② 最後棋盤格（RADAR_GRID_FALLBACK_V1）：座標全失敗、而且手上有距離量測時，
//    以推估點為中心、每格 100 公尺一圈圈往外掃；每輪只掃幾個點、跨回合續掃，
//    直到命中、超出半徑或點名關閉——與參考專案的「無界棋盤格」同一個精神，
//    差別只在我們設了半徑上限，並讓同一場點名每幾秒只有一個帳號掃（不讓 13 個帳號一起打）。

/// RADAR_EMPTY_ANSWER_V1 的開關：設 `false` 就完全不試空答案，一律用座標送
/// （例如學校補上定位驗證、或臨時不想用這條路時）。
const RADAR_EMPTY_ANSWER_ENABLED: bool = true;
/// 這些狀態最多留這麼久（點名早就結束了，留著只是長記憶體）。
const RADAR_STATE_TTL: Duration = Duration::from_secs(6 * 60 * 60);
/// RADAR_DISTANCE_PROBE_V1（fix77，S9）：距離探測。
///
/// 只有一筆量測時圓交會解不出來（兩圓才有交點），棋盤格又得掃上千格
/// （模擬：目標 1.8 公里外要 1186 個請求／39 分鐘）。這裡改成以那一筆量測為圓心、
/// 往三個方位各送一個「探測點」：每個探測點回報一個新距離，手上就有 4 筆量測，
/// 圓交會／最小平方可以直接解出教室（模擬：8 個請求、約 12 秒）。
const RADAR_PROBE_METERS: f64 = 150.0;
/// 探測方位（度；0＝正北、順時針 90＝正東）：三個方向均勻分開，量測三角形最漂亮。
const RADAR_PROBE_BEARINGS: [f64; 3] = [0.0, 120.0, 240.0];
/// 探測點之間的間隔（毫秒）：同一帳號不要連發，免得被學校當成攻擊（也讓 429 更難發生）。
const RADAR_PROBE_GAP_MS: u64 = 1200;
/// 與已試座標視為同一點的距離（沿用主流程的 10 公尺）。
const RADAR_PROBE_DEDUP_METERS: f64 = 10.0;
/// 棋盤格每格幾公尺（參考專案 `final_grid_step_meters` 預設也是 100）。
const RADAR_GRID_STEP_METERS: f64 = 100.0;
/// 每一輪最多掃幾個點（跨回合續掃，不是一次掃完）。
const RADAR_GRID_ATTEMPTS_PER_ROUND: usize = 4;
/// 最遠掃到離中心幾公尺（參考專案是無界；我們設上限，避免無限打學校的伺服器）。
const RADAR_GRID_MAX_RADIUS_METERS: f64 = 3000.0;
/// 同一場點名兩次掃描至少間隔幾秒（多帳號共用，避免變成 13 倍請求）。
const RADAR_GRID_MIN_INTERVAL_SECS: u64 = 10;
/// 中心點移動超過這個距離就從中心重新掃起。
const RADAR_GRID_CENTER_RESET_METERS: f64 = 50.0;
/// 與已試座標視為同一格的重複距離（沿用主流程的 10 公尺）。
const RADAR_GRID_DEDUP_METERS: f64 = 10.0;

/// (provider, rollcall_id, account_id)：狀態以「哪個帳號在哪一場點名」為單位。
type RadarStateKey = (String, u64, String);

/// 棋盤格游標（中心點 + 下一格編號 + 更新時間）。
#[derive(Clone, Copy)]
struct GridCursorState {
    lat: f64,
    lon: f64,
    next: usize,
    at: Instant,
}

// ─── RADAR_DEDUCE_TRACE_V1：把「教室位置是怎麼推出來的」記下來 ────────────────
//
// 點名頁要畫給使用者看：**系統不是隨便猜一個座標**，而是
//   ① 先送出手上的座標（輸入頁送的／設定的教室座標）→ 學校回報「還差幾公尺」
//   ② 兩個以上的「還差幾公尺」＝教室一定同時在這兩個圓上 → 兩圓交會＝候選座標
//   ③ 多個距離做最小平方（三邊測量）→ 推估點
//   ④ 以推估點為中心、每 100 公尺往外掃（棋盤格），找到就送出
// 這裡只記錄「真的發生過」的事；沒有量測就沒有紀錄（頁面不會編故事）。
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct RadarProbeMeasurement {
    pub latitude: f64,
    pub longitude: f64,
    /// 學校回報的距離（公尺）：這個點離教室還差多遠。
    pub distance_m: f64,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct RadarProbeGrid {
    /// 棋盤格中心（＝距離推估點；推不出來時是最後一個量測點）。
    pub center: [f64; 2],
    pub step_m: f64,
    pub max_radius_m: f64,
    /// 目前為止已經掃過幾格。
    pub scanned: usize,
    /// 這一場掃過的格子座標（頁面畫成小灰點）。
    pub cells: Vec<[f64; 2]>,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct RadarProbeTrace {
    /// 這份紀錄是哪個帳號跑出來的（多帳號時取最近一次）。
    pub account_id: String,
    /// 最後成功的座標（成功才有）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<[f64; 2]>,
    pub measurements: Vec<RadarProbeMeasurement>,
    /// 兩圓交會算出來的候選座標。
    pub candidates: Vec<[f64; 2]>,
    /// 最小平方（三邊測量）推估出來的教室位置。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimate: Option<[f64; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grid: Option<RadarProbeGrid>,
}

/// 推估紀錄的保存時間與筆數上限（避免長時間跑下來無限成長）。
const RADAR_PROBE_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const RADAR_PROBE_MAX_ENTRIES: usize = 64;

static RADAR_PROBE_TRACES: OnceLock<Mutex<HashMap<(String, u64), (Instant, RadarProbeTrace)>>> =
    OnceLock::new();

fn probe_key(provider: &str, rollcall_id: u64) -> (String, u64) {
    (provider.to_string(), rollcall_id)
}

/// 記下一次推估紀錄（同一場點名保留最近一次；太舊或太多筆就清掉）。
fn store_probe(provider: &str, rollcall_id: u64, trace: RadarProbeTrace) {
    let lock = RADAR_PROBE_TRACES.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut map) = lock.lock() else {
        return; // 取不到鎖就不要寫（KB §4：不 panic）
    };
    let now = Instant::now();
    map.retain(|_, (at, _)| now.saturating_duration_since(*at) < RADAR_PROBE_TTL);
    let key = probe_key(provider, rollcall_id);
    if map.len() >= RADAR_PROBE_MAX_ENTRIES && !map.contains_key(&key) {
        if let Some(oldest) = map
            .iter()
            .min_by_key(|(_, (at, _))| *at)
            .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
    }
    map.insert(key, (now, trace));
}

/// 狀態端點用：讀出這一場點名的推估紀錄（沒有就 None）。
pub fn probe_trace(provider: &str, rollcall_id: u64) -> Option<RadarProbeTrace> {
    let lock = RADAR_PROBE_TRACES.get_or_init(|| Mutex::new(HashMap::new()));
    let map = lock.lock().ok()?;
    map.get(&probe_key(provider, rollcall_id))
        .map(|(_, t)| t.clone())
}

/// 棋盤格紀錄（給頁面畫掃過的格子）。
fn grid_probe(center: Coordinate, cells: &[Coordinate], scanned: usize) -> RadarProbeGrid {
    RadarProbeGrid {
        center: [center.latitude, center.longitude],
        step_m: RADAR_GRID_STEP_METERS,
        max_radius_m: RADAR_GRID_MAX_RADIUS_METERS,
        scanned,
        cells: cells
            .iter()
            .map(|c| [c.latitude, c.longitude])
            .collect(),
    }
}

/// 把目前的量測／候選／推估／棋盤格整理成一份紀錄。
fn build_probe(
    account_id: &str,
    measurements: &[(Coordinate, f64)],
    candidates: &[Coordinate],
    estimate: Option<Coordinate>,
    grid: Option<&RadarProbeGrid>,
    success: Option<Coordinate>,
) -> RadarProbeTrace {
    RadarProbeTrace {
        account_id: account_id.to_string(),
        success: success.map(|c| [c.latitude, c.longitude]),
        measurements: measurements
            .iter()
            .map(|(c, d)| RadarProbeMeasurement {
                latitude: c.latitude,
                longitude: c.longitude,
                distance_m: *d,
            })
            .collect(),
        candidates: candidates
            .iter()
            .map(|c| [c.latitude, c.longitude])
            .collect(),
        estimate: estimate.map(|c| [c.latitude, c.longitude]),
        grid: grid.cloned(),
    }
}

static RADAR_EMPTY_ANSWER_TRIED: OnceLock<Mutex<HashMap<RadarStateKey, Instant>>> =
    OnceLock::new();
static RADAR_GRID_CURSOR: OnceLock<Mutex<HashMap<RadarStateKey, GridCursorState>>> =
    OnceLock::new();
static RADAR_GRID_LAST_SCAN: OnceLock<Mutex<HashMap<(String, u64), Instant>>> = OnceLock::new();
/// RADAR_DISTANCE_PROBE_V1（fix77）：這個帳號在這場點名做過距離探測了嗎？
static RADAR_DISTANCE_PROBED: OnceLock<Mutex<HashMap<RadarStateKey, Instant>>> = OnceLock::new();

/// 這個帳號在這場點名還沒試過空答案嗎？（順手清掉過期項目）
fn empty_answer_allowed(provider: &str, rollcall_id: u64, account_id: &str) -> bool {
    let lock = RADAR_EMPTY_ANSWER_TRIED.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut map) = lock.lock() else {
        // 鎖被前一次 panic 弄壞時當成「沒試過」：寧可多送一次，也不要白白放棄。
        return true;
    };
    let now = Instant::now();
    map.retain(|_, at| now.saturating_duration_since(*at) < RADAR_STATE_TTL);
    !map.contains_key(&(provider.to_string(), rollcall_id, account_id.to_string()))
}

/// 記下「這個帳號在這場點名已經試過空答案」。
fn mark_empty_answer_tried(provider: &str, rollcall_id: u64, account_id: &str) {
    let lock = RADAR_EMPTY_ANSWER_TRIED.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut map) = lock.lock() else { return };
    let now = Instant::now();
    map.retain(|_, at| now.saturating_duration_since(*at) < RADAR_STATE_TTL);
    map.insert((provider.to_string(), rollcall_id, account_id.to_string()), now);
}

/// 這一輪可以掃棋盤格嗎？（同一場點名每 [`RADAR_GRID_MIN_INTERVAL_SECS`] 秒只放行一次）
fn grid_scan_allowed(provider: &str, rollcall_id: u64) -> bool {
    let lock = RADAR_GRID_LAST_SCAN.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut map) = lock.lock() else { return false };
    let now = Instant::now();
    map.retain(|_, at| now.saturating_duration_since(*at) < RADAR_STATE_TTL);
    let key = (provider.to_string(), rollcall_id);
    if let Some(at) = map.get(&key) {
        if now.saturating_duration_since(*at) < Duration::from_secs(RADAR_GRID_MIN_INTERVAL_SECS) {
            return false;
        }
    }
    map.insert(key, now);
    true
}

/// 這一輪要從第幾格開始掃（中心點換了超過 [`RADAR_GRID_CENTER_RESET_METERS`] 就從頭）。
fn grid_cursor_next(
    provider: &str,
    rollcall_id: u64,
    account_id: &str,
    center: &Coordinate,
) -> usize {
    let lock = RADAR_GRID_CURSOR.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut map) = lock.lock() else { return 0 };
    let now = Instant::now();
    map.retain(|_, state| now.saturating_duration_since(state.at) < RADAR_STATE_TTL);
    match map.get(&(provider.to_string(), rollcall_id, account_id.to_string())) {
        Some(state) => {
            let moved = Coordinate::new(state.lat, state.lon).distance_meters(center);
            if moved > RADAR_GRID_CENTER_RESET_METERS {
                0
            } else {
                state.next
            }
        }
        None => 0,
    }
}

/// 記下棋盤格掃到哪裡（下一輪接著掃）。
fn grid_cursor_store(
    provider: &str,
    rollcall_id: u64,
    account_id: &str,
    center: &Coordinate,
    next: usize,
) {
    let lock = RADAR_GRID_CURSOR.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut map) = lock.lock() else { return };
    let now = Instant::now();
    map.retain(|_, state| now.saturating_duration_since(state.at) < RADAR_STATE_TTL);
    map.insert(
        (provider.to_string(), rollcall_id, account_id.to_string()),
        GridCursorState {
            lat: center.latitude,
            lon: center.longitude,
            next,
            at: now,
        },
    );
}

/// 棋盤格第 `index` 格：0＝中心，之後一圈一圈往外（第 r 圈 8r 格）。
/// 超過 [`RADAR_GRID_MAX_RADIUS_METERS`] 就回 `None`（＝這一場掃完了）。
fn grid_candidate(index: usize, center: &Coordinate, step_meters: f64) -> Option<Coordinate> {
    if index == 0 {
        return Some(*center);
    }
    let target = index - 1;
    let mut ring: usize = 1;
    let mut passed: usize = 0;
    loop {
        let count = 8 * ring;
        if target < passed + count {
            let pos = target - passed;
            let side = 2 * ring;
            let offset = (pos % side) as i64;
            let ring_i = ring as i64;
            let (ix, iy) = match pos / side {
                0 => (-ring_i + offset, -ring_i),
                1 => (ring_i, -ring_i + offset),
                2 => (ring_i - offset, ring_i),
                _ => (-ring_i, ring_i - offset),
            };
            let candidate = Coordinate::from_cartesian_meters(
                ix as f64 * step_meters,
                iy as f64 * step_meters,
                center,
            );
            if center.distance_meters(&candidate) > RADAR_GRID_MAX_RADIUS_METERS {
                return None;
            }
            return Some(candidate);
        }
        passed += count;
        ring += 1;
        if (ring as f64) * step_meters > RADAR_GRID_MAX_RADIUS_METERS + step_meters {
            return None;
        }
    }
}

// ─── RADAR_SCHOOL_BLOCK_V1（fix77）：學校端擋下（429／403）就停手＋推播 ────────
//
// 使用者拍板：「429／403 停手＋推播」。
//   ① 偵測：API 層把 429（限流）／403（禁止）包成帶固定標記的失敗訊息，
//      雷達流程用 `AttendanceResult::school_block()` / `crate::api::school_block_of()` 認出來。
//   ② 停手：同一場點名**所有帳號**都不再打學校，預設 30 分鐘；學校有給 `Retry-After`
//      就用它（界在 60–3600 秒）。停手期間 [`attempt_radar_rollcall`] 直接回傳錯誤、不發請求。
//   ③ 推播：同一場點名、同一個狀態碼只推一次給所有管理員（`notify_admins_school_block`）。

/// 沒有 `Retry-After` 時，學校端擋下後要停手多久。
const RADAR_SCHOOL_BLOCK_STOP_SECS: u64 = 30 * 60;
/// 學校指定秒數時的上下限（太短沒意義、太長怕點名早就結束）。
const RADAR_SCHOOL_BLOCK_MIN_STOP_SECS: u64 = 60;
const RADAR_SCHOOL_BLOCK_MAX_STOP_SECS: u64 = 60 * 60;
/// 學校回覆留在紀錄裡的長度上限（頁面／推播要看得懂就好）。
const RADAR_SCHOOL_BLOCK_DETAIL_CHARS: usize = 300;

/// 學校端擋下的紀錄（給頁面畫橫幅、給日誌與推播說明）。一律走 JSON 回應，
/// 欄位名稱就是頁面看到的欄位。
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct RadarSchoolBlock {
    /// 學校回的 HTTP 狀態碼（429 或 403）。
    pub status: u16,
    /// 是哪個帳號撞到的（多帳號時看得出來）。
    pub account_id: String,
    /// 學校的回覆摘要（截短）。
    pub detail: String,
    /// 什麼時候看到的（Unix 秒）。
    pub at_unix_secs: u64,
    /// 停手到什麼時候（Unix 秒）。
    pub stop_until_unix_secs: u64,
    /// 這次停手幾秒。
    pub stop_secs: u64,
}

impl RadarSchoolBlock {
    /// 給日誌／`RadarResult::Error` 用的一句話（頁面另外用自己的文案）。
    pub fn message(&self) -> String {
        format!(
            "🚫 學校端擋下送出（HTTP {}）：系統已停手 {} 秒（到 Unix {}），最後回覆：{}",
            self.status, self.stop_secs, self.stop_until_unix_secs, self.detail
        )
    }
}

/// (provider, rollcall_id) → 停手紀錄（含開始時間，用來判斷停手結束了沒）。
static RADAR_SCHOOL_BLOCKS: OnceLock<Mutex<HashMap<(String, u64), (Instant, RadarSchoolBlock)>>> =
    OnceLock::new();
/// (provider, rollcall_id, 狀態碼) → 已經推播過的時間（同一場同一個狀態碼只推一次）。
static RADAR_SCHOOL_BLOCK_NOTIFIED: OnceLock<Mutex<HashMap<(String, u64, u16), Instant>>> =
    OnceLock::new();

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// 這一場點名現在是不是「被學校端擋下、停手中」？是的話回傳紀錄（頁面也讀這個）。
pub fn school_block(provider: &str, rollcall_id: u64) -> Option<RadarSchoolBlock> {
    let lock = RADAR_SCHOOL_BLOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let map = lock.lock().ok()?;
    let (at, block) = map.get(&(provider.to_string(), rollcall_id))?;
    // 停手時間過了就當作沒這回事（會自動再試一次；再被擋就再記一次）。
    if at.elapsed().as_secs() >= block.stop_secs {
        return None;
    }
    Some(block.clone())
}

/// 記下「學校端擋下」並（必要時）推播管理員；回傳給日誌用的一句話。
fn note_school_block(
    provider: &str,
    rollcall_id: u64,
    account_id: &str,
    status: u16,
    detail: &str,
) -> String {
    let stop_secs = crate::api::school_block_retry_after(detail)
        .map(|secs| secs.clamp(RADAR_SCHOOL_BLOCK_MIN_STOP_SECS, RADAR_SCHOOL_BLOCK_MAX_STOP_SECS))
        .unwrap_or(RADAR_SCHOOL_BLOCK_STOP_SECS);
    let now = Instant::now();
    let block = RadarSchoolBlock {
        status,
        account_id: account_id.to_string(),
        detail: detail.chars().take(RADAR_SCHOOL_BLOCK_DETAIL_CHARS).collect(),
        at_unix_secs: now_unix_secs(),
        stop_until_unix_secs: now_unix_secs().saturating_add(stop_secs),
        stop_secs,
    };
    let lock = RADAR_SCHOOL_BLOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut map) = lock.lock() {
        map.retain(|_, (at, _)| now.saturating_duration_since(*at) < RADAR_STATE_TTL);
        map.insert((provider.to_string(), rollcall_id), (now, block.clone()));
    }
    notify_school_block_once(provider, rollcall_id, &block);
    block.message()
}

/// 推播管理員——同一場點名、同一個狀態碼只推一次（避免每個帳號各推一則）。
fn notify_school_block_once(provider: &str, rollcall_id: u64, block: &RadarSchoolBlock) {
    let lock = RADAR_SCHOOL_BLOCK_NOTIFIED.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut map) = lock.lock() else {
        return; // 取不到鎖就不要推（KB §4：不 panic）
    };
    let now = Instant::now();
    map.retain(|_, at| now.saturating_duration_since(*at) < RADAR_STATE_TTL);
    let key = (provider.to_string(), rollcall_id, block.status);
    if map.contains_key(&key) {
        return;
    }
    map.insert(key, now);
    drop(map);

    let label = if block.status == 429 {
        "429 限流"
    } else {
        "403 禁止"
    };
    warn!(
        status = block.status,
        rollcall_id = rollcall_id,
        account = %block.account_id,
        stop_secs = block.stop_secs,
        "學校端擋下送出：已停手並推播管理員"
    );
    crate::web_push_notifications::notify_admins_school_block(
        format!("🚫 雷達點名被學校擋下（HTTP {label}）"),
        format!(
            "點名 ID {rollcall_id} 的送出被學校端擋下，系統已停手 {stop_secs} 秒、不會再打學校。帳號：{account}。學校回覆：{detail}。若是 403，請確認學校網站是否正常、或該帳號是否要重新登入。",
            stop_secs = block.stop_secs,
            account = block.account_id,
            detail = block.detail
        ),
        format!("tronclass-radar-block-{provider}-{rollcall_id}"),
    );
}

/// 把一次失敗分類：學校端擋下（429／403）→ 記下來並回 [`SingleRadarResult::Blocked`]；
/// 其餘照舊回「非距離錯誤」。
fn classify_single_failure(
    provider: &str,
    rollcall_id: u64,
    account_id: &str,
    reason: String,
) -> SingleRadarResult {
    if let Some((status, detail)) = crate::api::school_block_of(&reason) {
        let message = note_school_block(provider, rollcall_id, account_id, status, &detail);
        return SingleRadarResult::Blocked { status, message };
    }
    SingleRadarResult::OtherFailure { reason }
}

/// 從錨點往 `bearing_deg` 方位走 `distance_m` 公尺的座標（距離探測點的算法）。
fn bearing_point(anchor: &Coordinate, distance_m: f64, bearing_deg: f64) -> Coordinate {
    let rad = bearing_deg.to_radians();
    let north = distance_m * rad.cos();
    let east = distance_m * rad.sin();
    Coordinate::from_cartesian_meters(east, north, anchor)
}

/// 這個帳號在這場點名做過距離探測了嗎？（整場只做一輪，避免每輪都多打三次學校）
fn distance_probe_allowed(provider: &str, rollcall_id: u64, account_id: &str) -> bool {
    let lock = RADAR_DISTANCE_PROBED.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut map) = lock.lock() else {
        return false; // 取不到鎖＝先不要探測（寧可少打學校）
    };
    let now = Instant::now();
    map.retain(|_, at| now.saturating_duration_since(*at) < RADAR_STATE_TTL);
    !map.contains_key(&(provider.to_string(), rollcall_id, account_id.to_string()))
}

fn mark_distance_probed(provider: &str, rollcall_id: u64, account_id: &str) {
    let lock = RADAR_DISTANCE_PROBED.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut map) = lock.lock() else {
        return;
    };
    map.insert(
        (provider.to_string(), rollcall_id, account_id.to_string()),
        Instant::now(),
    );
}

/// 點名還開著嗎？（棋盤格掃描前確認一次，關掉就不要再掃）
async fn rollcall_still_open(api: &ApiClient, rollcall_id: u64) -> bool {
    match api.get_rollcalls().await {
        Ok(list) => list.iter().any(|r| r.rollcall_id == rollcall_id),
        Err(e) => {
            debug!(error = %e, "確認點名狀態失敗（照樣繼續掃棋盤格）");
            true
        }
    }
}

// ─── 測試 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Haversine 距離 ────────────────────────────────────────────────────────

    #[test]
    fn test_haversine_same_point() {
        let d = haversine_distance_meters(24.3, 118.0, 24.3, 118.0);
        assert!(d.abs() < 1e-6, "同點距離應為 0，got {d}");
    }

    #[test]
    fn test_haversine_known_distance() {
        // 台北市政府 (25.0408, 121.5654) 到台北101 (25.0336, 121.5646) 約 800m
        let d = haversine_distance_meters(25.0408, 121.5654, 25.0336, 121.5646);
        assert!(
            (700.0..=900.0).contains(&d),
            "台北市政府到台北101距離應在 700~900m，got {d:.2}m"
        );
    }

    #[test]
    fn test_haversine_symmetry() {
        let d1 = haversine_distance_meters(24.3, 118.0, 24.6, 118.2);
        let d2 = haversine_distance_meters(24.6, 118.2, 24.3, 118.0);
        assert!((d1 - d2).abs() < 1e-6, "距離應對稱");
    }

    #[test]
    fn test_haversine_equator_1_degree() {
        // 赤道上 1 度經差 ≈ 111,320m
        let d = haversine_distance_meters(0.0, 0.0, 0.0, 1.0);
        assert!(
            (111_000.0..=111_700.0).contains(&d),
            "赤道 1 度 ≈ 111.32km，got {d:.2}m"
        );
    }

    // ── 座標轉換 ──────────────────────────────────────────────────────────────

    #[test]
    fn test_cartesian_round_trip() {
        let origin = Coordinate::new(24.3, 118.0);
        let point = Coordinate::new(24.35, 118.05);

        let (x, y) = point.to_cartesian_meters(&origin);
        let recovered = Coordinate::from_cartesian_meters(x, y, &origin);

        assert!(
            (recovered.latitude - point.latitude).abs() < 1e-8,
            "緯度轉換誤差過大：{:.10} vs {:.10}",
            recovered.latitude,
            point.latitude
        );
        assert!(
            (recovered.longitude - point.longitude).abs() < 1e-8,
            "經度轉換誤差過大：{:.10} vs {:.10}",
            recovered.longitude,
            point.longitude
        );
    }

    #[test]
    fn test_cartesian_origin_is_zero() {
        let origin = Coordinate::new(24.3, 118.0);
        let (x, y) = origin.to_cartesian_meters(&origin);
        assert!(x.abs() < 1e-6 && y.abs() < 1e-6, "原點轉換應為 (0,0)");
    }

    // ── 圓交叉點算法 ──────────────────────────────────────────────────────────

    #[test]
    fn test_circle_intersection_basic() {
        // 兩圓：圓心距 d，半徑分別為 r1, r2，使得它們確實相交
        // 設 c1 = (0, 0)，c2 = (0, 0.01)（約 1.1km），r1 = r2 = 800m
        let c1 = Coordinate::new(24.0, 118.0);
        let c2 = Coordinate::new(24.01, 118.0); // 約 1111m
        let r1 = 800.0;
        let r2 = 800.0;

        let result = circle_intersection(&c1, r1, &c2, r2);
        match result {
            CircleIntersection::Two(p1, p2) => {
                // 驗證兩個交叉點確實在兩圓上
                let d1_to_p1 =
                    haversine_distance_meters(c1.latitude, c1.longitude, p1.latitude, p1.longitude);
                let d2_to_p1 =
                    haversine_distance_meters(c2.latitude, c2.longitude, p1.latitude, p1.longitude);

                assert!(
                    (d1_to_p1 - r1).abs() < 5.0,
                    "P1 應在圓1上（誤差 < 5m），got {d1_to_p1:.2} vs {r1}"
                );
                assert!(
                    (d2_to_p1 - r2).abs() < 5.0,
                    "P1 應在圓2上（誤差 < 5m），got {d2_to_p1:.2} vs {r2}"
                );

                let d1_to_p2 =
                    haversine_distance_meters(c1.latitude, c1.longitude, p2.latitude, p2.longitude);
                let d2_to_p2 =
                    haversine_distance_meters(c2.latitude, c2.longitude, p2.latitude, p2.longitude);

                assert!(
                    (d1_to_p2 - r1).abs() < 5.0,
                    "P2 應在圓1上，got {d1_to_p2:.2} vs {r1}"
                );
                assert!(
                    (d2_to_p2 - r2).abs() < 5.0,
                    "P2 應在圓2上，got {d2_to_p2:.2} vs {r2}"
                );
            }
            other => panic!("預期兩個交叉點，got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn test_circle_intersection_no_intersection_too_far() {
        let c1 = Coordinate::new(24.0, 118.0);
        let c2 = Coordinate::new(25.0, 118.0); // 約 111km
        let result = circle_intersection(&c1, 1000.0, &c2, 1000.0);
        assert!(
            matches!(result, CircleIntersection::None),
            "兩圓相距太遠，應無交叉點"
        );
    }

    #[test]
    fn test_circle_intersection_no_intersection_contained() {
        let c1 = Coordinate::new(24.0, 118.0);
        let c2 = Coordinate::new(24.0001, 118.0001); // 很近
                                                     // 小圓完全在大圓內
        let result = circle_intersection(&c1, 10000.0, &c2, 100.0);
        assert!(
            matches!(result, CircleIntersection::None),
            "小圓在大圓內，應無交叉點"
        );
    }

    #[test]
    fn test_circle_intersection_tangent() {
        // 兩圓外切：d = r1 + r2
        // c1 = (24.0, 118.0), c2 約 2000m 外, r1 = r2 = 1000m
        let c1 = Coordinate::new(24.0, 118.0);
        // 往北 0.018 度 ≈ 2000m
        let c2 = Coordinate::new(24.018, 118.0);
        let d = haversine_distance_meters(c1.latitude, c1.longitude, c2.latitude, c2.longitude);
        let r = d / 2.0;
        let result = circle_intersection(&c1, r, &c2, r);
        // 外切時應為一個交叉點
        assert!(
            matches!(
                result,
                CircleIntersection::One(_) | CircleIntersection::Two(_, _)
            ),
            "外切應為一個（或接近一個）交叉點，got d={d:.2}, r={r:.2}"
        );
    }

    #[test]
    fn test_circle_intersection_identical() {
        let c = Coordinate::new(24.0, 118.0);
        let result = circle_intersection(&c, 1000.0, &c, 1000.0);
        assert!(
            matches!(result, CircleIntersection::Infinite),
            "完全相同的兩圓應為 Infinite"
        );
    }

    // ── 候選座標計算 ──────────────────────────────────────────────────────────

    #[test]
    fn test_compute_radar_candidates_two_points() {
        // 已知教室在 (24.5, 118.1)
        // 從 P1 量得距離，從 P2 量得距離
        let classroom = Coordinate::new(24.5, 118.1);
        let p1 = Coordinate::new(24.3, 118.0);
        let p2 = Coordinate::new(24.6, 118.2);

        let d1 = classroom.distance_meters(&p1);
        let d2 = classroom.distance_meters(&p2);

        let measurements = vec![(p1, d1), (p2, d2)];
        let candidates = compute_radar_candidates(&measurements);

        assert!(!candidates.is_empty(), "應有候選座標");

        // 至少一個候選座標應接近教室（容差 100m）
        let best = candidates
            .iter()
            .map(|c| c.distance_meters(&classroom))
            .fold(f64::INFINITY, f64::min);

        assert!(best < 100.0, "最佳候選應在教室 100m 內，got {best:.2}m");
    }

    #[test]
    fn test_compute_radar_candidates_empty_when_insufficient() {
        let measurements: Vec<(Coordinate, f64)> = vec![];
        let candidates = compute_radar_candidates(&measurements);
        assert!(candidates.is_empty());

        let one = vec![(Coordinate::new(24.0, 118.0), 500.0)];
        let candidates = compute_radar_candidates(&one);
        assert!(candidates.is_empty());
    }

    // ── estimate_location_from_distances ──────────────────────────────────────

    #[test]
    fn test_estimate_location_two_measurements() {
        let classroom = Coordinate::new(24.5, 118.1);
        let p1 = Coordinate::new(24.3, 118.0);
        let p2 = Coordinate::new(24.6, 118.2);
        let d1 = classroom.distance_meters(&p1);
        let d2 = classroom.distance_meters(&p2);

        // 兩點定位通常會得到兩個交叉點，因此 estimate_location_from_distances
        // 不再任意回傳其中一個；完整流程仍以 compute_radar_candidates 驗證。
        let est = estimate_location_from_distances(&[(p1, d1), (p2, d2)]);
        assert!(est.is_none(), "兩個交叉點時不應任意估算唯一位置");

        // 使用 compute_radar_candidates 驗證完整流程
        // 它會返回所有候選點（包含兩個交叉點），至少有一個應接近教室
        let candidates = compute_radar_candidates(&[(p1, d1), (p2, d2)]);
        assert!(!candidates.is_empty(), "應有候選座標");

        let best_dist = candidates
            .iter()
            .map(|c| c.distance_meters(&classroom))
            .fold(f64::INFINITY, f64::min);

        assert!(
            best_dist < 100.0,
            "候選列表中至少有一個座標應在教室 100m 內，最佳距離：{best_dist:.2}m"
        );
    }

    #[test]
    fn test_estimate_location_one_measurement() {
        let p = Coordinate::new(24.5, 118.1);
        let est = estimate_location_from_distances(&[(p, 500.0)]);
        assert!(est.is_some());
        // 只有一個量測時，返回圓心本身
        let est = est.unwrap();
        assert!((est.latitude - p.latitude).abs() < 1e-8);
    }

    #[test]
    fn test_estimate_location_empty() {
        let est = estimate_location_from_distances(&[]);
        assert!(est.is_none());
    }

    // ── weighted_midpoint ─────────────────────────────────────────────────────

    #[test]
    fn test_weighted_midpoint_equal_radii() {
        let c1 = Coordinate::new(24.0, 118.0);
        let c2 = Coordinate::new(25.0, 119.0);
        let mid = weighted_midpoint(&c1, 1000.0, &c2, 1000.0);
        // 等半徑時應為算術中點
        assert!((mid.latitude - 24.5).abs() < 1e-8);
        assert!((mid.longitude - 118.5).abs() < 1e-8);
    }

    #[test]
    fn test_weighted_midpoint_biased() {
        let c1 = Coordinate::new(24.0, 118.0);
        let c2 = Coordinate::new(26.0, 120.0);
        // r1 很小（精確），r2 很大（不精確）→ 應偏向 c1
        let mid = weighted_midpoint(&c1, 10.0, &c2, 10000.0);
        assert!(
            mid.latitude < 24.2,
            "中點應偏向 c1（lat=24.0），got {:.4}",
            mid.latitude
        );
    }

    // ── dedup_coordinates ─────────────────────────────────────────────────────

    #[test]
    fn test_dedup_coordinates() {
        let c1 = Coordinate::new(24.0, 118.0);
        let c2 = Coordinate::new(24.0001, 118.0001); // 約 15m，< 50m 閾值
        let c3 = Coordinate::new(25.0, 119.0); // 很遠

        let deduped = dedup_coordinates(vec![c1, c2, c3], 50.0);
        assert_eq!(deduped.len(), 2, "c1 和 c2 應被去重，剩 2 個");
    }

    #[test]
    fn test_dedup_coordinates_no_dup() {
        let coords = vec![
            Coordinate::new(24.0, 118.0),
            Coordinate::new(25.0, 119.0),
            Coordinate::new(26.0, 120.0),
        ];
        let deduped = dedup_coordinates(coords.clone(), 50.0);
        assert_eq!(deduped.len(), 3);
    }

    // ── coordinate display ────────────────────────────────────────────────────

    #[test]
    fn test_coordinate_display() {
        let c = Coordinate::new(24.123456, 118.654321);
        let s = c.to_string();
        assert!(s.contains("24.123456"));
        assert!(s.contains("118.654321"));
    }

    // ── radar result display ──────────────────────────────────────────────────

    #[test]
    fn test_radar_result_display() {
        let success = RadarResult::Success {
            coord: Coordinate::new(24.3, 118.0),
        };
        assert!(success.to_string().contains("成功"));

        let empty = RadarResult::SuccessEmpty;
        assert!(empty.to_string().contains("空答案"));
        assert!(empty.is_success());

        let failed = RadarResult::Failed {
            last_distance: Some(42.5),
            tried_coords: vec![],
        };
        assert!(failed.to_string().contains("失敗"));
        assert!(!failed.is_success());

        let err = RadarResult::Error("timeout".into());
        assert!(err.to_string().contains("timeout"));
    }

    // ── RADAR_REFERENCE_PARITY_V1：空答案只試一次 ─────────────────────────────

    #[test]
    fn test_empty_answer_allowed_only_once() {
        // 用測試專屬的 key，不干擾其他測試或正式路徑。
        let (provider, rollcall, account) = ("unittest-provider", 987_654_321_u64, "unittest-account");
        assert!(empty_answer_allowed(provider, rollcall, account), "第一次應該可以試");
        mark_empty_answer_tried(provider, rollcall, account);
        assert!(
            !empty_answer_allowed(provider, rollcall, account),
            "試過之後同一個帳號在這場點名不該再試"
        );
        // 不同帳號是各自獨立的（每個人要自己簽到）。
        assert!(empty_answer_allowed(provider, rollcall, "another-account"));
        // 換一場點名也是新的。
        assert!(empty_answer_allowed(provider, rollcall + 1, account));
    }

    // ── RADAR_REFERENCE_PARITY_V1：棋盤格 ─────────────────────────────────────

    #[test]
    fn test_grid_candidate_rings() {
        let center = Coordinate::new(25.0330, 121.5654); // 台北
        let step = RADAR_GRID_STEP_METERS;

        // 第 0 格＝中心
        let zero = grid_candidate(0, &center, step).expect("第 0 格是中心");
        assert!(zero.distance_meters(&center) < 0.5);

        // 1..=8 是第 1 圈：八個方向各一格，距離都接近 100 公尺
        let mut ring1 = Vec::new();
        for index in 1..=8usize {
            let candidate = grid_candidate(index, &center, step).expect("第 1 圈應該都在半徑內");
            let distance = center.distance_meters(&candidate);
            assert!(
                (distance - step).abs() < 5.0,
                "第 1 圈的格子應該離中心約 {step} 公尺，實際 {distance:.1}"
            );
            ring1.push(candidate);
        }
        // 八格互不相同（沒有重複列舉）
        for i in 0..ring1.len() {
            for j in (i + 1)..ring1.len() {
                assert!(
                    ring1[i].distance_meters(&ring1[j]) > 1.0,
                    "第 1 圈的第 {i} 格與第 {j} 格重疊"
                );
            }
        }

        // 第 2 圈（index 9..=24）距離應該落在 100～150 公尺之間（邊格 100、角格 ≈141）
        for index in 9..=24usize {
            let candidate = grid_candidate(index, &center, step).expect("第 2 圈應該都在半徑內");
            let distance = center.distance_meters(&candidate);
            assert!(
                distance >= step - 5.0 && distance <= step * 1.5 + 5.0,
                "第 2 圈距離應在 100～150 公尺之間，實際 {distance:.1}"
            );
        }
    }

    #[test]
    fn test_grid_candidate_stops_at_max_radius() {
        let center = Coordinate::new(25.0330, 121.5654);
        let step = RADAR_GRID_STEP_METERS;
        let mut index = 0usize;
        let mut last_inside = None;
        // 上限半徑內一定有格子；掃到第一個 None 就停（避免測試自己無限迴圈）。
        while index < 200_000 {
            match grid_candidate(index, &center, step) {
                Some(candidate) => {
                    assert!(
                        center.distance_meters(&candidate) <= RADAR_GRID_MAX_RADIUS_METERS + 1.0,
                        "不該產生超過上限半徑的格子（index {index}）"
                    );
                    last_inside = Some(index);
                    index += 1;
                }
                None => break,
            }
        }
        assert!(index > 0, "至少要有中心那一格");
        assert!(index < 200_000, "掃描必須會停在上限半徑（不能無限延伸）");
        assert!(
            last_inside.map(|i| i > 100).unwrap_or(false),
            "上限半徑 3 公里內應該有上百格可掃"
        );
    }
}
