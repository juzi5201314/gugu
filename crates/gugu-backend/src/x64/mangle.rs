//! 内部符号 mangling：同一 `CompilerIdentity` 镜像内使用 `__gugu_<kind>_<64 hex>`。
//!
//! C import/export 与 `Symbol::External` 使用源名、不加前缀。跨 fragment 的 veneer
//! 由镜像写出阶段处理，本模块不生成。

use crate::hash::hash_domain;
use crate::lir::body::{Body, RuntimeCall, Symbol};

/// 内部函数符号：`__gugu_fn_` + `body.instance` 的 64 hex。
pub fn mangle_function(body: &Body) -> String {
    mangle_instance(&body.instance)
}

/// 同一函数实例的规范符号；runtime 绑定改变导出名时仍须保留内部直接调用地址。
pub fn mangle_instance(instance: &[u8; 32]) -> String {
    mangle("fn", instance)
}

/// 内部符号：按 LIR `Symbol` 种类编码。
pub fn mangle_symbol(symbol: &Symbol) -> String {
    match symbol {
        Symbol::Instance(key) => mangle("fn", key),
        Symbol::FunctionAdapter {
            target,
            has_environment,
        } => mangle_instance(&crate::lir::function_adapter::key(target, *has_environment)),
        Symbol::External { name, .. } => name.clone(),
        Symbol::Global { key, .. } => mangle("static", key),
        Symbol::TypeDescriptor(key) => mangle("const", key),
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
pub fn mangle_runtime(name: &str) -> String {
    mangle(
        "runtime",
        &hash_domain("gugu-runtime-symbol-v1", name.as_bytes()),
    )
}

/// runtime glue 的完整 `Symbol`：key 与 mangled 名同源，symbol 去重不依赖二次哈希。
pub fn runtime_symbol(name: &str) -> Symbol {
    Symbol::External {
        key: hash_domain("gugu-runtime-symbol-v1", name.as_bytes()),
        name: mangle_runtime(name),
    }
}

/// `RuntimeCall` 的 runtime `Symbol`。
pub fn runtime_call_symbol(call: RuntimeCall) -> Symbol {
    runtime_symbol(runtime_call_name(call))
}

/// 编译器生成体（宽整除/memcpy 等）的稳定名哈希。
pub fn mangle_glue(name: &str) -> String {
    mangle("glue", &hash_domain("gugu-glue-symbol-v1", name.as_bytes()))
}

/// 编译器生成体的完整 `Symbol`；key 与 mangled 名同源。
pub fn glue_symbol(name: &str) -> Symbol {
    Symbol::External {
        key: hash_domain("gugu-glue-symbol-v1", name.as_bytes()),
        name: mangle_glue(name),
    }
}

fn mangle_vtable(interface: [u8; 32], concrete: [u8; 32]) -> String {
    let mut bytes = [0_u8; 64];
    bytes[..32].copy_from_slice(&interface);
    bytes[32..].copy_from_slice(&concrete);
    let key = hash_domain("gugu-vtable-symbol-v1", &bytes);
    mangle("vtable", &key)
}

fn mangle(kind: &str, key: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(7 + kind.len() + 1 + 64);
    out.push_str("__gugu_");
    out.push_str(kind);
    out.push('_');
    for byte in key {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 15)]));
    }
    out
}

fn data_key(index: u32) -> [u8; 32] {
    hash_domain("gugu-const-data-v1", &index.to_le_bytes())
}

/// 常量数据块的镜像符号名。下标是**全局**数据池下标（见 `codegen::DataPool`）。
pub fn const_symbol(index: u32) -> String {
    mangle("const", &data_key(index))
}

pub fn runtime_call_name(call: RuntimeCall) -> &'static str {
    match call {
        RuntimeCall::ValueCopy => "value_copy",
        RuntimeCall::ValuePublish => "value_publish",
        RuntimeCall::ValueDrop => "value_drop",
        RuntimeCall::ValueForget => "value_forget",
        RuntimeCall::CowSnapshot => "cow_snapshot",
        RuntimeCall::ValueTransfer => "value_transfer",
        RuntimeCall::ForeignCopy => "foreign_copy",
        RuntimeCall::ValueRepeat => "value_repeat",
        RuntimeCall::ResourceAcquire => "resource_acquire",
        RuntimeCall::ResourceRelease => "resource_release",
        RuntimeCall::ResourceTransfer => "resource_transfer",
        RuntimeCall::ResourceFinalize => "resource_finalize",
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
        RuntimeCall::JoinPanic => "join_panic",
        RuntimeCall::Yield => "yield",
        RuntimeCall::Spawn => "spawn",
        RuntimeCall::SelectCommit { .. } => "select_commit",
        RuntimeCall::Concat => "concat",
        RuntimeCall::StringCompare => "string_compare",
        RuntimeCall::Utf8Boundary => "utf8_boundary",
        RuntimeCall::Panic => "panic",
        RuntimeCall::DeferPush => "defer_push",
        RuntimeCall::DeferAction => "defer_action",
        RuntimeCall::DeferEnvironment => "defer_environment",
        RuntimeCall::DeferPop => "defer_pop",
        RuntimeCall::WideDiv { .. } => "wide_div",
        RuntimeCall::BlockingIo { .. } => "blocking_io",
        RuntimeCall::ArenaNew => "gc_arena_new",
        RuntimeCall::ArenaAlloc => "gc_arena_alloc",
        RuntimeCall::ArenaAllocSync => "gc_arena_alloc_sync",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn instance_symbols_preserve_leading_zeroes_and_both_hex_nibbles() {
        let key = std::array::from_fn(|index| [0x00, 0x0f, 0x10, 0xff][index % 4]);
        assert_eq!(
            super::mangle_instance(&key),
            concat!(
                "__gugu_fn_",
                "000f10ff000f10ff000f10ff000f10ff",
                "000f10ff000f10ff000f10ff000f10ff"
            )
        );
    }
}
