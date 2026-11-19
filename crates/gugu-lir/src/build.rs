//! 具体 GIR 到 LIR；变量读取由 block 参数封口，不引入 phi 或平行语义 IR。
mod calls;
mod comparison;
pub(crate) mod foreign;
mod intrinsics;
mod memory;
mod numeric;
mod rvalue;
mod ssa;
mod storage;
mod values;

use super::body::{
    self, Block, BlockId, Body, Definition, Edge, EdgeId, InstId, Instruction, Memory, Op, Origin,
    Parameter, Safepoint, SafepointId, Signature, Terminator, Type, Value, ValueId, ValueType, id,
};
use super::invalid;
use crate::Diagnostic;
use crate::late::universe::MetadataShape;
use crate::{gir, hir, mono};
use gir::body::{GirBody, SourceInfo};
use gir::concrete::{ConcreteBody, TypeKind, TypeLayout};
use mono::collect::InstanceSummaryV1;
use std::ops::Range;

/// 逐 body 查询用的 LIR 输入索引：按 LIR 输入建一次，`lower` 的每个 body 复用。
///
/// `lower` 要按 mono 摘要找到实例，`environment_observation` 要按闭包定义找到全部创建点。
/// 两者逐 body 线性扫描会把整体放大成 O(body × 实例) 次摘要计算与 O(body × 语句) 次扫描，
/// 那是本次编译最热的散列与扫描来源，因此改为一次建稠密索引、逐 body 点查。
pub struct LirIndex {
    /// `(mono 键摘要, mono.instances 下标)`，按摘要有序，供二分点查。
    instances: Vec<([u8; 32], u32)>,
    /// 按 `hir::DefId` 下标索引的闭包创建点 `(具体实例下标, 语句下标)`；保持输入顺序。
    closure_sites: Vec<Vec<(u32, u32)>>,
}

impl LirIndex {
    /// 扫描 mono 实例表与全部具体 GIR body 建立索引。
    pub fn new(mono: &mono::MonoWorldV1, concrete: &[ConcreteBody]) -> Self {
        let mut instances: Vec<([u8; 32], u32)> = mono
            .instances
            .iter()
            .enumerate()
            .map(|(index, instance)| {
                (
                    mono::digest_of(&instance.mono_key),
                    u32::try_from(index).expect("实例数量适配 u32"),
                )
            })
            .collect();
        instances.sort_unstable_by_key(|(digest, _)| *digest);
        let mut closure_sites: Vec<Vec<(u32, u32)>> = Vec::new();
        for (index, body) in concrete.iter().enumerate() {
            let concrete_index = u32::try_from(index).expect("具体实例数量适配 u32");
            for (statement, value) in body.body.statements.iter().enumerate() {
                let gir::body::StatementKind::Assign(
                    _,
                    gir::body::Rvalue::Aggregate {
                        kind: gir::body::AggregateKind::Closure(definition),
                        ..
                    },
                ) = &value.kind
                else {
                    continue;
                };
                let definition = definition.index();
                if closure_sites.len() <= definition {
                    closure_sites.resize(definition + 1, Vec::new());
                }
                closure_sites[definition].push((
                    concrete_index,
                    u32::try_from(statement).expect("语句下标适配 u32"),
                ));
            }
        }
        Self {
            instances,
            closure_sites,
        }
    }

    /// 返回摘要对应的 `mono.instances` 下标。
    pub fn instance(&self, digest: &[u8; 32]) -> Option<usize> {
        let position = self
            .instances
            .binary_search_by_key(digest, |(digest, _)| *digest)
            .ok()?;
        Some(usize::try_from(self.instances[position].1).expect("实例下标适配宿主"))
    }

    /// 返回闭包定义的全部创建点；定义没有创建点时返回空切片。
    pub fn closure_sites(&self, definition: hir::DefId) -> &[(u32, u32)] {
        self.closure_sites
            .get(definition.index())
            .map_or(&[], Vec::as_slice)
    }
}

