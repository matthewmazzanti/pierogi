# M1 nix-mode artifact — a fontconfig probe (sealed-rootfs edition).
#
# The cheapest isolator on the shelf: no display, no GPU, no socket routing. It
# runs `fc-match monospace`, which resolves the `monospace` alias through the
# sealed `/etc/fonts.conf` and prints the font it lands on. That's the pass/fail
# for the fontconfig axis — the same font-resolution machinery `foot.nix` /
# `ghostty.nix` rely on, but stripped of everything else (no Wayland, no window),
# so a font miss shows up here as a font miss, not a mystery blank glyph in a
# terminal.
#
# What the axis needs is exactly two things: a **font** on disk and a
# **self-sufficient fontconfig config** that names it (no reliance on a
# `/etc/fonts/conf.d` the sealed rootfs doesn't ship) with a **writable
# cachedir** under the `/tmp` tmpfs. See `bash.nix` for the sealed-rootfs
# pipeline.
{ pkgs, pierogi }:

let
  etcHostname = pkgs.writeText "hostname" "pierogi\n";

  # A monospace font + a self-sufficient fontconfig config (same shape as
  # foot/ghostty). Nix scans this file's *contents* for store refs, so it pulls
  # `dejavu_fonts` into the closure automatically — seeding `root` from it is
  # enough, no separate dep entry.
  dejavuFonts = pkgs.dejavu_fonts;
  fontsConf = pkgs.writeText "fonts.conf" ''
    <?xml version="1.0"?>
    <!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
    <fontconfig>
      <dir>${dejavuFonts}/share/fonts</dir>
      <cachedir>/tmp/fontconfig</cachedir>
      <alias><family>monospace</family><prefer><family>DejaVu Sans Mono</family></prefer></alias>
      <alias><family>sans-serif</family><prefer><family>DejaVu Sans</family></prefer></alias>
      <alias><family>serif</family><prefer><family>DejaVu Serif</family></prefer></alias>
    </fontconfig>
  '';

  lib = import ../lib.nix { inherit pkgs pierogi; };

  # root: /bin farm (fontconfig → `fc-match` / `fc-list` / `fc-cache`),
  # hand-rolled /etc (hostname + fonts.conf → dejavu), base mountpoint dirs.
  root = pkgs.runCommand "pierogi-fontconfig-root" { } ''
    source ${lib.buildLib}
    mkdir -p $out/bin $out/etc

    bin_farm ${pkgs.lib.escapeShellArg pkgs.fontconfig}
    ln -s ${etcHostname} $out/etc/hostname
    ln -s ${fontsConf}   $out/etc/fonts.conf

    bake_dirs ${pkgs.lib.escapeShellArgs lib.defaultMountDirs}
  '';

  profiles = import ../profiles.nix { };

  unresolvedSpec = pkgs.lib.recursiveUpdate profiles.base {
    process = {
      executable = "fc-match";
      # Resolve the `monospace` alias — prints the matched font and exits.
      args = [ "monospace" ];
      terminal = false;
      env = {
        PATH = "/bin";
        # The self-sufficient config + a writable cache under the /tmp tmpfs.
        FONTCONFIG_FILE = "/etc/fonts.conf";
        XDG_CACHE_HOME = "/tmp";
      };
    };
    mounts = lib.defaultMounts;
  };

  resolved = lib.resolveSpec {
    name = "pierogi-fontconfig-probe";
    spec = unresolvedSpec;
    root = root;
  };

# The runnable artifact — no host resource to route, so the wrapper just execs
# `pierogi run fc-match`. `/bin` also carries `fc-list`, `fc-cache`, etc.; the
# trailing `"$@"` overrides args, so `nix run .#fontconfig-probe -- -a
# sans-serif` shows all matches for another family (the executable stays
# `fc-match`, the sealed one).
in
pkgs.writeShellScriptBin "pierogi-fontconfig-probe" ''
  set -eu

  exec ${pierogi}/bin/pierogi run ${resolved} -- "$@"
''
