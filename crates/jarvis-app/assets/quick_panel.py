"""Panel rápido de Fokkus-Jarvis (clic derecho en la esfera).

Uso: python3 -I -c <script> <config.toml> --pid <pid>

Popup flotante compacto (336 px, tamaño fijo) que se abre junto al cursor y
reemplaza el menú plano de clic derecho.
Lee y guarda el TOML de configuración; el botón de micrófono habla con el
servidor IPC (jarvis.setMuted / jarvis.getState); "Salir" envía SIGTERM y
"Posición inicial" sale con el código 3 para que el daemon restaure el
anclaje de la esfera.

Código de salida:
  0  = cerrado normalmente (recargar configuración)
  1  = cancelado (Esc o pérdida de foco sin guardar)
  2  = error (sin pantalla)
  3  = volver a la posición inicial de la esfera
  10 = abrir Ajustes Unificados en General
  11 = abrir Ajustes Unificados en Voz
  12 = abrir Ajustes Unificados en Cerebro
  13 = abrir Ajustes Unificados en Escucha
"""

import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import tomllib

# En KWin Wayland una ventana nativa se abre como xdg_toplevel (sin posición
# libre y a veces en mosaico). Con Xwayland el panel se ubica junto al cursor.
if os.environ.get("DISPLAY") and not os.environ.get("QT_QPA_PLATFORM"):
    os.environ["QT_QPA_PLATFORM"] = "xcb"

from PyQt6.QtCore import QEvent, QPoint, QRectF, QSize, Qt, QTimer
from PyQt6.QtGui import QColor, QCursor, QGuiApplication, QIcon, QPainter, QPen, QPixmap
from PyQt6.QtWidgets import (
    QAbstractButton,
    QApplication,
    QButtonGroup,
    QFrame,
    QGraphicsDropShadowEffect,
    QHBoxLayout,
    QLabel,
    QLayout,
    QLineEdit,
    QPushButton,
    QVBoxLayout,
    QWidget,
)

# ---------------------------------------------------------------- tema
BG = "#141720"
CARD = "#1A1E2A"
CARD_BORDER = "#232838"
FIELD = "#161924"
FIELD_BORDER = "#252B3B"
BADGE = "#252B3B"
SEGMENT_ON = "#252E38"
TEXT = "#E8EAF2"
MUTED = "#7E8B9B"
CAPTION = "#717D8E"
ACCENT = "#2DE3B0"
ON_ACCENT = "#0B0F14"
MIC_MUTED = "#2B313F"
PLAY_BG = "#16332C"
DANGER = "#F87171"
DANGER_BG = "#26181D"
DANGER_BORDER = "#3E232B"
SWITCH_OFF = "#353B4B"

PANEL_WIDTH = 336
SHADOW = 12
CURSOR_GAP = 8

# Códigos de salida hacia el runtime Rust.
EXIT_SAVED = 0
EXIT_CANCELLED = 1
EXIT_ERROR = 2
EXIT_RESET_POSITION = 3
EXIT_OPEN_GENERAL = 10
EXIT_OPEN_VOZ = 11
EXIT_OPEN_CEREBRO = 12
EXIT_OPEN_ESCUCHA = 13

PREVIEW_TEXT = "Hola, soy Jarvis, tu asistente de escritorio."

DEFAULTS = {
    "brain_mode": "cli",
    "cli_command": "agy --effort low --dangerously-skip-permissions -p",
    "continuous_listening": False,
    "barge_in_enabled": False,
    "voice_id": "es-CL-LorenzoNeural",
    "voice_lang": "es-CL",
    "api_base_url": "",
    "api_model": "",
    "gaming_mode": False,
}

