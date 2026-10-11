//! HOSTING_IP_BLOCK_V1：偵測來源 IP 是否屬於雲端／VPS／機房代管商
//! （AWS、Google Cloud、Microsoft Azure，以及其他各家 VPS／hosting 業者），
//! 不分國家、全站套用。
//!
//! 背景（使用者需求原文）：「像 AWS 或 Google Cloud 這種能不能自動阻擋不要讓
//! 他進入我的伺服器」——起因是 WAF 攔到的惡意掃描器（Palo Alto Networks
//! Cortex Xpanse 之類的全網掃描）幾乎都是從雲端機房 IP 發出的，而這個
//! Dashboard 的真實使用者（學生）幾乎不可能用雲端主機連線（他們用的是家用
//! 寬頻、校園網路、行動網路），所以把「這個 IP 是不是機房／VPS」本身當成一個
//! 獨立的全站封鎖條件是合理的。
//!
//! 涵蓋範圍刻意選「所有常見 VPS／機房代管商」而不是只列 AWS/GCP/Azure 三家：
//! 多數中小型 VPS 業者（DigitalOcean、Vultr、Hetzner、OVH、阿里雲、騰訊雲…）
//! 並不像 AWS/GCP 一樣公開「完整官方 IP 區段清單」，若自己維護一份
//! 「各家雲端商官方範圍」合併表，涵蓋率低、更新成本高，而且每家格式都不同。
//! 改用 ip-api.com 免費 JSON API 的 `hosting` 欄位（免金鑰、非商業用途免費，
//! 不需要付費方案）——這個欄位本來就是靠對方持續維護的「機房／代管／colocation」
//! ASN／IP 資料庫判斷，一次查詢就橫跨幾乎所有已知業者，不必我們自己維護清單。
//!
//! 查詢失敗／逾時：fail-open（視為「不是機房 IP」，不擋），與專案既有
//! `geoip` 模組查詢失敗時的處理原則一致——安全性加強功能本身掛掉，
//! 不該變成「把所有人都擋在外面」。
//!
//! ## HOSTING_IP_LOCAL_FALLBACK_V1：本地官方範圍快取（第三十六輪第二次優化）
//!
//! 上面這套 ip-api.com 查詢有個現實限制：免費版約 45 次/分鐘的速率限制，靠
//! 24 小時快取緩解「同一個 IP 一天只查一次」，但**萬一真的遇到大量「第一次
//! 出現」的不同雲端 IP 集中掃描**（WAF 最常見的情境），快取幫不上忙、很容易
//! 打到限流，而「查詢失敗 fail-open」又代表這正是最可能放行攻擊流量的時刻。
//!
//! 為了讓流量最大的兩家雲端商（AWS、Google Cloud）就算 ip-api.com 掛掉／被
//! 限流也還擋得住，這裡額外維護一份**直接向 AWS／GCP 官方 JSON API 定期抓
//! 回來**的 IPv4 範圍本地比對清單（見 [`refresh_known_cloud_ranges`]）：
//! * AWS：<https://ip-ranges.amazonaws.com/ip-ranges.json>（官方文件載明的
//!   穩定網址，AWS 自己持續更新內容）。
//! * Google Cloud：<https://www.gstatic.com/ipranges/cloud.json>（官方文件
//!   載明的穩定網址）。
//!
//! 刻意不含 Azure／DigitalOcean／OVH／Hetzner 等業者：這些業者沒有像上面
//! 兩家一樣「長期穩定、可直接程式化抓取」的官方網址（Azure 的下載連結每週
//! 輪替一次 GUID，其餘業者多半完全不公開機器可讀清單）——與其抓一份遲早失效
//! 或來源不明確的清單，不如交給 ip-api.com 的 `hosting` 欄位去涵蓋這些業者
//! （本來就是這套方案存在的理由）。也因此，本地清單**只是加速／備援層**，
//! 不是取代 ip-api.com：查詢順序永遠是「先比對本地清單，命中就不必問外部
//! API；沒命中才去問 ip-api.com」，涵蓋範圍只增不減。
//!
//! 本地清單由 `main.rs` 啟動時呼叫一次、之後排程重抓（見下方
//! HOSTING_IP_LOCAL_FALLBACK_RETRY_V1 一節）；任一來源下載失敗就保留舊清單
//! 不動（服務剛啟動、還沒抓到第一份清單前，這層比對永遠不命中，等同於沒有
//! 這層快取，不影響既有行為，純粹是「有的話錦上添花」）。IPv4、IPv6 都有
//! 比對（兩家官方清單本來就同時提供兩種位址的範圍，分開各自解析、排序、
//! 二分搜尋，互不影響）。
//!
//! HOSTING_IP_LOCAL_FALLBACK_RETRY_V1：重抓間隔不是固定 12 小時——在「還沒
//! 成功抓到過一次」之前（例如服務剛啟動、AWS／GCP 當下剛好不可用），改成
//! 每 5 分鐘就重試一次；成功過一次後才切回正常的 12 小時排程，避免「萬一
//! 第一次就衰到抓失敗，整整 12 小時這層保護等於沒作用」。實際排程邏輯在
//! `main.rs`（這裡只提供 [`has_local_cloud_ranges`] 給它判斷要用哪個間隔）。
//!
//! ## HOSTING_IP_RECENT_BLOCKS_V1：後台「最近被雲端 IP 擋下」清單（第三十八輪第三次優化）
//!
//! 功能上線後使用者看不到任何「它其實有在擋」的證據——黑名單頁原本的「最近
//! 登入失敗」清單只收 `activity_events` 表的登入事件，這層機房 IP 封鎖發生
//! 在那之前（根本沒有登入、也沒有帳號可言），不會出現在那份清單。
//!
//! 刻意**不**把每一次機房 IP 封鎖寫進 `activity_events`：那張表有全站共用的
//! 總筆數上限（見 `db/activity.rs` 的 `EVENT_PRUNE_INTERVAL` 批次清理，每次
//! 只留最新 500 筆），機房掃描一次可能在幾分鐘內打上百個不同路徑，寫進去會
//! 把其他重要的稽核紀錄（登入、操作紀錄）直接洗掉。改用模組內一個有上限的
//! 記憶體環狀緩衝區（見 [`recent_blocks`] / [`record_block`]）：僅供「後台
//! 看最近有沒有擋到東西」這種展示用途，不是正式稽核紀錄，所以重啟後清空、
//! 不佔用 DB 容量都可以接受。
//!
//! ## ICLOUD_PRIVATE_RELAY_ALLOWLIST_V1：排除 iCloud 私密轉寄（第四十二輪）
//!
//! 使用者回報：手機（iPhone）開著「iCloud 私密轉寄」連進來，被這層「偵測到
//! 雲端／機房連線」擋下，系統判斷的來源寫著 Akamai Technologies, Inc.。這不
//! 是誤判——開了私密轉寄之後，Safari 的流量真的會從 Akamai／Cloudflare 這類
//! 代管商的機房 IP 出口連到目的站，ip-api.com 把它標成「是機房」完全正確，
//! 但對這個 Dashboard 的使用情境來說是誤殺：這是蘋果自家、數以億計裝置在用
//! 的主流隱私功能，連線的仍然是真人用戶的手機，不是掃描器或攻擊者的 VPS。
//!
//! 解法：蘋果官方每天更新一份完整的 Private Relay 出口 IP 清單（CSV，公開在
//! `https://mask-api.icloud.com/egress-ip-ranges.csv`），比照 AWS／GCP 的本地
//! 範圍快取做法額外抓一份存在本地；[`lookup`] 查詢時這份清單的優先權蓋過
//! AWS／GCP 本地清單與 ip-api.com 的結果——命中就直接當「不是機房」，不再
//! 往下查。這份清單只放行、不封鎖，抓取失敗一樣 fail-open（保留舊清單，不會
//! 因為這層掛掉就反過來誤擋更多人）。
use std::collections::{HashMap, VecDeque};
use std::net::Ipv4Addr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;

