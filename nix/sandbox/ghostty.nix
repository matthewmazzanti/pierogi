# M1 nix-mode artifact — a ghostty terminal sandbox (sealed-rootfs edition).
#
# The GPU follow-on to `foot.nix`. It reuses foot's Wayland socket
# routing verbatim (baked `/run/user/1000/wayland-0` mountpoint + spec env + a
# runtime host bind in the wrapper) and its font / fontconfig userland, but runs
# **ghostty** — a GTK4 + libadwaita terminal that renders its grid with
# **OpenGL**. Where foot is CPU/`wl_shm` (needs no GPU), ghostty draws through a
# `GdkGLContext` → libepoxy → libglvnd → EGL, so this sandbox adds the pieces a
# hardware-accelerated Wayland client needs:
#
#   * the **render node** `/dev/dri/renderD128` — bound in as a device (a Wayland
#     GL client makes its EGL context + dmabuf buffers on the *render* node; the
#     compositor still owns KMS/display via `card*`, which we deliberately do NOT
#     expose). It's a **producer placement** (spec `mounts`), not a runtime
#     override: the render-node path is stable across sessions (unlike the
#     Wayland socket, whose display number varies and so is discovered at
#     launch), and a placement passes through `oci.rs` verbatim — letting us
#     write the bind options **without `nodev`**, which a device node requires
#     and which the `--mount host:` override hardcodes on. The node is mode 0666
#     on the host, so container-root (the 1:1-mapped invoker) can open it with no
#     capability or group juggling.
#   * a read-only **`/sys`** — Mesa's loader resolves the render node to its
#     driver via libdrm, which reads the node's PCI info under `/sys`. Without
#     it, GL silently falls back to llvmpipe (software). `profiles.base` already
#     masks the sensitive `/sys` bits, which activate once /sys is mounted.
#   * **Mesa** (`radeonsi` for AMD) — the GL/EGL userspace driver. libglvnd
#     `dlopen`s the vendor driver at runtime via the EGL vendor JSON; Mesa is
#     therefore NOT in ghostty's build closure (ghostty carries libglvnd, not the
#     vendor). So it's an `extraSeed` (see `lib.resolveSpec`), pointed at with
#     `__EGL_VENDOR_LIBRARY_DIRS` (→ `50_mesa.json`, which names libEGL_mesa by
#     absolute store path) + `LIBGL_DRIVERS_PATH`. Mesa's closure already carries
#     `mesa-libgbm`, so gbm rides along.
#
# The GTK userland delta over foot:
#   * ghostty ships its **own terminfo** (`xterm-ghostty`, the `TERM` it sets for
#     its child shell) — ncurses' DB lacks it — so `/etc/terminfo` points at
#     ghostty's DB, with ncurses kept as `TERMINFO_DIRS` fallback;
#   * GTK needs compiled **GSettings schemas**: a combined `gtk4` +
#     `gsettings-desktop-schemas` DB is compiled and pointed at with
#     `GSETTINGS_SCHEMA_DIR`. To avoid a D-Bus/dconf dependency we run the GTK
#     chrome on the **GL** GSK renderer (not Vulkan), the **memory** GSettings
#     backend (no dconf), and with the a11y bridge off (no session bus);
#   * a **`/dev/shm`** tmpfs — GPU/GTK stacks commonly `shm_open`;
#   * a **`/dev/pts`** devpts — ghostty allocates its own pty for the child shell
#     (same as foot; the engine auto-mounts devpts only for `terminal`).
{ pkgs, pierogi }:

