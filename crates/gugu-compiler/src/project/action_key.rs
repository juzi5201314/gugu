use std::collections::{BTreeMap, BTreeSet};

/// 影响编译 action 身份的完整输入集合。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ActionInputs {
    compiler_identity: Vec<u8>,
    host: String,
    target: String,
    target_kind: String,
    harness: bool,
    instrumentation: BTreeSet<String>,
    features: BTreeSet<String>,
    lock_graph: Vec<u8>,
    source_inputs: BTreeMap<String, [u8; 32]>,
    embedded_files: BTreeMap<String, [u8; 32]>,
    macro_inputs: BTreeMap<String, [u8; 32]>,
    macro_budget: Vec<u8>,
    comptime_registry: Vec<u8>,
    type_universe: Vec<u8>,
    late_constants: Vec<u8>,
    public_summaries: BTreeMap<String, [u8; 32]>,
    build_inputs: BTreeMap<String, [u8; 32]>,
    build_outputs: BTreeMap<String, [u8; 32]>,
    cfg: BTreeMap<String, String>,
    native_link_metadata: Vec<u8>,
}

impl ActionInputs {
    /// 创建包含 compiler identity、宿主、目标和 target 种类的输入集合。
    pub fn new(
        compiler_identity: impl AsRef<[u8]>,
        host: impl Into<String>,
        target: impl Into<String>,
        target_kind: impl Into<String>,
    ) -> Self {
        Self {
            compiler_identity: compiler_identity.as_ref().to_vec(),
            host: host.into(),
            target: target.into(),
            target_kind: target_kind.into(),
            ..Self::default()
        }
    }

    /// 设置是否使用测试/benchmark harness。
    pub fn set_harness(&mut self, harness: bool) {
        self.harness = harness;
    }

    /// 加入插桩模式。
    pub fn add_instrumentation(&mut self, mode: impl Into<String>) {
        self.instrumentation.insert(mode.into());
    }

    /// 加入 feature 名。
    pub fn add_feature(&mut self, feature: impl Into<String>) {
        self.features.insert(feature.into());
    }

    /// 设置规范锁图字节。
    pub fn set_lock_graph(&mut self, lock_graph: impl AsRef<[u8]>) {
        self.lock_graph = lock_graph.as_ref().to_vec();
    }

    /// 加入源码输入；路径必须是 package-relative 逻辑路径。
    pub fn add_source(&mut self, path: impl Into<String>, bytes: impl AsRef<[u8]>) {
        self.source_inputs
            .insert(path.into(), input_digest(bytes.as_ref()));
    }

    /// 加入 `embed_file` 输入。
    pub fn add_embedded_file(&mut self, path: impl Into<String>, bytes: impl AsRef<[u8]>) {
        self.embedded_files
            .insert(path.into(), input_digest(bytes.as_ref()));
    }

    /// 加入源码宏输入或生成文本。
    pub fn add_macro_input(&mut self, key: impl Into<String>, bytes: impl AsRef<[u8]>) {
        self.macro_inputs
            .insert(key.into(), input_digest(bytes.as_ref()));
    }

    /// 设置宏预算属性的规范编码。
    pub fn set_macro_budget(&mut self, bytes: impl AsRef<[u8]>) {
        self.macro_budget = bytes.as_ref().to_vec();
    }

    /// 设置 comptime capability registry 摘要。
    pub fn set_comptime_registry(&mut self, bytes: impl AsRef<[u8]>) {
        self.comptime_registry = bytes.as_ref().to_vec();
    }

    /// 设置冻结 type universe 摘要。
    pub fn set_type_universe(&mut self, bytes: impl AsRef<[u8]>) {
        self.type_universe = bytes.as_ref().to_vec();
    }

    /// 设置本 action 实际消费的 late constant 摘要。
    pub fn set_late_constants(&mut self, bytes: impl AsRef<[u8]>) {
        self.late_constants = bytes.as_ref().to_vec();
    }

    /// 加入跨 package 公共分析摘要对象。
    pub fn add_public_summary(&mut self, key: impl Into<String>, bytes: impl AsRef<[u8]>) {
        self.public_summaries
            .insert(key.into(), input_digest(bytes.as_ref()));
    }

    /// 加入 build.gg 声明的直接输入。
    pub fn add_build_input(&mut self, key: impl Into<String>, bytes: impl AsRef<[u8]>) {
        self.build_inputs
            .insert(key.into(), input_digest(bytes.as_ref()));
    }

