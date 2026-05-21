//! Static metadata for every agent alleycat exposes — presentation
//! hints (label, beta, sort order, aliases) and behavioral capability
//! flags. Adding a new agent only requires extending the manifest list
//! here plus wiring its bridge in `agents.rs`.
//!
//! Icons are NOT shipped over the wire: clients keep their own asset
//! catalogs keyed by the agent `name` and render a generic monogram
//! fallback when no local asset is bundled.

use crate::protocol::{
    AgentCapabilities, AgentPresentation, AgentTerminalCapability, AgentTerminalTransport,
    AgentWire,
};

/// Compile-time form. Owns no heap allocations so it can sit in a
/// `const` slice. Runtime conversion to the wire types happens in
/// [`AgentManifest::presentation`] / [`AgentManifest::capabilities`].
pub struct AgentManifest {
    pub name: &'static str,
    pub display_name: &'static str,
    pub wire: AgentWire,
    pub title: Option<&'static str>,
    pub is_beta: bool,
    pub sort_order: i32,
    pub description: Option<&'static str>,
    pub aliases: &'static [&'static str],
    pub locks_reasoning_effort_after_activity: bool,
    pub visible_modes: Option<&'static [&'static str]>,
    pub supports_ssh_bridge: bool,
    pub uses_direct_codex_port: bool,
    pub supports_thread_permission_overrides: bool,
    pub reports_effective_thread_permissions: bool,
    pub terminal: Option<AgentTerminalManifest>,
}

#[derive(Clone, Copy)]
pub struct AgentTerminalManifest {
    pub transport: AgentTerminalTransport,
    pub protocol_version: u32,
    pub features: &'static [&'static str],
    pub launch_agent: Option<&'static str>,
    pub label: Option<&'static str>,
}

impl AgentManifest {
    pub fn presentation(&self) -> AgentPresentation {
        AgentPresentation {
            title: self.title.map(str::to_owned),
            is_beta: self.is_beta,
            sort_order: self.sort_order,
            description: self.description.map(str::to_owned),
            aliases: self.aliases.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    pub fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            locks_reasoning_effort_after_activity: self.locks_reasoning_effort_after_activity,
            visible_modes: self
                .visible_modes
                .map(|modes| modes.iter().map(|s| (*s).to_owned()).collect()),
            supports_ssh_bridge: self.supports_ssh_bridge,
            uses_direct_codex_port: self.uses_direct_codex_port,
            supports_thread_permission_overrides: self.supports_thread_permission_overrides,
            reports_effective_thread_permissions: self.reports_effective_thread_permissions,
            terminal: self.terminal.map(|terminal| AgentTerminalCapability {
                transport: terminal.transport,
                protocol_version: terminal.protocol_version,
                features: terminal
                    .features
                    .iter()
                    .map(|feature| (*feature).to_owned())
                    .collect(),
                launch_agent: terminal.launch_agent.map(str::to_owned),
                label: terminal.label.map(str::to_owned),
            }),
        }
    }
}

