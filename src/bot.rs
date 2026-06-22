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

const HELP_MESSAGES_DIR: &str = "resources/help-messages";

/// 将 `/help [topic]` 的 topic 映射到 `resources/help-messages/` 下的文件名。
fn help_message_filename(topic: Option<&str>) -> Option<&'static str> {
    match topic {
        None => Some("help.txt"),
        Some("tag") => Some("help_tag.txt"),
        Some("dep") => Some("help_dep.txt"),
        Some("example") => Some("help_example.txt"),
        Some(_) => None,
    }
}

/// 从 `resources/help-messages/` 读取 `/help [topic]` 文案；运行时可修改无需重启。
async fn load_help_message(topic: Option<&str>) -> String {
    let Some(filename) = help_message_filename(topic) else {
        warn!(topic = ?topic, "未知 /help 主题");
        return "未知帮助主题，可用：tag、dep、example".to_string();
    };
    let path = format!("{HELP_MESSAGES_DIR}/{filename}");
    match tokio::fs::read_to_string(&path).await {
        Ok(s) => {
            let s = s.trim().to_string();
            if s.is_empty() {
                if topic == Some("example") {
                    "例文暂未提供。".to_string()
                } else {
                    warn!(path = %path, "帮助文案为空");
                    "帮助文案暂不可用。".to_string()
                }
            } else {
                s
            }
        }
        Err(e) => {
            warn!(path = %path, error = %e, "读取 /help 文案失败，回退为简短提示");
            "帮助文案暂不可用。".to_string()
        }
    }
}

