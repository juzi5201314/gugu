//! 编译器存储类型：闭包环境的冻结布局生产者。
//!
//! 闭包环境与 spawn 参数包不是语言类型，但它们是真实分配出去的 managed 对象，必须有能被
//! `TypeUniverse` 解析、能被 GC 扫描的 descriptor。这里在实例闭合期间按 closure 定义与
//! **具体捕获类型列表**生成这些记录：它们与其它具体类型一起排序编号，冻结之后 LIR 只能
//! 按同一份键去查，不再自造 descriptor。
//!
//! 规范字节（GBC1 风格，小端定宽；经 `gugu-mono-v1` 域摘要，与语言类型同域）：
//!
//! ```text
//! u16 tag            = STORAGE_TAG (0x8000，与语言类型 tag 空间不重叠)
//! u8  role           = StorageRole
//! u8  representation = StorageRepresentation
//! u64 target_len; [u8] target     角色目标（定义稳定键 / 实例身份）
//! u64 size
//! u64 align
//! u64 field_count
//! [u64 offset, u8[32] key] * field_count   按 offset 升序
//! ```
//!
//! 目标只写稳定身份，不写宿主地址；字段列表保证「同一 closure 的不同具体实例只要有不同
//! 的捕获类型，就得到不同的 TypeId」，因此泛型闭包不会仅按 `DefId` 复用。
//!
//! capture cell 与 async 结果不单独造记录：它们的 payload 就是被捕获类型 `T` / async 结果
//! 类型 `T` 本身，逃逸分析与 placement 已经把这类局部物化成 `T` 的 managed box，再为同一
//! 布局发一条重复 descriptor 只会在类型表里制造两个身份。角色的区分来自分配点（placement、
//! completion record 的 descriptor 字段），不来自复制出的类型。

use super::instantiate::WalkEntry;
use super::keys::{MonoContext, StableTypeKey};
use crate::Diagnostic;
use crate::gir::passing::PassingClass;
use crate::hash::hash_domain;
use crate::hir;
use crate::late::universe::{MetadataShape, Shape, TypeRecord};
use std::collections::BTreeMap;

/// compiler-storage 类型在 GBC1 里的枚举 tag；语言类型 tag 目前只用到 1..=30。
pub const STORAGE_TAG: u16 = 0x8000;

/// spawn 参数包的字节数：9 个整数车道（含 code）与 8 个 16 字节浮点车道。
pub const SPAWN_PACK_BYTES: u64 = 200;
/// spawn 参数包的对齐：浮点车道需要 16 字节对齐。
pub const SPAWN_PACK_ALIGN: u64 = 16;

/// 存储类型角色；进入规范字节。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum StorageRole {
    /// 闭包环境：捕获槽按偏移升序排布。
    Environment = 1,
    /// spawn 参数包：调用者按内部 ABI 车道填好的原始缓冲。
    SpawnPack = 2,
}

impl StorageRole {
    fn name(self) -> &'static str {
        match self {
            Self::Environment => "closure-environment",
            Self::SpawnPack => "spawn-pack",
        }
    }
}

/// 环境的表示策略；进入规范字节，避免不同表示共享同一 TypeId。
///
/// 三种表示的槽宽都是 8 字节，区别在槽里是什么：direct 是全指针、compressed 是 cage 压缩字、
/// shared-handle 是 stable handle。GC 在扫描时按对象头的 representation 决定解码路径，因此
/// 这里只需要让身份不同，不需要为它们各写一份 trace 语义。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum StorageRepresentation {
    Direct = 0,
    Compressed = 1,
    SharedHandle = 2,
}

impl StorageRepresentation {
    /// spawn 参数包只有一种表示：原始 ABI 车道。
    const RAW: Self = Self::Direct;
}

/// 一个存储类型的完整规范输入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageShape {
    pub role: StorageRole,
    pub representation: StorageRepresentation,
    pub target: Vec<u8>,
    pub size: u64,
    pub align: u64,
    /// `(offset, key)`，按 offset 升序。
    pub fields: Vec<(u64, StableTypeKey)>,
}

