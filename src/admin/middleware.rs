//! Admin API 中间件

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};

use super::service::AdminService;
use super::types::AdminErrorResponse;
use crate::anthropic::SharedApiKey;
use crate::common::auth;

/// Admin API 共享状态
#[derive(Clone)]
pub struct AdminState {
    /// Admin API 密钥共享句柄（可被 update_app_config 热替换）
    pub admin_api_key: SharedApiKey,
    /// Admin 服务
    pub service: Arc<AdminService>,
}

impl AdminState {
    /// 用共享句柄构造。该句柄与 [`AdminService`] 内持有的是同一个，
    /// 因此 `update_app_config` 写入后认证中间件立即读到新值（热生效）。
    pub fn new(admin_api_key: SharedApiKey, service: AdminService) -> Self {
        Self {
            admin_api_key,
            service: Arc::new(service),
        }
    }
}

/// Admin API 认证中间件
pub async fn admin_auth_middleware(
    State(state): State<AdminState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let api_key = auth::extract_api_key(&request);
    let expected = state.admin_api_key.read().clone();

    match api_key {
        Some(key) if auth::constant_time_eq(&key, &expected) => next.run(request).await,
        _ => {
            let error = AdminErrorResponse::authentication_error();
            (StatusCode::UNAUTHORIZED, Json(error)).into_response()
        }
    }
}
