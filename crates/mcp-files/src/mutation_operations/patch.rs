use std::collections::HashSet;

use tokio_util::sync::CancellationToken;

use crate::{
    FilesystemError,
    diff::file_diff,
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
        let applied = self.should_apply(input.dry_run)?;
        let output = self.patch_output(&changes, applied)?;
        self.validate_patch_output(&output)?;
        // The exact text + structured MCP response shape, including a
        // conservative envelope, fits the MCP raw result ceiling
        // before the first file is published.
        if applied {
            publish_patch(self, &changes, token).await?;
        }
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
        let output = self.patch_output(&changes, false)?;
        self.validate_patch_output(&output)?;
        Ok((changes, output))
    }

    pub(crate) async fn publish_prepared_patch(
        &self,
        changes: &[PlannedChange],
        token: &CancellationToken,
    ) -> Result<(), FilesystemError> {
        self.should_apply(None)?;
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
        Ok(FileApplyPatchOutput {
            kind: FilePatchKind::Patch,
            applied,
            diff: files
                .iter()
                .map(|file| file.patch.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            truncated: files.iter().any(|file| file.truncated),
            files,
        })
    }

    fn validate_patch_output(&self, output: &FileApplyPatchOutput) -> Result<(), FilesystemError> {
        let output_size = mcp_response_size(output)
            .map_err(|_| FilesystemError::message("Cannot serialize patch result"))?;
        let output_limit = self
            .limits
            .max_patch_result_bytes
            .min(MCP_RAW_RESULT_CEILING_BYTES);
        if output_size > output_limit {
            return Err(FilesystemError::message(format!(
                "Patch result exceeds maximum size of {} bytes",
                output_limit
            )));
        }
        Ok(())
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
                    let new_content = apply_update_chunks(&hunk_path, &chunks, &snapshot.content)?;
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
                    let diff = file_diff(
                        &self.policy,
                        &file_path,
                        &snapshot.content,
                        &new_content,
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
