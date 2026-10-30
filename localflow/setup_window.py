"""Native-window dispatcher for `lf setup`."""

import sys


def main():
    if sys.platform == "darwin":
        try:
            from localflow.setup_mac import main as mac_main
        except ImportError:
            pass
        else:
            return mac_main()
    from localflow.setup_tk import main as tk_main

    return tk_main()


if __name__ == "__main__":
    main()
