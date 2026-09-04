use arsy_kernel::capability::CapabilityAction;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    time::Duration,
};
use wasmtime::{
    Config, Engine, InstanceAllocationStrategy, Linker, Module, PoolingAllocationConfig, Store,
    StoreLimits, StoreLimitsBuilder,
};

const READ_INPUT: &str = "arsy::read_input_byte";
const EMIT_OUTPUT: &str = "arsy::emit_byte";

#[derive(Clone, Debug)]
pub struct PluginManifest {
    pub id: String,
    pub version: String,
    pub imports: BTreeSet<String>,
    pub capabilities: BTreeSet<CapabilityAction>,
}

#[derive(Clone, Copy, Debug)]
pub struct InvocationLimits {
    pub fuel: u64,
    pub timeout: Duration,
    pub memory_bytes: usize,
    pub output_bytes: usize,
}

struct InstalledPlugin {
    manifest: PluginManifest,
    approved: BTreeSet<CapabilityAction>,
    module: Module,
}

struct InvocationState {
    input: Vec<u8>,
    output: Vec<u8>,
    output_limit: usize,
    limits: StoreLimits,
}

pub struct ExtensionHost {
    engine: Engine,
    plugins: BTreeMap<String, InstalledPlugin>,
}

impl ExtensionHost {
    pub fn new() -> Result<Self, ExtensionError> {
        let mut config = Config::new();
        config.consume_fuel(true);
        config.epoch_interruption(true);
        config.allocation_strategy(InstanceAllocationStrategy::Pooling(
            PoolingAllocationConfig::default(),
        ));
        Ok(Self {
            engine: Engine::new(&config).map_err(runtime)?,
            plugins: BTreeMap::new(),
        })
    }

    pub fn install(
        &mut self,
        manifest: PluginManifest,
        wasm: &[u8],
        approved: BTreeSet<CapabilityAction>,
    ) -> Result<(), ExtensionError> {
        let effective_approval = self
            .plugins
            .get(&manifest.id)
            .map_or(&approved, |installed| &installed.approved);
        if !manifest.capabilities.is_subset(effective_approval) {
            return Err(ExtensionError::CapabilityExpansion);
        }
        let module = Module::new(&self.engine, wasm).map_err(runtime)?;
        for import in module.imports() {
            let name = format!("{}::{}", import.module(), import.name());
            if !manifest.imports.contains(&name) {
                return Err(ExtensionError::UndeclaredImport(name));
            }
            if !matches!(name.as_str(), READ_INPUT | EMIT_OUTPUT) {
                return Err(ExtensionError::UnsupportedImport(name));
            }
        }
        self.plugins.insert(
            manifest.id.clone(),
            InstalledPlugin {
                manifest,
                approved: effective_approval.clone(),
                module,
            },
        );
        Ok(())
    }

    pub fn invoke(
        &self,
        plugin: &str,
        input: &[u8],
        grants: &BTreeSet<CapabilityAction>,
        limits: InvocationLimits,
    ) -> Result<Vec<u8>, ExtensionError> {
        let installed = self
            .plugins
            .get(plugin)
            .ok_or_else(|| ExtensionError::NotInstalled(plugin.into()))?;
        if !installed.manifest.capabilities.is_subset(grants) {
            return Err(ExtensionError::MissingGrant);
        }
        if limits.fuel == 0
            || limits.timeout.is_zero()
            || limits.memory_bytes == 0
            || limits.output_bytes == 0
        {
            return Err(ExtensionError::InvalidLimits);
        }
        if input.len() > limits.memory_bytes {
            return Err(ExtensionError::InputTooLarge);
        }
        let state = InvocationState {
            input: input.to_vec(),
            output: Vec::new(),
            output_limit: limits.output_bytes,
            limits: StoreLimitsBuilder::new()
                .memory_size(limits.memory_bytes)
                .instances(1)
                .build(),
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limits);
        store.set_fuel(limits.fuel).map_err(runtime)?;
        store.set_epoch_deadline(1);

        let mut linker = Linker::new(&self.engine);
        if installed.manifest.imports.contains(READ_INPUT) {
            linker
                .func_wrap(
                    "arsy",
                    "read_input_byte",
                    |caller: wasmtime::Caller<'_, InvocationState>, index: i32| -> i32 {
                        usize::try_from(index)
                            .ok()
                            .and_then(|index| caller.data().input.get(index).copied())
                            .map_or(-1, i32::from)
                    },
                )
                .map_err(runtime)?;
        }
        if installed.manifest.imports.contains(EMIT_OUTPUT) {
            linker
                .func_wrap(
                    "arsy",
                    "emit_byte",
                    |mut caller: wasmtime::Caller<'_, InvocationState>, byte: i32| -> i32 {
                        let state = caller.data_mut();
                        if state.output.len() >= state.output_limit {
                            return -1;
                        }
                        state.output.push(byte as u8);
                        0
                    },
                )
                .map_err(runtime)?;
        }

        let engine = self.engine.clone();
        std::thread::spawn(move || {
            std::thread::sleep(limits.timeout);
            engine.increment_epoch();
        });
        let instance = linker
            .instantiate(&mut store, &installed.module)
            .map_err(runtime)?;
        let run = instance
            .get_typed_func::<(), ()>(&mut store, "run")
            .map_err(runtime)?;
        run.call(&mut store, ()).map_err(runtime)?;
        Ok(std::mem::take(&mut store.data_mut().output))
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum ExtensionError {
    NotInstalled(String),
    CapabilityExpansion,
    MissingGrant,
    InvalidLimits,
    InputTooLarge,
    UndeclaredImport(String),
    UnsupportedImport(String),
    Runtime(String),
}

