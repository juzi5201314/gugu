use super::*;

pub(super) fn spawn(b: &mut Builder) {
    b.memory("ldr.X.Unsigned", PACKET, REQ, A);
    b.add(Reg::x(0), SP, 0);
    n(b, 1, 524288);
    syscall(b, 59);
    compare(b, Reg::x(0), 0);
    let pipe_failed = branch_cond(b, 11);
    n(b, 0, 17);
    for i in 1..5 {
        n(b, i, 0);
    }
    syscall(b, 220);
    compare(b, Reg::x(0), 0);
    let child = branch_cond(b, 0);
    let clone_failed = branch_cond(b, 11);
    parent(b);
    let parent_done = branch(b);
    patch_cond(b, clone_failed, 11);
    b.add(RESULT, Reg::x(0), 0);
    for offset in [0, 4] {
        b.memory("ldr.W.Unsigned", Reg::w(0), SP, offset);
        syscall(b, 57);
    }
    b.add(Reg::x(0), RESULT, 0);
    let error = branch(b);
    patch_cond(b, child, 0);
    child_exec(b);
    patch_cond(b, pipe_failed, 11);
    b.patch_branch(error, "b", &[], &[]);
    result(b);
    b.patch_branch(parent_done, "b", &[], &[]);
}

fn parent(b: &mut Builder) {
    b.add(CHILD, Reg::x(0), 0);
    b.memory("ldr.W.Unsigned", Reg::w(0), SP, 4);
    syscall(b, 57);
    let again = b.offset();
    b.memory("ldr.W.Unsigned", Reg::w(0), SP, 0);
    b.add(Reg::x(1), SP, 8);
    n(b, 2, 4);
    syscall(b, 63);
    retry_if_interrupted(b, again);
    b.add(RESULT, Reg::x(0), 0);
    b.memory("ldr.W.Unsigned", Reg::w(0), SP, 0);
    syscall(b, 57);
    compare(b, RESULT, 0);
    let executed = branch_cond(b, 0);
    let reported = branch_cond(b, 12);
    b.add(Reg::x(0), CHILD, 0);
    n(b, 1, 9);
    syscall(b, 129);
    patch_cond(b, reported, 12);
    reap(b);
    compare(b, RESULT, 0);
    let read_failed = branch_cond(b, 11);
    b.memory("ldr.W.Unsigned", Reg::w(0), SP, 8);
    b.memory("str.X.Unsigned", Reg::x(0), REQ, E);
    b.memory("str.X.Unsigned", Reg::x(0), PACKET, 72);
    let done = branch(b);
    patch_cond(b, read_failed, 11);
    b.add(Reg::x(0), RESULT, 0);
    result(b);
    let failed = branch(b);
    patch_cond(b, executed, 0);
    for offset in [56, 64] {
        b.memory("str.X.Unsigned", CHILD, PACKET, offset);
    }
    b.memory("str.X.Unsigned", CHILD, REQ, V);
    b.patch_branch(done, "b", &[], &[]);
    b.patch_branch(failed, "b", &[], &[]);
}

fn reap(b: &mut Builder) {
    let again = b.offset();
    b.add(Reg::x(0), CHILD, 0);
    n(b, 1, 0);
    n(b, 2, 0);
    n(b, 3, 0);
    syscall(b, 260);
    retry_if_interrupted(b, again);
}

fn child_exec(b: &mut Builder) {
    b.memory("ldr.W.Unsigned", Reg::w(0), SP, 0);
    syscall(b, 57);
    b.memory("ldr.X.Unsigned", Reg::x(0), PACKET, 24);
    syscall(b, 49);
    let mut errors = vec![negative_result(b)];
    for (fd, offset) in [(0, 32), (1, 40), (2, 48)] {
        b.memory("ldr.X.Unsigned", Reg::x(0), PACKET, offset);
        n(b, 1, fd);
        n(b, 2, 0);
        syscall(b, 24);
        errors.push(negative_result(b));
    }
    for offset in [32, 40, 48] {
        b.memory("ldr.X.Unsigned", Reg::x(0), PACKET, offset);
        syscall(b, 57);
    }
    for offset in [32, 40, 48, 56] {
        b.memory("str.X.Unsigned", ZERO, SP, offset);
    }
    n(b, 0, 13);
    b.add(Reg::x(1), SP, 32);
    n(b, 2, 0);
    n(b, 3, 8);
    syscall(b, 134);
    errors.push(negative_result(b));
    for (r, offset) in [(0, 0), (1, 8), (2, 16)] {
        b.memory("ldr.X.Unsigned", Reg::x(r), PACKET, offset);
    }
    syscall(b, 221);
    for at in errors {
        patch_cond(b, at, 11);
    }
    child_error(b);
}

fn negative_result(b: &mut Builder) -> u32 {
    compare(b, Reg::x(0), 0);
    branch_cond(b, 11)
}

fn child_error(b: &mut Builder) {
    negate(b, Reg::x(0), Reg::x(0));
    b.memory("str.X.Unsigned", Reg::x(0), SP, 8);
    let again = b.offset();
    b.memory("ldr.W.Unsigned", Reg::w(0), SP, 4);
    b.add(Reg::x(1), SP, 8);
    n(b, 2, 4);
    syscall(b, 64);
    retry_if_interrupted(b, again);
    n(b, 0, 127);
    syscall(b, 93);
    b.emit("brk", [imm(0)]);
}