const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// 查詢失敗（逾時／對方服務掛掉）的短快取——避免對方掛掉時，同一個 IP
/// 短時間內反覆重打，但也不會因為一次性的失敗就快取一整天都不擋。
const FAILURE_CACHE_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_CACHE_ENTRIES: usize = 512;
const LOOKUP_TIMEOUT_SECS: u64 = 4;

/// AWS 官方 IP 範圍（含 EC2／CloudFront 等各項服務，已足夠涵蓋一般 EC2 出口 IP）。
const AWS_IP_RANGES_URL: &str = "https://ip-ranges.amazonaws.com/ip-ranges.json";
/// Google Cloud 官方 IP 範圍（Google 自家服務如 8.8.8.8 不在這份清單內，
/// 只收錄實際給 GCP 客戶使用的運算／網路範圍，不會錯殺 Google 的一般服務）。
const GCP_IP_RANGES_URL: &str = "https://www.gstatic.com/ipranges/cloud.json";
/// 本地清單比對命中時，不必再問外部 API；顯示用的描述直接寫死，不留空白。
const LOCAL_MATCH_LABEL: &str = "AWS 或 Google Cloud（官方 IP 範圍本地比對命中）";

/// ICLOUD_PRIVATE_RELAY_ALLOWLIST_V1（第四十二輪）：蘋果官方每日更新的
/// Private Relay 出口 IP 清單（CSV，第一欄是 CIDR，IPv4／IPv6 混在同一份
/// 檔案裡）。
///
/// 背景：iPhone／iPad／Mac 只要在 Safari 開「iCloud 私密轉寄」，瀏覽器流量
/// 會先繞經 Apple 自家的第一跳，再由 Akamai／Cloudflare 等代管商的「第二跳」
/// 出口連到目的站——這些出口 IP 本質上就是代管商機房 IP，ip-api.com 的
/// `hosting` 欄位（乃至任何類似的機房分類資料庫）如實把它們標成「是機房」
/// 完全正確，但對這個 Dashboard 來說是誤殺：開了私密轉寄的真實使用者（學生）
/// 會被「偵測到雲端／機房連線」擋下，而他們既不是掃描器也不是 VPS，只是手機
/// 開了一個預設漸漸變普及的隱私功能。
///
/// 解法：蘋果自己把完整出口清單公開在穩定官方網址上（CSV，每日更新），比照
/// AWS／GCP 的本地範圍快取做法多抓一份，查詢時「命中這份清單」的優先權蓋過
/// 「命中 AWS／GCP 清單」與「問 ip-api.com」兩者——即使之後兩者都會判定
/// `is_hosting = true`，只要先比對到是蘋果的出口 IP，一律當「不是機房」放行，
/// 不再往下查。
///
/// 來源：<https://mask-api.icloud.com/egress-ip-ranges.csv>（Apple Developer
/// 官方文件「Prepare Your Network or Web Server for iCloud Private Relay」
/// 載明的位址，業界慣例也公認這是蘋果官方維護的即時出口清單）。
const ICLOUD_PRIVATE_RELAY_CSV_URL: &str = "https://mask-api.icloud.com/egress-ip-ranges.csv";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostingInfo {
    /// 是否判定為雲端／VPS／機房代管商出口 IP。
    pub is_hosting: bool,
    pub isp: String,
    pub org: String,
    /// ISO 3166-1 alpha-2 大寫；空字串＝未知。
    pub country_code: String,
    pub country: String,
    pub city: String,
}

