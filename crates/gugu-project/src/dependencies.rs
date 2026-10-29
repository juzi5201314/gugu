#[path = "dependency_manifest.rs"]
mod dependency_manifest;
#[path = "dependency_model.rs"]
mod dependency_model;
#[path = "lock.rs"]
mod lock;
#[path = "resolver.rs"]
mod resolver;
#[path = "semver.rs"]
mod semver;

pub use dependency_model::*;
pub(crate) use resolver::resolve_project;
pub use semver::{Version, VersionReq};

#[cfg(test)]
#[path = "dependencies_tests.rs"]
mod tests;
