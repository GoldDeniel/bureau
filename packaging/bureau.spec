%define debug_package %{nil}
%define _build_id_links none

Name:           bureau
Version:        0.5.3
Release:        1%{?dist}
Summary:        Ticket system for a company of AI agents
License:        MIT
BuildArch:      x86_64
Requires:       glibc >= 2.39
Recommends:     curl

%description
Bureau is a local ticket server with divisions, one manager agent per division,
and worker agents. Managers triage incoming tickets into self-contained
sub-tickets and workers run them with a pluggable adapter (Ollama, Hermes,
OpenCode or a shell command). Ships a single binary, an embedded web board and
a systemd template unit: bureau@<user>.service.

%prep

%build

%install
install -Dm0755 %{stage}/bureau %{buildroot}/usr/bin/bureau
install -Dm0644 %{stage}/bureau@.service %{buildroot}/usr/lib/systemd/system/bureau@.service
install -Dm0644 %{stage}/README.md %{buildroot}/usr/share/doc/bureau/README.md
install -Dm0644 %{stage}/AGENT-INTEGRATION.md %{buildroot}/usr/share/doc/bureau/AGENT-INTEGRATION.md
install -Dm0644 %{stage}/INSTALL.md %{buildroot}/usr/share/doc/bureau/INSTALL.md
install -Dm0644 %{stage}/LICENSE %{buildroot}/usr/share/licenses/bureau/LICENSE

%post
if [ ! -e /etc/bureau/bureau.env ]; then
  install -d -m 0755 /etc/bureau
  TOKEN=$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')
  : > /etc/bureau/bureau.env
  chmod 600 /etc/bureau/bureau.env
  printf 'BUREAU_BIND=0.0.0.0:8787\nBUREAU_TOKEN=%s\n' "$TOKEN" >> /etc/bureau/bureau.env
fi
if [ -d /run/systemd/system ]; then
  systemctl daemon-reload >/dev/null 2>&1 || :
fi

%postun
if [ -d /run/systemd/system ]; then
  systemctl daemon-reload >/dev/null 2>&1 || :
fi

%files
/usr/bin/bureau
/usr/lib/systemd/system/bureau@.service
%doc /usr/share/doc/bureau/README.md
%doc /usr/share/doc/bureau/AGENT-INTEGRATION.md
%doc /usr/share/doc/bureau/INSTALL.md
%license /usr/share/licenses/bureau/LICENSE

%changelog
* Mon Nov 09 2026 Daniel <you@example.com> - 0.2.0-1
- LAN binding, optional token auth, systemd template unit, packaging.
