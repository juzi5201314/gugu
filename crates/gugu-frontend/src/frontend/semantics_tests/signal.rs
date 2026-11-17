use super::{accept_cases, accepts};

accept_cases! {
    std_signal_subscribe_facade_00: true,
        "use std.signal.{Signal, SignalEvent, SignalSubscription, subscribe, subscribe_with_capacity}\nfn main() { let arr = [Signal::Interrupt, Signal::Terminate]\n match subscribe(arr[0..]) { Ok(sub) => { match sub.recv() { Ok(e) => { _ = e.signal\n _ = e.occurrences }, Err(e) => { _ = e } }\n match sub.try_recv() { Ok(e) => { _ = e }, Err(e) => { _ = e } }\n _ = sub.dropped()\n sub.close() }, Err(e) => { _ = e } }\n _ = subscribe_with_capacity(arr[0..], uint(4)) }";
    std_signal_empty_slice_rejected_01: true,
        "use std.signal.{Signal, subscribe}\nfn main() { let arr: [Signal; 0] = []\n _ = subscribe(arr[0..]) }";
    std_signal_zero_capacity_rejected_02: true,
        "use std.signal.{Signal, subscribe_with_capacity}\nfn main() { let arr = [Signal::Interrupt]\n _ = subscribe_with_capacity(arr[0..], uint(0)) }";
    std_signal_subscription_type_checked_03: false,
        "use std.signal.{Signal, subscribe}\nfn main() { let arr = [Signal::Interrupt]\n match subscribe(arr[0..]) { Ok(sub) => { _ = sub + 1 }, Err(e) => { _ = e } } }";
    std_signal_error_variants_04: true,
        "use std.signal.{Error}\nfn main() { match Error::Closed { Error::Closed => (), Error::Unsupported => (), Error::InvalidSignal => (), Error::InvalidCapacity => (), Error::Empty => () } }";
}

#[test]
fn std_signal_rejects_unknown_variant_construction() {
    assert!(!accepts(
        "use std.signal.{Signal}\nfn main() { _ = Signal::Other }"
    ));
}
