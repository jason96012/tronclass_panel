//! Discord adapter powered by serenity.

use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    hash::{Hash, Hasher},
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use miette::{IntoDiagnostic, Result, WrapErr};
use once_cell::sync::{Lazy, OnceCell};
use serenity::all::{
    ButtonStyle, ChannelId, Colour, Command, CommandDataOptionValue, CommandInteraction,
    CommandOptionType, ComponentInteraction, Context, CreateActionRow, CreateButton,
    CreateCommand, CreateCommandOption, CreateEmbed, CreateEmbedFooter,
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage, EventHandler,
    GatewayIntents, GuildId, Http, Interaction, InteractionId, Message, Ready, UserId,
};
use serenity::Client;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::account::RawAccountConfig;
use crate::adapters::events::{
    AdapterAccountTarget, AdapterMessenger, MonitorStatus, OutboundMessage, SecurityAlertEvent,
    StatusMessage,
};
use crate::adapters::requests::{
    self, AdapterRequest, RequestCommand, RequestContent, RequestState,
};
use crate::config::DiscordBotConfig;
use crate::db::{AccountsDb, UsernameUpdateResult};

const REPLY_INTERACTION_PREFIX: &str = "discord_interaction:";
const COMPONENT_STATUS: &str = "trc:status";
const COMPONENT_FORCE: &str = "trc:force";
const COMPONENT_REAUTH: &str = "trc:reauth";
const COMPONENT_HELP: &str = "trc:help";
const COMPONENT_NUMBER_AUTO_PREFIX: &str = "trc:number-auto:";
/// NUMBER_AUTO_PICK_V1：多個點名同時等待時，選單上每個點名的按鈕（格式 `trc:number-pick:<帳號>:<點名ID>`）。
const COMPONENT_NUMBER_PICK_PREFIX: &str = "trc:number-pick:";
const COMPONENT_ACCOUNT_APPROVE_PREFIX: &str = "trc:account-approve:";
const COMPONENT_BLOCK_IP_PREFIX: &str = "trc:block-ip:";
const COMPONENT_UNBLOCK_IP_PREFIX: &str = "trc:unblock-ip:";
/// WAF_AUTO_BLOCK_NOTIFY_V2：WAF 自動封鎖的解除按鈕；值是黑名單鍵（IPv4 或 IPv6 /64）。
const COMPONENT_UNBLOCK_WAF_PREFIX: &str = "trc:unblock-waf:";
const COMPONENT_UNBLOCK_OWN_IP_PREFIX: &str = "trc:unblock-own-ip:";
/// LOGIN_TIERED_LOCKOUT_V1：帳號持有人取消第一層登入鎖定 / 直接封鎖來源 IP。
/// payload：`{account_id}|{ip}`；account_id 只做顯示與鍵值，授權以資料庫的 Discord 綁定為準。
const COMPONENT_CANCEL_LOCK_PREFIX: &str = "trc:cancel-lock:";
const COMPONENT_OWNER_BLOCK_IP_PREFIX: &str = "trc:owner-block-ip:";
/// ADMIN_GEO_RESTRICT_V1：payload 是 `geo_access_requests.id`（UUID）。
const COMPONENT_GEO_ALLOW_PREFIX: &str = "trc:geo-allow:";
const COMPONENT_GEO_IGNORE_PREFIX: &str = "trc:geo-ignore:";
/// WAF 指紋封鎖的放行按鈕：後面接 `waf_fingerprint_blocks.pair`。
const COMPONENT_FP_RELEASE_PREFIX: &str = "trc:fp-release:";
const DISCORD_SEND_INTERVAL_MS: u64 = 350;
const DISCORD_DASHBOARD_DEDUPE_TTL_SECS: u64 = 10 * 60;
const MAX_DISCORD_DM_CHANNEL_CACHE: usize = 2048;
const MAX_DISCORD_DASHBOARD_DEDUPE: usize = 4096;
const MAX_PENDING_ACCOUNT_REQUESTS: usize = 256;
const MAX_DISCORD_PROVIDER_BYTES: usize = 64;
const MAX_DISCORD_USERNAME_BYTES: usize = 128;
const MAX_DISCORD_PASSWORD_BYTES: usize = 1024;
const MAX_DISCORD_IDENTIFIER_BYTES: usize = 256;

static DISCORD_NOTIFICATION_PREFS_DB: OnceCell<AccountsDb> = OnceCell::new();

pub fn set_notification_preferences_db(db: AccountsDb) {
    let _ = DISCORD_NOTIFICATION_PREFS_DB.set(db);
}

fn discord_notification_key(message: &OutboundMessage) -> &'static str {
    match message {
        OutboundMessage::SystemStarted(_) => "system_started",
        OutboundMessage::RollcallDetected(_) => "rollcall_detected",
        OutboundMessage::NumberCodeRequested(_) => "number_code",
        OutboundMessage::QrCodeRequested(_) => "qr_code",
        OutboundMessage::RollcallFinished(_) => "rollcall_finished",
        OutboundMessage::HomeworkDeadline(_) => "homework_deadline",
        OutboundMessage::Bulletin(_) => "bulletin",
        // PUSH_STALE_CLEANUP_V1：歸到「其他通知」，使用者可在通知頁一併關閉。
        OutboundMessage::PushSubscriptionExpired(_) => "other",
        // ROLLCALL_INPUT_REMINDER_V1：跟著原本的數字／QR 通知開關走，關掉就不催。
        OutboundMessage::RollcallInputReminder(event) => {
            if event.is_number() {
                "number_code"
            } else {
                "qr_code"
            }
        }
        OutboundMessage::Status(_) => "status",
        OutboundMessage::SecurityAlert(_) => "security_alert",
        // DISCORD_NOTIFY_UNIFY_V1：新結構化事件的偏好鍵（沿用既有分類，不新增開關）。
        OutboundMessage::MonitorEvent(_) => "other",
        // DC_CHANNEL_PREFS_V1：監控警示獨立成鍵，管理員可在通知設定頁單獨關閉。
        OutboundMessage::AdminAlert(_) => "monitor_alert",
        OutboundMessage::ClassReminder(_) => "other",
        OutboundMessage::TimetableChanged(_) => "other",
        OutboundMessage::MonitorPaused
        | OutboundMessage::MonitorResumed
        | OutboundMessage::ForcePollTriggered
        | OutboundMessage::ReauthTriggered => "control",
        _ => "other",
    }
}

async fn discord_notification_enabled(account_id: &str, key: &str) -> bool {
    let Some(db) = DISCORD_NOTIFICATION_PREFS_DB.get() else {
        return true;
    };
    match db
        .notification_preference_enabled(account_id, "discord", key)
        .await
    {
        Ok(enabled) => enabled,
        Err(e) => {
            warn!(account = %account_id, key = %key, error = %e, "讀取 Discord 通知設定失敗，預設允許通知");
            true
        }
    }
}

static DISCORD_HTTP_THROTTLE: Lazy<Mutex<Instant>> = Lazy::new(|| Mutex::new(Instant::now()));
static DISCORD_DASHBOARD_DEDUPE: Lazy<Mutex<HashMap<String, Instant>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
const COMPONENT_ACCOUNT_REJECT_PREFIX: &str = "trc:account-reject:";

#[derive(Clone)]
pub struct DiscordBotClient {
    http: Arc<Http>,
    bot_token: String,
    admin_user_id: String,
    admin_channel_id: String,
    public_base_url: String,
    dm_channels: Arc<Mutex<HashMap<String, ChannelId>>>,
}

impl DiscordBotClient {
    pub fn new(config: &DiscordBotConfig) -> Result<Self> {
        let http = Arc::new(Http::new(&config.bot_token));
        Ok(Self {
            http,
            bot_token: config.bot_token.clone(),
            admin_user_id: config.admin_user_id.clone(),
            admin_channel_id: config.admin_channel_id.clone(),
            public_base_url: config.public_base_url.clone(),
            dm_channels: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn admin_channel_id(&self) -> Option<ChannelId> {
        parse_discord_id(&self.admin_channel_id).map(ChannelId::new)
    }

    fn admin_url(&self) -> Option<String> {
        let base = self.public_base_url.trim().trim_end_matches('/');
        if base.is_empty() {
            None
        } else {
            Some(format!("{base}/admin"))
        }
    }

    async fn send_text_to_user(&self, user_id: &str, message: &OutboundMessage) -> Result<()> {
        let user_id_raw = user_id.trim();
        let user_id_num = parse_discord_id(user_id_raw)
            .ok_or_else(|| miette::miette!("Discord user id 無效：{user_id}"))?;
        let cache_key = user_id_num.to_string();

        let cached_channel_id = {
            let cache = self.dm_channels.lock().await;
            cache.get(&cache_key).cloned()
        };
        let channel_id = if let Some(channel_id) = cached_channel_id {
            channel_id
        } else {
            throttle_discord_http().await;
            let channel = UserId::new(user_id_num)
                .create_dm_channel(&self.http)
                .await
                .into_diagnostic()
                .wrap_err("建立 Discord DM channel 失敗")?;
            let mut cache = self.dm_channels.lock().await;
            if cache.len() >= MAX_DISCORD_DM_CHANNEL_CACHE && !cache.contains_key(&cache_key) {
                if let Some(old_key) = cache.keys().next().cloned() {
                    cache.remove(&old_key);
                }
            }
            cache.insert(cache_key, channel.id);
            channel.id
        };

        send_outbound_to_channel(&self.http, channel_id, message).await
    }

    async fn send_dashboard_copy(&self, message: &OutboundMessage) -> Result<()> {
        let Some(channel_id) = self.admin_channel_id() else {
            return Ok(());
        };
        if !should_send_dashboard_copy(message).await {
            debug!("Discord dashboard duplicate notification suppressed");
            return Ok(());
        }
        send_outbound_to_channel(&self.http, channel_id, message).await
    }
}

#[async_trait]
impl AdapterMessenger for DiscordBotClient {
    fn adapter_name(&self) -> &'static str {
        "discord"
    }

    fn admin_user_id(&self) -> &str {
        &self.admin_user_id
    }

    async fn reply(&self, reply_token: &str, message: &OutboundMessage) -> Result<()> {
        if let Some((id, token)) = parse_interaction_reply(reply_token) {
            // DISCORD_NOTIFY_UNIFY_V1：所有出口統一走 render_discord_payload。
            let payload = render_discord_payload(message);
            // DISCORD_INTERACTION_EPHEMERAL_V1：slash／按鈕回覆只給操作者看，
            // 避免群組頻道洩漏監控狀態或刷版。主動推播仍走 DM／管理頻道。
            let mut builder = CreateInteractionResponseMessage::new()
                .content(payload.content)
                .ephemeral(true)
                .components(action_rows_for_message(message));
            if let Some(embed) = payload.embed {
                builder = builder.embed(embed);
            }
            let response = CreateInteractionResponse::Message(builder);
            self.http
                .create_interaction_response(InteractionId::new(id), token, &response, Vec::new())
                .await
                .into_diagnostic()
                .wrap_err("回覆 Discord interaction 失敗")?;
            return Ok(());
        }

        let channel_id = parse_discord_id(reply_token)
            .ok_or_else(|| miette::miette!("Discord reply target 無效：{reply_token}"))?;
        send_outbound_to_channel(&self.http, ChannelId::new(channel_id), message).await
    }

    async fn push(&self, to: &str, message: &OutboundMessage) -> Result<()> {
        self.send_text_to_user(to, message).await
    }

    async fn push_to_account_or_admin(
        &self,
        target: &AdapterAccountTarget,
        message: &OutboundMessage,
    ) -> Result<()> {
        let notification_key = discord_notification_key(message);
        if !discord_notification_enabled(&target.account_id, notification_key).await {
            debug!(account = %target.account_id, key = %notification_key, "Discord notification disabled by user preference");
            return Ok(());
        }

        if !target.discord_user_id.trim().is_empty() {
            if let Err(e) = self
                .send_text_to_user(&target.discord_user_id, message)
                .await
            {
                warn!(
                    account = %target.account_id,
                    user_id = %target.discord_user_id,
                    error = %e,
                    "Discord 帳號 DM 通知失敗"
                );
            }
        } else if !self.admin_user_id.trim().is_empty() {
            if let Err(e) = self.send_text_to_user(&self.admin_user_id, message).await {
                warn!(error = %e, "Discord admin DM fallback 失敗");
            }
        }

        self.send_dashboard_copy(message).await
    }
}

impl std::fmt::Debug for DiscordBotClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordBotClient")
            .field("bot_token", &"<redacted>")
            .field("admin_user_id", &self.admin_user_id)
            .field("admin_channel_id", &self.admin_channel_id)
            .finish()
    }
}

#[derive(Clone)]
pub struct DiscordRuntime {
    bot: Arc<DiscordBotClient>,
    requests: RequestState,
    accounts_db: AccountsDb,
    register_commands: bool,
    guild_ids: Vec<String>,
    pending_accounts: Arc<Mutex<HashMap<String, PendingAccountRequest>>>,
    // ACCOUNT_HOT_ADD_V1：新增／核准帳號後在背景熱啟動監控循環；None 時退回「請重啟服務」。
    monitor_spawner: Option<crate::monitor::MonitorSpawner>,
}

