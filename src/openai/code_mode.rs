//! Codex Code Mode 桥接
//!
//! codex `gpt-5.6-*`（`tool_mode = "code_mode_only"` + `use_responses_lite = true`）不把工具放在顶层
//! `tools` 字段，而是塞进请求 `input` 里一条 `type:"additional_tools"` 的 item。其中核心工具是
//! `exec`（`type:"custom"` freeform 工具）：模型输出一段 JS（如 `await tools.apply_patch(...)`），
//! 由 codex 本地的 `code-mode-host`（V8）执行，JS 里对 `tools.xxx` 的调用被拦截成真正的工具执行。
//!
//! Kiro 上游只接受 JSON-schema 工具，不认 freeform。本模块采用「拆解为独立工具」策略：
//!
//! - **输入侧**：把 `additional_tools` 里 `exec` 声明的子工具（apply_patch/exec_command/...）拆成独立的
//!   JSON 工具下发给上游 Claude；其余 namespace 里的普通 function 工具（wait/collaboration 等）直接转换。
//! - **输出侧**：当上游 Claude 调用某个被拆出来的子工具时，把它「反向合成」成一个 `exec` 的
//!   `custom_tool_call`，其 `input` 是 `await tools.<name>(<args>);` 形式的 JS —— codex 本地的
//!   code-mode-host 会执行它，从而完成真正的工具操作。
//! - **历史往返**：codex 下一轮会把我们合成的 `exec` custom_tool_call 原样回传。由于 JS 格式由本模块
//!   生成、可机械解析，`parse_exec_js` 能把它还原成结构化的 `(name, input)`，让 Claude 看到连贯对话。
//!
//! 该桥接经 spike 验证：合成的 `exec` JS 能被 code-mode-host 执行并真正写文件。

use serde_json::{Value, json};

/// exec 声明中约定的 freeform 子工具（其 JS 参数为单个字符串，而非对象）。
/// apply_patch 是 FREEFORM：`tools.apply_patch(input: string)`。
const FREEFORM_SUBTOOLS: &[&str] = &["apply_patch"];

/// 返回图片的子工具：返回值形如 `{image_url, detail}`，需用 exec 的 `image()` 帮助函数
/// 追加为图像项（用 `text()` 会把 base64 当纯文本，模型看不到图且浪费 token）。
const IMAGE_SUBTOOLS: &[&str] = &["view_image"];

/// 合成的 exec 工具名（codex code-mode-host 识别的公共入口）。
pub const EXEC_TOOL_NAME: &str = "exec";

/// 判断一条 Responses input item 是否是 code mode 的 `additional_tools`。
pub fn is_additional_tools_item(item: &Value) -> bool {
    item.get("type").and_then(|v| v.as_str()) == Some("additional_tools")
}

/// 判断整个请求是否走 code mode（input 里含 additional_tools item）。
pub fn request_uses_code_mode(input: &[Value]) -> bool {
    input.iter().any(is_additional_tools_item)
}

/// 从 `additional_tools` item 中提取子工具，拆成独立的 JSON 工具描述 `(name, description, params_schema)`。
///
/// 结构：`additional_tools.tools` → 每项 `type:"namespace"`（如 functions/collaboration）→ 内含 `tools`。
/// - `exec`（type:custom）：从其 description 里解析出各子工具（apply_patch/exec_command/...），
///   每个子工具用其 TS 契约段落作为描述，schema 用宽松的 object（freeform 子工具用 `{input:string}`）。
/// - 其余普通 function 工具：直接透传 name/description/parameters。
pub fn extract_code_mode_tools(item: &Value) -> Vec<CodeModeTool> {
    let mut out = Vec::new();
    let Some(namespaces) = item.get("tools").and_then(|v| v.as_array()) else {
        return out;
    };

    for ns in namespaces {
        let inner = match ns.get("type").and_then(|v| v.as_str()) {
            Some("namespace") => ns.get("tools").and_then(|v| v.as_array()),
            // 少数情况下 tool 可能直接平铺（无 namespace 包装）
            _ => None,
        };
        let tools_arr = inner
            .map(|a| a.as_slice())
            .unwrap_or(std::slice::from_ref(ns));

        for tool in tools_arr {
            let typ = tool.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() {
                continue;
            }

            if typ == "custom" && name == EXEC_TOOL_NAME {
                // exec：拆解其 description 里声明的子工具
                let desc = tool
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                out.extend(parse_exec_subtools(desc));
            } else {
                // 普通 function 工具：直接转换
                let description = tool
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let params = tool
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                out.push(CodeModeTool {
                    name: name.to_string(),
                    description,
                    parameters: params,
                    freeform: false,
                });
            }
        }
    }

    out
}

