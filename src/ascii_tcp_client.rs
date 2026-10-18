use std::sync::Arc;
use std::time::Duration;

use ascii_tcp::{AsciiResponseMode, ChannelConfig, Context};

use crate::channel::{ChannelEvent, ChannelValue};
use crate::config::{ProtocolConfig, WidgetConfig};

fn parse_numeric_response(s: &str) -> Result<f64, String> {
    for token in s
        .split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == '=')
        .filter(|t| !t.is_empty())
    {
        if let Ok(v) = token.parse::<f64>() {
            return Ok(v);
        }
    }
    Err(format!("No numeric token found in response '{}'", s))
}

fn parse_bool_response(s: &str) -> Result<f64, String> {
    let norm = s.trim().to_ascii_lowercase();
    match norm.as_str() {
        "1" | "true" | "on" | "open" => Ok(1.0),
        "0" | "false" | "off" | "closed" => Ok(0.0),
        _ => Err(format!("Could not parse boolean response '{}'", s)),
    }
}

fn parse_response(cfg: &ChannelConfig, response: &str) -> Result<f64, String> {
    match cfg.response_mode {
        AsciiResponseMode::Number => parse_numeric_response(response),
        AsciiResponseMode::Bool => parse_bool_response(response),
        AsciiResponseMode::Text => Ok(0.0),
        // Content is irrelevant; matching `read_response` already confirmed this is the pulse we want.
        AsciiResponseMode::Presence => Ok(1.0),
    }
}

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

/// Narrows a response line to the field selected by `read_response`, if configured.
fn extract_field(
    template: Option<&ascii_tcp::ResponseTemplate>,
    response: String,
) -> Result<String, String> {
    match template {
        Some(template) if template.spec_count() > 0 => template
            .capture_first(&response)
            .map(|capture| capture.text)
            .map_err(|e| e.to_string()),
        _ => Ok(response),
    }
}

/// Executes a single request through the shared `ascii_tcp::Context` and returns the response line.
///
/// Lines `accept` rejects (e.g. the Opta's connect banner or a heartbeat) are skipped until
/// a match arrives or `io_timeout_ms` elapses. Runs once rather than retrying forever so
/// callers see failures and own their retry cadence.
async fn exchange<F>(
    ctx: &Context,
    a: &ChannelConfig,
    request: &str,
    accept: F,
) -> Result<String, ascii_tcp::AsciiTcpError>
where
    F: Fn(&str) -> bool,
{
    Ok(ctx
        .execute_command_matching(&a.command(request), accept)
        .await?
        .response)
}

/// Create an async stream of `ChannelEvent`s backed by ascii_tcp
pub fn ascii_tcp_stream(
    config: Arc<WidgetConfig>,
    ctx: Arc<Context>,
) -> impl tokio_stream::Stream<Item = ChannelEvent> + Send + 'static {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ChannelEvent>();

    let a = match config.protocol.as_ref() {
        Some(ProtocolConfig::AsciiTcp(a)) => a.clone(),
        _ => {
            let _ = tx.send(ChannelEvent::Error(
                "ascii_tcp_stream: not an ascii-tcp widget".into(),
            ));
            return tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
        }
    };

    // A presence widget watches for unsolicited lines, so it needs no request to send.
    let read_command = match a.read_command.clone() {
        Some(command) => command,
        None if matches!(a.response_mode, AsciiResponseMode::Presence) => String::new(),
        None => {
            // Write-only endpoint: report it live once so the widget renders enabled, then idle.
            let _ = tx.send(ChannelEvent::Connected);
            let _ = tx.send(ChannelEvent::Value(build_channel_value(0.0, "", &config)));
            tokio::spawn(async move { tx.closed().await });
            return tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
        }
    };

    tokio::spawn(run_poll(a, read_command, config, tx, ctx));

    tokio_stream::wrappers::UnboundedReceiverStream::new(rx)
}