#[derive(Clone)]
struct PendingAccountRequest {
    provider: String,
    username: String,
    password: String,
    discord_user_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CreatedAccount {
    provider: String,
    username: String,
    account_id: String,
    discord_user_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccountReviewAction {
    Approve,
    Reject,
}

impl DiscordRuntime {
    pub fn new(
        bot: Arc<DiscordBotClient>,
        requests: RequestState,
        accounts_db: AccountsDb,
        config: &DiscordBotConfig,
    ) -> Self {
        Self {
            bot,
            requests,
            accounts_db,
            register_commands: config.register_commands,
            guild_ids: config.guild_ids.clone(),
            pending_accounts: Arc::new(Mutex::new(HashMap::new())),
            monitor_spawner: None,
        }
    }

    /// ACCOUNT_HOT_ADD_V1：掛上監控 spawner，讓 Discord 新增帳號後不必重啟即可開始監控。
    pub fn with_monitor_spawner(mut self, spawner: crate::monitor::MonitorSpawner) -> Self {
        self.monitor_spawner = Some(spawner);
        self
    }

    /// ACCOUNT_HOT_ADD_V1：在背景熱啟動剛寫入 DB 的帳號，並回傳要附在回覆裡的狀態說明。
    ///
    /// Discord interaction 必須在 3 秒內回覆，而登入學校系統可能要數十秒，
    /// 所以這裡只負責開背景任務；結果透過 DM 通知管理員與被綁定的使用者。
    ///
    /// ACCOUNT_HOT_ADD_FEEDBACK_V1：結果除了 DM 管理員，也會回貼到下指令／按核准的那個頻道
    /// （`origin_channel`），沒有的話退回 admin_channel；失敗訊息會 @管理員，確保一定看得到。
    fn start_monitor_in_background(
        &self,
        account: &CreatedAccount,
        origin_channel: Option<ChannelId>,
    ) -> String {
        let Some(spawner) = self.monitor_spawner.clone() else {
            return "新帳號會在服務重新啟動後開始監控。".to_string();
        };

        let bot = Arc::clone(&self.bot);
        let account_id = account.account_id.clone();
        let username = account.username.clone();
        let bound_user_id = account.discord_user_id.clone();
        let report_channel = origin_channel.or_else(|| bot.admin_channel_id());
        tokio::spawn(async move {
            let announce = |text: String| {
                let bot = Arc::clone(&bot);
                async move {
                    let message = OutboundMessage::Text(text);
                    if !bot.admin_user_id.is_empty() {
                        let _ = bot.push(&bot.admin_user_id, &message).await;
                    }
                    if let Some(channel_id) = report_channel {
                        if let Err(e) =
                            send_outbound_to_channel(&bot.http, channel_id, &message).await
                        {
                            warn!(error = %e, "回貼熱啟動結果到 Discord 頻道失敗");
                        }
                    }
                }
            };
            match spawner.spawn_account(&account_id).await {
                Ok(started) => {
                    info!(account = %started.account_id, "Discord 新增的帳號已熱啟動監控");
                    let who = if started.user_name.trim().is_empty() {
                        String::new()
                    } else {
                        format!("（{}）", started.user_name.trim())
                    };
                    announce(format!(
                        "帳號 `{account_id}`{who} 登入成功，已開始監控，不需重啟服務。"
                    ))
                    .await;
                    if !bound_user_id.is_empty() && bound_user_id != bot.admin_user_id {
                        let _ = bot
                            .push(
                                &bound_user_id,
                                &OutboundMessage::Text(format!(
                                    "你的 Tronclass username `{username}` 已登入成功並開始監控。"
                                )),
                            )
                            .await;
                    }
                }
                Err(reason) => {
                    warn!(account = %account_id, reason = %reason, "Discord 新增的帳號熱啟動監控失敗");
                    let mention = if bot.admin_user_id.trim().is_empty() {
                        String::new()
                    } else {
                        format!("<@{}> ", bot.admin_user_id.trim())
                    };
                    announce(format!(
                        "{mention}帳號 `{account_id}` 已寫入資料庫，但監控【沒有】啟動：{reason}\n\
                         目前這個帳號不會被點名偵測。請確認帳號密碼後，到 Web 管理頁「帳號」→「未在監控的帳號」按「啟動監控」重試，或重啟服務。"
                    ))
                    .await;
                    if !bound_user_id.is_empty() && bound_user_id != bot.admin_user_id {
                        let _ = bot
                            .push(
                                &bound_user_id,
                                &OutboundMessage::Text(format!(
                                    "你的 Tronclass username `{username}` 已建立，但目前無法登入：{reason}\n請聯絡管理員確認密碼。"
                                )),
                            )
                            .await;
                    }
                }
            }
        });

        "正在背景登入並啟動監控（不需重啟服務），完成後會私訊通知結果。".to_string()
    }
}

pub async fn start_discord_bot(runtime: DiscordRuntime) -> Result<()> {
    let intents =
        GatewayIntents::GUILDS | GatewayIntents::DIRECT_MESSAGES | GatewayIntents::GUILD_MESSAGES;
    let token = runtime.bot.bot_token.clone();
    let mut client = Client::builder(&token, intents)
        .event_handler(runtime)
        .await
        .into_diagnostic()
        .wrap_err("建立 Discord client 失敗")?;

    client
        .start()
        .await
        .into_diagnostic()
        .wrap_err("Discord client 已停止")
}

#[async_trait]
impl EventHandler for DiscordRuntime {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!(user = %ready.user.name, "Discord bot 已連線");
        if !self.register_commands {
            return;
        }

        if let Err(e) = register_commands(&ctx, &self.guild_ids).await {
            error!(error = %e, "註冊 Discord slash commands 失敗");
        }
    }

    async fn message(&self, _ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }
        if msg.content.trim().is_empty() {
            return;
        }

        let user_id = msg.author.id.to_string();
        let is_dm = msg.guild_id.is_none();
        let is_admin_channel = self
            .bot
            .admin_channel_id()
            .is_some_and(|channel_id| channel_id == msg.channel_id);

        if !is_dm && !is_admin_channel {
            debug!("忽略非 DM / 非管理 channel 的 Discord 文字訊息");
            return;
        }

        if is_admin_channel && user_id != self.bot.admin_user_id {
            let _ = msg
                .channel_id
                .say(
                    &self.bot.http,
                    render_discord_message(&OutboundMessage::NotAuthorized),
                )
                .await;
            return;
        }

        // 若 /admin slash command 尚未同步到 Discord，使用者手打 /admin 會變成一般文字。
        // 在這裡直接支援文字版 /admin，避免被共用 requests parser 當成未知指令。
        let text = msg.content.trim();
        if text.eq_ignore_ascii_case("/admin") || text.eq_ignore_ascii_case("admin") {
            let target = format!("channel={}", msg.channel_id);
            crate::logs::record_audit(
                "discord",
                &user_id,
                "text_admin",
                None,
                Some(target.as_str()),
                "Discord 文字指令：/admin",
            );
            let message = match self.bot.admin_url() {
                Some(url) => OutboundMessage::Text(format!("Web 管理頁：{url}")),
                None => OutboundMessage::Text(
                    "尚未設定 adapters.discord.public_base_url，無法產生 /admin 網址。".to_string(),
                ),
            };
            if let Err(e) = send_outbound_to_channel(&self.bot.http, msg.channel_id, &message).await
            {
                error!(error = %e, "發送 Discord admin URL 失敗");
            }
            return;
        }

        if let Some(action) = discord_text_audit_action(&msg.content) {
            let target = format!("channel={}", msg.channel_id);
            crate::logs::record_audit(
                "discord",
                &user_id,
                action,
                None,
                Some(target.as_str()),
                "Discord 文字指令",
            );
        }

        let request = AdapterRequest::new(
            user_id,
            Some(msg.channel_id.to_string()),
            true,
            RequestContent::Text(msg.content.clone()),
        );

        if let Err(e) = requests::handle_request(request, &self.requests).await {
            error!(error = %e, "處理 Discord 文字請求失敗");
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let result = match interaction {
            Interaction::Command(command) => self.handle_command(&ctx, command).await,
            Interaction::Component(component) => self.handle_component(&ctx, component).await,
            _ => Ok(()),
        };

        if let Err(e) = result {
            error!(error = %e, "處理 Discord interaction 失敗");
        }
    }
}

impl DiscordRuntime {
    /// GEO_ACCESS_ALL_ADMINS_V1：「暫時放行／忽略」按鈕要讓所有管理員都能按，
    /// 不是只有設定檔裡那一組 `admin_user_id`——通知本來就會發給所有
    /// `is_admin=true` 的帳號（見 `admin_discord_recipients`），按鈕權限理當
    /// 跟著放寬，否則其他管理員收到通知卻按不動，體驗不一致。讀取帳號清單
    /// 失敗時保守地只認設定檔管理員，不因為資料庫錯誤而放寬權限。
    async fn is_admin_discord_user(&self, user_id: &str) -> bool {
        if user_id == self.bot.admin_user_id {
            return true;
        }
        match self.accounts_db.list().await {
            Ok(accounts) => accounts
                .into_iter()
                .any(|account| account.is_admin && account.discord_user_id.trim() == user_id),
            Err(error) => {
                warn!(error = %error, "讀取帳號清單失敗，Discord 按鈕權限檢查以設定檔管理員為準");
                false
            }
        }
    }

    async fn handle_command(&self, ctx: &Context, command: CommandInteraction) -> Result<()> {
        let user_id = command.user.id.to_string();
        let reply_token = interaction_reply_token(command.id, &command.token);
        let action = format!("slash_{}", command.data.name);
        let target = discord_interaction_target(command.guild_id.map(|id| id.to_string()));
        crate::logs::record_audit(
            "discord",
            &user_id,
            &action,
            None,
            Some(target.as_str()),
            "Discord slash command",
        );
        match command.data.name.as_str() {
            "add-account" => {
                // L2（安全審查）：密碼是斜線指令參數，會出現在公共頻道。與 /request-account 一致，只允許私訊。
                if command.guild_id.is_some() {
                    reply_interaction(
                        ctx,
                        &command,
                        OutboundMessage::Text(
                            "請在 bot DM 使用 /add-account，避免帳密出現在公共 channel。".to_string(),
                        ),
                        true,
                    )
                    .await?;
                    return Ok(());
                }
                self.ensure_admin_interaction(ctx, &command).await?;
                let provider = string_option(&command, "provider").unwrap_or_default();
                let username = string_option(&command, "username").unwrap_or_default();
                let password = string_option(&command, "password").unwrap_or_default();
                let discord_user_id = user_option(&command, "user").unwrap_or_default();
                let account_id = string_option(&command, "id").unwrap_or_default();
                let message = self
                    .add_account(
                        &provider,
                        &username,
                        &password,
                        &discord_user_id,
                        &account_id,
                        Some(command.channel_id),
                    )
                    .await;
                reply_interaction(ctx, &command, message, true).await
            }
            "request-account" => {
                if command.guild_id.is_some() {
                    reply_interaction(
                        ctx,
                        &command,
                        OutboundMessage::Text(
                            "請在 bot DM 使用 /request-account，避免帳密出現在公共 channel。"
                                .to_string(),
                        ),
                        true,
                    )
                    .await?;
                    return Ok(());
                }

                let provider = string_option(&command, "provider").unwrap_or_default();
                let username = string_option(&command, "username").unwrap_or_default();
                let password = string_option(&command, "password").unwrap_or_default();
                let message = self
                    .request_account(&provider, &username, &password, &user_id)
                    .await;
                reply_interaction(ctx, &command, message, true).await
            }
            "bind-account" => {
                self.ensure_admin_interaction(ctx, &command).await?;
                let username = string_option(&command, "username").unwrap_or_default();
                let provider = string_option(&command, "provider");
                let discord_user_id = user_option(&command, "user").unwrap_or_default();
                let message = self
                    .bind_account_by_username(&username, provider.as_deref(), &discord_user_id)
                    .await;
                reply_interaction(ctx, &command, message, true).await
            }
            "unbind-account" => {
                self.ensure_admin_interaction(ctx, &command).await?;
                let username = string_option(&command, "username").unwrap_or_default();
                let provider = string_option(&command, "provider");
                let message = self
                    .bind_account_by_username(&username, provider.as_deref(), "")
                    .await;
                reply_interaction(ctx, &command, message, true).await
            }
            "bindings" => {
                self.ensure_admin_interaction(ctx, &command).await?;
                let message = OutboundMessage::Text(self.render_bindings().await);
                reply_interaction(ctx, &command, message, true).await
            }
            "qr" => {
                let qr_data = string_option(&command, "data").unwrap_or_default();
                requests::execute_command(
                    RequestCommand::QrCode {
                        data: qr_data,
                        rollcall_id: string_option(&command, "id").and_then(|raw| requests::parse_rollcall_id(&raw)),
                    },
                    &reply_token,
                    &self.requests,
                    &user_id,
                )
                .await
            }
            "num" => {
                let code = string_option(&command, "code").unwrap_or_default();
                requests::execute_command(
                    RequestCommand::NumberCode {
                        code,
                        rollcall_id: string_option(&command, "id").and_then(|raw| requests::parse_rollcall_id(&raw)),
                    },
                    &reply_token,
                    &self.requests,
                    &user_id,
                )
                .await
            }
            "status" => {
                requests::execute_command(
                    RequestCommand::GetStatus,
                    &reply_token,
                    &self.requests,
                    &user_id,
                )
                .await
            }
            "admin" => {
                let message = match self.bot.admin_url() {
                    Some(url) => OutboundMessage::Text(format!("Web 管理頁：{url}")),
                    None => OutboundMessage::Text(
                        "尚未設定 adapters.discord.public_base_url，無法產生 /admin 網址。"
                            .to_string(),
                    ),
                };
                reply_interaction(ctx, &command, message, true).await
            }
            "start" => {
                self.execute_admin_command(
                    ctx,
                    &command,
                    RequestCommand::Start,
                    &reply_token,
                    &user_id,
                )
                .await
            }
            "stop" => {
                self.execute_admin_command(
                    ctx,
                    &command,
                    RequestCommand::Stop,
                    &reply_token,
                    &user_id,
                )
                .await
            }
            "force" => {
                requests::execute_command(
                    RequestCommand::ForceAttend,
                    &reply_token,
                    &self.requests,
                    &user_id,
                )
                .await
            }
            "reauth" => {
                requests::execute_command(
                    RequestCommand::ReAuth,
                    &reply_token,
                    &self.requests,
                    &user_id,
                )
                .await
            }
            "help" => {
                requests::execute_command(
                    RequestCommand::Help,
                    &reply_token,
                    &self.requests,
                    &user_id,
                )
                .await
            }
            _ => {
                reply_interaction(
                    ctx,
                    &command,
                    OutboundMessage::UnknownCommand {
                        text: command.data.name.clone(),
                    },
                    true,
                )
                .await
            }
        }
    }