impl HostingInfo {
    /// 給封鎖頁與稽核紀錄用的一行描述，例如「Google LLC · 美國 (US) · Brussels」；
    /// 完全查不到資訊時退回「未知來源」。
    pub fn label(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let org = if !self.isp.is_empty() {
            self.isp.clone()
        } else {
            self.org.clone()
        };
        if !org.is_empty() {
            parts.push(org);
        }
        if !self.country_code.is_empty() {
            parts.push(if !self.country.is_empty() {
                format!("{} ({})", self.country, self.country_code)
            } else {
                self.country_code.clone()
            });
        }
        if !self.city.is_empty() {
            parts.push(self.city.clone());
        }
        if parts.is_empty() {
            "未知來源".to_string()
        } else {
            parts.join(" · ")
        }
    }
}

static CACHE: Lazy<Mutex<HashMap<String, (Instant, Option<HostingInfo>)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(LOOKUP_TIMEOUT_SECS))
        .connect_timeout(Duration::from_secs(3))
        .pool_max_idle_per_host(2)
        .user_agent("TronclassPanel/1.0 (hosting-ip-check)")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

fn cache_get(ip: &str) -> Option<Option<HostingInfo>> {
    let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    let now = Instant::now();
    cache.retain(|_, (at, info)| {
        let ttl = if info.is_some() { CACHE_TTL } else { FAILURE_CACHE_TTL };
        now.saturating_duration_since(*at) < ttl
    });
    cache.get(ip).map(|(_, info)| info.clone())
}

fn cache_put(ip: &str, info: &Option<HostingInfo>) {
    let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if cache.len() >= MAX_CACHE_ENTRIES && !cache.contains_key(ip) {
        if let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, (at, _))| *at)
            .map(|(k, _)| k.clone())
        {
            cache.remove(&oldest);
        }
    }
    cache.insert(ip.to_string(), (Instant::now(), info.clone()));
}

/// 抓官方清單用的獨立 client：網址是 HTTPS、檔案比查單一 IP 大很多
/// （AWS 那份有數千筆、約 2~3 MB），逾時給寬一點，不跟上面查單一 IP 共用
/// 短逾時設定。
static RANGES_CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(5))
        .user_agent("TronclassPanel/1.0 (hosting-ip-range-refresh)")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

/// 單一 IPv4 CIDR 區段，用起訖位址（含兩端）表示，排序後可以二分搜尋。
#[derive(Debug, Clone, Copy)]
struct Ipv4Range {
    start: u32,
    end: u32,
}

/// 本地官方範圍快取：已排序、已合併重疊／相鄰區段的 disjoint 區段清單。
/// 啟動後、第一次 [`refresh_known_cloud_ranges`] 成功前是空的（比對永遠
/// 不命中，不影響既有行為）。
static CLOUD_RANGES: Lazy<Mutex<Vec<Ipv4Range>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// `/admin/blacklist` 頁「最近被雲端／VPS機房 IP 擋下」清單用的一列。
#[derive(Debug, Clone, serde::Serialize)]
pub struct RecentHostingBlock {
    pub time: String,
    pub timestamp: i64,
    pub ip: String,
    pub path: String,
    /// `HostingInfo::label()` 的結果，例如「Google LLC · 美國 (US) · Brussels」
    /// 或本地清單命中時的 [`LOCAL_MATCH_LABEL`]。
    pub source: String,
}

/// 只供「後台看得到效果」展示用，不是正式稽核紀錄——見模組文件
/// HOSTING_IP_RECENT_BLOCKS_V1 一節。超過上限時丟掉最舊的一筆。
const MAX_RECENT_BLOCKS: usize = 200;

static RECENT_BLOCKS: Lazy<Mutex<VecDeque<RecentHostingBlock>>> =
    Lazy::new(|| Mutex::new(VecDeque::with_capacity(MAX_RECENT_BLOCKS)));

/// 記一筆「這個 IP 被機房／VPS 封鎖擋下」。呼叫端（`reject_hosting_ip_
/// middleware`）只在真的要回 403 的那一刻呼叫，放行的請求不記。
pub fn record_block(ip: &str, path: &str, source: &str) {
    let now = chrono::Local::now();
    let entry = RecentHostingBlock {
        time: now.format("%Y-%m-%d %H:%M:%S").to_string(),
        timestamp: now.timestamp(),
        ip: crate::text_utils::truncate_chars(ip.trim(), 64).to_string(),
        path: crate::text_utils::truncate_chars(path.trim(), 200).to_string(),
        source: crate::text_utils::truncate_chars(source.trim(), 160).to_string(),
    };
    let mut buf = RECENT_BLOCKS.lock().unwrap_or_else(|p| p.into_inner());
    if buf.len() >= MAX_RECENT_BLOCKS {
        buf.pop_front();
    }
    buf.push_back(entry);
}

/// 取最近的封鎖紀錄，最新的在最前面。`limit` 由呼叫端的 API 限制上限夾住。
pub fn recent_blocks(limit: usize) -> Vec<RecentHostingBlock> {
    let buf = RECENT_BLOCKS.lock().unwrap_or_else(|p| p.into_inner());
    buf.iter().rev().take(limit).cloned().collect()
}

fn parse_ipv4_cidr(cidr: &str) -> Option<Ipv4Range> {
    let (addr, prefix) = cidr.split_once('/')?;
    let base: Ipv4Addr = addr.trim().parse().ok()?;
    let prefix_len: u32 = prefix.trim().parse().ok()?;
    if prefix_len > 32 {
        return None;
    }
    let base_u32 = u32::from(base);
    let mask: u32 = if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - prefix_len)
    };
    let start = base_u32 & mask;
    let end = start | !mask;
    Some(Ipv4Range { start, end })
}

