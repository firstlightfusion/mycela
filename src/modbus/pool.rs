//! Shared Modbus TCP connections.
//!
//! A [`ModbusPool`] owns one connection-manager task per `host:port`. Every
//! device (`unit_id`) behind that address is a lightweight [`DeviceHandle`]
//! onto the same task, so a gateway fronting several units still costs a
//! single socket.
//!
//! The task is the only place that touches the wire, which keeps Modbus's
//! strict request/response ordering trivial and lets it do the efficiency
//! work no single widget could:
//!
//! * pending reads for adjacent or overlapping registers are merged into one
//!   block read and the result is sliced back out to each caller;
//! * writes always overtake queued reads;
//! * only transport failures drop the socket. A Modbus exception (a healthy
//!   device refusing one address) is returned to that caller alone;
//! * connection state is published on a [`watch`] channel, so pollers sleep
//!   while the device is down instead of hammering a dead queue.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_modbus::client::tcp;
use tokio_modbus::prelude::*;

use crate::config::ModbusRegisterType;
use crate::metrics::RuntimeMetrics;

const READ_QUEUE_CAPACITY: usize = 256;
const WRITE_QUEUE_CAPACITY: usize = 32;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const RETRY_INTERVAL: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);

/// Largest block a single read may cover (Modbus spec limits).
const MAX_REGISTERS_PER_READ: u32 = 125;
const MAX_BITS_PER_READ: u32 = 2000;

/// Why a Modbus request failed.
///
/// The distinction decides whether the TCP connection survives: an
/// [`Exception`](Self::Exception) is a well-formed reply from a healthy device
/// (e.g. illegal data address), so other widgets sharing the connection must
/// not be disturbed. Every other variant means the connection is unusable or
/// unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModbusError {
    /// Transport failure, timeout, or no connection: the link must be re-established.
    Connection(String),
    /// The device answered with a Modbus exception code; the link is fine.
    Exception(String),
    /// The request itself is invalid (e.g. writing a read-only register type).
    Invalid(String),
}

impl ModbusError {
    /// `true` when the connection can still be used for further requests.
    pub fn keeps_connection(&self) -> bool {
        !matches!(self, Self::Connection(_))
    }
}