    /// 加入 build.gg 产生的输出内容。
    pub fn add_build_output(&mut self, key: impl Into<String>, bytes: impl AsRef<[u8]>) {
        self.build_outputs
            .insert(key.into(), input_digest(bytes.as_ref()));
    }

    /// 加入一个 cfg 键值。
    pub fn set_cfg(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.cfg.insert(key.into(), value.into());
    }

    /// 设置 native link metadata 的规范编码。
    pub fn set_native_link_metadata(&mut self, bytes: impl AsRef<[u8]>) {
        self.native_link_metadata = bytes.as_ref().to_vec();
    }

    /// 计算域隔离的 BLAKE3 action key。
    pub fn key(&self) -> ActionKey {
        let mut canonical = Vec::new();
        encode_bytes(&mut canonical, &self.compiler_identity);
        encode_string(&mut canonical, &self.host);
        encode_string(&mut canonical, &self.target);
        encode_string(&mut canonical, &self.target_kind);
        canonical.push(u8::from(self.harness));
        encode_strings(&mut canonical, &self.instrumentation);
        encode_strings(&mut canonical, &self.features);
        encode_bytes(&mut canonical, &self.lock_graph);
        encode_digest_map(&mut canonical, &self.source_inputs);
        encode_digest_map(&mut canonical, &self.embedded_files);
        encode_digest_map(&mut canonical, &self.macro_inputs);
        encode_bytes(&mut canonical, &self.macro_budget);
        encode_bytes(&mut canonical, &self.comptime_registry);
        encode_bytes(&mut canonical, &self.type_universe);
        encode_bytes(&mut canonical, &self.late_constants);
        encode_digest_map(&mut canonical, &self.public_summaries);
        encode_digest_map(&mut canonical, &self.build_inputs);
        encode_digest_map(&mut canonical, &self.build_outputs);
        encode_string_map(&mut canonical, &self.cfg);
        encode_bytes(&mut canonical, &self.native_link_metadata);
        ActionKey(hash_domain("gugu-action-v1", &canonical))
    }
}

/// 编译 action 的 32 字节内容身份。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ActionKey([u8; 32]);

impl ActionKey {
    /// 返回原始摘要字节。
    pub fn bytes(self) -> [u8; 32] {
        self.0
    }

    /// 返回小写十六进制身份。
    pub fn hex(self) -> String {
        hex_encode(&self.0)
    }
}

fn input_digest(bytes: &[u8]) -> [u8; 32] {
    hash_domain("gugu-input-v1", bytes)
}

fn hash_domain(domain: &str, bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    hasher.update(&[0]);
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}

fn encode_bytes(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(
        &u64::try_from(bytes.len())
            .expect("input length fits u64")
            .to_le_bytes(),
    );
    output.extend_from_slice(bytes);
}

fn encode_string(output: &mut Vec<u8>, value: &str) {
    encode_bytes(output, value.as_bytes());
}

fn encode_strings(output: &mut Vec<u8>, values: &BTreeSet<String>) {
    output.extend_from_slice(
        &u64::try_from(values.len())
            .expect("set length fits u64")
            .to_le_bytes(),
    );
    let mut keys = values.iter().collect::<Vec<_>>();
    keys.sort_by_key(|value| encoded_key(value));
    for value in keys {
        encode_string(output, value);
    }
}

fn encode_digest_map(output: &mut Vec<u8>, values: &BTreeMap<String, [u8; 32]>) {
    output.extend_from_slice(
        &u64::try_from(values.len())
            .expect("map length fits u64")
            .to_le_bytes(),
    );
    let mut keys = values.keys().collect::<Vec<_>>();
    keys.sort_by_key(|key| encoded_key(key));
    for key in keys {
        encode_string(output, key);
        output.extend_from_slice(&values[key]);
    }
}

fn encode_string_map(output: &mut Vec<u8>, values: &BTreeMap<String, String>) {
    output.extend_from_slice(
        &u64::try_from(values.len())
            .expect("map length fits u64")
            .to_le_bytes(),
    );
    let mut keys = values.keys().collect::<Vec<_>>();
    keys.sort_by_key(|key| encoded_key(key));
    for key in keys {
        encode_string(output, key);
        encode_string(output, &values[key]);
    }
}

fn encoded_key(value: &str) -> Vec<u8> {
    let mut encoded = Vec::new();
    encode_string(&mut encoded, value);
    encoded
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}
