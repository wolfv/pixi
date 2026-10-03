from std.testing import assert_equal, TestSuite
from geometry import Vec2
from stats import mean, centroid


def test_mean() raises:
    var xs: List[Float64] = [1.0, 2.0, 3.0]
    assert_equal(mean(xs), 2.0)


def test_centroid() raises:
    var pts: List[Vec2] = [Vec2(0, 0), Vec2(2, 0), Vec2(2, 2), Vec2(0, 2)]
    var c = centroid(pts)
    assert_equal(c.x, 1.0)
    assert_equal(c.y, 1.0)


def main() raises:
    TestSuite.discover_tests[__functions_in_module()]().run()