impl std::fmt::Display for ModbusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connection(msg) | Self::Exception(msg) | Self::Invalid(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for ModbusError {}

/// Flattens tokio-modbus's nested result: the outer error is transport-level,
/// the inner one is a Modbus exception response.
fn flatten_reply<T>(
    reply: Result<Result<T, ExceptionCode>, tokio_modbus::Error>,
) -> Result<T, ModbusError> {
    match reply {
        Err(e) => Err(ModbusError::Connection(e.to_string())),
        Ok(Err(code)) => Err(ModbusError::Exception(format!("modbus exception: {code}"))),
        Ok(Ok(value)) => Ok(value),
    }
}

// ── Requests ─────────────────────────────────────────────────────────────────

struct QueueTicket {
    metrics: Arc<RuntimeMetrics>,
    pending: bool,
}

impl QueueTicket {
    fn new(metrics: Arc<RuntimeMetrics>) -> Self {
        metrics.increment_modbus_queue_depth();
        Self {
            metrics,
            pending: true,
        }
    }

    fn mark_dequeued(&mut self) {
        if self.pending {
            self.pending = false;
            self.metrics.decrement_modbus_queue_depth();
        }
    }
}

impl Drop for QueueTicket {
    fn drop(&mut self) {
        if self.pending {
            self.metrics.decrement_modbus_queue_depth();
        }
    }
}

struct ReadRequest {
    unit: u8,
    register: u16,
    register_type: ModbusRegisterType,
    word_count: u8,
    respond: oneshot::Sender<Result<Vec<u16>, ModbusError>>,
    queue_ticket: QueueTicket,
}

struct WriteRequest {
    unit: u8,
    register: u16,
    register_type: ModbusRegisterType,
    values: Vec<u16>,
    respond: oneshot::Sender<Result<(), ModbusError>>,
    queue_ticket: QueueTicket,
}

enum DeviceRequest {
    Read(ReadRequest),
    Write(WriteRequest),
}

impl DeviceRequest {
    fn mark_dequeued(&mut self) {
        match self {
            Self::Read(request) => request.queue_ticket.mark_dequeued(),
            Self::Write(request) => request.queue_ticket.mark_dequeued(),
        }
    }

    fn fail(self, error: ModbusError) {
        match self {
            Self::Read(request) => {
                let _ = request.respond.send(Err(error));
            }
            Self::Write(request) => {
                let _ = request.respond.send(Err(error));
            }
        }
    }
}

// ── Public handles ───────────────────────────────────────────────────────────

/// The queues and connection state of one `host:port` connection task.
struct Connection {
    read_tx: mpsc::Sender<ReadRequest>,
    write_tx: mpsc::Sender<WriteRequest>,
    state: watch::Receiver<bool>,
}

impl Connection {
    fn is_closed(&self) -> bool {
        self.read_tx.is_closed() || self.write_tx.is_closed()
    }
}

/// A cloneable handle to one Modbus device (`host:port` + `unit_id`).
///
/// Handles for different units on the same `host:port` share one TCP
/// connection.
pub struct DeviceHandle {
    unit_id: u8,
    connection: Arc<Connection>,
    metrics: Arc<RuntimeMetrics>,
}

impl DeviceHandle {
    pub async fn read(
        &self,
        register: u16,
        register_type: ModbusRegisterType,
        word_count: u8,
    ) -> Result<Vec<u16>, ModbusError> {
        let (respond, rx) = oneshot::channel();
        let request = ReadRequest {
            unit: self.unit_id,
            register,
            register_type,
            word_count,
            respond,
            queue_ticket: QueueTicket::new(self.metrics.clone()),
        };
        if self.connection.read_tx.send(request).await.is_err() {
            return Err(ModbusError::Connection("device task closed".into()));
        }
        rx.await
            .map_err(|_| ModbusError::Connection("device task dropped respond channel".into()))?
    }

    pub async fn write(
        &self,
        register: u16,
        register_type: ModbusRegisterType,
        values: Vec<u16>,
    ) -> Result<(), ModbusError> {
        let started = Instant::now();
        let (respond, rx) = oneshot::channel();
        let request = WriteRequest {
            unit: self.unit_id,
            register,
            register_type,
            values,
            respond,
            queue_ticket: QueueTicket::new(self.metrics.clone()),
        };
        let result = if self.connection.write_tx.send(request).await.is_err() {
            Err(ModbusError::Connection("device task closed".into()))
        } else {
            rx.await.unwrap_or_else(|_| {
                Err(ModbusError::Connection(
                    "device task dropped respond channel".into(),
                ))
            })
        };
        self.metrics
            .record_modbus_write_latency(started.elapsed().as_micros() as u64);
        result
    }

    /// Returns `true` when the backing connection task has exited and this
    /// handle can no longer send requests. [`ModbusPool::get_or_create`] then
    /// hands out a fresh handle backed by a new task.
    pub fn is_closed(&self) -> bool {
        self.connection.is_closed()
    }

    /// `true` while the TCP connection to the device is established.
    pub fn is_connected(&self) -> bool {
        *self.connection.state.borrow()
    }

    /// Subscribes to connection state transitions of the underlying socket.
    ///
    /// The channel reports `Err` from `changed()` once the task has gone away
    /// (see [`is_closed`](Self::is_closed)).
    pub fn connection_state(&self) -> watch::Receiver<bool> {
        self.connection.state.clone()
    }
}

/// Shared Modbus connection pool.
///
/// Use `Arc<ModbusPool>` everywhere so all SSE handlers and the write path
/// share the same set of connections.
pub struct ModbusPool {
    /// One connection task per `"host:port"`.
    connections: DashMap<String, Arc<Connection>>,
    /// One handle per `"host:port:unit_id"`, so equal keys yield the same `Arc`.
    devices: DashMap<String, Arc<DeviceHandle>>,
    task_handles: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    metrics: Arc<RuntimeMetrics>,
}

impl ModbusPool {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            connections: DashMap::new(),
            devices: DashMap::new(),
            task_handles: std::sync::Mutex::new(Vec::new()),
            metrics: Arc::new(RuntimeMetrics::default()),
        })
    }

    pub fn metrics(&self) -> Arc<RuntimeMetrics> {
        self.metrics.clone()
    }

    /// Abort all connection tasks and clear the pool.
    /// Existing `DeviceHandle`s become closed, so pollers report
    /// `ChannelEvent::Disconnected` and re-acquire a fresh handle.
    pub fn disconnect_all(&self) {
        self.devices.clear();
        self.connections.clear();
        let handles: Vec<_> = self.task_handles.lock().unwrap().drain(..).collect();
        for handle in handles {
            handle.abort();
        }
    }

    /// Return the existing handle for this device or create one, spawning the
    /// connection task for `host:port` if it does not exist yet.
    pub fn get_or_create(&self, host: &str, port: u16, unit_id: u8) -> Arc<DeviceHandle> {
        let key = format!("{host}:{port}:{unit_id}");
        match self.devices.entry(key) {
            Entry::Occupied(mut entry) if entry.get().is_closed() => {
                let handle = self.new_handle(host, port, unit_id);
                entry.insert(handle.clone());
                handle
            }
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => {
                let handle = self.new_handle(host, port, unit_id);
                entry.insert(handle.clone());
                handle
            }
        }
    }

    fn new_handle(&self, host: &str, port: u16, unit_id: u8) -> Arc<DeviceHandle> {
        Arc::new(DeviceHandle {
            unit_id,
            connection: self.connection(host, port),
            metrics: self.metrics.clone(),
        })
    }

    fn connection(&self, host: &str, port: u16) -> Arc<Connection> {
        match self.connections.entry(format!("{host}:{port}")) {
            Entry::Occupied(mut entry) if entry.get().is_closed() => {
                let connection = self.spawn_connection(host, port);
                entry.insert(connection.clone());
                connection
            }
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => {
                let connection = self.spawn_connection(host, port);
                entry.insert(connection.clone());
                connection
            }
        }
    }

    fn spawn_connection(&self, host: &str, port: u16) -> Arc<Connection> {
        let (read_tx, read_rx) = mpsc::channel(READ_QUEUE_CAPACITY);
        let (write_tx, write_rx) = mpsc::channel(WRITE_QUEUE_CAPACITY);
        let (state_tx, state_rx) = watch::channel(false);

        let join = tokio::spawn(run_connection(
            host.to_string(),
            port,
            RequestQueues::new(read_rx, write_rx),
            state_tx,
        ));
        let mut handles = self.task_handles.lock().unwrap();
        handles.retain(|h| !h.is_finished());
        handles.push(join);

        Arc::new(Connection {
            read_tx,
            write_tx,
            state: state_rx,
        })
    }
}

