%bcond_without check

%global crate bootupd

Name:           rust-%{crate}
Version:        0.2.9
Release:        1%{?dist}
Summary:        Bootloader updater

License:        Apache-2.0
URL:            https://github.com/coreos/bootupd
Source0:        %{url}/releases/download/v%{version}/bootupd-%{version}.tar.zstd
Source1:        %{url}/releases/download/v%{version}/bootupd-%{version}-vendor.tar.zstd
%if 0%{?fedora} || 0%{?rhel} >= 10
ExcludeArch:    %{ix86}
%endif

BuildRequires: git-core
# For now, see upstream
BuildRequires: make
BuildRequires:  openssl-devel
%if 0%{?rhel}
BuildRequires: rust-toolset
%else
BuildRequires:  cargo-rpm-macros >= 25
%endif
BuildRequires:  systemd

%global _description %{expand:
Bootloader updater}
%description %{_description}

%package     -n %{crate}
Summary:        %{summary}
# Apache-2.0
# Apache-2.0 OR BSL-1.0
# Apache-2.0 OR MIT
# Apache-2.0 WITH LLVM-exception
# Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT
# BSD-3-Clause
# MIT
# MIT OR Apache-2.0
# Unlicense OR MIT
License:        Apache-2.0 AND (Apache-2.0 WITH LLVM-exception) AND BSD-3-Clause AND MIT AND (Apache-2.0 OR BSL-1.0) AND (Apache-2.0 OR MIT) AND (Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT) AND (Unlicense OR MIT)
%{?systemd_requires}

%description -n %{crate} %{_description}

%files -n %{crate}
%license LICENSE
%license LICENSE.dependencies
%license cargo-vendor.txt
%doc README.md
%{_bindir}/bootupctl
%{_libexecdir}/bootupd
%{_prefix}/lib/bootupd/grub2-static/
%{_unitdir}/bootloader-update.service

%prep
%autosetup -n %{crate}-%{version} -p1 -Sgit -a1
# Default -v vendor config doesn't support non-crates.io deps (i.e. git)
cp .cargo/vendor-config.toml .
%cargo_prep -N
cat vendor-config.toml >> .cargo/config.toml
rm vendor-config.toml

%build
%cargo_build
%cargo_vendor_manifest
# https://pagure.io/fedora-rust/rust-packaging/issue/33
sed -i -e '/https:\/\//d' cargo-vendor.txt
%cargo_license_summary
%{cargo_license} > LICENSE.dependencies

%install
%make_install INSTALL="install -p -c"
%{__make} install-grub-static DESTDIR=%{?buildroot} INSTALL="%{__install} -p"
%{__make} install-systemd-unit DESTDIR=%{?buildroot} INSTALL="%{__install} -p"

%changelog
* Tue Oct 18 2022 Colin Walters <walters@verbum.org> - 0.2.8-3
- Dummy changelog
