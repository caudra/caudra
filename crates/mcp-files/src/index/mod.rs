mod languages;
mod model;
mod render;
mod traversal;
mod types;

use std::{
    cmp::Ordering,
    fs::Metadata,
    ops::ControlFlow,
    path::{Path, PathBuf},
    str,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::{
    fs,
    sync::{Semaphore, SemaphorePermit},
};
use tokio_util::sync::CancellationToken;
use tree_sitter::{ParseOptions, Parser};
use workcell_source_languages::Language;

use crate::{
    FileResource, FileResourceAccess, FilesystemError,
    operations::FilesystemCore,
    text::{check_cancelled, read_bounded, reject_binary},
};

pub use types::*;

use self::{
    model::ParsedSkeleton,
    traversal::{Context, ExtractionGuard, ParseFailure, inspect_tree},
};

const TRUNCATED: &str = "[truncated]";
static PARSER_SEMAPHORE: Semaphore = Semaphore::const_new(INDEX_PARSER_CONCURRENCY);

fn parse_skeleton(
    source: String,
    language: Language,
    limits: IndexLimits,
    guard: ExtractionGuard,
) -> Result<ParsedSkeleton, ParseFailure> {
    guard.check()?;
    let mut parser = Parser::new();
    parser
        .set_language(&language.grammar())
        .map_err(|_| ParseFailure::Parser)?;
    let mut progress = |_: &tree_sitter::ParseState| {
        if guard.interrupted() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let options = ParseOptions::new().progress_callback(&mut progress);
    let bytes = source.as_bytes();
    let mut input = |offset: usize, _| bytes.get(offset..).unwrap_or_default();
    let tree = parser
        .parse_with_options(&mut input, None, Some(options))
        .ok_or_else(|| guard.failure())?;
    inspect_tree(&tree, limits, &guard)?;
    let parse_error = tree.root_node().has_error();
    let context = Context::new(&source, &guard);
    let mut output = languages::extract(language, tree.root_node(), &context)?;
    guard.check()?;
    output.parse_error = parse_error;
    Ok(output)
}

async fn run_parser(
    source: String,
    language: Language,
    limits: IndexLimits,
    cancellation: CancellationToken,
    permit: SemaphorePermit<'static>,
) -> Result<ParsedSkeleton, FilesystemError> {
    let deadline = Instant::now() + Duration::from_millis(limits.parser_deadline_ms);
    let queued_permit = Arc::new(Mutex::new(Some(permit)));
    let worker_permit = Arc::clone(&queued_permit);
    let worker_cancellation = cancellation.clone();
    let mut worker = tokio::task::spawn_blocking(move || {
        let guard = ExtractionGuard::new(worker_cancellation, deadline);
        guard.check()?;
        let permit = worker_permit
            .lock()
            .map_err(|_| ParseFailure::Parser)?
            .take()
            .ok_or_else(|| guard.failure())?;
        let _permit = permit;
        parse_skeleton(source, language, limits, guard)
    });
    let timer = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
    tokio::pin!(timer);
    let parsed = tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            revoke_queued_permit(&queued_permit);
            worker.abort();
            return Err(FilesystemError::Aborted);
        }
        result = &mut worker => result
            .map_err(|_| FilesystemError::message("Index parser worker failed"))?,
        () = &mut timer => {
            revoke_queued_permit(&queued_permit);
            worker.abort();
            return Err(ParseFailure::Deadline.into_filesystem_error());
        }
    };
    parsed.map_err(ParseFailure::into_filesystem_error)
}

fn revoke_queued_permit(permit: &Mutex<Option<SemaphorePermit<'static>>>) {
    if let Ok(mut permit) = permit.lock() {
        permit.take();
    }
}

/// Resolves the language for a path, or the refusal the `index` tool reports.
///
/// Detection itself lives in `workcell-source-languages` so the code map and this tool cannot
/// disagree about what a `.tf` file is. Only the refusal wording is owned here, because it names
/// the tool a caller should reach for instead.
fn detect(path: &Path) -> Result<Language, FilesystemError> {
    if let Some(language) = Language::from_path(path) {
        return Ok(language);
    }
    match path.extension().and_then(|value| value.to_str()) {
        Some(extension) => Err(FilesystemError::message(format!(
            "Unsupported file type: .{extension}. Use file_read instead."
        ))),
        None => Err(FilesystemError::message(
            "Unsupported file type: (no extension). Use file_read instead.",
        )),
    }
}

