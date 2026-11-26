//! 面向用户的错误文案映射。
//!
//! `RiboError::Display` 输出的是 `[domain] code: message`，适合开发者排查，
//! 不适合直接展示给终端用户。本模块按 `domain` 与 `code` 把底层错误收敛为
//! 「中文结论 + 可操作建议」，原始错误文本仍由调用方写入日志。
//!
//! 错误码可能带域前缀（例如 `provider.rate_limited`），也可能只有裸码
//! （例如 `config_file_missing`）；匹配前会按 `domain` 归一化，两种形式
//! 得到相同文案。未收录的错误码按域返回兜底提示，不暴露内部错误码。

use core::fmt;

use super::RiboError;

/// 面向用户的错误文案：一句结论，加一句可操作建议。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserFacingError {
  /// 结论：发生了什么。
  pub summary: &'static str,
  /// 建议：用户下一步可以做什么，句末带标点。
  pub action: &'static str,
}

impl fmt::Display for UserFacingError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}。{}", self.summary, self.action)
  }
}

/// 从 [`RiboError`] 取用户可见文案。
pub fn for_error(err: &RiboError) -> UserFacingError {
  for_code(err.domain(), err.code())
}

/// 从域与错误码取用户可见文案。
///
/// 供只有错误码、没有完整错误对象的场景使用（例如流内错误事件）。
pub fn for_code(domain: &str, code: &str) -> UserFacingError {
  let key = code
    .strip_prefix(domain)
    .and_then(|rest| rest.strip_prefix('.'))
    .unwrap_or(code);
  match domain {
    "provider" => provider(key),
    "config" => config(key),
    "agent" => agent(key),
    "tool" => tool(key),
    "plugin" => plugin(key),
    _ => generic(domain),
  }
}

fn provider(code: &str) -> UserFacingError {
  let (summary, action) = match code {
    "rate_limited" => ("模型请求过于频繁", "请稍后重试，或切换其他模型。"),
    "auth_failed" => (
      "模型服务拒绝了当前密钥",
      "请检查 API 密钥是否与模型服务匹配。",
    ),
    "quota_exceeded" => {
      ("模型服务额度已用尽", "请检查账户额度，或切换其他模型。")
    }
    "invalid_request" => (
      "请求未被模型服务接受",
      "请缩短输入内容后重试；若持续失败，请检查模型配置。",
    ),
    "timeout" => ("模型服务响应超时", "请稍后重试，或切换其他模型。"),
    "server_error" => ("模型服务暂时不可用", "请稍后重试。"),
    "transport_error" => {
      ("连接模型服务失败", "请检查网络与模型服务地址后重试。")
    }
    "invalid_response" => (
      "模型服务返回了无法解析的内容",
      "请稍后重试；若持续失败，请更换模型服务。",
    ),
    "content_filtered" => ("内容被模型服务拦截", "请修改输入内容后重试。"),
    "stream_interrupted" => ("模型响应中途断开", "请重试本轮对话。"),
    "cancelled" => ("请求已取消", "可重新发送本轮内容。"),
    "provider_not_found" => (
      "配置的模型提供方不存在",
      "请检查 ribo.toml 中的 provider 配置。",
    ),
    "model_not_found" => {
      ("配置的模型不存在", "请检查 ribo.toml 中的模型名称。")
    }
    "circuit_open" => ("模型服务暂时被熔断", "请稍后重试，或切换其他模型。"),
    "overloaded" => ("模型服务当前繁忙", "请稍后重试。"),
    "duplicate_provider" => (
      "模型提供方重复配置",
      "请检查 ribo.toml 中是否有重复的 provider id。",
    ),
    "retry_provider_refused" => (
      "模型提供方配置与网关重试策略冲突",
      "请检查该提供方是否自行开启了重试。",
    ),
    _ => (
      "模型请求失败",
      "请检查网络、模型地址与密钥配置，或稍后重试。",
    ),
  };
  UserFacingError { summary, action }
}

fn config(code: &str) -> UserFacingError {
  let (summary, action) = match code {
    "config_file_missing" => (
      "未找到指定的配置文件",
      "请确认文件路径，或使用 --config 指定 ribo.toml。",
    ),
    "no_providers_in_release" => (
      "发布版未配置模型服务",
      "请在 ribo.toml 中至少启用一个模型提供方。",
    ),
    "placeholder_in_release" => (
      "配置仍是示例占位内容",
      "请把 ribo.toml 中的占位项替换为真实配置。",
    ),
    "query_model_unavailable_in_release" => (
      "默认模型配置不可用",
      "请检查 ribo.toml 的 [routing].default 配置（provider 与 model），并确认对应 API 密钥已设置。",
    ),
    "compression_invalid" => (
      "上下文压缩配置无效",
      "请检查 ribo.toml 中的 compression 配置项。",
    ),
    "mount_invalid" => (
      "文件系统挂载配置无效",
      "请检查 ribo.toml 中的 fs.mounts 配置。",
    ),
    "plugin_file_missing" => {
      ("插件文件不存在", "请检查 ribo.toml 中的插件路径。")
    }
    "plugin_load_failed" => {
      ("插件加载失败", "请检查插件文件、依赖与来源校验配置。")
    }
    "plugin_trusted_keys_unreadable" => {
      ("插件信任列表无法读取", "请检查用户目录中的信任文件权限。")
    }
    "plugin_trusted_keys_invalid" => {
      ("插件信任列表格式不正确", "请检查信任文件中的公钥配置。")
    }
    "plugin_manifest_unreadable" => {
      ("插件清单无法读取", "请检查插件清单文件是否存在且可读。")
    }
    "plugin_manifest_invalid" => {
      ("插件清单格式不正确", "请核对插件清单内容后重试。")
    }
    "plugin_provenance_unreadable" => {
      ("插件来源信息无法读取", "请检查插件来源描述文件。")
    }
    "plugin_duplicate_id" => {
      ("插件 id 重复", "请检查 ribo.toml 中是否有重复的插件声明。")
    }
    _ => ("配置校验未通过", "请检查 ribo.toml 与用户目录配置后重试。"),
  };
  UserFacingError { summary, action }
}

