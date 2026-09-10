use std::collections::HashSet;

use tokio_util::sync::CancellationToken;

use crate::{
    FilesystemError,
    diff::{file_diff, file_diff_hunks, shorten_patch},
    group::{MCP_RAW_RESULT_CEILING_BYTES, mcp_response_size},
    operations::FilesystemCore,
    patch::{PatchHunk, apply_update_chunks, parse_patch},
    text::{check_cancelled, enforce_bytes, exists, read_text_snapshot_required},
    types::{FileApplyPatchInput, FileApplyPatchOutput, FileMutation, FilePatchKind},
};

use super::{PlannedChange, PlannedChangeType, patch_publication::publish_patch, path_string};

impl FilesystemCore {
    pub(crate) async fn file_apply_patch(
        &self,
        input: FileApplyPatchInput,
        token: &CancellationToken,
    ) -> Result<FileApplyPatchOutput, FilesystemError> {
        let _guard = self.mutation.lock().await;
        check_cancelled(token)?;
        enforce_bytes("patchText", &input.patch_text, self.limits.max_patch_bytes)?;
        let changes = self.plan_patch(&input.patch_text, token).await?;
        self.require_write()?;
        // The exact text + structured MCP response shape, including a
        // conservative envelope, fits the MCP raw result ceiling
        // before the first file is published.
        let output = self.fit_patch_output(self.patch_output(&changes, true)?)?;
        publish_patch(self, &changes, token).await?;
        Ok(output)
    }

    pub(crate) async fn prepare_patch(
        &self,
        patch_text: &str,
        token: &CancellationToken,
    ) -> Result<(Vec<PlannedChange>, FileApplyPatchOutput), FilesystemError> {
        check_cancelled(token)?;
        enforce_bytes("patchText", patch_text, self.limits.max_patch_bytes)?;
        let changes = self.plan_patch(patch_text, token).await?;
        let output = self.fit_patch_output(self.patch_output(&changes, false)?)?;
        Ok((changes, output))
    }

    pub(crate) async fn publish_prepared_patch(
        &self,
        changes: &[PlannedChange],
        token: &CancellationToken,
    ) -> Result<(), FilesystemError> {
        self.require_write()?;
        publish_patch(self, changes, token).await
    }

    fn patch_output(
        &self,
        changes: &[PlannedChange],
        applied: bool,
    ) -> Result<FileApplyPatchOutput, FilesystemError> {
        let files = changes
            .iter()
            .map(|change| {
                let target = change.move_path.as_deref().unwrap_or(&change.file_path);
                Ok(FileMutation {
                    file_path: path_string(&change.file_path),
                    relative_path: self.policy.relative(target)?,
                    mutation_type: change.change_type.into(),
                    patch: change.diff.patch.clone(),
                    additions: change.diff.additions,
                    deletions: change.diff.deletions,
                    truncated: change.diff.truncated,
                    move_path: change.move_path.as_deref().map(path_string),
                })
            })
            .collect::<Result<Vec<_>, FilesystemError>>()?;
        Ok(assemble(files, applied))
    }

    fn fit_patch_output(
        &self,
        output: FileApplyPatchOutput,
    ) -> Result<FileApplyPatchOutput, FilesystemError> {
        // A configured budget can tighten the protocol ceiling but never loosen
        // it, so the smaller of the two is the target.
        fit(
            output,
            self.limits
                .max_patch_result_bytes
                .min(MCP_RAW_RESULT_CEILING_BYTES),
        )
    }