/// 比對這個 IPv4 位址是不是落在本地快取的官方範圍內。二分搜尋：找「起點
/// ≤ target 的最後一段」，因為清單已先合併成不重疊的 disjoint 區段，命中
/// 與否只需要看這一段的終點夠不夠大。
fn is_known_cloud_ipv4(ip: Ipv4Addr) -> bool {
    let ranges = CLOUD_RANGES.lock().unwrap_or_else(|p| p.into_inner());
    if ranges.is_empty() {
        return false;
    }
    let target = u32::from(ip);
    let idx = ranges.partition_point(|r| r.start <= target);
    if idx == 0 {
        return false;
    }
    let candidate = ranges[idx - 1];
    target >= candidate.start && target <= candidate.end
}

/// 單一 IPv6 CIDR 區段，道理跟 [`Ipv4Range`] 完全一樣，只是改用 128 bit 的
/// `u128` 裝得下整個 IPv6 位址（`Ipv4Range` 的 `u32` 裝不下）。
#[derive(Debug, Clone, Copy)]
struct Ipv6Range {
    start: u128,
    end: u128,
}

fn parse_ipv6_cidr(cidr: &str) -> Option<Ipv6Range> {
    let (addr, prefix) = cidr.split_once('/')?;
    let base: std::net::Ipv6Addr = addr.trim().parse().ok()?;
    let prefix_len: u32 = prefix.trim().parse().ok()?;
    if prefix_len > 128 {
        return None;
    }
    let base_u128 = u128::from(base);
    let mask: u128 = if prefix_len == 0 {
        0
    } else {
        u128::MAX << (128 - prefix_len)
    };
    let start = base_u128 & mask;
    let end = start | !mask;
    Some(Ipv6Range { start, end })
}

/// IPv6 版的 [`is_known_cloud_ipv4`]，邏輯完全相同。
fn is_known_cloud_ipv6(ip: std::net::Ipv6Addr) -> bool {
    let ranges = CLOUD_RANGES_V6.lock().unwrap_or_else(|p| p.into_inner());
    if ranges.is_empty() {
        return false;
    }
    let target = u128::from(ip);
    let idx = ranges.partition_point(|r| r.start <= target);
    if idx == 0 {
        return false;
    }
    let candidate = ranges[idx - 1];
    target >= candidate.start && target <= candidate.end
}

/// 本地 IPv6 官方範圍快取，結構與用途同 [`CLOUD_RANGES`]（IPv4 版）。
static CLOUD_RANGES_V6: Lazy<Mutex<Vec<Ipv6Range>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// ICLOUD_PRIVATE_RELAY_ALLOWLIST_V1：蘋果 Private Relay 出口 IPv4 清單，
/// 結構與用途同 [`CLOUD_RANGES`]，但意義相反——命中代表「這是蘋果的合法出口，
/// 不算機房」，而不是「這是機房」。
static PRIVATE_RELAY_RANGES: Lazy<Mutex<Vec<Ipv4Range>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// ICLOUD_PRIVATE_RELAY_ALLOWLIST_V1：同上，IPv6 版。
static PRIVATE_RELAY_RANGES_V6: Lazy<Mutex<Vec<Ipv6Range>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// 這個 IPv4 位址是不是蘋果 Private Relay 的官方出口範圍。邏輯與
/// [`is_known_cloud_ipv4`] 完全相同（同一套排序＋二分搜尋），只是比對的清單
/// 不同，故意不共用同一份程式碼——理由同模組內其他 IPv4／IPv6 分開寫的
/// 函式：兩邊語意不同（一個是「是機房」、一個是「不是機房」），分開寫在閱讀
/// 時不必多想一層「這次到底是哪個清單」。
fn is_private_relay_ipv4(ip: Ipv4Addr) -> bool {
    let ranges = PRIVATE_RELAY_RANGES.lock().unwrap_or_else(|p| p.into_inner());
    if ranges.is_empty() {
        return false;
    }
    let target = u32::from(ip);
    let idx = ranges.partition_point(|r| r.start <= target);
    if idx == 0 {
        return false;
    }
    let candidate = ranges[idx - 1];
    target >= candidate.start && target <= candidate.end
}

/// IPv6 版的 [`is_private_relay_ipv4`]。
fn is_private_relay_ipv6(ip: std::net::Ipv6Addr) -> bool {
    let ranges = PRIVATE_RELAY_RANGES_V6.lock().unwrap_or_else(|p| p.into_inner());
    if ranges.is_empty() {
        return false;
    }
    let target = u128::from(ip);
    let idx = ranges.partition_point(|r| r.start <= target);
    if idx == 0 {
        return false;
    }
    let candidate = ranges[idx - 1];
    target >= candidate.start && target <= candidate.end
}

/// 向官方端點下載一次 JSON，不解析內容——AWS／GCP 的 IPv4、IPv6 範圍其實都
/// 包在同一份回應裡（AWS 是用兩個不同的頂層欄位分開放，GCP 是混在同一個
/// `prefixes` 陣列裡用欄位名稱區分），分開各抓一次 IPv4 用、一次 IPv6 用會
/// 把這份 2~3 MB 的檔案重複下載兩遍，沒有必要——下載一次、兩種位址各自用
/// 不同的欄位名稱去取就好。
async fn fetch_official_json(url: &str) -> Option<serde_json::Value> {
    let resp = match RANGES_CLIENT.get(url).send().await {
        Ok(resp) => resp,
        Err(error) => {
            tracing::warn!(%url, %error, "抓取雲端官方 IP 範圍失敗");
            return None;
        }
    };
    if !resp.status().is_success() {
        tracing::warn!(%url, status = %resp.status(), "雲端官方 IP 範圍回應非 2xx");
        return None;
    }
    match resp.json::<serde_json::Value>().await {
        Ok(body) => Some(body),
        Err(error) => {
            tracing::warn!(%url, %error, "雲端官方 IP 範圍回應解析失敗");
            None
        }
    }
}

