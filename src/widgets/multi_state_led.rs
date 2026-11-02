//! `MultiStateLed` widget — multi-state polygon indicator for valve feedback.
//!
//! Reads a single holding register and maps its value to visual states.
//!
//! Default (two-state) behaviour when `states` is not configured:
//! - **open**    (green)  — register = 1 (or 0 when `invert = true`)
//! - **closed**  (red)    — register = 0 (or 1 when `invert = true`)
//! - **pending** (grey)   — disconnected / Modbus error / unmapped value
//!
//! Custom states: set `widget.states` to a list of `{ value, label, color? }`
//! entries (e.g. LOW / MED / HIGH). Values not listed render as pending, and
//! `invert` is ignored.
//!
//! The polygon shape is configurable via `widget.polygon_points`.
//! mycela defaults to a plain rectangle; downstream apps can supply a for example a bowtie:
//! `"0,0 0,18 15,9 30,18 30,0 15,9"` (30 × 18 px).

use maud::{html, Markup};
use std::sync::Arc;
use futures::StreamExt;
use crate::channel::{ChannelContext, ChannelEvent, ChannelValue};
use crate::config::WidgetConfig;

/// Default polygon for mycela's generic rectangle indicator (30 × 18).
const DEFAULT_POINTS: &str = "0,0 30,0 30,18 0,18";

/// Compute the bounding box of a polygon points string (e.g. `"0,0 30,18 …"`).
/// Returns `(width, height)` as the ceiling of the max x and max y values.
/// Falls back to `(30, 18)` if the string cannot be parsed.
fn polygon_dimensions(points: &str) -> (u32, u32) {
    let mut max_x: f64 = 0.0;
    let mut max_y: f64 = 0.0;
    for pair in points.split_whitespace() {
        let mut parts = pair.splitn(2, ',');
        if let (Some(xs), Some(ys)) = (parts.next(), parts.next()) {
            if let (Ok(x), Ok(y)) = (xs.parse::<f64>(), ys.parse::<f64>()) {
                if x > max_x { max_x = x; }
                if y > max_y { max_y = y; }
            }
        }
    }
    let w = if max_x > 0.0 { max_x.ceil() as u32 } else { 30 };
    let h = if max_y > 0.0 { max_y.ceil() as u32 } else { 18 };
    (w, h)
}

pub struct MultiStateLed {
    config: WidgetConfig,
}

impl MultiStateLed {
    pub fn new(config: WidgetConfig) -> Self {
        Self { config }
    }

    pub fn into_sse_stream(
        self,
        ctx: Arc<ChannelContext>,
    ) -> impl tokio_stream::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>
           + Send
           + 'static {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let config = Arc::new(self.config);

        tokio::spawn(MultiStateLed::run_monitor_async(config.clone(), ctx, tx));

        async_stream::stream! {
            yield Ok(axum::response::sse::Event::default().data(
                render_inner_pending(&config).into_string()
            ));
            let mut rx = rx;
            while let Some(html) = rx.recv().await {
                yield Ok(axum::response::sse::Event::default().data(html));
            }
        }
    }

    pub(crate) async fn run_monitor_async(
        config: Arc<WidgetConfig>,
        ctx: Arc<ChannelContext>,
        tx: tokio::sync::mpsc::UnboundedSender<String>,
    ) {
        let ctx_clone = ctx.clone();
        let widget_id = config.id.clone();
        let ctx_publish = ctx_clone.clone();
        let widget_id_publish = widget_id.clone();
        let mut stream = crate::channel::channel_stream(config.clone(), ctx)
            .inspect(move |e| {
                if let ChannelEvent::Value(cv) = e {
                    ctx_publish.publish_widget_value(&widget_id_publish, cv.clone());
                }
            });
        let mut last_value: Option<ChannelValue> = None;
        while let Some(event) = stream.next().await {
            let html = match event {
                ChannelEvent::Value(cv)          => {
                    last_value = Some(cv.clone());
                    render_inner_connected(&config, &cv).into_string()
                }
                ChannelEvent::Disconnected(_)
                | ChannelEvent::Error(_)         => {
                    render_inner_pending_with_last(&config, last_value.as_ref()).into_string()
                }
                ChannelEvent::Connected          => continue,
            };
            if tx.send(html).is_err() {
                ctx_clone.set_widget_connected(&widget_id, false);
                break;
            }
        }
    }
}

// ─── State helpers ─────────────────────────────────────────────────────────────

const DEFAULT_PALETTE: [&str; 6] = ["#cc2222", "#22cc66", "#e6b800", "#2288dd", "#aa55cc", "#ee7722"];

const OPEN_COLOR: &str = "#22cc66";
const CLOSED_COLOR: &str = "#cc2222";

