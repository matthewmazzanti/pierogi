{
  description = "pierogi — Nix-focused namespace runtime (M0 shell-run spike)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: import nixpkgs { inherit system; };
    in
    {
      packages = forAll (system:
        let
          pkgs = pkgsFor system;
          # pierogi drives the `crun` *binary* as a subprocess — it links nothing
          # from crun, so the build needs no C toolchain, headers, or libcrun.so.
          # The one input is `$CRUN_BIN`, the absolute path to crun, which the
          # runtime bakes in via `env!` (and which Nix's reference scan then turns
          # into a runtime dependency). The nix-mode builder only calls `resolve`
          # (no backend), so it never execs crun — but pinning the path here keeps
          # the runnable sandboxes' pierogi identical to the one we build.
          pierogi = pkgs.rustPlatform.buildRustPackage {
            pname = "pierogi";
            version = "0.0.0";
            # Only the actual build inputs, so edits to nix/ , docs/, README,
            # todo.md don't invalidate the pierogi build (and the
            # sandboxes that depend on its binary): the crate + lockfile + the
            # trivial build.rs (which just re-exports $CRUN_BIN).
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./build.rs
                ./src
              ];
            };
            cargoLock.lockFile = ./Cargo.lock;
            CRUN_BIN = "${pkgs.crun}/bin/crun";
            # tests hit nix-store / namespaces — not build-sandbox friendly.
            doCheck = false;
          };
        in {
          inherit pierogi;
          # Each sandbox file evaluates directly to its runnable wrapper — a
          # `writeShellScriptBin` that execs `pierogi run` against the pre-baked
          # spec (closure stamped in at build), forwarding args. The built spec is
          # the only runnable form (no live-query path); the wrapper just pairs it
          # with the pierogi binary that runs it.

          # M1 tire-kicker: a static bash + coreutils sandbox with a sealed rootfs
          # (baked /etc + mountpoints). `nix run .#bash-sandbox` — or
          # `… -- -c 'echo hi'` to pass args to bash.
          bash-sandbox = import ./nix/sandbox/bash.nix { inherit pkgs pierogi; };
          # M1 tire-kicker: a wayland-info sandbox that routes the host Wayland
          # socket in (baked socket mountpoint + a runtime host bind) — a smoke
          # test for socket routing. Run inside a Wayland session.
          wayland-sandbox = import ./nix/sandbox/wayland.nix { inherit pkgs pierogi; };
          # M1 tire-kicker: foot — a graphical Wayland terminal over the same
          # socket routing, plus a baked font / fontconfig / terminfo userland.
          # Run inside a Wayland session: `nix run .#foot-sandbox` (opens a window).
          foot-sandbox = import ./nix/sandbox/foot.nix { inherit pkgs pierogi; };
          # M1 tire-kicker: ghostty — a GPU (OpenGL) Wayland terminal. Adds the
          # AMD render node (/dev/dri/renderD128) + a seeded Mesa driver on top of
          # foot's socket/font userland. Run inside a Wayland session:
          # `nix run .#ghostty-sandbox` (opens a window).
          ghostty-sandbox = import ./nix/sandbox/ghostty.nix { inherit pkgs pierogi; };
        });

      devShells = forAll (system:
        let
          pkgs = pkgsFor system;
        in {
          default = pkgs.mkShell {
            nativeBuildInputs = with pkgs; [
              rustc
              cargo
              clippy
              rustfmt
              rust-analyzer
            ];

            buildInputs = with pkgs; [
              # nix CLI for building/inspecting sandboxes (nix build .#bash-sandbox)
              # — not the runtime, which no longer queries the store.
              nix
              # the crun binary pierogi execs (also on PATH for manual runs)
              crun
            ];

            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
            # the crun binary pierogi bakes in via env! (build.rs re-exports it)
            CRUN_BIN = "${pkgs.crun}/bin/crun";
          };
        });
    };
}