/// 一个被拆解出来、准备下发给上游的 code mode 子工具。
#[derive(Debug, Clone)]
pub struct CodeModeTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    /// freeform 子工具（如 apply_patch）：其 JS 参数是单个字符串。
    /// 我们给上游的 schema 用 `{input: string}`，输出侧生成 JS 时取 input 字段作为字符串实参。
    pub freeform: bool,
}

/// 从 exec 的超长 description 中解析出各子工具段落（以 `### \`name\`` 分隔），
/// 用整段文本作为该子工具的描述，schema 用宽松 object。
fn parse_exec_subtools(desc: &str) -> Vec<CodeModeTool> {
    let mut out = Vec::new();
    // 段落标题形如：### `apply_patch`
    let marker = "### `";
    let mut idx = 0;
    let bytes = desc.as_bytes();
    let mut heads: Vec<(String, usize)> = Vec::new();
    while let Some(pos) = desc[idx..].find(marker) {
        let start = idx + pos;
        let after = start + marker.len();
        if let Some(end_tick) = desc[after..].find('`') {
            let name = desc[after..after + end_tick].to_string();
            heads.push((name, start));
            idx = after + end_tick;
        } else {
            break;
        }
    }
    let _ = bytes;

    for (i, (name, start)) in heads.iter().enumerate() {
        let end = heads.get(i + 1).map(|(_, s)| *s).unwrap_or(desc.len());
        let section = desc[*start..end].trim().to_string();
        let freeform = FREEFORM_SUBTOOLS.contains(&name.as_str());
        let (description, parameters) = if freeform {
            // freeform 子工具下发给上游时其实是 JSON 工具（input:string）。
            // codex 原文里的 "do not wrap in JSON / FREEFORM" 是针对 code-mode-host 的，
            // 对上游 Claude 会产生误导，故重写描述并补上 apply_patch 的 V4A patch 格式规范
            // （该格式规范不在 code mode 请求中，需自行提供，否则模型可能产出非法 patch）。
            let desc = augment_freeform_description(name, &section);
            let params = json!({
                "type": "object",
                "properties": {
                    "input": {"type": "string", "description": "The full freeform text argument for this tool (for apply_patch: the complete patch text)."}
                },
                "required": ["input"]
            });
            (desc, params)
        } else {
            // 非 freeform：从段落里的 TS 契约 `args: {...}` 解析出 JSON schema，
            // 让 Claude 拿到精确的参数定义（cmd/session_id/plan 等），提升调用成功率。
            // 解析失败时退回宽松 object（additionalProperties:true，上游可接受）。
            let schema = parse_ts_args_schema(&section)
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            (section, schema)
        };
        out.push(CodeModeTool {
            name: name.clone(),
            description,
            parameters,
            freeform,
        });
    }
    out
}

/// 从子工具段落里的 TS 契约中解析顶层 `args: { ... }` 块，生成 JSON schema。
///
/// 处理形如 `create_goal(args: { objective: string; token_budget?: number; })` 的声明：
/// 提取每个字段的名称、可选性、类型和前置 `//` 注释。仅解析顶层字段；嵌套对象/复杂类型
/// 降级为宽松处理。若声明是 `args: {}`（无字段）或无法解析，返回 None。
fn parse_ts_args_schema(section: &str) -> Option<Value> {
    // 找到 `args:` 之后的第一个 `{`，并按大括号配平截取到匹配的 `}`。
    let args_pos = section.find("args:")?;
    let rest = &section[args_pos + "args:".len()..];
    let open = rest.find('{')?;
    let bytes = rest.as_bytes();
    let mut depth = 0usize;
    let mut end = None;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let end = end?;
    let body = &rest[open + 1..end]; // args 对象内部

    let mut properties = serde_json::Map::new();
    let mut required: Vec<Value> = Vec::new();
    let mut pending_comment: Vec<String> = Vec::new();
    let mut inner_depth = 0i32; // 跳过嵌套对象/数组的内部行

    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        // 注释行：累积作为下一个字段的描述
        if let Some(c) = line.strip_prefix("//") {
            if inner_depth == 0 {
                pending_comment.push(c.trim().to_string());
            }
            continue;
        }

        // 跟踪嵌套深度（字段值是内联对象/数组时跳过其内部）
        let opens = line.matches('{').count() as i32;
        let closes = line.matches('}').count() as i32;

        if inner_depth > 0 {
            inner_depth += opens - closes;
            continue;
        }

        // 匹配 `name: type;` 或 `name?: type;`
        if let Some((field, optional, ty)) = parse_ts_field(line) {
            let mut prop = ts_type_to_schema(&ty);
            if !pending_comment.is_empty() {
                if let Value::Object(m) = &mut prop {
                    m.insert("description".to_string(), json!(pending_comment.join(" ")));
                }
            }
            properties.insert(field.clone(), prop);
            if !optional {
                required.push(json!(field));
            }
        }
        pending_comment.clear();
        // 若该字段行开启了未闭合的嵌套（如 `plan: Array<{`），进入跳过模式
        if opens > closes {
            inner_depth += opens - closes;
        }
    }

    if properties.is_empty() {
        return None;
    }
    Some(json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": Value::Array(required),
    }))
}

