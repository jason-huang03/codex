use std::collections::HashSet;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;

use crate::app_event::AppEvent;
use crate::app_event::ExternalDecisionInput;
use crate::app_event::ExternalDecisionSource;
use crate::app_event::ExternalPromptInput;
use crate::app_event_sender::AppEventSender;

const TELEGRAM_BOTS_FILENAME: &str = "telegram-bots.toml";
const TELEGRAM_LOCKS_DIRNAME: &str = "telegram-bot-locks";
const TELEGRAM_API_BASE_URL: &str = "https://api.telegram.org";
const TELEGRAM_POLL_FAILURE_DELAY: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Deserialize)]
struct TelegramBotsToml {
    #[serde(default)]
    bots: Vec<TelegramBotConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramBotConfig {
    name: String,
    token: String,
    chat_id: String,
}

#[derive(Debug)]
struct TelegramLease {
    config: TelegramBotConfig,
    // Keep the lock file alive for the duration of the active session selection.
    _lock_file: File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TelegramBotAvailability {
    Available,
    InUseByAnotherSession,
    ActiveInThisSession,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TelegramBotOption {
    pub(crate) name: String,
    pub(crate) availability: TelegramBotAvailability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TelegramBotSelectionSnapshot {
    pub(crate) config_path: PathBuf,
    pub(crate) options: Vec<TelegramBotOption>,
    pub(crate) active_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TelegramActivation {
    Activated,
    AlreadyActive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TelegramActiveBot {
    pub(crate) name: String,
    pub(crate) token: String,
    pub(crate) chat_id: String,
}

#[derive(Debug, Error)]
pub(crate) enum TelegramError {
    #[error("Telegram config file not found at {path}")]
    MissingConfig { path: PathBuf },
    #[error("failed to read Telegram config file {path}: {source}")]
    ReadConfig {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse Telegram config file {path}: {source}")]
    ParseConfig {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("Telegram config file {path} does not define any [[bots]] entries")]
    EmptyConfig { path: PathBuf },
    #[error("Telegram bot entry #{index} has an empty {field}")]
    EmptyField { index: usize, field: &'static str },
    #[error("duplicate Telegram bot name \"{name}\" in {path}")]
    DuplicateName { path: PathBuf, name: String },
    #[error("Telegram bot \"{name}\" not found in {path}")]
    BotNotFound { path: PathBuf, name: String },
    #[error("Telegram bot \"{name}\" is already in use by another Codex session")]
    BotInUse { name: String },
    #[error("failed to create Telegram lock directory {path}: {source}")]
    CreateLockDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to open Telegram lock file {path}: {source}")]
    OpenLockFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to lock Telegram bot \"{name}\": {source}")]
    LockBot {
        name: String,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug)]
pub(crate) struct TelegramNotifier {
    codex_home: PathBuf,
    active_lease: Option<TelegramLease>,
}

impl TelegramNotifier {
    pub(crate) fn new(codex_home: PathBuf) -> Self {
        Self {
            codex_home,
            active_lease: None,
        }
    }

    pub(crate) fn config_path(&self) -> PathBuf {
        self.codex_home.join(TELEGRAM_BOTS_FILENAME)
    }

    fn locks_dir(&self) -> PathBuf {
        self.codex_home.join(TELEGRAM_LOCKS_DIRNAME)
    }

    fn active_name(&self) -> Option<&str> {
        self.active_lease
            .as_ref()
            .map(|lease| lease.config.name.as_str())
    }

    pub(crate) fn active_bot(&self) -> Option<TelegramActiveBot> {
        let lease = self.active_lease.as_ref()?;
        Some(TelegramActiveBot {
            name: lease.config.name.clone(),
            token: lease.config.token.clone(),
            chat_id: lease.config.chat_id.clone(),
        })
    }

    pub(crate) fn deactivate(&mut self) -> bool {
        self.active_lease.take().is_some()
    }

    pub(crate) fn select_bot(&mut self, name: &str) -> Result<TelegramActivation, TelegramError> {
        if self.active_name() == Some(name) {
            return Ok(TelegramActivation::AlreadyActive);
        }

        let bots = self.load_bots()?;
        let config_path = self.config_path();
        let bot = bots
            .into_iter()
            .find(|bot| bot.name == name)
            .ok_or_else(|| TelegramError::BotNotFound {
                path: config_path,
                name: name.to_string(),
            })?;
        let lock_file = self.try_lock_bot(&bot)?;

        self.active_lease = Some(TelegramLease {
            config: bot,
            _lock_file: lock_file,
        });
        Ok(TelegramActivation::Activated)
    }

    pub(crate) fn selection_snapshot(&self) -> Result<TelegramBotSelectionSnapshot, TelegramError> {
        let bots = self.load_bots()?;
        let mut options = Vec::with_capacity(bots.len());
        let active_name = self.active_name().map(str::to_string);

        for bot in bots {
            let availability = if active_name.as_deref() == Some(bot.name.as_str()) {
                TelegramBotAvailability::ActiveInThisSession
            } else {
                match self.try_lock_bot(&bot) {
                    Ok(lock_file) => {
                        drop(lock_file);
                        TelegramBotAvailability::Available
                    }
                    Err(TelegramError::BotInUse { .. }) => {
                        TelegramBotAvailability::InUseByAnotherSession
                    }
                    Err(err) => return Err(err),
                }
            };
            options.push(TelegramBotOption {
                name: bot.name,
                availability,
            });
        }

        Ok(TelegramBotSelectionSnapshot {
            config_path: self.config_path(),
            options,
            active_name,
        })
    }

    pub(crate) fn send_message<F>(&self, text: String, on_success: F) -> bool
    where
        F: FnOnce() + Send + 'static,
    {
        let Some(active_lease) = self.active_lease.as_ref() else {
            return false;
        };

        let bot_name = active_lease.config.name.clone();
        let token = active_lease.config.token.clone();
        let chat_id = active_lease.config.chat_id.clone();

        tokio::spawn(async move {
            let client = match reqwest::Client::builder().build() {
                Ok(client) => client,
                Err(err) => {
                    tracing::warn!(
                        bot = %bot_name,
                        error = %err,
                        "failed to construct Telegram HTTP client"
                    );
                    return;
                }
            };
            if send_message_once(&client, &bot_name, &token, &chat_id, text).await {
                on_success();
            }
        });
        true
    }

    pub(crate) fn send_message_after_delay<F>(
        &self,
        text: String,
        delay: Duration,
        on_success: F,
    ) -> bool
    where
        F: FnOnce() + Send + 'static,
    {
        let Some(active_lease) = self.active_lease.as_ref() else {
            return false;
        };

        let bot_name = active_lease.config.name.clone();
        let token = active_lease.config.token.clone();
        let chat_id = active_lease.config.chat_id.clone();

        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let client = match reqwest::Client::builder().build() {
                Ok(client) => client,
                Err(err) => {
                    tracing::warn!(
                        bot = %bot_name,
                        error = %err,
                        "failed to construct Telegram HTTP client"
                    );
                    return;
                }
            };
            if send_message_once(&client, &bot_name, &token, &chat_id, text).await {
                on_success();
            }
        });
        true
    }

    pub(crate) fn send_messages_in_order<F>(
        &self,
        messages: Vec<String>,
        delay_before_followup_messages: Duration,
        on_any_success: F,
    ) -> bool
    where
        F: FnOnce() + Send + 'static,
    {
        let Some(active_lease) = self.active_lease.as_ref() else {
            return false;
        };
        if messages.is_empty() {
            return false;
        }

        let bot_name = active_lease.config.name.clone();
        let token = active_lease.config.token.clone();
        let chat_id = active_lease.config.chat_id.clone();

        tokio::spawn(async move {
            let client = match reqwest::Client::builder().build() {
                Ok(client) => client,
                Err(err) => {
                    tracing::warn!(
                        bot = %bot_name,
                        error = %err,
                        "failed to construct Telegram HTTP client"
                    );
                    return;
                }
            };

            let mut any_success = false;
            let mut messages = messages.into_iter();
            if let Some(first_message) = messages.next()
                && send_message_once(&client, &bot_name, &token, &chat_id, first_message).await
            {
                any_success = true;
            }

            let followup_messages: Vec<String> = messages.collect();
            if !followup_messages.is_empty() {
                tokio::time::sleep(delay_before_followup_messages).await;

                let mut followup_tasks = JoinSet::new();
                for message in followup_messages {
                    let client = client.clone();
                    let bot_name = bot_name.clone();
                    let token = token.clone();
                    let chat_id = chat_id.clone();
                    followup_tasks.spawn(async move {
                        send_message_once(&client, &bot_name, &token, &chat_id, message).await
                    });
                }

                while let Some(result) = followup_tasks.join_next().await {
                    let sent = match result {
                        Ok(sent) => sent,
                        Err(err) => {
                            tracing::warn!(
                                bot = %bot_name,
                                error = %err,
                                "failed to join Telegram follow-up send task"
                            );
                            false
                        }
                    };
                    if sent {
                        any_success = true;
                    }
                }
            }

            if any_success {
                on_any_success();
            }
        });
        true
    }

    pub(crate) fn spawn_choice_command_poller(
        &self,
        app_event_tx: AppEventSender,
    ) -> Option<JoinHandle<()>> {
        let bot = self.active_bot()?;
        Some(spawn_choice_command_poller(bot, app_event_tx))
    }

    fn load_bots(&self) -> Result<Vec<TelegramBotConfig>, TelegramError> {
        let path = self.config_path();
        if !path.exists() {
            return Err(TelegramError::MissingConfig { path });
        }

        let file_contents =
            std::fs::read_to_string(&path).map_err(|source| TelegramError::ReadConfig {
                path: path.clone(),
                source,
            })?;
        let parsed: TelegramBotsToml =
            toml::from_str(&file_contents).map_err(|source| TelegramError::ParseConfig {
                path: path.clone(),
                source,
            })?;
        if parsed.bots.is_empty() {
            return Err(TelegramError::EmptyConfig { path });
        }

        let mut seen_names = HashSet::new();
        for (index, bot) in parsed.bots.iter().enumerate() {
            if bot.name.trim().is_empty() {
                return Err(TelegramError::EmptyField {
                    index,
                    field: "name",
                });
            }
            if bot.token.trim().is_empty() {
                return Err(TelegramError::EmptyField {
                    index,
                    field: "token",
                });
            }
            if bot.chat_id.trim().is_empty() {
                return Err(TelegramError::EmptyField {
                    index,
                    field: "chat_id",
                });
            }
            if !seen_names.insert(bot.name.clone()) {
                return Err(TelegramError::DuplicateName {
                    path,
                    name: bot.name.clone(),
                });
            }
        }

        Ok(parsed.bots)
    }

    fn try_lock_bot(&self, bot: &TelegramBotConfig) -> Result<File, TelegramError> {
        let lock_path = self.lock_path_for_name(&bot.name);
        let lock_dir = self.locks_dir();
        std::fs::create_dir_all(&lock_dir).map_err(|source| TelegramError::CreateLockDir {
            path: lock_dir,
            source,
        })?;

        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            options.mode(0o600);
        }
        let lock_file = options
            .open(&lock_path)
            .map_err(|source| TelegramError::OpenLockFile {
                path: lock_path,
                source,
            })?;

        match lock_file.try_lock() {
            Ok(()) => Ok(lock_file),
            Err(std::fs::TryLockError::WouldBlock) => Err(TelegramError::BotInUse {
                name: bot.name.clone(),
            }),
            Err(std::fs::TryLockError::Error(source)) => Err(TelegramError::LockBot {
                name: bot.name.clone(),
                source,
            }),
        }
    }

    fn lock_path_for_name(&self, name: &str) -> PathBuf {
        let mut encoded = String::with_capacity(name.len() * 2);
        for byte in name.as_bytes() {
            encoded.push(hex_digit(byte >> 4));
            encoded.push(hex_digit(byte & 0x0f));
        }
        self.locks_dir().join(format!("{encoded}.lock"))
    }
}

pub(crate) fn spawn_choice_command_poller(
    bot: TelegramActiveBot,
    app_event_tx: AppEventSender,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let client = match reqwest::Client::builder().build() {
            Ok(client) => client,
            Err(err) => {
                tracing::warn!(
                    bot = %bot.name,
                    error = %err,
                    "failed to construct Telegram HTTP client for decision polling"
                );
                return;
            }
        };
        let mut next_offset = bootstrap_offset(&client, &bot).await;

        loop {
            let updates = match fetch_updates(&client, &bot, next_offset, 20).await {
                Ok(None) => {
                    tokio::time::sleep(TELEGRAM_POLL_FAILURE_DELAY).await;
                    continue;
                }
                Ok(Some(updates)) => updates,
                Err(err) => {
                    tracing::warn!(
                        bot = %bot.name,
                        error = %err,
                        "failed to poll Telegram updates for decisions"
                    );
                    tokio::time::sleep(TELEGRAM_POLL_FAILURE_DELAY).await;
                    continue;
                }
            };

            for update in updates {
                next_offset = Some(update.update_id.saturating_add(1));
                let Some(input) = map_update_to_external_input(&bot, update) else {
                    continue;
                };
                match input {
                    TelegramExternalInput::Decision(input) => {
                        app_event_tx.send(AppEvent::ExternalDecisionInput(input));
                    }
                    TelegramExternalInput::Prompt(input) => {
                        app_event_tx.send(AppEvent::ExternalPromptInput(input));
                    }
                }
            }
        }
    })
}

async fn bootstrap_offset(client: &reqwest::Client, bot: &TelegramActiveBot) -> Option<i64> {
    match fetch_updates(client, bot, None, 0).await {
        Ok(Some(updates)) => updates
            .last()
            .map(|update| update.update_id.saturating_add(1)),
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(
                bot = %bot.name,
                error = %err,
                "failed to bootstrap Telegram updates offset"
            );
            None
        }
    }
}

