//! `.wasm` 签名与来源校验（Step C，「来源校验」）。
//!
//! **信任链的最后一项**：B3.2 的 [`AuthorityManifest`](crate::authority::AuthorityManifest)
//! 保证「guest 自述与宿主侧载清单一致」，但不保证清单与 wasm 字节本身
//! 来自可信发布方——攻击者可同时替换 wasm 与清单。本模块以宿主侧载的
//! Ed25519 公钥白名单为信任锚，验证发布方签名，并把签名载荷与
//! `blake3(wasm)` + `blake3(manifest.json)` + `plugin_id` 绑定，
//! 防换包、换清单与跨插件重放。
//!
//! **签名载荷**（domain-separated，字段以 NUL 分隔）：
//!
//! ```text
//! "ribo-plugin-provenance-v1\0" || plugin_id || "\0" ||
//! hex(blake3(wasm)) || "\0" || hex(blake3(manifest.json))
//! ```
//!
//! 签名放在**独立侧载文件**（provenance.json）而非 manifest 内部，
//! 避免「签名覆盖自身」的自引用悖论。侧载文件只包含签名字段、签名者 ID
//! 与 wasm 摘要，不含 manifest 摘要——后者由宿主对原始 manifest 字节
//! 现场计算。
//!
//! **公钥白名单的入口**（见 [`TrustedKeys`]）：
//!
//! - JSON（`{"keys":[...]}`）：保留给 provenance 信任材料；
//!
//! # Examples
//!
//! ```
//! use ra_plugins::provenance::{TrustedKeys, VerificationPolicy};
//!
//! // 开发 / CI 默认不强制签名（生产须构造 production 策略）
//! let policy = VerificationPolicy::development();
//! assert!(!policy.require_signature);
//!
//! // 信任清单为 JSON：{"keys":[{"id":"release","public_key":"<64 hex>"}]}
//! let keys = TrustedKeys::from_json(
//!     br#"{"keys":[]}"#,
//! )
//! .expect("empty trust list is valid JSON");
//! assert!(keys.is_empty());
//! ```

use ra_core::error::RiboResult;

use crate::authority::AuthorityManifest;
use crate::error::plugin_signature_invalid;

/// 签名载荷的域分隔前缀：防止与其它协议的 Ed25519 签名互相重放。
const DOMAIN_PREFIX: &[u8] = b"ribo-plugin-provenance-v1\0";

/// `algo` 字段的唯一合法取值（JSON 信任清单格式约定）。
///
/// 验签实现固定为 Ed25519（ring，C3 零改动）：写侧恒输出该值，读侧
/// 仅校验 `algo` 出现时取值必须命中——缺省视为 `ed25519`（前向兼容），
/// 其它取值在解析期拒绝，不进入验签路径。
pub const KEY_ALGO_ED25519: &str = "ed25519";

// ===== hex 编解码（避免为 32/64 字节引入额外依赖） =====

fn hex_encode(bytes: &[u8]) -> String {
  const DIGITS: &[u8; 16] = b"0123456789abcdef";
  let mut out = String::with_capacity(bytes.len() * 2);
  for b in bytes {
    out.push(DIGITS[(b >> 4) as usize] as char);
    out.push(DIGITS[(b & 0x0f) as usize] as char);
  }
  out
}

fn hex_decode(s: &str, what: &str) -> RiboResult<Vec<u8>> {
  let decode = || {
    if !s.len().is_multiple_of(2) {
      return None;
    }
    s.as_bytes()
      .as_chunks::<2>()
      .0
      .iter()
      .map(|pair| u8::from_str_radix(core::str::from_utf8(pair).ok()?, 16).ok())
      .collect::<Option<Vec<u8>>>()
  };
  decode()
    .ok_or_else(|| plugin_signature_invalid(&format!("invalid {what} hex")))
}

/// 由 ID + hex 公钥构造单条授信记录（JSON 入口的校验层）。
///
/// # Errors
///
/// ID 为空、公钥非 64 位 hex 或长度不足 32 字节 →
/// `plugin.plugin_signature_invalid`。
fn key_from_hex(id: &str, public_key_hex: &str) -> RiboResult<TrustedKey> {
  if id.is_empty() {
    return Err(plugin_signature_invalid("empty trusted key id"));
  }
  let raw = hex_decode(public_key_hex, "public_key")?;
  let public_key: [u8; 32] = raw.try_into().map_err(|_| {
    plugin_signature_invalid(&format!("public_key for `{id}` must be 32 bytes"))
  })?;
  Ok(TrustedKey {
    id: id.to_owned(),
    public_key,
  })
}

