//! PDK 过程宏：`#[ribo_tool]` 与 `#[ribo_plugin]`。
//!
//! - `#[ribo_tool(name = "...", description = "...", parameters = r#"{...}"#,
//!   idempotent = true, timeout_hint_ms = 10000, memory_limit_bytes = 16777216)]`
//!   标注工具函数 `fn(Value) -> Result<Value, String>`，生成
//!   `__RIBO_TOOL_<NAME>` 描述符常量与调用包装；
//! - `#[ribo_plugin(id = "...")]` 标注工具所在的模块，收集模块内全部
//!   `__RIBO_TOOL_*` 常量，生成 ABI v0 导出（`memory` 由 rustc wasm 目标
//!   默认导出；`alloc` / `dealloc` / `list_tools` / `execute` 在此生成）。
//!
//! ABI 约定（wasmtime 50 / rustc 稳定性实测校准，与宿主侧
//! `crates/plugins/src/wasm` 对应）：
//!
//! - rustc 不支持 `extern "C"` 多值返回 → `list_tools` / `execute` 返回
//!   **单 i32 打包指针**，指向 8 字节块 `{ptr: i32 LE, len: i32 LE}`；
//! - host import（storage_get / http_proxy）只能返回单 i32 → 采用
//!   **帧化返回** `[len: u32 LE][payload]`。

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
  Item, ItemFn, ItemMod, Lit, Meta, Token, parse::Parser, parse_macro_input,
  punctuated::Punctuated, spanned::Spanned,
};

/// `#[ribo_tool(...)]` 的属性参数（解析后的结构化形式）。
struct ToolArgs {
  name: String,
  description: String,
  /// `parameters` 的 JSON 源文本（字符串字面量解析后的值）。
  parameters: String,
  idempotent: bool,
  timeout_hint_ms: Option<u64>,
  memory_limit_bytes: Option<usize>,
}

/// 解析 `key = value` 逗号分隔的属性参数。
fn parse_tool_args(attr: TokenStream) -> syn::Result<ToolArgs> {
  let punctuated =
    Punctuated::<Meta, Token![,]>::parse_terminated.parse(attr.clone())?;
  let mut name = None;
  let mut description = None;
  let mut parameters = None;
  let mut idempotent = false;
  let mut timeout_hint_ms = None;
  let mut memory_limit_bytes = None;

  for meta in punctuated {
    let pair = meta.require_name_value()?;
    let key = pair.path.get_ident().ok_or_else(|| {
      syn::Error::new(pair.path.span(), "expected a single key like `name`")
    })?;
    match key.to_string().as_str() {
      "name" => {
        name = Some(expect_str_lit(&pair.value)?);
      }
      "description" => {
        description = Some(expect_str_lit(&pair.value)?);
      }
      "parameters" => {
        // JSON 字符串字面量：取解析后的字符串值（即 JSON 源文本），
        // 生成侧插值回字符串字面量供 serde_json::from_str 消费
        parameters = Some(expect_str_lit(&pair.value)?);
      }
      "idempotent" => {
        idempotent = expect_bool_lit(&pair.value)?;
      }
      "timeout_hint_ms" => {
        timeout_hint_ms = Some(expect_int_lit(&pair.value)?);
      }
      "memory_limit_bytes" => {
        memory_limit_bytes = Some(
          usize::try_from(expect_int_lit(&pair.value)?).map_err(|_| {
            syn::Error::new(
              pair.value.span(),
              "memory_limit_bytes must be >= 0",
            )
          })?,
        );
      }
      other => {
        return Err(syn::Error::new(
          pair.path.span(),
          format!("unknown ribo_tool key `{other}`"),
        ));
      }
    }
  }

  // proc_macro::TokenStream 不实现 ToTokens/Spanned，转 proc_macro2 后
  // 用 new_spanned 定位错误（覆盖整个属性参数）
  let attr2: proc_macro2::TokenStream = attr.clone().into();
  Ok(ToolArgs {
    name: name
      .ok_or_else(|| syn::Error::new_spanned(&attr2, "missing `name`"))?,
    description: description.ok_or_else(|| {
      syn::Error::new_spanned(&attr2, "missing `description`")
    })?,
    parameters: parameters
      .ok_or_else(|| syn::Error::new_spanned(&attr2, "missing `parameters`"))?,
    idempotent,
    timeout_hint_ms,
    memory_limit_bytes,
  })
}

