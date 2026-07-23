use axum::{Json, http::StatusCode, response::IntoResponse};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("未找到服务")]
    NotFound,
    #[error("请求格式无效: {0}")]
    Invalid(String),
    #[error("版本不存在: {0}")]
    VersionNotFound(String),
    #[error("已有部署任务正在运行")]
    Busy,
    #[error("无法读取镜像版本: {0}")]
    Package(String),
    #[error("内部错误: {0}")]
    Internal(String),
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
        };
        (status, Json(ErrorBody { code, message })).into_response()
    }
}

impl From<sqlx::Error> for AppError {
    fn from(value: sqlx::Error) -> Self {
        Self::Internal(value.to_string())
    }
}
