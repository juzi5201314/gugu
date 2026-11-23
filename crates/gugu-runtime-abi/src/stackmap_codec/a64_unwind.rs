//! A64 safepoint-only SP-based CFA/LR recovery, paired with a v5 stack-map table.
//! 站点之后接 landing 记录（`pc_start..pc_end` 调用点范围 → `landing_pc` 落地入口 +
//! `cleanup` 链下标），与 x64 unwind 节的 `LandingRecord` 字段一致。
use super::{CodeLayout, DecodedSection, StackMapArchitecture};
use crate::model::RawModelError;

const MAGIC: &[u8; 8] = b"GUGUCF01";
const HEADER: usize = 40;
const RECORD: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Site {
    pub pc_offset: u32,
    pub entry: bool,
}

/// invoke 调用点的 panic 落地记录；`cleanup` 语义与 x64 `LandingRecord` 相同
///（`u32::MAX` 表示只恢复传播）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Landing {
    pub pc_start: u32,
    pub pc_end: u32,
    pub landing_pc: u32,
    pub cleanup: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Table {
    pub code_rva: u64,
    pub code_size: u32,
    pub frame_size: u32,
    pub fp_offset: u32,
    pub sites: Vec<Site>,
    pub landings: Vec<Landing>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Caller {
    pub sp: u64,
    pub pc: u64,
}

fn fail(reason: &str) -> RawModelError {
    RawModelError::new(reason)
}
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("verified CFA field"))
}

