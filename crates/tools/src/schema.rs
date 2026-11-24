//! JSON Schema 校验：编译缓存与参数校验。

use jsonschema::Validator;
use ra_core::{
  error::{RiboError, RiboResult, Severity},
  message::Schema,
};

use crate::error::{DOMAIN, INVALID_ARGUMENTS, invalid_arguments};

/// 编译后的 JSON Schema 校验器（`jsonschema` 的可复用实例）。
pub type CompiledValidator = Validator;

/// 把工具的入参声明编译为校验器（每个工具注册期执行一次）。
///
/// # Errors
///
/// Schema 本身非法（不合 JSON Schema 规范）→ `tool.invalid_arguments`，
/// 携带 `reason` context 说明编译失败原因。
pub fn compile_validator(schema: &Schema) -> RiboResult<CompiledValidator> {
  jsonschema::validator_for(&schema.0).map_err(|e| {
    RiboError::new(
      DOMAIN,
      INVALID_ARGUMENTS,
      format!("invalid input schema: {e}"),
    )
    .with_severity(Severity::Error)
    .with_context("reason", format!("{e}"))
  })
}

/// 用编译好的校验器校验一次工具入参。
///
/// 校验错误聚合展示：最多取前 5 条，格式为
/// `实例路径:错误信息`，用 `; ` 连接，便于模型定位并修正。
///
/// # Errors
///
/// 任一条目校验失败 → `tool.invalid_arguments`。
pub fn validate_arguments(
  tool_name: impl Into<String>,
  validator: &CompiledValidator,
  args: &serde_json::Value,
) -> RiboResult<()> {
  // 限制条数防止超大入参产生过长错误信息挤占模型上下文
  let errors: Vec<String> = validator
    .iter_errors(args)
    .take(5)
    .map(|e| format!("{}:{e}", e.instance_path()))
    .collect();

  if errors.is_empty() {
    Ok(())
  } else {
    Err(invalid_arguments(tool_name, errors.join("; ")))
  }
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;
  use crate::error::INVALID_ARGUMENTS;

  /// 编译一个严格对象 Schema：必填字符串字段 `a`，禁止额外字段。
  fn strict_object_validator() -> CompiledValidator {
    let schema = Schema(json!({
      "type": "object",
      "properties": { "a": { "type": "string" } },
      "required": ["a"],
      "additionalProperties": false
    }));
    compile_validator(&schema).expect("strict object schema should compile")
  }

  /// 场景：合法参数通过校验。
  ///
  /// 预期：满足必填与类型的入参返回 Ok(())。
  #[test]
  fn valid_arguments_pass() {
    let validator = strict_object_validator();
    validate_arguments("t", &validator, &json!({ "a": "x" }))
      .expect("valid arguments should pass");
  }

  /// 场景：类型错误 / 缺必填 / 多余字段均被拒绝。
  ///
  /// 预期：三种违规入参都返回 `tool.invalid_arguments`。
  #[test]
  fn invalid_arguments_are_rejected() {
    let validator = strict_object_validator();

    // 类型错误：a 传数字
    let err =
      validate_arguments("t", &validator, &json!({ "a": 1 })).unwrap_err();
    assert_eq!(err.code(), INVALID_ARGUMENTS);

    // 缺少必填字段
    let err = validate_arguments("t", &validator, &json!({})).unwrap_err();
    assert_eq!(err.code(), INVALID_ARGUMENTS);

    // additionalProperties=false：多余字段被拒
    let err =
      validate_arguments("t", &validator, &json!({ "a": "x", "b": true }))
        .unwrap_err();
    assert_eq!(err.code(), INVALID_ARGUMENTS);
  }

  /// 场景：Schema 本身非法 → 编译失败。
  ///
  /// 预期：`compile_validator` 返回 `tool.invalid_arguments`。
  #[test]
  fn invalid_schema_fails_to_compile() {
    // "type" 关键字取值非字符串，不符合 JSON Schema 规范
    let schema = Schema(json!({ "type": 123 }));
    let err = compile_validator(&schema).unwrap_err();
    assert_eq!(err.code(), INVALID_ARGUMENTS);
  }

  /// 场景：校验错误聚合最多展示前 5 条。
  ///
  /// 预期：6 个字段全部类型错误时，错误消息按 "; " 拆分恰好 5 段
  /// （`iter_errors` 的 `take(5)` 截断，避免错误信息无限膨胀）。
  #[test]
  fn validation_errors_capped_at_five_entries() {
    // 6 个字段全部声明为 integer，入参全部传字符串 → 产生 6 条类型错误
    let schema = Schema(json!({
      "type": "object",
      "properties": {
        "p1": { "type": "integer" },
        "p2": { "type": "integer" },
        "p3": { "type": "integer" },
        "p4": { "type": "integer" },
        "p5": { "type": "integer" },
        "p6": { "type": "integer" }
      }
    }));
    let validator = compile_validator(&schema).expect("schema should compile");
    let args = json!({
      "p1": "s", "p2": "s", "p3": "s", "p4": "s", "p5": "s", "p6": "s"
    });

    let err = validate_arguments("t", &validator, &args).unwrap_err();
    assert_eq!(err.code(), INVALID_ARGUMENTS);

    // 聚合上限：恰好 5 条，用 "; " 连接
    let segments: Vec<&str> = err.message().split("; ").collect();
    assert_eq!(
      segments.len(),
      5,
      "aggregated errors should cap at 5 entries: {err}"
    );
  }
}
