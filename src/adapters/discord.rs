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
    AdapterAccountTarget, AdapterMessenger, MonitorStatus, OutboundMessage, StatusMessage,
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
const COMPONENT_ACCOUNT_APPROVE_PREFIX: &str = "trc:account-approve:";
const COMPONENT_BLOCK_IP_PREFIX: &str = "trc:block-ip:";
const COMPONENT_UNBLOCK_IP_PREFIX: &str = "trc:unblock-ip:";
const COMPONENT_UNBLOCK_OWN_IP_PREFIX: &str = "trc:unblock-own-ip:";
/// LOGIN_TIERED_LOCKOUT_V1：帳號持有人取消第一層登入鎖定 / 直接封鎖來源 IP。
/// payload：`{account_id}|{ip}`；account_id 只做顯示與鍵值，授權以資料庫的 Discord 綁定為準。
const COMPONENT_CANCEL_LOCK_PREFIX: &str = "trc:cancel-lock:";
const COMPONENT_OWNER_BLOCK_IP_PREFIX: &str = "trc:owner-block-ip:";
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
        OutboundMessage::AdminAlert(_) => "other",
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
            let mut builder = CreateInteractionResponseMessage::new()
                .content(payload.content)
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
                    RequestCommand::QrCode(qr_data),
                    &reply_token,
                    &self.requests,
                    &user_id,
                )
                .await
            }
            "num" => {
                let code = string_option(&command, "code").unwrap_or_default();
                requests::execute_command(
                    RequestCommand::NumberCode(code),
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

        if let Some(account_id) = parse_number_auto_custom_id(&component.data.custom_id) {
            requests::execute_command(
                RequestCommand::NumberAutoLookup {
                    account_id: Some(account_id.to_string()),
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
                // LOGIN_UNLOCK_ALSO_IP_V1：來源 IP 的失敗計數一併歸零（避免解鎖後再錯兩下就進黑名單）。
                crate::adapters::admin_web::clear_ip_failure_count(&ip).await;
                crate::logs::record_audit(
                    "discord",
                    &user_id,
                    "login_lock_cancel",
                    Some(&account_id),
                    Some(&ip),
                    "帳號持有人由 Discord 取消登入鎖定",
                );
                // LOGIN_UNLOCK_ALSO_IP_V1：舊版把鎖定期間的每次重試都算失敗，持有人常常
                // 在按到「取消這次鎖定」前就已把自己的 IP 推進第二層自動黑名單——鎖解了、
                // 頁面重整還是進不去（/admin 直接回封鎖頁）。這裡順手把「這個 IP 的自動黑名單」
                // 一併解除（只限 system:auto-login-failure 且屬於此 Discord 使用者的帳號；
                // 管理員手動封鎖的不動）。
                let ip_unblocked = if is_admin {
                    self.accounts_db
                        .unblock_automatic_ip_for_discord_user(&ip, &user_id)
                        .await
                        .unwrap_or(false)
                        || match self.accounts_db.auto_block_owner_for_ip(&ip).await {
                            Ok(Some(owner)) if owner == account_id => {
                                self.accounts_db.unblock_ip(&ip).await.unwrap_or(false)
                            }
                            _ => false,
                        }
                } else {
                    self.accounts_db
                        .unblock_automatic_ip_for_discord_user(&ip, &user_id)
                        .await
                        .unwrap_or(false)
                };
                if ip_unblocked {
                    crate::logs::record_audit(
                        "discord",
                        &user_id,
                        "ip_blacklist_owner_unblock",
                        Some(&account_id),
                        Some(&ip),
                        "取消登入鎖定時一併解除來源 IP 的自動黑名單",
                    );
                }
                let ip_note = if ip_unblocked {
                    format!("\n來源 IP `{ip}` 的自動黑名單也已一併解除。")
                } else {
                    String::new()
                };
                if removed {
                    OutboundMessage::Text(format!("[安全] 已取消帳號 `{account_id}` 的登入鎖定，現在可以直接重新登入（登入頁會自動解除鎖定狀態，不必重整）。{ip_note}"))
                } else {
                    OutboundMessage::Text(format!("[安全] 帳號 `{account_id}` 目前沒有生效中的鎖定（可能已到期），可直接登入。{ip_note}"))
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

fn discord_commands() -> Vec<CreateCommand> {
    vec![
        CreateCommand::new("status").description("查看目前監控狀態"),
        CreateCommand::new("start").description("啟動簽到監控"),
        CreateCommand::new("stop").description("暫停簽到監控"),
        CreateCommand::new("force").description("立即觸發一次簽到檢查"),
        CreateCommand::new("reauth").description("重新登入"),
        CreateCommand::new("admin").description("取得 Web 管理頁連結"),
        CreateCommand::new("help").description("顯示說明"),
        CreateCommand::new("qr")
            .description("回傳 QR Code 掃描資料")
            .add_option(
                CreateCommandOption::new(CommandOptionType::String, "data", "QR Code URL 或資料")
                    .required(true),
            ),
        CreateCommand::new("num")
            .description("回傳數字點名 4 位數代碼")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "code",
                    "4 位數字點名碼，例如 1234",
                )
                .required(true),
            ),
        CreateCommand::new("request-account")
            .description("使用者：申請新增 Tronclass 帳號，需管理員核准")
            .add_option(
                CreateCommandOption::new(CommandOptionType::String, "provider", "Provider 名稱")
                    .required(true),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "username",
                    "Tronclass username",
                )
                .required(true),
            )
            .add_option(
                CreateCommandOption::new(CommandOptionType::String, "password", "Tronclass 密碼")
                    .required(true),
            ),
        CreateCommand::new("add-account")
            .description("管理員：新增 Tronclass 帳號並可立即綁定 Discord 使用者")
            .add_option(
                CreateCommandOption::new(CommandOptionType::String, "provider", "Provider 名稱")
                    .required(true),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "username",
                    "Tronclass username",
                )
                .required(true),
            )
            .add_option(
                CreateCommandOption::new(CommandOptionType::String, "password", "Tronclass 密碼")
                    .required(true),
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
                    "DB account id；留空時使用 Tronclass username",
                )
                .required(false),
            ),
        CreateCommand::new("bind-account")
            .description("管理員：用 Tronclass username 綁定 Discord 使用者")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "username",
                    "Tronclass username",
                )
                .required(true),
            )
            .add_option(
                CreateCommandOption::new(CommandOptionType::User, "user", "Discord 使用者")
                    .required(true),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "provider",
                    "Provider 名稱；同 username 多帳號時用來消除歧義",
                )
                .required(false),
            ),
        CreateCommand::new("unbind-account")
            .description("管理員：用 Tronclass username 解除 Discord 綁定")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "username",
                    "Tronclass username",
                )
                .required(true),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "provider",
                    "Provider 名稱；同 username 多帳號時用來消除歧義",
                )
                .required(false),
            ),
        CreateCommand::new("bindings").description("管理員：列出 Discord 綁定"),
    ]
}

