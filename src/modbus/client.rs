use std::sync::Arc;
use std::time::Duration;

use tokio::time::MissedTickBehavior;

use crate::channel::{ChannelEvent, ChannelValue};
use crate::config::{ModbusTcpConfig, ProtocolConfig, WidgetConfig};

pub use crate::modbus::pool::{DeviceHandle, ModbusError, ModbusPool};

/// Create an async stream of `ChannelEvent`s by polling a Modbus register
/// at the interval specified in `config.protocol.modbus`.
pub fn modbus_stream(
    config: Arc<WidgetConfig>,
    pool: Arc<ModbusPool>,
) -> impl tokio_stream::Stream<Item = ChannelEvent> + Send + 'static {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ChannelEvent>();

    let m = match config.protocol.as_ref() {
        Some(ProtocolConfig::ModbusTcp(m)) => m.clone(),
        _ => {
            let _ = tx.send(ChannelEvent::Error(
                "modbus_stream: not a modbus widget".into(),
            ));
            return tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
        }
    };

    tokio::spawn(run_modbus_poll(m, config, pool, tx));

    tokio_stream::wrappers::UnboundedReceiverStream::new(rx)
}

async fn run_modbus_poll(
    m: ModbusTcpConfig,
    config: Arc<WidgetConfig>,
    pool: Arc<ModbusPool>,
    tx: tokio::sync::mpsc::UnboundedSender<ChannelEvent>,
) {
    tracing::info!(
        "Modbus monitor starting: widget_id='{}' label='{}' host='{}' port={} unit_id={} register={} register_type='{:?}' word_count={} poll_ms={}",
        config.id,
        config.label,
        m.host,
        m.port,
        m.unit_id,
        m.register,
        m.register_type,
        m.word_count,
        m.min_poll_interval_ms.max(50)
    );

    let mut handle = pool.get_or_create(&m.host, m.port, m.unit_id);
    let mut connection = handle.connection_state();
    let mut interval = tokio::time::interval(Duration::from_millis(m.min_poll_interval_ms.max(50)));
    // A slow read must not be followed by a burst of catch-up polls.
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut was_connected = false;
    let mut last_value_str: Option<String> = None;
    let mut last_error: Option<String> = None;

    loop {
        if tx.is_closed() {
            break;
        }

        // The connection task was aborted (e.g. after a Stop/Start cycle): get
        // a fresh handle so the pool spawns a new task and reconnects.
        if handle.is_closed() {
            handle = pool.get_or_create(&m.host, m.port, m.unit_id);
            connection = handle.connection_state();
        }

        // While the device is down the connection task already fails every
        // request instantly; polling would only add noise. Tell the widget,
        // then sleep until the connection state changes.
        if !*connection.borrow_and_update() {
            if was_connected {
                was_connected = false;
                last_value_str = None;
                if tx
                    .send(ChannelEvent::Disconnected("connection closed".to_string()))
                    .is_err()
                {
                    break;
                }
            }
            tokio::select! {
                changed = connection.changed() => {
                    if changed.is_err() {
                        // Task gone; the check above replaces the handle.
                        tokio::task::yield_now().await;
                    }
                }
                _ = tx.closed() => break,
            }
            continue;
        }

        interval.tick().await;

        match handle
            .read(m.register, m.register_type.clone(), m.word_count)
            .await
        {
            Ok(words) => {
                last_error = None;
                if !was_connected {
                    was_connected = true;
                    let _ = tx.send(ChannelEvent::Connected);
                }
                let raw = if let Some(bit) = m.bit_index {
                    let bit = bit.min(15);
                    let word0 = words.first().copied().unwrap_or(0);
                    ((word0 >> bit) & 1) as f64
                } else {
                    decode_words(&words, m.word_count)
                };
                let physical = raw * m.scale + m.offset;
                let cv = build_channel_value(physical, &m, &config);

                // Only push an update when the value actually changed -- this
                // matches EPICS monitor semantics and prevents the SSE stream
                // from overwriting an in-progress text-entry on every tick.
                if last_value_str.as_deref() != Some(&cv.value_str) {
                    let bit_info = m
                        .bit_index
                        .map(|b| format!(", bit_index={}", b))
                        .unwrap_or_default();
                    tracing::debug!(
                        "[{}] read_channel: ch=modbus-tcp://{}:{}/reg{}, register_type={:?}, word_count={}{} words={:?}, value='{}'",
                        config.id,
                        m.host,
                        m.port,
                        m.register,
                        m.register_type,
                        m.word_count,
                        bit_info,
                        words,
                        cv.value_str
                    );
                    last_value_str = Some(cv.value_str.clone());
                    if tx.send(ChannelEvent::Value(cv)).is_err() {
                        break;
                    }
                }
            }
            Err(e) => {
                let message = e.to_string();
                // Log each distinct failure once, not on every poll tick.
                if last_error.as_deref() != Some(&message) {
                    tracing::warn!(
                        "Modbus poll error for {}:{}/reg{}: {}",
                        m.host,
                        m.port,
                        m.register,
                        message
                    );
                    last_error = Some(message.clone());
                }
                if was_connected {
                    was_connected = false;
                    last_value_str = None;
                    if tx.send(ChannelEvent::Disconnected(message)).is_err() {
                        break;
                    }
                }
            }
        }
    }
}

