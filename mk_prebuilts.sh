#!/bin/bash

set -o errexit
set -o xtrace

VERSION="$(python3 -c 'import toml; print(toml.load("Cargo.toml")["package"]["version"])')"

TARGETS=("x86_64-unknown-linux-gnu" "aarch64-unknown-linux-gnu" "x86_64-pc-windows-gnu")

cargo clean
for target in "${TARGETS[@]}"; do
    cargo build --release --target "${target}"
done

rm -fr prebuilts 2>/dev/null
prebuilts="./prebuilts/${VERSION}"
mkdir -p "${prebuilts}"
for target in "${TARGETS[@]}"; do
    in="./target/${target}/release/ut325f-fourup"
    out="${prebuilts}/ut325f-fourup-${VERSION}-${target}"
    suffix=""
    if [[ "${target}" =~ windows ]]; then
       suffix=".exe"
    fi
    cp -p "${in}${suffix}" "${out}${suffix}"
done