/// 按 `(body, statement)` 在有序分配表里点查放置车道；未登记按 `LocalHeap`。
///
/// 同一键可能有多条记录，取最左侧一条，与线性查找的首次命中一致。
fn placement_kind(
    allocs: &[gir::placement::AllocPlacement],
    body: u32,
    statement: u32,
) -> gir::placement::PlacementKind {
    let Ok(found) =
        allocs.binary_search_by_key(&(body, statement), |alloc| (alloc.body, alloc.statement))
    else {
        return gir::placement::PlacementKind::LocalHeap;
    };
    let mut first = found;
    while first > 0 && allocs[first - 1].body == body && allocs[first - 1].statement == statement {
        first -= 1;
    }
    allocs[first].kind
}

#[derive(Clone, Copy)]
struct Variable {
    source: (u32, u64),
    kind: ValueType,
}

#[derive(Clone)]
enum Storage {
    Values(Range<u32>),
    Stack {
        slot: body::SlotId,
        address: ValueId,
    },
    Heap {
        variable: u32,
        placement: gir::placement::PlacementKind,
    },
    Capture {
        address: ValueId,
    },
}

struct PendingBlock {
    parameters: Vec<(Option<u32>, Parameter)>,
    instructions: Vec<InstId>,
    definitions: Vec<Option<ValueId>>,
    memory: ValueId,
    terminator: Option<Terminator>,
    source: SourceInfo,
    cleanup: bool,
}

struct Builder<'a> {
    concrete: &'a ConcreteBody,
    gir: &'a GirBody,
    module: &'a hir::Module,
    sources: &'a crate::SourceMap,
    world: &'a gir::GirWorldV1,
    mono: &'a mono::MonoWorldV1,
    /// LIR 输入级索引：实例摘要与闭包创建点都按它点查，不再逐 body 线性扫描世界。
    index: &'a LirIndex,
    instance: &'a InstanceSummaryV1,
    owner: &'a hir::Owner,
    /// cage profile 开关；关闭态所有 lowering 路径与历史行为逐指令一致。
    compression: crate::runtime::CompressionPolicyV1,
    body: Body,
    blocks: Vec<PendingBlock>,
    block_map: Vec<Option<BlockId>>,
    variables: Vec<Variable>,
    storage: Vec<Storage>,
    entry_arguments: Vec<Option<Vec<ValueId>>>,
    current: BlockId,
    source: SourceInfo,
    sret: Option<ValueId>,
    environment: Option<ValueId>,
    next_allocation: u32,
    fixed_edges: Vec<(EdgeId, ValueId, ValueId)>,
    /// 正在 lowering 的 generic GIR 语句下标；placement 分配点表按它点查。
    statement: u32,
    /// 每个 block 的 region 结束动作；由 `EscapeAndPlacement` 的 region 计划静态决定。
    region_ends: Vec<Vec<RegionEnd>>,
    /// 单调递增的 shared access token；每个 body 内唯一且非零。
    next_shared_token: u32,
    /// 共享环境捕获源槽的提升 handle 变量；按 GIR local 索引。
    ///
    /// 提升发生在闭包构造点，而变量必须建 block 之前登记，因此这里在 `prepare_storage`
    /// 就为「被共享环境捕获的源槽」备好 handle 变量。
    shared_promotions: Vec<Option<u32>>,
    /// 当前打开的 shared access guard 栈；栈顶是最近一次 `SharedAccessBegin` 的 token。
    ///
    /// 共享 place 的字段访问必须在这个 guard 内发生：`address`/`read_place`/`write_place`
    /// 用它决定 `SharedFieldBarrier` 关联的 token，并阻止 guard 内的派生地址泄漏到 guard 外。
    shared_guards: Vec<(u32, ValueId, ValueId)>,
}

