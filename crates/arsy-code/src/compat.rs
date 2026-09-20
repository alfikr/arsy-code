use crate::edit::EditAddress;
use arsy_kernel::{
    domain::StateVersion,
    secret::{SecretError, SecretHandle, OS_STORE_ID},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs, io,
    path::{Path, PathBuf},
};

const MAX_COMPAT_SOURCE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ecosystem {
    AgentsMd,
    Claude,
    Codex,
    Omp,
}

pub const CLAUDE_ADAPTER_VERSION: u32 = 1;
pub const CODEX_ADAPTER_VERSION: u32 = 1;
pub const OMP_ADAPTER_VERSION: u32 = 1;

pub fn adapter_version(ecosystem: Ecosystem) -> u32 {
    match ecosystem {
        Ecosystem::AgentsMd | Ecosystem::Codex => CODEX_ADAPTER_VERSION,
        Ecosystem::Claude => CLAUDE_ADAPTER_VERSION,
        Ecosystem::Omp => OMP_ADAPTER_VERSION,
    }
}

pub fn imported_credential(provider: &str) -> Result<SecretHandle, SecretError> {
    SecretHandle::new(OS_STORE_ID, provider)
}

impl Ecosystem {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentsMd => "agents-md",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Omp => "omp",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompatibilityLevel {
    Parsed,
    Mapped,
    BehaviorTested,
    Unsupported,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Diagnostic {
    pub source: String,
    pub key: String,
    pub message: String,
    pub fail_closed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImportReport {
    pub canonical: Value,
    pub loss: Value,
    pub diagnostics: Vec<Diagnostic>,
}

impl ImportReport {
    pub fn explain(&self) -> Value {
        json!({
            "sources": self.canonical["instructions"],
            "precedence": self.canonical["settings"]["sources"],
            "mapping": self.canonical,
            "loss": self.loss,
            "diagnostics": self.diagnostics,
        })
    }
}

/// Imports declarations as data. `display_root` only controls source labels in
/// reports, which lets fixtures retain their `input/` prefix without changing
/// discovery semantics.
pub struct CompatibilityImporter {
    root: PathBuf,
    display_root: PathBuf,
}

impl CompatibilityImporter {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            display_root: root.clone(),
            root,
        }
    }

    pub fn with_display_root(root: impl Into<PathBuf>, display_root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            display_root: display_root.into(),
        }
    }

    /// Inspect only integration declarations; unrelated model/policy settings
    /// do not need to be importable, and no declaration is activated.
    pub fn mcp_declarations(
        &self,
        ecosystem: Ecosystem,
        working_directory: &Path,
    ) -> Result<Vec<Value>, CompatError> {
        if !fs::canonicalize(working_directory)?.starts_with(fs::canonicalize(&self.root)?) {
            return Err(CompatError::OutsideRoot);
        }
        match ecosystem {
            Ecosystem::Claude => mcp_json(self, &self.root.join(".mcp.json"), "mapped"),
            Ecosystem::Codex => {
                let path = self.root.join(".codex/config.toml");
                if !path.try_exists()? {
                    return Ok(Vec::new());
                }
                let config: toml::Value = toml::from_str(&self.read(&path)?)
                    .map_err(|error| CompatError::Parse(error.to_string()))?;
                mcp_toml(self, &path, config.get("mcp_servers"))
            }
            Ecosystem::Omp => match nearest_native(&self.root, working_directory) {
                Ok(native) => mcp_json(self, &native.join("mcp.json"), "mapped"),
                Err(CompatError::Missing(_)) => Ok(Vec::new()),
                Err(error) => Err(error),
            },
            Ecosystem::AgentsMd => Ok(Vec::new()),
        }
    }

    /// Skills declared by one ecosystem, as data. A skill is layer-1 content:
    /// reading it grants nothing, which is why the listing is available without
    /// loading anything.
    pub fn skill_declarations(&self, ecosystem: Ecosystem) -> Result<Vec<Value>, CompatError> {
        let directory = match ecosystem {
            Ecosystem::Claude => self.root.join(".claude/skills"),
            Ecosystem::Codex => self.root.join(".codex/skills"),
            Ecosystem::Omp => self.root.join(".omp/skills"),
            Ecosystem::AgentsMd => return Ok(Vec::new()),
        };
        skills(self, &directory, "mapped")
    }

    pub fn hook_declarations(&self) -> Result<Vec<Value>, CompatError> {
        let source = self.root.join(".claude/settings.json");
        let local = self.root.join(".claude/settings.local.json");
        // Claude runs the hooks of both files, so both are listed -- the same
        // two the runtime loader reads.
        let mut hooks = claude_hooks(
            &self.source(&source)?,
            &source,
            &read_json_or_empty(self, &source)?,
        )?;
        hooks.extend(claude_hooks(
            &self.source(&local)?,
            &local,
            &read_json_or_empty(self, &local)?,
        )?);
        Ok(hooks)
    }

    pub fn import(
        &self,
        ecosystem: Ecosystem,
        working_directory: &Path,
        fixture: &str,
    ) -> Result<ImportReport, CompatError> {
        if !working_directory.starts_with(&self.root)
            || !fs::canonicalize(working_directory)?.starts_with(fs::canonicalize(&self.root)?)
        {
            return Err(CompatError::OutsideRoot);
        }
        match ecosystem {
            Ecosystem::AgentsMd => self.import_agents(working_directory, fixture),
            Ecosystem::Claude => self.import_claude(working_directory, fixture),
            Ecosystem::Codex => self.import_codex(working_directory, fixture),
            Ecosystem::Omp => self.import_omp(working_directory, fixture),
        }
    }