impl FilesystemCore {
    #[cfg(test)]
    pub(crate) async fn index(
        &self,
        input: IndexInput,
        configuration: IndexExecutionConfiguration,
        token: &CancellationToken,
    ) -> Result<IndexOutput, FilesystemError> {
        check_cancelled(token)?;
        let limits = configuration.limits.validate()?;
        let path = self.policy.resolve(&input.path).await?;
        let metadata = fs::metadata(&path).await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                FilesystemError::message(format!("Path not found: {}", input.path))
            } else {
                FilesystemError::io_path("Cannot inspect", &path, error)
            }
        })?;
        self.index_resolved(path, metadata, limits, token).await
    }

    pub(crate) async fn index_authorized(
        &self,
        resource: FileResource,
        configuration: IndexExecutionConfiguration,
        token: &CancellationToken,
    ) -> Result<IndexOutput, FilesystemError> {
        let limits = configuration.limits.validate()?;
        self.index_authorized_with_limits(resource, limits, token)
            .await
    }

    pub(crate) async fn index_authorized_with_limits(
        &self,
        resource: FileResource,
        limits: IndexLimits,
        token: &CancellationToken,
    ) -> Result<IndexOutput, FilesystemError> {
        check_cancelled(token)?;
        let path = self.policy.revalidate(&resource.path).await?;
        if path != resource.path {
            return Err(FilesystemError::message(format!(
                "Index path changed after authorization: {}",
                resource.requested_path
            )));
        }
        let metadata = fs::metadata(&path)
            .await
            .map_err(|error| FilesystemError::io_path("Cannot inspect", &path, error))?;
        let access = if metadata.is_dir() {
            FileResourceAccess::Traverse
        } else if metadata.is_file() {
            FileResourceAccess::Read
        } else {
            return Err(FilesystemError::message(format!(
                "Path is not a regular file or directory: {}",
                resource.requested_path
            )));
        };
        if access != resource.access {
            return Err(FilesystemError::message(format!(
                "Index path type changed after authorization: {}",
                resource.requested_path
            )));
        }
        self.index_resolved(path, metadata, limits, token).await
    }

    async fn index_resolved(
        &self,
        path: PathBuf,
        metadata: Metadata,
        limits: IndexLimits,
        token: &CancellationToken,
    ) -> Result<IndexOutput, FilesystemError> {
        if metadata.is_dir() {
            return self.index_directory(&path, limits, token).await;
        }
        if !metadata.is_file() {
            return Err(FilesystemError::message(format!(
                "Path is not a regular file or directory: {}",
                path.to_string_lossy()
            )));
        }
        self.index_file(&path, limits, token).await
    }

    async fn index_file(
        &self,
        path: &Path,
        limits: IndexLimits,
        token: &CancellationToken,
    ) -> Result<IndexOutput, FilesystemError> {
        let language = detect(path)?;
        let bytes = read_bounded(path, limits.max_source_bytes, token).await?;
        reject_binary(path, &bytes)?;
        let source = str::from_utf8(&bytes).map_err(|_| {
            FilesystemError::message(format!(
                "Index requires strict UTF-8 text: {}",
                path.to_string_lossy()
            ))
        })?;
        let source_line_count = source.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let permit = tokio::select! {
            () = token.cancelled() => return Err(FilesystemError::Aborted),
            result = tokio::time::timeout(
                Duration::from_millis(limits.admission_deadline_ms),
                PARSER_SEMAPHORE.acquire(),
            ) => result
                .map_err(|_| FilesystemError::message("Index parser admission deadline exceeded"))?
                .map_err(|_| FilesystemError::message("Index parser is unavailable"))?,
        };
        let source = source.to_owned();
        let parsed = run_parser(source, language, limits, token.clone(), permit).await?;
        let parse_error = parsed.parse_error;
        let BoundedSkeleton {
            skeleton,
            lines,
            truncated,
        } = bound_skeleton(parsed, limits, token)?;
        Ok(IndexOutput::File {
            path: path_string(path),
            relative_path: self.policy.relative(path)?,
            language: language.name().to_owned(),
            skeleton,
            lines,
            source_line_count,
            parse_error,
            truncated,
        })
    }

    async fn index_directory(
        &self,
        path: &Path,
        limits: IndexLimits,
        token: &CancellationToken,
    ) -> Result<IndexOutput, FilesystemError> {
        let mut reader = fs::read_dir(path)
            .await
            .map_err(|error| FilesystemError::io_path("Cannot read directory", path, error))?;
        let mut entries = Vec::new();
        let mut total_count = 0usize;
        let mut scanned = 0usize;
        let mut truncated = false;
        loop {
            check_cancelled(token)?;
            let Some(entry) = reader
                .next_entry()
                .await
                .map_err(|error| FilesystemError::io_path("Cannot read directory", path, error))?
            else {
                break;
            };
            if scanned == limits.max_directory_scan_entries {
                truncated = true;
                break;
            }
            scanned += 1;
            let name = entry.file_name().to_string_lossy().into_owned();
            let candidate = match self.policy.resolve(&path_string(&entry.path())).await {
                Ok(candidate) => candidate,
                Err(FilesystemError::RootEscape(_) | FilesystemError::ProtectedPath(_)) => continue,
                Err(_) => {
                    truncated = true;
                    continue;
                }
            };
            let metadata = match fs::metadata(candidate).await {
                Ok(metadata) => metadata,
                Err(_) => {
                    truncated = true;
                    continue;
                }
            };
            let kind = if metadata.is_dir() {
                IndexDirectoryEntryKind::Directory
            } else if metadata.is_file() {
                IndexDirectoryEntryKind::File
            } else {
                continue;
            };
            total_count += 1;
            entries.push(IndexDirectoryEntry { name, kind });
        }
        entries.sort_by(|left, right| match left.kind.cmp(&right.kind) {
            Ordering::Equal => left.name.cmp(&right.name),
            ordering => ordering,
        });
        if entries.len() > limits.max_directory_entries {
            entries.truncate(limits.max_directory_entries);
            truncated = true;
        }
        let mut listing_lines = Vec::with_capacity(entries.len());
        for entry in &entries {
            check_cancelled(token)?;
            listing_lines.push(match entry.kind {
                IndexDirectoryEntryKind::Directory => format!("{}/", entry.name),
                IndexDirectoryEntryKind::File => entry.name.clone(),
            });
        }
        let raw_listing = listing_lines.join("\n");
        let (mut listing, listing_truncated) = bound_model_text(
            &raw_listing,
            limits.max_output_line_bytes,
            limits.max_model_output_bytes,
            Some(token),
        )?;
        truncated |= listing_truncated;
        if truncated {
            append_truncation_marker(&mut listing, limits.max_model_output_bytes);
        }
        Ok(IndexOutput::Directory {
            path: path_string(path),
            relative_path: self.policy.relative(path)?,
            entries,
            total_count,
            truncated,
            listing,
        })
    }
}

