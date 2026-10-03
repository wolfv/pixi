from geometry import Vec2
from stats import centroid


def main():
    var pts: List[Vec2] = [Vec2(0, 0), Vec2(4, 0), Vec2(4, 3)]
    var c = centroid(pts)
    print("centroid:", c.x, c.y, "norm of (3,4):", Vec2(3, 4).norm())