    fn import_agents(
        &self,
        working_directory: &Path,
        fixture: &str,
    ) -> Result<ImportReport, CompatError> {
        Ok(ImportReport {
            canonical: canonical(
                fixture,
                Ecosystem::AgentsMd,
                vec!["nested_instructions"],
                self.walk_instructions(working_directory, "AGENTS.md", "AGENTS.override.md")?,
                json!({}),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ),
            loss: loss(fixture, Vec::new()),
            diagnostics: Vec::new(),
        })
    }

    fn import_claude(
        &self,
        working_directory: &Path,
        fixture: &str,
    ) -> Result<ImportReport, CompatError> {
        let instructions = self.walk_named(working_directory, "CLAUDE.md", "mapped")?;
        let settings_path = self.root.join(".claude/settings.json");
        let local_path = self.root.join(".claude/settings.local.json");
        let mut settings = read_json_or_empty(self, &settings_path)?;
        let local_settings = read_json_or_empty(self, &local_path)?;
        let mut hooks = claude_hooks(&self.source(&settings_path)?, &settings_path, &settings)?;
        hooks.extend(claude_hooks(
            &self.source(&local_path)?,
            &local_path,
            &local_settings,
        )?);
        merge_object(&mut settings, local_settings)?;
        let diagnostics = unknown_keys(
            &settings,
            &["model", "permissions", "hooks"],
            self.source(&settings_path)?,
        )?;
        reject_sensitive_unknowns(&diagnostics)?;

        let permissions = settings.get("permissions").and_then(Value::as_object);
        let allow = mapped_strings(
            permissions.and_then(|p| p.get("allow")),
            map_claude_permission,
        );
        let deny = mapped_strings(
            permissions.and_then(|p| p.get("deny")),
            map_claude_permission,
        );
        let sources = existing_sources(self, [&settings_path, &local_path])?;
        let agents = markdown_agents(self, &self.root.join(".claude/agents"), None)?;
        let skills = skills(self, &self.root.join(".claude/skills"), "mapped")?;
        let commands = markdown_declarations(self, &self.root.join(".claude/commands"), "mapped")?;
        let plugins = claude_plugins(self)?;
        let mcp = mcp_json(self, &self.root.join(".mcp.json"), "mapped")?;
        let losses = vec![
            json!({
                "source": self.source(&self.root.join(".claude-plugin/plugin.json"))?,
                "concept": "plugin_code_execution",
                "level": "unsupported",
                "reason": "Plugin declarations are imported, but JavaScript entrypoints never execute in the core."
            }),
            json!({
                "source": self.source(&settings_path)?,
                "concept": "hook_command_execution",
                "level": "mapped",
                "reason": "The lifecycle mapping is preserved; execution still requires an explicit operation policy grant."
            }),
        ];
        let mut canonical = canonical(
            fixture,
            Ecosystem::Claude,
            vec![
                "nested_instructions",
                "settings_precedence",
                "skills",
                "hooks",
                "agents",
                "plugins",
                "mcp",
            ],
            instructions,
            json!({
                "sources": sources,
                "model": settings.get("model").and_then(Value::as_str),
                "allow": allow,
                "deny": deny,
                "repository_can_grant": false,
            }),
            skills,
            hooks,
            agents,
            plugins,
            mcp,
        );
        if !commands.is_empty() {
            canonical["commands"] = Value::Array(commands);
        }
        Ok(ImportReport {
            canonical,
            loss: loss(fixture, losses),
            diagnostics,
        })
    }

    fn import_codex(
        &self,
        working_directory: &Path,
        fixture: &str,
    ) -> Result<ImportReport, CompatError> {
        let config_path = self.root.join(".codex/config.toml");
        let config: toml::Value = toml::from_str(&self.read(&config_path)?)
            .map_err(|error| CompatError::Parse(error.to_string()))?;
        let table = config
            .as_table()
            .ok_or_else(|| CompatError::Parse("Codex config root must be a table".into()))?;
        let diagnostics = unknown_toml_keys(
            table,
            &[
                "model",
                "model_provider",
                "approval_policy",
                "sandbox_mode",
                "mcp_servers",
            ],
            self.source(&config_path)?,
        );
        reject_sensitive_unknowns(&diagnostics)?;
        let sandbox = string(table, "sandbox_mode")?;
        if !matches!(sandbox, "read-only" | "workspace-write") {
            return Err(CompatError::UnsupportedSandbox(sandbox.to_owned()));
        }
        let mcp = mcp_toml(self, &config_path, table.get("mcp_servers"))?;
        Ok(ImportReport {
            canonical: canonical(
                fixture,
                Ecosystem::Codex,
                vec![
                    "nested_instructions",
                    "settings_precedence",
                    "skills",
                    "mcp",
                ],
                self.walk_instructions(working_directory, "AGENTS.md", "AGENTS.override.md")?,
                json!({
                    "sources": [self.source(&config_path)?],
                    "provider": string(table, "model_provider")?,
                    "model": string(table, "model")?,
                    "approval_default": map_approval(string(table, "approval_policy")?)?,
                    "sandbox_request": sandbox,
                    "mapping_requirement": "equal-or-stronger",
                    "repository_can_grant": false,
                }),
                skills(self, &self.root.join(".codex/skills"), "mapped")?,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                mcp,
            ),
            loss: loss(
                fixture,
                vec![json!({
                    "source": self.source(&config_path)?,
                    "concept": "workspace_write_sandbox",
                    "level": "mapped",
                    "reason": "The request maps only when the local backend can provide equal or stronger confinement."
                })],
            ),
            diagnostics,
        })
    }

