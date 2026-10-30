//! runtime 层：契约与 verifier 在 `gugu-runtime-abi`，world 参照实现在 `gugu-runtime-world`；
//! 这里只保留需要整条管线的 runtime 回归。

pub use gugu_runtime_abi::*;

#[cfg(all(test, feature = "slow-tests"))]
mod blocking_io_tests;
#[cfg(test)]
mod cage_tests;
#[cfg(all(test, feature = "slow-tests"))]
mod collections_tests;
#[cfg(test)]
mod compression_tests;
#[cfg(all(test, feature = "slow-tests"))]
mod coroutine_tests;
#[cfg(all(test, feature = "slow-tests"))]
mod foreign_tests;
#[cfg(test)]
mod heap_tests;
#[cfg(all(test, feature = "slow-tests"))]
mod platform_tests;
#[cfg(test)]
mod provenance_tests;
#[cfg(test)]
mod roots_tests;
#[cfg(test)]
mod routing_tests;
#[cfg(test)]
mod shared_heap_tests;
#[cfg(test)]
#[allow(unused_imports)]
mod sync_tests;
#[cfg(test)]
mod tests;
#[cfg(all(test, feature = "slow-tests"))]
mod text_tests;
#[cfg(test)]
#[allow(unused_imports)]
mod wait_tests;
