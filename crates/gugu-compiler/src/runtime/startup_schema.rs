//! rt0 启动、生命周期、终止与报告的契约段。
//!
//! 该段把 rt0 五步启动序列、四个进程状态与单向迁移、环境快照字段、7 个启动变量的文法
//! 与默认值、7 类 fatal、退出类别与码规则、`gugu-runtime-report-v1` 报告 schema、
//! `TerminationPlan` 字段与关闭设施顺序、emergency buffer 策略固定成一个带版本的对象。
//! `startup`/`lifecycle`/`report`/`termination` 参照实现消费同一组枚举，不建立平行表示。

use serde::{Deserialize, Serialize};

use super::model::RawModelError;
pub(crate) use super::startup_kinds::*;

/// rt0 契约段的 schema 版本。
pub(crate) const RT0_SCHEMA: u32 = 1;

/// 由编译产物推导出的 rt0 需求视图。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Rt0Demand {
    /// 编译产物是否存在 `main` 入口。
    pub(crate) entry_present: bool,
    /// `main` 是否返回 `Result[(), E]`；决定 `main-error` 报告路径是否可达。
    pub(crate) main_returns_result: bool,
}

/// rt0 启动、终止与报告的契约段。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Rt0SchemaV1 {
    schema: u32,
    steps: Vec<Rt0Step>,
    states: Vec<LifecycleStateName>,
    transitions: Vec<LifecycleTransition>,
    snapshot_fields: Vec<EnvSnapshotField>,
    startup_vars: Vec<StartupVar>,
    byte_units: Vec<ByteUnit>,
    trace_categories: Vec<TraceCategory>,
    fatal_kinds: Vec<FatalKind>,
    exit_categories: Vec<ExitCategory>,
    report_schema_name: String,
    report_fields: Vec<String>,
    report_events: Vec<ReportEvent>,
    report_classes: Vec<ExitCategory>,
    report_reasons: Vec<ReportReason>,
    termination_modes: Vec<TerminationMode>,
    termination_fields: Vec<String>,
    shutdown_facilities: Vec<ShutdownFacility>,
    emergency: EmergencyBufferPolicy,
    demand: Rt0Demand,
    fingerprint: [u8; 32],
}

impl Rt0SchemaV1 {
    /// 由需求视图构建契约段；目录固定，指纹覆盖全部内容。
    pub(crate) fn build(demand: Rt0Demand) -> Result<Self, RawModelError> {
        let mut contract = Self {
            schema: RT0_SCHEMA,
            steps: Rt0Step::all().to_vec(),
            states: LifecycleStateName::all().to_vec(),
            transitions: LifecycleTransition::all(),
            snapshot_fields: [
                EnvSnapshotField::Argv,
                EnvSnapshotField::Environment,
                EnvSnapshotField::WorkingDirectory,
            ]
            .to_vec(),
            startup_vars: StartupVar::all(),
            byte_units: [
                ByteUnit::B,
                ByteUnit::KiB,
                ByteUnit::MiB,
                ByteUnit::GiB,
                ByteUnit::TiB,
            ]
            .to_vec(),
            trace_categories: [
                TraceCategory::Scheduler,
                TraceCategory::Gc,
                TraceCategory::Signal,
                TraceCategory::Panic,
            ]
            .to_vec(),
            fatal_kinds: FatalKind::all().to_vec(),
            exit_categories: ExitCategory::all().to_vec(),
            report_schema_name: REPORT_SCHEMA_NAME.to_owned(),
            report_fields: REPORT_FIELDS
                .iter()
                .map(|field| (*field).to_owned())
                .collect(),
            report_events: [ReportEvent::Panic, ReportEvent::Termination].to_vec(),
            report_classes: ExitCategory::all().to_vec(),
            report_reasons: ReportReason::all().to_vec(),
            termination_modes: TerminationMode::all().to_vec(),
            termination_fields: TERMINATION_FIELDS
                .iter()
                .map(|field| (*field).to_owned())
                .collect(),
            shutdown_facilities: ShutdownFacility::all().to_vec(),
            emergency: EmergencyBufferPolicy {
                capacity_bytes: EMERGENCY_BUFFER_BYTES,
                min_bytes: EMERGENCY_BUFFER_MIN_BYTES,
                fallback: EmergencyFallback::PlainText,
                truncation: "truncate-message-then-drop-backtrace-tail".to_owned(),
            },
            demand,
            fingerprint: [0; 32],
        };
        if contract.emergency.capacity_bytes < contract.emergency.min_bytes {
            return Err(RawModelError::new(
                "emergency buffer 容量低于必填字段的容量下限",
            ));
        }
        contract.fingerprint = contract.compute_fingerprint();
        Ok(contract)
    }

