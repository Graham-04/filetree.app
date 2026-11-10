#!/bin/sh
# Usage: scripts/dmg.sh <app> <dmg>
#
# Packs an app into a compressed disk image that opens to the app beside an
# Applications shortcut, ready to drag across. Finder lays out the window, so
# the first run asks to let the terminal control Finder. Signing and
# notarizing the image are left to the caller.
set -eu
app="$1"
dmg="$2"
name=$(basename "$app" .app)
# Finder finds the disk by name, so another one by that name would get the layout.
if [ -e "/Volumes/$name" ]; then
    echo "Eject the disk named $name first." >&2
    exit 1
fi

work=$(mktemp -d)
trap 'hdiutil detach -quiet "$work/mount" 2>/dev/null || true; rm -rf "$work"' EXIT
mkdir "$work/contents"
ditto "$app" "$work/contents/$name.app"
ln -s /Applications "$work/contents/Applications"

# A writable image first, so Finder can save the window layout into it.
hdiutil create -quiet -volname "$name" -srcfolder "$work/contents" -fs HFS+ \
    -format UDRW "$work/layout.dmg"
mkdir "$work/mount"
hdiutil attach -quiet -nobrowse -noautoopen -mountpoint "$work/mount" "$work/layout.dmg"
osascript - "$work/mount" "$name.app" <<'APPLESCRIPT'
on run {mountPoint, appName}
    tell application "Finder"
        set theDisk to (POSIX file mountPoint as alias)
        open theDisk
        -- Finder opens the window asynchronously; wait until it is there.
        repeat 50 times
            if exists container window of theDisk then exit repeat
            delay 0.1
        end repeat
        delay 0.5
        set theWindow to container window of theDisk
        set current view of theWindow to icon view
        set toolbar visible of theWindow to false
        set statusbar visible of theWindow to false
        set bounds of theWindow to {200, 200, 740, 560}
        set options to icon view options of theWindow
        set arrangement of options to not arranged
        set icon size of options to 128
        set text size of options to 13
        set position of item appName of theDisk to {140, 160}
        set position of item "Applications" of theDisk to {400, 160}
        update theDisk without registering applications
        delay 1
        close theWindow
    end tell
end run
APPLESCRIPT
sync
hdiutil detach -quiet "$work/mount"

rm -f "$dmg"
hdiutil convert -quiet "$work/layout.dmg" -format UDZO -imagekey zlib-level=9 -o "$dmg"
echo "Built $dmg"
