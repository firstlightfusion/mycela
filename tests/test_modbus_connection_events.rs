#![cfg(feature = "modbus")]

mod test_modbus_connection_events {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_stream::StreamExt;

    use mycela::channel::ChannelEvent;
    use mycela::config::{ModbusTcpConfig, ModbusRegisterType, ProtocolConfig, WidgetConfig, WidgetType};
    use mycela::modbus::client::modbus_stream;
    use mycela::modbus::pool::{ModbusError, ModbusPool};

    // ── in-process mock Modbus TCP server ─────────────────────────────────────

    /// Binds to an OS-assigned port and loops forever accepting connections,
    /// serving every FC=0x03 read with a fixed register value of 1234.
    /// Abort the returned `JoinHandle` to stop the server.
    async fn start_mock_server() -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            loop {
                if let Ok((socket, _)) = listener.accept().await {
                    tokio::spawn(serve_modbus(socket, None, None));
                }
            }
        });
        (port, handle)
    }

    /// Binds to an OS-assigned port, accepts exactly ONE connection, serves at
    /// most `max_requests` reads on it, then drops the socket (closing the
    /// connection) and the listener (so reconnect attempts fail immediately).
    async fn start_one_shot_server(max_requests: usize) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((socket, _)) = listener.accept().await {
                serve_modbus(socket, Some(max_requests), None).await;
            }
            // listener dropped here — subsequent connect attempts get "connection refused"
        });
        port
    }

    /// Respond to FC=0x03 (Read Holding Registers) requests with value 1234.
    /// If `max` is `Some(n)`, exits after serving `n` requests (dropping the socket).
    async fn start_counting_mock_server() -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let request_count = Arc::new(AtomicUsize::new(0));
        let server_count = request_count.clone();
        tokio::spawn(async move {
            loop {
                if let Ok((socket, _)) = listener.accept().await {
                    tokio::spawn(serve_modbus(socket, None, Some(server_count.clone())));
                }
            }
        });
        (port, request_count)
    }

    /// Accepts connections forever; returns the port, the number of accepted
    /// connections, and every `(unit_id, register_count)` request frame seen.
    async fn start_instrumented_server() -> (
        u16,
        Arc<AtomicUsize>,
        Arc<std::sync::Mutex<Vec<(u8, u16)>>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepts = Arc::new(AtomicUsize::new(0));
        let frames = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (server_accepts, server_frames) = (accepts.clone(), frames.clone());
        tokio::spawn(async move {
            loop {
                if let Ok((mut socket, _)) = listener.accept().await {
                    server_accepts.fetch_add(1, Ordering::Relaxed);
                    let frames = server_frames.clone();
                    tokio::spawn(async move {
                        serve_modbus_recording(&mut socket, None, None, Some(frames)).await
                    });
                }
            }
        });
        (port, accepts, frames)
    }

    async fn serve_modbus(
        mut socket: TcpStream,
        max: Option<usize>,
        request_count: Option<Arc<AtomicUsize>>,
    ) {
        serve_modbus_recording(&mut socket, max, request_count, None).await
    }

    /// Like [`serve_modbus`], additionally recording `(unit_id, register_count)`
    /// of every request frame it sees.
    async fn serve_modbus_recording(
        socket: &mut TcpStream,
        max: Option<usize>,
        request_count: Option<Arc<AtomicUsize>>,
        frames: Option<Arc<std::sync::Mutex<Vec<(u8, u16)>>>>,
    ) {
        let mut buf = vec![0u8; 256];
        let mut served = 0usize;
        loop {
            if max.is_some_and(|m| served >= m) {
                break;
            }
            let n = match socket.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if n < 12 {
                continue;
            }
            if let Some(request_count) = &request_count {
                request_count.fetch_add(1, Ordering::Relaxed);
            }
            let txn_id  = u16::from_be_bytes([buf[0], buf[1]]);
            let unit_id = buf[6];
            let fc      = buf[7];
            let count   = u16::from_be_bytes([buf[10], buf[11]]);
            let start   = u16::from_be_bytes([buf[8], buf[9]]);
            if let Some(frames) = &frames {
                frames.lock().unwrap().push((unit_id, count));
            }

            // Address 666 is "unmapped": any read covering it gets IllegalDataAddress.
            let covers_unmapped = (start as u32..start as u32 + count as u32).contains(&666);

            let pdu: Vec<u8> = if fc == 0x03 && covers_unmapped {
                vec![fc | 0x80, 0x02]
            } else if fc == 0x03 {
                let bc = (count * 2) as u8;
                let mut p = vec![0x03, bc];
                for _ in 0..count {
                    p.extend_from_slice(&1234u16.to_be_bytes());
                }
                p
            } else {
                vec![fc | 0x80, 0x01] // Modbus exception for any other FC
            };

            let len = (1 + pdu.len()) as u16;
            let mut resp = Vec::new();
            resp.extend_from_slice(&txn_id.to_be_bytes());
            resp.extend_from_slice(&0u16.to_be_bytes()); // protocol id
            resp.extend_from_slice(&len.to_be_bytes());
            resp.push(unit_id);
            resp.extend_from_slice(&pdu);

            if socket.write_all(&resp).await.is_err() {
                break;
            }
            served += 1;
        }
    }

    // ── helpers ───────────────────────────────────────────────────────────────

    fn widget_cfg(port: u16) -> Arc<WidgetConfig> {
        Arc::new(WidgetConfig {
            id: "mb-test".to_string(),
            widget_type: WidgetType::TextUpdate,
            label: "test".to_string(),
            protocol: Some(ProtocolConfig::ModbusTcp(ModbusTcpConfig {
                host: "127.0.0.1".to_string(),
                port,
                unit_id: 1,
                register: 1000,
                register_type: ModbusRegisterType::HoldingRegister,
                min_poll_interval_ms: 50,
                scale: 1.0,
                offset: 0.0,
                word_count: 1,
                bit_index: None,
            })),
            ..Default::default()
        })
    }

    async fn next_event(
        stream: &mut (impl tokio_stream::Stream<Item = ChannelEvent> + Unpin),
    ) -> ChannelEvent {
        tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("timed out waiting for channel event")
            .expect("stream ended unexpectedly")
    }

    // ── pool unit tests ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn pool_get_or_create_deduplicates_same_key() {
        let pool = ModbusPool::new();
        let h1 = pool.get_or_create("127.0.0.1", 9901, 1);
        let h2 = pool.get_or_create("127.0.0.1", 9901, 1);
        assert!(Arc::ptr_eq(&h1, &h2), "same key should return the same Arc");
    }

    #[tokio::test]
    async fn pool_get_or_create_deduplicates_concurrent_calls() {
        let pool = ModbusPool::new();
        let mut tasks = Vec::new();
        for _ in 0..32 {
            let pool = pool.clone();
            tasks.push(tokio::spawn(async move {
                pool.get_or_create("127.0.0.1", 9910, 1)
            }));
        }

        let first = tasks.remove(0).await.unwrap();
        for task in tasks {
            assert!(Arc::ptr_eq(&first, &task.await.unwrap()));
        }
    }

    #[tokio::test]
    async fn concurrent_identical_reads_are_coalesced() {
        let (port, request_count) = start_counting_mock_server().await;
        let pool = ModbusPool::new();
        let handle = pool.get_or_create("127.0.0.1", port, 1);

        let (first, second) = tokio::join!(
            handle.read(1000, ModbusRegisterType::HoldingRegister, 1),
            handle.read(1000, ModbusRegisterType::HoldingRegister, 1),
        );

        assert_eq!(first.unwrap(), vec![1234]);
        assert_eq!(second.unwrap(), vec![1234]);
        assert_eq!(request_count.load(Ordering::Relaxed), 1);
        assert_eq!(pool.metrics().snapshot().modbus_queue_depth, 0);
    }

    #[tokio::test]
    async fn pool_different_keys_return_different_handles() {
        let pool = ModbusPool::new();
        let h1 = pool.get_or_create("127.0.0.1", 9902, 1);
        let h2 = pool.get_or_create("127.0.0.1", 9903, 1);
        assert!(!Arc::ptr_eq(&h1, &h2), "different keys should return distinct Arcs");
    }

    #[tokio::test]
    async fn pool_handle_not_closed_when_newly_created() {
        let pool = ModbusPool::new();
        let h = pool.get_or_create("127.0.0.1", 9904, 1);
        assert!(!h.is_closed(), "freshly spawned device task should be running");
    }

    #[tokio::test]
    async fn pool_handle_is_closed_after_disconnect_all() {
        let pool = ModbusPool::new();
        let h = pool.get_or_create("127.0.0.1", 9905, 1);
        assert!(!h.is_closed());

        pool.disconnect_all();
        // Yield to let the tokio runtime process the abort.
        tokio::task::yield_now().await;

        assert!(h.is_closed(), "handle sender should be closed after disconnect_all");
    }

    #[tokio::test]
    async fn pool_get_or_create_returns_fresh_open_handle_after_disconnect_all() {
        let pool = ModbusPool::new();
        let old = pool.get_or_create("127.0.0.1", 9906, 1);
        pool.disconnect_all();
        tokio::task::yield_now().await;

        let new = pool.get_or_create("127.0.0.1", 9906, 1);

        assert!(old.is_closed(), "old handle should be closed");
        assert!(!new.is_closed(), "new handle should have a live task");
        assert!(!Arc::ptr_eq(&old, &new), "should be different Arc instances");
    }

    #[tokio::test]
    async fn modbus_exception_does_not_drop_the_connection() {
        let (port, accepts, _frames) = start_instrumented_server().await;
        let pool = ModbusPool::new();
        let handle = pool.get_or_create("127.0.0.1", port, 1);

        // The mock answers every function code except 0x03 with an exception.
        let rejected = handle.read(1000, ModbusRegisterType::InputRegister, 1).await;
        assert!(
            matches!(rejected, Err(ModbusError::Exception(_))),
            "expected an exception, got {rejected:?}"
        );
        assert!(handle.is_connected(), "an exception must not mark the device down");

        let ok = handle.read(1000, ModbusRegisterType::HoldingRegister, 1).await;
        assert_eq!(ok.unwrap(), vec![1234]);
        assert_eq!(accepts.load(Ordering::Relaxed), 1, "connection must be reused");
    }

    #[tokio::test]
    async fn one_bad_register_does_not_fail_its_merged_neighbours() {
        let (port, accepts, frames) = start_instrumented_server().await;
        let pool = ModbusPool::new();
        let handle = pool.get_or_create("127.0.0.1", port, 1);

        // Merged into one 3-register read that the device rejects (666 is
        // unmapped); the neighbours must then be retried on their own.
        let (before, bad, after) = tokio::join!(
            handle.read(665, ModbusRegisterType::HoldingRegister, 1),
            handle.read(666, ModbusRegisterType::HoldingRegister, 1),
            handle.read(667, ModbusRegisterType::HoldingRegister, 1),
        );
        assert_eq!(before.unwrap(), vec![1234]);
        assert!(matches!(bad, Err(ModbusError::Exception(_))));
        assert_eq!(after.unwrap(), vec![1234]);
        assert_eq!(accepts.load(Ordering::Relaxed), 1, "connection must survive");
        assert_eq!(frames.lock().unwrap().len(), 4, "1 merged read + 3 individual retries");
        assert_eq!(pool.metrics().snapshot().modbus_queue_depth, 0);
    }

    #[tokio::test]
    async fn units_on_one_address_share_a_single_connection() {
        let (port, accepts, frames) = start_instrumented_server().await;
        let pool = ModbusPool::new();
        let unit1 = pool.get_or_create("127.0.0.1", port, 1);
        let unit2 = pool.get_or_create("127.0.0.1", port, 2);
        assert!(!Arc::ptr_eq(&unit1, &unit2));

        unit1.read(10, ModbusRegisterType::HoldingRegister, 1).await.unwrap();
        unit2.read(10, ModbusRegisterType::HoldingRegister, 1).await.unwrap();

        assert_eq!(accepts.load(Ordering::Relaxed), 1);
        let units: Vec<u8> = frames.lock().unwrap().iter().map(|(unit, _)| *unit).collect();
        assert_eq!(units, vec![1, 2], "each request must carry its own unit id");
    }

    #[tokio::test]
    async fn adjacent_reads_are_merged_into_one_request() {
        let (port, _accepts, frames) = start_instrumented_server().await;
        let pool = ModbusPool::new();
        let handle = pool.get_or_create("127.0.0.1", port, 1);

        let (a, b, c) = tokio::join!(
            handle.read(100, ModbusRegisterType::HoldingRegister, 1),
            handle.read(101, ModbusRegisterType::HoldingRegister, 2),
            handle.read(103, ModbusRegisterType::HoldingRegister, 1),
        );

        assert_eq!(a.unwrap(), vec![1234]);
        assert_eq!(b.unwrap(), vec![1234, 1234]);
        assert_eq!(c.unwrap(), vec![1234]);
        assert_eq!(*frames.lock().unwrap(), vec![(1, 4)], "one 4-register read expected");
        assert_eq!(pool.metrics().snapshot().modbus_queue_depth, 0);
    }

    #[tokio::test]
    async fn reads_with_a_gap_stay_separate() {
        let (port, _accepts, frames) = start_instrumented_server().await;
        let pool = ModbusPool::new();
        let handle = pool.get_or_create("127.0.0.1", port, 1);

        let (a, b) = tokio::join!(
            handle.read(100, ModbusRegisterType::HoldingRegister, 1),
            handle.read(105, ModbusRegisterType::HoldingRegister, 1),
        );

        a.unwrap();
        b.unwrap();
        assert_eq!(frames.lock().unwrap().len(), 2);
        assert_eq!(pool.metrics().snapshot().modbus_queue_depth, 0);
    }

    #[tokio::test]
    async fn connection_state_follows_the_socket() {
        let port = start_one_shot_server(1).await;
        let pool = ModbusPool::new();
        let handle = pool.get_or_create("127.0.0.1", port, 1);
        let mut state = handle.connection_state();
        assert!(!handle.is_connected(), "no connection before the first request is served");

        handle.read(1000, ModbusRegisterType::HoldingRegister, 1).await.unwrap();
        assert!(handle.is_connected());

        // The server closes after one reply and stops listening. A quiet link is
        // only noticed by the next request, which is what polling provides.
        let dead = handle.read(1000, ModbusRegisterType::HoldingRegister, 1).await;
        assert!(matches!(dead, Err(ModbusError::Connection(_))), "got {dead:?}");
        tokio::time::timeout(Duration::from_secs(5), state.wait_for(|up| !*up))
            .await
            .expect("timed out waiting for the device to be reported down")
            .expect("connection task ended");
        assert!(!handle.is_connected());
    }

    #[tokio::test]
    async fn stream_reconnects_after_disconnect_all() {
        let (port, _server) = start_mock_server().await;
        let pool = ModbusPool::new();
        let mut stream = Box::pin(modbus_stream(widget_cfg(port), pool.clone()));

        assert!(matches!(next_event(&mut stream).await, ChannelEvent::Connected));
        pool.disconnect_all();

        let mut saw_disconnect = false;
        loop {
            match next_event(&mut stream).await {
                ChannelEvent::Disconnected(_) => saw_disconnect = true,
                ChannelEvent::Connected => break,
                ChannelEvent::Value(_) => {}
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert!(saw_disconnect, "widget must see the disconnect before it reconnects");
    }

    // ── stream event tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn stream_emits_connected_then_value_when_server_is_up() {
        let (port, _server) = start_mock_server().await;
        let pool   = ModbusPool::new();
        let config = widget_cfg(port);
        let mut stream = Box::pin(modbus_stream(config, pool));

        let ev = next_event(&mut stream).await;
        assert!(
            matches!(ev, ChannelEvent::Connected),
            "first event should be Connected, got {:?}",
            ev
        );

        let ev = next_event(&mut stream).await;
        assert!(
            matches!(ev, ChannelEvent::Value(_)),
            "second event should be Value, got {:?}",
            ev
        );
    }

    #[tokio::test]
    async fn stream_emits_disconnected_after_server_closes_connection() {
        // Server serves 2 requests then closes the socket and stops listening.
        let port = start_one_shot_server(2).await;
        let pool = ModbusPool::new();
        let config = widget_cfg(port);
        let mut stream = Box::pin(modbus_stream(config, pool));

        // Wait for the initial Connected + at least one Value.
        assert!(matches!(next_event(&mut stream).await, ChannelEvent::Connected));
        assert!(matches!(next_event(&mut stream).await, ChannelEvent::Value(_)));

        // Drain until Disconnected; skip any extra Value events that arrive before
        // the device task detects the broken connection.
        loop {
            match next_event(&mut stream).await {
                ChannelEvent::Disconnected(_) => break,
                ChannelEvent::Value(_)        => continue,
                other => panic!("unexpected event while waiting for Disconnected: {:?}", other),
            }
        }
    }
}
