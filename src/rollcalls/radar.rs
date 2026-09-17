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
use std::time::{Duration, Instant};

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

    let mut tried_coords: Vec<Coordinate> = Vec::new();
    // 量測資料：(座標, 距離) 用於後續圓交叉點計算
    let mut measurements: Vec<(Coordinate, f64)> = Vec::new();
    // 輸入頁送來、已經試過的座標（同一組不重複試，避免使用者連按時多打學校）。
    let mut manual_tried: Vec<Coordinate> = Vec::new();

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

    if measurements.len() >= 2 {
        info!(
            measurements = measurements.len(),
            "Phase 2：使用 {} 個距離量測點計算候選座標",
            measurements.len()
        );

        let candidates = compute_radar_candidates(&measurements);

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
                        grid_cursor_store(provider, rollcall_id, account_id, &center, next);
                        return RadarResult::Success { coord: candidate };
                    }
                    Ok(SingleRadarResult::TooFar { distance }) => {
                        measurements.push((candidate, distance));
                    }
                    Ok(SingleRadarResult::OtherFailure { reason }) => {
                        warn!(reason = %reason, "棋盤格第 {} 格失敗（非距離錯誤）", next - 1);
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
}

/// RADAR_INPUT_PAGE_V1（fix71）：同一組「輸入頁座標」的重複判定距離（公尺）。
/// 使用者連按送出時，10 公尺內視為同一組，不重複打學校。
const MANUAL_POINT_DEDUP_METERS: f64 = 10.0;

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
        AttendanceResult::TransientFailure { reason } => {
            Ok(SingleRadarResult::OtherFailure { reason })
        }
        AttendanceResult::Failed { reason } => Ok(SingleRadarResult::OtherFailure { reason }),
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

static RADAR_EMPTY_ANSWER_TRIED: OnceLock<Mutex<HashMap<RadarStateKey, Instant>>> =
    OnceLock::new();
static RADAR_GRID_CURSOR: OnceLock<Mutex<HashMap<RadarStateKey, GridCursorState>>> =
    OnceLock::new();
static RADAR_GRID_LAST_SCAN: OnceLock<Mutex<HashMap<(String, u64), Instant>>> = OnceLock::new();

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
