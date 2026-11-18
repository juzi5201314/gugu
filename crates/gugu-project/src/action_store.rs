//! 持久 action record store：内容寻址、原子发布、LRU 回收、损坏隔离。
//!
//! 布局：`<root>/actions/v1/<key-hex>/{meta.json,payload.bin}`；发布经
//! `tmp/` 目录原子 rename，正在读写的 entry 不被回收：写中的 `tmp/` 目录
//! 与最近使用中窗口内的 entry 一律跳过。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gugu_base::hash::hash_domain;
use serde::{Deserialize, Serialize};

use super::{CacheError, invalid_error, io_error, write_file};

/// 存储 schema；编码变化时递增使旧 entry 失效。
const STORE_VERSION: u32 = 1;
/// entry 被视为「正在使用」的窗口：比写入/读取更宽的保守值。
const IN_USE_LEASE: Duration = Duration::from_secs(300);

const META_FILE: &str = "meta.toml";
const PAYLOAD_FILE: &str = "payload.bin";

/// 一条 action record 的元数据。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActionMeta {
    /// 存储 schema 版本。
    pub version: u32,
    /// action key 十六进制。
    pub key: String,
    /// record 种类：`image`、`stripped-image`、`summary`。
    pub kind: String,
    /// 目标三元组名。
    pub target: String,
    /// 入口/产物名。
    pub entry: String,
    /// payload 字节数。
    pub bytes_len: u64,
    /// payload 的 blake3 摘要（十六进制）。
    pub digest: String,
    /// 创建时间（Unix 秒）。
    pub created_at: u64,
    /// 最近使用时间（Unix 秒）；读取时刷新，是 LRU 的键。
    pub last_used_at: u64,
}

/// `verify` 的结果摘要。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VerifyReport {
    /// 检查过的 entry 数。
    pub checked: u32,
    /// 被隔离的损坏 entry 数。
    pub quarantined: u32,
}

/// `gc` 的结果摘要。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GcReport {
    /// 被回收的 entry 数。
    pub evicted: u32,
    /// 回收的字节数。
    pub freed_bytes: u64,
    /// 回收后剩余的字节数。
    pub remaining_bytes: u64,
}

/// action record store 句柄。
#[derive(Clone, Debug)]
pub struct ActionStore {
    root: PathBuf,
}

impl ActionStore {
    /// `root` 是编译缓存根（默认 `default_cache_root()`），store 落在 `actions/v1/`。
    pub fn new(cache_root: impl Into<PathBuf>) -> Self {
        Self {
            root: cache_root.into().join("actions").join("v1"),
        }
    }