#[cfg(test)]
fn discord_command_names() -> Vec<&'static str> {
    vec![
        "status",
        "start",
        "stop",
        "force",
        "reauth",
        "admin",
        "help",
        "qr",
        "num",
        "request-account",
        "add-account",
        "bind-account",
        "unbind-account",
        "bindings",
    ]
}

async fn send_outbound_to_channel(
    http: &Arc<Http>,
    channel_id: ChannelId,
    message: &OutboundMessage,
) -> Result<()> {
    throttle_discord_http().await;
    // DISCORD_NOTIFY_UNIFY_V1：統一 payload（純文字或 embed 卡片）。
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
                .label("Status")
                .style(ButtonStyle::Primary),
            CreateButton::new(COMPONENT_FORCE)
                .label("Force")
                .style(ButtonStyle::Secondary),
            CreateButton::new(COMPONENT_REAUTH)
                .label("Reauth")
                .style(ButtonStyle::Secondary),
            CreateButton::new(COMPONENT_HELP)
                .label("Help")
                .style(ButtonStyle::Secondary),
        ])],
        OutboundMessage::NumberCodeRequested(request) => {
            let mut buttons = vec![
                CreateButton::new(format!("{COMPONENT_NUMBER_AUTO_PREFIX}{}", request.account))
                    .label("自動讀取代碼")
                    .style(ButtonStyle::Primary),
                CreateButton::new(COMPONENT_STATUS)
                    .label("Status")
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
                        "Open scanner"
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
                    .label("Status")
                    .style(ButtonStyle::Secondary),
            );
            vec![CreateActionRow::Buttons(buttons)]
        }
        OutboundMessage::QrCodeRequested(request) => vec![
            CreateActionRow::Buttons(vec![
                CreateButton::new_link(&request.scan_url).label("Open scanner")
            ]),
            CreateActionRow::Buttons(vec![CreateButton::new(COMPONENT_STATUS)
                .label("Status")
                .style(ButtonStyle::Primary)]),
        ],
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
        RequestCommand::QrCode(_) => Some("text_qr"),
        RequestCommand::NumberCode(_) => Some("text_num"),
        RequestCommand::NumberAutoLookup { .. } => Some("text_number_auto_lookup"),
        RequestCommand::Help => Some("text_help"),
        RequestCommand::Unknown(_) => None,
    }
}