/// 一个边界 block 上的 region 结束动作。
///
/// 每条到达该边界的路径都先登记 export summary，再选择「重置」「保留（promote）」或「移交给
/// 接收 owner」。三者互斥，`RegionExit.transfer` 由 placement 决定。
#[derive(Clone, Copy, Debug)]
struct RegionEnd {
    region: u32,
    export: u8,
    kind: RegionEndKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegionEndKind {
    Reset,
    Promote,
    Transfer,
}

#[expect(
    clippy::too_many_arguments,
    reason = "lower 需要具体实例、冻结 HIR/GIR/mono、输入索引、目标与压缩策略"
)]
pub fn lower(
    concrete: &ConcreteBody,
    module: &hir::Module,
    world: &gir::GirWorldV1,
    mono: &mono::MonoWorldV1,
    index: &LirIndex,
    target: &str,
    compression: crate::runtime::CompressionPolicyV1,
    sources: &crate::SourceMap,
) -> Result<Body, Diagnostic> {
    concrete.verify()?;
    let instance = index
        .instance(&concrete.instance)
        .map(|position| &mono.instances[position])
        .ok_or_else(|| invalid("LIR 输入不在闭合实例图中"))?;
    let owner = module
        .owners
        .iter()
        .find(|owner| owner.definition == concrete.body.owner)
        .ok_or_else(|| invalid("具体 GIR 缺少冻结 owner"))?;
    let source = concrete.body.blocks[concrete.body.entry.index()]
        .source
        .clone();
    let mut builder = Builder {
        concrete,
        gir: &concrete.body,
        module,
        sources,
        world,
        mono,
        index,
        instance,
        owner,
        compression,
        body: Body {
            revision: body::REVISION,
            instance: concrete.instance,
            owner: concrete.body.owner_key,
            name: instance.symbol.clone(),
            target: target.to_owned(),
            signature: Signature {
                parameters: Vec::new(),
                results: Vec::new(),
                sret: None,
                by_value: Vec::new(),
            },
            values: Vec::new(),
            blocks: Vec::new(),
            instructions: Vec::new(),
            operands: Vec::new(),
            parameters: Vec::new(),
            edges: Vec::new(),
            predecessors: Vec::new(),
            switch_cases: Vec::new(),
            stack_slots: Vec::new(),
            lifetimes: Vec::new(),
            root_inits: Vec::new(),
            safepoints: Vec::new(),
            barrier_permits: Vec::new(),
            no_safepoint_regions: concrete.body.no_safepoint_regions.clone(),
            source_scopes: concrete.body.source_scopes.clone(),
            uses: Vec::new(),
            data: Vec::new(),
            assembly: owner.assembly.clone(),
            environments: Vec::new(),
            entry: BlockId(0),
            poll_summary: body::PollSummary::default(),
            poll_interface: body::PollInterface::default(),
        },
        blocks: Vec::new(),
        block_map: vec![None; concrete.body.blocks.len()],
        variables: Vec::new(),
        storage: Vec::new(),
        entry_arguments: vec![None; concrete.body.locals.len()],
        current: BlockId(0),
        source,
        sret: None,
        environment: None,
        next_allocation: 0,
        fixed_edges: Vec::new(),
        statement: 0,
        region_ends: region_ends(world, concrete.generic_body, concrete.body.blocks.len()),
        next_shared_token: 0,
        shared_promotions: vec![None; concrete.body.locals.len()],
        shared_guards: Vec::new(),
    };
    builder.prepare_storage()?;
    builder.prepare_blocks();
    builder.prepare_entry()?;
    for original in 0..builder.gir.blocks.len() {
        let Some(block) = builder.block_map[original] else {
            continue;
        };
        builder.current = block;
        let input = &builder.gir.blocks[original];
        builder.source = input.source.clone();
        let start = builder.gir.blocks[original].statements.start;
        for (offset, statement) in builder
            .gir
            .block_statements(gir::body::BlockId(id(original)))
            .iter()
            .enumerate()
        {
            builder.statement = start + offset as u32;
            builder.source = statement.source.clone();
            builder.statement(&statement.kind)?;
        }
        builder.source = input.source.clone();
        builder.emit_region_ends(original as u32)?;
        builder.terminator(&input.terminator)?;
    }
    builder.finish()
}

