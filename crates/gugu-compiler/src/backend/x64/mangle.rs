//! 内部符号 mangling：同一 `CompilerIdentity` 镜像内使用 `__gugu_<kind>_<64 hex>`。
//!
//! C import/export 与 `Symbol::External` 使用源名、不加前缀。跨 fragment 的 veneer
//! 由镜像写出阶段处理，本模块不生成。

use crate::frontend::mono::keys::hash_domain;
use crate::lir::body::{Body, RuntimeCall, Symbol};

/// 内部函数符号：`__gugu_fn_` + `body.instance` 的 64 hex。
pub(crate) fn mangle_function(body: &Body) -> String {
    mangle("fn", &body.instance)
}

/// 内部符号：按 LIR `Symbol` 种类编码。
pub(crate) fn mangle_symbol(symbol: &Symbol) -> String {
    match symbol {
        Symbol::Instance(key) => mangle("fn", key),
        Symbol::External { name, .. } => name.clone(),
        Symbol::Global { key, .. } => mangle("static", key),
        Symbol::TypeDescriptor(key) | Symbol::TypeId(key) => mangle("const", key),
        Symbol::TypeRecords => mangle("const", &hash_domain("gugu-type-records-v1", b"records")),
        Symbol::TypeNames => mangle("const", &hash_domain("gugu-type-names-v1", b"names")),
        Symbol::Vtable {
            interface,
            concrete,
        } => mangle_vtable(*interface, *concrete),
        Symbol::Data(index) => mangle("const", &data_key(*index)),
    }
}

/// runtime glue：`__gugu_runtime_<name>` 的稳定哈希。
pub(crate) fn mangle_runtime(name: &str) -> String {
    mangle(
        "runtime",
        &hash_domain("gugu-runtime-symbol-v1", name.as_bytes()),
    )
}

/// `RuntimeCall` 判别名的稳定哈希。
pub(crate) fn mangle_runtime_call(call: RuntimeCall) -> String {
    mangle_runtime(runtime_call_name(call))
}

/// 编译器生成体（宽整除/memcpy 等）的稳定名哈希。
pub(crate) fn mangle_glue(name: &str) -> String {
    mangle("glue", &hash_domain("gugu-glue-symbol-v1", name.as_bytes()))
}

fn mangle_vtable(interface: [u8; 32], concrete: [u8; 32]) -> String {
    let mut bytes = [0_u8; 64];
    bytes[..32].copy_from_slice(&interface);
    bytes[32..].copy_from_slice(&concrete);
    let key = hash_domain("gugu-vtable-symbol-v1", &bytes);
    mangle("vtable", &key)
}

fn mangle(kind: &str, key: &[u8; 32]) -> String {
    let mut out = String::with_capacity(8 + kind.len() + 1 + 64);
    out.push_str("__gugu_");
    out.push_str(kind);
    out.push('_');
    for byte in key {
        let _ = core::fmt::Write::write_fmt(&mut out, format_args!("{byte:02x}"));
    }
    out
}

fn data_key(index: u32) -> [u8; 32] {
    hash_domain("gugu-const-data-v1", &index.to_le_bytes())
}

fn runtime_call_name(call: RuntimeCall) -> &'static str {
    match call {
        RuntimeCall::ValueCopy => "value_copy",
        RuntimeCall::ValuePublish => "value_publish",
        RuntimeCall::ValueDrop => "value_drop",
        RuntimeCall::ValueForget => "value_forget",
        RuntimeCall::CowSnapshot => "cow_snapshot",
        RuntimeCall::ValueTransfer => "value_transfer",
        RuntimeCall::ValueRepeat => "value_repeat",
        RuntimeCall::ResourceAcquire => "resource_acquire",
        RuntimeCall::ResourceRelease => "resource_release",
        RuntimeCall::ResourceTransfer => "resource_transfer",
        RuntimeCall::ResourceFinalize => "resource_finalize",
        RuntimeCall::Pin => "pin",
        RuntimeCall::Unpin => "unpin",
        RuntimeCall::DynamicErase => "dynamic_erase",
        RuntimeCall::TypeIs => "type_is",
        RuntimeCall::Downcast => "downcast",
        RuntimeCall::DowncastCopy => "downcast_copy",
        RuntimeCall::ChannelNew => "channel_new",
        RuntimeCall::ChannelClose => "channel_close",
        RuntimeCall::ChannelSend => "channel_send",
        RuntimeCall::ChannelReceive => "channel_receive",
        RuntimeCall::ChannelTrySend => "channel_try_send",
        RuntimeCall::ChannelTryRecv => "channel_try_recv",
        RuntimeCall::JoinWait => "join_wait",
        RuntimeCall::Yield => "yield",
        RuntimeCall::Spawn => "spawn",
        RuntimeCall::SelectCommit { .. } => "select_commit",
        RuntimeCall::Format => "format",
        RuntimeCall::Concat => "concat",
        RuntimeCall::Utf8Boundary => "utf8_boundary",
        RuntimeCall::Panic => "panic",
        RuntimeCall::DeferPush => "defer_push",
        RuntimeCall::DeferAction => "defer_action",
        RuntimeCall::DeferEnvironment => "defer_environment",
        RuntimeCall::DeferPop => "defer_pop",
        RuntimeCall::WideDiv { .. } => "wide_div",
    }
}
