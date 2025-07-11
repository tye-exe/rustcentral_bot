mod user_message;

use std::{collections::VecDeque, path::Path, sync::Arc, time::Duration};

use anyhow::Context;
use async_openai::{
    Client as AIClient,
    config::OpenAIConfig,
    types::{
        ChatChoice, ChatCompletionRequestMessage, ChatCompletionResponseMessage,
        CreateChatCompletionRequestArgs,
    },
};
use serde::Deserialize;
use tokio::{
    sync::{broadcast, mpsc},
    time::{Instant, sleep_until},
};
use tracing::{debug, error};
use twilight_gateway::Event;
use twilight_http::Client;
use twilight_model::id::{Id, marker::ChannelMarker};
use user_message::queue_messages;

use crate::{
    config::file_watch::{load_prompt, monitor_prompt},
    error::send_error_msg,
};

#[derive(Debug, Deserialize)]
pub struct Configuration {
    channel_id: Id<ChannelMarker>,
    llm_api_key: String,
    /// The base API endpoint to use. If not set the OpenAI API will be used.
    llm_api_base: Option<String>,
    model_name: String,
    /// The maximum amount of messages to include as history when generating a response. This does
    /// *not* include the channel prompt.
    ///
    /// When this limit is reached, the bot will remove messages until it the history has
    /// `min_history_size` messages.
    #[serde(default = "default_max_history_size")]
    max_history_size: u32,
    /// The minimum amount of messages that should be kept when downsizing the message history.
    #[serde(default = "default_min_history_size")]
    min_history_size: u32,
    /// If set to true, the LLM will also be able to see images sent by users. This requires the LLM
    /// used supports images as input.
    ///
    /// WARNING: this can be expensive.
    #[serde(default)]
    image_support: bool,
    /// The maximum size images are allowed to be before sent to the API.
    ///
    /// Images that have one or both dimensions bigger than this value will be downsized.
    #[serde(default = "default_max_image_size")]
    max_image_size: u32,
    /// The filepath to the prompt used for this channel.
    ///
    /// This should be a plain text file.
    prompt_path: Box<Path>,
}

impl Configuration {
    pub fn get_prompt_path(&self) -> &Path {
        self.prompt_path.as_ref()
    }

    pub fn get_channel_id(&self) -> &Id<ChannelMarker> {
        &self.channel_id
    }
}

fn default_max_history_size() -> u32 {
    40
}

fn default_min_history_size() -> u32 {
    30
}

fn default_max_image_size() -> u32 {
    800
}