async fn run_poll(
    a: ChannelConfig,
    read_command: String,
    config: Arc<WidgetConfig>,
    tx: tokio::sync::mpsc::UnboundedSender<ChannelEvent>,
    ctx: Arc<Context>,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(a.min_poll_interval_ms.max(50)));
    let mut was_connected = false;
    let mut last_value_str: Option<String> = None;

    let response_template = match a.read_response.as_deref() {
        Some(source) => match ascii_tcp::ResponseTemplate::compile(source) {
            Ok(template) => Some(template),
            Err(e) => {
                let _ = tx.send(ChannelEvent::Error(e.to_string()));
                return;
            }
        },
        None => None,
    };

    // A listen-only poll already blocks until a line arrives. Pacing it would leave gaps in
    // which unsolicited lines are missed, so it only backs off after a failure.
    let listen_only = read_command.is_empty();
    let mut failed = false;

    loop {
        if !listen_only || failed {
            interval.tick().await;
        }
        failed = false;

        let accepts_response = |line: &str| {
            response_template
                .as_ref()
                .map_or(true, |t| t.captures(line).is_ok())
        };

        match exchange(&ctx, &a, &read_command, accepts_response).await {
            Ok(response) => {
                if !was_connected {
                    was_connected = true;
                    let _ = tx.send(ChannelEvent::Connected);
                }

                // A line that does not fit the template is not the reply we are waiting for.
                if let Some(template) = response_template.as_ref() {
                    if template.captures(&response).is_err() {
                        if matches!(a.response_mode, AsciiResponseMode::Presence) {
                            continue;
                        }
                        let message = format!(
                            "response '{}' does not match template '{}'",
                            response,
                            template.as_str()
                        );
                        if tx.send(ChannelEvent::Error(message)).is_err() {
                            break;
                        }
                        continue;
                    }
                }

                let field = match extract_field(response_template.as_ref(), response) {
                    Ok(field) => field,
                    Err(e) => {
                        if tx.send(ChannelEvent::Error(e)).is_err() {
                            break;
                        }
                        continue;
                    }
                };

                let physical = match parse_response(&a, &field) {
                    Ok(raw) => raw * a.scale + a.offset,
                    Err(e) => {
                        if tx.send(ChannelEvent::Error(e)).is_err() {
                            break;
                        }
                        continue;
                    }
                };

                let cv = if matches!(a.response_mode, AsciiResponseMode::Text) {
                    let mut cv = build_channel_value(0.0, &field, &config);
                    cv.value_str = field;
                    cv
                } else {
                    build_channel_value(physical, &field, &config)
                };

                // Presence pulses are events in their own right (e.g. a heartbeat), not a
                // value the UI displays, so every accepted pulse must propagate even though
                // it never differs from the last one.
                let is_repeat_pulse = matches!(a.response_mode, AsciiResponseMode::Presence);
                if is_repeat_pulse || last_value_str.as_deref() != Some(&cv.value_str) {
                    last_value_str = Some(cv.value_str.clone());
                    if tx.send(ChannelEvent::Value(cv)).is_err() {
                        break;
                    }
                }
            }
            Err(e) => {
                failed = true;
                if was_connected {
                    was_connected = false;
                    last_value_str = None;
                    if tx.send(ChannelEvent::Disconnected(e.to_string())).is_err() {
                        break;
                    }
                }
            }
        }
    }
}

pub async fn write(a: &ChannelConfig, value_str: &str, ctx: &Context) -> Result<(), String> {
    let outbound_value = if matches!(a.response_mode, AsciiResponseMode::Number) {
        let physical: f64 = value_str
            .trim()
            .parse()
            .map_err(|_| format!("Invalid numeric value '{}'", value_str.trim()))?;
        let raw = (physical - a.offset) / a.scale;
        raw.to_string()
    } else {
        value_str.trim().to_string()
    };

    let command = match &a.write_command {
        Some(tpl) => tpl.replace("{value}", &outbound_value),
        None => outbound_value,
    };

    if a.write_expects_response {
        // Skip unsolicited lines; the device acknowledges a write by echoing it or reporting an error.
        let response = exchange(ctx, a, &command, |line| {
            line == command || line.starts_with("ERROR")
        })
        .await
        .map_err(|e| e.to_string())?;
        if response.starts_with("ERROR") {
            return Err(response);
        }
        Ok(())
    } else {
        ctx.send_command(&a.command(command))
            .await
            .map_err(|e| e.to_string())
    }
}