    async fn handle_component(&self, ctx: &Context, component: ComponentInteraction) -> Result<()> {
        let user_id = component.user.id.to_string();
        let reply_token = interaction_reply_token(component.id, &component.token);
        let (action, target) = discord_component_audit(&component.data.custom_id);
        crate::logs::record_audit(
            "discord",
            &user_id,
            action,
            None,
            target.as_deref(),
            "Discord button/component",
        );

        // NUMBER_AUTO_PICK_V1：選單上的單一點名按鈕 → 直接用該點名 ID 送出。
        if let Some((account_id, rollcall_id)) = parse_number_pick_custom_id(&component.data.custom_id) {
            requests::execute_command(
                RequestCommand::NumberAutoLookup {
                    account_id: Some(account_id.to_string()),
                    rollcall_id: Some(rollcall_id),
                },
                &reply_token,
                &self.requests,
                &user_id,
            )
            .await?;
            return Ok(());
        }

        // NUMBER_AUTO_PICK_V1：「自動讀取代碼」按鈕。
        // 只有一個點名在等待 → 直接送出；兩個以上 → 列出選單讓使用者選（只有按的人看得到）。
        // 未綁定該帳號的人不列 ID，仍由 execute_command 做授權檢查。
        if let Some(account_id) = parse_number_auto_custom_id(&component.data.custom_id) {
            let owned = user_id == self.bot.admin_user_id
                || self.discord_user_owns_account(&user_id, account_id).await;
            let waiting = requests::waiting_number_rollcall_ids(account_id);
            if owned && waiting.len() >= 2 {
                reply_number_picker(ctx, &component, account_id, &waiting).await?;
                return Ok(());
            }
            let only = if waiting.len() == 1 { Some(waiting[0]) } else { None };
            requests::execute_command(
                RequestCommand::NumberAutoLookup {
                    account_id: Some(account_id.to_string()),
                    rollcall_id: only,
                },
                &reply_token,
                &self.requests,
                &user_id,
            )
            .await?;
            return Ok(());
        }

        if let Some((action, token)) = parse_account_review_custom_id(&component.data.custom_id) {
            if user_id != self.bot.admin_user_id {
                reply_component(ctx, &component, OutboundMessage::NotAuthorized, true).await?;
                return Ok(());
            }

            let message = match action {
                AccountReviewAction::Approve => {
                    self.approve_pending_account(token, Some(component.channel_id))
                        .await
                }
                AccountReviewAction::Reject => self.reject_pending_account(token).await,
            };
            reply_component(ctx, &component, message, true).await?;
            return Ok(());
        }

        // LOGIN_TIERED_LOCKOUT_V1：第一層鎖定的兩顆持有人按鈕。
        if let Some((account_id, ip)) = parse_prefixed_account_ip(&component.data.custom_id, COMPONENT_CANCEL_LOCK_PREFIX) {
            let is_admin = user_id == self.bot.admin_user_id;
            let owned = is_admin || self.discord_user_owns_account(&user_id, &account_id).await;
            let message = if !owned {
                OutboundMessage::NotAuthorized
            } else {
                // 鍵涵蓋帳號 id 與學號（與 admin_web 上鎖時一致）。
                let mut keys = vec![account_id.clone()];
                if let Ok(Some(acct)) = self.accounts_db.get(&account_id).await {
                    keys.push(acct.username.clone());
                }
                let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
                let removed = crate::adapters::admin_web::cancel_login_lock(&key_refs).await;
                crate::logs::record_audit(
                    "discord",
                    &user_id,
                    "login_lock_cancel",
                    Some(&account_id),
                    Some(&ip),
                    "帳號持有人由 Discord 取消登入鎖定",
                );
                if removed {
                    OutboundMessage::Text(format!("[安全] 已取消帳號 `{account_id}` 的登入鎖定，現在可以直接重新登入。"))
                } else {
                    OutboundMessage::Text(format!("[安全] 帳號 `{account_id}` 目前沒有生效中的鎖定（可能已到期），可直接登入。"))
                }
            };
            reply_component(ctx, &component, message, true).await?;
            return Ok(());
        }

        if let Some((account_id, ip)) = parse_prefixed_account_ip(&component.data.custom_id, COMPONENT_OWNER_BLOCK_IP_PREFIX) {
            let is_admin = user_id == self.bot.admin_user_id;
            let owned = is_admin || self.discord_user_owns_account(&user_id, &account_id).await;
            let message = if !owned {
                OutboundMessage::NotAuthorized
            } else {
                let blocked_by = format!("owner:{account_id}");
                match self
                    .accounts_db
                    .block_ip(&ip, &blocked_by, "Account holder blocked the source IP from a login-lock alert")
                    .await
                {
                    Ok(true) => {
                        crate::logs::record_audit(
                            "discord",
                            &user_id,
                            "ip_blacklist_owner_block",
                            Some(&account_id),
                            Some(&ip),
                            "帳號持有人由 Discord 封鎖來源 IP",
                        );
                        OutboundMessage::Text(format!("[安全] 已封鎖 IP `{ip}`。之後來自此 IP 的登入一律被拒絕；管理員可在黑名單頁解除。"))
                    }
                    Ok(false) => OutboundMessage::Text(format!("[安全] IP `{ip}` 已經在黑名單中。")),
                    Err(error) => OutboundMessage::Text(format!("[安全] 封鎖 IP 失敗：{error}")),
                }
            };
            reply_component(ctx, &component, message, true).await?;
            return Ok(());
        }

        if let Some((account_id, ip)) = parse_unblock_own_ip_custom_id(&component.data.custom_id) {
            let is_admin = user_id == self.bot.admin_user_id;
            let result = if is_admin {
                self.accounts_db.unblock_ip(&ip).await
            } else {
                // Do not trust account_id from a Discord component. Database
                // ownership and the current Discord binding are authoritative.
                let _ = account_id;
                self.accounts_db
                    .unblock_automatic_ip_for_discord_user(&ip, &user_id)
                    .await
            };
            let message = match result {
                Ok(true) => OutboundMessage::Text(format!("[安全] 已移除自動黑名單：`{ip}`。")),
                Ok(false) => OutboundMessage::Text("[安全] 找不到可由你解除的自動黑名單。".to_string()),
                Err(error) => OutboundMessage::Text(format!("[安全] 移除黑名單失敗：{error}")),
            };
            reply_component(ctx, &component, message, true).await?;
            return Ok(());
        }

        if let Some(ip) = parse_block_ip_custom_id(&component.data.custom_id) {
            if user_id != self.bot.admin_user_id {
                reply_component(ctx, &component, OutboundMessage::NotAuthorized, true).await?;
                return Ok(());
            }
            let message = match self.accounts_db.block_ip(&ip, &user_id, "Discord security alert").await {
                Ok(true) => OutboundMessage::Text(format!("[安全] 已封鎖 IP `{ip}`。黑名單頁面可查看與解除封鎖。")),
                Ok(false) => OutboundMessage::Text(format!("[安全] IP `{ip}` 已經在黑名單中。")),
                Err(error) => OutboundMessage::Text(format!("[安全] 封鎖 IP 失敗：{error}")),
            };
            reply_component(ctx, &component, message, true).await?;
            return Ok(());
        }

        if let Some(pair) = parse_fp_release_custom_id(&component.data.custom_id) {
            // 指紋放行：只有 admin_user_id 能按（與 WAF 封鎖通知只發給他一致）。
            if user_id != self.bot.admin_user_id {
                reply_component(ctx, &component, OutboundMessage::NotAuthorized, true).await?;
                return Ok(());
            }
            let now = chrono::Local::now().timestamp();
            let message = match self
                .accounts_db
                .release_fingerprint_block(&pair, &user_id, now)
                .await
            {
                Ok(true) => OutboundMessage::Text(
                    "[安全] 已放行此 IP 群組＋指紋組合，之後不會再因指紋被封鎖。".to_string(),
                ),
                Ok(false) => OutboundMessage::Text("[安全] 找不到這筆指紋封鎖紀錄。".to_string()),
                Err(error) => OutboundMessage::Text(format!("[安全] 放行失敗：{error}")),
            };
            reply_component(ctx, &component, message, true).await?;
            return Ok(());
        }

        if let Some(key) = parse_unblock_waf_custom_id(&component.data.custom_id) {
            // WAF_AUTO_BLOCK_NOTIFY_V2：只有 admin_user_id 能解除 WAF 封鎖（與告警只發給他一致）。
            if user_id != self.bot.admin_user_id {
                reply_component(ctx, &component, OutboundMessage::NotAuthorized, true).await?;
                return Ok(());
            }
            let message = match self.accounts_db.unblock_ip(&key).await {
                Ok(true) => OutboundMessage::Text(format!("[安全] 已解除 WAF 封鎖 `{key}`。")),
                Ok(false) => OutboundMessage::Text(format!("[安全] `{key}` 已不在黑名單中。")),
                Err(error) => OutboundMessage::Text(format!("[安全] 解除失敗：{error}")),
            };
            reply_component(ctx, &component, message, true).await?;
            return Ok(());
        }

        if let Some(ip) = parse_unblock_ip_custom_id(&component.data.custom_id) {
            let is_admin = user_id == self.bot.admin_user_id;
            let result = if is_admin {
                self.accounts_db.unblock_ip(&ip).await
            } else {
                self.accounts_db
                    .unblock_automatic_ip_for_discord_user(&ip, &user_id)
                    .await
            };
            let message = match result {
                Ok(true) => OutboundMessage::Text(format!("[安全] 已將 IP `{ip}` 從黑名單移除。")),
                Ok(false) => OutboundMessage::Text("[安全] 找不到可由你解除的自動黑名單。".to_string()),
                Err(error) => OutboundMessage::Text(format!("[安全] 移除黑名單失敗：{error}")),
            };
            reply_component(ctx, &component, message, true).await?;
            return Ok(());
        }

        // ADMIN_GEO_RESTRICT_V1：只有真正的管理員能核准／忽略；GEO_ACCESS_ALL_
        // ADMINS_V1 之後改成「任一 is_admin 帳號」都算數，不再只認設定檔裡
        // 那一組 `admin_user_id`——通知本來就會發給所有管理員，按鈕也該一致。
        if let Some(request_id) = parse_geo_allow_custom_id(&component.data.custom_id) {
            if !self.is_admin_discord_user(&user_id).await {
                reply_component(ctx, &component, OutboundMessage::NotAuthorized, true).await?;
                return Ok(());
            }
            let message = match self
                .accounts_db
                .approve_geo_access_request(&request_id, crate::db::GEO_ACCESS_ALLOW_SECONDS, &user_id)
                .await
            {
                Ok(Some(row)) => OutboundMessage::Text(format!(
                    "[安全] 已暫時放行 IP `{}`，{} 小時內可跳過僅限台灣的限制。",
                    row.ip,
                    crate::db::GEO_ACCESS_ALLOW_SECONDS / 3600
                )),
                Ok(None) => {
                    OutboundMessage::Text("[安全] 找不到這筆申請，可能已過期或已被處理過。".to_string())
                }
                Err(error) => OutboundMessage::Text(format!("[安全] 暫時放行失敗：{error}")),
            };
            reply_component(ctx, &component, message, true).await?;
            return Ok(());
        }

        if let Some(request_id) = parse_geo_ignore_custom_id(&component.data.custom_id) {
            if !self.is_admin_discord_user(&user_id).await {
                reply_component(ctx, &component, OutboundMessage::NotAuthorized, true).await?;
                return Ok(());
            }
            let message = match self
                .accounts_db
                .dismiss_geo_access_request(&request_id, &user_id)
                .await
            {
                Ok(true) => OutboundMessage::Text("[安全] 已忽略這筆申請（維持擋下）。".to_string()),
                Ok(false) => {
                    OutboundMessage::Text("[安全] 找不到這筆申請，可能已過期或已被處理過。".to_string())
                }
                Err(error) => OutboundMessage::Text(format!("[安全] 忽略申請失敗：{error}")),
            };
            reply_component(ctx, &component, message, true).await?;
            return Ok(());
        }

        let cmd = match component.data.custom_id.as_str() {
            COMPONENT_STATUS => RequestCommand::GetStatus,
            COMPONENT_FORCE => RequestCommand::ForceAttend,
            COMPONENT_REAUTH => RequestCommand::ReAuth,
            COMPONENT_HELP => RequestCommand::Help,
            other => {
                reply_component(
                    ctx,
                    &component,
                    OutboundMessage::UnknownCommand {
                        text: other.to_string(),
                    },
                    true,
                )
                .await?;
                return Ok(());
            }
        };

        requests::execute_command(cmd, &reply_token, &self.requests, &user_id).await
    }

    async fn execute_admin_command(
        &self,
        ctx: &Context,
        command: &CommandInteraction,
        cmd: RequestCommand,
        reply_token: &str,
        user_id: &str,
    ) -> Result<()> {
        if user_id != self.bot.admin_user_id {
            reply_interaction(ctx, command, OutboundMessage::NotAuthorized, true).await?;
            return Ok(());
        }

        requests::execute_command(cmd, reply_token, &self.requests, user_id).await
    }

    async fn ensure_admin_interaction(
        &self,
        ctx: &Context,
        command: &CommandInteraction,
    ) -> Result<()> {
        if command.user.id.to_string() == self.bot.admin_user_id {
            return Ok(());
        }

        reply_interaction(ctx, command, OutboundMessage::NotAuthorized, true).await?;
        Err(miette::miette!("Discord 使用者未授權執行管理指令"))
    }

    async fn add_account(
        &self,
        provider: &str,
        username: &str,
        password: &str,
        discord_user_id: &str,
        account_id: &str,
        origin_channel: Option<ChannelId>,
    ) -> OutboundMessage {
        match self
            .create_account(provider, username, password, discord_user_id, account_id)
            .await
        {
            Ok(account) => {
                self.notify_created_account(&account).await;
                let monitor_note = self.start_monitor_in_background(&account, origin_channel);
                OutboundMessage::Text(format!(
                    "已新增 Tronclass 帳號 `{}`（provider: `{}`，id: `{}`）。{monitor_note}",
                    account.username, account.provider, account.account_id
                ))
            }
            Err(e) => OutboundMessage::Text(e),
        }
    }

    async fn create_account(
        &self,
        provider: &str,
        username: &str,
        password: &str,
        discord_user_id: &str,
        account_id: &str,
    ) -> std::result::Result<CreatedAccount, String> {
        let provider = provider.trim();
        let username = username.trim();
        let password = password.trim();
        let discord_user_id = discord_user_id.trim();
        let account_id = if account_id.trim().is_empty() {
            username
        } else {
            account_id.trim()
        };

        if provider.is_empty() || username.is_empty() || password.is_empty() {
            return Err("新增失敗：provider、username、password 都必須填寫。".to_string());
        }
        if account_id.is_empty() {
            return Err("新增失敗：無法建立空白帳號 ID。".to_string());
        }
        if provider.len() > MAX_DISCORD_PROVIDER_BYTES
            || username.len() > MAX_DISCORD_USERNAME_BYTES
            || password.len() > MAX_DISCORD_PASSWORD_BYTES
            || account_id.len() > MAX_DISCORD_IDENTIFIER_BYTES
            || discord_user_id.len() > MAX_DISCORD_IDENTIFIER_BYTES
        {
            return Err("新增失敗：provider、username、password 或識別碼過長。".to_string());
        }
        // PROVIDER_KIND_NO_PANIC_V1
        if let Err(message) = crate::auth::providers::check_provider_supported(provider) {
            return Err(format!("新增失敗：{message}"));
        }

        let account = RawAccountConfig {
            id: account_id.to_string(),
            provider: provider.to_string(),
            username: username.to_string(),
            password: password.to_string(),
            enabled: true,
            line_user_id: String::new(),
            discord_user_id: discord_user_id.to_string(),
            session_cookie: String::new(),
            is_admin: false,
            is_viewer: false,
            dashboard_password_hash: String::new(),
        };

        self.accounts_db
            .insert(&account)
            .await
            .map_err(|e| format!("新增帳號失敗：{e}"))?;

        Ok(CreatedAccount {
            provider: provider.to_string(),
            username: username.to_string(),
            account_id: account_id.to_string(),
            discord_user_id: discord_user_id.to_string(),
        })
    }

    async fn notify_created_account(&self, account: &CreatedAccount) {
        if account.discord_user_id.is_empty() {
            return;
        }

        // ACCOUNT_HOT_ADD_V1：有 spawner 時監控會在背景立即啟動，結果另行通知。
        let monitor_note = if self.monitor_spawner.is_some() {
            "系統正在為你啟動監控，完成後會再通知你。"
        } else {
            "新帳號會在服務重新啟動後開始監控。"
        };
        let welcome = OutboundMessage::Text(format!(
            "你的 Discord 已綁定 Tronclass username `{}`。{monitor_note}",
            account.username
        ));
        let _ = self.bot.push(&account.discord_user_id, &welcome).await;
    }

