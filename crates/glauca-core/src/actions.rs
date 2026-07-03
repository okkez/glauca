//! User-defined custom actions.
//!
//! A custom action runs an arbitrary command against the selected PR/Issue,
//! substituting item fields (repo, number, kind, url, …) into the command's
//! argv. Definitions live in a shared TOML file under the user config dir so
//! both front-ends (TUI/GUI) read the same list. This is a generic hook: the
//! command is run as-is (no shell), so `gh` and user scripts can be invoked
//! directly.
//!
//! Reads are best-effort — a missing file yields an empty list; a corrupt one
//! logs a warning and yields empty.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::types::ItemEntry;

/// A single user-defined action.
#[derive(Debug, Clone, Deserialize)]
pub struct CustomAction {
    /// Stable identifier, also the fallback display label.
    pub name: String,
    /// Human label shown in the picker. Falls back to `name` when absent.
    #[serde(default)]
    pub label: Option<String>,
    /// Command as an argv list; the first element is the program. Each element
    /// is a template rendered with `{{ key }}` placeholders before execution.
    pub command: Vec<String>,
    /// Item kinds this action applies to (`pull_request` / `issue`). Empty means
    /// every kind.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Extra environment variables to set for the command. Values are templated
    /// like `command` elements.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl CustomAction {
    /// Label for menus: `label` if set, otherwise `name`.
    pub fn display_label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }

    /// Whether this action should be offered for `kind`. An empty `kinds` list
    /// matches every kind.
    pub fn matches_kind(&self, kind: &str) -> bool {
        self.kinds.is_empty() || self.kinds.iter().any(|k| k == kind)
    }
}

/// The parsed `actions.toml` document.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CustomActions {
    #[serde(default, rename = "actions")]
    pub actions: Vec<CustomAction>,
}

impl CustomActions {
    /// `~/.config/glauca/actions.toml` (or the platform equivalent); falls back
    /// to the local data dir if no config dir is available.
    fn path() -> Option<PathBuf> {
        let base = dirs::config_dir().or_else(dirs::data_local_dir)?;
        Some(base.join("glauca").join("actions.toml"))
    }

    /// Load defined actions, or an empty list if the file is
    /// missing/unreadable/corrupt (a parse error is logged).
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        let Ok(contents) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match toml::from_str(&contents) {
            Ok(actions) => actions,
            Err(e) => {
                tracing::warn!("failed to parse {}: {e}", path.display());
                Self::default()
            }
        }
    }

    /// Actions applicable to `kind`, in definition order.
    pub fn for_kind(&self, kind: &str) -> Vec<&CustomAction> {
        self.actions
            .iter()
            .filter(|a| a.matches_kind(kind))
            .collect()
    }

    /// Whether any action applies to `kind`. Allocation-free — prefer this over
    /// `!for_kind(kind).is_empty()` on hot paths (per-frame / per-keystroke).
    pub fn has_for_kind(&self, kind: &str) -> bool {
        self.actions.iter().any(|a| a.matches_kind(kind))
    }
}

/// Build the template substitution context for an item. All values are scalar
/// strings; list-valued fields (labels, reviewers) are intentionally omitted as
/// they do not map cleanly onto argv.
pub fn build_action_context(item: &ItemEntry) -> BTreeMap<&'static str, String> {
    let mut ctx = BTreeMap::new();
    ctx.insert("owner", item.repo_owner.clone());
    ctx.insert("repo", item.repo_name.clone());
    ctx.insert("repo_full", item.repo_display());
    ctx.insert("number", item.number.to_string());
    ctx.insert("kind", item.kind.clone());
    ctx.insert("url", item.url.clone());
    ctx.insert("title", item.title.clone());
    ctx.insert(
        "author",
        item.author
            .as_ref()
            .map(|u| u.login.clone())
            .unwrap_or_default(),
    );
    ctx.insert("state", item.state.clone());
    ctx.insert("is_draft", item.is_draft.to_string());
    ctx.insert("base_ref", item.base_ref.clone().unwrap_or_default());
    ctx.insert("head_ref", item.head_ref.clone().unwrap_or_default());
    ctx.insert(
        "created_at",
        item.created_at_item.clone().unwrap_or_default(),
    );
    ctx.insert("updated_at", item.updated_at.clone());
    ctx
}

