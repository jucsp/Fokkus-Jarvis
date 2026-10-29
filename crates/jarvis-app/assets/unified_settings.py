"""Ajustes unificados de Fokkus-Jarvis (PyQt6).

Uso: python3 -I -c <script> <config.toml> [--pid <pid>] [--tab <pestaña>]

Ventana de 860x600 px con barra lateral de pestañas: Cerebro, Voz, Escucha
y General. Lee y guarda el TOML de configuración conservando las claves y
tablas que no edita; "volver a posición inicial" sale con el código 3 para
que el daemon restablezca el anclaje de la esfera.

Guardado: la barra inferior es común a todas las pestañas («Guardar cambios»
y «Descartar»). Ctrl+S o Enter en un campo de texto guardan y cierran; la ✕
guarda si hubo cambios; Esc descarta.

Código de salida: 0 = guardado, 1 = cancelado, 2 = error, 3 = volver a
posición inicial.

Vista previa en vivo (HU-33/HU-34): los sliders de la esfera escriben por
stdout una línea JSON por cambio, como máximo a 30 Hz, y siempre el valor
final al soltar el slider:

    {"preview": {"sphere_scale_pct": 135}}
    {"preview": {"sphere_opacity_pct": 60}}

El daemon la aplica al instante; si se cancela, vuelve al valor guardado.
"""

import datetime
import json
import os
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import tomllib

# En KWin Wayland una ventana nativa puede quedar en mosaico; con Xwayland
# flota libre centrada en el escritorio activo donde está el cursor.
if os.environ.get("DISPLAY") and not os.environ.get("QT_QPA_PLATFORM"):
    os.environ["QT_QPA_PLATFORM"] = "xcb"

from PyQt6.QtCore import Qt, QTimer
from PyQt6.QtGui import QColor, QCursor, QGuiApplication, QKeySequence, QShortcut
from PyQt6.QtWidgets import (
    QApplication,
    QButtonGroup,
    QCheckBox,
    QComboBox,
    QFrame,
    QGraphicsDropShadowEffect,
    QGridLayout,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QPushButton,
    QScrollArea,
    QSlider,
    QStackedWidget,
    QVBoxLayout,
    QWidget,
)

# ---------------------------------------------------------------- tema
BG = "#12141B"
SIDEBAR = "#171A23"
CARD = "#1A1E2A"
FIELD = "#0E1016"
BORDER = "#272B3A"
TEXT = "#E8EAF2"
MUTED = "#8A90A6"
ACCENT = "#2DE3B0"
ACCENT_DARK = "#24C296"
DANGER = "#FF8A8A"
SWITCH_OFF = "#3A4052"

WINDOW_W = 860
WINDOW_H = 600
SIDEBAR_W = 200
SHADOW = 20

EXIT_RESET_POSITION = 3

# Sliders de la esfera: (mínimo, máximo, paso) en porcentaje.
SPHERE_SCALE_RANGE = (50, 200, 5)
SPHERE_OPACITY_RANGE = (20, 100, 5)
# Intervalo mínimo entre líneas de vista previa (≈30 Hz).
PREVIEW_INTERVAL_MS = 33

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
    "sphere_scale_pct": 100,
    "sphere_opacity_pct": 100,
    "fullscreen_behavior": "dim",
    "hud_enabled": True,
}

# Reacción a ventanas a pantalla completa (HU-35): (valor TOML, etiqueta).
FULLSCREEN_BEHAVIORS = [
    ("dim", "Atenuar al 25% (recomendado)"),
    ("hide", "Ocultar esfera"),
    ("ignore", "Ignorar (desactivado)"),
]

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

REGION_FILTERS = [
    ("Todas", lambda v: True),
    ("Latam", lambda v: v[3].startswith("es-") and v[3] != "es-ES"),
    ("España", lambda v: v[3] == "es-ES"),
    ("Inglés", lambda v: v[3].startswith("en-")),
]

TABS = ["cerebro", "voz", "escucha", "general"]


def load_config(path):
    try:
        with open(path, "rb") as f:
            return tomllib.load(f)
    except (FileNotFoundError, tomllib.TOMLDecodeError):
        return {}


BARE_KEY = re.compile(r"^[A-Za-z0-9_-]+$")


def toml_key(key):
    key = str(key)
    return key if BARE_KEY.match(key) else json.dumps(key, ensure_ascii=False)


def toml_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return repr(value)
    if isinstance(value, (datetime.datetime, datetime.date, datetime.time)):
        return value.isoformat()
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(v) for v in value) + "]"
    if isinstance(value, dict):
        inner = ", ".join(f"{toml_key(k)} = {toml_value(v)}" for k, v in value.items())
        return "{ " + inner + " }" if inner else "{}"
    return json.dumps(str(value), ensure_ascii=False)


