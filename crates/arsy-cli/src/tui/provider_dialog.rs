//! The interactive `/provider` dialog: a 3-pane split view for
//! access-method -> provider -> manage. It unifies what `/provider` and
//! `/auth` used to collect separately: picking an endpoint, adding one,
//! signing in with OAuth, storing an API key, and removing one.
//!
//! The dialog is pure navigation. Anything that collects text — an API key,
//! a base URL, an OAuth code — is handed back to the existing composer wizard
//! by the driver, so every validation and masking rule stays in one place.
use super::*;

/// Which column of the dialog currently holds navigation focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderPane {
    Access,
    List,
    Manage,
}

impl ProviderPane {
    pub fn next(self) -> Self {
        match self {
            Self::Access => Self::List,
            Self::List => Self::Manage,
            Self::Manage => Self::Access,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Access => Self::Manage,
            Self::List => Self::Access,
            Self::Manage => Self::List,
        }
    }
}

/// How the operator wants to reach a provider. Every one of these is a thin
/// framing over the two mechanisms the configuration actually has: an OAuth
/// login, or a dialect + base URL + credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessMethod {
    OAuth,
    Key,
    Custom,
    Local,
}

impl AccessMethod {
    pub fn all() -> Vec<Self> {
        vec![Self::OAuth, Self::Key, Self::Custom, Self::Local]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::OAuth => "OAuth",
            Self::Key => "API key",
            Self::Custom => "Custom",
            Self::Local => "Local",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::OAuth => "subscription / browser sign-in",
            Self::Key => "a provider API key",
            Self::Custom => "any OpenAI-compatible URL",
            Self::Local => "Ollama / LM Studio on localhost",
        }
    }
}

/// A configured endpoint, sorted into the one access bucket it belongs to, so
/// the access column actually filters the provider column rather than showing
/// every endpoint under every method.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfiguredEndpoint {
    pub id: String,
    pub access: AccessMethod,
    /// Signing in is possible: the endpoint carries an OAuth block or matches a
    /// built-in OAuth preset.
    pub oauth: bool,
}

/// Which access bucket a configured endpoint belongs to. One bucket only: a
/// login endpoint is OAuth, a `localhost` one is Local, a curated key preset is
/// API key, and anything else the operator set by hand is Custom.
pub fn classify(id: &str, base_url: &str, has_oauth: bool) -> AccessMethod {
    if has_oauth || arsy_kernel::oauth::presets::get(id).is_some() {
        AccessMethod::OAuth
    } else if is_local(base_url) {
        AccessMethod::Local
    } else if PROVIDER_PRESETS
        .iter()
        .any(|p| p.id == id && p.method == AccessMethod::Key)
    {
        AccessMethod::Key
    } else {
        AccessMethod::Custom
    }
}

fn is_local(base_url: &str) -> bool {
    ["localhost", "127.0.0.1", "0.0.0.0", "[::1]"]
        .iter()
        .any(|host| base_url.contains(host))
}

/// A curated base-URL shortcut for the Key and Local methods. It only prefills
/// the add wizard's fields; it is not a new adapter, and the operator can edit
/// every value it fills.
pub struct ProviderPreset {
    pub id: &'static str,
    pub label: &'static str,
    /// One of the two dialects the configuration accepts.
    pub kind: &'static str,
    pub base_url: &'static str,
    pub method: AccessMethod,
    /// Offered by `/model` after the endpoint is added; the first is default.
    pub models: &'static [&'static str],
}

