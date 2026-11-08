use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use futures::future::{join_all, BoxFuture};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_modbus::prelude::ExceptionCode;

use crate::app::AppState;
use crate::channel::{ChannelContext, ChannelValue};
use crate::config::{
    ModbusBridgeAccessMode, ModbusBridgeReadMode, ModbusBridgeRegisterMap,
    ModbusBridgeUpstreamConfig, ModbusRegisterType, ProtocolConfig, WidgetConfig,
    WidgetServerProtocolConfig, DEFAULT_MODBUS_BRIDGE_CACHE_TTL_MS,
};
use crate::modbus::server::{start_modbus_server, RegisterBackend, UnitRouter, WriteAccessKind};
use crate::protocol_control::ProtocolControlError;

fn write_kind_matches_type(kind: WriteAccessKind, register_type: &ModbusRegisterType) -> bool {
    match register_type {
        ModbusRegisterType::Coil => {
            matches!(kind, WriteAccessKind::SingleCoil | WriteAccessKind::MultiCoil)
        }
        ModbusRegisterType::HoldingRegister => {
            matches!(kind, WriteAccessKind::SingleRegister | WriteAccessKind::MultiRegister)
        }
        // Discrete/Input registers are read-only by Modbus definition.
        ModbusRegisterType::DiscreteInput | ModbusRegisterType::InputRegister => false,
    }
}

/// The configured bridge mappings, keyed by exposed register.
pub struct ModbusBridgeContext {
    pub mappings: DashMap<u16, ModbusBridgeRegisterMap>,
}

impl ModbusBridgeContext {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            mappings: DashMap::new(),
        })
    }

    pub fn configure(&self, mappings: &[ModbusBridgeRegisterMap]) {
        self.mappings.clear();

        for mapping in mappings {
            self.mappings
                .insert(mapping.exposed_register, mapping.clone());
        }
    }
}

fn collect_widget_proxy_mappings(state: &AppState) -> Vec<ModbusBridgeRegisterMap> {
    let mut mappings = Vec::new();
    for screen in &state.config.screens {
        for widget in crate::widgets::collect_data_widgets(&screen.widgets) {
            let Some(server) = widget.server.clone() else {
                continue;
            };
            match widget.protocol.clone() {
                Some(ProtocolConfig::ModbusTcp(modbus)) => {
                    mappings.push(ModbusBridgeRegisterMap {
                        exposed_register: server.proxy_register,
                        register_type: modbus.register_type.clone(),
                        word_count: modbus.word_count.max(1),
                        source_widget_id: Some(widget.id.clone()),
                        source_upstream_register: Some(modbus.register),
                        source_upstream_register_type: Some(modbus.register_type.clone()),
                        target_upstream_register: server
                            .target_upstream_register
                            .or(Some(modbus.register)),
                        access: server.access,
                    });
                    tracing::info!(
                        "[bridge] widget {} proxy mapping: exposed register {} -> upstream register {} (type {:?}, word count {})",
                        widget.id,
                        server.proxy_register,
                        modbus.register,
                        modbus.register_type,
                        modbus.word_count.max(1)
                    );
                }
                Some(ProtocolConfig::Local(_)) => {
                    let Some(WidgetServerProtocolConfig::ModbusTcp(server_modbus)) =
                        server.protocol.clone()
                    else {
                        continue;
                    };

                    mappings.push(ModbusBridgeRegisterMap {
                        exposed_register: server.proxy_register,
                        register_type: server_modbus.register_type.clone(),
                        word_count: server_modbus.word_count.max(1),
                        source_widget_id: Some(widget.id.clone()),
                        source_upstream_register: None,
                        source_upstream_register_type: None,
                        target_upstream_register: server.target_upstream_register,
                        access: server.access,
                    });
                    tracing::info!(
                        "[bridge] widget {} proxy mapping: exposed register {} -> local widget (type {:?}, word count {})",
                        widget.id,
                        server.proxy_register,
                        server_modbus.register_type,
                        server_modbus.word_count.max(1)
                    );
                }
                _ => {}
            }
        }
    }
    mappings
}

fn encode_widget_value_to_words(raw: f64, register_type: &ModbusRegisterType, word_count: u8) -> Vec<u16> {
    let wc = word_count.max(1);
    if !raw.is_finite() {
        return vec![0; wc as usize];
    }

    if wc == 1 || matches!(register_type, ModbusRegisterType::Coil | ModbusRegisterType::DiscreteInput) {
        return vec![raw.round().clamp(0.0, u16::MAX as f64) as u16];
    }

    // For two-word scalar values, expose big-endian IEEE754 f32 words.
    if wc == 2 {
        let bits = (raw as f32).to_bits();
        return vec![(bits >> 16) as u16, (bits & 0xFFFF) as u16];
    }

    vec![raw.round().clamp(0.0, u16::MAX as f64) as u16]
}

