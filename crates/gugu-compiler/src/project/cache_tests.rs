use std::collections::BTreeMap;

use flate2::{Compression, write::GzEncoder};
use sha2::{Digest, Sha256};
use tar::Builder;
use tempfile::TempDir;

use super::super::super::{
    DependencyDomain, LockGraph, LockedPackage, PackageId, PackageSource, Version,
};
use super::*;

fn registry_id() -> PackageId {
    PackageId::new(
        "acme/data",
        Version::new(1, 2, 3),
        PackageSource::Registry {
            registry: "public".to_owned(),
        },
    )
}

fn files() -> PackageFiles {
    PackageFiles::new(BTreeMap::from([
        (
            "gugu.toml".to_owned(),
            b"[package]\nname = \"data\"\n".to_vec(),
        ),
        ("src/lib.gg".to_owned(), b"fn value() {}\n".to_vec()),
    ]))
    .expect("valid package files")
}

fn lock_for(input: &DependencyInput) -> LockGraph {
    LockGraph {
        version: 1,
        packages: vec![LockedPackage {
            id: input.package().clone(),
            checksum: Some(input.checksum()),
            dependencies: Vec::new(),
            features: BTreeMap::from([(DependencyDomain::Normal, Vec::new())]),
        }],
    }
}

#[test]
fn package_checksum_uses_sorted_canonical_content_stream() {
    let package = files();
    let mut expected = Sha256::new();
    expected.update(b"gugu-package-v1\n");
    for (path, bytes) in package.files() {
        expected.update((path.len() as u64).to_be_bytes());
        expected.update(path.as_bytes());
        expected.update((bytes.len() as u64).to_be_bytes());
        expected.update(bytes);
    }
    assert_eq!(package.checksum(), hex_encode(&expected.finalize()));
}

#[test]
fn archive_is_verified_before_cache_publish_and_can_be_replayed() {
    let root = TempDir::new().expect("cache root");
    let input_files = files();
    let checksum = input_files.checksum();
    let mut encoded = Vec::new();
    {
        let encoder = GzEncoder::new(&mut encoded, Compression::default());
        let mut builder = Builder::new(encoder);
        for (path, bytes) in input_files.files() {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, path, bytes.as_slice())
                .expect("append archive entry");
        }
        let encoder = builder.into_inner().expect("finish tar");
        encoder.finish().expect("finish gzip");
    }
    let cache = DependencyCache::new(root.path());
    let input = cache
        .store_archive(registry_id(), &checksum, &encoded)
        .expect("archive stores");
    assert_eq!(input.checksum(), checksum);
    let replay = cache
        .load(&registry_id(), Some(&checksum))
        .expect("cached input loads");
    assert_eq!(replay, input);
}

#[test]
fn checksum_failure_does_not_publish_an_archive() {
    let root = TempDir::new().expect("cache root");
    let mut encoded = Vec::new();
    {
        let encoder = GzEncoder::new(&mut encoded, Compression::default());
        let mut builder = Builder::new(encoder);
        let bytes = b"not the declared package";
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, "gugu.toml", bytes.as_slice())
            .expect("append archive entry");
        let encoder = builder.into_inner().expect("finish tar");
        encoder.finish().expect("finish gzip");
    }
    let cache = DependencyCache::new(root.path());
    let error = cache
        .store_archive(registry_id(), &"0".repeat(64), &encoded)
        .expect_err("checksum mismatch");
    assert!(matches!(error, CacheError::Checksum { .. }));
    assert!(!root.path().join("dependencies/v1/packages").exists());
}

#[test]
fn corrupted_cached_entry_is_quarantined() {
    let root = TempDir::new().expect("cache root");
    let input = DependencyInput::new(registry_id(), files());
    let cache = DependencyCache::new(root.path());
    cache.store(&input).expect("cache stores");
    let entry = root
        .path()
        .join("dependencies/v1/packages")
        .read_dir()
        .expect("package cache directory")
        .next()
        .expect("one entry")
        .expect("entry read")
        .path();
    std::fs::write(entry.join("files/src/lib.gg"), b"tampered").expect("tamper cache");
    let error = cache
        .load(&registry_id(), Some(&input.checksum()))
        .expect_err("tampered cache fails");
    assert!(matches!(error, CacheError::Corrupt { .. }));
    assert!(!entry.exists());
    assert!(
        root.path()
            .join("dependencies/v1/quarantine")
            .read_dir()
            .expect("quarantine directory")
            .next()
            .is_some()
    );
}