def is_table_array(value):
    return isinstance(value, list) and bool(value) and all(
        isinstance(v, dict) for v in value
    )


def toml_lines(data, prefix=()):
    """Serializa `data` a TOML: primero los valores simples y después las
    tablas (`[a.b]`) y arreglos de tablas (`[[a]]`), como exige el formato."""
    lines = [
        f"{toml_key(key)} = {toml_value(value)}"
        for key, value in data.items()
        if not isinstance(value, dict) and not is_table_array(value)
    ]
    for key, value in data.items():
        path = prefix + (toml_key(key),)
        if isinstance(value, dict):
            lines += ["", f"[{'.'.join(path)}]"] + toml_lines(value, path)
        elif is_table_array(value):
            for item in value:
                lines += ["", f"[[{'.'.join(path)}]]"] + toml_lines(item, path)
    return lines


def save_config(path, data):
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    lines = toml_lines(data)
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")
    os.replace(tmp, path)


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


def clamp_pct(value, bounds):
    low, high, _step = bounds
    try:
        value = int(value)
    except (TypeError, ValueError):
        value = 100
    return max(low, min(high, value))


def emit_preview(key, value):
    """Una línea JSON de vista previa por stdout (la lee el daemon)."""
    try:
        sys.stdout.write(json.dumps({"preview": {key: int(value)}}) + "\n")
        sys.stdout.flush()
    except (BrokenPipeError, OSError):
        pass


class PreviewThrottle:
    """Limita la emisión a una línea cada PREVIEW_INTERVAL_MS por clave.

    Coalescencia: si llega un valor dentro del intervalo se guarda y se emite
    al vencer el temporizador; `flush()` emite el pendiente al soltar.
    """

    def __init__(self, key, parent):
        self.key = key
        self.last_sent = None
        self.pending = None
        self.last_at = 0.0
        self.timer = QTimer(parent)
        self.timer.setSingleShot(True)
        self.timer.timeout.connect(self.flush)

    def push(self, value):
        self.pending = value
        elapsed_ms = (time.monotonic() - self.last_at) * 1000.0
        if elapsed_ms >= PREVIEW_INTERVAL_MS:
            self.flush()
        elif not self.timer.isActive():
            self.timer.start(int(PREVIEW_INTERVAL_MS - elapsed_ms) + 1)

    def flush(self):
        self.timer.stop()
        if self.pending is None:
            return
        value, self.pending = self.pending, None
        self.last_at = time.monotonic()
        if value != self.last_sent:
            self.last_sent = value
            emit_preview(self.key, value)


