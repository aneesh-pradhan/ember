# Animations

ember has a few small motions. They're cosmetic and don't change what the agent does:

- **Startup:** the banner's sparkles twinkle once, for about a second, then hold still.
- **Mascot:** at an idle prompt, the kawaii mascot's eyes blink every 2.5 to 4 seconds.
- **Working:** a sparkle spinner sits beside the status line, and the line names the step ember is on, such as "Reading files…" or "Running tests…".
- **Fix verified:** a short burst of sparkles, about half a second, when `/fix` produces a passing fix.
- **Errors:** the status line flashes the error color once.

Turn them off with `/animation off`, and back on with `/animation on`. The choice is saved to `~/.config/ember/config.toml`, which also takes these settings:

```toml
[animation]
enabled = true
speed = "normal"     # slow | normal | fast
mascot_blink = true
```

Animations stay off when stdout isn't a terminal, when `CI` or `NO_COLOR` is set, or when `EMBER_REDUCED_MOTION=1`. The working spinner that was already there keeps its existing behavior.
