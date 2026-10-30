import pytest

tk = pytest.importorskip("tkinter")

from localflow.config import Config
from localflow.dictionary import DictionaryStore
from localflow.history import HistoryStore
from localflow.hub_tk import PAGES, HubWindow


def _window(tmp_path):
    try:
        root = tk.Tk()
    except tk.TclError as exc:
        pytest.skip(f"Tk unavailable: {exc}")
    history = HistoryStore(tmp_path / "history.sqlite", tmp_path / "clips")
    dictionary = DictionaryStore(tmp_path / "dictionary.json")
    window = HubWindow(root, history, dictionary, Config())
    return root, window, history, dictionary


def test_hub_renders_all_pages_and_error_states(tmp_path):
    root, window, _history, _dictionary = _window(tmp_path)
    try:
        for page in PAGES:
            window.show_page(page)
            root.update()
            assert window.page == page
            window.set_page_error(page, "fixture error")
            root.update()
            assert window.page_errors[page] == "fixture error"
    finally:
        root.destroy()


def test_hub_dictionary_edit_and_style_save_use_local_stores(tmp_path, monkeypatch):
    root, window, _history, dictionary = _window(tmp_path)
    saved = []
    monkeypatch.setattr("localflow.hub_tk.save_config", saved.append)
    try:
        window.show_page("Dictionary")
        window.word_var.set("LocalFlow")
        window._add_word()
        window.from_var.set("teh")
        window.to_var.set("the")
        window._add_replacement()
        window._edit_dictionary("LocalFlow", "", "word")
        window.word_var.set("Local Flow")
        window._add_word()
        window._edit_dictionary("teh", "the", "replacement")
        window.from_var.set("teh")
        window.to_var.set("the")
        window._add_replacement()
        assert dictionary.list()["words"] == ["Local Flow"]
        assert dictionary.list()["replacements"] == [{"from": "teh", "to": "the"}]

        window.show_page("Style")
        window.cleanup_var.set("medium")
        window.style_vars["work"].set("casual")
        window._save_style()
        assert saved[0].cleanup_level == "medium"
        assert saved[0].style_work == "casual"
    finally:
        root.destroy()


def test_hub_history_populated_and_empty_states(tmp_path):
    root, window, history, _dictionary = _window(tmp_path)
    try:
        window.show_page("History")
        assert history.recent() == []
        history.add(
            kind="dictate", status="ok", raw_text="raw", final_text="final"
        )
        window.show_page("History")
        root.update()
        assert window.history_rows.winfo_children()
    finally:
        root.destroy()