    fn import_omp(
        &self,
        working_directory: &Path,
        fixture: &str,
    ) -> Result<ImportReport, CompatError> {
        let native = nearest_native(&self.root, working_directory)?;
        let config_path = native.join("config.yml");
        let config: yaml_serde::Value = yaml_serde::from_str(&self.read(&config_path)?)
            .map_err(|error| CompatError::Parse(error.to_string()))?;
        let model_roles = config
            .get("modelRoles")
            .cloned()
            .unwrap_or(yaml_serde::Value::Mapping(Default::default()));
        let model_roles = serde_json::to_value(model_roles)
            .map_err(|error| CompatError::Parse(error.to_string()))?;
        let skill_enabled = config
            .get("skills")
            .and_then(|v| v.get("enabled"))
            .and_then(yaml_serde::Value::as_bool)
            .unwrap_or(false);
        let instruction = native.join("AGENTS.md");
        let root_native = self.root.join(".omp/AGENTS.md");
        let agents = markdown_agents(self, &native.join("agents"), Some("review"))?;
        let plugins = typescript_extensions(self, &native.join("extensions"))?;
        let mcp_path = native.join("mcp.json");
        Ok(ImportReport {
            canonical: canonical(
                fixture,
                Ecosystem::Omp,
                vec![
                    "nested_instructions",
                    "settings_precedence",
                    "skills",
                    "agents",
                    "plugins",
                    "mcp",
                ],
                vec![json!({
                    "source": self.source(&instruction)?,
                    "scope": slash(native.parent().unwrap_or(&self.root).strip_prefix(&self.root).unwrap_or(Path::new(""))),
                    "precedence": 0,
                    "level": "behavior-tested",
                })],
                json!({
                    "sources": [self.source(&config_path)?],
                    "model_roles": model_roles,
                    "skills_enabled": skill_enabled,
                    "repository_can_grant": false,
                }),
                skills(self, &native.join("skills"), "behavior-tested")?,
                Vec::new(),
                agents,
                plugins,
                mcp_json(self, &mcp_path, "behavior-tested")?,
            ),
            loss: loss(
                fixture,
                vec![
                    json!({
                        "source": self.source(&root_native)?,
                        "concept": "farther_native_context",
                        "level": "behavior-tested",
                        "reason": "OMP nearest non-empty native discovery intentionally shadows the farther root file."
                    }),
                    json!({
                        "source": self.source(&native.join("extensions/audit.ts"))?,
                        "concept": "typescript_extension_execution",
                        "level": "unsupported",
                        "reason": "TypeScript extensions never execute inside the ARSY core."
                    }),
                ],
            ),
            diagnostics: Vec::new(),
        })
    }

    fn walk_named(
        &self,
        working_directory: &Path,
        name: &str,
        level: &str,
    ) -> Result<Vec<Value>, CompatError> {
        let mut output = Vec::new();
        for directory in ancestor_walk(&self.root, working_directory)? {
            let path = directory.join(name);
            if path.is_file() {
                output.push(instruction(self, &path, output.len(), level)?);
            }
        }
        Ok(output)
    }

    fn walk_instructions(
        &self,
        working_directory: &Path,
        regular: &str,
        override_name: &str,
    ) -> Result<Vec<Value>, CompatError> {
        let mut output = Vec::new();
        for directory in ancestor_walk(&self.root, working_directory)? {
            let override_path = directory.join(override_name);
            let regular_path = directory.join(regular);
            let path = if override_path.is_file() {
                override_path
            } else {
                regular_path
            };
            if path.is_file() {
                output.push(instruction(self, &path, output.len(), "behavior-tested")?);
            }
        }
        Ok(output)
    }

    fn source(&self, path: &Path) -> Result<String, CompatError> {
        path.strip_prefix(&self.display_root)
            .map(slash)
            .map_err(|_| CompatError::OutsideRoot)
    }

    fn read(&self, path: &Path) -> Result<String, CompatError> {
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| CompatError::OutsideRoot)?;
        let content = crate::resource::Workspace::open(&self.root)?
            .resolve_file(relative)
            .map_err(|error| match error {
                crate::resource::ResolveError::OutsideWorkspace => CompatError::OutsideRoot,
                error => CompatError::Parse(error.to_string()),
            })?
            .read(MAX_COMPAT_SOURCE_BYTES)?;
        String::from_utf8(content.bytes).map_err(|error| CompatError::Parse(error.to_string()))
    }
}

