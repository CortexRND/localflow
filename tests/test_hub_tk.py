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

        dictionary.add_word("Existing")
        window._edit_dictionary("Local Flow", "", "word")
        window.word_var.set("Existing")
        window._add_word()
        assert dictionary.list()["words"] == ["Local Flow", "Existing"]

        window.show_page("Style")
        assert set(window.cleanup_buttons) == {"none", "light", "medium"}
        assert set(window.style_buttons["work"]) == {
            "formal", "casual", "very_casual", "excited"
        }
        window.cleanup_var.set("medium")
        window.style_vars["work"].set("casual")
        window._save_style()
        assert saved[0].cleanup_level == "medium"
        assert saved[0].style_work == "casual"
    finally:
        root.destroy()


def test_models_page_reloads_config(tmp_path, monkeypatch):
    root, window, _history, _dictionary = _window(tmp_path)
    refreshed = Config(model_size="fresh-model")
    monkeypatch.setattr("localflow.hub_tk.load_config", lambda: refreshed)
    try:
        window.show_page("Models")
        assert window.config is refreshed
    finally:
        root.destroy()


def test_hub_history_populated_and_empty_states(tmp_path):
    root, window, history, _dictionary = _window(tmp_path)
    try:
        window.show_page("History")
        assert history.recent() == []
        for index in range(10):
            history.add(
                kind="dictate",
                status="ok",
                raw_text=f"raw {index}",
                final_text=f"final {index}",
                ts=index,
            )
        window.show_page("History")
        root.update()
        assert window.history_rows.winfo_children()
        assert len(window.history_rows.winfo_children()) == 8
        window._change_history_page(8)
        root.update()
        assert len(window.history_rows.winfo_children()) == 2
        buttons = [
            child.cget("text")
            for frame in window.history_rows.winfo_children()
            for child in frame.winfo_children()
            if isinstance(child, tk.Button)
        ]
        assert "Copy original" in buttons
    finally:
        root.destroy()
