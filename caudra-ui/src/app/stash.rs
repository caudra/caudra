use caudra_agent::ImageSource;
use caudra_providers::ImageMediaType;
use caudra_storage::prompt_stash::{PromptStash, PromptStashError, StashDraft, StashEntry};
use caudra_storage::sessions::{StoredImage, StoredPasteRange};

use crate::components::Action;
use crate::components::input::InputState;
use crate::components::stash_picker::StashPickerAction;
use crate::input_document::InputDraft;

use super::App;

const NOTHING_TO_STASH: &str = "Nothing to stash";
const STASH_EMPTY: &str = "Stash is empty";
const COMPOSER_BUSY: &str = "Composer already holds a draft. Stash it first.";
const QUEUE_EDIT_ACTIVE: &str = "Finish editing the queued prompt first";
const ENTRY_GONE: &str = "Stash entry is already gone";

impl App {
    /// Opened per operation rather than held: stashing is rare, the file is
    /// small, and another Caudra process may have written to it since the last
    /// time this one looked.
    fn stash(&self) -> Result<PromptStash, PromptStashError> {
        PromptStash::open(&self.storage)
    }

    pub(super) fn stash_push(&mut self) -> Vec<Action> {
        if !self.composer_is_own_draft() {
            return Vec::new();
        }
        if self.active_input_box().is_empty() {
            self.flash(NOTHING_TO_STASH.into());
            return Vec::new();
        }
        let cwd = self.state.session.cwd.clone();
        let mut stash = match self.stash() {
            Ok(stash) => stash,
            Err(error) => return self.report(error),
        };

        let state = self.active_input_box_mut().take_state();
        let (draft, images) = state.into_parts();
        if let Err(error) = stash.push(to_stash_draft(draft, images, cwd)) {
            return self.report(error);
        }

        self.command_palette.close();
        self.flash(format!("Stashed ({} total)", stash.len()));
        Vec::new()
    }

    pub(super) fn stash_pop(&mut self) -> Vec<Action> {
        if !self.composer_ready_for_restore() {
            return Vec::new();
        }
        let mut stash = match self.stash() {
            Ok(stash) => stash,
            Err(error) => return self.report(error),
        };
        match stash.pop() {
            Ok(Some(entry)) => self.restore(entry),
            Ok(None) => self.flash(STASH_EMPTY.into()),
            Err(error) => return self.report(error),
        }
        Vec::new()
    }

    pub(super) fn stash_list(&mut self) -> Vec<Action> {
        let stash = match self.stash() {
            Ok(stash) => stash,
            Err(error) => return self.report(error),
        };
        if stash.is_empty() {
            self.flash(STASH_EMPTY.into());
            return Vec::new();
        }
        self.stash_picker
            .open(stash.entries().to_vec(), caudra_storage::now_epoch());
        Vec::new()
    }

    pub(super) fn handle_stash_picker_action(&mut self, action: StashPickerAction) -> Vec<Action> {
        match action {
            StashPickerAction::Consumed | StashPickerAction::Closed => {}
            StashPickerAction::Restore(entry) => {
                if !self.composer_ready_for_restore() {
                    return Vec::new();
                }
                let mut stash = match self.stash() {
                    Ok(stash) => stash,
                    Err(error) => return self.report(error),
                };
                match stash.remove(&entry.id) {
                    Ok(Some(entry)) => self.restore(entry),
                    Ok(None) => self.flash(ENTRY_GONE.into()),
                    Err(error) => return self.report(error),
                }
            }
            StashPickerAction::Delete(id) => {
                let mut stash = match self.stash() {
                    Ok(stash) => stash,
                    Err(error) => return self.report(error),
                };
                if let Err(error) = stash.remove(&id) {
                    return self.report(error);
                }
                if stash.is_empty() {
                    self.stash_picker.close();
                    self.flash(STASH_EMPTY.into());
                } else {
                    self.stash_picker
                        .open(stash.entries().to_vec(), caudra_storage::now_epoch());
                }
            }
        }
        Vec::new()
    }

    /// Restoring overwrites the composer, so a draft already sitting there has
    /// to be dealt with first rather than silently replaced.
    fn composer_ready_for_restore(&mut self) -> bool {
        if !self.composer_is_own_draft() {
            return false;
        }
        if self.active_input_box().is_empty() {
            return true;
        }
        self.flash(COMPOSER_BUSY.into());
        false
    }

    /// While a queued prompt is being edited the composer belongs to the queue,
    /// and `QueueEditor` restores the real draft when the edit ends. Moving
    /// text in or out from under it would strand both.
    fn composer_is_own_draft(&mut self) -> bool {
        if !self.queue_editor_active() {
            return true;
        }
        self.flash(QUEUE_EDIT_ACTIVE.into());
        false
    }

    fn restore(&mut self, entry: StashEntry) {
        let images = entry
            .images
            .into_iter()
            .filter_map(|image| match ImageMediaType::from_mime(&image.media_type) {
                Some(media_type) => Some(ImageSource::new(media_type, image.data.into())),
                None => {
                    tracing::warn!(media_type = %image.media_type, "dropping stashed image");
                    None
                }
            })
            .collect();
        let draft = InputDraft {
            text: entry.text,
            paste_ranges: entry
                .paste_ranges
                .into_iter()
                .map(|range| range.start..range.end)
                .collect(),
        };
        self.active_input_box_mut()
            .set_state(InputState::new(draft, images));
        if self.is_main_chat() {
            let palette_text = self.input_box.palette_text();
            self.command_palette.sync(&palette_text);
        }
    }

    fn report(&mut self, error: PromptStashError) -> Vec<Action> {
        tracing::warn!(error = %error, "prompt stash unavailable");
        self.flash(format!("Stash unavailable: {error}"));
        Vec::new()
    }
}

fn to_stash_draft(draft: InputDraft, images: Vec<ImageSource>, cwd: String) -> StashDraft {
    StashDraft {
        text: draft.text,
        paste_ranges: draft
            .paste_ranges
            .into_iter()
            .map(|range| StoredPasteRange {
                start: range.start,
                end: range.end,
            })
            .collect(),
        images: images
            .into_iter()
            .map(|image| StoredImage {
                media_type: image.media_type.mime().into(),
                data: image.data.to_string(),
            })
            .collect(),
        cwd,
    }
}
