# dmgbuild settings — the GlobalTokenTracker installer window.
#   dmgbuild -s macos/dmg/settings.py -D app=<GlobalTokenTracker.app> \
#            -D background=macos/dmg/background.tiff "GlobalTokenTracker" <out.dmg>
# (dmgbuild execs this file without __file__, so paths come in as defines.)
# dmgbuild writes Finder's .DS_Store itself (no AppleScript / Finder session),
# so the layout is identical on CI runners and developer Macs.
import os.path

application = defines.get("app", "target/macos/GlobalTokenTracker.app")  # noqa: F821
appname = os.path.basename(application)

# Volume
format = defines.get("format", "ULMO")  # noqa: F821  LZMA; macOS 10.15+
filesystem = "APFS"
size = None  # computed from contents
files = [application]
symlinks = {"Applications": "/Applications"}
icon = os.path.join(application, "Contents", "Resources", "AppIcon.icns")  # volume icon
# No hide_extensions: dmgbuild sets it as FinderInfo on the bundle root, which
# breaks `codesign --verify --strict` (Finder hides ".app" by default anyway).

# Window — content size equals background.tiff (660×420 pt); @2x inside.
background = defines["background"]  # noqa: F821  macos/dmg/background.tiff
show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
window_rect = ((200, 140), (660, 420))
default_view = "icon-view"
include_icon_view_settings = True

# Icons — centres must match the slots drawn by make-background.swift.
arrange_by = None
icon_size = 128
text_size = 13
label_pos = "bottom"
icon_locations = {
    appname: (170, 230),
    "Applications": (490, 230),
}