async fn fetch_updates(
    client: &reqwest::Client,
    bot: &TelegramActiveBot,
    offset: Option<i64>,
    timeout_secs: u64,
) -> Result<Option<Vec<TelegramUpdate>>, reqwest::Error> {
    let endpoint = format!("{TELEGRAM_API_BASE_URL}/bot{}/getUpdates", bot.token);
    let response = client
        .post(endpoint)
        .json(&TelegramGetUpdatesRequest {
            offset,
            timeout: timeout_secs,
        })
        .send()
        .await?;
    if !response.status().is_success() {
        tracing::warn!(
            bot = %bot.name,
            status = %response.status(),
            "Telegram getUpdates returned non-success status"
        );
        return Ok(None);
    }

    match response.json::<TelegramGetUpdatesResponse>().await {
        Ok(payload) if payload.ok => Ok(Some(payload.result)),
        Ok(payload) => {
            tracing::warn!(
                bot = %bot.name,
                description = payload.description.unwrap_or_else(|| "unknown error".to_string()),
                "Telegram getUpdates returned unsuccessful payload"
            );
            Ok(None)
        }
        Err(err) => {
            tracing::warn!(
                bot = %bot.name,
                error = %err,
                "failed to decode Telegram getUpdates payload"
            );
            Ok(None)
        }
    }
}

