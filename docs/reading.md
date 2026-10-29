# 阅读状态

普通会话支持本人阅读水位查询与精确消息边界写回。查询通过 `conversations.info.last_read`，写入通过 `conversations.mark`；Slack 时间戳保留全部六位微秒，不经浮点数或毫秒换算。独立话题通过公开 `conversations.replies` 的根消息 `last_read` 查询本人阅读位置；核对根消息与所属话题，缺少字段时保持未知，不根据最新回复或未读数推断。公开接口未提供独立话题写回，继续不声明。

写入前核对会话、消息与位置，已读过的目标不再写；消息归属由目标会话的精确历史查询确认。每次只发送一次写请求，再只读回查确认。连接中断、响应无法判断或回查未确认时返回 `unknown`，不自动重复写入。

新登录增加 `channels:write`、`groups:write`、`im:write`、`mpim:write` 四项阅读写权限。旧授权仍可登录和读消息；缺少所需权限时明确返回权限错误，可重新授权补齐，不把缺权限解释成未读。

自动化测试覆盖精确边界、缺权限、响应不确定、不存在消息、跨范围拒绝，以及话题根消息字段缺失、畸形与串范围。真实验收结果记录在各次任务的报告中。

Slack 不提供他人已读回执，不声明他人阅读人数、分母或人员名单。接口依据：[conversations.mark](https://docs.slack.dev/reference/methods/conversations.mark/)、[conversations.replies](https://docs.slack.dev/reference/methods/conversations.replies/)。
