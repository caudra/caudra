use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{IdentifierError, identity::validate_identifier};

const MAX_ATTACHMENT_NAME_BYTES: usize = 255;

fn validate_local_id(value: &str) -> Result<(), IdentifierError> {
    validate_identifier(value)
}

macro_rules! local_ref {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdentifierError> {
                let value = value.into();
                validate_local_id(&value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_tuple(stringify!($name))
                    .field(&"<opaque>")
                    .finish()
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdentifierError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

local_ref!(
    ClientAttachmentId,
    "Opaque identity for content attached by the client rather than resolved from a workspace."
);
local_ref!(
    PlanRef,
    "Opaque reference into the client-owned plan store."
);
local_ref!(
    MemoryRef,
    "Opaque reference into the client-owned memory store."
);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "reference", rename_all = "snake_case")]
/// Typed reference to a local document; persistence belongs to the caller, not this crate.
pub enum LocalDocumentRef {
    Plan(PlanRef),
    Memory(MemoryRef),
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "content", rename_all = "snake_case")]
pub enum ClientAttachmentContent {
    Text(String),
    Bytes(Vec<u8>),
}

impl fmt::Debug for ClientAttachmentContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, size) = match self {
            Self::Text(text) => ("text", text.len()),
            Self::Bytes(bytes) => ("bytes", bytes.len()),
        };
        formatter
            .debug_struct("ClientAttachmentContent")
            .field("kind", &kind)
            .field("size_bytes", &size)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Client-owned attachment payload kept distinct from authority-owned workspace resources.
pub struct ClientAttachment {
    pub id: ClientAttachmentId,
    pub name: String,
    pub media_type: Option<String>,
    pub content: ClientAttachmentContent,
}

impl ClientAttachment {
    pub fn new(
        id: ClientAttachmentId,
        name: impl Into<String>,
        media_type: Option<String>,
        content: ClientAttachmentContent,
    ) -> Result<Self, IdentifierError> {
        let name = name.into();
        if name.is_empty() {
            return Err(IdentifierError::Empty);
        }
        if name.len() > MAX_ATTACHMENT_NAME_BYTES {
            return Err(IdentifierError::TooLong);
        }
        if name.chars().any(char::is_control) {
            return Err(IdentifierError::ControlCharacter);
        }
        Ok(Self {
            id,
            name,
            media_type,
            content,
        })
    }
}

impl fmt::Debug for ClientAttachment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientAttachment")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("media_type", &self.media_type)
            .field("content", &self.content)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClientAttachment, ClientAttachmentContent, ClientAttachmentId, LocalDocumentRef, MemoryRef,
        PlanRef,
    };

    const ATTACHMENT_SECRET: &str = "private attachment contents";
    const PLAN_ID: &str = "plan-1";
    const MEMORY_ID: &str = "memory-1";

    #[test]
    fn local_document_references_remain_typed() {
        let plan = LocalDocumentRef::Plan(PlanRef::new(PLAN_ID).expect("valid plan ref"));
        let memory = LocalDocumentRef::Memory(MemoryRef::new(MEMORY_ID).expect("valid memory ref"));

        assert_ne!(plan, memory);
        assert_eq!(
            serde_json::to_string(&plan).expect("serialize plan ref"),
            r#"{"kind":"plan","reference":"plan-1"}"#
        );
    }

    #[test]
    fn attachment_debug_output_redacts_payload() {
        let attachment = ClientAttachment::new(
            ClientAttachmentId::new("attachment-1").expect("valid attachment id"),
            "notes.txt",
            Some("text/plain".to_owned()),
            ClientAttachmentContent::Text(ATTACHMENT_SECRET.to_owned()),
        )
        .expect("valid attachment");
        let debug = format!("{attachment:?}");

        assert!(!debug.contains(ATTACHMENT_SECRET));
        assert!(debug.contains("size_bytes"));
    }

    #[test]
    fn client_image_attachment_round_trip_remains_client_owned_bytes() {
        let attachment = ClientAttachment::new(
            ClientAttachmentId::new("image-attachment").expect("valid attachment id"),
            "image.png",
            Some("image/png".to_owned()),
            ClientAttachmentContent::Bytes(vec![1, 2, 3]),
        )
        .expect("valid attachment");

        let encoded = serde_json::to_string(&attachment).expect("serialize attachment");
        let decoded: ClientAttachment =
            serde_json::from_str(&encoded).expect("deserialize attachment");

        assert_eq!(decoded, attachment);
        assert!(matches!(
            decoded.content,
            ClientAttachmentContent::Bytes(bytes) if bytes == [1, 2, 3]
        ));
    }
}
