# Research: Slim LLVM package — file subset vs rebuild

## Metadata

**Date:** 2026-11-08
**Domain:** packaging
**Triggered by:** `.agents/discussions/cargo-native-build.md` — owner wants one slim LLVM prefix (~30–60 MB) instead of the ~1.6 GB official distribution
**Expires:** 2027-05-08

## Direct Answer

No measured evidence that a 30–60 MB clang+lld package can be cut from the official tarballs by file selection alone; every 30–60 MB data point found is a rebuilt or re-packaged single tool. Per-binary sizes inside the official tarballs were **not found** — measure first: extract `bin/clang`, `bin/lld`, `lib/clang/NN` from `LLVM-23.1.5-Linux-X64.tar.xz` and `du` them.

## Evidence

| Item | Size | Status | Source |
|---|---|---|---|
| `LLVM-23.1.5-Linux-X64.tar.xz` / Linux-ARM64 / macOS-ARM64 | 2.01 / 1.83 / 1.57 GB compressed | measured | GitHub release assets, llvmorg-23.1.5 |
| `clang+llvm-23.1.5-{x86_64,aarch64}-pc-windows-msvc.tar.xz` | 0.90 / 0.77 GB | measured | same |
| Release build config | projects clang, lld, lldb, clang-tools-extra, polly, mlir (+bolt Linux, +flang non-Darwin); runtimes compiler-rt, libcxx, openmp, …; `LLVM_TARGETS_TO_BUILD=Native`; thin LTO; no `LLVM_LINK_LLVM_DYLIB` → tools likely statically linked (inferred) | read | `clang/cmake/caches/Release.cmake` |
| `clang-format` wheel 23.1.5 | 1.5–3.3 MB | measured | pypi.org/pypi/clang-format/23.1.5/json |
| `clang-tidy` wheel 23.1.4 | 28.5–45.6 MB compressed | measured | pypi.org/pypi/clang-tidy/23.1.4/json |
| `ziglang` 0.17.1 wheel (clang+lld+libc sources) | ~97–107 MB compressed | measured | pypi.org/pypi/ziglang/0.17.1/json |
| Debian `libllvm21` .deb | 27 MB compressed (shared-libLLVM split is a distro build choice) | measured | packages.debian.org |

Build time (weak): X86-only clang with lld ~8 min vs ~10 default (cores unstated, cambus.net); dropping GPU targets 21→16 min (Yocto patch). No 4-core CI or ccache/sccache figures found.

## Gaps

Per-binary sizes in the official tarballs; resource-dir + builtins size; conda-forge / NDK / Chromium / wasi-sdk / llvm-mingw sizes; quantified effect of `LLVM_TARGETS_TO_BUILD=X86;AArch64`, `LLVM_DISTRIBUTION_COMPONENTS`, dylib builds.
