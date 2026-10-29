//! 读取与动作（protocol-v1 §7–§10）：把宿主的参数交给 Slack 适配器（`slack/methods.rs`，规则同内置 Slack），结果本来就是
//! 协议的统一格式，写成 JSON 回去。平台请求数由运行时数、随结果报给宿主。
//!
//! 授权的事（用前看一眼、被拒时强制续期一次）在 [`Server::with_client`]；资源与头像只用现在能用的客户端，不为它续期。

use std::sync::Arc;

use hhh_im_protocol::method as pm;
use hhh_im_protocol::rpc::RpcError;
use hhh_im_protocol::runtime::{Segments, Uploads, chunk_of_bytes};
use hhh_im_protocol::types::ResourceKind;
use serde_json::{Value, json};

use super::Server;
use super::session::rpc_error;
use crate::ErrorKind;
use crate::slack::UPLOAD_MAX_BYTES;

/// 发过的消息记着多少个（同一个操作编号再来时回同一个结果，§10）。
const SENDS_REMEMBERED: usize = 256;

/// 发过的消息：操作编号 → 结果。
#[derive(Default)]
pub(crate) struct Sends {
    done: tokio::sync::Mutex<Vec<(String, Arc<tokio::sync::OnceCell<Value>>)>>,
}

/// 资源分段与上传（一个插件进程一份）。
pub(crate) struct Transfers {
    pub(crate) segments: Segments,
    pub(crate) uploads: Uploads,
}

impl Default for Transfers {
    fn default() -> Self {
        let max = UPLOAD_MAX_BYTES as u64;
        Self {
            segments: Segments::default(),
            // 图片与文件同一个上限（同内置 Slack 的发送上限）；表情包不能上传。
            uploads: Uploads::new(
                move |kind| (kind != ResourceKind::Sticker).then_some(max),
                format!(
                    "图片与文件须在 {} MB 以内，且不能为空。",
                    UPLOAD_MAX_BYTES / 1024 / 1024
                ),
            ),
        }
    }
}

fn to_value(value: impl serde::Serialize) -> Value {
    serde_json::to_value(value).expect("结果总能写成 JSON")
}

impl Server {
    pub(crate) async fn reading_mark(
        &self,
        params: hhh_im_protocol::reading::MarkReadParams,
    ) -> Result<Value, RpcError> {
        self.with_client(|client| {
            let params = params.clone();
            async move { client.reading_mark(&params).await }
        })
        .await
        .map(to_value)
    }

    pub(crate) async fn reading_sync(
        &self,
        params: hhh_im_protocol::reading::ReadSyncParams,
    ) -> Result<Value, RpcError> {
        self.with_client(|client| {
            let params = params.clone();
            async move { client.reading_sync(&params).await }
        })
        .await
        .map(to_value)
    }

    /// `conversations/list`。
    pub(crate) async fn conversations_list(
        &self,
        params: pm::ConversationsListParams,
    ) -> Result<Value, RpcError> {
        let limit = params.limit;
        let page = self
            .with_client(|client| {
                let cursor = params.cursor.clone();
                async move { client.list_conversations(limit, cursor.as_deref()).await }
            })
            .await?;
        Ok(to_value(page))
    }

    /// `conversations/get`。
    pub(crate) async fn conversations_get(
        &self,
        params: pm::ConversationsGetParams,
    ) -> Result<Value, RpcError> {
        let conversation = self
            .with_client(|client| {
                let id = params.conversation_id.clone();
                async move { client.get_conversation(&id).await }
            })
            .await?;
        Ok(to_value(conversation))
    }

    /// `conversations/search`。
    pub(crate) async fn conversations_search(
        &self,
        params: pm::SearchParams,
    ) -> Result<Value, RpcError> {
        let limit = params.limit;
        let page = self
            .with_client(|client| {
                let (text, cursor) = (params.text.clone(), params.cursor.clone());
                async move {
                    client
                        .search_conversations(&text, limit, cursor.as_deref())
                        .await
                }
            })
            .await?;
        Ok(to_value(page))
    }

    /// `conversations/members`。
    pub(crate) async fn conversations_members(
        &self,
        params: pm::MembersParams,
    ) -> Result<Value, RpcError> {
        let limit = params.limit;
        let page = self
            .with_client(|client| {
                let (id, cursor) = (params.conversation_id.clone(), params.cursor.clone());
                async move { client.list_members(&id, limit, cursor.as_deref()).await }
            })
            .await?;
        Ok(to_value(page))
    }

    /// `conversations/findDirect`。
    pub(crate) async fn conversations_find_direct(
        &self,
        params: pm::FindDirectParams,
    ) -> Result<Value, RpcError> {
        let found = self
            .with_client(|client| {
                let user = params.user_id.clone();
                async move { client.find_direct(&user).await }
            })
            .await?;
        Ok(to_value(found))
    }