pub fn parse_hashline_anchor(value: &str) -> Result<EditAddress, CompatError> {
    let (before, after) = value
        .split_once(':')
        .ok_or_else(|| CompatError::Parse("hashline anchor must be BEFORE:AFTER".into()))?;
    Ok(EditAddress::ContentAnchor {
        before: before
            .parse::<StateVersion>()
            .map_err(|error| CompatError::Parse(error.to_string()))?,
        after: after
            .parse::<StateVersion>()
            .map_err(|error| CompatError::Parse(error.to_string()))?,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImportedSessionEntry {
    Known { kind: String, payload: Value },
    Opaque(Value),
}

pub fn import_session_entry(value: Value, known: &BTreeSet<String>) -> ImportedSessionEntry {
    match value.get("type").and_then(Value::as_str) {
        Some(kind) if known.contains(kind) => ImportedSessionEntry::Known {
            kind: kind.to_owned(),
            payload: value,
        },
        _ => ImportedSessionEntry::Opaque(value),
    }
}

#[allow(clippy::too_many_arguments)]
fn canonical(
    fixture: &str,
    ecosystem: Ecosystem,
    coverage: Vec<&str>,
    instructions: Vec<Value>,
    settings: Value,
    skills: Vec<Value>,
    hooks: Vec<Value>,
    agents: Vec<Value>,
    plugins: Vec<Value>,
    mcp: Vec<Value>,
) -> Value {
    json!({
        "schema_version": 1,
        "fixture": fixture,
        "ecosystem": ecosystem.as_str(),
        "coverage": coverage,
        "instructions": instructions,
        "settings": settings,
        "skills": skills,
        "hooks": hooks,
        "agents": agents,
        "plugins": plugins,
        "mcp": mcp,
    })
}

fn loss(fixture: &str, losses: Vec<Value>) -> Value {
    json!({"schema_version": 1, "fixture": fixture, "losses": losses})
}

fn instruction(
    importer: &CompatibilityImporter,
    path: &Path,
    precedence: usize,
    level: &str,
) -> Result<Value, CompatError> {
    let _ = importer.read(path)?;
    let scope = path
        .parent()
        .and_then(|parent| parent.strip_prefix(&importer.root).ok())
        .map(slash)
        .filter(|scope| !scope.is_empty())
        .unwrap_or_else(|| ".".into());
    Ok(json!({
        "source": importer.source(path)?,
        "scope": scope,
        "precedence": precedence,
        "level": level,
    }))
}

fn ancestor_walk(root: &Path, working_directory: &Path) -> Result<Vec<PathBuf>, CompatError> {
    let relative = working_directory
        .strip_prefix(root)
        .map_err(|_| CompatError::OutsideRoot)?;
    let mut output = vec![root.to_owned()];
    let mut current = root.to_owned();
    for component in relative.components() {
        current.push(component);
        output.push(current.clone());
    }
    Ok(output)
}

fn nearest_native(root: &Path, working_directory: &Path) -> Result<PathBuf, CompatError> {
    let mut current = working_directory.to_owned();
    loop {
        let native = current.join(".omp");
        if native.is_dir() && fs::read_dir(&native)?.next().is_some() {
            return Ok(native);
        }
        if current == root || !current.pop() {
            return Err(CompatError::Missing("nearest .omp directory".into()));
        }
    }
}

fn skills(
    importer: &CompatibilityImporter,
    directory: &Path,
    level: &str,
) -> Result<Vec<Value>, CompatError> {
    let mut paths = child_files(directory, "SKILL.md")?;
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let name = path
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .ok_or_else(|| CompatError::Parse("skill path has no UTF-8 name".into()))?;
            Ok(json!({"source": importer.source(&path)?, "name": name, "level": level}))
        })
        .collect()
}

fn markdown_declarations(
    importer: &CompatibilityImporter,
    directory: &Path,
    level: &str,
) -> Result<Vec<Value>, CompatError> {
    let mut paths = files_with_extension(directory, "md")?;
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let name = path
                .file_stem()
                .and_then(|name| name.to_str())
                .ok_or_else(|| CompatError::Parse("command path has no UTF-8 name".into()))?;
            Ok(json!({"source": importer.source(&path)?, "name": name, "level": level}))
        })
        .collect()
}

fn markdown_agents(
    importer: &CompatibilityImporter,
    directory: &Path,
    model_role: Option<&str>,
) -> Result<Vec<Value>, CompatError> {
    let mut paths = files_with_extension(directory, "md")?;
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let source = importer.read(&path)?;
            let fields = front_matter(&source);
            let name = fields
                .get("name")
                .cloned()
                .or_else(|| path.file_stem()?.to_str().map(str::to_owned))
                .ok_or_else(|| CompatError::Parse("agent has no name".into()))?;
            let tools = fields
                .get("tools")
                .map(|tools| {
                    tools
                        .split(',')
                        .filter_map(|tool| map_tool(tool.trim()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let mut value = json!({
                "source": importer.source(&path)?,
                "name": name,
                "requested_operations": tools,
                "authority": "request-only",
                "level": "mapped",
            });
            if let Some(role) = model_role {
                value["model_role"] = json!(role);
            }
            Ok(value)
        })
        .collect()
}

fn claude_plugins(importer: &CompatibilityImporter) -> Result<Vec<Value>, CompatError> {
    let path = importer.root.join(".claude-plugin/plugin.json");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_str(&importer.read(&path)?)?;
    Ok(vec![json!({
        "source": importer.source(&path)?,
        "name": required_json_string(&value, "name")?,
        "version": required_json_string(&value, "version")?,
        "code_execution": "quarantined",
        "level": "parsed",
    })])
}

fn typescript_extensions(
    importer: &CompatibilityImporter,
    directory: &Path,
) -> Result<Vec<Value>, CompatError> {
    let mut paths = files_with_extension(directory, "ts")?;
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let source = importer.read(&path)?;
            let name = source
                .split("name:")
                .nth(1)
                .and_then(|tail| tail.split(['\'', '"']).nth(1))
                .or_else(|| path.file_stem().and_then(|name| name.to_str()))
                .ok_or_else(|| CompatError::Parse("extension has no name".into()))?;
            Ok(json!({
                "source": importer.source(&path)?,
                "name": name,
                "code_execution": "quarantined",
                "level": "parsed",
            }))
        })
        .collect()
}