/// The curated presets. Deliberately short — a bigger registry is a future
/// step tracked in `.claude/provider-access-roadmap.md`, not code carried now.
pub static PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "openrouter",
        label: "OpenRouter — many models behind one key",
        kind: "openai",
        base_url: "https://openrouter.ai/api/v1",
        method: AccessMethod::Key,
        models: &[],
    },
    ProviderPreset {
        id: "deepseek",
        label: "DeepSeek API",
        kind: "openai",
        base_url: "https://api.deepseek.com",
        method: AccessMethod::Key,
        models: &["deepseek-chat", "deepseek-reasoner"],
    },
    ProviderPreset {
        id: "openai",
        label: "OpenAI",
        kind: "openai",
        base_url: "https://api.openai.com/v1",
        method: AccessMethod::Key,
        models: &["gpt-4o", "gpt-4o-mini", "o3", "o4-mini"],
    },
    ProviderPreset {
        id: "anthropic",
        label: "Anthropic",
        kind: "anthropic",
        base_url: "https://api.anthropic.com",
        method: AccessMethod::Key,
        models: &["claude-opus-4-8", "claude-sonnet-4-6", "claude-haiku-4-5"],
    },
    ProviderPreset {
        id: "mistral",
        label: "Mistral AI",
        kind: "openai",
        base_url: "https://api.mistral.ai/v1",
        method: AccessMethod::Key,
        models: &["mistral-large-latest", "mistral-small-latest"],
    },
    ProviderPreset {
        id: "gemini",
        label: "Google Gemini",
        kind: "openai",
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai/",
        method: AccessMethod::Key,
        models: &["gemini-2.5-pro", "gemini-2.5-flash"],
    },
    ProviderPreset {
        id: "xai",
        label: "xAI — Grok",
        kind: "openai",
        base_url: "https://api.x.ai/v1",
        method: AccessMethod::Key,
        models: &["grok-3", "grok-3-mini"],
    },
    ProviderPreset {
        id: "groq",
        label: "Groq — fast inference",
        kind: "openai",
        base_url: "https://api.groq.com/openai/v1",
        method: AccessMethod::Key,
        models: &[],
    },
    ProviderPreset {
        id: "together",
        label: "Together AI",
        kind: "openai",
        base_url: "https://api.together.xyz/v1",
        method: AccessMethod::Key,
        models: &[],
    },
    ProviderPreset {
        id: "fireworks",
        label: "Fireworks AI",
        kind: "openai",
        base_url: "https://api.fireworks.ai/inference/v1",
        method: AccessMethod::Key,
        models: &[],
    },
    ProviderPreset {
        id: "cerebras",
        label: "Cerebras — fast inference",
        kind: "openai",
        base_url: "https://api.cerebras.ai/v1",
        method: AccessMethod::Key,
        models: &["llama3.3-70b", "llama-3.1-8b"],
    },
    ProviderPreset {
        id: "perplexity",
        label: "Perplexity — search-augmented",
        kind: "openai",
        base_url: "https://api.perplexity.ai",
        method: AccessMethod::Key,
        models: &["sonar-pro", "sonar"],
    },
    ProviderPreset {
        id: "cohere",
        label: "Cohere",
        kind: "openai",
        base_url: "https://api.cohere.com/v1",
        method: AccessMethod::Key,
        models: &["command-r-plus", "command-r"],
    },
    ProviderPreset {
        id: "nvidia",
        label: "NVIDIA NIM",
        kind: "openai",
        base_url: "https://integrate.api.nvidia.com/v1",
        method: AccessMethod::Key,
        models: &[],
    },
    ProviderPreset {
        id: "ollama",
        label: "Ollama — local server",
        kind: "openai",
        base_url: "http://localhost:11434/v1",
        method: AccessMethod::Local,
        models: &[],
    },
    ProviderPreset {
        id: "lmstudio",
        label: "LM Studio — local server",
        kind: "openai",
        base_url: "http://localhost:1234/v1",
        method: AccessMethod::Local,
        models: &[],
    },
];

/// A row in the List pane: a preset offered for the chosen access method, an
/// endpoint already configured, or the pseudo-row that starts a manual add.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderRow {
    pub id: String,
    pub label: String,
    /// Already named in the configuration, so it can be used or removed.
    pub configured: bool,
    /// A login is possible: an OAuth preset, or a configured endpoint that
    /// matches one.
    pub oauth: bool,
    pub kind: String,
    pub base_url: String,
    pub models: Vec<String>,
    /// The "add a new endpoint by hand" row, which carries no preset.
    pub is_new: bool,
}

/// One action the Manage pane offers for the selected List row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManageKind {
    Use,
    SignIn,
    SetKey,
    FetchModels,
    Add,
    Remove,
}

impl ManageKind {
    fn label(self) -> &'static str {
        match self {
            Self::Use => "Use (make default)",
            Self::SignIn => "Sign in (OAuth)",
            Self::SetKey => "Set API key",
            Self::FetchModels => "Fetch model list",
            Self::Add => "Add this endpoint",
            Self::Remove => "Remove",
        }
    }
}