    async fn request_account(
        &self,
        provider: &str,
        username: &str,
        password: &str,
        discord_user_id: &str,
    ) -> OutboundMessage {
        let provider = provider.trim();
        let username = username.trim();
        let password = password.trim();
        let discord_user_id = discord_user_id.trim();

        if provider.is_empty() || username.is_empty() || password.is_empty() {
            return OutboundMessage::Text(
                "申請失敗：provider、username、password 都必須填寫。".to_string(),
            );
        }
        if provider.len() > MAX_DISCORD_PROVIDER_BYTES
            || username.len() > MAX_DISCORD_USERNAME_BYTES
            || password.len() > MAX_DISCORD_PASSWORD_BYTES
            || discord_user_id.len() > MAX_DISCORD_IDENTIFIER_BYTES
        {
            return OutboundMessage::Text(
                "申請失敗：provider、username、password 或 Discord 識別碼過長。".to_string(),
            );
        }

        match self
            .accounts_db
            .find_by_username(username, Some(provider))
            .await
        {
            Ok(accounts) if !accounts.is_empty() => {
                return OutboundMessage::Text(format!(
                    "Tronclass username `{username}`（provider: `{provider}`）已存在，請管理員改用綁定。"
                ));
            }
            Err(e) => {
                return OutboundMessage::Text(format!("檢查既有帳號失敗：{e}"));
            }
            _ => {}
        }

        match self.accounts_db.get(username).await {
            Ok(Some(_)) => {
                return OutboundMessage::Text(format!(
                    "帳號 ID `{username}` 已存在，請管理員手動新增或綁定。"
                ));
            }
            Err(e) => {
                return OutboundMessage::Text(format!("檢查既有帳號 ID 失敗：{e}"));
            }
            _ => {}
        }

        let Some(channel_id) = self.bot.admin_channel_id() else {
            return OutboundMessage::Text("申請失敗：Discord 管理 channel 尚未設定。".to_string());
        };

        let pending = PendingAccountRequest {
            provider: provider.to_string(),
            username: username.to_string(),
            password: password.to_string(),
            discord_user_id: discord_user_id.to_string(),
        };
        let token = account_review_token();

        {
            let mut pending_accounts = self.pending_accounts.lock().await;
            if pending_accounts.values().any(|request| {
                request.provider == pending.provider
                    && request.username == pending.username
                    && request.discord_user_id == pending.discord_user_id
            }) {
                return OutboundMessage::Text(
                    "你已送出同一個 Tronclass 帳號申請，請等待管理員核准。".to_string(),
                );
            }
            if pending_accounts.len() >= MAX_PENDING_ACCOUNT_REQUESTS {
                return OutboundMessage::Text("目前待審核帳號申請已滿，請稍後再試。".to_string());
            }
            pending_accounts.insert(token.clone(), pending.clone());
        }

        let send_result = channel_id
            .send_message(
                &self.bot.http,
                CreateMessage::new()
                    .content(render_pending_account_request(&pending))
                    .components(account_review_action_rows(&token)),
            )
            .await;

        if let Err(e) = send_result {
            self.pending_accounts.lock().await.remove(&token);
            return OutboundMessage::Text(format!("申請建立失敗：無法通知管理員：{e}"));
        }

        OutboundMessage::Text(format!(
            "已送出 Tronclass username `{username}` 的新增申請，請等待管理員核准。"
        ))
    }

    /// LOGIN_TIERED_LOCKOUT_V1：這個 Discord 使用者是否綁定在該帳號上
    /// （比對帳號 id 或 Tronclass 學號；資料庫綁定是唯一權威，不信任按鈕 payload）。
    async fn discord_user_owns_account(&self, discord_user_id: &str, account_id: &str) -> bool {
        let discord_user_id = discord_user_id.trim();
        if discord_user_id.is_empty() {
            return false;
        }
        if let Ok(Some(acct)) = self.accounts_db.get(account_id).await {
            return acct.discord_user_id.trim() == discord_user_id;
        }
        if let Ok(accounts) = self.accounts_db.find_by_username(account_id, None).await {
            return accounts
                .iter()
                .any(|acct| acct.discord_user_id.trim() == discord_user_id);
        }
        false
    }

    async fn approve_pending_account(
        &self,
        token: &str,
        origin_channel: Option<ChannelId>,
    ) -> OutboundMessage {
        let Some(request) = self.pending_accounts.lock().await.remove(token) else {
            return OutboundMessage::Text("這筆帳號申請不存在或已處理。".to_string());
        };

        match self
            .create_account(
                &request.provider,
                &request.username,
                &request.password,
                &request.discord_user_id,
                "",
            )
            .await
        {
            Ok(account) => {
                self.notify_created_account(&account).await;
                let monitor_note = self.start_monitor_in_background(&account, origin_channel);
                OutboundMessage::Text(format!(
                    "已核准並新增 Tronclass username `{}`（provider: `{}`）。{monitor_note}",
                    account.username, account.provider
                ))
            }
            Err(e) => {
                if !request.discord_user_id.is_empty() {
                    let _ = self
                        .bot
                        .push(
                            &request.discord_user_id,
                            &OutboundMessage::Text(format!(
                                "你的 Tronclass username `{}` 申請在核准時新增失敗：{e}",
                                request.username
                            )),
                        )
                        .await;
                }
                OutboundMessage::Text(format!("核准失敗：{e}"))
            }
        }
    }

    async fn reject_pending_account(&self, token: &str) -> OutboundMessage {
        let Some(request) = self.pending_accounts.lock().await.remove(token) else {
            return OutboundMessage::Text("這筆帳號申請不存在或已處理。".to_string());
        };

        if !request.discord_user_id.is_empty() {
            let _ = self
                .bot
                .push(
                    &request.discord_user_id,
                    &OutboundMessage::Text(format!(
                        "你的 Tronclass username `{}` 新增申請已被管理員拒絕。",
                        request.username
                    )),
                )
                .await;
        }

        OutboundMessage::Text(format!(
            "已拒絕 Tronclass username `{}` 的新增申請。",
            request.username
        ))
    }

    async fn bind_account_by_username(
        &self,
        username: &str,
        provider: Option<&str>,
        discord_user_id: &str,
    ) -> OutboundMessage {
        let username = username.trim();
        let provider = provider.map(str::trim).filter(|value| !value.is_empty());
        let discord_user_id = discord_user_id.trim();
        if username.is_empty() {
            return OutboundMessage::Text("缺少 username 參數".to_string());
        }
        if username.len() > MAX_DISCORD_USERNAME_BYTES
            || provider.is_some_and(|value| value.len() > MAX_DISCORD_PROVIDER_BYTES)
            || discord_user_id.len() > MAX_DISCORD_IDENTIFIER_BYTES
        {
            return OutboundMessage::Text(
                "綁定失敗：provider、username 或 Discord 識別碼過長。".to_string(),
            );
        }

        match self
            .accounts_db
            .set_discord_user_id_by_username(username, provider, discord_user_id)
            .await
        {
            Ok(UsernameUpdateResult::Updated { account_id }) => {
                let updated = self
                    .requests
                    .set_discord_user_id(&account_id, discord_user_id.trim())
                    .await;
                if !updated {
                    warn!(username = %username, account = %account_id, "DB 已更新，但 runtime 找不到帳號");
                }

                if discord_user_id.trim().is_empty() {
                    OutboundMessage::Text(format!(
                        "已解除 Tronclass username `{}` 的 Discord 綁定。",
                        username.trim()
                    ))
                } else {
                    let welcome = OutboundMessage::Text(format!(
                        "你的 Discord 已綁定 Tronclass username `{}`。可用 /status 查詢狀態。",
                        username.trim()
                    ));
                    let _ = self.bot.push(discord_user_id.trim(), &welcome).await;
                    OutboundMessage::Text(format!(
                        "已將 Tronclass username `{}` 綁定到 <@{}>。",
                        username.trim(),
                        discord_user_id.trim()
                    ))
                }
            }
            Ok(UsernameUpdateResult::NotFound) => {
                OutboundMessage::Text(format!("找不到 Tronclass username `{}`。", username.trim()))
            }
            Ok(UsernameUpdateResult::Ambiguous { account_ids }) => OutboundMessage::Text(format!(
                "Tronclass username `{}` 對應多個帳號：{}。請加上 provider 參數消除歧義。",
                username.trim(),
                account_ids.join(", ")
            )),
            Err(e) => OutboundMessage::Text(format!("更新綁定失敗：{e}")),
        }
    }

    async fn render_bindings(&self) -> String {
        let bindings = self.requests.discord_bindings().await;
        if bindings.is_empty() {
            return "目前沒有可列出的帳號。".to_string();
        }

        let mut lines = vec!["Discord 綁定清單".to_string()];
        for (username, discord_user_id) in bindings {
            let target = if discord_user_id.is_empty() {
                "(未綁定)".to_string()
            } else {
                format!("<@{discord_user_id}>")
            };
            lines.push(format!("- `{username}` -> {target}"));
        }
        lines.join("\n")
    }
}

async fn register_commands(ctx: &Context, guild_ids: &[String]) -> Result<()> {
    let commands = discord_commands();
    if guild_ids.is_empty() {
        Command::set_global_commands(&ctx.http, commands)
            .await
            .into_diagnostic()
            .wrap_err("註冊 Discord global commands 失敗")?;
        return Ok(());
    }

    for guild_id in guild_ids {
        let guild_id = parse_discord_id(guild_id)
            .ok_or_else(|| miette::miette!("Discord guild id 無效：{guild_id}"))?;
        GuildId::new(guild_id)
            .set_commands(&ctx.http, commands.clone())
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("註冊 Discord guild commands 失敗：{guild_id}"))?;
    }

    Ok(())
}

/// DISCORD_COMMAND_UX_V1：學校選項。不鎖死 choices，以免 config 自訂 provider 名稱選不到。
fn discord_provider_option(required: bool) -> CreateCommandOption {
    let opt = CreateCommandOption::new(
        CommandOptionType::String,
        "provider",
        "學校代碼：tku＝淡江 iClass，fju＝輔仁，或 config 裡的名稱",
    )
    .min_length(1)
    .max_length(64);
    if required {
        opt.required(true)
    } else {
        opt.required(false)
    }
}

fn discord_commands() -> Vec<CreateCommand> {
    vec![
        CreateCommand::new("status").description("查看監控狀態（僅自己看得到）"),
        CreateCommand::new("start").description("管理員：啟動全部帳號的簽到監控"),
        CreateCommand::new("stop").description("管理員：暫停全部帳號的簽到監控"),
        CreateCommand::new("force").description("立刻檢查點名（已綁定使用者會喚醒全部帳號）"),
        CreateCommand::new("reauth").description("重新登入學校（一般使用者只重登自己的帳號）"),
        CreateCommand::new("admin").description("取得 Web 管理頁連結（僅自己看得到）"),
        CreateCommand::new("help").description("顯示可用指令"),
        CreateCommand::new("qr")
            .description("送出課堂 QR 網址或內容")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "data",
                    "QR 網址或掃描內容",
                )
                .required(true)
                .min_length(1)
                .max_length(4000),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "id",
                    "點名 ID（多個點名同時等待時必填，例如 123）",
                )
                .required(false),
            ),
        CreateCommand::new("num")
            .description("送出數字點名 4 位數（手動送出，不受自動讀碼 15%）")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "code",
                    "剛好 4 位數字，例如 1234",
                )
                .required(true)
                .min_length(4)
                .max_length(4),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "id",
                    "點名 ID（多個點名同時等待時必填，例如 123）",
                )
                .required(false),
            ),
        CreateCommand::new("request-account")
            .description("申請新增帳號（請在 bot 私訊使用，需管理員核准）")
            .add_option(discord_provider_option(true))
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "username",
                    "學校登入帳號",
                )
                .required(true)
                .min_length(1)
                .max_length(MAX_DISCORD_USERNAME_BYTES as u16),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "password",
                    "學校密碼（回覆僅自己看得到；請在私訊輸入）",
                )
                .required(true)
                .min_length(1)
                .max_length(MAX_DISCORD_PASSWORD_BYTES as u16),
            ),
        CreateCommand::new("add-account")
            .description("管理員：新增帳號，可順便綁定 Discord 使用者")
            .add_option(discord_provider_option(true))
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "username",
                    "學校登入帳號",
                )
                .required(true)
                .min_length(1)
                .max_length(MAX_DISCORD_USERNAME_BYTES as u16),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "password",
                    "學校密碼（回覆僅自己看得到）",
                )
                .required(true)
                .min_length(1)
                .max_length(MAX_DISCORD_PASSWORD_BYTES as u16),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::User,
                    "user",
                    "要綁定的 Discord 使用者",
                )
                .required(false),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "id",
                    "資料庫帳號 id；留空則用學校帳號",
                )
                .required(false)
                .max_length(MAX_DISCORD_IDENTIFIER_BYTES as u16),
            ),
        CreateCommand::new("bind-account")
            .description("管理員：把學校帳號綁到 Discord 使用者")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "username",
                    "學校登入帳號",
                )
                .required(true)
                .min_length(1)
                .max_length(MAX_DISCORD_USERNAME_BYTES as u16),
            )
            .add_option(
                CreateCommandOption::new(CommandOptionType::User, "user", "Discord 使用者")
                    .required(true),
            )
            .add_option(discord_provider_option(false)),
        CreateCommand::new("unbind-account")
            .description("管理員：解除學校帳號的 Discord 綁定")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "username",
                    "學校登入帳號",
                )
                .required(true)
                .min_length(1)
                .max_length(MAX_DISCORD_USERNAME_BYTES as u16),
            )
            .add_option(discord_provider_option(false)),
        CreateCommand::new("bindings").description("管理員：列出 Discord 綁定"),
    ]
}


async fn send_outbound_to_channel(
    http: &Arc<Http>,
    channel_id: ChannelId,
    message: &OutboundMessage,
) -> Result<()> {
    throttle_discord_http().await;
    // DISCORD_NOTIFY_UNIFY_V1／DISCORD_NOTIFY_ALL_CARDS_V1：一律 embed 卡片。
    let payload = render_discord_payload(message);
    let mut builder = CreateMessage::new()
        .content(payload.content)
        .components(action_rows_for_message(message));
    if let Some(embed) = payload.embed {
        builder = builder.embed(embed);
    }
    channel_id
        .send_message(http, builder)
        .await
        .into_diagnostic()
        .wrap_err("發送 Discord 訊息失敗")?;
    Ok(())
}

async fn throttle_discord_http() {
    let interval = Duration::from_millis(DISCORD_SEND_INTERVAL_MS);
    let mut last_sent = DISCORD_HTTP_THROTTLE.lock().await;
    let elapsed = last_sent.elapsed();
    if elapsed < interval {
        tokio::time::sleep(interval - elapsed).await;
    }
    *last_sent = Instant::now();
}