fn decode_words_to_widget_value(words: &[u16], register_type: &ModbusRegisterType, word_count: u8) -> f64 {
    let wc = word_count.max(1);
    if wc == 0 || words.is_empty() {
        return 0.0;
    }

    if wc == 1 || matches!(register_type, ModbusRegisterType::Coil | ModbusRegisterType::DiscreteInput) {
        return words[0] as f64;
    }

    if wc == 2 && words.len() >= 2 {
        // For two-word scalar values, decode big-endian IEEE754 f32 words.
        let bits = ((words[0] as u32) << 16) | (words[1] as u32);
        return f32::from_bits(bits) as f64;
    }

    words[0] as f64
}

fn to_local_value_string(widget: &WidgetConfig, raw: f64) -> String {
    match widget.data_type.as_deref() {
        Some("bool") => {
            if raw == 0.0 { "0".to_string() } else { "1".to_string() }
        }
        Some("int") | Some("int32") | Some("integer") => (raw as i64).to_string(),
        Some("enum") => (raw as i16).to_string(),
        Some("string") => raw.to_string(),
        _ => raw.to_string(),
    }
}

/// Inverse-scale factors of widgets whose server mapping declares one, by widget id.
/// Built once at startup so a write never has to walk the widget tree.
fn collect_server_scales(state: &AppState) -> HashMap<String, f32> {
    let mut scales = HashMap::new();
    for screen in &state.config.screens {
        for widget in crate::widgets::collect_data_widgets(&screen.widgets) {
            let Some(server) = widget.server.as_ref() else {
                continue;
            };
            #[allow(irrefutable_let_patterns)]
            if let Some(WidgetServerProtocolConfig::ModbusTcp(mb)) = server.protocol.as_ref() {
                if let Some(scale) = mb.scale {
                    scales.entry(widget.id.clone()).or_insert(scale);
                }
            }
        }
    }
    scales
}

// ── Backend ──────────────────────────────────────────────────────────────────

struct Entry {
    mapping: ModbusBridgeRegisterMap,
    width: u16,
    /// Live value of the widget this mapping mirrors, if it is widget-sourced.
    widget_value: Option<watch::Receiver<ChannelValue>>,
    /// Local-protocol widget that receives client writes.
    local_target: Option<WidgetConfig>,
    /// Inverse scale applied to 2-word float writes forwarded upstream.
    scale: Option<f32>,
    /// Last upstream read and when it was made (`Cached` mode). Held across the
    /// refresh, so concurrent readers wait for one shared upstream read.
    cache: tokio::sync::Mutex<Option<(Instant, Vec<u16>)>>,
}

/// Serves the bridge's exposed registers from their sources, with no register
/// memory of its own.
///
/// * Widget-sourced mappings read the widget's live value from the value bus.
/// * Upstream-sourced mappings read through the shared [`ModbusPool`], so every
///   downstream client rides the same upstream connection and concurrent reads
///   of neighbouring registers are merged into block reads. An unreachable
///   upstream answers `ServerDeviceFailure`. The [read mode] decides whether
///   each read goes upstream (`Live`) or may reuse a read younger than the TTL
///   (`Cached`, which also makes concurrent readers share one upstream read).
///   A cached value is never served past its TTL: a failed refresh is an error.
/// * Addresses without a mapping read as 0.
///
/// A write is forwarded synchronously: to the local widget and/or the upstream
/// device, and its outcome is the client's response. It also drops the cached
/// value of every mapping it touched.
///
/// [`ModbusPool`]: crate::modbus::pool::ModbusPool
/// [read mode]: BridgeBackend::with_read_mode
pub struct BridgeBackend {
    ctx: Arc<ChannelContext>,
    upstream: Option<ModbusBridgeUpstreamConfig>,
    entries: Vec<Entry>,
    /// Exposed address -> index into `entries` (the later mapping wins on overlap).
    by_address: HashMap<u16, usize>,
    read_mode: ModbusBridgeReadMode,
    cache_ttl: Duration,
}

