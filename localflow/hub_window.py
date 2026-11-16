"""Hub entrypoint: `python -m localflow.hub_window [--page PAGE]`.

On macOS every request goes to the single main-app instance (main_app), which
owns the one native Hub window and routes `--page` to it. `run_backend` is the
actual platform backend (native AppKit hub, else Tk); main_app's portable
fallback calls it directly so the dispatcher never routes back into itself.
"""

import argparse
import sys

from localflow.hub_route import DEFAULT_PAGE, HUB_PAGES, canonical_page


def run_backend(page=DEFAULT_PAGE):
    """Run the platform hub (no singleton, no routing)."""
    if sys.platform == "darwin":
        try:
            from localflow.hub_mac import main as mac_main
        except ImportError:
            pass
        else:
            return mac_main(initial_page=page)
    if page == "Hot Phrases":
        # The Tk hub has no Hot Phrases page; its own editor window is the
        # portable fallback.
        from localflow.phrases_tk import main as phrases_tk_main

        return phrases_tk_main()
    from localflow.hub_tk import PAGES as TK_PAGES
    from localflow.hub_tk import main as tk_main

    # The Tk fallback has no Prompts page yet.
    return tk_main(initial_page=page if page in TK_PAGES else "Home")


def route_to_main_app(page):
    """Hand `page` to the main-app singleton (macOS)."""
    from localflow.main_app import main as app_main

    return app_main(["--page", page])


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--page", choices=HUB_PAGES, default=DEFAULT_PAGE,
        # "settings" -> "Settings"; anything unknown still fails `choices`.
        type=lambda value: canonical_page(value) or value,
    )
    args = parser.parse_args(argv)
    if sys.platform == "darwin":
        return route_to_main_app(args.page)
    return run_backend(args.page)


if __name__ == "__main__":
    main()
