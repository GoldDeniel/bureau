# Installing Bureau

Packages install the binary, a systemd template unit and docs. The ticket
database is created on first start under `/var/lib/bureau/<user>/`.

## Dependencies

Running a package needs only libc + systemd: SQLite is compiled into the binary
(`rusqlite`, `bundled`), so no `libsqlite3-dev`, no database server, no OpenSSL.

Building from source needs Rust **1.85+** (`edition = "2024"`) plus a C compiler for
the bundled SQLite (`sudo dnf group install "Development Tools"` /
`sudo apt install build-essential`). Building a package additionally needs one tool:
`rpmbuild` for RPM, `makepkg` on an Arch-family host for the Arch package, nothing
extra for the .deb. `packaging/build-all.sh` builds only the formats your distro can
produce and reports the others as skipped.

## Fedora / RHEL / Nobara

```bash
sudo dnf install ./dist/bureau-*.rpm
```

## Debian / Ubuntu

```bash
sudo apt install ./dist/bureau_*.deb          # resolves libc6 automatically
# or: sudo dpkg -i ./dist/bureau_*.deb && sudo apt -f install
```

## Arch / CachyOS

```bash
sudo pacman -U ./dist/bureau-*-x86_64.pkg.tar.zst
```

## Start it

The unit is a template: the instance name is the Linux user that owns your
projects, so the agents can actually edit them.

```bash
sudo systemctl enable --now bureau@$USER
systemctl status bureau@$USER
```

First start runs `bureau init`, which seeds the divisions and a welcome ticket.

**`inactive (dead)` is almost always "never started", not "crashed".** Packages ship
the unit disabled on purpose. `systemctl show bureau@$USER -p Result` returning
`success` with `NRestarts=0` confirms nothing ever ran, and the proof is the state
directory: a first start creates `/var/lib/bureau/<user>/`, so if `/var/lib/bureau/`
is missing there has been no successful start. Fix with the `enable --now` line
above; then read `journalctl -u bureau@$USER -n 40` if it still fails.

## Open the board

The installer writes `/etc/bureau/bureau.env` with a random access token and
`BUREAU_BIND=0.0.0.0:8787` (LAN enabled):

```bash
sudo cat /etc/bureau/bureau.env      # the BUREAU_TOKEN line
```

Then open:

```
http://<host>:8787/?token=<the-token>
```

The `?token=` part is stored in the browser and removed from the address bar,
so bookmarks can be plain `http://<host>:8787/`. To reset the browser
token, clear localStorage for the site (or use the `?token=` link again).

### If the LAN still cannot reach it

- Check the bind address: `ss -ltn | grep 8787` should show `0.0.0.0:8787`.
- ufw: `sudo ufw allow 8787/tcp`
- firewalld: `sudo firewall-cmd --add-port=8787/tcp --permanent && sudo firewall-cmd --reload`
- nftables: allow tcp dport 8787 from your LAN subnet.
- Never port-forward this to the internet — the API runs agents with your
  user's permissions. There is no TLS; use it on a trusted LAN or behind a
  reverse proxy with HTTPS.

## Configuration

`/etc/bureau/bureau.env`:

| variable | meaning |
|---|---|
| `BUREAU_BIND` | listen address, default `0.0.0.0:8787` in the packaged file |
| `BUREAU_TOKEN` | require this token for `/api/*`; empty disables auth |
| `BUREAU_PROJECTS_ROOT` | base path for the seeded divisions |

Set `BUREAU_TOKEN=` (empty) to disable auth, or delete the line. The board and
`/api/health` stay public either way.

Apply changes:

```bash
sudo systemctl restart bureau@$USER
```

## Data and logs

- Database: `/var/lib/bureau/<user>/bureau.db` (SQLite, WAL)
- Logs: `journalctl -u bureau@$USER -f`
- Environment: `/etc/bureau/bureau.env`

## Uninstall

```bash
sudo systemctl disable --now bureau@$USER
# Arch
sudo pacman -R bureau
# Debian
sudo apt remove bureau        # add --purge to drop /etc/bureau/bureau.env
# Fedora
sudo dnf remove bureau
```

Ticket data is intentionally kept. Remove `/var/lib/bureau/<user>/` for a
clean slate.

## Notes on portability

These packages bundle a dynamically linked binary built on CachyOS
(glibc symbols up to 2.39). They run on Fedora 40+, Ubuntu 24.04+, Debian 13+
and current Arch. For an older distro, rebuild inside it:

```bash
cargo build --release --locked
packaging/build-deb.sh    # or build-rpm.sh / build-arch.sh
```
