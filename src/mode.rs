use std::collections::HashMap;

/// A mode active in a buffer. Modes contribute a highlighting language and, via
/// the global [`Keymap`], key bindings. Keymaps live globally (keyed by mode
/// name) rather than on the mode instance, so every buffer using a mode shares
/// its bindings — the Emacs model.
#[derive(Debug, Clone)]
pub struct Mode {
    pub name: String,
    /// The tree-sitter language this mode highlights with, if any.
    pub language: Option<String>,
    /// Minor modes sit ABOVE the major on the stack; their lighter renders in
    /// the mode line's `modes` field instead of a Scheme slot per mode.
    pub minor: bool,
    pub lighter: Option<String>,
}

impl Mode {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            language: None,
            minor: false,
            lighter: None,
        }
    }

    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        self.language = Some(language.into());
        self
    }
}

/// The ordered set of modes active in a buffer (top of stack = last element).
#[derive(Debug, Default, Clone)]
pub struct ModeStack {
    modes: Vec<Mode>,
}

impl ModeStack {
    /// Just the always-present `fundamental` base mode.
    pub fn fundamental() -> Self {
        Self {
            modes: vec![Mode::new("fundamental")],
        }
    }

    /// `major` stacked on the `fundamental` base — a buffer's usual shape.
    pub fn with_major(major: Mode) -> Self {
        let mut stack = Self::fundamental();
        stack.push(major);
        stack
    }

    pub fn push(&mut self, mode: Mode) {
        self.modes.push(mode);
    }

    /// Drop the top-most entry (test helper for shrinking a stack).
    #[cfg(test)]
    pub fn pop_top(&mut self) {
        self.modes.pop();
    }

    /// Remove the top-most minor-mode entry named `name` (enable/disable).
    pub fn remove_minor(&mut self, name: &str) {
        if let Some(pos) = self.modes.iter().rposition(|m| m.minor && m.name == name) {
            self.modes.remove(pos);
        }
    }

    /// Is the minor mode `name` on the stack?
    pub fn minor(&self, name: &str) -> bool {
        self.modes.iter().any(|m| m.minor && m.name == name)
    }

    /// The top-most non-minor mode above `fundamental` (the major), for
    /// `major-mode` reporting — the top of the stack may be a minor.
    pub fn major(&self) -> &str {
        self.modes
            .iter()
            .rev()
            .find(|m| !m.minor)
            .map(|m| m.name.as_str())
            .unwrap_or("fundamental")
    }

    /// Active minor-mode names, stack order.
    pub fn minor_names(&self) -> Vec<&str> {
        self.modes
            .iter()
            .filter(|m| m.minor)
            .map(|m| m.name.as_str())
            .collect()
    }

    /// Clones of the active minor-mode entries (for re-pushing on a major
    /// switch — minor modes SURVIVE a major-mode change, Emacs parity).
    pub fn minor_modes_cloned(&self) -> Vec<Mode> {
        self.modes.iter().filter(|m| m.minor).cloned().collect()
    }

    /// The major chain (fundamental + ancestors + major) — what
    /// `run_mode_hooks_once` walks; minor modes' hooks fire on their own
    /// enable/disable instead, so they don't re-fire on a major switch.
    pub fn major_chain_names(&self) -> Vec<String> {
        self.modes
            .iter()
            .filter(|m| !m.minor)
            .map(|m| m.name.clone())
            .collect()
    }

    /// Mode-line text per entry: the mode's name, or its lighter for minors.
    /// The `fundamental` base is hidden while a real major sits above it
    /// (`fundamental toml` renders as just `toml`) — but shown when it IS the
    /// major, as in Emacs.
    pub fn names_with_lighters(&self) -> Vec<String> {
        let has_major = self
            .modes
            .iter()
            .any(|m| !m.minor && m.name != "fundamental");
        self.modes
            .iter()
            .filter(|m| !(m.name == "fundamental" && !m.minor && has_major))
            .map(|m| {
                if m.minor {
                    m.lighter.clone().unwrap_or_else(|| m.name.clone())
                } else {
                    m.name.clone()
                }
            })
            .collect()
    }

    /// Mode names bottom-to-top, for the mode line.
    pub fn names(&self) -> Vec<&str> {
        self.modes.iter().map(|m| m.name.as_str()).collect()
    }

    /// Mode names top-to-bottom (newer shadows older). Key lookup goes
    /// through [`resolution_order_with`](Self::resolution_order_with).
    #[cfg(test)]
    pub fn resolution_order(&self) -> impl Iterator<Item = &str> {
        self.modes.iter().rev().map(|m| m.name.as_str())
    }

