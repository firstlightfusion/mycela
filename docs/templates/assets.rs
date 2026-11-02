/// Embedded mycela static assets served from memory instead of the filesystem.
/// This avoids path-resolution issues when the binary is run from any directory.
use axum::{
    http::header,
    response::IntoResponse,
};

/// Declares an embedded static file from one filename: `$url` is its `/static/<file>` URL
/// and `$bytes` its contents. `include_bytes!` needs a literal, so this is done with
/// `concat!` rather than a `const`.
macro_rules! embedded {
    ($url:ident, $bytes:ident, $dir:literal, $file:literal) => {
        pub const $url: &str = concat!("/static/", $file);
        pub const $bytes: &[u8] = include_bytes!(concat!($dir, $file));
    };
}

embedded!(HTMX_PATH, HTMX_JS, "../../../3rdparty/mycela/static/", "htmx.min.js");
embedded!(MYCELA_CSS_PATH, STYLE_CSS, "../../../3rdparty/mycela/static/", "style.css");
embedded!(APP_CSS_PATH, APP_CSS, "../static/", "app.css");
embedded!(TOOLTIP_PATH, TOOLTIP_JS, "../../../3rdparty/mycela/static/", "tooltip.js");
embedded!(CLIENT_TRANSPORT_PATH, CLIENT_TRANSPORT_JS, "../../../3rdparty/mycela/static/", "client_transport.js");

pub const INTER_REGULAR: &[u8] = include_bytes!("../../../3rdparty/mycela/static/fonts/inter/Inter-Regular.woff2");
pub const INTER_MEDIUM: &[u8] = include_bytes!("../../../3rdparty/mycela/static/fonts/inter/Inter-Medium.woff2");
pub const INTER_BOLD: &[u8] = include_bytes!("../../../3rdparty/mycela/static/fonts/inter/Inter-Bold.woff2");
pub const IBM_PLEX_MONO_REGULAR: &[u8] = include_bytes!("../../../3rdparty/mycela/static/fonts/ibm-plex-mono/IBMPlexMono-Regular.woff2");
pub const IBM_PLEX_MONO_MEDIUM: &[u8] = include_bytes!("../../../3rdparty/mycela/static/fonts/ibm-plex-mono/IBMPlexMono-Medium.woff2");
pub const IBM_PLEX_MONO_BOLD: &[u8] = include_bytes!("../../../3rdparty/mycela/static/fonts/ibm-plex-mono/IBMPlexMono-Bold.woff2");
pub const APP_BACKGROUND_SVG: &[u8] = include_bytes!("../static/app_background.svg");

pub fn get_asset(path: &str) -> Option<(&'static [u8], &'static str)> {
    match path {
        "htmx.min.js" => Some((HTMX_JS, "application/javascript")),
        "style.css" => Some((STYLE_CSS, "text/css")),
        "tooltip.js" => Some((TOOLTIP_JS, "application/javascript")),
        "client_transport.js" => Some((CLIENT_TRANSPORT_JS, "application/javascript")),
        "app_background.svg" => Some((APP_BACKGROUND_SVG, "image/svg+xml")),
        "fonts/inter/Inter-Regular.woff2" => Some((INTER_REGULAR, "font/woff2")),
        "fonts/inter/Inter-Medium.woff2" => Some((INTER_MEDIUM, "font/woff2")),
        "fonts/inter/Inter-Bold.woff2" => Some((INTER_BOLD, "font/woff2")),
        "fonts/ibm-plex-mono/IBMPlexMono-Regular.woff2" => Some((IBM_PLEX_MONO_REGULAR, "font/woff2")),
        "fonts/ibm-plex-mono/IBMPlexMono-Medium.woff2" => Some((IBM_PLEX_MONO_MEDIUM, "font/woff2")),
        "fonts/ibm-plex-mono/IBMPlexMono-Bold.woff2" => Some((IBM_PLEX_MONO_BOLD, "font/woff2")),
        _ => None,
    }
}

pub async fn serve_htmx() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/javascript")], HTMX_JS)
}

pub async fn serve_css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css")], STYLE_CSS)
}

pub async fn serve_APP_CSS() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css")], APP_CSS)
}

pub async fn serve_tooltip() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/javascript")], TOOLTIP_JS)
}

pub async fn serve_client_transport() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/javascript")], CLIENT_TRANSPORT_JS)
}

pub async fn serve_inter_regular() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "font/woff2")], INTER_REGULAR)
}

pub async fn serve_inter_medium() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "font/woff2")], INTER_MEDIUM)
}

pub async fn serve_inter_bold() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "font/woff2")], INTER_BOLD)
}

pub async fn serve_ibm_plex_mono_regular() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "font/woff2")], IBM_PLEX_MONO_REGULAR)
}

pub async fn serve_ibm_plex_mono_medium() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "font/woff2")], IBM_PLEX_MONO_MEDIUM)
}

pub async fn serve_ibm_plex_mono_bold() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "font/woff2")], IBM_PLEX_MONO_BOLD)
}

pub async fn serve_background_svg() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/svg+xml")], APP_BACKGROUND_SVG)
}
