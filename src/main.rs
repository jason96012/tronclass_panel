#![allow(dead_code)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::derivable_impls)]
#![allow(clippy::field_reassign_with_default)]
#![allow(clippy::manual_clamp)]
#![allow(clippy::manual_is_multiple_of)]
#![allow(clippy::map_entry)]
#![allow(clippy::print_literal)]
#![allow(clippy::result_large_err)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::type_complexity)]
#![allow(clippy::upper_case_acronyms)]
#![allow(clippy::while_let_on_iterator)]
#![allow(clippy::wrong_self_convention)]

//! Tronclass Rollcall — 程式進入點
//!
//! 負責：
//! 1. 解析命令列參數（設定檔路徑）
//! 2. 初始化日誌（tracing）
//! 3. 載入並驗證設定（config.toml + accounts.db）
//! 4. 建立監控器上下文（每帳號各自登錄 + 共用 Line Bot）
//! 5. 啟動 Webhook 伺服器（若 Line Bot 啟用）
//! 6. 進入主監控循環
//! 7. 優雅關閉（捕捉 Ctrl+C / SIGTERM）
//!
//! Credits:
//! [KrsMt-0113/XMU-Rollcall-Bot](https://github.com/KrsMt-0113/XMU-Rollcall-Bot)

mod account;
mod adapters;
mod api;
mod auth;
mod class_reminders;
mod config;
mod dashboard_auth;
mod db;
mod detection_schedule;
mod homework_reminders;
mod logs;
mod monitor;
mod rollcalls;
mod runtime_settings;
mod web_push_notifications;

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use miette::Result;
use tracing::{error, info, warn};
use tracing_subscriber::{
    fmt::{self, format::Writer, time::FormatTime},
    prelude::*,
    EnvFilter,
};

// ─── 帳號管理子命令 ───────────────────────────────────────────────────────────