let
  # The login shell ghostty spawns, surfaced at /bin/bash.
  loginShell = "/bin/bash";

  # Hand-rolled /etc identity — a real login user, so ghostty's `getpwuid` /
  # `$SHELL` resolution lands on a valid shell.
  etcPasswd = pkgs.writeText "passwd" ''
    root:x:0:0:root:/root:${loginShell}
    pierogi:x:1000:100:pierogi:/home/pierogi:${loginShell}
  '';
  etcGroup = pkgs.writeText "group" ''
    root:x:0:
    users:x:100:
  '';
  etcHostname = pkgs.writeText "hostname" ''
    pierogi
  '';
  etcBashrc = pkgs.writeText "bashrc" ''
    # pierogi ghostty sandbox — minimal interactive rc.
    # Colored prompt: cyan label, blue cwd. `\[..\]` wraps non-printing bytes.
    PS1='\[\e[1;36m\][ghostty]\[\e[0m\] \[\e[1;34m\]\w\[\e[0m\] \$ '
    alias ls='ls --color=auto'
    alias grep='grep --color=auto'
    alias ll='ls -la'
    alias l='ls -CF'
  '';

  # A monospace font + a self-sufficient fontconfig config (reused from foot).
  # The config carries the `monospace` alias itself and a writable
  # `/tmp/fontconfig` cachedir; Nix scans its contents for store refs, so it
  # pulls `dejavu_fonts` into the closure automatically.
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

  # Compiled GSettings schemas — GTK4/libadwaita read schema definitions at
  # startup (even under the memory backend), and `g_settings_new` on a missing
  # schema aborts. Combine gtk4's + gsettings-desktop-schemas' XML and compile a
  # single `gschemas.compiled`. Fail-loud: a glob that matches nothing makes `cp`
  # error and the build fail (both packages ship these; see the flake's pinned
  # nixpkgs). libadwaita ships no schemas of its own, so it isn't listed.
  gschemas = pkgs.runCommand "pierogi-ghostty-schemas" { } ''
    mkdir -p $out/glib-2.0/schemas
    cp ${pkgs.gtk4}/share/gsettings-schemas/gtk4-*/glib-2.0/schemas/*.gschema.xml $out/glib-2.0/schemas/
    cp ${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/gsettings-desktop-schemas-*/glib-2.0/schemas/*.gschema.xml $out/glib-2.0/schemas/
    ${pkgs.glib.dev}/bin/glib-compile-schemas $out/glib-2.0/schemas
  '';

  # The minimal-but-usable terminal userland (shell, coreutils, pager, text
  # tools, tree, ncurses+terminfo) — shared with the bash/foot sandboxes.
  terminalTools = import ../terminal-tools.nix { inherit pkgs; };

  # Light-touch resolution utilities (build shell + resolveSpec).
  lib = import ../lib.nix { inherit pkgs pierogi; };

  # The AMD render node, bound in as a **device**. A producer placement (stable
  # path → sealed, unlike the session-varying Wayland socket). Options omit
  # `nodev` deliberately — a device node bound with MS_NODEV is inert. It lands on
  # the runtime `/dev` tmpfs (writable, `nodev`-free), so crun creates the
  # `/dev/dri/renderD128` target at mount time (no baked dir needed) and the node
  # is honored. Ordered after the `/dev` tmpfs placement so its parent exists.
  renderNode = {
    destination = "/dev/dri/renderD128";
    source = "/dev/dri/renderD128";
    type = "bind";
    options = [ "bind" "rw" "nosuid" ];
  };

  # A /dev/shm tmpfs — GPU/GTK stacks commonly `shm_open`. Mounts onto the /dev
  # tmpfs at runtime, so it needs no baked dir.
  devShm = {
    destination = "/dev/shm";
    type = "tmpfs";
    source = "tmpfs";
    options = [ "nosuid" "nodev" "mode=1777" ];
  };

  # A read-only sysfs. Mesa's loader resolves the render node to its driver
  # (radeonsi) via libdrm, which reads `/sys/dev/char/226:128 →
  # /sys/devices/.../drm/renderD128` for the PCI vendor/device ID. Without /sys,
  # `drmGetDevice` fails ("MESA-LOADER: failed to retrieve device information")
  # and GL silently falls back to llvmpipe (software). A fresh sysfs is a live
  # kernel view (only /sys/class/net is netns-scoped), so it shows the host GPU;
  # `profiles.base` already masks the sensitive bits (`/sys/firmware`,
  # `…/powercap`), which activate now that /sys exists. Needs a baked mountpoint.
  sysfs = {
    destination = "/sys";
    type = "sysfs";
    source = "sysfs";
    options = [ "nosuid" "noexec" "nodev" "ro" ];
  };

  # root: the tree the author designs — /bin farm, hand-rolled /etc, base
  # mountpoint dirs (incl. an empty /nix/store and the Wayland socket file). It IS
  # the `/` mount; `lib.resolveSpec` derives the closure from its actual
  # references, plus `mesa` as an `extraSeed` (the GL driver libglvnd discovers at
  # runtime, which root doesn't reference).
  root = pkgs.runCommand "pierogi-ghostty-root" { } ''
    source ${lib.buildLib}
    mkdir -p $out/bin $out/etc

    # /bin symlink farm — the shared terminal userland plus ghostty itself. See
    # `bin_farm` in build-lib.sh (fails loud on a command-name collision).
    bin_farm ${pkgs.lib.escapeShellArgs (terminalTools ++ [ pkgs.ghostty ])}

    # Hand-rolled /etc — store symlinks (closure members, resolve in-sandbox and
    # fold into root's references).
    ln -s ${etcPasswd}   $out/etc/passwd
    ln -s ${etcGroup}    $out/etc/group
    ln -s ${etcHostname} $out/etc/hostname
    ln -s ${etcBashrc}   $out/etc/bashrc
    ln -s ${fontsConf}   $out/etc/fonts.conf
    # terminfo: ghostty's DB (carries `xterm-ghostty`, the TERM it sets) as the
    # primary lookup; ncurses' DB as the fallback path (xterm-256color et al.).
    ln -s ${pkgs.ghostty}/share/terminfo $out/etc/terminfo
    ln -s ${pkgs.ncurses}/share/terminfo $out/etc/terminfo-base
    # Compiled GSettings schemas (gtk4 + desktop schemas).
    ln -s ${gschemas}/glib-2.0/schemas   $out/etc/gschemas

    # Baked mountpoints — the base dirs (proc/dev/tmp + /nix/store) + this
    # sandbox's own (/home/pierogi, /root, and the `/run/user/1000` Wayland
    # socket dir) + /sys (the sysfs mountpoint). The `wayland-0` **file** is the
    # socket bind target (a socket binds onto a file), so it's `touch`ed.
    # /dev/{dri,pts,shm} need no baked dirs — they mount onto the runtime /dev
    # tmpfs.
    bake_dirs ${pkgs.lib.escapeShellArgs lib.defaultMountDirs} /sys /home/pierogi /root /run/user/1000
    touch $out/run/user/1000/wayland-0
  '';

  # The default-closed floor, as spec fields (see nix/profiles.nix). GL on a
  # render node needs no capabilities and no network, so the base is untouched.
  profiles = import ../profiles.nix { };

  # The sources spec: `profiles.base` merged with this sandbox's fields.
  # `root.path` + `closure` are injected by `lib.resolveSpec`.
  unresolvedSpec = pkgs.lib.recursiveUpdate profiles.base {
    process = {
      executable = "ghostty";
      args = [ ];
      # ghostty opens its own Wayland window and forks a pty for the child shell —
      # it doesn't use pierogi's stdio as a tty, so no relay.
      terminal = false;
      # ghostty (and the shell it spawns) starts in /root — the persistent host
      # dir the wrapper binds there; HOME too, so state survives the container.
      cwd = "/root";
      env = {
        PATH = "/bin";
        HOME = "/root";
        # ghostty spawns this in its window.
        SHELL = "/bin/bash";
        # ghostty sets TERM=xterm-ghostty for the child; /etc/terminfo carries it,
        # with ncurses' DB as the fallback search path.
        TERMINFO = "/etc/terminfo";
        TERMINFO_DIRS = "/etc/terminfo-base";
        # A UTF-8 locale (glibc's C.UTF-8 needs no archive).
        LANG = "C.UTF-8";
        # fontconfig: the self-sufficient config + a writable cache under /tmp.
        FONTCONFIG_FILE = "/etc/fonts.conf";
        XDG_CACHE_HOME = "/tmp";
        # ghostty config/state live under the persistent /root bind.
        XDG_CONFIG_HOME = "/root/.config";
        XDG_DATA_HOME = "/root/.local/share";
        # Wayland socket routing — the baked mountpoint path (wrapper binds the
        # host's real socket onto it).
        XDG_RUNTIME_DIR = "/run/user/1000";
        WAYLAND_DISPLAY = "wayland-0";
        # GTK: force the Wayland backend and the GL GSK renderer (keep the chrome
        # off Vulkan). Avoid a session bus: compiled schemas via a dir, the
        # in-memory GSettings backend (no dconf), and the a11y bridge off.
        GDK_BACKEND = "wayland";
        GSK_RENDERER = "gl";
        GSETTINGS_SCHEMA_DIR = "/etc/gschemas";
        GSETTINGS_BACKEND = "memory";
        GTK_A11Y = "none";
        # Mesa GL/EGL userspace driver (radeonsi). libglvnd discovers the vendor
        # via this EGL vendor dir (`50_mesa.json` names libEGL_mesa by absolute
        # store path); mesa is an `extraSeed` (not in ghostty's build closure).
        __EGL_VENDOR_LIBRARY_DIRS = "${pkgs.mesa}/share/glvnd/egl_vendor.d";
        LIBGL_DRIVERS_PATH = "${pkgs.mesa}/lib/dri";
      };
    };
    # Placements — the common base trio (proc/dev/tmp; `/tmp` also the fontconfig
    # cache), a read-only **/sys** (so libdrm can resolve the GPU driver), then
    # the extras that mount onto the /dev tmpfs: a **devpts** (own pty for the
    # child shell), the **render node** device, and **/dev/shm**. The /dev-nested
    # three follow `/dev` (defaultMounts index 1), so their parent is in place.
    mounts = lib.defaultMounts ++ [ sysfs lib.mounts.devpts renderNode devShm ];
    # `root.path` + `closure` are injected by `lib.resolveSpec` (below).
  };

  # Resolve against `root` (+ `mesa` as an extraSeed): `resolveSpec` injects
  # `root.path`, seeds the closure from `[ root, mesa ]`, and stamps
  # `closure.resolved`. mesa is seeded explicitly because libglvnd loads it at
  # runtime, so root/ghostty don't reference it. The store topology is a runtime
  # RO bind, not baked. See bash.nix.
  resolved = lib.resolveSpec {
    name = "pierogi-ghostty-sandbox";
    spec = unresolvedSpec;
    root = root;
    extraSeeds = [ pkgs.mesa ];
  };

