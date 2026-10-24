use std::sync::Arc;
use std::time::Duration;

use ascii_tcp::{AsciiResponseMode, AsciiTcpError, ChannelReader, Context};

use crate::channel::{ChannelEvent, ChannelValue};
use crate::config::{ProtocolConfig, WidgetConfig};

fn build_channel_value(physical: f64, raw_response: &str, config: &WidgetConfig) -> ChannelValue {
    let meta_display = config.metadata.as_ref().and_then(|md| md.display.as_ref());
    let meta_control = config.metadata.as_ref().and_then(|md| md.control.as_ref());
    let meta_alarm = config.metadata.as_ref().and_then(|md| md.alarm.as_ref());

    let precision = meta_display.map(|d| d.precision).unwrap_or(2);
    let units = meta_display.map(|d| d.units.clone()).unwrap_or_default();
    let description = meta_display
        .map(|d| d.description.clone())
        .unwrap_or_default();

    let value_str = match config.data_type.as_deref() {
        Some("bool") | Some("int32") | Some("int") | Some("enum") => (physical as i64).to_string(),
        Some("string") => raw_response.to_string(),
        _ => format!("{:.prec$}", physical, prec = precision as usize),
    };

    let display_low = meta_display.map(|d| d.limit_low).unwrap_or(f64::MIN);
    let display_high = meta_display.map(|d| d.limit_high).unwrap_or(f64::MAX);
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
        low_alarm_limit: meta_alarm.map(|a| a.low_alarm_limit).unwrap_or(f64::MIN),
        low_warn_limit: meta_alarm
            .map(|a| a.low_warning_limit)
            .unwrap_or(f64::MIN),
        high_warn_limit: meta_alarm
            .map(|a| a.high_warning_limit)
            .unwrap_or(f64::MAX),
        high_alarm_limit: meta_alarm.map(|a| a.high_alarm_limit).unwrap_or(f64::MAX),
        alarm_severity,
        enum_index: if matches!(config.data_type.as_deref(), Some("enum")) {
            physical.round() as i16
        } else {
            0
        },
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

/// Create an async stream of `ChannelEvent`s that widgets consume, yielding connection status and values.
/// Handles polling, response ordering, per-widget disconnect reporting.
/// 
/// The device only answers requests, so the stream polls `read_command` every
/// `min_poll_interval_ms`. Polling stops as soon as the stream is dropped.
pub fn ascii_tcp_stream(
    config: Arc<WidgetConfig>,
    ctx: Arc<Context>,
) -> impl tokio_stream::Stream<Item = ChannelEvent> + Send + 'static {
    async_stream::stream! {
        let Some(ProtocolConfig::AsciiTcp(a)) = config.protocol.clone() else {
            yield ChannelEvent::Error("ascii_tcp_stream: not an ascii-tcp widget".into());
            return;
        };

        // A presence widget without a read command listens for unsolicited lines.
        if a.read_command.is_none() && a.response_mode != AsciiResponseMode::Presence {
            // Write-only endpoint: report it live once so the widget renders enabled, then idle.
            yield ChannelEvent::Connected;
            yield ChannelEvent::Value(build_channel_value(0.0, "", &config));
            std::future::pending::<()>().await;
        }

        let reader = match ChannelReader::new(a) {
            Ok(reader) => reader,
            Err(e) => {
                yield ChannelEvent::Error(e.to_string());
                return;
            }
        };
        let a = reader.config();
        let mut interval =
            tokio::time::interval(Duration::from_millis(a.min_poll_interval_ms.max(50)));
        let mut was_connected = false;
        let mut last_value_str: Option<String> = None;

        // A listen-only poll already blocks until a line arrives. Pacing it would leave gaps in
        // which unsolicited lines are missed, so it only backs off after a failure.
        let listen_only = a.read_command.as_deref().unwrap_or_default().is_empty();
        let mut failed = false;

        loop {
            if !listen_only || failed {
                interval.tick().await;
            }
            failed = false;

            let result = ctx.read_channel(&reader).await;
            // A reply that holds no usable value still proves the device is reachable.
            let replied = matches!(
                result,
                Ok(_) | Err(AsciiTcpError::InvalidValue(_) | AsciiTcpError::ResponseMismatch { .. })
            );
            if replied && !was_connected {
                was_connected = true;
                yield ChannelEvent::Connected;
            }

            match result {
                Ok(reading) => {
                    let mut cv = build_channel_value(reading.value, &reading.text, &config);
                    if a.response_mode == AsciiResponseMode::Text {
                        cv.value_str = reading.text;
                    }

                    // Presence pulses are events in their own right (e.g. a heartbeat), not a
                    // value the UI displays, so every accepted pulse must propagate even though
                    // it never differs from the last one.
                    let is_repeat_pulse = a.response_mode == AsciiResponseMode::Presence;
                    if is_repeat_pulse || last_value_str.as_deref() != Some(&cv.value_str) {
                        last_value_str = Some(cv.value_str.clone());
                        yield ChannelEvent::Value(cv);
                    }
                }
                Err(e) if replied => yield ChannelEvent::Error(e.to_string()),
                Err(e) => {
                    failed = true;
                    if was_connected {
                        was_connected = false;
                        last_value_str = None;
                        yield ChannelEvent::Disconnected(e.to_string());
                    }
                }
            }
        }
    }
}