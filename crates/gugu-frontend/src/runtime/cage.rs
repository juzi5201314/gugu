//! checked pointer compression 的确定性参照实现。
//!
//! 一个已预留 cage 的稳定描述是 base/len/generation，不含宿主路径或线程信息；cage 被切成
//! 固定粒度的 island，每个 island 承载一个 owner 的 managed arena。压缩字按
//! `cage id | generation | offset` 编码，解码路径只此一处：空字、未登记 cage、过期
//! generation、越界 offset 与非 canonical 地址全部返回 `RawInvariant`，绝不猜测。
//!
//! FFI 交接遵守三条规则：native code 只能拿到已 resolve 的完整地址（`resolve-then-pin`），
//! 压缩字不得直接交给 native（`no-compressed-pass-through`），把地址保存到 native 生命周期
//! 之外必须持有活动 pin lease（`save-requires-active-lease`）。

use super::compression_schema::CompressionRuntimeContract;
use super::provider::{RangeId, RangeProvider};
use super::slab::{MemoryDomainId, RawInvariant};

/// 一个已预留 cage 的稳定描述：base/len/generation；不含宿主路径或线程信息。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CageDescriptor {
    /// cage 基址。
    pub(crate) base: u64,
    /// cage 字节数。
    pub(crate) len: u64,
    /// 当前 generation；旧 generation 的压缩字全部过期。
    pub(crate) generation: u32,
}

/// 从 cage 里切出的一段 arena 区间：range 加区间内偏移。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CageIsland {
    /// island 所属的平台 range（cage reservation 本身）。
    pub(crate) range: RangeId,
    /// island 在 range 内的字节偏移。
    pub(crate) offset: u64,
    /// island 的绝对基址。
    pub(crate) base: u64,
}

/// FFI 交接 lease：绑定 cage、offset 与建立时的 generation。
///
/// `sequence` 让同一 offset/generation 上的多次 pin 可区分：释放一个 lease 不会意外释放
/// 另一个。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ForeignPin {
    /// cage 编号。
    pub(crate) cage: u8,
    /// cage 内 offset。
    pub(crate) offset: u32,
    /// 建立时的 cage generation。
    pub(crate) generation: u32,
    /// 世界内单调的 lease 序号。
    pub(crate) sequence: u32,
}

/// 压缩平面的累计统计；顺序与 `CAGE_STATISTICS` 登记一致。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CompressionStats {
    /// 成功解码的压缩引用数。
    pub(crate) decodes: u64,
    /// 被拒绝的压缩引用解码数。
    pub(crate) rejections: u64,
    /// 建立的 FFI pin 数。
    pub(crate) foreign_pins: u64,
    /// 成功的 FFI 保存数。
    pub(crate) foreign_saves: u64,
    /// 成功的 FFI 复制数。
    pub(crate) foreign_copies: u64,
    /// 被拒绝的 FFI 保存或直接交接数。
    pub(crate) foreign_rejections: u64,
}

/// 一个已预留 cage 的运行状态：平台 range、基址、容量、generation 与 island bump。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Cage {
    range: RangeId,
    base: u64,
    len: u64,
    generation: u32,
    /// 下一个 island 的字节偏移；只增不减。
    bump: u64,
}

/// checked pointer compression 的确定性参照实现。
#[derive(Clone, Debug)]
pub(crate) struct CompressionPlane {
    contract: CompressionRuntimeContract,
    /// 已预留 cage；关闭态为空，`reserve` 每次压入一个。
    cages: Vec<Cage>,
    /// 活动 FFI pin lease；按建立顺序追加。
    active_pins: Vec<ForeignPin>,
    /// 下一个 pin 序号；从 1 起单调推进，回绕时跳过 0。
    next_pin: u32,
    stats: CompressionStats,
}

impl CompressionPlane {
    /// 按契约建立平面；未启用时 `cages` 为空，`reserve` 是 no-op。
    pub(crate) fn new(contract: &CompressionRuntimeContract) -> Self {
        Self {
            contract: contract.clone(),
            cages: Vec::new(),
            active_pins: Vec::new(),
            next_pin: 0,
            stats: CompressionStats::default(),
        }
    }

    /// 返回 cage profile 是否启用。
    pub(crate) fn enabled(&self) -> bool {
        self.contract.enabled()
    }

