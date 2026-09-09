//! Health check page/API handlers for Admin Web.

use super::*;

/// HEALTHZ_REAL_V1：真正的健康檢查（給 Pterodactyl／uptime 監測／Cloudflare health check 用）。
///
/// 檢查項目：
/// 1. SQLite 可寫可讀（寫入 `web_push_settings` 的 `system:healthz_probe` 再讀回；不動 schema）。
/// 2. 每個「執行中」帳號的監控循環心跳是否在看門狗門檻內（MONITOR_WATCHDOG_V1）。
///
/// 任一項失敗 → 503 + `ok:false` 與各項細節；全部正常 → 200。
/// 不需登入（黑名單／同源／密碼中介層皆放行 /healthz），所以回應不含帳號識別以外的敏感資料，
/// 帳號 id 也只在 503 時列出失聯者。
pub(super) async fn healthz_handler(State(state): State<AdminWebState>) -> Response {
    let started = std::time::Instant::now();
    let now = chrono::Local::now().timestamp();

    // ── 1. 資料庫讀寫 ─────────────────────────────────────────────────────
    let db_probe_value = now.to_string();
    let db_result = async {
        state
            .accounts_db
            .set_web_push_setting("system:healthz_probe", &db_probe_value)
            .await?;
        let read_back = state
            .accounts_db
            .get_web_push_setting("system:healthz_probe")
            .await?;
        Ok::<bool, miette::Report>(read_back.as_deref() == Some(db_probe_value.as_str()))
    };
    let db_result = tokio::time::timeout(std::time::Duration::from_secs(5), db_result).await;
    let (db_ok, db_error) = match db_result {
        Ok(Ok(true)) => (true, None),
        Ok(Ok(false)) => (false, Some("read-back mismatch".to_string())),
        Ok(Err(error)) => (false, Some(error.to_string())),
        Err(_) => (false, Some("timeout (5s)".to_string())),
    };
    let db_latency_ms = started.elapsed().as_millis() as u64;

    // ── 2. 監控循環心跳 ───────────────────────────────────────────────────
    let poll_interval_secs =
        crate::runtime_settings::poll_interval(state.default_poll_interval_secs)
            .await
            .as_secs()
            .max(1);
    let threshold_secs = crate::monitor_watchdog::stale_after_secs(poll_interval_secs);
    let accounts = state.accounts.snapshot();
    let mut running = 0usize;
    let mut paused = 0usize;
    let mut starting = 0usize;
    let mut stale: Vec<serde_json::Value> = Vec::new();
    let mut oldest_heartbeat_age: Option<i64> = None;
    for account in accounts.iter() {
        let is_running = *account.is_running.lock().await;
        if !is_running {
            paused += 1;
            continue;
        }
        running += 1;
        let heartbeat = account.last_heartbeat();
        if heartbeat <= 0 {
            starting += 1;
            continue;
        }
        // MONITOR_WATCHDOG_V1：哨兵值 1 = 循環已異常結束、等待看門狗重生。
        let crashed = heartbeat == crate::adapters::requests::HEARTBEAT_CRASHED;
        let age = if crashed { 0 } else { now.saturating_sub(heartbeat) };
        if !crashed {
            oldest_heartbeat_age = Some(oldest_heartbeat_age.map_or(age, |old| old.max(age)));
        }
        if crate::monitor_watchdog::is_stale(heartbeat, now, poll_interval_secs) {
            let last_poll = account.monitor_status.lock().await.last_poll_timestamp;
            stale.push(json!({
                "account_id": account.account_id,
                "heartbeat_age_secs": age,
                "crashed": crashed,
                "last_poll_timestamp": last_poll,
            }));
        }
    }
    let monitors_ok = stale.is_empty();
    let ok = db_ok && monitors_ok;

    let body = json!({
        "ok": ok,
        "service": "tronclass_dashboard",
        "version": env!("CARGO_PKG_VERSION"),
        "time": now,
        "checks": {
            "database": {
                "ok": db_ok,
                "latency_ms": db_latency_ms,
                "error": db_error,
            },
            "monitors": {
                "ok": monitors_ok,
                "total": accounts.len(),
                "running": running,
                "paused": paused,
                "starting": starting,
                "stale": stale,
                "poll_interval_secs": poll_interval_secs,
                "stale_after_secs": threshold_secs,
                "oldest_heartbeat_age_secs": oldest_heartbeat_age,
            },
        },
    });
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, [(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

pub(super) async fn admin_health_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    // ADMIN_PAGE_TRI_STATE_V1：伺服器端驗證（含 refresh 續期），未登入導回登入頁。
    let Some(page) = super::pages::resolve_page_session(&state, &headers, &query.token).await else {
        return super::admin_login_redirect("/admin/health");
    };
    if page.session.must_change_password {
        return super::pages::attach_fresh_cookies(super::must_change_password_redirect(), &page, &headers);
    }
    super::pages::attach_fresh_cookies(
        super::html_page_response(&headers, super::pages::inject_auth_attrs(&super::pages::inject_theme(super::pages::ADMIN_HEALTH_HTML), &page.session)),
        &page,
        &headers,
    )
}

pub(super) async fn admin_health_api_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return unauthorized_response();
    }

    let mut running_accounts = 0usize;
    let mut detected_accounts = 0usize;
    let mut latest_poll_timestamp: Option<i64> = None;
    let mut total_failures = 0u32;
    let runtime_accounts = state.accounts.snapshot();
    for account in runtime_accounts.iter() {
        if *account.is_running.lock().await {
            running_accounts += 1;
        }
        let status = account.monitor_status.lock().await.clone();
        if status.last_detected_timestamp.is_some() {
            detected_accounts += 1;
        }
        if let Some(ts) = status.last_poll_timestamp {
            latest_poll_timestamp =
                Some(latest_poll_timestamp.map(|old| old.max(ts)).unwrap_or(ts));
        }
        total_failures = total_failures.saturating_add(status.consecutive_failures);
    }

    let db_status = match state.accounts_db.list().await {
        Ok(accounts) => json!({ "ok": true, "account_rows": accounts.len(), "error": null }),
        Err(e) => json!({ "ok": false, "account_rows": 0, "error": e.to_string() }),
    };

    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "-".to_string());

    let uptime = read_proc_uptime();
    let loadavg =
        std::fs::read_to_string("/proc/loadavg").unwrap_or_else(|_| "unavailable".to_string());
    let meminfo = read_meminfo_summary();
    let disk = run_health_command("df", &["-h", "."]).await;
    let process = run_health_command(
        "ps",
        &[
            "-p",
            &std::process::id().to_string(),
            "-o",
            "pid,ppid,etime,%cpu,%mem,rss,cmd",
        ],
    )
    .await;
    // Pterodactyl (and most containers) have no systemd, so `systemctl` fails
    // with "exit status: 1" and that raw error used to be rendered as if it
    // were the service state. Report the real supervisor instead.
    let systemctl = supervisor_status().await;
    let watchdog = watchdog_status();

    (
        StatusCode::OK,
        Json(json!({
            "time": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            "version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
            "cwd": cwd,
            "uptime": uptime,
            "loadavg": loadavg.trim(),
            "memory": meminfo,
            "accounts": {
                "total": runtime_accounts.len(),
                "running": running_accounts,
                "paused": runtime_accounts.len().saturating_sub(running_accounts),
                "detected": detected_accounts,
                "total_failures": total_failures,
                "latest_poll_timestamp": latest_poll_timestamp,
            },
            "database": db_status,
            "commands": {
                "disk": disk,
                "process": process,
                "systemctl": systemctl,
                "watchdog": watchdog,
                "cloudflare": cloudflare_connector_status(),
            },
            // LOG_DISK_GUARD_V1：logs/、config/ 兩個目錄各占多少硬碟，讓維護頁一眼看到。
            "storage": storage_usage(),
        })),
    )
        .into_response()
}

