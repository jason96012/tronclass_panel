//! 作業清單共享快取（demand-driven fetch）
//!
//! 解決「一群人同時請求」的問題，三個機制：
//!
//! 1. **TTL 快取** — 剛抓過的資料在 [`CACHE_TTL`] 內直接重用，不打上游。
//! 2. **單一飛行（single-flight）** — 同一帳號同時有多個請求時（多分頁、
//!    多使用者、排程器與網頁撞在一起），只有第一個真的去抓，其餘等它的結果。
//! 3. **共用** — 網頁與背景排程器共享同一份快取；誰先讀誰付成本，另一邊白拿。
//!
//! 沒有這一層時，每次網頁載入、每 5 分鐘自動刷新、每 15 分鐘排程輪詢，
//! 都會各自對學校打一輪 20 次請求。

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use tokio::sync::Mutex;
use tracing::debug;

use crate::api::todos::TronclassTodoItem;

/// 快取有效期限。
///
/// 設為 5 分鐘，與前端自動刷新間隔一致：正常瀏覽時多數請求會落在快取內，
/// 但資料不會舊到影響 1 小時／6 小時的截止提醒判斷。
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// 快取項目數上限，避免帳號被刪除後殘留條目無限增長。
const MAX_CACHE_ENTRIES: usize = 256;

type Items = Arc<Vec<TronclassTodoItem>>;

struct CacheEntry {
    items: Items,
    fetched_at: Instant,
    /// 對應的 Unix 時間戳記，供前端顯示「資料時間」。
    /// 從 SQLite 快照回填時為快照寫入時間。
    fetched_at_unix: Option<i64>,
}

static CACHE: Lazy<Mutex<HashMap<String, CacheEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// 每帳號一把鎖，用來實現 single-flight。
static FETCH_LOCKS: Lazy<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// 讀取仍在有效期內的快取；過期或不存在時回傳 `None`。
async fn read_fresh(account_id: &str) -> Option<Items> {
    let cache = CACHE.lock().await;
    let entry = cache.get(account_id)?;
    (entry.fetched_at.elapsed() < CACHE_TTL).then(|| Arc::clone(&entry.items))
}

/// 寫入快取，並順手清掉過期條目。
async fn write(account_id: &str, items: Items) {
    let mut cache = CACHE.lock().await;
    cache.retain(|_, entry| entry.fetched_at.elapsed() < CACHE_TTL);
    if cache.len() >= MAX_CACHE_ENTRIES {
        cache.clear();
    }
    cache.insert(
        account_id.to_string(),
        CacheEntry {
            items,
            fetched_at: Instant::now(),
            fetched_at_unix: Some(chrono::Local::now().timestamp()),
        },
    );
}

/// 取得某帳號專屬的 fetch 鎖。
async fn fetch_lock(account_id: &str) -> Arc<Mutex<()>> {
    let mut locks = FETCH_LOCKS.lock().await;
    if locks.len() >= MAX_CACHE_ENTRIES {
        // 只保留仍被持有的鎖（strong_count > 1 表示有請求正在等待）。
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
    }
    Arc::clone(
        locks
            .entry(account_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    )
}

/// 取得作業清單：優先用快取，未命中才呼叫 `fetch`。
///
/// 同一帳號同時有多個呼叫者時，只有一個會真的執行 `fetch`，
/// 其餘會等它完成並共用結果（single-flight）。
///
/// - `force`：略過快取強制重抓（供使用者手動「重新整理」使用）。
///   仍受 single-flight 保護，不會因連點而重複打上游。
pub async fn get_or_fetch<F, Fut>(
    account_id: &str,
    force: bool,
    fetch: F,
) -> miette::Result<Items>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = miette::Result<Vec<TronclassTodoItem>>>,
{
    if !force {
        if let Some(items) = read_fresh(account_id).await {
            debug!(account = %account_id, count = items.len(), "作業清單快取命中");
            return Ok(items);
        }
    }

    // force 時記下「請求發生的時刻」。等鎖期間若有人抓到比這更新的資料，
    // 才算滿足這次的強制刷新；否則不能拿請求之前的舊資料充數。
    let requested_at = Instant::now();

    let lock = fetch_lock(account_id).await;
    let _guard = lock.lock().await;

    // 取得鎖之後再確認一次：等鎖期間可能已有人抓完並填好快取。
    // 併發的重新整理只需要一次真實請求，但必須是「這次請求之後」抓到的資料。
    let usable = {
        let cache = CACHE.lock().await;
        cache.get(account_id).and_then(|entry| {
            let fresh = entry.fetched_at.elapsed() < CACHE_TTL;
            let newer_than_request = entry.fetched_at >= requested_at;
            (fresh && (!force || newer_than_request)).then(|| Arc::clone(&entry.items))
        })
    };
    if let Some(items) = usable {
        debug!(account = %account_id, "等待期間已有其他請求完成，重用其結果");
        return Ok(items);
    }

    debug!(account = %account_id, force = force, "作業清單快取未命中，向 Tronclass 抓取");
    let items = Arc::new(fetch().await?);
    write(account_id, Arc::clone(&items)).await;
    Ok(items)
}

/// 僅讀取快取，不觸發任何上游請求。
///
/// 背景排程器用它判斷「網頁剛抓過了嗎」，避免重複打同一份資料。
pub async fn peek(account_id: &str) -> Option<Items> {
    read_fresh(account_id).await
}

/// 讀取任何可用的既有資料，**即使已超過 TTL**，並附上抓取時間。
///
/// 供「開頁先秒顯示、背景再更新」（stale-while-revalidate）使用：
/// 先看記憶體，記憶體沒有再回落到 SQLite 快照（例如程式剛重啟）。
pub async fn peek_stale(
    account_id: &str,
    db: &crate::db::AccountsDb,
) -> Option<(Items, Option<i64>)> {
    {
        let cache = CACHE.lock().await;
        if let Some(entry) = cache.get(account_id) {
            return Some((Arc::clone(&entry.items), entry.fetched_at_unix));
        }
    }

    // 記憶體沒有 → 讀 SQLite 快照（程式重啟後的第一次開頁會走這裡）。
    let (items_json, fetched_at) = db.get_homework_snapshot(account_id).await.ok()??;
    let items: Vec<TronclassTodoItem> = serde_json::from_str(&items_json).ok()?;
    let items: Items = Arc::new(items);

    // 回填記憶體，但把時間戳記標成「已過期」，確保仍會觸發背景更新。
    let mut cache = CACHE.lock().await;
    cache.insert(
        account_id.to_string(),
        CacheEntry {
            items: Arc::clone(&items),
            fetched_at: Instant::now()
                .checked_sub(CACHE_TTL)
                .unwrap_or_else(Instant::now),
            fetched_at_unix: Some(fetched_at),
        },
    );

    Some((items, Some(fetched_at)))
}

/// 抓取成功後同步寫入 SQLite 快照，讓下次冷啟動也能秒顯示。
pub async fn persist(account_id: &str, items: &[TronclassTodoItem], db: &crate::db::AccountsDb) {
    let Ok(json) = serde_json::to_string(items) else {
        return;
    };
    if let Err(error) = db
        .save_homework_snapshot(account_id, &json, items.len() as i64)
        .await
    {
        debug!(account = %account_id, %error, "寫入作業快照失敗（不影響本次回應）");
    }
}

/// 清除指定帳號的快取（帳號設定變更時使用）。
pub async fn invalidate(account_id: &str) {
    CACHE.lock().await.remove(account_id);
}

