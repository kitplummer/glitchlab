import pytest
from glitchlab.router import select_with_fallbacks

def test_select_with_fallbacks_basic():
    primary = "model_a"
    fallbacks = ["model_b", "model_c"]
    expected = ["model_a", "model_b", "model_c"]
    assert select_with_fallbacks(primary, fallbacks) == expected

def test_select_with_fallbacks_no_fallbacks():
    primary = "model_a"
    fallbacks = []
    expected = ["model_a"]
    assert select_with_fallbacks(primary, fallbacks) == expected

def test_select_with_fallbacks_duplicate_primary_in_fallbacks():
    primary = "model_a"
    fallbacks = ["model_b", "model_a", "model_c"]
    expected = ["model_a", "model_b", "model_c"]
    assert select_with_fallbacks(primary, fallbacks) == expected
