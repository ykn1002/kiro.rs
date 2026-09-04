//! 流式响应处理模块
//!
//! 实现 Kiro → Anthropic 流式响应转换和 SSE 状态管理

use std::collections::HashMap;

use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::kiro::model::events::Event;

/// 为 extended thinking 块生成占位 signature（Kiro 上游不返回真实 signature）
pub fn compute_thinking_signature(thinking: &str) -> String {
    let hash = Sha256::digest(thinking.as_bytes());
    hex::encode(hash)
}

/// 找到小于等于目标位置的最近有效UTF-8字符边界
///
/// UTF-8字符可能占用1-4个字节，直接按字节位置切片可能会切在多字节字符中间导致panic。
/// 这个函数从目标位置向前搜索，找到最近的有效字符边界。
fn find_char_boundary(s: &str, target: usize) -> usize {
    if target >= s.len() {
        return s.len();
    }
    if target == 0 {
        return 0;
    }
    // 从目标位置向前搜索有效的字符边界
    let mut pos = target;
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

/// 需要跳过的包裹字符
///
/// 当 thinking 标签被这些字符包裹时，认为是在引用标签而非真正的标签：
/// - 反引号 (`)：行内代码
/// - 双引号 (")：字符串
/// - 单引号 (')：字符串
const QUOTE_CHARS: &[u8] = &[
    b'`', b'"', b'\'', b'\\', b'#', b'!', b'@', b'$', b'%', b'^', b'&', b'*', b'(', b')', b'-',
    b'_', b'=', b'+', b'[', b']', b'{', b'}', b';', b':', b'<', b'>', b',', b'.', b'?', b'/',
];

/// 检查指定位置的字符是否是引用字符
fn is_quote_char(buffer: &str, pos: usize) -> bool {
    buffer
        .as_bytes()
        .get(pos)
        .map(|c| QUOTE_CHARS.contains(c))
        .unwrap_or(false)
}

/// 查找真正的 thinking 结束标签（不被引用字符包裹，且后面有双换行符）
///
/// 当模型在思考过程中提到 `</thinking>` 时，通常会用反引号、引号等包裹，
/// 或者在同一行有其他内容（如"关于 </thinking> 标签"）。
/// 这个函数会跳过这些情况，只返回真正的结束标签位置。
///
/// 跳过的情况：
/// - 被引用字符包裹（反引号、引号等）
/// - 后面没有双换行符（真正的结束标签后面会有 `\n\n`）
/// - 标签在缓冲区末尾（流式处理时需要等待更多内容）
///
/// # 参数
/// - `buffer`: 要搜索的字符串
///
/// # 返回值
/// - `Some(pos)`: 真正的结束标签的起始位置
/// - `None`: 没有找到真正的结束标签
fn find_real_thinking_end_tag(buffer: &str) -> Option<usize> {
    const TAG: &str = "</thinking>";
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;

        // 检查前面是否有引用字符
        let has_quote_before = absolute_pos > 0 && is_quote_char(buffer, absolute_pos - 1);

        // 检查后面是否有引用字符
        let after_pos = absolute_pos + TAG.len();
        let has_quote_after = is_quote_char(buffer, after_pos);

        // 如果被引用字符包裹，跳过
        if has_quote_before || has_quote_after {
            search_start = absolute_pos + 1;
            continue;
        }

        // 检查后面的内容
        let after_content = &buffer[after_pos..];

        // 如果标签后面内容不足以判断是否有双换行符，等待更多内容
        if after_content.len() < 2 {
            return None;
        }

        // 真正的 thinking 结束标签后面会有双换行符 `\n\n`
        if after_content.starts_with("\n\n") {
            return Some(absolute_pos);
        }

        // 不是双换行符，跳过继续搜索
        search_start = absolute_pos + 1;
    }

    None
}

/// 查找缓冲区末尾的 thinking 结束标签（允许末尾只有空白字符）
///
/// 用于“边界事件”场景：例如 thinking 结束后立刻进入 tool_use，或流结束，
/// 此时 `</thinking>` 后面可能没有 `\n\n`，但结束标签依然应被识别并过滤。
///
/// 约束：只有当 `</thinking>` 之后全部都是空白字符时才认为是结束标签，
/// 以避免在 thinking 内容中提到 `</thinking>`（非结束标签）时误判。
fn find_real_thinking_end_tag_at_buffer_end(buffer: &str) -> Option<usize> {
    const TAG: &str = "</thinking>";
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;

        // 检查前面是否有引用字符
        let has_quote_before = absolute_pos > 0 && is_quote_char(buffer, absolute_pos - 1);

        // 检查后面是否有引用字符
        let after_pos = absolute_pos + TAG.len();
        let has_quote_after = is_quote_char(buffer, after_pos);

        if has_quote_before || has_quote_after {
            search_start = absolute_pos + 1;
            continue;
        }

        // 只有当标签后面全部是空白字符时才认定为结束标签
        if buffer[after_pos..].trim().is_empty() {
            return Some(absolute_pos);
        }

        search_start = absolute_pos + 1;
    }

    None
}

/// 查找真正的 thinking 开始标签（不被引用字符包裹）
///
/// 与 `find_real_thinking_end_tag` 类似，跳过被引用字符包裹的开始标签。
fn find_real_thinking_start_tag(buffer: &str) -> Option<usize> {
    const TAG: &str = "<thinking>";
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;

        // 检查前面是否有引用字符
        let has_quote_before = absolute_pos > 0 && is_quote_char(buffer, absolute_pos - 1);

        // 检查后面是否有引用字符
        let after_pos = absolute_pos + TAG.len();
        let has_quote_after = is_quote_char(buffer, after_pos);

        // 如果不被引用字符包裹，则是真正的开始标签
        if !has_quote_before && !has_quote_after {
            return Some(absolute_pos);
        }

        // 继续搜索下一个匹配
        search_start = absolute_pos + 1;
    }

    None
}

/// 从完整文本中提取 thinking 块（用于非流式响应）
///
/// 使用与流式处理相同的标签检测逻辑（引用字符过滤），确保一致性。
/// 非流式场景下文本已完整，无需处理跨 chunk 分割问题。
///
/// # 返回值
/// - `(Some(thinking_content), remaining_text)` — 检测到有效 thinking 块
/// - `(None, original_text)` — 未检测到，原样返回
pub(crate) fn extract_thinking_from_complete_text(text: &str) -> (Option<String>, String) {
    let start_pos = match find_real_thinking_start_tag(text) {
        Some(pos) => pos,
        None => return (None, text.to_string()),
    };

    let before = &text[..start_pos];
    let after_open = &text[start_pos + "<thinking>".len()..];

    // 查找结束标签：优先匹配带 \n\n 后缀的，退而使用末尾匹配
    let (thinking_raw, text_after) = if let Some(end_pos) = find_real_thinking_end_tag(after_open) {
        (
            &after_open[..end_pos],
            &after_open[end_pos + "</thinking>\n\n".len()..],
        )
    } else if let Some(end_pos) = find_real_thinking_end_tag_at_buffer_end(after_open) {
        let after_tag = end_pos + "</thinking>".len();
        (&after_open[..end_pos], after_open[after_tag..].trim_start())
    } else {
        // 找不到有效的结束标签，不做提取
        return (None, text.to_string());
    };

    // 剥离开头的换行符（与流式处理一致：模型输出 <thinking>\n）
    let thinking_content = thinking_raw.strip_prefix('\n').unwrap_or(thinking_raw);

    // 组装剩余文本：跳过纯空白的 before 部分
    let mut remaining = String::new();
    if !before.trim().is_empty() {
        remaining.push_str(before);
    }
    remaining.push_str(text_after);

    if thinking_content.is_empty() {
        (None, remaining)
    } else {
        (Some(thinking_content.to_string()), remaining)
    }
}

/// SSE 事件
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: serde_json::Value,
}

impl SseEvent {
    pub fn new(event: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            event: event.into(),
            data,
        }
    }

    /// 格式化为 SSE 字符串
    pub fn to_sse_string(&self) -> String {
        format!(
            "event: {}\ndata: {}\n\n",
            self.event,
            serde_json::to_string(&self.data).unwrap_or_default()
        )
    }
}

/// 内容块状态
#[derive(Debug, Clone)]
struct BlockState {
    block_type: String,
    started: bool,
    stopped: bool,
}

impl BlockState {
    fn new(block_type: impl Into<String>) -> Self {
        Self {
            block_type: block_type.into(),
            started: false,
            stopped: false,
        }
    }
}

/// SSE 状态管理器
///
/// 确保 SSE 事件序列符合 Claude API 规范：
/// 1. message_start 只能出现一次
/// 2. content_block 必须先 start 再 delta 再 stop
/// 3. message_delta 只能出现一次，且在所有 content_block_stop 之后
/// 4. message_stop 在最后
#[derive(Debug)]
pub struct SseStateManager {
    /// message_start 是否已发送
    message_started: bool,
    /// message_delta 是否已发送
    message_delta_sent: bool,
    /// 活跃的内容块状态
    active_blocks: HashMap<i32, BlockState>,
    /// 消息是否已结束
    message_ended: bool,
    /// 下一个块索引
    next_block_index: i32,
    /// 当前 stop_reason
    stop_reason: Option<String>,
    /// 是否有工具调用
    has_tool_use: bool,
}

impl Default for SseStateManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SseStateManager {
    pub fn new() -> Self {
        Self {
            message_started: false,
            message_delta_sent: false,
            active_blocks: HashMap::new(),
            message_ended: false,
            next_block_index: 0,
            stop_reason: None,
            has_tool_use: false,
        }
    }

    /// 判断指定块是否处于可接收 delta 的打开状态
    fn is_block_open_of_type(&self, index: i32, expected_type: &str) -> bool {
        self.active_blocks
            .get(&index)
            .is_some_and(|b| b.started && !b.stopped && b.block_type == expected_type)
    }

    /// 获取下一个块索引
    pub fn next_block_index(&mut self) -> i32 {
        let index = self.next_block_index;
        self.next_block_index += 1;
        index
    }

    /// 记录工具调用
    pub fn set_has_tool_use(&mut self, has: bool) {
        self.has_tool_use = has;
    }

    /// 设置 stop_reason
    pub fn set_stop_reason(&mut self, reason: impl Into<String>) {
        self.stop_reason = Some(reason.into());
    }

    /// 检查是否存在非 thinking 类型的内容块（如 text 或 tool_use）
    fn has_non_thinking_blocks(&self) -> bool {
        self.active_blocks
            .values()
            .any(|b| b.block_type != "thinking")
    }

    /// 获取最终的 stop_reason
    pub fn get_stop_reason(&self) -> String {
        if let Some(ref reason) = self.stop_reason {
            reason.clone()
        } else if self.has_tool_use {
            "tool_use".to_string()
        } else {
            "end_turn".to_string()
        }
    }

    /// message_start 是否已发送
    pub fn message_started(&self) -> bool {
        self.message_started
    }

    /// 处理 message_start 事件
    pub fn handle_message_start(&mut self, event: serde_json::Value) -> Option<SseEvent> {
        if self.message_started {
            tracing::debug!("跳过重复的 message_start 事件");
            return None;
        }
        self.message_started = true;
        Some(SseEvent::new("message_start", event))
    }

    /// 处理 content_block_start 事件
    pub fn handle_content_block_start(
        &mut self,
        index: i32,
        block_type: &str,
        data: serde_json::Value,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 如果是 tool_use 块，先关闭之前的文本块
        if block_type == "tool_use" {
            self.has_tool_use = true;
            for (block_index, block) in self.active_blocks.iter_mut() {
                if block.block_type == "text" && block.started && !block.stopped {
                    // 自动发送 content_block_stop 关闭文本块
                    events.push(SseEvent::new(
                        "content_block_stop",
                        json!({
                            "type": "content_block_stop",
                            "index": block_index
                        }),
                    ));
                    block.stopped = true;
                }
            }
        }

        // 检查块是否已存在
        if let Some(block) = self.active_blocks.get_mut(&index) {
            if block.started {
                tracing::debug!("块 {} 已启动，跳过重复的 content_block_start", index);
                return events;
            }
            block.started = true;
        } else {
            let mut block = BlockState::new(block_type);
            block.started = true;
            self.active_blocks.insert(index, block);
        }

        events.push(SseEvent::new("content_block_start", data));
        events
    }

