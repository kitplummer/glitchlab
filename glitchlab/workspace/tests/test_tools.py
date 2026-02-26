import os
import tempfile
from pathlib import Path
import pytest

from glitchlab.workspace.tools import TruncationGuardError, write_file_with_truncation_guard

def test_write_file_truncation_guard_triggers():
    with tempfile.TemporaryDirectory() as tmpdir:
        file_path = Path(tmpdir) / "test_file.txt"
        file_path.write_text("This is a long string that should be truncated.")
        old_size = os.path.getsize(file_path)

        new_content = "short"
        with pytest.raises(TruncationGuardError):
            write_file_with_truncation_guard(file_path, new_content, min_old_size_for_check=10, min_ratio=0.5)

def test_write_file_truncation_guard_does_not_trigger_new_file():
    with tempfile.TemporaryDirectory() as tmpdir:
        file_path = Path(tmpdir) / "new_file.txt"
        new_content = "This is new content."
        write_file_with_truncation_guard(file_path, new_content)
        assert file_path.read_text() == new_content

def test_write_file_truncation_guard_does_not_trigger_minor_change():
    with tempfile.TemporaryDirectory() as tmpdir:
        file_path = Path(tmpdir) / "modified_file.txt"
        original_content = "Line 1\nLine 2\nLine 3"
        file_path.write_text(original_content)

        new_content = "Line 1\nLine 2\nLine 3\nLine 4"
        write_file_with_truncation_guard(file_path, new_content)
        assert file_path.read_text() == new_content

def test_write_file_truncation_guard_does_not_trigger_small_old_file():
    with tempfile.TemporaryDirectory() as tmpdir:
        file_path = Path(tmpdir) / "small_file.txt"
        file_path.write_text("small") # Less than 100 bytes
        old_size = os.path.getsize(file_path)

        new_content = "s"
        write_file_with_truncation_guard(file_path, new_content, min_old_size_for_check=100)
        assert file_path.read_text() == new_content

def test_write_file_truncation_guard_does_not_trigger_large_new_content():
    with tempfile.TemporaryDirectory() as tmpdir:
        file_path = Path(tmpdir) / "large_new_content.txt"
        file_path.write_text("short")
        old_size = os.path.getsize(file_path)

        new_content = "This is a much longer string now."
        write_file_with_truncation_guard(file_path, new_content)
        assert file_path.read_text() == new_content
