use std::sync::Arc;

use botrs::{
    http::HttpClient, C2CMessage, Client, Context, EventHandler, GroupMessage, Intents, Ready,
    Token,
};
use tracing::{debug, info, warn};

use crate::qq_media::{reply_c2c_with_png_bytes, reply_group_with_png_bytes};
use crate::worker::WorkerPool;

pub struct MyBot {
    pub pool: Arc<WorkerPool>,
    /// 与 [`Client`] 相同 `sandbox`/`timeout`，用于自选 JSON 上传体（见 `qq_media`）。
    pub http: HttpClient,
}

/// 文本回复或直接发渲染出的 PNG。
enum CommandOutcome {
    Text(String),
    Png(Vec<u8>),
}

/// 截取字符串，用于日志预览
fn char_prefix(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect() //姑且不管字素簇
}

const HELP_MESSAGE_PATH: &str = "resources/help_message.txt";

/// 从 `resources/help_message.txt` 读取 `/help` 文案；运行时可修改无需重启。
async fn load_help_message() -> String {
    match tokio::fs::read_to_string(HELP_MESSAGE_PATH).await {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            warn!(path = HELP_MESSAGE_PATH, error = %e, "读取 /help 文案失败，回退为简短提示");
            "帮助文案暂不可用。".to_string()
        }
    }
}

/// 处理命令文本；`None` 表示不需回复。
async fn process_command(pool: &WorkerPool, text: &str) -> Option<CommandOutcome> {
    if text.starts_with("/tex") {
        let source = text.trim_start_matches("/tex").trim();
        debug!(
            source_len = source.len(),
            source_preview = char_prefix(source, 80),
            "解析到 /tex 命令"
        );
        if source.is_empty() {
            debug!("source 为空，返回帮助提示");
            Some(CommandOutcome::Text(
                "请提供 Typst 代码，例如：/tex $E = mc^2$".to_string(),
            ))
        } else {
            debug!("开始调用 pool.render()");
            match pool.render(source, "quiconf").await {
                Ok(image_bytes) => {
                    debug!(png_bytes = image_bytes.len(), "pool.render() 返回成功");
                    Some(CommandOutcome::Png(image_bytes))
                }
                Err(e) => {
                    warn!("pool.render() 返回错误：{}", e);
                    Some(CommandOutcome::Text(format!("渲染失败：{}", e)))
                }
            }
        }
    } else {
        match text {
            "/help" => {
                debug!("匹配到 /help 命令");
                Some(CommandOutcome::Text(load_help_message().await))
            }
            "/about" => {
                debug!("匹配到 /about 命令");
                Some(CommandOutcome::Text(
                    "Typst 渲染！".to_string(),
                ))
            }
            "/typd" => {
                debug!("匹配到 /typd 命令");
                Some(CommandOutcome::Text(
                    "Typst 渲染！请暂时用 /tex 接口。".to_string(),
                ))
            }
            "/typm" => {
                debug!("匹配到 /typm 命令");
                Some(CommandOutcome::Text(
                    "Typst 公式渲染！请暂时用 /tex 接口。".to_string(),
                ))
            }
            _ => {
                debug!(text = %text, "消息不匹配任何命令，跳过");
                None
            }
        }
    }
}