/// 解析单行 TS 字段声明 `name: type;` / `name?: type;`，返回 (字段名, 是否可选, 类型串)。
fn parse_ts_field(line: &str) -> Option<(String, bool, String)> {
    let colon = line.find(':')?;
    let key_part = line[..colon].trim();
    // 字段名必须是合法标识符（可带尾随 ?）
    let (name, optional) = if let Some(n) = key_part.strip_suffix('?') {
        (n.trim(), true)
    } else {
        (key_part, false)
    };
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    // 类型：冒号后到行尾，去掉尾随 ; 和空白
    let ty = line[colon + 1..].trim().trim_end_matches(';').trim();
    if ty.is_empty() {
        return None;
    }
    Some((name.to_string(), optional, ty.to_string()))
}

/// 把 TS 类型串映射成 JSON schema 片段。覆盖常见形态，其余降级为宽松。
fn ts_type_to_schema(ty: &str) -> Value {
    let ty = ty.trim();
    // string union → enum: "a" | "b"
    if ty.contains('|') && ty.contains('"') {
        let variants: Vec<Value> = ty
            .split('|')
            .filter_map(|s| {
                let s = s.trim();
                s.strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .map(|s| json!(s))
            })
            .collect();
        if !variants.is_empty() {
            return json!({"type": "string", "enum": Value::Array(variants)});
        }
    }
    match ty {
        "string" => json!({"type": "string"}),
        "number" => json!({"type": "number"}),
        "boolean" => json!({"type": "boolean"}),
        "Array<string>" => json!({"type": "array", "items": {"type": "string"}}),
        "Array<number>" => json!({"type": "array", "items": {"type": "number"}}),
        _ if ty.starts_with("Array<") => {
            // 复杂数组（如 Array<{...}>）：宽松处理
            json!({"type": "array"})
        }
        _ if ty.starts_with('{') => json!({"type": "object"}),
        // 其余（含裸 union、未知）：不限制类型
        _ => json!({}),
    }
}