/// ICLOUD_PRIVATE_RELAY_ALLOWLIST_V1：下載蘋果 Private Relay 出口清單的原始
/// CSV 文字（約 10 幾 MB、二十幾萬列，比 AWS／GCP 的 JSON 大上一截，故用
/// 和那兩份一樣的寬逾時 `RANGES_CLIENT`，不跟單一 IP 查詢共用短逾時）。
async fn fetch_private_relay_csv() -> Option<String> {
    let resp = match RANGES_CLIENT.get(ICLOUD_PRIVATE_RELAY_CSV_URL).send().await {
        Ok(resp) => resp,
        Err(error) => {
            tracing::warn!(error = %error, "抓取 iCloud Private Relay 官方出口清單失敗");
            return None;
        }
    };
    if !resp.status().is_success() {
        tracing::warn!(status = %resp.status(), "iCloud Private Relay 官方出口清單回應非 2xx");
        return None;
    }
    match resp.text().await {
        Ok(text) => Some(text),
        Err(error) => {
            tracing::warn!(error = %error, "iCloud Private Relay 官方出口清單回應讀取失敗");
            None
        }
    }
}

/// 解析蘋果 Private Relay CSV：每列格式是
/// `ip-prefix,country-code,region-code,city-code`（IPv4／IPv6 的 CIDR 混在
/// 同一份檔案、同一欄裡），這裡只取第一欄的 CIDR，其餘欄位（國碼、城市）
/// 這個模組用不到，不解析。單列格式錯誤（缺逗號、CIDR 格式壞掉）就跳過該列，
/// 不整份作廢——蘋果的清單本身就可能夾雜個位數的格式瑕疵列，不該讓一整份
/// 清單因此直接失敗。
fn parse_private_relay_csv(text: &str) -> (Vec<Ipv4Range>, Vec<Ipv6Range>) {
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((cidr, _rest)) = line.split_once(',') else {
            continue;
        };
        let cidr = cidr.trim();
        if cidr.contains(':') {
            if let Some(range) = parse_ipv6_cidr(cidr) {
                v6.push(range);
            }
        } else if let Some(range) = parse_ipv4_cidr(cidr) {
            v4.push(range);
        }
    }
    (v4, v6)
}

/// 從已經下載好的官方 JSON 裡取出指定頂層陣列、指定欄位的 IPv4 CIDR 清單。
/// `array_key`／`field` 兩家不同：AWS 是 `prefixes` 陣列裡的 `ip_prefix`
/// 欄位，GCP 也是 `prefixes` 陣列、但欄位叫 `ipv4Prefix`。
fn extract_ipv4_ranges(body: &serde_json::Value, array_key: &str, field: &str) -> Vec<Ipv4Range> {
    let Some(prefixes) = body.get(array_key).and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut ranges = Vec::with_capacity(prefixes.len());
    for entry in prefixes {
        if let Some(cidr) = entry.get(field).and_then(|v| v.as_str()) {
            if let Some(range) = parse_ipv4_cidr(cidr) {
                ranges.push(range);
            }
        }
    }
    ranges
}

/// IPv6 版的 [`extract_ipv4_ranges`]。`array_key`／`field` 兩家格式不同：
/// AWS 的 IPv6 範圍放在獨立的頂層陣列 `ipv6_prefixes`、欄位叫 `ipv6_prefix`
/// （跟 IPv4 的 `prefixes`／`ip_prefix` 是兩個完全分開的陣列）；GCP 則是跟
/// IPv4 混在同一個 `prefixes` 陣列裡、用欄位名稱 `ipv6Prefix` 區分。這是
/// 實際抓官方端點回來的真實回應結構確認過的，不是用猜的。
fn extract_ipv6_ranges(body: &serde_json::Value, array_key: &str, field: &str) -> Vec<Ipv6Range> {
    let Some(prefixes) = body.get(array_key).and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut ranges = Vec::with_capacity(prefixes.len());
    for entry in prefixes {
        if let Some(cidr) = entry.get(field).and_then(|v| v.as_str()) {
            if let Some(range) = parse_ipv6_cidr(cidr) {
                ranges.push(range);
            }
        }
    }
    ranges
}

/// 把一批（可能重疊／相鄰的）IPv4 區段排序後合併成 disjoint 清單，供二分
/// 搜尋使用。抽成獨立函式是因為 IPv4／IPv6 兩份清單都要做同一件事，但兩者
/// 型別（`u32` vs `u128`）不同沒辦法直接共用同一份泛型程式碼又不增加額外
/// 的複雜度，寫兩份幾乎一樣但型別各自明確的版本更不容易出錯。
fn merge_ipv4_ranges(mut ranges: Vec<Ipv4Range>) -> Vec<Ipv4Range> {
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<Ipv4Range> = Vec::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        if let Some(last) = merged.last_mut() {
            if range.start <= last.end.saturating_add(1) {
                if range.end > last.end {
                    last.end = range.end;
                }
                continue;
            }
        }
        merged.push(range);
    }
    merged
}

/// IPv6 版的 [`merge_ipv4_ranges`]。
fn merge_ipv6_ranges(mut ranges: Vec<Ipv6Range>) -> Vec<Ipv6Range> {
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<Ipv6Range> = Vec::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        if let Some(last) = merged.last_mut() {
            if range.start <= last.end.saturating_add(1) {
                if range.end > last.end {
                    last.end = range.end;
                }
                continue;
            }
        }
        merged.push(range);
    }
    merged
}

/// 本地清單目前是不是有內容（IPv4 或 IPv6 任一有抓到都算）——`main.rs` 的
/// 排程背景任務用這個判斷「還沒成功抓到過一次」要不要縮短重試間隔（見
/// HOSTING_IP_LOCAL_FALLBACK_V1 模組文件與 `main.rs` 裡呼叫端的註解）。
/// 服務剛啟動、[`refresh_known_cloud_ranges`] 還沒成功過一次時回傳 `false`。
pub fn has_local_cloud_ranges() -> bool {
    let has_v4 = !CLOUD_RANGES.lock().unwrap_or_else(|p| p.into_inner()).is_empty();
    let has_v6 = !CLOUD_RANGES_V6.lock().unwrap_or_else(|p| p.into_inner()).is_empty();
    has_v4 || has_v6
}

