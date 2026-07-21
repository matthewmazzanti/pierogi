{
  description = "pierogi — Nix-focused namespace runtime (M0 shell-run spike)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: import nixpkgs { inherit system; };

      # crun is packaged in nixpkgs as an *application*: it ships bin/crun and a
      # build-byproduct lib/libcrun.a, but NO headers, NO pkgconfig, NO .so —
      # because nothing in nixpkgs links it; podman just execs the binary. To
      # link libcrun into our FFI shim we override the build to harvest the dev
      # bits from the build tree. Two of the needed headers (config.h and the
      # libocispec schema header) are *generated* during the build, so they only
      # exist here — they can't be vendored from upstream source. See docs.
      crunDevFor = pkgs: pkgs.crun.overrideAttrs (old: {
        # skip the (slow) upstream test suite for our dev build
        doCheck = false;
        doInstallCheck = false;

        # crun defaults to LT_INIT([disable-shared]) → static-only. --enable-shared
        # builds a real libcrun.so (exports curated via libcrun.lds: libcrun_*
        # public + the bundled libocispec runtime_spec_* symbols), letting
        # consumers link just `-lcrun` with deps resolved via DT_NEEDED + rpath.
        configureFlags = (old.configureFlags or [ ]) ++ [ "--enable-shared" ];

        # We link the shared libcrun.so, which carries its own deps (cap,
        # seccomp, criu, systemd) via DT_NEEDED + RUNPATH — so consumers neither
        # link nor search for those. The one thing still needed downstream is
        # yajl's *headers* at compile time: crun's container.h transitively does
        # `#include <yajl/yajl_tree.h>`. Propagate just yajl so consumers get
        # that include path from crunDev.
        propagatedBuildInputs = (old.propagatedBuildInputs or [ ]) ++ [ pkgs.yajl ];

        # crun's `make install` installs the lib (libcrun.so via --enable-shared)
        # but NO headers, under any flag. So we still harvest the headers the
        # shim needs to compile. (We no longer harvest libocispec.a — ocispec is
        # folded into libcrun.so now, so there's nothing to link separately.)
        postInstall = (old.postInstall or "") + ''
          echo "[pierogi] harvesting crun dev headers"
          mkdir -p $out/include/crun $out/include/ocispec

          # crun's source headers, plus config.h — which is generated at the
          # build root by AC_CONFIG_HEADERS, not in src/libcrun, and is pulled by
          # `#include <config.h>` in container.h.
          # (git-version.h is deliberately NOT harvested: it's #include'd only by
          # crun's .c files, already compiled into libcrun.so — no public header
          # we consume references it.)
          cp src/libcrun/*.h $out/include/crun/
          cp config.h $out/include/crun/

          # generated libocispec schema headers — still needed at compile time
          # (container.h includes <ocispec/...> even though the symbols live in
          # the .so). Drop the basic_test* fixtures: libocispec test schemas,
          # not real API.
          cp libocispec/src/ocispec/*.h $out/include/ocispec/
          rm -f $out/include/ocispec/basic_test*

          echo "[pierogi] installed headers:" ; ls $out/include/crun $out/include/ocispec
          echo "[pierogi] installed libs:"    ; ls $out/lib
        '';
      });
    in
    {
      packages = forAll (system:
        let
          pkgs = pkgsFor system;
          # The harvested libcrun dev tree (headers + libcrun.so), shared by the
          # devShell and the pierogi build below.
          crunDev = crunDevFor pkgs;
          # pierogi with the crun backend compiled in (now the default feature).
          # crun links libcrun via the shim, so this build needs the same bits as
          # the devShell: a C toolchain (from stdenv), $CRUN_DEV, and crunDev on
          # buildInputs (libcrun.so to link + its RUNPATH, yajl headers via the
          # cc-wrapper). The nix-mode builder only calls `resolve` (no backend),
          # but the binary still links libcrun — a heavier *build-time* closure,
          # with no effect on the emitted spec.json artifact.
          pierogi = pkgs.rustPlatform.buildRustPackage {
            pname = "pierogi";
            version = "0.0.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ pkgs.libseccomp crunDev ];
            LIBSECCOMP_LIB_PATH = "${pkgs.libseccomp.lib}/lib";
            CRUN_DEV = "${crunDev}";
            # tests hit nix-store / namespaces — not build-sandbox friendly.
            doCheck = false;
          };
          # M1 tire-kicker: a static bash + coreutils sandbox with a hand-rolled
          # /etc. Yields both the sealed resolved spec and the unresolved spec.
          bashSandbox = import ./nix/bash-sandbox.nix { inherit pkgs pierogi; };
        in {
          crun-dev = crunDev;
          inherit pierogi;
          # The self-contained sealed spec.json (closure stamped in, no query).
          bash-sandbox = bashSandbox.resolved;
          # The sources-only spec — `pierogi run` it to resolve the closure live
          # (nix-store query) for faster shell iteration.
          bash-sandbox-unresolved = bashSandbox.unresolved;
        });

      devShells = forAll (system:
        let
          pkgs = pkgsFor system;
          crunDev = crunDevFor pkgs;
        in {
          default = pkgs.mkShell {
            nativeBuildInputs = with pkgs; [
              rustc
              cargo
              clippy
              rustfmt
              rust-analyzer
              pkg-config
              # startup-time benchmarking (bench.sh)
              hyperfine
            ];

            buildInputs = with pkgs; [
              # youki path
              libseccomp
              nix
              # crun/libcrun: brings libcrun + its link chain via propagation
              crunDev
            ];

            LIBSECCOMP_LIB_PATH = "${pkgs.libseccomp.lib}/lib";
            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
            # so the smoke test / build.rs can find the harvested dev tree
            CRUN_DEV = "${crunDev}";
          };
        });
    };
}
