# Quota Widget 0.3.10

Windows taskbar pins now have an icon available even when the application is
closed. Earlier builds set the running window's icon but contained no PE icon
resource, so a pinned shortcut falling back to the EXE could show no app icon.

The existing PNG artwork is encoded at 16, 24, 32, 48, 64, 128 and 256 pixels and
embedded as Windows RT_ICON/RT_GROUP_ICON resources. No artwork, accounting,
token import, settings or database schema changes are made. Resource generation
uses the Rust standard library; MSVC needs no additional resource compiler.
GNU Windows cross-builds use MinGW windres (overridable with WINDRES).

Fixed-path deployment also supplies quota-widget.ico for shortcut icon paths.
Keep launching the same installed quota-widget.exe after updates. Existing pins
that point to old version folders should use the fixed deployment path instead.

Windows resource format reference:
https://learn.microsoft.com/en-us/windows/win32/menurc/resource-file-formats