enum AccountCmd {
    List,
    Show {
        id: String,
    },
    Add {
        id: String,
        provider: String,
        username: String,
        password: String,
        line_user_id: String,
        discord_user_id: String,
        enabled: bool,
        is_admin: bool,
    },
    Bind {
        id: String,
        line_user_id: Option<String>,
        discord_user_id: Option<String>,
    },
    Remove {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    SetAdmin {
        id: String,
        is_admin: bool,
    },
    /// 預先為 Web Dashboard 設定獨立密碼（從 stdin 讀取）。
    SetDashboardPassword {
        id: String,
        password: String,
    },
}

impl AccountCmd {
    fn parse_from(args: &[String], i: &mut usize) -> Self {
        let subcmd = args.get(*i).map(String::as_str).unwrap_or("");
        match subcmd {
            "list" => Self::List,
            "show" => {
                *i += 1;
                let id = cli_require_positional(args, *i, "show", "<id>");
                Self::Show { id }
            }
            "add" => {
                *i += 1;
                let id = cli_require_positional(args, *i, "add", "<id>");
                let mut provider = String::new();
                let mut username = String::new();
                let mut password = String::new();
                let mut line_user_id = String::new();
                let mut discord_user_id = String::new();
                let mut enabled = true;
                let mut is_admin = false;
                *i += 1;
                while *i < args.len() {
                    match args[*i].as_str() {
                        "-p" | "--provider" => {
                            *i += 1;
                            provider = cli_require_value(args, *i, "--provider");
                        }
                        "-u" | "--username" => {
                            *i += 1;
                            username = cli_require_value(args, *i, "--username");
                        }
                        "-P" | "--password" => {
                            *i += 1;
                            password = cli_require_value(args, *i, "--password");
                        }
                        "--line-user-id" => {
                            *i += 1;
                            line_user_id = cli_require_value(args, *i, "--line-user-id");
                        }
                        "--discord-user-id" => {
                            *i += 1;
                            discord_user_id = cli_require_value(args, *i, "--discord-user-id");
                        }
                        "--disabled" => enabled = false,
                        "--admin" => is_admin = true,
                        other => {
                            eprintln!("未知引數：{other}");
                            std::process::exit(1);
                        }
                    }
                    *i += 1;
                }
                if provider.is_empty() {
                    eprintln!("錯誤：account add 需要 -p/--provider");
                    std::process::exit(1);
                }
                if username.is_empty() {
                    eprintln!("錯誤：account add 需要 -u/--username");
                    std::process::exit(1);
                }
                if password.is_empty() {
                    eprintln!("錯誤：account add 需要 -P/--password");
                    std::process::exit(1);
                }
                Self::Add {
                    id,
                    provider,
                    username,
                    password,
                    line_user_id,
                    discord_user_id,
                    enabled,
                    is_admin,
                }
            }
            "bind" => {
                *i += 1;
                let id = cli_require_positional(args, *i, "bind", "<id>");
                let mut line_user_id = None;
                let mut discord_user_id = None;
                *i += 1;
                while *i < args.len() {
                    match args[*i].as_str() {
                        "--line-user-id" => {
                            *i += 1;
                            line_user_id = Some(cli_require_value(args, *i, "--line-user-id"));
                        }
                        "--discord-user-id" => {
                            *i += 1;
                            discord_user_id =
                                Some(cli_require_value(args, *i, "--discord-user-id"));
                        }
                        other => {
                            eprintln!("未知引數：{other}");
                            std::process::exit(1);
                        }
                    }
                    *i += 1;
                }
                if line_user_id.is_none() && discord_user_id.is_none() {
                    eprintln!("錯誤：account bind 至少需要一個綁定參數");
                    std::process::exit(1);
                }
                Self::Bind {
                    id,
                    line_user_id,
                    discord_user_id,
                }
            }
            "dashboard-password" | "set-dashboard-password" => {
                *i += 1;
                let id = cli_require_positional(args, *i, "dashboard-password", "<id>");
                *i += 1;
                match args.get(*i).map(String::as_str) {
                    Some("--stdin") | Some("--password-stdin") => {
                        *i += 1;
                    }
                    _ => {
                        eprintln!(
                            "錯誤：account dashboard-password 必須使用 --stdin，避免將 Dashboard 密碼暴露在命令列歷史紀錄中"
                        );
                        std::process::exit(1);
                    }
                }
                if *i < args.len() {
                    eprintln!("錯誤：account dashboard-password 不接受額外引數");
                    std::process::exit(1);
                }
                let password = read_dashboard_password_from_stdin();
                Self::SetDashboardPassword { id, password }
            }
            "admin" => {
                *i += 1;
                let id = cli_require_positional(args, *i, "admin", "<id>");
                Self::SetAdmin { id, is_admin: true }
            }
            "unadmin" => {
                *i += 1;
                let id = cli_require_positional(args, *i, "unadmin", "<id>");
                Self::SetAdmin {
                    id,
                    is_admin: false,
                }
            }
            "remove" => {
                *i += 1;
                let id = cli_require_positional(args, *i, "remove", "<id>");
                Self::Remove { id }
            }
            "enable" => {
                *i += 1;
                let id = cli_require_positional(args, *i, "enable", "<id>");
                Self::Enable { id }
            }
            "disable" => {
                *i += 1;
                let id = cli_require_positional(args, *i, "disable", "<id>");
                Self::Disable { id }
            }
            "" => {
                eprintln!(
                    "錯誤：account 需要子命令\n用法：account <list|show|add|bind|admin|unadmin|dashboard-password|remove|enable|disable>"
                );
                std::process::exit(1);
            }
            other => {
                eprintln!("未知的 account 子命令：{other}");
                std::process::exit(1);
            }
        }
    }
}

fn cli_require_positional(args: &[String], i: usize, cmd: &str, name: &str) -> String {
    match args.get(i) {
        Some(v) if !v.starts_with('-') => v.clone(),
        _ => {
            eprintln!("錯誤：account {cmd} 需要 {name}");
            std::process::exit(1);
        }
    }
}

fn cli_require_value(args: &[String], i: usize, flag: &str) -> String {
    match args.get(i) {
        Some(v) => v.clone(),
        None => {
            eprintln!("錯誤：{flag} 需要一個值");
            std::process::exit(1);
        }
    }
}

/// Read a Dashboard password from a pipe or redirected stdin.  Requiring stdin
/// keeps it out of shell history and process argument listings.
fn read_dashboard_password_from_stdin() -> String {
    let mut password = String::new();
    if let Err(error) = std::io::stdin().read_to_string(&mut password) {
        eprintln!("錯誤：無法從 stdin 讀取 Dashboard 密碼：{error}");
        std::process::exit(1);
    }
    let password = password.trim_end_matches(&['\r', '\n'][..]).to_string();
    if password.is_empty() {
        eprintln!("錯誤：Dashboard 密碼不可為空");
        std::process::exit(1);
    }
    password
}

// ─── 命令列引數 ───────────────────────────────────────────────────────────────

/// CLI 子命令
enum Subcommand {
    /// 正常啟動（預設）
    Run { validate_only: bool },
    /// 初始化設定檔
    Init { force: bool },
    /// 帳號管理
    Account(AccountCmd),
}

/// 命令列引數結構
struct CliArgs {
    /// 設定檔路徑（預設：`./config/config.toml`）
    config_path: PathBuf,

    /// 帳號資料庫路徑（預設：`./config/accounts.db`）
    accounts_path: PathBuf,

    /// 是否印出版本資訊後退出
    show_version: bool,

    /// 是否印出說明後退出
    show_help: bool,