#[test]
fn vendor_materialization_and_lock_mapping_are_deterministic() {
    let root = TempDir::new().expect("workspace root");
    let cache_root = TempDir::new().expect("cache root");
    let input = DependencyInput::new(registry_id(), files());
    let lock = lock_for(&input);
    let vendor = root.path().join("vendor");
    materialize_vendor(&vendor, &lock, std::slice::from_ref(&input)).expect("vendor writes");
    let replay = prepare_dependency_inputs(
        &lock,
        &DependencyCache::new(cache_root.path()),
        root.path(),
        &vendor,
        CachePolicy {
            vendor: true,
            ..CachePolicy::default()
        },
    )
    .expect("vendor replays");
    assert_eq!(replay, vec![input]);
    let text = std::fs::read_to_string(vendor.join(VENDOR_RECORD)).expect("vendor record");
    assert!(text.contains("registry+public"));
    assert!(text.contains(&lock.packages[0].checksum.clone().expect("checksum")));
}

#[test]
fn vendor_content_tampering_is_rejected_without_cache_fallback() {
    let root = TempDir::new().expect("workspace root");
    let input = DependencyInput::new(registry_id(), files());
    let lock = lock_for(&input);
    let vendor = root.path().join("vendor");
    materialize_vendor(&vendor, &lock, std::slice::from_ref(&input)).expect("vendor writes");
    let directory = std::fs::read_dir(&vendor)
        .expect("vendor directory")
        .filter_map(Result::ok)
        .find(|entry| entry.file_name() != VENDOR_RECORD)
        .expect("package directory")
        .path();
    std::fs::write(directory.join("src/lib.gg"), b"tampered").expect("tamper vendor");
    let error = prepare_dependency_inputs(
        &lock,
        &DependencyCache::new(root.path().join("cache")),
        root.path(),
        &vendor,
        CachePolicy {
            vendor: true,
            offline: true,
            ..CachePolicy::default()
        },
    )
    .expect_err("tampered vendor fails");
    assert!(matches!(error, CacheError::Checksum { .. }));
}

#[test]
fn action_key_contains_all_inputs_and_is_order_independent() {
    let mut first = ActionInputs::new(b"compiler", "x86_64-linux", "x86_64-linux", "bin");
    first.set_harness(true);
    first.add_feature("default");
    first.add_feature("cli");
    first.add_instrumentation("coverage");
    first.add_source("src/main.gg", b"fn main() {}\n");
    first.add_embedded_file("assets/data", b"data");
    first.set_lock_graph(b"lock");
    first.set_cfg("target_os", "linux");
    first.set_native_link_metadata(b"link");

    let mut second = ActionInputs::new(b"compiler", "x86_64-linux", "x86_64-linux", "bin");
    second.set_harness(true);
    second.add_feature("cli");
    second.add_feature("default");
    second.add_instrumentation("coverage");
    second.add_source("src/main.gg", b"fn main() {}\n");
    second.add_embedded_file("assets/data", b"data");
    second.set_lock_graph(b"lock");
    second.set_cfg("target_os", "linux");
    second.set_native_link_metadata(b"link");
    assert_eq!(first.key(), second.key());

    second.add_source("src/main.gg", b"fn changed() {}\n");
    assert_ne!(first.key(), second.key());
    assert_eq!(first.key().hex().len(), 64);
}

#[test]
fn target_view_rejects_traversal_and_writes_artifacts_atomically() {
    let root = TempDir::new().expect("target root");
    let view = TargetView::new(root.path());
    let artifact = TargetArtifact::new("bin/app", b"image".to_vec()).expect("artifact");
    let paths = view
        .materialize("x86_64-linux", &[artifact])
        .expect("materialize target");
    assert_eq!(std::fs::read(&paths[0]).expect("read artifact"), b"image");
    assert!(TargetArtifact::new("../escape", Vec::new()).is_err());
    assert!(view.materialize("../escape", &[]).is_err());
    assert!(!root.path().join("x86_64-linux/bin/app.tmp").exists());
}

#[test]
fn frozen_policy_is_explicitly_locked_and_offline() {
    assert_eq!(
        CachePolicy::frozen(),
        CachePolicy {
            offline: true,
            locked: true,
            vendor: false,
        }
    );
}
