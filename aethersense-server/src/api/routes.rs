/// Definição de rotas da API.
///
/// Monta o router Axum com todos os endpoints REST e WebSocket.
use axum::extract::DefaultBodyLimit;
use axum::middleware;
use axum::routing::{delete, get, post};
use axum::Router;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

use crate::api::accessories;
use crate::api::auth;
use crate::api::handlers;
use crate::api::websocket;
use crate::state::AppState;

/// Cria o router com todas as rotas da aplicação.
pub fn create_router(state: AppState) -> Router {
    let api_routes = Router::new()
        .route("/health", get(handlers::health))
        .route(
            "/settings",
            get(handlers::get_settings).post(handlers::update_settings),
        )
        .route(
            "/shower-box/settings",
            get(handlers::get_shower_box_settings).post(handlers::update_shower_box_settings),
        )
        .route(
            "/shower-box/settings/reset",
            post(handlers::reset_shower_box_settings),
        )
        .route("/clear", post(handlers::clear_history))
        .route("/replay", get(handlers::get_replay))
        .route("/replay/start", post(handlers::start_replay))
        .route("/replay/stop", post(handlers::stop_replay))
        .route(
            "/devices",
            get(handlers::get_devices).post(handlers::register_device),
        )
        .route("/map-config", get(handlers::get_map_config))
        .route("/map-config/floorplans", post(handlers::upsert_floorplan))
        .route(
            "/map-config/floorplans/{id}",
            delete(handlers::delete_floorplan),
        )
        .route(
            "/map-config/environment-objects",
            post(handlers::upsert_environment_object),
        )
        .route(
            "/map-config/environment-objects/{id}",
            delete(handlers::delete_environment_object),
        )
        .route(
            "/location/baseline",
            post(handlers::calibrate_empty_room).delete(handlers::clear_empty_room_baseline),
        )
        .route("/detection", get(handlers::get_detection))
        .route("/ml/status", get(handlers::get_ml_status))
        .route(
            "/mqtt/settings",
            get(handlers::get_mqtt_settings).post(handlers::update_mqtt_settings),
        )
        .route("/mqtt/discovery", post(handlers::republish_discovery))
        .route("/accessories", get(accessories::get_accessories))
        .route("/accessories/onboard", post(accessories::onboard_device))
        .route("/accessories/commands", post(accessories::create_command))
        .route(
            "/accessories/commands/{request_id}",
            get(accessories::get_command),
        )
        .route(
            "/ir/codes",
            get(accessories::list_ir_codes).post(accessories::create_ir_code),
        )
        .route(
            "/ir/codes/{id}",
            get(accessories::get_ir_code)
                .put(accessories::update_ir_code)
                .delete(accessories::delete_ir_code),
        )
        .route("/ir/learn", post(accessories::learn_ir))
        .route("/ir/send", post(accessories::send_ir))
        .route("/storage/uploads", post(accessories::start_upload))
        .route(
            "/storage/uploads/{upload_id}",
            get(accessories::get_upload).put(accessories::append_upload),
        )
        .route(
            "/storage/uploads/{upload_id}/complete",
            post(accessories::complete_upload),
        )
        .route("/results", get(handlers::list_results))
        .route("/results/{category}", post(handlers::save_result))
        .route(
            "/results/{category}/{session_id}/{filename}",
            post(handlers::save_result_file),
        );

    Router::new()
        .nest("/api", api_routes)
        .route("/ws", get(websocket::ws_handler))
        .fallback_service(ServeDir::new("../frontend"))
        .layer(middleware::from_fn(auth::require_mutation_bearer))
        .layer(CorsLayer::permissive())
        // Plantas baixas (SVG/PNG de CAD) viram data URL base64 e estouram o limite
        // padrão de 2 MB do Axum (resultava em 413 e "nada acontece" no upload).
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
        .with_state(state)
}