pub const MANIFESTS: &[AgentManifest] = &[
    AgentManifest {
        name: "codex",
        display_name: "Codex",
        wire: AgentWire::Websocket,
        title: None,
        is_beta: false,
        sort_order: 0,
        description: Some("OpenAI Codex app-server."),
        aliases: &[],
        locks_reasoning_effort_after_activity: false,
        visible_modes: None,
        supports_ssh_bridge: true,
        uses_direct_codex_port: true,
        supports_thread_permission_overrides: true,
        reports_effective_thread_permissions: true,
        terminal: None,
    },
    AgentManifest {
        name: "pi",
        display_name: "Pi",
        wire: AgentWire::Jsonl,
        title: None,
        is_beta: true,
        sort_order: 1,
        description: Some("Pi coding agent."),
        aliases: &["pi.dev", "pidev"],
        locks_reasoning_effort_after_activity: false,
        visible_modes: None,
        supports_ssh_bridge: true,
        uses_direct_codex_port: false,
        supports_thread_permission_overrides: false,
        reports_effective_thread_permissions: false,
        terminal: None,
    },
    AgentManifest {
        name: "amp",
        display_name: "Amp",
        wire: AgentWire::Jsonl,
        title: Some("Amp"),
        is_beta: true,
        sort_order: 2,
        description: Some("Sourcegraph Amp."),
        aliases: &["ampcode", "amp-code", "amp_code", "amp code"],
        locks_reasoning_effort_after_activity: true,
        visible_modes: Some(&["smart", "rush", "deep"]),
        supports_ssh_bridge: false,
        uses_direct_codex_port: false,
        supports_thread_permission_overrides: false,
        reports_effective_thread_permissions: false,
        terminal: None,
    },
    AgentManifest {
        name: "opencode",
        display_name: "opencode",
        wire: AgentWire::Jsonl,
        title: Some("Opencode"),
        is_beta: true,
        sort_order: 3,
        description: Some("Open-source local coding agent."),
        aliases: &["open-code", "open_code", "open code"],
        locks_reasoning_effort_after_activity: false,
        visible_modes: None,
        supports_ssh_bridge: true,
        uses_direct_codex_port: false,
        supports_thread_permission_overrides: false,
        reports_effective_thread_permissions: false,
        terminal: None,
    },
    AgentManifest {
        name: "claude",
        display_name: "Claude",
        wire: AgentWire::Jsonl,
        title: None,
        is_beta: true,
        sort_order: 4,
        description: Some("Anthropic Claude Code."),
        aliases: &["claude-code", "claude_code"],
        locks_reasoning_effort_after_activity: false,
        visible_modes: None,
        supports_ssh_bridge: true,
        uses_direct_codex_port: false,
        supports_thread_permission_overrides: false,
        reports_effective_thread_permissions: false,
        terminal: None,
    },
    AgentManifest {
        name: "droid",
        display_name: "Droid",
        wire: AgentWire::Jsonl,
        title: Some("Factory Droid"),
        is_beta: true,
        sort_order: 5,
        description: Some("Factory Droid coding agent."),
        aliases: &["factory", "factory-droid", "factory_droid", "factory droid"],
        locks_reasoning_effort_after_activity: false,
        visible_modes: None,
        supports_ssh_bridge: false,
        uses_direct_codex_port: false,
        supports_thread_permission_overrides: false,
        reports_effective_thread_permissions: false,
        terminal: None,
    },
    AgentManifest {
        name: "droid-pty",
        display_name: "Droid TUI",
        wire: AgentWire::Terminal,
        title: Some("Factory Droid TUI"),
        is_beta: true,
        sort_order: 6,
        description: Some("Factory Droid interactive PTY/TUI terminal."),
        aliases: &["droid-terminal", "droid_tui", "factory-droid-pty", "factory droid tui"],
        locks_reasoning_effort_after_activity: false,
        visible_modes: Some(&["terminal"]),
        supports_ssh_bridge: false,
        uses_direct_codex_port: false,
        supports_thread_permission_overrides: false,
        reports_effective_thread_permissions: false,
        terminal: Some(AgentTerminalManifest {
            transport: AgentTerminalTransport::DroidPty,
            protocol_version: 1,
            features: &["hello", "start", "output", "input", "resize", "close", "exit", "error"],
            launch_agent: Some("droid-pty"),
            label: Some("Droid TUI"),
        }),
    },
    AgentManifest {
        name: "hermes",
        display_name: "Hermes",
        wire: AgentWire::Jsonl,
        title: Some("Hermes"),
        is_beta: true,
        sort_order: 7,
        description: Some("Nous Research Hermes agent."),
        aliases: &[],
        locks_reasoning_effort_after_activity: false,
        visible_modes: None,
        supports_ssh_bridge: false,
        uses_direct_codex_port: false,
        supports_thread_permission_overrides: false,
        reports_effective_thread_permissions: false,
        terminal: None,
    },
    AgentManifest {
        name: "devin",
        display_name: "Devin",
        wire: AgentWire::Jsonl,
        title: Some("Devin"),
        is_beta: true,
        sort_order: 8,
        description: Some("Devin coding agent."),
        aliases: &[],
        locks_reasoning_effort_after_activity: false,
        visible_modes: None,
        supports_ssh_bridge: true,
        uses_direct_codex_port: false,
        supports_thread_permission_overrides: false,
        reports_effective_thread_permissions: false,
        terminal: None,
    },
    AgentManifest {
        name: "grok",
        display_name: "Grok",
        wire: AgentWire::Jsonl,
        title: Some("Grok"),
        is_beta: true,
        sort_order: 9,
        description: Some("xAI Grok coding agent."),
        aliases: &["grok-code", "xai-grok", "xai grok"],
        locks_reasoning_effort_after_activity: false,
        visible_modes: None,
        supports_ssh_bridge: true,
        uses_direct_codex_port: false,
        supports_thread_permission_overrides: false,
        reports_effective_thread_permissions: false,
        terminal: None,
    },
    AgentManifest {
        name: "shell",
        display_name: "Shell",
        wire: AgentWire::Jsonl,
        title: Some("Shell"),
        is_beta: true,
        sort_order: 10,
        description: Some("PTY-backed host shell."),
        aliases: &["terminal"],
        locks_reasoning_effort_after_activity: false,
        visible_modes: None,
        supports_ssh_bridge: false,
        uses_direct_codex_port: false,
        supports_thread_permission_overrides: false,
        reports_effective_thread_permissions: false,
        terminal: None,
    },
];

pub fn manifest_for(name: &str) -> Option<&'static AgentManifest> {
    MANIFESTS.iter().find(|m| m.name == name)
}
