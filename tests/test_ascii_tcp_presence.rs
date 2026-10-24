#![cfg(feature = "ascii-tcp")]

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use mycela::channel::ChannelEvent;
use mycela::config::WidgetConfig;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

fn widget(port: u16, id: &str, read_command: Option<&str>, read_response: &str, mode: &str) -> WidgetConfig {
    let mut protocol = serde_json::json!({
        "type": "ascii-tcp", "host": "127.0.0.1", "port": port,
        "read_response": read_response, "line_ending": "crlf",
        "response_mode": mode, "io_timeout_ms": 5000
    });
    if let Some(command) = read_command {
        protocol["read_command"] = command.into();
    }
    serde_json::from_value(serde_json::json!({
        "id": id, "type": "hidden", "label": id, "data_type": "boolean", "protocol": protocol
    }))
    .unwrap()
}

/// Device that sends a banner, answers `HVE?`, ignores everything else and sends `ALIVE` every 200 ms.
async fn spawn_device() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        writer.write_all(b"TCP client connected from 1.2.3.4\r\n").await.unwrap();
        let mut lines = BufReader::new(reader).lines();
        let mut beat = tokio::time::interval(Duration::from_millis(200));
        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let Ok(Some(line)) = line else { return };
                    if line == "HVE?" {
                        writer.write_all(b"HVE=0\r\n").await.unwrap();
                    }
                }
                _ = beat.tick() => { writer.write_all(b"ALIVE\r\n").await.unwrap(); }
            }
        }
    });
    port
}

/// A presence widget without a `read_command` listens for unsolicited lines and must
/// report every one, even while other widgets poll (and one is ignored) on the shared connection.
#[tokio::test]
async fn presence_widget_without_read_command_reports_every_pulse() {
    let port = spawn_device().await;
    let ctx = Arc::new(ascii_tcp::Context::default());

    for (id, command, response) in [("hve", "HVE?", "HVE=%d"), ("ignored", "XINTL?", "XINTL=%d")] {
        let mut poller = Box::pin(mycela::ascii_tcp_client::ascii_tcp_stream(
            Arc::new(widget(port, id, Some(command), response, "number")),
            ctx.clone(),
        ));
        tokio::spawn(async move { while poller.next().await.is_some() {} });
    }

    let mut stream = Box::pin(mycela::ascii_tcp_client::ascii_tcp_stream(
        Arc::new(widget(port, "heartbeat_pulse", None, "ALIVE", "presence")),
        ctx,
    ));

    let mut pulses = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline, stream.next()).await {
        if matches!(event, ChannelEvent::Value(_)) {
            pulses += 1;
        }
    }
    assert!(pulses >= 5, "only {pulses} pulses in 1.5 s at one ALIVE per 200 ms");
}