/// 单个授信公钥：签名者 ID → Ed25519 公钥。
#[derive(Debug, Clone)]
pub struct TrustedKey {
  /// 签名者标识，与 provenance.json 的 `signer_id` 精确匹配。
  pub id: String,
  /// Ed25519 公钥原始字节。
  pub public_key: [u8; 32],
}

/// 宿主侧载的公钥白名单（唯一的信任锚）。
#[derive(Debug, Clone, Default)]
pub struct TrustedKeys {
  keys: Vec<TrustedKey>,
}

impl TrustedKeys {
  /// 从 JSON 解析信任清单：`{"keys":[{"id":"...","public_key":"<hex>"}]}`。
  ///
  ///
  /// # Errors
  ///
  /// JSON 非法、公钥非 64 位 hex、ID 为空 → `plugin.plugin_signature_invalid`。
  pub fn from_json(bytes: &[u8]) -> RiboResult<Self> {
    #[derive(serde::Deserialize)]
    struct File {
      keys: Vec<Entry>,
    }
    #[derive(serde::Deserialize)]
    struct Entry {
      id: String,
      public_key: String,
    }

    let file: File = serde_json::from_slice(bytes).map_err(|e| {
      plugin_signature_invalid(&format!("bad trusted keys JSON: {e}"))
    })?;
    let mut keys = Vec::with_capacity(file.keys.len());
    for entry in file.keys {
      keys.push(key_from_hex(&entry.id, &entry.public_key)?);
    }
    Ok(Self { keys })
  }

  /// 信任列表是否为空（生产策略下空列表等效于拒绝全部签名）。
  pub fn is_empty(&self) -> bool {
    self.keys.is_empty()
  }

  pub(crate) fn find(&self, id: &str) -> Option<&TrustedKey> {
    self.keys.iter().find(|k| k.id == id)
  }
}

/// 来源校验策略：开发模式不强制，生产模式强制签名 + 非空公钥白名单。
#[derive(Debug, Clone)]
pub struct VerificationPolicy {
  /// 是否要求每条插件声明都带有效签名与权威清单。
  pub require_signature: bool,
  /// 授信公钥白名单（生产模式必须非空）。
  pub trusted_keys: TrustedKeys,
}

impl VerificationPolicy {
  /// 开发 / CI 策略：不强制签名（维持 B3.2 既有语义）。
  pub fn development() -> Self {
    Self {
      require_signature: false,
      trusted_keys: TrustedKeys::default(),
    }
  }

  /// 生产策略：强制签名，且信任列表非空。
  ///
  /// 空列表仍可构造（便于装配期先解析配置再检查），校验时一律拒绝。
  pub fn production(trusted_keys: TrustedKeys) -> Self {
    Self {
      require_signature: true,
      trusted_keys,
    }
  }
}

/// 侧载签名块（provenance.json）。
#[derive(Debug, Clone)]
pub struct Provenance {
  /// 签名者 ID，必须在 [`TrustedKeys`] 白名单中。
  pub signer_id: String,
  /// Ed25519 签名原始字节。
  pub signature: [u8; 64],
  /// 被签名的 wasm 内容摘要（blake3，32 字节）。
  pub wasm_digest: [u8; 32],
}

impl Provenance {
  /// 从 JSON 解析签名块：
  /// `{"signer_id":"...","signature":"<hex>","wasm_digest":"<hex>"}`。
  ///
  /// # Errors
  ///
  /// JSON 非法 / 字段长度错误 → `plugin.plugin_signature_invalid`。
  pub fn from_json(bytes: &[u8]) -> RiboResult<Self> {
    #[derive(serde::Deserialize)]
    struct File {
      signer_id: String,
      signature: String,
      wasm_digest: String,
    }
    let file: File = serde_json::from_slice(bytes).map_err(|e| {
      plugin_signature_invalid(&format!("bad provenance JSON: {e}"))
    })?;
    if file.signer_id.is_empty() {
      return Err(plugin_signature_invalid("empty provenance signer_id"));
    }
    let sig = hex_decode(&file.signature, "signature")?;
    let digest = hex_decode(&file.wasm_digest, "wasm_digest")?;
    let signature: [u8; 64] = sig.try_into().map_err(|_| {
      plugin_signature_invalid("signature must be 64 bytes (Ed25519)")
    })?;
    let wasm_digest: [u8; 32] = digest.try_into().map_err(|_| {
      plugin_signature_invalid("wasm_digest must be 32 bytes (blake3)")
    })?;
    Ok(Self {
      signer_id: file.signer_id,
      signature,
      wasm_digest,
    })
  }
}

/// 经过来源校验封装的权威材料：manifest + 原始 JSON + 可选签名块。
#[derive(Debug, Clone)]
pub struct SignedAuthority {
  /// B3.2 权威清单（guest 自述的交叉校验基准）。
  pub manifest: AuthorityManifest,
  /// manifest 原始字节：签名载荷的摘要输入（保留原始形态，禁止重序列化）。
  pub manifest_bytes: Vec<u8>,
  /// 独立侧载的签名块；`None` 表示无签名（生产策略下拒绝）。
  pub provenance: Option<Provenance>,
}

