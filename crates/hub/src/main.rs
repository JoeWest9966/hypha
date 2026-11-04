//! hyphad — the hub. Panel, API, agent rendezvous. One static binary.

mod api;
mod db;
mod pty;
mod state;
mod util;
mod ws;

use anyhow::{Context, Result};
use axum::routing::{delete, get, post};
use axum::Router;
use clap::Parser;
use state::{AppState, Config};
use std::net::SocketAddr;
use std::path::PathBuf;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

#[derive(Parser, Debug)]
#[command(name = "hyphad", version, about = "hypha hub: panel, API, agent rendezvous")]
struct Args {
    /// Path to hub.json
    #[arg(long, default_value = "./hub.json", env = "HYPHA_CONFIG")]
    config: PathBuf,
    #[arg(long)]
    listen: Option<String>,
    #[arg(long)]
    data_dir: Option<String>,
    #[arg(long)]
    bin_dir: Option<String>,
    #[arg(long)]
    static_dir: Option<String>,
    /// Public base URL for install commands, e.g. https://hub.example.com
    #[arg(long)]
    public_url: Option<String>,
}

fn find_static_dir(configured: &str) -> PathBuf {
    let candidates = [
        configured.to_string(),
        "./static".into(),
        "./crates/hub/static".into(),
        concat!(env!("CARGO_MANIFEST_DIR"), "/static").to_string(),
        "/usr/local/share/hypha/static".into(),
    ];
    for c in candidates {
        let p = PathBuf::from(&c);
        if p.join("index.html").exists() {
            return p;
        }
    }
    PathBuf::from(configured)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hyphad=info,tower_http=warn".into()),
        )
        .with_target(false)
        .init();

    let args = Args::parse();
    let mut cfg = Config::load(&args.config).context("loading hub config")?;
    if let Some(v) = args.listen {
        cfg.listen = v;
    }
    if let Some(v) = args.data_dir {
        cfg.data_dir = v;
    }
    if let Some(v) = args.bin_dir {
        cfg.bin_dir = v;
    }
    if let Some(v) = args.static_dir {
        cfg.static_dir = v;
    }
    if let Some(v) = args.public_url {
        cfg.public_url = v;
    }

    std::fs::create_dir_all(&cfg.data_dir).ok();
    let db_path = PathBuf::from(&cfg.data_dir).join("hypha.db");
    let db = db::Db::open(&db_path)?;
    let st = AppState::new(cfg.clone(), db);

    let static_dir = find_static_dir(&cfg.static_dir);
    tracing::info!("static dir: {}", static_dir.display());
    tracing::info!("bin dir: {}", cfg.bin_dir);

    let index = static_dir.join("index.html");
    let app = Router::new()
        .route("/api/meta", get(api::meta))
        .route("/api/enroll", post(api::enroll))
        .route("/api/hosts", get(api::list_hosts))
        .route("/api/hosts/{id}", delete(api::delete_host))
        .route("/api/hosts/{id}/tags", post(api::set_tags))
        .route(
            "/api/enroll-tokens",
            get(api::list_enroll_tokens).post(api::create_enroll_token),
        )
        .route(
            "/api/enroll-tokens/{token}",
            delete(api::revoke_enroll_token),
        )
        .route("/api/exec", post(api::create_exec))
        .route("/api/exec/{id}/events", get(api::exec_events))
        .route("/api/execs", get(api::recent_execs))
        .route("/api/pty", get(api::pty))
        .route("/agent", get(ws::upgrade))
        .route("/install.sh", get(api::install_sh))
        .route("/dl/{name}", get(api::download))
        .route("/health", get(api::health))
        .fallback_service(ServeDir::new(&static_dir).not_found_service(ServeFile::new(&index)))
        .layer(TraceLayer::new_for_http())
        .with_state(st);

    let addr: SocketAddr = cfg.listen.parse().context("invalid --listen")?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("hypha hub listening on http://{addr}");
    println!("  panel   http://{addr}/");
    println!("  agents  ws://{addr}/agent");
    if cfg.public_url.is_empty() {
        println!("  note: set public_url (or use --public-url) so install commands are correct behind a proxy");
    }
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
