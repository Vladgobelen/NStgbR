use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use dotenv::dotenv;
use log::{error, info, warn};
use teloxide::dispatching::Dispatcher;
use teloxide::prelude::*;
use teloxide::types::{ChatId, ChatMemberStatus, Message, MessageId, UserId};
use tokio::sync::Mutex;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone)]
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

struct BotState {
    whitelist: Mutex<HashSet<UserId>>,
    whitelist_file: String,
    group_chat_id: ChatId,
}

impl BotState {
    fn new(group_chat_id: ChatId, whitelist_file: &str) -> Self {
        Self {
            whitelist: Mutex::new(Self::load_whitelist(whitelist_file)),
            whitelist_file: whitelist_file.to_string(),
            group_chat_id,
        }
    }

    fn load_whitelist(path: &str) -> HashSet<UserId> {
        let mut whitelist = HashSet::new();

        if !Path::new(path).exists() {
            return whitelist;
        }

        match File::open(path) {
            Ok(file) => {
                for line in BufReader::new(file).lines().flatten() {
                    if let Ok(id) = line.split_whitespace().next().unwrap_or("").parse::<u64>() {
                        whitelist.insert(UserId(id));
                    }
                }
            }
            Err(e) => error!("Failed to load whitelist: {}", e),
        }
        whitelist
    }

    async fn add_to_whitelist(&self, user_id: UserId, username: &str) -> Result<()> {
        let mut whitelist = self.whitelist.lock().await;
        if whitelist.insert(user_id) {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.whitelist_file)?;
            writeln!(file, "{} {}", user_id.0, username)?;
        }
        Ok(())
    }

    async fn is_whitelisted(&self, user_id: UserId) -> bool {
        self.whitelist.lock().await.contains(&user_id)
    }
}

async fn handle_start(bot: Bot, msg: Message, state: Arc<BotState>) -> Result<()> {
    let user = match msg.from.as_ref() {
        Some(user) => user,
        None => return Ok(()),
    };

    let text = if state.is_whitelisted(user.id).await {
        "✅ Вы уже подтверждены!"
    } else {
        "👋 Для доступа к группе:\n1. Оставайтесь в группе\n2. Отправьте /confirm здесь"
    };

    let response = bot.send_message(msg.chat.id, text).await?;
    delete_message_later(bot, msg.chat.id, response.id);
    Ok(())
}

async fn handle_confirm(bot: Bot, msg: Message, state: Arc<BotState>) -> Result<()> {
    let user = match msg.from.as_ref() {
        Some(user) => user,
        None => return Ok(()),
    };

    if let Err(e) = bot.delete_message(msg.chat.id, msg.id).await {
        warn!("Failed to delete message: {}", e);
    }

    if state.is_whitelisted(user.id).await {
        let response = bot
            .send_message(msg.chat.id, "ℹ️ Вы уже подтверждены")
            .await?;
        delete_message_later(bot, msg.chat.id, response.id);
        return Ok(());
    }

    match bot.get_chat_member(state.group_chat_id, user.id).await {
        Ok(member) if is_member(&member) => {
            let username = user.username.as_deref().unwrap_or(&user.first_name);

            if let Err(e) = state.add_to_whitelist(user.id, username).await {
                error!("Failed to add to whitelist: {}", e);
                let response = bot
                    .send_message(msg.chat.id, "⚠️ Ошибка. Попробуйте позже")
                    .await?;
                delete_message_later(bot, msg.chat.id, response.id);
                return Ok(());
            }

            let text = match member.status() {
                ChatMemberStatus::Member => "✅ Вы подтверждены!",
                _ => "👑 Админ подтверждён!",
            };

            let response = bot.send_message(msg.chat.id, text).await?;
            delete_message_later(bot, msg.chat.id, response.id);
        }
        _ => {
            let response = bot
                .send_message(
                    msg.chat.id,
                    "❌ Вы должны быть участником группы для подтверждения!",
                )
                .await?;
            delete_message_later(bot, msg.chat.id, response.id);
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
    if let Some(user) = &msg.from {
        if !state.is_whitelisted(user.id).await {
            if let Some(text) = msg.text() {
                if !Command::parse(text).is_some() {
                    if let Err(e) = bot.delete_message(msg.chat.id, msg.id).await {
                        warn!("Failed to delete message: {}", e);
                    }

                    let response = bot
                        .send_message(
                            msg.chat.id,
                            format!("{}, для доступа отправьте /confirm", user.first_name),
                        )
                        .await?;
                    delete_message_later(bot, msg.chat.id, response.id);
                }
            }
        }
    }
    Ok(())
}

fn delete_message_later(bot: Bot, chat_id: ChatId, message_id: MessageId) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        if let Err(e) = bot.delete_message(chat_id, message_id).await {
            warn!("Failed to delete message: {}", e);
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

    let state = Arc::new(BotState::new(
        ChatId(group_chat_id),
        &std::env::var("WHITELIST_FILE").unwrap_or_else(|_| "whitelist.txt".to_string()),
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
        .branch(
            dptree::entry()
                .filter(|msg: Message| msg.text().is_some())
                .endpoint(handle_group_message),
        );

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}