impl BridgeBackend {
    /// `local_widgets` maps an exposed register to the local widget a write to it updates.
    /// Reads are [`Live`](ModbusBridgeReadMode::Live) until [`with_read_mode`](Self::with_read_mode) says otherwise.
    pub fn new(
        ctx: Arc<ChannelContext>,
        upstream: Option<ModbusBridgeUpstreamConfig>,
        mappings: &[ModbusBridgeRegisterMap],
        local_widgets: &HashMap<u16, WidgetConfig>,
        scales: &HashMap<String, f32>,
    ) -> Self {
        let mut entries = Vec::with_capacity(mappings.len());
        let mut by_address = HashMap::new();
        for (index, mapping) in mappings.iter().enumerate() {
            let width = mapping.word_count.max(1) as u16;
            for offset in 0..width {
                if let Some(address) = mapping.exposed_register.checked_add(offset) {
                    by_address.insert(address, index);
                }
            }
            entries.push(Entry {
                mapping: mapping.clone(),
                width,
                widget_value: mapping
                    .source_widget_id
                    .as_deref()
                    .map(|id| ctx.subscribe_widget_value_updates(id)),
                local_target: local_widgets.get(&mapping.exposed_register).cloned(),
                scale: mapping
                    .source_widget_id
                    .as_deref()
                    .and_then(|id| scales.get(id))
                    .copied(),
                cache: tokio::sync::Mutex::new(None),
            });
        }
        Self {
            ctx,
            upstream,
            entries,
            by_address,
            read_mode: ModbusBridgeReadMode::Live,
            cache_ttl: Duration::from_millis(DEFAULT_MODBUS_BRIDGE_CACHE_TTL_MS),
        }
    }

    /// Chooses how downstream reads of upstream-sourced registers are answered.
    /// `ttl` only matters in [`Cached`](ModbusBridgeReadMode::Cached) mode.
    pub fn with_read_mode(mut self, mode: ModbusBridgeReadMode, ttl: Duration) -> Self {
        self.read_mode = mode;
        self.cache_ttl = ttl;
        self
    }

    /// Words of one mapping as a downstream read should see them.
    async fn read_words(&self, entry: &Entry) -> Result<Vec<u16>, ExceptionCode> {
        let from_upstream = entry.widget_value.is_none()
            && self.upstream.is_some()
            && entry.mapping.source_upstream_register.is_some();
        if self.read_mode != ModbusBridgeReadMode::Cached || !from_upstream {
            return self.fetch_words(entry).await;
        }

        // The lock is held across the refresh: readers arriving meanwhile wait
        // for it and then find the fresh value, so one upstream read serves them all.
        let mut cache = entry.cache.lock().await;
        if let Some((read_at, words)) = cache.as_ref() {
            if read_at.elapsed() <= self.cache_ttl {
                return Ok(words.clone());
            }
        }
        match self.fetch_words(entry).await {
            Ok(words) => {
                *cache = Some((Instant::now(), words.clone()));
                Ok(words)
            }
            Err(e) => {
                // Past the TTL and unrefreshable: report the failure, never a stale value.
                *cache = None;
                Err(e)
            }
        }
    }

    /// Current words of one mapping, read from its source right now
    /// (at most `width`; sources that are neither a widget nor upstream read as 0).
    async fn fetch_words(&self, entry: &Entry) -> Result<Vec<u16>, ExceptionCode> {
        if let Some(value) = &entry.widget_value {
            let raw = value.borrow().raw_value;
            return Ok(encode_widget_value_to_words(
                raw,
                &entry.mapping.register_type,
                entry.mapping.word_count,
            ));
        }

        match (&self.upstream, entry.mapping.source_upstream_register) {
            (Some(upstream), Some(register)) => {
                let device = self
                    .ctx
                    .modbus_pool
                    .get_or_create(&upstream.host, upstream.port, upstream.unit_id);
                let register_type = entry
                    .mapping
                    .source_upstream_register_type
                    .clone()
                    .unwrap_or(ModbusRegisterType::HoldingRegister);
                device
                    .read(register, register_type, entry.mapping.word_count.max(1))
                    .await
                    .map_err(|e| {
                        tracing::debug!(
                            "[bridge] upstream read failed for {}:{} unit {} register {}: {}",
                            upstream.host,
                            upstream.port,
                            upstream.unit_id,
                            register,
                            e
                        );
                        ExceptionCode::ServerDeviceFailure
                    })
            }
            _ => Ok(vec![0; entry.width as usize]),
        }
    }