    /// [`resolution_order`](Self::resolution_order) with the editor-wide
    /// `:global` minor modes spliced in just BELOW this buffer's lowest
    /// buffer-local minor: local minors shadow global ones, global ones
    /// shadow the major chain. With no globals it is exactly
    /// `resolution_order`.
    pub fn resolution_order_with<'a>(
        &'a self,
        globals: &'a [String],
    ) -> impl Iterator<Item = &'a str> {
        let split = self
            .modes
            .iter()
            .position(|m| m.minor)
            .unwrap_or(self.modes.len());
        let (below, above) = self.modes.split_at(split);
        above
            .iter()
            .rev()
            .map(|m| m.name.as_str())
            .chain(globals.iter().rev().map(String::as_str))
            .chain(below.iter().rev().map(|m| m.name.as_str()))
    }

    /// The highlighting language of the topmost mode that declares one.
    pub fn active_language(&self) -> Option<&str> {
        self.modes.iter().rev().find_map(|m| m.language.as_deref())
    }
}

/// A registered mode: how a buffer's [`ModeStack`] is built when the mode
/// becomes major, and where its grammar comes from. `define-derived-mode`
/// fills this via the `register-mode` primitive.
#[derive(Debug, Clone, Default)]
pub struct ModeDef {
    /// The mode this one derives from; `Some("fundamental")` is normalized to
    /// `None` (fundamental is always the stack's base anyway).
    pub parent: Option<String>,
    /// The tree-sitter language id to highlight with, if any.
    pub language: Option<String>,
    /// Minor mode? Pushed above the major on enable instead of rebuilding the
    /// stack; renders its lighter in the mode line.
    pub minor: bool,
    pub lighter: Option<String>,
    /// `define-minor-mode`'s `:init-value` — enable in newly-activated
    /// buffers that haven't applied init-values yet (a `:global` mode is
    /// instead switched on once, editor-wide, at its first registration).
    pub init_value: bool,
    /// `:global` minor mode: one editor-wide on/off state, kept in the
    /// keymap's global layer ([`Keymap::global_minors`]) instead of a stack
    /// entry per buffer.
    pub global: bool,
}

/// Every mode definition registered from Scheme. `set-major-mode` consults it
/// to build the stack from the ancestry, so keymap resolution, hook firing
/// and grammar attachment all inherit down the chain.
#[derive(Debug, Default)]
pub struct ModeRegistry {
    defs: HashMap<String, ModeDef>,
}

impl ModeRegistry {
    pub fn register(&mut self, name: impl Into<String>, def: ModeDef) {
        let name = name.into();
        let def = ModeDef {
            parent: def.parent.filter(|p| p != "fundamental"),
            ..def
        };
        self.defs.insert(name, def);
    }

    pub fn get(&self, name: &str) -> Option<&ModeDef> {
        self.defs.get(name)
    }

    /// Minor modes to enable automatically in a buffer that hasn't applied
    /// init-values yet (`define-minor-mode`'s `:init-value`). Sorted by name:
    /// `defs` is a HashMap, so without the sort the enable order (and thus
    /// key precedence and lighter order) would vary between runs.
    pub fn init_minors(&self) -> Vec<(String, Option<String>)> {
        let mut minors: Vec<_> = self
            .defs
            .iter()
            .filter(|(_, d)| d.minor && d.init_value && !d.global)
            .map(|(n, d)| (n.clone(), d.lighter.clone()))
            .collect();
        minors.sort_by(|a, b| a.0.cmp(&b.0));
        minors
    }

    /// The mode and its ancestors, NEAREST FIRST (`gc`, `gb`, `ga` for
    /// ga → gb → gc). A hand-made cycle stops at the first repeat.
    pub fn ancestry(&self, name: &str) -> Vec<String> {
        let mut chain = vec![name.to_string()];
        let mut seen = std::collections::HashSet::from([name.to_string()]);
        let mut cur = name.to_string();
        while let Some(parent) = self
            .defs
            .get(&cur)
            .and_then(|d| d.parent.clone())
            .filter(|p| p != "fundamental")
        {
            if !seen.insert(parent.clone()) {
                break;
            }
            cur = parent.clone();
            chain.push(parent);
        }
        chain
    }

