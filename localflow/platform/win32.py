import base64
import os
import subprocess
import sys
from pathlib import Path


def _xml_escape(value: str) -> str:
    return (
        value.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace('"', "&quot;")
        .replace("'", "&apos;")
    )


class Win32Platform:
    name = "win32"

    def paste(self, method: str = "auto") -> None:
        if method not in ("auto", "pynput"):
            raise ValueError(f"unknown paste method: {method!r}")
        from pynput import keyboard

        controller = keyboard.Controller()
        with controller.pressed(keyboard.Key.ctrl):
            controller.press("v")
            controller.release("v")

    def notify(self, title: str, message: str) -> None:
        xml = (
            "<toast><visual><binding template=\"ToastText02\">"
            f"<text id=\"1\">{_xml_escape(title)}</text>"
            f"<text id=\"2\">{_xml_escape(message)}</text>"
            "</binding></visual></toast>"
        )
        encoded_xml = base64.b64encode(xml.encode("utf-8")).decode("ascii")
        script = (
            "[Windows.UI.Notifications.ToastNotificationManager, "
            "Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null; "
            "[Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, "
            "ContentType = WindowsRuntime] | Out-Null; "
            "$xml = New-Object Windows.Data.Xml.Dom.XmlDocument; "
            "$xml.LoadXml([Text.Encoding]::UTF8.GetString("
            f"[Convert]::FromBase64String('{encoded_xml}'))); "
            "$toast = [Windows.UI.Notifications.ToastNotification]::new($xml); "
            '[Windows.UI.Notifications.ToastNotificationManager]::'
            'CreateToastNotifier("localflow").Show($toast)'
        )
        try:
            subprocess.Popen(
                ["powershell", "-NoProfile", "-NonInteractive", "-Command", script],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        except (OSError, RuntimeError, ValueError):
            return

    def play_sound(self, cue: str) -> None:
        try:
            import winsound

            winsound.MessageBeep(
                winsound.MB_ICONASTERISK if cue == "start" else winsound.MB_OK
            )
        except (OSError, RuntimeError, ValueError):
            return

    def open_path(self, target: str) -> None:
        os.startfile(target)

    def autostart_install(self, program: str | None = None) -> Path:
        if program is None:
            program = str(Path(sys.executable).parent / "localflow.exe")
        import winreg

        with winreg.OpenKey(
            winreg.HKEY_CURRENT_USER,
            r"Software\Microsoft\Windows\CurrentVersion\Run",
            0,
            winreg.KEY_SET_VALUE,
        ) as key:
            winreg.SetValueEx(key, "localflow", 0, winreg.REG_SZ, f'"{program}"')
        return Path(program)

    def autostart_uninstall(self) -> None:
        import winreg

        try:
            with winreg.OpenKey(
                winreg.HKEY_CURRENT_USER,
                r"Software\Microsoft\Windows\CurrentVersion\Run",
                0,
                winreg.KEY_SET_VALUE,
            ) as key:
                winreg.DeleteValue(key, "localflow")
        except FileNotFoundError:
            pass

    def autostart_status(self) -> str:
        import winreg

        try:
            with winreg.OpenKey(
                winreg.HKEY_CURRENT_USER,
                r"Software\Microsoft\Windows\CurrentVersion\Run",
                0,
                winreg.KEY_READ,
            ) as key:
                command, _kind = winreg.QueryValueEx(key, "localflow")
        except FileNotFoundError:
            return "not installed"
        return f"installed ({command})"

    def try_lock(self, fd: int) -> bool:
        import msvcrt

        try:
            os.lseek(fd, 0, os.SEEK_SET)
            msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)
        except OSError:
            return False
        return True

    def unlock(self, fd: int) -> None:
        import msvcrt

        os.lseek(fd, 0, os.SEEK_SET)
        msvcrt.locking(fd, msvcrt.LK_UNLCK, 1)
