"""Hot-phrase editor entrypoint. On macOS the editor is the Hot Phrases page of
the single native Hub (main_app owns the window), so this dispatcher routes
there: `lf phrases`, the menu bar's "Hot Phrases…" item and the optional
"LocalFlow Hot Phrases.app" compatibility alias all end up on that page.
Everywhere else (and with an explicit `path`, which only the standalone
editors understand) the Tk window (phrases_tk) opens. `python -m
localflow.phrases_window` keeps working for all of those entry points.
"""

import sys

PAGE = "Hot Phrases"


def main(path=None):
    if sys.platform == "darwin" and path is None:
        from localflow.hub_window import route_to_main_app

        return route_to_main_app(PAGE)
    from localflow.phrases_tk import main as tk_main

    return tk_main(path)


if __name__ == "__main__":
    main()