fn claude_hooks(label: &str, source: &Path, settings: &Value) -> Result<Vec<Value>, CompatError> {
    let Some(hooks) = settings.get("hooks") else {
        return Ok(Vec::new());
    };
    let hooks = hooks
        .as_object()
        .ok_or_else(|| CompatError::Parse("hooks must be an object".into()))?;
    let mut mapped = Vec::new();
    for (original_event, entries) in hooks {
        mapped.extend(claude_hook_event(label, source, original_event, entries)?);
    }
    Ok(mapped)
}

/// One event's declarations: the mapping lives with the engine that
/// dispatches it, so a declaration reported here as `before_operation` has
/// to be the one the engine will actually run.
fn claude_hook_event(
    label: &str,
    source: &Path,
    original_event: &str,
    entries: &Value,
) -> Result<Vec<Value>, CompatError> {
    let event = crate::hook::LifecycleEvent::from_external(original_event)
        .map_or("unsupported", crate::hook::LifecycleEvent::as_str);
    let entries = entries
        .as_array()
        .ok_or_else(|| CompatError::Parse("hook event must contain an array".into()))?;
    let mut mapped = Vec::new();
    for (position, entry) in entries.iter().enumerate() {
        mapped.extend(claude_hook_entry(
            label,
            source,
            original_event,
            event,
            position,
            entry,
        )?);
    }
    Ok(mapped)
}

/// One entry of one event: read it once, then emit one row per handler.
fn claude_hook_entry(
    label: &str,
    source: &Path,
    original_event: &str,
    event: &str,
    position: usize,
    entry: &Value,
) -> Result<Vec<Value>, CompatError> {
    let matcher = match entry.get("matcher") {
        None => "*",
        Some(value) => value
            .as_str()
            .ok_or_else(|| CompatError::Parse("hook matcher must be a string".into()))?,
    };
    let handlers = entry
        .get("hooks")
        .and_then(Value::as_array)
        .ok_or_else(|| CompatError::Parse("hook entry requires a hooks array".into()))?;
    // The entry's reading, which every handler of it shares: a matcher
    // is declared once for the entry, and so is the class the engine
    // would register it as.
    let effect = if handlers.iter().all(|handler| handler["type"] == "command") {
        "external_command"
    } else {
        "external_hook"
    };
    let matcher = map_tool(matcher).unwrap_or(matcher);
    // One row per handler, because the key an operator switches off
    // names one handler: an entry holding two is two declarations, and
    // a single row for both would name neither of them.
    let mut mapped = Vec::new();
    for (index, handler) in handlers.iter().enumerate() {
        mapped.push(claude_hook_handler(
            label,
            source,
            original_event,
            event,
            position,
            index,
            matcher,
            effect,
            handler,
        )?);
    }
    Ok(mapped)
}

/// One handler of one entry, as the single declaration row the engine keys it by.
#[allow(clippy::too_many_arguments)]
fn claude_hook_handler(
    label: &str,
    source: &Path,
    original_event: &str,
    event: &str,
    position: usize,
    index: usize,
    matcher: &str,
    effect: &str,
    handler: &Value,
) -> Result<Value, CompatError> {
    let kind = required_json_string(handler, "type")?;
    // A handler whose type this build has no reading of is still
    // declared, and still a key an operator can switch off, so it
    // is listed rather than dropped.
    if let Some(key) = match kind {
        "command" => Some("command"),
        "prompt" | "agent" => Some("prompt"),
        "http" => Some("url"),
        _ => None,
    } {
        if required_json_string(handler, key)?.trim().is_empty() {
            return Err(CompatError::Parse("hook handler must not be empty".into()));
        }
    }
    Ok(json!({
        "source": label,
        "event": event,
        "original_event": original_event,
        // The key the engine builds for this declaration, so a
        // listing and the operator's own switches name one thing.
        "declaration": format!("{}#{original_event}[{position}].{index}", source.display()),
        "position": position,
        "index": index,
        "matcher": matcher,
        "effect": effect,
        "handlers": [json!({"type": handler.get("type"), "status": "not_loaded"})],
        "level": if event == "unsupported" { "unsupported" } else { "mapped" },
    }))
}
/// The MCP connections the operator declared for Claude in their own home
/// directory, rather than in a workspace.
///
/// This is deliberately not a `CompatibilityImporter` method: that importer
/// resolves every path inside the workspace, and the guarantee that it cannot
/// be talked into reading outside one is worth keeping. A file the operator
/// owns is a different question to a file a repository carries, so it is read
/// here, by absolute path, and nowhere else.
///
/// The declarations carry the same shape and the same `untrusted` trust label
/// as any other: reading a definition is not connecting to it, and the layer
/// that adopts one decides what it may do.
/// The hooks the operator declared for Claude in their own home directory.
///
/// The engine loads these — `~/.claude/settings.json` carries the operator's
/// own authority — so a listing that read only the workspace named a subset of
/// what runs, and the switches in `/hooks` could not reach a home hook at all.
///
/// Read by absolute path for the same reason [`user_mcp_declarations`] is: the
/// workspace importer must stay unable to read outside its root. The
/// `declaration` key is built from that absolute path, which is the key the
/// engine builds too, so a switch here names the rule there.
pub fn user_hook_declarations(path: &Path) -> Result<Vec<Value>, CompatError> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    if fs::metadata(path)?.len() > MAX_COMPAT_SOURCE_BYTES {
        return Err(CompatError::Parse(format!(
            "{} is larger than {MAX_COMPAT_SOURCE_BYTES} bytes",
            path.display()
        )));
    }
    let settings: Value = serde_json::from_str(&fs::read_to_string(path)?)?;
    claude_hooks(&path.display().to_string(), path, &settings)
}