impl StorageShape {
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.fields.len() * 40 + self.target.len());
        out.extend_from_slice(&STORAGE_TAG.to_le_bytes());
        out.push(self.role as u8);
        out.push(self.representation as u8);
        out.extend_from_slice(&(self.target.len() as u64).to_le_bytes());
        out.extend_from_slice(&self.target);
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(&self.align.to_le_bytes());
        out.extend_from_slice(&(self.fields.len() as u64).to_le_bytes());
        for (offset, key) in &self.fields {
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(key);
        }
        out
    }

    /// 规范字节的 `gugu-mono-v1` 域摘要；与语言类型共用同一身份空间。
    pub fn key(&self) -> StableTypeKey {
        hash_domain("gugu-mono-v1", &self.canonical_bytes())
    }
}

/// 闭包环境的规范输入。
///
/// 槽宽恒为 8 字节：direct 与 compressed 环境各存一个 capture cell 地址 / 压缩字，
/// shared 环境存 stable handle。捕获顺序就是 HIR owner 捕获表的顺序，两侧都不得重排。
pub fn environment_shape(
    definition: StableTypeKey,
    captures: &[StableTypeKey],
    representation: StorageRepresentation,
) -> StorageShape {
    StorageShape {
        role: StorageRole::Environment,
        representation,
        target: definition.to_vec(),
        size: captures.len() as u64 * 8,
        align: 8,
        fields: captures
            .iter()
            .enumerate()
            .map(|(index, key)| (index as u64 * 8, *key))
            .collect(),
    }
}

/// spawn 参数包的规范输入；pack 是原始 ABI 车道缓冲，managed 车道由子协程入口栈图描述。
pub fn spawn_pack_shape(definition: StableTypeKey) -> StorageShape {
    StorageShape {
        role: StorageRole::SpawnPack,
        representation: StorageRepresentation::RAW,
        target: definition.to_vec(),
        size: SPAWN_PACK_BYTES,
        align: SPAWN_PACK_ALIGN,
        fields: Vec::new(),
    }
}

/// 从规范输入生成冻结类型记录。
///
/// 环境字段是 capture cell 的**地址**（shared 表示下是 stable handle），所以 direct 与
/// compressed 两种表示的每个槽都是一个 managed 直接指针字，必须按 `Direct(slot)` 扫描。
/// 这里**不能**用 `Aggregate` 把槽声明成内联的被捕获类型 `T`：那会让 trace 递归展开 `T` 的
/// 布局（捕获 `[int; 512]` 时展开出 512 个标量，一个受管字都不登记），于是捕获的受管对象
/// 在 nursery 重置后被丢弃。shared 表示的槽是身份而不是地址，扫描由 descriptor 驱动的
/// handle visitor 负责，这里显式声明「没有 direct managed pointer」。
pub fn record_of(shape: &StorageShape) -> TypeRecord {
    let direct = shape.representation != StorageRepresentation::SharedHandle;
    let metadata = if shape.fields.is_empty() || !direct {
        MetadataShape::None
    } else {
        // 槽宽恒为 8 字节且从字 0 起连续：`environment_shape` 按捕获下标排布，
        // 因此一段 `Pointers { count }` 就精确等于全部槽。
        for (index, (offset, _)) in shape.fields.iter().enumerate() {
            debug_assert_eq!(
                *offset,
                u64::try_from(index).expect("捕获下标适配 u64") * 8,
                "环境捕获槽必须从字 0 起按 8 字节连续排布"
            );
        }
        MetadataShape::Pointers {
            count: u64::try_from(shape.fields.len()).expect("捕获数量适配 u64"),
        }
    };
    let mut children = shape.fields.iter().map(|(_, key)| *key).collect::<Vec<_>>();
    children.sort_unstable();
    children.dedup();
    TypeRecord {
        key: shape.key(),
        canonical: shape.canonical_bytes(),
        name: format!(
            "<{}:{:02x}{:02x}{:02x}{:02x}>",
            shape.role.name(),
            shape.target.first().copied().unwrap_or(0),
            shape.target.get(1).copied().unwrap_or(0),
            shape.target.get(2).copied().unwrap_or(0),
            shape.target.get(3).copied().unwrap_or(0),
        ),
        layout: Some((shape.size, shape.align)),
        children,
        shape: Shape::Other,
        metadata,
        passing: PassingClass::IDENTITY.bits(),
    }
}

