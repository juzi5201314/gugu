//! Architecture-neutral root bitmap construction.
pub(crate) fn empty_bitmaps(slot_count: u32) -> [Vec<u64>; 5] {
    let bytes = slot_count.div_ceil(8) as usize;
    let lanes = bytes.div_ceil(8).max(1);
    std::array::from_fn(|_| vec![0; lanes])
}
