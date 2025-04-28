use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use dotenv::dotenv;
use futures::future::BoxFuture;
use log::{debug, error, info, warn};
use teloxide::dispatching::Dispatcher;
use teloxide::prelude::*;
use teloxide::types::{ChatId, ChatMemberStatus, Message, MessageId, UserId};
use tokio::sync::Mutex;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, Clone)]
enum Command {
    Start,
    Confirm,
}

impl Command {
    fn parse(text: &str) -> Option<Self> {
        let cmd_text = text.split('@').next().unwrap_or(text).trim();
        match cmd_text {
            "/start" => Some(Command::Start),
            "/confirm" => Some(Command::Confirm),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct ForbiddenPatterns {
    starts_with: Vec<String>,
    contains: Vec<String>,
}

impl ForbiddenPatterns {
    fn load(path: &str) -> Self {
        info!("Loading forbidden patterns from {}", path);
        let mut starts_with = Vec::new();
        let mut contains = Vec::new();

        if Path::new(path).exists() {
            if let Ok(file) = File::open(path) {
                for line in BufReader::new(file).lines().flatten() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    if line.starts_with('*') {
                        contains.push(line[1..].trim().to_lowercase());
                    } else {
                        starts_with.push(line.trim().to_lowercase());
                    }
                }
            }
        }

        info!(
            "Loaded {} starts_with and {} contains patterns",
            starts_with.len(),
            contains.len()
        );
        Self {
            starts_with,
            contains,
        }
    }

    fn matches(&self, text: &str) -> bool {
        let text = text.trim().to_lowercase();
        self.starts_with.iter().any(|p| text.starts_with(p))
            || self.contains.iter().any(|p| text.contains(p))
    }
}

struct BotState {
    whitelist: Mutex<HashSet<UserId>>,
    whitelist_file: String,
    group_chat_id: ChatId,
    forbidden_patterns: Arc<Mutex<ForbiddenPatterns>>,
}

impl BotState {
    fn new(group_chat_id: ChatId, whitelist_file: &str, patterns_file: &str) -> Self {
        info!("Initializing BotState for group {}", group_chat_id.0);
        Self {
            whitelist: Mutex::new(Self::load_whitelist(whitelist_file)),
            whitelist_file: whitelist_file.to_string(),
            group_chat_id,
            forbidden_patterns: Arc::new(Mutex::new(ForbiddenPatterns::load(patterns_file))),
        }
    }

    fn load_whitelist(path: &str) -> HashSet<UserId> {
        info!("Loading whitelist from {}", path);
        let mut whitelist = HashSet::new();

        if !Path::new(path).exists() {
            warn!(
                "Whitelist file {} does not exist, creating empty whitelist",
                path
            );
            return whitelist;
        }

        match File::open(path) {
            Ok(file) => {
                for line in BufReader::new(file).lines().flatten() {
                    if let Ok(id) = line.split_whitespace().next().unwrap_or("").parse::<u64>() {
                        whitelist.insert(UserId(id));
                    }
                }
                info!("Loaded {} whitelisted users", whitelist.len());
            }
            Err(e) => error!("Failed to load whitelist: {}", e),
        }
        whitelist
    }

    async fn add_to_whitelist(&self, user_id: UserId, username: &str) -> Result<()> {
        info!("Adding user {} ({}) to whitelist", user_id.0, username);
        let mut whitelist = self.whitelist.lock().await;
        if whitelist.insert(user_id) {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.whitelist_file)?;
            writeln!(file, "{} {}", user_id.0, username)?;
            info!("Successfully added user {} to whitelist file", user_id.0);
        } else {
            warn!("User {} was already in whitelist", user_id.0);
        }
        Ok(())
    }

    async fn is_whitelisted(&self, user_id: UserId) -> bool {
        let whitelist = self.whitelist.lock().await;
        let is_whitelisted = whitelist.contains(&user_id);
        debug!(
            "Checking if user {} is whitelisted: {}",
            user_id.0, is_whitelisted
        );
        is_whitelisted
    }

    async fn check_message(&self, text: &str) -> bool {
        let patterns = self.forbidden_patterns.lock().await;
        let matches = patterns.matches(text);
        if matches {
            warn!("Message matched forbidden patterns: '{}'", text);
        }
        matches
    }
}

async fn retry_telegram_request<F, T>(action: F, action_name: &str) -> Result<T>
where
    F: Fn() -> BoxFuture<'static, Result<T>>,
{
    let mut attempts = 0;
    let max_attempts = 3; // Уменьшено количество попыток для более быстрого реагирования
    let mut last_error = None;

    while attempts < max_attempts {
        attempts += 1;
        match action().await {
            Ok(result) => {
                info!(
                    "Successfully completed {} after {} attempts",
                    action_name, attempts
                );
                return Ok(result);
            }
            Err(e) => {
                last_error = Some(e);
                let delay = Duration::from_millis(500 * attempts); // Уменьшена задержка между попытками
                warn!(
                    "Attempt {} of {} failed for {}: {:?}. Retrying in {:?}",
                    attempts, max_attempts, action_name, last_error, delay
                );
                tokio::time::sleep(delay).await;
            }
        }
    }

    error!(
        "Failed to complete {} after {} attempts. Last error: {:?}",
        action_name, max_attempts, last_error
    );
    Err(last_error.unwrap())
}

async fn handle_start(bot: Bot, msg: Message, state: Arc<BotState>) -> Result<()> {
    let user = match msg.from.as_ref() {
        Some(user) => {
            info!(
                "Received /start from user {} ({} @{}) in chat {}",
                user.id,
                user.full_name(),
                user.username.as_deref().unwrap_or(""),
                msg.chat.id
            );
            user
        }
        None => {
            warn!("Received /start without user info");
            return Ok(());
        }
    };

    let text = if state.is_whitelisted(user.id).await {
        "✅ Вы уже подтверждены!"
    } else {
        "👋 Для доступа к группе:\n1. Оставайтесь в группе\n2. Отправьте /confirm здесь"
    };

    let chat_id = msg.chat.id;
    let bot_clone = bot.clone();
    let response = retry_telegram_request(
        move || {
            let text = text.to_owned();
            let bot = bot_clone.clone();
            Box::pin(async move {
                info!("Sending start response to chat {}", chat_id);
                bot.send_message(chat_id, text).await.map_err(|e| e.into())
            })
        },
        "send start response",
    )
    .await?;

    info!("Scheduled deletion of start response in chat {}", chat_id);
    delete_message_later(bot, chat_id, response.id);
    Ok(())
}

async fn handle_confirm(bot: Bot, msg: Message, state: Arc<BotState>) -> Result<()> {
    let user = match msg.from.as_ref() {
        Some(user) => {
            info!(
                "Received /confirm from user {} ({} @{}) in chat {}",
                user.id,
                user.full_name(),
                user.username.as_deref().unwrap_or(""),
                msg.chat.id
            );
            user
        }
        None => {
            warn!("Received /confirm without user info");
            return Ok(());
        }
    };

    let chat_id = msg.chat.id;
    let message_id = msg.id;
    let bot_clone = bot.clone();

    info!(
        "Deleting /confirm command from user {} in chat {}",
        user.id, chat_id
    );
    retry_telegram_request(
        move || {
            let bot = bot_clone.clone();
            Box::pin(async move {
                bot.delete_message(chat_id, message_id)
                    .await
                    .map_err(|e| e.into())
            })
        },
        "delete confirm command",
    )
    .await?;

    if state.is_whitelisted(user.id).await {
        info!("User {} is already whitelisted", user.id);
        let bot_clone = bot.clone();
        let response = retry_telegram_request(
            move || {
                let bot = bot_clone.clone();
                Box::pin(async move {
                    bot.send_message(chat_id, "ℹ️ Вы уже подтверждены")
                        .await
                        .map_err(|e| e.into())
                })
            },
            "send already confirmed message",
        )
        .await?;
        delete_message_later(bot.clone(), chat_id, response.id);
        return Ok(());
    }

    let group_chat_id = state.group_chat_id;
    let user_id = user.id;
    let bot_clone = bot.clone();

    info!(
        "Checking group membership for user {} in group {}",
        user_id, group_chat_id
    );
    match retry_telegram_request(
        move || {
            let bot = bot_clone.clone();
            Box::pin(async move {
                bot.get_chat_member(group_chat_id, user_id)
                    .await
                    .map_err(|e| e.into())
            })
        },
        "get chat member",
    )
    .await
    {
        Ok(member) if is_member(&member) => {
            let username = user
                .username
                .as_deref()
                .unwrap_or(&user.first_name)
                .to_owned();
            info!(
                "User {} is a member of group {}, adding to whitelist",
                user_id, group_chat_id
            );

            if let Err(e) = state.add_to_whitelist(user.id, &username).await {
                error!("Failed to add to whitelist: {}", e);
                let bot_clone = bot.clone();
                let response = retry_telegram_request(
                    move || {
                        let bot = bot_clone.clone();
                        Box::pin(async move {
                            bot.send_message(chat_id, "⚠️ Ошибка. Попробуйте позже")
                                .await
                                .map_err(|e| e.into())
                        })
                    },
                    "send whitelist error",
                )
                .await?;
                delete_message_later(bot.clone(), chat_id, response.id);
                return Ok(());
            }

            let text = match member.status() {
                ChatMemberStatus::Member => "✅ Вы подтверждены!",
                _ => "👑 Админ подтверждён!",
            };

            info!("User {} successfully confirmed", user_id);
            let bot_clone = bot.clone();
            let response = retry_telegram_request(
                move || {
                    let text = text.to_owned();
                    let bot = bot_clone.clone();
                    Box::pin(
                        async move { bot.send_message(chat_id, text).await.map_err(|e| e.into()) },
                    )
                },
                "send confirmation message",
            )
            .await?;
            delete_message_later(bot.clone(), chat_id, response.id);
        }
        Ok(_) => {
            warn!(
                "User {} is not a member of group {}",
                user_id, group_chat_id
            );
            let bot_clone = bot.clone();
            let response = retry_telegram_request(
                move || {
                    let bot = bot_clone.clone();
                    Box::pin(async move {
                        bot.send_message(
                            chat_id,
                            "❌ Вы должны быть участником группы для подтверждения!",
                        )
                        .await
                        .map_err(|e| e.into())
                    })
                },
                "send not member message",
            )
            .await?;
            delete_message_later(bot.clone(), chat_id, response.id);
        }
        Err(e) => {
            error!(
                "Failed to check group membership for user {}: {}",
                user.id, e
            );
            let bot_clone = bot.clone();
            let response = retry_telegram_request(
                move || {
                    let bot = bot_clone.clone();
                    Box::pin(async move {
                        bot.send_message(
                            chat_id,
                            "⚠️ Ошибка проверки членства в группе. Попробуйте позже.",
                        )
                        .await
                        .map_err(|e| e.into())
                    })
                },
                "send membership check error",
            )
            .await?;
            delete_message_later(bot.clone(), chat_id, response.id);
        }
    }

    Ok(())
}

fn is_member(member: &teloxide::types::ChatMember) -> bool {
    matches!(
        member.status(),
        ChatMemberStatus::Member | ChatMemberStatus::Administrator | ChatMemberStatus::Owner
    )
}

async fn handle_group_message(bot: Bot, msg: Message, state: Arc<BotState>) -> Result<()> {
    if let Some(user) = msg.from.clone() {
        info!(
            "Processing message from user {} ({} @{}) in chat {}: {}",
            user.id,
            user.full_name(),
            user.username.as_deref().unwrap_or(""),
            msg.chat.id,
            msg.text().unwrap_or("[non-text message]")
        );

        // Проверяем команды только для текстовых сообщений
        if let Some(text) = msg.text() {
            if let Some(cmd) = Command::parse(text) {
                info!("Detected command {:?} from user {}", cmd, user.id);
                match cmd {
                    Command::Start => {
                        return handle_start(bot.clone(), msg.clone(), state.clone()).await
                    }
                    Command::Confirm => {
                        return handle_confirm(bot.clone(), msg.clone(), state.clone()).await
                    }
                }
            }
        }

        let chat_id = msg.chat.id;
        let message_id = msg.id;
        let user_first_name = user.first_name.clone();
        let bot_clone = bot.clone();

        // Для неподтверждённых пользователей удаляем ЛЮБОЕ сообщение
        if !state.is_whitelisted(user.id).await {
            warn!("User {} is not whitelisted, deleting message", user.id);
            if let Err(e) = retry_telegram_request(
                move || {
                    let bot = bot_clone.clone();
                    Box::pin(async move {
                        info!(
                            "Deleting message from unwhitelisted user {} in chat {}",
                            user.id, chat_id
                        );
                        bot.delete_message(chat_id, message_id)
                            .await
                            .map_err(|e| e.into())
                    })
                },
                "delete unwhitelisted message",
            )
            .await
            {
                error!(
                    "Failed to delete message from unwhitelisted user {}: {}",
                    user.id, e
                );
            }

            // Отправляем предупреждение только для текстовых сообщений (чтобы не спамить)
            if msg.text().is_some() {
                info!(
                    "Sending warning to unwhitelisted user {} in chat {}",
                    user.id, chat_id
                );
                let bot_clone = bot.clone();
                let response = retry_telegram_request(
                    move || {
                        let user_first_name = user_first_name.clone();
                        let bot = bot_clone.clone();
                        Box::pin(async move {
                            bot.send_message(
                                chat_id,
                                format!("{}, для доступа отправьте /confirm", user_first_name),
                            )
                            .await
                            .map_err(|e| e.into())
                        })
                    },
                    "send unwhitelisted warning",
                )
                .await?;
                delete_message_later(bot.clone(), chat_id, response.id);
            }
            return Ok(());
        }

        // Для подтверждённых пользователей проверяем запрещённые паттерны в текстовых сообщениях
        if let Some(text) = msg.text() {
            if state.check_message(text).await {
                warn!(
                    "Message from user {} contains forbidden pattern, deleting",
                    user.id
                );
                let bot_clone = bot.clone();
                if let Err(e) = retry_telegram_request(
                    move || {
                        let bot = bot_clone.clone();
                        Box::pin(async move {
                            info!(
                                "Deleting forbidden message from user {} in chat {}",
                                user.id, chat_id
                            );
                            bot.delete_message(chat_id, message_id)
                                .await
                                .map_err(|e| e.into())
                        })
                    },
                    "delete forbidden message",
                )
                .await
                {
                    error!(
                        "Failed to delete forbidden message from user {}: {}",
                        user.id, e
                    );
                }

                let bot_clone = bot.clone();
                let response = retry_telegram_request(
                    move || {
                        let user_first_name = user_first_name.clone();
                        let bot = bot_clone.clone();
                        Box::pin(async move {
                            info!(
                                "Sending warning about forbidden pattern to user {} in chat {}",
                                user.id, chat_id
                            );
                            bot.send_message(
                                chat_id,
                                format!(
                                    "{}, ваше сообщение нарушает правила чата!",
                                    user_first_name
                                ),
                            )
                            .await
                            .map_err(|e| e.into())
                        })
                    },
                    "send forbidden pattern warning",
                )
                .await?;
                delete_message_later(bot.clone(), chat_id, response.id);
            }
        }
    }
    Ok(())
}

fn delete_message_later(bot: Bot, chat_id: ChatId, message_id: MessageId) {
    info!(
        "Scheduling deletion of message {} in chat {} in 30 seconds",
        message_id, chat_id
    );
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        if let Err(e) = retry_telegram_request(
            move || {
                let bot = bot.clone();
                Box::pin(async move {
                    info!(
                        "Executing scheduled deletion of message {} in chat {}",
                        message_id, chat_id
                    );
                    bot.delete_message(chat_id, message_id)
                        .await
                        .map_err(|e| e.into())
                })
            },
            "delete delayed message",
        )
        .await
        {
            error!(
                "Failed to delete scheduled message {} in chat {}: {}",
                message_id, chat_id, e
            );
        }
    });
}