    /// 处理 content_block_delta 事件
    pub fn handle_content_block_delta(
        &mut self,
        index: i32,
        data: serde_json::Value,
    ) -> Option<SseEvent> {
        // 确保块已启动
        if let Some(block) = self.active_blocks.get(&index) {
            if !block.started || block.stopped {
                tracing::warn!(
                    "块 {} 状态异常: started={}, stopped={}",
                    index,
                    block.started,
                    block.stopped
                );
                return None;
            }
        } else {
            // 块不存在，可能需要先创建
            tracing::warn!("收到未知块 {} 的 delta 事件", index);
            return None;
        }

        Some(SseEvent::new("content_block_delta", data))
    }

    /// 注销一个块，使其不再出现在最终的收尾事件中
    ///
    /// 用于缓冲模式下丢弃「参数未写完就被截断」的 tool_use：这些块的
    /// `content_block_start` 从未发给客户端，若仍留在 `active_blocks` 里，
    /// [`Self::generate_final_events`] 会为其补发一个孤立的 `content_block_stop`，
    /// 客户端会因索引不存在而报错。
    pub fn discard_block(&mut self, index: i32) {
        self.active_blocks.remove(&index);
    }

    /// 处理 content_block_stop 事件
    pub fn handle_content_block_stop(&mut self, index: i32) -> Option<SseEvent> {
        if let Some(block) = self.active_blocks.get_mut(&index) {
            if block.stopped {
                tracing::debug!("块 {} 已停止，跳过重复的 content_block_stop", index);
                return None;
            }
            block.stopped = true;
            return Some(SseEvent::new(
                "content_block_stop",
                json!({
                    "type": "content_block_stop",
                    "index": index
                }),
            ));
        }
        None
    }

    /// 生成最终事件序列
    pub fn generate_final_events(
        &mut self,
        input_tokens: i32,
        output_tokens: i32,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 关闭所有未关闭的块
        for (index, block) in self.active_blocks.iter_mut() {
            if block.started && !block.stopped {
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop",
                        "index": index
                    }),
                ));
                block.stopped = true;
            }
        }

        // 发送 message_delta
        if !self.message_delta_sent {
            self.message_delta_sent = true;
            events.push(SseEvent::new(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": self.get_stop_reason(),
                        "stop_sequence": null
                    },
                    "usage": {
                        "input_tokens": input_tokens,
                        "output_tokens": output_tokens
                    }
                }),
            ));
        }

        // 发送 message_stop
        if !self.message_ended {
            self.message_ended = true;
            events.push(SseEvent::new(
                "message_stop",
                json!({ "type": "message_stop" }),
            ));
        }

        events
    }
}

use super::converter::{active_chunk_lines, get_context_window_size};

/// 提取 SSE 事件顶层的 `index` 字段（content_block_* 事件都带）
///
/// 用于缓冲模式下区分「属于 tool_use 块的事件」与「关闭前置 text 块的事件」。
fn event_block_index(e: &SseEvent) -> Option<i32> {
    e.data
        .get("index")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32)
}

/// tool_use 被截断时回灌给模型的纠正文本
///
/// 逐字取自 Kiro IDE 本体 `OutputTruncatedError` 的消息（`extension.js` 中
/// `errors-BBdZwTgx.js` 的 `m17` 类），仅把 `fs_write` / `fs_append` /
/// `str_replace` 替换为 Claude Code 侧实际可用的 `Write` / `Edit`。
/// `{limit}` 由生效的 `chunkedWritePolicy.writeLimitLines` 代入（默认 50，
/// 与官方 `WRITE_LIMIT` 一致）。
pub(crate) fn truncated_tool_correction(chunk_lines: u32) -> String {
    format!(
        "The model output was truncated before this tool call was complete. The tool was NOT \
         executed. When writing files, limit each Write call to {chunk_lines} lines or \
         fewer, then use Edit to add remaining content in chunks of {chunk_lines} lines or \
         fewer. For edits, use multiple Edit calls instead of one large edit."
    )
}

/// 流处理上下文
pub struct StreamContext {
    /// SSE 状态管理器
    pub state_manager: SseStateManager,
    /// 请求的模型名称
    pub model: String,
    /// 消息 ID
    pub message_id: String,
    /// 输入 tokens（估算值）
    pub input_tokens: i32,
    /// 从 contextUsageEvent 计算的实际输入 tokens
    pub context_input_tokens: Option<i32>,
    /// 来自 metadataEvent 的精确 token 用量（优先于上面两个估算值）
    pub token_usage: Option<crate::kiro::model::events::TokenUsage>,
    /// 输出 tokens 累计
    pub output_tokens: i32,
    /// 工具块索引映射 (tool_id -> block_index)
    pub tool_block_indices: HashMap<String, i32>,
    /// 工具名称反向映射（短名称 → 原始名称），用于响应时还原
    pub tool_name_map: HashMap<String, String>,
    /// thinking 是否启用
    pub thinking_enabled: bool,
    /// thinking 内容缓冲区
    pub thinking_buffer: String,
    /// 是否在 thinking 块内
    pub in_thinking_block: bool,
    /// thinking 块是否已提取完成
    pub thinking_extracted: bool,
    /// thinking 块索引
    pub thinking_block_index: Option<i32>,
    /// 文本块索引（thinking 启用时动态分配）
    pub text_block_index: Option<i32>,
    /// 是否需要剥离 thinking 内容开头的换行符
    /// 模型输出 `<thinking>\n` 时，`\n` 可能与标签在同一 chunk 或下一 chunk
    strip_thinking_leading_newline: bool,
    /// 是否延迟发送 message_start 直到收到 contextUsageEvent
    delay_message_start: bool,
    /// 延迟模式下尚未发送的事件（含 message_start）
    pending_events: Vec<SseEvent>,
    /// message_start 是否已释放给客户端
    message_start_released: bool,
    /// 是否已生成初始 SSE 状态
    stream_initialized: bool,
    /// 上游流是否已失败（不再发送正常结束事件）
    pub stream_failed: bool,
    /// 各 tool_use 已累积的输入字节数（tool_id -> 字节数）
    ///
    /// 仅用于 `ContentLengthExceededException` 的诊断日志：上游截断时需要知道
    /// 截断发生在哪个工具、已写出多少字节，以便定位真实长度上限。
    tool_input_bytes: HashMap<String, usize>,
    /// 尚未收到 `stop: true` 的 tool_use 的 SSE 事件（按到达顺序）
    ///
    /// 仅在缓冲模式（`delay_message_start`，即 `/cc` 路径）下使用：tool_use 的
    /// 事件先在此暂存，直到该工具的 `stop: true` 到达才整体放行。若流结束时仍
    /// 有残留，说明模型输出在参数 JSON 写完前被截断，这些事件会被**丢弃**并改为
    /// 发送一段纠正文本（见 [`Self::take_truncated_tool_correction`]）。
    ///
    /// 对齐 Kiro IDE 本体：它用「已开启工具调用 + 参数已开始 + 未收到 stop」
    /// 判定截断，并让工具**完全不执行**，再把纠正指令作为工具结果回灌给模型。
    buffered_tool_events: Vec<(String, SseEvent)>,
    /// 已收到 `stop: true` 的 tool_use id 集合（缓冲模式下用于判定是否完整）
    completed_tool_ids: std::collections::HashSet<String>,
    /// 模型用量是否已上报（防止多个 finalize 出口重复计数）
    usage_reported: bool,
    /// 本次流实际使用的上游凭据 id（-1 = 未知），供监控按凭据统计。
    /// 重连换凭据时更新为最新值，上报时取收尾那次的凭据。
    credential_id: i64,
    /// 本次请求上游计费的 credits 消耗（来自 meteringEvent，累加）。
    credits_used: f64,
}

impl StreamContext {
    /// 创建启用thinking的StreamContext
    pub fn new_with_thinking(
        model: impl Into<String>,
        input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
        delay_message_start: bool,
    ) -> Self {
        Self {
            state_manager: SseStateManager::new(),
            model: model.into(),
            message_id: format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
            input_tokens,
            context_input_tokens: None,
            token_usage: None,
            output_tokens: 0,
            tool_block_indices: HashMap::new(),
            tool_name_map,
            thinking_enabled,
            thinking_buffer: String::new(),
            in_thinking_block: false,
            thinking_extracted: false,
            thinking_block_index: None,
            text_block_index: None,
            strip_thinking_leading_newline: false,
            delay_message_start,
            pending_events: Vec::new(),
            message_start_released: false,
            stream_initialized: false,
            stream_failed: false,
            tool_input_bytes: HashMap::new(),
            buffered_tool_events: Vec::new(),
            completed_tool_ids: std::collections::HashSet::new(),
            usage_reported: false,
            credential_id: -1,
            credits_used: 0.0,
        }
    }

    /// 设置本次流使用的上游凭据 id（供监控按凭据统计）
    pub fn set_credential_id(&mut self, id: u64) {
        self.credential_id = id as i64;
    }

    /// 是否处于「延迟放行 message_start 且尚未放行」的等待状态（仅 `/cc`）
    ///
    /// 供上层在超时兜底时判断是否需要强制放行：只有还在等待时才有意义。
    pub fn awaiting_message_start_release(&self) -> bool {
        self.delay_message_start && !self.message_start_released
    }

    /// 超时兜底：强制放行缓冲中的 `message_start`（仅 `/cc` 等待期有效）
    ///
    /// `/cc` 默认等 `contextUsageEvent` 到达再放行以填入精确 `input_tokens`；
    /// 上游长时间思考时会迟迟不放行，客户端只收到 ping、触发重试倒计时。
    /// 超过配置上限后调用此方法：用当前 `effective_input_tokens()`（此时多为
    /// 本地估算值）放行 `message_start`，让客户端立即进入接收状态。后续
    /// `contextUsageEvent` / `metadataEvent` 仍会在 `message_delta` 与用量上报中
    /// 修正真实 token 数，语义无损。
    ///
    /// 返回需要立即发往客户端的事件（未在等待期则返回空）。
    pub fn force_release_message_start(&mut self) -> Vec<SseEvent> {
        if !self.awaiting_message_start_release() {
            return Vec::new();
        }
        if !self.stream_initialized {
            self.pending_events = self.generate_initial_events();
            self.stream_initialized = true;
        }
        let out = self.release_pending_events();
        self.message_start_released = true;
        out
    }

    /// 客户端是否已收到过正文事件（`message_start` 及其后续）
    ///
    /// 用于判断上游断流后能否透明重放请求：只要客户端还没看到 `message_start`，
    /// 就可以丢弃本次上下文重新来一遍；一旦发出，协议上不允许第二个
    /// `message_start`，也无法撤回已输出的内容，只能走失败收尾。
    ///
    /// 延迟模式（`/cc`）下事件先缓冲，故以「是否已释放」为准；
    /// 直通模式下首个事件即已发出，以「是否已初始化」为准。
    /// ping 保活不算正文事件，不影响重放。
    pub fn client_saw_output(&self) -> bool {
        if self.delay_message_start {
            self.message_start_released
        } else {
            self.stream_initialized
        }
    }

    /// 创建 SSE 流内 error 事件
    pub fn create_error_event(message: &str) -> SseEvent {
        SseEvent::new(
            "error",
            json!({
                "type": "error",
                "error": {
                    "type": "api_error",
                    "message": message
                }
            }),
        )
    }

    fn effective_input_tokens(&self) -> i32 {
        // 优先级：metadataEvent 的精确值 > contextUsageEvent 反推值 > 本地估算
        self.token_usage
            .as_ref()
            .and_then(|u| u.anthropic_input_tokens())
            .or(self.context_input_tokens)
            .unwrap_or(self.input_tokens)
    }

    /// 最终上报的输出 tokens：优先用 metadataEvent 精确值，否则用累计估算
    fn effective_output_tokens(&self) -> i32 {
        self.token_usage
            .as_ref()
            .and_then(|u| u.anthropic_output_tokens())
            .unwrap_or(self.output_tokens)
    }

