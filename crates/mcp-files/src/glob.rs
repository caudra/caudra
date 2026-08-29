use crate::{FilesystemError, FilesystemLimits};

#[derive(Clone, Copy, Debug)]
enum Token {
    RecursiveDirectories,
    Recursive,
    SegmentStar,
    Any,
    Literal(u16),
}

#[derive(Debug)]
pub(crate) struct GlobMatcher {
    alternatives: Vec<Vec<Token>>,
    fast_path: Option<FastPath>,
}

#[derive(Debug)]
enum FastPath {
    Literal(String),
    RecursiveLiteralSuffix(String),
}

impl GlobMatcher {
    pub(crate) fn new(pattern: &str, limits: &FilesystemLimits) -> Result<Self, FilesystemError> {
        if pattern.len() > limits.max_glob_bytes {
            return Err(FilesystemError::message(format!(
                "glob pattern exceeds maximum size of {} bytes",
                limits.max_glob_bytes
            )));
        }
        let normalized = normalize_separators(pattern);
        let mut expanded = Vec::new();
        let mut generated_bytes = 0usize;
        expand_bounded(&normalized, 0, &mut expanded, &mut generated_bytes, limits)?;
        let fast_path = (expanded.len() == 1)
            .then(|| FastPath::new(&expanded[0]))
            .flatten();
        Ok(Self {
            alternatives: expanded
                .iter()
                .map(|alternative| tokenize(alternative))
                .collect(),
            fast_path,
        })
    }

    pub(crate) fn is_match(
        &self,
        value: &str,
        remaining_steps: &mut usize,
    ) -> Result<bool, FilesystemError> {
        let normalized = normalize_separators(value);
        if let Some(fast_path) = &self.fast_path {
            let steps = fast_path.steps();
            if steps > *remaining_steps {
                return Err(FilesystemError::message(
                    "glob matching exceeded its operation work budget",
                ));
            }
            *remaining_steps -= steps;
            return Ok(fast_path.is_match(&normalized));
        }
        let value = normalized.encode_utf16().collect::<Vec<_>>();
        let token_count = self.alternatives.iter().map(Vec::len).sum::<usize>();
        let steps = token_count
            .checked_mul(value.len().saturating_add(1))
            .ok_or_else(|| FilesystemError::message("glob matching work is too large"))?;
        if steps > *remaining_steps {
            return Err(FilesystemError::message(
                "glob matching exceeded its operation work budget",
            ));
        }
        *remaining_steps -= steps;
        Ok(self
            .alternatives
            .iter()
            .any(|tokens| matches_tokens(tokens, &value)))
    }
}

impl FastPath {
    fn new(pattern: &str) -> Option<Self> {
        if let Some(suffix) = pattern.strip_prefix("**/")
            && !suffix.is_empty()
            && !suffix.contains(['*', '?'])
        {
            return Some(Self::RecursiveLiteralSuffix(suffix.to_owned()));
        }
        (!pattern.contains(['*', '?'])).then(|| Self::Literal(pattern.to_owned()))
    }

    fn steps(&self) -> usize {
        match self {
            Self::Literal(literal) | Self::RecursiveLiteralSuffix(literal) => literal.len() + 1,
        }
    }

    fn is_match(&self, value: &str) -> bool {
        match self {
            Self::Literal(literal) => value == literal,
            Self::RecursiveLiteralSuffix(suffix) => {
                value == suffix
                    || value
                        .strip_suffix(suffix)
                        .is_some_and(|prefix| prefix.ends_with('/'))
            }
        }
    }
}