/// What the dialog decided. The driver turns each into either a direct write
/// or a handoff to the composer wizard that collects the text it needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderDialogAction {
    SetDefault(String),
    Remove(String),
    Login(String),
    SetKey(String),
    /// Add a preset endpoint; the driver prefills the wizard from this row.
    AddPreset(ProviderRow),
    /// Start the manual add wizard from its first question.
    NewCustom,
    /// Fetch the live model list for a configured endpoint and update config.
    FetchModels(String),
    Close,
}

/// Interactive state for the 3-pane provider dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderDialogState {
    pub methods: Vec<AccessMethod>,
    pub endpoints: Vec<ConfiguredEndpoint>,
    pub default_provider: Option<String>,
    pub active_pane: ProviderPane,
    pub selected_method: usize,
    pub selected_row: usize,
    pub selected_action: usize,
    pub notice: Option<String>,
}

impl ProviderDialogState {
    pub fn new(endpoints: Vec<ConfiguredEndpoint>, default_provider: Option<String>) -> Self {
        Self {
            methods: AccessMethod::all(),
            endpoints,
            default_provider,
            active_pane: ProviderPane::Access,
            selected_method: 0,
            selected_row: 0,
            selected_action: 0,
            notice: None,
        }
    }

    fn method(&self) -> AccessMethod {
        self.methods
            .get(self.selected_method)
            .copied()
            .unwrap_or(AccessMethod::OAuth)
    }

    /// The rows the List pane shows for the currently selected access method.
    ///
    /// Each access lists the presets that are ways to add through it, then the
    /// configured endpoints that belong to *this* method only — so the access
    /// column genuinely filters the provider column instead of repeating every
    /// endpoint under every method.
    pub fn rows(&self) -> Vec<ProviderRow> {
        let method = self.method();
        let is_configured = |id: &str| self.endpoints.iter().any(|e| e.id == id);
        let mut rows: Vec<ProviderRow> = Vec::new();
        match method {
            AccessMethod::OAuth => {
                for preset in arsy_kernel::oauth::presets::all() {
                    rows.push(ProviderRow {
                        id: preset.id.to_owned(),
                        label: preset.label.to_owned(),
                        configured: is_configured(preset.id),
                        oauth: true,
                        kind: String::new(),
                        base_url: preset.base_url.to_owned(),
                        models: preset.models.iter().map(|m| (*m).to_owned()).collect(),
                        is_new: false,
                    });
                }
            }
            AccessMethod::Key | AccessMethod::Local => {
                for preset in PROVIDER_PRESETS.iter().filter(|p| p.method == method) {
                    rows.push(ProviderRow {
                        id: preset.id.to_owned(),
                        label: preset.label.to_owned(),
                        configured: is_configured(preset.id),
                        oauth: false,
                        kind: preset.kind.to_owned(),
                        base_url: preset.base_url.to_owned(),
                        models: preset.models.iter().map(|m| (*m).to_owned()).collect(),
                        is_new: false,
                    });
                }
            }
            AccessMethod::Custom => {}
        }
        // Configured endpoints sorted into this access bucket, and not already
        // shown as one of its presets.
        for ep in self.endpoints.iter().filter(|e| e.access == method) {
            if !rows.iter().any(|row| row.id == ep.id) {
                rows.push(ProviderRow {
                    id: ep.id.clone(),
                    label: "configured endpoint".to_owned(),
                    configured: true,
                    oauth: ep.oauth,
                    kind: String::new(),
                    base_url: String::new(),
                    models: Vec::new(),
                    is_new: false,
                });
            }
        }
        // Custom always ends with the manual-add row; there is nothing to
        // preset for a URL only the operator knows.
        if method == AccessMethod::Custom {
            rows.push(ProviderRow {
                id: "+new".to_owned(),
                label: "add a new endpoint by hand".to_owned(),
                configured: false,
                oauth: false,
                kind: String::new(),
                base_url: String::new(),
                models: Vec::new(),
                is_new: true,
            });
        }
        rows
    }

    fn selected(&self) -> Option<ProviderRow> {
        self.rows().get(self.selected_row).cloned()
    }