impl fmt::Display for ExtensionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled(id) => write!(formatter, "plugin {id} is not installed"),
            Self::CapabilityExpansion => {
                formatter.write_str("plugin install or update requests unapproved capabilities")
            }
            Self::MissingGrant => {
                formatter.write_str("invocation grant does not cover plugin capabilities")
            }
            Self::InvalidLimits => formatter.write_str("plugin invocation limits must be non-zero"),
            Self::InputTooLarge => {
                formatter.write_str("plugin input exceeds its invocation memory limit")
            }
            Self::UndeclaredImport(name) => write!(formatter, "WASM import {name} is not declared"),
            Self::UnsupportedImport(name) => {
                write!(formatter, "WASM import {name} is not exposed by the host")
            }
            Self::Runtime(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ExtensionError {}

fn runtime(error: impl ToString) -> ExtensionError {
    ExtensionError::Runtime(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(output_bytes: usize) -> InvocationLimits {
        InvocationLimits {
            fuel: 10_000,
            timeout: Duration::from_secs(1),
            memory_bytes: 64 * 1024,
            output_bytes,
        }
    }

    #[test]
    fn imports_quotas_state_reset_and_update_authority_are_enforced() {
        let wasm = br#"
            (module
              (import "arsy" "read_input_byte" (func $read (param i32) (result i32)))
              (import "arsy" "emit_byte" (func $emit (param i32) (result i32)))
              (memory 1)
              (global $calls (mut i32) (i32.const 0))
              (func (export "run")
                (global.set $calls (i32.add (global.get $calls) (i32.const 1)))
                (drop (call $emit (call $read (i32.const 0))))
                (drop (call $emit (global.get $calls)))))
        "#;
        let mut host = ExtensionHost::new().unwrap();
        let manifest = PluginManifest {
            id: "example".into(),
            version: "1".into(),
            imports: BTreeSet::from([READ_INPUT.into(), EMIT_OUTPUT.into()]),
            capabilities: BTreeSet::new(),
        };
        host.install(manifest.clone(), wasm, BTreeSet::new())
            .unwrap();
        assert_eq!(
            host.invoke("example", b"A", &BTreeSet::new(), limits(2))
                .unwrap(),
            b"A\x01"
        );
        assert_eq!(
            host.invoke("example", b"B", &BTreeSet::new(), limits(2))
                .unwrap(),
            b"B\x01"
        );
        assert_eq!(
            host.invoke("example", b"C", &BTreeSet::new(), limits(1))
                .unwrap(),
            b"C"
        );

        let mut expanded = manifest;
        expanded.version = "2".into();
        expanded
            .capabilities
            .insert(CapabilityAction::NetworkConnect);
        assert_eq!(
            host.install(
                expanded,
                wasm,
                BTreeSet::from([CapabilityAction::NetworkConnect])
            ),
            Err(ExtensionError::CapabilityExpansion)
        );
    }
}
