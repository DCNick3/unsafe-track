use crate::analysis::AnalysisCache;
use crate::plot::{XCoord, YCoord};
use crate::{analysis, plot};
use axum::extract::State;
use axum::{
    extract::{Path, Query},
    routing::get,
    Router,
};
use axum_extra::TypedHeader;
use axum_tracing_opentelemetry::middleware::{OtelAxumLayer, OtelInResponseLayer};
use headers::{CacheControl, ContentType};
use regex::Regex;
use serde::Deserialize;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tower_http::catch_panic::CatchPanicLayer;
use tracing::{info, info_span, Span};

const ANALYSIS_CACHE_SIZE: u64 = 50_000;

#[derive(Clone)]
struct AppState {
    blob_analysis_cache: AnalysisCache,
}

pub async fn start(port: u16) {
    let middleware = tower::ServiceBuilder::new()
        .layer(CatchPanicLayer::new())
        // include trace context as header into the response
        .layer(OtelInResponseLayer)
        // start OpenTelemetry trace on incoming request
        .layer(OtelAxumLayer::default());

    // create the axum server
    let app = Router::new()
        .route("/github/:owner/:repo", get(github))
        .with_state(AppState {
            blob_analysis_cache: AnalysisCache::new(ANALYSIS_CACHE_SIZE),
        })
        .layer(middleware);

    let listener =
        tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port))
            .await
            .unwrap();

    info!("Listening on port {}", port);
    axum::serve(listener, app).await.unwrap();
}

#[derive(Deserialize)]
pub struct GithubParams {
    pub path_filter: Option<String>,
    #[serde(default)]
    pub x_coord: XCoord,
    #[serde(default)]
    pub y_coord: YCoord,
}

async fn github(
    State(AppState {
        blob_analysis_cache,
    }): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
    Query(params): Query<GithubParams>,
) -> (TypedHeader<ContentType>, TypedHeader<CacheControl>, String) {
    let url = format!("https://github.com/{}/{}", owner, repo);
    let path_filter = Regex::new(&params.path_filter.unwrap_or(r"\.rs$".to_string())).unwrap();

    let span = Span::current();

    // TODO: return error
    // TODO: cache
    let rendered = tokio::task::spawn_blocking(move || {
        // connect the parent manually
        let _span = info_span!(parent: &span, "blocking_analysis", url = %url).entered();

        let results = analysis::analyse_repo(&blob_analysis_cache, &url, path_filter);

        plot::plot_results_svg(&results, params.x_coord, params.y_coord)
    })
    .await
    .unwrap();

    (
        TypedHeader(mime::IMAGE_SVG.into()),
        TypedHeader(CacheControl::new().with_no_cache()),
        rendered,
    )
}
