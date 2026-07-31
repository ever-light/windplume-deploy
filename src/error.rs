use axum::{Json, http::StatusCode, response::IntoResponse};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("未找到 Compose 项目或服务")]
    NotFound,
    #[error("请求格式无效: {0}")]
    Invalid(String),
    #[error("版本不存在: {0}")]
    VersionNotFound(String),
    #[error("项目已有操作正在运行")]
    Busy,
    #[error("系统更新期间暂停新操作")]
    Updating,
    #[error("无法读取镜像版本: {0}")]
    Package(String),
    #[error("内部错误: {0}")]
    Internal(String),
    #[error("无法更新程序: {0}")]
    Update(String),
    #[error("无法读取或清理系统资源: {0}")]
    System(String),
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let (status, code, message) = match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "service_not_found", self.to_string()),
            Self::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid_request", self.to_string()),
            Self::VersionNotFound(_) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "version_not_found",
                self.to_string(),
            ),
            Self::Busy => (
                StatusCode::CONFLICT,
                "deployment_in_progress",
                self.to_string(),
            ),
            Self::Updating => (
                StatusCode::CONFLICT,
                "system_update_in_progress",
                self.to_string(),
            ),
            Self::Package(_) => (
                StatusCode::BAD_GATEWAY,
                "package_query_failed",
                self.to_string(),
            ),
            Self::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "内部服务错误".into(),
            ),
            Self::Update(_) => (
                StatusCode::BAD_GATEWAY,
                "system_update_failed",
                self.to_string(),
            ),
            Self::System(_) => (
                StatusCode::BAD_GATEWAY,
                "system_resource_failed",
                self.to_string(),
            ),
        };
        (status, Json(ErrorBody { code, message })).into_response()
    }
}

impl From<sqlx::Error> for AppError {
    fn from(value: sqlx::Error) -> Self {
        Self::Internal(value.to_string())
    }
}

impl From<std::io::Error> for AppError {
    fn from(value: std::io::Error) -> Self {
        Self::Internal(value.to_string())
    }
}
