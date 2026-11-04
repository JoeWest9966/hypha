//! hyphad — the hub. API, agent rendezvous, installer channel. No UI.

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

/// Locate a built panel bundle. Returns `None` when there is nothing to serve,
/// which is a legal production state: the hub then runs API-only and nginx (or
/// Caddy) serves the panel from wherever it was deployed.
fn find_static_dir(configured: &str) -> Option<PathBuf> {
    let configured = configured.trim();

    // An explicit setting is the whole answer, including when it points nowhere.
    // Falling back from a configured path would make "API only" impossible to
    // ask for, and the panel no longer lives in this repository's roadmap.
    if !configured.is_empty() {
        let p = PathBuf::from(configured);
        return p.join("index.html").is_file().then_some(p);
    }

    // The panel is a separate project now; this repository ships no bundle. A
    // configured path is the only thing tried first, then the conventional
    // install location for a bundle someone dropped next to the binary.
    let candidates = ["./static".to_string(), "/usr/local/share/hypha/static".to_string()];

    for c in candidates {
        let p = PathBuf::from(&c);
        if p.join("index.html").is_file() {
            return Some(p);
        }
    }
    None
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
    let serves_panel = static_dir.is_some();
    match &static_dir {
        Some(dir) => tracing::info!("panel: serving {}", dir.display()),
        None => tracing::info!(
            "panel: none (api only) -- serve the panel yourself, e.g. nginx root -> mycelium/dist"
        ),
    }
    tracing::info!("bin dir: {}", cfg.bin_dir);

    let mut app = Router::new()
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
        .route("/api/exec/{id}/cancel", post(api::cancel_exec))
        .route("/api/files", post(api::push_files))
        .route("/api/audit", get(api::list_audit))
        .route("/api/pty", get(api::pty).post(api::open_pty))
        .route("/agent", get(ws::upgrade))
        .route("/install.sh", get(api::install_sh))
        .route("/dl/{name}", get(api::download))
        .route("/health", get(api::health));

    app = match static_dir {
        Some(dir) => {
            let index = dir.join("index.html");
            app.fallback_service(ServeDir::new(&dir).not_found_service(ServeFile::new(&index)))
        }
        // No bundle here: answer with a pointer instead of a 404, so an empty
        // `static_dir` is an intentional mode and not a deployment mystery.
        None => app.fallback(api::no_panel),
    };
    let app = app.layer(TraceLayer::new_for_http()).with_state(st);

    let addr: SocketAddr = cfg.listen.parse().context("invalid --listen")?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("hypha hub listening on http://{addr}");
    if serves_panel {
        println!("  panel   http://{addr}/");
    } else {
        println!("  panel   not served here (api only)");
    }
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