    /// store 根目录（`gugu cache dir` 打印这个）。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 原子发布一条 record：先写临时目录再 rename；同 key 幂等覆盖。
    pub fn publish(
        &self,
        key: &str,
        kind: &str,
        target: &str,
        entry: &str,
        payload: &[u8],
    ) -> Result<PathBuf, CacheError> {
        let now = unix_now();
        let meta = ActionMeta {
            version: STORE_VERSION,
            key: key.to_owned(),
            kind: kind.to_owned(),
            target: target.to_owned(),
            entry: entry.to_owned(),
            bytes_len: payload.len() as u64,
            digest: hex(&hash_domain("gugu-action-payload-v1", payload)),
            created_at: now,
            last_used_at: now,
        };
        // 唯一临时目录：同秒并发发布互不踩踏。
        let tmp = self
            .root
            .join("tmp")
            .join(format!("publish-{}", super::temporary_suffix()));
        fs::create_dir_all(&tmp).map_err(|error| io_error(&tmp, error))?;
        write_file(&tmp.join(META_FILE), meta_json(&meta)?.as_bytes())?;
        write_file(&tmp.join(PAYLOAD_FILE), payload)?;
        let destination = self.entry_dir(key);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|e| io_error(parent, e))?;
        }
        // 旧 entry 先原位移走再发布新 entry：重命名失败时回滚，读者至多看到
        // 一次 miss（按未命中重算），不会读到半更新状态。
        let replaced = destination
            .parent()
            .unwrap_or(&self.root)
            .join(format!("replaced-{}", super::temporary_suffix()));
        let had_previous = destination.exists();
        if had_previous {
            fs::rename(&destination, &replaced).map_err(|e| io_error(&destination, e))?;
        }
        if let Err(error) = fs::rename(&tmp, &destination) {
            if had_previous {
                let _ = fs::rename(&replaced, &destination);
            }
            return Err(io_error(&destination, error));
        }
        if had_previous {
            let _ = fs::remove_dir_all(&replaced);
        }
        Ok(destination)
    }

    /// 读取 record 并刷新 last_used_at；digest 不符返回损坏错误（由调用方隔离）。
    pub fn read(&self, key: &str) -> Result<Option<(ActionMeta, Vec<u8>)>, CacheError> {
        let dir = self.entry_dir(key);
        if !dir.is_dir() {
            return Ok(None);
        }
        let meta = self.read_meta(&dir)?;
        let payload =
            fs::read(dir.join(PAYLOAD_FILE)).map_err(|e| io_error(&dir.join(PAYLOAD_FILE), e))?;
        if hex(&hash_domain("gugu-action-payload-v1", &payload)) != meta.digest {
            return Err(invalid_error(&dir, "action record payload 摘要不符"));
        }
        let mut meta = meta;
        meta.last_used_at = unix_now();
        write_file(&dir.join(META_FILE), meta_json(&meta)?.as_bytes())?;
        Ok(Some((meta, payload)))
    }

    /// 全量校验：meta 解析失败、payload 缺失、摘要不符的 entry 移入 quarantine。
    pub fn verify(&self) -> Result<VerifyReport, CacheError> {
        let mut report = VerifyReport::default();
        for dir in self.entry_dirs()? {
            report.checked += 1;
            let bad = match self.read_meta(&dir) {
                Err(_) => true,
                Ok(meta) => {
                    let payload_path = dir.join(PAYLOAD_FILE);
                    match fs::read(&payload_path) {
                        Err(_) => true,
                        Ok(payload) => {
                            hex(&hash_domain("gugu-action-payload-v1", &payload)) != meta.digest
                        }
                    }
                }
            };
            if bad {
                self.quarantine(&dir)?;
                report.quarantined += 1;
            }
        }
        Ok(report)
    }

    /// LRU 回收：按 last_used_at 升序淘汰至 `max_bytes` 以下；
    /// `tmp/` 写中目录与在使用窗口内的 entry 不回收。
    pub fn gc(&self, max_bytes: u64) -> Result<GcReport, CacheError> {
        let mut report = GcReport::default();
        let mut entries = Vec::new();
        for dir in self.entry_dirs()? {
            if let Ok(meta) = self.read_meta(&dir) {
                entries.push((meta.last_used_at, dir));
            }
        }
        report.remaining_bytes = self.total_bytes()?;
        if report.remaining_bytes <= max_bytes {
            return Ok(report);
        }
        entries.sort_by_key(|(used, _)| *used);
        let lease_cutoff = unix_now().saturating_sub(IN_USE_LEASE.as_secs());
        for (used, dir) in entries {
            if report.remaining_bytes <= max_bytes {
                break;
            }
            if used >= lease_cutoff {
                continue; // 在使用窗口内，不回收。
            }
            let size = dir_size(&dir);
            fs::remove_dir_all(&dir).map_err(|e| io_error(&dir, e))?;
            report.evicted += 1;
            report.freed_bytes += size;
            report.remaining_bytes = report.remaining_bytes.saturating_sub(size);
        }
        Ok(report)
    }

    /// 删除全部 action record（保留 `tmp/` 空壳）。
    pub fn clean(&self) -> Result<u64, CacheError> {
        let freed = self.total_bytes()?;
        for dir in self.entry_dirs()? {
            fs::remove_dir_all(&dir).map_err(|e| io_error(&dir, e))?;
        }
        Ok(freed)
    }

    /// 容量上限：`GUGU_ACTION_CACHE_MAX_MB`（MiB）覆盖，默认 512 MiB。
    pub fn capacity() -> u64 {
        std::env::var("GUGU_ACTION_CACHE_MAX_MB")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .map_or(512 * 1024 * 1024, |mb| mb * 1024 * 1024)
    }

    /// 当前全部 entry 的 meta（CLI verify/list 用）。
    pub fn entries(&self) -> Result<Vec<ActionMeta>, CacheError> {
        let mut metas = Vec::new();
        for dir in self.entry_dirs()? {
            if let Ok(meta) = self.read_meta(&dir) {
                metas.push(meta);
            }
        }
        metas.sort_by(|a, b| a.last_used_at.cmp(&b.last_used_at));
        Ok(metas)
    }

    fn entry_dir(&self, key: &str) -> PathBuf {
        self.root.join("entries").join(key)
    }

    fn entry_dirs(&self) -> Result<Vec<PathBuf>, CacheError> {
        let entries = self.root.join("entries");
        if !entries.is_dir() {
            return Ok(Vec::new());
        }
        let mut dirs = Vec::new();
        for entry in fs::read_dir(&entries).map_err(|e| io_error(&entries, e))? {
            let path = entry.map_err(|e| io_error(&entries, e))?.path();
            if path.is_dir() {
                dirs.push(path);
            }
        }
        Ok(dirs)
    }

    fn read_meta(&self, dir: &Path) -> Result<ActionMeta, CacheError> {
        let meta_path = dir.join(META_FILE);
        let text = fs::read_to_string(&meta_path).map_err(|e| io_error(&meta_path, e))?;
        let meta: ActionMeta = toml::from_str(&text)
            .map_err(|e| invalid_error(&meta_path, format!("meta 解析失败：{e}")))?;
        if meta.version != STORE_VERSION {
            return Err(invalid_error(&meta_path, "meta 版本不受支持"));
        }
        Ok(meta)
    }

    fn quarantine(&self, dir: &Path) -> Result<(), CacheError> {
        let quarantine = self.root.join("quarantine");
        fs::create_dir_all(&quarantine).map_err(|e| io_error(&quarantine, e))?;
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_owned().to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        fs::rename(dir, quarantine.join(format!("{name}-{:x}", unix_now())))
            .map_err(|e| io_error(dir, e))?;
        Ok(())
    }

    fn total_bytes(&self) -> Result<u64, CacheError> {
        let mut total = 0;
        for dir in self.entry_dirs()? {
            total += dir_size(&dir);
        }
        Ok(total)
    }
}