// ── Connection task ──────────────────────────────────────────────────────────

struct RequestQueues {
    read_rx: mpsc::Receiver<ReadRequest>,
    write_rx: mpsc::Receiver<WriteRequest>,
    /// Reads taken off the channel but not yet executed (they did not fit the
    /// block read in progress). Kept in arrival order.
    pending_reads: VecDeque<ReadRequest>,
}

impl RequestQueues {
    fn new(read_rx: mpsc::Receiver<ReadRequest>, write_rx: mpsc::Receiver<WriteRequest>) -> Self {
        Self {
            read_rx,
            write_rx,
            pending_reads: VecDeque::new(),
        }
    }

    /// Next request to execute: writes first, then leftover reads, then
    /// whatever arrives. `None` once every handle is gone.
    async fn next(&mut self) -> Option<DeviceRequest> {
        let mut request = if let Ok(request) = self.write_rx.try_recv() {
            DeviceRequest::Write(request)
        } else if let Some(request) = self.pending_reads.pop_front() {
            DeviceRequest::Read(request)
        } else {
            tokio::select! {
                biased;
                request = self.write_rx.recv(), if !self.write_rx.is_closed() => request.map(DeviceRequest::Write),
                request = self.read_rx.recv(), if !self.read_rx.is_closed() => request.map(DeviceRequest::Read),
                else => None,
            }?
        };
        request.mark_dequeued();
        Some(request)
    }

    /// Answers every queued read with `error`.
    fn fail_pending_reads(&mut self, error: &ModbusError) {
        while let Some(request) = self.pending_reads.pop_front() {
            DeviceRequest::Read(request).fail(error.clone());
        }
    }
}

enum Served {
    /// The socket failed; reconnect.
    LinkLost,
    /// All handles are gone; the task should exit.
    Shutdown,
}