enum TelegramExternalInput {
    Decision(ExternalDecisionInput),
    Prompt(ExternalPromptInput),
}

fn map_update_to_external_input(
    bot: &TelegramActiveBot,
    update: TelegramUpdate,
) -> Option<TelegramExternalInput> {
    let message = update.message?;
    if message.chat.id.to_string() != bot.chat_id {
        return None;
    }
    let from = message.from?;
    if from.is_bot {
        return None;
    }
    let text = message.text?;
    if let Some((token, choice)) = parse_choice_command(&text) {
        return Some(TelegramExternalInput::Decision(ExternalDecisionInput {
            decision_id: None,
            token,
            choice,
            source: ExternalDecisionSource::Telegram { user_id: from.id },
        }));
    }

    let (token, text) = parse_prompt_command(&text)?;
    Some(TelegramExternalInput::Prompt(ExternalPromptInput {
        token,
        text,
        source: ExternalDecisionSource::Telegram { user_id: from.id },
    }))
}

async fn send_message_once(
    client: &reqwest::Client,
    bot_name: &str,
    token: &str,
    chat_id: &str,
    text: String,
) -> bool {
    let endpoint = format!("{TELEGRAM_API_BASE_URL}/bot{token}/sendMessage");
    let request = TelegramSendMessageRequest {
        chat_id: chat_id.to_string(),
        text,
        disable_web_page_preview: true,
    };

    let response = match client.post(endpoint).json(&request).send().await {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!(
                bot = %bot_name,
                error = %err,
                "failed to send Telegram notification"
            );
            return false;
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        tracing::warn!(
            bot = %bot_name,
            %status,
            "Telegram API returned a non-success status"
        );
        return false;
    }

    match response.json::<TelegramSendMessageResponse>().await {
        Ok(payload) if payload.ok => true,
        Ok(payload) => {
            tracing::warn!(
                bot = %bot_name,
                description = payload
                    .description
                    .unwrap_or_else(|| "unknown error".to_string()),
                "Telegram API reported an unsuccessful response"
            );
            false
        }
        Err(err) => {
            tracing::warn!(
                bot = %bot_name,
                error = %err,
                "failed to decode Telegram API response"
            );
            false
        }
    }
}

