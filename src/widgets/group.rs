use maud::{html, Markup};
use crate::config::WidgetConfig;

fn render_group_html(config: &WidgetConfig) -> Markup {
    let level = config.level.unwrap_or(1).clamp(1, 3);
    let level_class = format!("widget-group--h{}", level);

    let mut section_style = super::widget_container_style(config);
    if let Some(size) = &config.size {
        let style = section_style.get_or_insert_with(String::new);
        if let Some(width) = &size.width {
            style.push_str(&format!("min-width:{};", width));
        }
        if let Some(height) = &size.height {
            style.push_str(&format!("min-height:{};", height));
        }
    }

    html! {
        section class={"widget-group " (level_class)} style=[section_style] {
            div class="widget-group__header" {
                @match level {
                    1 => h2 class="widget-group__title" { (config.label) },
                    2 => h3 class="widget-group__title" { (config.label) },
                    _ => h4 class="widget-group__title" { (config.label) },
                }
                @if let Some(desc) = &config.description {
                    @if !desc.is_empty() {
                        p class="widget-group__subtitle" { (desc) }
                    }
                }
            }
            div class="widget-group__content" {
                @if let Some(children) = &config.children {
                    @for child in children {
                        (super::render_widget_from_config(child))
                    }
                }
            }
        }
    }
}

pub fn render_group(widget: &WidgetConfig) -> Markup {
    render_group_html(widget)
}
