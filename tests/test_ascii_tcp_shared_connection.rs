#![cfg(feature = "ascii-tcp")]

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use mycela::channel::ChannelEvent;
use mycela::config::{ProtocolConfig, WidgetConfig};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

fn widget(port: u16, id: &str, protocol: serde_json::Value, data_type: &str) -> WidgetConfig {
    let mut base = serde_json::json!({
        "type": "ascii-tcp", "host": "127.0.0.1", "port": port,
        "line_ending": "crlf", "min_poll_interval_ms": 50, "io_timeout_ms": 1000
    });
    base.as_object_mut().unwrap().extend(protocol.as_object().unwrap().clone());
    serde_json::from_value(serde_json::json!({
        "id": id, "type": "text_update", "label": id, "data_type": data_type, "protocol": base
    }))
    .unwrap()
}

/// Tagged request/response device: `NAME=value` replies arrive in request order, writes are
/// echoed and `IGNORE?` is never answered.
async fn spawn_device() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let reply = match line.as_str() {
                "NUM?" => "NUM=42".to_string(),
                "FLAG?" => "FLAG=ON".to_string(),
                "IGNORE?" => continue,
                _ => line,
            };
            if writer.write_all(format!("{reply}\r\n").as_bytes()).await.is_err() {
                return;
            }
        }
    });
    port
}

/// Widgets on one endpoint share its connection; their `read_response` templates keep replies
/// apart, and a command the device ignores must not stall the others.
#[tokio::test]
async fn concurrent_polls_and_writes_on_a_shared_connection_get_their_own_replies() {
    let port = spawn_device().await;
    let ctx = Arc::new(ascii_tcp::Context::default());

    let mut ignored = Box::pin(mycela::ascii_tcp_client::ascii_tcp_stream(
        Arc::new(widget(port, "ignored", serde_json::json!({
            "read_command": "IGNORE?", "read_response": "IGNORE=%f", "response_mode": "number"
        }), "double")),
        ctx.clone(),
    ));
    tokio::spawn(async move { while ignored.next().await.is_some() {} });

    let mut streams = Vec::new();
    for i in 0..4 {
        for (command, template, mode, data_type, expected) in [
            ("NUM?", "NUM=%f", "number", "double", "42.00"),
            ("FLAG?", "FLAG=%s", "bool", "bool", "1"),
        ] {
            let protocol = serde_json::json!({
                "read_command": command, "read_response": template, "response_mode": mode
            });
            let config = Arc::new(widget(port, &format!("{command}{i}"), protocol, data_type));
            streams.push((
                Box::pin(mycela::ascii_tcp_client::ascii_tcp_stream(config, ctx.clone())),
                expected,
            ));
        }
    }

    let writer = {
        let ctx = ctx.clone();
        let config = widget(port, "w", serde_json::json!({
            "write_command": "SET FLAG {value}", "response_mode": "bool"
        }), "bool");
        let Some(ProtocolConfig::AsciiTcp(a)) = config.protocol else { unreachable!() };
        tokio::spawn(async move {
            for _ in 0..10 {
                ctx.write_channel(&a, "1").await?;
            }
            Ok::<_, ascii_tcp::AsciiTcpError>(())
        })
    };

    // Widgets drive their streams concurrently, and a stream only polls while it is driven.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(600);
    let readers: Vec<_> = streams
        .into_iter()
        .map(|(mut stream, expected)| {
            tokio::spawn(async move {
                let mut values = 0;
                while let Ok(Some(event)) = tokio::time::timeout_at(deadline, stream.next()).await {
                    match event {
                        ChannelEvent::Value(cv) => {
                            assert_eq!(cv.value_str, expected);
                            values += 1;
                        }
                        ChannelEvent::Connected => {}
                        other => panic!("unexpected event: {other:?}"),
                    }
                }
                assert!(values >= 1, "no value received for expected '{expected}'");
            })
        })
        .collect();
    for reader in readers {
        reader.await.unwrap();
    }
    writer.await.unwrap().expect("echo-acknowledged writes succeed");
}

/// Polling must stop once the widget drops its stream, even when the value never changes.
#[tokio::test]
async fn dropping_the_stream_stops_polling() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = requests.clone();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(_)) = lines.next_line().await {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if writer.write_all(b"NUM=42\r\n").await.is_err() {
                return;
            }
        }
    });

    let config = widget(port, "num", serde_json::json!({
        "read_command": "NUM?", "read_response": "NUM=%f", "response_mode": "number"
    }), "double");
    let mut stream = Box::pin(mycela::ascii_tcp_client::ascii_tcp_stream(
        Arc::new(config),
        Arc::new(ascii_tcp::Context::default()),
    ));
    while !matches!(stream.next().await, Some(ChannelEvent::Value(_))) {}
    drop(stream);

    tokio::time::sleep(Duration::from_millis(100)).await;
    let after_drop = requests.load(std::sync::atomic::Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), after_drop);
}
