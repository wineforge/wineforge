use std::collections::BTreeMap;

use wineforge_core::{
    Capability, CapabilityComposition, CapabilityInventory, CapabilityProvider,
    CapabilityRequirement, CapabilitySet, KeyboardMapping, KeyboardOverride, KeyboardPreset,
    MacosWindowIsolation, McpBinding, McpEndpoint, McpTransport, ProfileConfiguration,
    RecipeConfiguration, ScrollAction, ScrollAxis, ScrollingMapping, resolve_configuration,
};

fn requirement(name: &str, provider: CapabilityProvider) -> CapabilityRequirement {
    CapabilityRequirement {
        name: name.into(),
        minimum_version: 1,
        provider,
    }
}

#[test]
fn composed_capability_requires_engine_and_runtime_parts() {
    let inventory = CapabilityInventory {
        engine: CapabilitySet(BTreeMap::from([(
            "bridge.socket".into(),
            Capability { version: 1 },
        )])),
        runtime: CapabilitySet(BTreeMap::from([(
            "mcp.broker".into(),
            Capability { version: 1 },
        )])),
        composed: Default::default(),
    };
    let composed = BTreeMap::from([(
        "mcp.forwarding".into(),
        CapabilityComposition {
            version: 1,
            requires: vec![
                requirement("bridge.socket", CapabilityProvider::Engine),
                requirement("mcp.broker", CapabilityProvider::Runtime),
            ],
        },
    )]);
    let resolved = inventory.compose(&composed).unwrap();
    resolved
        .satisfy(&[requirement("mcp.forwarding", CapabilityProvider::Composed)])
        .unwrap();
}

#[test]
fn unavailable_composed_capability_reports_the_missing_part() {
    let composed = BTreeMap::from([(
        "mcp.forwarding".into(),
        CapabilityComposition {
            version: 1,
            requires: vec![requirement("mcp.broker", CapabilityProvider::Runtime)],
        },
    )]);
    let inventory = CapabilityInventory::default().compose(&composed).unwrap();
    let error = inventory
        .satisfy(&[requirement("mcp.forwarding", CapabilityProvider::Composed)])
        .unwrap_err()
        .to_string();
    assert!(error.contains("mcp.forwarding"));
    assert!(error.contains("unavailable"));
}

#[test]
fn profile_disables_overrides_and_adds_recipe_configuration() {
    let mut recipe = RecipeConfiguration::default();
    recipe.keyboard.preset = Some(KeyboardPreset::MacNative);
    recipe.keyboard.mappings = vec![
        KeyboardMapping {
            id: "save".into(),
            from: "command+s".into(),
            to: "control+s".into(),
        },
        KeyboardMapping {
            id: "find".into(),
            from: "command+f".into(),
            to: "control+f".into(),
        },
    ];
    recipe.scrolling.mappings.push(ScrollingMapping {
        id: "down".into(),
        input: "option+j".into(),
        action: ScrollAction::Scroll,
        axis: Some(ScrollAxis::Vertical),
        amount: Some(3),
    });
    recipe.mcp.endpoints.push(McpEndpoint {
        id: "tools".into(),
        transport: McpTransport::Stdio,
    });

    let mut profile = ProfileConfiguration::default();
    profile.keyboard.overrides = vec![
        KeyboardOverride {
            id: "save".into(),
            enabled: true,
            from: None,
            to: Some("control+shift+s".into()),
        },
        KeyboardOverride {
            id: "find".into(),
            enabled: false,
            from: None,
            to: None,
        },
    ];
    profile.keyboard.mappings.push(KeyboardMapping {
        id: "local".into(),
        from: "command+r".into(),
        to: "control+r".into(),
    });
    profile.scrolling.recipe_mappings = false;
    profile.mcp.bindings.push(McpBinding {
        endpoint: "tools".into(),
        server: "trusted-tools".into(),
        permissions: vec!["tools.call".into()],
    });

    let effective = resolve_configuration(&recipe, &profile).unwrap();
    assert_eq!(effective.keyboard_preset, Some(KeyboardPreset::MacNative));
    assert_eq!(effective.keyboard_mappings.len(), 2);
    assert!(
        effective
            .keyboard_mappings
            .iter()
            .any(|m| m.id == "save" && m.to == "control+shift+s")
    );
    assert!(effective.scrolling_mappings.is_empty());
    assert_eq!(effective.mcp_bindings[0].server, "trusted-tools");
}