    async fn plan_patch(
        &self,
        patch_text: &str,
        token: &CancellationToken,
    ) -> Result<Vec<PlannedChange>, FilesystemError> {
        let hunks = parse_patch(patch_text)?;
        if hunks.len() > self.limits.max_patch_files {
            return Err(FilesystemError::message(format!(
                "Patch exceeds maximum of {} file sections",
                self.limits.max_patch_files
            )));
        }
        let mut used = HashSet::new();
        let mut changes = Vec::new();
        let mut budget = PatchPlanBudget::new(self.limits.max_patch_plan_bytes);
        for hunk in hunks {
            check_cancelled(token)?;
            let hunk_path = hunk.path().to_owned();
            let file_path = self.policy.resolve(&hunk_path).await?;
            if !used.insert(file_path.clone()) {
                return Err(FilesystemError::message(format!(
                    "Patch references a path more than once: {hunk_path}"
                )));
            }
            match hunk {
                PatchHunk::Add { contents, .. } => {
                    if exists(&file_path).await {
                        return Err(FilesystemError::message(format!(
                            "Cannot add existing file: {hunk_path}"
                        )));
                    }
                    enforce_bytes("added file", &contents, self.limits.max_write_bytes)?;
                    budget.ensure_peak(0, contents.len(), 0)?;
                    let diff = file_diff(
                        &self.policy,
                        &file_path,
                        "",
                        &contents,
                        None,
                        self.limits.max_diff_bytes,
                    )?;
                    budget.push(
                        &mut changes,
                        PlannedChange {
                            diff,
                            file_path,
                            new_content: contents,
                            change_type: PlannedChangeType::Add,
                            move_path: None,
                            expected_source: None,
                        },
                        0,
                    )?;
                }
                PatchHunk::Delete { .. } => {
                    let snapshot =
                        read_text_snapshot_required(&file_path, self.limits.max_file_bytes, token)
                            .await?;
                    let old_bytes = snapshot.content.len();
                    budget.ensure_peak(old_bytes, 0, 0)?;
                    let diff = file_diff(
                        &self.policy,
                        &file_path,
                        &snapshot.content,
                        "",
                        None,
                        self.limits.max_diff_bytes,
                    )?;
                    budget.push(
                        &mut changes,
                        PlannedChange {
                            diff,
                            file_path,
                            new_content: String::new(),
                            change_type: PlannedChangeType::Delete,
                            move_path: None,
                            expected_source: Some(snapshot.version),
                        },
                        old_bytes,
                    )?;
                }
                PatchHunk::Update {
                    move_path, chunks, ..
                } => {
                    let snapshot =
                        read_text_snapshot_required(&file_path, self.limits.max_file_bytes, token)
                            .await?;
                    let old_bytes = snapshot.content.len();
                    budget.ensure_peak(old_bytes, 0, 0)?;
                    let (new_content, spans) =
                        apply_update_chunks(&hunk_path, &chunks, &snapshot.content)?;
                    enforce_bytes("patched file", &new_content, self.limits.max_write_bytes)?;
                    budget.ensure_peak(old_bytes, new_content.len(), 0)?;
                    let move_path = match move_path {
                        Some(path) => Some((path.clone(), self.policy.resolve(&path).await?)),
                        None => None,
                    };
                    if let Some((requested_move, target)) = &move_path {
                        if used.contains(target) {
                            return Err(FilesystemError::message(format!(
                                "Patch target conflicts with another path: {requested_move}"
                            )));
                        }
                        if exists(target).await {
                            return Err(FilesystemError::message(format!(
                                "Cannot move over existing file: {requested_move}"
                            )));
                        }
                        used.insert(target.clone());
                    }
                    let target = move_path.as_ref().map(|(_, target)| target.as_path());
                    let is_move = target.is_some();
                    let diff = file_diff_hunks(
                        &self.policy,
                        &file_path,
                        &snapshot.content,
                        &new_content,
                        &spans,
                        target,
                        self.limits.max_diff_bytes,
                    )?;
                    budget.push(
                        &mut changes,
                        PlannedChange {
                            diff,
                            file_path,
                            new_content,
                            change_type: if is_move {
                                PlannedChangeType::Move
                            } else {
                                PlannedChangeType::Update
                            },
                            move_path: move_path.map(|(_, target)| target),
                            expected_source: Some(snapshot.version),
                        },
                        old_bytes,
                    )?;
                }
            }
        }
        Ok(changes)
    }
}