    /// 上报本次请求的模型累计用量（每请求恰好一次，多个 finalize 出口靠此 guard 去重）
    fn report_usage_once(&mut self) {
        if self.usage_reported {
            return;
        }
        self.usage_reported = true;
        let input = self.effective_input_tokens() as i64;
        let output = self.effective_output_tokens() as i64;
        crate::model_stats::record(&self.model, input, output, self.credits_used);
        // 缓存读写取自 metadataEvent 精确值（无则 0）
        let (cache_read, cache_write) = self
            .token_usage
            .as_ref()
            .map(|u| {
                (
                    u.cache_read_input_tokens.unwrap_or(0).max(0),
                    u.cache_write_input_tokens.unwrap_or(0).max(0),
                )
            })
            .unwrap_or((0, 0));
        crate::stats_db::record(
            &self.model,
            self.credential_id,
            input,
            output,
            cache_read,
            cache_write,
            self.credits_used,
        );
    }

    fn patch_message_start_tokens(events: &mut [SseEvent], input_tokens: i32) {
        for event in events.iter_mut() {
            if event.event == "message_start" {
                if let Some(message) = event.data.get_mut("message") {
                    if let Some(usage) = message.get_mut("usage") {
                        usage["input_tokens"] = json!(input_tokens);
                    }
                }
            }
        }
    }

    fn release_pending_events(&mut self) -> Vec<SseEvent> {
        let input_tokens = self.effective_input_tokens();
        Self::patch_message_start_tokens(&mut self.pending_events, input_tokens);
        std::mem::take(&mut self.pending_events)
    }

    /// 处理 Kiro 事件；延迟模式下在 contextUsageEvent 之前只缓冲，之后实时转发
    pub fn take_events_for_kiro(&mut self, event: &Event) -> Vec<SseEvent> {
        if self.stream_failed {
            return Vec::new();
        }

        if self.delay_message_start && !self.message_start_released {
            if !self.stream_initialized {
                self.pending_events = self.generate_initial_events();
                self.stream_initialized = true;
            }

            if matches!(event, Event::ContextUsage(_)) {
                let events = self.process_kiro_event(event);
                let mut out = self.release_pending_events();
                self.message_start_released = true;
                out.extend(events);
                return out;
            }

            let events = self.process_kiro_event(event);
            if !events.is_empty() {
                let mut out = self.release_pending_events();
                self.message_start_released = true;
                out.extend(events);
                return out;
            }
            return Vec::new();
        }

        let mut out = Vec::new();
        if !self.stream_initialized {
            out.extend(self.generate_initial_events());
            self.stream_initialized = true;
        }
        out.extend(self.process_kiro_event(event));
        out
    }

    /// 流结束时生成剩余事件（含失败路径）
    pub fn finalize_stream(&mut self) -> Vec<SseEvent> {
        if self.stream_failed {
            return self.finalize_stream_on_failure();
        }
        self.report_usage_once();
        let mut out = Vec::new();
        if self.delay_message_start && !self.message_start_released {
            if !self.stream_initialized {
                out.extend(self.generate_initial_events());
                self.stream_initialized = true;
            }
            out.extend(self.release_pending_events());
            self.message_start_released = true;
        }
        out.extend(self.take_truncated_tool_correction());
        out.extend(self.generate_final_events());
        out
    }

    /// 流异常结束时补全 SSE 序列（关闭块 + message_delta + message_stop）
    pub fn finalize_stream_on_failure(&mut self) -> Vec<SseEvent> {
        self.report_usage_once();
        let mut out = Vec::new();

        if self.delay_message_start && !self.message_start_released {
            if !self.stream_initialized {
                out.extend(self.generate_initial_events());
                self.stream_initialized = true;
            }
            out.extend(self.release_pending_events());
            self.message_start_released = true;
        } else if !self.state_manager.message_started() {
            out.extend(self.generate_initial_events());
            self.stream_initialized = true;
        }

        let final_input_tokens = self.effective_input_tokens();
        let final_output_tokens = self.effective_output_tokens().max(1);
        // 本方法用于两种收尾：调用方已发过 error 事件（解码失败等），或紧随其后
        // 补全 SSE 序列。无论哪种，残留的不完整 tool_use 事件都必须丢弃，避免半个
        // 参数 JSON 外泄。
        //
        // 关键：这些 tool_use 块的 content_block_start 只进了 buffered_tool_events、
        // **从未发给客户端**，但块已登记进 active_blocks（started=true）。若只 clear()
        // 缓冲而不注销块，generate_final_events 会为其补发一个孤立的 content_block_stop，
        // 客户端收到「未曾 start 的 index 的 stop」即报 "Content block not found"。
        // 因此必须先 discard_block 再 clear（与 take_truncated_tool_correction 一致）。
        let dropped_ids: Vec<String> = self
            .buffered_tool_events
            .iter()
            .map(|(id, _)| id.clone())
            .collect();
        for id in &dropped_ids {
            if let Some(&idx) = self.tool_block_indices.get(id) {
                self.state_manager.discard_block(idx);
            }
        }
        self.buffered_tool_events.clear();
        out.extend(
            self.state_manager
                .generate_final_events(final_input_tokens, final_output_tokens),
        );
        out
    }

    /// 中途断流（已输出正文、无法透明重放）时的优雅收尾。
    ///
    /// 与 [`Self::finalize_stream_on_failure`] 的区别：**不向客户端发送 SSE `error`
    /// 事件**，而是把已输出的内容以 `stop_reason=max_tokens` 正常封口
    /// （`message_delta` + `message_stop`）。这样 Claude Code 会把已生成的部分当作
    /// 一次有效（被截断）的回答接收，拿去继续补全，而不是判定整轮失败后重试整轮
    /// ——后者既多扣配额又可能形成重试循环。
    ///
    /// 语义选 `max_tokens` 而非 `end_turn`：它明确表示「输出被截断」，客户端据此
    /// 知道内容不完整、需要续写，避免把半截内容误当作模型主动结束的完整回答。
    pub fn finalize_stream_on_interrupt(&mut self) -> Vec<SseEvent> {
        self.state_manager.set_stop_reason("max_tokens");
        self.finalize_stream_on_failure()
    }