fn discord_component_audit(custom_id: &str) -> (&'static str, Option<String>) {
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
        "待審核 Tronclass 帳號新增\n申請人：<@{}>\nProvider：`{}`\nUsername：`{}`\n\n密碼不會顯示在控制台；核准後會寫入 accounts.db、綁定申請人並立即在背景啟動監控（不需重啟服務）。",
        request.discord_user_id, request.provider, request.username
    )
}

// ─── DISCORD_NOTIFY_UNIFY_V1：統一通知模板 ─────────────────────────────────
//
// 所有 Discord 出口（DM、管理員頻道、slash 回覆、按鈕回覆）都經過
// `render_discord_payload`。兩種呈現方式：
//
// * **純文字**：急件（點名偵測／需要輸入代碼／點名結果／安全警示）與指令回覆。
//   手機鎖屏預覽能看到完整內容，不受 embed 只顯示標題的限制。
// * **Embed 卡片**：其餘背景通知（作業、公告、上課提醒、課表更新、監控事件、
//   管理員警示、推播失效、系統啟動）。依類別上色、欄位對齊，頻道內一眼可分類。
//
// 兩種格式共用 `NoticeCard`：標題／類別／帳號／欄位／結尾提示，文字版由同一
// 張卡片攤平而成，確保 DM 與頻道、Discord 與 log 說的是同一件事。
// 不使用表情符號（與專案通知規範一致），改以 embed 顏色與類別文字區分。

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

/// 統一卡片：所有背景通知都先組成這個結構，再依出口決定 embed 或純文字。
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
        let mut lines = vec![format!("[{}] {}", self.category.label(), self.title)];
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

/// 一次 Discord 送出的內容：`content` 一定有（純文字或 embed 的一行標題），
/// `embed` 只在卡片式通知時存在。
struct DiscordPayload {
    content: String,
    embed: Option<CreateEmbed>,
}

/// 把 OutboundMessage 轉成 Discord payload。卡片式通知回 embed＋一行 content
/// （手機通知預覽顯示這一行），其餘回純文字。
fn render_discord_payload(message: &OutboundMessage) -> DiscordPayload {
    match notice_card_for(message) {
        Some(card) => DiscordPayload {
            // content 放「類別｜標題」一行，讓推播預覽與關閉 embed 的使用者也看得懂。
            content: format!("{}｜{}", card.category.label(), card.title),
            embed: Some(card.to_embed()),
        },
        None => DiscordPayload {
            content: render_discord_message(message),
            embed: None,
        },
    }
}