/// Render a `{{ key }}` template against `ctx`. Whitespace inside the braces is
/// trimmed. An unknown key or an unclosed `{{` is an error (surfacing config
/// typos rather than silently producing a wrong command).
pub fn render_template(tmpl: &str, ctx: &BTreeMap<&'static str, String>) -> anyhow::Result<String> {
    // Note: `{{{{` / `}}}}` in the error format strings below are `format!`
    // escapes — each doubled brace renders as a single literal `{` / `}`, so the
    // messages show the real `{{ key }}` syntax back to the user.
    let mut out = String::with_capacity(tmpl.len());
    let mut rest = tmpl;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find("}}") else {
            anyhow::bail!("unclosed '{{{{' in template: {tmpl}");
        };
        let key = after_open[..end].trim();
        let value = ctx
            .get(key)
            .ok_or_else(|| anyhow::anyhow!("unknown template variable '{{{{ {key} }}}}'"))?;
        out.push_str(value);
        rest = &after_open[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ctx() -> BTreeMap<&'static str, String> {
        let mut ctx = BTreeMap::new();
        ctx.insert("repo_full", "octocat/hello".to_string());
        ctx.insert("number", "42".to_string());
        ctx
    }

    #[test]
    fn render_substitutes_and_trims() {
        let out = render_template("{{ repo_full }} #{{number}}", &sample_ctx()).unwrap();
        assert_eq!(out, "octocat/hello #42");
    }

    #[test]
    fn render_passes_through_plain_text() {
        let out = render_template("gh pr view", &sample_ctx()).unwrap();
        assert_eq!(out, "gh pr view");
    }

    #[test]
    fn render_unknown_key_errors() {
        assert!(render_template("{{ nope }}", &sample_ctx()).is_err());
    }

    #[test]
    fn render_unclosed_placeholder_errors() {
        assert!(render_template("{{ repo_full", &sample_ctx()).is_err());
    }

    /// A minimal action with a fixed command and no label, for exercising
    /// `display_label` / `matches_kind`.
    fn action(name: &str, kinds: &[&str]) -> CustomAction {
        CustomAction {
            name: name.into(),
            label: None,
            command: vec!["x".into()],
            kinds: kinds.iter().map(|s| s.to_string()).collect(),
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn display_label_falls_back_to_name() {
        assert_eq!(action("review", &[]).display_label(), "review");
    }

    #[test]
    fn matches_kind_empty_matches_all() {
        let a = action("a", &[]);
        assert!(a.matches_kind("pull_request"));
        assert!(a.matches_kind("issue"));
    }

    #[test]
    fn matches_kind_filters() {
        let a = action("a", &["pull_request"]);
        assert!(a.matches_kind("pull_request"));
        assert!(!a.matches_kind("issue"));
    }

    #[test]
    fn parses_toml_document() {
        let toml = r#"
[[actions]]
name = "checkout"
label = "gh pr checkout"
command = ["gh", "pr", "checkout", "{{ number }}", "-R", "{{ repo_full }}"]
kinds = ["pull_request"]
"#;
        let doc: CustomActions = toml::from_str(toml).unwrap();
        assert_eq!(doc.actions.len(), 1);
        let a = &doc.actions[0];
        assert_eq!(a.display_label(), "gh pr checkout");
        assert_eq!(a.command.len(), 6);
        assert!(a.matches_kind("pull_request"));
    }

    #[test]
    fn empty_document_yields_no_actions() {
        let doc: CustomActions = toml::from_str("").unwrap();
        assert!(doc.actions.is_empty());
    }

    #[test]
    fn for_kind_and_has_for_kind_filter_by_kind() {
        let toml = r#"
[[actions]]
name = "pr-only"
command = ["x"]
kinds = ["pull_request"]

[[actions]]
name = "any"
command = ["x"]
"#;
        let doc: CustomActions = toml::from_str(toml).unwrap();

        let pr: Vec<&str> = doc
            .for_kind("pull_request")
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(pr, vec!["pr-only", "any"]);
        let issue: Vec<&str> = doc
            .for_kind("issue")
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(issue, vec!["any"]);

        assert!(doc.has_for_kind("pull_request"));
        assert!(doc.has_for_kind("issue"));
        assert!(!CustomActions::default().has_for_kind("pull_request"));
    }

    #[test]
    fn build_context_exposes_repo_full_and_number() {
        let item = ItemEntry {
            number: 7,
            repo_owner: "octocat".into(),
            repo_name: "hello".into(),
            kind: "pull_request".into(),
            ..Default::default()
        };
        let ctx = build_action_context(&item);
        assert_eq!(ctx.get("repo_full").unwrap(), "octocat/hello");
        assert_eq!(ctx.get("number").unwrap(), "7");
        assert_eq!(ctx.get("kind").unwrap(), "pull_request");
    }
}
