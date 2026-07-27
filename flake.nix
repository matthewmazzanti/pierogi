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
            # the xdg-dbus-proxy binary pierogi supervises for the D-Bus filter
            # layer (src/dbus.rs), baked in the same way as crun.
            XDG_DBUS_PROXY_BIN = "${pkgs.xdg-dbus-proxy}/bin/xdg-dbus-proxy";
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

          # --- axis probes: minimal, single-use testers, each isolating one
          # config axis with the smallest test tool that gives a clean pass/fail
          # (see nix/probe/*.nix headers). ---

          # GPU-userspace trilogy: the same exposed render node (/sys + seeded
          # Mesa, NO Wayland), each resolving the driver a different way.
          #   GL/EGL via glvnd vendor JSON — `nix run .#egl-probe`.
          egl-probe = import ./nix/probe/egl.nix { inherit pkgs pierogi; };
          #   Vulkan via the ICD loader manifest — `nix run .#vulkan-probe`.
          vulkan-probe = import ./nix/probe/vulkan.nix { inherit pkgs pierogi; };
          #   VA-API (hardware video) via a libva driver .so — `nix run .#vainfo-probe`.
          vainfo-probe = import ./nix/probe/vainfo.nix { inherit pkgs pierogi; };

          # fontconfig: font resolution isolated (no display/GPU) —
          # `fc-match monospace`. `nix run .#fontconfig-probe`.
          fontconfig-probe = import ./nix/probe/fontconfig.nix { inherit pkgs pierogi; };

          # Local AF_UNIX socket routers (the wayland.nix pattern, other services):
          #   D-Bus session bus → dbus-send ListNames. `nix run .#dbus-probe`.
          dbus-probe = import ./nix/probe/dbus.nix { inherit pkgs pierogi; };
          #   PipeWire → pw-dump the media graph. `nix run .#pipewire-probe`.
          pipewire-probe = import ./nix/probe/pipewire.nix { inherit pkgs pierogi; };

          # Wayland-protocol probes over the socket route (run in a Wayland session):
          #   clipboard / data-device → wl-paste. `nix run .#clipboard-probe`.
          clipboard-probe = import ./nix/probe/clipboard.nix { inherit pkgs pierogi; };
          #   wlr-screencopy → grim a PNG (wlroots only). `nix run .#screenshot-probe`.
          screenshot-probe = import ./nix/probe/screenshot.nix { inherit pkgs pierogi; };
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
              # the xdg-dbus-proxy binary pierogi supervises for the D-Bus layer
              xdg-dbus-proxy
            ];

            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
            # the binaries pierogi bakes in via env! (build.rs re-exports them)
            CRUN_BIN = "${pkgs.crun}/bin/crun";
            XDG_DBUS_PROXY_BIN = "${pkgs.xdg-dbus-proxy}/bin/xdg-dbus-proxy";
          };
        });
    };
}
