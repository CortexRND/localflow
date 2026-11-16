"""Greedy word wrap with an injected measure (no AppKit, no heavy imports).

Shared by the Ask layout and the Hub's pure layout helpers: AppKit callers
pass a real font measurement, tests pass a fake one.
"""


def wrap_lines(text, width, measure_line):
    """Greedy word wrap of `text` into lines no wider than `width`.

    `measure_line(str) -> width`. A single word wider than `width` is broken
    by characters, like a wrapping label does.
    """
    lines = []
    for paragraph in str(text).split("\n"):
        current = ""
        for word in paragraph.split(" "):
            candidate = word if not current else current + " " + word
            if measure_line(candidate) <= width:
                current = candidate
                continue
            if current:
                lines.append(current)
                current = ""
            while len(word) > 1 and measure_line(word) > width:
                cut = len(word) - 1
                while cut > 1 and measure_line(word[:cut]) > width:
                    cut -= 1
                lines.append(word[:cut])
                word = word[cut:]
            current = word
        lines.append(current)
    return lines