#[tokio::main]
async fn main() {
    dotenv().ok();
    pretty_env_logger::init();
    info!("Starting verification bot...");

    let bot_token =
        std::env::var("VERIFICATION_BOT_TOKEN").expect("VERIFICATION_BOT_TOKEN must be set");
    let group_chat_id = std::env::var("GROUP_CHAT_ID")
        .unwrap_or("-1001380105834".to_string())
        .parse::<i64>()
        .expect("Invalid GROUP_CHAT_ID");

    info!("Group chat ID: {}", group_chat_id);

    let state = Arc::new(BotState::new(
        ChatId(group_chat_id),
        &std::env::var("WHITELIST_FILE").unwrap_or_else(|_| "whitelist.txt".to_string()),
        &std::env::var("FORBIDDEN_PATTERNS_FILE")
            .unwrap_or_else(|_| "forbidden_patterns.txt".to_string()),
    ));

    let bot = Bot::new(bot_token);

    let handler = Update::filter_message()
        .branch(
            dptree::entry()
                .filter(|msg: Message| msg.text().and_then(|text| Command::parse(text)).is_some())
                .endpoint(|bot: Bot, msg: Message, state: Arc<BotState>| async move {
                    let cmd = Command::parse(msg.text().unwrap()).unwrap();
                    match cmd {
                        Command::Start => handle_start(bot, msg, state).await,
                        Command::Confirm => handle_confirm(bot, msg, state).await,
                    }
                }),
        )
        .branch(dptree::entry().endpoint(handle_group_message));

    info!("Starting dispatcher...");
    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}
