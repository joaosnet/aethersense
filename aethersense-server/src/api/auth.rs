//! Global authentication guard for REST mutations.
//!
//! Browser reads and WebSocket upgrades remain public. Every state-changing
//! request must use the server API token, except resumable SD uploads: those
//! endpoints authenticate the individual device credential in their handlers.

use axum::extract::Request;
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::accessories::constant_time_eq;

const API_TOKEN_ENV: &str = "AETHERSENSE_API_TOKEN";
const UPLOADS_PATH: &str = "/api/storage/uploads";

/// Require the global Bearer token for every state-changing REST request.
pub async fn require_mutation_bearer(request: Request, next: Next) -> Response {
    let configured_token = std::env::var(API_TOKEN_ENV)
        .ok()
        .filter(|value| !value.is_empty());

    match authorize_mutation(
        request.method(),
        request.uri().path(),
        request.headers(),
        configured_token.as_deref(),
    ) {
        Ok(()) => next.run(request).await,
        Err(rejection) => rejection.into_response(),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct AuthRejection {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}

impl IntoResponse for AuthRejection {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(json!({
                "ok": false,
                "error": self.code,
                "message": self.message,
            })),
        )
            .into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"aethersense-api\""),
            );
        }
        response
    }
}

fn authorize_mutation(
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    configured_token: Option<&str>,
) -> Result<(), AuthRejection> {
    if is_safe_method(method) || uses_device_upload_auth(path) {
        return Ok(());
    }

    let expected = configured_token
        .filter(|token| !token.is_empty())
        .ok_or(AuthRejection {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "auth_not_configured",
            message: "configure AETHERSENSE_API_TOKEN para habilitar mutações REST",
        })?;

    let supplied = bearer_token(headers).ok_or(AuthRejection {
        status: StatusCode::UNAUTHORIZED,
        code: "invalid_token",
        message: "Bearer token ausente ou inválido",
    })?;

    if constant_time_eq(expected, supplied) {
        Ok(())
    } else {
        Err(AuthRejection {
            status: StatusCode::UNAUTHORIZED,
            code: "invalid_token",
            message: "Bearer token ausente ou inválido",
        })
    }
}

fn is_safe_method(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

fn uses_device_upload_auth(path: &str) -> bool {
    path == UPLOADS_PATH
        || path
            .strip_prefix(UPLOADS_PATH)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?.trim();
    let separator = value.find(char::is_whitespace)?;
    let (scheme, token) = value.split_at(separator);
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("Bearer") && !token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bearer(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn mutacao_sem_token_configurado_retorna_503() {
        let rejection = authorize_mutation(&Method::POST, "/api/settings", &HeaderMap::new(), None)
            .unwrap_err();
        assert_eq!(rejection.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(rejection.code, "auth_not_configured");
    }

    #[test]
    fn mutacao_sem_bearer_valido_retorna_401() {
        for headers in [
            HeaderMap::new(),
            bearer("Basic abc"),
            bearer("Bearer incorreto"),
        ] {
            let rejection = authorize_mutation(
                &Method::DELETE,
                "/api/map-config/floorplans/sala",
                &headers,
                Some("segredo"),
            )
            .unwrap_err();
            assert_eq!(rejection.status, StatusCode::UNAUTHORIZED);
            assert_eq!(rejection.code, "invalid_token");
        }
    }

    #[test]
    fn mutacao_com_bearer_correto_e_aceita() {
        assert!(authorize_mutation(
            &Method::PATCH,
            "/api/settings",
            &bearer("bearer segredo"),
            Some("segredo"),
        )
        .is_ok());
    }

    #[test]
    fn leituras_options_e_uploads_preservam_seus_fluxos() {
        let empty = HeaderMap::new();
        assert!(authorize_mutation(&Method::GET, "/api/settings", &empty, None).is_ok());
        assert!(authorize_mutation(&Method::OPTIONS, "/api/settings", &empty, None).is_ok());
        assert!(authorize_mutation(&Method::POST, UPLOADS_PATH, &empty, None).is_ok());
        assert!(
            authorize_mutation(&Method::PUT, "/api/storage/uploads/upload-1", &empty, None,)
                .is_ok()
        );

        let rejection =
            authorize_mutation(&Method::POST, "/api/storage/uploads-indevido", &empty, None)
                .unwrap_err();
        assert_eq!(rejection.status, StatusCode::SERVICE_UNAVAILABLE);
    }
}