/// Shorten the receipt rather than refuse a change that is otherwise valid. The
/// allowance is shared, so no file loses its preview while another keeps a full
/// one, and every file keeps its row.
fn fit(
    output: FileApplyPatchOutput,
    limit: usize,
) -> Result<FileApplyPatchOutput, FilesystemError> {
    if measure(&output)? <= limit {
        return Ok(output);
    }
    // Envelope cost is not linear in the diff alone: paths, counts and JSON
    // escaping all contribute, so search the allowance rather than compute it.
    // Shortening is monotone in the allowance, so the search converges.
    let mut lower = 0usize;
    let mut upper = output
        .files
        .iter()
        .map(|file| file.patch.len())
        .max()
        .unwrap_or(0);
    while lower < upper {
        let candidate = lower + (upper - lower).div_ceil(2);
        if measure(&shorten(&output, candidate))? <= limit {
            lower = candidate;
        } else {
            upper = candidate - 1;
        }
    }
    let fitted = shorten(&output, lower);
    if measure(&fitted)? > limit {
        // Every preview is already gone and the receipt still does not fit, so
        // no representable result exists. The caller learns that before any
        // file is published rather than after.
        return Err(FilesystemError::message(format!(
            "Patch result exceeds maximum size of {limit} bytes"
        )));
    }
    Ok(fitted)
}