async fn run_connection(
    host: String,
    port: u16,
    mut queues: RequestQueues,
    state_tx: watch::Sender<bool>,
) {
    loop {
        let Some(mut ctx) = connect(&host, port, &mut queues).await else {
            return;
        };
        tracing::info!("Modbus connected to {}:{}", host, port);
        state_tx.send_replace(true);

        let served = serve(&mut ctx, &mut queues).await;
        state_tx.send_replace(false);
        match served {
            Served::Shutdown => return,
            Served::LinkLost => {
                queues.fail_pending_reads(&ModbusError::Connection("connection lost".into()));
                tracing::warn!("Modbus connection to {}:{} lost, reconnecting...", host, port);
            }
        }
    }
}

/// Resolves and connects, retrying until it succeeds. Requests arriving
/// during a retry window are failed immediately so no widget waits on a dead
/// link. DNS is resolved on every attempt so address changes are picked up.
/// Returns `None` when all handles are gone.
async fn connect(
    host: &str,
    port: u16,
    queues: &mut RequestQueues,
) -> Option<tokio_modbus::client::Context> {
    loop {
        let reason = match try_connect(host, port).await {
            Ok(ctx) => return Some(ctx),
            Err(reason) => reason,
        };
        tracing::warn!("Modbus connect to {}:{} failed: {} -- retrying in {:?}", host, port, reason, RETRY_INTERVAL);

        let retry_at = tokio::time::Instant::now() + RETRY_INTERVAL;
        loop {
            tokio::select! {
                request = queues.next() => match request {
                    Some(request) => request.fail(ModbusError::Connection(reason.clone())),
                    None => return None,
                },
                _ = tokio::time::sleep_until(retry_at) => break,
            }
        }
    }
}

async fn try_connect(host: &str, port: u16) -> Result<tokio_modbus::client::Context, String> {
    let target = format!("{host}:{port}");
    let addr: SocketAddr = tokio::net::lookup_host(&target)
        .await
        .map_err(|e| format!("DNS resolution failed for '{host}': {e}"))?
        .next()
        .ok_or_else(|| format!("hostname '{host}' resolved to no addresses"))?;

    match tokio::time::timeout(CONNECT_TIMEOUT, tcp::connect(addr)).await {
        Ok(Ok(ctx)) => Ok(ctx),
        Ok(Err(e)) => Err(format!("Connection failed: {e}")),
        Err(_) => Err("Connection timed out".to_string()),
    }
}

async fn serve(ctx: &mut tokio_modbus::client::Context, queues: &mut RequestQueues) -> Served {
    loop {
        let Some(request) = queues.next().await else {
            return Served::Shutdown;
        };
        let link_ok = match request {
            DeviceRequest::Read(first) => serve_reads(ctx, first, queues).await,
            DeviceRequest::Write(request) => serve_write(ctx, request).await,
        };
        if !link_ok {
            return Served::LinkLost;
        }
    }
}

/// Executes `first` together with every queued read that can share its block
/// read; the rest stay queued for the next round. Returns `false` if the
/// connection must be dropped.
async fn serve_reads(
    ctx: &mut tokio_modbus::client::Context,
    first: ReadRequest,
    queues: &mut RequestQueues,
) -> bool {
    // Let sibling pollers that were woken by the same tick enqueue their reads.
    tokio::task::yield_now().await;

    let mut candidates = vec![first];
    candidates.extend(queues.pending_reads.drain(..));
    while let Ok(queued) = queues.read_rx.try_recv() {
        candidates.push(queued);
    }
    let block = take_block(&mut candidates);
    queues.pending_reads.extend(candidates);

    let outcome = read_block(ctx, block.unit, &block.register_type, block.start, block.count).await;
    match outcome {
        Ok(words) => {
            for request in block.requests {
                respond_slice(request, block.start, &words);
            }
            true
        }
        Err(ModbusError::Exception(_)) if block.requests.len() > 1 => {
            // One bad address must not fail its neighbours: retry individually.
            let mut results: HashMap<(u16, u8), Result<Vec<u16>, ModbusError>> = HashMap::new();
            let mut link_ok = true;
            for mut request in block.requests {
                request.queue_ticket.mark_dequeued();
                let key = (request.register, request.word_count);
                let result = match results.get(&key) {
                    Some(result) => result.clone(),
                    None if !link_ok => Err(ModbusError::Connection("connection lost".into())),
                    None => {
                        let result = read_block(
                            ctx,
                            request.unit,
                            &request.register_type,
                            request.register as u32,
                            request.word_count as u16,
                        )
                        .await;
                        link_ok = result.as_ref().map_or_else(ModbusError::keeps_connection, |_| true);
                        results.insert(key, result.clone());
                        result
                    }
                };
                let _ = request.respond.send(result);
            }
            link_ok
        }
        Err(error) => {
            let link_ok = error.keeps_connection();
            for mut request in block.requests {
                request.queue_ticket.mark_dequeued();
                let _ = request.respond.send(Err(error.clone()));
            }
            link_ok
        }
    }
}