async fn should_send_dashboard_copy(message: &OutboundMessage) -> bool {
    let Some(key) = dashboard_dedupe_key(message) else {
        return true;
    };

    let now = Instant::now();
    let ttl = Duration::from_secs(DISCORD_DASHBOARD_DEDUPE_TTL_SECS);
    let mut seen = DISCORD_DASHBOARD_DEDUPE.lock().await;
    seen.retain(|_, instant| now.duration_since(*instant) < ttl);

    if seen.contains_key(&key) {
        return false;
    }
    if seen.len() >= MAX_DISCORD_DASHBOARD_DEDUPE {
        if let Some(oldest) = seen
            .iter()
            .min_by_key(|(_, at)| **at)
            .map(|(key, _)| key.clone())
        {
            seen.remove(&oldest);
        }
    }
    seen.insert(key, now);
    true
}

fn dashboard_dedupe_id<T: Hash + ?Sized>(value: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn dashboard_dedupe_key(message: &OutboundMessage) -> Option<String> {
    match message {
        OutboundMessage::SystemStarted(_) => Some("system-started".to_string()),
        OutboundMessage::RollcallDetected(event) => Some(format!(
            "detected:{:016x}",
            dashboard_dedupe_id(&event.rollcall_id)
        )),
        OutboundMessage::NumberCodeRequested(request) => Some(format!(
            "number-request:{:016x}",
            dashboard_dedupe_id(&request.rollcall_id)
        )),
        OutboundMessage::QrCodeRequested(request) => Some(format!(
            "qr-request:{:016x}",
            dashboard_dedupe_id(&request.rollcall_id)
        )),
        OutboundMessage::RollcallFinished(event) => Some(format!(
            "finished:{:016x}:{}",
            dashboard_dedupe_id(&event.rollcall_id),
            if event.success { "success" } else { "failed" }
        )),
        // ROLLCALL_INPUT_REMINDER_V1：同一筆點名的催促只往 dashboard 頻道送一次。
        OutboundMessage::RollcallInputReminder(event) => Some(format!(
            "reminder:{:016x}",
            dashboard_dedupe_id(&event.rollcall_id)
        )),
        // DISCORD_NOTIFY_UNIFY_V1：同帳號同種監控事件在 TTL 內只往 dashboard 頻道送一次，
        // 多帳號同時重登失敗時頻道不會被洗版（DM 不受影響）。
        OutboundMessage::MonitorEvent(event) => Some(format!(
            "monitor:{:?}:{:016x}",
            event.kind,
            dashboard_dedupe_id(&event.account)
        )),
        OutboundMessage::TimetableChanged(event) => Some(format!(
            "timetable:{:016x}",
            dashboard_dedupe_id(&event.account)
        )),
        _ => None,
    }
}

async fn reply_interaction(
    ctx: &Context,
    command: &CommandInteraction,
    message: OutboundMessage,
    ephemeral: bool,
) -> Result<()> {
    let payload = render_discord_payload(&message);
    let mut builder = CreateInteractionResponseMessage::new()
        .content(payload.content)
        .ephemeral(ephemeral)
        .components(action_rows_for_message(&message));
    if let Some(embed) = payload.embed {
        builder = builder.embed(embed);
    }
    command
        .create_response(&ctx.http, CreateInteractionResponse::Message(builder))
        .await
        .into_diagnostic()
        .wrap_err("回覆 Discord slash command 失敗")
}

/// NUMBER_AUTO_PICK_V1：多個數字點名同時等待時，回覆只有按的人看得到的選單，每個點名一顆按鈕。
async fn reply_number_picker(
    ctx: &Context,
    component: &ComponentInteraction,
    account_id: &str,
    rollcall_ids: &[u64],
) -> Result<()> {
    // Discord 一則訊息最多 5 列、每列 5 顆按鈕，這裡最多列 20 個。
    let shown: Vec<u64> = rollcall_ids.iter().copied().take(20).collect();
    let mut rows: Vec<CreateActionRow> = Vec::new();
    for chunk in shown.chunks(5) {
        let buttons: Vec<CreateButton> = chunk
            .iter()
            .map(|rollcall_id| {
                CreateButton::new(number_pick_custom_id(account_id, *rollcall_id))
                    .label(format!("點名 #{rollcall_id}"))
                    .style(ButtonStyle::Primary)
            })
            .collect();
        rows.push(CreateActionRow::Buttons(buttons));
    }
    let text = format!(
        "帳號 `{account_id}` 目前有 {} 個數字點名同時等待，請選擇要自動讀取哪一個：",
        rollcall_ids.len()
    );
    let builder = CreateInteractionResponseMessage::new()
        .content(text)
        .ephemeral(true)
        .components(rows);
    component
        .create_response(&ctx.http, CreateInteractionResponse::Message(builder))
        .await
        .into_diagnostic()
        .wrap_err("回覆 Discord 點名選單失敗")
}

async fn reply_component(
    ctx: &Context,
    component: &ComponentInteraction,
    message: OutboundMessage,
    ephemeral: bool,
) -> Result<()> {
    let payload = render_discord_payload(&message);
    let mut builder = CreateInteractionResponseMessage::new()
        .content(payload.content)
        .ephemeral(ephemeral)
        .components(action_rows_for_message(&message));
    if let Some(embed) = payload.embed {
        builder = builder.embed(embed);
    }
    component
        .create_response(&ctx.http, CreateInteractionResponse::Message(builder))
        .await
        .into_diagnostic()
        .wrap_err("回覆 Discord component 失敗")
}

fn action_rows_for_message(message: &OutboundMessage) -> Vec<CreateActionRow> {
    match message {
        OutboundMessage::SystemStarted(_)
        | OutboundMessage::RollcallDetected(_)
        | OutboundMessage::RollcallFinished(_)
        | OutboundMessage::Status(_)
        | OutboundMessage::Help
        | OutboundMessage::Welcome => vec![CreateActionRow::Buttons(vec![
            CreateButton::new(COMPONENT_STATUS)
                .label("狀態")
                .style(ButtonStyle::Primary),
            CreateButton::new(COMPONENT_FORCE)
                .label("立即檢查")
                .style(ButtonStyle::Secondary),
            CreateButton::new(COMPONENT_REAUTH)
                .label("重新登入")
                .style(ButtonStyle::Secondary),
            CreateButton::new(COMPONENT_HELP)
                .label("說明")
                .style(ButtonStyle::Secondary),
        ])],
        OutboundMessage::NumberCodeRequested(request) => {
            let mut buttons = vec![
                CreateButton::new(format!("{COMPONENT_NUMBER_AUTO_PREFIX}{}", request.account))
                    .label("自動讀取代碼")
                    .style(ButtonStyle::Primary),
                CreateButton::new(COMPONENT_STATUS)
                    .label("狀態")
                    .style(ButtonStyle::Secondary),
            ];
            if !request.input_url.trim().is_empty() {
                buttons.insert(
                    0,
                    CreateButton::new_link(&request.input_url).label("開啟數字點名頁"),
                );
            }
            vec![CreateActionRow::Buttons(buttons)]
        }
        // ROLLCALL_INPUT_REMINDER_V1：催促訊息附上輸入頁按鈕（數字另給「自動讀取代碼」）。
        OutboundMessage::RollcallInputReminder(event) => {
            let mut buttons = Vec::new();
            if !event.input_url.trim().is_empty() {
                buttons.push(
                    CreateButton::new_link(event.input_url.trim()).label(if event.is_number() {
                        "開啟數字點名頁"
                    } else {
                        "開啟掃碼頁"
                    }),
                );
            }
            if event.is_number() {
                buttons.push(
                    CreateButton::new(format!("{COMPONENT_NUMBER_AUTO_PREFIX}{}", event.account))
                        .label("自動讀取代碼")
                        .style(ButtonStyle::Primary),
                );
            }
            buttons.push(
                CreateButton::new(COMPONENT_STATUS)
                    .label("狀態")
                    .style(ButtonStyle::Secondary),
            );
            vec![CreateActionRow::Buttons(buttons)]
        }
        OutboundMessage::QrCodeRequested(request) => vec![
            CreateActionRow::Buttons(vec![
                CreateButton::new_link(&request.scan_url).label("開啟掃碼頁")
            ]),
            CreateActionRow::Buttons(vec![CreateButton::new(COMPONENT_STATUS)
                .label("狀態")
                .style(ButtonStyle::Primary)]),
        ],
        OutboundMessage::SecurityAlert(event) if event.waf_block.is_some() => {
            vec![CreateActionRow::Buttons(vec![
                CreateButton::new(format!("{COMPONENT_UNBLOCK_WAF_PREFIX}{}", event.ip))
                    .label("解除封鎖")
                    .style(ButtonStyle::Success),
                CreateButton::new_link("https://dashboard.jasoncloudserver.me/admin/blacklist")
                    .label("查看黑名單"),
            ])]
        }
        OutboundMessage::SecurityAlert(event) => {
            if parse_discord_ip(&event.ip).is_some() {
                // LOGIN_TIERED_LOCKOUT_V1：第一層鎖定 → 持有人可「取消這次鎖定」快速登入，
                // 或「封鎖此 IP」直接把來源加入黑名單（兩者都由 handler 驗證 Discord 綁定）。
                if event.login_locked && !event.ip_is_blacklisted {
                    return vec![CreateActionRow::Buttons(vec![
                        CreateButton::new(format!("{COMPONENT_CANCEL_LOCK_PREFIX}{}|{}", event.account_id, event.ip))
                            .label("取消這次鎖定")
                            .style(ButtonStyle::Success),
                        CreateButton::new(format!("{COMPONENT_OWNER_BLOCK_IP_PREFIX}{}|{}", event.account_id, event.ip))
                            .label("封鎖此 IP")
                            .style(ButtonStyle::Danger),
                        CreateButton::new_link(
                            "https://dashboard.jasoncloudserver.me/admin/blacklist",
                        )
                        .label("查看黑名單"),
                    ])];
                }
                let action = if event.ip_is_blacklisted && event.allow_owner_unblock {
                    CreateButton::new(format!("{COMPONENT_UNBLOCK_OWN_IP_PREFIX}{}|{}", event.account_id, event.ip))
                        .label("移除黑名單")
                        .style(ButtonStyle::Success)
                } else if event.ip_is_blacklisted {
                    CreateButton::new(format!("{COMPONENT_UNBLOCK_IP_PREFIX}{}", event.ip))
                        .label("移除黑名單")
                        .style(ButtonStyle::Success)
                } else {
                    CreateButton::new(format!("{COMPONENT_BLOCK_IP_PREFIX}{}", event.ip))
                        .label("加入黑名單")
                        .style(ButtonStyle::Danger)
                };
                vec![CreateActionRow::Buttons(vec![
                    action,
                    CreateButton::new_link(
                        "https://dashboard.jasoncloudserver.me/admin/blacklist",
                    )
                    .label("查看黑名單"),
                ])]
            } else {
                vec![]
            }
        }
        OutboundMessage::FingerprintBlock(event) => vec![CreateActionRow::Buttons(vec![
            CreateButton::new(format!("{COMPONENT_FP_RELEASE_PREFIX}{}", event.pair))
                .label("放行此組合")
                .style(ButtonStyle::Success),
            CreateButton::new_link("https://dashboard.jasoncloudserver.me/admin/blacklist")
                .label("查看管理後台"),
        ])],
        OutboundMessage::GeoAccessRequest(event) => vec![CreateActionRow::Buttons(vec![
            CreateButton::new(format!("{COMPONENT_GEO_ALLOW_PREFIX}{}", event.request_id))
                .label("暫時放行")
                .style(ButtonStyle::Success),
            CreateButton::new(format!("{COMPONENT_GEO_IGNORE_PREFIX}{}", event.request_id))
                .label("忽略")
                .style(ButtonStyle::Secondary),
            CreateButton::new_link("https://dashboard.jasoncloudserver.me/admin/blacklist")
                .label("查看管理後台"),
        ])],
        _ => vec![],
    }
}

fn account_review_token() -> String {
    Uuid::new_v4().simple().to_string()
}

fn account_review_custom_id(action: AccountReviewAction, token: &str) -> String {
    let prefix = match action {
        AccountReviewAction::Approve => COMPONENT_ACCOUNT_APPROVE_PREFIX,
        AccountReviewAction::Reject => COMPONENT_ACCOUNT_REJECT_PREFIX,
    };
    format!("{prefix}{token}")
}

fn discord_interaction_target(guild_id: Option<String>) -> String {
    guild_id
        .map(|id| format!("guild={id}"))
        .unwrap_or_else(|| "dm".to_string())
}

fn discord_text_audit_action(content: &str) -> Option<&'static str> {
    match RequestCommand::parse(content) {
        RequestCommand::GetStatus => Some("text_status"),
        RequestCommand::ForceAttend => Some("text_force"),
        RequestCommand::Stop => Some("text_stop"),
        RequestCommand::Start => Some("text_start"),
        RequestCommand::ReAuth => Some("text_reauth"),
        RequestCommand::QrCode { .. } => Some("text_qr"),
        RequestCommand::NumberCode { .. } => Some("text_num"),
        RequestCommand::NumberAutoLookup { .. } => Some("text_number_auto_lookup"),
        RequestCommand::Help => Some("text_help"),
        RequestCommand::Unknown(_) => None,
    }
}

fn discord_component_audit(custom_id: &str) -> (&'static str, Option<String>) {
    if let Some((account_id, rollcall_id)) = parse_number_pick_custom_id(custom_id) {
        return (
            "button_number_pick",
            Some(format!("account={account_id} rollcall={rollcall_id}")),
        );
    }
    if let Some(account_id) = parse_number_auto_custom_id(custom_id) {
        return (
            "button_number_auto_lookup",
            Some(format!("account={account_id}")),
        );
    }

    if let Some((action, _token)) = parse_account_review_custom_id(custom_id) {
        return match action {
            AccountReviewAction::Approve => ("button_account_approve", None),
            AccountReviewAction::Reject => ("button_account_reject", None),
        };
    }

    if let Some(ip) = parse_block_ip_custom_id(custom_id) {
        return ("button_block_ip", Some(format!("ip={ip}")));
    }

    match custom_id {
        COMPONENT_STATUS => ("button_status", None),
        COMPONENT_FORCE => ("button_force", None),
        COMPONENT_REAUTH => ("button_reauth", None),
        COMPONENT_HELP => ("button_help", None),
        _ => ("button_unknown", None),
    }
}

fn parse_number_auto_custom_id(value: &str) -> Option<&str> {
    value
        .strip_prefix(COMPONENT_NUMBER_AUTO_PREFIX)
        .filter(|account_id| !account_id.trim().is_empty())
}

fn number_pick_custom_id(account_id: &str, rollcall_id: u64) -> String {
    format!("{COMPONENT_NUMBER_PICK_PREFIX}{account_id}:{rollcall_id}")
}

