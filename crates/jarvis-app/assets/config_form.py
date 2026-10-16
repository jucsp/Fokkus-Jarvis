"""Formulario de configuración de Fokkus-Jarvis (menú de la esfera).

Uso: python3 -I -c <script> <config.toml> <binario fokkus-jarvis>

Lee y guarda el TOML de configuración conservando las claves que no edita.
La API key nunca se escribe en el TOML: se entrega por stdin a
`fokkus-jarvis --set-api-key`, que la guarda en el llavero del sistema.

Código de salida: 0 = guardado, 1 = cancelado, 2 = error.
"""

import json
import os
import subprocess
import sys
import tkinter as tk
import tomllib
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

DEFAULTS = {
    "brain_mode": "cli",
    "cli_command": "agy --effort low --dangerously-skip-permissions -p",
    "api_base_url": "",
    "api_model": "",
    "continuous_listening": False,
    "barge_in_enabled": False,
}


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
    # Un string JSON es un basic string TOML válido.
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


class PlaceholderEntry(tk.Entry):
    """Entry con texto de ayuda gris mientras está vacío."""

    def __init__(self, master, placeholder, show=None, **kw):
        super().__init__(master, **kw)
        self.placeholder = placeholder
        self.secret = show
        self.showing = False
        self.bind("<FocusIn>", self._clear)
        self.bind("<FocusOut>", self._restore)
        self._restore()

    def _clear(self, _event=None):
        if self.showing:
            self.delete(0, tk.END)
            self.config(fg=TEXT, show=self.secret or "")
            self.showing = False

    def _restore(self, _event=None):
        if not self.get() and self.placeholder:
            self.config(fg=MUTED, show="")
            self.insert(0, self.placeholder)
            self.showing = True

    def set(self, value):
        self._clear()
        self.delete(0, tk.END)
        if value:
            self.insert(0, value)
        else:
            self._restore()

    def value(self):
        return "" if self.showing else self.get().strip()