/// METRICS_HISTORY_V1：24 小時內每分鐘一筆的系統／輪詢延遲快照（維護頁折線圖）。
pub(super) async fn admin_health_history_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return unauthorized_response();
    }
    (StatusCode::OK, Json(crate::metrics_history::snapshot())).into_response()
}

pub(super) async fn admin_health_check_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return unauthorized_response();
    }

    let mut checks = Vec::new();

    // 1. SQLite / accounts.db
    match state.accounts_db.integrity_check().await {
        Ok(result) if result.trim().eq_ignore_ascii_case("ok") => checks.push(json!({
            "name": "SQLite 完整性",
            "ok": true,
            "level": "ok",
            "message": "PRAGMA integrity_check：ok",
            "details": result,
        })),
        Ok(result) => checks.push(json!({
            "name": "SQLite 完整性",
            "ok": false,
            "level": "bad",
            "message": "資料庫完整性檢查失敗",
            "details": result,
        })),
        Err(e) => checks.push(json!({
            "name": "SQLite 完整性",
            "ok": false,
            "level": "bad",
            "message": "完整性檢查無法執行",
            "details": e.to_string(),
        })),
    }
    match state.accounts_db.list().await {
        Ok(rows) => checks.push(json!({
            "name": "SQLite 帳號資料庫",
            "ok": true,
            "level": "ok",
            "message": format!("可讀取，帳號列 {} 筆", rows.len()),
            "details": null,
        })),
        Err(e) => checks.push(json!({
            "name": "SQLite 帳號資料庫",
            "ok": false,
            "level": "bad",
            "message": "讀取失敗",
            "details": e.to_string(),
        })),
    }

    // 2. runtime accounts
    let mut running = 0usize;
    let mut detected = 0usize;
    let mut latest_poll = None::<i64>;
    let mut failures = 0u32;
    let runtime_accounts = state.accounts.snapshot();
    for account in runtime_accounts.iter() {
        if *account.is_running.lock().await {
            running += 1;
        }
        let status = account.monitor_status.lock().await.clone();
        if status.last_detected_timestamp.is_some() {
            detected += 1;
        }
        if let Some(ts) = status.last_poll_timestamp {
            latest_poll = Some(latest_poll.map(|old| old.max(ts)).unwrap_or(ts));
        }
        failures = failures.saturating_add(status.consecutive_failures);
    }
    let total = runtime_accounts.len();
    let accounts_ok = total > 0 && running > 0;
    // ACCOUNT_HOT_ADD_FEEDBACK_V1：在 DB 且 enabled 但沒有監控循環的帳號也要浮上來。
    let inactive: Vec<serde_json::Value> = match state.monitor_spawner.as_ref() {
        Some(spawner) => state
            .accounts_db
            .list()
            .await
            .unwrap_or_default()
            .into_iter()
            // VIEWER_NO_SCHOOL_API_V1：檢視者本來就不監控，不列為異常。
            .filter(|row| row.enabled && !row.is_viewer && !spawner.is_active(&row.id))
            .map(|row| json!({ "account_id": row.id, "last_error": spawner.last_error(&row.id) }))
            .collect(),
        None => Vec::new(),
    };
    let inactive_note = if inactive.is_empty() {
        String::new()
    } else {
        format!("，另有 {} 個帳號在資料庫中但未在監控", inactive.len())
    };
    checks.push(json!({
        "name": "監控帳號",
        "ok": accounts_ok,
        "level": if accounts_ok { if running < total || failures > 0 || !inactive.is_empty() { "warn" } else { "ok" } } else { "bad" },
        "message": format!("運行 {running}/{total}，偵測 {detected}，失敗加總 {failures}{inactive_note}"),
        "details": json!({ "latest_poll_timestamp": latest_poll, "inactive_accounts": inactive }),
    }));

    // 3. materials directory write test
    let materials = materials_root();
    let material_check = async {
        tokio::fs::create_dir_all(&materials).await?;
        let test_file = materials.join(".healthcheck-write-test.tmp");
        tokio::fs::write(&test_file, b"ok").await?;
        let _ = tokio::fs::remove_file(&test_file).await;
        Ok::<(), std::io::Error>(())
    }
    .await;
    match material_check {
        Ok(()) => checks.push(json!({
            "name": "教材 materials 目錄",
            "ok": true,
            "level": "ok",
            "message": "可建立、可寫入、可刪除測試檔",
            "details": materials.display().to_string(),
        })),
        Err(e) => checks.push(json!({
            "name": "教材 materials 目錄",
            "ok": false,
            "level": "bad",
            "message": "寫入測試失敗",
            "details": format!("{}: {e}", materials.display()),
        })),
    }

    // 4. disk usage
    let disk = run_quick_check_command("df", &["-h", "."]).await;
    checks.push(json!({
        "name": "磁碟空間",
        "ok": !disk.contains("command failed") && !disk.contains("timeout"),
        "level": "ok",
        "message": first_line_after_header(&disk).unwrap_or_else(|| disk.clone()),
        "details": disk,
    }));

    // 5. memory/swap
    let mem = read_meminfo_summary();
    let mem_used = mem
        .get("mem_used_label")
        .and_then(|v| v.as_str())
        .unwrap_or("-");
    let mem_total = mem
        .get("mem_total_label")
        .and_then(|v| v.as_str())
        .unwrap_or("-");
    let swap_used = mem
        .get("swap_used_label")
        .and_then(|v| v.as_str())
        .unwrap_or("-");
    let swap_total = mem
        .get("swap_total_label")
        .and_then(|v| v.as_str())
        .unwrap_or("-");
    checks.push(json!({
        "name": "RAM / Swap",
        "ok": true,
        "level": "ok",
        "message": format!("RAM {mem_used}/{mem_total}，Swap {swap_used}/{swap_total}"),
        "details": mem,
    }));

    // 6. 程序監管（systemd 或容器）
    let systemctl = supervisor_status().await;
    let has_systemd = systemd_available().await;
    // Under a container supervisor the process is alive by definition: this
    // code is running. Only treat systemd as failed when systemd is actually
    // in use and reports something other than active.
    let systemd_ok = if has_systemd {
        systemctl.trim() == "active"
    } else {
        true
    };
    checks.push(json!({
        "name": if has_systemd { "systemd 服務" } else { "程序狀態" },
        "ok": systemd_ok,
        "level": if systemd_ok { "ok" } else { "bad" },
        "message": systemctl.trim(),
        "details": systemctl,
    }));

    // 7. watchdog background services
    let wd = watchdog_status();
    let wd_down = wd.get("services").and_then(|v| v.as_array()).map(|xs| xs.iter().filter(|x| x.get("id").and_then(|v| v.as_str()) != Some("dashboard") && x.get("alive").and_then(|v| v.as_bool()) != Some(true)).count()).unwrap_or(0);
    checks.push(json!({
        "name": "Tunnel / Watchdog",
        "ok": wd_down == 0,
        "level": if wd_down == 0 { "ok" } else { "bad" },
        "message": if wd_down == 0 { format!("監控中，間隔 {} 秒", wd.get("interval_secs").and_then(|v| v.as_u64()).unwrap_or(15)) } else { format!("有 {wd_down} 個背景服務停止") },
        "details": wd,
    }));

    // 8. logs recent errors
    let recent_logs = crate::logs::snapshot(None, 30);
    let recent_bad = recent_logs
        .iter()
        .filter(|entry| entry.level == "ERROR" || entry.level == "WARN")
        .count();
    checks.push(json!({
        "name": "最近系統紀錄",
        "ok": recent_bad == 0,
        "level": if recent_bad == 0 { "ok" } else { "warn" },
        "message": if recent_bad == 0 { "最近 30 筆沒有 WARN/ERROR".to_string() } else { format!("最近 30 筆有 {recent_bad} 筆 WARN/ERROR") },
        "details": recent_logs.into_iter().take(5).collect::<Vec<_>>(),
    }));

    let failed = checks
        .iter()
        .filter(|c| c.get("ok").and_then(|v| v.as_bool()) == Some(false))
        .count();
    let warn = checks
        .iter()
        .filter(|c| c.get("level").and_then(|v| v.as_str()) == Some("warn"))
        .count();
    let level = if failed > 0 {
        "bad"
    } else if warn > 0 {
        "warn"
    } else {
        "ok"
    };

    (
        StatusCode::OK,
        Json(json!({
            "time": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            "summary": {
                "level": level,
                "total": checks.len(),
                "failed": failed,
                "warn": warn,
            },
            "checks": checks,
        })),
    )
        .into_response()
}

