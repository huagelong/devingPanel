Name: deving-panel
Version: %{app_version}
Release: 1
Summary: Desktop client for Docker deployment over SSH
License: BSD-3-Clause
Requires: libsecret-1.so.0()(64bit)
Requires: libvulkan.so.1()(64bit)
%global debug_package %{nil}

%description
Desktop client for Docker deployment over SSH.

%prep

%build

%install
mkdir -p %{buildroot}
cp -a "%{_sourcedir}/usr" "%{buildroot}/"

%files
/usr/bin/deving-panel
/usr/share/applications/deving-panel.desktop
/usr/share/icons/hicolor/scalable/apps/deving-panel.svg
%license /usr/share/doc/deving-panel/LICENSE
