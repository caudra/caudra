use std::collections::{HashMap, HashSet};

use futures_util::future::join_all;
use tokio_util::sync::CancellationToken;
use url::Url;
use workcell_source_icons::{ResolvedSourceIcon, SourceIconError};

use crate::WebToolDependencies;
use crate::dependencies::IconRequest;
use crate::types::WebsearchResult;

const BATCH_SIZE: usize = 3;

pub(super) async fn enrich(
    mut results: Vec<WebsearchResult>,
    dependencies: &WebToolDependencies,
    cancellation: CancellationToken,
) -> Result<Vec<WebsearchResult>, SourceIconError> {
    if cancellation.is_cancelled() {
        return Err(SourceIconError::Cancelled);
    }
    if !dependencies.source_icons_enabled {
        for result in &mut results {
            // Opt-out suppresses provider-supplied inline icons as well as local resolution.
            result.icon_url = None;
            result.icon_data_url = None;
        }
        return Ok(results);
    }
    // Resolve one icon per unique origin in sequential batches of three. This
    // bounds best-effort decoration fan-out and never pollutes model text.
    let mut target_origins = HashSet::new();
    let targets = results
        .iter()
        .filter(|result| result.icon_data_url.is_none())
        .filter_map(|result| {
            let url = Url::parse(&result.url).ok()?;
            let origin = url.origin().ascii_serialization();
            target_origins
                .insert(origin.clone())
                .then_some((origin, result.url.clone()))
        })
        .collect::<Vec<_>>();
    let mut icons = HashMap::<String, ResolvedSourceIcon>::new();
    for batch in targets.chunks(BATCH_SIZE) {
        let resolved = join_all(batch.iter().map(|(origin, url)| {
            let origin = origin.clone();
            let request = IconRequest {
                page_url: url.clone(),
                html: None,
                cancellation: cancellation.clone(),
            };
            async move { (origin, dependencies.icons.resolve(request).await) }
        }))
        .await;
        for (origin, icon) in resolved {
            match icon {
                Ok(Some(icon)) => {
                    icons.insert(origin, icon);
                }
                Err(SourceIconError::Cancelled) => return Err(SourceIconError::Cancelled),
                Ok(None) | Err(_) => {}
            }
        }
    }
    for result in &mut results {
        if result.icon_data_url.is_some() {
            continue;
        }
        let Some(icon) = Url::parse(&result.url)
            .ok()
            .and_then(|url| icons.get(&url.origin().ascii_serialization()))
        else {
            continue;
        };
        result.icon_url = Some(icon.icon_url.clone());
        result.icon_data_url = Some(icon.icon_data_url.clone());
    }
    Ok(results)
}
