use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}
impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    pub fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "Требуется действительный код комнаты или локальный токен",
        )
    }
    pub fn internal(error: impl std::fmt::Display) -> Self {
        eprintln!("Spark: {error}");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Внутренняя ошибка сервиса",
        )
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(serde_json::json!({"error":self.message}))).into_response()
    }
}
impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        Self::internal(e)
    }
}
impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        Self::internal(e)
    }
}
impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        Self::bad(format!("Некорректный JSON: {e}"))
    }
}
pub type ApiResult<T> = Result<T, ApiError>;

pub fn json<T>(
    value: Result<axum::Json<T>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<T> {
    value
        .map(|v| v.0)
        .map_err(|e| ApiError::new(e.status(), format!("Некорректный JSON: {}", e.body_text())))
}
