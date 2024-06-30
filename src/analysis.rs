use cargo_geiger_serde::CounterBlock;
use geiger::{IncludeTests, RsFileMetrics};
use gix_hash::ObjectId;
use gix_object::tree::EntryKind;
use gix_object::{CommitRef, Kind, ObjectRef};
use gix_pack::data::entry::Header;
use gix_pack::Bundle;
use gix_protocol::fetch::{Action, Arguments, Delegate, DelegateBlocking, Response};
use gix_protocol::handshake::Ref;
use gix_protocol::FetchConnection;
use moka::sync::Cache;
use prodash::NestedProgress;
use rayon::prelude::*;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::sync::atomic::AtomicBool;
use tempfile::{NamedTempFile, TempDir};
use thiserror::Error;
use tracing::{debug, error, info, instrument};

// I hope nobody will send zip bombs, haha :sweat:
const MAX_PACK_SIZE: u64 = 10 * 1024 * 1024;

struct FetchDelegate {
    pack_sink: File,
}

impl DelegateBlocking for FetchDelegate {
    fn negotiate(
        &mut self,
        refs: &[Ref],
        arguments: &mut Arguments,
        _previous_response: Option<&Response>,
    ) -> std::io::Result<Action> {
        // debug!("Server has offered refs: {:?}", refs);

        let Some(wanted) = refs.iter().find_map(|r| match r {
            &Ref::Symbolic {
                ref full_ref_name,
                object,
                ..
            } if full_ref_name == "HEAD" => Some(object),
            _ => None,
        }) else {
            error!("Could not find the wanted ref");
            return Ok(Action::Cancel);
        };

        debug!("Found the wanted object: {}", wanted);

        // TODO: when we'll have a cache, tell the server our haves
        // arguments.have();
        arguments.want(wanted);

        Ok(Action::Cancel)
    }
}

impl Delegate for FetchDelegate {
    fn receive_pack(
        &mut self,
        mut input: impl BufRead,
        _progress: impl NestedProgress + 'static,
        _refs: &[Ref],
        _previous_response: &Response,
    ) -> std::io::Result<()> {
        info!("Downloading the pack file...");
        // copy the data to pack_sink, but fail if we download more than MAX_PACK_SIZE

        let mut total_bytes = 0;
        let mut buf = [0; 8192];
        loop {
            let bytes_read = input.read(&mut buf)?;
            if bytes_read == 0 {
                break;
            }
            total_bytes += bytes_read as u64;
            if total_bytes > MAX_PACK_SIZE {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Pack file too large",
                ));
            }
            self.pack_sink.write_all(&buf[..bytes_read])?;
        }

        info!("Finished downloading {} bytes pack", total_bytes);

        Ok(())
    }
}