    /// The actions the Manage pane offers for the selected List row.
    pub fn manage_kinds(&self) -> Vec<ManageKind> {
        let Some(row) = self.selected() else {
            return Vec::new();
        };
        if row.is_new {
            return vec![ManageKind::Add];
        }
        if row.configured {
            let mut kinds = vec![ManageKind::Use];
            if row.oauth {
                kinds.push(ManageKind::SignIn);
            }
            kinds.push(ManageKind::SetKey);
            kinds.push(ManageKind::FetchModels);
            kinds.push(ManageKind::Remove);
            return kinds;
        }
        // A preset not yet configured: sign in, or add it with a key.
        if row.oauth {
            vec![ManageKind::SignIn]
        } else {
            vec![ManageKind::Add]
        }
    }

    pub fn handle_key(&mut self, key: Key) -> Option<ProviderDialogAction> {
        match key {
            Key::Left | Key::CycleMode => {
                self.active_pane = self.active_pane.prev();
                self.clamp();
                None
            }
            Key::Right | Key::Tab => {
                self.active_pane = self.active_pane.next();
                self.clamp();
                None
            }
            Key::Up => {
                self.step(false);
                None
            }
            Key::Down => {
                self.step(true);
                None
            }
            Key::Enter | Key::Newline => self.confirm(),
            Key::Interrupt | Key::Eof => Some(ProviderDialogAction::Close),
            _ => None,
        }
    }

    /// Enter acts from whichever pane holds focus, so the operator can commit
    /// as soon as the marked action is right rather than always tabbing to it.
    fn confirm(&mut self) -> Option<ProviderDialogAction> {
        let kinds = self.manage_kinds();
        let Some(row) = self.selected() else {
            self.notice = Some("nothing to act on for this access method".to_owned());
            return None;
        };
        let Some(kind) = kinds.get(self.selected_action).copied() else {
            self.notice = Some("no action available for this row".to_owned());
            return None;
        };
        Some(match kind {
            ManageKind::Use => ProviderDialogAction::SetDefault(row.id),
            ManageKind::SignIn => ProviderDialogAction::Login(row.id),
            ManageKind::SetKey => ProviderDialogAction::SetKey(row.id),
            ManageKind::FetchModels => ProviderDialogAction::FetchModels(row.id),
            ManageKind::Remove => ProviderDialogAction::Remove(row.id),
            ManageKind::Add if row.is_new => ProviderDialogAction::NewCustom,
            ManageKind::Add => ProviderDialogAction::AddPreset(row),
        })
    }

    fn step(&mut self, forward: bool) {
        match self.active_pane {
            ProviderPane::Access => {
                let count = self.methods.len();
                if count > 0 {
                    self.selected_method = wrap(self.selected_method, count, forward);
                    self.selected_row = 0;
                    self.selected_action = 0;
                }
            }
            ProviderPane::List => {
                let count = self.rows().len();
                if count > 0 {
                    self.selected_row = wrap(self.selected_row, count, forward);
                    self.selected_action = 0;
                }
            }
            ProviderPane::Manage => {
                let count = self.manage_kinds().len();
                if count > 0 {
                    self.selected_action = wrap(self.selected_action, count, forward);
                }
            }
        }
    }

    /// Keep the row and action markers in range after the list under them
    /// changes, so a shorter list never leaves a marker past its end.
    fn clamp(&mut self) {
        let rows = self.rows().len();
        if self.selected_row >= rows {
            self.selected_row = rows.saturating_sub(1);
        }
        let actions = self.manage_kinds().len();
        if self.selected_action >= actions {
            self.selected_action = actions.saturating_sub(1);
        }
    }

    pub fn render(&self, width: usize, colour: bool) -> String {
        let width = width.max(MIN_WIDTH);
        let inner = width.saturating_sub(4);
        let mut lines = vec![dialog_top(" PROVIDER ", width, colour)];
        if inner < 65 {
            lines.extend(self.single_pane_lines(inner, colour));
        } else {
            lines.extend(self.multi_pane_lines(inner, colour));
        }
        lines.push(dialog_line("", inner, colour, ""));
        if let Some(notice) = &self.notice {
            lines.push(dialog_line(notice, inner, colour, sgr_dim()));
        }
        let footer = "[←/→/Tab] Switch Pane  [↑/↓] Select  [Enter] Confirm  [Esc] Close";
        lines.push(dialog_line(footer, inner, colour, sgr_dim()));
        lines.push(dialog_line(
            "a provider change applies at the next session; a key or login is usable now",
            inner,
            colour,
            sgr_dim(),
        ));
        lines.push(paint(
            colour,
            sgr_border(),
            &format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
        ));
        lines.join("\n")
    }