/// Runs the main AI channel logic.
pub async fn serve(
    config: Configuration,
    events: broadcast::Receiver<Arc<Event>>,
    http: Arc<Client>,
) {
    let (prompt_sender, prompt_receiver) = match load_prompt(config.get_prompt_path()).await {
        Ok(var) => var,
        Err(err) => {
            tracing::error!("Unable to read channel prompt: {err}");
            tracing::error!(
                "Channel with id '{}' will not be activated",
                config.get_channel_id()
            );
            return;
        }
    };

    if let Err(err) = monitor_prompt(config.get_prompt_path(), prompt_sender) {
        tracing::error!(
            "Unable to watch prompt file at '{}' for channel '{}'. The channel will be active, but the prompt wont be updated unless the program is restarted.",
            config.get_prompt_path().display(),
            config.get_channel_id()
        );
        tracing::error!("{err}");
    };

    let mut llm_config = OpenAIConfig::new().with_api_key(&config.llm_api_key);
    if let Some(api_base) = &config.llm_api_base {
        llm_config = llm_config.with_api_base(api_base);
    }
    let llm_client = AIClient::with_config(llm_config).with_backoff(
        backoff::ExponentialBackoffBuilder::new()
            .with_max_elapsed_time(Some(Duration::from_secs(5)))
            .build(),
    );

    let max_history_size = config.max_history_size as usize;
    let (message_tx, mut message_rx) = mpsc::channel(max_history_size / 2);

    // Spawn a task to handle incoming message events and queue them in the channel above.
    tokio::spawn(queue_messages(events, message_tx, config.channel_id));

    let mut last_response_time = Instant::now();
    let mut last_error_response = None;
    let mut history = VecDeque::new();

    // Batch new messages together to avoid generating a separate response to each one.
    let mut new_messages = Vec::new();
    loop {
        // Wait to avoid getting rate limited by the LLM endpoint.
        // TODO: this could be handled better.
        sleep_until(last_response_time + Duration::from_millis(1500)).await;

        let recv_amt = message_rx
            .recv_many(&mut new_messages, max_history_size)
            .await;

        if recv_amt == 0 {
            // The message ingestion channel has closed, gracefully shut down this task.
            break;
        }

        let current_prompt =
            ChatCompletionRequestMessage::System(prompt_receiver.borrow().as_ref().into());

        for msg in &new_messages {
            let msg =
                ChatCompletionRequestMessage::User(msg.as_chat_completion_message(&config).await);

            history.push_back(msg);
        }
        new_messages.clear();

        if history.len() > max_history_size {
            // Downsize the history buffer by removing some elements from the front until it is back
            // to `min_history_size`. This is to ensure all messages fit in the context window while
            // allowing the LLM cache to be re-used for the next messages.
            let remove_from_front = history
                .len()
                .saturating_sub(config.min_history_size as usize);
            // TODO: count history in tokens rather amount of messages.
            history.drain(0..remove_from_front);

            debug!("Downsized history to {}", history.len());
        }

        let messages: Vec<_> = [current_prompt]
            .into_iter()
            .chain(history.iter().cloned())
            .collect();

        let response = generate_response(&llm_client, &config.model_name, messages).await;
        last_response_time = Instant::now();

        // Delete the previous error message. This should happen both if there is a new error
        // message or there is another error.
        if let Some(prev_err_msg_id) = last_error_response {
            let http2 = http.clone();
            tokio::spawn(async move {
                if let Err(err) = http2
                    .delete_message(config.channel_id, prev_err_msg_id)
                    .await
                {
                    error!("Failed to delete previous error message: {err}");
                }
            });

            last_error_response = None;
        }

        let mut response_content = match response {
            Ok(v) => v,
            Err(err) => {
                error!("Error creating response: {err:?}");

                // Log the error in the channel.
                let err_msg = send_error_msg(
                    &http,
                    config.channel_id,
                    &format!("Something went wrong while generating a response\n```\n{err}\n```"),
                )
                .await;

                if let Some(err_msg) = err_msg {
                    last_error_response = Some(err_msg.id);
                };
                continue;
            }
        };
        // Take only the first 2000 characters to stay within the discord character limit.
        response_content.truncate(
            response_content
                .char_indices()
                .take(2000)
                .map(|v| v.0 + v.1.len_utf8())
                .last()
                .unwrap_or(0),
        );

        if response_content.contains("<empty/>") {
            debug!("Model chose to not respond");
            continue;
        }

        history.push_back(ChatCompletionRequestMessage::Assistant(
            response_content.as_str().into(),
        ));

        if let Err(err) = http
            .create_message(config.channel_id)
            .content(&response_content)
            .await
        {
            error!("Failed to send response message: {err}");
            continue;
        }
    }

    // Don't clutter the channel with lots of error messages.
    if let Some(msg_id) = last_error_response {
        _ = http.delete_message(config.channel_id, msg_id).await;
    }
}

/// Sent by the model in response to a chat history.
///
/// A custom type is used here as some (gemini *caugh caugh*) APIs dont return all fields.
#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
}

/// Send the chat history to the LLM api and generate a response based on this history.
async fn generate_response(
    client: &AIClient<OpenAIConfig>,
    model_name: &str,
    history: Vec<ChatCompletionRequestMessage>,
) -> anyhow::Result<String> {
    let request = CreateChatCompletionRequestArgs::default()
        .model(model_name)
        .max_tokens(400u32)
        .messages(history)
        .build()
        .context("Failed to build request")?;

    let response: ChatCompletionResponse = client
        .chat()
        .create_byot(request)
        .await
        .context("LLM api returned an error")?;

    let response_content = match response.choices.first() {
        Some(ChatChoice {
            message:
                ChatCompletionResponseMessage {
                    content: Some(content),
                    ..
                },
            ..
        }) => content.as_str(),
        _ => {
            anyhow::bail!("LLM response did not include message content");
        }
    };

    Ok(response_content.to_string())
}