fn parse_number_pick_custom_id(value: &str) -> Option<(&str, u64)> {
    let rest = value.strip_prefix(COMPONENT_NUMBER_PICK_PREFIX)?;
    let (account_id, rollcall_id) = rest.rsplit_once(':')?;
    if account_id.trim().is_empty() {
        return None;
    }
    let rollcall_id = rollcall_id.parse::<u64>().ok()?;
    Some((account_id, rollcall_id))
}

fn parse_account_review_custom_id(value: &str) -> Option<(AccountReviewAction, &str)> {
    if let Some(token) = value.strip_prefix(COMPONENT_ACCOUNT_APPROVE_PREFIX) {
        return Some((AccountReviewAction::Approve, token));
    }
    if let Some(token) = value.strip_prefix(COMPONENT_ACCOUNT_REJECT_PREFIX) {
        return Some((AccountReviewAction::Reject, token));
    }
    None
}

fn account_review_action_rows(token: &str) -> Vec<CreateActionRow> {
    vec![CreateActionRow::Buttons(vec![
        CreateButton::new(account_review_custom_id(
            AccountReviewAction::Approve,
            token,
        ))
        .label("核准")
        .style(ButtonStyle::Success),
        CreateButton::new(account_review_custom_id(AccountReviewAction::Reject, token))
            .label("拒絕")
            .style(ButtonStyle::Danger),
    ])]
}

fn render_pending_account_request(request: &PendingAccountRequest) -> String {
    format!(
        "🆕 待審核帳號申請\n\n申請人：<@{}>\nProvider：`{}`\nUsername：`{}`\n\n說明：密碼不會顯示在控制台；核准後會寫入 accounts.db、綁定申請人並立即在背景啟動監控（不需重啟服務）。",
        request.discord_user_id, request.provider, request.username
    )
}

// ─── DISCORD_NOTIFY_UNIFY_V1 + DISCORD_NOTIFY_ALL_CARDS_V1 + NOTIF_CARD_ONLY_V1 ─
//
// 所有 Discord 出口（DM、管理員頻道、slash 回覆、按鈕回覆）都經過
// `render_discord_payload`，**一律 embed 卡片**（含點名急件與指令回覆）。
// NOTIF_CARD_ONLY_V1（取代舊版 fix132 的「content 鎖屏預覽」設計）：有卡片時
// content 留空，不再額外疊一行「類別：標題」——使用者反映這樣在 Discord 上
// 看起來像同一則通知顯示兩次（純文字 + 卡片各一份幾乎一樣的內容）。
// 純文字 `render_discord_message` 目前只有兩種用途：(1) 測試／攤平比對、
// (2) 極少數刻意要用純文字 `.say()`（不走卡片）的地方，例如非管理 channel
// 裡有人誤用管理指令時的拒絕訊息。`render_discord_payload` 裡的 `None`
// 分支（也會落到這個 fallback）目前沒有任何 `OutboundMessage` 變體會真的
// 走到——`notice_card_for` 窮舉所有變體都回傳 `Some`，這段純屬保險。
// NOTIF_STRUCTURED_V1・fix132：embed 標題統一「類別 emoji＋主結論＋《課名》」；
// 「[安全]」「[點名]」前綴會映射為對應類別卡片。LINE 文案不動、不受影響。

/// 通知類別：決定 embed 顏色與 footer 文字。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoticeCategory {
    Rollcall,
    Homework,
    Bulletin,
    Schedule,
    Monitor,
    Security,
    System,
    Success,
    Warning,
    Critical,
}

impl NoticeCategory {
    fn colour(self) -> Colour {
        match self {
            NoticeCategory::Rollcall => Colour::new(0x5865F2),  // blurple
            NoticeCategory::Homework => Colour::new(0xF5A623),  // amber
            NoticeCategory::Bulletin => Colour::new(0x3BA55D),  // green
            NoticeCategory::Schedule => Colour::new(0x00B0F4),  // sky
            NoticeCategory::Monitor => Colour::new(0x99AAB5),   // grey
            NoticeCategory::Security => Colour::new(0xED4245),  // red
            NoticeCategory::System => Colour::new(0x747F8D),    // slate
            NoticeCategory::Success => Colour::new(0x57F287),   // bright green
            NoticeCategory::Warning => Colour::new(0xFEE75C),   // yellow
            NoticeCategory::Critical => Colour::new(0xED4245),  // red
        }
    }

    /// NOTIF_STRUCTURED_V1・fix132：類別 emoji，放在 embed 標題開頭（與 Web Push 同套）。
    fn emoji(self) -> &'static str {
        match self {
            NoticeCategory::Rollcall => "\u{1F6A8}",
            NoticeCategory::Homework => "\u{1F4DD}",
            NoticeCategory::Bulletin => "\u{1F4E2}",
            NoticeCategory::Schedule => "\u{1F4C5}",
            NoticeCategory::Monitor => "\u{1F4CA}",
            NoticeCategory::Security => "\u{1F510}",
            NoticeCategory::System => "\u{2699}\u{FE0F}",
            NoticeCategory::Success => "\u{2705}",
            NoticeCategory::Warning => "\u{26A0}\u{FE0F}",
            NoticeCategory::Critical => "\u{1F6A8}",
        }
    }

    fn label(self) -> &'static str {
        match self {
            NoticeCategory::Rollcall => "點名",
            NoticeCategory::Homework => "作業",
            NoticeCategory::Bulletin => "公告",
            NoticeCategory::Schedule => "課表",
            NoticeCategory::Monitor => "監控",
            NoticeCategory::Security => "安全",
            NoticeCategory::System => "系統",
            NoticeCategory::Success => "監控・已恢復",
            NoticeCategory::Warning => "監控・警告",
            NoticeCategory::Critical => "監控・嚴重",
        }
    }
}

/// 統一卡片：所有 Discord 通知都先組成這個結構，再編成 embed（純文字由 to_text 攤平）。
struct NoticeCard {
    category: NoticeCategory,
    title: String,
    /// 帳號 id（放在 footer，每張卡片一致位置）。
    account: String,
    /// 主要說明（embed description）。
    description: String,
    /// (欄位名, 值, inline)。
    fields: Vec<(String, String, bool)>,
    /// 結尾提示／建議動作。
    hint: String,
}

impl NoticeCard {
    fn new(category: NoticeCategory, title: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            category,
            title: title.into(),
            account: account.into(),
            description: String::new(),
            fields: Vec::new(),
            hint: String::new(),
        }
    }

    fn description(mut self, text: impl Into<String>) -> Self {
        self.description = text.into();
        self
    }

    fn field(mut self, name: &str, value: impl Into<String>, inline: bool) -> Self {
        let value = value.into();
        if !value.trim().is_empty() {
            self.fields.push((name.to_string(), value, inline));
        }
        self
    }

    fn hint(mut self, text: impl Into<String>) -> Self {
        self.hint = text.into();
        self
    }

    fn footer_text(&self) -> String {
        if self.account.trim().is_empty() {
            format!("Tronclass ・ {}", self.category.label())
        } else {
            format!("Tronclass ・ {} ・ 帳號 {}", self.category.label(), self.account.trim())
        }
    }

    fn to_embed(&self) -> CreateEmbed {
        let mut embed = CreateEmbed::new()
            .title(truncate_embed(&self.title, 256))
            .colour(self.category.colour())
            .footer(CreateEmbedFooter::new(truncate_embed(&self.footer_text(), 2048)));
        let mut description = self.description.trim().to_string();
        if !self.hint.trim().is_empty() {
            if !description.is_empty() {
                description.push_str("\n\n");
            }
            description.push_str(self.hint.trim());
        }
        if !description.is_empty() {
            embed = embed.description(truncate_embed(&description, 4096));
        }
        for (name, value, inline) in self.fields.iter().take(25) {
            embed = embed.field(truncate_embed(name, 256), truncate_embed(value, 1024), *inline);
        }
        embed
    }

    /// 同一張卡片攤平成純文字（無 embed 的出口、或使用者關閉 embed 預覽時的 fallback）。
    fn to_text(&self) -> String {
        let mut lines = vec![format!("{}：{}", self.category.label(), self.title)];
        if !self.description.trim().is_empty() {
            lines.push(String::new());
            lines.push(self.description.trim().to_string());
        }
        if !self.fields.is_empty() {
            lines.push(String::new());
            for (name, value, _) in &self.fields {
                lines.push(format!("{name}：{value}"));
            }
        }
        if !self.account.trim().is_empty() {
            lines.push(format!("帳號：{}", self.account.trim()));
        }
        if !self.hint.trim().is_empty() {
            lines.push(String::new());
            lines.push(self.hint.trim().to_string());
        }
        lines.join("\n")
    }
}

/// 去掉字串開頭的表情符號／變體選擇子與其後空白（只處理開頭，內文不動）。
fn strip_leading_emoji(text: &str) -> String {
    let is_emoji_like = |c: char| {
        matches!(
            c as u32,
            0x2600..=0x27BF          // Misc symbols / dingbats（⚠ ✅ ❌ ⏰ ✳ ℹ 以外）
            | 0x2B00..=0x2BFF        // arrows / stars
            | 0x1F000..=0x1FAFF      // emoji blocks
            | 0xFE0F                 // variation selector-16
            | 0x200D                 // ZWJ
            | 0x2139                 // ℹ
            | 0x203C | 0x2049        // ‼ ⁉
        )
    };
    let trimmed = text.trim_start_matches(|c: char| is_emoji_like(c) || c == ' ' || c == '\u{3000}');
    if trimmed.is_empty() {
        text.to_string()
    } else {
        trimmed.to_string()
    }
}

fn truncate_embed(value: &str, max_chars: usize) -> String {
    crate::text_utils::truncate_chars(value, max_chars).to_string()
}

/// 一次 Discord 送出的內容：有 `embed` 時 `content` 留空，避免卡片上方又疊一行
/// 幾乎一樣的文字（NOTIF_CARD_ONLY_V1，取代舊版 fix132 的「content 鎖屏預覽」
/// 設計——使用者反映疊字看起來像重複通知兩次，故改成只留卡片）；完全沒有卡片
/// 的訊息（`notice_card_for` 回傳 `None`）才會用 `content` 承載純文字。
struct DiscordPayload {
    content: String,
    embed: Option<CreateEmbed>,
}

/// 把 OutboundMessage 轉成 Discord payload。NOTIF_CARD_ONLY_V1：有卡片就只送
/// 卡片（`content` 留空字串），不再額外疊一行「類別：標題」的純文字 content。
fn render_discord_payload(message: &OutboundMessage) -> DiscordPayload {
    match notice_card_for(message) {
        Some(card) => DiscordPayload {
            content: String::new(),
            embed: Some(card.to_embed()),
        },
        None => DiscordPayload {
            content: render_discord_message(message),
            embed: None,
        },
    }
}

