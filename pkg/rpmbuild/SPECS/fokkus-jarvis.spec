%global debug_package %{nil}
%global __os_install_post %{nil}
%define _build_id_links none

Name:           fokkus-jarvis
Version:        0.2.15
Release:        1
Summary:        Asistente de voz nativo de Fokkus con esfera líquida e integración IPC con Fokkus IDE.
License:        Proprietary
BuildArch:      x86_64
Source0:        fokkus-jarvis
Source1:        fokkus-jarvis.desktop

%description
Asistente de voz nativo de Fokkus con esfera líquida e integración IPC con Fokkus IDE.

%install
install -Dm755 %{SOURCE0} %{buildroot}%{_bindir}/fokkus-jarvis
install -Dm644 %{SOURCE1} %{buildroot}%{_datadir}/applications/fokkus-jarvis.desktop

%files
%{_bindir}/fokkus-jarvis
%{_datadir}/applications/fokkus-jarvis.desktop

%changelog
* Fri Oct 16 2026 Juan Carlos Saavedra <juanc.saavedra07@gmail.com> - 0.2.15-1
- HU-33: Redimensionado dinámico en tiempo real (50% a 200%) desde Ajustes Generales.
- HU-34: Opacidad y transparencia variable (20% a 100%) con interpolador OpacityFade.
- Actualización reactiva por stdout piped entre subproceso de ajustes y daemon.

* Fri Oct 16 2026 Juan Carlos Saavedra <juanc.saavedra07@gmail.com> - 0.2.14-1
- Apertura local de la carpeta de Descargas ("abre la carpeta de descargas").
- El prompt del CLI pide usar herramientas para ejecutar acciones en el equipo.
