//! RADAR_PANEL_V1（fix70）：雷達點名設定的持久化。
//!
//! 過去雷達座標只能改 `config.toml`（且要重啟）。面板化之後改成：**每個 provider 一組**，
//! 存進資料庫（重啟不會消失），並由 `crate::radar_settings` 載入記憶體供 monitor 即時取用。
//!
//! `coords` 以 JSON 字串存（`[[lat, lon], ...]`）：座標是「一組有序清單」，
//! 用關聯表反而要處理排序與刪除，JSON 在這裡更簡單也更好驗證。

use miette::{IntoDiagnostic, Result, WrapErr};
use sqlx::FromRow;

use super::AccountsDb;

/// 一列＝一個 provider 的雷達設定覆寫。
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct RadarPreferenceRow {
    pub provider: String,
    /// JSON：`[[lat, lon], ...]`（有序，最多 [`crate::radar_settings::MAX_RADAR_COORDS`] 組）。
    pub coords: String,
    pub accuracy: i64,
    pub altitude: i64,
    /// 是否把「我的位置」（面板上按按鈕取得）列入雷達考量範圍。
    pub use_device_location: bool,
    pub device_latitude: Option<f64>,
    pub device_longitude: Option<f64>,
    /// 取得位置的來源說明（裝置／瀏覽器字串），純顯示用。
    pub device_label: String,
    pub device_updated_at: i64,
    pub updated_at: i64,
    pub updated_time: String,
}

impl AccountsDb {
    /// 讀取某 provider 的雷達設定覆寫；沒設定過回 `None`（＝沿用 `config.toml`）。
    pub async fn get_radar_preference(&self, provider: &str) -> Result<Option<RadarPreferenceRow>> {
        let provider = provider.trim();
        if provider.is_empty() {
            return Ok(None);
        }
        sqlx::query_as::<_, RadarPreferenceRow>(
            "SELECT provider, coords, accuracy, altitude, use_device_location,
                    device_latitude, device_longitude, device_label, device_updated_at,
                    updated_at, updated_time
             FROM radar_preferences WHERE provider = ?",
        )
        .bind(provider)
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("讀取 `{provider}` 的雷達設定失敗"))
    }

    /// 列出所有已覆寫的 provider（啟動時載入記憶體用）。
    pub async fn list_radar_preferences(&self) -> Result<Vec<RadarPreferenceRow>> {
        sqlx::query_as::<_, RadarPreferenceRow>(
            "SELECT provider, coords, accuracy, altitude, use_device_location,
                    device_latitude, device_longitude, device_label, device_updated_at,
                    updated_at, updated_time
             FROM radar_preferences ORDER BY provider",
        )
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("列出雷達設定失敗")
    }

    /// 寫入／更新覆寫。數值一律先收斂到合法範圍（與面板的輸入限制一致）。
    #[allow(clippy::too_many_arguments)]
    pub async fn set_radar_preference(
        &self,
        provider: &str,
        coords_json: &str,
        accuracy: u32,
        altitude: i32,
        use_device_location: bool,
        device_latitude: Option<f64>,
        device_longitude: Option<f64>,
        device_label: &str,
        device_updated_at: i64,
    ) -> Result<()> {
        let provider = provider.trim();
        let accuracy = crate::radar_settings::clamp_accuracy(accuracy) as i64;
        let altitude = crate::radar_settings::clamp_altitude(altitude) as i64;
        let (device_latitude, device_longitude) =
            crate::radar_settings::clamp_device_location(device_latitude, device_longitude);
        let device_label = crate::text_utils::truncate_chars(device_label.trim(), 64);
        let now = chrono::Local::now();
        sqlx::query(
            "INSERT INTO radar_preferences
                (provider, coords, accuracy, altitude, use_device_location,
                 device_latitude, device_longitude, device_label, device_updated_at,
                 updated_at, updated_time)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(provider) DO UPDATE SET
                coords = excluded.coords,
                accuracy = excluded.accuracy,
                altitude = excluded.altitude,
                use_device_location = excluded.use_device_location,
                device_latitude = excluded.device_latitude,
                device_longitude = excluded.device_longitude,
                device_label = excluded.device_label,
                device_updated_at = excluded.device_updated_at,
                updated_at = excluded.updated_at,
                updated_time = excluded.updated_time",
        )
        .bind(provider)
        .bind(coords_json)
        .bind(accuracy)
        .bind(altitude)
        .bind(use_device_location)
        .bind(device_latitude)
        .bind(device_longitude)
        .bind(device_label)
        .bind(device_updated_at)
        .bind(now.timestamp())
        .bind(now.format("%Y-%m-%d %H:%M:%S").to_string())
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("儲存 `{provider}` 的雷達設定失敗"))?;
        Ok(())
    }

    /// 刪除覆寫（回到 `config.toml` 的設定）。
    pub async fn clear_radar_preference(&self, provider: &str) -> Result<()> {
        let provider = provider.trim();
        sqlx::query("DELETE FROM radar_preferences WHERE provider = ?")
            .bind(provider)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("清除 `{provider}` 的雷達設定失敗"))?;
        Ok(())
    }
}
