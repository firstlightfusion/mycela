// Modbus TCP server infrastructure for mycela.
//
// `start_modbus_server` binds a Modbus TCP listener and serves every request
// from the `RegisterBackend` its unit id selects (`UnitRouter`): the server
// owns the protocol (unit routing, function codes, request limits,
// address-space checks, exception mapping) and the backend owns the data. A
// backend can answer straight from a live source (e.g. a `ModbusPool` device,
// as the bridge does) or be plain memory (`MemoryBackend`).
//
// Backend calls are async and each client connection runs on its own task, so
// a slow backend call delays only the connection that made it.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, PoisonError, RwLock};

use futures::future::BoxFuture;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_modbus::prelude::*;
use tokio_modbus::server::tcp::{accept_tcp_connection, Server};

/// Number of addressable registers / coils (the 16-bit Modbus address space).
const ADDRESS_SPACE: u32 = 1 << 16;

// Per-request limits from the Modbus application protocol specification.
const MAX_READ_REGISTERS: u16 = 125;
const MAX_READ_BITS: u16 = 2000;
const MAX_WRITE_REGISTERS: usize = 123;
const MAX_WRITE_COILS: usize = 1968;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteAccessKind {
    SingleCoil,
    SingleRegister,
    MultiCoil,
    MultiRegister,
}

/// The data behind a Modbus server. Coils and registers share one address
/// space; coils are exchanged as 0 / 1 words.
///
/// The server has already validated `count` / `values.len()` against the
/// protocol limits and checked that the block fits the address space, so a
/// backend only decides what the data is.
pub trait RegisterBackend: Send + Sync + 'static {
    /// Returns exactly `count` words starting at `addr`.
    fn read(&self, addr: u16, count: u16) -> BoxFuture<'_, Result<Vec<u16>, ExceptionCode>>;

    /// Applies the whole write or rejects it. A request that fails validation
    /// must leave the data untouched; `values` is the complete written block.
    fn write(
        &self,
        kind: WriteAccessKind,
        addr: u16,
        values: Vec<u16>,
    ) -> BoxFuture<'_, Result<(), ExceptionCode>>;
}

// ── Internal service impl ─────────────────────────────────────────────────────

struct BackendService {
    router: Arc<UnitRouter>,
}

fn fits_address_space(addr: u16, len: usize) -> bool {
    addr as u32 + len as u32 <= ADDRESS_SPACE
}

async fn read_words(
    backend: &dyn RegisterBackend,
    addr: u16,
    count: u16,
    max: u16,
) -> Result<Vec<u16>, ExceptionCode> {
    if count == 0 || count > max {
        return Err(ExceptionCode::IllegalDataValue);
    }
    if !fits_address_space(addr, count as usize) {
        return Err(ExceptionCode::IllegalDataAddress);
    }
    let words = backend.read(addr, count).await?;
    if words.len() != count as usize {
        tracing::error!(
            "[modbus-server] backend returned {} words for a {}-word read at {}",
            words.len(),
            count,
            addr
        );
        return Err(ExceptionCode::ServerDeviceFailure);
    }
    Ok(words)
}

async fn write_words(
    backend: &dyn RegisterBackend,
    addr: u16,
    values: Vec<u16>,
    kind: WriteAccessKind,
    max: usize,
) -> Result<(), ExceptionCode> {
    if values.is_empty() || values.len() > max {
        return Err(ExceptionCode::IllegalDataValue);
    }
    if !fits_address_space(addr, values.len()) {
        return Err(ExceptionCode::IllegalDataAddress);
    }
    backend.write(kind, addr, values).await
}

fn to_bits(words: Vec<u16>) -> Vec<bool> {
    words.into_iter().map(|w| w != 0).collect()
}

