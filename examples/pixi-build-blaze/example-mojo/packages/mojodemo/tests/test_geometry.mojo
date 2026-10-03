from std.testing import assert_equal, TestSuite
from geometry import Vec2


def test_norm() raises:
    assert_equal(Vec2(3, 4).norm(), 5.0)


def test_add() raises:
    var v = Vec2(1, 2) + Vec2(3, 4)
    assert_equal(v.x, 4.0)
    assert_equal(v.y, 6.0)


def main() raises:
    TestSuite.discover_tests[__functions_in_module()]().run()