    /// 返回需求视图。
    pub(crate) const fn demand(&self) -> &Rt0Demand {
        &self.demand
    }

    /// 返回 rt0 启动步骤目录。
    pub(crate) fn steps(&self) -> &[Rt0Step] {
        &self.steps
    }

    /// 返回生命周期状态目录。
    pub(crate) fn states(&self) -> &[LifecycleStateName] {
        &self.states
    }

    /// 返回启动变量目录。
    pub(crate) fn startup_vars(&self) -> &[StartupVar] {
        &self.startup_vars
    }

    /// 返回 fatal 目录。
    pub(crate) fn fatal_kinds(&self) -> &[FatalKind] {
        &self.fatal_kinds
    }

    /// 返回报告 reason 目录。
    pub(crate) fn report_reasons(&self) -> &[ReportReason] {
        &self.report_reasons
    }

    /// 返回 emergency buffer 策略。
    pub(crate) const fn emergency(&self) -> &EmergencyBufferPolicy {
        &self.emergency
    }

    /// 返回内容身份。
    pub(crate) const fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// 校验目录与固定登记一致、emergency 策略合法且指纹与内容一致。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        let expected = Self::build(self.demand)?;
        if *self != expected {
            return Err(RawModelError::new("rt0 启动契约目录与固定登记不一致"));
        }
        if self.fingerprint != self.compute_fingerprint() {
            return Err(RawModelError::new("rt0 启动契约指纹与内容不一致"));
        }
        Ok(())
    }

    /// 返回规范编码；指纹字段不参与编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        for step in &self.steps {
            bytes.push(*step as u8);
        }
        for state in &self.states {
            bytes.push(*state as u8);
        }
        for transition in &self.transitions {
            bytes.push(transition.from as u8);
            bytes.push(transition.to as u8);
            bytes.extend_from_slice(transition.trigger.as_bytes());
            bytes.push(0);
        }
        for field in &self.snapshot_fields {
            bytes.push(*field as u8);
        }
        for var in &self.startup_vars {
            bytes.extend_from_slice(var.name.as_bytes());
            bytes.push(0);
            bytes.push(var.grammar as u8);
            bytes.extend_from_slice(var.default.as_bytes());
            bytes.push(0);
        }
        for unit in &self.byte_units {
            bytes.extend_from_slice(&unit.multiplier().to_le_bytes());
        }
        for category in &self.trace_categories {
            bytes.push(*category as u8);
        }
        for kind in &self.fatal_kinds {
            bytes.push(*kind as u8);
        }
        for category in &self.exit_categories {
            bytes.push(*category as u8);
            match category.code_rule() {
                ExitCodeRule::Fixed(code) => {
                    bytes.push(0);
                    bytes.extend_from_slice(&code.to_le_bytes());
                }
                ExitCodeRule::Caller => bytes.push(1),
                ExitCodeRule::TargetSignal => bytes.push(2),
            }
        }
        bytes.extend_from_slice(self.report_schema_name.as_bytes());
        bytes.push(0);
        for field in &self.report_fields {
            bytes.extend_from_slice(field.as_bytes());
            bytes.push(0);
        }
        for event in &self.report_events {
            bytes.push(*event as u8);
        }
        for class in &self.report_classes {
            bytes.push(*class as u8);
        }
        for reason in &self.report_reasons {
            bytes.push(*reason as u8);
        }
        for mode in &self.termination_modes {
            bytes.push(*mode as u8);
        }
        for field in &self.termination_fields {
            bytes.extend_from_slice(field.as_bytes());
            bytes.push(0);
        }
        for facility in &self.shutdown_facilities {
            bytes.push(*facility as u8);
        }
        bytes.extend_from_slice(&self.emergency.capacity_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.emergency.min_bytes.to_le_bytes());
        bytes.push(self.emergency.fallback as u8);
        bytes.extend_from_slice(self.emergency.truncation.as_bytes());
        bytes.push(0);
        bytes.push(u8::from(self.demand.entry_present));
        bytes.push(u8::from(self.demand.main_returns_result));
        bytes
    }

    fn compute_fingerprint(&self) -> [u8; 32] {
        *blake3::Hasher::new_derive_key("gugu-rt0-startup-v1")
            .update(&self.canonical_bytes())
            .finalize()
            .as_bytes()
    }

    /// 返回稳定文本 dump；不含地址、宿主路径与线程编号。
    pub(crate) fn dump(&self) -> String {
        let mut output = String::new();
        output.push_str(&format!(
            "rt0 schema={} report-schema={} emergency-bytes={} emergency-min={}\n",
            self.schema,
            self.report_schema_name,
            self.emergency.capacity_bytes,
            self.emergency.min_bytes
        ));
        for step in &self.steps {
            output.push_str(&format!("rt0-step {}\n", step.name()));
        }
        for state in &self.states {
            output.push_str(&format!("rt0-state {}\n", state.name()));
        }
        for transition in &self.transitions {
            output.push_str(&format!(
                "rt0-transition {} -> {} on {}\n",
                transition.from.name(),
                transition.to.name(),
                transition.trigger
            ));
        }
        for field in &self.snapshot_fields {
            output.push_str(&format!("rt0-env {} fixed-once\n", field.name()));
        }
        for var in &self.startup_vars {
            output.push_str(&format!(
                "startup-var {} grammar={} default={}\n",
                var.name,
                var.grammar.name(),
                var.default
            ));
        }
        for unit in &self.byte_units {
            output.push_str(&format!(
                "startup-byte-unit {} multiplier={}\n",
                unit.name(),
                unit.multiplier()
            ));
        }
        for category in &self.trace_categories {
            output.push_str(&format!("rt0-trace-category {}\n", category.name()));
        }
        self.dump_report(&mut output);
        for mode in &self.termination_modes {
            output.push_str(&format!("termination-mode {}\n", mode.name()));
        }
        for field in &self.termination_fields {
            output.push_str(&format!("termination-field {field}\n"));
        }
        for facility in &self.shutdown_facilities {
            output.push_str(&format!("rt0-facility {}\n", facility.name()));
        }
        output.push_str(&format!(
            "rt0-demand entry={} main-result={}\n",
            self.demand.entry_present, self.demand.main_returns_result
        ));
        output
    }

    fn dump_report(&self, output: &mut String) {
        for kind in &self.fatal_kinds {
            output.push_str(&format!(
                "rt0-fatal {} reason={} exit={}\n",
                kind.name(),
                kind.reason().name(),
                ExitCategory::RuntimeFailure.name()
            ));
        }
        for category in &self.exit_categories {
            output.push_str(&format!("rt0-exit-category {}\n", category.name()));
        }
        output.push_str(&format!("report-schema {}\n", self.report_schema_name));
        for event in &self.report_events {
            output.push_str(&format!("report-event {}\n", event.name()));
        }
        for class in &self.report_classes {
            output.push_str(&format!("report-class {}\n", class.name()));
        }
        for reason in &self.report_reasons {
            output.push_str(&format!("report-reason {}\n", reason.name()));
        }
        for field in &self.report_fields {
            output.push_str(&format!("report-field {field}\n"));
        }
    }
}
