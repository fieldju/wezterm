---
tags:
  - appearance
---
# `win32_window_background_blur = 0`

{{since('nightly')}}

When combined with [window_background_opacity](window_background_opacity.md),
configures the blur radius used when compositing what is behind the window on
Windows.

Unlike [win32_system_backdrop](win32_system_backdrop.md), whose Acrylic
effect always uses the strength chosen by Windows, this lets you choose the
amount, from a light frosting to a heavy blur.

```lua
config.front_end = 'WebGpu'
config.window_background_opacity = 0.4
config.win32_window_background_blur = 4
```

Notes:

* Requires `front_end = "WebGpu"`; it has no effect with the OpenGL or
  Software front ends.
* When non-zero, it replaces `win32_system_backdrop`.
* Changing the amount takes effect immediately. Changing it from `0` to a
  non-zero value (or back) applies to newly created windows.

The default value for `win32_window_background_blur` is `0`.

See also [macos_window_background_blur](macos_window_background_blur.md) and
[wayland_window_background_blur](wayland_window_background_blur.md) for
similar effects on other platforms.