/// Resolved presentation of a value: CSS class, display label and optional custom colour.
struct StateView {
    cls: &'static str,
    label: String,
    color: Option<String>,
}

fn pending_view() -> StateView {
    StateView { cls: "ms-pending", label: "--".to_string(), color: None }
}

/// Accept only characters valid in simple CSS colours (hex, named, rgb()/hsl()).
fn sanitize_color(color: &str) -> Option<String> {
    let ok = !color.is_empty()
        && color.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '#' | '(' | ')' | ',' | '.' | '%' | ' '));
    ok.then(|| color.trim().to_string())
}

fn state_view(config: &WidgetConfig, raw_value: f64) -> StateView {
    if let Some(states) = config.states.as_ref().filter(|s| !s.is_empty()) {
        return match states.iter().enumerate().find(|(_, s)| s.value == raw_value) {
            Some((i, s)) => StateView {
                cls: "vs-custom",
                label: s.label.clone(),
                color: s.color.as_deref().and_then(sanitize_color)
                    .or_else(|| Some(DEFAULT_PALETTE[i % DEFAULT_PALETTE.len()].to_string())),
            },
            None => pending_view(),
        };
    }

    if raw_value != 0.0 && raw_value != 1.0 {
        return pending_view();
    }
    let is_open = (raw_value != 0.0) ^ config.invert.unwrap_or(false);
    if is_open {
        StateView { cls: "vs-custom", label: "OPEN".to_string(), color: Some(OPEN_COLOR.to_string()) }
    } else {
        StateView { cls: "vs-custom", label: "CLOSED".to_string(), color: Some(CLOSED_COLOR.to_string()) }
    }
}

pub fn render_inner_connected(config: &WidgetConfig, cv: &ChannelValue) -> Markup {
    let view = state_view(config, cv.raw_value);
    let icon: Option<&str> = match cv.alarm_severity {
        1 => Some(super::MINOR_ALARM_SVG),
        2 => Some(super::MAJOR_ALARM_SVG),
        3 => Some(super::INVALID_SVG),
        _ => None,
    };
    render_polygon_html(config, &view, icon, &super::tooltips::build_led_tooltip(config, cv))
}

pub fn render_inner_pending(config: &WidgetConfig) -> Markup {
    render_inner_pending_with_last(config, None)
}

pub fn render_inner_pending_with_last(config: &WidgetConfig, last_value: Option<&ChannelValue>) -> Markup {
    let view = match last_value {
        Some(cv) => state_view(config, cv.raw_value),
        None => pending_view(),
    };
    let tooltip = super::tooltips::build_disconnected_tooltip(config);
    render_polygon_html(config, &view, Some(super::OFFLINE_SVG), &tooltip)
}

fn render_polygon_html(config: &WidgetConfig, view: &StateView, icon: Option<&str>, tooltip: &str) -> Markup {
    let points = config.polygon_points.as_deref().unwrap_or(DEFAULT_POINTS);
    let (w, h) = polygon_dimensions(points);
    let view_box = format!("0 0 {} {}", w, h);
    let flex_dir = match config.label_position.as_deref() {
        Some("top")    => "column-reverse",
        Some("bottom") => "column",
        Some("right")  => "row-reverse",
        _              => "row",   // "left" or unset → default
    };
    let state_pos = config.label_position.as_deref().unwrap_or("bottom");
    let wrapper_style = format!("flex-direction:{};", flex_dir);
    let polygon_style = view.color.as_ref().map(|c| format!("fill:{c};stroke:color-mix(in srgb, {c} 45%, white);"));
    let text_style = view.color.as_ref().map(|c| format!("color:{c};"));
    html! {
        div class="widget-inner ms-widget" style=(wrapper_style) data-state-pos=(state_pos) {
            svg class="ms-polygon" width=(w) height=(h) viewBox=(view_box)
                xmlns="http://www.w3.org/2000/svg" {
                polygon class=(view.cls) style=[polygon_style] points=(points) {}
            }
            span class={"ms-state " (view.cls) "-text"} style=[text_style] {
                (view.label)
            }
            label class="widget-label" {
                (config.label)
                @if let Some(src) = icon {
                    img class="widget-status-icon" src=(src) alt="status";
                }
                @if !tooltip.is_empty() {
                    (super::tooltips::render_tooltip_info_btn(tooltip))
                }
            }
        }
    }
}

/// Full outer div wrapper with SSE swap binding.
/// Used inside a container that has `hx-sse="connect:…"`.
pub fn render_multi_state_led(widget: &WidgetConfig) -> Markup {
    html! {
        div style=[super::widget_container_style(widget)]
            data-widget-id=(widget.id)
            data-ch=(widget.channel_address())
            hx-sse=(format!("swap:{}", widget.id)) {
            (render_inner_pending(widget))
        }
    }
}