pub fn user_mcp_declarations(path: &Path) -> Result<Vec<Value>, CompatError> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    if fs::metadata(path)?.len() > MAX_COMPAT_SOURCE_BYTES {
        return Err(CompatError::Parse(format!(
            "{} is larger than {MAX_COMPAT_SOURCE_BYTES} bytes",
            path.display()
        )));
    }
    let value: Value = serde_json::from_str(&fs::read_to_string(path)?)?;
    let Some(servers) = value.get("mcpServers").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let source = path.display().to_string();
    servers
        .iter()
        .map(|(name, server)| mcp_definition(source.clone(), name, server, "mapped"))
        .collect()
}

fn mcp_json(
    importer: &CompatibilityImporter,
    path: &Path,
    level: &str,
) -> Result<Vec<Value>, CompatError> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_str(&importer.read(path)?)?;
    let Some(servers) = value.get("mcpServers") else {
        return Ok(Vec::new());
    };
    let servers = servers
        .as_object()
        .ok_or_else(|| CompatError::Parse("mcpServers must be an object".into()))?;
    servers
        .iter()
        .map(|(name, server)| mcp_definition(importer.source(path)?, name, server, level))
        .collect()
}

fn mcp_toml(
    importer: &CompatibilityImporter,
    source: &Path,
    value: Option<&toml::Value>,
) -> Result<Vec<Value>, CompatError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let servers = value
        .as_table()
        .ok_or_else(|| CompatError::Parse("mcp_servers must be a table".into()))?;
    servers
        .iter()
        .map(|(name, server)| {
            mcp_definition(
                importer.source(source)?,
                name,
                &serde_json::to_value(server)?,
                "mapped",
            )
        })
        .collect()
}

fn mcp_definition(
    source: String,
    name: &str,
    server: &Value,
    level: &str,
) -> Result<Value, CompatError> {
    if !server.is_object() || name.trim().is_empty() {
        return Err(CompatError::Parse(
            "MCP definition requires a name and object".into(),
        ));
    }
    let transport = match server.get("type") {
        Some(value) => value
            .as_str()
            .ok_or_else(|| CompatError::Parse("MCP type must be a string".into()))?,
        None if server.get("url").is_some() => "http",
        None => "stdio",
    };
    let mut result = json!({"source": source, "name": name, "transport": transport, "trust": "untrusted", "level": level});
    match transport {
        "stdio" => {
            let command = required_json_string(server, "command")?;
            if command.trim().is_empty() || server.get("url").is_some() {
                return Err(CompatError::Parse(
                    "stdio MCP requires a non-empty command and no URL".into(),
                ));
            }
            let args = server.get("args").cloned().unwrap_or_else(|| json!([]));
            if !args
                .as_array()
                .is_some_and(|args| args.iter().all(Value::is_string))
            {
                return Err(CompatError::Parse(
                    "MCP args must be an array of strings".into(),
                ));
            }
            result["command"] = json!(command);
            result["args"] = args;
        }
        "http" | "sse" => {
            let url = required_json_string(server, "url")?;
            if !(url.starts_with("https://") || url.starts_with("http://"))
                || url.chars().any(char::is_whitespace)
                || server.get("command").is_some()
            {
                return Err(CompatError::Parse(
                    "HTTP MCP requires an HTTP(S) URL and no command".into(),
                ));
            }
            result["url"] = json!(url);
            if transport == "sse" {
                result["level"] = json!("unsupported");
            }
        }
        _ => return Err(CompatError::Parse("unsupported MCP transport".into())),
    }
    if let Some(enabled) = server.get("enabled") {
        if !enabled.is_boolean() {
            return Err(CompatError::Parse("MCP enabled must be a boolean".into()));
        }
        result["enabled"] = enabled.clone();
    }
    Ok(result)
}

fn existing_sources<const N: usize>(
    importer: &CompatibilityImporter,
    paths: [&Path; N],
) -> Result<Vec<String>, CompatError> {
    paths
        .into_iter()
        .filter(|path| path.is_file())
        .map(|path| importer.source(path))
        .collect()
}

fn child_files(directory: &Path, name: &str) -> Result<Vec<PathBuf>, CompatError> {
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for child in fs::read_dir(directory)? {
        let path = child?.path().join(name);
        if path.is_file() {
            files.push(path);
        }
    }
    Ok(files)
}

fn files_with_extension(directory: &Path, extension: &str) -> Result<Vec<PathBuf>, CompatError> {
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    fs::read_dir(directory)?
        .filter_map(|entry| match entry {
            Ok(entry)
                if entry.path().extension().and_then(|value| value.to_str()) == Some(extension) =>
            {
                Some(Ok(entry.path()))
            }
            Ok(_) => None,
            Err(error) => Some(Err(CompatError::Io(error))),
        })
        .collect()
}

fn front_matter(input: &str) -> BTreeMap<String, String> {
    let mut lines = input.lines();
    if lines.next() != Some("---") {
        return BTreeMap::new();
    }
    lines
        .take_while(|line| *line != "---")
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| {
            (
                key.trim().to_owned(),
                value.trim().trim_matches('"').to_owned(),
            )
        })
        .collect()
}

