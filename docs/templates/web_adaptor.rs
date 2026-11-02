use mycela::app::AppState;
use mycela::axum::extract::State;
use mycela::axum::response::Html;
use mycela::axum::routing::{get, post};
use mycela::config::{AppConfig, WidgetConfig};

use crate::assets;

// Web adaptor template for a user APP.

const APP_SCREEN_ID: &str = "APP_NAME";
const APP_ENTRY_PATH: &str = "/APP_URL";
const APP_BACKGROUND_ASSET_PATH: &str = "/static/APP.svg";


pub fn build_app_state(config: AppConfig, loopback_token: Option<String>) -> AppState {
    let state = mycela::app::AppState {
        config: std::sync::Arc::new(config),
        channel_ctx: mycela::channel::ChannelContext::new(),
        loopback_token,
        ascii_tcp_task: std::sync::Arc::new(std::sync::Mutex::new(None)),
        ascii_tcp_start_hook: None,
    };
    // Start every widget monitor now, so they run with no browser connected and the
    // SSE streams reuse them instead of spawning duplicate pollers.
    mycela::app::start_shared_widget_monitors(&state);
    
    // Spawn any threads from the application's logic here.

    state
}

pub fn build_routes(state: AppState) -> mycela::axum::Router {
    mycela::axum::Router::new()
        // App entry point `/`
        .route("/", get(render_app_screen))
        // App entry point with URL `/APP_URL`
        .route(APP_ENTRY_PATH, get(render_app_screen))
        .route("/stream/screen/{screen_id}",get(mycela::app::stream_screen_widgets),)
        // Loaded by the page head. Every button, toggle, slider and select posts here. Without it you can't send any commands.
        .route("/api/widget/{widget_id}/set",post(mycela::app::write_widget),)
        // Loaded by the page head. Enables HTMX functionality.
        .route(assets::HTMX_PATH, get(assets::serve_htmx))
        // Loaded by the page head. It includes mycela's default stylesheet.
        .route(assets::MYCELA_CSS_PATH, get(assets::serve_css))
        // Injected into the head by `render_app_screen`. APP's custom css layout overrides on top of style.css.
        // This needs to be added here so that when the custom css is called the browser knows about it.
        .route(assets::APP_CSS_PATH, get(assets::serve_app_css))
        // Loaded by the page head. It displays tooltips for the widgets.
        .route(assets::TOOLTIP_PATH, get(assets::serve_tooltip))
        // Loaded by the page head. It binds the widget writes.
        .route(assets::CLIENT_TRANSPORT_PATH, get(assets::serve_client_transport))
        .with_state(state)
}

/// Finds a widget by ID anywhere in the config, including inside `group` children.
pub fn find_widget(config: &AppConfig, id: &str) -> Option<WidgetConfig> {
    config
        .screens
        .iter()
        .flat_map(|s| mycela::widgets::collect_data_widgets(&s.widgets))
        .find(|w| w.id == id)
}

async fn render_app_screen(State(state): State<AppState>) -> Html<String> {
    let screen = state
        .config
        .screens
        .iter()
        .find(|s| s.id == APP_SCREEN_ID)
        .unwrap_or_else(|| panic!("screen '{}' missing from app_config.json", APP_SCREEN_ID));

    let page = mycela::widgets::render_screen_with_options(
        screen,
        true,
        None,
        state.loopback_token.as_deref(),
    )
    .into_string();

    // mycela already renders a complete document; only add App's extras to it.
    let head_extras = mycela::maud::html! {
        link rel="stylesheet" href=(assets::APP_CSS_PATH);
    }
    .into_string();
    let body_extras = mycela::maud::html! {
        div style="position: fixed; top: 10px; right: 10px; z-index: 9999;" {
            button onclick="window.toggleInfo()" style="padding: 8px 12px; background: var(--accent); color: white; border: none; border-radius: 4px; cursor: pointer; font-weight: 500;" {
                span id="info-lbl" { "Hide Info" }
            }
        }
    }
    .into_string();

    let page = page.replacen("</head>", &format!("{head_extras}</head>"), 1);
    let page = match page.find("<body").and_then(|i| page[i..].find('>').map(|j| i + j + 1)) {
        Some(end) => format!("{}{}{}", &page[..end], body_extras, &page[end..]),
        None => page,
    };

    Html(page)
}