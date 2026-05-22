mod db;
mod github;
mod model;
mod tui;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db_path = db::default_db_path();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let pool = db::open_pool(&db_path).await?;
    let gh_client = github::build_client()?;
    tui::run(pool, gh_client).await?;
    Ok(())
}