/// 处理命令文本；`None` 表示不需回复。
async fn process_command(pool: &WorkerPool, text: &str) -> Option<CommandOutcome> {
    if text == "/help" || text.starts_with("/help ") {
        let topic = text.strip_prefix("/help").unwrap_or("").trim();
        let topic = if topic.is_empty() {
            None
        } else {
            Some(topic)
        };
        debug!(?topic, "匹配到 /help 命令");
        return Some(CommandOutcome::Text(load_help_message(topic).await));
    }

    if text.starts_with("/typd") {
        let source = text.trim_start_matches("/typd").trim();
        debug!(
            source_len = source.len(),
            source_preview = char_prefix(source, 80),
            "匹配到 /typd 命令"
        );
        if source.is_empty() {
            debug!("source 为空，返回帮助提示");
            Some(CommandOutcome::Text(
                "请提供 Typst 代码，例如：/typd $E = mc^2$".to_string(),
            ))
        } else {
            let source = rewrite_tag_shorthand(source);
            debug!(
                source_len = source.len(),
                source_preview = char_prefix(&source, 80),
                "tag 简写改写后的 source"
            );
            debug!("开始调用 pool.render()");
            match pool.render(&source, "quiconf").await {
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
            "/about" => {
                debug!("匹配到 /about 命令");
                Some(CommandOutcome::Text(
                    "Typst 渲染！".to_string(),
                ))
            }
            "/tex" => {
                debug!("匹配到 /tex 命令");
                Some(CommandOutcome::Text(
                    "目前停止提供TeX渲染，请通过 /typd 指令使用Typst渲染。".to_string(),
                ))
            }
            "/typm" => {
                debug!("匹配到 /typm 命令");
                Some(CommandOutcome::Text(
                    "未注册指令。".to_string(),
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


// ======== Tag shorthand

/// tag 简写 token 允许的单个字符：拉丁字母、数字，以及嵌入 Typst 字符串/长度时无需转义的 `-`、`_`、`.`。
fn is_tag_shorthand_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
}

/// tag 简写首行各段须为非空，且仅含 [`is_tag_shorthand_token_char`] 允许的字符。
fn is_tag_shorthand_token(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_tag_shorthand_token_char)
}

/// 将 tag 简写中的一个参数格式化为 `tag.with(...)` 内的片段。
fn format_tag_with_arg(token: &str) -> String {
    debug_assert!(is_tag_shorthand_token(token));
    if token.as_bytes()[0].is_ascii_digit() {
        token.to_string()
    } else {
        format!("\"{}\"", token)
    }
}

/// 按首个 `\n` 拆成首行与剩余正文；`\n` 为单字节，切片位置合法。
/// 首行若以 `\r\n` 结尾则剥掉 `\r`。
/// 剩余部分为 `None` 表示原文不含换行；`Some("")` 表示首行后紧跟换行但无后续正文。
fn split_first_line(source: &str) -> (&str, Option<&str>) {
    match source.find('\n') {
        Some(i) => {
            let first_line = source[..i].strip_suffix('\r').unwrap_or(&source[..i]);
            (first_line, Some(&source[i + 1..]))
        }
        None => (source, None),
    }
}

/// 若首行为逗号分隔的可见 ASCII 标记列表，则改写为 `#show: tag.with(...)`。
fn rewrite_tag_shorthand(source: &str) -> String {
    let (first_line, rest) = split_first_line(source);

    if !first_line.contains(',') {
        return source.to_string();
    }

    let tokens: Vec<String> = first_line
        .split(',')
        .map(str::trim)
        .map(str::to_string)
        .collect();
    if tokens.iter().any(String::is_empty) || !tokens.iter().all(|t| is_tag_shorthand_token(t)) {
        return source.to_string();
    }

    let args = tokens
        .iter()
        .map(|t| format_tag_with_arg(t))
        .collect::<Vec<_>>()
        .join(", ");
    let rewritten = format!("#show: tag.with({args})");

    match rest {
        None => rewritten,
        Some(rest) => format!("{rewritten}\n{rest}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_tag_shorthand_expands_comma_list() {
        assert_eq!(
            rewrite_tag_shorthand("  a1, bbb, c-d,14pt "),
            "#show: tag.with(\"a1\", \"bbb\", \"c-d\", 14pt)"
        );
    }

    #[test]
    fn rewrite_tag_shorthand_preserves_following_lines() {
        assert_eq!(
            rewrite_tag_shorthand("a1, bbb\n= Title"),
            "#show: tag.with(\"a1\", \"bbb\")\n= Title"
        );
    }

    #[test]
    fn rewrite_tag_shorthand_preserves_trailing_newline_after_tag_line() {
        assert_eq!(
            rewrite_tag_shorthand("a1, bbb\n"),
            "#show: tag.with(\"a1\", \"bbb\")\n"
        );
    }

    #[test]
    fn rewrite_tag_shorthand_handles_crlf_after_tag_line() {
        assert_eq!(
            rewrite_tag_shorthand("a1, bbb\r\n= Title"),
            "#show: tag.with(\"a1\", \"bbb\")\n= Title"
        );
    }

    #[test]
    fn rewrite_tag_shorthand_preserves_utf8_body() {
        let input = "a1, bbb\n中文*加粗*与_强调_";
        assert_eq!(
            rewrite_tag_shorthand(input),
            "#show: tag.with(\"a1\", \"bbb\")\n中文*加粗*与_强调_"
        );
    }

    #[test]
    fn rewrite_tag_shorthand_skips_non_matching_first_line() {
        let input = "$E = mc^2$";
        assert_eq!(rewrite_tag_shorthand(input), input);
    }

    #[test]
    fn rewrite_tag_shorthand_skips_utf8_on_first_line() {
        let input = "中文, bbb\n= Title";
        assert_eq!(rewrite_tag_shorthand(input), input);
    }

    #[test]
    fn rewrite_tag_shorthand_expands_decimal_length() {
        assert_eq!(
            rewrite_tag_shorthand("lxgw, 1.5pt\n= Title"),
            "#show: tag.with(\"lxgw\", 1.5pt)\n= Title"
        );
    }

    #[test]
    fn rewrite_tag_shorthand_skips_token_with_quote() {
        let input = "a\"1, bbb";
        assert_eq!(rewrite_tag_shorthand(input), input);
    }

    #[test]
    fn rewrite_tag_shorthand_skips_token_with_backslash() {
        let input = r"a\1, bbb";
        assert_eq!(rewrite_tag_shorthand(input), input);
    }

    #[test]
    fn rewrite_tag_shorthand_skips_token_with_typst_syntax_char() {
        let input = "a1, #foo";
        assert_eq!(rewrite_tag_shorthand(input), input);
    }

    #[test]
    fn rewrite_tag_shorthand_skips_empty_segment() {
        let input = "a1,,bbb";
        assert_eq!(rewrite_tag_shorthand(input), input);
    }

    #[test]
    fn help_message_filename_maps_known_topics() {
        assert_eq!(help_message_filename(None), Some("help.txt"));
        assert_eq!(help_message_filename(Some("tag")), Some("help_tag.txt"));
        assert_eq!(help_message_filename(Some("dep")), Some("help_dep.txt"));
        assert_eq!(help_message_filename(Some("example")), Some("help_example.txt"));
    }

    #[test]
    fn help_message_filename_rejects_unknown_topic() {
        assert_eq!(help_message_filename(Some("foo")), None);
    }

    #[test]
    fn split_first_line_strips_cr_before_lf() {
        assert_eq!(
            split_first_line("a1, bbb\r\nrest"),
            ("a1, bbb", Some("rest"))
        );
    }

    #[test]
    fn split_first_line_distinguishes_no_newline_from_trailing_newline() {
        assert_eq!(split_first_line("a1, bbb"), ("a1, bbb", None));
        assert_eq!(split_first_line("a1, bbb\n"), ("a1, bbb", Some("")));
    }
}