/// 收集本实例创建的闭包环境与 spawn 参数包类型。
///
/// 捕获类型在**创建闭包的 body**里解析：被捕获槽属于外层 owner，只有它的实例绑定能把
/// 类型参数具体化。收集顺序与 LIR 侧一致，都是 HIR owner 捕获表的顺序。
pub fn collect(
    context: &MonoContext<'_>,
    entry: &WalkEntry,
) -> Result<Vec<TypeRecord>, Diagnostic> {
    let Some(body) = entry.body else {
        return Ok(Vec::new());
    };
    let owner = &context.module.owners[body];
    let mut shapes = BTreeMap::<StableTypeKey, StorageShape>::new();
    for expression in &owner.expressions {
        let definition = match &expression.kind {
            hir::ExprKind::Closure { definition } | hir::ExprKind::Spawn { definition } => {
                *definition
            }
            // `spawn f(args)`：参数包按被 spawn 函数的定义身份登记一份原始车道缓冲。
            hir::ExprKind::SpawnCall {
                target: hir::CallTarget::Dispatch(dispatch),
                ..
            } => {
                let Some(dispatch) = owner
                    .dispatches
                    .get(usize::try_from(*dispatch).expect("dispatch 编号不超过 u32"))
                else {
                    continue;
                };
                let Some(definition) = dispatch.function else {
                    continue;
                };
                let shape = spawn_pack_shape(context.definition_key(definition));
                shapes.insert(shape.key(), shape);
                continue;
            }
            hir::ExprKind::SpawnCall { .. } => continue,
            _ => continue,
        };
        let Some(closure) = context
            .module
            .owners
            .iter()
            .find(|candidate| candidate.definition == definition)
        else {
            continue;
        };
        if closure.captures.is_empty() {
            continue;
        }
        let mut captures = Vec::with_capacity(closure.captures.len());
        for capture in &closure.captures {
            captures.push(capture_type(context, entry, owner, capture)?);
        }
        for representation in [
            StorageRepresentation::SharedHandle,
            StorageRepresentation::Compressed,
            StorageRepresentation::Direct,
        ] {
            let shape = environment_shape(
                context.definition_key(definition),
                &captures,
                representation,
            );
            shapes.insert(shape.key(), shape);
        }
    }
    Ok(shapes
        .into_values()
        .map(|shape| record_of(&shape))
        .collect())
}

/// 解析一条捕获在**外层实例**里的具体类型键。
fn capture_type(
    context: &MonoContext<'_>,
    entry: &WalkEntry,
    owner: &hir::Owner,
    capture: &hir::Capture,
) -> Result<StableTypeKey, Diagnostic> {
    let local = if capture.owner == entry.definition {
        capture.source
    } else {
        owner
            .captures
            .iter()
            .find(|outer| outer.owner == capture.owner && outer.source == capture.source)
            .ok_or_else(|| {
                crate::Diagnostic::error(
                    crate::DiagnosticCode::MonoDivergence,
                    "嵌套捕获缺少外层传递槽",
                    None,
                )
            })?
            .local
    };
    let ty = context.type_at(owner.locals[local.index()].ty, &entry.bindings)?;
    let ty = super::universe::concrete(context, &ty)?.into_owned();
    if !super::universe::is_concrete(&ty) {
        return Err(crate::Diagnostic::error(
            crate::DiagnosticCode::MonoDivergence,
            "捕获类型不是具体类型",
            None,
        ));
    }
    context.type_key(&ty)
}
