use super::Error;
use crate::lir::body::Body;

pub(super) fn validate(body: &Body) -> Result<(), Error> {
    for slot in &body.stack_slots {
        for (index, (offset, provenance)) in slot.roots.iter().enumerate() {
            if slot.align < 8
                || !offset.is_multiple_of(8)
                || offset.checked_add(8).is_none_or(|end| end > slot.bytes)
                || provenance.root_kind().is_none()
                || slot.roots[..index].contains(&(*offset, *provenance))
            {
                return Err(super::error(
                    "invalid rooted local offset/provenance/alignment",
                ));
            }
        }
    }
    let valid_position = |slot, block, position| {
        body.stack_slots.get(slot as usize).is_some()
            && body.blocks.get(block as usize).is_some_and(|b| {
                b.instructions
                    .end
                    .checked_sub(b.instructions.start)
                    .is_some_and(|end| position <= end)
            })
    };
    for event in &body.root_inits {
        if !valid_position(event.slot.0, event.block.0, event.position)
            || body
                .stack_slots
                .get(event.slot.index())
                .is_none_or(|slot| event.bit as usize >= slot.roots.len())
        {
            return Err(super::error("invalid root initialization event"));
        }
    }
    for event in &body.lifetimes {
        if !valid_position(event.slot.0, event.block.0, event.position) {
            return Err(super::error("invalid rooted-local lifetime event"));
        }
    }
    Ok(())
}