fn respond_slice(mut request: ReadRequest, block_start: u32, words: &[u16]) {
    request.queue_ticket.mark_dequeued();
    let offset = (request.register as u32 - block_start) as usize;
    let result = words
        .get(offset..offset + request.word_count as usize)
        .map(<[u16]>::to_vec)
        .ok_or_else(|| ModbusError::Exception("device returned fewer registers than requested".into()));
    let _ = request.respond.send(result);
}

async fn serve_write(ctx: &mut tokio_modbus::client::Context, mut request: WriteRequest) -> bool {
    request.queue_ticket.mark_dequeued();
    ctx.set_slave(Slave(request.unit));
    let result = tokio::time::timeout(
        REQUEST_TIMEOUT,
        execute_write(ctx, request.register, &request.register_type, &request.values),
    )
    .await
    .unwrap_or_else(|_| Err(ModbusError::Connection("write timed out".into())));
    let link_ok = result.as_ref().map_or_else(ModbusError::keeps_connection, |_| true);
    let _ = request.respond.send(result);
    link_ok
}

async fn read_block(
    ctx: &mut tokio_modbus::client::Context,
    unit: u8,
    register_type: &ModbusRegisterType,
    start: u32,
    count: u16,
) -> Result<Vec<u16>, ModbusError> {
    ctx.set_slave(Slave(unit));
    tokio::time::timeout(
        REQUEST_TIMEOUT,
        execute_read(ctx, start as u16, register_type, count),
    )
    .await
    .unwrap_or_else(|_| Err(ModbusError::Connection("read timed out".into())))
}

async fn execute_read(
    ctx: &mut tokio_modbus::client::Context,
    register: u16,
    register_type: &ModbusRegisterType,
    count: u16,
) -> Result<Vec<u16>, ModbusError> {
    fn bits_to_words(bits: Vec<bool>) -> Vec<u16> {
        bits.into_iter().map(u16::from).collect()
    }

    match register_type {
        ModbusRegisterType::HoldingRegister => {
            flatten_reply(ctx.read_holding_registers(register, count).await)
        }
        ModbusRegisterType::InputRegister => {
            flatten_reply(ctx.read_input_registers(register, count).await)
        }
        ModbusRegisterType::Coil => {
            flatten_reply(ctx.read_coils(register, count).await).map(bits_to_words)
        }
        ModbusRegisterType::DiscreteInput => {
            flatten_reply(ctx.read_discrete_inputs(register, count).await).map(bits_to_words)
        }
    }
}

async fn execute_write(
    ctx: &mut tokio_modbus::client::Context,
    register: u16,
    register_type: &ModbusRegisterType,
    values: &[u16],
) -> Result<(), ModbusError> {
    match register_type {
        ModbusRegisterType::HoldingRegister => match values {
            [value] => flatten_reply(ctx.write_single_register(register, *value).await),
            _ => flatten_reply(ctx.write_multiple_registers(register, values).await),
        },
        ModbusRegisterType::Coil => flatten_reply(
            ctx.write_single_coil(register, values.first().copied().unwrap_or(0) != 0)
                .await,
        ),
        ModbusRegisterType::InputRegister | ModbusRegisterType::DiscreteInput => {
            Err(ModbusError::Invalid(
                "Cannot write to read-only register types (input_register / discrete_input)".into(),
            ))
        }
    }
}

// ── Read merging ─────────────────────────────────────────────────────────────

/// One wire-level read covering the requests in `requests`.
struct Block {
    unit: u8,
    register_type: ModbusRegisterType,
    start: u32,
    count: u16,
    requests: Vec<ReadRequest>,
}