pub(crate) fn map_tool(value: &str) -> Option<&'static str> {
    match value.to_ascii_lowercase().as_str() {
        "read" => Some("fs.read"),
        "grep" => Some("search.text"),
        "bash" => Some("process.exec"),
        _ => None,
    }
}

fn map_claude_permission(value: &str) -> String {
    value
        .replace("Read(", "fs.read:")
        .replace("Bash(", "process.exec:")
        .trim_end_matches(')')
        .to_owned()
}

fn mapped_strings(value: Option<&Value>, map: fn(&str) -> String) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(map)
        .collect()
}

fn read_json_or_empty(importer: &CompatibilityImporter, path: &Path) -> Result<Value, CompatError> {
    if path.is_file() {
        serde_json::from_str(&importer.read(path)?).map_err(Into::into)
    } else {
        Ok(json!({}))
    }
}

fn merge_object(target: &mut Value, update: Value) -> Result<(), CompatError> {
    let target = target
        .as_object_mut()
        .ok_or_else(|| CompatError::Parse("settings root must be an object".into()))?;
    let update = update
        .as_object()
        .ok_or_else(|| CompatError::Parse("settings override must be an object".into()))?;
    target.extend(update.clone());
    Ok(())
}

fn unknown_keys(
    value: &Value,
    known: &[&str],
    source: String,
) -> Result<Vec<Diagnostic>, CompatError> {
    let object = value
        .as_object()
        .ok_or_else(|| CompatError::Parse("settings root must be an object".into()))?;
    Ok(object
        .keys()
        .filter(|key| !known.contains(&key.as_str()))
        .map(|key| diagnostic(source.clone(), key))
        .collect())
}

fn unknown_toml_keys(
    table: &toml::map::Map<String, toml::Value>,
    known: &[&str],
    source: String,
) -> Vec<Diagnostic> {
    table
        .keys()
        .filter(|key| !known.contains(&key.as_str()))
        .map(|key| diagnostic(source.clone(), key))
        .collect()
}

fn diagnostic(source: String, key: &str) -> Diagnostic {
    let lower = key.to_ascii_lowercase();
    let fail_closed = [
        "permission",
        "sandbox",
        "approval",
        "hook",
        "secret",
        "token",
        "key",
    ]
    .iter()
    .any(|part| lower.contains(part));
    Diagnostic {
        source,
        key: key.to_owned(),
        message: "unknown configuration key".into(),
        fail_closed,
    }
}

fn reject_sensitive_unknowns(diagnostics: &[Diagnostic]) -> Result<(), CompatError> {
    if let Some(diagnostic) = diagnostics.iter().find(|item| item.fail_closed) {
        return Err(CompatError::SecurityUnknown(diagnostic.key.clone()));
    }
    Ok(())
}

fn required_json_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, CompatError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| CompatError::Missing(key.into()))
}

fn string<'a>(
    table: &'a toml::map::Map<String, toml::Value>,
    key: &str,
) -> Result<&'a str, CompatError> {
    table
        .get(key)
        .and_then(toml::Value::as_str)
        .ok_or_else(|| CompatError::Missing(key.into()))
}

fn map_approval(value: &str) -> Result<&'static str, CompatError> {
    match value {
        "on-request" | "untrusted" => Ok("ask"),
        "never" => Ok("deny"),
        other => Err(CompatError::Parse(format!(
            "unsupported approval policy {other}"
        ))),
    }
}