struct BoundedSkeleton {
    skeleton: String,
    lines: Vec<IndexOutputLine>,
    truncated: bool,
}

fn bound_skeleton(
    parsed: ParsedSkeleton,
    limits: IndexLimits,
    token: &CancellationToken,
) -> Result<BoundedSkeleton, FilesystemError> {
    let raw = parsed.skeleton.trim_end_matches('\n');
    let (skeleton, output_truncated) = bound_model_text(
        raw,
        limits.max_output_line_bytes,
        limits.max_model_output_bytes,
        Some(token),
    )?;
    let raw_lines = raw.split('\n').collect::<Vec<_>>();
    let emitted = if skeleton.is_empty() {
        Vec::new()
    } else {
        skeleton.split('\n').collect::<Vec<_>>()
    };
    let mut lines = Vec::with_capacity(emitted.len());
    for (index, text) in emitted.into_iter().enumerate() {
        check_cancelled(token)?;
        let original = raw_lines.get(index).copied().unwrap_or(text);
        let (semantic, body, source_range) =
            if output_truncated && text == TRUNCATED && original != TRUNCATED {
                (IndexLineSemantic::Dimmed, None, None)
            } else {
                line_metadata(original, parsed.metadata.get(index))
            };
        lines.push(IndexOutputLine {
            output_line: index + 1,
            text: text.to_owned(),
            semantic,
            body: body.map(|body| truncate_utf8(&body, limits.max_output_line_bytes).0),
            source_range,
        });
    }
    let semantic_truncation = raw.contains(TRUNCATED) || raw.contains(" more truncated]");
    Ok(BoundedSkeleton {
        skeleton,
        lines,
        truncated: output_truncated || semantic_truncation,
    })
}

fn line_metadata(
    line: &str,
    metadata: Option<&model::RawLineMetadata>,
) -> (IndexLineSemantic, Option<String>, Option<IndexSourceRange>) {
    if let Some(metadata) = metadata {
        let range = metadata.range.as_deref().and_then(parse_range);
        let semantic = match metadata.tag {
            Some("section") => IndexLineSemantic::Section,
            Some("dim") => IndexLineSemantic::Dimmed,
            _ if range.is_some() => IndexLineSemantic::Item,
            _ => IndexLineSemantic::Plain,
        };
        return (semantic, metadata.body.clone(), range);
    }
    if line.ends_with(TRUNCATED)
        || (line.trim_start().starts_with('[') && line.ends_with(" more truncated]"))
    {
        return (IndexLineSemantic::Dimmed, None, None);
    }
    if let Some((body, range)) = trailing_range(line) {
        let semantic = if body.trim_end().ends_with(':') && !line.starts_with(' ') {
            IndexLineSemantic::Section
        } else {
            IndexLineSemantic::Item
        };
        return (semantic, Some(body.to_owned()), Some(range));
    }
    if !line.starts_with(' ') && line.trim_end().ends_with(':') {
        return (IndexLineSemantic::Section, None, None);
    }
    (IndexLineSemantic::Plain, None, None)
}