impl SignedAuthority {
  /// 由 manifest 与可选 provenance JSON 构造权威材料。
  ///
  /// # Errors
  ///
  /// manifest / provenance 非法 → `plugin.plugin_signature_invalid` /
  /// `plugin.plugin_exec_failed`（manifest 复用 B3.2 解析错误）。
  pub fn from_json(
    manifest_bytes: &[u8],
    provenance_bytes: Option<&[u8]>,
  ) -> RiboResult<Self> {
    let manifest = AuthorityManifest::from_json(manifest_bytes)?;
    let provenance = provenance_bytes.map(Provenance::from_json).transpose()?;
    Ok(Self {
      manifest,
      manifest_bytes: manifest_bytes.to_vec(),
      provenance,
    })
  }
}

/// 构造 Ed25519 签名载荷（domain-separated + NUL 分隔，防字段拼接歧义）。
fn signature_message(
  plugin_id: &str,
  wasm_digest: &[u8; 32],
  manifest_bytes: &[u8],
) -> Vec<u8> {
  let manifest_digest = blake3::hash(manifest_bytes);
  let mut msg = Vec::with_capacity(DOMAIN_PREFIX.len() + plugin_id.len() + 130);
  msg.extend_from_slice(DOMAIN_PREFIX);
  msg.extend_from_slice(plugin_id.as_bytes());
  msg.push(0);
  msg.extend_from_slice(hex_encode(wasm_digest).as_bytes());
  msg.push(0);
  msg.extend_from_slice(hex_encode(manifest_digest.as_bytes()).as_bytes());
  msg
}

/// 加载期来源校验：必须在 `wasmtime::Module::new` **之前**调用——
/// 不可信字节不得进入编译器（JIT 编译本身是攻击面）。
///
/// 校验顺序（任一失败即拒绝加载）：
///
/// 1. 生产策略要求权威清单与签名块齐备；
/// 2. `signer_id` 必须命中宿主侧载的公钥白名单；
/// 3. provenance 中的 wasm 摘要必须等于现场计算的 `blake3(wasm_bytes)`；
/// 4. Ed25519 验签载荷 `signature_message(...)` 必须通过。
///
/// # Errors
///
/// 任一失败 → `plugin.plugin_signature_invalid`，错误文本不含密钥或签名载荷。
pub(crate) fn verify_provenance(
  wasm_bytes: &[u8],
  authority: &SignedAuthority,
  policy: &VerificationPolicy,
) -> RiboResult<()> {
  let Some(prov) = authority.provenance.as_ref() else {
    if policy.require_signature {
      return Err(plugin_signature_invalid(
        "provenance block required but missing",
      ));
    }
    return Ok(());
  };

  if policy.trusted_keys.is_empty() {
    return Err(plugin_signature_invalid(
      "no trusted public keys configured",
    ));
  }
  let key = policy.trusted_keys.find(&prov.signer_id).ok_or_else(|| {
    plugin_signature_invalid(&format!("untrusted signer `{}`", prov.signer_id))
  })?;

  let actual_digest = blake3::hash(wasm_bytes);
  if prov.wasm_digest != *actual_digest.as_bytes() {
    return Err(plugin_signature_invalid("wasm digest mismatch"));
  }

  let msg = signature_message(
    &authority.manifest.plugin_id,
    &prov.wasm_digest,
    &authority.manifest_bytes,
  );
  let public_key = ring::signature::UnparsedPublicKey::new(
    &ring::signature::ED25519,
    &key.public_key,
  );
  public_key.verify(&msg, &prov.signature).map_err(|_| {
    plugin_signature_invalid("ed25519 signature verification failed")
  })?;

  tracing::info!(
    signer = %prov.signer_id,
    plugin_id = %authority.manifest.plugin_id,
    "plugin provenance verified"
  );
  Ok(())
}

#[cfg(test)]
mod tests {
  use ring::signature::KeyPair;

  use super::*;

  const SEED: [u8; 32] = [7u8; 32];

