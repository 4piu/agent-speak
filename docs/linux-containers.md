# Linux container audio

This is an edge-case deployment note for running Agent Speak inside an
unprivileged Linux container. Agent Speak needs ALSA device nodes and udev's
sound-card metadata. The following LXC setup has been tested:

```ini
lxc.cgroup2.devices.allow = c 116:* rwm
lxc.mount.entry = /dev/snd dev/snd none bind,optional,create=dir
lxc.mount.entry = tmpfs run tmpfs rw,nosuid,nodev,mode=0755,size=20%,nr_inodes=800k,create=dir
lxc.mount.entry = /run/udev/data run/udev/data none bind,ro,create=dir
```

Mount only `/run/udev/data`, not all of `/run/udev`; the latter can expose the
host udev control socket. The explicit `/run` entry must precede the udev-data
bind so the guest's runtime tmpfs does not hide it.

For an unprivileged container, grant the mapped host UID access whenever sound
nodes are created. If container UID `1000` maps to host UID `101000`, place this
late rule at `/etc/udev/rules.d/99-z-lxc-audio.rules` on the host:

```udev
ACTION=="add", SUBSYSTEM=="sound", ENV{DEVNAME}!="", RUN{program}+="/usr/bin/setfacl -m u:101000:rw $env{DEVNAME}"
```

The late filename matters: the standard `uaccess` rule can otherwise
recalculate and erase an earlier ACL. Reload and apply it with:

```sh
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=sound --action=add
sudo udevadm settle
```

Adjust `101000` for the container's actual UID map. Inside the container,
`aplay -l`, `agent-speak devices`, and `wpctl status` should then show the
expected card and sink.

Passing through an audio device weakens container isolation. Review the host's
device and UID mappings rather than copying these values unchanged.
