//! `resources/runtime/collect.gg` 的 coordinator 页布局：`gc_enter` 片段与 Gugu 执行体
//! 共用的唯一契约。
//!
//! 片段只能做固定偏移的读写，运行期只能按同一份字段表访问；两侧任何漂移都会让 `gc_enter`
//! 保存/恢复错误的槽位，表现为收集器切栈后 mutator 回不来。因此偏移在这里集中定义，并由
//! `collect.gg` 的源文本交叉校验钉住。

/// `gc_enter` 片段写发起者 RSP 的槽；dispatcher 读它定位发起者帧。
pub const COORDINATOR_ENTRY_RSP_OFFSET: i32 = 16;
/// `gc_enter` 保存并在 dispatcher 返回后恢复 mutator RSP 的槽。
pub const COORDINATOR_SAVED_RSP_OFFSET: i32 = 24;
/// runtime 专用 system stack 顶；`gc_enter` 把 RSP 换成它。
pub const COORDINATOR_SYSTEM_TOP_OFFSET: i32 = 32;
/// collector 专用 system-stack 描述符（`CoroutineSlot` 形状的 hot + stack descriptor）。
pub const COORDINATOR_SYSTEM_DESCRIPTOR_OFFSET: i32 = 128;
/// `gc_enter` 保存并在 dispatcher 返回后恢复的 mutator r14。
pub const COORDINATOR_SAVED_R14_OFFSET: i32 = 136;

/// 源文本交叉校验：`collect.gg` 的同名常量必须与片段使用的偏移逐项一致。
#[cfg(test)]
pub fn verify_source(source: &str) -> Result<(), String> {
    for (name, value) in [
        ("COORDINATOR_ENTRY_RSP_OFFSET", COORDINATOR_ENTRY_RSP_OFFSET),
        ("COORDINATOR_SAVED_RSP_OFFSET", COORDINATOR_SAVED_RSP_OFFSET),
        (
            "COORDINATOR_SYSTEM_TOP_OFFSET",
            COORDINATOR_SYSTEM_TOP_OFFSET,
        ),
        (
            "COORDINATOR_SYSTEM_DESCRIPTOR_OFFSET",
            COORDINATOR_SYSTEM_DESCRIPTOR_OFFSET,
        ),
        ("COORDINATOR_SAVED_R14_OFFSET", COORDINATOR_SAVED_R14_OFFSET),
    ] {
        let needle = format!("pub const {name}: uint = {value}");
        if !source.contains(&needle) {
            return Err(format!("collect.gg 缺少 `{needle}`"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// 片段与 Gugu 执行体必须读同一份 coordinator 布局。
    #[test]
    fn coordinator_offsets_match_the_mirror() {
        super::verify_source(include_str!(
            "../../gugu-frontend/resources/runtime/collect.gg"
        ))
        .expect("coordinator 布局与 collect.gg 不一致");
    }

    /// 分配侧发布请求用的处理器槽位必须与 `processor.rs` 一致。
    #[test]
    fn request_slot_matches_the_gc_source() {
        let slot = crate::processor::PROCESSOR_GC_REQUEST_OFFSET;
        let needle = format!("pub const PROCESSOR_GC_REQUEST_OFFSET: uint = {slot}");
        assert!(
            include_str!("../../gugu-frontend/resources/runtime/gc.gg").contains(&needle),
            "gc.gg 缺少 `{needle}`"
        );
    }
}
