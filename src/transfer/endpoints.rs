use axum::{
    extract::{Request, State},
    response::Response,
};

use super::{TransferGroup, reviewed_http};

pub(crate) async fn download(State(group): State<TransferGroup>, request: Request) -> Response {
    reviewed_http::download(group.reviewed.as_ref(), request).await
}

pub(crate) async fn upload(State(group): State<TransferGroup>, request: Request) -> Response {
    reviewed_http::upload(group.reviewed.as_ref(), request).await
}
