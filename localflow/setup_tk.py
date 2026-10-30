"""Tk fallback for the native LocalFlow setup wizard."""

import queue
import subprocess
import sys
import threading
import webbrowser
from pathlib import Path
import tkinter as tk
from tkinter import messagebox, ttk

from localflow.audio import list_input_devices
from localflow.config import load_config
from localflow.models import (
    LLM_MODELS,
    DownloadCancelled,
    OllamaClient,
    available_stt_models,
    detect_hardware,
    download_stt,
    is_stt_downloaded,
    recommend,
)
from localflow import setup_flow
from localflow.setup_flow import HOTKEY_OPTIONS, MicLevelMonitor, SetupChoices, initial_choices

TOKENS = {
    "paper": "#FFF8E7",
    "card": "#FFFFFF",
    "ink": "#111111",
    "muted": "#4A4A4A",
    "cobalt": "#1D4ED8",
    "sun": "#FFD60A",
    "red": "#E63946",
    "shadow": 4,
    "radius": 10,
    "space": (4, 8, 12, 16, 24, 32),
}

_STEP_TITLES = (
    "Let's get you talking.",
    "A quick setup check.",
    "Pick your speech model.",
    "Choose your cleanup model.",
    "Mic and hotkey.",
    "Give it a try.",
    "All set.",
)


