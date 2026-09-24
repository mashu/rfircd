# Packaging

A `v*` tag on `main` is the release. CI then:

1. Builds static musl binaries for `x86_64` and `aarch64`
2. Wraps them as AppImage (x86_64), `.run` installer, and `.tar.gz`
3. Attaches those files to the GitHub Release
4. Deploys this documentation under that version and aliases it as **stable**

`main` itself deploys the **dev** docs. The version menu at the top of the docs
site is how you switch.

## Local dist

```sh
cargo build --release
./packaging/linux/build-dist.sh
# dist/rfircd-<arch>.AppImage
# dist/rfircd-<arch>.run
# dist/rfircd-<arch>-linux.tar.gz
```

Musl (what CI uses):

```sh
cargo zigbuild --release --target x86_64-unknown-linux-musl
./packaging/linux/build-dist.sh \
  --bin-dir target/x86_64-unknown-linux-musl/release --arch x86_64
```

## systemd

`packaging/rfircd.service` — SIGINT on stop so the station can identify
before it goes silent. The `.run` installer drops a user unit under
`~/.config/systemd/user/` or a system unit with `--system`.
