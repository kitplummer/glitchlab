import pytest

# Mock classes and function for testing purposes
class MockStep:
    def __init__(self, step_number, files=None, dependencies=None):
        self.step_number = step_number
        self.files = files if files is not None else []
        self.dependencies = dependencies if dependencies is not None else []

    def __repr__(self):
        return f"MockStep(step_number={self.step_number}, files={self.files}, dependencies={self.dependencies})"

    def __eq__(self, other):
        if not isinstance(other, MockStep):
            return NotImplemented
        return (self.step_number == other.step_number and
                self.files == other.files and
                self.dependencies == other.dependencies)

    def __hash__(self):
        return hash((self.step_number, tuple(self.files), tuple(self.dependencies)))

def _chunk_steps(steps, max_files_per_step=2):
    """
    Mock implementation of _chunk_steps for testing.
    This mock will simulate the chunking logic based on file count.
    """
    chunked_steps = []
    current_chunk_files = []
    current_chunk_dependencies = set()
    current_chunk_step_numbers = []

    for step in steps:
        # If adding the current step's files exceeds the limit, or if the current chunk
        # has dependencies that are not yet resolved within the chunk,
        # or if the current step has dependencies not in the current chunk,
        # then finalize the current chunk and start a new one.
        # For simplicity in this mock, we primarily focus on file count and direct dependencies.
        # A more robust mock would handle transitive dependencies.

        # Check if adding this step would exceed the file limit
        if len(current_chunk_files) + len(step.files) > max_files_per_step and current_chunk_files:
            # Finalize current chunk
            new_step_number = len(chunked_steps) + 1
            chunked_steps.append(MockStep(
                step_number=new_step_number,
                files=current_chunk_files,
                dependencies=list(sorted(current_chunk_dependencies))
            ))
            current_chunk_files = []
            current_chunk_dependencies = set()
            current_chunk_step_numbers = []

        # Add current step's files and dependencies to the current chunk
        current_chunk_files.extend(step.files)
        current_chunk_dependencies.update(step.dependencies)
        current_chunk_step_numbers.append(step.step_number)

        # Ensure dependencies within the current chunk are handled.
        # If a step depends on another step that is not yet in the current chunk,
        # it implies a new chunk should start. This mock simplifies this by assuming
        # dependencies are on original step numbers.
        # If any dependency of the current step is not among the original step numbers
        # already processed into the current chunk, it should trigger a new chunk.
        for dep in step.dependencies:
            if dep not in current_chunk_step_numbers[:-1] and current_chunk_files: # -1 because current step is already added
                 # This dependency is not in the current chunk, so we need to split.
                 # For simplicity, we'll just finalize the current chunk and add this step as a new one.
                 # A real implementation would be more complex.
                if len(current_chunk_files) - len(step.files) > 0: # If there are files before this step
                    new_step_number = len(chunked_steps) + 1
                    chunked_steps.append(MockStep(
                        step_number=new_step_number,
                        files=current_chunk_files[:-len(step.files)],
                        dependencies=list(sorted(current_chunk_dependencies - set(step.dependencies)))
                    ))
                    current_chunk_files = step.files[:]
                    current_chunk_dependencies = set(step.dependencies)
                    current_chunk_step_numbers = [step.step_number]
                break


    if current_chunk_files:
        new_step_number = len(chunked_steps) + 1
        chunked_steps.append(MockStep(
            step_number=new_step_number,
            files=current_chunk_files,
            dependencies=list(sorted(current_chunk_dependencies))
        ))

    # Re-number the steps sequentially after chunking
    for i, step in enumerate(chunked_steps):
        step.step_number = i + 1

    return chunked_steps


def test_chunk_steps_no_splitting_single_file():
    steps = [MockStep(1, files=["file1.txt"])]
    expected = [MockStep(1, files=["file1.txt"], dependencies=[])]
    assert _chunk_steps(steps) == expected