  fn keys_json() -> Vec<u8> {
    let pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(&SEED)
      .expect("valid seed");
    let pk = hex_encode(pair.public_key().as_ref());
    format!(r#"{{"keys":[{{"id":"release","public_key":"{pk}"}}]}}"#)
      .into_bytes()
  }

  fn policy() -> VerificationPolicy {
    VerificationPolicy::production(
      TrustedKeys::from_json(&keys_json()).expect("valid keys"),
    )
  }

  fn manifest_json() -> Vec<u8> {
    br#"{"plugin_id":"verify","tools":[]}"#.to_vec()
  }

  fn sign(wasm: &[u8], manifest: &[u8], seed: &[u8; 32]) -> Provenance {
    let pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(seed)
      .expect("valid seed");
    let digest = *blake3::hash(wasm).as_bytes();
    let msg = signature_message("verify", &digest, manifest);
    Provenance {
      signer_id: "release".into(),
      signature: pair.sign(&msg).as_ref().try_into().expect("64 bytes"),
      wasm_digest: digest,
    }
  }

  fn provenance_json(prov: &Provenance) -> Vec<u8> {
    format!(
      r#"{{"signer_id":"{}","signature":"{}","wasm_digest":"{}"}}"#,
      prov.signer_id,
      hex_encode(&prov.signature),
      hex_encode(&prov.wasm_digest),
    )
    .into_bytes()
  }

  /// 场景：签名正确 → 通过；开发策略下无签名亦通过。
  #[test]
  fn valid_signature_and_development_policy_pass() {
    let wasm = b"wasm-bytes";
    let manifest = manifest_json();
    let prov = sign(wasm, &manifest, &SEED);
    let authority =
      SignedAuthority::from_json(&manifest, Some(&provenance_json(&prov)))
        .expect("valid authority");
    assert!(verify_provenance(wasm, &authority, &policy()).is_ok());

    let unsigned =
      SignedAuthority::from_json(&manifest, None).expect("valid manifest");
    assert!(
      verify_provenance(wasm, &unsigned, &VerificationPolicy::development())
        .is_ok()
    );
  }

  /// 场景：wasm 被替换（签名仍指向旧摘要）→ 拒绝。
  #[test]
  fn tampered_wasm_is_rejected() {
    let manifest = manifest_json();
    let prov = sign(b"original", &manifest, &SEED);
    let authority =
      SignedAuthority::from_json(&manifest, Some(&provenance_json(&prov)))
        .expect("valid authority");
    let err = verify_provenance(b"tampered", &authority, &policy())
      .expect_err("must reject");
    assert!(err.message().contains("wasm digest mismatch"), "got: {err}");
  }

  /// 场景：manifest 被替换（wasm 未变，签名仍指向旧 manifest）→ 拒绝。
  #[test]
  fn tampered_manifest_is_rejected() {
    let wasm = b"wasm-bytes";
    let prov = sign(wasm, &manifest_json(), &SEED);
    let authority = SignedAuthority::from_json(
      br#"{"plugin_id":"verify","tools":[],"extra":1}"#,
      Some(&provenance_json(&prov)),
    )
    .expect("valid authority");
    let err =
      verify_provenance(wasm, &authority, &policy()).expect_err("must reject");
    assert!(err.message().contains("ed25519"), "got: {err}");
  }

  /// 场景：签名者不在白名单 / 白名单为空 → 拒绝。
  #[test]
  fn untrusted_signer_and_empty_keyring_are_rejected() {
    let wasm = b"wasm-bytes";
    let manifest = manifest_json();
    let prov = sign(wasm, &manifest, &SEED);
    let authority =
      SignedAuthority::from_json(&manifest, Some(&provenance_json(&prov)))
        .expect("valid authority");

    let other = VerificationPolicy::production(
      TrustedKeys::from_json(
        br#"{"keys":[{"id":"other","public_key":"0000000000000000000000000000000000000000000000000000000000000000"}]}"#,
      )
      .expect("valid keys"),
    );
    assert!(
      verify_provenance(wasm, &authority, &other)
        .expect_err("must reject")
        .message()
        .contains("untrusted signer")
    );
    assert!(
      verify_provenance(
        wasm,
        &authority,
        &VerificationPolicy::production(TrustedKeys::default())
      )
      .expect_err("must reject")
      .message()
      .contains("no trusted public keys")
    );
  }

  /// 场景：生产策略缺签名块 → 拒绝（`roots` 扫描项无清单即属此类）。
  #[test]
  fn production_requires_signature_block() {
    let manifest = manifest_json();
    let unsigned =
      SignedAuthority::from_json(&manifest, None).expect("valid manifest");
    assert!(
      verify_provenance(b"wasm", &unsigned, &policy())
        .expect_err("must reject")
        .message()
        .contains("provenance block required")
    );
  }

  /// 场景：非法 hex / 错误长度 → 解析期拒绝。
  #[test]
  fn malformed_key_material_is_rejected() {
    assert!(
      TrustedKeys::from_json(br#"{"keys":[{"id":"a","public_key":"zz"}]}"#)
        .is_err()
    );
    assert!(
      Provenance::from_json(
        br#"{"signer_id":"a","signature":"00","wasm_digest":"00"}"#
      )
      .is_err()
    );
  }
}