    /// 返回所依据的压缩契约。
    pub(crate) fn contract(&self) -> &CompressionRuntimeContract {
        &self.contract
    }

    /// 预留契约要求的 cage。
    ///
    /// 只预留虚拟地址：物理页在 island 内的 extent 提交时才 commit。基址必须是粒度的整数倍
    /// 且落在 canonical 正半区；容量的完整边界由解码时的 checked 判断负责。
    pub(crate) fn reserve(
        &mut self,
        provider: &mut impl RangeProvider,
    ) -> Result<(), RawInvariant> {
        if !self.enabled() {
            return Ok(());
        }
        let alignment = self
            .contract
            .cage_granule_bytes()
            .max(self.contract.capability().min_alignment);
        let range = provider.reserve_aligned(
            self.contract.cage_bytes(),
            alignment,
            MemoryDomainId::MANAGED_LOCAL,
        )?;
        let descriptor = provider
            .describe(range)
            .ok_or_else(|| RawInvariant::new("cage 预留后描述缺失"))?;
        if !descriptor.base.is_multiple_of(alignment) {
            return Err(RawInvariant::new("cage 基址未按粒度对齐"));
        }
        if descriptor.base.checked_add(descriptor.bytes).is_none()
            || !is_canonical(descriptor.base, self.contract.canonical_bits())
        {
            return Err(RawInvariant::new("cage 预留落在非 canonical 地址空间"));
        }
        if descriptor.bytes < self.contract.cage_bytes() {
            return Err(RawInvariant::new("cage 预留小于契约要求"));
        }
        self.cages.push(Cage {
            range,
            base: descriptor.base,
            len: descriptor.bytes,
            generation: self.contract.generation_min(),
            bump: 0,
        });
        Ok(())
    }

    /// 从 cage 切出一个 island 交给一个 managed arena。
    ///
    /// island 按粒度对齐、容量不足时拒绝且不推进 bump；island 之外的 cage 区域保持未使用。
    pub(crate) fn take_island(&mut self, bytes: u64) -> Result<CageIsland, RawInvariant> {
        if !self.enabled() {
            return Err(RawInvariant::new(
                "未启用 cage profile 时不能切分 managed arena",
            ));
        }
        if !bytes.is_multiple_of(self.contract.cage_granule_bytes()) {
            return Err(RawInvariant::new("island 字节数必须是 arena 粒度的整数倍"));
        }
        let cage = self
            .cages
            .first_mut()
            .ok_or_else(|| RawInvariant::new("cage profile 尚未预留 cage"))?;
        let end = cage
            .bump
            .checked_add(bytes)
            .ok_or_else(|| RawInvariant::new("island 偏移溢出"))?;
        if end > cage.len {
            return Err(RawInvariant::new("cage 容量不足以容纳新的 managed arena"));
        }
        let island = CageIsland {
            range: cage.range,
            offset: cage.bump,
            base: cage.base + cage.bump,
        };
        cage.bump = end;
        Ok(island)
    }

    /// checked 解码一个压缩字；空字返回 `None`，成功返回完整地址。
    ///
    /// 这是压缩引用的唯一解码路径：空字不解码也不计数，其余失败路径都计入
    /// `rejections` 后再返回错误。
    pub(crate) fn decode(&mut self, word: u64) -> Result<Option<u64>, RawInvariant> {
        if word == self.contract.null_word() {
            return Ok(None);
        }
        let cage_id = (word >> self.contract.cage_id_shift()) as u8;
        let generation = ((word >> self.contract.cage_generation_shift())
            & self.contract.cage_generation_mask()) as u32;
        let offset = word & self.contract.cage_offset_mask();
        let Some(cage) = self.cages.get(usize::from(cage_id)) else {
            return Err(self.reject_decode("压缩引用 cage 未登记"));
        };
        if generation < self.contract.generation_min() || generation != cage.generation {
            return Err(self.reject_decode("压缩引用 generation 过期"));
        }
        if offset >= cage.len {
            return Err(self.reject_decode("压缩引用 offset 越过 cage 范围"));
        }
        let Some(address) = cage.base.checked_add(offset) else {
            return Err(self.reject_decode("压缩引用解码出非 canonical 地址"));
        };
        if !is_canonical(address, self.contract.canonical_bits()) {
            return Err(self.reject_decode("压缩引用解码出非 canonical 地址"));
        }
        self.stats.decodes += 1;
        Ok(Some(address))
    }