fn max_block_len(register_type: &ModbusRegisterType) -> u32 {
    match register_type {
        ModbusRegisterType::HoldingRegister | ModbusRegisterType::InputRegister => MAX_REGISTERS_PER_READ,
        ModbusRegisterType::Coil | ModbusRegisterType::DiscreteInput => MAX_BITS_PER_READ,
    }
}

/// Removes the oldest request from `candidates` and every other request that
/// can share one read with it: same unit and type, overlapping or directly
/// adjacent, and within the protocol's block-size limit. Gaps are never
/// bridged, so a block never touches an address nobody asked for. Requests
/// left in `candidates` keep their relative order.
fn take_block(candidates: &mut Vec<ReadRequest>) -> Block {
    let seed = candidates.remove(0);
    let unit = seed.unit;
    let register_type = seed.register_type.clone();
    let limit = max_block_len(&register_type);
    let mut start = seed.register as u32;
    let mut end = start + seed.word_count as u32;
    let mut requests = vec![seed];

    loop {
        let mut grew = false;
        let mut i = 0;
        while i < candidates.len() {
            let c = &candidates[i];
            let c_start = c.register as u32;
            let c_end = c_start + c.word_count as u32;
            let new_start = start.min(c_start);
            let new_end = end.max(c_end);
            let joins = c.unit == unit
                && c.register_type == register_type
                && c_start <= end
                && c_end >= start
                && new_end - new_start <= limit;
            if joins {
                start = new_start;
                end = new_end;
                requests.push(candidates.remove(i));
                grew = true;
            } else {
                i += 1;
            }
        }
        if !grew {
            break;
        }
    }

    Block {
        unit,
        register_type,
        start,
        count: (end - start) as u16,
        requests,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(unit: u8, register: u16, word_count: u8, register_type: ModbusRegisterType) -> ReadRequest {
        let (respond, _rx) = oneshot::channel();
        ReadRequest {
            unit,
            register,
            register_type,
            word_count,
            respond,
            queue_ticket: QueueTicket::new(Arc::new(RuntimeMetrics::default())),
        }
    }

    fn holding(unit: u8, register: u16, word_count: u8) -> ReadRequest {
        request(unit, register, word_count, ModbusRegisterType::HoldingRegister)
    }

    #[test]
    fn adjacent_and_overlapping_reads_merge_into_one_block() {
        let mut queue = vec![holding(1, 10, 2), holding(1, 12, 1), holding(1, 11, 2), holding(1, 10, 2)];
        let block = take_block(&mut queue);
        assert_eq!((block.start, block.count), (10, 3));
        assert_eq!(block.requests.len(), 4);
        assert!(queue.is_empty());
    }

    #[test]
    fn a_later_request_can_bridge_two_earlier_ones() {
        let mut queue = vec![holding(1, 10, 1), holding(1, 12, 1), holding(1, 11, 1)];
        let block = take_block(&mut queue);
        assert_eq!((block.start, block.count), (10, 3));
        assert_eq!(block.requests.len(), 3);
    }

    #[test]
    fn gaps_are_not_bridged() {
        let mut queue = vec![holding(1, 10, 1), holding(1, 12, 1)];
        let block = take_block(&mut queue);
        assert_eq!((block.start, block.count), (10, 1));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].register, 12);
    }

    #[test]
    fn different_units_and_types_never_merge() {
        let mut queue = vec![
            holding(1, 10, 1),
            holding(2, 10, 1),
            request(1, 10, 1, ModbusRegisterType::InputRegister),
            request(1, 10, 1, ModbusRegisterType::Coil),
        ];
        let block = take_block(&mut queue);
        assert_eq!(block.requests.len(), 1);
        assert_eq!(queue.len(), 3);
    }

    #[test]
    fn blocks_respect_the_register_limit() {
        let mut queue = vec![holding(1, 0, 100), holding(1, 100, 30), holding(1, 100, 25)];
        let block = take_block(&mut queue);
        assert_eq!((block.start, block.count), (0, 125));
        assert_eq!(block.requests.len(), 2);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].word_count, 30);
    }

    #[test]
    fn exceptions_keep_the_connection_but_transport_errors_do_not() {
        assert!(ModbusError::Exception("x".into()).keeps_connection());
        assert!(ModbusError::Invalid("x".into()).keeps_connection());
        assert!(!ModbusError::Connection("x".into()).keeps_connection());
    }
}
