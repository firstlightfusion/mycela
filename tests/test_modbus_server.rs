#![cfg(all(feature = "modbus", feature = "modbus-server"))]

mod test_modbus_server {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use futures::future::BoxFuture;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_modbus::client::{tcp, Context};
    use tokio_modbus::prelude::*;

    use mycela::modbus::server::{
        start_modbus_server, MemoryBackend, RegisterBackend, UnitRouter, WriteAccessKind,
    };

    /// 64K words of plain memory; address 500 simulates a failing device.
    struct Memory {
        words: Mutex<Vec<u16>>,
        writes: Mutex<Vec<(WriteAccessKind, u16, Vec<u16>)>>,
    }

    impl RegisterBackend for Memory {
        fn read(&self, addr: u16, count: u16) -> BoxFuture<'_, Result<Vec<u16>, ExceptionCode>> {
            Box::pin(async move {
                let end = addr as u32 + count as u32;
                if (addr as u32..end).contains(&500) {
                    return Err(ExceptionCode::ServerDeviceFailure);
                }
                let words = self.words.lock().unwrap();
                Ok(words[addr as usize..end as usize].to_vec())
            })
        }

        fn write(
            &self,
            kind: WriteAccessKind,
            addr: u16,
            values: Vec<u16>,
        ) -> BoxFuture<'_, Result<(), ExceptionCode>> {
            Box::pin(async move {
                if addr == 600 {
                    return Err(ExceptionCode::IllegalFunction);
                }
                let mut words = self.words.lock().unwrap();
                words[addr as usize..addr as usize + values.len()].copy_from_slice(&values);
                self.writes.lock().unwrap().push((kind, addr, values));
                Ok(())
            })
        }
    }

    /// Starts a server for `router` on a free port and waits until it accepts connections.
    async fn serve(router: UnitRouter) -> SocketAddr {
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = probe.local_addr().unwrap();
        drop(probe);

        start_modbus_server(addr, router);
        for _ in 0..100 {
            if TcpStream::connect(addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        addr
    }

    async fn start() -> (Context, Arc<Memory>) {
        let memory = Arc::new(Memory {
            words: Mutex::new(vec![0; 1 << 16]),
            writes: Mutex::new(Vec::new()),
        });
        let addr = serve(UnitRouter::single(1, memory.clone())).await;
        (tcp::connect(addr).await.unwrap(), memory)
    }

    #[tokio::test]
    async fn registers_and_coils_round_trip() {
        let (mut client, memory) = start().await;

        client.write_multiple_registers(10, &[1, 2, 3]).await.unwrap().unwrap();
        assert_eq!(
            client.read_holding_registers(9, 5).await.unwrap().unwrap(),
            vec![0, 1, 2, 3, 0]
        );
        assert_eq!(
            client.read_input_registers(10, 1).await.unwrap().unwrap(),
            vec![1]
        );

        client.write_single_coil(20, true).await.unwrap().unwrap();
        client.write_multiple_coils(21, &[false, true]).await.unwrap().unwrap();
        assert_eq!(
            client.read_coils(20, 3).await.unwrap().unwrap(),
            vec![true, false, true]
        );

        let writes = memory.writes.lock().unwrap();
        assert_eq!(writes[0], (WriteAccessKind::MultiRegister, 10, vec![1, 2, 3]));
        assert_eq!(writes[1], (WriteAccessKind::SingleCoil, 20, vec![1]));
        assert_eq!(writes[2], (WriteAccessKind::MultiCoil, 21, vec![0, 1]));
    }

    #[tokio::test]
    async fn read_limits_are_enforced_before_the_backend_is_asked() {
        let (mut client, _memory) = start().await;

        assert_eq!(
            client.read_holding_registers(1000, 125).await.unwrap().unwrap().len(),
            125
        );
        assert_eq!(
            client.read_holding_registers(1000, 126).await.unwrap(),
            Err(ExceptionCode::IllegalDataValue)
        );
        assert_eq!(
            client.read_holding_registers(1000, 0).await.unwrap(),
            Err(ExceptionCode::IllegalDataValue)
        );
        assert_eq!(client.read_coils(1000, 2000).await.unwrap().unwrap().len(), 2000);
        assert_eq!(
            client.read_coils(1000, 2001).await.unwrap(),
            Err(ExceptionCode::IllegalDataValue)
        );
    }

    #[tokio::test]
    async fn blocks_past_the_address_space_are_rejected() {
        let (mut client, memory) = start().await;

        assert_eq!(
            client.read_holding_registers(u16::MAX, 2).await.unwrap(),
            Err(ExceptionCode::IllegalDataAddress)
        );
        assert_eq!(
            client.write_multiple_registers(u16::MAX, &[1, 2]).await.unwrap(),
            Err(ExceptionCode::IllegalDataAddress)
        );
        assert!(memory.writes.lock().unwrap().is_empty());
        // The very last address is still reachable.
        assert_eq!(
            client.read_holding_registers(u16::MAX, 1).await.unwrap().unwrap(),
            vec![0]
        );
    }

    #[tokio::test]
    async fn backend_exceptions_reach_the_client_and_the_connection_survives() {
        let (mut client, _memory) = start().await;

        assert_eq!(
            client.read_holding_registers(499, 2).await.unwrap(),
            Err(ExceptionCode::ServerDeviceFailure)
        );
        assert_eq!(
            client.write_single_register(600, 1).await.unwrap(),
            Err(ExceptionCode::IllegalFunction)
        );
        assert_eq!(
            client.read_holding_registers(0, 1).await.unwrap().unwrap(),
            vec![0]
        );
    }

    #[tokio::test]
    async fn memory_backend_serves_clients_and_reads_zero_until_written() {
        let memory = MemoryBackend::new();
        let addr = serve(UnitRouter::single(1, memory.clone())).await;
        let mut client = tcp::connect(addr).await.unwrap();

        assert_eq!(client.read_holding_registers(0, 3).await.unwrap().unwrap(), vec![0, 0, 0]);
        client.write_multiple_registers(1, &[7, 8]).await.unwrap().unwrap();
        assert_eq!(memory.read_block(0, 3), Some(vec![0, 7, 8]));
        memory.set(2, 99);
        assert_eq!(client.read_holding_registers(1, 2).await.unwrap().unwrap(), vec![7, 99]);
        assert_eq!(
            client.write_multiple_registers(u16::MAX, &[1, 2]).await.unwrap(),
            Err(ExceptionCode::IllegalDataAddress)
        );
    }

    #[test]
    fn memory_backend_blocks_are_bounded_and_all_or_nothing() {
        let memory = MemoryBackend::new();
        assert!(memory.write_block(10, &[1, 2, 3]));
        assert_eq!(memory.read_block(9, 5), Some(vec![0, 1, 2, 3, 0]));
        assert!(memory.read_block(u16::MAX, 2).is_none());
        assert!(!memory.write_block(u16::MAX, &[1, 2]));
        assert_eq!(memory.get(u16::MAX), 0, "a refused write must not be partially applied");
        memory.clear();
        assert_eq!(memory.get(11), 0);
    }

    // ── unit ids ──────────────────────────────────────────────────────────────

    async fn client_for(addr: SocketAddr, unit: u8) -> Context {
        tcp::connect_slave(addr, Slave(unit)).await.unwrap()
    }

    #[tokio::test]
    async fn a_request_for_another_unit_id_is_refused_with_a_gateway_exception() {
        let memory = MemoryBackend::new();
        memory.set(0, 11);
        let addr = serve(UnitRouter::single(1, memory.clone())).await;

        // The configured unit answers.
        assert_eq!(
            client_for(addr, 1).await.read_holding_registers(0, 1).await.unwrap().unwrap(),
            vec![11]
        );
        // Another unit does not, and its writes never reach the data.
        let mut other = client_for(addr, 2).await;
        assert_eq!(
            other.read_holding_registers(0, 1).await.unwrap(),
            Err(ExceptionCode::GatewayTargetDevice)
        );
        assert_eq!(
            other.write_single_register(0, 99).await.unwrap(),
            Err(ExceptionCode::GatewayTargetDevice)
        );
        assert_eq!(memory.get(0), 11);
    }

    #[tokio::test]
    async fn unit_255_reaches_a_single_device_server() {
        let memory = MemoryBackend::new();
        memory.set(0, 5);
        let addr = serve(UnitRouter::single(7, memory)).await;

        // `tcp::connect` addresses unit 255: "the device at this address".
        let mut direct = tcp::connect(addr).await.unwrap();
        assert_eq!(direct.read_holding_registers(0, 1).await.unwrap().unwrap(), vec![5]);
    }

    #[tokio::test]
    async fn units_are_isolated_and_255_is_unknown_when_acting_as_a_gateway() {
        let (first, second) = (MemoryBackend::new(), MemoryBackend::new());
        let addr = serve(
            UnitRouter::new()
                .with_unit(1, first.clone())
                .with_unit(2, second.clone()),
        )
        .await;

        client_for(addr, 1).await.write_single_register(0, 10).await.unwrap().unwrap();
        client_for(addr, 2).await.write_single_register(0, 20).await.unwrap().unwrap();
        assert_eq!((first.get(0), second.get(0)), (10, 20));

        assert_eq!(
            tcp::connect(addr).await.unwrap().read_holding_registers(0, 1).await.unwrap(),
            Err(ExceptionCode::GatewayTargetDevice)
        );
    }

    #[tokio::test]
    async fn broadcast_requests_are_never_answered_nor_executed() {
        let memory = MemoryBackend::new();
        let addr = serve(UnitRouter::single(1, memory.clone())).await;
        let mut socket = TcpStream::connect(addr).await.unwrap();

        // MBAP: txn 1, protocol 0, length 6, unit 0; PDU: write single register 0 = 99.
        let frame = [0, 1, 0, 0, 0, 6, 0, 0x06, 0, 0, 0, 99];
        socket.write_all(&frame).await.unwrap();

        let mut buf = [0u8; 16];
        let reply = tokio::time::timeout(Duration::from_millis(300), socket.read(&mut buf)).await;
        assert!(reply.is_err(), "a broadcast must not be answered");
        assert_eq!(memory.get(0), 0, "a broadcast must not be executed");

        // The connection is still usable afterwards.
        let mut client = tcp::connect(addr).await.unwrap();
        assert_eq!(client.read_holding_registers(0, 1).await.unwrap().unwrap(), vec![0]);
    }

    #[tokio::test]
    async fn unsupported_function_codes_are_illegal_function() {
        let (mut client, _memory) = start().await;

        let reply = client
            .masked_write_register(1, 0xFFFF, 0)
            .await
            .unwrap();
        assert_eq!(reply, Err(ExceptionCode::IllegalFunction));
    }
}