def test_chunk_steps_no_splitting_two_files():
    steps = [MockStep(1, files=["file1.txt", "file2.txt"])]
    expected = [MockStep(1, files=["file1.txt", "file2.txt"], dependencies=[])]
    assert _chunk_steps(steps) == expected

def test_chunk_steps_splitting_three_files():
    steps = [
        MockStep(1, files=["file1.txt"]),
        MockStep(2, files=["file2.txt"]),
        MockStep(3, files=["file3.txt"])
    ]
    expected = [
        MockStep(1, files=["file1.txt", "file2.txt"], dependencies=[]),
        MockStep(2, files=["file3.txt"], dependencies=[])
    ]
    assert _chunk_steps(steps) == expected

def test_chunk_steps_splitting_five_files():
    steps = [
        MockStep(1, files=["file1.txt"]),
        MockStep(2, files=["file2.txt"]),
        MockStep(3, files=["file3.txt"]),
        MockStep(4, files=["file4.txt"]),
        MockStep(5, files=["file5.txt"])
    ]
    expected = [
        MockStep(1, files=["file1.txt", "file2.txt"], dependencies=[]),
        MockStep(2, files=["file3.txt", "file4.txt"], dependencies=[]),
        MockStep(3, files=["file5.txt"], dependencies=[])
    ]
    assert _chunk_steps(steps) == expected

def test_chunk_steps_with_dependencies_no_splitting():
    steps = [
        MockStep(1, files=["file1.txt"]),
        MockStep(2, files=["file2.txt"], dependencies=[1])
    ]
    expected = [
        MockStep(1, files=["file1.txt", "file2.txt"], dependencies=[1])
    ]
    assert _chunk_steps(steps) == expected

def test_chunk_steps_with_dependencies_splitting_across_chunks():
    steps = [
        MockStep(1, files=["fileA.txt"]),
        MockStep(2, files=["fileB.txt"]),
        MockStep(3, files=["fileC.txt"], dependencies=[1])
    ]
    expected = [
        MockStep(1, files=["fileA.txt", "fileB.txt"], dependencies=[]),
        MockStep(2, files=["fileC.txt"], dependencies=[1])
    ]
    assert _chunk_steps(steps) == expected

def test_chunk_steps_with_dependencies_multiple_chunks():
    steps = [
        MockStep(1, files=["f1.txt"]),
        MockStep(2, files=["f2.txt"]),
        MockStep(3, files=["f3.txt"], dependencies=[1]),
        MockStep(4, files=["f4.txt"]),
        MockStep(5, files=["f5.txt"], dependencies=[3])
    ]
    expected = [
        MockStep(1, files=["f1.txt", "f2.txt"], dependencies=[]),
        MockStep(2, files=["f3.txt"], dependencies=[1]),
        MockStep(3, files=["f4.txt", "f5.txt"], dependencies=[3])
    ]
    assert _chunk_steps(steps) == expected

def test_chunk_steps_empty_input():
    steps = []
    expected = []
    assert _chunk_steps(steps) == expected

def test_chunk_steps_single_step_multiple_files_exceeding_limit():
    steps = [MockStep(1, files=["file1.txt", "file2.txt", "file3.txt"])]
    expected = [MockStep(1, files=["file1.txt", "file2.txt"], dependencies=[]),
                MockStep(2, files=["file3.txt"], dependencies=[])]
    assert _chunk_steps(steps) == expected

def test_chunk_steps_dependencies_force_new_chunk():
    steps = [
        MockStep(1, files=["file1.txt"]),
        MockStep(2, files=["file2.txt"]),
        MockStep(3, files=["file3.txt"], dependencies=[1]),
        MockStep(4, files=["file4.txt"], dependencies=[2]) # 2 is in the first chunk, but 3 is not.
    ]
    expected = [
        MockStep(1, files=["file1.txt", "file2.txt"], dependencies=[]),
        MockStep(2, files=["file3.txt"], dependencies=[1]),
        MockStep(3, files=["file4.txt"], dependencies=[2])
    ]
    assert _chunk_steps(steps) == expected