/// 每則 OutboundMessage 編成一張卡片。DISCORD_NOTIFY_ALL_CARDS_V1：
/// 點名急件與指令回覆也走 embed；content 仍放「類別｜標題」給鎖屏預覽。
fn notice_card_for(message: &OutboundMessage) -> Option<NoticeCard> {
    match message {
        OutboundMessage::SystemStarted(event) => Some(
            NoticeCard::new(NoticeCategory::System, "⚙️ Tronclass Rollcall 已啟動", &event.account)
                .field("使用者", &event.user_name, true)
                .field("輪詢間隔", format!("{} 秒", event.poll_interval_secs), true)
                .field("Adapter", &event.adapter_name, true),
        ),
        OutboundMessage::HomeworkDeadline(event) => Some(
            NoticeCard::new(
                NoticeCategory::Homework,
                format!("⏳ 作業截止提醒：《{}》", event.course_name),
                &event.account,
            )
            .field("類別", &event.stage_badge, true)
            .field("標題", &event.title, false)
            .field("截止", &event.deadline, true)
            .field("剩餘", &event.time_remaining, true)
            .hint("請儘速至 Tronclass 確認並繳交。"),
        ),
        OutboundMessage::Bulletin(event) => Some(
            NoticeCard::new(
                NoticeCategory::Bulletin,
                format!("📢 新公告：《{}》", event.course_name),
                &event.account,
            )
            .description(if event.summary.trim().is_empty() {
                format!("【{}】", event.title)
            } else {
                format!("【{}】\n{}", event.title, event.summary.trim())
            }),
        ),
        OutboundMessage::ClassReminder(event) => Some(
            NoticeCard::new(
                NoticeCategory::Schedule,
                format!("🔔 上課提醒：《{}》", event.course_name),
                &event.account,
            )
            .field("時間", &event.time_text, true)
            .field("教室", &event.classroom, true)
            .field("老師", &event.teacher, true)
            .hint(format!("你設定的是上課前 {} 分鐘提醒。", event.configured_minutes)),
        ),
        OutboundMessage::TimetableChanged(event) => {
            let mut card = NoticeCard::new(NoticeCategory::Schedule, "📅 課表已自動更新", &event.account);
            if event.added_total > 0 {
                let mut list = event.added.iter().map(|s| format!("＋ {s}")).collect::<Vec<_>>();
                if event.added_total > event.added.len() {
                    list.push(format!("…等 {} 堂", event.added_total));
                }
                card = card.field(&format!("新增 {} 堂", event.added_total), list.join("\n"), false);
            }
            if event.removed_total > 0 {
                let mut list = event.removed.iter().map(|s| format!("－ {s}")).collect::<Vec<_>>();
                if event.removed_total > event.removed.len() {
                    list.push(format!("…等 {} 堂", event.removed_total));
                }
                card = card.field(&format!("移除 {} 堂", event.removed_total), list.join("\n"), false);
            }
            Some(card.hint("可到 Dashboard 課表分頁查看完整內容。"))
        }
        OutboundMessage::PushSubscriptionExpired(event) => Some(
            NoticeCard::new(NoticeCategory::System, "⚙️ 手機推播訂閱已失效", &event.account)
                .field("已移除裝置", format!("{} 台", event.removed_devices), true)
                .field("仍有效裝置", format!("{} 台", event.remaining_devices), true)
                .field("原因", &event.reason, false)
                .hint(if event.dashboard_url.trim().is_empty() {
                    "請用手機開啟 Dashboard，到「手機推播狀態」重新啟用背景推播。".to_string()
                } else {
                    format!("請用手機開啟 {} 重新啟用背景推播。", event.dashboard_url.trim())
                }),
        ),
        OutboundMessage::MonitorEvent(event) => {
            let category = match event.kind {
                crate::adapters::events::MonitorEventKind::ReauthSucceeded => NoticeCategory::Success,
                crate::adapters::events::MonitorEventKind::AutoReauthStarted => NoticeCategory::Warning,
                crate::adapters::events::MonitorEventKind::ReauthFailed
                | crate::adapters::events::MonitorEventKind::AutoReauthFailed
                | crate::adapters::events::MonitorEventKind::LoopCrashed => NoticeCategory::Critical,
            };
            let mut card = NoticeCard::new(
                category,
                format!("{} {}", category.emoji(), event.title()),
                &event.account,
            );
            if let Some(d) = &event.diagnosis {
                card = card.field("原因", &d.label, false).field("建議", &d.advice, false);
            }
            if !event.error.trim().is_empty() {
                card = card.field("錯誤", truncate_embed(event.error.trim(), 300), false);
            }
            let hint = match event.kind {
                crate::adapters::events::MonitorEventKind::LoopCrashed => "監控看門狗會在 30 秒內自動重新登入並重啟此帳號的監控（結果另行通知）；若反覆發生，請檢查系統 Log。",
                crate::adapters::events::MonitorEventKind::AutoReauthFailed => {
                    "系統會以逐步拉長的間隔自動重試；也可輸入 /reauth 立即再試。"
                }
                crate::adapters::events::MonitorEventKind::AutoReauthStarted => "不需要操作，成功後會再通知。",
                crate::adapters::events::MonitorEventKind::ReauthSucceeded => "監控已恢復正常。",
                crate::adapters::events::MonitorEventKind::ReauthFailed => "請確認帳號密碼後再試一次 /reauth。",
            };
            Some(card.hint(hint))
        }
        OutboundMessage::AdminAlert(event) => {
            let category = if event.is_recovery {
                NoticeCategory::Success
            } else if event.grade.contains("嚴重") {
                NoticeCategory::Critical
            } else {
                NoticeCategory::Warning
            };
            Some(
                NoticeCard::new(
                    category,
                    format!("{} {}", category.emoji(), &event.title),
                    &event.account,
                )
                .description(&event.body),
            )
        }
        OutboundMessage::RollcallDetected(event) => Some(
            NoticeCard::new(
                NoticeCategory::Rollcall,
                format!("🚨 偵測到新簽到：《{}》", event.course_name),
                &event.account,
            )
            .field("教師", &event.teacher_name, true)
                .field("類型", &event.attendance_type, true)
                .field("點名 ID", event.rollcall_id.to_string(), true),
        ),
        OutboundMessage::NumberCodeRequested(request) => {
            let mut card = NoticeCard::new(
                NoticeCategory::Rollcall,
                format!("🔢 需要數字點名碼：《{}》", request.course_name),
                &request.account,
            )
            .field("教師", &request.teacher_name, true)
                .field("點名 ID", request.rollcall_id.to_string(), true)
                .field("時限", format!("{} 秒", request.timeout_secs), true);
            if !request.input_url.trim().is_empty() {
                card = card.field("數字點名頁", request.input_url.trim(), false);
            }
            if !request.alt_input_url.trim().is_empty() {
                card = card.field("備用頁", request.alt_input_url.trim(), false);
            }
            Some(card.hint(
                "預設不會自動讀取代碼。請手動輸入 /num code:1234，或開啟數字點名頁／按「自動讀取代碼」。",
            ))
        }
        OutboundMessage::QrCodeRequested(request) => {
            let mut card = NoticeCard::new(
                NoticeCategory::Rollcall,
                format!("📷 需要 QR Code 簽到：《{}》", request.course_name),
                &request.account,
            )
            .field("教師", &request.teacher_name, true)
                .field("點名 ID", request.rollcall_id.to_string(), true)
                .field("時限", format!("{} 秒", request.timeout_secs), true);
            if !request.scan_url.trim().is_empty() {
                card = card.field("掃碼頁", request.scan_url.trim(), false);
            }
            if !request.alt_scan_url.trim().is_empty() {
                card = card.field("備用掃碼頁", request.alt_scan_url.trim(), false);
            }
            Some(card.hint("請在時限內開啟掃碼頁，或把 QR Code 圖片傳給我。"))
        }
        OutboundMessage::RollcallFinished(event) => {
            let title = if event.success {
                format!("✅ 你已完成點名：《{}》", event.course_name)
            } else {
                format!("❌ 點名尚未完成：《{}》", event.course_name)
            };
            let hint = if event.success {
                "你可以到 Web Admin 或 /status 查看紀錄。"
            } else {
                "請重新送出資料、開啟點名頁，或聯絡管理員確認。"
            };
            let elapsed = if event.elapsed_ms >= 1000 {
                format!("{:.1} 秒", event.elapsed_ms as f64 / 1000.0)
            } else {
                format!("{} ms", event.elapsed_ms)
            };
            Some(
                NoticeCard::new(NoticeCategory::Rollcall, title, &event.account)
                    .field("類型", &event.attendance_type, true)
                    .field("點名 ID", event.rollcall_id.to_string(), true)
                    .field("結果", strip_leading_emoji(&event.result), false)
                    .field("耗時", elapsed, true)
                    .hint(hint),
            )
        }
        OutboundMessage::RollcallInputReminder(event) => {
            let title = if event.is_number() {
                format!("⏰ 還沒收到數字點名碼：《{}》", event.course_name)
            } else {
                format!("⏰ 還沒收到 QR Code：《{}》", event.course_name)
            };
            let mut card = NoticeCard::new(NoticeCategory::Rollcall, title, &event.account)
                .field("類型", &event.attendance_type, true)
                .field("點名 ID", event.rollcall_id.to_string(), true)
                .field("已等待", format!("{} 秒", event.waited_secs), true)
                .field("剩餘", format!("{} 秒", event.remaining_secs), true);
            if !event.input_url.trim().is_empty() {
                let label = if event.is_number() {
                    "數字點名頁"
                } else {
                    "掃碼頁"
                };
                card = card.field(label, event.input_url.trim(), false);
            }
            Some(card.hint("逾時後這次點名就會失敗。"))
        }
        OutboundMessage::SecurityAlert(event) if event.waf_block.is_some() => {
            let (reason, path) = waf_block_reason_path(event);
            Some(
                NoticeCard::new(NoticeCategory::Security, "🛡️ WAF 自動封鎖 IP", "")
                    .field("來源 IP", &event.ip, true)
                    .field("觸發原因", reason, false)
                    .field("請求路徑", path, false)
                    .field("時間", &event.timestamp, false)
                    .hint(format!(
                        "{}\n黑名單管理頁：https://dashboard.jasoncloudserver.me/admin/blacklist",
                        waf_block_action(event)
                    )),
            )
        }
        OutboundMessage::SecurityAlert(event) => {
            let action = if event.ip_is_blacklisted {
                "此 IP 已因登入失敗超過十次自動加入黑名單；如需恢復，請按「移除黑名單」。"
            } else if event.login_locked {
                "此帳號的密碼登入已暫時鎖定 10 分鐘（通行密鑰不受影響）。若是你本人：按「取消這次鎖定」。若不是你：按「封鎖此 IP」。"
            } else {
                "請確認後按下「加入黑名單」。"
            };
            Some(
                NoticeCard::new(NoticeCategory::Security, "🔐 Dashboard 安全警示：連續密碼錯誤", "")
                    .field("帳號", &event.username, true)
                    .field("來源 IP", &event.ip, true)
                    .field("帳號失敗次數", event.user_failures.to_string(), true)
                    .field("IP 失敗次數", event.ip_failures.to_string(), true)
                    .field("時間", &event.timestamp, false)
                    .hint(format!(
                        "{action}\n黑名單管理頁：https://dashboard.jasoncloudserver.me/admin/blacklist"
                    )),
            )
        }
        OutboundMessage::FingerprintBlock(event) => Some(
            NoticeCard::new(NoticeCategory::Security, "🛡️ WAF 指紋封鎖（指數時間）", "")
                .field("IP 群組", &event.ip_group, true)
                .field("第幾次", event.strikes.to_string(), true)
                .field("解除時間", &event.blocked_until, true)
                .field("指紋（節錄）", &event.fingerprint_preview, false)
                .field("時間", &event.timestamp, false)
                .hint(
                    "按「放行此組合」後，這個 IP 群組＋此指紋 30 天內不再因指紋被封鎖，到期後恢復檢查（同指紋的其他 IP 不受影響）。\n\
                     未放行時，封鎖會到期自動解除，次數越多封鎖越久（上限 24 小時）。\n\
                     黑名單管理頁：https://dashboard.jasoncloudserver.me/admin/blacklist",
                ),
        ),
        OutboundMessage::GeoAccessRequest(event) => Some(
            NoticeCard::new(NoticeCategory::Security, "🌐 國外 IP 嘗試存取管理後台", "")
                .field("來源 IP", &event.ip, true)
                .field("位置", &event.location, true)
                .field("想前往", if event.path.is_empty() { "/admin" } else { &event.path }, true)
                .field("時間", &event.timestamp, false)
                .hint(format!(
                    "按「暫時放行」可讓這個 IP 在 {} 小時內跳過僅限台灣的限制；按「忽略」維持擋下。\n黑名單管理頁：https://dashboard.jasoncloudserver.me/admin/blacklist",
                    crate::db::GEO_ACCESS_ALLOW_SECONDS / 3600
                )),
        ),
        OutboundMessage::Text(text) => {
            let text = strip_leading_emoji(text);
            // NOTIF_STRUCTURED_V1・fix132：內建「[安全]／[點名]」前綴映射為對應類別卡片。
            let (category, text) = if let Some(rest) = text.trim().strip_prefix("[安全] ") {
                (NoticeCategory::Security, rest.trim())
            } else if let Some(rest) = text.trim().strip_prefix("[點名] ") {
                (NoticeCategory::Rollcall, rest.trim())
            } else {
                (NoticeCategory::System, text.trim())
            };
            let (title, desc) = match text.split_once('\n') {
                Some((first, rest)) if !first.trim().is_empty() => {
                    (first.trim().to_string(), rest.trim().to_string())
                }
                _ if !text.is_empty() => (text.to_string(), String::new()),
                _ => ("📋 通知".to_string(), String::new()),
            };
            let mut card = NoticeCard::new(category, title, "");
            if !desc.is_empty() {
                card = card.description(desc);
            }
            Some(card)
        }
        OutboundMessage::Help => Some(
            NoticeCard::new(NoticeCategory::System, "ℹ️ 指令說明", "").description(help_text()),
        ),
        OutboundMessage::Welcome => Some(
            NoticeCard::new(NoticeCategory::System, "👋 歡迎使用 Tronclass Rollcall", "")
                .description(help_text()),
        ),
        OutboundMessage::UnsupportedMedia => Some(
            NoticeCard::new(NoticeCategory::Warning, "⚠️ 不支援的檔案", "")
                .description("不支援媒體訊息，請傳送文字指令。"),
        ),
        OutboundMessage::LocationReceived {
            latitude,
            longitude,
        } => Some(
            NoticeCard::new(NoticeCategory::System, "📍 已收到位置", "").description(format!(
                "{latitude:.6}, {longitude:.6}（位置功能尚未實作）"
            )),
        ),
        OutboundMessage::Status(status) => Some(
            NoticeCard::new(NoticeCategory::Monitor, "📊 帳號狀態", "")
                .description(render_status_message(status)),
        ),
        OutboundMessage::NotAuthorized => Some(
            NoticeCard::new(NoticeCategory::Warning, "⛔ 權限不足", "")
                .description("你可以查詢自己的帳號狀態，但此操作僅限管理員使用。"),
        ),
        OutboundMessage::MonitorPaused => Some(
            NoticeCard::new(NoticeCategory::Monitor, "⏸️ 簽到監控已暫停", "")
                .hint("暫停期間不會自動點名。用 /start 恢復。"),
        ),
        OutboundMessage::MonitorResumed => Some(
            NoticeCard::new(NoticeCategory::Success, "▶️ 簽到監控已恢復", "")
                .hint("立即執行一次簽到檢查。"),
        ),
        OutboundMessage::ForcePollTriggered => Some(
            NoticeCard::new(NoticeCategory::Monitor, "🔍 已觸發立即簽到檢查", ""),
        ),
        OutboundMessage::ReauthTriggered => Some(
            NoticeCard::new(NoticeCategory::Monitor, "🔄 已觸發重新認證", ""),
        ),
        OutboundMessage::QrAccepted => Some(
            NoticeCard::new(NoticeCategory::Rollcall, "✅ 已收到 QR Code", "")
                .hint("正在嘗試簽到。"),
        ),
        OutboundMessage::QrAmbiguousTarget => Some(
            NoticeCard::new(NoticeCategory::Warning, "⚠️ QR Code 目標不明", "")
                .description("多帳號模式下請由綁定該帳號的使用者傳送 QR Code。"),
        ),
        OutboundMessage::QrNoBoundAccount => Some(
            NoticeCard::new(NoticeCategory::Warning, "⚠️ 尚未綁定帳號", "")
                .description("找不到與你的 Discord 綁定的 Tronclass 帳號。"),
        ),
        OutboundMessage::QrNoPendingRequest => Some(
            NoticeCard::new(NoticeCategory::Warning, "⚠️ 目前沒有待處理的 QR 點名", "")
                .description("目前沒有等待 QR Code 的簽到，或簽到已逾時。"),
        ),
        OutboundMessage::UnknownCommand { text } => Some(
            NoticeCard::new(NoticeCategory::System, "❓ 未知指令", "").description(format!(
                "不認識的指令：{}\n\n{}",
                text.chars().take(80).collect::<String>(),
                help_text()
            )),
        ),
    }
}

