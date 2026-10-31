#!/bin/sh
set -eu

# Run from the Embed-Cores build phase. Copies the shared SDL2 and every
# emulator core dylib into the app bundle's Frameworks folder, and signs them
# with the same identity as the app.
#
# This is a script rather than a Copy Files phase because the source paths
# depend on the configuration and SDK, and because the number of cores is not
# fixed.

DESTINATION="${TARGET_BUILD_DIR:?}/${FRAMEWORKS_FOLDER_PATH:?}"
mkdir -p "$DESTINATION"

sign() {
    # CODE_SIGNING_ALLOWED=NO is used for the unsigned build that becomes a
    # release IPA; the signer the user runs handles Frameworks itself.
    if [ "${CODE_SIGNING_ALLOWED:-YES}" = NO ] || [ -z "${EXPANDED_CODE_SIGN_IDENTITY:-}" ]; then
        return 0
    fi
    codesign --force --sign "$EXPANDED_CODE_SIGN_IDENTITY" \
        ${OTHER_CODE_SIGN_FLAGS:-} --timestamp=none "$1"
}

embed() {
    source_path=$1
    if [ ! -f "$source_path" ]; then
        echo "error: $source_path was not built. Run build-sdl-shared.sh and build-rust.sh first." >&2
        exit 1
    fi
    destination_path="$DESTINATION/$(basename "$source_path")"
    rsync -a --delete "$source_path" "$destination_path"

    # dyld rejects a dylib whose string pool is not 8-byte aligned. The cores
    # are corrected as they are built, but a stale or hand-copied one would
    # only fail at runtime, on the device, so check here too.
    string_pool_offset=$(otool -l "$destination_path" \
        | awk '/LC_SYMTAB/ { found = 1 } found && $1 == "stroff" { print $2; exit }')
    if [ -n "$string_pool_offset" ] && [ $((string_pool_offset % 8)) -ne 0 ]; then
        echo "error: $(basename "$source_path") has a mis-aligned LINKEDIT string pool" >&2
        echo "Run platform/ios/scripts/fix-linkedit-alignment.py on it." >&2
        exit 1
    fi

    # Same reasoning for the minimum OS version. Cargo does not rebuild when
    # IPHONEOS_DEPLOYMENT_TARGET changes, and a core built for a newer iOS than
    # the app claims to support will not load at all on the older devices — on
    # the device, at launch, with nothing to explain it.
    built_for=$(vtool -show-build "$destination_path" 2>/dev/null |
        awk '/minos/ { print $2; exit }')
    if [ -n "$built_for" ] && [ -n "${IPHONEOS_DEPLOYMENT_TARGET:-}" ] \
        && [ "$built_for" != "$IPHONEOS_DEPLOYMENT_TARGET" ]; then
        echo "error: $(basename "$source_path") was built for iOS $built_for," \
            "but this app targets $IPHONEOS_DEPLOYMENT_TARGET." >&2
        echo "Delete its build/rust-ios-native/<target> directory and build it again." >&2
        exit 1
    fi

    sign "$destination_path"
    echo "Embedded $(basename "$source_path")"
}

embed "${SDL_SHARED_DIR:?}/lib/libSDL2-2.0.0.dylib"

embedded_a_core=0
for core in "${RUST_LIB_DIR:?}"/*.dylib; do
    [ -f "$core" ] || continue
    embed "$core"
    embedded_a_core=1
done

# Cores built from another repository bring their own default options, named
# for the core (see build-rust.sh and CoreKind.defaultOptionsFileName).
RESOURCES="${TARGET_BUILD_DIR:?}/${UNLOCALIZED_RESOURCES_FOLDER_PATH:?}"
for options in "$RUST_LIB_DIR"/touchHLE_default_options.*.txt; do
    [ -f "$options" ] || continue
    cp -f "$options" "$RESOURCES/"
    echo "Embedded $(basename "$options")"
done
for core_resources in "$RUST_LIB_DIR"/*_resources; do
    [ -d "$core_resources" ] || continue
    rsync -a --delete "$core_resources/" "$RESOURCES/$(basename "$core_resources")/"
    echo "Embedded $(basename "$core_resources")"
done

if [ "$embedded_a_core" = 0 ]; then
    echo "error: no core dylib found in $RUST_LIB_DIR. Run build-rust.sh first." >&2
    exit 1
fi