    /// The stack for `name` becoming major: `fundamental → …ancestors… → name`
    /// (bottom-up). The grammar is the NEAREST chain entry that declares one —
    /// a derived mode highlights with its parent's language until it sets its
    /// own — and it rides the major entry so
    /// [`ModeStack::active_language`] picks it up.
    pub fn build_stack(&self, name: &str) -> ModeStack {
        let chain = self.ancestry(name); // nearest-first, `name` included
        let language = chain
            .iter()
            .find_map(|m| self.defs.get(m).and_then(|d| d.language.clone()));
        let mut stack = ModeStack::fundamental();
        for mode in chain.iter().rev() {
            let mut m = Mode::new(mode.as_str());
            if mode == name
                && let Some(l) = language.clone()
            {
                m = m.with_language(l);
            }
            stack.push(m);
        }
        stack
    }
}

/// The keymap holding modal leader chords (`evil-leader-set`). Normal state in
/// file buffers resolves them from `evil-normal` like any other key; this copy
/// is what the places `evil-normal` never reaches consult — special buffers
/// (magit, dashboards, terminals) and Insert state (via the alt leader).
pub const LEADER_KEYMAP: &str = "evil-leader";

/// Where a key sequence lands in a special buffer's modal overlay plus the
/// leader keymap (see [`Keymap::resolve_leader`]).
#[derive(Debug, PartialEq, Eq)]
pub enum LeaderMatch<'a> {
    /// A complete binding: run this expression.
    Bound(&'a str),
    /// A strict prefix of a longer binding: wait for the next key.
    Prefix,
    /// Neither keymap knows the sequence.
    Miss,
}

/// Global key bindings, keyed by mode name then by chord sequence (e.g.
/// `"ctrl-x ctrl-f"`) → Steel expression. Shared across all buffers.
#[derive(Debug, Default)]
pub struct Keymap {
    maps: HashMap<String, HashMap<String, String>>,
    /// The `:global` minor modes currently on, in enable order (newest
    /// last = highest precedence among globals). Consulted by every
    /// stack-based lookup below, between the buffer's local minors and its
    /// major chain — see [`ModeStack::resolution_order_with`].
    global_minors: Vec<String>,
}

impl Keymap {
    /// Bind `sequence` to `expr` in `mode`'s keymap — what `define-key` drives.
    pub fn define(
        &mut self,
        mode: impl Into<String>,
        sequence: impl Into<String>,
        expr: impl Into<String>,
    ) {
        self.maps
            .entry(mode.into())
            .or_default()
            .insert(sequence.into(), expr.into());
    }

    /// The `:global` minor modes that are on, enable order.
    pub fn global_minors(&self) -> &[String] {
        &self.global_minors
    }

    /// Is the `:global` minor mode `name` on?
    pub fn global_minor(&self, name: &str) -> bool {
        self.global_minors.iter().any(|m| m == name)
    }

    /// Switch the `:global` minor mode `name` on or off editor-wide. Returns
    /// whether the state actually changed (the caller fires hooks only then).
    pub fn set_global_minor(&mut self, name: &str, on: bool) -> bool {
        match (on, self.global_minor(name)) {
            (true, false) => {
                self.global_minors.push(name.to_string());
                true
            }
            (false, true) => {
                self.global_minors.retain(|m| m != name);
                true
            }
            _ => false,
        }
    }

    /// A buffer's modes in lookup order, global layer included.
    fn order<'a>(&'a self, modes: &'a ModeStack) -> impl Iterator<Item = &'a str> {
        modes.resolution_order_with(&self.global_minors)
    }

    /// Resolve a sequence against a buffer's active modes (top-down); the first
    /// mode that binds it wins.
    pub fn resolve(&self, modes: &ModeStack, sequence: &str) -> Option<&str> {
        self.order(modes)
            .find_map(|name| self.maps.get(name).and_then(|m| m.get(sequence)))
            .map(String::as_str)
    }

    /// Resolve a sequence against a single named mode's keymap, ignoring the
    /// mode stack. Used for transient overlays (e.g. the completion popup) that
    /// borrow their bindings from one mode regardless of the active buffer.
    pub fn resolve_mode(&self, mode: &str, sequence: &str) -> Option<&str> {
        self.maps
            .get(mode)
            .and_then(|m| m.get(sequence))
            .map(String::as_str)
    }

    /// Whether `sequence` is a strict prefix of a longer binding in `mode` — for
    /// multi-key sequences (e.g. `g g`, leader `space …`) in a single mode.
    pub fn is_prefix_mode(&self, mode: &str, sequence: &str) -> bool {
        let with_sep = format!("{sequence} ");
        self.maps
            .get(mode)
            .is_some_and(|m| m.keys().any(|k| k.starts_with(&with_sep)))
    }

