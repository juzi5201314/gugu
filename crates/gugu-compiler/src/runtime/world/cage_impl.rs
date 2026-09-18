//! 世界侧的 checked pointer compression 接入：cage 配置、压缩根 map 与 FFI 闸门。
//!
//! 世界只经 `CompressionPlane` 的 checked 解码路径读写压缩引用：根槽在参与 owner 解析与
//! 标记前先解码，搬迁后按新 payload 重新编码；native 交接先取不移动租约再建立 FFI lease，
//! 压缩字永远不出世界边界。

use super::super::cage::{CompressionPlane, CompressionStats, ForeignPin};
use super::super::gc_metadata_schema::GcRootKindV1;
use super::super::slab::RawInvariant;
use super::RawWorld;

/// 根槽登记中压缩引用的判别值；与 `GcRootKindV1::CompressedRef` 的 `as u32` 一致。
const COMPRESSED_ROOT_TAG: u32 = GcRootKindV1::CompressedRef as u32;

impl RawWorld {
    /// 返回压缩平面；`configure_gc` 之前为 `None`。
    pub(crate) fn compression(&self) -> Option<&CompressionPlane> {
        self.cages.as_ref()
    }

    /// 返回压缩平面（可变）；未配置时失败。
    pub(crate) fn compression_mut(&mut self) -> Result<&mut CompressionPlane, RawInvariant> {
        self.cages
            .as_mut()
            .ok_or_else(|| RawInvariant::new("压缩平面未按契约配置"))
    }

    /// 返回压缩统计；未配置时为 `None`。
    pub(crate) fn compression_stats(&self) -> Option<CompressionStats> {
        self.cages.as_ref().map(CompressionPlane::stats)
    }

    /// 按已验证契约配置压缩平面：开启态预留 cage，关闭态不预留任何地址。
    pub(super) fn configure_compression(
        &mut self,
        contract: &super::super::CompressionRuntimeContract,
    ) -> Result<(), RawInvariant> {
        let mut plane = CompressionPlane::new(contract);
        plane.reserve(&mut self.provider)?;
        self.cages = Some(plane);
        Ok(())
    }

    /// 解码根槽：压缩根槽解出完整地址，其余槽原样返回。
    ///
    /// 解码失败（未登记 cage、过期 generation、越界 offset、非 canonical）直接成为
    /// `RuntimeInvariant`，绝不把编码字当作地址使用。
    pub(super) fn decode_root_slots(&mut self) -> Result<Vec<u64>, RawInvariant> {
        let mut decoded = self.managed_roots.clone();
        for slot in 0..decoded.len() {
            if self.managed_root_kinds[slot].0 != COMPRESSED_ROOT_TAG {
                continue;
            }
            if let Some(address) = self.compression_mut()?.decode(decoded[slot])? {
                decoded[slot] = address;
            }
        }
        Ok(decoded)
    }

    /// 回写根槽：压缩根槽按解码后的地址重新编码，其余槽写回地址。
    ///
    /// 根槽数组在解码与回写之间不得增长：`&mut [u64]` 切片不携带长度变化，任何长度漂移都是
    /// 世界不变量破损。
    pub(super) fn encode_root_slots(&mut self, decoded: &[u64]) -> Result<(), RawInvariant> {
        if decoded.len() != self.managed_roots.len() {
            return Err(RawInvariant::new("根槽数量在解码与回写之间变化"));
        }
        for slot in 0..decoded.len() {
            let value = decoded[slot];
            if self.managed_root_kinds[slot].0 != COMPRESSED_ROOT_TAG {
                self.managed_roots[slot] = value;
                continue;
            }
            // 压缩根槽要么是空值，要么必须落回某个 cage：对象搬到 cage 外（例如大对象或
            // 共享 payload）时不能悄悄留下完整地址，必须在这里暴露。
            let word = if value == 0 {
                0
            } else {
                self.compression_mut()?
                    .encode(value)
                    .map_err(|_| RawInvariant::new("压缩根指向的地址不在任何 cage 内"))?
            };
            self.managed_roots[slot] = word;
        }
        Ok(())
    }

    /// 登记一个指向 `address` 的压缩根，返回槽位下标。
    pub(crate) fn register_compressed_root(
        &mut self,
        type_index: u32,
        address: u64,
    ) -> Result<u32, RawInvariant> {
        if !self.compression().is_some_and(CompressionPlane::enabled) {
            return Err(RawInvariant::new("未启用 cage profile 时不能登记压缩根"));
        }
        let word = self.compression_mut()?.encode(address)?;
        let slot = self.register_managed_root(GcRootKindV1::CompressedRef, type_index)?;
        self.managed_roots[slot as usize] = word;
        Ok(slot)
    }

    /// 返回压缩根槽当前的编码字；诊断与测试读它确认搬迁后确实重新编码。
    pub(crate) fn compressed_root_word(&self, slot: u32) -> Result<u64, RawInvariant> {
        let index = usize::try_from(slot).expect("根槽下标适配宿主");
        let Some((kind, _)) = self.managed_root_kinds.get(index) else {
            return Err(RawInvariant::new("根槽下标越界"));
        };
        if *kind != COMPRESSED_ROOT_TAG {
            return Err(RawInvariant::new("根槽不是压缩根"));
        }
        self.managed_roots
            .get(index)
            .copied()
            .ok_or_else(|| RawInvariant::new("根槽下标越界"))
    }

    /// 为一个 managed 对象建立 FFI pin lease。
    ///
    /// 先取不移动租约：`pin_managed` 可能把对象从 nursery 晋升到 pinned arena，因此 FFI
    /// lease 必须绑定晋升后的地址，native 拿到的地址在 lease 结束前不再移动。
    pub(crate) fn pin_for_foreign(
        &mut self,
        owner: u32,
        address: u64,
    ) -> Result<ForeignPin, RawInvariant> {
        let (pinned, _) = self.pin_managed(owner, address)?;
        match self.compression_mut()?.pin_for_foreign(pinned) {
            Ok(pin) => Ok(pin),
            Err(error) => {
                self.unpin_managed(owner, pinned)?;
                Err(error)
            }
        }
    }

    /// 用活动 pin lease 把地址保存到 native 生命周期之外。
    pub(crate) fn save_for_foreign(&mut self, pin: ForeignPin) -> Result<u64, RawInvariant> {
        self.compression_mut()?.save_for_foreign(pin)
    }

    /// 释放 FFI pin lease 与对应的不移动租约。
    ///
    /// 先撤销 FFI lease 再 unpin：lease 不存在时不得连带解除 managed pin，否则 native 侧仍
    /// 持有的地址会在后续 cycle 被搬迁。
    pub(crate) fn release_for_foreign(
        &mut self,
        owner: u32,
        address: u64,
        pin: ForeignPin,
    ) -> Result<(), RawInvariant> {
        self.compression_mut()?.release_for_foreign(pin)?;
        self.unpin_managed(owner, address)?;
        Ok(())
    }

    /// 把一个 cage 内对象复制给 native code；copy 不延长地址生命周期，因此不要求 lease。
    pub(crate) fn copy_for_foreign(
        &mut self,
        address: u64,
        bytes: &[u8],
    ) -> Result<Vec<u8>, RawInvariant> {
        self.compression_mut()?.copy_for_foreign(address, bytes)
    }
}
