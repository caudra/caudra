//! Rules shared by the composer's sigils.
//!
//! `@path` and `#hash` differ in what they name and nothing else: both start a
//! token only where prose cannot already be running, both shed the punctuation
//! a sentence leaves behind them, and both cap how many candidates one scan
//! will weigh. Stating that once is what keeps the two from drifting apart.

pub(crate) const TRAILING_PUNCTUATION: [char; 9] = ['.', ',', ';', ':', '!', '?', ')', ']', '}'];
/// Characters a sigil may follow, alongside whitespace and the start of the
/// text. Excluding everything else is what keeps `user@host` and `HEAD@{1}`
/// from ever reaching a resolution check.
const OPENING_DELIMITERS: [char; 3] = ['(', '[', '{'];
/// How many candidates one scan will resolve. A prompt full of sigils must not
/// turn one keystroke or one mouse move into a burst of work.
pub(crate) const MAX_CANDIDATES: usize = 32;

/// Whether a sigil may open a token immediately after `character`.
pub(crate) fn opens_after(character: char) -> bool {
    character.is_whitespace() || OPENING_DELIMITERS.contains(&character)
}
