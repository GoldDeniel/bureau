# Installing Bureau

Packages install the binary, a systemd template unit and docs. The ticket
database is created on first start under `/var/lib/bureau/<user>/`.

## Arch / CachyOS

```bash
sudo pacman -U bureau-0.2.2-1-x86_64.pkg.tar.zst
```

## Debian / Ubuntu

```bash
sudo apt install ./bureau_0.2.2_amd64.deb     # resolves libc6 automatically
# or: sudo dpkg -i bureau_0.2.2_amd64.deb && sudo apt -f install
```

## Fedora / RHEL

```bash
sudo dnf install ./bureau-0.2.2-1.x86_64.rpm
```

## Start it

The unit is a template: the instance name is the Linux user that owns your
projects, so the agents can actually edit them.

```bash
sudo systemctl enable --now bureau@$USER
systemctl status bureau@$USER
```

First start runs `bureau init`, which seeds the divisions and a welcome ticket.

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