/// Decode one or two u16 register words into an f64.
/// * `word_count == 1` -> treat as unsigned 16-bit integer.
/// * `word_count == 2` -> treat as IEEE 754 single-precision float (big-endian
///   word order: high word first).
fn decode_words(words: &[u16], word_count: u8) -> f64 {
    match (word_count, words) {
        (2, [hi, lo, ..]) => {
            let bits = ((*hi as u32) << 16) | (*lo as u32);
            f32::from_bits(bits) as f64
        }
        (_, [w, ..]) => *w as f64,
        _ => 0.0,
    }
}

pub fn build_channel_value(
    physical: f64,
    m: &ModbusTcpConfig,
    config: &WidgetConfig,
) -> ChannelValue {
    let meta_display = config.metadata.as_ref().and_then(|md| md.display.as_ref());
    let meta_control = config.metadata.as_ref().and_then(|md| md.control.as_ref());
    let meta_alarm = config.metadata.as_ref().and_then(|md| md.alarm.as_ref());

    let precision = meta_display.map(|d| d.precision).unwrap_or(2);
    let units = meta_display.map(|d| d.units.clone()).unwrap_or_default();
    let description = meta_display
        .map(|d| d.description.clone())
        .unwrap_or_default();

    let enum_index = if matches!(config.data_type.as_deref(), Some("enum")) {
        physical.round() as i16
    } else {
        0
    };

    let value_str = match config.data_type.as_deref() {
        Some("bool") | Some("int32") | Some("int") | Some("enum") => {
            (physical as i64).to_string()
        }
        _ => format!("{:.prec$}", physical, prec = precision as usize),
    };

    // Display / control range: prefer config metadata, then derive from register range.
    // When scale is negative (inverted sensor), raw_range_high is less than offset, so
    // take min/max to always keep display_low <= display_high.
    let raw_range_low = m.offset;
    let raw_range_high = 65535.0 * m.scale + m.offset;
    let derived_low = raw_range_low.min(raw_range_high);
    let derived_high = raw_range_low.max(raw_range_high);
    let display_low = meta_display.map(|d| d.limit_low).unwrap_or(derived_low);
    let display_high = meta_display.map(|d| d.limit_high).unwrap_or(derived_high);
    let control_low = meta_control.map(|c| c.limit_low).unwrap_or(display_low);
    let control_high = meta_control.map(|c| c.limit_high).unwrap_or(display_high);

    let alarm_severity = meta_alarm
        .map(|a| a.compute_severity(physical))
        .unwrap_or(0);

    ChannelValue {
        raw_value: physical,
        value_str,
        precision,
        display_low,
        display_high,
        control_low,
        control_high,
        low_alarm_limit: meta_alarm.map(|a| a.low_alarm_limit).unwrap_or(0.0),
        low_warn_limit: meta_alarm.map(|a| a.low_warning_limit).unwrap_or(0.0),
        high_warn_limit: meta_alarm
            .map(|a| a.high_warning_limit)
            .unwrap_or(display_high),
        high_alarm_limit: meta_alarm
            .map(|a| a.high_alarm_limit)
            .unwrap_or(display_high),
        alarm_severity,
        enum_index,
        enum_choices: config.options.clone().unwrap_or_default(),
        primary_meta: crate::channel::PrimaryMeta {
            alarm_severity,
            description,
            units: units.clone(),
            limit_lo: display_low,
            limit_hi: display_high,
        },
        units,
        ..ChannelValue::default()
    }
}

/// Write a physical value back to a Modbus register, reversing the scale/offset.
pub async fn modbus_write(
    m: &ModbusTcpConfig,
    physical_value: f64,
    pool: &ModbusPool,
) -> Result<(), String> {
    let handle = pool.get_or_create(&m.host, m.port, m.unit_id);
    let raw = (physical_value - m.offset) / m.scale;

    let words: Vec<u16> = if m.word_count == 2 {
        let bits = (raw as f32).to_bits();
        vec![(bits >> 16) as u16, (bits & 0xFFFF) as u16]
    } else {
        // Round before casting -- prevents floating-point edge cases where
        // e.g. (99.1 / 0.1) evaluates to 990.9999... and floors to 990.
        vec![raw.round().clamp(0.0, 65535.0) as u16]
    };

    handle
        .write(m.register, m.register_type.clone(), words)
        .await
        .map_err(|e| e.to_string())
}
