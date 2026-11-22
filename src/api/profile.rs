//! 使用者資料 Payload、Response 資料結構，以及 /api/profile 呼叫實作

use miette::{IntoDiagnostic, Result, WrapErr};
use serde::{Deserialize, Serialize};
use tracing::{debug, instrument};

// ─── Response 資料結構 ────────────────────────────────────────────────────────

/// `GET /api/profile` — 使用者基本資訊
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UserProfile {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub user_no: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub department: Option<ProfileDepartment>,
    #[serde(default)]
    pub grade: Option<ProfileGrade>,
    #[serde(default)]
    pub klass: Option<ProfileKlass>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProfileDepartment {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub short_name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProfileGrade {
    pub id: u64,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProfileKlass {
    pub id: u64,
    pub name: String,
}

// ─── ApiClient /api/profile 方法 ──────────────────────────────────────────────

impl super::ApiClient {
    /// `GET /api/profile` — 取得使用者資料（同時驗證 Session 有效性）
    #[instrument(skip(self))]
    pub async fn get_profile(&self) -> Result<UserProfile> {
        let url = format!("{}/api/profile", self.base_url);
        debug!(url = %url, "GET /api/profile");

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .into_diagnostic()
            .wrap_err("GET /api/profile failed")?;

        self.handle_response(resp).await
    }
}

// ─── 測試 ─────────────────────────────────────────────────────────────────────

