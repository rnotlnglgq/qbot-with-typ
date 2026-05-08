//! 群 / C2C 富媒体上传与被动回复。
//!
//! 腾讯开放文档主推 `url` 拉取上传；本站另试论坛流传的 **`file_data`（BASE64）**，非文档保证字段，服务端行为可能变更。

use base64::Engine;
use botrs::{
    api::BotApi,
    http::HttpClient,
    models::message::{C2CMessageParams, GroupMessageParams, Media},
    token::Token,
    BotError, C2CMessage, GroupMessage,
};

const FILE_TYPE_IMAGE: u32 = 1;

/// 不包含 `url`；若服务端拒绝，可再试附带 `"url": ""` 的请求体变种（见模组注释）。
fn upload_file_body_base64(image_bytes: &[u8]) -> serde_json::Value {
    let file_data = base64::engine::general_purpose::STANDARD.encode(image_bytes);
    serde_json::json!({
        "file_type": FILE_TYPE_IMAGE,
        "srv_send_msg": false,
        "file_data": file_data,
    })
}

async fn upload_group_file_data(
    http: &HttpClient,
    token: &Token,
    group_openid: &str,
    image_bytes: &[u8],
) -> botrs::error::Result<serde_json::Value> {
    let path = format!("/v2/groups/{group_openid}/files");
    let body = upload_file_body_base64(image_bytes);
    http.post(token, &path, None::<&()>, Some(&body)).await
}

async fn upload_c2c_file_data(
    http: &HttpClient,
    token: &Token,
    user_openid: &str,
    image_bytes: &[u8],
) -> botrs::error::Result<serde_json::Value> {
    let path = format!("/v2/users/{user_openid}/files");
    let body = upload_file_body_base64(image_bytes);
    http.post(token, &path, None::<&()>, Some(&body)).await
}

fn value_to_media(v: serde_json::Value) -> Result<Media, BotError> {
    serde_json::from_value::<Media>(v).map_err(BotError::from)
}

/// 上传 PNG 后以 `msg_type: 7` 被动回复群消息。
pub async fn reply_group_with_png_bytes(
    message: &GroupMessage,
    api: &BotApi,
    token: &Token,
    http: &HttpClient,
    caption: Option<&str>,
    png_bytes: &[u8],
) -> Result<botrs::models::api::MessageResponse, BotError> {
    let group_openid = message
        .group_openid
        .as_deref()
        .ok_or_else(|| BotError::InvalidData("Missing group_openid".into()))?;

    let msg_id = message
        .id
        .as_deref()
        .ok_or_else(|| BotError::InvalidData("Missing message id for passive reply".into()))?;

    let upload = upload_group_file_data(http, token, group_openid, png_bytes).await?;

    tracing::debug!(upload_preview = ?upload, "群文件上传接口响应");

    let media = value_to_media(upload)?;

    let params = GroupMessageParams {
        msg_type: 7,
        content: caption.map(|s| s.to_string()),
        msg_id: Some(msg_id.to_string()),
        event_id: message.event_id.clone(),
        media: Some(media),
        ..Default::default()
    };

    api.post_group_message_with_params(token, group_openid, params)
        .await
}

/// 上传 PNG 后以 `msg_type: 7` 被动回复单聊。
pub async fn reply_c2c_with_png_bytes(
    message: &C2CMessage,
    api: &BotApi,
    token: &Token,
    http: &HttpClient,
    caption: Option<&str>,
    png_bytes: &[u8],
) -> Result<botrs::models::api::MessageResponse, BotError> {
    let user_openid = message
        .author
        .as_ref()
        .and_then(|a| a.user_openid.as_deref())
        .ok_or_else(|| BotError::InvalidData("Missing user_openid".into()))?;

    let msg_id = message
        .id
        .as_deref()
        .ok_or_else(|| BotError::InvalidData("Missing message id for passive reply".into()))?;

    let upload = upload_c2c_file_data(http, token, user_openid, png_bytes).await?;

    tracing::debug!(upload_preview = ?upload, "C2C 文件上传接口响应");

    let media = value_to_media(upload)?;

    let params = C2CMessageParams {
        msg_type: 7,
        content: caption.map(|s| s.to_string()),
        msg_id: Some(msg_id.to_string()),
        msg_seq: Some(1),
        event_id: message.event_id.clone(),
        media: Some(media),
        ..Default::default()
    };

    api.post_c2c_message_with_params(token, user_openid, params)
        .await
}