    /// 子命令
    subcommand: Subcommand,
}

impl CliArgs {
    fn parse() -> Self {
        let args: Vec<String> = std::env::args().collect();
        let mut config_path = PathBuf::from("config/config.toml");
        let mut accounts_path = PathBuf::from("config/accounts.db");
        let mut show_version = false;
        let mut show_help = false;
        let mut validate_only = false;
        let mut init_force = false;
        let mut subcommand_tag: Option<&'static str> = None;
        let mut account_cmd: Option<AccountCmd> = None;

        let mut i = 1usize;
        while i < args.len() {
            match args[i].as_str() {
                "-v" | "--version" => show_version = true,
                "-h" | "--help" => show_help = true,
                "--validate" => validate_only = true,
                "--force" | "-f" => init_force = true,
                "-c" | "--config" => {
                    i += 1;
                    if i < args.len() {
                        config_path = PathBuf::from(&args[i]);
                    } else {
                        eprintln!("錯誤：-c/--config 需要指定路徑");
                        std::process::exit(1);
                    }
                }
                "-a" | "--accounts" => {
                    i += 1;
                    if i < args.len() {
                        accounts_path = PathBuf::from(&args[i]);
                    } else {
                        eprintln!("錯誤：-a/--accounts 需要指定路徑");
                        std::process::exit(1);
                    }
                }
                "init" => subcommand_tag = Some("init"),
                "account" => {
                    subcommand_tag = Some("account");
                    i += 1;
                    account_cmd = Some(AccountCmd::parse_from(&args, &mut i));
                    break;
                }
                other if !other.starts_with('-') && subcommand_tag.is_none() => {
                    config_path = PathBuf::from(other);
                }
                other => {
                    eprintln!("未知引數：{other}");
                    std::process::exit(1);
                }
            }
            i += 1;
        }

        let subcommand = match subcommand_tag {
            Some("init") => Subcommand::Init { force: init_force },
            Some("account") => Subcommand::Account(account_cmd.unwrap()),
            _ => Subcommand::Run { validate_only },
        };

        Self {
            config_path,
            accounts_path,
            show_version,
            show_help,
            subcommand,
        }
    }
}

// ─── 版本資訊 ─────────────────────────────────────────────────────────────────

const VERSION: &str = env!("CARGO_PKG_VERSION");
const PKG_NAME: &str = env!("CARGO_PKG_NAME");
const DESCRIPTION: &str = env!("CARGO_PKG_DESCRIPTION");

fn print_version() {
    println!("{PKG_NAME} v{VERSION}");
    println!("{DESCRIPTION}");
}

fn print_help() {
    println!("{PKG_NAME} v{VERSION}");
    println!("{DESCRIPTION}");
    println!();
    println!("用法：");
    println!("  {PKG_NAME} [選項] [設定檔路徑]");
    println!("  {PKG_NAME} init [--force] [-c <PATH>]");
    println!("  {PKG_NAME} account <子命令> [-a <PATH>]");
    println!();
    println!("子命令：");
    println!("  init                       產生預設 config.toml");
    println!("    --force, -f              覆蓋已存在的檔案");
    println!();
    println!("  account list               列出所有帳號");
    println!("  account show <id>          顯示帳號詳情");
    println!("  account add <id>           新增帳號");
    println!("    -p, --provider <name>      Provider 名稱（必填）");
    println!("    -u, --username <user>      使用者名稱（必填）");
    println!("    -P, --password <pass>      Tronclass 上游密碼（首次網站登入後會強制改 Dashboard 密碼）");
    println!("    --line-user-id <uid>       Line User ID（選填）");
    println!("    --discord-user-id <id>     Discord User ID（選填）");
    println!("    --admin                    新增時授予 Web 管理頁管理員權限");
    println!("    --disabled                 新增時停用帳號");
    println!("  account bind <id>           更新帳號的 adapter 使用者綁定");
    println!("    --line-user-id <uid>       更新 Line User ID");
    println!("    --discord-user-id <id>     更新 Discord User ID");
    println!("  account admin <id>          授予 Web 管理頁管理員權限");
    println!("  account unadmin <id>        移除 Web 管理頁管理員權限");
    println!("  account dashboard-password <id> --stdin");
    println!("                             從 stdin 設定獨立 Dashboard 密碼");
    println!("  account remove <id>        刪除帳號");
    println!("  account enable <id>        啟用帳號");
    println!("  account disable <id>       停用帳號");
    println!();
    println!("選項：");
    println!("  -c, --config <PATH>    指定設定檔路徑（預設：config/config.toml）");
    println!("  -a, --accounts <PATH>  指定帳號資料庫路徑（預設：config/accounts.db）");
    println!("  --validate             只驗證設定檔，不啟動程式");
    println!("  -v, --version          顯示版本資訊");
    println!("  -h, --help             顯示此說明");
    println!();
    println!("環境變數：");
    println!("  TRONCLASS_ROLLCALL__PROVIDERS__<name>__BASE_URL  覆蓋設定中的 Base URL");
    println!("  RUST_LOG                                          控制日誌等級（info, debug…）");
}

// ─── 設定初始化 ───────────────────────────────────────────────────────────────

const CONFIG_EXAMPLE: &str = include_str!("../config/config.toml.example");

fn run_init(config_path: &PathBuf, force: bool) -> std::io::Result<()> {
    write_init_file(config_path, CONFIG_EXAMPLE, force)?;
    println!("💡 帳號資料庫將在首次啟動時自動建立（預設：config/accounts.db）");
    println!("💡 使用 `{PKG_NAME} account add` 新增帳號");
    Ok(())
}

fn write_init_file(path: &PathBuf, content: &str, force: bool) -> std::io::Result<()> {
    if path.exists() && !force {
        eprintln!("⚠️  已存在：{}（使用 --force 強制覆蓋）", path.display());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    println!("✅ 已建立：{}", path.display());
    Ok(())
}

// ─── 帳號管理指令執行 ─────────────────────────────────────────────────────────

async fn run_account_cmd(db: db::AccountsDb, cmd: AccountCmd) -> Result<()> {
    use account::RawAccountConfig;

    match cmd {
        AccountCmd::List => {
            let accounts = db.list().await?;
            if accounts.is_empty() {
                println!("（沒有帳號）");
            } else {
                println!(
                    "{:<20} {:<12} {:<30} {:<8} {:<7} {:<10} {:<34} {}",
                    "ID",
                    "Provider",
                    "Username",
                    "Enabled",
                    "Admin",
                    "Dashboard",
                    "Line User ID",
                    "Discord User ID"
                );
                println!("{}", "─".repeat(136));
                for a in &accounts {
                    println!(
                        "{:<20} {:<12} {:<30} {:<8} {:<7} {:<10} {:<34} {}",
                        a.id,
                        a.provider,
                        a.username,
                        if a.enabled { "✓" } else { "✗" },
                        if a.is_admin { "✓" } else { "" },
                        if a.dashboard_password_hash.is_empty() {
                            "首次登入"
                        } else {
                            "已設定"
                        },
                        a.line_user_id,
                        a.discord_user_id,
                    );
                }
                println!();
                println!("共 {} 個帳號", accounts.len());
            }
        }
        AccountCmd::Show { id } => match db.get(&id).await? {
            None => {
                eprintln!("找不到帳號：{id}");
                std::process::exit(1);
            }
            Some(a) => {
                println!("ID:           {}", a.id);
                println!("Provider:     {}", a.provider);
                println!("Username:     {}", a.username);
                println!("Password:     {}", "*".repeat(a.password.len().min(16)));
                println!(
                    "Dashboard password: {}",
                    if a.dashboard_password_hash.is_empty() {
                        "未設定（首次可用 Tronclass 密碼登入，且會強制建立新 Dashboard 密碼）"
                    } else {
                        "已設定"
                    }
                );
                println!("Enabled:      {}", if a.enabled { "是" } else { "否" });
                println!("Admin:        {}", if a.is_admin { "是" } else { "否" });
                println!(
                    "Line User ID: {}",
                    if a.line_user_id.is_empty() {
                        "(未設定)"
                    } else {
                        &a.line_user_id
                    }
                );
                println!(
                    "Discord User ID: {}",
                    if a.discord_user_id.is_empty() {
                        "(未設定)"
                    } else {
                        &a.discord_user_id
                    }
                );
            }
        },
        AccountCmd::Add {
            id,
            provider,
            username,
            password,
            line_user_id,
            discord_user_id,
            enabled,
            is_admin,
        } => {
            let account = RawAccountConfig {
                id: id.clone(),
                provider,
                username,
                password,
                enabled,
                line_user_id,
                discord_user_id,
                session_cookie: String::new(),
                is_admin,
                dashboard_password_hash: String::new(),
            };
            db.insert(&account).await?;
            println!("✅ 已新增帳號：{id}");
            println!(
                "ℹ️ Dashboard 密碼尚未設定；首次可用 Tronclass 密碼登入網站，且系統會強制立即設定新的 Dashboard 密碼。"
            );
        }
        AccountCmd::Bind {
            id,
            line_user_id,
            discord_user_id,
        } => {
            if let Some(value) = line_user_id {
                if !db.set_line_user_id(&id, &value).await? {
                    eprintln!("找不到帳號：{id}");
                    std::process::exit(1);
                }
            }
            if let Some(value) = discord_user_id {
                if !db.set_discord_user_id(&id, &value).await? {
                    eprintln!("找不到帳號：{id}");
                    std::process::exit(1);
                }
            }
            println!("✅ 已更新帳號綁定：{id}");
        }
        AccountCmd::Remove { id } => {
            if db.delete(&id).await? {
                println!("✅ 已刪除帳號：{id}");
            } else {
                eprintln!("找不到帳號：{id}");
                std::process::exit(1);
            }
        }
        AccountCmd::Enable { id } => {
            if db.set_enabled(&id, true).await? {
                println!("✅ 已啟用帳號：{id}");
            } else {
                eprintln!("找不到帳號：{id}");
                std::process::exit(1);
            }
        }
        AccountCmd::Disable { id } => {
            if db.set_enabled(&id, false).await? {
                println!("✅ 已停用帳號：{id}");
            } else {
                eprintln!("找不到帳號：{id}");
                std::process::exit(1);
            }
        }
        AccountCmd::SetAdmin { id, is_admin } => {
            if db.set_admin(&id, is_admin).await? {
                println!(
                    "✅ 已{}帳號管理員權限：{id}",
                    if is_admin { "授予" } else { "移除" }
                );
            } else {
                eprintln!("找不到帳號：{id}");
                std::process::exit(1);
            }
        }
        AccountCmd::SetDashboardPassword { id, password } => {
            let Some(account) = db.get(&id).await? else {
                eprintln!("找不到帳號：{id}");
                std::process::exit(1);
            };
            let password_hash =
                crate::dashboard_auth::hash_dashboard_password(&password, &account.password)?;
            if db.set_dashboard_password_hash(&id, &password_hash).await? {
                println!("✅ 已設定帳號 `{id}` 的獨立 Dashboard 密碼");
                println!("   Tronclass 密碼僅用於背景監控，不能再用於網站登入。");
            } else {
                eprintln!("找不到帳號：{id}");
                std::process::exit(1);
            }
        }
    }

    Ok(())
}

// ─── 日誌初始化 ───────────────────────────────────────────────────────────────

/// 初始化 tracing 日誌系統
///
/// 優先順序：
/// 1. `RUST_LOG` 環境變數
/// 2. `config.logging.level` 設定
/// 3. 預設 `info`
fn init_tracing(log_level: &str) {
    let filter = if std::env::var("RUST_LOG").is_ok() {
        EnvFilter::from_default_env()
    } else {
        EnvFilter::try_new(log_level).unwrap_or_else(|_| EnvFilter::new("info"))
    };

    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_timer(LocalTimestamp)
                .with_target(true)
                .with_thread_ids(false)
                .with_file(false)
                .with_line_number(false)
                .with_ansi(supports_ansi()),
        )
        .with(crate::logs::MemoryLogLayer::new())
        .with(filter)
        .init();
}

#[derive(Debug, Clone, Copy, Default)]
struct LocalTimestamp;

impl FormatTime for LocalTimestamp {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z"))
    }
}