// TUNNEL_RESTART_LOOP_FIX_V1
// 之前這裡看的是 cloudflared.log「最後 80 行」：檔案是追加寫、永不清除，
// 一次正常瞬斷的錯誤行會一直留在尾巴，卡片就永遠顯示「近期有連線錯誤」，
// 前端又把它算進 Watchdog 的「有服務停止」，造成兩張卡同時紅。
// 改成與 ptero-start.sh 同一套判定：只看 watchdog 游標（cloudflared-watchdog.offset）
// 之後新寫入的內容，而且看到重連成功訊息就視為已恢復；另外統計最近一小時
// watchdog 重啟了幾次，讓「一直在重啟」這件事直接顯示在卡片上。
fn cloudflare_connector_status() -> serde_json::Value {
    let log_bytes = std::fs::read("logs/cloudflared.log").unwrap_or_default();
    let offset = std::fs::read_to_string("logs/cloudflared-watchdog.offset")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let start = if offset > log_bytes.len() { 0 } else { offset };
    let new_part = String::from_utf8_lossy(&log_bytes[start..]).replace('\0', "");
    // 太長時只看尾端 32 KB 即可（游標之後通常很短，這是保護）。
    let new_part = if new_part.len() > 32 * 1024 {
        let cut = new_part.len() - 32 * 1024;
        let mut idx = cut;
        while idx < new_part.len() && !new_part.is_char_boundary(idx) {
            idx += 1;
        }
        new_part[idx..].to_string()
    } else {
        new_part
    };
    // REGEX_STATIC_V1：健康頁每次載入都會呼叫，regex 不再重複編譯。
    let fatal = CLOUDFLARED_FATAL_RE.is_match(&new_part);
    let transient = CLOUDFLARED_ERROR_RE.is_match(&new_part);
    let recovered = CLOUDFLARED_RECOVERY_RE.is_match(&new_part);
    let error = fatal || (transient && !recovered);

    // 最近一小時 watchdog 重啟次數（從 cloudflared-watchdog.log 的重啟行解析 ISO 時間）。
    let now = chrono::Local::now();
    let watchdog_log = std::fs::read_to_string("logs/cloudflared-watchdog.log").unwrap_or_default();
    let mut restarts_last_hour = 0usize;
    let mut last_restart: Option<String> = None;
    for line in watchdog_log.lines().rev().take(400) {
        if !line.contains("重新啟動") {
            continue;
        }
        let Some(ts) = CLOUDFLARED_RESTART_TS_RE
            .captures(line)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
        else {
            continue;
        };
        if last_restart.is_none() {
            last_restart = Some(ts.clone());
        }
        if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(&ts) {
            if now.signed_duration_since(parsed.with_timezone(&chrono::Local)).num_seconds() <= 3600 {
                restarts_last_hour += 1;
            }
        }
    }
    let log_failures = std::fs::read_to_string("logs/cloudflared-log-failures")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(0);
    let http_failures = std::fs::read_to_string("logs/cloudflared-http-failures")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(0);

    json!({
        "pid_alive": std::path::Path::new("logs/cloudflared.pid").exists(),
        "recent_error": error,
        "fatal_error": fatal,
        "recovered": recovered,
        "restart_loop": restarts_last_hour >= 3,
        "restarts_last_hour": restarts_last_hour,
        "last_restart": last_restart,
        "log_failures": log_failures,
        "http_failures": http_failures,
        "log_file": "logs/cloudflared.log",
    })
}

