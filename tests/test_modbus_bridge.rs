#![cfg(all(feature = "modbus", feature = "modbus-server"))]

mod test_modbus_bridge {
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use futures::future::BoxFuture;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_modbus::prelude::ExceptionCode;

    use mycela::channel::{ChannelContext, ChannelValue};
    use mycela::config::{
        ModbusBridgeAccessMode, ModbusBridgeReadMode, ModbusBridgeRegisterMap,
        ModbusBridgeUpstreamConfig, ModbusRegisterType,
    };
    use mycela::modbus::bridge::BridgeBackend;
    use mycela::modbus::server::{
        start_modbus_server, MemoryBackend, RegisterBackend, UnitRouter, WriteAccessKind,
    };

    /// The "real device" behind the bridge: memory that counts reads and can be made to fail.
    struct Device {
        memory: Arc<MemoryBackend>,
        reads: AtomicUsize,
        failing: AtomicBool,
    }

    impl RegisterBackend for Device {
        fn read(&self, addr: u16, count: u16) -> BoxFuture<'_, Result<Vec<u16>, ExceptionCode>> {
            Box::pin(async move {
                self.reads.fetch_add(1, Ordering::Relaxed);
                if self.failing.load(Ordering::Relaxed) {
                    return Err(ExceptionCode::ServerDeviceFailure);
                }
                self.memory.read(addr, count).await
            })
        }

