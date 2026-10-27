mod test_widgets_container_style {
    use mycela::config::{WidgetConfig, WidgetStyle, WidgetType};
    use mycela::widgets::{render_gauge, render_widget_from_config, widget_container_style};
    use serde_json::json;

    fn make_widget(style: Option<WidgetStyle>) -> WidgetConfig {
        WidgetConfig {
            id: "test1".into(),
            widget_type: WidgetType::Gauge,
            label: "Test".into(),
            style,
            ..Default::default()
        }
    }

    #[test]
    fn test_no_style_config_produces_no_style_attribute_in_html() {
        let w = make_widget(None);
        let css = widget_container_style(&w).expect("container style is always present");
        assert_eq!(css, "position:relative;");
        let html = render_gauge(&w).into_string();
        let outer_tag = &html[..html.find('>').unwrap() + 1];
        assert!(
            outer_tag.contains("style=\"position:relative;\""),
            "expected default relative-position style on outer div, got: {outer_tag}"
        );
    }

    #[test]
    fn test_width_and_height_style_config_produces_inline_style_attribute() {
        let w = make_widget(Some(WidgetStyle {
            width: Some("400px".into()),
            height: Some("200px".into()),
            background: None,
            left: None,
            top: None,
        }));
        let css = widget_container_style(&w).unwrap();
        assert!(css.contains("width:400px;"), "CSS must contain width");
        assert!(css.contains("height:200px;"), "CSS must contain height");

        let html = render_gauge(&w).into_string();
        assert!(
            html.contains("style=\"position:relative;width:400px;height:200px;\""),
            "rendered HTML must include inline style, got: {html}"
        );
    }

    #[test]
    fn test_width_only_style_config_produces_width_without_height() {
        let w = make_widget(Some(WidgetStyle {
            width: Some("50%".into()),
            height: None,
            background: None,
            left: None,
            top: None,
        }));
        let css = widget_container_style(&w).unwrap();
        assert_eq!(css, "position:relative;width:50%;");
        assert!(!css.contains("height"));
    }

    fn visible_widget_types() -> [&'static str; 11] {
        [
            "text_entry",
            "text_update",
            "gauge",
            "led",
            "button",
            "toggle_button",
            "slider",
            "chart",
            "select",
            "multi_state_led",
            "group",
        ]
    }

    #[test]
    fn all_visible_widgets_apply_style_to_the_outer_container() {
        for kind in visible_widget_types() {
            let widget: WidgetConfig = serde_json::from_value(json!({
                "id": "styled", "type": kind, "label": "Styled",
                "style": {
                    "left": "14px", "top": "458px",
                    "width": "54px", "height": "20px", "background": "#123456"
                }
            }))
            .unwrap();
            let html = render_widget_from_config(&widget).into_string();
            let outer_tag = &html[..html.find('>').unwrap() + 1];
            assert!(
                outer_tag.contains(
                    "style=\"position:absolute;width:54px;height:20px;left:14px;top:458px;background:#123456;\""
                ),
                "{kind}: {outer_tag}"
            );
        }
    }

    #[test]
    fn all_visible_widgets_preserve_normal_layout_without_coordinates() {
        for kind in visible_widget_types() {
            for style in [json!(null), json!({}), json!({"width": "54px"})] {
                let widget: WidgetConfig = serde_json::from_value(json!({
                    "id": "default", "type": kind, "label": "Default", "style": style
                }))
                .unwrap();
                let css = widget_container_style(&widget).unwrap();
                assert!(css.starts_with("position:relative;"), "{kind}: {css}");
                assert!(
                    !css.contains("left:") && !css.contains("top:") && !css.contains("height:")
                );
                let html = render_widget_from_config(&widget).into_string();
                let outer_tag = &html[..html.find('>').unwrap() + 1];
                assert!(
                    outer_tag.contains(&format!("style=\"{css}\"")),
                    "{kind}: {outer_tag}"
                );
            }
        }
    }

    #[test]
    fn either_coordinate_enables_absolute_positioning_without_other_overrides() {
        for (style, expected) in [
            (json!({"left": "0px"}), "position:absolute;left:0px;"),
            (json!({"top": "20%"}), "position:absolute;top:20%;"),
        ] {
            let widget: WidgetConfig = serde_json::from_value(json!({
                "id": "partial", "type": "gauge", "label": "Partial", "style": style
            }))
            .unwrap();
            assert_eq!(widget_container_style(&widget).as_deref(), Some(expected));
        }
    }

    #[test]
    fn group_style_preserves_legacy_minimum_size_and_nested_widget_styles() {
        let group: WidgetConfig = serde_json::from_value(json!({
            "id": "group", "type": "group", "label": "Group",
            "style": {"width": "600px", "height": "700px"},
            "size": {"width": "400px", "height": "500px"},
            "children": [{
                "id": "child", "type": "text_update", "label": "Child",
                "style": {"left": "14px", "top": "458px", "width": "54px", "height": "20px"}
            }]
        }))
        .unwrap();
        let html = render_widget_from_config(&group).into_string();
        assert!(html.contains(
            "style=\"position:relative;width:600px;height:700px;min-width:400px;min-height:500px;\""
        ), "{html}");
        assert!(
            html.contains(
                "style=\"position:absolute;width:54px;height:20px;left:14px;top:458px;\""
            ),
            "{html}"
        );
    }

    #[test]
    fn hidden_widgets_remain_invisible_with_style() {
        let widget: WidgetConfig = serde_json::from_value(json!({
            "id": "hidden", "type": "hidden", "label": "Hidden",
            "style": {"left": "14px", "top": "458px", "width": "54px", "height": "20px"}
        }))
        .unwrap();
        assert!(render_widget_from_config(&widget).into_string().is_empty());
    }
}
