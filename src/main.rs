use crate::analysis::AnalysisCache;
use clap::Parser;
use mimalloc::MiMalloc;
use plotters::style::FontStyle;
use regex::Regex;

mod analysis;
mod init_tracing;
mod plot;
mod server;

// we need a TON of allocations.
// we prefer static builds for the server.
// this leaves us with musl allocator, which is very bad.
// replace it with mimalloc.
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Parser)]
enum Cli {
    Server {
        port: u16,
    },
    Analyse {
        url: String,

        #[clap(short, long, default_value = r"\.rs$")]
        filter: String,

        #[clap(short, long, value_enum, default_value_t)]
        x_coord: plot::XCoord,
        #[clap(short, long, value_enum, default_value_t)]
        y_coord: plot::YCoord,
        #[clap(short, long)]
        svg_out: Option<String>,
    },
}

#[tokio::main]
async fn main() {
    // tracing_subscriber::fmt::init();
    init_tracing::init_tracing().expect("Failed to init tracing");

    plotters::style::register_font(
        "sans-serif",
        FontStyle::Normal,
        include_bytes!("../FiraSans-Regular.otf"),
    )
    .map_err(|_| "BUG: failed to register font")
    .unwrap();

    let cli = Cli::parse();

    match cli {
        Cli::Server { port } => {
            server::start(port).await;
        }
        Cli::Analyse {
            url,
            filter,
            x_coord,
            y_coord,
            svg_out,
        } => {
            // let url = "/home/dcnick3/git_cloned/unsafe-libopus/";
            // let url = "https://github.com/DCNick3/unsafe-libopus";
            // let url = "https://github.com/rust-lang/rust";

            // let path_filter = Regex::new(
            //     r"(?x)
            // ^/src/(
            //         celt/(
            //             [^/]* \.rs|
            //             modes/[^/]* \.rs
            //         )|
            //         silk/(
            //             [^/]* \.rs|
            //             float/[^/]* \.rs|
            //             resampler/[^/]* \.rs
            //         )
            //         src/ .* \.rs
            // )$",
            // )
            //     .unwrap();

            // let a = r"(?x)";

            let path_filter = Regex::new(&filter).unwrap();

            let cache = AnalysisCache::new(0);

            let results = analysis::analyse_repo(&cache, &url, path_filter);

            if let Some(svg_out) = svg_out {
                let svg = plot::plot_results_svg(&results, x_coord, y_coord);
                std::fs::write(svg_out, &svg).unwrap();
            }

            for r in &results {
                let counts = y_coord.get_counts(r);
                println!(
                    "{} {}: [{}] {} | {}",
                    r.oid,
                    r.date.format(gix_date::time::format::SHORT),
                    r.failed_files_count,
                    counts.unsafe_,
                    counts.safe,
                );
            }
        }
    }
}
