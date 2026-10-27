use caudra_providers::ReasoningSource;

const MODEL_LABEL: &str = "Model";
const THINKING_LABEL: &str = "Thinking";
const CHANGE_SEPARATOR: &str = " · ";

/// Which model wrote a message, and at what level, as its history stamp or
/// the request that streamed it says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Producer {
    pub model: String,
    pub thinking: Option<String>,
}

impl From<&ReasoningSource> for Producer {
    fn from(source: &ReasoningSource) -> Self {
        Self {
            model: source.spec(),
            thinking: source.thinking.clone(),
        }
    }
}

impl Producer {
    /// The note between a message this wrote and the next one, which `next`
    /// wrote, naming what changed. A level only one side recorded says
    /// nothing, since the other may simply predate the stamp.
    pub(crate) fn change_note(&self, next: &Self) -> Option<String> {
        let model = (self.model != next.model)
            .then(|| format!("{MODEL_LABEL}: {} → {}", self.model, next.model));
        let thinking = match (&self.thinking, &next.thinking) {
            (Some(from), Some(to)) if from != to => {
                Some(format!("{THINKING_LABEL}: {from} → {to}"))
            }
            _ => None,
        };
        match (model, thinking) {
            (None, None) => None,
            (Some(model), Some(thinking)) => Some(format!("{model}{CHANGE_SEPARATOR}{thinking}")),
            (Some(only), None) | (None, Some(only)) => Some(only),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const OPUS: &str = "anthropic/claude-opus-5-5";
    const SOL: &str = "openai/gpt-6.1-sol";
    const HIGH: &str = "high";
    const LOW: &str = "low";

    fn producer(model: &str, thinking: Option<&str>) -> Producer {
        Producer {
            model: model.into(),
            thinking: thinking.map(str::to_owned),
        }
    }

    #[test_case(producer(OPUS, Some(HIGH)), producer(OPUS, Some(HIGH)), None ; "unchanged")]
    #[test_case(producer(OPUS, Some(HIGH)), producer(SOL, Some(HIGH)), Some("Model: anthropic/claude-opus-5-5 → openai/gpt-6.1-sol") ; "model")]
    #[test_case(producer(OPUS, Some(HIGH)), producer(OPUS, Some(LOW)), Some("Thinking: high → low") ; "thinking")]
    #[test_case(producer(OPUS, Some(HIGH)), producer(SOL, Some(LOW)), Some("Model: anthropic/claude-opus-5-5 → openai/gpt-6.1-sol · Thinking: high → low") ; "both")]
    #[test_case(producer(OPUS, None), producer(OPUS, Some(LOW)), None ; "level_unknown_before")]
    #[test_case(producer(OPUS, Some(HIGH)), producer(SOL, None), Some("Model: anthropic/claude-opus-5-5 → openai/gpt-6.1-sol") ; "level_unknown_after")]
    fn change_note_names_what_moved(from: Producer, to: Producer, expected: Option<&str>) {
        assert_eq!(from.change_note(&to).as_deref(), expected);
    }
}
