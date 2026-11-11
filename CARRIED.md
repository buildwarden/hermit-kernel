# Carried by BuildWarden

This branch, `bw/v0.13.2`, is Hermit's kernel at tag `v0.13.2` plus the
commits BuildWarden carries for its relay
([buildwarden-relay](https://github.com/buildwarden/buildwarden-relay)).
`main` mirrors upstream and carries nothing.

Each carried commit is one of two classes:

- **Upstream-bound:** meant for `hermit-os/kernel`, and dropped once it
  lands there.
- **Deviation:** carried by design, whether or not upstream takes it.

On each upstream release, a new `bw/<tag>` is cut from the release tag and
these commits are replayed. Commits that landed upstream are dropped, and
any that conflicted are re-reviewed. Nothing is submitted upstream without
Jeff's approval of that submission, and AI assistance is disclosed when it
is. A rejected or stalled PR turns its commit into a deviation; it never
blocks the relay.

| Commit | Class | Upstream status | What |
|---|---|---|---|
| `58eaa91` aarch64, virtio: boot on Apple's Virtualization framework | Upstream-bound | Not submitted | Nine fixes found booting on VZ: no PMU, a root node without `compatible`, the GIC by `compatible`, no UART, early log buffering, the 8 KiB console write, waiting for the virtio reset, a short net device config, and VZ's net feature set |
| `0b0d1e3` entropy: seed from virtio-rng and fail closed without a source | Deviation | Not submitted | A virtio-rng driver, plus `sys_read_entropy` failing instead of falling back to a timer-seeded PRNG. The driver could be offered upstream; failing closed is what BuildWarden needs regardless |
| `f248077` vsock: fix guest-initiated connections and peer shutdown | Upstream-bound | Not submitted | Four bugs in vsock connection handling |
| `d4c0f8c` virtio-net: survive VZ's file handle attachment | Upstream-bound | Not submitted | 1530-byte receive buffers, and no `todo!()` on configuration interrupts |
| `2f5029d` net: packet sockets for a device kept from the network stack | Deviation | Not submitted | `HERMIT_RAW_MAC` and `AF_PACKET` sockets: the relay's build link |
| `62346b3` tcp: implement shutdown(), and let half-closed sockets read and write | Upstream-bound | Not submitted | TCP half-close |

Not carried: the boot-tracing patch used to find the VZ fixes
(`spike/patches/kernel-vz-tracing-wip.patch` in buildwarden-relay). It is a
debugging tool, not a change to ship.