fn parse_choice_command(text: &str) -> Option<(String, String)> {
    let mut parts = text.split_whitespace();
    let command = parts.next()?.to_ascii_lowercase();
    if command != "/cx" && !command.starts_with("/cx@") {
        return None;
    }
    let token = parts.next()?.to_ascii_lowercase();
    if token.is_empty() || token.len() > 64 || !token.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    let choice = parts.next()?.to_ascii_lowercase();
    if choice.is_empty() {
        return None;
    }
    if parts.next().is_some() {
        return None;
    }
    Some((token, choice))
}

const MAX_PROMPT_CHARS: usize = 2_000;

fn parse_prompt_command(text: &str) -> Option<(String, String)> {
    let mut parts = text.split_whitespace();
    let command = parts.next()?.to_ascii_lowercase();
    if command != "/cxi" && !command.starts_with("/cxi@") {
        return None;
    }
    let token = parts.next()?.to_ascii_lowercase();
    if token.is_empty() || token.len() > 64 || !token.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    let prompt = parts.collect::<Vec<_>>().join(" ");
    let prompt = prompt.trim();
    if prompt.is_empty() || prompt.chars().count() > MAX_PROMPT_CHARS {
        return None;
    }
    Some((token, prompt.to_string()))
}

#[derive(Debug, Serialize)]
struct TelegramSendMessageRequest {
    chat_id: String,
    text: String,
    disable_web_page_preview: bool,
}