    /// Distinct mappings covering `addr..addr + len`, in address order.
    fn touched_entries(&self, addr: u16, len: usize) -> Vec<usize> {
        let mut touched = Vec::new();
        for offset in 0..len {
            if let Some(&index) = u16::try_from(addr as u32 + offset as u32)
                .ok()
                .and_then(|a| self.by_address.get(&a))
            {
                if !touched.contains(&index) {
                    touched.push(index);
                }
            }
        }
        touched
    }

    /// Delivers the complete words of one mapping to its local widget and/or the upstream device.
    async fn forward(&self, entry: &Entry, mut words: Vec<u16>) -> Result<(), ExceptionCode> {
        let mapping = &entry.mapping;

        if let Some(widget) = &entry.local_target {
            let raw = decode_words_to_widget_value(&words, &mapping.register_type, mapping.word_count);
            let value_str = to_local_value_string(widget, raw);
            crate::local_channel::local_write(widget, &value_str, &self.ctx.local_store).map_err(|e| {
                tracing::warn!(
                    "[bridge] failed to push local widget '{}' from proxy write register {}: {}",
                    widget.id,
                    mapping.exposed_register,
                    e
                );
                ExceptionCode::IllegalDataValue
            })?;
        }

        let Some(upstream) = self.upstream.as_ref() else {
            tracing::debug!(
                "[bridge] write-through for exposed register {} has no upstream bridge target configured",
                mapping.exposed_register
            );
            return Ok(());
        };

        let target_register = mapping
            .target_upstream_register
            .or(mapping.source_upstream_register)
            .unwrap_or(mapping.exposed_register);

        if mapping.register_type != ModbusRegisterType::Coil && words.len() == 2 {
            if let Some(scale) = entry.scale {
                // Inverse scale: bridge_value / scale = upstream value.
                let bridge_f32 = f32::from_bits(((words[0] as u32) << 16) | (words[1] as u32));
                let upstream_value = bridge_f32 / scale;
                let bits = upstream_value.to_bits();
                words = vec![(bits >> 16) as u16, (bits & 0xFFFF) as u16];
                tracing::debug!(
                    "[bridge] applying inverse scale {} to write for widget {}: bridge_value={} -> upstream_value={}",
                    scale,
                    mapping.source_widget_id.as_deref().unwrap_or("unknown"),
                    bridge_f32,
                    upstream_value
                );
            }
        }

        let device = self
            .ctx
            .modbus_pool
            .get_or_create(&upstream.host, upstream.port, upstream.unit_id);
        match device
            .write(target_register, mapping.register_type.clone(), words.clone())
            .await
        {
            Ok(()) => {
                tracing::debug!(
                    "[bridge] write-through forwarded exposed register {} -> upstream {}:{} unit {} register {} words {:?}",
                    mapping.exposed_register,
                    upstream.host,
                    upstream.port,
                    upstream.unit_id,
                    target_register,
                    words
                );
                Ok(())
            }
            Err(e) => {
                tracing::warn!(
                    "[bridge] write-through failed for exposed register {} -> upstream {}:{} unit {} register {} words {:?}: error - {}",
                    mapping.exposed_register,
                    upstream.host,
                    upstream.port,
                    upstream.unit_id,
                    target_register,
                    words,
                    e
                );
                Err(ExceptionCode::ServerDeviceFailure)
            }
        }
    }
}

