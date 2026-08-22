//! Health check page/API handlers for Admin Web.

use super::*;

pub(super) async fn admin_health_page_handler() -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(super::pages::inject_theme(ADMIN_HEALTH_HTML)),
    )
        .into_response()
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
    for account in state.accounts.iter() {
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
                "total": state.accounts.len(),
                "running": running_accounts,
                "paused": state.accounts.len().saturating_sub(running_accounts),
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
            }
        })),
    )
        .into_response()
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
    for account in state.accounts.iter() {
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
    let total = state.accounts.len();
    let accounts_ok = total > 0 && running > 0;
    checks.push(json!({
        "name": "監控帳號",
        "ok": accounts_ok,
        "level": if accounts_ok { if running < total || failures > 0 { "warn" } else { "ok" } } else { "bad" },
        "message": format!("運行 {running}/{total}，偵測 {detected}，失敗加總 {failures}"),
        "details": json!({ "latest_poll_timestamp": latest_poll }),
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

fn cloudflare_connector_status() -> serde_json::Value {
    let log = std::fs::read_to_string("logs/cloudflared.log").unwrap_or_default();
    let recent: String = log.lines().rev().take(80).collect::<Vec<_>>().join("\n");
    let error = regex::Regex::new("(?i)connection refused|no connection|failed to serve|unable to reach|context deadline exceeded|connection lost|quic connection closed|serve connection error").ok().map(|r| r.is_match(&recent)).unwrap_or(false);
    json!({ "pid_alive": std::path::Path::new("logs/cloudflared.pid").exists(), "recent_error": error, "log_file": "logs/cloudflared.log" })
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
        let alive = pid.map(|p| unsafe { libc_kill_zero(p) }).unwrap_or(false);
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