/// 純文字 render（測試／攤平 fallback；Discord 送出一律走 notice_card_for）。
fn render_discord_message(message: &OutboundMessage) -> String {
    match message {
        // DISCORD_NOTIFY_UNIFY_V1：Text 仍有多處呼叫端（同時送 LINE 與 Discord），
        // Discord 端統一去掉開頭的表情符號，LINE 文案不動。
        OutboundMessage::Text(text) => strip_leading_emoji(text),
        OutboundMessage::SecurityAlert(event) if event.waf_block.is_some() => waf_alert_text(event),
        OutboundMessage::SecurityAlert(event) => {
            let action = if event.ip_is_blacklisted {
                "此 IP 已因登入失敗超過十次自動加入黑名單；如需恢復，請按「移除黑名單」。"
            } else if event.login_locked {
                "此帳號的密碼登入已暫時鎖定 10 分鐘（通行密鑰不受影響）。\n若是你本人：按「取消這次鎖定」即可立刻重新登入。\n若不是你：按「封鎖此 IP」直接把來源加入黑名單，並盡快變更 Dashboard 密碼。"
            } else {
                "請確認後按下「加入黑名單」。"
            };
            format!(
                "[安全] Dashboard 安全警示：連續密碼錯誤\n\n帳號：{}\n來源 IP：{}\n帳號失敗次數：{}\nIP 失敗次數：{}\n時間：{}\n\n{}\n黑名單管理頁：https://dashboard.jasoncloudserver.me/admin/blacklist",
                event.username,
                event.ip,
                event.user_failures,
                event.ip_failures,
                event.timestamp,
                action,
            )
        },
        OutboundMessage::FingerprintBlock(event) => format!(
            "[安全] WAF 指紋封鎖（第 {} 次）\n\nIP 群組：{}\n解除時間：{}\n指紋（節錄）：{}\n時間：{}\n\n按「放行此組合」可讓這個 IP 群組＋此指紋 30 天內不再因指紋被封鎖；未放行則到期自動解除。",
            event.strikes,
            event.ip_group,
            event.blocked_until,
            event.fingerprint_preview,
            event.timestamp,
        ),
        OutboundMessage::GeoAccessRequest(event) => format!(
            "[安全] 國外 IP 嘗試存取管理後台\n\n來源 IP：{}\n位置：{}\n想前往：{}\n時間：{}\n\n按「暫時放行」可讓這個 IP 在 {} 小時內跳過僅限台灣的限制；按「忽略」維持擋下。\n黑名單管理頁：https://dashboard.jasoncloudserver.me/admin/blacklist",
            event.ip,
            event.location,
            if event.path.is_empty() { "/admin" } else { &event.path },
            event.timestamp,
            crate::db::GEO_ACCESS_ALLOW_SECONDS / 3600,
        ),
        OutboundMessage::SystemStarted(event) => format!(
            "Tronclass Rollcall 已啟動\n帳號：{} / {}\n輪詢間隔：{} 秒\nAdapter：{}",
            event.account, event.user_name, event.poll_interval_secs, event.adapter_name
        ),
        OutboundMessage::RollcallDetected(event) => format!(
            "[點名] 偵測到新簽到\n\n課程：{1}\n教師：{2}\n類型：{3}\n點名 ID：{4}\n帳號：{0}",
            event.account,
            event.course_name,
            event.teacher_name,
            event.attendance_type,
            event.rollcall_id
        ),
        OutboundMessage::NumberCodeRequested(request) => {
            let page = if request.input_url.trim().is_empty() {
                "".to_string()
            } else {
                format!("\n數字點名頁：{}", request.input_url)
            };
            format!(
                "[點名] 需要數字點名碼\n\n課程：{}\n教師：{}\n點名 ID：{}\n帳號：{}\n\n預設不會自動讀取代碼。請在 {} 秒內手動輸入 `/num code:1234`，或開啟數字點名頁 / 按下方「自動讀取代碼」。{}",
                request.course_name,
                request.teacher_name,
                request.rollcall_id,
                request.account,
                request.timeout_secs,
                page,
            )
        },
        OutboundMessage::QrCodeRequested(request) => format!(
            "[點名] 需要 QR Code 簽到\n\n課程：{}\n教師：{}\n點名 ID：{}\n帳號：{}\n\n請在 {} 秒內開啟 scanner 或貼回 QR Code 資料：\n{}",
            request.course_name,
            request.teacher_name,
            request.rollcall_id,
            request.account,
            request.timeout_secs,
            request.scan_url
        ),
        OutboundMessage::RollcallFinished(event) => {
            let title = if event.success {
                "[點名] 你已完成點名"
            } else {
                "[點名] 點名尚未完成"
            };
            let hint = if event.success {
                "你可以到 Web Admin 或 /status 查看紀錄。"
            } else {
                "請重新送出資料、開啟點名頁，或聯絡管理員確認。"
            };
            let elapsed = if event.elapsed_ms >= 1000 {
                format!("{:.1} 秒", event.elapsed_ms as f64 / 1000.0)
            } else {
                format!("{} ms", event.elapsed_ms)
            };
            format!(
                "{title}\n\n課程：{}\n類型：{}\n點名 ID：{}\n結果：{}\n耗時：{}\n帳號：{}\n\n{}",
                event.course_name,
                event.attendance_type,
                event.rollcall_id,
                // DISCORD_NOTIFY_UNIFY_V1：RollcallResult 的 Display 帶 ✅❌ 前綴，Discord 端去掉。
                strip_leading_emoji(&event.result),
                elapsed,
                event.account,
                hint,
            )
        },
        OutboundMessage::Help => help_text().to_string(),
        OutboundMessage::Welcome => format!("歡迎使用 Tronclass Rollcall\n\n{}", help_text()),
        OutboundMessage::UnsupportedMedia => "不支援媒體訊息，請傳送文字指令。".to_string(),
        OutboundMessage::LocationReceived {
            latitude,
            longitude,
        } => format!("收到位置訊息：{latitude:.6}, {longitude:.6}（位置功能尚未實作）"),
        OutboundMessage::Status(status) => render_status_message(status),
        OutboundMessage::NotAuthorized => {
            "你可以查詢自己的帳號狀態，但此操作僅限管理員使用。".to_string()
        }
        OutboundMessage::MonitorPaused => "簽到監控已暫停。".to_string(),
        OutboundMessage::MonitorResumed => "簽到監控已恢復，立即執行一次簽到檢查。".to_string(),
        OutboundMessage::ForcePollTriggered => "已觸發立即簽到檢查。".to_string(),
        OutboundMessage::ReauthTriggered => "已觸發重新認證。".to_string(),
        OutboundMessage::QrAccepted => "已收到 QR Code，正在嘗試簽到。".to_string(),
        OutboundMessage::QrAmbiguousTarget => {
            "多帳號模式下 QR Code 目標不明，請由綁定該帳號的使用者傳送 QR Code。"
                .to_string()
        }
        OutboundMessage::QrNoBoundAccount => {
            "找不到與你的 Discord 綁定的 Tronclass 帳號。".to_string()
        }
        OutboundMessage::QrNoPendingRequest => {
            "目前沒有等待 QR Code 的簽到，或簽到已逾時。".to_string()
        }
        OutboundMessage::HomeworkDeadline(event) => event.render(),
        OutboundMessage::Bulletin(event) => event.render(),
        OutboundMessage::PushSubscriptionExpired(event) => event.render(),
        OutboundMessage::RollcallInputReminder(event) => format!("[點名] {}", event.render()),
        // DISCORD_NOTIFY_UNIFY_V1：這些平常走 embed；純文字 fallback 用同一張卡片攤平。
        OutboundMessage::MonitorEvent(_)
        | OutboundMessage::AdminAlert(_)
        | OutboundMessage::ClassReminder(_)
        | OutboundMessage::TimetableChanged(_) => notice_card_for(message)
            .map(|card| card.to_text())
            .unwrap_or_default(),
        OutboundMessage::UnknownCommand { text } => {
            format!("不認識的指令：{}\n\n{}", text.chars().take(80).collect::<String>(), help_text())
        }
    }
}

fn render_status_message(message: &StatusMessage) -> String {
    match message {
        StatusMessage::NoAccounts => "目前沒有可查詢的 Tronclass 帳號。".to_string(),
        StatusMessage::Single(status) => render_monitor_status(status),
        StatusMessage::UserAccount { account_id, status } => {
            format!(
                "你的 Tronclass 帳號狀態\n帳號 ID：{account_id}\n{}",
                render_monitor_status(status)
            )
        }
        StatusMessage::AdminAccounts(accounts) => {
            let mut lines = vec![format!("系統狀態（{} 個帳號）", accounts.len())];
            for account in accounts {
                let status = &account.status;
                let running = if status.is_running {
                    "運行中"
                } else {
                    "已暫停"
                };
                let last_success = status.last_success_course.as_deref().unwrap_or("無");
                lines.push(format!(
                    "\n帳號：{}\n使用者：{}\n狀態：{}\n最後成功：{}\n連續失敗：{} 次{}",
                    account.account_id,
                    status.user_name,
                    running,
                    last_success,
                    status.consecutive_failures,
                    render_failure_hint(status)
                ));
            }
            lines.join("\n")
        }
    }
}

fn render_monitor_status(status: &MonitorStatus) -> String {
    let running = if status.is_running {
        "運行中"
    } else {
        "已暫停"
    };
    let last_success = status.last_success_course.as_deref().unwrap_or("無");
    format!(
        "狀態：{running}\n帳號：{}\n最後成功：{last_success}\n連續失敗：{} 次{}",
        status.user_name,
        status.consecutive_failures,
        render_failure_hint(status)
    )
}

/// FAILURE_DIAGNOSIS_V1：連續失敗時附上原因分類與自助建議（成功後為空字串）。
fn render_failure_hint(status: &MonitorStatus) -> String {
    match (&status.last_failure, status.consecutive_failures) {
        (Some(d), n) if n > 0 => format!("\n原因：{}\n建議：{}", d.label, d.advice),
        _ => String::new(),
    }
}

fn help_text() -> &'static str {
    "可用指令（斜線回覆只給你自己看）：\n\
/status　查看監控狀態\n\
/force　立刻檢查點名（已綁定會喚醒全部帳號）\n\
/reauth　重新登入學校（一般使用者只重登自己的帳號）\n\
/num　送出 4 位數字碼（手動送出，不受自動讀碼 15%）\n\
/qr　送出課堂 QR 網址或掃描內容\n\
/admin　管理頁連結\n\
/help　顯示這段說明\n\
申請帳號請在「與 bot 的私訊」用 /request-account（群組會拒收，請勿在公開頻道打密碼）。\n\
\n\
管理員另有 /start、/stop、/add-account、/bind-account、/unbind-account、/bindings。"
}

fn string_option(command: &CommandInteraction, name: &str) -> Option<String> {
    command
        .data
        .options
        .iter()
        .find(|option| option.name == name)
        .and_then(|option| option.value.as_str())
        .map(ToOwned::to_owned)
}

fn user_option(command: &CommandInteraction, name: &str) -> Option<String> {
    command
        .data
        .options
        .iter()
        .find(|option| option.name == name)
        .and_then(|option| match option.value {
            CommandDataOptionValue::User(id) => Some(id.to_string()),
            _ => None,
        })
}

fn interaction_reply_token(id: InteractionId, token: &str) -> String {
    format!("{REPLY_INTERACTION_PREFIX}{}:{token}", id.get())
}

fn parse_interaction_reply(value: &str) -> Option<(u64, &str)> {
    let value = value.strip_prefix(REPLY_INTERACTION_PREFIX)?;
    let (id, token) = value.split_once(':')?;
    Some((id.parse().ok()?, token))
}

fn parse_discord_id(value: &str) -> Option<u64> {
    value.trim().parse::<u64>().ok()
}

fn parse_discord_ip(value: &str) -> Option<String> {
    value.trim().parse::<std::net::IpAddr>().ok().map(|ip| ip.to_string())
}

fn parse_block_ip_custom_id(value: &str) -> Option<String> {
    let ip = value.strip_prefix(COMPONENT_BLOCK_IP_PREFIX)?;
    parse_discord_ip(ip)
}

/// WAF 解除按鈕的 custom_id：只接受 WAF 實際寫入的鍵形狀（IPv4 原樣，IPv6 為 `/64` 群組鍵），
/// 其餘一律拒絕，避免 custom_id 被竄改成任意字串傳進 SQL。
/// 解析放行按鈕的 custom_id：只接受 `hash16|群組` 格式，避免任意字串進資料庫查詢。
fn parse_fp_release_custom_id(value: &str) -> Option<String> {
    let raw = value.strip_prefix(COMPONENT_FP_RELEASE_PREFIX)?;
    let (hash, group) = raw.split_once('|')?;
    let well_formed = hash.len() == 16
        && hash.chars().all(|c| c.is_ascii_hexdigit())
        && !group.is_empty()
        && group.len() <= 64;
    well_formed.then(|| raw.to_string())
}

fn parse_unblock_waf_custom_id(value: &str) -> Option<String> {
    let raw = value.strip_prefix(COMPONENT_UNBLOCK_WAF_PREFIX)?.trim();
    let host = raw.strip_suffix("/64").unwrap_or(raw);
    let ip: std::net::IpAddr = host.parse().ok()?;
    let key = crate::trusted_ip::blacklist_group_key(&ip.to_string());
    if key == raw {
        Some(key)
    } else {
        None
    }
}

fn waf_block_reason_path(event: &SecurityAlertEvent) -> (&str, &str) {
    event
        .waf_block
        .as_ref()
        .map(|waf| (waf.reason.as_str(), waf.path.as_str()))
        .unwrap_or(("", ""))
}

/// WAF_AUTO_BLOCK_NOTIFY_V2：依封鎖類型說明會不會自動解除。
fn waf_block_action(event: &SecurityAlertEvent) -> &'static str {
    match event.waf_block.as_ref() {
        Some(waf) if waf.permanent => {
            "攻擊特徵類封鎖為永久。若確認是誤判，按「解除封鎖」即可立即移除。"
        }
        _ => "異常 4xx 類封鎖 30 分鐘後自動解除。若要立即放行，按「解除封鎖」。",
    }
}

/// WAF 告警的純文字版（與卡片內容相同）。
fn waf_alert_text(event: &SecurityAlertEvent) -> String {
    let (reason, path) = waf_block_reason_path(event);
    format!(
        "[安全] WAF 自動封鎖 IP\n\n來源 IP：{}\n觸發原因：{}\n請求路徑：{}\n時間：{}\n\n{}\n同一 IP 24 小時內只通知一次。\n黑名單管理頁：https://dashboard.jasoncloudserver.me/admin/blacklist",
        event.ip,
        reason,
        path,
        event.timestamp,
        waf_block_action(event),
    )
}

fn parse_unblock_ip_custom_id(value: &str) -> Option<String> {
    let ip = value.strip_prefix(COMPONENT_UNBLOCK_IP_PREFIX)?;
    parse_discord_ip(ip)
}

/// `geo_access_requests.id` 是 `Uuid::new_v4().simple()`（32 個小寫十六進位字元，
/// 不含連字號）；只接受這個形狀，避免 custom_id 被竄改成任意字串傳進 SQL 查詢鍵。
fn parse_geo_request_id(value: &str) -> Option<String> {
    if value.len() == 32 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(value.to_string())
    } else {
        None
    }
}

fn parse_geo_allow_custom_id(value: &str) -> Option<String> {
    parse_geo_request_id(value.strip_prefix(COMPONENT_GEO_ALLOW_PREFIX)?)
}

fn parse_geo_ignore_custom_id(value: &str) -> Option<String> {
    parse_geo_request_id(value.strip_prefix(COMPONENT_GEO_IGNORE_PREFIX)?)
}

/// LOGIN_TIERED_LOCKOUT_V1：解析 `{prefix}{account_id}|{ip}` 形式的 custom_id。
fn parse_prefixed_account_ip(value: &str, prefix: &str) -> Option<(String, String)> {
    let value = value.strip_prefix(prefix)?;
    let (account_id, ip) = value.split_once('|')?;
    if account_id.trim().is_empty() || account_id.len() > 64 { return None; }
    Some((account_id.trim().to_string(), parse_discord_ip(ip)?))
}

fn parse_unblock_own_ip_custom_id(value: &str) -> Option<(String, String)> {
    let value = value.strip_prefix(COMPONENT_UNBLOCK_OWN_IP_PREFIX)?;
    let (account_id, ip) = value.split_once('|')?;
    if account_id.trim().is_empty() || account_id.len() > 64 { return None; }
    Some((account_id.to_string(), parse_discord_ip(ip)?))
}

