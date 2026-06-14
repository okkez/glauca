// glauca-core: framework-agnostic core for glauca (TUI/GUI 共有).
// フェーズ A で db/github/model をここへ移設済み。
// フェーズ A6 で engine（非同期タスク／メッセージ型）も移設済み。

pub mod db;
pub mod engine;
pub mod filter;
pub mod github;
pub mod logic;
pub mod model;
pub mod types;