#[derive(Error, Debug, Clone)]
enum BlobAnalysisError {
    #[error("UTF-8 error: {0}")]
    NotUtf8(#[from] std::str::Utf8Error),
    #[error("Syn error: {0}")]
    Syn(#[from] syn::Error),
}

#[derive(Clone)]
pub struct CommitResult {
    pub oid: ObjectId,
    pub index: u32,
    pub date: gix_date::Time,
    pub failed_files_count: usize,
    pub counters: CounterBlock,
}

#[derive(Clone)]
pub struct AnalysisCache {
    cache: Cache<ObjectId, Result<RsFileMetrics, BlobAnalysisError>>,
}

impl AnalysisCache {
    pub fn new(capacity: u64) -> Self {
        Self {
            cache: Cache::new(capacity),
        }
    }
}

#[tracing::instrument]
fn download_repo_pack(url: &str, tempfile: NamedTempFile) -> NamedTempFile {
    let options = gix_transport::connect::Options::default();

    let transport = gix_transport::connect(url, options).expect("Connect");

    let (pack_file, pack_path) = tempfile.into_parts();

    let mut delegate = FetchDelegate {
        pack_sink: pack_file,
    };

    let agent = gix_protocol::agent("unsafe-track");

    gix_protocol::fetch(
        transport,
        &mut delegate,
        |_| todo!(),
        prodash::progress::Discard,
        FetchConnection::TerminateOnSuccessfulCompletion,
        agent,
        true,
    )
    .expect("Fetch");

    NamedTempFile::from_parts(delegate.pack_sink, pack_path)
}

#[tracing::instrument]
fn build_bundle(mut pack_file: NamedTempFile) -> (TempDir, Bundle) {
    pack_file.as_file_mut().seek(SeekFrom::Start(0)).unwrap();

    let mut pack_iobuf = BufReader::new(pack_file.as_file_mut());

    let index_dir = tempfile::tempdir().unwrap();

    info!("Resolving deltas...");
    let should_interrupt = AtomicBool::new(false);
    let bundle = Bundle::write_to_directory(
        &mut pack_iobuf,
        Some(index_dir.path()),
        &mut prodash::progress::Discard,
        &should_interrupt,
        Some(gix_object::find::Never),
        Default::default(),
    )
    .expect("Indexing failed")
    .to_bundle()
    .unwrap()
    .unwrap();

    (index_dir, bundle)
}

struct CommitInfo {
    date: gix_date::Time,
    matching_blobs: Vec<(String, ObjectId)>,
}

struct PlannedAnalysis {
    commits: HashMap<ObjectId, CommitInfo>,
    interesting_blobs: HashSet<ObjectId>,
}

#[instrument(skip(bundle))]
pub fn plan_analysis(bundle: &Bundle, path_filter: &Regex) -> PlannedAnalysis {
    let mut commits: HashMap<ObjectId, CommitInfo> = HashMap::new();
    let mut interesting_blobs: HashSet<ObjectId> = HashSet::new();

    struct RecurCtx<'a> {
        interesting_blobs: &'a mut HashSet<ObjectId>,
        commit_matching_blobs: &'a mut Vec<(String, ObjectId)>,
        cache: &'a mut gix_pack::cache::lru::MemoryCappedHashmap,
        inflate: &'a mut gix_features::zlib::Inflate,
    }

    fn entry_kind(bundle: &gix_pack::Bundle, entry: &gix_pack::data::Entry) -> Kind {
        match entry.header {
            Header::Commit => Kind::Commit,
            Header::Tree => Kind::Tree,
            Header::Blob => Kind::Blob,
            Header::Tag => Kind::Tag,
            Header::RefDelta { .. } => {
                todo!()
            }
            Header::OfsDelta { base_distance } => entry_kind(
                bundle,
                &bundle
                    .pack
                    .entry(entry.base_pack_offset(base_distance))
                    .unwrap(),
            ),
        }
    }

    // TODO: tune the cache size
    let mut cache = gix_pack::cache::lru::MemoryCappedHashmap::new(2 * 1024 * 1024);

    info!("Finding the blobs to analyse...");

    let mut inflate = gix_features::zlib::Inflate::default();
    let mut out_buf = Vec::new();
    for entry in bundle.index.iter() {
        let oid = entry.oid;
        let entry = bundle.pack.entry(entry.pack_offset).unwrap();
        if let Kind::Commit = entry_kind(&bundle, &entry) {
            let _ = bundle
                .pack
                .decode_entry(entry, &mut out_buf, &mut inflate, &|_, _| None, &mut cache)
                .unwrap();
            let commit = CommitRef::from_bytes(&out_buf).unwrap();

            let mut info = CommitInfo {
                date: commit.committer.time,
                matching_blobs: Vec::new(),
            };

            fn recur_tree(
                bundle: &gix_pack::Bundle,
                oid: ObjectId,
                path: String,
                path_filter: &Regex,
                ctx: &mut RecurCtx,
            ) {
                // TODO: reuse those
                let mut buf = Vec::new();
                let (data, _location) = bundle
                    .find(&oid, &mut buf, ctx.inflate, ctx.cache)
                    .unwrap()
                    .unwrap();
                let ObjectRef::Tree(tree) = data.decode().unwrap() else {
                    unreachable!()
                };
                for entry in &tree.entries {
                    let oid = entry.oid.to_owned();
                    match entry.mode.kind() {
                        EntryKind::Tree => {
                            let path = format!("{}/{}", path, entry.filename);
                            recur_tree(bundle, oid, path, path_filter, ctx);
                        }
                        EntryKind::Blob | EntryKind::BlobExecutable => {
                            let path = format!("{}/{}", path, entry.filename);
                            if path_filter.is_match(&path) {
                                ctx.interesting_blobs.insert(oid);
                                ctx.commit_matching_blobs.push((path, oid));
                            }
                        }
                        EntryKind::Link | EntryKind::Commit => {}
                    }
                }
            }

            recur_tree(
                &bundle,
                commit.tree(),
                "".to_string(),
                &path_filter,
                &mut RecurCtx {
                    interesting_blobs: &mut interesting_blobs,
                    commit_matching_blobs: &mut info.matching_blobs,
                    cache: &mut cache,
                    inflate: &mut inflate,
                },
            );

            commits.insert(oid, info);
        }
    }

    PlannedAnalysis {
        commits,
        interesting_blobs,
    }
}

#[instrument(skip_all, fields(blob_count = interesting_blobs.len()))]
fn analyse_with_cache(
    blob_analysis_cache: &AnalysisCache,
    bundle: &Bundle,
    interesting_blobs: &HashSet<ObjectId>,
) -> HashMap<ObjectId, Result<RsFileMetrics, BlobAnalysisError>> {
    debug!("Analysing {} blobs...", interesting_blobs.len());

    let cached_blob_analysis_results = interesting_blobs
        .iter()
        .filter_map(|&oid| Some((oid, blob_analysis_cache.cache.get(&oid)?)))
        .collect::<HashMap<_, _>>();

    debug!(
        "Re-used {} ({}%) results from cache",
        cached_blob_analysis_results.len(),
        cached_blob_analysis_results.len() * 100 / interesting_blobs.len()
    );

    let blob_analysis_results = interesting_blobs
        .iter()
        .cloned()
        .filter(|oid| !cached_blob_analysis_results.contains_key(oid))
        .collect::<Vec<_>>()
        .par_iter()
        .map_init(
            || {
                (
                    Vec::new(),
                    gix_features::zlib::Inflate::default(),
                    blob_analysis_cache.cache.clone(),
                )
            },
            |(buf, inflate, cache), oid| {
                let (data, _location) = bundle
                    // no cache, because we will never look up a repeated oid
                    .find(&oid, buf, inflate, &mut gix_pack::cache::Never)
                    .unwrap()
                    .unwrap();
                let ObjectRef::Blob(blob) = data.decode().unwrap() else {
                    unreachable!()
                };

                let result: Result<RsFileMetrics, BlobAnalysisError> = (|| {
                    let data = std::str::from_utf8(blob.data)?;
                    let metrics = geiger::find_unsafe_in_string(data, IncludeTests::Yes)?;
                    Ok(metrics)
                })();

                cache.insert(oid.to_owned(), result.clone());

                (oid.to_owned(), result)
            },
        )
        .chain(cached_blob_analysis_results)
        .collect::<HashMap<_, _>>();

    info!("Analysis finished!");

    blob_analysis_results
}

#[tracing::instrument(skip_all, fields(commit_count = commits.len()))]
fn build_results(
    commits: &HashMap<ObjectId, CommitInfo>,
    blob_analysis_results: &HashMap<ObjectId, Result<RsFileMetrics, BlobAnalysisError>>,
) -> Vec<CommitResult> {
    let mut results = Vec::new();
    for (&oid, info) in commits.iter() {
        let mut counters = CounterBlock::default();
        let mut failed_files_count = 0;
        for (_path, blob_oid) in &info.matching_blobs {
            match blob_analysis_results.get(blob_oid).unwrap() {
                Ok(result) => {
                    counters += result.counters.clone();
                }
                Err(_e) => {
                    // warn!("Analysing file {} @ {} failed: {}", path, oid, e);
                    failed_files_count += 1;
                }
            }
        }

        results.push(CommitResult {
            oid,
            date: info.date,
            // this will be filled after sorting
            index: 0,
            failed_files_count,
            counters,
        });
    }

    results.sort_by_key(|c| c.date);

    // fill in the index
    for (i, r) in results.iter_mut().enumerate() {
        r.index = i as u32;
    }

    results
}

#[tracing::instrument(skip(blob_analysis_cache))]
pub fn analyse_repo(
    blob_analysis_cache: &AnalysisCache,
    url: &str,
    path_filter: Regex,
) -> Vec<CommitResult> {
    let mut pack_file = download_repo_pack(url, NamedTempFile::new().unwrap());
    pack_file.as_file_mut().seek(SeekFrom::Start(0)).unwrap();

    let (_index_dir, bundle) = build_bundle(pack_file);

    let PlannedAnalysis {
        commits,
        interesting_blobs,
    } = plan_analysis(&bundle, &path_filter);

    let blob_analysis_results =
        analyse_with_cache(blob_analysis_cache, &bundle, &interesting_blobs);

    build_results(&commits, &blob_analysis_results)
}