    /// 把一个 cage 内地址编码为压缩字。
    pub(crate) fn encode(&mut self, address: u64) -> Result<u64, RawInvariant> {
        if !self.enabled() {
            return Err(RawInvariant::new("未启用 cage profile 时不能编码压缩引用"));
        }
        let (cage_id, cage) = self
            .cages
            .iter()
            .enumerate()
            .find(|(_, cage)| cage.contains(address))
            .ok_or_else(|| RawInvariant::new("地址不在任何 cage 内，不能编码为压缩引用"))?;
        let offset = address - cage.base;
        Ok(self.pack(cage_id as u8, cage.generation, offset))
    }

    /// 按 cage 与 offset 重新编码；世界搬迁后本地槽与根槽用它写回。
    pub(crate) fn encode_offset(&self, cage: u8, offset: u64) -> Result<u64, RawInvariant> {
        let entry = self
            .cages
            .get(usize::from(cage))
            .ok_or_else(|| RawInvariant::new("压缩引用 cage 未登记"))?;
        if offset >= entry.len {
            return Err(RawInvariant::new("压缩引用 offset 越过 cage 范围"));
        }
        Ok(self.pack(cage, entry.generation, offset))
    }

    /// 返回地址所属的 cage 编号与当前 generation。
    pub(crate) fn cage_of(&self, address: u64) -> Option<(u8, u32)> {
        self.cages
            .iter()
            .enumerate()
            .find(|(_, cage)| cage.contains(address))
            .map(|(index, cage)| (index as u8, cage.generation))
    }

    /// 返回 cage 的稳定描述。
    pub(crate) fn descriptor(&self, cage: u8) -> Result<CageDescriptor, RawInvariant> {
        let entry = self
            .cages
            .get(usize::from(cage))
            .ok_or_else(|| RawInvariant::new("cage 编号未登记"))?;
        Ok(CageDescriptor {
            base: entry.base,
            len: entry.len,
            generation: entry.generation,
        })
    }

    /// 推进一个 cage 的 generation；旧字全部变为过期，返回新值。
    pub(crate) fn advance_generation(&mut self, cage: u8) -> Result<u32, RawInvariant> {
        let entry = self
            .cages
            .get_mut(usize::from(cage))
            .ok_or_else(|| RawInvariant::new("cage 编号未登记"))?;
        if entry.generation >= self.contract.generation_max() {
            return Err(RawInvariant::new("cage generation 已耗尽"));
        }
        entry.generation += 1;
        Ok(entry.generation)
    }

    /// 为一个 cage 内地址建立 FFI pin lease。
    pub(crate) fn pin_for_foreign(&mut self, address: u64) -> Result<ForeignPin, RawInvariant> {
        if !self.enabled() {
            return Err(RawInvariant::new(
                "未启用 cage profile 时不存在压缩 FFI 交接",
            ));
        }
        let Some((cage_id, generation)) = self.cage_of(address) else {
            return Err(RawInvariant::new("cage 外地址不能建立 FFI pin"));
        };
        let offset = u32::try_from(address - self.cages[usize::from(cage_id)].base)
            .map_err(|_| RawInvariant::new("FFI pin 的 offset 超过 u32"))?;
        self.next_pin = self.next_pin.wrapping_add(1);
        if self.next_pin == 0 {
            self.next_pin = 1;
        }
        let pin = ForeignPin {
            cage: cage_id,
            offset,
            generation,
            sequence: self.next_pin,
        };
        self.active_pins.push(pin);
        self.stats.foreign_pins += 1;
        Ok(pin)
    }

    /// 用活动 pin lease 把地址保存到 native 生命周期之外。
    pub(crate) fn save_for_foreign(&mut self, pin: ForeignPin) -> Result<u64, RawInvariant> {
        if !self.active_pins.contains(&pin) {
            return Err(self.reject_foreign("FFI 保存缺少活动 pin lease"));
        }
        let Some(cage) = self.cages.get(usize::from(pin.cage)) else {
            return Err(self.reject_foreign("FFI pin 引用的 cage 未登记"));
        };
        if pin.generation != cage.generation {
            return Err(self.reject_foreign("FFI 保存的 cage generation 已过期"));
        }
        self.stats.foreign_saves += 1;
        Ok(cage.base + u64::from(pin.offset))
    }

