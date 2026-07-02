//! ghq のルート規約に沿ってリポジトリのローカルチェックアウト先を解決する。
//!
//! ghq バイナリには依存せず、ghq / gh-q が読むのと同じ設定（`GHQ_ROOT` 環境変数と
//! `git config --get-all ghq.root`）だけを読んで規約を再現する。レイアウトは
//! `{root}/github.com/{owner}/{name}`。

use std::path::PathBuf;
use std::process::Command;

/// `github.com/{owner}/{name}` のローカルチェックアウト先を ghq のルート規約で解決する。
///
/// ルート候補（`ghq_roots`）を優先度順にたどり、`{root}/github.com/{owner}/{name}` が
/// 実在するディレクトリなら最初のものを返す。見つからなければ `None`。
pub fn resolve_local_checkout(owner: &str, name: &str) -> Option<PathBuf> {
    resolve_in_roots(&ghq_roots(), owner, name)
}

/// 各ルートで `{root}/github.com/{owner}/{name}` を探し、実在するディレクトリを返す純粋関数。
/// 環境変数や git を触らないのでユニットテストしやすい。
fn resolve_in_roots(roots: &[PathBuf], owner: &str, name: &str) -> Option<PathBuf> {
    let rel_path = format!("github.com/{owner}/{name}");
    roots
        .iter()
        .map(|root| root.join(&rel_path))
        .find(|path| path.is_dir())
}

/// ルート候補を優先度順で返す:
/// 1. `GHQ_ROOT` 環境変数（設定されていればこれを唯一のルート源にする。ghq と同じ挙動）
/// 2. `git config --get-all ghq.root`（複数値可）
/// 3. 既定 `~/ghq`
///
/// 各値は先頭の `~/` をホームディレクトリに展開し、重複は除去する。
fn ghq_roots() -> Vec<PathBuf> {
    let raw_roots = ghq_root_env().unwrap_or_else(git_config_ghq_roots);

    let mut roots: Vec<PathBuf> = raw_roots.iter().map(|root| expand_tilde(root)).collect();
    if roots.is_empty()
        && let Some(home) = dirs::home_dir()
    {
        roots.push(home.join("ghq"));
    }
    dedup(roots)
}

/// `GHQ_ROOT` 環境変数を PATH リスト区切りで分割して返す。未設定なら `None`。
fn ghq_root_env() -> Option<Vec<String>> {
    let value = std::env::var_os("GHQ_ROOT")?;
    let roots: Vec<String> = std::env::split_paths(&value)
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    (!roots.is_empty()).then_some(roots)
}

/// `git config --get-all ghq.root` の各行をルートとして返す。git 不在・非 git 環境・
/// 未設定など何らかの失敗時は空 Vec（→ 既定へフォールバック）。
fn git_config_ghq_roots() -> Vec<String> {
    let Ok(output) = Command::new("git")
        .args(["config", "--get-all", "ghq.root"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// 先頭の `~/`（または単独の `~`）をホームディレクトリへ展開する。
fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// 順序を保ったまま重複を除去する。
fn dedup(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    roots
        .into_iter()
        .filter(|root| seen.insert(root.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_existing_checkout_in_first_matching_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root1 = tmp.path().join("root1");
        let root2 = tmp.path().join("root2");
        // root2 にだけ checkout がある
        let checkout = root2.join("github.com/okkez/glauca");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::create_dir_all(&root1).unwrap();

        let roots = vec![root1, root2];
        let resolved = resolve_in_roots(&roots, "okkez", "glauca");
        assert_eq!(resolved, Some(checkout));
    }

    #[test]
    fn prefers_earlier_root_when_multiple_match() {
        let tmp = tempfile::tempdir().unwrap();
        let root1 = tmp.path().join("root1");
        let root2 = tmp.path().join("root2");
        let checkout1 = root1.join("github.com/okkez/glauca");
        let checkout2 = root2.join("github.com/okkez/glauca");
        std::fs::create_dir_all(&checkout1).unwrap();
        std::fs::create_dir_all(&checkout2).unwrap();

        let roots = vec![root1, root2];
        let resolved = resolve_in_roots(&roots, "okkez", "glauca");
        assert_eq!(resolved, Some(checkout1));
    }

    #[test]
    fn returns_none_when_no_root_has_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();

        let roots = vec![root];
        assert_eq!(resolve_in_roots(&roots, "okkez", "glauca"), None);
    }

    #[test]
    fn ignores_non_directory_at_checkout_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let parent = root.join("github.com/okkez");
        std::fs::create_dir_all(&parent).unwrap();
        // ディレクトリではなくファイルを置く
        std::fs::write(parent.join("glauca"), b"not a dir").unwrap();

        let roots = vec![root];
        assert_eq!(resolve_in_roots(&roots, "okkez", "glauca"), None);
    }

    #[test]
    fn expand_tilde_expands_leading_home() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde("~/wc/src"), home.join("wc/src"));
        assert_eq!(expand_tilde("~"), home);
        // 途中の ~ は展開しない
        assert_eq!(expand_tilde("/abs/~/x"), PathBuf::from("/abs/~/x"));
    }

    #[test]
    fn dedup_preserves_order_and_removes_duplicates() {
        let roots = vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/a"),
        ];
        assert_eq!(dedup(roots), vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    }
}
