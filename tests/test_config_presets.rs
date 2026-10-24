use mycela::config::{AppConfig, ScreenConfig, WidgetConfig, WidgetType};
use serde_json::{json, Value};

fn canonical<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap()
}

#[test]
fn scoped_defaults_and_presets_expand_without_changing_runtime_shape() {
    let compact = json!({
        "title": "Compact",
        "defaults": {
            "data_type": "double",
            "style": {"width": "100%", "height": "auto"},
            "protocol": {"type": "local"},
            "options": ["inherited"]
        },
        "presets": {
            "reading": {
                "type": "text_update",
                "protocol": {"channel": "temperature", "initial_value": "21"},
                "style": {"width": "50%"}
            }
        },
        "screens": [{
            "id": "main", "title": "Main",
            "defaults": {"style": {"height": "40px"}},
            "widgets": [{
                "id": "group", "type": "group", "label": "Group",
                "defaults": {"style": {"left": "10px"}, "data_type": "string"},
                "children": [{
                    "id": "nested", "type": "group", "label": "Nested",
                    "defaults": {"style": {"top": "20px"}},
                    "children": [{
                        "id": "value", "label": "Value", "preset": "reading",
                        "data_type": "double",
                        "style": {"width": "25%"},
                        "options": [],
                        "description": null
                    }]
                }]
            }, {
                "id": "sibling", "label": "Sibling", "preset": "reading"
            }]
        }]
    });
    let config: AppConfig = serde_json::from_value(compact).unwrap();
    let group = &config.screens[0].widgets[0];
    assert!(group.protocol.is_none());
    assert!(group.style.is_none());
    let nested = &group.children.as_ref().unwrap()[0];
    let actual = &nested.children.as_ref().unwrap()[0];
    let expected: WidgetConfig = serde_json::from_value(json!({
        "id": "value", "type": "text_update", "label": "Value",
        "data_type": "double",
        "protocol": {"type": "local", "channel": "temperature", "initial_value": "21"},
        "style": {"width": "25%", "height": "40px", "left": "10px", "top": "20px"},
        "options": []
    }))
    .unwrap();
    assert_eq!(canonical(actual), canonical(&expected));
    let sibling = &config.screens[0].widgets[1];
    assert_eq!(sibling.style.as_ref().unwrap().left, None);
    assert_eq!(
        sibling.style.as_ref().unwrap().width.as_deref(),
        Some("50%")
    );
    assert_eq!(sibling.options.as_ref().unwrap(), &["inherited"]);
    let serialized = canonical(&config);
    assert!(serialized.get("defaults").is_none());
    assert!(serialized.get("presets").is_none());
    let roundtrip: AppConfig = serde_json::from_value(serialized.clone()).unwrap();
    assert_eq!(canonical(&roundtrip), serialized);
}

#[test]
fn standalone_screens_support_presets_all_widget_types_and_null_overrides() {
    let types = [
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
        "hidden",
    ];
    let widgets: Vec<_> = types
        .iter()
        .enumerate()
        .map(|(i, kind)| {
            json!({
                "id": format!("w{i}"), "type": kind, "label": kind, "preset": "common",
                "protocol": null, "metadata": null, "style": null
            })
        })
        .collect();
    let screen: ScreenConfig = serde_json::from_value(json!({
        "id": "main", "title": "Main",
        "defaults": {"protocol": {"type": "local", "channel": "shared"}},
        "presets": {"common": {
            "metadata": {"control": {"limit_low": 0, "limit_high": 10}},
            "style": {"width": "100%"}
        }},
        "widgets": widgets
    }))
    .unwrap();
    assert_eq!(screen.widgets.len(), types.len());
    ScreenConfig::validate_config(&screen).unwrap();
    for widget in screen.widgets {
        assert!(widget.protocol.is_none());
        assert!(widget.metadata.is_none());
        assert!(widget.style.is_none());
    }
}

#[test]
fn screen_presets_shadow_app_presets_without_leaking_to_other_screens() {
    let config: AppConfig = serde_json::from_value(json!({
        "title": "Test",
        "presets": {"value": {"type": "text_update", "data_type": "double"}},
        "screens": [{
            "id": "a", "title": "A",
            "presets": {"value": {"type": "led"}},
            "widgets": [{"id": "a1", "label": "A1", "preset": "value"}]
        }, {
            "id": "b", "title": "B",
            "widgets": [{"id": "b1", "label": "B1", "preset": "value"}]
        }]
    }))
    .unwrap();
    assert_eq!(config.screens[0].widgets[0].widget_type, WidgetType::Led);
    assert!(config.screens[0].widgets[0].data_type.is_none());
    assert_eq!(
        config.screens[1].widgets[0].widget_type,
        WidgetType::TextUpdate
    );
}

