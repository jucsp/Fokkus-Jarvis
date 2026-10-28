%global debug_package %{nil}
%global __os_install_post %{nil}
%define _build_id_links none

Name:           fokkus-jarvis
Version:        0.3.0
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
* Wed Oct 28 2026 Juan Carlos Saavedra <juanc.saavedra07@gmail.com> - 0.3.0-1
- Épica 14: Hilo Central + Sub-sesiones Ramificadas (HU-59 a HU-68).
- HU-59: Normalizador léxico unificado con preservación de acentos y puntuación.
- HU-60: Cerebro desacoplado y guardia antirrecursión de subagentes (JARVIS_SUB_SESSION).
- HU-61: Red de seguridad local para fallos LLM y unificación 'task run'.
- HU-62: SessionManager persistente multi-turno y multi-hilo con rehidratación.
- HU-63: TurnRouter con planificador LLM, turnos compuestos y concurrencia.
- HU-64: Direccionamiento verbal de sesiones por id/ordinal/alias y foco de 5 min.
- HU-65: Roster de sub-sesiones en contexto efímero (presupuesto <= 1600 chars).
- HU-66: Métodos IPC jarvis.sessions.*, eventos JSON-RPC y CLI de sesiones.
- HU-67: HUD con chips interactivos de sub-sesiones y menú en bandeja del sistema.
- HU-68: Métricas de enrutamiento y suite de validación E2E.

* Tue Oct 27 2026 Juan Carlos Saavedra <juanc.saavedra07@gmail.com> - 0.2.29-1
- HU-55: estados visuales de la esfera (escucha pasiva/activa, background) con paleta y velocidad propias.
- HU-56: barge-in anti-eco más sensible e interrupción inmediata con clic simple en la esfera.
- HU-57: subagentes ejecutores reales de tareas OS en segundo plano (monitor/ping/contador) y cancelación flexible por voz.
- HU-58: aislamiento headless del CLI hijo y MCPs (BROWSER=/bin/true, NO_BROWSER=1).

* Mon Oct 19 2026 Juan Carlos Saavedra <juanc.saavedra07@gmail.com> - 0.2.27-1
- Feat (Issue #7 Fallback): tool nativa 'fokkus-jarvis dev run <tarea>' e instrucción en SYSTEM_PROMPT para despacho desacoplado cognitivo de tareas de desarrollo

* Mon Oct 19 2026 Juan Carlos Saavedra <juanc.saavedra07@gmail.com> - 0.2.26-1
- Feat (Issue #7): tareas de desarrollo en segundo plano con contexto de la propuesta, timeout CLI configurable (cli_timeout_secs) y consulta verbal del estado del desarrollo

* Fri Oct 16 2026 Juan Carlos Saavedra <juanc.saavedra07@gmail.com> - 0.2.25-1
- Fix: comando local de silencio y reposo inmediato sin pasar por el LLM ni reactivar follow-up

* Fri Oct 16 2026 Juan Carlos Saavedra <juanc.saavedra07@gmail.com> - 0.2.24-1
- HU-38/HU-39: subcomandos CLI 'fokkus-jarvis input (type|key|click)' y 'capture' para ojos y manos del agente.
- Fix: inyeccion explicita de YDOTOOL_SOCKET en invocaciones de ydotool.
- Orquestador: reconocimiento flexible de ordenes de escritura y eliminacion de restricciones en system prompt.

* Fri Oct 16 2026 Juan Carlos Saavedra <juanc.saavedra07@gmail.com> - 0.2.23-1
- HU-38/HU-39: despachador de navegación web/YouTube (<50ms), controles multimedia y escritura en pantalla vía jarvis-automation e ydotool.
- HU-38/HU-39: guardarraíles contra ejecución ciega en bash.

* Fri Oct 16 2026 Juan Carlos Saavedra <juanc.saavedra07@gmail.com> - 0.2.22-1
- Fix UI: eliminar falso auto-ocultamiento en ventanas maximizadas CSD y desktop plasmashell (regla estricta de fullscreen).
- Fix UI: soporte StatusNotifierItem (bandeja del sistema) vía D-Bus evitando aparición en la barra de tareas.
- HU-38: crate jarvis-automation (geometría, captura multiplataforma, entrada, guard con kill switch y auditoría).
- HU-39: matriz de riesgo dual (LowRisk autónomo vs HighRisk con autoverificación y confirmación vocal).

* Fri Oct 16 2026 Juan Carlos Saavedra <juanc.saavedra07@gmail.com> - 0.2.21-1
- Fix orquestador: restringir 'is_sounding' exclusivamente a 'Speaking' evitando falsos barge-ins en 'Thinking'.
- Inferencia protegida: procesos largos de LLM/CLI en silencio no son cancelados por ruido ambiente.

* Fri Oct 16 2026 Juan Carlos Saavedra <juanc.saavedra07@gmail.com> - 0.2.20-1
- HU-AUDIO-CONTINUITY: histéresis VAD de dos niveles (open 0.45, hold 0.25) y silencio extendido a 1200 ms / conector a 2000 ms.
- Concatenación de turnos consecutivos en Thinking para pausas naturales sin pérdida de contexto ni corte de frases.
- Fusión de globos de texto del usuario en HUD renovando TTL en continuaciones.

* Fri Oct 16 2026 Juan Carlos Saavedra <juanc.saavedra07@gmail.com> - 0.2.19-1
- Fix HUD Wayland: evitar desconfiguración en remapeo/resize manteniendo capa activa transparente.
- Voz: ampliar silencio de cierre de 450 ms a 750 ms en endpointer para pausas naturales sin corte.

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