#[async_trait::async_trait]
impl EventHandler for MyBot {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!("机器人已就绪！登录为：{}", ready.user.username);
    }

    async fn group_message_create(&self, ctx: Context, message: GroupMessage) {
        debug!(
            group_id = ?message.group_openid,
            msg_id = ?message.id,
            has_content = message.content.is_some(),
            "收到 group_message_create 事件"
        );

        let content = match &message.content {
            Some(content) => content,
            None => {
                debug!("消息 content 为 None，跳过");
                return;
            }
        };

        info!("收到群聊消息：{}", content);

        let outcome = match process_command(&self.pool, content.trim()).await {
            Some(o) => o,
            None => return,
        };

        match outcome {
            CommandOutcome::Text(response) => {
                debug!(
                    response_len = response.len(),
                    response_preview = char_prefix(&response, 80),
                    "准备发送文本回复"
                );
                match message.reply(&ctx.api, &ctx.token, &response).await {
                    Ok(_) => info!("群聊回复发送成功"),
                    Err(e) => warn!("群聊发送回复失败：{}", e),
                }
            }
            CommandOutcome::Png(bytes) => {
                debug!(png_len = bytes.len(), "准备发送群聊图片回复");
                match reply_group_with_png_bytes(
                    &message,
                    &ctx.api,
                    &ctx.token,
                    &self.http,
                    None,
                    &bytes,
                )
                .await
                {
                    Ok(_) => info!("群聊图片回复发送成功"),
                    Err(e) => {
                        warn!("群聊图片发送失败：{:?}，尝试回退为文本", e);
                        let fallback = format!("图片已生成但发送失败：{}", e);
                        if let Err(e2) = message.reply(&ctx.api, &ctx.token, &fallback).await {
                            warn!("群聊回退文本也失败：{}", e2);
                        }
                    }
                }
            }
        }
    }

    async fn c2c_message_create(&self, ctx: Context, message: C2CMessage) {
        debug!(
            msg_id = ?message.id,
            has_content = message.content.is_some(),
            "收到 c2c_message_create 事件"
        );

        let content = match &message.content {
            Some(content) => content,
            None => {
                debug!("消息 content 为 None，跳过");
                return;
            }
        };

        info!("收到私聊消息：{}", content);

        let outcome = match process_command(&self.pool, content.trim()).await {
            Some(o) => o,
            None => return,
        };

        match outcome {
            CommandOutcome::Text(response) => {
                debug!(
                    response_len = response.len(),
                    response_preview = char_prefix(&response, 80),
                    "准备发送文本回复"
                );
                match message.reply(&ctx.api, &ctx.token, &response).await {
                    Ok(_) => info!("私聊回复发送成功"),
                    Err(e) => warn!("私聊发送回复失败：{}", e),
                }
            }
            CommandOutcome::Png(bytes) => {
                debug!(png_len = bytes.len(), "准备发送私聊图片回复");
                match reply_c2c_with_png_bytes(
                    &message,
                    &ctx.api,
                    &ctx.token,
                    &self.http,
                    None,
                    &bytes,
                )
                .await
                {
                    Ok(_) => info!("私聊图片回复发送成功"),
                    Err(e) => {
                        warn!("私聊图片发送失败：{:?}，尝试回退为文本", e);
                        let fallback = format!("图片已生成但发送失败：{}", e);
                        if let Err(e2) = message.reply(&ctx.api, &ctx.token, &fallback).await {
                            warn!("私聊回退文本也失败：{}", e2);
                        }
                    }
                }
            }
        }
    }
}

pub async fn run_bot(
    pool: Arc<WorkerPool>,
    qq_sandbox: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("启动机器人...");

    let app_id =
        std::env::var("QQ_BOT_APP_ID").expect("未设置 QQ_BOT_APP_ID 环境变量");
    let secret =
        std::env::var("QQ_BOT_SECRET").expect("未设置 QQ_BOT_SECRET 环境变量");

    debug!(app_id = %app_id, "已读取环境变量凭证");

    let token = Token::new(app_id, secret);

    let intents = Intents::default()
        .with_direct_message()
        .with_interaction()
        .with_public_messages();

    debug!(?intents, "配置 intents");

    let http = HttpClient::new(botrs::DEFAULT_TIMEOUT, qq_sandbox)?;

    let mut client = Client::new(
        token,
        intents,
        MyBot {
            pool,
            http: http.clone(),
        },
        qq_sandbox,
    )?;

    info!("连接到 QQ...");
    debug!("调用 client.start()，等待 WebSocket 连接...");

    client.start().await?;

    Ok(())
}
