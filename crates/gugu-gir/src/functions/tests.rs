use super::{close_over_callees, shared_owners};

fn digest(byte: u8) -> [u8; 32] {
    [byte; 32]
}

#[test]
fn closure_revokes_callers_of_uninstallable_callees_through_cycles() {
    // 0 ↔ 1 成环且 1 调用不可安装的 2；3 只调用无 body 的 4；5 调用闭合图之外的实例。
    let digests = [
        digest(0),
        digest(1),
        digest(2),
        digest(3),
        digest(4),
        digest(5),
    ];
    let bodyless = [false, false, false, false, true, false];
    let edges: [&[[u8; 32]]; 6] = [
        &[digest(1)],
        &[digest(0), digest(2)],
        &[],
        &[digest(4)],
        &[],
        &[digest(9)],
    ];
    let mut entry = [Some(0), Some(1), None, Some(3), None, Some(5)];
    close_over_callees(&digests, &bodyless, &edges, &mut entry);
    assert_eq!(entry, [None, None, None, Some(3), None, None]);
}

#[test]
fn owners_with_several_instances_are_shared() {
    let owners = [
        Some(3),
        Some(1),
        Some(3),
        None,
        Some(2),
        Some(3),
        Some(1),
        None,
    ];
    assert_eq!(shared_owners(&owners, 4), vec![false, true, false, true]);
}
