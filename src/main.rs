mod db;
mod github;
mod model;
mod tui;

#[tokio::main]
async fn main() {
    let db_path = db::default_db_path();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).expect("failed to create data directory");
    }
    let pool = db::open_pool(&db_path).await.expect("failed to open database");
    let gh_client = github::build_client().expect("failed to build GitHub client");
    if let Err(e) = tui::run(pool, gh_client).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
