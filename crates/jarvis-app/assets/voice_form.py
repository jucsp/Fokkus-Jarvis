"""Selector de voces de Fokkus-Jarvis (menú de la esfera -> CAMBIAR VOZ).

Uso: python3 -I -c <script> <config.toml> <binario fokkus-jarvis>

Muestra las voces de Microsoft Edge TTS por categoría (Latinoamérica, España
e Inglés), permite preescuchar cada voz con `edge-tts` y guarda la selección
(`voice_id` y `voice_lang`) en el TOML de configuración, conservando el resto
de claves. El runtime aplica el cambio en caliente al detectar el guardado.

Código de salida: 0 = guardado, 1 = cancelado, 2 = error.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import tomllib
import tkinter as tk
from tkinter import messagebox

BG = "#10101C"
CARD = "#181826"
FIELD = "#202030"
BORDER = "#333340"
TEXT = "#D6D6E2"
MUTED = "#9A9AAE"
ACCENT = "#37ECBA"
DANGER = "#F87171"
FONT = ("Inter", 10)
FONT_BOLD = ("Inter", 10, "bold")
FONT_TITLE = ("Inter", 13, "bold")

PREVIEW_TEXT = "Hola, soy Jarvis, tu asistente de escritorio."

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

CATEGORIES = [
    ("Latinoamérica", lambda v: v[3].startswith("es-") and v[3] != "es-ES"),
    ("España", lambda v: v[3] == "es-ES"),
    ("Inglés", lambda v: v[3].startswith("en-")),
]


def load(path):
    try:
        with open(path, "rb") as f:
            return tomllib.load(f)
    except FileNotFoundError:
        return {}


def toml_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return repr(value)
    return json.dumps(str(value), ensure_ascii=False)


def save(path, data):
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
    messagebox.showinfo(
        "Jarvis",
        "No se encontró un reproductor de audio del sistema para la preescucha.",
    )


class VoiceForm:
    def __init__(self, root, path, binary):
        self.root = root
        self.path = path
        self.binary = binary
        self.data = load(path)
        current = dict(self.data)
        self.current_voice = str(current.get("voice_id") or "es-CL-LorenzoNeural")
        self.saved = False
        self.selected = tk.StringVar(value=self.current_voice)
        self.rows = {}

        root.title("Jarvis — Cambiar voz")
        root.configure(bg=BG)
        root.resizable(False, False)
        root.protocol("WM_DELETE_WINDOW", self.cancel)
        root.bind("<Escape>", lambda _e: self.cancel())

        outer = tk.Frame(root, bg=BG, padx=22, pady=18)
        outer.pack(fill="both", expand=True)
        tk.Label(
            outer, text="Cambiar voz", bg=BG, fg=TEXT, font=FONT_TITLE
        ).pack(anchor="w")
        tk.Label(
            outer,
            text="Voces neuronales de Microsoft Edge TTS",
            bg=BG,
            fg=MUTED,
            font=FONT,
        ).pack(anchor="w", pady=(0, 12))

        for category, predicate in CATEGORIES:
            voices = [v for v in VOICES if predicate(v)]
            if not voices:
                continue
            tk.Label(
                outer, text=category, bg=BG, fg=ACCENT, font=FONT_BOLD
            ).pack(anchor="w", pady=(8, 3))
            for voice in voices:
                self.voice_row(outer, voice)

        actions = tk.Frame(outer, bg=BG)
        actions.pack(fill="x", pady=(16, 0))
        self.button(actions, "Guardar", self.submit, primary=True).pack(side="right")
        self.button(actions, "Cancelar", self.cancel).pack(side="right", padx=(0, 8))
        root.bind("<Return>", lambda _e: self.submit())

    def voice_row(self, parent, voice):
        voice_id, name, gender, lang, region, flag = voice
        row = tk.Frame(parent, bg=CARD, highlightbackground=BORDER, highlightthickness=1)
        row.pack(fill="x", pady=2, padx=2)
        tk.Radiobutton(
            row,
            text=f"{flag}  {name} · {gender} · {region}",
            value=voice_id,
            variable=self.selected,
            bg=CARD,
            fg=TEXT,
            selectcolor=FIELD,
            activebackground=FIELD,
            activeforeground=ACCENT,
            font=FONT,
            bd=0,
            highlightthickness=0,
            anchor="w",
            cursor="hand2",
        ).pack(side="left", fill="x", expand=True, padx=8, pady=4)
        self.button(row, "Probar", lambda vid=voice_id: self.preview(vid)).pack(
            side="right", padx=6, pady=4
        )
        self.button(row, "Guardar", lambda vid=voice_id: self.choose(vid), primary=True).pack(
            side="right", pady=4
        )

    def button(self, parent, text, command, primary=False):
        return tk.Button(
            parent,
            text=text,
            command=command,
            font=FONT_BOLD,
            bg=ACCENT if primary else CARD,
            fg=BG if primary else TEXT,
            activebackground="#2BD3A5" if primary else FIELD,
            activeforeground=BG if primary else TEXT,
            relief="flat",
            bd=0,
            padx=12,
            pady=4,
            cursor="hand2",
        )

    def preview(self, voice_id):
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
                stderr=subprocess.PIPE,
                text=True,
                timeout=30,
            )
            if result.returncode != 0:
                detail = (result.stderr or "").strip()
                messagebox.showerror("Jarvis", f"No se pudo sintetizar la voz:\n{detail}")
                return
            play_file(tmp.name)
        except (OSError, subprocess.TimeoutExpired) as e:
            messagebox.showerror("Jarvis", f"No se pudo preescuchar la voz: {e}")
        finally:
            try:
                os.unlink(tmp.name)
            except OSError:
                pass

    def choose(self, voice_id):
        self.selected.set(voice_id)
        self.submit()

    def submit(self):
        voice_id = self.selected.get() or "es-CL-LorenzoNeural"
        lang = next((v[3] for v in VOICES if v[0] == voice_id), "es-CL")
        self.data.update({"voice_id": voice_id, "voice_lang": lang})
        try:
            save(self.path, self.data)
        except OSError as e:
            messagebox.showerror("Jarvis", f"No se pudo guardar {self.path}: {e}")
            return
        self.saved = True
        self.root.destroy()

    def cancel(self):
        self.root.destroy()


def main():
    if len(sys.argv) < 2:
        print("uso: voice_form <config.toml>", file=sys.stderr)
        return 2
    try:
        root = tk.Tk()
    except tk.TclError as e:
        print(f"sin pantalla para el selector: {e}", file=sys.stderr)
        return 2
    form = VoiceForm(root, sys.argv[1], None)
    root.update_idletasks()
    width, height = root.winfo_reqwidth(), root.winfo_reqheight()
    x = (root.winfo_screenwidth() - width) // 2
    y = (root.winfo_screenheight() - height) // 3
    root.geometry(f"+{x}+{y}")
    root.lift()
    root.focus_force()
    root.mainloop()
    return 0 if form.saved else 1


if __name__ == "__main__":
    sys.exit(main())