    /// `conversations/recentActivity`（会话发现，新版搜索）。
    pub(crate) async fn conversations_recent_activity(
        &self,
        params: pm::RecentActivityParams,
    ) -> Result<Value, RpcError> {
        let (since_ms, pages_max) = (params.since_ms, params.pages_max);
        let result = self
            .with_client(|client| async move { client.recent_activity(since_ms, pages_max).await })
            .await?;
        Ok(to_value(result))
    }

    /// `people/get`：读人与按编号读机器人；明确没有的写进 `missing`，这次没读到的两边都不写（宿主下次再读）。
    pub(crate) async fn people_get(&self, params: pm::PeopleGetParams) -> Result<Value, RpcError> {
        if params.ids.is_empty() && params.bots.is_empty() {
            return Ok(to_value(pm::PeopleGetResult {
                found: Vec::new(),
                missing: Vec::new(),
            }));
        }
        let result = self
            .with_client(|client| {
                let (ids, bots) = (params.ids.clone(), params.bots.clone());
                async move { client.look_up_people(&ids, &bots).await }
            })
            .await?;
        Ok(to_value(result))
    }

    /// `people/search`。
    pub(crate) async fn people_search(&self, params: pm::SearchParams) -> Result<Value, RpcError> {
        let limit = params.limit;
        let page = self
            .with_client(|client| {
                let (text, cursor) = (params.text.clone(), params.cursor.clone());
                async move { client.search_people(&text, limit, cursor.as_deref()).await }
            })
            .await?;
        Ok(to_value(page))
    }

    /// `messages/list`（会话的历史，或一个话题）。
    pub(crate) async fn messages_list(
        &self,
        params: pm::MessagesListParams,
    ) -> Result<Value, RpcError> {
        let params = Arc::new(params);
        let page = self
            .with_client(|client| {
                let params = params.clone();
                async move { client.list_messages(&params).await }
            })
            .await?;
        Ok(to_value(page))
    }

    /// `messages/get`。
    pub(crate) async fn messages_get(
        &self,
        params: pm::MessagesGetParams,
    ) -> Result<Value, RpcError> {
        let message = self
            .with_client(|client| {
                let (conversation, id) =
                    (params.conversation_id.clone(), params.message_id.clone());
                async move { client.get_message(&conversation, &id).await }
            })
            .await?;
        Ok(to_value(message))
    }

    /// `reactions/list`（Slack 一次给全，不分页）。
    pub(crate) async fn reactions_list(
        &self,
        params: pm::ReactionsListParams,
    ) -> Result<Value, RpcError> {
        let page = self
            .with_client(|client| {
                let (conversation, id) =
                    (params.conversation_id.clone(), params.message_id.clone());
                async move { client.list_reactions(&conversation, &id).await }
            })
            .await?;
        Ok(to_value(page))
    }

    /// `emoji/catalog`。分类带上图标（HHH 契约 provider-protocol v31 的 `icon`，可选字段）：本插件锁的协议包还没有这个字段，
    /// 直接写进 JSON，升级协议包时换成 `EmojiCategory::icon`。
    pub(crate) async fn emoji_catalog(&self) -> Result<Value, RpcError> {
        let catalog = self
            .with_client(|client| async move { client.emoji_catalog().await })
            .await?;
        Ok(crate::slack::catalog_json(&catalog))
    }

    /// `emoji/image`：表情图很小（≤512 KB），整份读下来交出要的那一段。
    pub(crate) async fn emoji_image(
        &self,
        params: pm::EmojiImageParams,
    ) -> Result<Value, RpcError> {
        let bytes = self
            .with_ready_client(|client| {
                let name = params.emoji_type.clone();
                async move { client.emoji_image(&name).await }
            })
            .await?;
        Ok(to_value(chunk_of_bytes(
            &bytes,
            params.offset,
            params.length,
        )))
    }

    /// `resources/read`：同一个资源顺着读复用一条读流；文件从要的位置读（Range），读不了就从头读、运行时跳过去。
    pub(crate) async fn resources_describe(
        &self,
        params: pm::ResourcesDescribeParams,
    ) -> Result<Value, RpcError> {
        self.with_ready_client(|client| async move {
            client.describe_resource(&params.key, params.kind).await
        })
        .await
        .map(to_value)
    }

    pub(crate) async fn resources_read(
        &self,
        params: pm::ResourcesReadParams,
    ) -> Result<Value, RpcError> {
        let key = params.segment_key();
        let variant = params.variant.clone();
        let (resource, kind) = (params.key.clone(), params.kind);
        let chunk = self
            .transfers
            .segments
            .read(key, params.offset, params.length, |offset| async move {
                self.with_ready_client(|client| async move {
                    client
                        .open_resource_variant(&resource, kind, offset, variant.as_deref())
                        .await
                })
                .await
            })
            .await?;
        Ok(to_value(chunk))
    }