fn trailing_range(line: &str) -> Option<(&str, IndexSourceRange)> {
    let separator = line.rfind(" [")?;
    let range = parse_range(&line[separator + 1..])?;
    Some((&line[..separator], range))
}

fn parse_range(value: &str) -> Option<IndexSourceRange> {
    let inner = value.strip_prefix('[')?.strip_suffix(']')?;
    let (start, end) = inner.split_once('-').unwrap_or((inner, inner));
    let start_line = start.parse().ok()?;
    let end_line = end.parse().ok()?;
    (start_line > 0 && end_line >= start_line).then_some(IndexSourceRange {
        start_line,
        end_line,
    })
}

fn bound_model_text(
    text: &str,
    max_line: usize,
    maximum: usize,
    token: Option<&CancellationToken>,
) -> Result<(String, bool), FilesystemError> {
    if text.is_empty() {
        return Ok((String::new(), false));
    }
    let mut output = String::new();
    let mut truncated = false;
    let mut total_truncated = false;
    let mut source_lines = text.split('\n').peekable();
    while let Some(line) = source_lines.next() {
        if token.is_some_and(CancellationToken::is_cancelled) {
            return Err(FilesystemError::Aborted);
        }
        let (line, line_truncated) = truncate_utf8(line, max_line);
        truncated |= line_truncated;
        let separator = usize::from(!output.is_empty());
        if output
            .len()
            .saturating_add(separator)
            .saturating_add(line.len())
            > maximum
        {
            truncated = true;
            total_truncated = true;
            break;
        }
        if separator == 1 {
            output.push('\n');
        }
        output.push_str(&line);
        if source_lines.peek().is_some() && output.len() == maximum {
            truncated = true;
            total_truncated = true;
            break;
        }
    }
    if total_truncated && !output.ends_with(TRUNCATED) {
        append_truncation_marker(&mut output, maximum);
    }
    Ok((output, truncated))
}

fn append_truncation_marker(output: &mut String, maximum: usize) {
    if output.ends_with(TRUNCATED) {
        return;
    }
    let reserved = TRUNCATED.len() + usize::from(!output.is_empty());
    if output.len().saturating_add(reserved) > maximum {
        let target = maximum.saturating_sub(reserved);
        output.truncate(floor_char_boundary(output, target));
        while output.ends_with('\n') {
            output.pop();
        }
    }
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(TRUNCATED);
}

fn truncate_utf8(value: &str, maximum: usize) -> (String, bool) {
    if value.len() <= maximum {
        return (value.to_owned(), false);
    }
    let target = maximum.saturating_sub(TRUNCATED.len());
    let boundary = floor_char_boundary(value, target);
    (format!("{}{TRUNCATED}", &value[..boundary]), true)
}

