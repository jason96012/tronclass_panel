//! WAF 規則基線（A4，唯讀 TOML，WAF_RULES_V1；未經 cargo 編譯驗證，見 CHANGELOG）。
//!
//! 形式（使用者決定 a）：啟動時讀取一次 TOML，之後不變；改規則需重新啟動。
//! - 只能「新增」蜜罐路徑與惡意 User-Agent。內建規則永遠保留，不能被停用或覆寫。
//! - 規則在內建檢測器鏈之後執行（`WafEngine::inspect_threat` 的 `or_else`），只會增加命中。
//! - 檔案不存在：只用內建規則，行為與未實作前相同。
//! - 檔案有任何錯誤（語法、未知欄位、單筆不合規）：整份拒絕，退回內建規則（fail-safe）。
//! - `version` 用於回報與稽核，不做線上回滾；回滾方式是修正檔案後重新啟動。

use super::*;

use axum::http::{header, HeaderMap, Uri};
use std::path::Path;

/// 規則檔路徑的環境變數；未設定時使用預設路徑。
pub const WAF_RULES_ENV: &str = "WAF_RULES_PATH";
pub const WAF_RULES_DEFAULT_PATH: &str = "config/waf_rules.toml";

const MAX_ENTRIES_PER_LIST: usize = 500;
const MIN_PATTERN_LEN: usize = 3;
const MAX_PATTERN_LEN: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RulesSource {
    Builtin,
    File,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesFile {
    version: u32,
    #[serde(default)]
    honeypot_paths: Vec<String>,
    #[serde(default)]
    user_agents: Vec<String>,
}

/// 已驗證的附加規則（只增不減）。
#[derive(Debug, Clone)]
pub struct WafRules {
    pub version: u32,
    pub source: RulesSource,
    pub(super) honeypot_paths: Vec<String>,
    pub(super) user_agents: Vec<String>,
}

impl Default for WafRules {
    fn default() -> Self {
        Self::builtin()
    }
}

impl WafRules {
    /// 只有內建規則（沒有附加項目）。
    pub fn builtin() -> Self {
        Self {
            version: 0,
            source: RulesSource::Builtin,
            honeypot_paths: Vec::new(),
            user_agents: Vec::new(),
        }
    }

    /// 解析並驗證 TOML 文字。任何錯誤都回傳 Err，不接受部分結果。
    pub fn parse(text: &str) -> Result<Self, String> {
        let file: RulesFile = toml::from_str(text).map_err(|e| format!("TOML 解析失敗: {e}"))?;
        if file.version == 0 {
            return Err("version 必須 >= 1".to_string());
        }
        let honeypot_paths = normalize("honeypot_paths", &file.honeypot_paths, true)?;
        let user_agents = normalize("user_agents", &file.user_agents, false)?;
        Ok(Self {
            version: file.version,
            source: RulesSource::File,
            honeypot_paths,
            user_agents,
        })
    }

    /// 讀取檔案；缺檔或任何錯誤都退回內建規則。第二個回傳值是給 log 的說明（沒有則為 None）。
    pub fn load_or_builtin(path: &Path) -> (Self, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(text) => match Self::parse(&text) {
                Ok(rules) => (rules, None),
                Err(e) => (Self::builtin(), Some(format!("{} 無效：{e}；退回內建規則", path.display()))),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Self::builtin(), None),
            Err(e) => (
                Self::builtin(),
                Some(format!("{} 讀取失敗：{e}；退回內建規則", path.display())),
            ),
        }
    }

    pub fn source_label(&self) -> &'static str {
        match self.source {
            RulesSource::Builtin => "builtin",
            RulesSource::File => "file",
        }
    }

    /// 只新增：命中才回傳威脅，未命中回傳 None。
    pub(super) fn check(&self, uri: &Uri, headers: &HeaderMap) -> Option<WafThreat> {
        if !self.honeypot_paths.is_empty() {
            let lower_path = uri.path().to_ascii_lowercase();
            for probe in &self.honeypot_paths {
                if lower_path == *probe || lower_path.starts_with(&format!("{probe}/")) {
                    return Some(WafThreat::HoneypotProbe(uri.path().to_string()));
                }
            }
        }
        if !self.user_agents.is_empty() {
            if let Some(ua) = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()) {
                let ua_lower = ua.to_ascii_lowercase();
                for bot in &self.user_agents {
                    if ua_lower.contains(bot.as_str()) {
                        return Some(WafThreat::MaliciousUserAgent(ua.to_string()));
                    }
                }
            }
        }
        None
    }
}

/// 驗證並正規化一個清單：小寫、去頭尾空白、去重；長度與字元都必須合規。
fn normalize(field: &str, items: &[String], is_path: bool) -> Result<Vec<String>, String> {
    if items.len() > MAX_ENTRIES_PER_LIST {
        return Err(format!("{field} 超過 {MAX_ENTRIES_PER_LIST} 筆"));
    }
    let mut out: Vec<String> = Vec::new();
    for raw in items {
        let s = raw.trim().to_ascii_lowercase();
        if s.len() < MIN_PATTERN_LEN || s.len() > MAX_PATTERN_LEN {
            return Err(format!(
                "{field} 的 {raw:?} 長度須在 {MIN_PATTERN_LEN}–{MAX_PATTERN_LEN} 之間"
            ));
        }
        if is_path {
            if !s.starts_with('/') {
                return Err(format!("{field} 的 {raw:?} 必須以 / 開頭"));
            }
            if !s.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(format!("{field} 的 {raw:?} 含有不允許的字元"));
            }
        } else if !s.bytes().all(|b| b.is_ascii_graphic() || b == b' ') {
            return Err(format!("{field} 的 {raw:?} 含有不允許的字元"));
        }
        if !out.contains(&s) {
            out.push(s);
        }
    }
    Ok(out)
}

/// 依環境變數 WAF_RULES_PATH（預設 config/waf_rules.toml）載入規則。失敗只記 log，不影響啟動。
pub fn load_rules_from_env() -> WafRules {
    let path = std::env::var(WAF_RULES_ENV).unwrap_or_else(|_| WAF_RULES_DEFAULT_PATH.to_string());
    let (rules, warning) = WafRules::load_or_builtin(Path::new(&path));
    if let Some(msg) = warning {
        tracing::warn!("🛡️ [WAF] 規則檔：{msg}");
    }
    rules
}