/// cloudflared 日誌中的「瞬斷」特徵（cloudflared 會自行重連，不代表要重啟）。
static CLOUDFLARED_ERROR_RE: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| {
        regex::Regex::new(
            "(?i)connection refused|no connection|unable to reach|context deadline exceeded|connection lost|quic connection closed|serve connection error|failed to dial|Retrying connection",
        )
        .expect("valid regex")
    });

/// 致命錯誤（token／tunnel 設定問題），不會自己好。
static CLOUDFLARED_FATAL_RE: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| {
        regex::Regex::new("(?i)Unauthorized|invalid tunnel|tunnel not found|failed to serve|Token is invalid|credentials")
            .expect("valid regex")
    });

/// 重連成功特徵：看到就代表前面的瞬斷已恢復。
static CLOUDFLARED_RECOVERY_RE: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| {
        regex::Regex::new("(?i)Registered tunnel connection|Connection .* registered|Updated to new configuration")
            .expect("valid regex")
    });

/// watchdog 重啟行內的 ISO 8601 時間戳（`date -Iseconds` 格式）。
static CLOUDFLARED_RESTART_TS_RE: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}[+-]\d{2}:?\d{2})").expect("valid regex")
    });

/// LOG_DISK_GUARD_V1：統計 logs/ 與 config/ 的大小（只走一層，檔案數很少，成本可忽略）。
fn dir_size_shallow(dir: &str) -> (u64, usize, Option<(String, u64)>) {
    let mut total = 0u64;
    let mut files = 0usize;
    let mut largest: Option<(String, u64)> = None;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let len = meta.len();
            total += len;
            files += 1;
            if largest.as_ref().map(|(_, l)| len > *l).unwrap_or(true) {
                largest = Some((entry.file_name().to_string_lossy().into_owned(), len));
            }
        }
    }
    (total, files, largest)
}