fn slash(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[derive(Debug)]
pub enum CompatError {
    OutsideRoot,
    Missing(String),
    Parse(String),
    SecurityUnknown(String),
    UnsupportedSandbox(String),
    Io(io::Error),
    Json(serde_json::Error),
}

impl fmt::Display for CompatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideRoot => {
                formatter.write_str("working directory or source is outside import root")
            }
            Self::Missing(value) => {
                write!(formatter, "missing required compatibility value {value}")
            }
            Self::Parse(message) => write!(formatter, "compatibility parse failed: {message}"),
            Self::SecurityUnknown(key) => write!(
                formatter,
                "unknown security-sensitive key {key}; import failed closed"
            ),
            Self::UnsupportedSandbox(mode) => write!(
                formatter,
                "sandbox mode {mode} has no equal-or-stronger ARSY mapping"
            ),
            Self::Io(error) => error.fmt(formatter),
            Self::Json(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CompatError {}

impl From<io::Error> for CompatError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for CompatError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_transports_and_arguments_are_validated_without_executing() {
        let parse = |value| mcp_definition("config".into(), "docs", &value, "mapped");
        let http = parse(json!({"url": "https://example.test/mcp", "enabled": false, "headers": {"Authorization": "never display"}})).unwrap();
        assert_eq!(http["transport"], "http");
        assert_eq!(http["enabled"], false);
        assert!(http.get("headers").is_none());
        for bad in [
            json!({"command": ""}),
            json!({"command": "node", "args": [1]}),
            json!({"command": "node", "url": "https://example.test"}),
            json!({"url": "file:///private"}),
            json!({"command": "node", "enabled": "yes"}),
            json!({"type": 4}),
        ] {
            assert!(parse(bad).is_err());
        }
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        fs::write(&config, "").unwrap();
        let importer = CompatibilityImporter::new(root.path());
        let bad: toml::Value =
            toml::from_str("[docs]\ncommand = 'node'\nargs = ['valid', 1]").unwrap();
        assert!(mcp_toml(&importer, &config, Some(&bad)).is_err());
    }

    #[test]
    fn hooks_preserve_events_matchers_and_override_origin_without_executing() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".claude")).unwrap();
        let local = root.path().join(".claude/settings.local.json");
        fs::write(&local, json!({"hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": "never execute"}]}],
            "PostToolUse": [{"matcher": "Bash|Read", "hooks": [{"type": "prompt", "prompt": "inspect"}]}],
            "FutureEvent": [{"hooks": [{"type": "future"}]}]
        }}).to_string()).unwrap();
        let importer = CompatibilityImporter::new(root.path());
        let report = importer
            .import(Ecosystem::Claude, root.path(), "hooks")
            .unwrap();
        let hooks = report.canonical["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 3);
        assert!(hooks.iter().all(|hook| hook["source"]
            .as_str()
            .unwrap()
            .ends_with("settings.local.json")));
        assert!(hooks
            .iter()
            .any(|hook| hook["event"] == "unsupported" && hook["level"] == "unsupported"));
        assert!(hooks
            .iter()
            .any(|hook| hook["matcher"] == "Bash|Read" && hook["effect"] == "external_hook"));
        for bad in [
            json!({"hooks": []}),
            json!({"hooks": {"Stop": {}}}),
            json!({"hooks": {"Stop": [{"hooks": [{"type": "command"}]}]}}),
        ] {
            assert!(claude_hooks(&importer.source(&local).unwrap(), &local, &bad).is_err());
        }
    }

    /// The key the engine builds and the key a row carries have to be the same
    /// string, or an operator switching a declaration off would be naming
    /// something the engine never registers.
    #[test]
    fn a_hook_row_names_the_declaration_key_the_engine_registers() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join(".claude/settings.json");
        let settings = json!({"hooks": {
            "PreToolUse": [
                {"matcher": "Bash", "hooks": [
                    {"type": "command", "command": "audit"},
                    {"type": "prompt", "prompt": "think about it"}
                ]},
                {"hooks": [{"type": "command", "command": "check"}]}
            ],
            "Stop": [{"hooks": [{"type": "command", "command": "done"}]}]
        }});
        let importer = CompatibilityImporter::new(root.path());

        let hooks = claude_hooks(&importer.source(&source).unwrap(), &source, &settings).unwrap();
        let declarations: Vec<&str> = hooks
            .iter()
            .map(|hook| hook["declaration"].as_str().unwrap())
            .collect();
        // One row per handler, so the second handler of the first entry is a
        // declaration of its own and not folded into the first.
        assert_eq!(
            declarations,
            [
                format!("{}#PreToolUse[0].0", source.display()),
                format!("{}#PreToolUse[0].1", source.display()),
                format!("{}#PreToolUse[1].0", source.display()),
                format!("{}#Stop[0].0", source.display()),
            ],
            "{hooks:#?}"
        );
        // The two numbers that compose it, so a listing can show them apart
        // from the key.
        let nested = &hooks[2];
        assert_eq!(nested["position"], 1);
        assert_eq!(nested["index"], 0);
        assert_eq!(nested["original_event"], "PreToolUse");
        assert_eq!(nested["handlers"].as_array().unwrap().len(), 1);
        // The entry's own reading still reaches every row of it: `Bash` maps,
        // and an entry with a `prompt` in it is not a plain command hook.
        assert_eq!(hooks[0]["matcher"], "process.exec");
        assert_eq!(hooks[1]["effect"], "external_hook");
        assert_eq!(hooks[1]["level"], "mapped");
    }

    #[test]
    fn sensitive_unknowns_and_unsafe_sandboxes_fail_closed() {
        let diagnostics = vec![diagnostic("config".into(), "permission_magic")];
        assert!(matches!(
            reject_sensitive_unknowns(&diagnostics),
            Err(CompatError::SecurityUnknown(_))
        ));
        assert_eq!(map_approval("never").unwrap(), "deny");
    }

    #[test]
    fn unknown_session_entries_remain_opaque_and_hashlines_map_directly() {
        let value = json!({"type": "future", "payload": {"x": 1}});
        assert_eq!(
            import_session_entry(value.clone(), &BTreeSet::new()),
            ImportedSessionEntry::Opaque(value)
        );
        let digest = "00".repeat(32);
        assert!(matches!(
            parse_hashline_anchor(&format!("{digest}:{digest}")).unwrap(),
            EditAddress::ContentAnchor { .. }
        ));
        let handle = imported_credential("openai").unwrap();
        assert_eq!(handle.to_string(), "secret://os/openai");
    }

    #[test]
    fn markdown_front_matter_accepts_windows_line_endings() {
        let fields = front_matter("---\r\nname: reviewer\r\ntools: Read, Grep\r\n---\r\nbody");
        assert_eq!(fields.get("name").map(String::as_str), Some("reviewer"));
        assert_eq!(fields.get("tools").map(String::as_str), Some("Read, Grep"));
    }

    #[cfg(unix)]
    #[test]
    fn instruction_symlinks_outside_the_import_root_need_explicit_consent() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        symlink(outside.path(), root.path().join("CLAUDE.md")).unwrap();
        let result = CompatibilityImporter::new(root.path()).import(
            Ecosystem::Claude,
            root.path(),
            "fixture",
        );
        assert!(
            matches!(result, Err(CompatError::OutsideRoot)),
            "{result:?}"
        );
    }
}