/// 哪些訊息走 embed 卡片。急件（點名相關、安全警示）與指令回覆刻意回 None → 純文字。
fn notice_card_for(message: &OutboundMessage) -> Option<NoticeCard> {
    match message {
        OutboundMessage::SystemStarted(event) => Some(
            NoticeCard::new(NoticeCategory::System, "Tronclass Rollcall 已啟動", &event.account)
                .field("使用者", &event.user_name, true)
                .field("輪詢間隔", format!("{} 秒", event.poll_interval_secs), true)
                .field("Adapter", &event.adapter_name, true),
        ),
        OutboundMessage::HomeworkDeadline(event) => Some(
            NoticeCard::new(
                NoticeCategory::Homework,
                format!("作業截止提醒｜{}", event.stage_badge),
                &event.account,
            )
            .field("課程", &event.course_name, false)
            .field("標題", &event.title, false)
            .field("截止", &event.deadline, true)
            .field("剩餘", &event.time_remaining, true)
            .hint("請儘速至 Tronclass 確認並繳交。"),
        ),
        OutboundMessage::Bulletin(event) => Some(
            NoticeCard::new(
                NoticeCategory::Bulletin,
                format!("新公告｜{}", event.course_name),
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
                format!("上課提醒｜{}", event.headline()),
                &event.account,
            )
            .field("課程", &event.course_name, false)
            .field("時間", &event.time_text, true)
            .field("教室", &event.classroom, true)
            .field("老師", &event.teacher, true)
            .hint(format!("你設定的是上課前 {} 分鐘提醒。", event.configured_minutes)),
        ),
        OutboundMessage::TimetableChanged(event) => {
            let mut card = NoticeCard::new(NoticeCategory::Schedule, "課表已自動更新", &event.account);
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
            NoticeCard::new(NoticeCategory::System, "手機推播訂閱已失效", &event.account)
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
            let mut card = NoticeCard::new(category, event.title(), &event.account);
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
                NoticeCard::new(category, &event.title, &event.account)
                    .description(&event.body),
            )
        }
        _ => None,
    }
}