    /// 生成 message_start 事件
    pub fn create_message_start_event(&self) -> serde_json::Value {
        json!({
            "type": "message_start",
            "message": {
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": self.model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": self.effective_input_tokens(),
                    "output_tokens": 1
                }
            }
        })
    }

    /// 生成初始事件序列 (message_start + 文本块 start)
    ///
    /// 当 thinking 启用时，不在初始化时创建文本块，而是等到实际收到内容时再创建。
    /// 这样可以确保 thinking 块（索引 0）在文本块（索引 1）之前。
    pub fn generate_initial_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // message_start
        let msg_start = self.create_message_start_event();
        if let Some(event) = self.state_manager.handle_message_start(msg_start) {
            events.push(event);
        }

        // 如果启用了 thinking，不在这里创建文本块
        // thinking 块和文本块会在 process_content_with_thinking 中按正确顺序创建
        if self.thinking_enabled {
            return events;
        }

        // 创建初始文本块（仅在未启用 thinking 时）
        let text_block_index = self.state_manager.next_block_index();
        self.text_block_index = Some(text_block_index);
        let text_block_events = self.state_manager.handle_content_block_start(
            text_block_index,
            "text",
            json!({
                "type": "content_block_start",
                "index": text_block_index,
                "content_block": {
                    "type": "text",
                    "text": ""
                }
            }),
        );
        events.extend(text_block_events);

        events
    }

    /// 处理 Kiro 事件并转换为 Anthropic SSE 事件
    pub fn process_kiro_event(&mut self, event: &Event) -> Vec<SseEvent> {
        match event {
            Event::AssistantResponse(resp) => self.process_assistant_response(&resp.content),
            Event::ToolUse(tool_use) => self.process_tool_use(tool_use),
            Event::ContextUsage(context_usage) => {
                // 从上下文使用百分比计算实际的 input_tokens
                let window_size = get_context_window_size(&self.model);
                let actual_input_tokens =
                    (context_usage.context_usage_percentage * (window_size as f64) / 100.0) as i32;
                self.context_input_tokens = Some(actual_input_tokens);
                // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                if context_usage.context_usage_percentage >= 100.0 {
                    self.state_manager
                        .set_stop_reason("model_context_window_exceeded");
                }
                tracing::debug!(
                    "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                    context_usage.context_usage_percentage,
                    actual_input_tokens
                );
                Vec::new()
            }
            Event::Metadata(metadata) => {
                if let Some(usage) = metadata.token_usage.as_ref().filter(|u| u.has_counts()) {
                    tracing::debug!(
                        uncached_input_tokens = ?usage.uncached_input_tokens,
                        output_tokens = ?usage.output_tokens,
                        total_tokens = ?usage.total_tokens,
                        cache_read_input_tokens = ?usage.cache_read_input_tokens,
                        cache_write_input_tokens = ?usage.cache_write_input_tokens,
                        "收到 metadataEvent，使用上游精确 token 用量"
                    );
                    self.token_usage = Some(usage.clone());
                }
                Vec::new()
            }
            Event::Metering(metering) => {
                // 上游计费事件：累加本次请求消耗的 credits（Kiro 实际计费单位）
                if metering.usage > 0.0 {
                    self.credits_used += metering.usage;
                    tracing::debug!(
                        usage = metering.usage,
                        unit = %metering.unit,
                        "收到 meteringEvent，累加 credits 消耗"
                    );
                }
                Vec::new()
            }
            Event::InvalidState(invalid) => {
                // 上游主动上报会话状态非法：视为流失败，向客户端发 error 事件
                tracing::error!(
                    reason = %invalid.reason,
                    message = %invalid.message,
                    "收到 invalidStateEvent，上游报告会话状态非法"
                );
                self.stream_failed = true;
                vec![Self::create_error_event(&format!(
                    "Upstream reported invalid state: {invalid}"
                ))]
            }
            Event::Error {
                error_code,
                error_message,
            } => {
                self.stream_failed = true;
                tracing::error!("收到错误事件: {} - {}", error_code, error_message);
                vec![Self::create_error_event(&format!(
                    "{error_code}: {error_message}"
                ))]
            }
            Event::Exception {
                exception_type,
                message,
            } => {
                if exception_type == "ContentLengthExceededException" {
                    // 模型输出被截断：tool_use 参数 JSON 不完整，整次调用作废。
                    // 记录截断位置便于对照 Kiro 官方 WRITE_LIMIT（50 行）——官方该值
                    // 仅为提示词文本、不参与程序校验，故这里只观测不强制。
                    let largest = self
                        .tool_input_bytes
                        .iter()
                        .max_by_key(|(_, bytes)| **bytes)
                        .map(|(id, &bytes)| (id.clone(), bytes));
                    tracing::warn!(
                        model = %self.model,
                        output_tokens_est = self.output_tokens,
                        tool_count = self.tool_input_bytes.len(),
                        largest_tool_id = ?largest.as_ref().map(|(id, _)| id),
                        largest_tool_input_bytes = ?largest.as_ref().map(|(_, b)| b),
                        total_tool_input_bytes = self.tool_input_bytes.values().sum::<usize>(),
                        upstream_message = %message,
                        "上游内容长度超限，输出被截断（stop_reason=max_tokens）"
                    );
                    self.state_manager.set_stop_reason("max_tokens");
                    return Vec::new();
                }
                self.stream_failed = true;
                tracing::error!("收到异常事件: {} - {}", exception_type, message);
                vec![Self::create_error_event(&format!(
                    "{exception_type}: {message}"
                ))]
            }
            _ => Vec::new(),
        }
    }

    /// 处理助手响应事件
    fn process_assistant_response(&mut self, content: &str) -> Vec<SseEvent> {
        if content.is_empty() {
            return Vec::new();
        }

        // 估算 tokens
        self.output_tokens += estimate_tokens(content);

        // 如果启用了thinking，需要处理thinking块
        if self.thinking_enabled {
            return self.process_content_with_thinking(content);
        }

        // 非 thinking 模式同样复用统一的 text_delta 发送逻辑，
        // 以便在 tool_use 自动关闭文本块后能够自愈重建新的文本块，避免“吞字”。
        self.create_text_delta_events(content)
    }

    /// 处理包含thinking块的内容
    fn process_content_with_thinking(&mut self, content: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 将内容添加到缓冲区进行处理
        self.thinking_buffer.push_str(content);

        loop {
            if !self.in_thinking_block && !self.thinking_extracted {
                // 查找 <thinking> 开始标签（跳过被反引号包裹的）
                if let Some(start_pos) = find_real_thinking_start_tag(&self.thinking_buffer) {
                    // 发送 <thinking> 之前的内容作为 text_delta
                    // 注意：如果前面只是空白字符（如 adaptive 模式返回的 \n\n），则跳过，
                    // 避免在 thinking 块之前产生无意义的 text 块导致客户端解析失败
                    let before_thinking = self.thinking_buffer[..start_pos].to_string();
                    if !before_thinking.is_empty() && !before_thinking.trim().is_empty() {
                        events.extend(self.create_text_delta_events(&before_thinking));
                    }

                    // 进入 thinking 块
                    self.in_thinking_block = true;
                    self.strip_thinking_leading_newline = true;
                    self.thinking_buffer =
                        self.thinking_buffer[start_pos + "<thinking>".len()..].to_string();

                    // 创建 thinking 块的 content_block_start 事件
                    let thinking_index = self.state_manager.next_block_index();
                    self.thinking_block_index = Some(thinking_index);
                    let start_events = self.state_manager.handle_content_block_start(
                        thinking_index,
                        "thinking",
                        json!({
                            "type": "content_block_start",
                            "index": thinking_index,
                            "content_block": {
                                "type": "thinking",
                                "thinking": ""
                            }
                        }),
                    );
                    events.extend(start_events);
                } else {
                    // 没有找到 <thinking>，检查是否可能是部分标签
                    // 保留可能是部分标签的内容
                    let target_len = self
                        .thinking_buffer
                        .len()
                        .saturating_sub("<thinking>".len());
                    let safe_len = find_char_boundary(&self.thinking_buffer, target_len);
                    if safe_len > 0 {
                        let safe_content = self.thinking_buffer[..safe_len].to_string();
                        // 如果 thinking 尚未提取，且安全内容只是空白字符，
                        // 则不发送为 text_delta，继续保留在缓冲区等待更多内容。
                        // 这避免了 4.6 模型中 <thinking> 标签跨事件分割时，
                        // 前导空白（如 "\n\n"）被错误地创建为 text 块，
                        // 导致 text 块先于 thinking 块出现的问题。
                        if !safe_content.is_empty() && !safe_content.trim().is_empty() {
                            events.extend(self.create_text_delta_events(&safe_content));
                            self.thinking_buffer = self.thinking_buffer[safe_len..].to_string();
                        }
                    }
                    break;
                }
            } else if self.in_thinking_block {
                // 剥离 <thinking> 标签后紧跟的换行符（可能跨 chunk）
                if self.strip_thinking_leading_newline {
                    if self.thinking_buffer.starts_with('\n') {
                        self.thinking_buffer = self.thinking_buffer[1..].to_string();
                        self.strip_thinking_leading_newline = false;
                    } else if !self.thinking_buffer.is_empty() {
                        // buffer 非空但不以 \n 开头，不再需要剥离
                        self.strip_thinking_leading_newline = false;
                    }
                    // buffer 为空时保留标志，等待下一个 chunk
                }

                // 在 thinking 块内，查找 </thinking> 结束标签（跳过被反引号包裹的）
                if let Some(end_pos) = find_real_thinking_end_tag(&self.thinking_buffer) {
                    let thinking_content = self.thinking_buffer[..end_pos].to_string();
                    self.in_thinking_block = false;
                    self.thinking_extracted = true;

                    if let Some(thinking_index) = self.thinking_block_index {
                        events.extend(self.close_thinking_block(thinking_index, &thinking_content));
                    }

                    self.thinking_buffer =
                        self.thinking_buffer[end_pos + "</thinking>\n\n".len()..].to_string();
                } else {
                    // 没有找到结束标签，发送当前缓冲区内容作为 thinking_delta。
                    // 保留末尾可能是部分 `</thinking>\n\n` 的内容：
                    // find_real_thinking_end_tag 要求标签后有 `\n\n` 才返回 Some，
                    // 因此保留区必须覆盖 `</thinking>\n\n` 的完整长度（13 字节），
                    // 否则当 `</thinking>` 已在 buffer 但 `\n\n` 尚未到达时，
                    // 标签的前几个字符会被错误地作为 thinking_delta 发出。
                    let target_len = self
                        .thinking_buffer
                        .len()
                        .saturating_sub("</thinking>\n\n".len());
                    let safe_len = find_char_boundary(&self.thinking_buffer, target_len);
                    if safe_len > 0 {
                        let safe_content = self.thinking_buffer[..safe_len].to_string();
                        if !safe_content.is_empty() {
                            if let Some(thinking_index) = self.thinking_block_index {
                                events.push(
                                    self.create_thinking_delta_event(thinking_index, &safe_content),
                                );
                            }
                        }
                        self.thinking_buffer = self.thinking_buffer[safe_len..].to_string();
                    }
                    break;
                }
            } else {
                // thinking 已提取完成，剩余内容作为 text_delta
                if !self.thinking_buffer.is_empty() {
                    let remaining = self.thinking_buffer.clone();
                    self.thinking_buffer.clear();
                    events.extend(self.create_text_delta_events(&remaining));
                }
                break;
            }
        }

        events
    }

    /// 创建 text_delta 事件
    ///
    /// 如果文本块尚未创建，会先创建文本块。
    /// 当发生 tool_use 时，状态机会自动关闭当前文本块；后续文本会自动创建新的文本块继续输出。
    ///
    /// 返回值包含可能的 content_block_start 事件和 content_block_delta 事件。
    fn create_text_delta_events(&mut self, text: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 如果当前 text_block_index 指向的块已经被关闭（例如 tool_use 开始时自动 stop），
        // 则丢弃该索引并创建新的文本块继续输出，避免 delta 被状态机拒绝导致“吞字”。
        if let Some(idx) = self.text_block_index {
            if !self.state_manager.is_block_open_of_type(idx, "text") {
                self.text_block_index = None;
            }
        }

        // 获取或创建文本块索引
        let text_index = if let Some(idx) = self.text_block_index {
            idx
        } else {
            // 文本块尚未创建，需要先创建
            let idx = self.state_manager.next_block_index();
            self.text_block_index = Some(idx);

            // 发送 content_block_start 事件
            let start_events = self.state_manager.handle_content_block_start(
                idx,
                "text",
                json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": {
                        "type": "text",
                        "text": ""
                    }
                }),
            );
            events.extend(start_events);
            idx
        };

        // 发送 content_block_delta 事件
        if let Some(delta_event) = self.state_manager.handle_content_block_delta(
            text_index,
            json!({
                "type": "content_block_delta",
                "index": text_index,
                "delta": {
                    "type": "text_delta",
                    "text": text
                }
            }),
        ) {
            events.push(delta_event);
        }

        events
    }

    /// 创建 thinking_delta 事件
    fn create_thinking_delta_event(&self, index: i32, thinking: &str) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "thinking_delta",
                    "thinking": thinking
                }
            }),
        )
    }

    /// 创建 signature_delta 事件（extended thinking 兼容）
    fn create_signature_delta_event(&self, index: i32, signature: &str) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "signature_delta",
                    "signature": signature
                }
            }),
        )
    }

    /// 关闭 thinking 块：signature_delta + content_block_stop
    fn close_thinking_block(
        &mut self,
        thinking_index: i32,
        thinking_content: &str,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if !thinking_content.is_empty() {
            events.push(self.create_thinking_delta_event(thinking_index, thinking_content));
        }
        events.push(self.create_thinking_delta_event(thinking_index, ""));
        events.push(self.create_signature_delta_event(
            thinking_index,
            &compute_thinking_signature(thinking_content),
        ));
        if let Some(stop_event) = self.state_manager.handle_content_block_stop(thinking_index) {
            events.push(stop_event);
        }
        events
    }

    /// 处理工具使用事件
    fn process_tool_use(
        &mut self,
        tool_use: &crate::kiro::model::events::ToolUseEvent,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        self.state_manager.set_has_tool_use(true);

        // tool_use 必须发生在 thinking 结束之后。
        // 但当 `</thinking>` 后面没有 `\n\n`（例如紧跟 tool_use 或流结束）时，
        // thinking 结束标签会滞留在 thinking_buffer，导致后续 flush 时把 `</thinking>` 当作内容输出。
        // 这里在开始 tool_use block 前做一次“边界场景”的结束标签识别与过滤。
        if self.thinking_enabled && self.in_thinking_block {
            if let Some(end_pos) = find_real_thinking_end_tag_at_buffer_end(&self.thinking_buffer) {
                let thinking_content = self.thinking_buffer[..end_pos].to_string();
                self.in_thinking_block = false;
                self.thinking_extracted = true;

                if let Some(thinking_index) = self.thinking_block_index {
                    events.extend(self.close_thinking_block(thinking_index, &thinking_content));
                }

                let after_pos = end_pos + "</thinking>".len();
                let remaining = self.thinking_buffer[after_pos..].trim_start().to_string();
                self.thinking_buffer.clear();
                if !remaining.is_empty() {
                    events.extend(self.create_text_delta_events(&remaining));
                }
            }
        }

        // thinking 模式下，process_content_with_thinking 可能会为了探测 `<thinking>` 而暂存一小段尾部文本。
        // 如果此时直接开始 tool_use，状态机会自动关闭 text block，导致这段"待输出文本"看起来被 tool_use 吞掉。
        // 约束：只在尚未进入 thinking block、且 thinking 尚未被提取时，将缓冲区当作普通文本 flush。
        if self.thinking_enabled
            && !self.in_thinking_block
            && !self.thinking_extracted
            && !self.thinking_buffer.is_empty()
        {
            let buffered = std::mem::take(&mut self.thinking_buffer);
            events.extend(self.create_text_delta_events(&buffered));
        }

        // 获取或分配块索引
        let block_index = if let Some(&idx) = self.tool_block_indices.get(&tool_use.tool_use_id) {
            idx
        } else {
            let idx = self.state_manager.next_block_index();
            self.tool_block_indices
                .insert(tool_use.tool_use_id.clone(), idx);
            idx
        };

        // 还原工具名称（如果有映射）
        let original_name = self
            .tool_name_map
            .get(&tool_use.name)
            .cloned()
            .unwrap_or_else(|| tool_use.name.clone());

        // 发送 content_block_start
        let start_events = self.state_manager.handle_content_block_start(
            block_index,
            "tool_use",
            json!({
                "type": "content_block_start",
                "index": block_index,
                "content_block": {
                    "type": "tool_use",
                    "id": tool_use.tool_use_id,
                    "name": original_name,
                    "input": {}
                }
            }),
        );
        events.extend(start_events);

        // 发送参数增量 (ToolUseEvent.input 是 String 类型)
        if !tool_use.input.is_empty() {
            self.output_tokens += (tool_use.input.len() as i32 + 3) / 4; // 估算 token
            *self
                .tool_input_bytes
                .entry(tool_use.tool_use_id.clone())
                .or_insert(0) += tool_use.input.len();

            if let Some(delta_event) = self.state_manager.handle_content_block_delta(
                block_index,
                json!({
                    "type": "content_block_delta",
                    "index": block_index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": tool_use.input
                    }
                }),
            ) {
                events.push(delta_event);
            }
        }

        // 如果是完整的工具调用（stop=true），发送 content_block_stop
        if tool_use.stop {
            if let Some(stop_event) = self.state_manager.handle_content_block_stop(block_index) {
                events.push(stop_event);
            }
            self.completed_tool_ids.insert(tool_use.tool_use_id.clone());
        }

        // 缓冲模式（/cc）：tool_use 事件先暂存，直到 stop 到达才整体放行。
        // 目的是让「参数未写完就被截断」的 tool_use 不外泄给客户端——不完整的
        // 参数 JSON 会让客户端解析失败或执行半个操作。
        if self.buffers_tool_events() {
            let tool_id = tool_use.tool_use_id.clone();
            // events 里可能混有「开始 tool_use 时自动关闭前置 text 块」产生的
            // content_block_stop（其 index != 本 tool_use 块）。这类事件属于**已放行**
            // 的 text 块，必须立即放行——若跟着 tool_use 一起缓冲，截断时会随
            // buffered_tool_events 被整体丢弃，导致 text 块 start 已发、stop 丢失的
            // 孤立块，客户端据此报 "Invalid tool parameters"。
            let mut passthrough = Vec::new();
            for e in events {
                if event_block_index(&e) == Some(block_index) {
                    self.buffered_tool_events.push((tool_id.clone(), e));
                } else {
                    passthrough.push(e);
                }
            }
            if tool_use.stop {
                passthrough.extend(self.release_buffered_tool_events());
            }
            return passthrough;
        }

        events
    }

    /// 是否启用 tool_use 事件缓冲（仅 `/cc` 路径）
    fn buffers_tool_events(&self) -> bool {
        self.delay_message_start
    }

    /// 放行已完整（收到 stop）的 tool_use 事件，**严格按 block index 递增顺序**
    ///
    /// Anthropic 协议要求 content_block 的 index 按递增顺序开始。多个 tool_use 交错
    /// 到达、且完成顺序与 start 顺序不一致时（Claude Code 并行调用多个工具的常见
    /// 场景），若按「谁先收到 stop」放行，会先发高 index 的 `content_block_start`
    /// 再发低 index 的，客户端据此报 "Invalid tool parameters"（首次失败、重试
    /// 恰好顺序一致时又正常，表现为偶发）。
    ///
    /// 因此只放行「连续的、已完成的最小 index 前缀」：最小 index 的块尚未完成时
    /// 停住，不放行任何更高 index 的块——哪怕它已经完成，也要等前面的先放行。
    /// 残留的（因前序未完成而被扣住的）块由流结束时的收尾逻辑处理。
    fn release_buffered_tool_events(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        loop {
            // 找出缓冲中 block index 最小的工具 id
            let next_id = {
                let indices = &self.tool_block_indices;
                self.buffered_tool_events
                    .iter()
                    .filter_map(|(id, _)| indices.get(id).map(|&idx| (idx, id)))
                    .min_by_key(|(idx, _)| *idx)
                    .map(|(_, id)| id.clone())
            };
            let Some(next_id) = next_id else { break };
            // 最小 index 的块未完成 → 不能放行它，更不能越过它放行更高 index 的块
            if !self.completed_tool_ids.contains(&next_id) {
                break;
            }
            // 放行该工具的全部缓冲事件（保持到达顺序：start → delta… → stop）
            let mut i = 0;
            while i < self.buffered_tool_events.len() {
                if self.buffered_tool_events[i].0 == next_id {
                    out.push(self.buffered_tool_events.remove(i).1);
                } else {
                    i += 1;
                }
            }
        }
        out
    }

    /// 流结束时处理残留的不完整 tool_use：丢弃其事件，改发一段纠正文本
    ///
    /// 判定条件与 Kiro IDE 本体一致——已开启 tool_use、参数流已开始、但结束时
    /// 未收到 `stop`，即模型输出被截断、参数 JSON 不完整。
    ///
    /// 官方的做法是让工具**完全不执行**，并把纠正指令作为 `role: "tool"` 的
    /// `toolUseResponse` 回灌进自己的对话历史。作为代理我们不持有历史（历史在
    /// 客户端），所以改为把那段指令作为**助手文本**发出：客户端看到模型「说」了
    /// 这段话而非发起工具调用，模型下一轮即会改用分块写入。这同时避免了把不完整
    /// 的参数 JSON 传给客户端。
    fn take_truncated_tool_correction(&mut self) -> Vec<SseEvent> {
        if self.buffered_tool_events.is_empty() {
            return Vec::new();
        }

        // 收集被丢弃的工具，用于诊断日志与块注销
        let mut dropped_ids: Vec<String> = self
            .buffered_tool_events
            .iter()
            .map(|(id, _)| id.clone())
            .collect();
        dropped_ids.dedup();
        let largest_bytes = dropped_ids
            .iter()
            .filter_map(|id| self.tool_input_bytes.get(id))
            .max()
            .copied();
        tracing::warn!(
            model = %self.model,
            dropped_tool_ids = ?dropped_ids,
            dropped_event_count = self.buffered_tool_events.len(),
            largest_tool_input_bytes = ?largest_bytes,
            chunk_lines = active_chunk_lines(),
            "模型输出在 tool_use 参数写完前被截断，已丢弃不完整的工具调用并回灌分块指令"
        );

        self.buffered_tool_events.clear();

        // 注销这些块，否则 generate_final_events 会为其补发孤立的
        // content_block_stop（它们的 content_block_start 从未发出）。
        for id in &dropped_ids {
            if let Some(idx) = self.tool_block_indices.get(id) {
                self.state_manager.discard_block(*idx);
            }
        }

        let mut out = Vec::new();
        let correction = truncated_tool_correction(active_chunk_lines());
        out.extend(self.create_text_delta_events(&correction));
        self.state_manager.set_stop_reason("max_tokens");
        out
    }

    /// 生成最终事件序列
    pub fn generate_final_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // Flush thinking_buffer 中的剩余内容
        if self.thinking_enabled && !self.thinking_buffer.is_empty() {
            if self.in_thinking_block {
                // 末尾可能残留 `</thinking>`（例如紧跟 tool_use 或流结束），需要在 flush 时过滤掉结束标签。
                if let Some(end_pos) =
                    find_real_thinking_end_tag_at_buffer_end(&self.thinking_buffer)
                {
                    let thinking_content = self.thinking_buffer[..end_pos].to_string();
                    if let Some(thinking_index) = self.thinking_block_index {
                        events.extend(self.close_thinking_block(thinking_index, &thinking_content));
                    }

                    let after_pos = end_pos + "</thinking>".len();
                    let remaining = self.thinking_buffer[after_pos..].trim_start().to_string();
                    self.thinking_buffer.clear();
                    self.in_thinking_block = false;
                    self.thinking_extracted = true;
                    if !remaining.is_empty() {
                        events.extend(self.create_text_delta_events(&remaining));
                    }
                } else {
                    if let Some(thinking_index) = self.thinking_block_index {
                        let buffer = self.thinking_buffer.clone();
                        events.extend(self.close_thinking_block(thinking_index, &buffer));
                    }
                }
            } else {
                // 否则发送剩余内容作为 text_delta
                let buffer_content = self.thinking_buffer.clone();
                events.extend(self.create_text_delta_events(&buffer_content));
            }
            self.thinking_buffer.clear();
        }

        // 如果整个流中只产生了 thinking 块，没有 text 也没有 tool_use，
        // 则设置 stop_reason 为 max_tokens（表示模型耗尽了 token 预算在思考上），
        // 并补发一套完整的 text 事件（内容为一个空格），确保 content 数组中有 text 块
        if self.thinking_enabled
            && self.thinking_block_index.is_some()
            && !self.state_manager.has_non_thinking_blocks()
        {
            self.state_manager.set_stop_reason("max_tokens");
            events.extend(self.create_text_delta_events(" "));
        }

        // token 用量优先级：metadataEvent 精确值 > contextUsageEvent 反推值 > 本地估算
        let final_input_tokens = self.effective_input_tokens();
        let final_output_tokens = self.effective_output_tokens();

        // 生成最终事件
        events.extend(
            self.state_manager
                .generate_final_events(final_input_tokens, final_output_tokens),
        );
        events
    }
}