    /// Whether `sequence` is a strict prefix of some longer binding in any of
    /// the active modes (a pending prefix awaiting more keys).
    pub fn is_prefix(&self, modes: &ModeStack, sequence: &str) -> bool {
        self.order(modes)
            .any(|name| self.is_prefix_mode(name, sequence))
    }

    /// Resolve an evil/helix `state` keymap, letting every active mode's
    /// scoped keymap `"<state>:<mode>"` shadow the global one, top of the
    /// stack first — so an active minor mode beats the major chain, a child
    /// beats its parent, and derived modes get per-state inheritance.
    pub fn resolve_evil(&self, state: &str, modes: &ModeStack, sequence: &str) -> Option<&str> {
        self.order(modes)
            .find_map(|name| self.resolve_mode(&format!("{state}:{name}"), sequence))
            .or_else(|| self.resolve_mode(state, sequence))
    }

    /// Like [`is_prefix_mode`](Self::is_prefix_mode) but across every active
    /// mode's scoped keymap and the global `state` keymap (so a mode-specific
    /// multi-key sequence still pends).
    pub fn is_prefix_evil(&self, state: &str, modes: &ModeStack, sequence: &str) -> bool {
        self.order(modes)
            .any(|name| self.is_prefix_mode(&format!("{state}:{name}"), sequence))
            || self.is_prefix_mode(state, sequence)
    }

    /// Resolve `sequence` against the leader keymap, letting `overlay` (a
    /// special buffer's `<style>-normal:<major>` map, when there is one) shadow
    /// it — so `evil-leader-set-for-modes` beats the global leader in that mode.
    /// Same precedence as the special-buffer layer: an exact overlay binding,
    /// then a LONGER overlay binding (pend), then the leader map's.
    pub fn resolve_leader(&self, overlay: Option<&str>, sequence: &str) -> LeaderMatch<'_> {
        if let Some(o) = overlay {
            if let Some(expr) = self.resolve_mode(o, sequence) {
                return LeaderMatch::Bound(expr);
            }
            if self.is_prefix_mode(o, sequence) {
                return LeaderMatch::Prefix;
            }
        }
        if let Some(expr) = self.resolve_mode(LEADER_KEYMAP, sequence) {
            LeaderMatch::Bound(expr)
        } else if self.is_prefix_mode(LEADER_KEYMAP, sequence) {
            LeaderMatch::Prefix
        } else {
            LeaderMatch::Miss
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Keymap, LEADER_KEYMAP, LeaderMatch};

    #[test]
    fn registry_builds_stack_from_ancestry_with_language_inheritance() {
        use super::{ModeDef, ModeRegistry};
        let mut reg = ModeRegistry::default();
        reg.register("ga", ModeDef::default());
        reg.register(
            "gb",
            ModeDef {
                parent: Some("ga".into()),
                ..Default::default()
            },
        );
        reg.register(
            "gc",
            ModeDef {
                parent: Some("gb".into()),
                ..Default::default()
            },
        );

        let stack = reg.build_stack("gc");
        assert_eq!(stack.names(), vec!["fundamental", "ga", "gb", "gc"]);
        assert_eq!(
            stack.resolution_order().collect::<Vec<_>>(),
            vec!["gc", "gb", "ga", "fundamental"]
        );

        // The NEAREST ancestor's language wins, riding the major entry.
        reg.register(
            "gb",
            ModeDef {
                parent: Some("ga".into()),
                language: Some("gblang".into()),
                ..Default::default()
            },
        );
        assert_eq!(reg.build_stack("gc").active_language(), Some("gblang"));

        // "fundamental" as parent is normalized away (same stack as none).
        reg.register(
            "gd",
            ModeDef {
                parent: Some("fundamental".into()),
                ..Default::default()
            },
        );
        assert_eq!(reg.build_stack("gd").names(), vec!["fundamental", "gd"]);
    }

    #[test]
    fn registry_cycle_guard_terminates() {
        use super::{ModeDef, ModeRegistry};
        let mut reg = ModeRegistry::default();
        reg.register(
            "x",
            ModeDef {
                parent: Some("y".into()),
                ..Default::default()
            },
        );
        reg.register(
            "y",
            ModeDef {
                parent: Some("x".into()),
                ..Default::default()
            },
        );
        assert_eq!(reg.build_stack("x").names(), vec!["fundamental", "y", "x"]);
    }

