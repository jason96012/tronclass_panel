//! RADAR_PANEL_V1（fix70）：雷達點名設定的「執行期有效設定」。
//!
//! 雷達座標原本只能改 `config.toml`，而且改完要重啟。這裡把面板上的設定（座標清單／
//! accuracy／altitude／「我的位置」）放進記憶體，讓 monitor 每次雷達簽到都取用最新的值：
//!
//! - `config.toml` 的 `providers.<provider>.radar` ＝ **基準值**（啟動時登記，見 [`set_baselines`]）
//! - 面板存進資料庫的覆寫 ＝ **優先於基準值**（啟動時 [`load_from_db`]，儲存時 [`set`] 立即生效）
//! - 「我的位置」只有在開關開啟、且有取得座標時，才會**接在座標清單最後**一起嘗試
//!
//! 為什麼要放記憶體而不是每次查資料庫：雷達簽到發生在 monitor 迴圈裡（每次點名可能連打
//! 好幾個座標），這裡只讀一份 `RwLock<HashMap>`，不需要為了設定值增加資料庫往返。
//! 資料庫仍是唯一真相（重啟時載入），記憶體只是快取。

use std::collections::HashMap;

use once_cell::sync::Lazy;
use tokio::sync::RwLock;

use crate::config::RadarConfig;

/// 座標清單上限（面板與 API 都不得超過）。
pub const MAX_RADAR_COORDS: usize = 20;
/// accuracy 的合法範圍（公尺）。
pub const MIN_RADAR_ACCURACY: u32 = 1;
pub const MAX_RADAR_ACCURACY: u32 = 10_000;
/// altitude 的合法範圍（公尺；死海～聖母峰等級的寬鬆範圍）。
pub const MIN_RADAR_ALTITUDE: i32 = -500;
pub const MAX_RADAR_ALTITUDE: i32 = 9_000;
/// 「我的位置」與其他座標距離小於此值時視為同一點（與 radar.rs 的候選去重一致）。
pub const DEVICE_LOCATION_DEDUP_METERS: f64 = 10.0;

/// 一個 provider 的覆寫內容。
#[derive(Debug, Clone, PartialEq)]
pub struct RadarOverride {
    /// 座標清單（有序，最多 [`MAX_RADAR_COORDS`] 組）。空清單＝沿用設定檔的座標。
    pub coords: Vec<[f64; 2]>,
    pub accuracy: u32,
    pub altitude: i32,
    /// 是否把「我的位置」列入雷達考量範圍。
    pub use_device_location: bool,
    /// 面板上按按鈕取得的座標（緯度、經度）。
    pub device_location: Option<[f64; 2]>,
    pub device_label: String,
    pub device_updated_at: i64,
    /// 這筆覆寫最後儲存的時間（Unix 秒；面板顯示用）。
    pub updated_at: i64,
}

/// `provider` → 覆寫。
static OVERRIDES: Lazy<RwLock<HashMap<String, RadarOverride>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// 設定檔的基準值（`provider` → `config.toml` 的 radar 設定）。
static BASELINES: Lazy<RwLock<HashMap<String, RadarConfig>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

// ─── 數值收斂（面板、API、資料庫三邊共用同一套規則）───────────────────────────

pub fn clamp_accuracy(value: u32) -> u32 {
    value.clamp(MIN_RADAR_ACCURACY, MAX_RADAR_ACCURACY)
}

pub fn clamp_altitude(value: i32) -> i32 {
    value.clamp(MIN_RADAR_ALTITUDE, MAX_RADAR_ALTITUDE)
}

/// 合法經緯度才保留；同時去除極接近的重複點並套用數量上限。
pub fn sanitize_coords(coords: Vec<[f64; 2]>) -> Vec<[f64; 2]> {
    let mut out: Vec<[f64; 2]> = Vec::new();
    for [lat, lon] in coords {
        if !is_valid_coordinate(lat, lon) {
            continue;
        }
        let near_duplicate = out
            .iter()
            .any(|[a, b]| crate::rollcalls::radar::haversine_distance_meters(*a, *b, lat, lon) < DEVICE_LOCATION_DEDUP_METERS);
        if near_duplicate {
            continue;
        }
        out.push([lat, lon]);
        if out.len() >= MAX_RADAR_COORDS {
            break;
        }
    }
    out
}

