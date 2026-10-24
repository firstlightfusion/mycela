#![cfg(feature = "ascii-tcp")]

mod test_ascii_tcp_server {
    use mycela;
    use ascii_tcp::LineEnding;
    use std::net::SocketAddr;
    use std::time::Duration;

    #[tokio::test]
    async fn ascii_tcp_server_entrypoint_is_available() {
        let handle = mycela::ascii_tcp_server::start_server(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            |request, _peer| async move { Ok(request) },
        );
        handle.abort();
    }

    #[tokio::test]
    async fn configured_crlf_server_interoperates_with_crlf_client() {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
        let port = probe.local_addr().expect("probe address").port();
        drop(probe);

        let handle = mycela::ascii_tcp_server::start_server_with_line_ending(
            SocketAddr::from(([127, 0, 0, 1], port)),
            LineEnding::CrLf,
            |request, _peer| async move {
                assert_eq!(request, "READ VALUE");
                Ok("42.5".to_string())
            },
        );

        let config = ascii_tcp::AsciiTcpConfig {
            host: "127.0.0.1".to_string(),
            port,
            connect_timeout: Duration::from_millis(200),
            io_timeout: Duration::from_millis(200),
            line_ending: LineEnding::CrLf,
        };

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let response = loop {
            match ascii_tcp::exchange_line(&config, "READ VALUE").await {
                Ok(response) => break response,
                Err(ascii_tcp::AsciiTcpError::ConnectTimeout) => {
                    assert!(std::time::Instant::now() < deadline, "server did not start");
                    tokio::task::yield_now().await;
                }
                Err(ascii_tcp::AsciiTcpError::Io(error))
                    if error.kind() == std::io::ErrorKind::ConnectionRefused =>
                {
                    assert!(std::time::Instant::now() < deadline, "server did not start");
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("CRLF exchange failed: {error}"),
            }
        };

        assert_eq!(response, "42.5");
        handle.abort();
    }

    #[tokio::test]
    async fn handler_errors_are_answered_without_closing_the_connection() {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
        let port = probe.local_addr().expect("probe address").port();
        drop(probe);

        let handle = mycela::ascii_tcp_server::start_server_with_line_ending(
            SocketAddr::from(([127, 0, 0, 1], port)),
            LineEnding::CrLf,
            |request, _peer| async move {
                if request == "BAD" {
                    Err("unknown command".to_string())
                } else {
                    Ok(format!("{request}=1"))
                }
            },
        );

        let channel: ascii_tcp::ChannelConfig = serde_json::from_value(serde_json::json!({
            "host": "127.0.0.1", "port": port, "line_ending": "crlf",
            "connect_timeout_ms": 200, "io_timeout_ms": 500
        }))
        .unwrap();
        let ctx = ascii_tcp::Context::default();

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let reply = loop {
            match ctx.execute_command_matching(&channel.command("BAD"), |_| true).await {
                Ok(reply) => break reply.response,
                Err(_) if std::time::Instant::now() < deadline => tokio::task::yield_now().await,
                Err(error) => panic!("server did not start: {error}"),
            }
        };
        assert_eq!(reply, "ERROR unknown command");

        let reply = ctx.execute_command_matching(&channel.command("A"), |_| true).await.unwrap();
        assert_eq!(reply.response, "A=1");
        handle.abort();
    }
}
