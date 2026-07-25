# profiles.base — the default-closed **floor**, as plain spec fields.
#
# This is where the sealed base actually lives. The engine
# (pierogi-the-binary) holds **no policy**: it projects whatever the resolved
# spec carries, `absent = absent` (see docs/control-surface.md,
# docs/architecture.md's third principle). So "default-closed" is not engine
# behaviour — it is this attrset, which a producer **composes** into a spec
# before the engine ever sees it. A sandbox is hardened because it merged
# `profiles.base`; omit it and the sandbox runs open (the runc contract),
# visibly.
#
# Every value here is a faithful OCI field the engine passes straight through.
# The curated lists (masks, caps posture) are reviewed once and shared, and get
# to be bespoke per domain — masking's list, the network rung — instead of one
# uniform engine floor. Compose with `pkgs.lib.recursiveUpdate base { … }`.
{ }:
{
  # Nested like the OCI runtime spec (root / process / linux), which is the
  # structure `pierogi`'s spec mirrors.
  base = {
    # A hostname is descriptive, but the base picks a neutral default.
    hostname = "pierogi";

    # Read-only root.
    root.readonly = true;

    process = {
      no_new_privileges = true;
      # Drop every capability ([] = the empty set, distinct from "absent"). Add
      # back per-profile if a workload genuinely needs one.
      capabilities = [ ];
      # No core dumps (the one always-available rootless rlimit floor).
      rlimits = [ { type = "RLIMIT_CORE"; soft = 0; hard = 0; } ];
    };

    linux = {
      # Private mount propagation.
      rootfs_propagation = "rprivate";

      # Hide the /proc + /sys bits that leak host info or allow DoS — runc's
      # reviewed list (the /sys entries lie dormant until /sys is mounted).
      masked_paths = [
        "/proc/kcore"
        "/proc/keys"
        "/proc/latency_stats"
        "/proc/timer_list"
        "/proc/timer_stats"
        "/proc/sched_debug"
        "/proc/acpi"
        "/proc/asound"
        "/proc/scsi"
        "/sys/firmware"
        "/sys/devices/virtual/powercap"
      ];
      readonly_paths = [
        "/proc/bus"
        "/proc/fs"
        "/proc/irq"
        "/proc/sys"
        "/proc/sysrq-trigger"
      ];

      # Private namespaces, network included (closes the shared-host-net hole).
      namespaces = [
        { type = "user"; }
        { type = "mount"; }
        { type = "pid"; }
        { type = "uts"; }
        { type = "ipc"; }
        { type = "network"; }
      ];

      # Rootless 1:1 map: container-0 → the invoking user. `"self"` is resolved to
      # getuid()/getgid() by the engine at launch (the host uid isn't known at
      # build time). Subuid ranges are an M4 override.
      uid_mappings = [ { container = 0; host = "self"; size = 1; } ];
      gid_mappings = [ { container = 0; host = "self"; size = 1; } ];
    };
  };
}