fn apply_runtime_timezone(timezone: &str) -> Result<()> {
    let timezone = timezone.trim();
    if timezone.is_empty() {
        return Err(miette::miette!("time.timezone 不可為空"));
    }

    let use_system_timezone = timezone.eq_ignore_ascii_case("local");
    if use_system_timezone {
        std::env::remove_var("TZ");
    } else {
        std::env::set_var("TZ", timezone);
    }

    #[cfg(unix)]
    unsafe {
        tzset();
    }

    Ok(())
}

fn supports_ansi() -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        libc_isatty(std::io::stderr().as_raw_fd())
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(unix)]
fn libc_isatty(fd: i32) -> bool {
    extern "C" {
        fn isatty(fd: i32) -> i32;
    }
    unsafe { isatty(fd) != 0 }
}

#[cfg(unix)]
unsafe extern "C" {
    fn tzset();
    fn umask(mask: u32) -> u32;
}

/// Make every newly created local secret private by default. The accounts
/// database contains upstream credentials by design, and SQLite may create
/// -wal/-shm sidecars after a restart. A process-level umask keeps those new
/// files at 0600 (and newly created directories at 0700) even if the service
/// manager inherited a permissive default umask.
#[cfg(unix)]
fn enforce_private_process_umask() {
    // PRIVATE_FILE_UMASK_V1
    unsafe {
        umask(0o077);
    }
}

