// Fokkus-Jarvis — monitor de ventana activa para KWin (HU-35).
//
// Lo carga el daemon con org.kde.kwin.Scripting.loadScript y lo descarga al
// salir. Reporta la ventana activa al servicio de sesión
// io.fokkus.Jarvis.WindowWatch. Por privacidad NO envía el título.
//
// `@@JARVIS_PID@@` lo sustituye el daemon al escribir el archivo.

const JARVIS_PID = @@JARVIS_PID@@;
const SERVICE = "io.fokkus.Jarvis.WindowWatch";

let current = null;

function send(payload) {
    callDBus(SERVICE, "/WindowWatch", SERVICE, "Report", payload);
}

// El escritorio (plasmashell) y los paneles/docks cubren el output sin
// bordes: nunca son pantalla completa, se reportan como "sin ventana activa".
function isShellWindow(w) {
    return !!(w.desktopWindow || w.dock);
}

function report() {
    const w = current;
    if (!w || isShellWindow(w)) {
        send({ active: false });
        return;
    }
    const g = w.frameGeometry;
    const o = w.output;
    const og = o ? o.geometry : null;
    send({
        active: true,
        app_id: String(w.resourceClass || ""),
        pid: w.pid || 0,
        fullscreen: !!w.fullScreen,
        decorated: !w.noBorder,
        x: Math.round(g.x),
        y: Math.round(g.y),
        w: Math.round(g.width),
        h: Math.round(g.height),
        output: o ? String(o.name) : "",
        ox: og ? Math.round(og.x) : 0,
        oy: og ? Math.round(og.y) : 0,
        ow: og ? Math.round(og.width) : 0,
        oh: og ? Math.round(og.height) : 0,
        own: w.pid === JARVIS_PID,
    });
}

function untrack() {
    if (!current) {
        return;
    }
    try {
        current.fullScreenChanged.disconnect(report);
        current.frameGeometryChanged.disconnect(report);
        if (current.outputChanged) {
            current.outputChanged.disconnect(report);
        }
    } catch (e) {
        // La ventana ya se destruyó: no quedan conexiones que soltar.
    }
    current = null;
}

function track(w) {
    untrack();
    current = w || null;
    if (current) {
        current.fullScreenChanged.connect(report);
        current.frameGeometryChanged.connect(report);
        if (current.outputChanged) {
            current.outputChanged.connect(report);
        }
    }
    report();
}

workspace.windowActivated.connect(track);
workspace.windowRemoved.connect(function (w) {
    if (w === current) {
        track(null);
    }
});
track(workspace.activeWindow);
