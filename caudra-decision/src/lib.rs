#![forbid(unsafe_code)]

pub mod client;
pub mod engine;
mod openai;
pub mod question_set;
pub mod wire;

pub use client::HttpDecisionClient;
pub use engine::{CachedDecisionEngine, DecisionEngine, DecisionError};
pub use question_set::QuestionSet;
pub use wire::{
    Answer, ChoiceAnswer, DecisionRequest, DecisionResponse, NoulAnswer, Question, QuestionType,
    Questions, ScoreAnswer, Usage,
};
