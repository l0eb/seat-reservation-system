//! Axum's extractors, except a rejected request gets the usual
//! `{"error": ...}` JSON body instead of axum's plain-text one.

use axum::extract::{FromRequest, FromRequestParts};

use crate::error::AppError;

#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(AppError))]
pub struct JsonBody<T>(pub T);

#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Query), rejection(AppError))]
pub struct QueryParams<T>(pub T);

#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Path), rejection(AppError))]
pub struct PathParam<T>(pub T);