#[test]
fn invalid_expansion_settings_are_reported_with_context() {
    let cases = [
        (json!({"preset": "missing"}), "unknown preset 'missing'"),
        (json!({"preset": 3}), "'preset' must be a string"),
        (
            json!({"defaults": {}}),
            "'defaults' is only valid on groups",
        ),
        (
            json!({"type": "group", "defaults": null}),
            "'defaults' must be an object",
        ),
        (
            json!({"type": "group", "defaults": {"id": "shared"}}),
            "cannot contain 'id'",
        ),
        (json!({"children": {}}), "invalid type"),
        (
            json!({"presets": {}}),
            "'presets' is only valid on apps and screens",
        ),
    ];
    for (overrides, expected) in cases {
        let mut widget = json!({"id": "bad", "type": "text_update", "label": "Bad"});
        widget
            .as_object_mut()
            .unwrap()
            .extend(overrides.as_object().unwrap().clone());
        let error = serde_json::from_value::<ScreenConfig>(json!({
            "id": "main", "title": "Main", "widgets": [widget]
        }))
        .unwrap_err()
        .to_string();
        assert!(error.contains("Screen 'main'"), "{error}");
        assert!(error.contains(expected), "{error}");
    }
    for key in ["id", "children", "defaults", "presets", "preset"] {
        for scope in ["defaults", "presets"] {
            let fields = json!({key: "invalid"});
            let mut config = json!({"title": "Test", "screens": []});
            config[scope] = if scope == "presets" {
                json!({"bad": fields})
            } else {
                fields
            };
            let error = serde_json::from_value::<AppConfig>(config)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(&format!("cannot contain '{key}'")),
                "{error}"
            );
        }
    }
}

#[test]
fn changing_protocol_type_replaces_instead_of_merging_connection_fields() {
    let screen: ScreenConfig = serde_json::from_value(json!({
        "id": "main", "title": "Main",
        "defaults": {"protocol": {
            "type": "local", "channel": "inherited", "initial_value": "42"
        }},
        "widgets": [{
            "id": "value", "type": "text_update", "label": "Value",
            "protocol": {"type": "local", "channel": "overridden"}
        }]
    }))
    .unwrap();
    assert_eq!(
        screen.widgets[0].local().unwrap().initial_value.as_deref(),
        Some("42")
    );

    #[cfg(feature = "epics-pvxs")]
    {
        let mut input = canonical(&screen);
        input["defaults"] = json!({"protocol": {"type": "epics-pvxs", "pv_name": "test:pv"}});
        input["widgets"][0]["protocol"] = json!({"type": "local"});
        let error = serde_json::from_value::<ScreenConfig>(input)
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing field `channel`"), "{error}");
    }
}

#[test]
fn common_protocol_and_widget_fields_can_be_shared() {
    let protocols = [
        json!({"type": "local", "channel": "test", "initial_value": "1"}),
        #[cfg(feature = "epics-pvxs")]
        json!({
            "type": "epics-pvxs", "pv_name": "test:pv",
            "server": {"alarm_severity": "NONE"},
            "pv_names": ["test:extra"]
        }),
        #[cfg(feature = "modbus")]
        json!({
            "type": "modbus-tcp", "host": "127.0.0.1",
            "register": 1, "register_type": "holding_register"
        }),
        #[cfg(feature = "ascii-tcp")]
        json!({
            "type": "ascii-tcp", "host": "127.0.0.1", "port": 4000, "read_command": "READ"
        }),
        #[cfg(feature = "ascii-serial")]
        json!({
            "type": "ascii-serial", "port_path": "COM3", "read_command": "READ"
        }),
    ];
    for protocol in protocols {
        let shared = json!({
            "protocol": protocol, "data_type": "double",
            "metadata": {"control": {"limit_low": 0, "limit_high": 100, "min_step": 1}},
            "reset_timeout": 500, "reset_default": 0,
            "max_points": 50, "chart_type": "line",
            "options": ["Off", "On"], "color": "green", "write_value": 0
        });
        #[cfg(feature = "modbus")]
        let shared = {
            let mut shared = shared;
            shared["server"] = json!({
                "proxy_register": 10,
                "protocol": {"type": "modbus-tcp", "register_type": "coil"}
            });
            shared
        };
        let mut expanded = shared.clone();
        expanded.as_object_mut().unwrap().extend(
            json!({"id": "value", "type": "toggle_button", "label": "Value"})
                .as_object()
                .unwrap()
                .clone(),
        );
        let expected: WidgetConfig = serde_json::from_value(expanded).unwrap();
        let screen: ScreenConfig = serde_json::from_value(json!({
            "id": "main", "title": "Main",
            "presets": {"shared": shared},
            "widgets": [{"id": "value", "type": "toggle_button", "label": "Value", "preset": "shared"}]
        })).unwrap();
        assert_eq!(canonical(&screen.widgets[0]), canonical(&expected));
    }
}

