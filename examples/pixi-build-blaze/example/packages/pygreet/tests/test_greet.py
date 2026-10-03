import pygreet


def test_greet():
    assert pygreet.greet("x") == "Hello, x! (from C in Python)"