async fn handle(backend: &dyn RegisterBackend, req: Request<'static>) -> Result<Response, ExceptionCode> {
    match req {
        Request::ReadCoils(addr, cnt) => Ok(Response::ReadCoils(to_bits(
            read_words(backend, addr, cnt, MAX_READ_BITS).await?,
        ))),
        Request::ReadDiscreteInputs(addr, cnt) => Ok(Response::ReadDiscreteInputs(to_bits(
            read_words(backend, addr, cnt, MAX_READ_BITS).await?,
        ))),
        Request::ReadHoldingRegisters(addr, cnt) => Ok(Response::ReadHoldingRegisters(
            read_words(backend, addr, cnt, MAX_READ_REGISTERS).await?,
        )),
        Request::ReadInputRegisters(addr, cnt) => Ok(Response::ReadInputRegisters(
            read_words(backend, addr, cnt, MAX_READ_REGISTERS).await?,
        )),

        Request::WriteSingleCoil(addr, val) => {
            tracing::debug!("[modbus-server] WriteSingleCoil addr={} value={}", addr, val);
            write_words(backend, addr, vec![u16::from(val)], WriteAccessKind::SingleCoil, 1).await?;
            Ok(Response::WriteSingleCoil(addr, val))
        }

        Request::WriteSingleRegister(addr, val) => {
            tracing::debug!("[modbus-server] WriteSingleRegister addr={} value={}", addr, val);
            write_words(backend, addr, vec![val], WriteAccessKind::SingleRegister, 1).await?;
            Ok(Response::WriteSingleRegister(addr, val))
        }

        Request::WriteMultipleCoils(addr, data) => {
            let values: Vec<u16> = data.iter().map(|&b| u16::from(b)).collect();
            let count = values.len() as u16;
            write_words(backend, addr, values, WriteAccessKind::MultiCoil, MAX_WRITE_COILS).await?;
            Ok(Response::WriteMultipleCoils(addr, count))
        }

        Request::WriteMultipleRegisters(addr, data) => {
            let count = data.len() as u16;
            write_words(backend, addr, data.into_owned(), WriteAccessKind::MultiRegister, MAX_WRITE_REGISTERS)
                .await?;
            Ok(Response::WriteMultipleRegisters(addr, count))
        }

        _ => Err(ExceptionCode::IllegalFunction),
    }
}

impl tokio_modbus::server::Service for BackendService {
    type Request = SlaveRequest<'static>;
    // `None`: the request is deliberately left unanswered (broadcast).
    type Response = Option<Response>;
    type Exception = ExceptionCode;
    type Future = BoxFuture<'static, Result<Option<Response>, ExceptionCode>>;

    fn call(&self, req: SlaveRequest<'static>) -> Self::Future {
        let SlaveRequest { slave, request } = req;

        // Broadcasts (unit 0) are executed by no one here and, by definition, never answered.
        if slave == BROADCAST_UNIT {
            tracing::debug!("[modbus-server] ignoring broadcast request {:?}", request);
            return Box::pin(std::future::ready(Ok(None)));
        }

        match self.router.resolve(slave).cloned() {
            Some(backend) => Box::pin(async move { handle(&*backend, request).await.map(Some) }),
            None => {
                tracing::debug!("[modbus-server] no device for unit id {}", slave);
                Box::pin(std::future::ready(Err(ExceptionCode::GatewayTargetDevice)))
            }
        }
    }
}

// ── Unit routing ─────────────────────────────────────────────────────────────

const BROADCAST_UNIT: u8 = 0;
/// The unit id Modbus TCP assigns to "the device at this address" (no gateway).
const TCP_DEVICE_UNIT: u8 = 255;

/// Maps the unit id of each request to the backend that serves it.
///
/// A request for a unit id with no backend is answered with the Modbus
/// exception *Gateway Target Device Failed To Respond*, so a client fails
/// immediately instead of timing out. Unit 255, the id a client uses for a
/// directly addressed TCP device, is served by the sole backend when exactly
/// one unit is registered (a single-device server); with several units the
/// server acts as a gateway and 255 is unknown. Unit 0 is the broadcast
/// address and is never answered.
#[derive(Default)]
pub struct UnitRouter {
    backends: HashMap<u8, Arc<dyn RegisterBackend>>,
}

impl UnitRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// A router with one device, answering as `unit`.
    pub fn single(unit: u8, backend: Arc<dyn RegisterBackend>) -> Self {
        Self::new().with_unit(unit, backend)
    }

    /// Adds (or replaces) the device answering as `unit`. Unit 0 (broadcast) cannot be served.
    pub fn with_unit(mut self, unit: u8, backend: Arc<dyn RegisterBackend>) -> Self {
        if unit == BROADCAST_UNIT {
            tracing::warn!("[modbus-server] unit id 0 is the broadcast address and cannot be served; ignored");
        } else {
            self.backends.insert(unit, backend);
        }
        self
    }

    fn resolve(&self, unit: u8) -> Option<&Arc<dyn RegisterBackend>> {
        self.backends.get(&unit).or_else(|| {
            if unit == TCP_DEVICE_UNIT && self.backends.len() == 1 {
                self.backends.values().next()
            } else {
                None
            }
        })
    }
}

