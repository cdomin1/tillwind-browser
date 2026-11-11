#!/bin/sh
set -eu

prefix=${1:?installation prefix required}
app_id=io.github.cdomin1.Tillwind

install -Dm755 target/release/tillwind "$prefix/bin/tillwind"
install -Dm644 "packaging/$app_id.desktop" "$prefix/share/applications/$app_id.desktop"
install -Dm644 assets/tillwind-icon.png "$prefix/share/icons/hicolor/1024x1024/apps/$app_id.png"