fn storage_usage() -> serde_json::Value {
    let (logs_bytes, logs_files, logs_largest) = dir_size_shallow("logs");
    let (config_bytes, config_files, config_largest) = dir_size_shallow("config");
    let max_log = std::env::var("LOG_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(10 * 1024 * 1024);
    let keep = std::env::var("LOG_ROTATE_KEEP")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(5);
    json!({
        "logs": {
            "bytes": logs_bytes,
            "label": format_bytes_label(logs_bytes),
            "files": logs_files,
            "largest": logs_largest.as_ref().map(|(n, l)| json!({ "name": n, "bytes": l, "label": format_bytes_label(*l) })),
            "rotate_max_bytes": max_log,
            "rotate_keep": keep,
            // 每個 .log 的理論上限 = 現用檔 + keep 份備份
            "per_log_cap_label": format_bytes_label(max_log * (keep + 1)),
        },
        "config": {
            "bytes": config_bytes,
            "label": format_bytes_label(config_bytes),
            "files": config_files,
            "largest": config_largest.as_ref().map(|(n, l)| json!({ "name": n, "bytes": l, "label": format_bytes_label(*l) })),
        }
    })
}

fn watchdog_status() -> serde_json::Value {
    let names = [
        ("dashboard", "主程式"),
        ("cloudflared", "Cloudflare Tunnel"),
        ("filebrowser-auth", "Filebrowser 密碼服務"),
        ("filebrowser-open", "Filebrowser 使用者服務"),
        ("filebrowser-admin", "Filebrowser 管理員服務"),
        ("filebrowser-caddy", "Filebrowser Caddy"),
    ];
    let filebrowser_enabled = std::path::Path::new("config/filebrowser.enable").exists() || std::env::var("FILEBROWSER_ENABLE").ok().as_deref() == Some("1");
    let services: Vec<serde_json::Value> = names.iter().filter(|(id, _)| filebrowser_enabled || !id.starts_with("filebrowser-")).map(|(id, label)| {
        let path = format!("logs/{id}.pid");
        let pid = std::fs::read_to_string(&path).ok().and_then(|v| v.trim().parse::<u32>().ok());
        let alive = pid.map(|p| libc_kill_zero(p)).unwrap_or(false);
        json!({ "id": id, "label": label, "pid": pid, "alive": alive, "pid_file": path })
    }).collect();
    json!({ "enabled": std::path::Path::new("logs/cloudflared.pid").exists(), "http_checks": true, "interval_secs": std::env::var("WATCHDOG_INTERVAL_SECS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(15), "services": services })
}

#[cfg(unix)]
fn libc_kill_zero(pid: u32) -> bool {
    std::process::Command::new("kill").args(["-0", &pid.to_string()]).status().map(|s| s.success()).unwrap_or(false)
}
#[cfg(not(unix))]
fn libc_kill_zero(_pid: u32) -> bool { false }

/// True when this host actually uses systemd. Containers such as Pterodactyl
/// do not, and probing them with `systemctl` only produces noise.
async fn systemd_available() -> bool {
    std::path::Path::new("/run/systemd/system").exists()
}

/// Describe how this process is supervised, without pretending a missing
/// systemd is a failure.
async fn supervisor_status() -> String {
    if systemd_available().await {
        return run_health_command("systemctl", &["is-active", "tronclass-rollcall"]).await;
    }
    let pid = std::process::id();
    let uptime = run_health_command("ps", &["-p", &pid.to_string(), "-o", "etime="]).await;
    let uptime = uptime.trim();
    if uptime.is_empty() || uptime.starts_with("exit") || uptime.starts_with("command failed") {
        format!("running (container, pid {pid})")
    } else {
        format!("running (container, pid {pid}, uptime {uptime})")
    }
}

async fn run_quick_check_command(program: &str, args: &[&str]) -> String {
    match tokio::time::timeout(
        std::time::Duration::from_secs(3),
        tokio::process::Command::new(program).args(args).output(),
    )
    .await
    {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if output.status.success() {
                if stdout.is_empty() {
                    "ok".to_string()
                } else {
                    stdout
                }
            } else if stderr.is_empty() {
                format!("command failed: {}: {}", output.status, stdout)
            } else {
                format!("command failed: {}: {}", output.status, stderr)
            }
        }
        Ok(Err(e)) => format!("command failed: {e}"),
        Err(_) => "command timeout".to_string(),
    }
}

fn first_line_after_header(text: &str) -> Option<String> {
    let mut lines = text.lines();
    let _ = lines.next();
    lines.next().map(str::to_string)
}

#[cfg(test)]
mod regex_tests {
    /// UNWRAP_SWEEP_V1：強制初始化 cloudflared 日誌用的 Lazy regex。
    #[test]
    fn static_regexes_compile() {
        let _ = &*super::CLOUDFLARED_ERROR_RE;
        let _ = &*super::CLOUDFLARED_FATAL_RE;
        let _ = &*super::CLOUDFLARED_RECOVERY_RE;
        let _ = &*super::CLOUDFLARED_RESTART_TS_RE;
    }
}

// ── CLIENT_ERROR_REPORT_V1 ────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub(super) struct ClientLogRequest {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    page: String,
    #[serde(default)]
    stack: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    line: u64,
    #[serde(default)]
    col: u64,
    #[serde(default)]
    ua: String,
    #[serde(default)]
    standalone: bool,
    #[serde(default)]
    visibility: String,
}

/// CLIENT_ERROR_REPORT_V1：每個 session 每分鐘最多幾筆（sendBeacon 沒有回應也沒關係）。
const CLIENT_LOG_MAX_PER_MINUTE: u32 = 30;

fn sanitize_client_field(value: &str, max_chars: usize) -> String {
    let cleaned: String = value
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect();
    crate::text_utils::truncate_chars(cleaned.trim(), max_chars).to_string()
}

/// CLIENT_ERROR_REPORT_V1：前端錯誤／看門狗事件回報。
///
/// - 需登入（access cookie；sendBeacon 會帶同源 cookie）。未登入回 401 但不寫任何東西。
/// - 每 session 每分鐘 30 筆（走 RateLimiter 的記憶體視窗，不寫 SQLite；key 以 session 帳號區分）。
/// - body 由 `ADMIN_TINY_BODY_LIMIT`（8 KB）擋掉超大 payload；欄位再各自截斷、濾控制字元。
/// - 寫入 `crate::logs::record_system`（kind=system，source=client）→ /admin/logs 可見，
///   同時以 tracing warn 輸出到 console。
pub(super) async fn client_log_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    ClientIp(ip): ClientIp,
    body: axum::body::Bytes,
) -> Response {
    let Some(session) = state.authorize_any(&query.token).await else {
        return unauthorized_response();
    };
    // sendBeacon 送出的 Content-Type 可能是 text/plain 或 application/json，
    // 所以不用 Json 抽取器，自己解析。
    let request: ClientLogRequest = serde_json::from_slice(&body).unwrap_or_default();
    let rate_key = format!("client-log:{}", session.account_id);
    if !state
        .rate_limiter
        .check_limit_memory(&rate_key, CLIENT_LOG_MAX_PER_MINUTE)
        .await
    {
        return rate_limited_response();
    }
    let kind = {
        let raw = sanitize_client_field(&request.kind, 32);
        if raw.is_empty() { "client_error".to_string() } else { raw }
    };
    let message = sanitize_client_field(&request.message, 1000);
    if message.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "message is required" }))).into_response();
    }
    let page = sanitize_client_field(&request.page, 200);
    let source = sanitize_client_field(&request.source, 300);
    let stack = sanitize_client_field(&request.stack, 2000);
    let ua = sanitize_client_field(&request.ua, 200);
    let visibility = sanitize_client_field(&request.visibility, 16);
    let level = match kind.as_str() {
        "boot_watchdog_reload" | "js_error" | "unhandled_rejection" => "ERROR",
        _ => "WARN",
    };
    let mut details = format!(
        "page={page} account={} ip={ip} standalone={} visibility={visibility} ua={ua}",
        session.account_id, request.standalone
    );
    if !source.is_empty() {
        details.push_str(&format!("\nsource={source}:{}:{}", request.line, request.col));
    }
    if !stack.is_empty() {
        details.push_str("\nstack=");
        details.push_str(&stack);
    }
    tracing::warn!(
        kind = %kind,
        account = %session.account_id,
        page = %page,
        message = %message,
        "client-side error report"
    );
    crate::logs::record_system(
        level,
        &format!("client:{kind}"),
        format!("[{}] {message}", session.account_id),
        Some(details),
    );
    (StatusCode::NO_CONTENT, ()).into_response()
}