fn agent(code: &str) -> UserFacingError {
  let (summary, action) = match code {
    "session_not_found" => ("会话不存在", "请切换已有会话，或新建会话后重试。"),
    "session_create_failed" => ("会话创建失败", "请检查存储目录权限后重试。"),
    "session_busy" => ("会话正在处理", "请等待当前回合结束后再操作。"),
    "max_iterations" => ("已达到迭代上限", "请简化任务或分批发送。"),
    "context_build_error" => ("上下文构建失败", "请切换会话或缩短输入后重试。"),
    "tool_exec_error" => ("工具执行失败", "请检查工具参数后重试。"),
    "cancelled" => ("请求已取消", "可重新发送本轮内容。"),
    _ => ("会话处理失败", "请重试当前操作，必要时切换会话。"),
  };
  UserFacingError { summary, action }
}

fn tool(code: &str) -> UserFacingError {
  let (summary, action) = match code {
    "not_found" => ("工具未注册", "请检查工具配置或更换工具名称。"),
    "already_registered" => {
      ("工具重复注册", "请检查插件或工具配置中是否有重复声明。")
    }
    "invalid_arguments" => ("工具参数不正确", "请检查参数后重试。"),
    "exec_failed" => ("工具执行失败", "请检查工具参数或目标文件后重试。"),
    "cancelled" => ("工具执行已取消", "可重新发送本轮内容。"),
    "rate_limited" => ("工具调用过于频繁", "请稍后重试。"),
    "plugin_forbidden" => {
      ("当前域不允许该工具写入", "请检查工具的目标域配置。")
    }
    _ => ("工具执行失败", "请稍后重试。"),
  };
  UserFacingError { summary, action }
}

fn plugin(code: &str) -> UserFacingError {
  let (summary, action) = match code {
    "plugin_exec_failed" => {
      ("插件执行失败", "请检查插件参数或插件实现后重试。")
    }
    "plugin_timeout" => ("插件执行超时", "请稍后重试或更换插件。"),
    "plugin_forbidden" => {
      ("插件缺少所需能力授权", "请检查插件的能力声明与信任配置。")
    }
    "plugin_internal" => {
      ("插件宿主内部错误", "请稍后重试；若持续失败，请禁用该插件。")
    }
    "plugin_invalid_arguments" => ("插件收到非法参数", "请检查插件接口实现。"),
    "plugin_signature_invalid" => {
      ("插件来源校验失败", "请确认插件签名与信任列表。")
    }
    "plugin_duplicate_id" => ("插件 id 重复", "请检查是否有同名插件。"),
    "plugin_invalid_name" => {
      ("插件名称不合法", "请检查插件 id 与工具名是否符合命名规则。")
    }
    "plugin_schema_invalid" => {
      ("插件工具参数定义无效", "请检查插件的工具 Schema。")
    }
    "plugin_batch_failed" => {
      ("插件批量注册失败", "请检查插件之间是否存在冲突。")
    }
    _ => ("插件加载或执行失败", "请检查插件配置与来源校验设置。"),
  };
  UserFacingError { summary, action }
}

fn generic(domain: &str) -> UserFacingError {
  let (summary, action) = match domain {
    "io" => ("文件读写失败", "请检查路径与权限后重试。"),
    _ => ("操作未完成", "请稍后重试。"),
  };
  UserFacingError { summary, action }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn maps_prefixed_provider_code() {
    let err =
      RiboError::new("provider", "provider.rate_limited", "HTTP 429: {...}");
    let msg = for_error(&err);
    assert_eq!(msg.summary, "模型请求过于频繁");
    assert!(msg.to_string().contains("请稍后重试"));
    assert!(!msg.to_string().contains("HTTP 429"));
  }

  #[test]
  fn maps_bare_config_code() {
    let err = RiboError::new(
      "config",
      "query_model_unavailable_in_release",
      "default model no longer exposed after skip (release build)",
    );
    let msg = for_error(&err);
    assert_eq!(msg.summary, "默认模型配置不可用");
    assert!(msg.to_string().contains("[routing].default"));
    assert!(msg.to_string().contains("API 密钥"));
    assert!(!msg.to_string().contains("query 模型"));
    assert!(!msg.to_string().contains("发布版"));
    assert!(!msg.to_string().contains("release build"));
  }

  #[test]
  fn unknown_code_falls_back_to_domain_hint() {
    let err =
      RiboError::new("provider", "provider.internal_mystery", "secret detail");
    let msg = for_error(&err);
    assert_eq!(msg.summary, "模型请求失败");
    assert!(!msg.to_string().contains("internal_mystery"));
    assert!(!msg.to_string().contains("secret detail"));
  }
}