    #[test]
    fn evil_scoped_maps_resolve_over_the_whole_stack() {
        use super::{Mode, ModeStack};
        let mut km = Keymap::default();
        km.define("evil-normal", "d", "global-d");
        km.define("evil-normal:minor-b", "d", "minor-b-d");
        km.define("evil-normal:minor-a", "d", "minor-a-d");
        km.define("evil-normal:child", "d", "child-d");
        km.define("evil-normal:parent", "d", "parent-d");

        let mut modes = ModeStack::with_major(Mode::new("parent"));
        modes.push(Mode::new("child"));
        modes.push(Mode::new("minor-a"));
        modes.push(Mode::new("minor-b"));

        // Top of the stack wins: minor-b shadows minor-a, the major chain,
        // then the global state map.
        assert_eq!(
            km.resolve_evil("evil-normal", &modes, "d"),
            Some("minor-b-d")
        );
        let mut less = modes.clone();
        less.pop_top(); // drop minor-b
        assert_eq!(
            km.resolve_evil("evil-normal", &less, "d"),
            Some("minor-a-d")
        );
        let mut majors_only = modes.clone();
        majors_only.pop_top(); // drop minor-b
        majors_only.pop_top(); // drop minor-a → the child is the top major
        assert_eq!(
            km.resolve_evil("evil-normal", &majors_only, "d"),
            Some("child-d")
        );
        // Unbound key falls through to nothing; prefixes pend across all
        // scoped maps plus the global one.
        assert_eq!(km.resolve_evil("evil-normal", &modes, "x"), None);
        assert!(!km.is_prefix_evil("evil-normal", &modes, "d"));
    }

    #[test]
    fn global_minor_layer_sits_between_local_minors_and_majors() {
        use super::{Mode, ModeStack};
        let mut km = Keymap::default();
        km.define("rust", "ctrl-c g", "rust-g");
        km.define("glob-mode", "ctrl-c g", "glob-g");
        km.define("glob-mode", "ctrl-c h", "glob-h");
        km.define("local-mode", "ctrl-c h", "local-h");
        km.define("evil-normal:glob-mode", "g x", "glob-gx");
        km.define("evil-normal:local-mode", "g x", "local-gx");

        let mut modes = ModeStack::with_major(Mode::new("rust"));
        let local = Mode {
            name: "local-mode".into(),
            language: None,
            minor: true,
            lighter: None,
        };
        modes.push(local);
        // Off: the global map is not consulted.
        assert_eq!(km.resolve(&modes, "ctrl-c g"), Some("rust-g"));
        assert!(!km.is_prefix(&modes, "g"));

        assert!(km.set_global_minor("glob-mode", true));
        assert!(!km.set_global_minor("glob-mode", true), "idempotent");
        // Global beats the major; the buffer-local minor beats the global —
        // even though the local one was enabled FIRST.
        assert_eq!(km.resolve(&modes, "ctrl-c g"), Some("glob-g"));
        assert_eq!(km.resolve(&modes, "ctrl-c h"), Some("local-h"));
        assert_eq!(
            km.resolve_evil("evil-normal", &modes, "g x"),
            Some("local-gx")
        );
        // A stack with no minors (another buffer) still sees the global.
        let other = ModeStack::with_major(Mode::new("go"));
        assert_eq!(km.resolve(&other, "ctrl-c h"), Some("glob-h"));
        assert_eq!(
            km.resolve_evil("evil-normal", &other, "g x"),
            Some("glob-gx")
        );
        assert!(km.is_prefix_evil("evil-normal", &other, "g"));
        assert_eq!(
            modes
                .resolution_order_with(km.global_minors())
                .collect::<Vec<_>>(),
            vec!["local-mode", "glob-mode", "rust", "fundamental"]
        );

        assert!(km.set_global_minor("glob-mode", false));
        assert_eq!(km.resolve(&other, "ctrl-c h"), None);
        assert_eq!(km.resolve(&modes, "ctrl-c g"), Some("rust-g"));
    }

