use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use workcell_source_icons::{ResolvedSourceIcon, SourceIconError};

use crate::WebToolDependencies;
use crate::dependencies::IconRequest;

pub(super) async fn resolve(
    dependencies: &WebToolDependencies,
    page_url: &str,
    html: Option<String>,
    cancellation: CancellationToken,
    deadline: Instant,
) -> Result<Option<ResolvedSourceIcon>, SourceIconError> {
    if cancellation.is_cancelled() {
        return Err(SourceIconError::Cancelled);
    }
    if !dependencies.source_icons_enabled {
        return Ok(None);
    }
    let request = IconRequest {
        page_url: page_url.to_owned(),
        html,
        cancellation: cancellation.clone(),
    };
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(SourceIconError::Cancelled),
        result = tokio::time::timeout_at(deadline, dependencies.icons.resolve(request)) => {
            match result {
                Ok(result) => result,
                Err(_) => Ok(None),
            }
        }
    }
}