/// 为 freeform 子工具重写/补充描述，使其适配「下发为 JSON 工具（input:string）」的形态。
/// 对 apply_patch 追加 V4A patch 格式规范（该规范不在 code mode 请求中）。
fn augment_freeform_description(name: &str, original: &str) -> String {
    // 去掉原文里针对 code-mode-host 的误导性 TS 声明块与 "do not wrap in JSON" 句
    let cleaned = original
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.starts_with("exec tool declaration")
                && !t.starts_with("```")
                && !t.starts_with("declare const tools")
        })
        .map(|l| {
            l.replace(
                "This is a FREEFORM tool, so do not wrap the patch in JSON.",
                "",
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let cleaned = cleaned.trim();

    if name == "apply_patch" {
        format!(
            "{cleaned}\n\nCall this tool with a single string parameter `input` containing the complete patch. \
Use the V4A patch format:\n\
```\n\
*** Begin Patch\n\
*** Add File: path/to/new_file.ext\n\
+<each new line prefixed with +>\n\
*** Update File: path/to/existing_file.ext\n\
@@ context line (e.g. the function signature)\n\
 <unchanged context line, prefixed with a single space>\n\
-<removed line>\n\
+<added line>\n\
*** Delete File: path/to/removed_file.ext\n\
*** End Patch\n\
```\n\
Paths are relative to the working directory. For Update File, include a few unchanged context lines \
(each prefixed with a single space) around every change so the hunk can be located; use `@@` markers \
for larger files. Always wrap the whole patch between `*** Begin Patch` and `*** End Patch`."
        )
    } else {
        format!(
            "{cleaned}\n\nCall this tool with a single string parameter `input` containing the freeform text argument."
        )
    }
}

/// 输出侧：把一个结构化的子工具调用反向合成为 exec 的 JS 源码。
///
/// code-mode-host 执行 JS 后，只有通过 `text(...)` 追加或脚本输出的内容会回传给模型；
/// 裸调用 `await tools.xxx()` 的返回值会被丢弃。因此对**取值型**子工具（exec_command/get_goal/
/// list_mcp_* 等，需要看返回结果的），用 `text(await tools.xxx(...));` 把结果输出给模型；
/// 对 apply_patch（freeform、副作用型，返回值无意义）保持裸调用。
///
/// - freeform 子工具（apply_patch）：`await tools.apply_patch("<input字符串>");`
/// - 取值型子工具：`text(await tools.<name>(<args JSON>));`
pub fn generate_exec_js(name: &str, input: &Value, freeform: bool) -> String {
    if freeform {
        // 取 input 字段作为字符串实参；若 input 本身就是字符串则直接用
        let arg = match input {
            Value::String(s) => s.clone(),
            Value::Object(map) => map
                .get("input")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| input.to_string()),
            _ => input.to_string(),
        };
        format!("await tools.{}({});", name, json!(arg))
    } else {
        let args = if input.is_null() {
            json!({})
        } else {
            input.clone()
        };
        let args_str = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
        if IMAGE_SUBTOOLS.contains(&name) {
            // 返回图片：用 image() 追加为图像项（返回值形如 {image_url, detail}）
            format!("image(await tools.{}({}));", name, args_str)
        } else {
            // text() 把返回值输出给模型（非字符串会被 JSON.stringify）
            format!("text(await tools.{}({}));", name, args_str)
        }
    }
}

/// 输入侧（历史往返）：把我们生成的 exec JS 还原成 `(sub_tool_name, input_value)`。
///
/// 只解析本模块生成的单调用形态 `await tools.NAME(ARG);`。若解析失败，返回 None，
/// 调用方应回退为把整段 JS 作为 exec 的原始输入透传。
pub fn parse_exec_js(js: &str) -> Option<(String, Value)> {
    let js = js.trim();
    // 剥掉可能的 text(...) / image(...) 包裹（取值型/图片型子工具用它输出结果）
    let unwrapped = js
        .strip_prefix("text(")
        .or_else(|| js.strip_prefix("image("));
    let inner = if let Some(rest) = unwrapped {
        rest.trim_end_matches(';')
            .trim()
            .strip_suffix(')')
            .unwrap_or(rest)
    } else {
        js
    };
    let inner = inner.trim();
    let rest = inner.strip_prefix("await ").unwrap_or(inner);
    let rest = rest.strip_prefix("tools.")?;
    // 找到第一个 '('
    let paren = rest.find('(')?;
    let name = rest[..paren].trim().to_string();
    if name.is_empty() {
        return None;
    }
    // 取最外层括号内的实参，去掉结尾的 ');' 或 ')'
    let after = &rest[paren + 1..];
    let close = after.rfind(')')?;
    let arg_str = after[..close].trim();

    let freeform = FREEFORM_SUBTOOLS.contains(&name.as_str());
    if arg_str.is_empty() {
        return Some((name, json!({})));
    }
    // 实参是 JSON（字符串字面量或对象）
    let parsed: Value = serde_json::from_str(arg_str).ok()?;
    if freeform {
        // freeform：实参是字符串，包成 {input: ...}
        match parsed {
            Value::String(s) => Some((name, json!({"input": s}))),
            other => Some((name, other)),
        }
    } else {
        Some((name, parsed))
    }
}

/// 判断某子工具名是否 freeform（供输出侧决定 JS 生成方式）。
pub fn is_freeform_subtool(name: &str) -> bool {
    FREEFORM_SUBTOOLS.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_additional_tools() {
        let item = json!({"type": "additional_tools", "role": "developer", "tools": []});
        assert!(is_additional_tools_item(&item));
        assert!(request_uses_code_mode(&[item]));
        assert!(!request_uses_code_mode(&[json!({"type": "message"})]));
    }

    #[test]
    fn test_extract_exec_subtools_and_functions() {
        let item = json!({
            "type": "additional_tools",
            "tools": [
                {
                    "type": "namespace",
                    "name": "functions",
                    "tools": [
                        {"type": "custom", "name": "exec", "description":
                            "preamble\n### `apply_patch`\nThe apply_patch tool edits files. FREEFORM.\n### `exec_command`\nRuns a command.\ncmd: string;\n"},
                        {"type": "function", "name": "wait", "description": "wait desc", "parameters": {"type":"object","properties":{"ms":{"type":"number"}}}}
                    ]
                }
            ]
        });
        let tools = extract_code_mode_tools(&item);
        let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"apply_patch"));
        assert!(names.contains(&"exec_command"));
        assert!(names.contains(&"wait"));
        // apply_patch 应为 freeform
        let ap = tools.iter().find(|t| t.name == "apply_patch").unwrap();
        assert!(ap.freeform);
        assert!(ap.parameters["properties"].get("input").is_some());
        // wait 直接透传 parameters
        let w = tools.iter().find(|t| t.name == "wait").unwrap();
        assert!(!w.freeform);
        assert_eq!(w.parameters["properties"]["ms"]["type"], "number");
    }

    #[test]
    fn test_parse_ts_args_schema_exec_command() {
        let section = r#"### `exec_command`
Runs a command in a PTY.

exec tool declaration:
```ts
declare const tools: { exec_command(args: {
  // Shell command to execute.
  cmd: string;
  // True runs the shell with -l/-i semantics.
  login?: boolean;
  // Output token budget.
  max_output_tokens?: number;
  prefix_rule?: Array<string>;
  sandbox_permissions?: "use_default" | "require_escalated";
}): Promise<unknown>; };
```"#;
        let schema = parse_ts_args_schema(section).expect("应解析出 schema");
        let props = &schema["properties"];
        assert_eq!(props["cmd"]["type"], "string");
        assert_eq!(props["cmd"]["description"], "Shell command to execute.");
        assert_eq!(props["login"]["type"], "boolean");
        assert_eq!(props["max_output_tokens"]["type"], "number");
        assert_eq!(props["prefix_rule"]["type"], "array");
        assert_eq!(props["prefix_rule"]["items"]["type"], "string");
        // union → enum
        assert_eq!(props["sandbox_permissions"]["type"], "string");
        assert_eq!(
            props["sandbox_permissions"]["enum"],
            json!(["use_default", "require_escalated"])
        );
        // 仅 cmd 必填
        assert_eq!(schema["required"], json!(["cmd"]));
    }

    #[test]
    fn test_parse_ts_args_schema_nested_array_skipped() {
        // update_plan: plan 是 Array<{...}>，嵌套内部行应被跳过，不污染顶层
        let section = r#"### `update_plan`
exec tool declaration:
```ts
declare const tools: { update_plan(args: {
  // Optional explanation.
  explanation?: string;
  // The list of steps
  plan: Array<{
  // Step status.
  status: "pending" | "in_progress" | "completed";
  step: string;
}>;
}): Promise<unknown>; };
```"#;
        let schema = parse_ts_args_schema(section).expect("应解析");
        let props = &schema["properties"];
        assert_eq!(props["explanation"]["type"], "string");
        assert_eq!(props["plan"]["type"], "array");
        // 嵌套的 status/step 不应出现在顶层
        assert!(props.get("status").is_none());
        assert!(props.get("step").is_none());
        assert_eq!(schema["required"], json!(["plan"]));
    }

    #[test]
    fn test_parse_ts_args_schema_empty_returns_none() {
        // get_goal(args: {}) 无字段 → None
        let section = "### `get_goal`\nexec tool declaration:\n```ts\ndeclare const tools: { get_goal(args: {}): Promise<unknown>; };\n```";
        assert!(parse_ts_args_schema(section).is_none());
    }

    #[test]
    fn test_exec_command_gets_real_schema_via_extract() {
        // 端到端：exec 描述里带 args 契约的 exec_command 应拿到真实 schema
        let item = json!({
            "type": "additional_tools",
            "tools": [{
                "type": "namespace", "name": "functions",
                "tools": [{"type": "custom", "name": "exec", "description":
                    "pre\n### `exec_command`\nRuns a command.\nexec tool declaration:\n```ts\ndeclare const tools: { exec_command(args: {\n  // Shell command.\n  cmd: string;\n}): Promise<unknown>; };\n```\n"}]
            }]
        });
        let tools = extract_code_mode_tools(&item);
        let ec = tools.iter().find(|t| t.name == "exec_command").unwrap();
        assert!(!ec.freeform);
        assert_eq!(ec.parameters["properties"]["cmd"]["type"], "string");
        assert_eq!(ec.parameters["required"], json!(["cmd"]));
    }

    #[test]
    fn test_apply_patch_description_augmented() {
        let item = json!({
            "type": "additional_tools",
            "tools": [{
                "type": "namespace", "name": "functions",
                "tools": [{"type": "custom", "name": "exec", "description":
                    "pre\n### `apply_patch`\nThe `apply_patch` tool can be used to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON.\n\nexec tool declaration:\n```ts\ndeclare const tools: { apply_patch(input: string): Promise<unknown>; };\n```\n"}]
            }]
        });
        let tools = extract_code_mode_tools(&item);
        let ap = tools.iter().find(|t| t.name == "apply_patch").unwrap();
        // 误导句被移除，补上了 V4A 格式规范
        assert!(!ap.description.contains("do not wrap the patch in JSON"));
        assert!(!ap.description.contains("declare const tools"));
        assert!(ap.description.contains("*** Begin Patch"));
        assert!(ap.description.contains("input"));
    }

    #[test]
    fn test_generate_exec_js_freeform() {
        let js = generate_exec_js(
            "apply_patch",
            &json!({"input": "*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch"}),
            true,
        );
        assert!(js.starts_with("await tools.apply_patch(\""));
        assert!(js.ends_with(");"));
        assert!(js.contains("Begin Patch"));
    }

    #[test]
    fn test_generate_exec_js_object_wraps_text() {
        // 取值型子工具用 text() 包裹返回值，模型才看得到命令输出
        let js = generate_exec_js("exec_command", &json!({"cmd": "ls -la"}), false);
        assert_eq!(js, "text(await tools.exec_command({\"cmd\":\"ls -la\"}));");
    }

    #[test]
    fn test_parse_exec_js_roundtrip_freeform() {
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch";
        let js = generate_exec_js("apply_patch", &json!({"input": patch}), true);
        let (name, input) = parse_exec_js(&js).unwrap();
        assert_eq!(name, "apply_patch");
        assert_eq!(input["input"], patch);
    }

    #[test]
    fn test_parse_exec_js_roundtrip_object() {
        let js = generate_exec_js("exec_command", &json!({"cmd": "ls", "login": true}), false);
        let (name, input) = parse_exec_js(&js).unwrap();
        assert_eq!(name, "exec_command");
        assert_eq!(input["cmd"], "ls");
        assert_eq!(input["login"], true);
    }

    #[test]
    fn test_generate_exec_js_view_image_uses_image() {
        // view_image 返回图片，应用 image() 而非 text()
        let js = generate_exec_js("view_image", &json!({"path": "/tmp/a.png"}), false);
        assert!(
            js.starts_with("image(await tools.view_image("),
            "实际: {js}"
        );
        assert!(!js.contains("text("));
    }

    #[test]
    fn test_parse_exec_js_strips_image_wrapper() {
        let js = "image(await tools.view_image({\"path\":\"/tmp/a.png\"}));";
        let (name, input) = parse_exec_js(js).unwrap();
        assert_eq!(name, "view_image");
        assert_eq!(input["path"], "/tmp/a.png");
    }

    #[test]
    fn test_parse_exec_js_strips_text_wrapper() {
        // 取值型子工具的 text(await tools.x(...)) 也能被历史解析还原
        let js = "text(await tools.exec_command({\"cmd\":\"ls\"}));";
        let (name, input) = parse_exec_js(js).unwrap();
        assert_eq!(name, "exec_command");
        assert_eq!(input["cmd"], "ls");
    }

    #[test]
    fn test_parse_exec_js_invalid() {
        assert!(parse_exec_js("some random text").is_none());
        assert!(parse_exec_js("console.log('x')").is_none());
    }
}
