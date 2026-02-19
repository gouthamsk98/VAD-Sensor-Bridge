use crate::persona::{ PersonaState, PersonaTrait };
use axum::{ extract::State, http::StatusCode, response::IntoResponse, routing::get, Json, Router };
use serde::{ Deserialize, Serialize };
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::info;

// ─────────────────────────────────────────────────────────────────────
//  JSON request / response types
// ─────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct PersonaResponse {
    persona: PersonaTrait,
    index: u8,
}

#[derive(Serialize)]
struct PersonaListResponse {
    current: PersonaTrait,
    available: Vec<PersonaEntry>,
}

#[derive(Serialize)]
struct PersonaEntry {
    index: u8,
    name: PersonaTrait,
}

#[derive(Deserialize)]
struct SetPersonaRequest {
    /// Accept either the string name or numeric index.
    #[serde(default)]
    persona: Option<PersonaTrait>,
    #[serde(default)]
    index: Option<u8>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

// ─────────────────────────────────────────────────────────────────────
//  Handlers
// ─────────────────────────────────────────────────────────────────────

/// `GET /persona` — return current active persona.
async fn get_persona(State(state): State<PersonaState>) -> impl IntoResponse {
    let p = state.get().await;
    Json(PersonaResponse {
        persona: p,
        index: p.index(),
    })
}

/// `GET /persona/list` — return all available personas + current.
async fn list_personas(State(state): State<PersonaState>) -> impl IntoResponse {
    let current = state.get().await;
    let available = PersonaTrait::ALL.iter()
        .map(|p| PersonaEntry {
            index: p.index(),
            name: *p,
        })
        .collect();
    Json(PersonaListResponse { current, available })
}

/// `PUT /persona` — change the active persona.
///
/// Accepts JSON body with either `"persona": "mischievous"` or `"index": 1`.
async fn set_persona(
    State(state): State<PersonaState>,
    Json(req): Json<SetPersonaRequest>
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let new_persona = match (req.persona, req.index) {
        (Some(p), _) => p,
        (None, Some(i)) =>
            PersonaTrait::from_index(i).ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: format!("invalid persona index: {i} (valid: 0–3)"),
                    }),
                )
            })?,
        (None, None) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "provide either \"persona\" (string) or \"index\" (0–3)".into(),
                }),
            ));
        }
    };

    let old = state.get().await;
    state.set(new_persona).await;

    info!(
        old = %old,
        new = %new_persona,
        "🎭 Persona changed"
    );

    Ok(
        Json(PersonaResponse {
            persona: new_persona,
            index: new_persona.index(),
        })
    )
}

/// `GET /health` — simple health check.
async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

// ─────────────────────────────────────────────────────────────────────
//  Server bootstrap
// ─────────────────────────────────────────────────────────────────────

/// Build the axum Router with all persona routes.
pub fn build_router(persona: PersonaState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/persona", get(get_persona).put(set_persona))
        .route("/persona/list", get(list_personas))
        .with_state(persona)
}

/// Start the REST API server.  Returns the `JoinHandle` so the caller
/// can select/join on it alongside the UDP listeners.
pub async fn start_api_server(
    host: &str,
    port: u16,
    persona: PersonaState
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    let app = build_router(persona);

    let listener = TcpListener::bind(addr).await?;
    info!(addr = %addr, "🌐 REST API listening");

    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "REST API server error");
        }
    });

    Ok(handle)
}
