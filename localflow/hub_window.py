"""Native AppKit hub on macOS, with a Tk fallback on other platforms."""

import argparse
import sys


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--page",
        choices=("Home", "Meetings", "History", "Dictionary", "Style", "Models"),
        default="Home",
    )
    args = parser.parse_args()
    if sys.platform == "darwin":
        try:
            from localflow.hub_mac import main as mac_main
        except ImportError:
            pass
        else:
            return mac_main(initial_page=args.page)
    from localflow.hub_tk import main as tk_main

    return tk_main(initial_page=args.page)


if __name__ == "__main__":
    main()