#[cfg(feature = "modbus")]
#[test]
fn modbus_alias_overrides_replace_inherited_spellings() {
    for (defaults, overrides) in [
        (
            json!({"unit_id": 1, "min_poll_interval_ms": 500}),
            json!({"slave_id": 2, "poll_interval_ms": 1000}),
        ),
        (
            json!({"slave_id": 1, "poll_interval_ms": 500}),
            json!({"unit_id": 2, "min_poll_interval_ms": 1000}),
        ),
    ] {
        let mut protocol = json!({
            "type": "modbus-tcp", "host": "127.0.0.1",
            "register": 1, "register_type": "coil"
        });
        protocol
            .as_object_mut()
            .unwrap()
            .extend(defaults.as_object().unwrap().clone());
        let screen: ScreenConfig = serde_json::from_value(json!({
            "id": "main", "title": "Main",
            "defaults": {"protocol": protocol},
            "widgets": [{
                "id": "value", "type": "led", "label": "Value", "protocol": overrides
            }]
        }))
        .unwrap();
        let protocol = screen.widgets[0].modbus_tcp().unwrap();
        assert_eq!(protocol.unit_id, 2);
        assert_eq!(protocol.min_poll_interval_ms, 1000);
    }
}

#[test]
fn compact_example_matches_explicit_configuration_and_reduces_json_size() {
    let example_path = std::path::Path::new("examples").join("compact_app.json");
    let compact: Value =
        serde_json::from_str(&std::fs::read_to_string(&example_path).unwrap()).unwrap();
    let mut explicit = compact.clone();
    let shared = explicit.as_object_mut().unwrap().remove("presets").unwrap()["setpoint"].clone();
    explicit.as_object_mut().unwrap().remove("defaults");
    explicit["screens"][0]
        .as_object_mut()
        .unwrap()
        .remove("defaults");
    let group = &mut explicit["screens"][0]["widgets"][0];
    group.as_object_mut().unwrap().remove("defaults");
    for widget in group["children"].as_array_mut().unwrap() {
        widget.as_object_mut().unwrap().remove("preset");
        widget
            .as_object_mut()
            .unwrap()
            .extend(shared.as_object().unwrap().clone());
        widget["style"] = json!({"width": "300px", "height": "60px"});
    }
    let compact_bytes = serde_json::to_vec(&compact).unwrap().len();
    let explicit_bytes = serde_json::to_vec(&explicit).unwrap().len();
    assert!(
        compact_bytes * 100 < explicit_bytes * 80,
        "expected at least 20% less JSON: compact={compact_bytes}, explicit={explicit_bytes}"
    );
    let compact: AppConfig = serde_json::from_value(compact).unwrap();
    let explicit: AppConfig = serde_json::from_value(explicit).unwrap();
    assert_eq!(canonical(&compact), canonical(&explicit));
    let loaded = AppConfig::load(example_path.to_str().unwrap()).unwrap();
    assert_eq!(canonical(&loaded), canonical(&compact));

    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "mycela-presets-{}-{unique}.json",
        std::process::id()
    ));
    loaded.screens[0].save(path.to_str().unwrap()).unwrap();
    let roundtrip = ScreenConfig::load(path.to_str().unwrap());
    std::fs::remove_file(path).unwrap();
    assert_eq!(
        canonical(&roundtrip.unwrap()),
        canonical(&loaded.screens[0])
    );
}
