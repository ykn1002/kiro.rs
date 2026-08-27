//! Admin API 模块
//!
//! 提供凭据管理和监控功能的 HTTP API
//!
//! # 功能
//! - 查询所有凭据状态
//! - 启用/禁用凭据
//! - 修改凭据优先级
//! - 重置失败计数
//! - 查询凭据余额
//!
//! # 使用
//! ```ignore
//! // shared_admin_api_key 由 AdminState 认证与 AdminService 修改共用，实现 adminApiKey 热替换
//! let admin_service = AdminService::new(
//!     token_manager.clone(), endpoint_names, shared_api_key.clone(), shared_admin_api_key.clone(),
//! );
//! let admin_state = AdminState::new(shared_admin_api_key, admin_service);
//! let admin_router = create_admin_router(admin_state);
//! ```

mod error;
mod handlers;
mod middleware;
mod router;
mod service;
pub mod types;

pub use middleware::AdminState;
pub use router::create_admin_router;
pub use service::AdminService;