fn expand_bounded(
    pattern: &str,
    depth: usize,
    output: &mut Vec<String>,
    generated_bytes: &mut usize,
    limits: &FilesystemLimits,
) -> Result<(), FilesystemError> {
    let Some((start, end)) = first_brace_group(pattern) else {
        if output.len() == limits.max_glob_alternatives {
            return Err(FilesystemError::message(format!(
                "glob pattern exceeds maximum of {} alternatives",
                limits.max_glob_alternatives
            )));
        }
        *generated_bytes = generated_bytes
            .checked_add(pattern.len())
            .ok_or_else(|| FilesystemError::message("glob expansion is too large"))?;
        if *generated_bytes > limits.max_glob_generated_bytes {
            return Err(FilesystemError::message(format!(
                "glob expansion exceeds maximum size of {} bytes",
                limits.max_glob_generated_bytes
            )));
        }
        output.push(pattern.to_owned());
        return Ok(());
    };
    if depth == limits.max_glob_brace_depth {
        return Err(FilesystemError::message(format!(
            "glob pattern exceeds maximum brace depth of {}",
            limits.max_glob_brace_depth
        )));
    }
    let before = &pattern[..start];
    let after = &pattern[end + 1..];
    for part in pattern[start + 1..end].split(',') {
        let estimated = before
            .len()
            .checked_add(part.len())
            .and_then(|size| size.checked_add(after.len()))
            .ok_or_else(|| FilesystemError::message("glob expansion is too large"))?;
        if estimated > limits.max_glob_generated_bytes {
            return Err(FilesystemError::message("glob expansion is too large"));
        }
        let mut alternative = String::with_capacity(estimated);
        alternative.push_str(before);
        alternative.push_str(part);
        alternative.push_str(after);
        expand_bounded(&alternative, depth + 1, output, generated_bytes, limits)?;
    }
    Ok(())
}

// Select the first innermost brace group, matching the legacy grammar without
// allowing nested expansion to bypass the explicit depth and size budgets.
fn first_brace_group(pattern: &str) -> Option<(usize, usize)> {
    for (end, character) in pattern.char_indices() {
        if character != '}' {
            continue;
        }
        let prefix = &pattern[..end];
        let Some(start) = prefix.rfind('{') else {
            continue;
        };
        let body = &pattern[start + 1..end];
        if !body.is_empty() && !body.contains(['{', '}']) {
            return Some((start, end));
        }
    }
    None
}

fn tokenize(pattern: &str) -> Vec<Token> {
    let characters = pattern.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut index = 0usize;
    while index < characters.len() {
        match characters[index] {
            '*' if characters.get(index + 1) == Some(&'*')
                && characters.get(index + 2) == Some(&'/') =>
            {
                tokens.push(Token::RecursiveDirectories);
                index += 3;
            }
            '*' if characters.get(index + 1) == Some(&'*') => {
                tokens.push(Token::Recursive);
                index += 2;
            }
            '*' => {
                tokens.push(Token::SegmentStar);
                index += 1;
            }
            '?' => {
                tokens.push(Token::Any);
                index += 1;
            }
            character => {
                let mut units = [0; 2];
                tokens.extend(
                    character
                        .encode_utf16(&mut units)
                        .iter()
                        .copied()
                        .map(Token::Literal),
                );
                index += 1;
            }
        }
    }
    tokens
}

fn matches_tokens(tokens: &[Token], value: &[u16]) -> bool {
    let mut next = vec![false; value.len() + 1];
    next[value.len()] = true;
    for token in tokens.iter().rev() {
        let mut current = vec![false; value.len() + 1];
        match token {
            Token::RecursiveDirectories => {
                let mut slash_exit = false;
                for index in (0..=value.len()).rev() {
                    if index < value.len() && value[index] == u16::from(b'/') && next[index + 1] {
                        slash_exit = true;
                    }
                    current[index] = next[index] || slash_exit;
                }
            }
            Token::Recursive => {
                for index in (0..=value.len()).rev() {
                    current[index] = next[index] || (index < value.len() && current[index + 1]);
                }
            }
            Token::SegmentStar => {
                for index in (0..=value.len()).rev() {
                    current[index] = next[index]
                        || (index < value.len()
                            && value[index] != u16::from(b'/')
                            && current[index + 1]);
                }
            }
            Token::Any => {
                for index in 0..value.len() {
                    current[index] = value[index] != u16::from(b'/') && next[index + 1];
                }
            }
            Token::Literal(expected) => {
                for index in 0..value.len() {
                    current[index] = value[index] == *expected && next[index + 1];
                }
            }
        }
        next = current;
    }
    next[0]
}

fn normalize_separators(value: &str) -> String {
    if std::path::MAIN_SEPARATOR == '/' {
        value.to_owned()
    } else {
        value.replace(std::path::MAIN_SEPARATOR, "/")
    }
}