// ── Memory backend ───────────────────────────────────────────────────────────

/// A [`RegisterBackend`] that is plain memory: one 16-bit word per address,
/// shared by coils and registers (coils are 0 / 1). Unwritten addresses read 0.
///
/// Block reads and writes are atomic with respect to each other, so a client
/// never sees half of a multi-register write (e.g. one word of a 32-bit float).
/// Useful for simulators and tests, or as the data of a server that is the
/// source of truth rather than a front for another device.
pub struct MemoryBackend {
    words: RwLock<Box<[u16]>>,
}

impl Default for MemoryBackend {
    fn default() -> Self {
        Self {
            words: RwLock::new(vec![0u16; ADDRESS_SPACE as usize].into_boxed_slice()),
        }
    }
}

impl MemoryBackend {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The word at `addr`.
    pub fn get(&self, addr: u16) -> u16 {
        self.words.read().unwrap_or_else(PoisonError::into_inner)[addr as usize]
    }

    /// Sets the word at `addr`.
    pub fn set(&self, addr: u16, value: u16) {
        self.words.write().unwrap_or_else(PoisonError::into_inner)[addr as usize] = value;
    }

    /// Reads `count` words atomically; `None` if the block leaves the address space.
    pub fn read_block(&self, addr: u16, count: u16) -> Option<Vec<u16>> {
        let range = block_range(addr, count as usize)?;
        Some(self.words.read().unwrap_or_else(PoisonError::into_inner)[range].to_vec())
    }

    /// Writes `values` atomically; `false` (writing nothing) if the block leaves the address space.
    pub fn write_block(&self, addr: u16, values: &[u16]) -> bool {
        let Some(range) = block_range(addr, values.len()) else {
            return false;
        };
        self.words.write().unwrap_or_else(PoisonError::into_inner)[range].copy_from_slice(values);
        true
    }

    /// Resets every address to 0.
    pub fn clear(&self) {
        self.words
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .fill(0);
    }
}

fn block_range(addr: u16, len: usize) -> Option<std::ops::Range<usize>> {
    let start = addr as usize;
    let end = start.checked_add(len)?;
    (end <= ADDRESS_SPACE as usize).then_some(start..end)
}

impl RegisterBackend for MemoryBackend {
    fn read(&self, addr: u16, count: u16) -> BoxFuture<'_, Result<Vec<u16>, ExceptionCode>> {
        Box::pin(async move {
            self.read_block(addr, count)
                .ok_or(ExceptionCode::IllegalDataAddress)
        })
    }

    fn write(
        &self,
        _kind: WriteAccessKind,
        addr: u16,
        values: Vec<u16>,
    ) -> BoxFuture<'_, Result<(), ExceptionCode>> {
        Box::pin(async move {
            if self.write_block(addr, &values) {
                Ok(())
            } else {
                Err(ExceptionCode::IllegalDataAddress)
            }
        })
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Start a Modbus TCP server that answers each request from the backend `router`
/// selects by unit id.
///
/// The server runs in a background tokio task.  The returned [`JoinHandle`]
/// can be used to await or abort the server.
pub fn start_modbus_server(addr: SocketAddr, router: UnitRouter) -> JoinHandle<()> {
    let router = Arc::new(router);
    tokio::spawn(async move {
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => {
                tracing::info!("[modbus-server] listening on {addr}");
                l
            }
            Err(e) => {
                tracing::error!("[modbus-server] failed to bind on {addr}: {e}");
                return;
            }
        };

        let server = Server::new(listener);
        let on_connected = |stream, peer| {
            tracing::info!("[modbus-server] peer client connected: {}", peer);
            let router = router.clone();
            std::future::ready(accept_tcp_connection(stream, peer, move |_peer_addr| {
                Ok(Some(BackendService {
                    router: router.clone(),
                }))
            }))
        };

        let result = server
            .serve(&on_connected, |err| {
                tracing::error!("[modbus-server] connection error: {err}");
            })
            .await;

        if let Err(e) = result {
            tracing::error!("[modbus-server] stopped with error: {e}");
        }
    })
}