fn floor_char_boundary(value: &str, mut boundary: usize) -> usize {
    boundary = boundary.min(value.len());
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::mpsc, time::Duration};

    use tokio::sync::{Mutex as AsyncMutex, Semaphore};
    use tokio_util::sync::CancellationToken;

    use super::{
        IndexExecutionConfiguration, IndexInput, IndexLimits, Language, PARSER_SEMAPHORE,
        bound_model_text, parse_range, run_parser,
    };
    use crate::{FilesystemError, operations::FilesystemCore};

    static PARSER_TEST_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
    static QUEUED_TEST_SEMAPHORE: Semaphore = Semaphore::const_new(1);

    #[test]
    fn utf8_output_bounds_keep_valid_character_boundaries() {
        let (bounded, truncated) =
            bound_model_text("αβγδεζηθικ", 13, 50, None).expect("bounded output");
        assert!(truncated);
        assert_eq!(bounded, "α[truncated]");
    }

    #[test]
    fn total_output_bounds_always_include_a_truncation_marker() {
        let (bounded, truncated) =
            bound_model_text("123456789\nnext", 20, 12, None).expect("bounded output");

        assert!(truncated);
        assert_eq!(bounded, "[truncated]");
    }

    #[test]
    fn range_parser_accepts_single_and_inclusive_ranges() {
        assert_eq!(parse_range("[3]").unwrap().start_line, 3);
        assert_eq!(parse_range("[3-8]").unwrap().end_line, 8);
        assert!(parse_range("[8-3]").is_none());
    }

    #[test]
    fn index_limits_reject_cross_limit_inconsistencies() {
        let limits = IndexLimits {
            max_output_line_bytes: 51,
            max_model_output_bytes: 50,
            ..IndexLimits::default()
        };
        assert!(limits.validate().is_err());
    }

    #[tokio::test]
    async fn process_wide_parser_admission_has_exactly_two_permits() {
        let _test_guard = PARSER_TEST_LOCK.lock().await;
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("source.rs"), "fn source() {}\n").unwrap();
        let core = FilesystemCore::create(root.path(), false, None)
            .await
            .unwrap();
        let first = PARSER_SEMAPHORE.acquire().await.unwrap();
        let second = PARSER_SEMAPHORE.acquire().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(1), PARSER_SEMAPHORE.acquire())
                .await
                .is_err()
        );
        let error = core
            .index(
                IndexInput {
                    path: "source.rs".into(),
                },
                IndexExecutionConfiguration {
                    limits: IndexLimits {
                        admission_deadline_ms: 1,
                        ..IndexLimits::default()
                    },
                },
                &CancellationToken::new(),
            )
            .await
            .expect_err("admission bound");
        assert!(error.to_string().contains("admission deadline"));
        drop((first, second));
    }

    #[test]
    fn parser_deadline_covers_the_spawn_blocking_queue() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let permit = QUEUED_TEST_SEMAPHORE.acquire().await.expect("permit");
            let (started_sender, started_receiver) = mpsc::channel();
            let (release_sender, release_receiver) = mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                started_sender.send(()).expect("started");
                release_receiver.recv().expect("release");
            });
            started_receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("blocking worker started");

            let result = tokio::time::timeout(
                Duration::from_millis(250),
                run_parser(
                    "fn queued() {}\n".into(),
                    Language::Rust,
                    IndexLimits {
                        parser_deadline_ms: 10,
                        ..IndexLimits::default()
                    },
                    CancellationToken::new(),
                    permit,
                ),
            )
            .await
            .expect("caller did not wait for the blocking queue");
            let Err(result) = result else {
                panic!("queued parser succeeded")
            };
            assert!(result.to_string().contains("deadline"), "{result}");
            assert_eq!(QUEUED_TEST_SEMAPHORE.available_permits(), 1);

            release_sender.send(()).expect("release blocker");
            blocker.await.expect("blocking worker");
        });
    }

    #[test]
    fn parser_cancellation_releases_a_queued_permit_for_the_next_index() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let _test_guard = PARSER_TEST_LOCK.lock().await;
            let root = tempfile::tempdir().expect("root");
            fs::write(root.path().join("source.rs"), "fn source() {}\n").expect("source");
            let core = FilesystemCore::create(root.path(), false, None)
                .await
                .expect("core");
            let held_permit = PARSER_SEMAPHORE.acquire().await.expect("held permit");
            let queued_permit = PARSER_SEMAPHORE.acquire().await.expect("queued permit");
            let (started_sender, started_receiver) = mpsc::channel();
            let (release_sender, release_receiver) = mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                started_sender.send(()).expect("started");
                release_receiver.recv().expect("release");
            });
            started_receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("blocking worker started");

            let cancellation = CancellationToken::new();
            let queued = run_parser(
                "fn queued() {}\n".into(),
                Language::Rust,
                IndexLimits::default(),
                cancellation.clone(),
                queued_permit,
            );
            tokio::pin!(queued);
            tokio::select! {
                biased;
                _ = &mut queued => panic!("queued parser completed while blocking worker was held"),
                () = tokio::task::yield_now() => {}
            }
            cancellation.cancel();
            let result = tokio::time::timeout(Duration::from_millis(250), &mut queued)
                .await
                .expect("cancelled caller waited for the blocking queue");
            assert!(matches!(result, Err(FilesystemError::Aborted)));
            assert_eq!(PARSER_SEMAPHORE.available_permits(), 1);

            release_sender.send(()).expect("release blocker");
            blocker.await.expect("blocking worker");
            let output = tokio::time::timeout(
                Duration::from_secs(1),
                core.index(
                    IndexInput {
                        path: "source.rs".into(),
                    },
                    IndexExecutionConfiguration::default(),
                    &CancellationToken::new(),
                ),
            )
            .await
            .expect("subsequent index admission")
            .expect("subsequent index");
            assert!(matches!(output, super::IndexOutput::File { .. }));
            drop(held_permit);
        });
    }
}
