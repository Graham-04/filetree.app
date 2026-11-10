#!/bin/sh
# Builds "Filetree.app" in target/bundle with an ad-hoc signature, for
# running on this Mac, including the folder-access prompts that running from
# Terminal hides (Terminal's own access is inherited). The identifier is
# local.filetree unless BUNDLE_ID is set. A build made this way never
# updates itself.
#
# Rust embeds source paths in the binary for error messages, including every
# dependency under ~/.cargo and the toolchain under ~/.rustup. They are
# rewritten to start with "~", and the project's own to be relative to it, so
# the app never contains the builder's home folder, username, or where they
# keep the project; the build fails if the home folder slips through. The bundle
# builds in its own target folder, so these flags do not force everyday
# builds to recompile.
#
# An ad-hoc signature changes on every build, so macOS asks for folder access
# again after each rebuild. Distributing the app needs a Developer ID
# signature and Apple's notarization.
set -eu
cd "$(dirname "$0")/.."
bundle_id="${BUNDLE_ID:-$(plutil -extract CFBundleIdentifier raw -o - packaging/Info.plist)}"

export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME=~ --remap-path-prefix=$PWD=."
export CARGO_TARGET_DIR=target/bundle
cargo build --release
binary="$CARGO_TARGET_DIR/release/filetree"
if strings "$binary" | grep -F -q "$HOME"; then
    echo "The binary still contains $HOME; not bundling it." >&2
    exit 1
fi
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
app="$CARGO_TARGET_DIR/Filetree.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/filetree"
sed "s/__VERSION__/$version/g" packaging/Info.plist > "$app/Contents/Info.plist"
# The icon: Assets.car, which macOS 26 and later draw from the Icon Composer
# icon, and Filetree.icns for older systems. actool comes with Xcode; without
# it the app builds with the generic icon.
if xcrun --find actool >/dev/null 2>&1; then
    scratch=$(mktemp -d)
    xcrun actool "$PWD/packaging/Filetree.icon" --compile "$PWD/$app/Contents/Resources" \
        --app-icon Filetree --platform macosx --minimum-deployment-target 11.0 \
        --output-partial-info-plist "$scratch/partial.plist" --output-format human-readable-text >"$scratch/log"
    # actool reports some failures only in its output.
    if [ ! -f "$app/Contents/Resources/Assets.car" ] || [ ! -f "$app/Contents/Resources/Filetree.icns" ]; then
        cat "$scratch/log" >&2
        exit 1
    fi
    rm -rf "$scratch"
else
    echo "actool not found (it comes with Xcode); building without the app icon." >&2
fi
plutil -replace CFBundleIdentifier -string "$bundle_id" "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist" >/dev/null
codesign --force --sign - "$app"
echo "Built $app"
echo "To see first-run prompts again: tccutil reset All $bundle_id"