class Form:
    def __init__(self, root, path, binary):
        self.root = root
        self.path = path
        self.binary = binary
        self.data = load(path)
        self.saved = False
        current = {**DEFAULTS, **self.data}

        root.title("Jarvis — Configuración")
        root.configure(bg=BG)
        root.resizable(False, False)
        root.protocol("WM_DELETE_WINDOW", self.cancel)
        root.bind("<Escape>", lambda _e: self.cancel())

        outer = tk.Frame(root, bg=BG, padx=22, pady=18)
        outer.pack(fill="both", expand=True)
        tk.Label(
            outer, text="Configuración de Jarvis", bg=BG, fg=TEXT, font=FONT_TITLE
        ).pack(anchor="w")
        tk.Label(
            outer,
            text="Cerebro conversacional e interrupción por voz",
            bg=BG,
            fg=MUTED,
            font=FONT,
        ).pack(anchor="w", pady=(0, 14))

        # Selector de modo [CLI | API].
        self.mode = tk.StringVar(value=str(current["brain_mode"]).lower())
        modes = tk.Frame(outer, bg=CARD, highlightbackground=BORDER, highlightthickness=1)
        modes.pack(fill="x")
        self.mode_buttons = {}
        for key, label in (("cli", "CLI"), ("api", "API")):
            button = tk.Radiobutton(
                modes,
                text=label,
                value=key,
                variable=self.mode,
                indicatoron=False,
                command=self.refresh,
                font=FONT_BOLD,
                bg=CARD,
                fg=MUTED,
                selectcolor=FIELD,
                activebackground=FIELD,
                activeforeground=ACCENT,
                relief="flat",
                bd=0,
                padx=10,
                pady=7,
                cursor="hand2",
            )
            button.pack(side="left", fill="x", expand=True)
            self.mode_buttons[key] = button

        self.body = tk.Frame(outer, bg=BG)
        self.body.pack(fill="x", pady=(14, 0))

        self.cli_frame = tk.Frame(self.body, bg=BG)
        self.cli_command = self.field(self.cli_frame, "Comando CLI", "agy -p")
        self.cli_command.set(current["cli_command"])
        tk.Label(
            self.cli_frame,
            text="El prompt se agrega como último argumento (o en {prompt}).",
            bg=BG,
            fg=MUTED,
            font=("Inter", 9),
        ).pack(anchor="w")

        self.api_frame = tk.Frame(self.body, bg=BG)
        self.api_key = self.field(
            self.api_frame, "API Key", "vacío = mantener la guardada", show="•"
        )
        self.base_url = self.field(
            self.api_frame, "Base URL", "https://api.anthropic.com"
        )
        self.base_url.set(current["api_base_url"])
        self.model = self.field(self.api_frame, "Modelo", "claude-opus-5-5")
        self.model.set(current["api_model"])

        self.continuous_listening = tk.BooleanVar(
            value=bool(current["continuous_listening"])
        )
        tk.Checkbutton(
            outer,
            text="Escucha continua (responder sin decir Jarvis)",
            variable=self.continuous_listening,
            bg=BG,
            fg=TEXT,
            selectcolor=FIELD,
            activebackground=BG,
            activeforeground=ACCENT,
            font=FONT,
            bd=0,
            highlightthickness=0,
            cursor="hand2",
        ).pack(anchor="w", pady=(12, 0))
        tk.Label(
            outer,
            text="Acepta cualquier frase directamente, sin exigir la palabra clave.",
            bg=BG,
            fg=MUTED,
            font=("Inter", 9),
        ).pack(anchor="w")

        self.barge_in = tk.BooleanVar(value=bool(current["barge_in_enabled"]))
        tk.Checkbutton(
            outer,
            text="Interrupción por voz (Barge-in)",
            variable=self.barge_in,
            bg=BG,
            fg=TEXT,
            selectcolor=FIELD,
            activebackground=BG,
            activeforeground=ACCENT,
            font=FONT,
            bd=0,
            highlightthickness=0,
            cursor="hand2",
        ).pack(anchor="w", pady=(12, 0))
        tk.Label(
            outer,
            text="Desactívala si usas altavoces sin audífonos (evita que el eco lo corte).",
            bg=BG,
            fg=MUTED,
            font=("Inter", 9),
        ).pack(anchor="w")

        actions = tk.Frame(outer, bg=BG)
        actions.pack(fill="x", pady=(18, 0))
        self.button(actions, "Guardar", self.submit, primary=True).pack(side="right")
        self.button(actions, "Cancelar", self.cancel).pack(side="right", padx=(0, 8))
        root.bind("<Return>", lambda _e: self.submit())

        self.refresh()

    def field(self, parent, label, placeholder, show=None):
        tk.Label(parent, text=label, bg=BG, fg=TEXT, font=FONT_BOLD).pack(
            anchor="w", pady=(0, 3)
        )
        entry = PlaceholderEntry(
            parent,
            placeholder,
            show=show,
            bg=FIELD,
            fg=TEXT,
            insertbackground=ACCENT,
            relief="flat",
            font=FONT,
            width=44,
            highlightthickness=1,
            highlightbackground=BORDER,
            highlightcolor=ACCENT,
        )
        entry.pack(fill="x", ipady=5, pady=(0, 8))
        return entry

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
            padx=16,
            pady=6,
            cursor="hand2",
        )

    def refresh(self):
        mode = self.mode.get()
        for key, button in self.mode_buttons.items():
            button.config(fg=ACCENT if key == mode else MUTED)
        self.cli_frame.pack_forget()
        self.api_frame.pack_forget()
        (self.api_frame if mode == "api" else self.cli_frame).pack(fill="x")

    def submit(self):
        mode = self.mode.get()
        command = self.cli_command.value() or DEFAULTS["cli_command"]
        api_key = self.api_key.value()
        if api_key:
            try:
                result = subprocess.run(
                    [self.binary, "--set-api-key"],
                    input=api_key + "\n",
                    text=True,
                    capture_output=True,
                    timeout=20,
                )
            except (OSError, subprocess.TimeoutExpired) as e:
                messagebox.showerror("Jarvis", f"No se pudo guardar la API key: {e}")
                return
            if result.returncode != 0:
                detail = (result.stderr or result.stdout).strip()
                messagebox.showerror("Jarvis", f"No se pudo guardar la API key:\n{detail}")
                return

        self.data.update(
            {
                "brain_mode": mode,
                "cli_command": command,
                "api_base_url": self.base_url.value(),
                "api_model": self.model.value(),
                "continuous_listening": bool(self.continuous_listening.get()),
                "barge_in_enabled": bool(self.barge_in.get()),
            }
        )
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
    if len(sys.argv) < 3:
        print("uso: config_form <config.toml> <fokkus-jarvis>", file=sys.stderr)
        return 2
    try:
        root = tk.Tk()
    except tk.TclError as e:
        print(f"sin pantalla para el formulario: {e}", file=sys.stderr)
        return 2
    form = Form(root, sys.argv[1], sys.argv[2])
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