#[cfg(not(unix))]
fn enforce_private_process_umask() {
    // Windows permission handling is delegated to the service account / ACL.
}

// ─── 主函式 ───────────────────────────────────────────────────────────────────

/// Compile timestamp baked in at build time via the `TRONCLASS_BUILD_STAMP`
/// env var (set by ptero-start.sh); falls back to "unknown" for plain builds.
fn compile_time_stamp() -> &'static str {
    option_env!("TRONCLASS_BUILD_STAMP").unwrap_or("unknown")
}

#[tokio::main]
async fn main() -> Result<()> {
    // Build marker: proves at a glance whether the running binary actually
    // contains the current source. Printed before logging is configured so it
    // always reaches the Pterodactyl console.
    println!(
        "[BUILD] tronclass_rollcall AUTHFIX-14 compiled {} {}",
        env!("CARGO_PKG_VERSION"),
        compile_time_stamp()
    );

    enforce_private_process_umask();

    // ── 1. 解析命令列引數 ──────────────────────────────────────────────────────
    let args = CliArgs::parse();

    if args.show_version {
        print_version();
        return Ok(());
    }

    if args.show_help {
        print_help();
        return Ok(());
    }

    // ── 子命令分發 ─────────────────────────────────────────────────────────────
    let validate_only = match args.subcommand {
        Subcommand::Init { force } => {
            run_init(&args.config_path, force).map_err(|e| miette::miette!("init 失敗：{e}"))?;
            return Ok(());
        }
        // account 子命令不需要 config.toml
        Subcommand::Account(cmd) => {
            let db = db::AccountsDb::open(&args.accounts_path)
                .await
                .map_err(|e| miette::miette!("無法開啟帳號資料庫：{e}"))?;
            return run_account_cmd(db, cmd).await;
        }
        Subcommand::Run { validate_only } => validate_only,
    };

    eprintln!("🎓 {PKG_NAME} v{VERSION} 啟動中...");

    // ── 2. 載入設定 ────────────────────────────────────────────────────────────
    let config = match config::AppConfig::load(&args.config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            error!(
                error = %e,
                config_path = %args.config_path.display(),
                "設定檔載入失敗"
            );
            eprintln!("\n❌ 設定檔載入失敗：{e}");
            eprintln!("\n💡 提示：");
            eprintln!("  - 請確認設定檔路徑正確：{}", args.config_path.display());
            eprintln!("  - 執行 `{PKG_NAME} init` 產生預設設定檔");
            return Err(e);
        }
    };

    let accounts_db = match db::AccountsDb::open(&args.accounts_path).await {
        Ok(db) => {
            info!(
                accounts_path = %args.accounts_path.display(),
                "帳號資料庫開啟成功"
            );
            db
        }
        Err(e) => {
            error!(
                error = %e,
                accounts_path = %args.accounts_path.display(),
                "帳號資料庫開啟失敗"
            );
            eprintln!("\n❌ 帳號資料庫開啟失敗：{e}");
            eprintln!("  - 執行 `{PKG_NAME} account add` 新增帳號");
            return Err(e);
        }
    };

    if let Err(e) = logs::init_sqlite(&args.accounts_path).await {
        eprintln!("⚠️ 系統紀錄 SQLite 初始化失敗，暫時只使用記憶體紀錄：{e}");
    }

    // ── 3. 驗證設定 ────────────────────────────────────────────────────────────
    if let Err(e) = config.validate() {
        error!(error = %e, "設定驗證失敗");
        eprintln!("\n❌ 設定驗證失敗：{e}");
        return Err(e);
    }

    let accounts = match accounts_db.resolve(&config).await {
        Ok(accounts) => accounts,
        Err(e) => {
            error!(error = %e, "帳號設定驗證失敗");
            eprintln!("\n❌ 帳號設定驗證失敗：{e}");
            return Err(e);
        }
    };

    info!(account_count = accounts.len(), "設定驗證通過");

    if validate_only {
        println!("✅ 設定檔驗證通過：{}", args.config_path.display());
        println!("✅ 帳號資料庫驗證通過：{}", args.accounts_path.display());
        println!("啟用帳號數：{}", accounts.len());
        println!("{config}");
        return Ok(());
    }

    apply_runtime_timezone(&config.time.timezone)?;

    // config 載入後才初始化 tracing，確保使用設定檔中的等級
    init_tracing(&config.logging.level);
    logs::install_panic_hook();

    info!(
        version = VERSION,
        config = %args.config_path.display(),
        accounts_db = %args.accounts_path.display(),
        timezone = %config.time.timezone,
        log_level = %config.logging.level,
        "設定載入完成"
    );

    // GLOBAL_CONFIG_INIT_V1
    // `config::get_config()` 讀的是 config.rs 裡的 OnceCell 全域單例，但這裡是用
    // `AppConfig::load()` 載入的，不會自動填進去。少了這一行，
    // /admin/api/homework 呼叫 get_config() 時就會 panic，導致連線被中斷、
    // 反向代理回 502（作業頁「無法載入作業資料 HTTP 502」）。
    config::set_global_config(config.clone());

    adapters::discord::set_notification_preferences_db(accounts_db.clone());
    if let Err(error) = web_push_notifications::initialize(accounts_db.clone()).await {
        warn!(error = %error, "Web Push 初始化失敗；背景手機通知暫停使用");
    }

    let config = Arc::new(config);
    let line_bot = if config.adapters.line_bot.enabled {
        match adapters::line::client::LineBotClient::new(&config.adapters.line_bot) {
            Ok(bot) => Some(Arc::new(bot)),
            Err(e) => {
                warn!(error = %e, "Line Bot 初始化失敗，將在無 Line Bot 模式下運行");
                None
            }
        }
    } else {
        None
    };
    let discord_bot = if config.adapters.discord.enabled {
        match adapters::discord::DiscordBotClient::new(&config.adapters.discord) {
            Ok(bot) => Some(Arc::new(bot)),
            Err(e) => {
                warn!(error = %e, "Discord Bot 初始化失敗，將在無 Discord 模式下運行");
                None
            }
        }
    } else {
        None
    };

    // ── 4. 建立監控器上下文 ────────────────────────────────────────────────────
    info!(account_count = accounts.len(), "初始化監控器...");
    let mut adapter_messengers: Vec<Arc<dyn adapters::events::AdapterMessenger>> = vec![];
    if let Some(bot) = line_bot.as_ref() {
        adapter_messengers.push(Arc::clone(bot) as Arc<dyn adapters::events::AdapterMessenger>);
    }
    if let Some(bot) = discord_bot.as_ref() {
        adapter_messengers.push(Arc::clone(bot) as Arc<dyn adapters::events::AdapterMessenger>);
    }
    let adapter_hub = if adapter_messengers.is_empty() {
        None
    } else {
        Some(
            Arc::new(adapters::events::AdapterHub::new(adapter_messengers))
                as Arc<dyn adapters::events::AdapterMessenger>,
        )
    };
    // CLASS_REMINDER_SCHEDULER_START_V1
    // 只透過 Discord bot 發送「上課前提醒」DM；每位使用者可在 Web 通知頁設定分鐘數。
    if let Some(bot) = discord_bot.as_ref() {
        let class_reminder_messenger =
            Arc::clone(bot) as Arc<dyn adapters::events::AdapterMessenger>;
        class_reminders::start_class_reminder_scheduler(
            accounts_db.clone(),
            class_reminder_messenger,
        );
    }
    // CLASS_REMINDER_SCHEDULER_END_V1

    // HOMEWORK_REMINDER_SCHEDULER_START
    // 線上定時輪詢各帳號未繳交作業，於截止前 24h、6h、1h 發送倒數提醒
    homework_reminders::start_homework_reminder_scheduler(
        accounts_db.clone(),
        Arc::clone(&config),
        adapter_hub.as_ref().map(Arc::clone),
    );
    // HOMEWORK_REMINDER_SCHEDULER_END

    let interactive_adapter = adapter_hub.is_some() || config.adapters.admin_web.enabled;
    let scanner_registry = if interactive_adapter {
        let line_public_base_url = config.adapters.line_bot.public_base_url.trim();
        let discord_public_base_url = config.adapters.discord.public_base_url.trim();
        let public_base_url = if !line_public_base_url.is_empty() {
            line_public_base_url
        } else {
            discord_public_base_url
        };
        if public_base_url.is_empty() {
            warn!("scanner public_base_url 未設定，QR Code 將沿用 legacy scanner URL");
            None
        } else {
            Some(Arc::new(adapters::scanner::QrScannerRegistry::new(
                public_base_url.to_string(),
            )))
        }
    } else {
        None
    };

    let mut contexts = Vec::with_capacity(accounts.len());
    for account in accounts {
        let account = Arc::new(account);
        let ctx = match monitor::MonitorContext::new(
            Arc::clone(&config),
            Arc::clone(&account),
            accounts_db.clone(),
            adapter_hub.as_ref().map(Arc::clone),
            interactive_adapter,
            scanner_registry.as_ref().map(Arc::clone),
        )
        .await
        {
            Ok(ctx) => ctx,
            Err(e) => {
                error!(account = %account.id, error = %e, "監控器初始化失敗");
                eprintln!("\n❌ 帳號 `{}` 初始化失敗：{e}", account.id);
                return Err(e);
            }
        };
        contexts.push(ctx);
    }

    // 任一帳號被使用者手動 /force 並偵測到點名時，通知其他帳號立即一起檢查。
    let force_cascade_peers = contexts
        .iter()
        .filter_map(|ctx| {
            ctx.request_account
                .as_ref()
                .map(|request| (ctx.account.id.clone(), Arc::clone(&request.force_poll_tx)))
        })
        .collect::<Vec<_>>();
    let force_cascade = Arc::new(monitor::ForceCascade::new(force_cascade_peers));
    for ctx in &mut contexts {
        ctx.force_cascade = Some(Arc::clone(&force_cascade));
    }

    // ── 5. 啟動 Webhook 伺服器（背景任務） ───────────────────────────────────
    let webhook_accounts = contexts
        .iter()
        .filter_map(|ctx| ctx.request_account.clone())
        .collect::<Vec<_>>();

    let admin_web_state = if config.adapters.admin_web.enabled {
        let default_poll_interval_secs = config
            .providers
            .values()
            .map(|provider| provider.api.poll_interval_secs)
            .min()
            .unwrap_or(10);
        Some(adapters::admin_web::AdminWebState::new(
            config.adapters.admin_web.access_token.clone(),
            webhook_accounts.clone(),
            accounts_db.clone(),
            scanner_registry.as_ref().map(Arc::clone),
            adapter_hub.as_ref().map(Arc::clone),
            default_poll_interval_secs,
        ))
    } else {
        None
    };

    let webhook_handle = if line_bot.is_some() {
        if let Some(bot) = line_bot.as_ref() {
            let request_state = adapters::requests::RequestState::new(
                Arc::clone(bot) as Arc<dyn adapters::events::AdapterMessenger>,
                webhook_accounts.clone(),
            );
            let webhook_state = adapters::line::webhook::LineWebhookState::new(
                Arc::clone(bot),
                request_state,
                scanner_registry.as_ref().map(Arc::clone),
            )
            .with_admin_web(admin_web_state.clone());
            let port = config.adapters.line_bot.webhook_port;
            let bind_addr = config.adapters.line_bot.webhook_bind_addr.clone();
            let webhook_path = config.adapters.line_bot.webhook_path.clone();

            info!(bind_addr = %bind_addr, port = port, path = %webhook_path, "啟動 Line Bot Webhook 伺服器...");

            let handle = tokio::spawn(async move {
                if let Err(e) = adapters::line::webhook::start_webhook_server(
                    webhook_state,
                    &bind_addr,
                    port,
                    &webhook_path,
                )
                .await
                {
                    error!(error = %e, "Webhook 伺服器異常退出");
                }
            });

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            info!(port = port, "✅ Webhook 伺服器已在 port {} 啟動", port);

            Some(handle)
        } else {
            warn!("Line Bot 啟用但客戶端未初始化，跳過 Webhook 伺服器");
            None
        }
    } else {
        info!("Line Bot 未啟用，跳過 Webhook 伺服器");
        None
    };

    let scanner_handle = if line_bot.is_none() {
        if let Some(scanner) = scanner_registry.as_ref() {
            let scanner = Arc::clone(scanner);
            let port = config.adapters.line_bot.webhook_port;
            let bind_addr = config.adapters.line_bot.webhook_bind_addr.clone();
            info!(bind_addr = %bind_addr, port = port, "啟動共用 QR scanner 伺服器...");
            let admin_web_state = admin_web_state.clone();
            Some(tokio::spawn(async move {
                if let Err(e) = adapters::scanner::start_scanner_server_with_admin(
                    scanner,
                    &bind_addr,
                    port,
                    admin_web_state,
                )
                .await
                {
                    error!(error = %e, "QR scanner 伺服器異常退出");
                }
            }))
        } else {
            None
        }
    } else {
        None
    };

    let discord_handle = if let Some(bot) = discord_bot.as_ref() {
        let request_state = adapters::requests::RequestState::new_with_binding(
            Arc::clone(bot) as Arc<dyn adapters::events::AdapterMessenger>,
            webhook_accounts.clone(),
            adapters::requests::AdapterBindingKind::Discord,
        );
        let runtime = adapters::discord::DiscordRuntime::new(
            Arc::clone(bot),
            request_state,
            accounts_db.clone(),
            &config.adapters.discord,
        );

        info!("啟動 Discord Bot...");
        Some(tokio::spawn(async move {
            if let Err(e) = adapters::discord::start_discord_bot(runtime).await {
                error!(error = %e, "Discord Bot 異常退出");
            }
        }))
    } else {
        None
    };

    // ── 6. 設定優雅關閉 ───────────────────────────────────────────────────────
    let shutdown_notify = Arc::new(tokio::sync::Notify::new());
    let shutdown_notify_clone = Arc::clone(&shutdown_notify);

    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        info!("收到關閉信號，準備優雅關閉...");
        shutdown_notify_clone.notify_one();
    });

    // ── 7. 進入主監控循環（帶優雅關閉） ──────────────────────────────────────
    info!(account_count = contexts.len(), "🚀 開始監控簽到...");

    let mut monitor_tasks = tokio::task::JoinSet::new();
    for ctx in contexts {
        let account_id = ctx.account.id.clone();
        monitor_tasks.spawn(async move {
            match monitor::run_monitor_loop(ctx).await {
                Ok(()) => Ok(account_id),
                Err(e) => Err((account_id, e.to_string())),
            }
        });
    }

    let monitor_result = tokio::select! {
        joined = monitor_tasks.join_next() => {
            Some(joined)
        }
        _ = shutdown_notify.notified() => {
            info!("收到關閉通知，開始優雅關閉");
            None
        }
    };

    if let Some(joined) = monitor_result {
        match joined {
            Some(Ok(Ok(account_id))) => {
                warn!(account = %account_id, "監控循環提前正常結束");
            }
            Some(Ok(Err((account_id, err)))) => {
                error!(account = %account_id, error = %err, "監控循環異常結束");
                monitor_tasks.abort_all();
                return Err(miette::miette!(
                    "account `{}` monitor failed: {}",
                    account_id,
                    err
                ));
            }
            Some(Err(join_err)) => {
                monitor_tasks.abort_all();
                return Err(miette::miette!("monitor task join failed: {}", join_err));
            }
            None => {}
        }
    }

    // ── 8. 清理資源 ───────────────────────────────────────────────────────────
    info!("清理資源...");

    monitor_tasks.abort_all();

    if let Some(handle) = webhook_handle {
        handle.abort();
        info!("Webhook 伺服器已關閉");
    }

    if let Some(handle) = scanner_handle {
        handle.abort();
        info!("QR scanner 伺服器已關閉");
    }

    if let Some(handle) = discord_handle {
        handle.abort();
        info!("Discord Bot 已關閉");
    }

    info!("👋 {} 已關閉，再見！", PKG_NAME);

    Ok(())
}

// ─── 關閉信號 ─────────────────────────────────────────────────────────────────

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut sigterm =
            signal(SignalKind::terminate()).expect("Failed to register SIGTERM handler");
        let mut sigint =
            signal(SignalKind::interrupt()).expect("Failed to register SIGINT handler");

        tokio::select! {
            _ = sigterm.recv() => {
                info!("收到 SIGTERM 信號");
            }
            _ = sigint.recv() => {
                info!("收到 SIGINT (Ctrl+C) 信號");
            }
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to register Ctrl+C handler");
        info!("收到 Ctrl+C 信號");
    }
}
