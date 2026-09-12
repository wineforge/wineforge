# Runtime capabilities

Wineforge treats optional engine behavior as a versioned contract. An engine
manifest advertises only features present in that engine; recipes state minimum
requirements, and profiles can disable, override, or extend recipe
recommendations. Preparation and installation fail closed when the effective
configuration needs an unavailable capability.

```toml
[capabilities."input.keyboard.mapping"]
version = 1

[capabilities."input.scroll.precise"]
version = 1

[capabilities."macos.window-isolation.strict"]
version = 1
```

Composed capabilities describe features implemented jointly by the engine and
Wineforge runtime:

```toml
[composed_capabilities."mcp.forwarding"]
version = 1

[[composed_capabilities."mcp.forwarding".requires]]
name = "bridge.socket"
minimum_version = 1
provider = "engine"

[[composed_capabilities."mcp.forwarding".requires]]
name = "mcp.broker"
minimum_version = 1
provider = "runtime"

[[composed_capabilities."mcp.forwarding".requires]]
name = "mcp.bridge"
minimum_version = 1
provider = "runtime"
```

An unmet composition is unavailable unless effective configuration requires it.

## Layered input configuration

Recipes can recommend application behavior:

```toml
[configuration.keyboard]
preset = "mac-native"

[[configuration.keyboard.mappings]]
id = "save"
from = "command+s"
to = "control+s"

[configuration.scrolling.settings]
precise = true
momentum = true

[[configuration.scrolling.mappings]]
id = "pan"
input = "space+pointer-drag"
action = "drag-scroll"
axis = "horizontal"
```

Profiles can disable recipe layers, override mappings by stable ID, and add
machine-local mappings:

```toml
[configuration.keyboard.recipe]
preset = true
mappings = true

[[configuration.keyboard.overrides]]
id = "save"
enabled = false

[configuration.scrolling]
recipe_settings = false
recipe_mappings = true
```

Capabilities are checked individually. Keyboard mappings require
`input.keyboard.mapping`, precise scrolling requires `input.scroll.precise`,
keyboard scrolling requires `input.scroll.keyboard-to-scroll`, and drag
scrolling requires `input.scroll.drag`.

## macOS window isolation

Recipes may recommend strict isolation for transient windows; profiles can
disable or replace the recommendation:

```toml
[configuration.windowing.macos]
isolation = "strict"
```

Strict mode requires `macos.window-isolation.strict`. Wineforge transports the
resolved value as `WINEFORGE_STRICT_WINDOW_ISOLATION=true` to the Wine process.
The engine acts only on its own windows and requires no screen recording,
Accessibility, input monitoring, or global window enumeration.

## MCP bindings

Recipes declare symbolic endpoints. Profiles bind them to trusted machine-local
server registrations. The resolver hands validated endpoints and bindings to
the broker adapter; recipes cannot provide native commands.

When a bound application launches, Wineforge installs the release-matched
`wineforge-mcp-bridge.exe` at
`C:\.wineforge\bin\wineforge-mcp-bridge.exe` inside its prefix. It also sets
`WINEFORGE_MCP_BRIDGE` to that stable guest path and
`WINEFORGE_MCP_CONFIG` to a per-launch configuration. A Windows application
starts an endpoint as a normal MCP stdio child:

```powershell
& $env:WINEFORGE_MCP_BRIDGE --endpoint application-tools
```

The Windows bridge uses authenticated IPv4 loopback rather than a host Unix
socket. Unix-domain socket paths are not a portable Windows/Wine interface;
loopback works through Winsock while remaining process-scoped by a random
per-launch token and broker method permissions.

```console
wineforge engine capabilities engine.toml
wineforge recipe explain recipe.toml --profile profile.toml --engine-manifest engine.toml
```
