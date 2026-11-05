#!/usr/bin/env bash

# Copyright 2022 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0

# fail if we encounter an error, uninitialized variable or a pipe breaks
set -eux -o pipefail

FC_TOOLS_DIR=$(dirname $(realpath $0))
source "$FC_TOOLS_DIR/functions"
FC_ROOT_DIR=$FC_TOOLS_DIR/..

function get-profile-dir {
    case $1 in
        dev)
            echo debug
        ;;
        *)
            echo "$1"
        ;;
    esac
}

function check_swagger_artifact {
    # Validate swagger version against target version.
    local swagger_path version swagger_ver
    swagger_path=$1
    version=$2
    swagger_ver=$(get_swagger_version "$swagger_path")
    if [[ ! $version =~ v$swagger_ver.* ]]; then
        die "Artifact $swagger_path's version: $swagger_ver does not match release version $version."
    fi
}

function check_bin_artifact {
    # Validate binary version against target version.
    local bin_path version bin_version
    bin_path=$1
    version=$2
    bin_version=$($bin_path --version | head -1 | grep -oP ' \Kv.*')
    if [[ "$bin_version" != "$version" ]]; then
        die "Artifact $bin_path's version: $bin_version does not match release version $version."
    fi
}

function strip-and-split-debuginfo {
    local bin=$1
    if [ $bin -ot $bin.debug ]; then
        return
    fi
    echo "STRIP $bin"
    objcopy --only-keep-debug $bin $bin.debug
    chmod a-x $bin.debug
    objcopy --preserve-dates --strip-debug --add-gnu-debuglink=$bin.debug $bin
}

function get-firecracker-version {
    (cd src/firecracker; echo -n v; cargo pkgid | cut -d# -f2 | cut -d: -f2)
}

#### MAIN ####

# defaults
LIBC=musl
PROFILE=dev
MAKE_RELEASE=
PGO=

#### Option parsing

while [[ $# -gt 0 ]]; do
  case $1 in
      --help)
          cat <<EOF
$0 - Build Firecracker

   --profile PROFILE  - Build with the specified Rust profile (default: dev)
   --libc [musl|gnu]  - Build with the specified libc (default: musl)
   --make-release     - Make release artifacts
   --pgo-instrument   - Only build PGO-instrumented binaries under
                        build/pgo/instrumented
   --pgo-use DIR      - Build with the PGO profiles (*.profraw) the training
                        workload collected under DIR
EOF
          exit 0
      ;;
    --profile)
      PROFILE="$2"
      shift 2
      ;;
    --libc)
      LIBC="$2"
      shift 2
      ;;
    --make-release)
      MAKE_RELEASE=true
      shift 1
      ;;
    --pgo-instrument)
      PGO=instrument
      shift 1
      ;;
    --pgo-use)
      PGO=use
      PGO_PROFRAW_DIR="$2"
      shift 2
      ;;
    *)
      echo "Unknown option $1"
      exit 1
      ;;
  esac
done


# workaround until we rebuild devctr
git config --global --replace-all safe.directory '*'

ARCH=$(uname -m)
VERSION=$(get-firecracker-version)
PROFILE_DIR=$(get-profile-dir "$PROFILE")
CARGO_TARGET=$ARCH-unknown-linux-$LIBC
CARGO_TARGET_DIR=build/cargo_target/$CARGO_TARGET/$PROFILE_DIR
RUST_TOOLCHAIN=$(cargo version | cut -f2 -d ' ')

CARGO_REGISTRY_DIR="build/cargo_registry"
CARGO_GIT_REGISTRY_DIR="build/cargo_git_registry"
for dir in "$CARGO_REGISTRY_DIR" "$CARGO_GIT_REGISTRY_DIR"; do
    mkdir -pv "$dir"
done


CARGO_OPTS=""
# We could use Cargo's --profile when that's stable
if [ "$PROFILE" = "release" ]; then
    CARGO_OPTS+=" --release"
fi

ARTIFACTS=(firecracker jailer seccompiler-bin rebase-snap cpu-template-helper snapshot-editor)

if [ "$LIBC" == "gnu" ]; then
    # Don't build jailer. See commit 3bf285c8f
    echo "Not building jailer because glibc selected instead of musl"
    CARGO_OPTS+=" --exclude jailer"
    ARTIFACTS=(firecracker seccompiler-bin rebase-snap cpu-template-helper snapshot-editor)
fi

# # # # Profile-guided optimization
#
# Release binaries are built in three steps: an instrumented Firecracker
# (--pgo-instrument), the training workload in
# tests/integration_tests/build/test_pgo_training.py, which boots, snapshots
# and restores microVMs with it and collects the profiles, and the final build
# (--pgo-use), which feeds the merged profile to rustc. The profile moves
# never-executed code out of the pages Firecracker touches at runtime, which
# shrinks the resident part of the binary.
PGO_DIR=build/pgo
PGO_INSTRUMENTED_DIR=$PGO_DIR/instrumented
PGO_PROFILE=$PGO_DIR/firecracker.profdata

