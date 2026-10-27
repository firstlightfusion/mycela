use axum::{body::to_bytes, extract::Path, http::StatusCode, routing::get, Router};
use mycela::assets::{get_asset, serve_asset};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ASSETS: [(&str, &str); 10] = [
    ("htmx.min.js", "application/javascript"),
    ("style.css", "text/css"),
    ("tooltip.js", "application/javascript"),
    ("client_transport.js", "application/javascript"),
    ("fonts/inter/Inter-Regular.woff2", "font/woff2"),
    ("fonts/inter/Inter-Medium.woff2", "font/woff2"),
    ("fonts/inter/Inter-Bold.woff2", "font/woff2"),
    (
        "fonts/ibm-plex-mono/IBMPlexMono-Regular.woff2",
        "font/woff2",
    ),
    ("fonts/ibm-plex-mono/IBMPlexMono-Medium.woff2", "font/woff2"),
    ("fonts/ibm-plex-mono/IBMPlexMono-Bold.woff2", "font/woff2"),
];

#[tokio::test]
async fn every_framework_asset_has_content_and_the_correct_http_type() {
    for (path, content_type) in ASSETS {
        let (expected, mime) = get_asset(path).expect(path);
        assert!(!expected.is_empty(), "{path}");
        assert_eq!(mime, content_type, "{path}");
        if content_type == "font/woff2" {
            assert!(expected.starts_with(b"wOF2"), "{path}");
        }
        let response = serve_asset(Path(path.to_owned())).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(response.headers()["content-type"], content_type, "{path}");
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), expected, "{path}");
    }
    assert_eq!(
        get_asset("style.css").unwrap().0,
        include_bytes!("..\\static\\style.css")
    );
}

#[tokio::test]
async fn unknown_and_filesystem_paths_are_not_served() {
    for path in [
        "missing.js",
        "../Cargo.toml",
        "..\\Cargo.toml",
        "/style.css",
        "",
    ] {
        assert!(get_asset(path).is_none(), "{path}");
        assert_eq!(
            serve_asset(Path(path.to_owned())).await.status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

#[tokio::test]
async fn embedded_assets_work_through_axum_routes() {
    let app = Router::new().route("/static/{*path}", get(serve_asset));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    for (method, path, status, expected_body) in [
        (
            "GET",
            "style.css",
            "200 OK",
            get_asset("style.css").unwrap().0,
        ),
        ("HEAD", "style.css", "200 OK", &b""[..]),
        (
            "GET",
            "fonts/inter/Inter-Regular.woff2",
            "200 OK",
            get_asset("fonts/inter/Inter-Regular.woff2").unwrap().0,
        ),
        ("GET", "missing.js", "404 Not Found", &b""[..]),
    ] {
        let mut connection = tokio::net::TcpStream::connect(address).await.unwrap();
        connection
            .write_all(
                format!(
            "{method} /static/{path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            connection.read_to_end(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        let split = response
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap();
        let headers = std::str::from_utf8(&response[..split]).unwrap();
        assert!(
            headers.starts_with(&format!("HTTP/1.1 {status}")),
            "{headers}"
        );
        assert_eq!(&response[split + 4..], expected_body, "{method} {path}");
    }
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}
