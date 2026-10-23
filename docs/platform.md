# Platform targets

## Must support

| Target | Arch | Notes |
|---|---|---|
| **Arch Linux on Asahi** (Apple Silicon MacBook) | aarch64 | Primary dev machine |
| **CachyOS** | x86_64 (v3/v4-optimized repos) | Desktop |
| **Hyprland** | — | The only compositor we target |

Both are Arch-based, but nothing in the bar depends on Arch. It uses Wayland protocols, Hyprland IPC and freedesktop D-Bus APIs, which are the same on every distro. Being "Arch only" would only affect packaging, so we don't restrict anything in the code.

## Why these constraints shape the code

### Two architectures
Everything must build from source on both aarch64 and x86_64. In practice:
- No dependencies that only ship prebuilt x86 binaries.
- When adding an AUR or system dependency, check that its `arch=()` includes `aarch64`.
- No inline assembly or arch-specific intrinsics without a portable fallback.
- Test any change to renderer or GPU code on both machines. The GPU stacks are very different: Asahi's Mesa drivers versus Intel/AMD/NVIDIA.

### Asahi specifics
- **GPU:** Asahi's Mesa drivers provide OpenGL and Vulkan, so wgpu works. The GPU renderer is the default, with the CPU one as a fallback and as `renderer = "cpu"` (see [decisions.md](decisions.md), D-67).
- **HiDPI / fractional scaling:** the MacBook panel runs at a non-1 scale, so the bar must render crisp text at fractional scales. If text ever looks blurry, check whether the fractional-scale and viewporter protocols are being used.
- **The notch:** by default Asahi hides the notch area, so the usable screen is just shorter. If it's enabled with the `appledrm.show_notch=1` kernel parameter (`apple_dcp.show_notch=1` on older kernels), the top-center of the display is physically missing. The bar finds it and splits its center zone around it ([D-69](decisions.md)). `eave calibrate` measures it, and the screen's rounded corners, pixel by pixel ([D-71](decisions.md)).
- **Rebooting into macOS** is picked in Apple's NVRAM, not by a boot loader: `asahi-bless --next --set-boot-macos`, run as root by `eave-boot-macos.service` for the power plugin. GRUB, which Asahi boots Linux with, knows nothing of macOS ([D-131](decisions.md)).
- **Battery and backlight** are read from `/sys/class/power_supply` and `/sys/class/backlight`. No Apple-specific code. `macsmc-battery` reports `charge_*` and `energy_*`, and its `capacity` is the firmware's rounded-up display value, so the percentage comes from the energy ratio instead ([D-23](decisions.md)).

- **Wi-Fi** is Broadcom (brcmfmac), through iwd. Asahi needs iwd: wpa_supplicant doesn't do the WPA3 this firmware offloads. Two things to know:
  - **Run iwd alone.** NetworkManager on top of iwd (`wifi.backend=iwd`) fights the bar's direct iwd calls: it marks the bar's disconnects and power-offs as failures, and after a few stops joining on its own until it restarts. With iwd alone, it does DHCP and DNS itself: `/etc/iwd/main.conf` with `[General] EnableNetworkConfiguration=true` and `[Network] NameResolvingService=systemd` (systemd-resolved). This MacBook runs that way since 2026-10-23.
  - **The firmware can wedge**, failing every join (`connect-failed, status: 1` or `16`) with scans failing (`CMD_TRIGGER_SCAN: Invalid exchange (52)`, `brcmf_run_escan: error (-52)`), until it's restarted. In that order: **Restart Wi-Fi** in the network popover or the dormer (the device off and on, [D-132](decisions.md)); `systemctl restart iwd`; reloading the driver (`modprobe -r brcmfmac_wcc brcmfmac; modprobe brcmfmac`). Only then reboot. On WPA2-only networks, `options brcmfmac feature_disable=0x82000` turns off the firmware's handshake offload, which iwd's docs suggest when joins that used to work fail.
- **Bluetooth** shares the Broadcom chip. Wi-Fi on 2.4 GHz and Bluetooth audio share the radio; 5 GHz Wi-Fi leaves Bluetooth more room.
- **The headphone jack** is its own ALSA device (`HiFi__Headphones__sink`, and the headset mic `HiFi__Headset__source`), with jack detection: its port turns available when something is plugged in, which the volume widget follows ([D-133](decisions.md)). Speakers and the built-in mic are virtual (asahi-audio's convolver and mic chain), not followed.

### CachyOS specifics
- Usually a desktop with one or more external monitors, so multi-monitor support (one bar per output) matters more there.
- Repos are built for x86-64-v3/v4. That doesn't affect us, since we build from source with the system toolchain.

## Runtime requirements

The bar must run on any Hyprland machine without checking what else is installed ([D-23](decisions.md)). What it needs at runtime:

- **Hyprland** (its IPC sockets).
- **A font.** Text is shaped with whatever fonts the system has (fontdb reads fontconfig's directories).
- **For volume only:** a PulseAudio-protocol server, meaning `pipewire-pulse` or PulseAudio. If none is running, the widget hides ([D-19](decisions.md)).

- **For setting brightness only:** a systemd-logind session, which lets the user's own session set its backlights without root. Without it the level still shows, but can't be changed ([D-80](decisions.md)).

- **For power only:** systemd-logind, to lock, suspend, reboot and shut down. Locking needs something that answers logind's Lock, like hypridle with `lock_cmd`. Rebooting into macOS, a GRUB entry or a firmware entry needs eave's boot helper units and polkit rule, installed once as root ([D-131](decisions.md)).

That's all. Battery, brightness and network read the kernel directly. The binary links only libc, and no plugin runs other programs. `eave-build` needs `cargo` and `git`, and uses `systemctl` only when systemd is there.

## Hyprland interface we rely on

- **`wlr-layer-shell`** for the bar surface itself (anchor, layer, exclusive zone).
- **Hyprland IPC sockets** in `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/`:
  - `.socket.sock`: request/response (same as `hyprctl -j ...`)
  - `.socket2.sock`: newline-separated event stream, `EVENT>>DATA`

- **`eave-build`** relaunches a bar started from the Hyprland config by writing to `.socket.sock` itself (no `hyprctl`). Hyprland 0.56 moved to Lua configs, where `dispatch <args>` is evaluated as Lua (`hl.dispatch(<args>)`), so it sends `eval hl.exec_cmd(…)` first and falls back to `dispatch exec` on older releases ([D-22](decisions.md)).

- **A held button keeps the pointer only while something has the keyboard.** Hyprland sends a surface a held button's motion past its edges only if some surface has keyboard focus. On an empty workspace nothing does, and a drag off the bar would get a leave. So while no window is focused, the bar takes the keyboard on demand. Becoming exclusive during a press doesn't work: Hyprland lets go of every held button when a layer takes the keyboard that way ([D-117](decisions.md)).

Hyprland's IPC changes between releases more often than freedesktop APIs do. Keep parsing tolerant: ignore unknown events, and don't crash on extra fields.
