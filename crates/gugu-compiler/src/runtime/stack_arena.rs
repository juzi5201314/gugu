//! 256 MiB stack arena、128个span、亚页slot与owner-local有界cache。
//! bitmap同时覆盖live/cached/pending slot，空页回收不改变arena内部protection。

use super::provider::{GuardEdges, RangeId, RangeProvider};
use super::slab::MemoryDomainId;
use super::stack::{
    STACK_ARENA_BYTES, STACK_CACHE_CLASSES, STACK_CACHE_LIMIT, STACK_CACHE_LOW, STACK_CLASSES,
    STACK_SPAN_BYTES, STACK_SPANS, StackError,
};

const PAGE: usize = 4096;
const SLOT_WORDS: usize = STACK_SPAN_BYTES / 512 / 64;
const PAGE_WORDS: usize = STACK_SPAN_BYTES / PAGE / 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StackHandle {
    pub(crate) index: u32,
    pub(crate) generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SpanRef {
    arena: usize,
    span: usize,
}

#[derive(Debug)]
struct Span {
    class: Option<usize>,
    occupied: [u64; SLOT_WORDS],
    free_words: u64,
    count: u32,
    committed: [u64; PAGE_WORDS],
    next: Option<SpanRef>,
}

impl Default for Span {
    fn default() -> Self {
        Self {
            class: None,
            occupied: [0; SLOT_WORDS],
            free_words: 0,
            count: 0,
            committed: [0; PAGE_WORDS],
            next: None,
        }
    }
}

impl Span {
    fn configure(&mut self, class: usize) {
        debug_assert!(self.count == 0 && class < STACK_CLASSES.len());
        self.class = Some(class);
        let count = STACK_SPAN_BYTES / STACK_CLASSES[class];
        let words = count.div_ceil(64);
        // 一个span最多4096个slot，因而只有64个bitmap word；汇总mask零堆分配。
        debug_assert!(words <= 64);
        self.free_words = if words == 64 {
            u64::MAX
        } else {
            (1_u64 << words) - 1
        };
        self.occupied = [u64::MAX; SLOT_WORDS];
        for word in 0..words {
            let available = (count - word * 64).min(64);
            self.occupied[word] = if available == 64 {
                0
            } else {
                u64::MAX << available
            };
        }
    }

    fn allocate(&mut self) -> u32 {
        debug_assert!(self.free_words != 0);
        let word = usize::try_from(self.free_words.trailing_zeros()).expect("word下标");
        let bit = (!self.occupied[word]).trailing_zeros();
        self.occupied[word] |= 1_u64 << bit;
        if self.occupied[word] == u64::MAX {
            self.free_words &= !(1_u64 << word);
        }
        self.count += 1;
        u32::try_from(word * 64).expect("slot下标") + bit
    }

    fn free(&mut self, unit: u32) {
        let word = usize::try_from(unit / 64).expect("word下标");
        let mask = 1_u64 << (unit % 64);
        debug_assert!(self.occupied[word] & mask != 0 && self.count != 0);
        self.occupied[word] &= !mask;
        self.free_words |= 1_u64 << word;
        self.count -= 1;
    }

    fn page_occupied(&self, page: usize) -> bool {
        let Some(class) = self.class else {
            return self.count != 0;
        };
        let capacity = STACK_CLASSES[class];
        let first = page * PAGE / capacity;
        let last = ((page + 1) * PAGE).div_ceil(capacity);
        (first..last).any(|unit| self.occupied[unit / 64] & (1_u64 << (unit % 64)) != 0)
    }
}

#[derive(Debug)]
struct Arena {
    range: RangeId,
    low: usize,
    /// 128个span严格有界；按机器字执行连续buddy extent认领。
    occupied: u128,
    spans: Box<[Span; STACK_SPANS]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlotState {
    Reserved,
    Live,
    Cached,
    Pending,
    Free,
}

#[derive(Debug)]
struct Allocation {
    generation: u64,
    state: SlotState,
    arena: Option<usize>,
    span: usize,
    unit: u32,
    range: RangeId,
    low: usize,
    capacity: usize,
    owner: u32,
    next: Option<u32>,
}

#[derive(Debug, Default)]
struct StackCache {
    heads: [Option<u32>; STACK_CACHE_CLASSES],
    bytes: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct StackStats {
    pub(crate) reserved_bytes: u64,
    pub(crate) committed_bytes: u64,
    pub(crate) live_bytes: u64,
    pub(crate) cache_bytes: u64,
    pub(crate) pending_bytes: u64,
    pub(crate) mappings: u32,
    pub(crate) cache_hits: u64,
}

#[derive(Debug, Default)]
pub(crate) struct StackAllocator {
    arenas: Vec<Option<Arena>>,
    available: [Option<SpanRef>; 13],
    slots: Vec<Allocation>,
    free_slot: Option<u32>,
    caches: Vec<StackCache>,
    stats: StackStats,
}

impl StackAllocator {
    pub(crate) fn stats(&self) -> StackStats {
        self.stats
    }

    pub(crate) fn adopt(&mut self, handle: StackHandle, owner: u32) -> Result<(), StackError> {
        if self.slot(handle)?.state != SlotState::Live {
            return Err(StackError::Invariant("只能交接live stack"));
        }
        self.slots[usize::try_from(handle.index).expect("slot下标")].owner = owner;
        Ok(())
    }

    pub(crate) fn return_identity(&self, index: u32) -> Result<(StackHandle, usize), StackError> {
        let slot = self
            .slots
            .get(usize::try_from(index).expect("slot下标"))
            .filter(|slot| slot.state == SlotState::Pending)
            .ok_or(StackError::Invariant("stack return引用未排队的slot"))?;
        Ok((
            StackHandle {
                index,
                generation: slot.generation,
            },
            slot.capacity,
        ))
    }

    pub(crate) fn shutdown(&mut self, provider: &mut impl RangeProvider) -> Result<(), StackError> {
        if self.slots.iter().any(|slot| slot.state != SlotState::Free) {
            return Err(StackError::Invariant(
                "stack arena关闭时仍有live/cache/pending slot",
            ));
        }
        for arena in &mut self.arenas {
            if let Some(value) = arena {
                provider.release(value.range)?;
                *arena = None;
            }
        }
        self.stats.reserved_bytes = 0;
        self.stats.mappings = 0;
        Ok(())
    }

    fn slot(&self, handle: StackHandle) -> Result<&Allocation, StackError> {
        self.slots
            .get(usize::try_from(handle.index).expect("slot下标"))
            .filter(|slot| slot.state != SlotState::Free && slot.generation == handle.generation)
            .ok_or(StackError::Invariant("stack handle已过期或已归还"))
    }

    pub(crate) fn bounds(&self, handle: StackHandle) -> Result<(usize, usize), StackError> {
        let slot = self.slot(handle)?;
        Ok((slot.low, slot.capacity))
    }

    pub(crate) fn owner(&self, handle: StackHandle) -> Result<u32, StackError> {
        Ok(self.slot(handle)?.owner)
    }

    fn span_mask(first: usize, count: usize) -> u128 {
        debug_assert!(
            count.is_power_of_two() && first.is_multiple_of(count) && first + count <= STACK_SPANS
        );
        if count == 128 {
            u128::MAX
        } else {
            ((1_u128 << count) - 1) << first
        }
    }

    fn reserve_mapping(
        provider: &mut impl RangeProvider,
        bytes: usize,
    ) -> Result<(RangeId, usize), StackError> {
        let total = bytes.checked_add(2 * PAGE).ok_or(StackError::Overflow)?;
        let range = provider.reserve_aligned(
            u64::try_from(total).map_err(|_| StackError::Overflow)?,
            u64::try_from(PAGE).expect("页大小"),
            MemoryDomainId::RUNTIME_RAW,
        )?;
        if let Err(error) = provider.protect_guard(range, GuardEdges::Both) {
            provider.release(range)?;
            return Err(error.into());
        }
        let descriptor = provider
            .describe(range)
            .ok_or(StackError::Invariant("stack range没有descriptor"))?;
        let low = usize::try_from(descriptor.base).map_err(|_| StackError::Overflow)? + PAGE;
        if low
            .checked_add(bytes)
            .is_none_or(|high| high >= super::coroutine::POLL_SENTINEL)
        {
            provider.release(range)?;
            return Err(StackError::Platform(
                super::provider::ProviderError::OutOfSpace,
            ));
        }
        Ok((range, low))
    }

    fn take_spans(
        &mut self,
        count: usize,
        provider: &mut impl RangeProvider,
    ) -> Result<SpanRef, StackError> {
        for (index, arena) in self.arenas.iter_mut().enumerate() {
            let Some(arena) = arena else {
                continue;
            };
            for first in (0..STACK_SPANS).step_by(count) {
                let mask = Self::span_mask(first, count);
                if arena.occupied & mask == 0 {
                    arena.occupied |= mask;
                    return Ok(SpanRef {
                        arena: index,
                        span: first,
                    });
                }
            }
        }
        let (range, low) = Self::reserve_mapping(provider, STACK_ARENA_BYTES)?;
        let arena = Arena {
            range,
            low,
            occupied: Self::span_mask(0, count),
            spans: Box::new(std::array::from_fn(|_| Span::default())),
        };
        let index = if let Some(index) = self.arenas.iter().position(Option::is_none) {
            self.arenas[index] = Some(arena);
            index
        } else {
            self.arenas.push(Some(arena));
            self.arenas.len() - 1
        };
        self.stats.reserved_bytes +=
            u64::try_from(STACK_ARENA_BYTES + 2 * PAGE).expect("arena大小");
        self.stats.mappings += 1;
        Ok(SpanRef {
            arena: index,
            span: 0,
        })
    }

    pub(crate) fn reserve(
        &mut self,
        owner: u32,
        capacity: usize,
        provider: &mut impl RangeProvider,
    ) -> Result<StackHandle, StackError> {
        if capacity < 512 || !capacity.is_power_of_two() {
            return Err(StackError::Invariant("stack capacity不属于二次幂阶梯"));
        }
        let (arena, span, unit, range, low) = if capacity > STACK_ARENA_BYTES / 2 {
            let (range, low) = Self::reserve_mapping(provider, capacity)?;
            self.stats.reserved_bytes +=
                u64::try_from(capacity + 2 * PAGE).expect("stack reservation大小");
            self.stats.mappings += 1;
            (None, 0, 0, range, low)
        } else if capacity > STACK_SPAN_BYTES {
            let count = capacity / STACK_SPAN_BYTES;
            let at = self.take_spans(count, provider)?;
            let arena = self.arenas[at.arena].as_mut().expect("arena存在");
            for span in &mut arena.spans[at.span..at.span + count] {
                span.count = 1;
            }
            (
                Some(at.arena),
                at.span,
                0,
                arena.range,
                arena.low + at.span * STACK_SPAN_BYTES,
            )
        } else {
            let class = usize::try_from(capacity.trailing_zeros() - 9).expect("class下标");
            if self.available[class].is_none() {
                let at = self.take_spans(1, provider)?;
                self.arenas[at.arena].as_mut().expect("arena存在").spans[at.span].configure(class);
                self.available[class] = Some(at);
            }
            let at = self.available[class].expect("非空class链");
            let arena = self.arenas[at.arena].as_mut().expect("arena存在");
            let span = &mut arena.spans[at.span];
            let unit = span.allocate();
            if span.free_words == 0 {
                self.available[class] = span.next.take();
            }
            (
                Some(at.arena),
                at.span,
                unit,
                arena.range,
                arena.low
                    + at.span * STACK_SPAN_BYTES
                    + usize::try_from(unit).expect("unit下标") * capacity,
            )
        };
        let allocation = Allocation {
            generation: 1,
            state: SlotState::Reserved,
            arena,
            span,
            unit,
            range,
            low,
            capacity,
            owner,
            next: None,
        };
        let index = if let Some(index) = self.free_slot {
            let old = &self.slots[usize::try_from(index).expect("slot下标")];
            self.free_slot = old.next;
            let generation = old.generation;
            self.slots[usize::try_from(index).expect("slot下标")] = Allocation {
                generation,
                ..allocation
            };
            index
        } else {
            let index = u32::try_from(self.slots.len()).map_err(|_| StackError::Overflow)?;
            self.slots.push(allocation);
            index
        };
        self.stats.live_bytes += u64::try_from(capacity).expect("capacity适配u64");
        Ok(StackHandle {
            index,
            generation: self.slots[usize::try_from(index).expect("slot下标")].generation,
        })
    }

    pub(crate) fn commit(
        &mut self,
        handle: StackHandle,
        provider: &mut impl RangeProvider,
    ) -> Result<(), StackError> {
        let slot = self.slot(handle)?;
        if slot.state == SlotState::Live {
            return Ok(());
        }
        if slot.state != SlotState::Reserved {
            return Err(StackError::Invariant("stack commit没有reservation所有权"));
        }
        let range = slot.range;
        let low = slot.low;
        let capacity = slot.capacity;
        let start = low / PAGE * PAGE;
        let end = (low + capacity).next_multiple_of(PAGE);
        let base = usize::try_from(
            provider
                .describe(range)
                .ok_or(StackError::Invariant("range丢失"))?
                .base,
        )
        .expect("base适配目标");
        let needs_commit = slot.arena.is_none_or(|index| {
            let arena = self.arenas[index].as_ref().expect("arena存在");
            (start - arena.low..end - arena.low)
                .step_by(PAGE)
                .any(|offset| {
                    let span = &arena.spans[offset / STACK_SPAN_BYTES];
                    let page = offset % STACK_SPAN_BYTES / PAGE;
                    span.committed[page / 64] & (1_u64 << (page % 64)) == 0
                })
        });
        if needs_commit {
            provider.commit_pages(
                range,
                u64::try_from(start - base).expect("offset"),
                u64::try_from(end - start).expect("bytes"),
            )?;
        }
        let slot = &self.slots[usize::try_from(handle.index).expect("slot下标")];
        if let Some(index) = slot.arena {
            let arena = self.arenas[index].as_mut().expect("arena存在");
            for offset in (start - arena.low..end - arena.low).step_by(PAGE) {
                let span = &mut arena.spans[offset / STACK_SPAN_BYTES];
                let page = offset % STACK_SPAN_BYTES / PAGE;
                let word = &mut span.committed[page / 64];
                let mask = 1_u64 << (page % 64);
                if *word & mask == 0 {
                    self.stats.committed_bytes += u64::try_from(PAGE).expect("page");
                    *word |= mask;
                }
            }
        } else {
            self.stats.committed_bytes += u64::try_from(capacity).expect("capacity");
        }
        self.slots[usize::try_from(handle.index).expect("slot下标")].state = SlotState::Live;
        Ok(())
    }

    fn cache_index(&mut self, owner: u32) -> usize {
        let index = usize::try_from(owner).expect("owner下标");
        if self.caches.len() <= index {
            self.caches.resize_with(index + 1, StackCache::default);
        }
        index
    }

    pub(crate) fn acquire(
        &mut self,
        owner: u32,
        capacity: usize,
        provider: &mut impl RangeProvider,
    ) -> Result<StackHandle, StackError> {
        let cache = self.cache_index(owner);
        if (512..=32768).contains(&capacity) && capacity.is_power_of_two() {
            let class = usize::try_from(capacity.trailing_zeros() - 9).expect("class下标");
            if let Some(index) = self.caches[cache].heads[class] {
                let slot = &mut self.slots[usize::try_from(index).expect("slot下标")];
                self.caches[cache].heads[class] = slot.next.take();
                self.caches[cache].bytes -= capacity;
                self.stats.cache_bytes -= u64::try_from(capacity).expect("capacity");
                self.stats.live_bytes += u64::try_from(capacity).expect("capacity");
                self.stats.cache_hits += 1;
                slot.state = SlotState::Live;
                return Ok(StackHandle {
                    index,
                    generation: slot.generation,
                });
            }
        }
        // 一次cache miss只转移所需slot，不预提交未使用的页；批量传输量因此不超过refill上限。
        let handle = self.reserve(owner, capacity, provider)?;
        if let Err(error) = self.commit(handle, provider) {
            self.release_global(handle, provider)?;
            return Err(error);
        }
        Ok(handle)
    }

    pub(crate) fn mark_pending(&mut self, handle: StackHandle) -> Result<(), StackError> {
        let slot = self.slot(handle)?;
        if slot.state != SlotState::Live {
            return Err(StackError::Invariant("stack只能归还一次"));
        }
        let capacity = u64::try_from(slot.capacity).expect("capacity");
        self.slots[usize::try_from(handle.index).expect("slot下标")].state = SlotState::Pending;
        self.stats.live_bytes -= capacity;
        self.stats.pending_bytes += capacity;
        Ok(())
    }

    pub(crate) fn reclaim_pending(
        &mut self,
        handle: StackHandle,
        owner: u32,
        provider: &mut impl RangeProvider,
    ) -> Result<(), StackError> {
        if self.slot(handle)?.state != SlotState::Pending {
            return Err(StackError::Invariant("stack return不是Pending"));
        }
        let slot = &mut self.slots[usize::try_from(handle.index).expect("slot下标")];
        slot.state = SlotState::Live;
        slot.owner = owner;
        self.stats.pending_bytes -= u64::try_from(slot.capacity).expect("capacity");
        self.stats.live_bytes += u64::try_from(slot.capacity).expect("capacity");
        self.recycle(handle, owner, provider)
    }

    pub(crate) fn recycle(
        &mut self,
        handle: StackHandle,
        owner: u32,
        provider: &mut impl RangeProvider,
    ) -> Result<(), StackError> {
        let slot = self.slot(handle)?;
        if slot.state != SlotState::Live || slot.owner != owner {
            return Err(StackError::Invariant("stack归还没有owner唯一所有权"));
        }
        let capacity = slot.capacity;
        if capacity > STACK_CLASSES[STACK_CACHE_CLASSES - 1] {
            return self.release_global(handle, provider);
        }
        let next_generation = slot
            .generation
            .checked_add(1)
            .ok_or(StackError::Invariant("stack generation溢出"))?;
        let cache = self.cache_index(owner);
        let class = usize::try_from(capacity.trailing_zeros() - 9).expect("class下标");
        let slot = &mut self.slots[usize::try_from(handle.index).expect("slot下标")];
        slot.generation = next_generation;
        slot.state = SlotState::Cached;
        slot.next = self.caches[cache].heads[class];
        self.caches[cache].heads[class] = Some(handle.index);
        self.caches[cache].bytes += capacity;
        self.stats.live_bytes -= u64::try_from(capacity).expect("capacity");
        self.stats.cache_bytes += u64::try_from(capacity).expect("capacity");
        if self.caches[cache].bytes > STACK_CACHE_LIMIT {
            self.trim_cache(owner, STACK_CACHE_LOW, provider)?;
        }
        debug_assert!(self.caches[cache].bytes <= STACK_CACHE_LIMIT);
        Ok(())
    }

    pub(crate) fn trim_cache(
        &mut self,
        owner: u32,
        target: usize,
        provider: &mut impl RangeProvider,
    ) -> Result<(), StackError> {
        let cache = self.cache_index(owner);
        for class in (0..STACK_CACHE_CLASSES).rev() {
            while self.caches[cache].bytes > target {
                let Some(index) = self.caches[cache].heads[class] else {
                    break;
                };
                let slot = &mut self.slots[usize::try_from(index).expect("slot下标")];
                self.caches[cache].heads[class] = slot.next.take();
                self.caches[cache].bytes -= slot.capacity;
                let handle = StackHandle {
                    index,
                    generation: slot.generation,
                };
                self.release_global(handle, provider)?;
            }
        }
        Ok(())
    }

    fn unlink_span(&mut self, class: usize, at: SpanRef) {
        let mut current = self.available[class];
        let mut previous: Option<SpanRef> = None;
        while let Some(cursor) = current {
            let next = self.arenas[cursor.arena].as_ref().expect("arena").spans[cursor.span].next;
            if cursor == at {
                if let Some(previous) = previous {
                    self.arenas[previous.arena].as_mut().expect("arena").spans[previous.span]
                        .next = next;
                } else {
                    self.available[class] = next;
                }
                break;
            }
            previous = current;
            current = next;
        }
    }

    pub(crate) fn release_global(
        &mut self,
        handle: StackHandle,
        provider: &mut impl RangeProvider,
    ) -> Result<(), StackError> {
        let slot = self.slot(handle)?;
        if slot.state == SlotState::Pending {
            return Err(StackError::Invariant("不能绕过owner消费在途stack"));
        }
        let (arena_index, first, unit, capacity, range, state) = (
            slot.arena,
            slot.span,
            slot.unit,
            slot.capacity,
            slot.range,
            slot.state,
        );
        let generation = slot
            .generation
            .checked_add(1)
            .ok_or(StackError::Invariant("stack generation溢出"))?;
        if let Some(index) = arena_index {
            let count = capacity.div_ceil(STACK_SPAN_BYTES);
            if capacity <= STACK_SPAN_BYTES {
                let class = usize::try_from(capacity.trailing_zeros() - 9).expect("class");
                let span = &mut self.arenas[index].as_mut().expect("arena").spans[first];
                let full = span.free_words == 0;
                span.free(unit);
                if full {
                    span.next = self.available[class];
                    self.available[class] = Some(SpanRef {
                        arena: index,
                        span: first,
                    });
                }
                if span.count == 0 {
                    self.unlink_span(
                        class,
                        SpanRef {
                            arena: index,
                            span: first,
                        },
                    );
                    self.arenas[index].as_mut().expect("arena").occupied &=
                        !Self::span_mask(first, 1);
                }
            } else {
                let arena = self.arenas[index].as_mut().expect("arena");
                for span in &mut arena.spans[first..first + count] {
                    span.count = 0;
                }
                arena.occupied &= !Self::span_mask(first, count);
            }
            self.trim_pages(index, first, count, provider)?;
            let arena = self.arenas[index].as_mut().expect("arena");
            for span in &mut arena.spans[first..first + count] {
                if span.count == 0 {
                    *span = Span::default();
                }
            }
            self.release_empty_arena(index, provider)?;
        } else {
            provider.release(range)?;
            if state != SlotState::Reserved {
                self.stats.committed_bytes -= u64::try_from(capacity).expect("capacity");
            }
            self.stats.reserved_bytes -= u64::try_from(capacity + 2 * PAGE).expect("reservation");
            self.stats.mappings -= 1;
        }
        if state == SlotState::Cached {
            self.stats.cache_bytes -= u64::try_from(capacity).expect("capacity");
        } else {
            self.stats.live_bytes -= u64::try_from(capacity).expect("capacity");
        }
        let slot = &mut self.slots[usize::try_from(handle.index).expect("slot下标")];
        slot.state = SlotState::Free;
        slot.generation = generation;
        slot.next = self.free_slot;
        self.free_slot = Some(handle.index);
        Ok(())
    }

    fn trim_pages(
        &mut self,
        index: usize,
        first: usize,
        count: usize,
        provider: &mut impl RangeProvider,
    ) -> Result<(), StackError> {
        let arena = self.arenas[index].as_mut().expect("arena");
        for span_index in first..first + count {
            let span = &mut arena.spans[span_index];
            if span.committed.iter().all(|word| *word == 0) {
                continue;
            }
            let reclaimable = |span: &Span, page: usize| {
                span.committed[page / 64] & (1_u64 << (page % 64)) != 0 && !span.page_occupied(page)
            };
            let mut page = 0;
            while page < STACK_SPAN_BYTES / PAGE {
                if !reclaimable(span, page) {
                    page += 1;
                    continue;
                }
                let first_page = page;
                page += 1;
                while page < STACK_SPAN_BYTES / PAGE && reclaimable(span, page) {
                    page += 1;
                }
                let offset = PAGE + span_index * STACK_SPAN_BYTES + first_page * PAGE;
                let bytes = (page - first_page) * PAGE;
                provider.decommit_pages(
                    arena.range,
                    u64::try_from(offset).expect("offset"),
                    u64::try_from(bytes).expect("页区间"),
                )?;
                for page in first_page..page {
                    span.committed[page / 64] &= !(1_u64 << (page % 64));
                }
                self.stats.committed_bytes -= u64::try_from(bytes).expect("页区间");
            }
        }
        Ok(())
    }

    fn release_empty_arena(
        &mut self,
        index: usize,
        provider: &mut impl RangeProvider,
    ) -> Result<(), StackError> {
        if self.arenas[index].as_ref().expect("arena").occupied != 0 {
            return Ok(());
        }
        let another = self.arenas.iter().enumerate().any(|(other, arena)| {
            other != index && arena.as_ref().is_some_and(|arena| arena.occupied == 0)
        });
        if another {
            let range = self.arenas[index].as_ref().expect("arena").range;
            provider.release(range)?;
            self.arenas[index] = None;
            self.stats.reserved_bytes -=
                u64::try_from(STACK_ARENA_BYTES + 2 * PAGE).expect("arena");
            self.stats.mappings -= 1;
        }
        Ok(())
    }
}