    /// 释放一个 FFI pin lease。
    pub(crate) fn release_for_foreign(&mut self, pin: ForeignPin) -> Result<(), RawInvariant> {
        let index = self
            .active_pins
            .iter()
            .position(|active| *active == pin)
            .ok_or_else(|| RawInvariant::new("FFI pin lease 已释放或不存在"))?;
        self.active_pins.swap_remove(index);
        Ok(())
    }

    /// 把一个 cage 内对象复制给 native code。
    ///
    /// copy 不延长地址生命周期，因此不要求 lease；但地址仍必须落在已知 cage 内，压缩字与
    /// cage 外地址都不能直接交给 native。
    pub(crate) fn copy_for_foreign(
        &mut self,
        address: u64,
        bytes: &[u8],
    ) -> Result<Vec<u8>, RawInvariant> {
        if self.cage_of(address).is_none() {
            return Err(RawInvariant::new("cage 外地址不能复制给 native code"));
        }
        self.stats.foreign_copies += 1;
        Ok(bytes.to_vec())
    }

    /// 压缩字永远不能直接交给 native code。
    pub(crate) fn handoff_word_for_foreign(&mut self, word: u64) -> Result<u64, RawInvariant> {
        let _ = word;
        Err(self.reject_foreign("压缩引用不能直接交给 native code，必须先 resolve+pin"))
    }

    /// 返回累计统计。
    pub(crate) fn stats(&self) -> CompressionStats {
        self.stats
    }

    /// 返回第一个已预留 cage 的描述；未预留时为 `None`。
    pub(crate) fn cage_descriptor(&self) -> Option<CageDescriptor> {
        self.cages.first().map(|cage| CageDescriptor {
            base: cage.base,
            len: cage.len,
            generation: cage.generation,
        })
    }

    /// 返回成功解码次数。
    pub(crate) fn decode_count(&self) -> u64 {
        self.stats.decodes
    }

    /// 返回本平面已切出的 island 数（按 bump 推进的粒度计数）。
    pub(crate) fn island_count(&self) -> u64 {
        self.cages
            .iter()
            .map(|cage| cage.bump / self.contract.cage_granule_bytes())
            .sum()
    }

    /// 返回当前活动的 FFI pin 数。
    pub(crate) fn active_pin_count(&self) -> u32 {
        u32::try_from(self.active_pins.len()).expect("活动 pin 数适配 u32")
    }

    /// 把 `(cage, generation, offset)` 打成压缩字。
    fn pack(&self, cage: u8, generation: u32, offset: u64) -> u64 {
        (u64::from(cage) << self.contract.cage_id_shift())
            | (u64::from(generation) << self.contract.cage_generation_shift())
            | (offset & self.contract.cage_offset_mask())
    }

    fn reject_decode(&mut self, message: &str) -> RawInvariant {
        self.stats.rejections += 1;
        RawInvariant::new(message)
    }

    fn reject_foreign(&mut self, message: &str) -> RawInvariant {
        self.stats.foreign_rejections += 1;
        RawInvariant::new(message)
    }
}

impl Cage {
    /// 判断地址是否落在本 cage 的 `[base, base + len)` 内。
    fn contains(&self, address: u64) -> bool {
        address >= self.base && address - self.base < self.len
    }
}

/// 判断地址是否落在 `bits` 位宽的 canonical 区间。
///
/// x86_64 的 48 位 canonical 地址是低半 `< 1 << 47` 或高半 `>= 2^64 - 2^47`：位 63..47
/// 必须全为 0 或全为 1，两者之间的区间是硬件会 fault 的 canonical hole。低半区由
/// `address < 1 << (bits - 1)` 判定，高半区由符号扩展判定；压缩解码用它拒绝任何落在
/// hole 里的地址。
pub(crate) fn is_canonical(address: u64, bits: u8) -> bool {
    if bits == 0 {
        return false;
    }
    if bits >= 64 {
        return true;
    }
    let top = address >> (bits - 1);
    top == 0 || top == u64::MAX >> (bits - 1)
}
