#!/usr/bin/env bash
#
# Thin wrapper around `dx` that installs the favetto launcher-icon overlay.
# `dx` 0.7.10 hardcodes the stock Android launcher resources, so the init
# script rewrites them from `web/android/res` before Gradle merges resources.
#
#   ./android/dx.sh bundle --locked --release --platform android \
#       --features mobile --target aarch64-linux-android --package-types apk
#   ./android/dx.sh serve --platform android --features mobile
set -euo pipefail
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
web_dir="$(cd -- "$script_dir/.." && pwd)"
gradle_home="${GRADLE_USER_HOME:-$HOME/.gradle}"
mkdir -p "$gradle_home/init.d"
cp "$script_dir/gradle/favetto-icons.gradle.kts" \
    "$gradle_home/init.d/favetto-icons.gradle.kts"
export FAVETTO_ANDROID_RES="$script_dir/res"
cd "$web_dir"
exec dx "$@"
