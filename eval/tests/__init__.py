"""Fast, model-free tests: python3 -m unittest eval.tests."""

from pathlib import Path


def load_tests(loader, tests, pattern):
    directory = Path(__file__).resolve().parent
    return loader.discover(str(directory), pattern=pattern or "test*.py",
                           top_level_dir=str(directory.parent.parent))
