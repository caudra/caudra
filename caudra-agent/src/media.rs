//! Moving the images and PDFs that enter a session into the media store, so a
//! saved session holds references, and loading their bytes back before a
//! request needs them.

use caudra_providers::{ContentBlock, DocumentSource, ImageSource, Message};
use caudra_storage::media::{MediaHash, MediaStore};
use caudra_storage::tool_outputs::ToolOutputStore;
use tracing::warn;

pub(crate) fn store_for(outputs: Option<&ToolOutputStore>) -> Option<MediaStore> {
    outputs.map(|outputs| MediaStore::new(outputs.state_dir()))
}

/// Stores `image`, leaving it inline when the store refuses it.
pub(crate) fn store_image(image: &mut ImageSource, store: &MediaStore) {
    if let Err(error) = image.store_in(store) {
        warn!(media_type = image.media_type.mime(), %error, "image kept inline");
    }
}

/// Stores `document`, leaving it host-only when the store refuses it.
pub(crate) fn store_document(document: &mut DocumentSource, store: &MediaStore) {
    if let Err(error) = document.store_in(store) {
        warn!(url = %document.url, %error, "PDF not stored");
    }
}

/// Stores every image off the executor, since each put fsyncs.
pub(crate) async fn stored_images(
    mut images: Vec<ImageSource>,
    store: MediaStore,
) -> Vec<ImageSource> {
    if images.iter().all(|image| image.stored().is_some()) {
        return images;
    }
    smol::unblock(move || {
        for image in &mut images {
            store_image(image, &store);
        }
        images
    })
    .await
}

/// Stores every image the messages carry, as [`stored_images`] does.
pub(crate) async fn stored_message_images(
    mut messages: Vec<Message>,
    store: MediaStore,
) -> Vec<Message> {
    let unstored = messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|block| matches!(block, ContentBlock::Image { source } if source.stored().is_none()));
    if !unstored {
        return messages;
    }
    smol::unblock(move || {
        for block in messages.iter_mut().flat_map(|message| &mut message.content) {
            if let ContentBlock::Image { source } = block {
                store_image(source, &store);
            }
        }
        messages
    })
    .await
}

/// The references in `messages` whose bytes are not loaded.
fn unloaded_media(messages: &[Message]) -> Vec<MediaHash> {
    let images = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Image { source } if source.data().is_none() => source.stored(),
            _ => None,
        });
    let documents = messages
        .iter()
        .flat_map(|message| message.tool_result_documents.values().flatten())
        .filter(|document| document.data.is_none())
        .filter_map(|document| document.stored.as_ref());
    images
        .chain(documents)
        .map(|stored| stored.sha256.clone())
        .collect()
}

/// Reads `hashes` into the store's cache off the executor, so loading them
/// into history afterwards does not block on disk.
async fn warm(store: &MediaStore, hashes: Vec<MediaHash>) {
    let store = store.clone();
    smol::unblock(move || {
        for hash in &hashes {
            if let Err(error) = store.base64(hash) {
                warn!(sha256 = %hash, %error, "stored media unreadable");
            }
        }
    })
    .await;
}

/// Fills in the bytes of every reference in `messages` the store still has,
/// reading the files off the executor first, so a request built from them
/// carries what a restored session holds only references to.
pub async fn load_stored_media(messages: &mut [Message], store: &MediaStore) {
    let unloaded = unloaded_media(messages);
    if unloaded.is_empty() {
        return;
    }
    warm(store, unloaded).await;
    load_media(messages, store);
}

/// Fills in the bytes of every reference in `messages` the store still has.
fn load_media(messages: &mut [Message], store: &MediaStore) {
    for message in messages {
        for block in &mut message.content {
            if let ContentBlock::Image { source } = block {
                source.load_from(store);
            }
        }
        for document in message.tool_result_documents.values_mut().flatten() {
            document.load_from(store);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use caudra_providers::{ImageMediaType, Role};
    use caudra_storage::StateDir;
    use tempfile::TempDir;

    use super::*;

    const PIXEL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

    fn store() -> (TempDir, MediaStore) {
        let temp = TempDir::new().unwrap();
        let store = MediaStore::new(&StateDir::from_path(temp.path().to_path_buf()));
        (temp, store)
    }

    fn restored(store: &MediaStore) -> Message {
        let images = smol::block_on(stored_images(
            vec![ImageSource::new(ImageMediaType::Png, Arc::from(PIXEL_PNG))],
            store.clone(),
        ));
        let saved =
            serde_json::to_string(&Message::user_with_images(String::new(), images)).unwrap();
        assert!(!saved.contains(PIXEL_PNG));
        serde_json::from_str(&saved).unwrap()
    }

    fn image(message: &Message) -> &ImageSource {
        message
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::Image { source } => Some(source),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn a_restored_image_loads_its_bytes_back_from_the_store() {
        let (_temp, store) = store();
        let mut messages = vec![restored(&store)];
        assert_eq!(unloaded_media(&messages).len(), 1);

        smol::block_on(load_stored_media(&mut messages, &store));

        assert!(matches!(messages[0].role, Role::User));
        assert_eq!(
            image(&messages[0]).data().map(|data| &**data),
            Some(PIXEL_PNG)
        );
        assert!(unloaded_media(&messages).is_empty());
    }

    #[test]
    fn an_image_whose_file_is_gone_stays_unloaded_and_says_so() {
        let (_temp, store) = store();
        let mut messages = vec![restored(&store)];
        let hash = image(&messages[0]).stored().unwrap().sha256.clone();
        fs::remove_file(store.path(&hash)).unwrap();

        smol::block_on(load_stored_media(&mut messages, &store));

        let source = image(&messages[0]);
        assert!(source.data().is_none());
        assert!(source.unavailable_note().contains(hash.as_str()));
    }
}
