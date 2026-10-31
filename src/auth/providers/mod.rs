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

/// PROVIDER_KIND_NO_PANIC_V1：依 provider 名稱／kind 決定實際使用的登入流程。
///
/// 分派規則（與舊版一致）：provider 名稱是 `fju` / `tku` 時直接對應；否則看
/// `kind`。`kind = "tronclass"`（預設值）目前**沒有**內建登入流程，回 `None`。
/// 這個函式是唯一的分派來源——`get_auth_flow`、啟動期驗證、新增帳號檢查
/// 都以它為準，避免三處規則走樣。
pub(crate) fn resolve_flow_kind(provider_name: &str, kind: &ProviderKind) -> Option<ProviderKind> {
    match provider_name.trim() {
        "fju" => return Some(ProviderKind::FJU),
        "tku" => return Some(ProviderKind::TKU),
        _ => {}
    }
    match kind {
        ProviderKind::FJU => Some(ProviderKind::FJU),
        ProviderKind::TKU => Some(ProviderKind::TKU),
        ProviderKind::Tronclass => None,
    }
}

/// PROVIDER_KIND_NO_PANIC_V1：組出人看得懂的「不支援」錯誤訊息（各處共用同一段文字）。
pub(crate) fn unsupported_provider_message(provider_name: &str) -> String {
    format!(
        "provider `{provider_name}` 的 kind 為 `tronclass`（或未設定），目前沒有內建登入流程；\
         請在 config.toml 的 [providers.{provider_name}] 設定 kind = \"fju\" 或 kind = \"tku\"，\
         或把 provider 直接命名為 fju / tku"
    )
}

/// PROVIDER_KIND_NO_PANIC_V1：以「目前載入的全域設定」檢查某個 provider 名稱是否可用。
///
/// 供 Web／Discord 新增帳號時提前擋下：provider 不存在，或 kind 無登入流程，
/// 都回 `Err(訊息)`。全域設定尚未初始化（理論上只在單元測試）時放行，
/// 交給啟動期 `AccountsFile::resolve` 做最終把關。
pub(crate) fn check_provider_supported(provider_name: &str) -> Result<(), String> {
    let provider_name = provider_name.trim();
    let Some(config) = crate::config::try_get_config() else {
        return Ok(());
    };
    let Some(provider) = config.providers.get(provider_name) else {
        let known: Vec<&str> = config.providers.keys().map(String::as_str).collect();
        return Err(format!(
            "provider `{provider_name}` 不存在於 config.toml；可用的 provider：{}",
            if known.is_empty() { "（無）".to_string() } else { known.join("、") }
        ));
    };
    if resolve_flow_kind(provider_name, &provider.kind).is_none() {
        return Err(unsupported_provider_message(provider_name));
    }
    Ok(())
}

/// 取得帳號對應的登入流程。
///
/// PROVIDER_KIND_NO_PANIC_V1：原本 `kind = tronclass` 會 `panic!`，把一個設定錯誤
/// 升級成整個 monitor task 崩潰（且只在第一次登入時才發現）。現在回 `Err`，
/// 由呼叫端當成一般登入失敗處理；啟動期 `AccountsFile::resolve` 也會先擋下。
pub(crate) fn get_auth_flow(account: &AccountConfig) -> miette::Result<Box<dyn AuthFlow>> {
    match resolve_flow_kind(&account.provider, &account.provider_config.kind) {
        Some(ProviderKind::FJU) => Ok(fju::create_flow()),
        Some(ProviderKind::TKU) => Ok(tku::create_flow()),
        Some(ProviderKind::Tronclass) | None => Err(miette::miette!(
            "帳號 {} 無法登入：{}",
            account.id,
            unsupported_provider_message(&account.provider)
        )),
    }
}