    /// `avatars/read`：头像键就是 Slack 给的头像地址（与内置时代本机记下的一样）。
    pub(crate) async fn avatars_read(
        &self,
        params: pm::AvatarsReadParams,
    ) -> Result<Value, RpcError> {
        let key = format!("avatar\u{1f}{}", params.key);
        let url = params.key.clone();
        let chunk = self
            .transfers
            .segments
            .read(key, params.offset, params.length, |_| async move {
                self.with_ready_client(|client| async move { client.open_avatar(&url).await })
                    .await
            })
            .await?;
        Ok(to_value(chunk))
    }

    /// `uploads/finish`：攒齐、核对过的整份交给 Slack（要上传地址、传字节），回文件编号当资源键。
    pub(crate) async fn uploads_finish(
        &self,
        params: pm::UploadsFinishParams,
    ) -> Result<Value, RpcError> {
        let upload = self.transfers.uploads.finish(params)?;
        let upload = Arc::new(upload);
        let key = self
            .with_client(|client| {
                let upload = upload.clone();
                async move { client.upload_bytes(&upload.name, upload.data.clone()).await }
            })
            .await?;
        Ok(to_value(pm::UploadsFinishResult { key }))
    }

    /// `messages/send`：同一个操作编号只发一次，再来时回同一个结果（§10）；没发成的忘掉，宿主可以重试。
    pub(crate) async fn messages_send(
        &self,
        params: pm::MessagesSendParams,
    ) -> Result<Value, RpcError> {
        let cell = {
            let mut done = self.sends.done.lock().await;
            match done.iter().find(|(id, _)| *id == params.operation_id) {
                Some((_, cell)) => cell.clone(),
                None => {
                    if done.len() >= SENDS_REMEMBERED {
                        done.remove(0);
                    }
                    let cell = Arc::new(tokio::sync::OnceCell::new());
                    done.push((params.operation_id.clone(), cell.clone()));
                    cell
                }
            }
        };
        let params = Arc::new(params);
        let result = cell
            .get_or_try_init(|| async {
                let outcome = self
                    .with_client(|client| {
                        let params = params.clone();
                        async move {
                            client
                                .send_message(&params.operation_id, &params.target, &params.content)
                                .await
                        }
                    })
                    .await?;
                Ok::<_, RpcError>(to_value(outcome))
            })
            .await;
        match result {
            Ok(value) => Ok(value.clone()),
            Err(error) => {
                self.sends
                    .done
                    .lock()
                    .await
                    .retain(|(id, kept)| !(*id == params.operation_id && Arc::ptr_eq(kept, &cell)));
                Err(error)
            }
        }
    }

    /// `messages/edit`：回平台记下的这条消息。
    pub(crate) async fn messages_edit(
        &self,
        params: pm::MessagesEditParams,
    ) -> Result<Value, RpcError> {
        let params = Arc::new(params);
        let message = self
            .with_client(|client| {
                let params = params.clone();
                async move {
                    client
                        .edit_message(&params.conversation_id, &params.message_id, &params.content)
                        .await
                }
            })
            .await?;
        Ok(to_value(message))
    }

    /// `messages/recall`。
    pub(crate) async fn messages_recall(
        &self,
        params: pm::MessagesRecallParams,
    ) -> Result<Value, RpcError> {
        self.with_client(|client| {
            let (conversation, id) = (params.conversation_id.clone(), params.message_id.clone());
            async move { client.recall_message(&conversation, &id).await }
        })
        .await?;
        Ok(json!({}))
    }

    /// `reactions/add`：回这个表情（Slack 没有表情编号，用会话、消息、人与表情名拼一个）。
    pub(crate) async fn reactions_add(
        &self,
        params: pm::ReactionsAddParams,
    ) -> Result<Value, RpcError> {
        let reaction = self
            .with_client(|client| {
                let (conversation, id, name) = (
                    params.conversation_id.clone(),
                    params.message_id.clone(),
                    params.emoji_type.clone(),
                );
                async move { client.add_message_reaction(&conversation, &id, &name).await }
            })
            .await?;
        Ok(to_value(reaction))
    }

    /// `reactions/remove`。
    pub(crate) async fn reactions_remove(
        &self,
        params: pm::ReactionsRemoveParams,
    ) -> Result<Value, RpcError> {
        self.with_client(|client| {
            let (conversation, id, reaction) = (
                params.conversation_id.clone(),
                params.message_id.clone(),
                params.reaction_id.clone(),
            );
            async move {
                client
                    .remove_message_reaction(&conversation, &id, &reaction)
                    .await
            }
        })
        .await?;
        Ok(json!({}))
    }

    /// 能力里没声明的方法（`messages/lookupSend`、`messages/retranslate`）：说清楚不支持。
    pub(crate) fn unsupported(method: &str) -> RpcError {
        rpc_error(
            ErrorKind::Unsupported,
            format!("Slack 插件不支持 {method}。"),
        )
    }
}
