# Source before building Bloo locally: `source bloo/env.sh`
# Tuned for an 8 GB M2: fewer parallel rustc jobs, lighter debug info.
export CARGO_BUILD_JOBS=4
export CARGO_PROFILE_DEV_DEBUG=line-tables-only

# webrtc-sys downloads a 243 MB prebuilt with a short HTTP timeout that fails on slow links.
# Fetch it once with curl into ~/.cache/bloo and point the build at it:
#   curl -fL -o /tmp/webrtc.zip https://github.com/zed-industries/livekit-rust-sdks/releases/download/webrtc-0001d84-4/webrtc-mac-arm64-release.zip
#   unzip -q /tmp/webrtc.zip -d ~/.cache/bloo/webrtc-0001d84-4
# The tag must match WEBRTC_TAG in the livekit-rust-sdks rev pinned in Cargo.lock.
export LK_CUSTOM_WEBRTC="$HOME/.cache/bloo/webrtc-0001d84-4/mac-arm64-release"