    fn multi_pane_lines(&self, inner: usize, colour: bool) -> Vec<String> {
        let available = inner.saturating_sub(6);
        let access_width = (available * 22 / 100).clamp(12, 18);
        let manage_width = (available * 30 / 100).clamp(18, 26);
        let list_width = available.saturating_sub(access_width + manage_width);
        let divider = paint(colour, sgr_border(), " │ ");

        let mut lines = Vec::new();
        let a_hdr = self.pane_header("ACCESS", ProviderPane::Access, colour);
        let l_hdr = self.pane_header("PROVIDER", ProviderPane::List, colour);
        let m_hdr = self.pane_header("MANAGE", ProviderPane::Manage, colour);
        lines.push(border_line(
            &format!(
                "{}{divider}{}{divider}{}",
                cell(&a_hdr, access_width),
                cell(&l_hdr, list_width),
                cell(&m_hdr, manage_width),
            ),
            inner,
            colour,
        ));

        let sep = format!(
            "{}─┼─{}─┼─{}",
            "─".repeat(access_width),
            "─".repeat(list_width),
            "─".repeat(manage_width)
        );
        lines.push(border_line(
            &paint(colour, sgr_border(), &sep),
            inner,
            colour,
        ));

        let a_lines = self.access_lines(colour);
        let l_lines = self.list_lines(colour);
        let m_lines = self.manage_lines(colour);
        let max_lines = a_lines.len().max(l_lines.len()).max(m_lines.len()).max(6);
        for i in 0..max_lines {
            let a_cell = cell(
                a_lines.get(i).map(String::as_str).unwrap_or(""),
                access_width,
            );
            let l_cell = cell(l_lines.get(i).map(String::as_str).unwrap_or(""), list_width);
            let m_cell = cell(
                m_lines.get(i).map(String::as_str).unwrap_or(""),
                manage_width,
            );
            lines.push(border_line(
                &format!("{a_cell}{divider}{l_cell}{divider}{m_cell}"),
                inner,
                colour,
            ));
        }
        lines
    }

    fn access_lines(&self, colour: bool) -> Vec<String> {
        self.methods
            .iter()
            .enumerate()
            .map(|(index, method)| {
                let selected = index == self.selected_method;
                let prefix = marker(self.active_pane == ProviderPane::Access, selected);
                self.paint_row(
                    colour,
                    ProviderPane::Access,
                    selected,
                    &format!("{prefix}{}", method.label()),
                )
            })
            .collect()
    }

    fn list_lines(&self, colour: bool) -> Vec<String> {
        let rows = self.rows();
        if rows.is_empty() {
            return vec![paint(colour, sgr_dim(), "  (nothing here)")];
        }
        rows.iter()
            .enumerate()
            .map(|(index, row)| {
                let selected = index == self.selected_row;
                let prefix = marker(self.active_pane == ProviderPane::List, selected);
                let badge = if self.default_provider.as_deref() == Some(row.id.as_str()) {
                    " [default]"
                } else if row.configured {
                    " [set]"
                } else {
                    ""
                };
                self.paint_row(
                    colour,
                    ProviderPane::List,
                    selected,
                    &format!("{prefix}{}{badge}", row.id),
                )
            })
            .collect()
    }

    fn manage_lines(&self, colour: bool) -> Vec<String> {
        let kinds = self.manage_kinds();
        if kinds.is_empty() {
            return vec![paint(colour, sgr_dim(), "  (no actions)")];
        }
        kinds
            .iter()
            .enumerate()
            .map(|(index, kind)| {
                let selected = index == self.selected_action;
                let prefix = marker(self.active_pane == ProviderPane::Manage, selected);
                self.paint_row(
                    colour,
                    ProviderPane::Manage,
                    selected,
                    &format!("{prefix}{}", kind.label()),
                )
            })
            .collect()
    }

    fn paint_row(&self, colour: bool, pane: ProviderPane, selected: bool, text: &str) -> String {
        if self.active_pane == pane && selected {
            paint(colour, sgr_accent(), text)
        } else if selected {
            paint(colour, BOLD, text)
        } else {
            paint(colour, sgr_dim(), text)
        }
    }

    fn pane_header(&self, title: &str, pane: ProviderPane, colour: bool) -> String {
        if self.active_pane == pane {
            if colour {
                format!("{BOLD}{title}{RESET}")
            } else {
                format!("[{title}]")
            }
        } else {
            paint(colour, sgr_dim(), title)
        }
    }