pub fn encode(
    code: CodeLayout,
    fp_offset: u32,
    sites: &[Site],
    landings: &[Landing],
) -> Result<Vec<u8>, RawModelError> {
    let mut out = MAGIC.to_vec();
    for value in [183, code.code_size, code.frame_size, fp_offset] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&code.code_rva.to_le_bytes());
    out.extend_from_slice(
        &u32::try_from(sites.len())
            .map_err(|_| fail("too many CFA sites"))?
            .to_le_bytes(),
    );
    out.extend_from_slice(
        &u32::try_from(landings.len())
            .map_err(|_| fail("too many A64 landings"))?
            .to_le_bytes(),
    );
    for site in sites {
        let lr = if site.entry {
            10
        } else {
            fp_offset
                .checked_add(8)
                .ok_or_else(|| fail("LR offset overflow"))?
        };
        for value in [
            site.pc_offset,
            if site.entry { 0 } else { code.frame_size },
            lr,
            u32::from(site.entry),
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    let mut previous = 0_u32;
    for landing in landings {
        if landing.pc_start < previous
            || landing.pc_start >= landing.pc_end
            || landing.pc_end > code.code_size
            || landing.landing_pc >= code.code_size
        {
            return Err(fail("invalid A64 landing range/order"));
        }
        previous = landing.pc_end;
        for value in [
            landing.pc_start,
            landing.pc_end,
            landing.landing_pc,
            landing.cleanup,
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    decode(&out)?;
    Ok(out)
}

pub fn decode(bytes: &[u8]) -> Result<Table, RawModelError> {
    if bytes.len() < HEADER || &bytes[..8] != MAGIC || u32_at(bytes, 8) != 183 {
        return Err(fail("invalid A64 CFA header"));
    }
    let count = u32_at(bytes, 32) as usize;
    let landing_count = u32_at(bytes, 36) as usize;
    if count
        .checked_add(landing_count)
        .and_then(|n| n.checked_mul(RECORD))
        .and_then(|n| n.checked_add(HEADER))
        != Some(bytes.len())
    {
        return Err(fail("invalid CFA section length"));
    }
    let code_size = u32_at(bytes, 12);
    let frame_size = u32_at(bytes, 16);
    let fp_offset = u32_at(bytes, 20);
    let code_rva = u64::from_le_bytes(bytes[24..32].try_into().expect("CFA RVA"));
    if !code_rva.is_multiple_of(4)
        || !code_size.is_multiple_of(4)
        || code_rva.checked_add(u64::from(code_size)).is_none()
        || !frame_size.is_multiple_of(16)
        || (frame_size != 0
            && (!fp_offset.is_multiple_of(16)
                || fp_offset.checked_add(16).is_none_or(|end| end > frame_size)))
        || frame_size == 0 && fp_offset != 0
    {
        return Err(fail("invalid A64 CFA code/frame alignment"));
    }
    let mut sites = Vec::with_capacity(count);
    let mut previous = 0;
    for index in 0..count {
        let at = HEADER + index * RECORD;
        let pc_offset = u32_at(bytes, at);
        let cfa = u32_at(bytes, at + 4);
        let lr = u32_at(bytes, at + 8);
        let flags = u32_at(bytes, at + 12);
        if pc_offset <= previous
            || pc_offset >= code_size
            || !pc_offset.is_multiple_of(4)
            || flags > 1
        {
            return Err(fail("invalid CFA return PC/order/flags"));
        }
        let entry = flags == 1;
        if entry && (cfa != 0 || lr != 10)
            || !entry
                && (frame_size == 0 || cfa != frame_size || fp_offset.checked_add(8) != Some(lr))
        {
            return Err(fail("invalid SP-based CFA/LR rule"));
        }
        sites.push(Site { pc_offset, entry });
        previous = pc_offset;
    }
    let mut landings = Vec::with_capacity(landing_count);
    let mut lo = 0_u32;
    for index in 0..landing_count {
        let at = HEADER + (count + index) * RECORD;
        let landing = Landing {
            pc_start: u32_at(bytes, at),
            pc_end: u32_at(bytes, at + 4),
            landing_pc: u32_at(bytes, at + 8),
            cleanup: u32_at(bytes, at + 12),
        };
        if landing.pc_start < lo
            || landing.pc_start >= landing.pc_end
            || landing.pc_end > code_size
            || landing.landing_pc >= code_size
        {
            return Err(fail("invalid A64 landing range/order"));
        }
        lo = landing.pc_end;
        landings.push(landing);
    }
    Ok(Table {
        code_rva,
        code_size,
        frame_size,
        fp_offset,
        sites,
        landings,
    })
}

impl Table {
    pub fn verify_joint(&self, maps: &DecodedSection) -> Result<(), RawModelError> {
        if maps.architecture != StackMapArchitecture::Aarch64 || maps.functions.len() != 1 {
            return Err(fail("CFA table requires one A64 map function"));
        }
        let function = &maps.functions[0];
        if function.code_rva != self.code_rva
            || function.code_size != self.code_size
            || function.frame_size != self.frame_size
            || function.unwind_index != 0
            || maps.safepoints.len() != self.sites.len()
        {
            return Err(fail("joint CFA/stack-map function mismatch"));
        }
        for (site, map) in self.sites.iter().zip(&maps.safepoints) {
            if site.pc_offset != map.pc_offset || site.entry != (map.kind == 4) {
                return Err(fail("joint CFA/stack-map return PC mismatch"));
            }
        }
        Ok(())
    }

    pub fn caller(
        &self,
        return_pc: u64,
        sp: u64,
        frame: &[u64],
        registers: &[u64],
    ) -> Result<Caller, RawModelError> {
        let offset = return_pc
            .checked_sub(self.code_rva)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| fail("CFA return PC outside function"))?;
        let index = self
            .sites
            .binary_search_by_key(&offset, |site| site.pc_offset)
            .map_err(|_| fail("CFA requires exact return PC"))?;
        let entry = self.sites[index].entry;
        let pc = if entry {
            registers.get(10)
        } else {
            frame.get((self.fp_offset / 8 + 1) as usize)
        }
        .copied()
        .ok_or_else(|| fail("missing CFA frame/register snapshot"))?;
        let sp = sp
            .checked_add(if entry { 0 } else { u64::from(self.frame_size) })
            .ok_or_else(|| fail("CFA stack pointer overflow"))?;
        Ok(Caller { sp, pc })
    }
}