class SetupWindow:
    def __init__(self, root: tk.Tk, config=None, hardware=None):
        self.root = root
        self.config = config or load_config()
        self.hardware = hardware or detect_hardware()
        self.choices = initial_choices(self.config, self.hardware)
        self.models = available_stt_models(self.hardware)
        self.stt_var = tk.StringVar(value=self.choices.stt_id)
        self.llm_var = tk.StringVar(value=self.choices.llm_id or "none")
        self.device_var = tk.StringVar(value=self.choices.input_device or "System default")
        self.hotkey_var = tk.StringVar(value=self._hotkey_label(self.choices.hotkey))
        self.language_var = tk.StringVar(value=self.choices.language or "")
        self.start_login_var = tk.BooleanVar(value=self.choices.start_on_login)
        self.step = 0
        self.saved = False
        self.checks = None
        self.downloaded: dict[str, bool] = {}
        self.ollama_running = False
        self.ollama_models: list[str] = []
        self._ollama_checked = False
        self.status_message = ""
        self.error_message = ""
        self._busy: set[str] = set()
        self._cancel: threading.Event | None = None
        self._events: queue.Queue = queue.Queue()
        self._monitor: MicLevelMonitor | None = None
        self._monitor_started = False
        self._test_transcriber = None
        self._progress_var = tk.StringVar(value="")

        root.title("LocalFlow Setup")
        root.geometry("720x560")
        root.minsize(680, 520)
        root.configure(background=TOKENS["paper"])
        root.protocol("WM_DELETE_WINDOW", self.close)
        self._build_shell()
        self._render()
        root.after(100, self._poll_events)

    def _hotkey_label(self, value: str) -> str:
        return next((label for option, label in HOTKEY_OPTIONS if option == value), HOTKEY_OPTIONS[0][1])

    def _hotkey_value(self, label: str) -> str:
        return next((option for option, text in HOTKEY_OPTIONS if text == label), "alt_l")

    def _build_shell(self) -> None:
        self.shell = tk.Frame(self.root, bg=TOKENS["paper"], padx=28, pady=18)
        self.shell.pack(fill="both", expand=True)
        self.brand = tk.Label(
            self.shell,
            text="LOCALFLOW   /   SETUP",
            bg=TOKENS["paper"],
            fg=TOKENS["muted"],
            font=("TkDefaultFont", 10, "bold"),
        )
        self.brand.pack(anchor="w")
        self.title_label = tk.Label(
            self.shell,
            bg=TOKENS["paper"],
            fg=TOKENS["ink"],
            font=("TkDefaultFont", 24, "bold"),
            anchor="w",
        )
        self.title_label.pack(fill="x", pady=(16, 2))
        self.subtitle_label = tk.Label(
            self.shell,
            bg=TOKENS["paper"],
            fg=TOKENS["muted"],
            font=("TkDefaultFont", 11),
            anchor="w",
        )
        self.subtitle_label.pack(fill="x", pady=(0, 10))
        self.body = tk.Frame(self.shell, bg=TOKENS["paper"])
        self.body.pack(fill="both", expand=True)
        self.footer = tk.Frame(self.shell, bg=TOKENS["paper"])
        self.footer.pack(fill="x", pady=(12, 0))

    def _clear(self, container) -> None:
        for child in container.winfo_children():
            child.destroy()

    def _render(self) -> None:
        self.title_label.configure(text=_STEP_TITLES[self.step])
        self.subtitle_label.configure(
            text=f"Step {self.step + 1} of 7  ·  {self.hardware.machine or self.hardware.system}"
            if self.step == 0 else f"Step {self.step + 1} of 7"
        )
        self._clear(self.body)
        self._clear(self.footer)
        getattr(self, f"_render_step_{self.step}")()
        self._render_footer()

    def _label(self, parent, text, *, font=None, fg=None, wrap=620, pady=(0, 8)):
        label = tk.Label(
            parent,
            text=text,
            bg=TOKENS["paper"],
            fg=fg or TOKENS["ink"],
            font=font or ("TkDefaultFont", 11),
            justify="left",
            anchor="w",
            wraplength=wrap,
        )
        label.pack(fill="x", pady=pady)
        return label

    def _card(self, parent, title, detail, selected=False, tag=""):
        shadow = tk.Frame(parent, bg=TOKENS["ink"], height=64)
        shadow.pack(fill="x", pady=(0, 8))
        color = TOKENS["sun"] if selected else TOKENS["card"]
        surface = tk.Frame(shadow, bg=color, highlightbackground=TOKENS["ink"], highlightthickness=2)
        surface.place(x=0, y=0, relwidth=1, relheight=1, width=-TOKENS["shadow"], height=-TOKENS["shadow"])
        top = tk.Frame(surface, bg=color)
        top.pack(fill="x", padx=12, pady=(8, 0))
        tk.Label(top, text=title, bg=color, fg=TOKENS["ink"], font=("TkDefaultFont", 12, "bold")).pack(side="left")
        if tag:
            tk.Label(top, text=tag, bg=color, fg=TOKENS["ink"], font=("TkDefaultFont", 9, "bold")).pack(side="right")
        tk.Label(
            surface, text=detail, bg=color, fg=TOKENS["muted"],
            font=("TkDefaultFont", 10), anchor="w",
        ).pack(fill="x", padx=12, pady=(2, 8))
        shadow.configure(height=64)
        shadow.pack_propagate(False)
        return shadow, surface

    def _scrolling_body(self):
        holder = tk.Frame(self.body, bg=TOKENS["paper"])
        holder.pack(fill="both", expand=True)
        canvas = tk.Canvas(holder, bg=TOKENS["paper"], highlightthickness=0)
        scroll = ttk.Scrollbar(holder, orient="vertical", command=canvas.yview)
        content = tk.Frame(canvas, bg=TOKENS["paper"])
        window = canvas.create_window((0, 0), window=content, anchor="nw")
        content.bind("<Configure>", lambda _e: canvas.configure(scrollregion=canvas.bbox("all")))
        canvas.bind("<Configure>", lambda event: canvas.itemconfigure(window, width=event.width))
        canvas.configure(yscrollcommand=scroll.set)
        canvas.pack(side="left", fill="both", expand=True)
        scroll.pack(side="right", fill="y")
        return content

    def _render_step_0(self) -> None:
        rec_stt, rec_llm = recommend(self.hardware)
        self._label(
            self.body,
            f"{self.hardware.ram_gb:.0f} GB memory  ·  {self.hardware.free_disk_gb:.0f} GB free disk"
            f"  ·  {'Apple Silicon' if self.hardware.apple_silicon else 'CPU'}",
            font=("TkDefaultFont", 12, "bold"),
        )
        self._card(
            self.body,
            "A local voice setup, ready in a few minutes",
            f"Suggested speech model: {rec_stt}   ·   Cleanup: {rec_llm or 'off'}",
            selected=True,
        )
        self._label(
            self.body,
            "Your audio and models stay on this machine. Pick what fits, download it, then try a short dictation.",
            pady=(8, 0),
        )

    def _render_step_1(self) -> None:
        self._label(self.body, "Nothing here blocks setup. Fix what you can; you can revisit any time.")
        if self.checks is None:
            self._label(self.body, "Checking this machine…", fg=TOKENS["muted"])
            self._start_checks()
            return
        if self.error_message or self.status_message:
            self._label(
                self.body, self.error_message or self.status_message,
                fg=TOKENS["red"] if self.error_message else TOKENS["muted"],
            )
        for check in self.checks:
            row = tk.Frame(self.body, bg=TOKENS["paper"])
            row.pack(fill="x", pady=3)
            badge = "✓" if check.ok else ("?" if check.ok is None else "✗")
            color = "#16803C" if check.ok else (TOKENS["muted"] if check.ok is None else TOKENS["red"])
            tk.Label(row, text=badge, fg=color, bg=TOKENS["paper"], font=("TkDefaultFont", 12, "bold")).pack(side="left")
            tk.Label(row, text=f"{check.label}  ·  {check.detail}", fg=TOKENS["ink"], bg=TOKENS["paper"], anchor="w").pack(side="left", padx=8)
            if check.fix_label:
                tk.Button(
                    row, text=check.fix_label, command=lambda item=check: self._fix_check(item),
                    bg=TOKENS["card"], fg=TOKENS["ink"], relief="solid", bd=1,
                ).pack(side="right")
        tk.Button(
            self.body, text="Recheck", command=self._recheck_checks,
            bg=TOKENS["card"], fg=TOKENS["ink"], relief="solid", bd=1,
        ).pack(anchor="w", pady=(6, 0))

    def _render_step_2(self) -> None:
        self._label(self.body, "Choose the speech model that turns your voice into text.")
        content = self._scrolling_body()
        rec_stt, _ = recommend(self.hardware)
        for model in self.models:
            downloaded = self.downloaded.get(model.id, False)
            tags = []
            if model.id == rec_stt:
                tags.append("RECOMMENDED")
            if downloaded:
                tags.append("DOWNLOADED")
            if model.english_only:
                tags.append("ENGLISH ONLY")
            detail = (
                f"{model.size_mb:,} MB   ·   Speed {'●' * model.speed}{'○' * (5 - model.speed)}"
                f"   Accuracy {'●' * model.accuracy}{'○' * (5 - model.accuracy)}"
            )
            if model.blurb:
                detail += f"   ·   {model.blurb}"
            card, surface = self._card(
                content, model.label, detail,
                selected=self.stt_var.get() == model.id, tag="  ".join(tags),
            )
            for widget in (card, surface, *surface.winfo_children()):
                widget.bind("<Button-1>", lambda _e, model_id=model.id: self._select_stt(model_id))
            for widget in surface.winfo_children():
                for child in widget.winfo_children():
                    child.bind("<Button-1>", lambda _e, model_id=model.id: self._select_stt(model_id))
        if self.step == 2 and not self.downloaded:
            self._start_download_checks()
        self._show_operation_state()

    def _render_step_3(self) -> None:
        if self.choices.llm_provider != "ollama":
            self._label(
                self.body,
                f"Configured provider: {self.choices.llm_provider}. Manage models with this provider in Settings.",
            )
            tk.Button(
                self.body,
                text="Change in Settings",
                command=self._open_settings,
            ).pack(anchor="w", pady=(12, 0))
            return
        self._label(self.body, "Cleanup fixes punctuation and filler words. No cleanup keeps the raw transcript.")
        content = self._scrolling_body()
        card, surface = self._card(
            content, "No cleanup (raw transcript)", "Fastest and smallest; no Ollama model needed.",
            selected=self.llm_var.get() == "none",
        )
        for widget in (card, surface, *surface.winfo_children()):
            widget.bind("<Button-1>", lambda _e: self._select_llm("none"))
        for model in LLM_MODELS:
            status = "DOWNLOADED" if model.id in self.ollama_models else ""
            card, surface = self._card(
                content,
                model.label,
                f"{model.size_mb:,} MB   ·   {model.min_ram_gb} GB RAM recommended   ·   {model.blurb}",
                selected=self.llm_var.get() == model.id,
                tag=status,
            )
            for widget in (card, surface, *surface.winfo_children()):
                widget.bind("<Button-1>", lambda _e, model_id=model.id: self._select_llm(model_id))
            for widget in surface.winfo_children():
                for child in widget.winfo_children():
                    child.bind("<Button-1>", lambda _e, model_id=model.id: self._select_llm(model_id))
        if self.ollama_running:
            self._label(content, "Ollama is ready.", fg="#16803C")
            tk.Button(content, text="Refresh models", command=self._recheck_ollama).pack(anchor="w")
        elif setup_flow.ollama_installed():
            self._label(content, "Start Ollama with `ollama serve` or open the Ollama app, then recheck.", fg=TOKENS["muted"])
            tk.Button(content, text="Recheck", command=self._start_ollama_check).pack(anchor="w")
        else:
            self._label(content, "Ollama is not installed. Cleanup is optional; install it when you want it.", fg=TOKENS["muted"])
            tk.Button(
                content, text="Get Ollama", command=lambda: webbrowser.open("https://ollama.com/download"),
            ).pack(anchor="w")
        if not self._ollama_checked:
            self._start_ollama_check()
        self._show_operation_state()

    def _render_step_4(self) -> None:
        self._label(self.body, "Pick a microphone and the key you'll hold to talk.")
        devices = ["System default", *list_input_devices()]
        if self.device_var.get() not in devices:
            self.device_var.set("System default")
        ttk.Label(self.body, text="Microphone").pack(anchor="w")
        self.device_box = ttk.Combobox(self.body, textvariable=self.device_var, values=devices, state="readonly")
        self.device_box.pack(fill="x", pady=(2, 12))
        self.device_box.bind("<<ComboboxSelected>>", self._change_device)
        ttk.Label(self.body, text="Hotkey").pack(anchor="w")
        self.hotkey_box = ttk.Combobox(
            self.body,
            textvariable=self.hotkey_var,
            values=[label for _key, label in HOTKEY_OPTIONS],
            state="readonly",
        )
        self.hotkey_box.pack(fill="x", pady=(2, 12))
        self._label(self.body, "Live mic level", font=("TkDefaultFont", 11, "bold"))
        self.meter = tk.Canvas(self.body, height=18, bg=TOKENS["card"], highlightbackground=TOKENS["ink"], highlightthickness=1)
        self.meter.pack(fill="x")
        self.meter_bar = self.meter.create_rectangle(0, 0, 0, 18, fill=TOKENS["red"], outline="")
        self.meter.bind("<Configure>", lambda _e: self._draw_meter())
        self._start_monitor()
        self._update_meter()

    def _render_step_5(self) -> None:
        self._label(self.body, "Record five seconds of speech. The selected speech model will transcribe it.")
        self.transcript = tk.Text(
            self.body, height=7, wrap="word", bg=TOKENS["card"], fg=TOKENS["ink"],
            relief="solid", bd=2, font=("TkDefaultFont", 12),
        )
        self.transcript.pack(fill="both", expand=True, pady=(8, 10))
        self.transcript.insert("1.0", self.status_message or "Your practice transcript will appear here.")
        self.transcript.configure(state="disabled")
        if self.error_message:
            self._label(self.body, self.error_message, fg=TOKENS["red"])

    def _render_step_6(self) -> None:
        self._label(
            self.body,
            f"Speech: {self.stt_var.get()}    ·    Cleanup: "
            f"{self.llm_var.get() if self.llm_var.get() != 'none' else 'off'}",
            font=("TkDefaultFont", 12, "bold"),
        )
        self._label(
            self.body,
            f"Microphone: {self.device_var.get()}    ·    Hotkey: {self.hotkey_var.get()}",
        )
        if self.hardware.system == "Darwin":
            tk.Checkbutton(
                self.body, text="Start LocalFlow on login", variable=self.start_login_var,
                bg=TOKENS["paper"], activebackground=TOKENS["paper"],
            ).pack(anchor="w", pady=(8, 0))
        if self.saved:
            self._label(self.body, "You're all set. Run `localflow` whenever you're ready.", font=("TkDefaultFont", 13, "bold"))
            tk.Button(
                self.body, text="Launch localflow now", command=self._launch_localflow,
                bg=TOKENS["sun"], fg=TOKENS["ink"], relief="solid", bd=2,
            ).pack(anchor="w", pady=(10, 0))

    def _render_footer(self) -> None:
        busy = bool(self._busy)
        if self.step > 0 and not self.saved:
            tk.Button(
                self.footer, text="Back", command=self.back,
                bg=TOKENS["card"], fg=TOKENS["ink"], relief="solid", bd=2,
            ).pack(side="left")
        if self._cancel is not None and busy and self.step in (2, 3):
            tk.Button(
                self.footer, text="Cancel", command=self.cancel_download,
                bg=TOKENS["card"], fg=TOKENS["ink"], relief="solid", bd=2,
            ).pack(side="left", padx=(8, 0))
        if self.step == 5 and not self.saved:
            tk.Button(
                self.footer, text="Skip", command=self.next_step,
                bg=TOKENS["card"], fg=TOKENS["ink"], relief="solid", bd=2,
            ).pack(side="left", padx=(8, 0))
        text = self._primary_label()
        self.primary_button = tk.Button(
            self.footer,
            text=text,
            command=self.primary_action,
            bg=TOKENS["cobalt"],
            fg="#FFFFFF",
            activebackground=TOKENS["cobalt"],
            activeforeground="#FFFFFF",
            relief="solid",
            bd=2,
            padx=20,
            pady=8,
            font=("TkDefaultFont", 11, "bold"),
            state="disabled" if busy else "normal",
        )
        self.primary_button.pack(side="right")
        if self.step in (2, 3) and (self._busy or self.error_message or self.status_message):
            self._progress_var.set(self.error_message or self.status_message)
            tk.Label(
                self.footer, textvariable=self._progress_var,
                bg=TOKENS["paper"], fg=TOKENS["red"] if self.error_message else TOKENS["muted"],
            ).pack(side="right", padx=12)

    def _primary_label(self) -> str:
        if self.saved:
            return "Close"
        if self.step == 0:
            return "Get started"
        if self.step == 1:
            return "Continue"
        if self.step == 2:
            if self._busy:
                return "Downloading…"
            return "Continue" if self.downloaded.get(self.stt_var.get()) else "Download & continue"
        if self.step == 3:
            if self.choices.llm_provider != "ollama":
                return "Continue"
            selected = self.llm_var.get()
            if selected == "none" or not self.ollama_running:
                return "Continue"
            if selected in self.ollama_models:
                return "Continue"
            return "Download & continue"
        if self.step == 4:
            return "Continue"
        if self.step == 5:
            return "Record 5 seconds" if not self._busy else "Recording…"
        return "Finish"

    def _start_job(self, key: str, function) -> None:
        self._busy.add(key)
        self.error_message = ""
        self.status_message = ""
        self._cancel = threading.Event() if key in ("stt-download", "llm-pull") else None
        self._render_footer()

        def work():
            try:
                result = function()
            except Exception as exc:
                self._events.put(("error", key, exc))
            else:
                self._events.put(("done", key, result))

        threading.Thread(target=work, name=f"setup-{key}", daemon=True).start()

    def _start_checks(self) -> None:
        if "checks" not in self._busy and self.checks is None:
            self._start_job("checks", lambda: setup_flow.run_checks(self.config))

    def _start_download_checks(self) -> None:
        if "stt-checks" not in self._busy and not self.downloaded:
            self._start_job(
                "stt-checks",
                lambda: {model.id: is_stt_downloaded(model) for model in self.models},
            )

    def _start_ollama_check(self) -> None:
        if self.choices.llm_provider == "ollama" and "ollama-check" not in self._busy:
            self._ollama_checked = True
            def check():
                client = OllamaClient(self.config.llm_base_url)
                if not client.is_running():
                    return False, []
                return True, client.installed()

            self._start_job("ollama-check", check)

    def _download_stt(self) -> None:
        selected = next(model for model in self.models if model.id == self.stt_var.get())
        cancel = self._cancel = threading.Event()
        self.error_message = ""
        self.status_message = "Preparing download…"
        self._busy.add("stt-download")
        self._render_footer()

        def work():
            try:
                path = download_stt(
                    selected,
                    lambda done, total: self._events.put(("progress", "stt-download", done, total)),
                    cancel,
                )
            except Exception as exc:
                self._events.put(("error", "stt-download", exc))
            else:
                self._events.put(("done", "stt-download", path))

        threading.Thread(target=work, name="setup-stt-download", daemon=True).start()

    def _pull_llm(self) -> None:
        selected = self.llm_var.get()
        cancel = self._cancel = threading.Event()
        self.error_message = ""
        self.status_message = "Preparing Ollama download…"
        self._busy.add("llm-pull")

        def work():
            try:
                client = OllamaClient(self.config.llm_base_url)
                client.pull(
                    selected,
                    lambda done, total, status: self._events.put(
                        ("progress", "llm-pull", done, total, status)
                    ),
                    cancel,
                )
            except Exception as exc:
                self._events.put(("error", "llm-pull", exc))
            else:
                self._events.put(("done", "llm-pull", selected))

        threading.Thread(target=work, name="setup-llm-pull", daemon=True).start()

    def _start_trial(self) -> None:
        self.error_message = ""
        self.status_message = "Loading speech model…"
        model_id = self.stt_var.get()
        language = self.language_var.get() or None
        device = "" if self.device_var.get() == "System default" else self.device_var.get()

        def run():
            from localflow.stt import Transcriber

            model = next(entry for entry in self.models if entry.id == model_id)
            transcriber = Transcriber(model_size=model.model_size, language=language,
                                      backend=model.backend)
            self._test_transcriber = transcriber
            return setup_flow.try_dictation(5, device, transcriber)

        self._start_job("trial", run)

    def _start_monitor(self) -> None:
        if self._monitor is not None:
            return
        device = "" if self.device_var.get() == "System default" else self.device_var.get()
        self._monitor = MicLevelMonitor(device)
        self._start_job("monitor", self._monitor.start)

    def _change_device(self, _event=None) -> None:
        if self._monitor is not None:
            self._monitor.stop()
            self._monitor = None
        self._start_monitor()

    def _update_meter(self) -> None:
        if self.step == 4 and self._monitor is not None:
            self._draw_meter()
            self.root.after(100, self._update_meter)

    def _draw_meter(self) -> None:
        if not hasattr(self, "meter") or not hasattr(self, "meter_bar"):
            return
        width = max(1, self.meter.winfo_width())
        level = self._monitor.level() if self._monitor is not None else 0.0
        color = TOKENS["red"] if level > 0.6 else TOKENS["cobalt"]
        self.meter.coords(self.meter_bar, 0, 0, int(width * level), 18)
        self.meter.itemconfigure(self.meter_bar, fill=color)

    def _show_operation_state(self) -> None:
        if self.step not in (2, 3):
            return
        if self.error_message:
            self._label(self.body, self.error_message, fg=TOKENS["red"], pady=(6, 0))
            tk.Button(self.body, text="Retry", command=self.primary_action).pack(anchor="w", pady=(4, 0))
        elif self.status_message:
            self._label(self.body, self.status_message, fg=TOKENS["muted"], pady=(6, 0))

    def _poll_events(self) -> None:
        while True:
            try:
                event = self._events.get_nowait()
            except queue.Empty:
                break
            kind, key, *data = event
            if kind == "progress":
                done, total, *extra = data
                status = extra[0] if extra else ""
                prefix = f"{done / 1_000_000:.0f} / {total / 1_000_000:.0f} MB"
                self.status_message = f"{prefix}  ·  {status}".strip(" ·") if status else prefix
                if self.step in (2, 3):
                    self._progress_var.set(self.status_message)
            elif kind == "error":
                error = data[0]
                self._busy.discard(key)
                self._cancel = None
                if isinstance(error, DownloadCancelled):
                    self.status_message = "Download cancelled."
                else:
                    self.error_message = str(error) or error.__class__.__name__
                if key == "monitor":
                    self.status_message = "Microphone meter unavailable."
                if key == "stt-checks" or key == "checks":
                    self.status_message = str(error)
                if key == "checks":
                    self.checks = []
                elif key == "stt-checks":
                    self.downloaded = {model.id: False for model in self.models}
                elif key == "ollama-check":
                    self.ollama_running, self.ollama_models = False, []
                    self._ollama_checked = True
                self._render()
            elif kind == "done":
                result = data[0]
                self._busy.discard(key)
                if key == "checks":
                    self.checks = result
                elif key == "stt-checks":
                    self.downloaded.update(result)
                elif key == "ollama-check":
                    self.ollama_running, self.ollama_models = result
                    self._ollama_checked = True
                elif key == "stt-download":
                    self.downloaded[self.stt_var.get()] = True
                    self.status_message = f"Downloaded {Path(result).name}"
                elif key == "llm-pull":
                    if result not in self.ollama_models:
                        self.ollama_models.append(result)
                    self.status_message = f"Downloaded {result}"
                elif key == "trial":
                    self.status_message = result or "No speech detected. Try again near your microphone."
                    self.step = 5
                elif key == "apply":
                    self.saved = True
                    self.status_message = str(result)
                elif key == "monitor":
                    self._monitor_started = True
                self._cancel = None
                self._render()
        if self.step == 4:
            self._draw_meter()
        self.root.after(100, self._poll_events)

    def _fix_check(self, check) -> None:
        if not check.fix_url:
            return
        if check.fix_url.startswith(("https://", "http://", "x-apple.systempreferences:")):
            webbrowser.open(check.fix_url)
        else:
            messagebox.showinfo("Run this command", check.fix_url, parent=self.root)

    def _select_stt(self, model_id: str) -> None:
        self.stt_var.set(model_id)
        self._render()

    def _select_llm(self, model_id: str) -> None:
        self.llm_var.set(model_id)
        self._render()

    def _open_settings(self) -> None:
        webbrowser.open(f"http://127.0.0.1:{self.config.server_port}/settings")

    def _continue(self) -> None:
        self.error_message = ""
        self.status_message = ""
        if self.step == 4:
            self.choices = SetupChoices(
                stt_id=self.stt_var.get(),
                llm_id=None if self.llm_var.get() == "none" else self.llm_var.get(),
                input_device="" if self.device_var.get() == "System default" else self.device_var.get(),
                hotkey=self._hotkey_value(self.hotkey_var.get()),
                language=self.language_var.get() or None,
                start_on_login=bool(self.start_login_var.get()),
                llm_provider=self.choices.llm_provider,
            )
        self.step = min(6, self.step + 1)
        self._render()

    def next_step(self) -> None:
        self._continue()

    def back(self) -> None:
        if self.step > 0 and not self.saved:
            if self.step == 4 and self._monitor is not None:
                self._monitor.stop()
                self._monitor = None
            self.step -= 1
            self._render()

    def primary_action(self) -> None:
        if self.saved:
            self.close()
        elif self.step in (0, 1):
            self._continue()
        elif self.step == 2:
            if self.downloaded.get(self.stt_var.get()):
                self._continue()
            elif self.error_message:
                self._download_stt()
            elif not self._busy:
                self._download_stt()
        elif self.step == 3:
            if self.choices.llm_provider != "ollama":
                self._continue()
                return
            selected = self.llm_var.get()
            if selected == "none" or not self.ollama_running or selected in self.ollama_models:
                self._continue()
            elif self.error_message or not self._busy:
                self._pull_llm()
        elif self.step == 4:
            self._continue()
        elif self.step == 5:
            if not self._busy:
                self._start_trial()
        elif self.step == 6:
            if "apply" not in self._busy:
                self._start_job("apply", lambda: setup_flow.apply(self.choices))

    def skip_trial(self) -> None:
        self._continue()

    def cancel_download(self) -> None:
        if self._cancel is not None:
            self._cancel.set()

    def _recheck_ollama(self) -> None:
        self._ollama_checked = False
        self._start_ollama_check()

    def _recheck_checks(self) -> None:
        self.checks = None
        self.error_message = ""
        self._start_checks()
        self._render()

    def _launch_localflow(self) -> None:
        subprocess.Popen([sys.executable, "-m", "localflow.app"])

    def close(self) -> None:
        if self._monitor is not None:
            self._monitor.stop()
            self._monitor = None
        self.root.destroy()


def main():
    root = tk.Tk()
    SetupWindow(root)
    root.mainloop()


if __name__ == "__main__":
    main()
