%global debug_package %{nil}
%global __os_install_post %{nil}
%define _build_id_links none

Name:           fokkus-jarvis
Version:        0.2.18
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
* Fri Oct 16 2026 Juan Carlos Saavedra <juanc.saavedra07@gmail.com> - 0.2.18-1
- HU-36: HUD de globos de texto efímeros (Speech Popups) con capa Wayland transparente y click-through.
- Rasterizado de texto en CPU con fontdue y Noto Sans embebida, TTL dinámico según longitud de texto.
- Sincronización visual con los turnos de voz y toggle de activación en Ajustes Generales.

* Fri Oct 16 2026 Juan Carlos Saavedra <juanc.saavedra07@gmail.com> - 0.2.17-1
- HU-37: Tareas pesadas asíncronas en segundo plano sin bloqueo conversacional.
- Acuse vocal inmediato (< 1s) y entrega secuencial en SpeechQueue con DeliveryGate.
- Inmunidad a barge-in para workers en segundo plano y métodos IPC de despacho y consulta.

* Fri Oct 16 2026 Juan Carlos Saavedra <juanc.saavedra07@gmail.com> - 0.2.16-1
- HU-35: Detección universal de pantalla completa y modo ventana sin bordes (Auto-Dim al 25% / Auto-Hide).
- Integración con KWin Scripting sobre D-Bus de sesión (io.fokkus.Jarvis.WindowWatch).
- Histéresis de 300 ms, override vocal para visibilidad al hablar y selector en Ajustes Generales.

* Fri Oct 16 2026 Juan Carlos Saavedra <juanc.saavedra07@gmail.com> - 0.2.15-1
- HU-33: Redimensionado dinámico en tiempo real (50% a 200%) desde Ajustes Generales.
- HU-34: Opacidad y transparencia variable (20% a 100%) con interpolador OpacityFade.
- Actualización reactiva por stdout piped entre subproceso de ajustes y daemon.
