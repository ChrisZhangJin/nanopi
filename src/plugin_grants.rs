//! What each loaded plugin was actually granted, rendered for display.
//!
//! Unconditionally compiled, for the same reason as `plugin_context`
//! and `subscriber`: `src/mode/tui.rs` is on the reading side of this
//! seam, so `/tools` stays free of `#[cfg(feature = "wasm")]`. Without
//! the `wasm` feature nothing ever pushes a [`PluginGrants`], the vec
//! is always empty, and [`grants_section`]'s caller renders nothing.
//!
//! The grant tokens are **pre-baked at load time**, not derived in the
//! TUI. Two reasons, and both matter: `ExtensionConfig` lives behind
//! the feature flag, so a TUI that formatted grants itself could not
//! compile with the feature off; and the config the TUI could reach is
//! the config on *disk*, which is not necessarily the config the
//! plugin was loaded under. A row must describe what the running
//! plugin holds.

/// One row of `/tools`'s grant section: a successfully-loaded plugin
/// and the capabilities it holds, already rendered.
///
/// Only *successfully-loaded* plugins get one. A row for a plugin that
/// failed to load would claim a grant nothing holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginGrants {
    /// The plugin's name as the rest of the UI knows it.
    pub plugin_name: String,
    /// Where it was loaded from, so two plugins with the same name are
    /// still tellable apart.
    pub path: String,
    /// Short tokens, e.g. `allow_fs`, `allow_network(github.com)`,
    /// `allow_tools(find, read)`. Empty means genuinely nothing —
    /// which still gets a row, reading `no grants`.
    pub grants: Vec<String>,
}

impl PluginGrants {
    /// Render the grants as one line's worth of text.
    ///
    /// A plugin with nothing granted reads `no grants` rather than
    /// being omitted: "this plugin can do nothing" is the answer a
    /// user came to `/tools` to get, and dropping the row would make
    /// "not installed" and "installed, powerless" look identical.
    pub fn summary(&self) -> String {
        if self.grants.is_empty() {
            "no grants".to_string()
        } else {
            self.grants.join(", ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plugin_with_nothing_granted_says_so_rather_than_going_blank() {
        let g = PluginGrants {
            plugin_name: "p".into(),
            path: "/tmp/p.wasm".into(),
            grants: Vec::new(),
        };
        assert_eq!(g.summary(), "no grants");
    }

    #[test]
    fn grants_render_in_the_order_they_were_baked() {
        let g = PluginGrants {
            plugin_name: "p".into(),
            path: "/tmp/p.wasm".into(),
            grants: vec!["allow_fs".into(), "allow_tools(find, read)".into()],
        };
        assert_eq!(g.summary(), "allow_fs, allow_tools(find, read)");
    }
}