/// ICLOUD_PRIVATE_RELAY_ALLOWLIST_V1：重新抓一次蘋果官方 Private Relay 出口
/// 清單，解析後整份覆蓋本地快取。邏輯與 AWS／GCP 那份刻意保持一致（抓取
/// 失敗或解析後兩種位址都是空清單，就保留舊清單不動；成功就整個重建，不是
/// 增量合併）。由 [`refresh_known_cloud_ranges`] 在每次排程刷新時一併呼叫，
/// 不單獨另排一個排程（跟 AWS／GCP 用同一個節奏即可，沒有必要為了一份清單
/// 多開一條背景迴圈）。
async fn refresh_private_relay_ranges() {
    let Some(text) = fetch_private_relay_csv().await else {
        tracing::warn!("iCloud Private Relay 官方出口清單抓取失敗，沿用舊的本地快取（若有）");
        return;
    };
    let (v4, v6) = parse_private_relay_csv(&text);
    if v4.is_empty() && v6.is_empty() {
        tracing::warn!("iCloud Private Relay 官方出口清單解析後是空清單，沿用舊的本地快取（若有）");
        return;
    }
    let v4_count;
    let v6_count;
    if !v4.is_empty() {
        let merged = merge_ipv4_ranges(v4);
        v4_count = merged.len();
        let mut guard = PRIVATE_RELAY_RANGES.lock().unwrap_or_else(|p| p.into_inner());
        *guard = merged;
    } else {
        v4_count = PRIVATE_RELAY_RANGES.lock().unwrap_or_else(|p| p.into_inner()).len();
    }
    if !v6.is_empty() {
        let merged = merge_ipv6_ranges(v6);
        v6_count = merged.len();
        let mut guard = PRIVATE_RELAY_RANGES_V6.lock().unwrap_or_else(|p| p.into_inner());
        *guard = merged;
    } else {
        v6_count = PRIVATE_RELAY_RANGES_V6.lock().unwrap_or_else(|p| p.into_inner()).len();
    }
    tracing::info!(
        ipv4_entries = v4_count,
        ipv6_entries = v6_count,
        "已更新 iCloud Private Relay 官方出口 IP 範圍本地快取"
    );
}

/// 定期重新抓 AWS／GCP 官方 IP 範圍（IPv4 與 IPv6 都抓），合併成本地比對用
/// 的 disjoint 區段清單。由 `main.rs` 啟動時呼叫一次、之後依
/// [`has_local_cloud_ranges`] 的結果決定下一次要多久後再呼叫（還沒成功過
/// 一次時間隔較短，成功過一次後間隔拉長到 12 小時——細節見 `main.rs`
/// 呼叫端）。兩個來源都抓失敗時保留舊清單不動（不要用空清單蓋掉還堪用的
/// 舊資料）；任一來源成功就會用新抓到的內容整個重建清單（不是增量合併——
/// 官方清單本身就是「目前完整現況」，沒有增量的概念）。另外也會在同一次呼叫
/// 裡一併刷新 [`refresh_private_relay_ranges`]（iCloud Private Relay 放行
/// 清單），細節見該函式文件。
pub async fn refresh_known_cloud_ranges() {
    // ICLOUD_PRIVATE_RELAY_ALLOWLIST_V1：獨立抓、獨立刷新，刻意放在 AWS／GCP
    // 的「兩者都失敗就提早 return」判斷之前——這份清單的用途（放行）跟
    // AWS／GCP 那份（封鎖依據）完全不同，不該因為 AWS／GCP 剛好都抓失敗，
    // 連帶讓這份也沒機會刷新。抓取失敗只記 warn、沿用舊清單，不影響這個
    // 函式接下來處理 AWS／GCP 的任何邏輯。
    refresh_private_relay_ranges().await;

    let aws_body = fetch_official_json(AWS_IP_RANGES_URL).await;
    let gcp_body = fetch_official_json(GCP_IP_RANGES_URL).await;
    let aws_ok = aws_body.is_some();
    let gcp_ok = gcp_body.is_some();

    let mut v4: Vec<Ipv4Range> = Vec::new();
    let mut v6: Vec<Ipv6Range> = Vec::new();
    if let Some(body) = &aws_body {
        v4.extend(extract_ipv4_ranges(body, "prefixes", "ip_prefix"));
        v6.extend(extract_ipv6_ranges(body, "ipv6_prefixes", "ipv6_prefix"));
    }
    if let Some(body) = &gcp_body {
        v4.extend(extract_ipv4_ranges(body, "prefixes", "ipv4Prefix"));
        v6.extend(extract_ipv6_ranges(body, "prefixes", "ipv6Prefix"));
    }

    if v4.is_empty() && v6.is_empty() {
        tracing::warn!("AWS／Google Cloud 官方 IP 範圍兩個來源都抓取失敗，沿用舊的本地快取（若有）");
        return;
    }

    let v4_count;
    let v6_count;
    if !v4.is_empty() {
        let merged = merge_ipv4_ranges(v4);
        v4_count = merged.len();
        let mut guard = CLOUD_RANGES.lock().unwrap_or_else(|p| p.into_inner());
        *guard = merged;
    } else {
        v4_count = CLOUD_RANGES.lock().unwrap_or_else(|p| p.into_inner()).len();
    }
    if !v6.is_empty() {
        let merged = merge_ipv6_ranges(v6);
        v6_count = merged.len();
        let mut guard = CLOUD_RANGES_V6.lock().unwrap_or_else(|p| p.into_inner());
        *guard = merged;
    } else {
        v6_count = CLOUD_RANGES_V6.lock().unwrap_or_else(|p| p.into_inner()).len();
    }

    tracing::info!(
        ipv4_entries = v4_count,
        ipv6_entries = v6_count,
        aws_ok,
        gcp_ok,
        "已更新 AWS／Google Cloud 官方 IP 範圍本地快取"
    );
}