# (id, nombre, género, lang, región, bandera)
VOICES = [
    ("es-CL-LorenzoNeural", "Lorenzo", "Masculino", "es-CL", "Chile", "🇨🇱"),
    ("es-CL-CatalinaNeural", "Catalina", "Femenino", "es-CL", "Chile", "🇨🇱"),
    ("es-AR-ElenaNeural", "Elena", "Femenino", "es-AR", "Argentina", "🇦🇷"),
    ("es-AR-TomasNeural", "Tomás", "Masculino", "es-AR", "Argentina", "🇦🇷"),
    ("es-CO-GonzaloNeural", "Gonzalo", "Masculino", "es-CO", "Colombia", "🇨🇴"),
    ("es-CO-SalomeNeural", "Salomé", "Femenino", "es-CO", "Colombia", "🇨🇴"),
    ("es-MX-DaliaNeural", "Dalia", "Femenino", "es-MX", "México", "🇲🇽"),
    ("es-MX-JorgeNeural", "Jorge", "Masculino", "es-MX", "México", "🇲🇽"),
    ("es-ES-AlvaroNeural", "Álvaro", "Masculino", "es-ES", "España", "🇪🇸"),
    ("es-ES-ElviraNeural", "Elvira", "Femenino", "es-ES", "España", "🇪🇸"),
    ("es-PE-CamilaNeural", "Camila", "Femenino", "es-PE", "Perú", "🇵🇪"),
    ("es-PE-AlexNeural", "Alex", "Masculino", "es-PE", "Perú", "🇵🇪"),
    ("es-US-AlonsoNeural", "Alonso", "Masculino", "es-US", "Estados Unidos", "🇺🇸"),
    ("es-US-PalomaNeural", "Paloma", "Femenino", "es-US", "Estados Unidos", "🇺🇸"),
    ("es-VE-PaolaNeural", "Paola", "Femenino", "es-VE", "Venezuela", "🇻🇪"),
    ("en-US-AriaNeural", "Aria", "Femenino", "en-US", "Estados Unidos", "🇺🇸"),
    ("en-US-GuyNeural", "Guy", "Masculino", "en-US", "Estados Unidos", "🇺🇸"),
    ("en-GB-SoniaNeural", "Sonia", "Femenino", "en-GB", "Reino Unido", "🇬🇧"),
    ("en-GB-RyanNeural", "Ryan", "Masculino", "en-GB", "Reino Unido", "🇬🇧"),
]


def load_config(path):
    try:
        with open(path, "rb") as f:
            return tomllib.load(f)
    except (FileNotFoundError, tomllib.TOMLDecodeError):
        return {}


def toml_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return repr(value)
    return json.dumps(str(value), ensure_ascii=False)