pub fn is_valid_coordinate(lat: f64, lon: f64) -> bool {
    lat.is_finite()
        && lon.is_finite()
        && (-90.0..=90.0).contains(&lat)
        && (-180.0..=180.0).contains(&lon)
        && !(lat == 0.0 && lon == 0.0) // (0,0) 幾乎一定是「沒拿到位置」的佔位值
}

/// 「我的位置」的收斂：不合法的座標直接視為沒有（寧可退回設定檔座標，也不要送出 (0,0)）。
pub fn clamp_device_location(
    lat: Option<f64>,
    lon: Option<f64>,
) -> (Option<f64>, Option<f64>) {
    match (lat, lon) {
        (Some(lat), Some(lon)) if is_valid_coordinate(lat, lon) => (Some(lat), Some(lon)),
        _ => (None, None),
    }
}

// ─── 座標序列化（資料庫以 JSON 字串存）────────────────────────────────────────

pub fn encode_coords(coords: &[[f64; 2]]) -> String {
    serde_json::to_string(coords).unwrap_or_else(|_| "[]".to_string())
}

/// 解析資料庫裡的 JSON；壞資料一律跳過（不 panic、不讓整個 provider 失效）。
pub fn parse_coords(raw: &str) -> Vec<[f64; 2]> {
    let parsed: Vec<[f64; 2]> = serde_json::from_str(raw).unwrap_or_default();
    sanitize_coords(parsed)
}

// ─── 記憶體存取 ───────────────────────────────────────────────────────────────

/// 啟動時登記 `config.toml` 的基準值（每個 provider 一組）。
pub async fn set_baselines(providers: &HashMap<String, RadarConfig>) {
    let mut baselines = BASELINES.write().await;
    baselines.clear();
    for (provider, radar) in providers {
        baselines.insert(provider.clone(), radar.clone());
    }
}

/// 設定檔的基準值。
pub async fn baseline(provider: &str) -> Option<RadarConfig> {
    BASELINES.read().await.get(provider).cloned()
}

