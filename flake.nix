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
        postInstall = (old.postInstall or "") + ''
          echo "[pierogi] harvesting crun dev headers + static libs"
          mkdir -p $out/include/crun $out/include/ocispec

          # crun's own headers + the generated config.h / git-version.h
          cp src/libcrun/*.h $out/include/crun/ || true
          [ -f config.h ]      && cp config.h      $out/include/crun/ || true
          [ -f git-version.h ] && cp git-version.h $out/include/crun/ || true

          # generated libocispec schema headers (wherever codegen put them)
          schema=$(find . -name 'runtime_spec_schema_config_schema.h' | head -1)
          if [ -n "$schema" ]; then
            cp "$(dirname "$schema")"/*.h $out/include/ocispec/ || true
          fi

          # libcrun.a is already installed; add the ocispec static lib it needs
          find . -name 'libocispec.a' -exec cp {} $out/lib/ \; || true

          echo "[pierogi] installed headers:" ; ls $out/include/crun $out/include/ocispec || true
          echo "[pierogi] installed libs:"    ; ls $out/lib || true
        '';
      });
    in
    {
      packages = forAll (system: {
        crun-dev = crunDevFor (pkgsFor system);
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
            ];

            buildInputs = with pkgs; [
              # youki path
              libseccomp
              nix
              # crun/libcrun link chain (libcrun.a + its transitive deps)
              crunDev
              libcap
              yajl
              criu
              systemdMinimal
            ];

            LIBSECCOMP_LIB_PATH = "${pkgs.libseccomp.lib}/lib";
            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
            # so the smoke test / build.rs can find the harvested dev tree
            CRUN_DEV = "${crunDev}";
          };
        });
    };
}