    fn single_pane_lines(&self, inner: usize, colour: bool) -> Vec<String> {
        let mut lines = Vec::new();
        let method = self.method();
        let access_marker = if self.active_pane == ProviderPane::Access {
            "›"
        } else {
            "·"
        };
        lines.push(paint(
            colour,
            sgr_dim(),
            &format!(
                "{access_marker} access: {}  [Tab] switch pane",
                method.label()
            ),
        ));
        for row in self.list_lines(colour) {
            lines.push(row);
        }
        for action in self.manage_lines(colour) {
            lines.push(action);
        }
        lines
            .into_iter()
            .map(|l| border_line(&l, inner, colour))
            .collect()
    }
}

fn wrap(current: usize, count: usize, forward: bool) -> usize {
    if forward {
        (current + 1) % count
    } else {
        (current + count - 1) % count
    }
}

fn marker(active: bool, selected: bool) -> &'static str {
    match (active, selected) {
        (true, true) => "› ",
        (false, true) => "· ",
        _ => "  ",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(id: &str, access: AccessMethod) -> ConfiguredEndpoint {
        ConfiguredEndpoint {
            id: id.to_owned(),
            access,
            oauth: access == AccessMethod::OAuth,
        }
    }

    fn state() -> ProviderDialogState {
        ProviderDialogState::new(
            vec![
                ep("deepseek", AccessMethod::Key),
                ep("work", AccessMethod::Custom),
            ],
            Some("work".to_owned()),
        )
    }

    #[test]
    fn tab_and_arrows_switch_panes_with_wrapping() {
        let mut s = state();
        assert_eq!(s.active_pane, ProviderPane::Access);
        s.handle_key(Key::Tab);
        assert_eq!(s.active_pane, ProviderPane::List);
        s.handle_key(Key::Tab);
        assert_eq!(s.active_pane, ProviderPane::Manage);
        s.handle_key(Key::Tab);
        assert_eq!(s.active_pane, ProviderPane::Access);
        s.handle_key(Key::Left);
        assert_eq!(s.active_pane, ProviderPane::Manage);
    }

    #[test]
    fn oauth_access_lists_presets_and_offers_sign_in() {
        let mut s = state();
        // Access pane starts on OAuth (index 0).
        assert_eq!(s.method(), AccessMethod::OAuth);
        let rows = s.rows();
        assert!(rows.iter().all(|r| r.oauth || r.configured));
        assert!(rows
            .iter()
            .any(|row| row.id == "codex-oauth" && row.oauth && !row.configured));
        // Move to the list, pick the first preset, and check the actions.
        s.handle_key(Key::Tab);
        assert_eq!(s.active_pane, ProviderPane::List);
        let kinds = s.manage_kinds();
        assert!(kinds.contains(&ManageKind::SignIn));
    }

    #[test]
    fn key_access_preset_maps_to_add_with_prefill() {
        let mut s = ProviderDialogState::new(Vec::new(), None);
        // Move to Key access.
        s.handle_key(Key::Down); // OAuth -> Key
        assert_eq!(s.method(), AccessMethod::Key);
        let rows = s.rows();
        let idx = rows
            .iter()
            .position(|r| r.id == "groq")
            .expect("groq preset");
        s.handle_key(Key::Tab); // to list
        for _ in 0..idx {
            s.handle_key(Key::Down);
        }
        s.handle_key(Key::Tab); // to manage
        match s.handle_key(Key::Enter) {
            Some(ProviderDialogAction::AddPreset(row)) => {
                assert_eq!(row.id, "groq");
                assert_eq!(row.kind, "openai");
                assert!(row.base_url.starts_with("https://"));
            }
            other => panic!("expected AddPreset, got {other:?}"),
        }
    }

    #[test]
    fn configured_endpoint_can_be_used_and_removed() {
        let mut s = state();
        // Custom access lists configured endpoints plus the manual-add row.
        s.handle_key(Key::Up); // OAuth -> Local (wrap back)
        s.handle_key(Key::Up); // Local -> Custom
        assert_eq!(s.method(), AccessMethod::Custom);
        let rows = s.rows();
        let idx = rows
            .iter()
            .position(|r| r.id == "work")
            .expect("configured work");
        s.handle_key(Key::Tab);
        for _ in 0..idx {
            s.handle_key(Key::Down);
        }
        let kinds = s.manage_kinds();
        assert_eq!(kinds[0], ManageKind::Use);
        assert!(kinds.contains(&ManageKind::Remove));
        s.handle_key(Key::Tab); // manage, first action = Use
        assert_eq!(
            s.handle_key(Key::Enter),
            Some(ProviderDialogAction::SetDefault("work".to_owned()))
        );
    }

    #[test]
    fn custom_new_row_starts_manual_wizard() {
        let mut s = ProviderDialogState::new(Vec::new(), None);
        s.handle_key(Key::Up); // -> Local
        s.handle_key(Key::Up); // -> Custom
        let rows = s.rows();
        assert!(rows.last().is_some_and(|r| r.is_new));
        s.handle_key(Key::Tab); // list
                                // Only the +new row exists (no configured endpoints).
        s.handle_key(Key::Tab); // manage
        assert_eq!(
            s.handle_key(Key::Enter),
            Some(ProviderDialogAction::NewCustom)
        );
    }

    #[test]
    fn access_filters_the_provider_list_to_its_own_bucket() {
        // A custom endpoint must appear only under Custom — not repeated under
        // every access method.
        let s = ProviderDialogState::new(
            vec![
                ep("myai", AccessMethod::Custom),
                ep("hari", AccessMethod::Custom),
                ep("antigravity", AccessMethod::OAuth),
            ],
            None,
        );
        let ids = |method: AccessMethod| {
            let mut s = s.clone();
            s.selected_method = s.methods.iter().position(|m| *m == method).unwrap();
            s.rows().into_iter().map(|r| r.id).collect::<Vec<_>>()
        };
        assert!(ids(AccessMethod::Custom).contains(&"myai".to_owned()));
        assert!(ids(AccessMethod::Custom).contains(&"hari".to_owned()));
        // Not leaked into other buckets.
        assert!(!ids(AccessMethod::Local).contains(&"myai".to_owned()));
        assert!(!ids(AccessMethod::Key).contains(&"hari".to_owned()));
        assert!(!ids(AccessMethod::OAuth).contains(&"myai".to_owned()));
        // The OAuth endpoint is in OAuth, not Custom.
        assert!(ids(AccessMethod::OAuth).contains(&"antigravity".to_owned()));
        assert!(!ids(AccessMethod::Custom).contains(&"antigravity".to_owned()));
    }

    #[test]
    fn classify_sorts_each_endpoint_into_one_bucket() {
        assert_eq!(
            classify("antigravity", "https://x", false),
            AccessMethod::OAuth
        );
        assert_eq!(classify("work", "https://x", true), AccessMethod::OAuth);
        assert_eq!(
            classify("local", "http://localhost:11434/v1", false),
            AccessMethod::Local
        );
        assert_eq!(
            classify("deepseek", "https://api.deepseek.com", false),
            AccessMethod::Key
        );
        assert_eq!(
            classify("myai", "https://my.example/v1", false),
            AccessMethod::Custom
        );
    }

    #[test]
    fn interrupt_and_eof_close_dialog() {
        let mut s = state();
        assert_eq!(
            s.handle_key(Key::Interrupt),
            Some(ProviderDialogAction::Close)
        );
        assert_eq!(s.handle_key(Key::Eof), Some(ProviderDialogAction::Close));
    }

    #[test]
    fn render_multi_pane_preserves_width_and_headers() {
        let s = state();
        let frame = s.render(90, true);
        for (idx, line) in frame.lines().enumerate() {
            assert_eq!(
                visible_len(line),
                90,
                "line {idx} width mismatch: {} for {line:?}",
                visible_len(line)
            );
        }
        let stripped = strip_sgr(&frame);
        assert!(stripped.contains("ACCESS"), "{frame}");
        assert!(stripped.contains("PROVIDER"), "{frame}");
        assert!(stripped.contains("MANAGE"), "{frame}");
    }

    #[test]
    fn narrow_viewport_falls_back_to_single_pane() {
        let s = state();
        let frame = s.render(50, false);
        for (idx, line) in frame.lines().enumerate() {
            assert_eq!(
                visible_len(line),
                50,
                "narrow line {idx} width mismatch: {} for {line:?}",
                visible_len(line)
            );
        }
        assert!(frame.contains("access:"), "{frame}");
    }
}