def save_config(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    lines = [
        f"{key} = {toml_value(value)}"
        for key, value in data.items()
        if not isinstance(value, (dict, list))
    ]
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")
    os.replace(tmp, path)


def socket_path():
    override = os.environ.get("FOKKUS_JARVIS_SOCKET")
    if override:
        return override
    return os.path.join(os.path.expanduser("~"), ".fokkus", "jarvis.sock")


def ipc_call(method, params=None, timeout=1.0):
    """Habla con el servidor IPC NDJSON de Jarvis. Devuelve el dict o None."""
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
            sock.settimeout(timeout)
            sock.connect(socket_path())
            message = {"jsonrpc": "2.0", "id": 1, "method": method}
            if params is not None:
                message["params"] = params
            sock.sendall((json.dumps(message) + "\n").encode("utf-8"))
            reader = sock.makefile("r", encoding="utf-8")
            line = reader.readline()
            if not line:
                return None
            return json.loads(line)
    except (OSError, ValueError):
        return None


def get_muted():
    response = ipc_call("jarvis.getState")
    if response and isinstance(response.get("result"), dict):
        return bool(response["result"].get("muted", False))
    return False


def set_muted(muted):
    return ipc_call("jarvis.setMuted", {"muted": bool(muted)})


def voice_by_id(voice_id):
    for voice in VOICES:
        if voice[0] == voice_id:
            return voice
    return VOICES[0]


def voice_badge(voice):
    _voice_id, _name, _gender, lang, _region, _flag = voice
    country = lang.split("-")[-1].upper() if "-" in lang else lang.upper()
    return country


def play_file(path):
    players = (
        ["pw-play"],
        ["paplay"],
        ["aplay"],
        ["ffplay", "-nodisp", "-autoexit"],
        ["play"],
    )
    for player in players:
        if shutil.which(player[0]):
            subprocess.run(
                player + [path],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            return


def preview_voice(voice_id):
    tmp = tempfile.NamedTemporaryFile(suffix=".mp3", delete=False)
    tmp.close()
    try:
        result = subprocess.run(
            [
                "edge-tts",
                "--voice",
                voice_id,
                "--text",
                PREVIEW_TEXT,
                "--write-media",
                tmp.name,
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=30,
        )
        if result.returncode == 0:
            play_file(tmp.name)
    except (OSError, subprocess.TimeoutExpired):
        pass
    finally:
        try:
            os.unlink(tmp.name)
        except OSError:
            pass


def send_signal(pid, sig):
    try:
        os.kill(int(pid), sig)
        return True
    except (OSError, ValueError):
        return False


def mic_icon(color, size=22):
    """Icono de micrófono: cápsula, arco en U, vástago y base."""
    ratio = 2.0
    pixmap = QPixmap(int(size * ratio), int(size * ratio))
    pixmap.setDevicePixelRatio(ratio)
    pixmap.fill(Qt.GlobalColor.transparent)
    painter = QPainter(pixmap)
    painter.setRenderHint(QPainter.RenderHint.Antialiasing)
    pen = QPen(QColor(color))
    pen.setWidthF(size * 0.09)
    pen.setCapStyle(Qt.PenCapStyle.RoundCap)
    painter.setPen(pen)
    s = float(size)
    # Cápsula.
    painter.drawRoundedRect(QRectF(s * 0.36, s * 0.10, s * 0.28, s * 0.46), s * 0.14, s * 0.14)
    # Arco en U alrededor de la cápsula (mitad inferior de un círculo).
    painter.drawArc(QRectF(s * 0.22, s * 0.20, s * 0.56, s * 0.52), 180 * 16, 180 * 16)
    # Vástago y base.
    painter.drawLine(QPoint(int(s * 0.5), int(s * 0.72)), QPoint(int(s * 0.5), int(s * 0.86)))
    painter.drawLine(QPoint(int(s * 0.36), int(s * 0.88)), QPoint(int(s * 0.64), int(s * 0.88)))
    painter.end()
    return QIcon(pixmap)


class ToggleSwitch(QAbstractButton):
    """Interruptor tipo píldora (44x24)."""

    def __init__(self, checked=False, parent=None):
        super().__init__(parent)
        self.setCheckable(True)
        self.setChecked(checked)
        self.setFixedSize(44, 24)
        self.setCursor(Qt.CursorShape.PointingHandCursor)

    def sizeHint(self):
        return QSize(44, 24)

    def paintEvent(self, _event):
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing)
        painter.setPen(Qt.PenStyle.NoPen)
        on = self.isChecked()
        painter.setBrush(QColor(ACCENT if on else SWITCH_OFF))
        painter.drawRoundedRect(QRectF(0, 0, 44, 24), 12, 12)
        knob = 18
        x = 44 - knob - 3 if on else 3
        painter.setBrush(QColor(ON_ACCENT if on else "#FFFFFF"))
        painter.drawEllipse(QRectF(x, 3, knob, knob))
        painter.end()


class QuickPanel(QWidget):
    def __init__(self, config_path, pid, muted=None):
        super().__init__()
        self.config_path = config_path
        self.pid = pid
        self.data = load_config(config_path)
        self.current = {**DEFAULTS, **self.data}
        # Si `muted` llega por CLI se evita la consulta síncrona al socket:
        # el panel se abre de inmediato con el estado conocido.
        self._muted = bool(muted) if muted is not None else get_muted()
        self._ready = False
        self._finished = False
        self._mouse_entered = False
        self._left_early = False

        # Popup: ventana flotante sin mosaico que se cierra al clicar fuera.
        self.setWindowFlags(
            Qt.WindowType.Popup | Qt.WindowType.FramelessWindowHint
        )
        self.setAttribute(Qt.WidgetAttribute.WA_TranslucentBackground)

        # Contenedor redondeado con sombra.
        self.container = QFrame()
        self.container.setObjectName("panel")
        self.container.setFixedWidth(PANEL_WIDTH)
        shadow = QGraphicsDropShadowEffect(self)
        shadow.setBlurRadius(24)
        shadow.setOffset(0, 4)
        shadow.setColor(QColor(0, 0, 0, 150))
        self.container.setGraphicsEffect(shadow)

        outer = QVBoxLayout(self)
        outer.setContentsMargins(SHADOW, SHADOW, SHADOW, SHADOW)
        outer.setSizeConstraint(QLayout.SizeConstraint.SetFixedSize)
        outer.addWidget(self.container)

        root = QVBoxLayout(self.container)
        root.setContentsMargins(0, 0, 0, 0)
        root.setSpacing(0)

        body = QVBoxLayout()
        body.setContentsMargins(16, 16, 16, 14)
        body.setSpacing(8)
        body.addLayout(self._build_header())
        body.addSpacing(8)
        body.addWidget(self._caption("VOZ"))
        body.addWidget(self._build_voice_card())
        body.addSpacing(6)
        body.addWidget(self._caption("CEREBRO"))
        body.addLayout(self._build_brain_row())
        body.addSpacing(4)
        body.addWidget(self._build_switches_card())
        root.addLayout(body)

        root.addWidget(self._separator())
        root.addLayout(self._build_footer())

        self.setStyleSheet(self._stylesheet())

        # Tamaño fijo: la ventana nunca se estira (ni en mosaico).
        self.adjustSize()
        self.setFixedSize(self.sizeHint())

        # Cierre por pérdida de foco (una vez visible y con foco).
        app = QApplication.instance()
        app.focusChanged.connect(self._on_focus_changed)
        app.applicationStateChanged.connect(self._on_app_state_changed)
        QTimer.singleShot(200, self._mark_ready)

        # En Wayland/Xwayland los eventos de foco y clics externos pueden no
        # llegar: un temporizador cierra el panel cuando el cursor sale de sus
        # límites (margen de tolerancia de 10 px).
        self._mouse_timer = QTimer(self)
        self._mouse_timer.setInterval(50)
        self._mouse_timer.timeout.connect(self._check_mouse_proximity)
        self._mouse_timer.start()

    # ------------------------------------------------------------- build
    def _build_header(self):
        row = QHBoxLayout()
        row.setSpacing(12)

        self.mic_btn = QPushButton()
        self.mic_btn.setFixedSize(52, 52)
        self.mic_btn.setIconSize(QSize(22, 22))
        self.mic_btn.setCursor(Qt.CursorShape.PointingHandCursor)
        self.mic_btn.clicked.connect(self._toggle_mute)
        row.addWidget(self.mic_btn)

        texts = QVBoxLayout()
        texts.setSpacing(2)
        texts.addStretch(1)
        self.mic_title = QLabel()
        self.mic_title.setObjectName("micTitle")
        texts.addWidget(self.mic_title)
        self.mic_hint = QLabel()
        self.mic_hint.setObjectName("micHint")
        texts.addWidget(self.mic_hint)
        texts.addStretch(1)
        row.addLayout(texts, 1)

        self.exit_btn = QPushButton("⏻")
        self.exit_btn.setFixedSize(36, 36)
        self.exit_btn.setObjectName("exit")
        self.exit_btn.setCursor(Qt.CursorShape.PointingHandCursor)
        self.exit_btn.setToolTip("Cerrar Jarvis")
        self.exit_btn.clicked.connect(self._quit)
        row.addWidget(self.exit_btn, 0, Qt.AlignmentFlag.AlignVCenter)

        self._update_mic_style()
        return row

    def _build_voice_card(self):
        card = self._card()
        row = QHBoxLayout(card)
        row.setContentsMargins(14, 10, 14, 10)
        row.setSpacing(10)

        voice = voice_by_id(str(self.current.get("voice_id") or VOICES[0][0]))
        self.voice_badge_label = QLabel(voice_badge(voice))
        self.voice_badge_label.setObjectName("badge")
        row.addWidget(self.voice_badge_label, 0, Qt.AlignmentFlag.AlignVCenter)

        texts = QVBoxLayout()
        texts.setSpacing(1)
        self.voice_name_label = QLabel(voice[1])
        self.voice_name_label.setObjectName("voiceName")
        texts.addWidget(self.voice_name_label)
        self.voice_detail_label = QLabel(f"{voice[2]} · {voice[4]}")
        self.voice_detail_label.setObjectName("voiceDetail")
        texts.addWidget(self.voice_detail_label)
        row.addLayout(texts, 1)

        play = QPushButton("▶")
        play.setFixedSize(32, 32)
        play.setObjectName("play")
        play.setCursor(Qt.CursorShape.PointingHandCursor)
        play.setToolTip("Preescuchar voz")
        play.clicked.connect(lambda: preview_voice(voice[0]))
        row.addWidget(play)

        chevron = QPushButton("›")
        chevron.setFixedSize(24, 32)
        chevron.setObjectName("chevron")
        chevron.setCursor(Qt.CursorShape.PointingHandCursor)
        chevron.setToolTip("Abrir selector de voz")
        chevron.clicked.connect(lambda: self._finish(EXIT_OPEN_VOZ))
        row.addWidget(chevron)
        return card

    def _build_brain_row(self):
        row = QHBoxLayout()
        row.setSpacing(8)

        segment = QFrame()
        segment.setObjectName("segment")
        seg_layout = QHBoxLayout(segment)
        seg_layout.setContentsMargins(3, 3, 3, 3)
        seg_layout.setSpacing(2)

        mode = str(self.current.get("brain_mode") or "cli").lower()
        self.mode_group = QButtonGroup(self)
        self.mode_group.setExclusive(True)
        for key, label in (("cli", "CLI"), ("api", "API")):
            button = QPushButton(label)
            button.setObjectName("seg")
            button.setCheckable(True)
            button.setCursor(Qt.CursorShape.PointingHandCursor)
            button.setProperty("mode", key)
            button.setChecked(key == mode)
            self.mode_group.addButton(button)
            seg_layout.addWidget(button)
        self.mode_group.idClicked.connect(self._mode_changed)
        row.addWidget(segment)

        self.cli_edit = QLineEdit()
        self.cli_edit.setObjectName("field")
        self.cli_edit.setText(str(self.current.get("cli_command") or DEFAULTS["cli_command"]))
        self.cli_edit.setPlaceholderText(DEFAULTS["cli_command"])
        self.cli_edit.editingFinished.connect(self._persist)
        row.addWidget(self.cli_edit, 1)
        return row

    def _build_switches_card(self):
        card = self._card()
        layout = QVBoxLayout(card)
        layout.setContentsMargins(14, 12, 14, 12)
        layout.setSpacing(10)

        row, self.continuous_toggle = self._switch_row(
            "Escucha continua",
            "Responder sin decir “Jarvis”",
            bool(self.current.get("continuous_listening")),
        )
        layout.addLayout(row)
        layout.addWidget(self._separator("cardSeparator"))

        row, self.barge_toggle = self._switch_row(
            "Interrupción por voz",
            "Usa audífonos para evitar eco",
            bool(self.current.get("barge_in_enabled")),
        )
        layout.addLayout(row)
        return card

    def _build_footer(self):
        wrap = QVBoxLayout()
        wrap.setContentsMargins(0, 0, 0, 12)
        wrap.setSpacing(0)

        row = QHBoxLayout()
        row.setContentsMargins(16, 12, 16, 6)

        reset = QPushButton("✛  Posición inicial")
        reset.setObjectName("link")
        reset.setCursor(Qt.CursorShape.PointingHandCursor)
        reset.clicked.connect(self._reset_position)
        row.addWidget(reset)

        row.addStretch(1)

        all_settings = QPushButton("Todos los ajustes →")
        all_settings.setObjectName("linkAccent")
        all_settings.setCursor(Qt.CursorShape.PointingHandCursor)
        all_settings.clicked.connect(lambda: self._finish(EXIT_OPEN_GENERAL))
        row.addWidget(all_settings)
        wrap.addLayout(row)

        # Versión instalada, centrada y sutil (HU-32).
        v_str = os.environ.get("JARVIS_VERSION", "v0.3.1")
        if not v_str.startswith("v"):
            v_str = f"v{v_str}"
        version = QLabel(v_str)
        version.setObjectName("version")
        version.setAlignment(Qt.AlignmentFlag.AlignHCenter)
        wrap.addWidget(version)
        return wrap

    # ------------------------------------------------------------ helpers
    def _card(self):
        card = QFrame()
        card.setObjectName("card")
        return card

    def _caption(self, text):
        label = QLabel(text.upper())
        label.setObjectName("caption")
        return label

    def _separator(self, name="separator"):
        line = QFrame()
        line.setObjectName(name)
        line.setFixedHeight(1)
        return line

    def _switch_row(self, title, hint, checked):
        row = QHBoxLayout()
        row.setSpacing(10)
        texts = QVBoxLayout()
        texts.setSpacing(1)
        title_label = QLabel(title)
        title_label.setObjectName("switchTitle")
        texts.addWidget(title_label)
        hint_label = QLabel(hint)
        hint_label.setObjectName("hint")
        texts.addWidget(hint_label)
        row.addLayout(texts, 1)

        toggle = ToggleSwitch(checked)
        toggle.toggled.connect(self._persist)
        row.addWidget(toggle, 0, Qt.AlignmentFlag.AlignVCenter)
        return row, toggle

    def place_near_cursor(self, pos=None):
        """Ubica el panel junto al cursor (o a `pos`) sin salirse del área."""
        cursor = QCursor.pos() if pos is None else QPoint(pos[0], pos[1])
        screen = QGuiApplication.screenAt(cursor) or QGuiApplication.primaryScreen()
        width, height = self.width(), self.height()
        if screen is None:
            self.move(cursor)
            return
        area = screen.availableGeometry()

        # Por defecto a la derecha y hacia abajo de la esfera; si no cabe,
        # se abre a la izquierda y/o hacia arriba.
        x = cursor.x() + CURSOR_GAP - SHADOW
        if x + width > area.right() + 1:
            x = cursor.x() - CURSOR_GAP - width + SHADOW
        y = cursor.y() + CURSOR_GAP - SHADOW
        if y + height > area.bottom() + 1:
            y = cursor.y() - CURSOR_GAP - height + SHADOW

        x = max(area.left(), min(x, area.right() + 1 - width))
        y = max(area.top(), min(y, area.bottom() + 1 - height))
        self.move(x, y)

    # ------------------------------------------------------------- actions
    def _update_mic_style(self):
        if self._muted:
            self.mic_btn.setStyleSheet(
                f"background: {MIC_MUTED}; border: none; border-radius: 26px;"
            )
            self.mic_btn.setIcon(mic_icon(TEXT))
            self.mic_btn.setToolTip("Activar micrófono")
            self.mic_title.setText("Silenciado")
            self.mic_hint.setText("Toca para activar · M")
        else:
            self.mic_btn.setStyleSheet(
                f"background: {ACCENT}; border: none; border-radius: 26px;"
            )
            self.mic_btn.setIcon(mic_icon(ON_ACCENT))
            self.mic_btn.setToolTip("Silenciar micrófono")
            self.mic_title.setText("Escuchando")
            self.mic_hint.setText("Toca para silenciar · M")

    def _toggle_mute(self):
        self._muted = not self._muted
        set_muted(self._muted)
        self._update_mic_style()

    def _mode_changed(self, _button_id):
        self._persist()

    def _persist(self, *_args):
        mode = "cli"
        for button in self.mode_group.buttons():
            if button.isChecked():
                mode = button.property("mode") or "cli"
        self.data.update(
            {
                "brain_mode": mode,
                "cli_command": self.cli_edit.text().strip() or DEFAULTS["cli_command"],
                "continuous_listening": bool(self.continuous_toggle.isChecked()),
                "barge_in_enabled": bool(self.barge_toggle.isChecked()),
            }
        )
        try:
            save_config(self.config_path, self.data)
        except OSError:
            pass

    def _reset_position(self):
        self._finish(EXIT_RESET_POSITION)

    def _quit(self):
        send_signal(self.pid, signal.SIGTERM)
        self._finish(EXIT_SAVED)

    def _finish(self, code):
        if self._finished:
            return
        self._finished = True
        self._persist()
        QApplication.instance().exit(code)

    def _mark_ready(self):
        self._ready = True
        self.raise_()
        self.activateWindow()
        if self._left_early or (
            self._mouse_entered and not self.underMouse() and not self.cli_edit.hasFocus()
        ):
            self._finish(EXIT_SAVED)

    def _check_mouse_proximity(self):
        if not self._ready or self._finished:
            return
        if self.cli_edit.hasFocus():
            return
        if self._mouse_entered and not self.underMouse():
            self._finish(EXIT_SAVED)
            return
        cursor = QCursor.pos()
        if not self.geometry().contains(cursor):
            self._finish(EXIT_SAVED)

    def enterEvent(self, event):
        self._mouse_entered = True
        super().enterEvent(event)

    def leaveEvent(self, event):
        if not self._ready:
            self._left_early = True
        elif not self._finished and not self.cli_edit.hasFocus():
            self._finish(EXIT_SAVED)
        super().leaveEvent(event)

    def mousePressEvent(self, event):
        if not self.rect().contains(event.position().toPoint()):
            self._finish(EXIT_SAVED)
            return
        super().mousePressEvent(event)

    def event(self, event):
        if self._ready and not self._finished:
            if event.type() in (QEvent.Type.MouseButtonPress, QEvent.Type.NonClientAreaMouseButtonPress):
                pos = event.position() if hasattr(event, "position") else None
                if pos is not None and not self.rect().contains(pos.toPoint()):
                    self._finish(EXIT_SAVED)
                    return True
        return super().event(event)

    def _on_focus_changed(self, _old, new):
        if not self._ready:
            return
        if new is None or new.window() is not self:
            self._finish(EXIT_SAVED)

    def _on_app_state_changed(self, state):
        if not self._ready or self._finished:
            return
        if state != Qt.ApplicationState.ApplicationActive:
            self._finish(EXIT_SAVED)

    def changeEvent(self, event):
        if event.type() == QEvent.Type.ActivationChange:
            if self._ready and not self.isActiveWindow():
                self._finish(EXIT_SAVED)
        super().changeEvent(event)

    def hideEvent(self, event):
        if self._ready:
            self._finish(EXIT_SAVED)
        super().hideEvent(event)

    def closeEvent(self, event):
        # Qt cierra el Popup al hacer clic fuera: equivale a cerrar y guardar.
        self._finish(EXIT_SAVED)
        super().closeEvent(event)

    def keyPressEvent(self, event):
        if event.key() == Qt.Key.Key_Escape:
            self._finish(EXIT_CANCELLED)
            return
        if event.key() == Qt.Key.Key_M and not self.cli_edit.hasFocus():
            self._toggle_mute()
            return
        super().keyPressEvent(event)

    def _stylesheet(self):
        return f"""
        QFrame#panel {{ background: {BG}; border: 1px solid {CARD_BORDER}; border-radius: 16px; }}
        QFrame#card {{ background: {CARD}; border: 1px solid {CARD_BORDER}; border-radius: 12px; }}
        QFrame#separator, QFrame#cardSeparator {{ background: {CARD_BORDER}; border: none; }}
        QLabel {{ color: {TEXT}; font-size: 13px; background: transparent; }}
        QLabel#micTitle {{ color: #FFFFFF; font-size: 15px; font-weight: 700; }}
        QLabel#micHint {{ color: {MUTED}; font-size: 12px; }}
        QLabel#caption {{ color: {CAPTION}; font-size: 10px; font-weight: 700; }}
        QLabel#badge {{
            background: {BADGE}; color: {TEXT}; font-size: 11px; font-weight: 600;
            border-radius: 6px; padding: 4px 7px;
        }}
        QLabel#voiceName {{ color: {TEXT}; font-size: 14px; font-weight: 700; }}
        QLabel#voiceDetail, QLabel#hint {{ color: {MUTED}; font-size: 11px; }}
        QLabel#switchTitle {{ color: {TEXT}; font-size: 13px; }}
        QLabel#version {{ color: {MUTED}; font-size: 11px; }}
        QPushButton {{ color: {TEXT}; background: transparent; border: none; font-size: 12px; }}
        QPushButton#exit {{
            background: {DANGER_BG}; color: {DANGER}; border: 1px solid {DANGER_BORDER};
            border-radius: 18px; font-size: 16px;
        }}
        QPushButton#exit:hover {{ border-color: {DANGER}; }}
        QPushButton#play {{ background: {PLAY_BG}; color: {ACCENT}; border-radius: 16px;
                            font-size: 11px; }}
        QPushButton#chevron {{ color: {MUTED}; font-size: 20px; padding: 0; }}
        QPushButton#chevron:hover {{ color: #FFFFFF; }}
        QFrame#segment {{ background: {FIELD}; border: 1px solid {FIELD_BORDER}; border-radius: 8px; }}
        QPushButton#seg {{ color: {MUTED}; border-radius: 6px; padding: 5px 12px;
                           font-size: 12px; font-weight: 700; }}
        QPushButton#seg:checked {{ background: {SEGMENT_ON}; color: {ACCENT}; }}
        QPushButton#link {{ color: {MUTED}; font-size: 12px; padding: 0; }}
        QPushButton#link:hover {{ color: {TEXT}; }}
        QPushButton#linkAccent {{ color: {ACCENT}; font-size: 12px; font-weight: 700; padding: 0; }}
        QLineEdit#field {{ background: {FIELD}; color: {TEXT}; border: 1px solid {FIELD_BORDER};
                           border-radius: 8px; padding: 7px 10px; font-size: 12px;
                           font-family: monospace; }}
        QLineEdit#field:focus {{ border-color: {ACCENT}; }}
        """


def main():
    args = sys.argv[1:]
    if len(args) < 2:
        print("uso: quick_panel <config.toml> --pid <pid>", file=sys.stderr)
        return EXIT_ERROR

    config_path = args[0]
    pid = None
    pos = None
    muted = None
    if "--pid" in args:
        idx = args.index("--pid")
        if idx + 1 < len(args):
            pid = args[idx + 1]
    if "--pos" in args:
        idx = args.index("--pos")
        if idx + 2 < len(args):
            try:
                pos = (int(args[idx + 1]), int(args[idx + 2]))
            except ValueError:
                pos = None
    if "--muted" in args:
        idx = args.index("--muted")
        if idx + 1 < len(args):
            muted = args[idx + 1] in ("1", "true", "True")

    try:
        app = QApplication(sys.argv)
    except Exception as exc:  # sin display Qt
        print(f"sin pantalla para el panel rápido: {exc}", file=sys.stderr)
        return EXIT_ERROR

    # Cualquier cierre de la ventana debe terminar el proceso (sin zombis).
    app.setQuitOnLastWindowClosed(True)
    panel = QuickPanel(config_path, pid, muted)
    panel.place_near_cursor(pos)
    panel.show()
    return app.exec()


if __name__ == "__main__":
    sys.exit(main())
