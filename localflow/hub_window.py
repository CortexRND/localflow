"""Native AppKit hub on macOS, with a Tk fallback on other platforms."""

import sys


def main():
    if sys.platform == "darwin":
        try:
            from localflow.hub_mac import main as mac_main
        except ImportError:
            pass
        else:
            return mac_main()
    from localflow.hub_tk import main as tk_main

    return tk_main()


if __name__ == "__main__":
    main()
