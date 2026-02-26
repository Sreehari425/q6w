# Maintainer: Sreehari Anil <sreehari7102008@gmail.com>
pkgname=q6w
pkgver=0.0.0.g84dddc3
pkgrel=1
pkgdesc="Wayland video wallpaper: GStreamer decode, wlr-layer-shell surface, minimal RAM footprint"
arch=('x86_64')
url="https://github.com/Sreehari425/q6w"
license=('AGPL-3.0-only')
depends=(
    'wayland'
    'gstreamer'
    'gst-plugins-base'
    'gst-plugins-good'
    'gst-plugins-bad'
    'vulkan-icd-loader'
    'gst-plugin-va'
)
makedepends=(
    'cargo'
    'git'
)
source=("git+https://github.com/Sreehari425/q6w.git")
sha256sums=('SKIP')

pkgver() {
    cd "$pkgname"
    local cargo_ver=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)
    local git_hash=$(git rev-parse --short=7 HEAD)
    printf "%s.g%s" "$cargo_ver" "$git_hash"
}

prepare() {
    cd "$pkgname"
    export RUSTUP_TOOLCHAIN=stable
    cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
    cd "$pkgname"
    export RUSTUP_TOOLCHAIN=stable
    cargo build --frozen --release
}

check() {
    cd "$pkgname"
    export RUSTUP_TOOLCHAIN=stable
    cargo test --frozen --release
}

package() {
    cd "$pkgname"
    install -Dm755 "target/release/$pkgname" "$pkgdir/usr/bin/$pkgname"
    install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