fn expect_str_lit(expr: &syn::Expr) -> syn::Result<String> {
  match expr {
    syn::Expr::Lit(lit) => match &lit.lit {
      Lit::Str(s) => Ok(s.value()),
      other => Err(syn::Error::new(other.span(), "expected a string literal")),
    },
    other => Err(syn::Error::new(other.span(), "expected a string literal")),
  }
}

fn expect_bool_lit(expr: &syn::Expr) -> syn::Result<bool> {
  match expr {
    syn::Expr::Lit(lit) => match &lit.lit {
      Lit::Bool(b) => Ok(b.value()),
      other => Err(syn::Error::new(other.span(), "expected true or false")),
    },
    other => Err(syn::Error::new(other.span(), "expected true or false")),
  }
}

fn expect_int_lit(expr: &syn::Expr) -> syn::Result<u64> {
  match expr {
    syn::Expr::Lit(lit) => match &lit.lit {
      Lit::Int(i) => Ok(i.base10_parse()?),
      other => {
        Err(syn::Error::new(other.span(), "expected an integer literal"))
      }
    },
    other => Err(syn::Error::new(other.span(), "expected an integer literal")),
  }
}

/// 工具函数标注：生成描述符常量 + 调用包装 + 参数 Schema 取值函数。
#[proc_macro_attribute]
pub fn ribo_tool(attr: TokenStream, item: TokenStream) -> TokenStream {
  let args = match parse_tool_args(attr.clone()) {
    Ok(a) => a,
    Err(e) => return e.to_compile_error().into(),
  };
  let func = parse_macro_input!(item as ItemFn);
  let user_fn = &func.sig.ident;

  let tool_name = args.name;
  let const_ident = format_ident!("__RIBO_TOOL_{}", tool_name);
  let call_ident = format_ident!("__ribo_call_{}", tool_name);
  let params_ident = format_ident!("__ribo_params_{}", tool_name);
  let description = args.description;
  let idempotent = args.idempotent;
  let timeout = match args.timeout_hint_ms {
    Some(ms) => quote! { ::core::option::Option::Some(#ms) },
    None => quote! { ::core::option::Option::None },
  };
  let mem_limit = match args.memory_limit_bytes {
    Some(n) => quote! { ::core::option::Option::Some(#n) },
    None => quote! { ::core::option::Option::None },
  };
  let params_group = &args.parameters;

  let expanded = quote! {
    #func

    #[doc(hidden)]
    pub fn #params_ident() -> ::serde_json::Value {
      ::serde_json::from_str(#params_group)
        .expect("invalid `parameters` JSON literal in #[ribo_tool]")
    }

    #[doc(hidden)]
    pub fn #call_ident(args: ::serde_json::Value) -> ::std::result::Result<::serde_json::Value, String> {
      #user_fn(args)
    }

    #[doc(hidden)]
    pub const #const_ident: ::ra_plug_pdk::ToolEntry = ::ra_plug_pdk::ToolEntry {
      meta: ::ra_plug_pdk::ToolMeta {
        name: #tool_name,
        description: #description,
        parameters_fn: #params_ident,
        timeout_hint_ms: #timeout,
        idempotent: #idempotent,
        memory_limit_bytes: #mem_limit,
      },
      call: #call_ident,
    };
  };
  expanded.into()
}

/// 插件模块标注：收集 `__RIBO_TOOL_*` 常量并生成 ABI v0 导出。
#[proc_macro_attribute]
pub fn ribo_plugin(attr: TokenStream, item: TokenStream) -> TokenStream {
  let attr2: proc_macro2::TokenStream = attr.clone().into();
  let plugin_id = match Punctuated::<Meta, Token![,]>::parse_terminated
    .parse(attr.clone())
    .and_then(|metas| {
      metas
        .iter()
        .find_map(|m| {
          let pair = m.require_name_value().ok()?;
          if pair.path.is_ident("id") {
            Some(expect_str_lit(&pair.value))
          } else {
            None
          }
        })
        .ok_or_else(|| {
          syn::Error::new_spanned(&attr2, "missing `id = \"...\"`")
        })?
    }) {
    Ok(id) => id,
    Err(e) => return e.to_compile_error().into(),
  };

  let mut module = parse_macro_input!(item as ItemMod);
  let Some((_, ref mut items)) = module.content else {
    return syn::Error::new(
      module.span(),
      "#[ribo_plugin] requires an inline module",
    )
    .to_compile_error()
    .into();
  };

  // 收集模块内的工具描述符来源（修复：宏展开顺序）。
  //
  // 属性宏自外向内展开：`#[ribo_plugin]` 先于其模块内的 `#[ribo_tool]`
  // 执行——此刻 `__RIBO_TOOL_*` 常量**尚不存在**（它们由 ribo_tool 在
  // 后续轮次展开生成）。故除扫描既有常量外，还须预解析带
  // `#[ribo_tool]` 标注的函数的 `name` 参数，提前生成对
  // `__RIBO_TOOL_{name}` 的引用；名字解析发生在全部展开完成之后，
  // 引用可以解析到后来生成的常量。
  let mut const_idents: Vec<proc_macro2::Ident> = Vec::new();
  for item in items.iter() {
    match item {
      Item::Const(c) if c.ident.to_string().starts_with("__RIBO_TOOL_") => {
        const_idents.push(c.ident.clone());
      }
      Item::Fn(f) => {
        if let Some(name) = ribo_tool_name_of(f) {
          const_idents.push(format_ident!("__RIBO_TOOL_{name}"));
        }
      }
      _ => {}
    }
  }

  let exports = quote! {
    #[doc(hidden)]
    pub(crate) const __RIBO_PLUGIN_ID: &str = #plugin_id;
    #[doc(hidden)]
    pub(crate) const __RIBO_TOOLS: &[::ra_plug_pdk::ToolEntry] = &[#(#const_idents),*];

    // ABI v0 的 alloc / dealloc 导出由 `ra_plug_pdk::runtime` 以
    // `#[unsafe(no_mangle)]` 提供且仅此一处——宏侧若再生成同名导出
    // 会在链接期报 duplicate symbol（每个使用 #[ribo_plugin] 的 guest
    // 都依赖 ra-plug-pdk，符号必然被链接进来）。

    /// ABI v0：返回 manifest JSON（单 i32 打包指针，见 crate 文档）。
    #[unsafe(no_mangle)]
    pub extern "C" fn list_tools() -> i32 {
      ::ra_plug_pdk::runtime::list_tools(__RIBO_PLUGIN_ID, __RIBO_TOOLS)
    }

    /// ABI v0：执行工具（信封解包 + 派发 + 结果打包）。
    #[unsafe(no_mangle)]
    pub extern "C" fn execute(name_ptr: i32, name_len: i32, args_ptr: i32, args_len: i32) -> i32 {
      ::ra_plug_pdk::runtime::execute(__RIBO_PLUGIN_ID, __RIBO_TOOLS, name_ptr, name_len, args_ptr, args_len)
    }
  };
  items.push(Item::Verbatim(exports));

  quote! { #module }.into()
}

/// 从函数的 `#[ribo_tool(...)]` 属性中预解析工具名（供
/// `#[ribo_plugin]` 在内层宏展开前生成描述符引用）。
///
/// 属性路径按末段匹配（兼容 `ribo_tool` / `ra_plug_pdk::ribo_tool`
/// 两种写法）；无法解析（属性缺失 / 无 `name`）返回 `None`，
/// 该函数不进入 `__RIBO_TOOLS`（后续 ribo_tool 展开会因缺参报错，
/// 错误在函数处更清晰）。
fn ribo_tool_name_of(f: &ItemFn) -> Option<String> {
  let attr = f.attrs.iter().find(|a| {
    a.path()
      .segments
      .last()
      .is_some_and(|s| s.ident == "ribo_tool")
  })?;
  let metas = attr
    .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
    .ok()?;
  metas.iter().find_map(|m| {
    let pair = m.require_name_value().ok()?;
    if pair.path.is_ident("name") {
      expect_str_lit(&pair.value).ok()
    } else {
      None
    }
  })
}
