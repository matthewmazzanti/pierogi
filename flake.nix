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
      devShells = forAll (system:
        let pkgs = pkgsFor system;
        in {
          default = pkgs.mkShell {
            # Toolchain. nixpkgs rustc/cargo are new enough for libcontainer
            # (needs >= 1.85); no rust-overlay input required.
            nativeBuildInputs = with pkgs; [
              rustc
              cargo
              clippy
              rustfmt
              rust-analyzer
              pkg-config
            ];

            # libseccomp is a compile-time dep of youki's libcontainer (default
            # features). nix is needed at runtime for closure calculation.
            buildInputs = with pkgs; [
              libseccomp
              nix
            ];

            # Belt-and-suspenders so the `libseccomp` crate's build script finds
            # the system library even if pkg-config discovery misbehaves.
            LIBSECCOMP_LIB_PATH = "${pkgs.libseccomp.lib}/lib";
            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
          };
        });
    };
}