if [ -n "$PGO" ] && [ "$PROFILE" != "release" ]; then
    die "--pgo-instrument/--pgo-use require --profile release"
fi

if [ "$PGO" = "instrument" ]; then
    say "Building PGO-instrumented Firecracker..."
    rm -rf "$PGO_DIR"
    mkdir -p "$PGO_INSTRUMENTED_DIR"
    # Same package selection as the final build below: cargo folds it into
    # the crate hash that mangled symbol names carry, and the profile is keyed
    # by those names. Building `-p firecracker` alone would produce a profile
    # that matches nothing in a `--workspace` build.
    # The profile is written to /pgo inside the jail when the process exits.
    CARGO_TARGET_DIR=$PGO_DIR/target RUSTFLAGS="-Cprofile-generate=/pgo" \
        cargo build --target "$CARGO_TARGET" --release --workspace --bins --examples
    # The training workload needs the jailer and compiles its own seccomp filter.
    for file in firecracker jailer seccompiler-bin; do
        cp "$PGO_DIR/target/$CARGO_TARGET/release/$file" "$PGO_INSTRUMENTED_DIR/"
    done
    say "Instrumented binaries placed under $PGO_INSTRUMENTED_DIR"
    exit 0
fi

if [ "$PGO" = "use" ]; then
    profraws=("$PGO_PROFRAW_DIR"/*.profraw)
    if [ ! -e "${profraws[0]}" ]; then
        die "No PGO profiles under $PGO_PROFRAW_DIR. Run the training workload first."
    fi
    host_triple=$(rustc -vV | awk '/^host:/ { print $2 }')
    llvm_profdata="$(rustc --print sysroot)/lib/rustlib/$host_triple/bin/llvm-profdata"
    mkdir -p "$PGO_DIR"
    say "Merging ${#profraws[@]} PGO profiles into $PGO_PROFILE..."
    "$llvm_profdata" merge -o "$PGO_PROFILE" "${profraws[@]}"
    # cold-count=0 sends only never-executed functions to .text.unlikely;
    # the default threshold also demotes rarely-run code that every microVM
    # still touches once.
    export RUSTFLAGS="-Cprofile-use=$PWD/$PGO_PROFILE -Cllvm-args=-profile-summary-cold-count=0"
fi

say "Building version=$VERSION, profile=$PROFILE, target=$CARGO_TARGET, Rust toolchain=${RUST_TOOLCHAIN}, PGO=${PGO:-off}..."
# shellcheck disable=SC2086
cargo build --target "$CARGO_TARGET" $CARGO_OPTS --workspace --bins --examples

# Only strip in release mode
if [ "$PROFILE" = "release" ]; then
    for file in "${ARTIFACTS[@]}"; do
        strip-and-split-debuginfo "$CARGO_TARGET_DIR/$file"
    done
fi

say "Binaries placed under $CARGO_TARGET_DIR"

# Check static linking:
# expected "statically linked" for aarch64 and
# "static-pie linked" for x86_64
binary_format=$(file $CARGO_TARGET_DIR/firecracker)
if [[ "$PROFILE" = "release"
        && "$binary_format" != *"statically linked"*
        && "$binary_format" != *"static-pie linked"* ]]; then
    die "Binary not statically linked: $binary_format"
fi

# # # # Make a release
if [ -z "$MAKE_RELEASE" ]; then
    exit 0
fi

if [ "$LIBC" != "musl" ]; then
    die "Releases using a libc other than musl not supported"
fi

SUFFIX=$VERSION-$ARCH
RELEASE_DIR=release-$SUFFIX
mkdir "$RELEASE_DIR"
for file in "${ARTIFACTS[@]}"; do
    check_bin_artifact "$CARGO_TARGET_DIR/$file" "$VERSION"
    cp -v "$CARGO_TARGET_DIR/$file" "$RELEASE_DIR/$file-$SUFFIX"
    cp -v "$CARGO_TARGET_DIR/$file.debug" "$RELEASE_DIR/$file-$SUFFIX.debug"
done
cp -v "resources/seccomp/$CARGO_TARGET.json" "$RELEASE_DIR/seccomp-filter-$SUFFIX.json"
# Copy over arch independent assets
cp -v -t "$RELEASE_DIR" LICENSE NOTICE THIRD-PARTY
check_swagger_artifact src/firecracker/swagger/firecracker.yaml "$VERSION"
cp -v src/firecracker/swagger/firecracker.yaml "$RELEASE_DIR/firecracker_spec-$VERSION.yaml"

(
    cd "$RELEASE_DIR"
    find . -type f -not -name "SHA256SUMS" |sort |xargs sha256sum >SHA256SUMS
)