/// 按 block 汇总 placement 给出的 region 结束动作。
///
/// 动作来自静态计划而不是 lowering 期间的动态状态：boundary block 在 CFG 里可能早于它的支配
/// 分配块被 lowering（回边），只有静态表才能保证每条路径上都发出恰好一次结束动作。
fn region_ends(world: &gir::GirWorldV1, body: u32, blocks: usize) -> Vec<Vec<RegionEnd>> {
    let mut ends = vec![Vec::new(); blocks];
    for plan in world
        .placement
        .regions
        .iter()
        .filter(|plan| plan.body == body)
    {
        for exit in &plan.exits {
            let Some(slot) = ends.get_mut(exit.block as usize) else {
                continue;
            };
            let kind = if exit.transfer {
                RegionEndKind::Transfer
            } else if plan.export == 0 {
                RegionEndKind::Reset
            } else {
                RegionEndKind::Promote
            };
            slot.push(RegionEnd {
                region: plan.region,
                export: plan.export,
                kind,
            });
        }
    }
    for slot in ends.iter_mut() {
        slot.sort_by_key(|end| end.region);
    }
    ends
}

impl Builder<'_> {
    /// 在边界 block 的 terminator 之前发出 region 结束动作。
    fn emit_region_ends(&mut self, block: u32) -> Result<(), Diagnostic> {
        let Some(ends) = self.region_ends.get(block as usize).cloned() else {
            return Ok(());
        };
        for end in ends {
            let publish = self.emit(
                Op::RegionPublish {
                    region: end.region,
                    export: end.export,
                },
                &[],
                &[],
            );
            debug_assert!(publish.is_empty());
            let op = match end.kind {
                RegionEndKind::Reset => Op::RegionReset { region: end.region },
                RegionEndKind::Promote => Op::PromoteManaged { region: end.region },
                RegionEndKind::Transfer => Op::RegionTransfer { region: end.region },
            };
            self.emit(op, &[], &[]);
        }
        Ok(())
    }

    fn layout(&self, ty: u32) -> &TypeLayout {
        &self.concrete.types[usize::try_from(ty).expect("类型编号适配宿主")]
    }

    /// 类型是否有 GC 可见字段：只有完全没有受管指针的值才允许按字节写进静态槽。
    ///
    /// 递归下钻 metadata 的子类型，函数指针/raw 指针这类没有受管字段的类型不算受管；
    /// 深度超限或键不在冻结表时保守按受管处理。
    pub(super) fn has_managed_fields(&self, ty: u32) -> bool {
        let key = self.layout(ty).key;
        self.metadata_is_managed(&key, 0)
    }

    fn metadata_is_managed(&self, key: &[u8; 32], depth: usize) -> bool {
        if depth > 16 {
            return true;
        }
        // 冻结表是 GC 元数据的唯一来源：表里没有记录的类型没有被追踪的指针，按位复制即
        // 完整语义（函数指针、raw 指针都属此类）。
        let Ok(record) = self.mono.universe.record(key) else {
            return false;
        };
        match &record.metadata {
            MetadataShape::None => false,
            // 引用/切片视图本身就是 GC 可见指针，与 pointee 有无字段无关。
            MetadataShape::String | MetadataShape::Direct(_) | MetadataShape::Interior(_) => true,
            // 每个槽都是一个 managed 直接指针字。
            MetadataShape::Pointers { count } => *count > 0,
            MetadataShape::Array { element, .. } => self.metadata_is_managed(element, depth + 1),
            MetadataShape::Aggregate { variants, .. } => variants
                .iter()
                .flatten()
                .any(|(child, _)| self.metadata_is_managed(child, depth + 1)),
        }
    }

    /// 判断一个 local 的 storage 是否是 SharedHeap stable handle。
    ///
    /// handle 不是地址：它的字段访问必须经过 access guard，`address`/`read_place`/`write_place`
    /// 因此走共享路径而不是 direct pointer 路径。
    pub(super) fn is_shared(&self, local: gir::body::LocalId) -> bool {
        matches!(
            self.storage[local.index()],
            Storage::Heap {
                placement: gir::placement::PlacementKind::SharedHeap,
                ..
            }
        )
    }

    /// 返回该 local 的 SharedHeap handle 值。
    pub(super) fn shared_handle(
        &mut self,
        local: gir::body::LocalId,
    ) -> Result<ValueId, Diagnostic> {
        let Storage::Heap { variable, .. } = self.storage[local.index()] else {
            return Err(invalid("shared handle 需要 Heap storage"));
        };
        self.read(variable)
    }

    /// 打开 shared access guard，返回只在 guard 内有效的 payload。
    pub(super) fn begin_shared(&mut self, handle: ValueId) -> ValueId {
        self.next_shared_token += 1;
        let token = self.next_shared_token;
        let payload = self.emit_one(
            Op::SharedAccessBegin { token },
            &[handle],
            ValueType::pointer(body::Provenance::GcHeap),
            Origin::Derived(handle),
        );
        self.shared_guards.push((token, handle, payload));
        payload
    }

    /// 关闭最近打开的 shared access guard。
    pub(super) fn end_shared(&mut self) -> Result<(), Diagnostic> {
        let (token, handle, _) = self
            .shared_guards
            .pop()
            .ok_or_else(|| invalid("shared access guard 栈为空"))?;
        self.emit(Op::SharedAccessEnd { token }, &[handle], &[]);
        Ok(())
    }

    /// 返回当前打开的 shared access guard 的 token。
    pub(super) fn active_shared_token(&self) -> Option<u32> {
        self.shared_guards.last().map(|entry| entry.0)
    }

    /// 世界扫描：按闭包定义逐实例点查环境分配点，观察环境车道与压缩可行性。
    ///
    /// 环境分配点就是闭包字面量所在的语句，因此按「闭包定义 → (body, statement) → 分配
    /// 记录」唯一点查。同一闭包本体出现互相冲突的表示时必须拒绝：一个闭包定义只有一条
    /// 环境车道，不允许在同一个定义里混用 handle 与 direct pointer。
    ///
    /// 压缩可行性与车道观察同源：cage 开启、所有创建点都落在 LocalHeap、每个捕获源
    /// cell 都能驻留 cage（非 pinned/shared 车道、footprint 不超过单个 block），且环境
    /// 对象自身不超过单个 block。
    fn environment_observation(
        &self,
        definition: hir::DefId,
    ) -> Result<(Option<gir::placement::PlacementKind>, bool), Diagnostic> {
        let Some(owner) = self
            .module
            .owners
            .iter()
            .find(|owner| owner.definition == definition)
        else {
            return Ok((None, false));
        };
        let mut observed: Option<gir::placement::PlacementKind> = None;
        let mut compressible = self.cage_enabled() && !owner.captures.is_empty();
        // 环境对象自身 footprint 超过 block 时 runtime 写 LOCAL_DIRECT | LARGE_OBJECT；
        // header 与字段表示必须同源，因此这样的环境整个定义退回 full-pointer 槽。
        let environment_bytes = u64::try_from(owner.captures.len()).expect("捕获数量适配 u64") * 8;
        compressible &= crate::runtime::local_heap::OBJECT_HEADER_BYTES
            + environment_bytes
            + u64::from(crate::runtime::local_heap_schema::HEAP_GRANULE_BYTES)
            - 1
            <= u64::from(crate::runtime::gc_metadata_contract::GC_BLOCK_BYTES);
        for &(concrete_index, statement) in self.index.closure_sites(definition) {
            let concrete = &self.world.concrete
                [usize::try_from(concrete_index).expect("具体实例下标适配宿主")];
            let kind = placement_kind(
                &self.world.placement.allocs,
                concrete.generic_body,
                statement,
            );
            if let Some(observed) = observed
                && observed != kind
            {
                return Err(invalid("同一闭包本体不能同时由 shared 与 local 环境实例化"));
            }
            observed = Some(kind);
            compressible &= kind == gir::placement::PlacementKind::LocalHeap;
            compressible &= self.capture_cells_compressible(owner);
        }
        Ok((observed, compressible))
    }

    /// 检查闭包定义的全部捕获源 cell 是否能驻留 cage。
    ///
    /// capture 槽保存被捕获变量的 cell 地址；cell 落在 pinned、shared 车道或 footprint
    /// 超过单个 block（large slow path）时压缩字无法表达该地址。泛型实例之间 cell 布局
    /// 可以不同，因此每个拥有源槽的实例都要可压缩。
    fn capture_cells_compressible(&self, owner: &hir::Owner) -> bool {
        let block_bytes = u64::from(crate::runtime::gc_metadata_contract::GC_BLOCK_BYTES);
        owner.captures.iter().all(|capture| {
            // 源槽由 capture.owner 的 body 分配 cell；placement 记录按 generic body 索引。
            let generic_index = self
                .world
                .bodies
                .iter()
                .position(|body| body.owner == capture.owner);
            let placement_ok = generic_index.is_some_and(|index| {
                let Some(local) = self.world.bodies[index]
                    .locals
                    .iter()
                    .position(|local| local.hir_local == Some(capture.source))
                else {
                    return false;
                };
                let placement = self
                    .world
                    .placement
                    .records
                    .iter()
                    .find(|record| {
                        record.body == u32::try_from(index).expect("body 下标适配 u32")
                            && record.local == u32::try_from(local).expect("local 编号")
                    })
                    .map_or(gir::placement::PlacementKind::LocalHeap, |record| {
                        record.kind
                    });
                matches!(
                    placement,
                    gir::placement::PlacementKind::Stack | gir::placement::PlacementKind::LocalHeap
                )
            });
            if !placement_ok {
                return false;
            }
            // footprint 按 concrete 实例的布局核对：同一源槽在不同实例里可以尺寸不同。
            self.world
                .concrete
                .iter()
                .filter(|instance| instance.body.owner == capture.owner)
                .all(|instance| {
                    let Some(local) = instance
                        .body
                        .locals
                        .iter()
                        .position(|local| local.hir_local == Some(capture.source))
                    else {
                        return false;
                    };
                    let ty = instance.body.locals[local].ty.0 as usize;
                    let Some(layout) = instance.types.get(ty).and_then(|ty| ty.layout) else {
                        return false;
                    };
                    layout.align <= 256
                        && crate::runtime::local_heap::OBJECT_HEADER_BYTES
                            + layout
                                .align
                                .max(crate::runtime::local_heap_schema::HEAP_GRANULE_BYTES.into())
                            - 1
                            + layout.size
                            <= block_bytes
                })
        })
    }

    pub(super) fn cage_enabled(&self) -> bool {
        self.compression.enabled
    }

    /// 闭包环境是否以压缩字保存 capture 槽。
    ///
    /// 判定与 [`Self::shared_environment`] 同源：环境车道必须是 LocalHeap
    /// （Shared、TurnRegion、Pinned 车道都不压缩），且每个创建点的捕获源 cell 都能
    /// 驻留 cage。placement 按闭包定义单态，两侧（构造与实例）使用同一答案。
    pub(super) fn compressed_environment(
        &self,
        definition: hir::DefId,
    ) -> Result<bool, Diagnostic> {
        let (observed, compressible) = self.environment_observation(definition)?;
        Ok(compressible && observed == Some(gir::placement::PlacementKind::LocalHeap))
    }

    /// 闭包环境的表示：由 placement 的分配点表决定，`SharedHeap` 环境以 stable handle 传递。
    ///
    /// 环境分配点就是闭包字面量所在的语句，因此按「闭包定义 → (body, statement) → 分配记录」
    /// 唯一点查。同一闭包本体出现互相冲突的表示时必须拒绝：一个 LIR body 只有一条环境车道，
    /// 不允许在同一个 body 里混用 handle 与 direct pointer。
    pub(super) fn shared_environment(&self, definition: hir::DefId) -> Result<bool, Diagnostic> {
        let (observed, _) = self.environment_observation(definition)?;
        Ok(observed == Some(gir::placement::PlacementKind::SharedHeap))
    }

    /// 非失败版本的闭包环境探测：ABI 车道与 root 推导用它决定 environment lane。
    ///
    /// 冲突表示由 `prepare_entry` 与 `capture_environment` 明确拒绝，这里只对车道推导给出
    /// 保守答案（按 direct pointer），不会让冲突悄悄进入镜像。
    pub(super) fn closure_is_shared(&self, definition: hir::DefId) -> bool {
        self.shared_environment(definition).unwrap_or(false)
    }

    fn local_ty(&self, local: gir::body::LocalId) -> u32 {
        self.gir.locals[local.index()].ty.0
    }
    fn kind(&self, ty: u32) -> &TypeKind {
        &self.layout(ty).kind
    }
    fn machine_type(&self, value: ValueId) -> ValueType {
        self.body.values[value.index()].kind
    }

    fn variable(&mut self, local: u32, offset: u64, kind: ValueType) -> u32 {
        let index = id(self.variables.len());
        self.variables.push(Variable {
            source: (local, offset),
            kind,
        });
        index
    }

    fn value(&mut self, kind: ValueType, definition: Definition, origin: Origin) -> ValueId {
        let value = ValueId(id(self.body.values.len()));
        self.body.values.push(Value {
            kind,
            definition,
            origin,
            uses: 0..0,
        });
        value
    }

    fn arguments(&mut self, arguments: &[ValueId]) -> Range<u32> {
        let start = id(self.body.operands.len());
        self.body.operands.extend_from_slice(arguments);
        start..id(self.body.operands.len())
    }

    fn emit(
        &mut self,
        op: Op,
        arguments: &[ValueId],
        results: &[(ValueType, Origin)],
    ) -> Vec<ValueId> {
        let instruction = InstId(id(self.body.instructions.len()));
        let first = id(self.body.values.len());
        for (index, (kind, origin)) in results.iter().enumerate() {
            self.value(
                *kind,
                Definition::Instruction {
                    instruction,
                    result: id(index),
                },
                origin.clone(),
            );
        }
        let result_range = first..id(self.body.values.len());
        let memory = op.has_memory().then(|| {
            let input = self.blocks[self.current.index()].memory;
            let output = self.value(
                ValueType::scalar(Type::Mem),
                Definition::Instruction {
                    instruction,
                    result: id(results.len()),
                },
                Origin::None,
            );
            self.blocks[self.current.index()].memory = output;
            Memory { input, output }
        });
        let safepoint = op.safepoint_kind().map(|kind| {
            let safepoint = SafepointId(id(self.body.safepoints.len()));
            self.body.safepoints.push(Safepoint {
                kind,
                block: self.current,
                instruction: Some(instruction),
            });
            safepoint
        });
        let arguments = self.arguments(arguments);
        self.body.instructions.push(Instruction {
            op,
            arguments,
            results: result_range.clone(),
            memory,
            safepoint,
            source: self.source.clone(),
        });
        self.blocks[self.current.index()]
            .instructions
            .push(instruction);
        result_range.map(ValueId).collect()
    }

    fn emit_one(
        &mut self,
        op: Op,
        arguments: &[ValueId],
        kind: ValueType,
        origin: Origin,
    ) -> ValueId {
        self.emit(op, arguments, &[(kind, origin)])[0]
    }

    fn constant(&mut self, value: u64, ty: Type) -> ValueId {
        self.emit_one(Op::IConst(value), &[], ValueType::scalar(ty), Origin::None)
    }

    /// 把符号化 `TypeId` 物化为冻结类型表里的稠密编号。
    ///
    /// 类型身份是 `0..type_id_count()` 的稠密编号（见 `spec/types.md`），不是地址也不是哈希：
    /// 这里在 LIR 期就写成 `u32` 常量，函数体里不再出现需要链接期解析的类型身份符号。
    pub(super) fn type_id_value(&mut self, key: &[u8; 32]) -> Result<ValueId, Diagnostic> {
        let type_id = self
            .mono
            .universe
            .type_id(key)
            .ok_or_else(|| invalid("TypeId 引用了冻结类型表以外的类型"))?;
        Ok(self.constant(u64::from(type_id), Type::I32))
    }

    fn fresh(&mut self, source: SourceInfo, cleanup: bool) -> BlockId {
        let block = BlockId(id(self.blocks.len()));
        let memory = self.value(
            ValueType::scalar(Type::Mem),
            Definition::Parameter { block, index: 0 },
            Origin::None,
        );
        self.blocks.push(PendingBlock {
            parameters: vec![(
                None,
                Parameter {
                    value: memory,
                    source: None,
                },
            )],
            instructions: Vec::new(),
            definitions: vec![None; self.variables.len()],
            memory,
            terminator: None,
            source,
            cleanup,
        });
        block
    }

    fn prepare_blocks(&mut self) {
        let mut reachable = vec![false; self.gir.blocks.len()];
        let mut work = vec![self.gir.entry];
        while let Some(block) = work.pop() {
            if std::mem::replace(&mut reachable[block.index()], true) {
                continue;
            }
            let terminator = &self.gir.blocks[block.index()].terminator;
            if let gir::body::Terminator::Call { site, normal, .. } = terminator
                && self.concrete.calls.iter().any(|call| {
                    call.site == *site
                        && call.target.is_none()
                        && matches!(
                            call.protocol,
                            Some(gir::concrete::Protocol::Eq | gir::concrete::Protocol::Cmp)
                        )
                })
            {
                work.push(*normal);
            } else {
                work.extend(terminator.successors());
            }
        }
        for (index, reachable) in reachable.into_iter().enumerate() {
            if reachable {
                let block = &self.gir.blocks[index];
                self.block_map[index] = Some(self.fresh(block.source.clone(), block.cleanup));
            }
        }
        self.body.entry = self.block_map[self.gir.entry.index()].expect("入口可达");
        self.current = self.body.entry;
    }

    fn target(&self, target: gir::body::BlockId) -> BlockId {
        self.block_map[target.index()].expect("可达 block 的后继也可达")
    }
    fn edge(&mut self, target: BlockId, unwind: bool) -> EdgeId {
        let edge = EdgeId(id(self.body.edges.len()));
        self.body.edges.push(Edge {
            from: self.current,
            to: target,
            arguments: 0..0,
            unwind,
        });
        edge
    }
    fn jump(&mut self, target: BlockId) {
        let edge = self.edge(target, false);
        self.blocks[self.current.index()].terminator = Some(Terminator::Jump(edge));
    }

    fn finish(mut self) -> Result<Body, Diagnostic> {
        self.seal_ssa()?;
        self.materialize_blocks()?;
        super::uses::rebuild(&mut self.body);
        Ok(self.body)
    }

    fn materialize_blocks(&mut self) -> Result<(), Diagnostic> {
        let mut instructions = Vec::with_capacity(self.body.instructions.len());
        let mut remap = vec![InstId(0); self.body.instructions.len()];
        // block ID 稠密；一次按原 edge 顺序归组，避免每个 block 重扫全部边。
        let mut incoming = vec![Vec::new(); self.blocks.len()];
        for (index, edge) in self.body.edges.iter().enumerate() {
            incoming[edge.to.index()].push(EdgeId(id(index)));
        }
        for (index, pending) in self.blocks.iter_mut().enumerate() {
            let instruction_start = id(instructions.len());
            for old in &pending.instructions {
                remap[old.index()] = InstId(id(instructions.len()));
                instructions.push(self.body.instructions[old.index()].clone());
            }
            let parameter_start = id(self.body.parameters.len());
            self.body.parameters.extend(
                pending
                    .parameters
                    .iter()
                    .map(|(_, parameter)| parameter.clone()),
            );
            let predecessor_start = id(self.body.predecessors.len());
            self.body.predecessors.extend_from_slice(&incoming[index]);
            self.body.blocks.push(Block {
                parameters: parameter_start..id(self.body.parameters.len()),
                instructions: instruction_start..id(instructions.len()),
                predecessors: predecessor_start..id(self.body.predecessors.len()),
                terminator: pending
                    .terminator
                    .take()
                    .ok_or_else(|| invalid("LIR block 没有终结符"))?,
                source: pending.source.clone(),
                cleanup: pending.cleanup,
            });
        }
        for value in &mut self.body.values {
            if let Definition::Instruction { instruction, .. } = &mut value.definition {
                *instruction = remap[instruction.index()];
            }
        }
        for instruction in &mut instructions {
            if let Op::GcWriteBarrier { store } | Op::GcWriteBarrierReserved { store, .. } =
                &mut instruction.op
            {
                *store = remap[store.index()];
            }
        }
        for safepoint in &mut self.body.safepoints {
            if let Some(instruction) = &mut safepoint.instruction {
                *instruction = remap[instruction.index()];
            }
        }
        self.body.instructions = instructions;
        Ok(())
    }
}