fn meta_json(meta: &ActionMeta) -> Result<String, CacheError> {
    toml::to_string_pretty(meta).map_err(|e| invalid_error(Path::new(META_FILE), e.to_string()))
}

fn dir_size(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| entry.metadata().ok())
                .map(|meta| meta.len())
                .sum()
        })
        .unwrap_or(0)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store(dir: &TempDir) -> ActionStore {
        ActionStore::new(dir.path())
    }

    fn age(dir: &Path, last_used_at: u64) {
        let path = dir.join(META_FILE);
        let mut meta: ActionMeta = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        meta.last_used_at = last_used_at;
        fs::write(&path, toml::to_string_pretty(&meta).unwrap()).unwrap();
    }

    #[test]
    fn publish_and_read_roundtrip() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store
            .publish("aa", "image", "x86_64-linux", "main", b"payload")
            .unwrap();
        let (meta, bytes) = store.read("aa").unwrap().unwrap();
        assert_eq!(bytes, b"payload");
        assert_eq!(meta.kind, "image");
        assert_eq!(meta.entry, "main");
    }

    #[test]
    fn read_detects_digest_mismatch() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store
            .publish("aa", "image", "x86_64-linux", "main", b"payload")
            .unwrap();
        fs::write(store.root().join("entries/aa/payload.bin"), b"tampered").unwrap();
        assert!(store.read("aa").is_err());
    }

    #[test]
    fn verify_quarantines_corrupt_entries() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store
            .publish("good", "image", "x86_64-linux", "main", b"ok")
            .unwrap();
        store
            .publish("bad", "image", "x86_64-linux", "main", b"ok")
            .unwrap();
        fs::write(store.root().join("entries/bad/payload.bin"), b"bad").unwrap();
        let report = store.verify().unwrap();
        assert_eq!(report.checked, 2);
        assert_eq!(report.quarantined, 1);
        assert!(store.read("good").unwrap().is_some());
        assert_eq!(store.entries().unwrap().len(), 1);
    }

    #[test]
    fn gc_evicts_oldest_beyond_capacity_but_keeps_in_use() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store
            .publish("old", "image", "x86_64-linux", "a", b"x")
            .unwrap();
        store
            .publish("fresh", "image", "x86_64-linux", "b", b"y")
            .unwrap();
        age(&store.root().join("entries/old"), 0);
        let report = store.gc(0).unwrap();
        assert_eq!(report.evicted, 1);
        assert!(store.read("fresh").unwrap().is_some());
        assert!(store.read("old").unwrap().is_none());
    }

    #[test]
    fn clean_removes_all_entries() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store
            .publish("a", "image", "x86_64-linux", "a", b"x")
            .unwrap();
        store
            .publish("b", "stripped-image", "x86_64-linux", "a", b"y")
            .unwrap();
        store.clean().unwrap();
        assert!(store.entries().unwrap().is_empty());
    }
}