impl RegisterBackend for BridgeBackend {
    fn read(&self, addr: u16, count: u16) -> BoxFuture<'_, Result<Vec<u16>, ExceptionCode>> {
        Box::pin(async move {
            let touched = self.touched_entries(addr, count as usize);
            // Issued together so the pool can merge neighbouring upstream reads.
            let fetched = join_all(
                touched
                    .iter()
                    .map(|&index| self.read_words(&self.entries[index])),
            )
            .await;

            let mut out = vec![0u16; count as usize];
            for (&index, words) in touched.iter().zip(fetched) {
                let words = words?;
                let base = self.entries[index].mapping.exposed_register as u32;
                for (offset, word) in words.into_iter().enumerate() {
                    let address = base + offset as u32;
                    let in_request = address >= addr as u32 && address < addr as u32 + count as u32;
                    if in_request && self.by_address.get(&(address as u16)) == Some(&index) {
                        out[(address - addr as u32) as usize] = word;
                    }
                }
            }
            Ok(out)
        })
    }

    fn write(
        &self,
        kind: WriteAccessKind,
        addr: u16,
        values: Vec<u16>,
    ) -> BoxFuture<'_, Result<(), ExceptionCode>> {
        Box::pin(async move {
            // Validate the whole request before anything is forwarded.
            for offset in 0..values.len() {
                let register = addr as u32 + offset as u32;
                let entry = u16::try_from(register)
                    .ok()
                    .and_then(|a| self.by_address.get(&a))
                    .map(|&index| &self.entries[index])
                    .filter(|e| e.mapping.access == ModbusBridgeAccessMode::ReadWrite);
                match entry {
                    None => {
                        tracing::warn!(
                            "[bridge] rejected external write to read-only or unmapped exposed register {} value {}",
                            register,
                            values[offset]
                        );
                        return Err(ExceptionCode::IllegalFunction);
                    }
                    Some(e) if !write_kind_matches_type(kind, &e.mapping.register_type) => {
                        tracing::warn!(
                            "[bridge] incompatible write rejected for exposed register {} type {:?} using {:?} value {}",
                            register,
                            e.mapping.register_type,
                            kind,
                            values[offset]
                        );
                        return Err(ExceptionCode::IllegalFunction);
                    }
                    Some(_) => {}
                }
            }

            let end = addr as u32 + values.len() as u32;
            for index in self.touched_entries(addr, values.len()) {
                let entry = &self.entries[index];
                let base = entry.mapping.exposed_register as u32;
                let fully_covered = base >= addr as u32 && base + entry.width as u32 <= end;

                // A write covering only part of a multi-word value is merged with the rest of it.
                let mut words = if fully_covered {
                    vec![0; entry.width as usize]
                } else {
                    // Merge with the live value, not a cached one: it must be what the device holds now.
                    let mut current = self.fetch_words(entry).await?;
                    current.resize(entry.width as usize, 0);
                    current
                };
                for (offset, word) in words.iter_mut().enumerate() {
                    let address = base + offset as u32;
                    if address >= addr as u32
                        && address < end
                        && self.by_address.get(&(address as u16)) == Some(&index)
                    {
                        *word = values[(address - addr as u32) as usize];
                    }
                }

                let forwarded = self.forward(entry, words).await;
                // Whatever happened, the cached value may no longer match the device.
                *entry.cache.lock().await = None;
                forwarded?;
            }
            Ok(())
        })
    }
}

pub fn start_bridge_runtime(state: &AppState) -> Result<Vec<JoinHandle<()>>, ProtocolControlError> {
    let bridge = state.config.startup.modbus_bridge.clone();
    if !bridge.enabled {
        return Err(ProtocolControlError::Operation(
            "Modbus bridge is disabled in startup.modbus_bridge.enabled".to_string(),
        ));
    }

    let mut mappings = bridge.registers.clone();
    mappings.extend(collect_widget_proxy_mappings(state));
    state.channel_ctx.modbus_bridge.configure(&mappings);

    let listen = format!("{}:{}", bridge.listen_addr, bridge.listen_port);
    let addr: SocketAddr = listen
        .parse()
        .map_err(|e| ProtocolControlError::Operation(format!("Invalid modbus bridge listen endpoint '{}': {}", listen, e)))?;

    let mut local_widgets = HashMap::<u16, WidgetConfig>::new();
    for m in &mappings {
        let Some(widget_id) = m.source_widget_id.as_deref() else {
            continue;
        };
        if m.access != ModbusBridgeAccessMode::ReadWrite {
            continue;
        }
        let local_widget = state
            .config
            .screens
            .iter()
            .flat_map(|screen| crate::widgets::collect_data_widgets(&screen.widgets))
            .find(|widget| widget.id == widget_id && matches!(widget.protocol, Some(ProtocolConfig::Local(_))));
        if let Some(widget) = local_widget {
            local_widgets.insert(m.exposed_register, widget.clone());
        }
    }

    let backend = Arc::new(
        BridgeBackend::new(
            state.channel_ctx.clone(),
            bridge.upstream.clone(),
            &mappings,
            &local_widgets,
            &collect_server_scales(state),
        )
        .with_read_mode(bridge.read_mode, Duration::from_millis(bridge.cache_ttl_ms)),
    );
    let handles = vec![start_modbus_server(
        addr,
        UnitRouter::single(bridge.listen_unit_id, backend),
    )];

    tracing::info!(
        "[bridge] runtime started on {}:{} as unit {} with {} mapping entries (reads: {:?}{})",
        bridge.listen_addr,
        bridge.listen_port,
        bridge.listen_unit_id,
        state.channel_ctx.modbus_bridge.mappings.len(),
        bridge.read_mode,
        if bridge.read_mode == ModbusBridgeReadMode::Cached {
            format!(", ttl {} ms", bridge.cache_ttl_ms)
        } else {
            String::new()
        }
    );

    Ok(handles)
}