        fn write(
            &self,
            kind: WriteAccessKind,
            addr: u16,
            values: Vec<u16>,
        ) -> BoxFuture<'_, Result<(), ExceptionCode>> {
            self.memory.write(kind, addr, values)
        }
    }

    impl Device {
        fn word(&self, addr: u16) -> u16 {
            self.memory.get(addr)
        }
    }

    fn channel_ctx() -> Arc<ChannelContext> {
        #[cfg(feature = "epics-pvxs")]
        let epics_ctx = Arc::new(std::sync::Mutex::new(
            mycela::pvxs::Context::from_env().expect("pvxs context required"),
        ));
        let modbus_pool = mycela::modbus::pool::ModbusPool::new();

        #[cfg(feature = "epics-pvxs")]
        {
            ChannelContext::new(epics_ctx, modbus_pool)
        }
        #[cfg(not(feature = "epics-pvxs"))]
        {
            ChannelContext::new(modbus_pool)
        }
    }

    async fn free_addr() -> SocketAddr {
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        probe.local_addr().unwrap()
    }

    async fn start_device(addr: SocketAddr, initial: &[(u16, u16)]) -> Arc<Device> {
        let device = Arc::new(Device {
            memory: MemoryBackend::new(),
            reads: AtomicUsize::new(0),
            failing: AtomicBool::new(false),
        });
        for &(addr, value) in initial {
            device.memory.set(addr, value);
        }
        start_modbus_server(addr, UnitRouter::single(1, device.clone()));
        for _ in 0..100 {
            if TcpStream::connect(addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        device
    }

    fn upstream(addr: SocketAddr) -> Option<ModbusBridgeUpstreamConfig> {
        Some(ModbusBridgeUpstreamConfig {
            host: addr.ip().to_string(),
            port: addr.port(),
            unit_id: 1,
        })
    }

    fn mapping(
        exposed: u16,
        word_count: u8,
        upstream_register: u16,
        access: ModbusBridgeAccessMode,
    ) -> ModbusBridgeRegisterMap {
        ModbusBridgeRegisterMap {
            exposed_register: exposed,
            register_type: ModbusRegisterType::HoldingRegister,
            word_count,
            source_widget_id: None,
            source_upstream_register: Some(upstream_register),
            source_upstream_register_type: Some(ModbusRegisterType::HoldingRegister),
            target_upstream_register: Some(upstream_register),
            access,
        }
    }

    fn backend(
        ctx: Arc<ChannelContext>,
        upstream: Option<ModbusBridgeUpstreamConfig>,
        mappings: &[ModbusBridgeRegisterMap],
    ) -> BridgeBackend {
        BridgeBackend::new(ctx, upstream, mappings, &HashMap::new(), &HashMap::new())
    }

    #[tokio::test]
    async fn upstream_registers_are_read_through_and_neighbours_share_one_upstream_read() {
        let addr = free_addr().await;
        let device = start_device(addr, &[(100, 10), (101, 11), (102, 12)]).await;
        let bridge = backend(
            channel_ctx(),
            upstream(addr),
            &[
                mapping(0, 2, 100, ModbusBridgeAccessMode::ReadOnly),
                mapping(2, 1, 102, ModbusBridgeAccessMode::ReadOnly),
            ],
        );

        // Exposed 0..3 are mapped; 3 is not and reads as 0.
        assert_eq!(bridge.read(0, 4).await.unwrap(), vec![10, 11, 12, 0]);
        // Upstream registers 100..103 are adjacent, so one upstream block read served both.
        assert_eq!(device.reads.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn partial_ranges_return_only_the_requested_words() {
        let addr = free_addr().await;
        let _device = start_device(addr, &[(100, 10), (101, 11)]).await;
        let bridge = backend(
            channel_ctx(),
            upstream(addr),
            &[mapping(0, 2, 100, ModbusBridgeAccessMode::ReadOnly)],
        );

        assert_eq!(bridge.read(1, 1).await.unwrap(), vec![11]);
    }

    #[tokio::test]
    async fn widget_sourced_registers_follow_the_widget_value_bus() {
        let ctx = channel_ctx();
        let mut widget_mapping = mapping(5, 1, 0, ModbusBridgeAccessMode::ReadOnly);
        widget_mapping.source_widget_id = Some("w1".to_string());
        widget_mapping.source_upstream_register = None;
        let bridge = backend(ctx.clone(), None, &[widget_mapping]);

        assert_eq!(bridge.read(5, 1).await.unwrap(), vec![0]);
        ctx.publish_widget_value(
            "w1",
            ChannelValue {
                raw_value: 42.0,
                ..ChannelValue::default()
            },
        );
        assert_eq!(bridge.read(5, 1).await.unwrap(), vec![42]);
    }

    #[tokio::test]
    async fn an_unreachable_upstream_is_a_server_device_failure() {
        let addr = free_addr().await; // nothing listens here
        let bridge = backend(
            channel_ctx(),
            upstream(addr),
            &[mapping(0, 1, 100, ModbusBridgeAccessMode::ReadOnly)],
        );

        let result = tokio::time::timeout(Duration::from_secs(5), bridge.read(0, 1))
            .await
            .expect("read must fail fast, not hang");
        assert_eq!(result, Err(ExceptionCode::ServerDeviceFailure));
    }

    #[tokio::test]
    async fn writes_are_forwarded_upstream_and_failures_are_reported() {
        let addr = free_addr().await;
        let device = start_device(addr, &[]).await;
        let bridge = backend(
            channel_ctx(),
            upstream(addr),
            &[mapping(10, 2, 200, ModbusBridgeAccessMode::ReadWrite)],
        );

        bridge
            .write(WriteAccessKind::MultiRegister, 10, vec![1, 2])
            .await
            .unwrap();
        assert_eq!((device.word(200), device.word(201)), (1, 2));

        // Writing one word of a two-word value keeps the other word as it is upstream.
        bridge
            .write(WriteAccessKind::SingleRegister, 11, vec![9])
            .await
            .unwrap();
        assert_eq!((device.word(200), device.word(201)), (1, 9));

        // Upstream going away surfaces to the client instead of being silently dropped.
        let dead = backend(
            channel_ctx(),
            upstream(free_addr().await),
            &[mapping(10, 1, 200, ModbusBridgeAccessMode::ReadWrite)],
        );
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            dead.write(WriteAccessKind::SingleRegister, 10, vec![1]),
        )
        .await
        .expect("write must fail fast, not hang");
        assert_eq!(result, Err(ExceptionCode::ServerDeviceFailure));
    }

    #[tokio::test]
    async fn read_only_unmapped_and_mismatched_writes_are_rejected_untouched() {
        let addr = free_addr().await;
        let device = start_device(addr, &[]).await;
        let bridge = backend(
            channel_ctx(),
            upstream(addr),
            &[
                mapping(0, 1, 300, ModbusBridgeAccessMode::ReadOnly),
                mapping(10, 1, 310, ModbusBridgeAccessMode::ReadWrite),
            ],
        );

        // Read-only mapping.
        assert_eq!(
            bridge.write(WriteAccessKind::SingleRegister, 0, vec![1]).await,
            Err(ExceptionCode::IllegalFunction)
        );
        // Unmapped address.
        assert_eq!(
            bridge.write(WriteAccessKind::SingleRegister, 50, vec![1]).await,
            Err(ExceptionCode::IllegalFunction)
        );
        // Coil write to a holding register.
        assert_eq!(
            bridge.write(WriteAccessKind::SingleCoil, 10, vec![1]).await,
            Err(ExceptionCode::IllegalFunction)
        );
        // One bad word rejects the whole request before any word is forwarded.
        assert_eq!(
            bridge.write(WriteAccessKind::MultiRegister, 10, vec![7, 8]).await,
            Err(ExceptionCode::IllegalFunction)
        );
        assert_eq!((device.word(300), device.word(310), device.word(311)), (0, 0, 0));
    }

    // ── live vs cached reads ──────────────────────────────────────────────────

    fn cached(
        ctx: Arc<ChannelContext>,
        addr: SocketAddr,
        mappings: &[ModbusBridgeRegisterMap],
        ttl: Duration,
    ) -> BridgeBackend {
        backend(ctx, upstream(addr), mappings).with_read_mode(ModbusBridgeReadMode::Cached, ttl)
    }

    #[tokio::test]
    async fn live_mode_reads_upstream_every_time() {
        let addr = free_addr().await;
        let device = start_device(addr, &[(100, 7)]).await;
        let bridge = backend(
            channel_ctx(),
            upstream(addr),
            &[mapping(0, 1, 100, ModbusBridgeAccessMode::ReadOnly)],
        );

        bridge.read(0, 1).await.unwrap();
        device.memory.set(100, 8);
        assert_eq!(bridge.read(0, 1).await.unwrap(), vec![8]);
        assert_eq!(device.reads.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn cached_mode_serves_a_value_younger_than_the_ttl_without_asking_upstream() {
        let addr = free_addr().await;
        let device = start_device(addr, &[(100, 7)]).await;
        let bridge = cached(
            channel_ctx(),
            addr,
            &[mapping(0, 1, 100, ModbusBridgeAccessMode::ReadOnly)],
            Duration::from_secs(30),
        );

        assert_eq!(bridge.read(0, 1).await.unwrap(), vec![7]);
        device.memory.set(100, 8);
        assert_eq!(bridge.read(0, 1).await.unwrap(), vec![7], "within the TTL the cached value is served");
        assert_eq!(device.reads.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn cached_mode_refreshes_once_the_ttl_is_exceeded() {
        let addr = free_addr().await;
        let device = start_device(addr, &[(100, 7)]).await;
        let bridge = cached(
            channel_ctx(),
            addr,
            &[mapping(0, 1, 100, ModbusBridgeAccessMode::ReadOnly)],
            Duration::from_millis(200),
        );

        assert_eq!(bridge.read(0, 1).await.unwrap(), vec![7]);
        device.memory.set(100, 8);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(bridge.read(0, 1).await.unwrap(), vec![8]);
        assert_eq!(device.reads.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn cached_mode_never_serves_a_stale_value_past_the_ttl() {
        let addr = free_addr().await;
        let device = start_device(addr, &[(100, 7)]).await;
        let bridge = cached(
            channel_ctx(),
            addr,
            &[mapping(0, 1, 100, ModbusBridgeAccessMode::ReadOnly)],
            Duration::from_millis(200),
        );

        assert_eq!(bridge.read(0, 1).await.unwrap(), vec![7]);
        device.failing.store(true, Ordering::Relaxed);
        assert_eq!(bridge.read(0, 1).await.unwrap(), vec![7], "still within the TTL");

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(bridge.read(0, 1).await, Err(ExceptionCode::ServerDeviceFailure));

        // Recovery needs no special handling.
        device.failing.store(false, Ordering::Relaxed);
        assert_eq!(bridge.read(0, 1).await.unwrap(), vec![7]);
    }

    #[tokio::test]
    async fn cached_mode_lets_concurrent_readers_share_one_upstream_read() {
        let addr = free_addr().await;
        let device = start_device(addr, &[(100, 7)]).await;
        let bridge = Arc::new(cached(
            channel_ctx(),
            addr,
            &[mapping(0, 1, 100, ModbusBridgeAccessMode::ReadOnly)],
            Duration::from_secs(30),
        ));

        let readers: Vec<_> = (0..8)
            .map(|_| {
                let bridge = bridge.clone();
                tokio::spawn(async move { bridge.read(0, 1).await })
            })
            .collect();
        for reader in readers {
            assert_eq!(reader.await.unwrap().unwrap(), vec![7]);
        }
        assert_eq!(device.reads.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn a_write_drops_the_cached_value_so_the_next_read_sees_it() {
        let addr = free_addr().await;
        let device = start_device(addr, &[(200, 1)]).await;
        let bridge = cached(
            channel_ctx(),
            addr,
            &[mapping(10, 1, 200, ModbusBridgeAccessMode::ReadWrite)],
            Duration::from_secs(30),
        );

        assert_eq!(bridge.read(10, 1).await.unwrap(), vec![1]);
        bridge
            .write(WriteAccessKind::SingleRegister, 10, vec![5])
            .await
            .unwrap();
        assert_eq!(device.word(200), 5);
        assert_eq!(bridge.read(10, 1).await.unwrap(), vec![5], "read-after-write must not be stale");
    }

    #[tokio::test]
    async fn cached_mode_does_not_cache_widget_sourced_registers() {
        let ctx = channel_ctx();
        let mut widget_mapping = mapping(5, 1, 0, ModbusBridgeAccessMode::ReadOnly);
        widget_mapping.source_widget_id = Some("w2".to_string());
        widget_mapping.source_upstream_register = None;
        let bridge = backend(ctx.clone(), None, &[widget_mapping])
            .with_read_mode(ModbusBridgeReadMode::Cached, Duration::from_secs(30));

        let publish = |raw| {
            ctx.publish_widget_value(
                "w2",
                ChannelValue {
                    raw_value: raw,
                    ..ChannelValue::default()
                },
            )
        };
        publish(1.0);
        assert_eq!(bridge.read(5, 1).await.unwrap(), vec![1]);
        publish(2.0);
        assert_eq!(bridge.read(5, 1).await.unwrap(), vec![2]);
    }
}
