# Adjutant container security profiles

## Seccomp for Bubblewrap startup

`seccomp-adjutant.json` is Moby's default seccomp allowlist from revision
[`61eaf32614c7c71b60bd8927d3e6a4ffc8ff1f31`](https://github.com/moby/profiles/blob/61eaf32614c7c71b60bd8927d3e6a4ffc8ff1f31/seccomp/default.json),
with three appended rules. The upstream rules are unchanged:

- On x86-64, `clone` may create the user, mount, IPC, PID, and network namespaces
  together. Its namespace flag mask is `0x7e020000` and required value is
  `0x78020000`; UTS/cgroup namespace flags remain excluded by this additional rule.
- `unshare` may use exactly `CLONE_NEWUSER` (`0x10000000`), which Bubblewrap uses
  to lock down the sandbox after its mount setup.
- `mount`, `umount2`, and `pivot_root` may build the private filesystem view.
  These calls still require kernel capabilities in the namespace they affect.
  The outer container does not receive `CAP_SYS_ADMIN`.

The default-deny action, `clone3` ENOSYS fallback, and other Docker restrictions
remain in place. These permissions apply to the entire `adjutant` container,
not just the helper executable. Codex adds its own read-only mounts and network
restrictions before model-generated commands execute. The other services do not
use this profile.

This profile was exercised with Codex 0.153.4 and Debian Bookworm's Bubblewrap
0.8.0. The VM bundle targets Linux x86-64. Review the appended rules and the
upstream snapshot when upgrading Codex, Bubblewrap, Docker, or the supported
architecture; a checked-in copy does not automatically receive Moby updates.
Run the [command sandbox check](../README.md#check-the-command-sandbox) on the
actual host after changes. Do not replace this allowlist with `seccomp=unconfined`.

The Moby-derived profiles use Apache-2.0; the upstream license is in
[LICENSE.moby](LICENSE.moby). AppArmor's upstream template uses ABI 3; our profile
uses ABI 4 to express `userns` and adds explicit Unix socket access to preserve
that part of the older `network` rule.

## AppArmor for Codex bubblewrap

Ubuntu 24.04 restricts unprivileged user namespaces by default. Codex uses
bubblewrap to create its read-only, network-disabled command sandbox, so the
Adjutant container needs the narrowly scoped `adjutant-sandbox` AppArmor
profile rather than a system-wide kernel setting.

The profile is derived from Moby's `docker-default` template at revision
`61eaf32614c7c71b60bd8927d3e6a4ffc8ff1f31`. It keeps the default profile's
procfs, sysfs, AF_ALG, and AF_VSOCK restrictions. It additionally allows user
namespace creation, mounting, and `pivot_root` so bubblewrap can build its
private sandbox.

Install or reload it on the VM before starting the Compose service:

```bash
sudo install -D -m 0644 security/apparmor-adjutant \
  /etc/apparmor.d/adjutant-sandbox
sudo apparmor_parser -r -W /etc/apparmor.d/adjutant-sandbox
sudo aa-status | grep -F adjutant-sandbox
```

Enable the Compose overlay for this VM by setting
`COMPOSE_FILE=compose.yaml:compose.apparmor.yaml` in `.env`. It selects this
profile only for the `adjutant` service. Do not disable
`kernel.apparmor_restrict_unprivileged_userns` globally and do not use Docker's
`apparmor=unconfined` option.

`mount,` is deliberately broad because AppArmor cannot label bubblewrap's
private mount namespace differently from its parent container. That permission
does not make mounts usable in the outer container: the service has no
`SYS_ADMIN` capability, uses `no-new-privileges`, and retains Docker's seccomp
filter. Once bubblewrap creates its user namespace, the permitted mounts only
form a private filesystem view; these permissions apply to all processes in this
container, not only the Bubblewrap executable. Keep the matching Docker
seccomp profile and the read-only/network-disabled Codex settings in place.

This repository can validate the policy's syntax with Ubuntu's parser, but
Docker Desktop does not enforce AppArmor. Verify it on the VM after deployment:

```bash
sudo docker compose exec -u 10001:10001 adjutant \
  cat /proc/self/attr/current
sudo docker compose run --rm --no-deps adjutant \
  node /usr/local/lib/adjutant/check-codex-sandbox.mjs
```

The first command should print `adjutant-sandbox (enforce)`. The smoke test
exercises the full bubblewrap setup and verifies the command sandbox still
blocks writes and network access. If it fails, inspect the relevant kernel audit
messages without exposing application data:

```bash
sudo dmesg --level=err,warn | grep -i apparmor
```