#[test]
fn materialized_profile_preserves_effective_recipe_configuration() {
    let mut recipe = RecipeConfiguration::default();
    recipe.keyboard.preset = Some(KeyboardPreset::MacNative);
    recipe.scrolling.settings.precise = Some(true);
    recipe.mcp.endpoints.push(McpEndpoint {
        id: "application-tools".into(),
        transport: McpTransport::Stdio,
    });
    recipe.windowing.macos.isolation = Some(wineforge_core::MacosWindowIsolation::Strict);
    let effective = resolve_configuration(&recipe, &ProfileConfiguration::default()).unwrap();
    let materialized = effective.clone().into_profile_configuration();
    let reloaded = resolve_configuration(&RecipeConfiguration::default(), &materialized).unwrap();
    assert_eq!(reloaded, effective);
}

#[test]
fn bindings_cannot_target_undeclared_endpoints() {
    let mut profile = ProfileConfiguration::default();
    profile.mcp.bindings.push(McpBinding {
        endpoint: "missing".into(),
        server: "local".into(),
        permissions: vec![],
    });
    let error = resolve_configuration(&RecipeConfiguration::default(), &profile)
        .unwrap_err()
        .to_string();
    assert!(error.contains("unknown endpoint missing"));
}

#[test]
fn effective_configuration_requires_each_requested_engine_feature() {
    let mut recipe = RecipeConfiguration::default();
    recipe.keyboard.preset = Some(KeyboardPreset::MacNative);
    recipe.keyboard.mappings.push(KeyboardMapping {
        id: "copy".into(),
        from: "command+c".into(),
        to: "control+c".into(),
    });
    recipe.scrolling.settings.precise = Some(true);
    recipe.scrolling.settings.momentum = Some(true);
    recipe.scrolling.mappings.push(ScrollingMapping {
        id: "pan".into(),
        input: "space+pointer-drag".into(),
        action: ScrollAction::DragScroll,
        axis: Some(ScrollAxis::Horizontal),
        amount: None,
    });
    recipe.windowing.macos.isolation = Some(MacosWindowIsolation::Strict);

    let effective = resolve_configuration(&recipe, &ProfileConfiguration::default()).unwrap();
    let names = effective
        .engine_requirements()
        .into_iter()
        .map(|requirement| requirement.name)
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "input.keyboard.mapping",
            "input.keyboard.preset.mac-native",
            "input.scroll.drag",
            "input.scroll.horizontal",
            "input.scroll.keyboard-to-scroll",
            "input.scroll.momentum",
            "input.scroll.precise",
            "macos.window-isolation.strict",
        ]
    );
}

#[test]
fn profile_can_disable_recipe_scroll_settings_and_window_isolation() {
    let mut recipe = RecipeConfiguration::default();
    recipe.scrolling.settings.precise = Some(true);
    recipe.windowing.macos.isolation = Some(MacosWindowIsolation::Strict);
    let mut profile = ProfileConfiguration::default();
    profile.scrolling.recipe_settings = false;
    profile.windowing.recipe_settings = false;

    let effective = resolve_configuration(&recipe, &profile).unwrap();
    assert_eq!(effective.scrolling_settings.precise, None);
    assert_eq!(effective.macos_window_isolation, None);

    profile.scrolling.settings.precise = Some(false);
    profile.windowing.macos.isolation = Some(MacosWindowIsolation::Standard);
    let effective = resolve_configuration(&recipe, &profile).unwrap();
    assert_eq!(effective.scrolling_settings.precise, Some(false));
    assert_eq!(
        effective.macos_window_isolation,
        Some(MacosWindowIsolation::Standard)
    );
}
