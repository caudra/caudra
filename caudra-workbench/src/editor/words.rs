//! What counts as one word. Word boundaries answer the caret motions and the
//! double click; the delete chords cut path components instead, as
//! [`caudra_workspace::path_components`] reads them.

/// What counts as one word to word motion and to a double click, so the two
/// never disagree about where a word ends.
pub(crate) fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}
