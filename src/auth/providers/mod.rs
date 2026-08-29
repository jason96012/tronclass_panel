use std::sync::Arc;

use async_trait::async_trait;
use reqwest::{cookie::Jar, Client};

use crate::account::AccountConfig;
use crate::config::ProviderKind;

use super::AuthSession;

mod fju;
// TIMETABLE_AUTO_SYNC_V1: `timetable_source::tku` 重用這裡的 SSO 核心
// （`sso_login_to_service`），因此模組本身必須對 crate 內可見。
pub(crate) mod tku;

// ─── AuthFlow trait ───────────────────────────────────────────────────────────

#[async_trait]
pub(crate) trait AuthFlow: Send + Sync {
    async fn login(
        &self,
        client: &Client,
        cookie_jar: &Arc<Jar>,
        base_url: &str,
        account: &AccountConfig,
    ) -> miette::Result<AuthSession>;
}

// ─── Provider registry ───────────────────────────────────────────────────────

pub(crate) fn get_auth_flow(account: &AccountConfig) -> Box<dyn AuthFlow> {
    // 先保留舊版行為：provider 名稱是 `fju` / `tku` 時直接使用對應流程。
    // 若使用者把 provider 命名為 `tamkang` 之類，也可透過 kind = "tku" 分派。
    match account.provider.as_str() {
        "fju" => return fju::create_flow(),
        "tku" => return tku::create_flow(),
        _ => {}
    }

    match &account.provider_config.kind {
        ProviderKind::FJU => fju::create_flow(),
        ProviderKind::TKU => tku::create_flow(),
        ProviderKind::Tronclass => panic!(
            "unknown provider: {} (kind = tronclass has no built-in auth flow yet)",
            account.provider
        ),
    }
}