class UnifiedSettings(QWidget):
    def __init__(self, config_path, pid, tab):
        super().__init__()
        self.config_path = config_path
        self.pid = pid
        self.data = load_config(config_path)
        self.current = {**DEFAULTS, **self.data}
        self.selected_voice = str(
            self.current.get("voice_id") or VOICES[0][0]
        )
        self.region_index = 0

        self.setWindowFlags(
            Qt.WindowType.FramelessWindowHint | Qt.WindowType.Tool
        )
        self.setAttribute(Qt.WidgetAttribute.WA_TranslucentBackground)
        self.setFixedSize(WINDOW_W + 2 * SHADOW, WINDOW_H + 2 * SHADOW)

        self.container = QFrame()
        self.container.setObjectName("window")
        shadow = QGraphicsDropShadowEffect(self)
        shadow.setBlurRadius(32)
        shadow.setOffset(0, 8)
        shadow.setColor(QColor(0, 0, 0, 160))
        self.container.setGraphicsEffect(shadow)

        outer = QVBoxLayout(self)
        outer.setContentsMargins(SHADOW, SHADOW, SHADOW, SHADOW)
        outer.addWidget(self.container)

        root = QHBoxLayout(self.container)
        root.setContentsMargins(0, 0, 0, 0)
        root.setSpacing(0)

        root.addWidget(self._build_sidebar())
        right = QVBoxLayout()
        right.setContentsMargins(0, 0, 0, 0)
        right.setSpacing(0)
        right.addWidget(self._build_content(), 1)
        right.addWidget(self._build_footer())
        root.addLayout(right, 1)

        save_shortcut = QShortcut(QKeySequence(QKeySequence.StandardKey.Save), self)
        save_shortcut.activated.connect(self._save_and_close)
        for edit in (self.cli_edit, self.api_url_edit, self.api_model_edit):
            edit.returnPressed.connect(self._save_and_close)

        self.initial = self._collect()
        self._watch_changes()
        self.setStyleSheet(self._stylesheet())
        self._center_on_cursor_screen()

    def _center_on_cursor_screen(self):
        """Centra la ventana en el monitor activo (donde está el cursor)."""
        screen = QGuiApplication.screenAt(QCursor.pos()) or QGuiApplication.primaryScreen()
        if screen is None:
            return
        geo = screen.availableGeometry()
        self.move(
            geo.center().x() - (WINDOW_W + 2 * SHADOW) // 2,
            geo.center().y() - (WINDOW_H + 2 * SHADOW) // 2,
        )

    # ------------------------------------------------------------ sidebar
    def _build_sidebar(self):
        sidebar = QFrame()
        sidebar.setObjectName("sidebar")
        sidebar.setFixedWidth(SIDEBAR_W)
        layout = QVBoxLayout(sidebar)
        layout.setContentsMargins(16, 22, 16, 22)
        layout.setSpacing(6)

        header = QHBoxLayout()
        header.setSpacing(6)
        title = QLabel("AJUSTES")
        title.setObjectName("sideTitle")
        header.addWidget(title)
        header.addStretch(1)
        close_btn = QPushButton("✕")
        close_btn.setObjectName("closeBtn")
        close_btn.setFixedSize(32, 32)
        close_btn.setCursor(Qt.CursorShape.PointingHandCursor)
        close_btn.setToolTip("Cerrar (guarda los cambios pendientes)")
        close_btn.clicked.connect(self._close)
        header.addWidget(close_btn)
        layout.addLayout(header)
        layout.addSpacing(12)

        self.nav_group = QButtonGroup(self)
        self.nav_group.setExclusive(True)
        self.nav_buttons = {}
        labels = [
            ("cerebro", "Cerebro"),
            ("voz", "Voz"),
            ("escucha", "Escucha"),
            ("general", "General"),
        ]
        for key, label in labels:
            button = QPushButton(label)
            button.setObjectName("nav")
            button.setCheckable(True)
            button.setCursor(Qt.CursorShape.PointingHandCursor)
            button.setProperty("tab", key)
            self.nav_group.addButton(button)
            self.nav_buttons[key] = button
            layout.addWidget(button)
        layout.addStretch(1)
        return sidebar

    # ------------------------------------------------------------- content
    def _build_content(self):
        self.stack = QStackedWidget()
        self.stack.setObjectName("content")

        self.cerebro_page = self._build_cerebro()
        self.voz_page = self._build_voz()
        self.escucha_page = self._build_escucha()
        self.general_page = self._build_general()

        self.stack.addWidget(self.cerebro_page)  # 0
        self.stack.addWidget(self.voz_page)  # 1
        self.stack.addWidget(self.escucha_page)  # 2
        self.stack.addWidget(self.general_page)  # 3

        self.nav_group.idClicked.connect(self._on_nav_clicked)
        return self.stack

    def _build_footer(self):
        """Barra de acciones común a todas las pestañas."""
        footer = QFrame()
        footer.setObjectName("footer")
        layout = QHBoxLayout(footer)
        layout.setContentsMargins(28, 12, 28, 14)
        layout.setSpacing(10)

        self.status_label = QLabel("")
        self.status_label.setObjectName("status")
        layout.addWidget(self.status_label, 1)

        discard = QPushButton("Descartar")
        discard.setObjectName("secondary")
        discard.setCursor(Qt.CursorShape.PointingHandCursor)
        discard.setToolTip("Cerrar sin guardar (Esc)")
        discard.clicked.connect(lambda: QApplication.instance().exit(1))
        layout.addWidget(discard)

        self.save_btn = QPushButton("Guardar cambios")
        self.save_btn.setObjectName("primary")
        self.save_btn.setCursor(Qt.CursorShape.PointingHandCursor)
        self.save_btn.setToolTip("Guardar y cerrar (Ctrl+S)")
        self.save_btn.setDefault(True)
        self.save_btn.clicked.connect(self._save_and_close)
        layout.addWidget(self.save_btn)
        return footer

    def _build_cerebro(self):
        page = QWidget()
        layout = QVBoxLayout(page)
        layout.setContentsMargins(28, 24, 28, 20)
        layout.setSpacing(14)

        layout.addWidget(self._page_title("Cerebro conversacional"))
        layout.addWidget(
            self._hint("Elige de dónde salen las respuestas de Jarvis.")
        )

        mode = str(self.current.get("brain_mode") or "cli").lower()
        self.mode_group = QButtonGroup(self)
        self.mode_group.setExclusive(True)
        cards = QHBoxLayout()
        cards.setSpacing(12)
        for key, title, subtitle in (
            ("cli", "CLI local", "Sin API key · Antigravity"),
            ("api", "API", "Anthropic Messages"),
        ):
            card = QFrame()
            card.setObjectName("optionCard")
            card.setProperty("mode", key)
            card.setCursor(Qt.CursorShape.PointingHandCursor)
            card_layout = QVBoxLayout(card)
            card_layout.setContentsMargins(16, 14, 16, 14)
            card_layout.setSpacing(4)
            t = QLabel(title)
            t.setObjectName("optionTitle")
            s = QLabel(subtitle)
            s.setObjectName("hint")
            s.setWordWrap(True)
            card_layout.addWidget(t)
            card_layout.addWidget(s)
            button = QPushButton()
            button.setObjectName("optionRadio")
            button.setCheckable(True)
            button.setChecked(key == mode)
            button.setProperty("mode", key)
            self.mode_group.addButton(button)
            card_layout.addWidget(button)
            cards.addWidget(card, 1)
        layout.addLayout(cards)

        layout.addSpacing(6)
        layout.addWidget(self._label("Comando CLI"))
        self.cli_edit = QLineEdit()
        self.cli_edit.setObjectName("field")
        self.cli_edit.setText(
            str(self.current.get("cli_command") or DEFAULTS["cli_command"])
        )
        self.cli_edit.setPlaceholderText(DEFAULTS["cli_command"])
        layout.addWidget(self.cli_edit)

        test_row = QHBoxLayout()
        test_row.addWidget(
            self._hint("El prompt se agrega como último argumento (o en {prompt}).")
        )
        test_row.addStretch(1)
        self.test_btn = QPushButton("Probar")
        self.test_btn.setObjectName("secondary")
        self.test_btn.setCursor(Qt.CursorShape.PointingHandCursor)
        self.test_btn.clicked.connect(self._test_cli)
        test_row.addWidget(self.test_btn)
        layout.addLayout(test_row)

        self.test_output = QLabel("")
        self.test_output.setObjectName("testOutput")
        self.test_output.setWordWrap(True)
        layout.addWidget(self.test_output)

        layout.addWidget(self._label("API (modo API)"))
        api_row = QHBoxLayout()
        api_row.setSpacing(10)
        self.api_url_edit = QLineEdit()
        self.api_url_edit.setObjectName("field")
        self.api_url_edit.setText(str(self.current.get("api_base_url") or ""))
        self.api_url_edit.setPlaceholderText("URL base (vacío = api.anthropic.com)")
        api_row.addWidget(self.api_url_edit, 3)
        self.api_model_edit = QLineEdit()
        self.api_model_edit.setObjectName("field")
        self.api_model_edit.setText(str(self.current.get("api_model") or ""))
        self.api_model_edit.setPlaceholderText("Modelo (vacío = por defecto)")
        api_row.addWidget(self.api_model_edit, 2)
        layout.addLayout(api_row)

        layout.addStretch(1)
        return page

    def _build_voz(self):
        page = QWidget()
        layout = QVBoxLayout(page)
        layout.setContentsMargins(28, 24, 28, 20)
        layout.setSpacing(14)

        layout.addWidget(self._page_title("Voz"))
        layout.addWidget(
            self._hint("Voces neuronales de Microsoft Edge TTS.")
        )

        self.voice_search = QLineEdit()
        self.voice_search.setObjectName("field")
        self.voice_search.setPlaceholderText("Buscar voz…")
        self.voice_search.textChanged.connect(self._refresh_voices)
        layout.addWidget(self.voice_search)

        chips = QHBoxLayout()
        chips.setSpacing(8)
        self.region_group = QButtonGroup(self)
        self.region_group.setExclusive(True)
        self.region_buttons = {}
        for index, (label, _predicate) in enumerate(REGION_FILTERS):
            chip = QPushButton(label)
            chip.setObjectName("chip")
            chip.setCheckable(True)
            chip.setChecked(index == 0)
            chip.setCursor(Qt.CursorShape.PointingHandCursor)
            self.region_group.addButton(chip, index)
            self.region_buttons[index] = chip
            chips.addWidget(chip)
        chips.addStretch(1)
        layout.addLayout(chips)
        self.region_group.idClicked.connect(self._region_changed)

        scroll = QScrollArea()
        scroll.setWidgetResizable(True)
        scroll.setObjectName("voiceScroll")
        self.voice_grid_host = QWidget()
        self.voice_grid = QGridLayout(self.voice_grid_host)
        self.voice_grid.setContentsMargins(0, 0, 8, 0)
        self.voice_grid.setSpacing(10)
        scroll.setWidget(self.voice_grid_host)
        layout.addWidget(scroll, 1)

        footer = QHBoxLayout()
        self.voice_selection = QLabel(
            f"Seleccionada: {self.selected_voice}"
        )
        self.voice_selection.setObjectName("hint")
        footer.addWidget(self.voice_selection)
        footer.addStretch(1)
        use_btn = QPushButton("Usar esta voz")
        use_btn.setObjectName("secondary")
        use_btn.setCursor(Qt.CursorShape.PointingHandCursor)
        use_btn.clicked.connect(self._use_selected_voice)
        footer.addWidget(use_btn)
        layout.addLayout(footer)

        self._refresh_voices()
        return page

    def _build_escucha(self):
        page = QWidget()
        layout = QVBoxLayout(page)
        layout.setContentsMargins(28, 24, 28, 20)
        layout.setSpacing(14)

        layout.addWidget(self._page_title("Escucha"))
        layout.addWidget(
            self._hint("Cuándo y cómo Jarvis presta atención al micrófono.")
        )

        self.continuous_card, self.continuous_check = self._check_card(
            "Escucha continua",
            "Acepta cualquier frase directamente, sin exigir «Jarvis».",
            bool(self.current.get("continuous_listening")),
        )
        layout.addWidget(self.continuous_card)

        self.barge_card, self.barge_check = self._check_card(
            "Interrupción por voz (Barge-in)",
            "Desactívala si usas altavoces sin audífonos: evita que el eco lo corte.",
            bool(self.current.get("barge_in_enabled")),
        )
        layout.addWidget(self.barge_card)

        layout.addStretch(1)
        return page

    def _build_general(self):
        page = QWidget()
        layout = QVBoxLayout(page)
        # Más compacta que las demás: comparte el alto con la barra inferior.
        layout.setContentsMargins(28, 20, 28, 12)
        layout.setSpacing(10)

        layout.addWidget(self._page_title("General"))

        layout.addWidget(self._label("Esfera"))
        scale = clamp_pct(self.current.get("sphere_scale_pct"), SPHERE_SCALE_RANGE)
        self.scale_card, self.scale_slider = self._slider_card(
            "Tamaño de la esfera",
            "Ajusta el tamaño según tu monitor. Se ve en vivo.",
            SPHERE_SCALE_RANGE,
            scale,
            "sphere_scale_pct",
        )
        layout.addWidget(self.scale_card)
        opacity = clamp_pct(
            self.current.get("sphere_opacity_pct"), SPHERE_OPACITY_RANGE
        )
        self.opacity_card, self.opacity_slider = self._slider_card(
            "Opacidad",
            "Hazla translúcida para ver lo que hay detrás (mínimo 20 %).",
            SPHERE_OPACITY_RANGE,
            opacity,
            "sphere_opacity_pct",
        )
        layout.addWidget(self.opacity_card)

        self.fullscreen_card, self.fullscreen_combo = self._fullscreen_card(
            self.current.get("fullscreen_behavior")
        )
        layout.addWidget(self.fullscreen_card)

        self.hud_card, self.hud_check = self._check_card(
            "Mostrar burbujas de conversación (HUD)",
            "Globos translúcidos con lo que dijiste y lo que responde Jarvis;"
            " se desvanecen solos.",
            bool(self.current.get("hud_enabled", True)),
        )
        layout.addWidget(self.hud_card)

        reset = QPushButton("Volver a posición inicial")
        reset.setObjectName("secondary")
        reset.setCursor(Qt.CursorShape.PointingHandCursor)
        reset.clicked.connect(self._reset_position)
        layout.addWidget(reset)
        layout.addWidget(
            self._hint(
                "Devuelve la esfera a su esquina original y guarda los cambios"
                " de todas las pestañas."
            )
        )

        layout.addStretch(1)
        return page

    # ------------------------------------------------------------ helpers
    def _page_title(self, text):
        label = QLabel(text)
        label.setObjectName("pageTitle")
        return label

    def _label(self, text):
        label = QLabel(text)
        label.setObjectName("label")
        return label

    def _hint(self, text):
        label = QLabel(text)
        label.setObjectName("hint")
        label.setWordWrap(True)
        return label

    def _slider_card(self, title, hint, bounds, value, key):
        low, high, step = bounds
        card = QFrame()
        card.setObjectName("card")
        layout = QVBoxLayout(card)
        layout.setContentsMargins(16, 14, 16, 14)
        layout.setSpacing(8)

        header = QHBoxLayout()
        t = QLabel(title)
        t.setObjectName("label")
        header.addWidget(t)
        header.addStretch(1)
        value_label = QLabel(f"{value} %")
        value_label.setObjectName("sliderValue")
        header.addWidget(value_label)
        layout.addLayout(header)

        slider = QSlider(Qt.Orientation.Horizontal)
        slider.setObjectName("slider")
        slider.setRange(low, high)
        slider.setSingleStep(step)
        slider.setPageStep(step * 2)
        slider.setValue(value)
        slider.setCursor(Qt.CursorShape.PointingHandCursor)
        layout.addWidget(slider)

        h = QLabel(hint)
        h.setObjectName("hint")
        h.setWordWrap(True)
        layout.addWidget(h)

        throttle = PreviewThrottle(key, self)

        def changed(raw):
            snapped = low + round((raw - low) / step) * step
            snapped = max(low, min(high, snapped))
            if snapped != raw:
                # Reentra con el valor alineado al paso.
                slider.setValue(snapped)
                return
            value_label.setText(f"{snapped} %")
            throttle.push(snapped)

        slider.valueChanged.connect(changed)
        slider.sliderReleased.connect(throttle.flush)
        return card, slider

    def _fullscreen_card(self, value):
        card = QFrame()
        card.setObjectName("card")
        layout = QHBoxLayout(card)
        layout.setContentsMargins(16, 14, 16, 14)
        layout.setSpacing(12)

        text = QVBoxLayout()
        text.setSpacing(3)
        t = QLabel("Pantalla completa")
        t.setObjectName("label")
        h = QLabel(
            "Al ver un video, jugar o presentar. La esfera sigue escuchando"
            " y reaparece mientras le hablas."
        )
        h.setObjectName("hint")
        h.setWordWrap(True)
        text.addWidget(t)
        text.addWidget(h)
        layout.addLayout(text, 1)

        combo = QComboBox()
        combo.setObjectName("field")
        combo.setCursor(Qt.CursorShape.PointingHandCursor)
        for key, label in FULLSCREEN_BEHAVIORS:
            combo.addItem(label, key)
        keys = [key for key, _label in FULLSCREEN_BEHAVIORS]
        combo.setCurrentIndex(keys.index(value) if value in keys else 0)
        layout.addWidget(combo)
        return card, combo

    def _check_card(self, title, hint, checked):
        card = QFrame()
        card.setObjectName("card")
        layout = QHBoxLayout(card)
        layout.setContentsMargins(16, 14, 16, 14)
        layout.setSpacing(12)

        text = QVBoxLayout()
        text.setSpacing(3)
        t = QLabel(title)
        t.setObjectName("label")
        h = QLabel(hint)
        h.setObjectName("hint")
        h.setWordWrap(True)
        text.addWidget(t)
        text.addWidget(h)
        layout.addLayout(text, 1)

        checkbox = QCheckBox()
        checkbox.setObjectName("switch")
        checkbox.setChecked(checked)
        checkbox.setCursor(Qt.CursorShape.PointingHandCursor)
        layout.addWidget(checkbox)
        return card, checkbox

    def _voice_card(self, voice):
        voice_id, name, gender, lang, region, flag = voice
        card = QFrame()
        card.setObjectName("voiceCard")
        layout = QHBoxLayout(card)
        layout.setContentsMargins(10, 10, 10, 10)
        layout.setSpacing(8)

        select = QPushButton(f"{flag}  {name}\n{gender} · {region}")
        select.setObjectName("voiceSelect")
        select.setCheckable(True)
        select.setChecked(voice_id == self.selected_voice)
        select.setCursor(Qt.CursorShape.PointingHandCursor)
        select.clicked.connect(lambda _=False, vid=voice_id: self._select_voice(vid))
        layout.addWidget(select, 1)

        play = QPushButton("▶")
        play.setObjectName("play")
        play.setFixedSize(32, 32)
        play.setCursor(Qt.CursorShape.PointingHandCursor)
        play.setToolTip("Preescuchar")
        play.clicked.connect(lambda _=False, vid=voice_id: preview_voice(vid))
        layout.addWidget(play)
        return card

    def _select_voice(self, voice_id):
        self.selected_voice = voice_id
        self.voice_selection.setText(f"Seleccionada: {voice_id}")
        self._refresh_voices()
        self._update_status()

    def _use_selected_voice(self):
        self._save_and_close()

    def _refresh_voices(self):
        # Limpia el grid conservando el host.
        while self.voice_grid.count():
            item = self.voice_grid.takeAt(0)
            widget = item.widget()
            if widget is not None:
                widget.deleteLater()

        _label, predicate = REGION_FILTERS[self.region_index]
        query = self.voice_search.text().strip().lower()
        voices = [v for v in VOICES if predicate(v)]
        if query:
            voices = [
                v
                for v in voices
                if query in v[0].lower()
                or query in v[1].lower()
                or query in v[4].lower()
            ]

        for index, voice in enumerate(voices):
            row, col = divmod(index, 2)
            self.voice_grid.addWidget(self._voice_card(voice), row, col)

    def _region_changed(self, index):
        if 0 <= index < len(REGION_FILTERS):
            self.region_index = index
            self._refresh_voices()

    def _on_nav_clicked(self, _button_id):
        for index, key in enumerate(TABS):
            if self.nav_buttons[key].isChecked():
                self.stack.setCurrentIndex(index)
                break

    def _test_cli(self):
        command = self.cli_edit.text().strip() or DEFAULTS["cli_command"]
        try:
            argv = shlex.split(command)
        except ValueError as exc:
            self.test_output.setText(f"Comando inválido: {exc}")
            return
        argv.append("responde OK")
        started = time.monotonic()
        try:
            result = subprocess.run(
                argv,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                timeout=20,
            )
            elapsed_ms = int((time.monotonic() - started) * 1000)
            output = (result.stdout or "").strip()
            if len(output) > 240:
                output = output[:240] + "…"
            self.test_output.setText(
                f"Tiempo: {elapsed_ms} ms · salida: {output or '(vacía)'}"
            )
        except subprocess.TimeoutExpired:
            self.test_output.setText("El comando excedió 20 s (timeout).")
        except OSError as exc:
            self.test_output.setText(f"No se pudo ejecutar: {exc}")

    def _collect(self):
        """Valores actuales del formulario, con las mismas claves del TOML."""
        mode = "cli"
        for button in self.mode_group.buttons():
            if button.isChecked():
                mode = button.property("mode") or "cli"
        voice = next(
            (v for v in VOICES if v[0] == self.selected_voice), VOICES[0]
        )
        return {
            "brain_mode": mode,
            "cli_command": self.cli_edit.text().strip() or DEFAULTS["cli_command"],
            "api_base_url": self.api_url_edit.text().strip(),
            "api_model": self.api_model_edit.text().strip(),
            "continuous_listening": bool(self.continuous_check.isChecked()),
            "barge_in_enabled": bool(self.barge_check.isChecked()),
            "voice_id": voice[0],
            "voice_lang": voice[3],
            "sphere_scale_pct": int(self.scale_slider.value()),
            "sphere_opacity_pct": int(self.opacity_slider.value()),
            "fullscreen_behavior": self.fullscreen_combo.currentData()
            or DEFAULTS["fullscreen_behavior"],
            "hud_enabled": bool(self.hud_check.isChecked()),
        }

    def _is_dirty(self):
        return self._collect() != self.initial

    def _watch_changes(self):
        for edit in (self.cli_edit, self.api_url_edit, self.api_model_edit):
            edit.textChanged.connect(self._update_status)
        for check in (self.continuous_check, self.barge_check, self.hud_check):
            check.toggled.connect(self._update_status)
        for slider in (self.scale_slider, self.opacity_slider):
            slider.valueChanged.connect(self._update_status)
        self.mode_group.buttonToggled.connect(self._update_status)
        self.fullscreen_combo.currentIndexChanged.connect(self._update_status)

    def _update_status(self, *_args):
        self.status_label.setProperty("error", False)
        self.status_label.setText("● Cambios sin guardar" if self._is_dirty() else "")
        self.status_label.style().polish(self.status_label)

    def _save(self):
        """Escribe el TOML. Devuelve False (y lo avisa) si no se pudo."""
        self.data.update(self._collect())
        try:
            save_config(self.config_path, self.data)
            return True
        except OSError as exc:
            self.status_label.setProperty("error", True)
            self.status_label.setText(f"No se pudo guardar: {exc}")
            self.status_label.style().polish(self.status_label)
            return False

    def _save_and_close(self):
        if self._save():
            QApplication.instance().exit(0)

    def _close(self):
        # La ✕ no descarta trabajo: guarda si hubo cambios.
        if self._is_dirty():
            self._save_and_close()
        else:
            QApplication.instance().exit(1)

    def _reset_position(self):
        if self._save():
            QApplication.instance().exit(EXIT_RESET_POSITION)

    def keyPressEvent(self, event):
        if event.key() == Qt.Key.Key_Escape:
            QApplication.instance().exit(1)
            return
        super().keyPressEvent(event)

    def _stylesheet(self):
        return f"""
        QFrame#window {{ background: {BG}; border: 1px solid {BORDER}; border-radius: 18px; }}
        QFrame#sidebar {{ background: {SIDEBAR}; border: none;
                          border-top-left-radius: 18px; border-bottom-left-radius: 18px; }}
        QWidget#content {{ background: {BG}; }}
        QFrame#footer {{ background: {BG}; border: none; border-top: 1px solid {BORDER};
                         border-bottom-right-radius: 18px; }}
        QLabel#status {{ color: {MUTED}; font-size: 12px; }}
        QLabel#status[error="true"] {{ color: {DANGER}; }}
        QLabel {{ color: {TEXT}; font-size: 13px; }}
        QLabel#sideTitle {{ color: {ACCENT}; font-size: 12px; font-weight: 700; letter-spacing: 3px; }}
        QLabel#pageTitle {{ color: {TEXT}; font-size: 20px; font-weight: 700; }}
        QLabel#label {{ color: {TEXT}; font-size: 13px; font-weight: 600; }}
        QLabel#hint {{ color: {MUTED}; font-size: 12px; }}
        QLabel#sliderValue {{ color: {ACCENT}; font-size: 13px; font-weight: 700; }}
        QSlider#slider::groove:horizontal {{ height: 6px; border-radius: 3px; background: {SWITCH_OFF}; }}
        QSlider#slider::sub-page:horizontal {{ height: 6px; border-radius: 3px; background: {ACCENT}; }}
        QSlider#slider::handle:horizontal {{ width: 18px; height: 18px; margin: -6px 0;
                                             border-radius: 9px; background: {TEXT}; }}
        QLabel#testOutput {{ color: {ACCENT}; font-size: 12px; }}
        QLabel#optionTitle {{ color: {TEXT}; font-size: 14px; font-weight: 700; }}
        QPushButton {{ color: {TEXT}; background: {CARD}; border: 1px solid {BORDER};
                       border-radius: 9px; padding: 8px 14px; font-size: 13px; }}
        QPushButton:hover {{ border-color: {ACCENT}; }}
        QPushButton#nav {{ background: transparent; border: none; border-radius: 9px;
                           text-align: left; padding: 11px 14px; font-size: 14px; }}
        QPushButton#nav:checked {{ background: {CARD}; color: {ACCENT}; font-weight: 700; }}
        QPushButton#primary {{ background: {ACCENT}; color: #0B0F14; border: none;
                               font-weight: 700; }}
        QPushButton#primary:hover {{ background: {ACCENT_DARK}; }}
        QPushButton#secondary {{ background: {CARD}; color: {TEXT}; }}
        QPushButton#chip {{ background: {CARD}; color: {MUTED}; border: 1px solid {BORDER};
                            border-radius: 15px; padding: 5px 14px; font-size: 12px; }}
        QPushButton#chip:checked {{ background: {ACCENT}; color: #0B0F14; border: none; }}
        QPushButton#play {{ background: {ACCENT}; color: #0B0F14; border: none;
                            border-radius: 16px; font-size: 11px; }}
        QPushButton#optionRadio {{ background: {FIELD}; border: 2px solid {BORDER};
                                   border-radius: 9px; min-height: 0; }}
        QPushButton#optionRadio:checked {{ background: {ACCENT}; border: 2px solid {ACCENT}; }}
        QPushButton#voiceSelect {{ background: transparent; border: none; text-align: left;
                                   font-size: 12px; color: {TEXT}; }}
        QPushButton#voiceSelect:checked {{ color: {ACCENT}; font-weight: 700; }}
        QFrame#card, QFrame#optionCard, QFrame#voiceCard {{
            background: {CARD}; border: 1px solid {BORDER}; border-radius: 12px;
        }}
        QLineEdit#field {{ background: {FIELD}; color: {TEXT}; border: 1px solid {BORDER};
                           border-radius: 9px; padding: 9px 12px; font-size: 13px; }}
        QComboBox#field {{ background: {FIELD}; color: {TEXT}; border: 1px solid {BORDER};
                           border-radius: 9px; padding: 7px 12px; font-size: 13px; }}
        QComboBox#field QAbstractItemView {{ background: {CARD}; color: {TEXT};
                                             selection-background-color: {ACCENT};
                                             selection-color: #0B0F14; }}
        QCheckBox#switch {{ color: {TEXT}; font-size: 13px; spacing: 10px; }}
        QCheckBox#switch::indicator {{
            width: 40px; height: 22px; border-radius: 11px; background: {SWITCH_OFF};
        }}
        QCheckBox#switch::indicator:checked {{ background: {ACCENT}; }}
        QScrollArea#voiceScroll {{ border: none; background: {BG}; }}
        QPushButton#closeBtn {{ background: transparent; border: none; color: {MUTED};
                                font-size: 16px; padding: 0; }}
        QPushButton#closeBtn:hover {{ color: {DANGER}; border: none; }}
        """


def main():
    args = sys.argv[1:]
    if not args:
        print("uso: unified_settings <config.toml> [--pid <pid>] [--tab <tab>]", file=sys.stderr)
        return 2

    config_path = args[0]
    pid = None
    tab = "cerebro"
    if "--pid" in args:
        idx = args.index("--pid")
        if idx + 1 < len(args):
            pid = args[idx + 1]
    if "--tab" in args:
        idx = args.index("--tab")
        if idx + 1 < len(args):
            tab = args[idx + 1].lower()
    if tab not in TABS:
        tab = "cerebro"

    try:
        app = QApplication(sys.argv)
    except Exception as exc:
        print(f"sin pantalla para los ajustes: {exc}", file=sys.stderr)
        return 2

    window = UnifiedSettings(config_path, pid, tab)
    # Abre directamente en la pestaña pedida.
    for index, key in enumerate(TABS):
        if key == tab:
            window.nav_buttons[key].setChecked(True)
            window.stack.setCurrentIndex(index)
            break
    window.show()
    return app.exec()


if __name__ == "__main__":
    sys.exit(main())
