# Warp Personal Build — notes

This fork (`tpc-faheem/warp`) exists for one reason: a **personal build of Warp
with Chrome-style tab tear-off enabled**, plus room to experiment (e.g. the
UPG-native terminal direction).

> Upstream `warpdotdev/warp` is the source of truth. This file documents only
> what's different here and how to (re)build it.

## What's changed vs upstream

Warp already ships a complete cross-window tab-drag implementation
(`app/src/workspace/cross_window_tab_drag.rs`) — including promoting a dragged
tab into a brand-new window (`finalize_preview_as_new_window`). It's gated behind
the `drag_tabs_to_windows` Cargo feature (plus `grouped_tabs`), which is **not**
in the app's `default` feature set, so the public `warp-oss` binary compiles it
out.

The only change here: **both features added to `default`** in `app/Cargo.toml`,
so a plain build includes tab tear-off.

- Branch: `enable-tab-tearoff`
- Diff: `app/Cargo.toml` `default = [ … "drag_tabs_to_windows", "grouped_tabs" ]`

That's it. No source changes — we unlocked a finished, dogfood-gated feature.

## Build prerequisites (macOS, Intel verified)

| Dep | How |
|---|---|
| Rust 1.92.0 | `rustup` auto-installs it from `rust-toolchain.toml` on first `cargo` run |
| `protoc` | `brew install protobuf` (a build script needs it — without it the build fails on `warp_multi_agent_api`) |
| Xcode + Metal | full Xcode (Metal compiler ships in 15.2; the `xcodebuild -downloadComponent MetalToolchain` step in `script/macos/install_build_deps` is Xcode 16+ only and not needed on 15.x) |

## Build & run

```bash
# debug (faster compile, ~8 min cold)
cargo run --bin warp-oss

# release (optimized daily driver, ~28 min cold)
cargo build --release --bin warp-oss
./target/release/warp-oss
```

A convenience symlink is on PATH: `warp-oss` → the release binary.

## Make it a clickable .app

`cargo bundle` is configured for this bin in `app/Cargo.toml`
(`[package.metadata.bundle.bin.warp-oss]`, identifier `dev.warp.WarpOss`, icon
included). The official `script/macos/bundle` also codesigns + notarizes with
Warp's private Apple certs — skip that; for a personal local app we only need the
bundle + an ad-hoc signature.

```bash
cargo install cargo-bundle
cd app && cargo bundle --release --bin warp-oss && cd ..   # MUST run from app/ (bundle metadata lives there)
# → target/release/bundle/osx/WarpOss.app  (+ a WarpOss.dmg)

# ad-hoc sign so Gatekeeper lets it launch, then install:
codesign --force --deep --sign - target/release/bundle/osx/WarpOss.app
cp -R target/release/bundle/osx/WarpOss.app /Applications/
xattr -dr com.apple.quarantine /Applications/WarpOss.app   # first launch only
open /Applications/WarpOss.app
```

Identity: `dev.warp.WarpOss`, name `WarpOss`. Runs the same code as the release
binary (assets are embedded via rust-embed, so the bundle is self-contained).

### Custom (tinted) icon

The stock bundle uses Warp's OSS icon — identical to real Warp in the Dock. To
tint it (violet, to mark it as the personal/dev build) and keep transparency:

```bash
python3 - <<'PY'
from PIL import Image
b=Image.open("app/channels/oss/icon/no-padding/512x512.png").convert("RGBA")
a=b.getchannel("A")
tint=Image.composite(Image.new("RGBA",b.size,(138,79,245,130)),Image.new("RGBA",b.size,(0,0,0,0)),a)
Image.alpha_composite(b,tint).save("/tmp/tint.png")
PY
mkdir -p /tmp/AppIcon.iconset
for s in 16 32 128 256 512; do
  sips -z $s $s /tmp/tint.png --out /tmp/AppIcon.iconset/icon_${s}x${s}.png >/dev/null
  sips -z $((s*2)) $((s*2)) /tmp/tint.png --out /tmp/AppIcon.iconset/icon_${s}x${s}@2x.png >/dev/null
done
iconutil -c icns /tmp/AppIcon.iconset -o WarpOss.icns
cp WarpOss.icns /Applications/WarpOss.app/Contents/Resources/WarpOss.icns
codesign --force --deep --sign - /Applications/WarpOss.app   # re-sign after editing resources
touch /Applications/WarpOss.app && killall Dock              # refresh icon cache
```

Rebuilding the binary later doesn't auto-update the installed `.app` — re-run
`cargo bundle` + `cp -R` (then re-apply the icon + re-sign).

## Tab tear-off — how to use it

Drag a tab out of the tab bar into empty space → it becomes its own window.
Drag a tab onto another window's tab bar → it merges in. Single-tab and
multi-tab (grouped) sources both work.

## Staying current with upstream

```bash
git fetch upstream
git rebase upstream/main      # on enable-tab-tearoff; the 5-line diff replays cleanly
cargo build --release --bin warp-oss
```

## Provenance

- Forked + feature unlocked 2026-06-02. The discovery (feature built upstream,
  gated behind the flag) is what surfaced how much Warp machinery we can reuse —
  see the UPG-native-terminal initiative in the TPC monorepo.
- Don't open an upstream PR to flip the flag to `default`: it's a deliberate
  product gate, almost certainly because the feature isn't fully launched.
