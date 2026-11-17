import unittest

from ci_memory_analysis import heap_types, regions, size


class MemoryAnalysisTests(unittest.TestCase):
    def test_sizes(self):
        self.assertEqual(size("1.5M"), 1572864)
        self.assertEqual(size("0K"), 0)
        self.assertEqual(size("24"), 24)
        for value in ["", "missing", "-1K", "1.0Q"]:
            with self.assertRaises(ValueError):
                size(value)

    def test_vmmap_regions_and_empty_or_error_diagnostics(self):
        self.assertEqual(regions("MALLOC_SMALL       24.0M  13.0M  12.0M  0K 0K\nStack       2M  688K  640K  0K"),
                         {"MALLOC_SMALL": (13631488, 12582912), "Stack": (704512, 655360)})
        self.assertEqual(regions(""), {})
        self.assertEqual(regions("Unable to inspect task"), {})

    def test_heap_class_names_and_empty_or_error_diagnostics(self):
        self.assertEqual(heap_types("  45   673920  14976.0   Swift Metadata    C libswiftCore.dylib"),
                         {"Swift Metadata    C libswiftCore.dylib": 673920})
        self.assertEqual(heap_types(""), {})
        self.assertEqual(heap_types("heap: denied"), {})


if __name__ == "__main__":
    unittest.main()