/// 目前有基準值或有覆寫的 provider 清單（面板的下拉選單）。
pub async fn known_providers() -> Vec<String> {
    let baselines = BASELINES.read().await;
    let overrides = OVERRIDES.read().await;
    let mut names: Vec<String> = baselines
        .keys()
        .chain(overrides.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    names.sort();
    names
}

/// 啟動時把資料庫裡的覆寫載入記憶體，回傳載入的 provider 數量。
pub async fn load_from_db(db: &crate::db::AccountsDb) -> miette::Result<usize> {
    let rows = db.list_radar_preferences().await?;
    let mut overrides = OVERRIDES.write().await;
    overrides.clear();
    let mut loaded = 0usize;
    for row in rows {
        let (device_latitude, device_longitude) =
            clamp_device_location(row.device_latitude, row.device_longitude);
        overrides.insert(
            row.provider.clone(),
            RadarOverride {
                coords: parse_coords(&row.coords),
                accuracy: clamp_accuracy(u32::try_from(row.accuracy).unwrap_or(35)),
                altitude: clamp_altitude(i32::try_from(row.altitude).unwrap_or(0)),
                use_device_location: row.use_device_location,
                device_location: match (device_latitude, device_longitude) {
                    (Some(lat), Some(lon)) => Some([lat, lon]),
                    _ => None,
                },
                device_label: row.device_label,
                device_updated_at: row.device_updated_at,
                updated_at: row.updated_at,
            },
        );
        loaded += 1;
    }
    Ok(loaded)
}

/// 面板儲存後立即生效。
pub async fn set(provider: &str, value: RadarOverride) {
    OVERRIDES.write().await.insert(provider.to_string(), value);
}

/// 面板按「回到設定檔預設」後立即生效。
pub async fn clear(provider: &str) {
    OVERRIDES.write().await.remove(provider);
}

/// 讀出目前覆寫（面板顯示「來源：面板」時用）。
pub async fn snapshot(provider: &str) -> Option<RadarOverride> {
    OVERRIDES.read().await.get(provider).cloned()
}

// ─── 有效設定（monitor 實際使用）──────────────────────────────────────────────

/// 把「設定檔基準值」與「面板覆寫」合併成 monitor 實際要用的雷達設定。
///
/// 規則：
/// 1. 覆寫的座標清單非空 → 用它；否則用設定檔的座標。
/// 2. `use_device_location` 且有取得座標 → **接在最後**一起嘗試
///    （與其他座標距離 < [`DEVICE_LOCATION_DEDUP_METERS`] 時不重複加入）。
///    放在最後是刻意的：人在教室附近時它就是最快命中的那一組；
///    人在別處時，它失敗回傳的距離也仍然是一筆有效量測，會讓圓交叉點的推算更準。
/// 3. `accuracy`／`altitude` 一律優先採用覆寫值。
pub async fn effective(provider: &str) -> Option<RadarConfig> {
    let baseline = baseline(provider).await;
    let overrides = OVERRIDES.read().await;
    let Some(override_) = overrides.get(provider) else {
        return baseline;
    };

    // `RadarConfig` 沒有 Default（設定檔的預設座標是 `[24.3, 118.0]` 這種佔位值，
    // 拿它當預設會讓「沒有基準值的 provider」偷偷多打一次無意義的座標）。
    let mut base = match baseline {
        Some(base) => base,
        None => RadarConfig {
            default_coords: Vec::new(),
            accuracy: override_.accuracy,
            altitude: override_.altitude,
        },
    };
    if !override_.coords.is_empty() {
        base.default_coords = override_.coords.clone();
    }
    base.accuracy = override_.accuracy;
    base.altitude = override_.altitude;

    if override_.use_device_location {
        if let Some([lat, lon]) = override_.device_location {
            let already = base
                .default_coords
                .iter()
                .any(|[a, b]| {
                    crate::rollcalls::radar::haversine_distance_meters(*a, *b, lat, lon)
                        < DEVICE_LOCATION_DEDUP_METERS
                });
            if !already && base.default_coords.len() < MAX_RADAR_COORDS {
                base.default_coords.push([lat, lon]);
            }
        }
    }

    Some(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_drops_invalid_and_duplicates() {
        let coords = vec![
            [25.0373, 121.4315],
            [25.0373, 121.4315], // 完全重複
            [25.03730, 121.43150],
            [91.0, 0.0],   // 緯度不合法
            [0.0, 0.0],    // 佔位值
            [24.5, 200.0], // 經度不合法
            [24.9, 121.0],
        ];
        let out = sanitize_coords(coords);
        assert_eq!(out.len(), 2, "應只留下兩個合法且不重複的座標：{out:?}");
    }

    #[test]
    fn sanitize_limits_count() {
        let coords: Vec<[f64; 2]> = (0..40)
            .map(|i| [24.0 + i as f64 * 0.01, 121.0])
            .collect();
        assert_eq!(sanitize_coords(coords).len(), MAX_RADAR_COORDS);
    }

    #[test]
    fn parse_coords_tolerates_garbage() {
        assert!(parse_coords("not json").is_empty());
        assert!(parse_coords("[]").is_empty());
        assert_eq!(parse_coords("[[24.5,121.0]]").len(), 1);
    }

    #[test]
    fn encode_round_trip() {
        let coords = vec![[25.0373, 121.4315], [24.9, 121.0]];
        assert_eq!(parse_coords(&encode_coords(&coords)), coords);
    }

    #[test]
    fn clamps_stay_in_range() {
        assert_eq!(clamp_accuracy(0), MIN_RADAR_ACCURACY);
        assert_eq!(clamp_accuracy(99_999), MAX_RADAR_ACCURACY);
        assert_eq!(clamp_altitude(-9_999), MIN_RADAR_ALTITUDE);
        assert_eq!(clamp_altitude(99_999), MAX_RADAR_ALTITUDE);
        assert_eq!(clamp_device_location(Some(0.0), Some(0.0)), (None, None));
        assert_eq!(
            clamp_device_location(Some(25.0373), Some(121.4315)),
            (Some(25.0373), Some(121.4315))
        );
    }
}
