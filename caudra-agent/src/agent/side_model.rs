use std::sync::Arc;

use caudra_config::ModelPolicy;
use caudra_providers::provider::{Provider, from_model_async};
use caudra_providers::{AgentError, Model, ModelError, ModelPurpose, Timeouts};
use tracing::{debug, warn};

/// A model resolved for a job that runs beside the conversation: a title, a
/// requirements list. It never enters the session's history, so it may differ
/// from the chat model without disturbing the cached prefix.
pub struct SideModel {
    pub provider: Arc<dyn Provider>,
    pub model: Model,
}

/// Falls back to the chat model whenever the binding cannot be resolved: a
/// side model that will not load is a reason to use what is already there, not
/// to skip the job.
///
/// `max_output_tokens` caps the answer; the resolved model keeps its own cap
/// when that is tighter.
pub async fn resolve(
    purpose: ModelPurpose,
    current_provider: &Arc<dyn Provider>,
    current_model: &Model,
    timeouts: Timeouts,
    model_policy: &ModelPolicy,
    max_output_tokens: u32,
) -> SideModel {
    let target_model = current_model.clone();
    let policy = model_policy.clone();
    // Catalog lookups and `warm_catalog` block, so they stay off the executor
    // that is currently carrying the turn this job belongs to.
    let resolved = smol::unblock(move || purpose_model(purpose, &target_model, &policy)).await;
    let mut model = match resolved {
        Ok(model) => model,
        Err(error) => {
            debug!(%error, %purpose, "falling back to the chat model");
            current_model.clone()
        }
    };
    model.max_output_tokens = Some(
        model
            .max_output_tokens
            .unwrap_or(max_output_tokens)
            .min(max_output_tokens),
    );

    let provider = if model.provider == current_model.provider {
        current_provider.adjust_model(&mut model);
        Arc::clone(current_provider)
    } else {
        match from_model_async(&mut model, timeouts).await {
            Ok(provider) => Arc::from(provider),
            Err(error) => {
                warn!(%error, model = %model.id, %purpose, "no provider for the side model, using the chat model");
                let mut model = current_model.clone();
                current_provider.adjust_model(&mut model);
                return SideModel {
                    provider: Arc::clone(current_provider),
                    model,
                };
            }
        }
    };
    SideModel { provider, model }
}

fn purpose_model(
    purpose: ModelPurpose,
    current_model: &Model,
    model_policy: &ModelPolicy,
) -> Result<Model, AgentError> {
    Model::resolve(purpose, current_model, model_policy)
        .map_err(|error| purpose_model_error(purpose, error))
}

fn purpose_model_error(purpose: ModelPurpose, error: ModelError) -> AgentError {
    AgentError::Config {
        message: format!("cannot resolve the {purpose} model: {error}"),
    }
}