/// The combined `diff` is derived, never stored twice on the wire, so it is
/// rebuilt from the per-file previews every time those change.
fn assemble(files: Vec<FileMutation>, applied: bool) -> FileApplyPatchOutput {
    FileApplyPatchOutput {
        kind: FilePatchKind::Patch,
        applied,
        diff: files
            .iter()
            .map(|file| file.patch.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        truncated: files.iter().any(|file| file.truncated),
        files,
    }
}

fn shorten(output: &FileApplyPatchOutput, allowance: usize) -> FileApplyPatchOutput {
    let files = output
        .files
        .iter()
        .map(|file| match shorten_patch(&file.patch, allowance) {
            Some(patch) => FileMutation {
                patch,
                truncated: true,
                ..file.clone()
            },
            None => file.clone(),
        })
        .collect();
    assemble(files, output.applied)
}

fn measure(output: &FileApplyPatchOutput) -> Result<usize, FilesystemError> {
    mcp_response_size(output).map_err(|_| FilesystemError::message("Cannot serialize patch result"))
}

struct PatchPlanBudget {
    // Per-file limits must not multiply into hundreds of MiB retained by a
    // multi-file plan. Previous new content and diffs remain charged while the
    // current old/new/diff material is measured as the candidate peak.
    retained_bytes: usize,
    maximum_bytes: usize,
}

impl PatchPlanBudget {
    fn new(maximum_bytes: usize) -> Self {
        Self {
            retained_bytes: 0,
            maximum_bytes,
        }
    }

    fn ensure_peak(
        &self,
        old_content_bytes: usize,
        new_content_bytes: usize,
        diff_bytes: usize,
    ) -> Result<(), FilesystemError> {
        let measured = self
            .retained_bytes
            .checked_add(old_content_bytes)
            .and_then(|value| value.checked_add(new_content_bytes))
            .and_then(|value| value.checked_add(diff_bytes))
            .unwrap_or(usize::MAX);
        if measured > self.maximum_bytes {
            return Err(FilesystemError::message(format!(
                "Patch plan exceeds maximum content budget of {} bytes",
                self.maximum_bytes
            )));
        }
        Ok(())
    }

    fn push(
        &mut self,
        changes: &mut Vec<PlannedChange>,
        change: PlannedChange,
        old_content_bytes: usize,
    ) -> Result<(), FilesystemError> {
        let new_content_bytes = change.new_content.len();
        let diff_bytes = change.diff.patch.len();
        self.ensure_peak(old_content_bytes, new_content_bytes, diff_bytes)?;
        self.retained_bytes = self
            .retained_bytes
            .checked_add(new_content_bytes)
            .and_then(|value| value.checked_add(diff_bytes))
            .ok_or_else(|| FilesystemError::message("Patch plan content budget overflow"))?;
        changes.push(change);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{MCP_RAW_RESULT_CEILING_BYTES, assemble, fit, measure};
    use crate::types::{FileMutation, FileMutationType};

    fn receipt(files: usize, preview_bytes: usize, path_bytes: usize) -> Vec<FileMutation> {
        (0..files)
            .map(|index| {
                let name = format!("{index:0>width$}", width = path_bytes);
                FileMutation {
                    file_path: format!("/root/{name}"),
                    relative_path: name,
                    mutation_type: FileMutationType::Update,
                    patch: "-old\n+new\n".repeat(preview_bytes / 10),
                    additions: 1,
                    deletions: 1,
                    truncated: false,
                    move_path: None,
                }
            })
            .collect()
    }

    #[test]
    fn a_receipt_within_the_ceiling_is_returned_untouched() {
        let output = assemble(receipt(2, 100, 8), true);
        let fitted = fit(output.clone(), MCP_RAW_RESULT_CEILING_BYTES).expect("fits");
        assert_eq!(fitted, output);
        assert!(!fitted.truncated);
    }

    #[test]
    fn an_oversized_receipt_is_shortened_until_it_fits_and_keeps_every_file() {
        let output = assemble(receipt(4, 40_000, 8), true);
        assert!(measure(&output).expect("size") > MCP_RAW_RESULT_CEILING_BYTES);

        let fitted =
            fit(output, MCP_RAW_RESULT_CEILING_BYTES).expect("shortened rather than failed");

        assert!(measure(&fitted).expect("size") <= MCP_RAW_RESULT_CEILING_BYTES);
        assert_eq!(fitted.files.len(), 4);
        assert!(fitted.truncated);
        assert!(fitted.files.iter().all(|file| file.truncated));
    }

    #[test]
    fn the_allowance_is_shared_so_no_file_keeps_a_full_preview_while_another_has_none() {
        let mut files = receipt(4, 40_000, 8);
        files[0].patch = "-only\n".to_owned();
        let fitted = fit(assemble(files, true), MCP_RAW_RESULT_CEILING_BYTES).expect("shortened");

        // The short preview is under the allowance and survives whole; the rest
        // are cut to one shared bound rather than dropped tail-first.
        assert!(measure(&fitted).expect("size") <= MCP_RAW_RESULT_CEILING_BYTES);
        assert_eq!(fitted.files[0].patch, "-only\n");
        assert!(!fitted.files[0].truncated);
        let lengths = fitted.files[1..]
            .iter()
            .map(|file| file.patch.len())
            .collect::<Vec<_>>();
        assert!(lengths.iter().all(|length| *length == lengths[0]));
        assert!(lengths[0] > 0 && lengths[0] < 40_000);
    }

    #[test]
    fn a_receipt_that_cannot_fit_without_previews_is_refused() {
        // Every preview removed still leaves the file rows, so a budget smaller
        // than those rows has no representable result at all.
        let error = fit(assemble(receipt(8, 40_000, 400), true), 1_000)
            .expect_err("no allowance can represent this receipt");
        assert!(
            error
                .to_string()
                .contains("Patch result exceeds maximum size of 1000 bytes")
        );
    }

    #[test]
    fn shortening_is_deterministic() {
        let output = assemble(receipt(5, 30_000, 12), true);
        let first = fit(output.clone(), MCP_RAW_RESULT_CEILING_BYTES).expect("shortened");
        let second = fit(output, MCP_RAW_RESULT_CEILING_BYTES).expect("shortened");
        assert!(first.truncated);
        assert_eq!(first, second);
    }
}