in
{
  inherit resolved;

  # The runnable artifact — same socket-discovery wrapper as `foot-sandbox`, plus
  # a render-node pre-check: find the host's Wayland socket, bind it onto the
  # baked mountpoint, bind persistent state to /root, exec `pierogi run ghostty`.
  # The render node itself is a sealed placement (stable path), so the wrapper
  # only verifies it exists for a clear error. Trailing `"$@"` is the args-only
  # override, forwarded to ghostty (e.g. `nix run .#ghostty-sandbox -- +version`).
  runnable = pkgs.writeShellScriptBin "pierogi-ghostty-sandbox" ''
    set -eu

    : "''${XDG_RUNTIME_DIR:?not set — is a Wayland session running?}"
    display="''${WAYLAND_DISPLAY:-wayland-0}"
    case "$display" in
      /*) sock="$display" ;;
      *)  sock="$XDG_RUNTIME_DIR/$display" ;;
    esac
    if [ ! -S "$sock" ]; then
      echo "pierogi-ghostty-sandbox: no Wayland socket at $sock" >&2
      exit 1
    fi

    # The render node is bound in by the sealed spec (a stable-path placement).
    # Fail loud here with a clearer message than the bind failing deeper in crun.
    if [ ! -e /dev/dri/renderD128 ]; then
      echo "pierogi-ghostty-sandbox: no render node at /dev/dri/renderD128 — is this an AMD/Intel (Mesa) GPU host?" >&2
      exit 1
    fi

    # Persistent state: a local (gitignored) dir bound to /root — ghostty's cwd +
    # HOME (config under .config, data under .local/share). Override with
    # $PIEROGI_STATE.
    state="''${PIEROGI_STATE:-$PWD/state/ghostty-sandbox}"
    mkdir -p "$state"

    exec ${pierogi}/bin/pierogi run ${resolved} \
      --mount "host:$sock:/run/user/1000/wayland-0:rw" \
      --mount "host:$state:/root:rw" \
      -- "$@"
  '';
}