/// 简单的 token 估算
fn estimate_tokens(text: &str) -> i32 {
    let chars: Vec<char> = text.chars().collect();
    let mut chinese_count = 0;
    let mut other_count = 0;

    for c in &chars {
        if *c >= '\u{4E00}' && *c <= '\u{9FFF}' {
            chinese_count += 1;
        } else {
            other_count += 1;
        }
    }

    // 中文约 1.5 字符/token，英文约 4 字符/token
    let chinese_tokens = (chinese_count * 2 + 2) / 3;
    let other_tokens = (other_count + 3) / 4;

    (chinese_tokens + other_tokens).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sse_event_format() {
        let event = SseEvent::new("message_start", json!({"type": "message_start"}));
        let sse_str = event.to_sse_string();

        assert!(sse_str.starts_with("event: message_start\n"));
        assert!(sse_str.contains("data: "));
        assert!(sse_str.ends_with("\n\n"));
    }

    #[test]
    fn test_sse_state_manager_message_start() {
        let mut manager = SseStateManager::new();

        // 第一次应该成功
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_some());

        // 第二次应该被跳过
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_none());
    }

    #[test]
    fn test_sse_state_manager_block_lifecycle() {
        let mut manager = SseStateManager::new();

        // 创建块
        let events = manager.handle_content_block_start(0, "text", json!({}));
        assert_eq!(events.len(), 1);

        // delta
        let event = manager.handle_content_block_delta(0, json!({}));
        assert!(event.is_some());

        // stop
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_some());

        // 重复 stop 应该被跳过
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_none());
    }

    #[test]
    fn test_tool_name_reverse_mapping_in_stream() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut map = HashMap::new();
        map.insert(
            "short_abc12345".to_string(),
            "mcp__very_long_original_tool_name".to_string(),
        );

        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, map, false);
        let _ = ctx.generate_initial_events();

        // 模拟 Kiro 返回短名称的 tool_use
        let tool_event = Event::ToolUse(ToolUseEvent {
            name: "short_abc12345".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#"{"key":"value"}"#.to_string(),
            stop: true,
        });

        let events = ctx.process_kiro_event(&tool_event);

        // content_block_start 中的 name 应该是原始长名称
        let start_event = events
            .iter()
            .find(|e| e.event == "content_block_start")
            .unwrap();
        assert_eq!(
            start_event.data["content_block"]["name"], "mcp__very_long_original_tool_name",
            "应还原为原始工具名称"
        );
    }

    /// 走真实入口 take_events_for_kiro 的完整 /cc 序列：
    /// text_delta → tool_use（分帧 + stop）→ contextUsage → finalize。
    /// 校验最终 SSE 序列结构合法：message_start 唯一、块 start/stop 配对、
    /// tool_use 的 partial_json 拼起来是合法 JSON。
    #[test]
    fn test_cc_full_sequence_via_real_entry_produces_valid_tool_json() {
        use crate::kiro::model::events::{ContextUsageEvent, ToolUseEvent};

        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), true);

        let mut all: Vec<SseEvent> = Vec::new();

        // 1. 先来一段助手文本
        all.extend(ctx.take_events_for_kiro(&Event::AssistantResponse(
            serde_json::from_str(r#"{"content":"我来写文件"}"#).unwrap(),
        )));

        // 2. tool_use 参数分三帧到达，最后一帧 stop
        all.extend(ctx.take_events_for_kiro(&Event::ToolUse(ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#"{"file_path":"/a.rs","#.to_string(),
            stop: false,
        })));
        all.extend(ctx.take_events_for_kiro(&Event::ToolUse(ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#""content":"fn main"#.to_string(),
            stop: false,
        })));
        all.extend(ctx.take_events_for_kiro(&Event::ToolUse(ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#"() {}"}"#.to_string(),
            stop: true,
        })));

        // 3. contextUsage 到达
        all.extend(
            ctx.take_events_for_kiro(&Event::ContextUsage(ContextUsageEvent {
                context_usage_percentage: 12.5,
            })),
        );

        // 4. 收尾
        all.extend(ctx.finalize_stream());

        // message_start 必须恰好一次
        let starts = all.iter().filter(|e| e.event == "message_start").count();
        assert_eq!(
            starts, 1,
            "message_start 应恰好一次，实际 {starts}: {all:?}"
        );

        // 收集 tool_use 块的 partial_json，拼起来必须是合法 JSON
        let tool_json: String = all
            .iter()
            .filter(|e| e.event == "content_block_delta")
            .filter_map(|e| e.data["delta"]["partial_json"].as_str())
            .collect();
        assert!(
            !tool_json.is_empty(),
            "应有 tool_use 参数增量放行，实际序列: {all:?}"
        );
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&tool_json);
        assert!(
            parsed.is_ok(),
            "tool_use 参数拼接后应是合法 JSON，实际 = {tool_json:?}，序列 = {all:?}"
        );

        // 块 start/stop 必须配对：每个 index 的 start 都要有对应 stop，反之亦然
        let mut opened: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        for e in &all {
            if e.event == "content_block_start" {
                if let Some(idx) = e.data["index"].as_i64() {
                    *opened.entry(idx).or_insert(0) += 1;
                }
            } else if e.event == "content_block_stop" {
                if let Some(idx) = e.data["index"].as_i64() {
                    *opened.entry(idx).or_insert(0) -= 1;
                }
            }
        }
        for (idx, bal) in &opened {
            assert_eq!(
                *bal, 0,
                "块 {idx} 的 start/stop 不配对（差值 {bal}）: {all:?}"
            );
        }
    }

    /// 走真实入口的截断路径：tool_use 缓冲后从未收到 stop，流结束。
    /// 校验发给客户端的纠正文本序列结构合法（有 message_start、text 块配对、
    /// 没有孤立的 tool_use 块或半截 partial_json 外泄）。
    #[test]
    fn test_cc_truncated_via_real_entry_no_orphan_and_no_leak() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), true);

        let mut all: Vec<SseEvent> = Vec::new();

        // tool_use 参数流开始但永远收不到 stop（模型输出被截断）
        all.extend(ctx.take_events_for_kiro(&Event::ToolUse(ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#"{"file_path":"/big.rs","content":"line1"#.to_string(),
            stop: false,
        })));
        // 收尾
        all.extend(ctx.finalize_stream());

        // message_start 必须恰好一次
        let starts = all.iter().filter(|e| e.event == "message_start").count();
        assert_eq!(
            starts, 1,
            "message_start 应恰好一次，实际 {starts}: {all:?}"
        );

        // 不应外泄任何 partial_json（半截参数）
        let leaked: Vec<&str> = all
            .iter()
            .filter(|e| e.event == "content_block_delta")
            .filter_map(|e| e.data["delta"]["partial_json"].as_str())
            .collect();
        assert!(
            leaked.is_empty(),
            "不应外泄 tool_use 半截参数，实际: {leaked:?}"
        );

        // 应发出纠正文本
        let text: String = all
            .iter()
            .filter(|e| e.event == "content_block_delta")
            .filter_map(|e| e.data["delta"]["text"].as_str())
            .collect();
        assert!(
            text.contains("output was truncated"),
            "应有纠正文本: {all:?}"
        );

        // 块 start/stop 必须配对
        let mut opened: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        for e in &all {
            if e.event == "content_block_start" {
                if let Some(idx) = e.data["index"].as_i64() {
                    *opened.entry(idx).or_insert(0) += 1;
                }
            } else if e.event == "content_block_stop" {
                if let Some(idx) = e.data["index"].as_i64() {
                    *opened.entry(idx).or_insert(0) -= 1;
                }
            }
        }
        for (idx, bal) in &opened {
            assert_eq!(
                *bal, 0,
                "块 {idx} 的 start/stop 不配对（差值 {bal}）: {all:?}"
            );
        }

        // 纠正文本用的 text 块必须真的发过 content_block_start
        let text_delta_indices: std::collections::HashSet<i64> = all
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .filter_map(|e| e.data["index"].as_i64())
            .collect();
        let text_start_indices: std::collections::HashSet<i64> = all
            .iter()
            .filter(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "text"
            })
            .filter_map(|e| e.data["index"].as_i64())
            .collect();
        for idx in &text_delta_indices {
            assert!(
                text_start_indices.contains(idx),
                "text_delta 用的块 {idx} 从未发过 content_block_start（孤立 delta）: {all:?}"
            );
        }
    }

    /// 超时兜底：/cc 等待期强制放行 message_start，用估算 input_tokens 填充，
    /// 且不影响后续正文事件的正常转发
    #[test]
    fn test_cc_force_release_message_start_on_timeout() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 42, false, HashMap::new(), true);

        // 尚未收到任何事件：处于等待期
        assert!(ctx.awaiting_message_start_release());

        let released = ctx.force_release_message_start();
        // 应放行 message_start，且 input_tokens 取估算值 42
        let msg_start = released
            .iter()
            .find(|e| e.event == "message_start")
            .expect("应放行 message_start");
        assert_eq!(
            msg_start.data["message"]["usage"]["input_tokens"], 42,
            "强制放行时应使用估算 input_tokens，实际: {msg_start:?}"
        );
        // 放行后不再处于等待期
        assert!(!ctx.awaiting_message_start_release());

        // 放行后正文事件应实时转发（不再缓冲）
        let text = ctx.process_assistant_response("hello");
        assert!(
            text.iter()
                .any(|e| e.event == "content_block_delta" && e.data["delta"]["text"] == "hello"),
            "放行后正文应实时转发，实际: {text:?}"
        );
    }

    /// 强制放行对「非延迟模式」与「已放行」都是幂等 no-op
    #[test]
    fn test_force_release_message_start_is_noop_when_not_waiting() {
        // 非 /cc（delay = false）：从不进入等待期
        let mut v1 =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), false);
        assert!(!v1.awaiting_message_start_release());
        assert!(
            v1.force_release_message_start().is_empty(),
            "非延迟模式强制放行应为空"
        );

        // /cc 已放行后再次调用应为空
        let mut cc = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), true);
        let _ = ctx_release(&mut cc);
        assert!(!cc.awaiting_message_start_release());
        assert!(
            cc.force_release_message_start().is_empty(),
            "已放行后再次强制放行应为空"
        );
    }

    /// 触发一次正常放行（收到 contextUsageEvent）
    fn ctx_release(ctx: &mut StreamContext) -> Vec<SseEvent> {
        ctx.take_events_for_kiro(&Event::ContextUsage(
            crate::kiro::model::events::ContextUsageEvent {
                context_usage_percentage: 10.0,
            },
        ))
    }

    /// 缓冲模式（/cc）下，完整的 tool_use（收到 stop）应正常放行
    #[test]
    fn test_cc_buffered_complete_tool_use_is_released() {
        // delay_message_start = true 即 /cc 路径
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), true);
        let _ = ctx.generate_initial_events();

        // 参数分两次到达，最后一次带 stop
        let partial = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#"{"path":"/a.rs","#.to_string(),
            stop: false,
        });
        // 第一帧会关闭初始 text 块（index 0），该 content_block_stop 属于已放行的
        // text 块，应立即放行；但不应放行任何 tool_use 块（index 1）的事件。
        assert!(
            partial
                .iter()
                .all(|e| e.event == "content_block_stop" && e.data["index"] == 0),
            "stop 之前只应放行关闭前置 text 块的事件，实际: {partial:?}"
        );
        assert!(
            !partial.iter().any(|e| e.event == "content_block_start"),
            "tool_use 块的 start 不应在 stop 前放行，实际: {partial:?}"
        );

        let released = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#""content":"x"}"#.to_string(),
            stop: true,
        });

        // 完整的一组事件应在 stop 时整体放行
        assert!(
            released.iter().any(|e| e.event == "content_block_start"),
            "应放行 content_block_start，实际: {released:?}"
        );
        assert!(
            released.iter().any(|e| e.event == "content_block_stop"),
            "应放行 content_block_stop，实际: {released:?}"
        );
        // 两段参数增量都应保留
        let deltas: Vec<&str> = released
            .iter()
            .filter(|e| e.event == "content_block_delta")
            .filter_map(|e| e.data["delta"]["partial_json"].as_str())
            .collect();
        assert_eq!(deltas.len(), 2, "两段参数增量都应放行，实际: {deltas:?}");

        // 收尾不应再产生纠正文本
        let final_events = ctx.finalize_stream();
        let text: String = final_events
            .iter()
            .filter(|e| e.event == "content_block_delta")
            .filter_map(|e| e.data["delta"]["text"].as_str())
            .collect();
        assert!(
            !text.contains("truncated"),
            "完整调用不应触发纠正文本，实际: {text}"
        );
    }

    /// 复现假设：/cc 缓冲模式下，多个 tool_use 交错到达、且**完成（stop）顺序与
    /// start 顺序相反**时，放行的 content_block_start 的 index 是否保持单调递增。
    ///
    /// Anthropic 协议要求 content_block 的 index 按递增顺序开始。若放行时按“谁先
    /// 收到 stop”排序，会先发 index 2 的 start 再发 index 1 的 start → 乱序 →
    /// Claude Code 报 "Invalid tool parameters"。此测试断言放行顺序按 index 递增。
    #[test]
    fn test_cc_interleaved_tools_release_in_index_order() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), true);
        let _ = ctx.generate_initial_events();

        // 按放行先后顺序记录所有 content_block_start 的 index
        let mut start_order: Vec<i64> = Vec::new();
        let collect = |events: Vec<SseEvent>, out: &mut Vec<i64>| {
            for e in &events {
                if e.event == "content_block_start"
                    && e.data["content_block"]["type"] == "tool_use"
                    && let Some(i) = e.data["index"].as_i64()
                {
                    out.push(i);
                }
            }
        };

        // 工具 A 先开始（占 index 1），参数未写完，暂不 stop
        collect(
            ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
                name: "Read".to_string(),
                tool_use_id: "toolu_A".to_string(),
                input: r#"{"path":"/a"#.to_string(),
                stop: false,
            }),
            &mut start_order,
        );
        // 工具 B 后开始（占 index 2），且先完成（stop）
        collect(
            ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
                name: "Read".to_string(),
                tool_use_id: "toolu_B".to_string(),
                input: r#"{"path":"/b"}"#.to_string(),
                stop: true,
            }),
            &mut start_order,
        );
        // 工具 A 后完成
        collect(
            ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
                name: "Read".to_string(),
                tool_use_id: "toolu_A".to_string(),
                input: r#""}"#.to_string(),
                stop: true,
            }),
            &mut start_order,
        );

        // 断言：放行的 tool_use start 的 index 必须单调递增（先 1 后 2）。
        // 若实现按“谁先 stop”放行，会得到 [2, 1] → 违反协议 → 客户端报错。
        let mut sorted = start_order.clone();
        sorted.sort();
        assert_eq!(
            start_order, sorted,
            "tool_use 的 content_block_start 放行顺序未按 index 递增（交错完成导致乱序）: {start_order:?}"
        );
    }

    /// 缓冲模式下中途断流：未收到 stop 的 tool_use 块只进了缓冲、从未发给客户端，
    /// 优雅收尾时不能为其补发孤立的 content_block_stop（否则客户端报
    /// "Content block not found"）。
    #[test]
    fn test_cc_interrupt_discards_buffered_tool_block() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), true);
        let _ = ctx.generate_initial_events();

        // tool_use 参数流开始但收不到 stop —— 被截断，事件全进缓冲
        let buffered = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#"{"path":"/big.rs","content":"line1"#.to_string(),
            stop: false,
        });
        // tool_use 块的 start 必须没放行（否则不是「未发出」的前提）
        assert!(
            !buffered.iter().any(|e| e.event == "content_block_start"),
            "tool_use start 不应在 stop 前放行，实际: {buffered:?}"
        );
        // 记下 tool_use 块的 index，用于校验收尾不为它补 stop
        let tool_idx = ctx.tool_block_indices["toolu_01"];

        // 中途断流 → 优雅收尾
        let final_events = ctx.finalize_stream_on_interrupt();

        // 不能出现该 tool_use 块 index 的 content_block_stop（它从未 start 过）
        assert!(
            !final_events
                .iter()
                .any(|e| e.event == "content_block_stop" && e.data["index"] == tool_idx),
            "不应为未发出的 tool_use 块补发孤立 content_block_stop，实际: {final_events:?}"
        );
        // 仍应正常封口
        assert_eq!(
            final_events
                .iter()
                .find(|e| e.event == "message_delta")
                .expect("应有 message_delta")
                .data["delta"]["stop_reason"],
            "max_tokens"
        );
        assert!(
            final_events.iter().any(|e| e.event == "message_stop"),
            "应有 message_stop，实际: {final_events:?}"
        );
    }

    /// 缓冲模式下，未收到 stop 的 tool_use 应被丢弃并替换为纠正文本
    #[test]
    fn test_cc_buffered_truncated_tool_use_replaced_with_correction() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), true);
        let _ = ctx.generate_initial_events();

        // 参数流开始但永远收不到 stop —— 即模型输出被截断
        let buffered = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#"{"path":"/big.rs","content":"line1\nline2"#.to_string(),
            stop: false,
        });
        // 第一帧可能放行「关闭前置 text 块」的 content_block_stop（index 0），
        // 但绝不能外泄 tool_use 块的任何事件（start / partial_json）。
        assert!(
            !buffered
                .iter()
                .any(|e| e.event == "content_block_start"
                    || e.data["delta"]["partial_json"].is_string()),
            "不完整的 tool_use 不应外泄 start 或参数增量，实际: {buffered:?}"
        );

        let final_events = ctx.finalize_stream();

        // 不应出现任何 tool_use 块的事件
        assert!(
            !final_events.iter().any(|e| e.event == "content_block_start"
                && e.data["content_block"]["type"] == "tool_use"),
            "被截断的 tool_use 不应发给客户端，实际: {final_events:?}"
        );
        // 不应出现孤立的 content_block_stop（start 从未发出）
        let stops = final_events
            .iter()
            .filter(|e| e.event == "content_block_stop")
            .count();
        let starts = final_events
            .iter()
            .filter(|e| e.event == "content_block_start")
            .count();
        assert!(
            stops <= starts,
            "content_block_stop 不应多于 start，start={starts} stop={stops}"
        );

        // 应改为发出纠正文本，且带上生效的行数
        let text: String = final_events
            .iter()
            .filter(|e| e.event == "content_block_delta")
            .filter_map(|e| e.data["delta"]["text"].as_str())
            .collect();
        assert!(
            text.contains("output was truncated"),
            "应发出截断纠正文本，实际: {text}"
        );
        assert!(
            text.contains("The tool was NOT executed"),
            "应说明工具未执行，实际: {text}"
        );
        assert!(text.contains("50 lines"), "应带上默认行数，实际: {text}");

        // stop_reason 应为 max_tokens
        let delta = final_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("应有 message_delta");
        assert_eq!(delta.data["delta"]["stop_reason"], "max_tokens");
    }

    /// 非缓冲模式（/v1）不应缓冲 tool_use —— 保持实时转发
    #[test]
    fn test_v1_does_not_buffer_tool_use() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), false);
        let _ = ctx.generate_initial_events();

        let events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#"{"path":"/a.rs""#.to_string(),
            stop: false,
        });
        assert!(
            events.iter().any(|e| e.event == "content_block_start"),
            "/v1 应实时转发 tool_use，实际: {events:?}"
        );
    }

    #[test]
    fn test_text_delta_after_tool_use_restarts_text_block() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), false);

        let initial_events = ctx.generate_initial_events();
        assert!(
            initial_events
                .iter()
                .any(|e| e.event == "content_block_start"
                    && e.data["content_block"]["type"] == "text")
        );

        let initial_text_index = ctx
            .text_block_index
            .expect("initial text block index should exist");

        // tool_use 开始会自动关闭现有 text block
        let tool_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "test_tool".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });
        assert!(
            tool_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(initial_text_index as i64)
            }),
            "tool_use should stop the previous text block"
        );

        // 之后再来文本增量，应自动创建新的 text block 而不是往已 stop 的块里写 delta
        let text_events = ctx.process_assistant_response("hello");
        let new_text_start_index = text_events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        assert!(
            new_text_start_index.is_some(),
            "should start a new text block"
        );
        assert_ne!(
            new_text_start_index.unwrap(),
            initial_text_index as i64,
            "new text block index should differ from the stopped one"
        );
        assert!(
            text_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "hello"
            }),
            "should emit text_delta after restarting text block"
        );
    }

    #[test]
    fn test_tool_use_flushes_pending_thinking_buffer_text_before_tool_block() {
        // thinking 模式下，短文本可能被暂存在 thinking_buffer 以等待 `<thinking>` 的跨 chunk 匹配。
        // 当紧接着出现 tool_use 时，应先 flush 这段文本，再开始 tool_use block。
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), false);
        let _initial_events = ctx.generate_initial_events();

        // 两段短文本（各 2 个中文字符），总长度仍可能不足以满足 safe_len>0 的输出条件，
        // 因而会留在 thinking_buffer 中等待后续 chunk。
        let ev1 = ctx.process_assistant_response("有修");
        assert!(
            ev1.iter().all(|e| e.event != "content_block_delta"),
            "short prefix should be buffered under thinking mode"
        );
        let ev2 = ctx.process_assistant_response("改：");
        assert!(
            ev2.iter().all(|e| e.event != "content_block_delta"),
            "short prefix should still be buffered under thinking mode"
        );

        let events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });

        let text_start_index = events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        let pos_text_delta = events.iter().position(|e| {
            e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta"
        });
        let pos_text_stop = text_start_index.and_then(|idx| {
            events.iter().position(|e| {
                e.event == "content_block_stop" && e.data["index"].as_i64() == Some(idx)
            })
        });
        let pos_tool_start = events.iter().position(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });

        assert!(
            text_start_index.is_some(),
            "should start a text block to flush buffered text"
        );
        assert!(
            pos_text_delta.is_some(),
            "should flush buffered text as text_delta"
        );
        assert!(
            pos_text_stop.is_some(),
            "should stop text block before tool_use block starts"
        );
        assert!(pos_tool_start.is_some(), "should start tool_use block");

        let pos_text_delta = pos_text_delta.unwrap();
        let pos_text_stop = pos_text_stop.unwrap();
        let pos_tool_start = pos_tool_start.unwrap();

        assert!(
            pos_text_delta < pos_text_stop && pos_text_stop < pos_tool_start,
            "ordering should be: text_delta -> text_stop -> tool_use_start"
        );

        assert!(
            events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "有修改："
            }),
            "flushed text should equal the buffered prefix"
        );
    }

    #[test]
    fn test_estimate_tokens() {
        assert!(estimate_tokens("Hello") > 0);
        assert!(estimate_tokens("你好") > 0);
        assert!(estimate_tokens("Hello 你好") > 0);
    }

    #[test]
    fn test_find_real_thinking_start_tag_basic() {
        // 基本情况：正常的开始标签
        assert_eq!(find_real_thinking_start_tag("<thinking>"), Some(0));
        assert_eq!(find_real_thinking_start_tag("prefix<thinking>"), Some(6));
    }

    #[test]
    fn test_find_real_thinking_start_tag_with_backticks() {
        // 被反引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("`<thinking>`"), None);
        assert_eq!(find_real_thinking_start_tag("use `<thinking>` tag"), None);

        // 先有被包裹的，后有真正的开始标签
        assert_eq!(
            find_real_thinking_start_tag("about `<thinking>` tag<thinking>content"),
            Some(22)
        );
    }

    #[test]
    fn test_find_real_thinking_start_tag_with_quotes() {
        // 被双引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("\"<thinking>\""), None);
        assert_eq!(find_real_thinking_start_tag("the \"<thinking>\" tag"), None);

        // 被单引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("'<thinking>'"), None);

        // 混合情况
        assert_eq!(
            find_real_thinking_start_tag("about \"<thinking>\" and '<thinking>' then<thinking>"),
            Some(40)
        );
    }

    #[test]
    fn test_find_real_thinking_end_tag_basic() {
        // 基本情况：正常的结束标签后面有双换行符
        assert_eq!(find_real_thinking_end_tag("</thinking>\n\n"), Some(0));
        assert_eq!(
            find_real_thinking_end_tag("content</thinking>\n\n"),
            Some(7)
        );
        assert_eq!(
            find_real_thinking_end_tag("some text</thinking>\n\nmore text"),
            Some(9)
        );

        // 没有双换行符的情况
        assert_eq!(find_real_thinking_end_tag("</thinking>"), None);
        assert_eq!(find_real_thinking_end_tag("</thinking>\n"), None);
        assert_eq!(find_real_thinking_end_tag("</thinking> more"), None);
    }

    #[test]
    fn test_find_real_thinking_end_tag_with_backticks() {
        // 被反引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("`</thinking>`\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("mention `</thinking>` in code\n\n"),
            None
        );

        // 只有前面有反引号
        assert_eq!(find_real_thinking_end_tag("`</thinking>\n\n"), None);

        // 只有后面有反引号
        assert_eq!(find_real_thinking_end_tag("</thinking>`\n\n"), None);
    }

    #[test]
    fn test_find_real_thinking_end_tag_with_quotes() {
        // 被双引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("\"</thinking>\"\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("the string \"</thinking>\" is a tag\n\n"),
            None
        );

        // 被单引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("'</thinking>'\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("use '</thinking>' as marker\n\n"),
            None
        );

        // 混合情况：双引号包裹后有真正的标签
        assert_eq!(
            find_real_thinking_end_tag("about \"</thinking>\" tag</thinking>\n\n"),
            Some(23)
        );

        // 混合情况：单引号包裹后有真正的标签
        assert_eq!(
            find_real_thinking_end_tag("about '</thinking>' tag</thinking>\n\n"),
            Some(23)
        );
    }

    #[test]
    fn test_find_real_thinking_end_tag_mixed() {
        // 先有被包裹的，后有真正的结束标签
        assert_eq!(
            find_real_thinking_end_tag("discussing `</thinking>` tag</thinking>\n\n"),
            Some(28)
        );

        // 多个被包裹的，最后一个是真正的
        assert_eq!(
            find_real_thinking_end_tag("`</thinking>` and `</thinking>` done</thinking>\n\n"),
            Some(36)
        );

        // 多种引用字符混合
        assert_eq!(
            find_real_thinking_end_tag(
                "`</thinking>` and \"</thinking>\" and '</thinking>' done</thinking>\n\n"
            ),
            Some(54)
        );
    }

    #[test]
    fn test_tool_use_immediately_after_thinking_filters_end_tag_and_closes_thinking_block() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), false);
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();

        // thinking 内容以 `</thinking>` 结尾，但后面没有 `\n\n`（模拟紧跟 tool_use 的场景）
        all_events.extend(ctx.process_assistant_response("<thinking>abc</thinking>"));

        let tool_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });
        all_events.extend(tool_events);

        all_events.extend(ctx.generate_final_events());

        // 不应把 `</thinking>` 当作 thinking 内容输出
        assert!(
            all_events.iter().all(|e| {
                !(e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "thinking_delta"
                    && e.data["delta"]["thinking"] == "</thinking>")
            }),
            "`</thinking>` should be filtered from output"
        );

        // thinking block 必须在 tool_use block 之前关闭
        let thinking_index = ctx
            .thinking_block_index
            .expect("thinking block index should exist");
        let pos_thinking_stop = all_events.iter().position(|e| {
            e.event == "content_block_stop"
                && e.data["index"].as_i64() == Some(thinking_index as i64)
        });
        let pos_tool_start = all_events.iter().position(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });
        assert!(
            pos_thinking_stop.is_some(),
            "thinking block should be stopped"
        );
        assert!(pos_tool_start.is_some(), "tool_use block should be started");
        assert!(
            pos_thinking_stop.unwrap() < pos_tool_start.unwrap(),
            "thinking block should stop before tool_use block starts"
        );
    }

    #[test]
    fn test_final_flush_filters_standalone_thinking_end_tag() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), false);
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>abc</thinking>"));
        all_events.extend(ctx.generate_final_events());

        assert!(
            all_events.iter().all(|e| {
                !(e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "thinking_delta"
                    && e.data["delta"]["thinking"] == "</thinking>")
            }),
            "`</thinking>` should be filtered during final flush"
        );
    }

    #[test]
    fn test_thinking_strips_leading_newline_same_chunk() {
        // <thinking>\n 在同一个 chunk 中，\n 应被剥离
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), false);
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>\nHello world");

        // 找到所有 thinking_delta 事件
        let thinking_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        // 拼接所有 thinking 内容
        let full_thinking: String = thinking_deltas
            .iter()
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_thinking.starts_with('\n'),
            "thinking content should not start with \\n, got: {:?}",
            full_thinking
        );
    }

    #[test]
    fn test_thinking_strips_leading_newline_cross_chunk() {
        // <thinking> 在第一个 chunk 末尾，\n 在第二个 chunk 开头
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), false);
        let _initial_events = ctx.generate_initial_events();

        let events1 = ctx.process_assistant_response("<thinking>");
        let events2 = ctx.process_assistant_response("\nHello world");

        let mut all_events = Vec::new();
        all_events.extend(events1);
        all_events.extend(events2);

        let thinking_deltas: Vec<_> = all_events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        let full_thinking: String = thinking_deltas
            .iter()
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_thinking.starts_with('\n'),
            "thinking content should not start with \\n across chunks, got: {:?}",
            full_thinking
        );
    }

    #[test]
    fn test_thinking_no_strip_when_no_leading_newline() {
        // <thinking> 后直接跟内容（无 \n），内容应完整保留
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), false);
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>abc</thinking>\n\ntext");

        let thinking_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        let full_thinking: String = thinking_deltas
            .iter()
            .filter(|e| {
                !e.data["delta"]["thinking"]
                    .as_str()
                    .unwrap_or("")
                    .is_empty()
            })
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert_eq!(full_thinking, "abc", "thinking content should be 'abc'");
    }

    #[test]
    fn test_text_after_thinking_strips_leading_newlines() {
        // `</thinking>\n\n` 后的文本不应以 \n\n 开头
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), false);
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>\nabc</thinking>\n\n你好");

        let text_deltas: Vec<_> = events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .collect();

        let full_text: String = text_deltas
            .iter()
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_text.starts_with('\n'),
            "text after thinking should not start with \\n, got: {:?}",
            full_text
        );
        assert_eq!(full_text, "你好");
    }

    /// 辅助函数：从事件列表中提取所有 thinking_delta 的拼接内容
    fn collect_thinking_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// 辅助函数：从事件列表中提取所有 text_delta 的拼接内容
    fn collect_text_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect()
    }

    #[test]
    fn test_end_tag_newlines_split_across_events() {
        // `</thinking>\n` 在 chunk 1，`\n` 在 chunk 2，`text` 在 chunk 3
        // 确保 `</thinking>` 不会被部分当作 thinking 内容发出
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), false);
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("你好"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "你好", "text should be '你好', got: {:?}", text);
    }

    #[test]
    fn test_end_tag_alone_in_chunk_then_newlines_in_next() {
        // `</thinking>` 单独在一个 chunk，`\n\ntext` 在下一个 chunk
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), false);
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all.extend(ctx.process_assistant_response("\n\n你好"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "你好", "text should be '你好', got: {:?}", text);
    }

    #[test]
    fn test_start_tag_newline_split_across_events() {
        // `\n\n` 在 chunk 1，`<thinking>` 在 chunk 2，`\n` 在 chunk 3
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), false);
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("\n\n"));
        all.extend(ctx.process_assistant_response("<thinking>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("abc</thinking>\n\ntext"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "text", "text should be 'text', got: {:?}", text);
    }

    #[test]
    fn test_full_flow_maximally_split() {
        // 极端拆分：每个关键边界都在不同 chunk
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), false);
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        // \n\n<thinking>\n 拆成多段
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("<thin"));
        all.extend(ctx.process_assistant_response("king>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("hello"));
        // </thinking>\n\n 拆成多段
        all.extend(ctx.process_assistant_response("</thi"));
        all.extend(ctx.process_assistant_response("nking>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("world"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "hello",
            "thinking should be 'hello', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "world", "text should be 'world', got: {:?}", text);
    }

    #[test]
    fn test_thinking_only_sets_max_tokens_stop_reason() {
        // 整个流只有 thinking 块，没有 text 也没有 tool_use，stop_reason 应为 max_tokens
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), false);
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "max_tokens",
            "stop_reason should be max_tokens when only thinking is produced"
        );

        // 应补发一套完整的 text 事件（content_block_start + delta 空格 + content_block_stop）
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "text"
            }),
            "should emit text content_block_start"
        );
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == " "
            }),
            "should emit text_delta with a single space"
        );
        // text block 应被 generate_final_events 自动关闭
        let text_block_index = all_events
            .iter()
            .find_map(|e| {
                if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                    e.data["index"].as_i64()
                } else {
                    None
                }
            })
            .expect("text block should exist");
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(text_block_index)
            }),
            "text block should be stopped"
        );
    }

    #[test]
    fn test_thinking_with_text_keeps_end_turn_stop_reason() {
        // thinking + text 的情况，stop_reason 应为 end_turn
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), false);
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n\nHello"));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "end_turn",
            "stop_reason should be end_turn when text is also produced"
        );
    }

    #[test]
    fn test_thinking_with_tool_use_keeps_tool_use_stop_reason() {
        // thinking + tool_use 的情况，stop_reason 应为 tool_use
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), false);
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(
            ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
                name: "test_tool".to_string(),
                tool_use_id: "tool_1".to_string(),
                input: "{}".to_string(),
                stop: true,
            }),
        );
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "tool_use",
            "stop_reason should be tool_use when tool_use is present"
        );
    }

    #[test]
    fn test_finalize_stream_on_failure_emits_message_stop() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-sonnet-4-6",
            100,
            false,
            HashMap::new(),
            false,
        );
        ctx.stream_failed = true;
        let events = ctx.finalize_stream_on_failure();
        assert!(
            events.iter().any(|e| e.event == "message_start"),
            "failure finalize should emit message_start when stream never started"
        );
        assert!(
            events.iter().any(|e| e.event == "message_stop"),
            "failure finalize should emit message_stop"
        );
        assert!(
            events.iter().any(|e| e.event == "message_delta"),
            "failure finalize should emit message_delta"
        );
    }

    /// 直通模式（/v1）：首个事件一到就已发给客户端，此后不可重放
    #[test]
    fn test_client_saw_output_passthrough() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), false);
        assert!(!ctx.client_saw_output(), "尚未处理任何事件时应可重放");

        let out = ctx.take_events_for_kiro(&Event::AssistantResponse(
            serde_json::from_str(r#"{"content":"hi"}"#).unwrap(),
        ));
        assert!(
            out.iter().any(|e| e.event == "message_start"),
            "直通模式首个事件应带 message_start，实际: {out:?}"
        );
        assert!(ctx.client_saw_output(), "message_start 已发出后不可重放");
    }

    /// 缓冲模式（/cc）：message_start 释放前仍可重放
    #[test]
    fn test_client_saw_output_delayed() {
        use crate::kiro::model::events::ContextUsageEvent;

        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), true);
        assert!(!ctx.client_saw_output());

        // 不产出任何 SSE 的事件（metering 等）只会触发内部初始化，
        // 客户端什么也没收到，仍应允许重放
        let out = ctx.take_events_for_kiro(&Event::Unknown {});
        assert!(out.is_empty(), "该事件不应产出 SSE，实际: {out:?}");
        assert!(
            !ctx.client_saw_output(),
            "缓冲模式下未释放 message_start 前应仍可重放"
        );

        // contextUsage 到达 → 释放 message_start
        let out = ctx.take_events_for_kiro(&Event::ContextUsage(ContextUsageEvent {
            context_usage_percentage: 10.0,
        }));
        assert!(
            out.iter().any(|e| e.event == "message_start"),
            "contextUsage 应释放 message_start，实际: {out:?}"
        );
        assert!(ctx.client_saw_output(), "message_start 已释放后不可重放");
    }

    /// 中途断流的优雅收尾：不发 error，以 stop_reason=max_tokens 正常封口
    #[test]
    fn test_finalize_on_interrupt_no_error_max_tokens() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), false);

        // 先输出一段正文，模拟「已向客户端发过内容后中途断流」
        let _ = ctx.take_events_for_kiro(&Event::AssistantResponse(
            serde_json::from_str(r#"{"content":"hello"}"#).unwrap(),
        ));
        assert!(ctx.client_saw_output(), "输出正文后应视为已输出");

        let events = ctx.finalize_stream_on_interrupt();

        assert!(
            !events.iter().any(|e| e.event == "error"),
            "优雅收尾不应发送 error 事件，实际: {events:?}"
        );
        let delta = events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("应有 message_delta");
        assert_eq!(
            delta.data["delta"]["stop_reason"], "max_tokens",
            "中途断流收尾 stop_reason 应为 max_tokens"
        );
        assert!(
            events.iter().any(|e| e.event == "message_stop"),
            "优雅收尾应有 message_stop，实际: {events:?}"
        );
    }

    /// 从 message_delta 里取出 usage
    fn usage_from_delta(events: &[SseEvent]) -> serde_json::Value {
        events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("应有 message_delta")
            .data["usage"]
            .clone()
    }

    #[test]
    fn test_metadata_event_overrides_estimated_tokens() {
        use crate::kiro::model::events::MetadataEvent;

        // 传入一个明显错误的估算值，验证被 metadataEvent 覆盖
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 99999, false, HashMap::new(), false);
        let _ = ctx.generate_initial_events();
        let _ = ctx.process_kiro_event(&Event::AssistantResponse(
            serde_json::from_str(r#"{"content":"hi"}"#).unwrap(),
        ));

        let metadata: MetadataEvent = serde_json::from_str(
            r#"{"tokenUsage":{"uncachedInputTokens":123,"outputTokens":45,"totalTokens":168}}"#,
        )
        .unwrap();
        let events = ctx.process_kiro_event(&Event::Metadata(metadata));
        assert!(events.is_empty(), "metadataEvent 本身不产生 SSE 事件");

        let usage = usage_from_delta(&ctx.finalize_stream());
        assert_eq!(usage["input_tokens"], 123);
        assert_eq!(usage["output_tokens"], 45);
    }

    #[test]
    fn test_metadata_takes_priority_over_context_usage() {
        use crate::kiro::model::events::{ContextUsageEvent, MetadataEvent};

        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), false);
        let _ = ctx.generate_initial_events();

        // contextUsageEvent 先到（反推值），metadataEvent 后到（精确值）
        let _ = ctx.process_kiro_event(&Event::ContextUsage(ContextUsageEvent {
            context_usage_percentage: 50.0,
        }));
        let reverse_derived = ctx.context_input_tokens.expect("应有反推值");

        let metadata: MetadataEvent =
            serde_json::from_str(r#"{"tokenUsage":{"uncachedInputTokens":777}}"#).unwrap();
        let _ = ctx.process_kiro_event(&Event::Metadata(metadata));

        let usage = usage_from_delta(&ctx.finalize_stream());
        assert_eq!(usage["input_tokens"], 777, "精确值应优先于反推值");
        assert_ne!(reverse_derived, 777, "测试前提：两者本应不同");
    }

    #[test]
    fn test_context_usage_still_used_without_metadata() {
        use crate::kiro::model::events::ContextUsageEvent;

        // 没有 metadataEvent 时保持原有行为（回退到反推值）
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), false);
        let _ = ctx.generate_initial_events();
        let _ = ctx.process_kiro_event(&Event::ContextUsage(ContextUsageEvent {
            context_usage_percentage: 50.0,
        }));
        let expected = ctx.context_input_tokens.expect("应有反推值");

        let usage = usage_from_delta(&ctx.finalize_stream());
        assert_eq!(usage["input_tokens"], expected);
    }

    #[test]
    fn test_invalid_state_event_emits_error_and_fails_stream() {
        use crate::kiro::model::events::InvalidStateEvent;

        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), false);
        let _ = ctx.generate_initial_events();

        let events = ctx.process_kiro_event(&Event::InvalidState(InvalidStateEvent {
            reason: "INVALID_TOOL_RESULT".to_string(),
            message: "missing id".to_string(),
        }));

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "error");
        let msg = events[0].data["error"]["message"]
            .as_str()
            .expect("error.message 应为字符串");
        assert!(msg.contains("INVALID_TOOL_RESULT"), "实际: {msg}");
        assert!(msg.contains("missing id"), "实际: {msg}");
        assert!(ctx.stream_failed, "invalidStateEvent 应标记流失败");

        // 标记失败后不再处理后续事件
        assert!(
            ctx.take_events_for_kiro(&Event::AssistantResponse(
                serde_json::from_str(r#"{"content":"ignored"}"#).unwrap(),
            ))
            .is_empty(),
            "流失败后应忽略后续事件"
        );
    }
}
