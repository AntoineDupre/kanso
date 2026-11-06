use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::error::Error;

/// One API failure, rendered as PLAN §5's error envelope.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    clear_cookie: Option<String>,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            clear_cookie: None,
        }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "validation", message)
    }

    /// A validation failure that callers distinguish with 422, used for an
    /// invalid `provider/model[#variant]` value.
    pub fn unprocessable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "validation", message)
    }

    /// A body-extraction failure keeps Axum's status (400/413/415) but uses the
    /// API's `validation` code. Data errors (422) become 400.
    pub fn rejection(status: StatusCode, message: impl Into<String>) -> Self {
        // Serde data errors surface as 422; the API contract treats them as
        // ordinary validation failures (400).
        let status = if status == StatusCode::UNPROCESSABLE_ENTITY {
            StatusCode::BAD_REQUEST
        } else {
            status
        };
        Self::new(status, "validation", message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }

    /// A conflict with a specific machine-readable code (`already_claimed`,
    /// `dependency_unmet`, `not_claimable`, …).
    pub fn conflict_code(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    pub fn csrf() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "csrf",
            "cross-origin request rejected",
        )
    }

    pub fn rate_limited() -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many failed login attempts; try again in a minute",
        )
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }

    /// A dependency that is not configured or cannot be reached.
    pub fn service_unavailable(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, code, message)
    }

    /// The upstream service rejected the request; surface its message.
    pub fn bad_gateway(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "openchamber_error", message)
    }

    /// Ask the response to expire the session cookie (expired/tampered/revoked).
    pub fn clear_cookie(mut self, cookie: impl Into<String>) -> Self {
        self.clear_cookie = Some(cookie.into());
        self
    }

    #[cfg(test)]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    #[cfg(test)]
    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        tracing::error!(%error, "internal API error");
        Self::internal("internal server error")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({
            "error": {
                "code": self.code,
                "message": self.message,
            }
        });
        let mut response = (self.status, Json(body)).into_response();
        if let Some(cookie) = self.clear_cookie {
            if let Ok(value) = HeaderValue::from_str(&cookie) {
                response.headers_mut().insert(header::SET_COOKIE, value);
            }
        }
        response
    }
}