    #[test]
    fn leader_keymap_resolves_under_mode_overlay() {
        let mut km = Keymap::default();
        km.define(LEADER_KEYMAP, "space n o", "delete-other-windows");
        km.define(LEADER_KEYMAP, "alt-space n o", "delete-other-windows");
        km.define("evil-normal:k8s-mode", "j", "next-line");
        km.define("evil-normal:k8s-mode", "space n d", "k8s-describe");

        let ov = Some("evil-normal:k8s-mode");
        assert_eq!(km.resolve_leader(ov, "space"), LeaderMatch::Prefix);
        assert_eq!(km.resolve_leader(ov, "space n"), LeaderMatch::Prefix);
        assert_eq!(
            km.resolve_leader(ov, "space n o"),
            LeaderMatch::Bound("delete-other-windows")
        );
        // A per-mode binding shadows the global leader in that mode only.
        assert_eq!(
            km.resolve_leader(ov, "space n d"),
            LeaderMatch::Bound("k8s-describe")
        );
        assert_eq!(km.resolve_leader(None, "space n d"), LeaderMatch::Miss);
        // The overlay's own plain keys still resolve (it runs first).
        assert_eq!(km.resolve_leader(ov, "j"), LeaderMatch::Bound("next-line"));
        // The alt leader works without any overlay (Insert state, terminals).
        assert_eq!(km.resolve_leader(None, "alt-space"), LeaderMatch::Prefix);
        assert_eq!(
            km.resolve_leader(None, "alt-space n o"),
            LeaderMatch::Bound("delete-other-windows")
        );
        assert_eq!(km.resolve_leader(ov, "space x"), LeaderMatch::Miss);
    }

    #[test]
    fn mode_stack_minor_operations() {
        use super::{Mode, ModeStack};
        let mut modes = ModeStack::with_major(Mode::new("rust"));
        modes.push(Mode {
            name: "m1".into(),
            language: None,
            minor: true,
            lighter: Some(" M1".into()),
        });
        modes.push(Mode {
            name: "m2".into(),
            language: None,
            minor: true,
            lighter: None,
        });
        assert!(modes.minor("m1") && modes.minor("m2") && !modes.minor("rust"));
        assert_eq!(modes.major(), "rust");
        assert_eq!(modes.minor_names(), vec!["m1", "m2"]);
        assert_eq!(modes.major_chain_names(), vec!["fundamental", "rust"]);
        modes.remove_minor("m1");
        assert!(!modes.minor("m1") && modes.minor("m2"));
        assert_eq!(modes.names(), vec!["fundamental", "rust", "m2"]);
    }

    #[test]
    fn mode_line_hides_the_fundamental_base_under_a_real_major() {
        use super::{Mode, ModeStack};
        // fundamental → toml: the base is internal — render only "toml".
        let mut modes = ModeStack::with_major(Mode::new("toml"));
        assert_eq!(modes.names_with_lighters(), vec!["toml"]);
        // Minors render regardless.
        modes.push(Mode {
            name: "abbrev-mode".into(),
            language: None,
            minor: true,
            lighter: Some(" abbrev".into()),
        });
        assert_eq!(modes.names_with_lighters(), vec!["toml", " abbrev"]);
        // Fundamental ALONE is the major — it shows, as in Emacs.
        assert_eq!(
            ModeStack::fundamental().names_with_lighters(),
            vec!["fundamental"]
        );
    }

    #[test]
    fn evil_mode_scoped_keymaps_shadow_global() {
        use super::{Mode, ModeStack};
        let mut km = Keymap::default();
        km.define("evil-normal", "space g g", "global-def");
        km.define("evil-normal:rust", "space g g", "lsp-find-definition");
        km.define("evil-normal:rust", "space t", "lsp-hover");

        let rust = ModeStack::with_major(Mode::new("rust"));
        let go = ModeStack::with_major(Mode::new("go"));
        let bare = ModeStack::fundamental();

        // In a rust buffer the scoped binding wins; the rust-only key resolves.
        assert_eq!(
            km.resolve_evil("evil-normal", &rust, "space g g"),
            Some("lsp-find-definition")
        );
        assert_eq!(
            km.resolve_evil("evil-normal", &rust, "space t"),
            Some("lsp-hover")
        );
        // In another major mode, only the global binding applies.
        assert_eq!(
            km.resolve_evil("evil-normal", &go, "space g g"),
            Some("global-def")
        );
        assert_eq!(km.resolve_evil("evil-normal", &go, "space t"), None);
        // A fundamental-only stack → global only.
        assert_eq!(
            km.resolve_evil("evil-normal", &bare, "space g g"),
            Some("global-def")
        );

        // Prefixes pend across both scoped and global keymaps.
        assert!(km.is_prefix_evil("evil-normal", &rust, "space"));
        assert!(km.is_prefix_evil("evil-normal", &rust, "space g"));
        assert!(km.is_prefix_evil("evil-normal", &go, "space")); // global "space g g"
        assert!(!km.is_prefix_evil("evil-normal", &go, "space t"));
    }
}