#[derive(Debug, Serialize)]
struct TelegramGetUpdatesRequest {
    offset: Option<i64>,
    timeout: u64,
}

#[derive(Debug, Deserialize)]
struct TelegramGetUpdatesResponse {
    ok: bool,
    result: Vec<TelegramUpdate>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
    chat: TelegramChat,
    from: Option<TelegramUser>,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramChat {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct TelegramUser {
    id: i64,
    is_bot: bool,
}

#[derive(Debug, Deserialize)]
struct TelegramSendMessageResponse {
    ok: bool,
    description: Option<String>,
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        10..=15 => (b'a' + (value - 10)) as char,
        _ => unreachable!("nibble must be in 0..=15"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use super::TelegramActivation;
    use super::TelegramError;
    use super::TelegramNotifier;
    use super::parse_choice_command;
    use super::parse_prompt_command;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    fn write_config(home: &TempDir, config: &str) {
        let path = home.path().join("telegram-bots.toml");
        std::fs::write(path, config).expect("write telegram config");
    }

    #[test]
    fn selection_snapshot_lists_available_bots() {
        let home = TempDir::new().expect("tempdir");
        write_config(
            &home,
            r#"
                [[bots]]
                name = "bot-a"
                token = "token-a"
                chat_id = "chat-a"

                [[bots]]
                name = "bot-b"
                token = "token-b"
                chat_id = "chat-b"
            "#,
        );
        let notifier = TelegramNotifier::new(home.path().to_path_buf());

        let snapshot = notifier
            .selection_snapshot()
            .expect("load bot selection snapshot");
        let names: Vec<String> = snapshot
            .options
            .into_iter()
            .map(|option| option.name)
            .collect();
        assert_eq!(names, vec!["bot-a".to_string(), "bot-b".to_string()]);
    }

    #[test]
    fn select_bot_is_ephemeral_and_can_be_disabled() {
        let home = TempDir::new().expect("tempdir");
        write_config(
            &home,
            r#"
                [[bots]]
                name = "bot-a"
                token = "token-a"
                chat_id = "chat-a"
            "#,
        );
        let mut notifier = TelegramNotifier::new(home.path().to_path_buf());

        let activation = notifier.select_bot("bot-a").expect("select bot");
        assert_eq!(activation, TelegramActivation::Activated);
        assert!(
            notifier.deactivate(),
            "expected active selection to be dropped"
        );
        assert!(
            !notifier.deactivate(),
            "expected second deactivate call to be a no-op"
        );
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let home = TempDir::new().expect("tempdir");
        write_config(
            &home,
            r#"
                [[bots]]
                name = "dup"
                token = "token-a"
                chat_id = "chat-a"

                [[bots]]
                name = "dup"
                token = "token-b"
                chat_id = "chat-b"
            "#,
        );
        let notifier = TelegramNotifier::new(home.path().to_path_buf());

        let err = notifier
            .selection_snapshot()
            .expect_err("expected duplicate names to fail");
        assert!(
            matches!(err, TelegramError::DuplicateName { .. }),
            "unexpected error type: {err:?}"
        );
    }

    #[test]
    fn missing_config_has_actionable_error() {
        let home = TempDir::new().expect("tempdir");
        let notifier = TelegramNotifier::new(home.path().to_path_buf());

        let err = notifier
            .selection_snapshot()
            .expect_err("expected missing config to fail");
        assert!(
            matches!(err, TelegramError::MissingConfig { .. }),
            "unexpected error type: {err:?}"
        );
    }

    #[test]
    fn send_message_without_active_bot_does_not_invoke_callback() {
        let home = TempDir::new().expect("tempdir");
        write_config(
            &home,
            r#"
                [[bots]]
                name = "bot-a"
                token = "token-a"
                chat_id = "chat-a"
            "#,
        );
        let notifier = TelegramNotifier::new(home.path().to_path_buf());
        let callback_called = Arc::new(AtomicBool::new(false));
        let callback_called_clone = Arc::clone(&callback_called);

        let sent = notifier.send_message("hello".to_string(), move || {
            callback_called_clone.store(true, Ordering::Relaxed);
        });

        assert!(!sent);
        assert!(!callback_called.load(Ordering::Relaxed));
    }

    #[test]
    fn send_messages_in_order_without_active_bot_does_not_invoke_callback() {
        let home = TempDir::new().expect("tempdir");
        write_config(
            &home,
            r#"
                [[bots]]
                name = "bot-a"
                token = "token-a"
                chat_id = "chat-a"
            "#,
        );
        let notifier = TelegramNotifier::new(home.path().to_path_buf());
        let callback_called = Arc::new(AtomicBool::new(false));
        let callback_called_clone = Arc::clone(&callback_called);

        let sent = notifier.send_messages_in_order(
            vec!["hello".to_string(), "world".to_string()],
            Duration::from_secs(2),
            move || {
                callback_called_clone.store(true, Ordering::Relaxed);
            },
        );

        assert!(!sent);
        assert!(!callback_called.load(Ordering::Relaxed));
    }

    #[test]
    fn send_message_after_delay_without_active_bot_does_not_invoke_callback() {
        let home = TempDir::new().expect("tempdir");
        write_config(
            &home,
            r#"
                [[bots]]
                name = "bot-a"
                token = "token-a"
                chat_id = "chat-a"
            "#,
        );
        let notifier = TelegramNotifier::new(home.path().to_path_buf());
        let callback_called = Arc::new(AtomicBool::new(false));
        let callback_called_clone = Arc::clone(&callback_called);

        let sent = notifier.send_message_after_delay(
            "hello".to_string(),
            Duration::from_secs(2),
            move || {
                callback_called_clone.store(true, Ordering::Relaxed);
            },
        );

        assert!(!sent);
        assert!(!callback_called.load(Ordering::Relaxed));
    }

    #[test]
    fn parse_choice_command_accepts_strict_format() {
        let parsed = parse_choice_command("/cx abcdef123456 approve");
        assert_eq!(
            parsed,
            Some(("abcdef123456".to_string(), "approve".to_string()))
        );
    }

    #[test]
    fn parse_choice_command_rejects_extra_tokens() {
        let parsed = parse_choice_command("/cx abcdef123456 approve now");
        assert_eq!(parsed, None);
    }

    #[test]
    fn parse_prompt_command_accepts_text_payload() {
        let parsed = parse_prompt_command("/cxi abcdef123456 hello from telegram");
        assert_eq!(
            parsed,
            Some((
                "abcdef123456".to_string(),
                "hello from telegram".to_string()
            ))
        );
    }

    #[test]
    fn parse_prompt_command_rejects_missing_text() {
        let parsed = parse_prompt_command("/cxi abcdef123456");
        assert_eq!(parsed, None);
    }

    #[test]
    fn parse_prompt_command_rejects_invalid_token() {
        let parsed = parse_prompt_command("/cxi token-not-hex hello");
        assert_eq!(parsed, None);
    }
}