/// 純文字 render（急件、指令回覆，以及所有沒有卡片的訊息）。
fn render_discord_message(message: &OutboundMessage) -> String {
    match message {
        // DISCORD_NOTIFY_UNIFY_V1：Text 仍有多處呼叫端（同時送 LINE 與 Discord），
        // Discord 端統一去掉開頭的表情符號，LINE 文案不動。
        OutboundMessage::Text(text) => strip_leading_emoji(text),
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
    "可用指令：\n/status - 查看狀態\n/admin - 取得 Web 管理頁連結\n/force - 立即觸發簽到檢查（已綁定使用者會觸發全部帳號一起檢查）\n/reauth - 重新認證（一般使用者只重登自己的綁定帳號；管理員重登全部）\n/help - 顯示說明\n/qr data:<資料> - 回傳 QR Code\n/num code:<4位數> - 回傳數字點名碼\n/request-account provider:<name> username:<Tronclass username> password:<password> - 在 DM 申請新增帳號\n\n管理員可在控制台使用 /start、/stop、/add-account、/bind-account、/unbind-account、/bindings。"
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

fn parse_unblock_ip_custom_id(value: &str) -> Option<String> {
    let ip = value.strip_prefix(COMPONENT_UNBLOCK_IP_PREFIX)?;
    parse_discord_ip(ip)
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn test_bot() -> Arc<DiscordBotClient> {
        Arc::new(DiscordBotClient {
            http: Arc::new(Http::new("secret-token")),
            bot_token: "secret-token".to_string(),
            admin_user_id: "1".to_string(),
            admin_channel_id: "2".to_string(),
            public_base_url: "https://admin.example.test".to_string(),
            dm_channels: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    async fn test_runtime(db: AccountsDb) -> DiscordRuntime {
        let bot = test_bot();
        let messenger = Arc::clone(&bot) as Arc<dyn AdapterMessenger>;
        DiscordRuntime {
            bot,
            requests: RequestState::new_with_binding(
                messenger,
                requests::AccountRegistry::default(),
                requests::AdapterBindingKind::Discord,
            ),
            accounts_db: db,
            register_commands: false,
            guild_ids: vec![],
            pending_accounts: Arc::new(Mutex::new(HashMap::new())),
            monitor_spawner: None,
        }
    }

    // ── DISCORD_NOTIFY_UNIFY_V1 ──────────────────────────────────────────────
    fn has_emoji(text: &str) -> bool {
        text.chars().any(|c| {
            matches!(c as u32, 0x2600..=0x27BF | 0x1F300..=0x1FAFF | 0xFE0F | 0x2139)
        })
    }

    #[test]
    fn unify_urgent_messages_are_plain_text() {
        let detected = OutboundMessage::RollcallDetected(crate::adapters::events::RollcallEvent {
            rollcall_id: 1,
            account: "student001".to_string(),
            course_name: "資料結構".to_string(),
            teacher_name: "王老師".to_string(),
            attendance_type: "數字".to_string(),
        });
        let payload = render_discord_payload(&detected);
        assert!(payload.embed.is_none(), "點名偵測必須是純文字");
        assert!(payload.content.starts_with("[點名] "));
        assert!(!has_emoji(&payload.content));

        let alert = OutboundMessage::SecurityAlert(crate::adapters::events::SecurityAlertEvent {
            account_id: "acc".to_string(),
            username: "student001".to_string(),
            ip: "203.0.113.9".to_string(),
            user_failures: 5,
            ip_failures: 7,
            timestamp: "2026-09-08 12:00".to_string(),
            ip_is_blacklisted: false,
            allow_owner_unblock: false,
            login_locked: false,
        });
        let payload = render_discord_payload(&alert);
        assert!(payload.embed.is_none(), "安全警示必須是純文字");
        assert!(payload.content.starts_with("[安全] "));
        assert!(!has_emoji(&payload.content));
    }

    #[test]
    fn unify_background_notices_are_embeds_with_uniform_content_line() {
        let msgs = vec![
            OutboundMessage::ClassReminder(crate::adapters::events::ClassReminderEvent {
                account: "student001".to_string(),
                course_name: "資料結構".to_string(),
                time_text: "08:10 - 09:00（第 1-2 節）".to_string(),
                classroom: "E101".to_string(),
                teacher: "王老師".to_string(),
                minutes_until: 10,
                configured_minutes: 10,
            }),
            OutboundMessage::TimetableChanged(crate::adapters::events::TimetableChangedEvent {
                account: "student001".to_string(),
                added: vec!["週一 第1節 資料結構".to_string()],
                added_total: 1,
                removed: vec![],
                removed_total: 0,
            }),
            OutboundMessage::MonitorEvent(crate::adapters::events::MonitorEventNotice {
                kind: crate::adapters::events::MonitorEventKind::AutoReauthFailed,
                account: "student001".to_string(),
                error: "HTTP 401".to_string(),
                diagnosis: Some(crate::monitor_alerts::diagnose("HTTP 401", true)),
            }),
            OutboundMessage::AdminAlert(crate::adapters::events::AdminAlertNotice {
                account: "student001".to_string(),
                grade: "嚴重".to_string(),
                is_recovery: false,
                title: "監控警示（嚴重）：student001".to_string(),
                body: "帳號：student001\n連續失敗：5 次".to_string(),
            }),
        ];
        for msg in msgs {
            let payload = render_discord_payload(&msg);
            assert!(payload.embed.is_some(), "{msg:?} 應為 embed 卡片");
            assert!(payload.content.contains('｜'), "content 需為「類別｜標題」：{}", payload.content);
            assert!(!has_emoji(&payload.content));
            let text = render_discord_message(&msg);
            assert!(text.contains("student001"), "純文字 fallback 需含帳號：{text}");
            assert!(!has_emoji(&text));
        }
    }

    #[test]
    fn unify_text_messages_drop_leading_emoji_only() {
        assert_eq!(strip_leading_emoji("✅ 已封鎖 IP"), "已封鎖 IP");
        assert_eq!(strip_leading_emoji("⚠️ 自動讀取失敗"), "自動讀取失敗");
        assert_eq!(strip_leading_emoji("ℹ️ 找不到"), "找不到");
        assert_eq!(strip_leading_emoji("內文中的 ✅ 保留"), "內文中的 ✅ 保留");
        assert_eq!(strip_leading_emoji("✅"), "✅");
        let payload = render_discord_payload(&OutboundMessage::Text("❌ 失敗：x".to_string()));
        assert_eq!(payload.content, "失敗：x");
        assert!(payload.embed.is_none());
    }

    #[test]
    fn render_qr_request_contains_scanner_url() {
        let msg = OutboundMessage::QrCodeRequested(crate::adapters::events::QrCodeRequest {
            rollcall_id: 42,
            account: "acc1".to_string(),
            course_name: "資料結構".to_string(),
            teacher_name: "王老師".to_string(),
            scan_url: "https://example.test/scanner".to_string(),
            alt_scan_url: String::new(),
            timeout_secs: 60,
        });

        let text = render_discord_message(&msg);
        assert!(text.contains("資料結構"));
        assert!(text.contains("https://example.test/scanner"));
        assert!(!action_rows_for_message(&msg).is_empty());
    }

    #[test]
    fn interaction_reply_token_roundtrips() {
        let token = interaction_reply_token(InteractionId::new(123), "abc");
        assert_eq!(parse_interaction_reply(&token), Some((123, "abc")));
    }

    #[test]
    fn discord_bot_debug_redacts_token() {
        let bot = test_bot();
        let text = format!("{bot:?}");
        assert!(text.contains("<redacted>"));
        assert!(!text.contains("secret-token"));
    }

    #[test]
    fn commands_include_binding_console_commands() {
        let names = discord_command_names();

        assert!(names.contains(&"request-account"));
        assert!(names.contains(&"bind-account"));
        assert!(names.contains(&"unbind-account"));
        assert!(names.contains(&"bindings"));
        assert!(names.contains(&"status"));
        assert_eq!(discord_commands().len(), names.len());
    }

    #[test]
    fn account_review_custom_id_roundtrips() {
        let approve = account_review_custom_id(AccountReviewAction::Approve, "token");
        let reject = account_review_custom_id(AccountReviewAction::Reject, "token");

        assert_eq!(
            parse_account_review_custom_id(&approve),
            Some((AccountReviewAction::Approve, "token"))
        );
        assert_eq!(
            parse_account_review_custom_id(&reject),
            Some((AccountReviewAction::Reject, "token"))
        );
        assert_eq!(parse_account_review_custom_id("trc:status"), None);
    }

    #[test]
    fn pending_account_admin_message_redacts_password() {
        let request = PendingAccountRequest {
            provider: "fju".to_string(),
            username: "student001".to_string(),
            password: "super-secret".to_string(),
            discord_user_id: "123".to_string(),
        };

        let text = render_pending_account_request(&request);
        assert!(text.contains("student001"));
        assert!(text.contains("<@123>"));
        assert!(!text.contains("super-secret"));
    }

    #[tokio::test]
    async fn add_account_inserts_tronclass_username_account() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let runtime = test_runtime(db.clone()).await;

        let message = runtime
            .add_account("fju", "student001", "secret", "", "", None)
            .await;
        assert!(render_discord_message(&message).contains("student001"));

        let account = db.get("student001").await.unwrap().unwrap();
        assert_eq!(account.provider, "fju");
        assert_eq!(account.username, "student001");
        assert_eq!(account.password, "secret");
    }

    #[tokio::test]
    async fn approve_pending_account_inserts_requested_account() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let runtime = test_runtime(db.clone()).await;

        runtime.pending_accounts.lock().await.insert(
            "token".to_string(),
            PendingAccountRequest {
                provider: "fju".to_string(),
                username: "student002".to_string(),
                password: "secret".to_string(),
                discord_user_id: String::new(),
            },
        );

        let message = runtime.approve_pending_account("token", None).await;
        assert!(render_discord_message(&message).contains("已核准"));

        let account = db.get("student002").await.unwrap().unwrap();
        assert_eq!(account.provider, "fju");
        assert_eq!(account.username, "student002");
        assert_eq!(account.password, "secret");
        assert!(runtime.pending_accounts.lock().await.is_empty());
    }

    #[tokio::test]
    async fn reject_pending_account_removes_request_without_insert() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let runtime = test_runtime(db.clone()).await;

        runtime.pending_accounts.lock().await.insert(
            "token".to_string(),
            PendingAccountRequest {
                provider: "fju".to_string(),
                username: "student003".to_string(),
                password: "secret".to_string(),
                discord_user_id: String::new(),
            },
        );

        let message = runtime.reject_pending_account("token").await;
        assert!(render_discord_message(&message).contains("已拒絕"));
        assert!(db.get("student003").await.unwrap().is_none());
        assert!(runtime.pending_accounts.lock().await.is_empty());
    }

    #[tokio::test]
    async fn bind_account_uses_tronclass_username() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        db.insert(&RawAccountConfig {
            id: "db-id".to_string(),
            provider: "fju".to_string(),
            username: "student001".to_string(),
            password: "secret".to_string(),
            enabled: true,
            line_user_id: String::new(),
            discord_user_id: "old".to_string(),
            session_cookie: String::new(),
            is_admin: false,
            is_viewer: false,
            dashboard_password_hash: String::new(),
        })
        .await
        .unwrap();
        let runtime = test_runtime(db.clone()).await;

        let message = runtime
            .bind_account_by_username("student001", None, "")
            .await;
        assert!(render_discord_message(&message).contains("student001"));

        let account = db.get("db-id").await.unwrap().unwrap();
        assert!(account.discord_user_id.is_empty());
    }
}