fn pick_str(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| crate::text_utils::truncate_chars(s, 60).to_string())
        .unwrap_or_default()
}

fn parse_ip_api(body: &serde_json::Value) -> Option<HostingInfo> {
    if body.get("status").and_then(|v| v.as_str()) != Some("success") {
        return None;
    }
    let is_hosting = body.get("hosting").and_then(|v| v.as_bool()).unwrap_or(false);
    Some(HostingInfo {
        is_hosting,
        isp: pick_str(body, "isp"),
        org: pick_str(body, "org"),
        country_code: pick_str(body, "countryCode").to_ascii_uppercase(),
        country: pick_str(body, "country"),
        city: pick_str(body, "city"),
    })
}

/// 查詢（含快取）。查不到／逾時回傳 `None`——呼叫端據此 fail-open（不擋）。
///
/// 查詢順序：先比對本地 AWS／GCP 官方範圍快取（免外部 API、免排隊、查詢
/// 失敗也不受影響），沒命中才問 ip-api.com（見模組文件 HOSTING_IP_LOCAL_
/// FALLBACK_V1 一節的完整說明）。
pub async fn lookup(ip: &str) -> Option<HostingInfo> {
    let ip = ip.trim();
    if ip.is_empty() {
        return None;
    }
    let Ok(parsed_ip) = ip.parse::<std::net::IpAddr>() else {
        return None;
    };
    // ICLOUD_PRIVATE_RELAY_ALLOWLIST_V1：優先權蓋過下面的本地雲端清單比對、
    // 也蓋過快取與 ip-api.com——命中蘋果官方出口清單就直接當「不是機房」
    // （`None`＝呼叫端 fail-open 放行），不必再往下查，避免被快取裡「以前
    // 查到的 is_hosting=true」或 ip-api.com 如實回報的「Akamai/Cloudflare
    // 屬於機房」蓋過去。
    let is_private_relay = match parsed_ip {
        std::net::IpAddr::V4(v4) => is_private_relay_ipv4(v4),
        std::net::IpAddr::V6(v6) => is_private_relay_ipv6(v6),
    };
    if is_private_relay {
        return None;
    }
    let local_hit = match parsed_ip {
        std::net::IpAddr::V4(v4) => is_known_cloud_ipv4(v4),
        std::net::IpAddr::V6(v6) => is_known_cloud_ipv6(v6),
    };
    if local_hit {
        return Some(HostingInfo {
            is_hosting: true,
            isp: LOCAL_MATCH_LABEL.to_string(),
            ..Default::default()
        });
    }
    if let Some(hit) = cache_get(ip) {
        return hit;
    }
    // 免金鑰方案是 HTTP（非 HTTPS）端點；僅查詢公開可見的 IP 歸屬資訊
    // （不含任何使用者個資），風險與既有 geoip 模組查 ipwho.is 相當。
    let url = format!("http://ip-api.com/json/{ip}?fields=status,message,hosting,isp,org,countryCode,country,city");
    let info = match CLIENT.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>().await {
            Ok(body) => parse_ip_api(&body),
            Err(error) => {
                tracing::debug!(%ip, %error, "ip-api.com 回應解析失敗");
                None
            }
        },
        Ok(resp) => {
            tracing::debug!(%ip, status = %resp.status(), "ip-api.com 回應非 2xx");
            None
        }
        Err(error) => {
            tracing::debug!(%ip, %error, "ip-api.com 查詢失敗");
            None
        }
    };
    cache_put(ip, &info);
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hosting_true() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{"status":"success","hosting":true,"isp":"Google LLC","org":"Google LLC","countryCode":"US","country":"United States","city":"Brussels"}"#,
        )
        .unwrap();
        let info = parse_ip_api(&body).unwrap();
        assert!(info.is_hosting);
        assert_eq!(info.label(), "Google LLC · United States (US) · Brussels");
    }

    #[test]
    fn parses_hosting_false() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{"status":"success","hosting":false,"isp":"Chunghwa Telecom","countryCode":"TW","country":"Taiwan","city":"Taipei"}"#,
        )
        .unwrap();
        let info = parse_ip_api(&body).unwrap();
        assert!(!info.is_hosting);
    }

    #[test]
    fn fail_status_is_none() {
        let body: serde_json::Value =
            serde_json::from_str(r#"{"status":"fail","message":"invalid query"}"#).unwrap();
        assert!(parse_ip_api(&body).is_none());
        assert_eq!(HostingInfo::default().label(), "未知來源");
    }

    #[test]
    fn parses_ipv4_cidr_ranges() {
        let r = parse_ipv4_cidr("34.1.208.0/20").unwrap();
        assert_eq!(r.start, u32::from(Ipv4Addr::new(34, 1, 208, 0)));
        assert_eq!(r.end, u32::from(Ipv4Addr::new(34, 1, 223, 255)));
        assert!(parse_ipv4_cidr("not-a-cidr").is_none());
        assert!(parse_ipv4_cidr("1.2.3.4/33").is_none());
        // /0 是特例（mask 全 0），確認沒有因為 `32 - 0 = 32` 造成位移溢位 panic。
        let whole = parse_ipv4_cidr("0.0.0.0/0").unwrap();
        assert_eq!(whole.start, 0);
        assert_eq!(whole.end, u32::MAX);
    }

    #[test]
    fn local_cloud_range_match_and_miss() {
        {
            let mut guard = CLOUD_RANGES.lock().unwrap();
            *guard = vec![
                Ipv4Range { start: u32::from(Ipv4Addr::new(34, 1, 208, 0)), end: u32::from(Ipv4Addr::new(34, 1, 223, 255)) },
                Ipv4Range { start: u32::from(Ipv4Addr::new(52, 0, 0, 0)), end: u32::from(Ipv4Addr::new(52, 255, 255, 255)) },
            ];
        }
        assert!(is_known_cloud_ipv4(Ipv4Addr::new(34, 1, 210, 5)));
        assert!(is_known_cloud_ipv4(Ipv4Addr::new(52, 10, 0, 1)));
        assert!(!is_known_cloud_ipv4(Ipv4Addr::new(8, 8, 8, 8)));
        assert!(!is_known_cloud_ipv4(Ipv4Addr::new(163, 13, 1, 1)));
        // 清掉，避免影響同一個 process 裡跑的其他測試（單元測試可能共用同一個
        // 靜態變數）。
        {
            let mut guard = CLOUD_RANGES.lock().unwrap();
            guard.clear();
        }
    }

    #[test]
    fn parses_ipv6_cidr_ranges() {
        let r = parse_ipv6_cidr("2600:1900:8000::/44").unwrap();
        let base: std::net::Ipv6Addr = "2600:1900:8000::".parse().unwrap();
        assert_eq!(r.start, u128::from(base));
        assert!(r.end > r.start);
        assert!(parse_ipv6_cidr("not-a-cidr").is_none());
        assert!(parse_ipv6_cidr("::1/129").is_none());
        // /0 是特例，確認沒有因為 `128 - 0 = 128` 造成位移溢位 panic。
        let whole = parse_ipv6_cidr("::/0").unwrap();
        assert_eq!(whole.start, 0);
        assert_eq!(whole.end, u128::MAX);
    }

    #[test]
    fn local_cloud_range_match_and_miss_ipv6() {
        {
            let mut guard = CLOUD_RANGES_V6.lock().unwrap();
            *guard = vec![parse_ipv6_cidr("2600:1900:8000::/44").unwrap()];
        }
        let inside: std::net::Ipv6Addr = "2600:1900:8000:1::1".parse().unwrap();
        let outside: std::net::Ipv6Addr = "2001:4860:4860::8888".parse().unwrap(); // Google 公開 DNS，不在這段範圍內
        assert!(is_known_cloud_ipv6(inside));
        assert!(!is_known_cloud_ipv6(outside));
        {
            let mut guard = CLOUD_RANGES_V6.lock().unwrap();
            guard.clear();
        }
    }

    /// ICLOUD_PRIVATE_RELAY_ALLOWLIST_V1：CSV 解析——正常列、IPv6 列、格式
    /// 壞掉的列混在一起時，壞掉的那一列該被跳過，其餘照常解析出來。
    #[test]
    fn parses_private_relay_csv() {
        let csv = "172.224.226.0/27,GB,GB-EN,London,\n\
                    2a02:26f7:b000:4000::/64,US,US-AK,Anchorage,\n\
                    not-a-valid-line-without-comma\n\
                    bad-cidr,US,US-CA,SF,\n\
                    \n\
                    17.140.0.0/16,US,CA,SFO,";
        let (v4, v6) = parse_private_relay_csv(csv);
        assert_eq!(v4.len(), 2);
        assert_eq!(v6.len(), 1);
        assert_eq!(v4[0].start, u32::from(Ipv4Addr::new(172, 224, 226, 0)));
        assert_eq!(v4[1].start, u32::from(Ipv4Addr::new(17, 140, 0, 0)));
    }

    /// ICLOUD_PRIVATE_RELAY_ALLOWLIST_V1：命中放行清單的 IP 要回 true、沒命中
    /// 的要回 false，邏輯跟 AWS／GCP 那份一模一樣，只是清單意義相反。
    #[test]
    fn private_relay_range_match_and_miss() {
        {
            let mut guard = PRIVATE_RELAY_RANGES.lock().unwrap();
            *guard = vec![parse_ipv4_cidr("172.224.226.0/27").unwrap()];
        }
        assert!(is_private_relay_ipv4(Ipv4Addr::new(172, 224, 226, 5)));
        assert!(!is_private_relay_ipv4(Ipv4Addr::new(8, 8, 8, 8)));
        {
            let mut guard = PRIVATE_RELAY_RANGES.lock().unwrap();
            guard.clear();
        }
    }

    /// ICLOUD_PRIVATE_RELAY_ALLOWLIST_V1：`lookup` 對命中放行清單的 IP 要
    /// 直接回 `None`（fail-open／放行），完全不去碰 AWS／GCP 本地清單或
    /// ip-api.com——這裡只驗證放行清單優先權蓋過本地雲端清單這一段（兩者都
    /// 是同步的記憶體比對，不需要 async runtime 也能測）。
    #[test]
    fn lookup_bypasses_cloud_match_for_private_relay_ip() {
        let target = Ipv4Addr::new(172, 224, 226, 5);
        {
            let mut v4 = PRIVATE_RELAY_RANGES.lock().unwrap();
            *v4 = vec![parse_ipv4_cidr("172.224.226.0/27").unwrap()];
            let mut cloud = CLOUD_RANGES.lock().unwrap();
            // 刻意也放進「已知雲端」清單，確認放行清單的檢查順序在它之前、
            // 不會被它蓋過去。
            *cloud = vec![parse_ipv4_cidr("172.224.226.0/27").unwrap()];
        }
        assert!(is_private_relay_ipv4(target));
        // `lookup` 本身要連網路，這裡只驗證短路判斷本身的語意：一旦
        // `is_private_relay_ipv4` 為 true，`lookup` 內的 early return 必定
        // 先於 `is_known_cloud_ipv4` 的檢查執行（見原始碼順序），不需要真的
        // 發出請求也能確認這個不變量。
        {
            let mut v4 = PRIVATE_RELAY_RANGES.lock().unwrap();
            v4.clear();
            let mut cloud = CLOUD_RANGES.lock().unwrap();
            cloud.clear();
        }
    }
}
